//! Rendering for full-screen code review mode.

use bcode_plugin_sdk::path::display_from_current_dir;
use std::fmt::Write as _;

use bcode_code_review_models::{
    ReviewSourceDiagnosticSeverity, ReviewSourceKind, ReviewSurfaceKind,
};
use bcode_markdown_render::{MarkdownRenderOptions, render_markdown_lines};
use bcode_syntax_render::SyntaxHighlighter;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
#[cfg(test)]
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::truncate_to_display_width;
use bmux_tui_components::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing, ModalTheme,
};

use crate::code_review_controls::{ControlSurface, ReviewActionGroup};
use crate::code_review_tui::{
    ReviewApp, ReviewFile, ReviewLineKind, ReviewMouseAction, ReviewPromptKind, ReviewPublishState,
    ReviewSidebarMode, add_source_menu_items, advanced_source_menu_items, review_thread_kind_label,
    review_thread_severity_label, sidebar_width,
};
use crate::code_review_tui_display::{
    ReviewDisplayRow, ReviewDisplayRowSource, ReviewDisplaySegment, ReviewDisplayTextRole,
};
use crate::code_review_tui_view::{
    DiffContextDirection, ReviewThreadAction, ReviewViewBlock, ReviewViewDocument, ReviewViewRow,
    ReviewViewTarget,
};
use bcode_code_review_models::ReviewSource;
use bmux_tui_components::action_row::ActionRowStyles;
use bmux_tui_components::tab_bar::TabBarStyles;

fn syntax_palette(
    theme: bcode_plugin_sdk::tui::PluginTuiSyntaxTheme,
) -> bcode_syntax_render::SyntaxPalette {
    let color = |color: bcode_plugin_sdk::tui::PluginTuiSyntaxColor| {
        bcode_syntax_render::SyntaxColor::from_tui(color.into())
    };
    bcode_syntax_render::SyntaxPalette {
        text: color(theme.text),
        comment: color(theme.comment),
        keyword: color(theme.keyword),
        function: color(theme.function),
        variable: color(theme.variable),
        string: color(theme.string),
        number: color(theme.number),
        type_name: color(theme.type_name),
        operator: color(theme.operator),
        punctuation: color(theme.punctuation),
        heading: color(theme.heading),
        link: color(theme.link),
        raw: color(theme.raw),
    }
}

#[derive(Debug, Clone, Copy)]
struct ReviewTheme {
    canvas: Style,
    muted: Style,
    focused: Style,
    selection: Style,
    overlay: Style,
    diff: bcode_plugin_sdk::tui::PluginTuiDiffTheme,
}

impl Default for ReviewTheme {
    fn default() -> Self {
        Self {
            canvas: Style::new(),
            muted: Style::new().fg(Color::BrightBlack),
            focused: Style::new().fg(Color::Cyan),
            selection: Style::new().add_modifier(Modifier::REVERSED),
            overlay: Style::new().fg(Color::White).bg(Color::BrightBlack),
            diff: fallback_review_diff_theme(),
        }
    }
}

impl From<bcode_plugin_sdk::tui::PluginTuiTheme> for ReviewTheme {
    fn from(theme: bcode_plugin_sdk::tui::PluginTuiTheme) -> Self {
        let component = theme.component_theme();
        Self {
            canvas: theme.canvas,
            muted: theme.muted,
            focused: theme.focused,
            selection: theme.selection,
            overlay: component.map_or_else(
                || theme.canvas.patch(theme.text),
                |theme| theme.surfaces.overlay.patch(theme.text),
            ),
            diff: theme.diff,
        }
    }
}

const fn fallback_review_diff_theme() -> bcode_plugin_sdk::tui::PluginTuiDiffTheme {
    bcode_plugin_sdk::tui::PluginTuiDiffTheme {
        text: Style::new(),
        muted: Style::new().fg(Color::BrightBlack),
        title: Style::new().fg(Color::BrightMagenta),
        label: Style::new().fg(Color::BrightBlack),
        added: Style::new().fg(Color::BrightGreen),
        removed: Style::new().fg(Color::BrightRed),
        hunk: Style::new().fg(Color::BrightMagenta),
        added_row: Style::new().bg(Color::Rgb(0, 24, 16)),
        removed_row: Style::new().bg(Color::Rgb(32, 10, 10)),
        added_emphasis: Style::new().fg(Color::BrightGreen),
        removed_emphasis: Style::new().fg(Color::BrightRed),
    }
}

/// Render one full-screen code review frame.
///
/// # Panics
/// Panics if a prior controls operation poisoned the review controls mutex.
pub fn render(
    app: &mut ReviewApp,
    frame: &mut PaintCx<'_, '_>,
    theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
) {
    let area = Rect::new(0, 0, frame.area().width, frame.area().height);
    app.clear_mouse_regions();
    app.controls
        .lock()
        .expect("review controls lock")
        .begin_frame(
            app.comment_editor.is_some()
                || app.prompt_state.is_some()
                || app.import_state.is_some()
                || app.publish_state.is_some()
                || app.help_visible,
        );
    app.comment_input_visible = false;
    if let Some(editor) = &mut app.comment_editor {
        editor
            .focused_input
            .painted(&editor.buffer, Rect::default(), Rect::default());
    }
    if area.is_empty() {
        app.observe_comment_input();
        return;
    }

    let review_theme = theme.map_or_else(ReviewTheme::default, |theme| ReviewTheme::from(*theme));
    frame.fill(LocalRect::terminal(area), " ", review_theme.canvas);
    render_chrome(app, area, frame, review_theme);
    let diff_area = render_body(app, area, frame, review_theme);
    render_main_content(app, diff_area, frame, theme, review_theme);
    render_overlays(app, area, frame, review_theme);
    app.observe_comment_input();
    app.confine_mouse_regions(area);
}

fn render_chrome(app: &ReviewApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    let header = Rect::new(area.x, area.y, area.width, 1);
    render_header(app, header, frame, theme);
    render_header_actions(
        app,
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(2).min(2),
        ),
        frame,
        theme,
    );

    let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    render_footer(app, footer, frame, theme);
}

fn render_header_actions(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    paint_review_controls(app, ControlSurface::Header, area, frame, theme);
}

fn paint_review_controls(
    app: &ReviewApp,
    surface: ControlSurface,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let styles = ActionRowStyles {
        normal: theme.focused,
        focused: theme.selection,
        hovered: theme.selection,
        pressed: theme.selection,
        disabled: theme.muted,
    };
    let tabs = TabBarStyles {
        normal: theme.focused,
        selected: theme.selection,
        focused: theme.selection,
        hovered: theme.selection,
        pressed: theme.selection,
        disabled: theme.muted,
        separator: theme.muted,
    };
    let mut controls = app.controls.lock().expect("review controls lock");
    controls.sync_sidebar(app.sidebar_mode);
    controls.paint_surface(surface, area, frame, styles, tabs);
}

fn render_body(
    app: &mut ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) -> Rect {
    let body = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(4),
    );
    let sidebar_width = sidebar_width(app, area.width);
    if sidebar_width == 0 {
        app.set_file_area(None);
        return body;
    }

    let file_area = Rect::new(body.x, body.y, sidebar_width, body.height);
    app.set_file_area(Some(file_area));
    render_sidebar(app, file_area, frame, theme);
    let separator = Rect::new(file_area.right(), body.y, 1, body.height);
    render_separator(separator, frame, theme);
    Rect::new(
        separator.right(),
        body.y,
        body.width.saturating_sub(sidebar_width).saturating_sub(1),
        body.height,
    )
}

fn render_sidebar(
    app: &mut ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let content = area;
    match app.sidebar_mode {
        ReviewSidebarMode::Included => render_included(app, content, frame, theme),
        ReviewSidebarMode::Repository => render_files(app, content, frame, theme),
        ReviewSidebarMode::Threads
        | ReviewSidebarMode::General
        | ReviewSidebarMode::NeedsAttention => {
            render_threads(app, content, frame, theme);
        }
        ReviewSidebarMode::Summary => render_review_summary(app, content, frame, theme),
        ReviewSidebarMode::Sources => render_sources(app, content, frame, theme),
    }
}

fn render_main_content(
    app: &mut ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    review_theme: ReviewTheme,
) {
    app.set_diff_area(area);
    if app.ux_mode == crate::code_review_tui::ReviewUxMode::Build {
        render_build_workspace(app, area, frame, review_theme);
    } else {
        render_diff(app, area, frame, theme, review_theme);
    }
}

fn render_overlays(
    app: &mut ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    if app.help_visible {
        render_help(app, area, frame, theme);
    }
    if app.comment_editor.is_some() {
        render_comment_editor(app, area, frame, theme);
    }
    if app.prompt_state.is_some() {
        render_prompt(app, area, frame, theme);
    }
    if app.import_state.is_some() {
        render_import_modal(app, area, frame, theme);
    }
    if app.publish_state.is_some() {
        render_publish_modal(app, area, frame, theme);
    }
}

fn render_header(app: &ReviewApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    if area.is_empty() {
        return;
    }
    let file_label = app
        .selected_file_data()
        .map_or("no files", |file| file.display_path());
    let file_position = if app.review.files.is_empty() {
        "0/0".to_string()
    } else {
        format!(
            "{}/{}",
            app.selected_file.saturating_add(1),
            app.review.files.len()
        )
    };
    let drafts = app.draft_comment_count();
    let draft_label = if drafts == 0 {
        String::new()
    } else {
        format!("  💬 {drafts} draft")
    };
    let (viewed_files, total_files) = app.viewed_file_counts();
    let readiness = app.review_readiness_label();
    let thread_label = header_thread_label(app);
    let surface_kind = app
        .review
        .surfaces()
        .get(app.selected_file)
        .map_or("diff", |surface| match surface.kind {
            ReviewSurfaceKind::Diff => "diff",
            ReviewSurfaceKind::File => "file",
        });
    let text = if app.ux_mode == crate::code_review_tui::ReviewUxMode::Build {
        let workspace = &app.workspace;
        let included_sources = app.included_source_count();
        let (info_sources, warning_sources, error_sources) = app.diagnostic_source_counts();
        let pending = match (app.pending_workspace_save, app.pending_workspace_reload) {
            (true, true) => "  pending save+reload",
            (true, false) => "  pending save",
            (false, true) => "  pending reload",
            (false, false) => "",
        };
        format!(
            " bcode review build  {}  {}/{} source(s) included  {} surface(s)  diag i/w/e {}/{}/{}{} ",
            workspace.title,
            included_sources,
            workspace.sources.len(),
            app.review.surfaces().len(),
            info_sources,
            warning_sources,
            error_sources,
            pending
        )
    } else if app.review.is_repository_review() {
        format!(
            " bcode review  {}  {}  File {}  Surface {}  Line {}{}{}  viewed {}/{}  {readiness} ",
            app.review.title,
            file_label,
            file_position,
            surface_kind,
            app.selected_diff_line.saturating_add(1),
            draft_label,
            thread_label,
            viewed_files,
            total_files
        )
    } else {
        let (hunk, hunk_total) = app.hunk_position();
        format!(
            " bcode review  {}  {}  File {}  Surface {}  Hunk {}/{}  Row {}{}{}  viewed {}/{}  {readiness}  +{} -{} ",
            app.review.title,
            file_label,
            file_position,
            surface_kind,
            hunk,
            hunk_total,
            app.selected_diff_line.saturating_add(1),
            draft_label,
            thread_label,
            viewed_files,
            total_files,
            app.review.additions,
            app.review.deletions
        )
    };
    frame.write_line_with_fallback_style(
        LocalRect::terminal(area),
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(&text, usize::from(area.width)),
            theme.focused.add_modifier(Modifier::BOLD),
        )]),
        theme.focused,
    );
}

fn header_thread_label(app: &ReviewApp) -> String {
    let (open_threads, resolved_threads) = app.thread_status_counts();
    if open_threads == 0 && resolved_threads == 0 {
        String::new()
    } else if app.show_resolved_threads {
        format!(
            "  threads {}  open {open_threads}  resolved {resolved_threads}  filter:{}",
            open_threads.saturating_add(resolved_threads),
            app.thread_filter.label()
        )
    } else {
        format!(
            "  threads {}  open {open_threads}  hidden {resolved_threads}  filter:{}",
            open_threads.saturating_add(resolved_threads),
            app.thread_filter.label()
        )
    }
}

fn render_footer(app: &ReviewApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    if area.is_empty() {
        return;
    }
    let text = app.status_message.as_deref().map_or_else(
        || {
            format!(
                "Tab controls · ←/→ select · Enter activate · Esc content  {}",
                crate::keybindings::hints().unwrap_or_default().join("  ")
            )
        },
        str::to_owned,
    );
    frame.write_line(
        LocalRect::terminal(area),
        &Line::from_spans(vec![Span::styled(
            text,
            theme.canvas.patch(theme.diff.text),
        )]),
    );
}

fn render_separator(area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    for y in area.y..area.bottom() {
        frame.write_line(
            LocalRect::terminal(Rect::new(area.x, y, 1, 1)),
            &Line::from_spans(vec![Span::styled("│", theme.muted)]),
        );
    }
}

fn render_included(app: &ReviewApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    if area.is_empty() {
        return;
    }
    frame.write_line(
        LocalRect::terminal(Rect::new(area.x, area.y, area.width, 1)),
        &Line::from_spans(vec![Span::styled(" Included", theme.focused)]),
    );
    let visible_rows = usize::from(area.height.saturating_sub(1));
    for row in 0..visible_rows {
        let Some(source) = app.workspace.sources.get(row) else {
            break;
        };
        let marker = if source.included { "✓" } else { " " };
        let text = format!(" [{marker}] {}", source.label);
        frame.write_line(
            LocalRect::terminal(Rect::new(
                area.x,
                area.y
                    .saturating_add(1)
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&text, usize::from(area.width)),
                theme.overlay,
            )]),
        );
    }
}

fn render_sources(app: &ReviewApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    if area.is_empty() {
        return;
    }
    frame.write_line(
        LocalRect::terminal(Rect::new(area.x, area.y, area.width, 1)),
        &Line::from_spans(vec![Span::styled(" Sources", theme.focused)]),
    );
    if app.workspace.sources.is_empty() {
        frame.write_line(
            LocalRect::terminal(Rect::new(area.x, area.y.saturating_add(1), area.width, 1)),
            &Line::from_spans(vec![Span::styled(
                " A add source",
                theme.muted.patch(theme.overlay),
            )]),
        );
        return;
    }
    for (row, source) in app
        .workspace
        .sources
        .iter()
        .enumerate()
        .take(usize::from(area.height.saturating_sub(1)))
    {
        let marker = if source.included { "✓" } else { " " };
        let kind = source_kind_short_label(&source.kind);
        let text = format!(" {marker} {kind:<10} {}", source.label);
        let style = if row == app.selected_build_row
            && app.ux_mode == crate::code_review_tui::ReviewUxMode::Build
        {
            theme.selection
        } else if source.included {
            theme.overlay
        } else {
            theme.muted.patch(theme.overlay)
        };
        frame.write_line(
            LocalRect::terminal(Rect::new(
                area.x,
                area.y
                    .saturating_add(1)
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&text, usize::from(area.width)),
                style,
            )]),
        );
    }
}

const fn source_kind_short_label(kind: &ReviewSourceKind) -> &'static str {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => "unstaged",
        ReviewSourceKind::IndexStaged => "staged",
        ReviewSourceKind::WorkingTreeAndIndex => "worktree",
        ReviewSourceKind::LastCommit => "last",
        ReviewSourceKind::Commit { .. } => "commit",
        ReviewSourceKind::CommitRange { merge_base, .. } => {
            if *merge_base {
                "range..."
            } else {
                "range.."
            }
        }
        ReviewSourceKind::BranchCompare { merge_base, .. } => {
            if *merge_base {
                "branch..."
            } else {
                "branch.."
            }
        }
        ReviewSourceKind::File { .. } => "file",
        ReviewSourceKind::FileRange { .. } => "file-range",
        ReviewSourceKind::Repository => "repo",
    }
}

const fn diagnostic_severity_label(severity: ReviewSourceDiagnosticSeverity) -> &'static str {
    match severity {
        ReviewSourceDiagnosticSeverity::Info => "info",
        ReviewSourceDiagnosticSeverity::Warning => "warn",
        ReviewSourceDiagnosticSeverity::Error => "error",
    }
}

fn render_files(app: &mut ReviewApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    use bmux_tui_components::tree_view::{TreeViewComponent, TreeViewStyles};
    use std::cell::RefCell;
    if area.is_empty() {
        return;
    }
    let model = app.navigation_items();
    let (items, rows) = model.as_ref();
    let visible = app.navigation_visible_indices(items);
    app.selected_tree_row = app.selected_tree_row.min(visible.len().saturating_sub(1));
    app.tree_scroll = app
        .tree_scroll
        .min(visible.len().saturating_sub(usize::from(area.height)));
    // Bound component measurement, labels, painting and native hits to the viewport.
    // This also avoids imposing terminal u16 geometry on the lifetime/size of a review.
    let mut viewport_items = Vec::new();
    let mut viewport_rows = Vec::new();
    for source in visible
        .iter()
        .copied()
        .skip(app.tree_scroll)
        .take(usize::from(area.height))
    {
        let mut item = items[source].clone();
        if let crate::code_review_tui::ReviewFileTreeRow::File { index, .. } = rows[source] {
            let viewed = if app.file_viewed(index) { "✓" } else { "○" };
            let opened = if index == app.selected_file {
                "●"
            } else {
                " "
            };
            let source_label = app
                .review
                .surfaces
                .get(index)
                .and_then(|surface| {
                    app.workspace
                        .sources
                        .iter()
                        .find(|source| source.id == surface.source_id)
                })
                .map(|source| source.label.as_str());
            let source = source_label.map_or_else(String::new, |source| format!(" [{source}]"));
            let drafts = app.draft_comment_count_for_file(index);
            let threads = app.open_thread_count_for_file(index);
            let counts = app.review.files.get(index).map_or_else(
                || {
                    let mut counts = String::new();
                    if drafts > 0 {
                        let _ = write!(counts, " 💬{drafts}");
                    }
                    if threads > 0 {
                        let _ = write!(counts, " ⚠{threads}");
                    }
                    counts
                },
                |file| file_row_counts(file, drafts, threads),
            );
            let status = app
                .review
                .files
                .get(index)
                .map_or("", |file| file.status.label());
            item.label = format!("{opened}{viewed} {status} {}{source}{counts}", item.label);
        }
        viewport_items.push(item);
        viewport_rows.push(rows[source].clone());
    }
    let mut tree_state = if app.navigation_items == viewport_items {
        app.navigation_pointer_state.clone()
    } else {
        bmux_tui_components::tree_view::TreeViewState::new(None)
    };
    app.reconcile_navigation_expansion(&mut tree_state, &viewport_rows);
    tree_state.set_selected_visible(
        app.selected_tree_row
            .checked_sub(app.tree_scroll)
            .filter(|row| *row < viewport_items.len()),
    );
    let state = RefCell::new(tree_state);
    let component =
        TreeViewComponent::new("review.files", &viewport_items, &state).styles(TreeViewStyles {
            normal: theme.overlay,
            selected: theme.selection,
            hovered: theme.focused,
            pressed: theme.selection,
            disabled: theme.muted,
            marker: theme.muted,
        });
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
    // Pointer input reuses exactly the component items and resolved layout that painted.
    app.navigation_pointer_state = state.into_inner();
    app.navigation_layout = Some(layout);
    app.navigation_items = viewport_items;
    app.navigation_visible_rows = viewport_rows;
}

fn render_review_summary(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    if area.is_empty() {
        return;
    }
    let (viewed, total_files) = app.viewed_file_counts();
    let (open_threads, resolved_threads) = app.thread_status_counts();
    let general_threads = app
        .thread_summaries()
        .into_iter()
        .filter(|thread| thread.anchor.is_review_level())
        .count();
    let lines = [
        " Summary".to_string(),
        format!(" title: {}", app.review.title),
        format!(" repo: {}", display_from_current_dir(&app.review.repo_root)),
        format!(" files: {viewed}/{total_files} viewed"),
        format!(" diff: +{} -{}", app.review.additions, app.review.deletions),
        format!(" threads: {open_threads} open, {resolved_threads} resolved"),
        format!(" general: {general_threads}"),
        format!(" sources: {}", app.workspace.sources.len()),
        format!(" surfaces: {}", app.review.surfaces().len()),
        format!(" commits: {}", app.review.repository_commits.len()),
    ];
    for (row, line) in lines.into_iter().enumerate().take(usize::from(area.height)) {
        let style = if row == 0 {
            theme.focused
        } else {
            theme.overlay
        };
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(
                area.x,
                area.y
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&line, usize::from(area.width)),
                style,
            )]),
            style,
        );
    }
}

#[allow(clippy::too_many_lines)]
fn render_threads(
    app: &mut ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    if area.is_empty() {
        return;
    }
    render_thread_toolbar(app, area, frame, theme);
    let threads = app.visible_thread_summaries();
    let detail_height = if threads.is_empty() {
        0
    } else {
        area.height.clamp(0, 7).min(area.height.saturating_sub(3))
    };
    let list_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1).saturating_sub(detail_height),
    );
    if list_area.is_empty() {
        return;
    }
    let visible_rows = usize::from(list_area.height);
    if threads.is_empty() {
        let label = match app.sidebar_mode {
            ReviewSidebarMode::NeedsAttention => "open".to_string(),
            ReviewSidebarMode::General => "general".to_string(),
            _ => app.thread_filter.label().to_string(),
        };
        frame.write_line(
            LocalRect::terminal(list_area),
            &Line::from_spans(vec![Span::styled(
                format!(" no {label} review threads"),
                theme.muted,
            )]),
        );
        return;
    }
    app.selected_thread = app.selected_thread.min(threads.len().saturating_sub(1));
    if app.selected_thread < app.thread_scroll {
        app.thread_scroll = app.selected_thread;
    }
    if app.selected_thread >= app.thread_scroll.saturating_add(visible_rows) {
        app.thread_scroll = app
            .selected_thread
            .saturating_sub(visible_rows.saturating_sub(1));
    }

    for row in 0..visible_rows {
        let y = list_area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        let index = app.thread_scroll.saturating_add(row);
        let line_area = Rect::new(list_area.x, y, list_area.width, 1);
        if let Some(thread) = threads.get(index) {
            let selected = index == app.selected_thread;
            let style = if selected {
                theme.selection
            } else {
                theme.overlay
            };
            let marker = if thread.external_provider_id.is_some() {
                "⇩"
            } else if thread.resolved {
                "✓"
            } else {
                "●"
            };
            let line_label = thread.line_label();
            let body = thread.latest_body.lines().next().unwrap_or_default();
            let kind = review_thread_kind_label(thread.thread_kind);
            let severity = review_thread_severity_label(thread.severity);
            let path_label = thread.anchor.scope_label();
            let agent_status =
                app.agent_state_for_anchor(&thread.anchor)
                    .map_or_else(String::new, |state| {
                        let warning = if state.stream_warning.is_some() {
                            " ⚠"
                        } else {
                            ""
                        };
                        format!("  🤖 {}{warning}", state.live_state_label())
                    });
            let suggestion_status = suggestion_sidebar_label(thread);
            let state_tokens = thread_sidebar_state_tokens(app, thread).join("/");
            let external_status = external_sidebar_label(thread);
            let text = format!(
                " {marker} [{state_tokens}] {kind}/{severity} {path_label}:{line_label} {external_status}{suggestion_status} {body}{agent_status}",
            );
            frame.write_line_with_fallback_style(
                LocalRect::terminal(line_area),
                &Line::from_spans(vec![Span::styled(
                    truncate_to_display_width(&text, usize::from(list_area.width)),
                    style,
                )]),
                style,
            );
            app.register_mouse_region(
                line_area,
                ReviewMouseAction::SelectThread(index),
                "select thread",
            );
        }
    }

    if detail_height > 0 {
        let detail_area = Rect::new(
            area.x,
            area.bottom().saturating_sub(detail_height),
            area.width,
            detail_height,
        );
        render_thread_detail(app, detail_area, frame, &threads, theme);
    }
}

fn thread_sidebar_state_tokens(
    app: &ReviewApp,
    thread: &crate::code_review_tui::ReviewThreadSummary,
) -> Vec<String> {
    let mut tokens = vec![if thread.resolved {
        "resolved".to_string()
    } else {
        "open".to_string()
    }];
    if thread.draft_count > 0 {
        tokens.push(format!("draft:{}", thread.draft_count));
    }
    if app.agent_state_for_anchor(&thread.anchor).is_some() {
        tokens.push("AI-exchange".to_string());
    }
    if thread.pending_suggestion_count
        + thread.refining_suggestion_count
        + thread.accepted_suggestion_count
        + thread.rejected_suggestion_count
        > 0
    {
        tokens.push("suggestion".to_string());
    }
    if thread.session_id.is_some() {
        tokens.push("linked-session".to_string());
    }
    if thread.external_provider_id.is_some() {
        tokens.push("external".to_string());
    }
    if app.thread_publish_label(&thread.anchor) == "published" {
        tokens.push("published".to_string());
    }
    tokens
}

fn external_sidebar_label(thread: &crate::code_review_tui::ReviewThreadSummary) -> String {
    let Some(provider) = thread.external_provider_id.as_deref() else {
        return String::new();
    };
    let mapping = match thread.external_mapping_status {
        Some(bcode_code_review_models::ExternalAnchorMappingStatus::Exact) => "mapped",
        Some(bcode_code_review_models::ExternalAnchorMappingStatus::Approximate) => "stale",
        Some(bcode_code_review_models::ExternalAnchorMappingStatus::Unmappable) => "unmappable",
        None => "external",
    };
    format!("[{provider}:{mapping}] ")
}

fn suggestion_sidebar_label(thread: &crate::code_review_tui::ReviewThreadSummary) -> String {
    let mut parts = Vec::new();
    if thread.pending_suggestion_count > 0 {
        parts.push(format!("suggest:{}", thread.pending_suggestion_count));
    }
    if thread.refining_suggestion_count > 0 {
        parts.push(format!("refining:{}", thread.refining_suggestion_count));
    }
    if thread.accepted_suggestion_count > 0 {
        parts.push(format!("accepted:{}", thread.accepted_suggestion_count));
    }
    if thread.rejected_suggestion_count > 0 {
        parts.push(format!("rejected:{}", thread.rejected_suggestion_count));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("[{}]", parts.join("/"))
    }
}

fn render_thread_detail(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    threads: &[crate::code_review_tui::ReviewThreadSummary],
    theme: ReviewTheme,
) {
    let Some(thread) = threads.get(app.selected_thread) else {
        return;
    };
    let border_style = theme.muted.patch(theme.overlay);
    frame.write_line(
        LocalRect::terminal(Rect::new(area.x, area.y, area.width, 1)),
        &Line::from_spans(vec![Span::styled(
            "─".repeat(usize::from(area.width)),
            border_style,
        )]),
    );
    let title = format!(
        " [{}] {} {} {} {}",
        thread_sidebar_state_tokens(app, thread).join("/"),
        review_thread_kind_label(thread.thread_kind),
        thread.anchor.scope_label(),
        thread.line_label(),
        suggestion_sidebar_label(thread)
    );
    frame.write_line(
        LocalRect::terminal(Rect::new(area.x, area.y.saturating_add(1), area.width, 1)),
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(&title, usize::from(area.width)),
            theme.diff.hunk.patch(theme.overlay),
        )]),
    );
    paint_review_controls(
        app,
        ControlSurface::Actions(ReviewActionGroup::Thread),
        Rect::new(
            area.x,
            area.y.saturating_add(2),
            area.width,
            area.height.saturating_sub(2).min(1),
        ),
        frame,
        theme,
    );
    let body = thread.latest_body.lines().next().unwrap_or_default();
    let mut detail_lines = Vec::new();
    if let Some(agent_state) = app.agent_state_for_anchor(&thread.anchor) {
        let warning = agent_state
            .stream_warning
            .as_ref()
            .map_or(String::new(), |warning| format!(" ⚠ {warning}"));
        detail_lines.push((
            format!(
                "🤖 Bcode: {} · {}{warning}",
                agent_state.live_state_label(),
                agent_state.status
            ),
            theme.focused.patch(theme.overlay),
        ));
        if let Some(activity) = &agent_state.activity {
            detail_lines.push((
                format!("   activity: {activity}"),
                theme.muted.patch(theme.overlay),
            ));
        }
        if let Some(answer) = agent_state.answer.lines().next()
            && !answer.is_empty()
        {
            detail_lines.push((format!("   answer: {answer}"), theme.overlay));
        }
    }
    detail_lines.push((body.to_string(), theme.overlay));
    for (index, (line, style)) in detail_lines.into_iter().enumerate() {
        let y = area
            .y
            .saturating_add(3)
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        frame.write_line(
            LocalRect::terminal(Rect::new(area.x, y, area.width, 1)),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&line, usize::from(area.width)),
                style,
            )]),
        );
    }
}

fn render_thread_toolbar(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    paint_review_controls(
        app,
        ControlSurface::Actions(ReviewActionGroup::Filters),
        area,
        frame,
        theme,
    );
}

fn file_row_counts(file: &ReviewFile, draft_comments: usize, open_threads: usize) -> String {
    let mut counts = String::new();
    if draft_comments > 0 {
        let _ = write!(counts, " 💬{draft_comments}");
    }
    if open_threads > 0 {
        let _ = write!(counts, " ⚠{open_threads}");
    }
    let _ = write!(counts, " +{} -{}", file.additions, file.deletions);
    counts
}

fn render_build_workspace(
    app: &mut ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    if area.is_empty() {
        return;
    }
    let rows = build_workspace_rows(app);
    let visible_rows = usize::from(area.height);
    if app.selected_build_row < app.build_scroll {
        app.build_scroll = app.selected_build_row;
    } else if app.selected_build_row >= app.build_scroll.saturating_add(visible_rows) {
        app.build_scroll = app
            .selected_build_row
            .saturating_add(1)
            .saturating_sub(visible_rows);
    }
    let mut selectable_index = 0usize;
    let visible_rows = rows
        .into_iter()
        .filter_map(|(prefix, text, selectable, warning)| {
            let row_index = selectable.then_some(selectable_index);
            if selectable {
                selectable_index = selectable_index.saturating_add(1);
            }
            if row_index.is_some_and(|index| index < app.build_scroll) {
                return None;
            }
            Some((prefix, text, selectable, warning, row_index))
        })
        .take(visible_rows);
    for (row, (prefix, text, selectable, warning, row_index)) in visible_rows.enumerate() {
        let selected = selectable && row_index == Some(app.selected_build_row);
        let style = if selected {
            theme.selection
        } else if warning {
            theme.diff.hunk.patch(theme.overlay)
        } else {
            theme.overlay
        };
        let line = if text.is_empty() {
            prefix
        } else {
            format!("{prefix} {text}")
        };
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(area.x, y, area.width, 1)),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&line, usize::from(area.width)),
                style,
            )]),
            style,
        );
    }
}

fn push_source_rows(app: &ReviewApp, rows: &mut Vec<(String, String, bool, bool)>) {
    let surfaces = app.review.surfaces();
    for source in &app.workspace.sources {
        let marker = if source.included { "✓" } else { " " };
        let surface_count = surfaces
            .iter()
            .filter(|surface| surface.source_id == source.id)
            .count();
        let diagnostics = app.source_diagnostics(&source.id);
        let status = source_status_label(source.included, surface_count, diagnostics.len());
        rows.push((
            format!("  [{marker}] {:<10}", source_kind_short_label(&source.kind)),
            format!("{}  · {status}", source.label),
            true,
            source.included && surface_count == 0,
        ));
        for diagnostic in diagnostics {
            rows.push((
                format!("    {}", diagnostic_severity_label(diagnostic.severity)),
                format!("{}: {}", diagnostic.code, diagnostic.message),
                false,
                true,
            ));
        }
    }
}

fn push_build_workspace_summary_rows(
    app: &ReviewApp,
    surfaces: &[bcode_code_review_models::ReviewSurface],
    rows: &mut Vec<(String, String, bool, bool)>,
) {
    let workspace = &app.workspace;
    let duplicate_sources = duplicate_source_count(&workspace.sources);
    let empty_sources = empty_included_source_count(app, surfaces);
    rows.push((
        "Summary".to_string(),
        format!(
            "{} source(s), {} included, {} surface(s), {} empty included, {} duplicate, {} diagnostic(s)",
            workspace.sources.len(),
            app.included_source_count(),
            surfaces.len(),
            empty_sources,
            duplicate_sources,
            app.review.diagnostics.len()
        ),
        false,
        empty_sources != 0 || duplicate_sources != 0 || !app.review.diagnostics.is_empty(),
    ));
}

fn empty_included_source_count(
    app: &ReviewApp,
    surfaces: &[bcode_code_review_models::ReviewSurface],
) -> usize {
    app.workspace
        .sources
        .iter()
        .filter(|source| {
            source.included
                && !surfaces
                    .iter()
                    .any(|surface| surface.source_id == source.id)
        })
        .count()
}

fn build_workspace_rows(app: &ReviewApp) -> Vec<(String, String, bool, bool)> {
    let workspace = &app.workspace;
    let surfaces = app.review.surfaces();
    let included_count = workspace
        .sources
        .iter()
        .filter(|source| source.included)
        .count();
    let mut rows = Vec::new();
    rows.push((
        "Review workspace".to_string(),
        format!(": {}", workspace.title),
        false,
        false,
    ));
    rows.push((
        "Sources".to_string(),
        format!(
            ": {included_count}/{} included, {} materialized surface(s)",
            workspace.sources.len(),
            surfaces.len()
        ),
        false,
        false,
    ));
    rows.push((
        "Materialization".to_string(),
        if app.workspace_materialization_in_progress {
            ": in progress — keep this review open".to_string()
        } else if app.workspace_materialization_failed {
            ": failed — inspect diagnostics, edit the source, then press R".to_string()
        } else {
            ": ready — press R to refresh sources".to_string()
        },
        false,
        app.workspace_materialization_failed,
    ));
    if workspace.sources.is_empty() {
        rows.push((
            "Quick add".to_string(),
            "u unstaged   s staged   w worktree   l last commit".to_string(),
            false,
            false,
        ));
        rows.push((
            "Choose".to_string(),
            "A source menu   + repository file source".to_string(),
            false,
            false,
        ));
        rows.push((String::new(), String::new(), false, false));
    }
    push_build_workspace_summary_rows(app, &surfaces, &mut rows);
    rows.push((String::new(), String::new(), false, false));
    rows.push(("Included sources".to_string(), String::new(), false, false));
    if workspace.sources.is_empty() {
        rows.push((
            "  !".to_string(),
            "no sources yet — pick a quick source or open the source menu".to_string(),
            false,
            true,
        ));
    }
    push_source_rows(app, &mut rows);
    rows.push((String::new(), String::new(), false, false));
    rows.push(("Review surfaces".to_string(), String::new(), false, false));
    if surfaces.is_empty() {
        rows.push((
            "  !".to_string(),
            "no reviewable surfaces yet".to_string(),
            false,
            true,
        ));
    }
    for surface in &surfaces {
        let kind = match surface.kind {
            ReviewSurfaceKind::Diff => "diff",
            ReviewSurfaceKind::File => "file",
        };
        rows.push((format!("  {kind:4}"), surface.path.clone(), true, false));
    }
    rows.push((String::new(), String::new(), false, false));
    rows.push((
        crate::keybindings::hints().unwrap_or_default().join("  "),
        String::new(),
        false,
        false,
    ));

    rows
}

fn duplicate_source_count(sources: &[ReviewSource]) -> usize {
    let mut seen = Vec::new();
    let mut duplicates = 0usize;
    for source in sources {
        if seen.contains(&source.kind) {
            duplicates = duplicates.saturating_add(1);
        } else {
            seen.push(source.kind.clone());
        }
    }
    duplicates
}

fn source_status_label(included: bool, surface_count: usize, diagnostic_count: usize) -> String {
    if !included {
        return "excluded".to_string();
    }
    if diagnostic_count != 0 && surface_count == 0 {
        return format!("no surfaces, {diagnostic_count} diagnostic(s)");
    }
    if diagnostic_count != 0 {
        return format!("{surface_count} surface(s), {diagnostic_count} diagnostic(s)");
    }
    if surface_count == 0 {
        return "no surfaces".to_string();
    }
    format!("{surface_count} surface(s)")
}

fn render_diff(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    review_theme: ReviewTheme,
) {
    if area.is_empty() {
        return;
    }
    if app.review.is_repository_review() {
        render_repository_file(app, area, frame, review_theme);
        return;
    }
    if selected_surface_kind(app) == Some(ReviewSurfaceKind::File) {
        render_materialized_file_surface(app, area, frame, review_theme);
        return;
    }
    let Some(file) = app.selected_file_data() else {
        render_empty(area, "No changed files", frame, review_theme);
        return;
    };
    if file.is_binary {
        render_empty(area, "Binary file diff not available", frame, review_theme);
        return;
    }
    let Some(document) = app.current_review_view_document() else {
        render_empty(area, "No textual changes", frame, review_theme);
        return;
    };
    if document.rows.is_empty() {
        render_empty(area, "No textual changes", frame, review_theme);
        return;
    }
    render_view_document(app, &document, area, frame, theme, review_theme);
}

fn render_view_document(
    app: &ReviewApp,
    document: &ReviewViewDocument,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    review_theme: ReviewTheme,
) {
    let syntax_highlighter = theme.map_or_else(SyntaxHighlighter::new, |theme| {
        SyntaxHighlighter::with_palette(syntax_palette(theme.syntax))
    });
    let syntax_hint = app
        .selected_file_path()
        .or_else(|| {
            app.selected_file_data()
                .map(|file| file.display_path().to_string())
        })
        .unwrap_or_default();
    let can_highlight = syntax_highlighter.can_highlight(&syntax_hint);
    let visible = usize::from(area.height);
    for row in 0..visible {
        let visual_row = app.diff_scroll.saturating_add(row);
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        let Some(view_row) = document.row_for_visual_row(visual_row) else {
            break;
        };
        let mut rendered = render_view_row(
            app,
            view_row,
            &syntax_highlighter,
            can_highlight,
            &syntax_hint,
            area.width,
            review_theme,
        );
        if app.is_view_target_selected(&view_row.target) {
            rendered.line = selected_line(&rendered.line, review_theme.selection);
            rendered.style = rendered.style.patch(review_theme.selection);
        }
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(area.x, y, area.width, 1)),
            &rendered.line,
            rendered.style,
        );
    }
}

fn render_omitted_context_row(
    direction: DiffContextDirection,
    hidden_line_count: usize,
    start_line: u32,
    end_line: u32,
    reason: Option<&str>,
    theme: ReviewTheme,
) -> RenderedRow {
    let step = hidden_line_count.min(20);
    let action = match direction {
        DiffContextDirection::Down => format!("expand next {step}"),
        DiffContextDirection::Up => format!("expand previous {step}"),
    };
    let (style, text) = reason.map_or_else(
        || {
            (
                theme.muted.patch(theme.overlay),
                format!(
                    " ⋯ {hidden_line_count} hidden lines ({start_line}-{end_line})  [{action}] "
                ),
            )
        },
        |reason| {
            (
                theme.diff.removed.patch(theme.overlay),
                format!(
                    " ⋯ {hidden_line_count} hidden lines unavailable ({start_line}-{end_line}): {reason}  [retry] "
                ),
            )
        },
    );
    RenderedRow {
        line: Line::from_spans(vec![Span::styled(text, style)]),
        style,
    }
}

fn render_inline_thread_header(
    anchor: &crate::code_review_tui_view::ReviewThreadAnchor,
    comment_count: usize,
    collapsed: bool,
    resolved: bool,
    theme: ReviewTheme,
) -> RenderedRow {
    let style = theme
        .diff
        .hunk
        .patch(theme.overlay)
        .add_modifier(Modifier::BOLD);
    let status = if resolved { "RESOLVED" } else { "OPEN" };
    let status_style = if resolved {
        theme.diff.added.patch(theme.overlay)
    } else {
        theme.diff.hunk.patch(theme.overlay)
    };
    RenderedRow {
        line: Line::from_spans(vec![
            Span::styled(
                format!(
                    "   {}─ review thread · rows {}-{} · {comment_count} comment{} ",
                    if collapsed { "▸" } else { "▾" },
                    anchor.source_row,
                    anchor.end_source_row(),
                    if comment_count == 1 { "" } else { "s" }
                ),
                style,
            ),
            Span::styled(
                format!("[{status}]"),
                status_style.add_modifier(Modifier::BOLD),
            ),
        ]),
        style,
    }
}

#[allow(clippy::too_many_lines)]
fn render_view_row(
    app: &ReviewApp,
    view_row: &ReviewViewRow,
    syntax_highlighter: &SyntaxHighlighter,
    can_highlight: bool,
    syntax_hint: &str,
    width: u16,
    theme: ReviewTheme,
) -> RenderedRow {
    match &view_row.block {
        ReviewViewBlock::OmittedContext { .. }
        | ReviewViewBlock::LoadingContext { .. }
        | ReviewViewBlock::UnavailableContext { .. } => {
            render_context_status_view_row(&view_row.block, theme)
        }
        ReviewViewBlock::DisplayRow(display_row) => {
            render_source_view_row(app, view_row, display_row, theme)
        }
        ReviewViewBlock::FileLine {
            line_number,
            content,
        } if matches!(
            view_row.target,
            ReviewViewTarget::ExpandedContextLine { .. }
        ) =>
        {
            render_expanded_context_file_line(
                *line_number,
                content,
                syntax_highlighter,
                can_highlight,
                syntax_hint,
                theme,
            )
        }
        ReviewViewBlock::FileLine {
            line_number,
            content,
        } => render_file_view_row(
            app,
            view_row,
            *line_number,
            content,
            SourceRendering {
                syntax_highlighter,
                can_highlight,
                syntax_hint,
                theme,
            },
        ),
        ReviewViewBlock::InlineThreadHeader {
            anchor,
            comment_count,
            collapsed,
            resolved,
            ..
        } => render_inline_thread_header(anchor, *comment_count, *collapsed, *resolved, theme),
        ReviewViewBlock::InlineComment {
            rendered,
            comment,
            body_line_index,
            body_line_count,
            ..
        } => {
            let style = theme.overlay;
            let label = if *body_line_index == 0 {
                if comment.thread_kind == bcode_code_review_models::ReviewThreadKind::Question {
                    "ask"
                } else if comment.persisted {
                    "draft"
                } else {
                    "saving"
                }
            } else {
                ""
            };
            let branch = if body_line_index.saturating_add(1) == *body_line_count {
                "╰"
            } else {
                "│"
            };
            RenderedRow {
                line: render_inline_comment_line(
                    branch,
                    label,
                    rendered.as_ref(),
                    width,
                    style,
                    theme,
                ),
                style,
            }
        }
        ReviewViewBlock::InlineSuggestion {
            rendered,
            suggestion,
            body_line_index,
            body_line_count,
            ..
        } => {
            let style = match suggestion.status {
                crate::code_review_tui::ReviewSuggestionStatus::Suggested => {
                    theme.diff.added.patch(theme.overlay)
                }
                crate::code_review_tui::ReviewSuggestionStatus::Refining => {
                    theme.focused.patch(theme.overlay)
                }
                crate::code_review_tui::ReviewSuggestionStatus::Accepted => theme
                    .diff
                    .muted
                    .patch(theme.diff.added_row)
                    .patch(theme.overlay),
                crate::code_review_tui::ReviewSuggestionStatus::Rejected => theme
                    .diff
                    .muted
                    .patch(theme.diff.removed_row)
                    .patch(theme.overlay),
            };
            let branch = if body_line_index.saturating_add(1) == *body_line_count {
                "╰"
            } else {
                "│"
            };
            RenderedRow {
                line: render_inline_suggestion_line(
                    branch,
                    suggestion,
                    rendered.as_ref(),
                    *body_line_index,
                    width,
                    style,
                    theme,
                ),
                style,
            }
        }
        ReviewViewBlock::InlineAgentThread { rendered, .. } => {
            let style = theme.focused.patch(theme.overlay);
            RenderedRow {
                line: rendered
                    .as_ref()
                    .map_or_else(Line::default, |row| render_agent_row(row, style, theme)),
                style,
            }
        }
        ReviewViewBlock::InlineThreadAction { action, .. } => {
            render_inline_thread_action(*action, theme)
        }
    }
}

fn render_inline_comment_line(
    branch: &str,
    label: &str,
    rendered: Option<&Line>,
    width: u16,
    style: Style,
    theme: ReviewTheme,
) -> Line {
    let prefix_style = theme.diff.hunk.patch(theme.overlay);
    let prefix = crate::code_review_tui_view::inline_prefix(branch, label, false, width);
    let markdown_line = rendered.cloned().unwrap_or_default();
    let mut spans = vec![Span::styled(prefix, prefix_style)];
    if markdown_line.spans.is_empty() {
        spans.push(Span::styled(String::new(), style));
    } else {
        spans.extend(markdown_line.spans);
    }
    Line::from_spans(spans)
}

fn render_inline_suggestion_line(
    branch: &str,
    suggestion: &crate::code_review_tui::ReviewSuggestedComment,
    rendered: Option<&Line>,
    body_line_index: usize,
    width: u16,
    style: Style,
    theme: ReviewTheme,
) -> Line {
    let prefix_style = theme.diff.added.patch(style);
    let status = match suggestion.status {
        crate::code_review_tui::ReviewSuggestionStatus::Suggested => "suggest",
        crate::code_review_tui::ReviewSuggestionStatus::Refining => "refining",
        crate::code_review_tui::ReviewSuggestionStatus::Accepted => "accepted",
        crate::code_review_tui::ReviewSuggestionStatus::Rejected => "rejected",
    };
    let label = if body_line_index == 0 { status } else { "" };
    let prefix = crate::code_review_tui_view::inline_prefix(branch, label, true, width);
    let mut spans = vec![Span::styled(prefix, prefix_style)];
    if let Some(line) = rendered {
        spans.extend(line.patch_style(style).spans);
    }
    Line::from_spans(spans)
}

fn render_agent_row(
    row: &crate::code_review_tui_view::AgentThreadRow,
    style: Style,
    theme: ReviewTheme,
) -> Line {
    use crate::code_review_tui::ReviewAgentSessionItemKind;
    let prefix_style = match row.kind {
        Some(ReviewAgentSessionItemKind::Assistant) => theme.overlay,
        Some(ReviewAgentSessionItemKind::Reasoning) => theme.muted.patch(theme.overlay),
        Some(ReviewAgentSessionItemKind::ActionNeeded) => theme.diff.hunk.patch(theme.overlay),
        Some(ReviewAgentSessionItemKind::Status) => theme.diff.removed.patch(theme.overlay),
        Some(ReviewAgentSessionItemKind::Tool) | None => theme.focused.patch(theme.overlay),
    };
    let mut spans = vec![Span::styled(row.prefix.clone(), prefix_style)];
    spans.extend(row.content.with_fallback_style(style).spans);
    Line::from_spans(spans)
}

fn render_inline_thread_action(action: ReviewThreadAction, theme: ReviewTheme) -> RenderedRow {
    let style = theme.muted.patch(theme.overlay);
    let shortcut_style = theme.focused.patch(theme.overlay);
    RenderedRow {
        line: Line::from_spans(vec![
            Span::styled("   ├─ [", style),
            Span::styled(
                action.shortcut(),
                shortcut_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("] {}", action.label()), style),
        ]),
        style,
    }
}

fn render_context_status_view_row(block: &ReviewViewBlock, theme: ReviewTheme) -> RenderedRow {
    match block {
        ReviewViewBlock::OmittedContext {
            key,
            hidden_line_count,
            start_line,
            end_line,
        }
        | ReviewViewBlock::LoadingContext {
            key,
            hidden_line_count,
            start_line,
            end_line,
        } => render_omitted_context_row(
            key.direction,
            *hidden_line_count,
            *start_line,
            *end_line,
            None,
            theme,
        ),
        ReviewViewBlock::UnavailableContext {
            key,
            hidden_line_count,
            start_line,
            end_line,
            reason,
        } => render_omitted_context_row(
            key.direction,
            *hidden_line_count,
            *start_line,
            *end_line,
            Some(reason),
            theme,
        ),
        _ => unreachable!("context status renderer only accepts context status blocks"),
    }
}

fn render_source_view_row(
    app: &ReviewApp,
    view_row: &ReviewViewRow,
    display_row: &ReviewDisplayRow,
    theme: ReviewTheme,
) -> RenderedRow {
    let Some(source_row) = view_row.source_row else {
        return render_display_row(display_row, theme);
    };
    let show_comment_affordance = source_row == app.selected_diff_line
        && matches!(
            display_row.source,
            ReviewDisplayRowSource::Added
                | ReviewDisplayRowSource::Removed
                | ReviewDisplayRowSource::Context
        );
    let rendered = render_display_row_with_affordance(show_comment_affordance, display_row, theme);
    let mut line = rendered.line;
    if let Some(marker) = app.range_selection_marker_at(app.selected_file, source_row) {
        line.spans.insert(0, Span::styled(marker, theme.focused));
    }
    if let Some(marker) = app.draft_marker_at(app.selected_file, source_row) {
        line.spans.insert(0, Span::styled(marker, theme.diff.hunk));
    }
    let (line, style) = if source_row == app.selected_diff_line {
        (
            selected_line(&line, theme.selection),
            rendered.style.patch(theme.selection),
        )
    } else if app.is_row_in_range_selection(app.selected_file, source_row) {
        (
            selected_line(&line, theme.focused),
            rendered.style.patch(theme.focused),
        )
    } else {
        (line, rendered.style)
    };
    RenderedRow { line, style }
}

fn render_expanded_context_file_line(
    line_number: Option<u32>,
    content: &str,
    syntax_highlighter: &SyntaxHighlighter,
    can_highlight: bool,
    syntax_hint: &str,
    theme: ReviewTheme,
) -> RenderedRow {
    let row_style = row_style(ReviewDisplayRowSource::Context, theme);
    let old = line_number.map_or_else(|| "    ".to_string(), |line| format!("{line:>4}"));
    let new = line_number.map_or_else(|| "    ".to_string(), |line| format!("{line:>4}"));
    let mut spans = vec![
        Span::styled(format!(" {old} {new} "), row_style.patch(theme.muted)),
        Span::styled(
            " ",
            row_style.patch(marker_style(ReviewDisplayRowSource::Context, theme)),
        ),
    ];
    spans.extend(highlighted_source_spans(
        syntax_highlighter,
        can_highlight,
        syntax_hint,
        content,
        row_style,
    ));
    RenderedRow {
        line: Line::from_spans(spans),
        style: row_style,
    }
}

#[derive(Clone, Copy)]
struct SourceRendering<'a> {
    syntax_highlighter: &'a SyntaxHighlighter,
    can_highlight: bool,
    syntax_hint: &'a str,
    theme: ReviewTheme,
}

fn render_file_view_row(
    app: &ReviewApp,
    view_row: &ReviewViewRow,
    line_number: Option<u32>,
    content: &str,
    rendering: SourceRendering<'_>,
) -> RenderedRow {
    let SourceRendering {
        syntax_highlighter,
        can_highlight,
        syntax_hint,
        theme,
    } = rendering;
    let source_row = view_row.source_row.unwrap_or(view_row.visual_row);
    let mut style = file_viewer_row_style(app, source_row, theme);
    let line_number =
        line_number.map_or_else(|| "      ".to_string(), |number| format!("{number:>5} "));
    let mut spans = vec![Span::styled(line_number.clone(), theme.muted)];
    if line_number.trim().is_empty() {
        spans.push(Span::styled(content.to_string(), style));
    } else {
        if source_row == app.selected_diff_line {
            spans.insert(
                0,
                Span::styled("+", style.patch(theme.focused.add_modifier(Modifier::BOLD))),
            );
        }
        spans.extend(highlighted_source_spans(
            syntax_highlighter,
            can_highlight,
            syntax_hint,
            content,
            style,
        ));
    }
    let mut line = Line::from_spans(spans);
    if let Some(marker) = app.draft_marker_at(app.selected_file, source_row) {
        line.spans.insert(0, Span::styled(marker, theme.diff.hunk));
        style = style.patch(theme.overlay);
    }
    RenderedRow { line, style }
}

fn selected_line(line: &Line, selection: Style) -> Line {
    let mut line = line.clone();
    for span in &mut line.spans {
        span.style = span.style.patch(selection);
    }
    line
}

fn render_empty(area: Rect, text: &str, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    frame.write_line(
        LocalRect::terminal(area),
        &Line::from_spans(vec![Span::styled(format!(" {text}"), theme.muted)]),
    );
}

fn selected_surface_kind(app: &ReviewApp) -> Option<ReviewSurfaceKind> {
    app.review
        .surfaces()
        .get(app.selected_file)
        .map(|surface| surface.kind)
}

fn render_materialized_file_surface(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let Some(file) = app.selected_file_data() else {
        render_empty(area, "No file surface", frame, theme);
        return;
    };
    if file.is_binary {
        render_empty(area, "Binary file content not available", frame, theme);
        return;
    }
    let Some(document) = app.current_review_view_document() else {
        render_empty(area, "No file content", frame, theme);
        return;
    };
    if document.rows.is_empty() {
        render_empty(area, "No file content", frame, theme);
        return;
    }
    render_view_document(app, &document, area, frame, None, theme);
}

#[must_use]
pub fn materialized_file_surface_rows(file: &ReviewFile) -> Vec<(Option<u32>, String)> {
    file.hunks
        .iter()
        .flat_map(|hunk| {
            let heading = hunk
                .heading
                .iter()
                .map(|heading| (None, format!("# {heading}")));
            heading.chain(
                hunk.lines
                    .iter()
                    .map(|line| (line.new_line.or(line.old_line), line.content.clone())),
            )
        })
        .collect()
}

fn render_repository_file(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let Some(path) = app.selected_file_path() else {
        render_empty(area, "No files", frame, theme);
        return;
    };
    let Some(cached) = app.file_cache.get(&path) else {
        render_empty(area, "Loading file…", frame, theme);
        return;
    };
    if let Some(reason) = &cached.unavailable_reason {
        render_empty(area, reason, frame, theme);
        return;
    }
    let Some(document) = app.current_review_view_document() else {
        render_empty(area, "No file content", frame, theme);
        return;
    };
    if document.rows.is_empty() {
        render_empty(area, "No file content", frame, theme);
        return;
    }
    render_view_document(app, &document, area, frame, None, theme);
}

fn file_viewer_row_style(app: &ReviewApp, index: usize, theme: ReviewTheme) -> Style {
    if index == app.selected_diff_line {
        theme.selection
    } else if app.is_row_in_range_selection(app.selected_file, index) {
        theme.focused
    } else if app.has_draft_comment_at(app.selected_file, index) {
        theme.diff.hunk.patch(theme.overlay)
    } else {
        theme.canvas
    }
}

fn highlighted_source_spans(
    syntax_highlighter: &SyntaxHighlighter,
    can_highlight: bool,
    syntax_hint: &str,
    content: &str,
    base_style: Style,
) -> Vec<Span> {
    if !can_highlight {
        return vec![Span::styled(content.to_string(), base_style)];
    }
    syntax_highlighter
        .highlight_line_tokens(syntax_hint, content)
        .into_iter()
        .map(|span| {
            Span::styled(
                span.content,
                base_style.patch(syntax_style_to_tui(span.style)),
            )
        })
        .collect()
}

fn render_display_row_with_affordance(
    show_comment_affordance: bool,
    row: &ReviewDisplayRow,
    theme: ReviewTheme,
) -> RenderedRow {
    let mut rendered = render_display_row(row, theme);
    if show_comment_affordance {
        rendered.line.spans.insert(
            0,
            Span::styled(
                "✎",
                rendered
                    .style
                    .patch(theme.focused.add_modifier(Modifier::BOLD)),
            ),
        );
    }
    rendered
}

fn render_display_row(row: &ReviewDisplayRow, theme: ReviewTheme) -> RenderedRow {
    match row.source {
        ReviewDisplayRowSource::HunkHeader => {
            let style = row_style(row.source, theme);
            RenderedRow {
                line: Line::from_spans(
                    row.segments
                        .iter()
                        .map(|segment| render_display_segment(segment, style, theme))
                        .collect::<Vec<_>>(),
                ),
                style,
            }
        }
        ReviewDisplayRowSource::Context
        | ReviewDisplayRowSource::Added
        | ReviewDisplayRowSource::Removed => {
            let old = row
                .old_line
                .map_or_else(|| "    ".to_string(), |line| format!("{line:>4}"));
            let new = row
                .new_line
                .map_or_else(|| "    ".to_string(), |line| format!("{line:>4}"));
            let row_style = row_style(row.source, theme);
            let marker = row.source.diff_marker().unwrap_or(' ');
            let mut spans = vec![
                Span::styled(format!(" {old} {new} "), row_style.patch(theme.muted)),
                Span::styled(
                    marker.to_string(),
                    row_style.patch(marker_style(row.source, theme).add_modifier(Modifier::BOLD)),
                ),
            ];
            spans.extend(
                row.segments
                    .iter()
                    .map(|segment| render_display_segment(segment, row_style, theme)),
            );
            RenderedRow {
                line: Line::from_spans(spans),
                style: row_style,
            }
        }
    }
}

fn render_display_segment(
    segment: &ReviewDisplaySegment,
    base_style: Style,
    theme: ReviewTheme,
) -> Span {
    Span::styled(
        segment.text.clone(),
        base_style.patch(style_for_segment(segment, theme)),
    )
}

fn style_for_segment(segment: &ReviewDisplaySegment, theme: ReviewTheme) -> Style {
    let mut style = Style::new();
    for role in &segment.roles {
        style = style.patch(style_for_role(role, theme));
    }
    style
}

const fn style_for_role(role: &ReviewDisplayTextRole, theme: ReviewTheme) -> Style {
    match role {
        ReviewDisplayTextRole::Code
        | ReviewDisplayTextRole::DiffAdded
        | ReviewDisplayTextRole::DiffRemoved
        | ReviewDisplayTextRole::DiffContext => Style::new(),
        ReviewDisplayTextRole::Syntax(style) => syntax_style_to_tui(*style),
        ReviewDisplayTextRole::HunkHeader => theme.diff.hunk.add_modifier(Modifier::BOLD),
    }
}

const fn row_style(source: ReviewDisplayRowSource, theme: ReviewTheme) -> Style {
    match source {
        ReviewDisplayRowSource::HunkHeader => theme.diff.hunk.add_modifier(Modifier::BOLD),
        ReviewDisplayRowSource::Context => theme.diff.text,
        ReviewDisplayRowSource::Added => theme.diff.added_row,
        ReviewDisplayRowSource::Removed => theme.diff.removed_row,
    }
}

const fn marker_style(source: ReviewDisplayRowSource, theme: ReviewTheme) -> Style {
    match source {
        ReviewDisplayRowSource::Added => theme.diff.added,
        ReviewDisplayRowSource::Removed => theme.diff.removed,
        ReviewDisplayRowSource::HunkHeader => theme.diff.hunk,
        ReviewDisplayRowSource::Context => theme.diff.muted,
    }
}

const fn syntax_style_to_tui(style: bcode_syntax_render::SyntaxStyle) -> Style {
    let mut output = Style::new().fg(style.foreground.to_tui());
    if style.bold {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        output = output.add_modifier(Modifier::UNDERLINE);
    }
    output
}

fn render_help(_app: &ReviewApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    let width = area.width.min(68);
    let height = 18;
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    let popup = Rect::new(x, y, width, height);
    frame.fill(LocalRect::terminal(popup), " ", theme.overlay);
    let lines = crate::keybindings::hints().unwrap_or_default();
    for (index, text) in lines.iter().enumerate() {
        let y = popup
            .y
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= popup.bottom() {
            break;
        }
        frame.write_line(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(text, usize::from(popup.width.saturating_sub(2))),
                theme.overlay,
            )]),
        );
    }
}

fn legacy_review_modal(
    area: Rect,
    width: u16,
    height: u16,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) -> Rect {
    legacy_review_modal_with_background(area, width, height, frame, theme.overlay, theme)
}

fn legacy_review_modal_with_background(
    area: Rect,
    width: u16,
    height: u16,
    frame: &mut PaintCx<'_, '_>,
    background: Style,
    theme: ReviewTheme,
) -> Rect {
    let modal = ModalFrame::new(
        ModalSizing::fixed(
            bmux_tui::geometry::Size::new(width.min(area.width), height.min(area.height)),
            bmux_tui::geometry::Insets::all(0),
        ),
        ModalTheme::new(
            background,
            background,
            background,
            background,
            theme.muted.patch(background),
            background,
        ),
    )
    .placement(ModalPlacement::Centered)
    .padding(bmux_tui::geometry::Insets::all(0));
    let panel = modal.panel_area(area);
    let component = ModalFrameComponent::new("review.modal", modal, TextBlock::new(""));
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
    panel
}

fn legacy_review_modal_at(
    area: Rect,
    popup: Rect,
    frame: &mut PaintCx<'_, '_>,
    background: Style,
    theme: ReviewTheme,
) -> Rect {
    let modal = ModalFrame::new(
        ModalSizing::fixed(
            bmux_tui::geometry::Size::new(popup.width, popup.height),
            bmux_tui::geometry::Insets::all(0),
        ),
        ModalTheme::new(
            background,
            background,
            background,
            background,
            theme.muted.patch(background),
            background,
        ),
    )
    .placement(ModalPlacement::Anchored(Point::new(popup.x, popup.y)))
    .padding(bmux_tui::geometry::Insets::all(0));
    let panel = modal.panel_area(area);
    let component = ModalFrameComponent::new("review.modal", modal, TextBlock::new(""));
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
    panel
}

fn render_import_modal(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let Some(state) = &app.import_state else {
        return;
    };
    let width = area.width.min(90);
    let height = area.height.min(24);
    if width < 30 || height < 8 {
        return;
    }
    let popup = legacy_review_modal(area, width, height, frame, theme);
    let header = Rect::new(
        popup.x.saturating_add(1),
        popup.y,
        popup.width.saturating_sub(2),
        1,
    );
    match state {
        crate::code_review_tui::ReviewImportState::Picker => {
            frame.write_line(
                LocalRect::terminal(header),
                &Line::from_spans(vec![Span::styled(
                    format!(
                        "Import external review  {}",
                        crate::keybindings::hints().unwrap_or_default().join("  ")
                    ),
                    theme.focused.add_modifier(Modifier::BOLD),
                )]),
            );
            for (row, importer) in app.external_importers().iter().enumerate() {
                let selected = row == app.selected_importer;
                let style = if selected {
                    theme.selection
                } else {
                    theme.overlay
                };
                let line = format!(
                    " {}  {}  {}",
                    importer.label, importer.description, importer.id
                );
                frame.write_line_with_fallback_style(
                    LocalRect::terminal(Rect::new(
                        popup.x.saturating_add(1),
                        popup
                            .y
                            .saturating_add(2)
                            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                        popup.width.saturating_sub(2),
                        1,
                    )),
                    &Line::from_spans(vec![Span::styled(
                        truncate_to_display_width(
                            &line,
                            usize::from(popup.width.saturating_sub(2)),
                        ),
                        style,
                    )]),
                    style,
                );
            }
        }
        crate::code_review_tui::ReviewImportState::Options {
            options, selected, ..
        } => {
            frame.write_line(
                LocalRect::terminal(header),
                &Line::from_spans(vec![Span::styled(
                    format!(
                        "Import options  {}",
                        crate::keybindings::hints().unwrap_or_default().join("  ")
                    ),
                    theme.focused.add_modifier(Modifier::BOLD),
                )]),
            );
            render_publish_options(options, *selected, popup, frame, theme);
        }
    }
}

fn render_publish_modal(
    app: &ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let Some(state) = &app.publish_state else {
        return;
    };
    let width = area.width.min(96);
    let height = area.height.min(24);
    if width < 30 || height < 8 {
        return;
    }
    let popup = legacy_review_modal(area, width, height, frame, theme);
    match state {
        ReviewPublishState::Checklist => render_publish_checklist(app, popup, frame, theme),
        ReviewPublishState::Picker => render_publisher_picker(app, popup, frame, theme),
        ReviewPublishState::Options {
            options, selected, ..
        } => render_publish_options(options, *selected, popup, frame, theme),
        ReviewPublishState::Preview {
            publisher_id,
            preview,
            selection_summary,
            scroll,
            ..
        } => render_publish_preview(
            PublishPreview {
                publisher_id,
                selection_summary,
                preview,
                scroll: *scroll,
                confirming: false,
            },
            popup,
            frame,
            theme,
        ),
        ReviewPublishState::ConfirmSubmit {
            publisher_id,
            preview,
            selection_summary,
            scroll,
            ..
        } => render_publish_preview(
            PublishPreview {
                publisher_id,
                selection_summary,
                preview,
                scroll: *scroll,
                confirming: true,
            },
            popup,
            frame,
            theme,
        ),
    }
}

fn render_publish_checklist(
    app: &ReviewApp,
    popup: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    frame.write_line(
        LocalRect::terminal(Rect::new(
            popup.x.saturating_add(1),
            popup.y,
            popup.width.saturating_sub(2),
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            format!(
                "Publish checklist  {}",
                crate::keybindings::hints().unwrap_or_default().join("  ")
            ),
            theme.selection.add_modifier(Modifier::BOLD),
        )]),
    );
    let checklist_lines = app
        .publish_checklist_lines()
        .into_iter()
        .chain(std::iter::once("threads selected for publish:".to_string()))
        .chain(app.publish_thread_selection_lines());
    for (row, line) in checklist_lines
        .take(usize::from(popup.height.saturating_sub(2)))
        .enumerate()
    {
        let style = if line.starts_with('!') {
            theme.diff.hunk.patch(theme.overlay)
        } else if line.starts_with('✓') {
            theme.diff.added.patch(theme.overlay)
        } else {
            theme.overlay
        };
        let y = popup
            .y
            .saturating_add(1 + u16::try_from(row).unwrap_or(u16::MAX));
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(
                    &format!(" {line}"),
                    usize::from(popup.width.saturating_sub(2)),
                ),
                style,
            )]),
            style,
        );
    }
}

fn render_publisher_picker(
    app: &ReviewApp,
    popup: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    frame.write_line(
        LocalRect::terminal(Rect::new(
            popup.x.saturating_add(1),
            popup.y,
            popup.width.saturating_sub(2),
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            format!(
                "Publish review  {}",
                crate::keybindings::hints().unwrap_or_default().join("  ")
            ),
            theme.focused.add_modifier(Modifier::BOLD),
        )]),
    );
    let rows = usize::from(popup.height.saturating_sub(2));
    for row in 0..rows {
        let Some(publisher) = app.publishers.get(row) else {
            break;
        };
        let selected = row == app.selected_publisher;
        let style = if selected {
            theme.selection
        } else {
            theme.overlay
        };
        let caps = publisher.capability_labels().join(",");
        let text = format!(
            " {}  {}  [{}]",
            publisher.label, publisher.description, caps
        );
        let y = popup
            .y
            .saturating_add(1 + u16::try_from(row).unwrap_or(u16::MAX));
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&text, usize::from(popup.width.saturating_sub(2))),
                style,
            )]),
            style,
        );
    }
}

fn render_publish_options(
    options: &[crate::code_review_tui::ReviewPublishOption],
    selected: usize,
    popup: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    frame.write_line(
        LocalRect::terminal(Rect::new(
            popup.x.saturating_add(1),
            popup.y,
            popup.width.saturating_sub(2),
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            format!(
                "Publisher options  {}",
                crate::keybindings::hints().unwrap_or_default().join("  ")
            ),
            theme.focused.add_modifier(Modifier::BOLD),
        )]),
    );
    let rows = usize::from(popup.height.saturating_sub(2));
    for (row, option) in options.iter().take(rows).enumerate() {
        let style = if row == selected {
            theme.selection
        } else {
            theme.overlay
        };
        let text = if option.choices.is_empty() {
            format!(" {}: {}", option.label, option.value)
        } else {
            format!(
                " {}: {}  ‹{}›",
                option.label,
                option.value,
                option.choices.join("/")
            )
        };
        let y = popup
            .y
            .saturating_add(1 + u16::try_from(row).unwrap_or(u16::MAX));
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&text, usize::from(popup.width.saturating_sub(2))),
                style,
            )]),
            style,
        );
    }
}

#[derive(Clone, Copy)]
struct PublishPreview<'a> {
    publisher_id: &'a str,
    selection_summary: &'a [String],
    preview: &'a str,
    scroll: usize,
    confirming: bool,
}

fn render_publish_preview(
    preview: PublishPreview<'_>,
    popup: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let PublishPreview {
        publisher_id,
        selection_summary,
        preview,
        scroll,
        confirming,
    } = preview;
    frame.write_line(
        LocalRect::terminal(Rect::new(
            popup.x.saturating_add(1),
            popup.y,
            popup.width.saturating_sub(2),
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            if confirming {
                format!(
                    "Confirm submit {publisher_id}  {}",
                    crate::keybindings::hints().unwrap_or_default().join("  ")
                )
            } else {
                format!(
                    "Preview {publisher_id}  {}",
                    crate::keybindings::hints().unwrap_or_default().join("  ")
                )
            },
            theme.focused.add_modifier(Modifier::BOLD),
        )]),
    );
    let rows = usize::from(popup.height.saturating_sub(2));
    let content = selection_summary
        .iter()
        .cloned()
        .chain(std::iter::once("--- provider preview ---".to_string()))
        .chain(preview.lines().map(ToString::to_string))
        .collect::<Vec<_>>();
    for (row, line) in content.iter().skip(scroll).take(rows).enumerate() {
        let y = popup
            .y
            .saturating_add(1 + u16::try_from(row).unwrap_or(u16::MAX));
        frame.write_line(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(line, usize::from(popup.width.saturating_sub(2))),
                theme.overlay,
            )]),
        );
    }
    if confirming {
        let warning = " This will publish the review. Activate the configured submit action again to publish. ";
        frame.write_line(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                popup.bottom().saturating_sub(1),
                popup.width.saturating_sub(2),
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(warning, usize::from(popup.width.saturating_sub(2))),
                theme.diff.hunk.patch(theme.overlay),
            )]),
        );
    }
}

fn render_comment_editor(
    app: &mut ReviewApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let editor = app.comment_editor.clone();
    let Some(editor) = editor.as_ref() else {
        return;
    };
    let width = area.width.min(72);
    let height = area.height.min(10);
    if width < 20 || height < 5 {
        return;
    }
    let diff_area = app.diff_area();
    let x = diff_area.map_or_else(
        || area.x.saturating_add(area.width.saturating_sub(width) / 2),
        |diff_area| {
            diff_area
                .x
                .saturating_add(2)
                .min(area.right().saturating_sub(width))
        },
    );
    let y = comment_editor_y(app, area, height);
    let popup = legacy_review_modal_at(
        area,
        Rect::new(x, y, width, height),
        frame,
        theme.overlay,
        theme,
    );
    render_comment_editor_header(editor, popup, frame, theme);
    let text_height = usize::from(height.saturating_sub(4));
    let geometry = render_comment_editor_body(editor, popup, text_height, frame, theme);
    if editor.buffer.text().is_empty() {
        frame.write_line(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                popup.y.saturating_add(2),
                popup.width.saturating_sub(2),
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                "write a review comment or question...",
                theme.muted.patch(theme.overlay),
            )]),
        );
    }
    render_comment_editor_footer(app, editor, popup, frame, theme);
    if let Some((content_rect, anchor)) = geometry {
        app.comment_input_visible = true;
        app.observe_comment_input();
        if let Some(live_editor) = &mut app.comment_editor {
            live_editor
                .focused_input
                .painted(&live_editor.buffer, content_rect, anchor);
        }
    }
}

fn comment_editor_title(editor: &crate::code_review_tui::ReviewCommentEditor) -> String {
    let action = editor.action.label();
    if matches!(
        editor.mode,
        crate::code_review_tui::ReviewCommentEditorMode::Edit { .. }
    ) {
        return if editor.preview {
            " Edit draft preview ".to_string()
        } else {
            " Edit draft comment ".to_string()
        };
    }
    if editor.preview {
        return if editor.existing_comment_count > 0 {
            format!(" Reply preview · {action} ")
        } else {
            format!(" Review preview · {action} ")
        };
    }
    if editor.existing_comment_count > 0 {
        format!(" Reply to review thread · {action} ")
    } else {
        format!(" Review composer · {action} ")
    }
}

fn render_comment_editor_header(
    editor: &crate::code_review_tui::ReviewCommentEditor,
    popup: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let title = comment_editor_title(editor);
    frame.write_line(
        LocalRect::terminal(Rect::new(popup.x, popup.y, popup.width, 1)),
        &Line::from_spans(vec![Span::styled(
            title,
            theme.focused.add_modifier(Modifier::BOLD),
        )]),
    );
    let anchor = format!(
        " {}:{}{}{} ",
        editor.anchor.path,
        editor
            .anchor
            .new_line
            .or(editor.anchor.old_line)
            .map_or_else(|| "?".to_string(), |line| line.to_string()),
        match editor.anchor.line_kind {
            ReviewLineKind::Added => " +",
            ReviewLineKind::Removed => " -",
            ReviewLineKind::Context => "",
        },
        if editor.existing_comment_count > 0 {
            format!(
                " · replying to {} draft{}",
                editor.existing_comment_count,
                if editor.existing_comment_count == 1 {
                    ""
                } else {
                    "s"
                }
            )
        } else {
            String::new()
        }
    );
    frame.write_line(
        LocalRect::terminal(Rect::new(
            popup.x.saturating_add(1),
            popup.y.saturating_add(1),
            popup.width.saturating_sub(2),
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(&anchor, usize::from(popup.width.saturating_sub(2))),
            theme.muted.patch(theme.overlay),
        )]),
    );
}

fn render_comment_editor_footer(
    app: &ReviewApp,
    editor: &crate::code_review_tui::ReviewCommentEditor,
    popup: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let group = if matches!(
        editor.mode,
        crate::code_review_tui::ReviewCommentEditorMode::Create
    ) {
        ReviewActionGroup::CreateComment
    } else {
        ReviewActionGroup::EditComment
    };
    app.controls
        .lock()
        .expect("review controls lock")
        .sync_comment_action(group, editor.action);
    paint_review_controls(
        app,
        ControlSurface::Actions(group),
        Rect::new(
            popup.x.saturating_add(1),
            popup.bottom().saturating_sub(1),
            popup.width.saturating_sub(2),
            u16::from(popup.height > 0),
        ),
        frame,
        theme,
    );
}

fn comment_editor_y(app: &ReviewApp, area: Rect, height: u16) -> u16 {
    let Some(diff_area) = app.diff_area() else {
        return area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2);
    };
    let selected_visual_row = app.selected_visual_row();
    let visible_offset = selected_visual_row.saturating_sub(app.diff_scroll);
    let anchor_y = diff_area
        .y
        .saturating_add(u16::try_from(visible_offset).unwrap_or(u16::MAX))
        .saturating_add(1);
    let max_y = area.bottom().saturating_sub(height);
    anchor_y.min(max_y).max(area.y)
}

fn render_comment_editor_body(
    editor: &crate::code_review_tui::ReviewCommentEditor,
    popup: Rect,
    text_height: usize,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) -> Option<(Rect, Rect)> {
    if editor.preview {
        render_comment_editor_preview(editor, popup, text_height, frame, theme);
        None
    } else {
        render_comment_editor_text(editor, popup, text_height, frame, theme)
    }
}

fn render_comment_editor_preview(
    editor: &crate::code_review_tui::ReviewCommentEditor,
    popup: Rect,
    text_height: usize,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let preview_width = popup.width.saturating_sub(2).max(1);
    for (index, line) in render_markdown_lines(
        editor.buffer.text(),
        MarkdownRenderOptions::new(preview_width),
    )
    .into_iter()
    .take(text_height)
    .enumerate()
    {
        let mut spans = line.spans;
        if spans.is_empty() {
            spans.push(Span::styled(String::new(), theme.overlay));
        }
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                popup
                    .y
                    .saturating_add(2)
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                preview_width,
                1,
            )),
            &Line::from_spans(spans),
            theme.overlay,
        );
    }
}

fn render_comment_editor_text(
    editor: &crate::code_review_tui::ReviewCommentEditor,
    popup: Rect,
    text_height: usize,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) -> Option<(Rect, Rect)> {
    let content = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(2),
        popup.width.saturating_sub(2),
        u16::try_from(text_height).unwrap_or(u16::MAX),
    );
    if content.is_empty() {
        return None;
    }
    let wrapped = editor.buffer.wrapped_layout(usize::from(content.width));
    let input = bmux_tui::input::TextInput::new(&editor.buffer)
        .wrapped_layout(&wrapped)
        .style(theme.overlay);
    let layout = input.layout(Constraints::tight(content.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(content.x),
        i64::from(content.y),
        LocalRect::new(0, 0, content.width, content.height),
        |cx| input.paint(&layout, cx),
    );
    // The same retained BMUX projection drives painting and completion geometry.
    // An offscreen or right-edge caret is deliberately ineligible.
    if wrapped.cursor.row >= usize::from(content.height)
        || wrapped.cursor.col >= usize::from(content.width)
    {
        return None;
    }
    Some((
        content,
        Rect::new(
            content
                .x
                .saturating_add(u16::try_from(wrapped.cursor.col).ok()?),
            content
                .y
                .saturating_add(u16::try_from(wrapped.cursor.row).ok()?),
            1,
            1,
        ),
    ))
}

fn prompt_popup_height(kind: &ReviewPromptKind, area: Rect) -> u16 {
    match kind {
        ReviewPromptKind::FilePicker
        | ReviewPromptKind::AddSourceKind
        | ReviewPromptKind::AddAdvancedSourceKind
        | ReviewPromptKind::AddFileSourcePicker
        | ReviewPromptKind::AddFileRangePathPicker
        | ReviewPromptKind::AddCommitPicker
        | ReviewPromptKind::AddCommitRangeBasePicker
        | ReviewPromptKind::AddCommitRangeHeadPicker { .. }
        | ReviewPromptKind::AddBranchCompareBasePicker
        | ReviewPromptKind::AddBranchCompareHeadPicker { .. } => area.height.min(16),
        ReviewPromptKind::JumpToLine
        | ReviewPromptKind::FileSearch
        | ReviewPromptKind::AddCommitSource
        | ReviewPromptKind::AddCommitRangeSource
        | ReviewPromptKind::AddBranchCompareSource
        | ReviewPromptKind::AddFileRangeSource
        | ReviewPromptKind::RenameWorkspace
        | ReviewPromptKind::EditSourceSpec { .. }
        | ReviewPromptKind::RenameSource => area.height.min(5),
    }
}

const fn prompt_title(kind: &ReviewPromptKind) -> &'static str {
    match kind {
        ReviewPromptKind::FilePicker => " Open file ",
        ReviewPromptKind::JumpToLine => " Jump to line ",
        ReviewPromptKind::FileSearch => " Search file ",
        ReviewPromptKind::AddSourceKind => " Add source ",
        ReviewPromptKind::AddAdvancedSourceKind => " Add advanced source ",
        ReviewPromptKind::AddCommitPicker => " Pick commit ",
        ReviewPromptKind::AddCommitSource => " Add commit ",
        ReviewPromptKind::AddCommitRangeBasePicker => " Pick base commit ",
        ReviewPromptKind::AddCommitRangeHeadPicker { .. } => " Pick head commit ",
        ReviewPromptKind::AddCommitRangeSource => " Add range ",
        ReviewPromptKind::AddBranchCompareBasePicker => " Pick base branch ",
        ReviewPromptKind::AddBranchCompareHeadPicker { .. } => " Pick head branch ",
        ReviewPromptKind::AddBranchCompareSource => " Add branch compare ",
        ReviewPromptKind::AddFileSourcePicker => " Add file source ",
        ReviewPromptKind::AddFileRangePathPicker | ReviewPromptKind::AddFileRangeSource => {
            " Add file range "
        }
        ReviewPromptKind::RenameWorkspace => " Rename workspace ",
        ReviewPromptKind::EditSourceSpec { .. } => " Edit source ",
        ReviewPromptKind::RenameSource => " Rename source ",
    }
}

fn render_add_source_menu(
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    render_source_menu_items(prompt, add_source_menu_items(), popup, height, frame, theme);
}

fn render_advanced_source_menu(
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    render_source_menu_items(
        prompt,
        advanced_source_menu_items(),
        popup,
        height,
        frame,
        theme,
    );
}

fn render_source_menu_items(
    prompt: &crate::code_review_tui::ReviewPromptState,
    items: &[crate::code_review_tui::AddSourceMenuItem],
    popup: Rect,
    height: u16,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    for (row, item) in items
        .iter()
        .take(usize::from(height.saturating_sub(3)))
        .enumerate()
    {
        let style = if row == prompt.selected {
            theme.selection
        } else {
            theme.overlay
        };
        let text = format!(" {:<16} {}", item.label, item.help);
        frame.write_line(
            LocalRect::terminal(Rect::new(
                popup.x.saturating_add(1),
                popup
                    .y
                    .saturating_add(2 + u16::try_from(row).unwrap_or(u16::MAX)),
                popup.width.saturating_sub(2),
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&text, usize::from(popup.width.saturating_sub(2))),
                style,
            )]),
        );
    }
}

fn render_prompt(app: &ReviewApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: ReviewTheme) {
    let Some(prompt) = &app.prompt_state else {
        return;
    };
    let width = area.width.min(80);
    let height = prompt_popup_height(&prompt.kind, area);
    if width < 20 || height < 3 {
        return;
    }
    let popup =
        legacy_review_modal_with_background(area, width, height, frame, theme.overlay, theme);
    let title = prompt_title(&prompt.kind);
    frame.write_line(
        LocalRect::terminal(Rect::new(popup.x, popup.y, popup.width, 1)),
        &Line::from_spans(vec![Span::styled(
            title,
            theme
                .selection
                .patch(Style::new().add_modifier(Modifier::BOLD)),
        )]),
    );
    let query = prompt.buffer.text();
    frame.write_line(
        LocalRect::terminal(Rect::new(
            popup.x.saturating_add(1),
            popup.y.saturating_add(1),
            popup.width.saturating_sub(2),
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(query, usize::from(popup.width.saturating_sub(2))),
            theme.overlay,
        )]),
    );
    match &prompt.kind {
        ReviewPromptKind::AddSourceKind => {
            render_add_source_menu(prompt, popup, height, frame, theme);
        }
        ReviewPromptKind::AddAdvancedSourceKind => {
            render_advanced_source_menu(prompt, popup, height, frame, theme);
        }
        ReviewPromptKind::AddFileSourcePicker | ReviewPromptKind::AddFileRangePathPicker => {
            render_add_repository_file_picker(app, prompt, popup, height, query, frame, theme);
        }
        ReviewPromptKind::AddCommitPicker
        | ReviewPromptKind::AddCommitRangeBasePicker
        | ReviewPromptKind::AddCommitRangeHeadPicker { .. } => {
            render_add_repository_commit_picker(app, prompt, popup, height, query, frame, theme);
        }
        ReviewPromptKind::AddBranchCompareBasePicker
        | ReviewPromptKind::AddBranchCompareHeadPicker { .. } => {
            render_add_repository_branch_picker(app, prompt, popup, height, query, frame, theme);
        }
        ReviewPromptKind::FilePicker => {
            render_file_picker(app, prompt, popup, height, query, frame, theme);
        }
        _ => {}
    }
    frame.write_line(
        LocalRect::terminal(Rect::new(
            popup.x.saturating_add(1),
            popup.bottom().saturating_sub(1),
            popup.width.saturating_sub(2),
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            prompt_footer_text(&prompt.kind),
            theme.selection,
        )]),
    );
}

fn render_add_repository_file_picker(
    app: &ReviewApp,
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    query: &str,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    for (row, path) in app
        .repository_file_picker_matches(query)
        .into_iter()
        .take(usize::from(height.saturating_sub(3)))
        .enumerate()
    {
        render_prompt_choice(row, prompt.selected, popup, &path, frame, theme);
    }
}

fn render_add_repository_commit_picker(
    app: &ReviewApp,
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    query: &str,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    for (row, commit) in app
        .repository_commit_picker_matches(query)
        .into_iter()
        .take(usize::from(height.saturating_sub(3)))
        .enumerate()
    {
        render_prompt_choice(
            row,
            prompt.selected,
            popup,
            &format!("{} {}", commit.short_rev, commit.subject),
            frame,
            theme,
        );
    }
}

fn render_add_repository_branch_picker(
    app: &ReviewApp,
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    query: &str,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    for (row, branch) in app
        .repository_branch_picker_matches(query)
        .into_iter()
        .take(usize::from(height.saturating_sub(3)))
        .enumerate()
    {
        render_prompt_choice(row, prompt.selected, popup, &branch, frame, theme);
    }
}

fn render_file_picker(
    app: &ReviewApp,
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    query: &str,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    for (row, index) in app
        .file_picker_matches(query)
        .into_iter()
        .take(usize::from(height.saturating_sub(3)))
        .enumerate()
    {
        render_prompt_choice(
            row,
            prompt.selected,
            popup,
            app.review.files[index].display_path(),
            frame,
            theme,
        );
    }
}

fn render_prompt_choice(
    row: usize,
    selected: usize,
    popup: Rect,
    text: &str,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewTheme,
) {
    let style = if row == selected {
        theme.selection
    } else {
        theme.overlay
    };
    frame.write_line(
        LocalRect::terminal(Rect::new(
            popup.x.saturating_add(1),
            popup
                .y
                .saturating_add(2 + u16::try_from(row).unwrap_or(u16::MAX)),
            popup.width.saturating_sub(2),
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(text, usize::from(popup.width.saturating_sub(2))),
            style,
        )]),
    );
}

fn prompt_footer_text(_kind: &ReviewPromptKind) -> String {
    crate::keybindings::hints().unwrap_or_default().join("  ")
}

struct RenderedRow {
    line: Line,
    style: Style,
}

#[cfg(test)]
mod tests {
    use crate::code_review_tui::ReviewFileStatus;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::text_width::display_width;

    use super::*;

    fn render_app_text_with_theme(
        app: &mut ReviewApp,
        width: u16,
        height: u16,
        theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) -> String {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        render(app, &mut PaintCx::new(&mut frame), theme);
        (0..buffer.area().height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_app_text(app: &mut ReviewApp, width: u16, height: u16) -> String {
        render_app_text_with_theme(app, width, height, None)
    }

    #[test]
    fn navigation_narrow_sidebar_prioritizes_filename_over_source_metadata() {
        let mut review = crate::code_review_tui::tests::sample_app().review;
        review.files.truncate(1);
        review.files[0].new_path = Some("src/scroll_anchor.rs".to_string());
        let mut workspace = review.workspace();
        workspace.sources[0].label = "Staged changes".to_string();
        let source_id = workspace.sources[0].id.clone();
        let expected_identity = format!("surface:{source_id}:staged-scroll");
        review.surfaces = vec![bcode_code_review_models::ReviewSurface {
            id: "staged-scroll".to_string(),
            source_id,
            path: "src/scroll_anchor.rs".to_string(),
            kind: ReviewSurfaceKind::Diff,
            file: None,
        }];
        review.workspace = Some(workspace);
        let mut app = ReviewApp::new(review);
        for width in [28, 34, 80] {
            let area = Rect::new(0, 0, width, 3);
            let mut buffer = Buffer::empty(area);
            let mut frame = Frame::new(&mut buffer);
            render_files(
                &mut app,
                area,
                &mut PaintCx::new(&mut frame),
                ReviewTheme::default(),
            );
            let filename_row = buffer.row_symbols(1).unwrap();
            assert!(
                filename_row.contains("scroll_anchor.rs"),
                "filename must remain useful at sidebar width {width}: {filename_row}"
            );
            if width == 80 {
                assert!(filename_row.contains("[Staged changes]"));
                assert!(
                    filename_row.find("scroll_anchor.rs").unwrap()
                        < filename_row.find("[Staged changes]").unwrap()
                );
            }
            assert_eq!(
                app.selected_file_path().as_deref(),
                Some("src/scroll_anchor.rs")
            );
            assert_eq!(app.navigation_items[1].id, expected_identity);
        }
        let text = render_app_text(&mut app, 112, 30);
        assert!(
            text.contains("src/scroll_anchor.rs"),
            "selected path remains available outside the clipped sidebar"
        );
    }

    #[test]
    fn navigation_nested_boundary_expansion_keeps_native_markers_and_scroll_current() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut review = crate::code_review_tui::tests::sample_app().review;
        let template = review.files[0].clone();
        review.files = (0..512)
            .map(|index| {
                let mut file = template.clone();
                file.new_path = Some(format!("dir-{index:04}/nested/界-e\u{301}.rs"));
                file
            })
            .collect();
        let mut app = ReviewApp::new(review);
        for index in 0..512 {
            let root = std::path::PathBuf::from(format!("dir-{index:04}"));
            if index != 0 {
                app.toggle_file_tree_directory(&root);
                app.toggle_file_tree_directory(&root.join("nested"));
            }
        }
        let target = std::path::PathBuf::from("dir-0256/nested");
        let target_row = app
            .file_tree_rows()
            .iter()
            .position(|row| {
                matches!(row,
            crate::code_review_tui::ReviewFileTreeRow::Directory { path, .. } if path == &target)
            })
            .unwrap();
        let area = Rect::new(2, 1, 36, 2);
        app.set_file_area(Some(area));
        app.tree_scroll = target_row - 1;
        let paint = |app: &mut ReviewApp| {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 6));
            let mut frame = Frame::new(&mut buffer);
            render_files(
                app,
                area,
                &mut PaintCx::new(&mut frame),
                ReviewTheme::default(),
            );
            assert!(
                frame
                    .hits()
                    .regions()
                    .iter()
                    .all(|hit| hit.area.bottom() <= area.bottom())
            );
            buffer.row_symbols(area.y + 1).unwrap()
        };
        assert!(paint(&mut app).contains('▾'));
        let before_items = app.navigation_items.clone();
        let target_position = Point::new(area.x, area.y + 1);
        app.handle_navigation_pointer(MouseEvent::new(MouseEventKind::Move, target_position));
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle_navigation_pointer(MouseEvent::new(kind, target_position));
        }
        assert!(paint(&mut app).contains('▸'));
        assert_eq!(
            app.navigation_items, before_items,
            "children changed only below viewport"
        );
        assert_eq!(app.tree_scroll, target_row - 1);
        assert!(
            !app.navigation_pointer_state
                .is_expanded("directory:dir-0256/nested")
        );
        let cached_model = app.navigation_items();
        let cached_visible = app.navigation_visible_indices(&cached_model.0);
        for _ in 0..4 {
            assert!(paint(&mut app).contains('▸'));
            assert!(std::sync::Arc::ptr_eq(
                &cached_visible,
                &app.navigation_visible_indices(&cached_model.0)
            ));
            assert!(
                app.navigation_pointer_state.expanded_ids().len() <= 2,
                "viewport state must not contain all expanded directories"
            );
        }
        assert!(app.expand_selected_tree_row());
        assert!(paint(&mut app).contains('▾'));
        assert_eq!(app.tree_scroll, target_row - 1);
        assert!(app.select_next_tree_row(1));
        assert_eq!(app.tree_scroll, target_row);
        paint(&mut app);
        assert!(matches!(
            app.navigation_visible_rows.last(),
            Some(crate::code_review_tui::ReviewFileTreeRow::File { index: 256, .. })
        ));
        assert!(app.collapse_selected_tree_row());
        paint(&mut app);
        assert_eq!(app.selected_tree_row, target_row);
        assert!(app.navigation_items.len() <= usize::from(area.height));
    }

    #[test]
    fn navigation_viewport_beyond_terminal_row_range_remains_bounded() {
        let mut review = crate::code_review_tui::tests::sample_app().review;
        review.files.clear();
        review.surfaces = (0..70_000)
            .map(|index| bcode_code_review_models::ReviewSurface {
                id: format!("surface-{index}"),
                source_id: "source-1".to_string(),
                path: format!("file-{index:05}.rs"),
                kind: ReviewSurfaceKind::File,
                file: None,
            })
            .collect();
        let mut app = ReviewApp::new(review);
        app.tree_scroll = 68_000;
        let area = Rect::new(0, 0, 24, 2);
        app.set_file_area(Some(area));
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render_files(
            &mut app,
            area,
            &mut PaintCx::new(&mut frame),
            ReviewTheme::default(),
        );
        assert_eq!(app.tree_scroll, 68_000);
        assert_eq!(app.navigation_items.len(), 2);
        assert_eq!(
            app.navigation_visible_rows[0],
            crate::code_review_tui::ReviewFileTreeRow::File {
                index: 68_000,
                depth: 0
            }
        );
        assert!(frame.hits().regions().len() <= 3);
    }

    #[test]
    fn navigation_tiny_unicode_resize_pointer_and_wheel_are_pane_local() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut review = crate::code_review_tui::tests::sample_app().review;
        let template = review.files[0].clone();
        review.files = (0..100)
            .map(|index| {
                let mut file = template.clone();
                file.new_path = Some(format!("界/👩‍💻/e\u{301}-{index:03}.rs"));
                file
            })
            .collect();
        let mut app = ReviewApp::new(review);
        app.expand_selected_file_dirs();
        for width in [0, 1, 2, 3, 9, 40, 2, 40] {
            let area = Rect::new(1, 1, width, 3);
            app.set_file_area(Some(area));
            app.set_diff_area(Rect::new(45, 1, 40, 8));
            let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 12));
            let mut frame = Frame::new(&mut buffer);
            render_files(
                &mut app,
                area,
                &mut PaintCx::new(&mut frame),
                ReviewTheme::default(),
            );
            assert!(frame.hits().regions().iter().all(|hit| hit.area.x >= area.x
                && hit.area.right() <= area.right()
                && hit.area.y >= area.y
                && hit.area.bottom() <= area.bottom()));
        }
        let diff_scroll = app.diff_scroll;
        assert!(app.scroll_files_down(20));
        let scroll = app.tree_scroll;
        let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 12));
        let mut frame = Frame::new(&mut buffer);
        let area = Rect::new(1, 1, 40, 3);
        render_files(
            &mut app,
            area,
            &mut PaintCx::new(&mut frame),
            ReviewTheme::default(),
        );
        assert_eq!(
            app.tree_scroll, scroll,
            "paint must not undo independent wheel scrolling"
        );
        assert_eq!(app.diff_scroll, diff_scroll);
        assert!(app.navigation_items.len() <= 3);
        let target = app.navigation_visible_rows[0].clone();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle_navigation_pointer(MouseEvent::new(kind, Point::new(area.x, area.y)));
        }
        if let crate::code_review_tui::ReviewFileTreeRow::File { index, .. } = target {
            assert_eq!(app.selected_file, index);
        } else {
            panic!("scrolled viewport should start with a file");
        }
    }

    #[test]
    fn painted_controls_are_live_and_app_stays_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ReviewApp>();
        let mut app = crate::code_review_tui::tests::sample_app();
        app.sidebar_mode = ReviewSidebarMode::Included;
        render_app_text(&mut app, 140, 40);
        let routed = app
            .controls
            .lock()
            .expect("controls")
            .route(&bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Tab),
            ));
        assert_eq!(routed, Some((true, None)));
        let routed = app
            .controls
            .lock()
            .expect("controls")
            .route(&bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Right),
            ));
        assert_eq!(
            routed.and_then(|(_, action)| action),
            Some(ReviewMouseAction::SidebarMode(
                ReviewSidebarMode::Repository
            ))
        );
        render_app_text(&mut app, 0, 0);
        assert_eq!(
            app.controls
                .lock()
                .expect("controls")
                .route(&bmux_tui::event::Event::Mouse(
                    bmux_tui::event::MouseEvent::new(
                        bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                        Point::new(1, 1)
                    )
                )),
            None
        );
    }

    #[test]
    fn host_theme_controls_help_overlay_surface() {
        let mut app = crate::code_review_tui::tests::sample_app();
        app.help_visible = true;
        let mut theme = terminal_native_theme();
        theme.canvas = Style::new().bg(Color::Blue);
        theme.text = Style::new().fg(Color::BrightWhite);
        let theme = theme.with_legacy_component();

        let area = Rect::new(0, 0, 100, 24);
        let mut buffer = Buffer::empty(area);
        render(
            &mut app,
            &mut PaintCx::new(&mut Frame::new(&mut buffer)),
            Some(&theme),
        );
        let popup = Rect::new(16, 3, 68, 18);

        assert_eq!(
            buffer
                .get(Point::new(popup.x, popup.y))
                .expect("help overlay")
                .style
                .bg,
            Some(Color::Blue)
        );
        assert!(
            buffer
                .get(Point::new(popup.x, popup.y))
                .expect("help text")
                .style
                .fg
                .is_none_or(|color| color == Color::BrightWhite)
        );
    }

    fn terminal_native_theme() -> bcode_plugin_sdk::tui::PluginTuiTheme {
        let default_style = Style::new();
        let syntax_color = bcode_plugin_sdk::tui::PluginTuiSyntaxColor::rgb(12, 34, 56);
        bcode_plugin_sdk::tui::PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas: default_style,
            text: default_style,
            muted: default_style,
            border: default_style,
            focused: default_style,
            selection: default_style.add_modifier(Modifier::REVERSED),
            source: bcode_plugin_sdk::tui::PluginTuiSourceTheme {
                source: default_style,
                border: default_style,
                gutter: default_style,
                truncated: default_style,
            },
            diff: bcode_plugin_sdk::tui::PluginTuiDiffTheme {
                text: default_style,
                muted: default_style,
                title: default_style,
                label: default_style,
                added: default_style,
                removed: default_style,
                hunk: default_style,
                added_row: default_style,
                removed_row: default_style,
                added_emphasis: default_style,
                removed_emphasis: default_style,
            },
            syntax: bcode_plugin_sdk::tui::PluginTuiSyntaxTheme {
                text: syntax_color,
                comment: syntax_color,
                keyword: syntax_color,
                function: syntax_color,
                variable: syntax_color,
                string: syntax_color,
                number: syntax_color,
                type_name: syntax_color,
                operator: syntax_color,
                punctuation: syntax_color,
                heading: syntax_color,
                link: syntax_color,
                raw: syntax_color,
            },
            component: bcode_plugin_sdk::tui::ComponentTheme::terminal_default(),
        }
        .with_legacy_component()
    }

    #[test]
    fn terminal_native_theme_does_not_force_black_canvas() {
        let mut app = crate::code_review_tui::tests::sample_app();
        let theme = terminal_native_theme();

        let rendered = render_app_text_with_theme(&mut app, 100, 24, Some(&theme));
        assert!(rendered.contains("bcode review"));
    }

    #[test]
    fn rendered_tui_smoke_covers_navigation_composer_and_lifecycle_states() {
        let mut app = crate::code_review_tui::tests::sample_app();
        app.selected_diff_line = 2;
        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("composer")
            .buffer
            .insert_str("draft");
        assert!(app.save_comment_editor());
        let anchor = app.selected_comment_anchor().expect("anchor");
        let exchange_id = app.create_ai_exchange(
            anchor.clone(),
            "question".to_string(),
            Some("session-1".to_string()),
        );
        app.mark_thread_session(&exchange_id, &anchor, "session-1");
        app.draft_comments
            .get_mut(&anchor)
            .expect("draft thread")
            .last_mut()
            .expect("draft")
            .session_id = Some("session-1".to_string());
        app.suggested_comments
            .entry(anchor.clone())
            .or_default()
            .push(crate::code_review_tui::ReviewSuggestedComment {
                id: "suggestion".to_string(),
                body: "suggestion".to_string(),
                session_id: Some("session-1".to_string()),
                status: crate::code_review_tui::ReviewSuggestionStatus::Suggested,
                rationale: None,
                created_at_ms: None,
                updated_at_ms: None,
            });
        app.record_published_thread_for_test(&anchor, "github");
        app.sidebar_mode = crate::code_review_tui::ReviewSidebarMode::Threads;

        let rendered = render_app_text(&mut app, 180, 42);

        for expected in ["open", "draft:1", "AI-exchange", "suggestion"] {
            assert!(
                rendered.contains(expected),
                "missing rendered state {expected}"
            );
        }
        let thread = app
            .thread_summaries()
            .into_iter()
            .find(|thread| thread.anchor == anchor)
            .expect("rendered thread");
        assert!(
            thread_sidebar_state_tokens(&app, &thread)
                .iter()
                .any(|token| token == "linked-session")
        );
        assert!(
            thread_sidebar_state_tokens(&app, &thread)
                .iter()
                .any(|token| token == "published")
        );

        assert!(app.open_comment_editor());
        app.comment_editor.as_mut().expect("composer").action =
            crate::code_review_tui::ReviewCommentAction::AskBcode;
        assert_painted_composer_ask(&mut app);
    }

    fn assert_painted_composer_ask(app: &mut ReviewApp) {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 42));
        let mut frame = Frame::new(&mut buffer);
        render(app, &mut PaintCx::new(&mut frame), None);
        let ask = frame
            .hits()
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "review.actions.3.1")
            .expect("visible Ask Bcode button");
        assert!(ask.enabled && ask.pointer_events && !ask.area.is_empty());
        let point = Point::new(ask.area.x, ask.area.y);
        for kind in [
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
        ] {
            let routed =
                app.controls
                    .lock()
                    .expect("controls")
                    .route(&bmux_tui::event::Event::Mouse(
                        bmux_tui::event::MouseEvent::new(kind, point),
                    ));
            if matches!(kind, bmux_tui::event::MouseEventKind::Up(_)) {
                assert_eq!(
                    routed.and_then(|(_, action)| action),
                    Some(ReviewMouseAction::CommentAction(
                        crate::code_review_tui::ReviewCommentAction::AskBcode
                    ))
                );
            }
        }
        let composer = (0..buffer.area().height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            composer.contains("ask Bcode"),
            "selected submit action stays explicit in the title"
        );
        for label in [
            "[ Save draft ]",
            "[ Ask Bcode ]",
            "[ Publish ]",
            "[ Cancel ]",
        ] {
            assert!(
                composer.contains(label),
                "missing usable composer control {label}"
            );
        }
    }

    #[test]
    fn sidebar_state_tokens_distinguish_review_lifecycle_dimensions() {
        let mut app = crate::code_review_tui::tests::sample_app();
        app.selected_diff_line = 2;
        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("composer")
            .buffer
            .insert_str("draft");
        assert!(app.save_comment_editor());
        let anchor = app.selected_comment_anchor().expect("anchor");
        let exchange_id = app.create_ai_exchange(
            anchor.clone(),
            "question".to_string(),
            Some("session-1".to_string()),
        );
        app.mark_thread_session(&exchange_id, &anchor, "session-1");
        app.suggested_comments
            .entry(anchor.clone())
            .or_default()
            .push(crate::code_review_tui::ReviewSuggestedComment {
                id: "suggestion".to_string(),
                body: "suggestion".to_string(),
                session_id: Some("session-1".to_string()),
                status: crate::code_review_tui::ReviewSuggestionStatus::Suggested,
                rationale: None,
                created_at_ms: None,
                updated_at_ms: None,
            });
        app.record_published_thread_for_test(&anchor, "github");
        let thread = app
            .thread_summaries()
            .into_iter()
            .find(|thread| thread.anchor == anchor)
            .expect("thread");

        assert_eq!(
            thread_sidebar_state_tokens(&app, &thread),
            vec![
                "open",
                "draft:1",
                "AI-exchange",
                "suggestion",
                "linked-session",
                "published",
            ]
        );
    }

    #[test]
    fn common_composer_title_makes_each_submit_action_explicit() {
        let anchor = crate::code_review_tui::ReviewCommentAnchor {
            file_index: 0,
            path: "src/lib.rs".to_string(),
            diff_row: 1,
            end_diff_row: None,
            old_line: None,
            new_line: Some(2),
            old_start: None,
            old_end: None,
            new_start: Some(2),
            new_end: Some(2),
            line_kind: crate::code_review_tui::ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        for (action, label) in [
            (
                crate::code_review_tui::ReviewCommentAction::SaveDraft,
                "save draft",
            ),
            (
                crate::code_review_tui::ReviewCommentAction::AskBcode,
                "ask Bcode",
            ),
            (
                crate::code_review_tui::ReviewCommentAction::Publish,
                "publish",
            ),
        ] {
            let editor = crate::code_review_tui::ReviewCommentEditor::new(anchor.clone(), action);
            assert!(comment_editor_title(&editor).contains(label));
        }
    }

    fn render_inline_session_item(
        item: &crate::code_review_tui::ReviewAgentSessionItem,
        width: u16,
        style: Style,
        theme: ReviewTheme,
    ) -> Line {
        let mut state = crate::code_review_tui::ReviewAgentThreadState::pending(String::new());
        state.session_items.push(item.clone());
        let rows = crate::code_review_tui_view::agent_thread_rows(&state, false, width);
        render_agent_row(
            rows.iter()
                .find(|row| row.kind.is_some())
                .expect("session preview"),
            style,
            theme,
        )
    }

    #[test]
    fn linked_session_markdown_renders_across_narrow_and_wide_widths() {
        let item = crate::code_review_tui::ReviewAgentSessionItem {
            id: "assistant:1".to_string(),
            revision: 1,
            kind: crate::code_review_tui::ReviewAgentSessionItemKind::Assistant,
            label: "answer".to_string(),
            text: "## Result\n\n* first\n* second\n\n```rust\nfn main() {}\n```".to_string(),
            format: bcode_session_view_models::TextFormat::Markdown,
            streaming: false,
            degraded: false,
        };

        let narrow = render_inline_session_item(&item, 24, Style::new(), ReviewTheme::default());
        let wide = render_inline_session_item(&item, 100, Style::new(), ReviewTheme::default());
        let narrow_text = narrow
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        let wide_text = wide
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert_ne!(narrow.spans, [] as [bmux_tui::text::Span; 0]);
        assert_ne!(wide.spans, [] as [bmux_tui::text::Span; 0]);
        assert!(display_width(&narrow_text) <= 24);
        assert!(display_width(&wide_text) <= 100);
    }

    #[test]
    fn linked_session_streaming_marker_is_explicit() {
        let item = crate::code_review_tui::ReviewAgentSessionItem {
            id: "assistant:live".to_string(),
            revision: 4,
            kind: crate::code_review_tui::ReviewAgentSessionItemKind::Assistant,
            label: "answer · streaming".to_string(),
            text: "incomplete **markdown".to_string(),
            format: bcode_session_view_models::TextFormat::Markdown,
            streaming: true,
            degraded: false,
        };

        let rendered = render_inline_session_item(&item, 50, Style::new(), ReviewTheme::default());
        let rendered_text = rendered
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(rendered_text.contains('…'));
        assert!(display_width(&rendered_text) <= 50);
    }

    #[test]
    fn thread_sidebar_labels_keep_lifecycle_states_explicit() {
        let anchor = crate::code_review_tui::ReviewCommentAnchor {
            file_index: 0,
            path: "src/lib.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(2),
            old_start: None,
            old_end: None,
            new_start: Some(2),
            new_end: Some(2),
            line_kind: crate::code_review_tui::ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let thread = crate::code_review_tui::ReviewThreadSummary {
            anchor,
            draft_count: 1,
            latest_body: "Review this".to_string(),
            session_id: Some("session-1".to_string()),
            resolved: true,
            thread_kind: bcode_code_review_models::ReviewThreadKind::Finding,
            severity: bcode_code_review_models::ReviewThreadSeverity::Warning,
            pending_suggestion_count: 1,
            refining_suggestion_count: 0,
            accepted_suggestion_count: 1,
            rejected_suggestion_count: 1,
            external_provider_id: None,
            external_mapping_status: None,
        };

        assert_eq!(
            suggestion_sidebar_label(&thread),
            "[suggest:1/accepted:1/rejected:1]"
        );
        assert!(thread.resolved);
        assert!(thread.session_id.is_some());
    }

    #[test]
    fn expanded_context_rows_render_with_runtime_syntax_highlighting() {
        let rendered = render_expanded_context_file_line(
            Some(2),
            "pub fn expanded() {}",
            &SyntaxHighlighter::new(),
            true,
            "src/lib.rs",
            ReviewTheme::default(),
        );

        assert!(
            rendered
                .line
                .spans
                .iter()
                .any(|span| span.style.fg.is_some_and(|fg| fg != Color::BrightBlack))
        );
    }

    #[test]
    fn file_row_counts_include_open_threads() {
        let file = ReviewFile {
            old_path: Some("a.rs".to_string()),
            new_path: Some("a.rs".to_string()),
            status: ReviewFileStatus::Modified,
            additions: 2,
            deletions: 1,
            is_binary: false,
            hunks: Vec::new(),
        };

        assert_eq!(file_row_counts(&file, 2, 1), " 💬2 ⚠1 +2 -1");
    }
}
