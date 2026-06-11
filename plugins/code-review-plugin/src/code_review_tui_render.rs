//! Rendering for full-screen code review mode.

use bcode_code_review_models::{
    ReviewSourceDiagnosticSeverity, ReviewSourceKind, ReviewSurfaceKind,
};
use bcode_markdown_render::{MarkdownRenderOptions, render_markdown_lines};
use bcode_syntax_render::SyntaxHighlighter;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::truncate_to_display_width;

use crate::code_review_tui::{
    ReviewApp, ReviewFile, ReviewLineKind, ReviewPromptKind, ReviewPublishState, ReviewSidebarMode,
    add_source_menu_items, sidebar_width,
};
use crate::code_review_tui_display::{
    ReviewDisplayRow, ReviewDisplayRowSource, ReviewDisplaySegment, ReviewDisplayTextRole,
};
use crate::code_review_tui_view::{
    ReviewThreadAction, ReviewViewBlock, ReviewViewDocument, ReviewViewRow,
};
use bcode_code_review_models::ReviewSource;

/// Render one full-screen code review frame.
pub fn render(app: &mut ReviewApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }

    frame.fill(area, " ", Style::new().bg(Color::Black));
    let header = Rect::new(area.x, area.y, area.width, 1);
    render_header(app, header, frame);

    let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    render_footer(app, footer, frame);

    let body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(2),
    );
    let sidebar_width = sidebar_width(app, area.width);
    let diff_area = if sidebar_width > 0 {
        let file_area = Rect::new(body.x, body.y, sidebar_width, body.height);
        app.set_file_area(Some(file_area));
        match app.sidebar_mode {
            ReviewSidebarMode::Included => render_included(app, file_area, frame),
            ReviewSidebarMode::Repository => render_files(app, file_area, frame),
            ReviewSidebarMode::Threads => render_threads(app, file_area, frame),
            ReviewSidebarMode::Sources => render_sources(app, file_area, frame),
        }
        let separator = Rect::new(file_area.right(), body.y, 1, body.height);
        render_separator(separator, frame);
        Rect::new(
            separator.right(),
            body.y,
            body.width.saturating_sub(sidebar_width).saturating_sub(1),
            body.height,
        )
    } else {
        app.set_file_area(None);
        body
    };
    app.set_diff_area(diff_area);
    if app.ux_mode == crate::code_review_tui::ReviewUxMode::Build {
        render_build_workspace(app, diff_area, frame);
    } else {
        render_diff(app, diff_area, frame);
    }

    if app.help_visible {
        render_help(app, area, frame);
    }
    if app.comment_editor.is_some() {
        render_comment_editor(app, area, frame);
    }
    if app.prompt_state.is_some() {
        render_prompt(app, area, frame);
    }
    if app.publish_state.is_some() {
        render_publish_modal(app, area, frame);
    }
}

fn render_header(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
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
            " bcode review  {}  {}  File {}  Surface {}  Line {}{}{}  viewed {}/{} ",
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
            " bcode review  {}  {}  File {}  Surface {}  Hunk {}/{}{}{}  viewed {}/{}  +{} -{} ",
            app.review.title,
            file_label,
            file_position,
            surface_kind,
            hunk,
            hunk_total,
            draft_label,
            thread_label,
            viewed_files,
            total_files,
            app.review.additions,
            app.review.deletions
        )
    };
    frame.write_line_with_fallback_style(
        area,
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(&text, usize::from(area.width)),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
        Style::new().fg(Color::Black).bg(Color::Cyan),
    );
}

fn header_thread_label(app: &ReviewApp) -> String {
    let (open_threads, resolved_threads) = app.thread_status_counts();
    if open_threads == 0 && resolved_threads == 0 {
        String::new()
    } else if app.show_resolved_threads {
        format!(
            "  threads {open_threads} open/{resolved_threads} resolved  filter:{}",
            app.thread_filter.label()
        )
    } else {
        format!(
            "  threads {open_threads} open/{resolved_threads} hidden  filter:{}",
            app.thread_filter.label()
        )
    }
}

fn render_footer(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let sidebar = if app.sidebar_visible { "on" } else { "off" };
    let help = if app.help_visible {
        "hide help"
    } else {
        "help"
    };
    let text = app.status_message.as_ref().map_or_else(
        || {
            if app.comment_editor.is_some() {
                return " enter/ctrl+s save comment  esc cancel ".to_string();
            }
            if let Some(range) = app.range_selection_label() {
                return format!(" {range}  c comment  a ask Bcode  esc clear ");
            }
            if app.sidebar_mode == ReviewSidebarMode::Threads && app.sidebar_visible {
                return app.selected_thread_preview().unwrap_or_else(|| {
                    " j/k thread  Enter jump  x publish  a ask/follow up  o open  e edit  D delete  t files  ? help ".to_string()
                });
            }
            if let Some(preview) = app.selected_draft_preview() {
                let linked = app
                    .selected_draft_session_id()
                    .map_or(String::new(), |_| "  🤖 session linked".to_string());
                return format!(" {preview}{linked}  [/]/ thread  {{/}} draft  Enter fold/action  r resolve  R resolved  T filter  U expand  Z collapse  c reply  a ask/follow up  o open  e edit  D delete ");
            }
            if app.sidebar_mode == ReviewSidebarMode::Threads && app.sidebar_visible {
                return app.selected_thread_preview().unwrap_or_else(|| {
                    " j/k thread  Enter jump  x publish  a ask/follow up  o open  e edit  D delete  t files  ? help ".to_string()
                });
            }
            if app.ux_mode == crate::code_review_tui::ReviewUxMode::Build {
                return build_footer_hint(app);
            }
            if app.review.is_repository_review() {
                return format!(
                    " j/k move  enter open/toggle  ←/→ collapse/expand  f picker  : line  / search  n/N next/prev  u/i unresolved  c comment  w viewed  W next-unviewed  V/E all-viewed/unviewed  v range  x publish  a ask Bcode  t sidebar-tab  b sidebar:{sidebar}  ? {help}  q exit "
                );
            }
            format!(
                " j/k move  [/]/ thread  {{/}} draft  Enter fold/action  r resolve  R resolved  T filter  U expand  Z collapse  n/p file  J/K hunk  c comment/reply  v range  x publish  a ask Bcode  o open session  e edit  D delete draft  t sidebar-tab  b sidebar:{sidebar}  ? {help}  q exit "
            )
        },
        |message| format!(" {message}"),
    );
    frame.write_line_with_fallback_style(
        area,
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(&text, usize::from(area.width)),
            Style::new().fg(Color::White).bg(Color::BrightBlack),
        )]),
        Style::new().fg(Color::White).bg(Color::BrightBlack),
    );
}

fn build_footer_hint(app: &ReviewApp) -> String {
    if app.workspace.sources.is_empty() {
        return " build mode  no sources yet — A source menu  + file  u/s/w/l quick add  ? help  q exit "
            .to_string();
    }
    let source_count = app.workspace.sources.len();
    let included_count = app.included_source_count();
    let surface_count = app.review.surfaces().len();
    let (info_sources, warning_sources, error_sources) = app.diagnostic_source_counts();
    let pending = match (app.pending_workspace_save, app.pending_workspace_reload) {
        (true, true) => "  pending save+reload",
        (true, false) => "  pending save",
        (false, true) => "  pending reload",
        (false, false) => "",
    };
    format!(
        " build mode  {included_count}/{source_count} included  {surface_count} surface(s)  diag i/w/e {info_sources}/{warning_sources}/{error_sources}{pending}  n empty  P dedupe  z exclude empty  C edit  m review "
    )
}

fn render_separator(area: Rect, frame: &mut Frame<'_>) {
    for y in area.y..area.bottom() {
        frame.write_line(
            Rect::new(area.x, y, 1, 1),
            &Line::from_spans(vec![Span::styled("│", Style::new().fg(Color::BrightBlack))]),
        );
    }
}

fn render_included(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    frame.write_line(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![Span::styled(
            " Included",
            Style::new().fg(Color::Cyan).bg(Color::Black),
        )]),
    );
    let visible_rows = usize::from(area.height.saturating_sub(1));
    for row in 0..visible_rows {
        let Some(source) = app.workspace.sources.get(row) else {
            break;
        };
        let marker = if source.included { "✓" } else { " " };
        let text = format!(" [{marker}] {}", source.label);
        frame.write_line(
            Rect::new(
                area.x,
                area.y
                    .saturating_add(1)
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&text, usize::from(area.width)),
                Style::new().fg(Color::White).bg(Color::Black),
            )]),
        );
    }
}

fn render_sources(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    frame.write_line(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![Span::styled(
            " Sources",
            Style::new().fg(Color::Cyan).bg(Color::Black),
        )]),
    );
    if app.workspace.sources.is_empty() {
        frame.write_line(
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
            &Line::from_spans(vec![Span::styled(
                " A add source",
                Style::new().fg(Color::BrightBlack).bg(Color::Black),
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
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else if source.included {
            Style::new().fg(Color::White).bg(Color::Black)
        } else {
            Style::new().fg(Color::BrightBlack).bg(Color::Black)
        };
        frame.write_line(
            Rect::new(
                area.x,
                area.y
                    .saturating_add(1)
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            ),
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

fn render_files(app: &mut ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let visible_rows = usize::from(area.height);
    if app.review.is_repository_review() {
        render_file_tree(app, area, frame, visible_rows);
        return;
    }
    if app.selected_file < app.file_scroll {
        app.file_scroll = app.selected_file;
    }
    if app.selected_file >= app.file_scroll.saturating_add(visible_rows) {
        app.file_scroll = app
            .selected_file
            .saturating_sub(visible_rows.saturating_sub(1));
    }

    for row in 0..visible_rows {
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        let index = app.file_scroll.saturating_add(row);
        let line_area = Rect::new(area.x, y, area.width, 1);
        if let Some(file) = app.review.files.get(index) {
            render_file_row(
                file,
                index == app.selected_file,
                app.file_viewed(index),
                app.draft_comment_count_for_file(index),
                line_area,
                frame,
            );
        }
    }
}

fn render_file_tree(app: &mut ReviewApp, area: Rect, frame: &mut Frame<'_>, visible_rows: usize) {
    let rows = app.file_tree_rows();
    let focused_row = app.selected_tree_row.min(rows.len().saturating_sub(1));
    app.selected_tree_row = focused_row;
    if focused_row < app.tree_scroll {
        app.tree_scroll = focused_row;
    }
    if focused_row >= app.tree_scroll.saturating_add(visible_rows) {
        app.tree_scroll = focused_row.saturating_sub(visible_rows.saturating_sub(1));
    }
    let opened_path = app.selected_file_path();
    for row in 0..visible_rows {
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        let tree_row_index = app.tree_scroll.saturating_add(row);
        let line_area = Rect::new(area.x, y, area.width, 1);
        let Some(tree_row) = rows.get(tree_row_index) else {
            continue;
        };
        match tree_row {
            crate::code_review_tui::ReviewFileTreeRow::Directory { path, depth } => {
                let focused = tree_row_index == focused_row;
                let style = if focused {
                    Style::new().fg(Color::Black).bg(Color::White)
                } else {
                    Style::new().fg(Color::Cyan).bg(Color::Black)
                };
                let expanded = if app.expanded_dirs.contains(path) {
                    "▾"
                } else {
                    "▸"
                };
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_else(|| path.to_str().unwrap_or_default());
                let text = format!(" {}{expanded} {name}/", "  ".repeat(*depth));
                frame.write_line_with_fallback_style(
                    line_area,
                    &Line::from_spans(vec![Span::styled(
                        truncate_to_display_width(&text, usize::from(area.width)),
                        style,
                    )]),
                    style,
                );
            }
            crate::code_review_tui::ReviewFileTreeRow::File { index, depth } => {
                if let Some(path) = app.review_path_for_index(*index) {
                    render_file_tree_file_row(
                        &FileTreeFileRow {
                            path: &path,
                            focused: tree_row_index == focused_row,
                            opened: opened_path.as_deref() == Some(path.as_str()),
                            viewed: app.viewed_files.contains(&path),
                            draft_comments: app.draft_comment_count_for_file(*index),
                            depth: *depth,
                        },
                        line_area,
                        frame,
                    );
                }
            }
        }
    }
}

struct FileTreeFileRow<'a> {
    path: &'a str,
    focused: bool,
    opened: bool,
    viewed: bool,
    draft_comments: usize,
    depth: usize,
}

fn render_file_tree_file_row(row: &FileTreeFileRow<'_>, area: Rect, frame: &mut Frame<'_>) {
    let style = if row.focused {
        Style::new().fg(Color::Black).bg(Color::White)
    } else {
        Style::new().fg(Color::White).bg(Color::Black)
    };
    let path = std::path::Path::new(row.path);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| path.to_str().unwrap_or_default());
    let comments = if row.draft_comments == 0 {
        String::new()
    } else {
        format!(" 💬{}", row.draft_comments)
    };
    let open_marker = if row.opened { "●" } else { " " };
    let viewed_marker = if row.viewed { "✓" } else { "○" };
    let text = format!(
        " {}{open_marker}{viewed_marker} {name}{comments}",
        "  ".repeat(row.depth)
    );
    frame.write_line_with_fallback_style(
        area,
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(&text, usize::from(area.width)),
            style,
        )]),
        style,
    );
}

fn render_threads(app: &mut ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let threads = app.visible_thread_summaries();
    let visible_rows = usize::from(area.height);
    if threads.is_empty() {
        frame.write_line(
            area,
            &Line::from_spans(vec![Span::styled(
                format!(" no {} review threads", app.thread_filter.label()),
                Style::new().fg(Color::BrightBlack),
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
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        let index = app.thread_scroll.saturating_add(row);
        let line_area = Rect::new(area.x, y, area.width, 1);
        if let Some(thread) = threads.get(index) {
            let selected = index == app.selected_thread;
            let style = if selected {
                Style::new().fg(Color::Black).bg(Color::White)
            } else {
                Style::new().fg(Color::White).bg(Color::Black)
            };
            let marker = match (thread.resolved, thread.session_id.is_some()) {
                (true, true) => "✓🤖",
                (true, false) => "✓",
                (false, true) => "🤖💬",
                (false, false) => "💬",
            };
            let line_label = thread
                .anchor
                .new_start
                .or(thread.anchor.old_start)
                .map_or_else(
                    || format!("@{}", thread.anchor.diff_row),
                    |line| format!("+{line}"),
                );
            let body = thread.latest_body.lines().next().unwrap_or_default();
            let status = if thread.resolved { "resolved" } else { "open" };
            let text = format!(
                " {marker} {status} {} {line_label} x{}  {body}",
                thread.anchor.path, thread.draft_count
            );
            frame.write_line_with_fallback_style(
                line_area,
                &Line::from_spans(vec![Span::styled(
                    truncate_to_display_width(&text, usize::from(area.width)),
                    style,
                )]),
                style,
            );
        }
    }
}

fn render_file_row(
    file: &ReviewFile,
    selected: bool,
    viewed: bool,
    draft_comments: usize,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let style = if selected {
        Style::new().fg(Color::Black).bg(Color::White)
    } else {
        Style::new().fg(Color::White).bg(Color::Black)
    };
    let status_style = match file.status.label() {
        "A" => Style::new()
            .fg(Color::Green)
            .bg(style.bg.unwrap_or(Color::Black)),
        "D" => Style::new()
            .fg(Color::Red)
            .bg(style.bg.unwrap_or(Color::Black)),
        "R" => Style::new()
            .fg(Color::Yellow)
            .bg(style.bg.unwrap_or(Color::Black)),
        _ => Style::new()
            .fg(Color::Cyan)
            .bg(style.bg.unwrap_or(Color::Black)),
    };
    let counts = if draft_comments == 0 {
        format!(" +{} -{}", file.additions, file.deletions)
    } else {
        format!(
            " 💬{draft_comments} +{} -{}",
            file.additions, file.deletions
        )
    };
    let viewed_marker = if viewed { "✓ " } else { "  " };
    let path_width = usize::from(area.width)
        .saturating_sub(counts.len())
        .saturating_sub(5);
    let path = truncate_to_display_width(file.display_path(), path_width);
    let line = Line::from_spans(vec![
        Span::raw(" "),
        Span::styled(viewed_marker, style),
        Span::styled(file.status.label(), status_style),
        Span::raw(" "),
        Span::styled(path, style),
        Span::styled(
            counts,
            Style::new()
                .fg(Color::BrightBlack)
                .bg(style.bg.unwrap_or(Color::Black)),
        ),
    ]);
    frame.write_line_with_fallback_style(area, &line, style);
}

fn render_build_workspace(app: &mut ReviewApp, area: Rect, frame: &mut Frame<'_>) {
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
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else if warning {
            Style::new().fg(Color::Yellow).bg(Color::Black)
        } else {
            Style::new().fg(Color::White).bg(Color::Black)
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
            Rect::new(area.x, y, area.width, 1),
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
        "enter open/toggle   O open source   Y source for surface   C edit source   I/E/V include/exclude/invert   m review"
            .to_string(),
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

fn render_diff(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    if app.review.is_repository_review() {
        render_repository_file(app, area, frame);
        return;
    }
    if selected_surface_kind(app) == Some(ReviewSurfaceKind::File) {
        render_materialized_file_surface(app, area, frame);
        return;
    }
    let Some(file) = app.selected_file_data() else {
        render_empty(area, "No changed files", frame);
        return;
    };
    if file.is_binary {
        render_empty(area, "Binary file diff not available", frame);
        return;
    }
    let Some(document) = app.current_review_view_document() else {
        render_empty(area, "No textual changes", frame);
        return;
    };
    if document.rows.is_empty() {
        render_empty(area, "No textual changes", frame);
        return;
    }
    render_view_document(app, &document, area, frame);
}

fn render_view_document(
    app: &ReviewApp,
    document: &ReviewViewDocument,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let syntax_highlighter = SyntaxHighlighter::new();
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
            syntax_highlighter,
            can_highlight,
            &syntax_hint,
            area.width,
        );
        if app.is_view_target_selected(&view_row.target) {
            rendered.line = selected_line(&rendered.line);
            rendered.style = rendered.style.bg(Color::BrightBlack);
        }
        frame.write_line_with_fallback_style(
            Rect::new(area.x, y, area.width, 1),
            &rendered.line,
            rendered.style,
        );
    }
}

fn render_view_row(
    app: &ReviewApp,
    view_row: &ReviewViewRow,
    syntax_highlighter: SyntaxHighlighter,
    can_highlight: bool,
    syntax_hint: &str,
    width: u16,
) -> RenderedRow {
    match &view_row.block {
        ReviewViewBlock::DisplayRow(display_row) => {
            render_source_view_row(app, view_row, display_row)
        }
        ReviewViewBlock::FileLine {
            line_number,
            content,
        } => render_file_view_row(
            app,
            view_row,
            *line_number,
            content,
            syntax_highlighter,
            can_highlight,
            syntax_hint,
        ),
        ReviewViewBlock::InlineThreadHeader {
            anchor,
            comment_count,
            collapsed,
            resolved,
            ..
        } => {
            let style = Style::new()
                .fg(Color::Yellow)
                .bg(Color::Rgb(30, 28, 12))
                .add_modifier(Modifier::BOLD);
            let status = if *resolved { "resolved" } else { "open" };
            let status_style = if *resolved {
                Style::new().fg(Color::Green).bg(Color::Rgb(30, 28, 12))
            } else {
                Style::new().fg(Color::Yellow).bg(Color::Rgb(30, 28, 12))
            };
            RenderedRow {
                line: Line::from_spans(vec![
                    Span::styled(
                        format!(
                            "   {}─ draft thread on rows {}-{} ({comment_count} comment{}) ",
                            if *collapsed { "▸" } else { "▾" },
                            anchor.source_row,
                            anchor.end_source_row(),
                            if *comment_count == 1 { "" } else { "s" }
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
        ReviewViewBlock::InlineComment {
            comment,
            body_line_index,
            body_line_count,
            ..
        } => {
            let style = Style::new().fg(Color::White).bg(Color::Rgb(20, 20, 20));
            let label = if *body_line_index == 0 {
                if comment.persisted { "draft" } else { "saving" }
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
                    comment,
                    *body_line_index,
                    width,
                    style,
                ),
                style,
            }
        }
        ReviewViewBlock::InlineThreadAction { action, .. } => render_inline_thread_action(*action),
    }
}

fn render_inline_comment_line(
    branch: &str,
    label: &str,
    comment: &crate::code_review_tui::ReviewDraftComment,
    body_line_index: usize,
    width: u16,
    style: Style,
) -> Line {
    let prefix_style = Style::new().fg(Color::Yellow).bg(Color::Rgb(20, 20, 20));
    let prefix = format!("   {branch} {label:<6} ");
    let markdown_width = width
        .saturating_sub(u16::try_from(prefix.chars().count()).unwrap_or(u16::MAX))
        .max(1);
    let markdown_line =
        render_markdown_lines(&comment.body, MarkdownRenderOptions::new(markdown_width))
            .get(body_line_index)
            .cloned()
            .unwrap_or_else(Line::default);
    let mut spans = vec![Span::styled(prefix, prefix_style)];
    if markdown_line.spans.is_empty() {
        spans.push(Span::styled(String::new(), style));
    } else {
        spans.extend(markdown_line.spans);
    }
    Line::from_spans(spans)
}

fn render_inline_thread_action(action: ReviewThreadAction) -> RenderedRow {
    let style = Style::new()
        .fg(Color::BrightBlack)
        .bg(Color::Rgb(18, 18, 18));
    let shortcut_style = Style::new().fg(Color::Yellow).bg(Color::Rgb(18, 18, 18));
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

fn render_source_view_row(
    app: &ReviewApp,
    view_row: &ReviewViewRow,
    display_row: &ReviewDisplayRow,
) -> RenderedRow {
    let Some(source_row) = view_row.source_row else {
        return render_display_row(display_row);
    };
    let rendered = render_display_row(display_row);
    let mut line = rendered.line;
    if let Some(marker) = app.draft_marker_at(app.selected_file, source_row) {
        line.spans
            .insert(0, Span::styled(marker, Style::new().fg(Color::Yellow)));
    }
    let (line, style) = if source_row == app.selected_diff_line {
        (selected_line(&line), rendered.style.bg(Color::BrightBlack))
    } else if app.is_row_in_range_selection(app.selected_file, source_row) {
        (selected_line(&line), rendered.style.bg(Color::Blue))
    } else {
        (line, rendered.style)
    };
    RenderedRow { line, style }
}

fn render_file_view_row(
    app: &ReviewApp,
    view_row: &ReviewViewRow,
    line_number: Option<u32>,
    content: &str,
    syntax_highlighter: SyntaxHighlighter,
    can_highlight: bool,
    syntax_hint: &str,
) -> RenderedRow {
    let source_row = view_row.source_row.unwrap_or(view_row.visual_row);
    let mut style = file_viewer_row_style(app, source_row);
    let line_number =
        line_number.map_or_else(|| "      ".to_string(), |number| format!("{number:>5} "));
    let mut spans = vec![Span::styled(
        line_number.clone(),
        Style::new().fg(Color::BrightBlack),
    )];
    if line_number.trim().is_empty() {
        spans.push(Span::styled(content.to_string(), style));
    } else {
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
        line.spans
            .insert(0, Span::styled(marker, Style::new().fg(Color::Yellow)));
        style = style.bg(style.bg.unwrap_or(Color::BrightBlack));
    }
    RenderedRow { line, style }
}

fn selected_line(line: &Line) -> Line {
    let mut line = line.clone();
    for span in &mut line.spans {
        span.style = span.style.bg(Color::BrightBlack);
    }
    line
}

fn render_empty(area: Rect, text: &str, frame: &mut Frame<'_>) {
    frame.write_line(
        area,
        &Line::from_spans(vec![Span::styled(
            format!(" {text}"),
            Style::new().fg(Color::BrightBlack),
        )]),
    );
}

fn selected_surface_kind(app: &ReviewApp) -> Option<ReviewSurfaceKind> {
    app.review
        .surfaces()
        .get(app.selected_file)
        .map(|surface| surface.kind)
}

fn render_materialized_file_surface(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    let Some(file) = app.selected_file_data() else {
        render_empty(area, "No file surface", frame);
        return;
    };
    if file.is_binary {
        render_empty(area, "Binary file content not available", frame);
        return;
    }
    let Some(document) = app.current_review_view_document() else {
        render_empty(area, "No file content", frame);
        return;
    };
    if document.rows.is_empty() {
        render_empty(area, "No file content", frame);
        return;
    }
    render_view_document(app, &document, area, frame);
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

fn render_repository_file(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    let Some(path) = app.selected_file_path() else {
        render_empty(area, "No files", frame);
        return;
    };
    let Some(cached) = app.file_cache.get(&path) else {
        render_empty(area, "Loading file…", frame);
        return;
    };
    if let Some(reason) = &cached.unavailable_reason {
        render_empty(area, reason, frame);
        return;
    }
    let Some(document) = app.current_review_view_document() else {
        render_empty(area, "No file content", frame);
        return;
    };
    if document.rows.is_empty() {
        render_empty(area, "No file content", frame);
        return;
    }
    render_view_document(app, &document, area, frame);
}

fn file_viewer_row_style(app: &ReviewApp, index: usize) -> Style {
    if index == app.selected_diff_line {
        Style::new().fg(Color::Black).bg(Color::Yellow)
    } else if app.is_row_in_range_selection(app.selected_file, index) {
        Style::new().fg(Color::White).bg(Color::Blue)
    } else if app.has_draft_comment_at(app.selected_file, index) {
        Style::new().fg(Color::White).bg(Color::BrightBlack)
    } else {
        Style::new()
    }
}

fn highlighted_source_spans(
    syntax_highlighter: SyntaxHighlighter,
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

fn render_display_row(row: &ReviewDisplayRow) -> RenderedRow {
    match row.source {
        ReviewDisplayRowSource::HunkHeader => {
            let style = row_style(row.source);
            RenderedRow {
                line: Line::from_spans(
                    row.segments
                        .iter()
                        .map(|segment| render_display_segment(segment, style))
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
            let row_style = row_style(row.source);
            let marker = row.source.diff_marker().unwrap_or(' ');
            let mut spans = vec![
                Span::styled(
                    format!(" {old} {new} "),
                    row_style.patch(Style::new().fg(Color::BrightBlack)),
                ),
                Span::styled(
                    marker.to_string(),
                    row_style.patch(marker_style(row.source).add_modifier(Modifier::BOLD)),
                ),
            ];
            spans.extend(
                row.segments
                    .iter()
                    .map(|segment| render_display_segment(segment, row_style)),
            );
            RenderedRow {
                line: Line::from_spans(spans),
                style: row_style,
            }
        }
    }
}

fn render_display_segment(segment: &ReviewDisplaySegment, base_style: Style) -> Span {
    Span::styled(
        segment.text.clone(),
        base_style.patch(style_for_segment(segment)),
    )
}

fn style_for_segment(segment: &ReviewDisplaySegment) -> Style {
    let mut style = Style::new();
    for role in &segment.roles {
        style = style.patch(style_for_role(role));
    }
    style
}

const fn style_for_role(role: &ReviewDisplayTextRole) -> Style {
    match role {
        ReviewDisplayTextRole::Code
        | ReviewDisplayTextRole::DiffAdded
        | ReviewDisplayTextRole::DiffRemoved
        | ReviewDisplayTextRole::DiffContext => Style::new(),
        ReviewDisplayTextRole::Syntax(style) => syntax_style_to_tui(*style),
        ReviewDisplayTextRole::HunkHeader => {
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
        }
    }
}

const fn row_style(source: ReviewDisplayRowSource) -> Style {
    match source {
        ReviewDisplayRowSource::HunkHeader => Style::new()
            .fg(Color::BrightMagenta)
            .bg(Color::Rgb(24, 18, 34))
            .add_modifier(Modifier::BOLD),
        ReviewDisplayRowSource::Context => Style::new(),
        ReviewDisplayRowSource::Added => Style::new().bg(Color::Rgb(0, 24, 16)),
        ReviewDisplayRowSource::Removed => Style::new().bg(Color::Rgb(32, 10, 10)),
    }
}

const fn marker_style(source: ReviewDisplayRowSource) -> Style {
    match source {
        ReviewDisplayRowSource::Added => Style::new().fg(Color::BrightGreen),
        ReviewDisplayRowSource::Removed => Style::new().fg(Color::BrightRed),
        ReviewDisplayRowSource::HunkHeader => Style::new().fg(Color::BrightMagenta),
        ReviewDisplayRowSource::Context => Style::new().fg(Color::BrightBlack),
    }
}

const fn syntax_style_to_tui(style: bcode_syntax_render::SyntaxStyle) -> Style {
    let mut output = Style::new().fg(Color::Rgb(
        style.foreground_r,
        style.foreground_g,
        style.foreground_b,
    ));
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

fn render_help(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    let width = area.width.min(68);
    let height = 18;
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    let popup = Rect::new(x, y, width, height);
    frame.fill(
        popup,
        " ",
        Style::new().fg(Color::White).bg(Color::BrightBlack),
    );
    let lines = help_lines(app);
    for (index, text) in lines.iter().enumerate() {
        let y = popup
            .y
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= popup.bottom() {
            break;
        }
        frame.write_line(
            Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(text, usize::from(popup.width.saturating_sub(2))),
                Style::new().fg(Color::White).bg(Color::BrightBlack),
            )]),
        );
    }
}

fn help_lines(app: &ReviewApp) -> &'static [&'static str] {
    if app.ux_mode == crate::code_review_tui::ReviewUxMode::Build {
        BUILD_HELP_LINES
    } else if app.review.is_repository_review() {
        REPOSITORY_HELP_LINES
    } else {
        DIFF_HELP_LINES
    }
}

const BUILD_HELP_LINES: &[&str] = &[
    " Build Review Help",
    "",
    " m                   switch to review mode",
    " g/G                first / last build row",
    " j/k or arrows       move selection",
    " A/+                add source menu / add file source",
    " T                  rename workspace",
    " R                  refresh/rematerialize sources",
    " I/E/V              include all / exclude all / invert sources",
    " n/N/z              next / previous / exclude empty sources",
    " d/Z                next diagnostic source / exclude error sources",
    " M/X/P              merge-base / remove excluded / remove duplicates",
    " O/Y                open source surface / source for surface",
    " f or ctrl-p         fuzzy file picker",
    " enter               inspect/open selected item",
    " t                   cycle included/repo/threads/sources",
    " b                   toggle sidebar",
    " ?                   toggle this help",
    " q or esc            exit review",
];

const REPOSITORY_HELP_LINES: &[&str] = &[
    " Repository Review Help",
    "",
    " j/k or arrows       move tree selection",
    " enter/right         open file or expand directory",
    " left                collapse directory/parent",
    " f or ctrl-p         fuzzy file picker",
    " :                   jump to line",
    " /                   search current file",
    " n/N                 next/previous search match",
    " v                   select/clear line range",
    " c                   create draft comment",
    " a                   ask Bcode about selected line",
    " x                   publish/export review",
    " w                   mark selected file viewed/unviewed",
    " W                   next unviewed file",
    " V                   mark all files viewed",
    " E                   mark all files unviewed",
    " t                   cycle included/repo/threads/sources",
    " b                   toggle sidebar",
    " ?                   toggle this help",
    " q or esc            exit review",
];

const DIFF_HELP_LINES: &[&str] = &[
    " Code Review Help",
    "",
    " j/k or arrows       move through diff, comments, and actions",
    " n/p                 next/previous file",
    " J/K                 next/previous hunk",
    " [/]/                previous/next review thread",
    " u/i                 next/previous unresolved review thread",
    " {/}                 previous/next draft comment",
    " Enter               fold thread or activate selected action",
    " r                   resolve/reopen selected thread",
    " R                   show/hide resolved threads",
    " T                   cycle thread sidebar filter",
    " U/Z                 expand/collapse all inline threads",
    " B                   switch to build/source mode",
    " b                   toggle sidebar",
    " t                   cycle included/repo/threads/sources",
    " mouse wheel         scroll diff",
    " click file          open file",
    " c                   create draft comment or reply",
    " x                   publish/export review",
    " w                   mark selected file viewed/unviewed",
    " W                   next unviewed file",
    " V                   mark all files viewed",
    " E                   mark all files unviewed",
    " v                   select/clear line range",
    " a                   ask Bcode about selected line/thread",
    " o                   open linked Bcode session",
    " e                   edit selected/latest draft",
    " D                   delete selected/latest draft",
    " ?                   toggle this help",
    " q or esc            exit review",
];

fn render_publish_modal(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    let Some(state) = &app.publish_state else {
        return;
    };
    let width = area.width.min(96);
    let height = area.height.min(24);
    if width < 30 || height < 8 {
        return;
    }
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    let popup = Rect::new(x, y, width, height);
    frame.fill(
        popup,
        " ",
        Style::new().fg(Color::White).bg(Color::BrightBlack),
    );
    match state {
        ReviewPublishState::Picker => render_publisher_picker(app, popup, frame),
        ReviewPublishState::Options {
            options, selected, ..
        } => render_publish_options(options, *selected, popup, frame),
        ReviewPublishState::Preview {
            publisher_id,
            preview,
            scroll,
            ..
        } => render_publish_preview(publisher_id, preview, *scroll, false, popup, frame),
        ReviewPublishState::ConfirmSubmit {
            publisher_id,
            preview,
            scroll,
            ..
        } => render_publish_preview(publisher_id, preview, *scroll, true, popup, frame),
    }
}

fn render_publisher_picker(app: &ReviewApp, popup: Rect, frame: &mut Frame<'_>) {
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup.y,
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            " Publish review  Enter preview  Esc cancel ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
    );
    let rows = usize::from(popup.height.saturating_sub(2));
    for row in 0..rows {
        let Some(publisher) = app.publishers.get(row) else {
            break;
        };
        let selected = row == app.selected_publisher;
        let style = if selected {
            Style::new().fg(Color::Black).bg(Color::White)
        } else {
            Style::new().fg(Color::White).bg(Color::BrightBlack)
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
            Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            ),
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
    frame: &mut Frame<'_>,
) {
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup.y,
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            " Publisher options  Enter preview  Tab next  ←/→ choice  Esc cancel ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
    );
    let rows = usize::from(popup.height.saturating_sub(2));
    for (row, option) in options.iter().take(rows).enumerate() {
        let style = if row == selected {
            Style::new().fg(Color::Black).bg(Color::White)
        } else {
            Style::new().fg(Color::White).bg(Color::BrightBlack)
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
            Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&text, usize::from(popup.width.saturating_sub(2))),
                style,
            )]),
            style,
        );
    }
}

fn render_publish_preview(
    publisher_id: &str,
    preview: &str,
    scroll: usize,
    confirming: bool,
    popup: Rect,
    frame: &mut Frame<'_>,
) {
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup.y,
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            if confirming {
                format!(" Confirm submit {publisher_id}  Enter publish  Esc cancel ")
            } else {
                format!(" Preview {publisher_id}  Enter confirm  Esc cancel ")
            },
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
    );
    let rows = usize::from(popup.height.saturating_sub(2));
    for (row, line) in preview.lines().skip(scroll).take(rows).enumerate() {
        let y = popup
            .y
            .saturating_add(1 + u16::try_from(row).unwrap_or(u16::MAX));
        frame.write_line(
            Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(line, usize::from(popup.width.saturating_sub(2))),
                Style::new().fg(Color::White).bg(Color::BrightBlack),
            )]),
        );
    }
    if confirming {
        let warning = " This will publish the review. Press Enter again to submit. ";
        frame.write_line(
            Rect::new(
                popup.x.saturating_add(1),
                popup.bottom().saturating_sub(1),
                popup.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(warning, usize::from(popup.width.saturating_sub(2))),
                Style::new().fg(Color::Black).bg(Color::Yellow),
            )]),
        );
    }
}

fn render_comment_editor(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    let Some(editor) = &app.comment_editor else {
        return;
    };
    let width = area.width.min(72);
    let height = area.height.min(10);
    if width < 20 || height < 5 {
        return;
    }
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    let popup = Rect::new(x, y, width, height);
    frame.fill(popup, " ", Style::new().fg(Color::White).bg(Color::Black));
    frame.write_line(
        Rect::new(popup.x, popup.y, popup.width, 1),
        &Line::from_spans(vec![Span::styled(
            if editor.preview {
                " Draft comment preview "
            } else {
                " Draft comment "
            },
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
    );
    let anchor = format!(
        " {}:{}{} ",
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
        }
    );
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup.y.saturating_add(1),
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(&anchor, usize::from(popup.width.saturating_sub(2))),
            Style::new().fg(Color::BrightBlack).bg(Color::Black),
        )]),
    );
    let text_height = usize::from(height.saturating_sub(4));
    render_comment_editor_body(editor, popup, text_height, frame);
    if editor.buffer.text().is_empty() {
        frame.write_line(
            Rect::new(
                popup.x.saturating_add(1),
                popup.y.saturating_add(2),
                popup.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                "write a draft comment...",
                Style::new().fg(Color::BrightBlack).bg(Color::Black),
            )]),
        );
    }
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup.bottom().saturating_sub(1),
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            if editor.preview {
                " tab edit  enter/ctrl+s save  esc cancel "
            } else {
                " tab preview  enter/ctrl+s save  esc cancel "
            },
            Style::new().fg(Color::Black).bg(Color::Yellow),
        )]),
    );
}

fn render_comment_editor_body(
    editor: &crate::code_review_tui::ReviewCommentEditor,
    popup: Rect,
    text_height: usize,
    frame: &mut Frame<'_>,
) {
    if editor.preview {
        render_comment_editor_preview(editor, popup, text_height, frame);
    } else {
        render_comment_editor_text(editor, popup, text_height, frame);
    }
}

fn render_comment_editor_preview(
    editor: &crate::code_review_tui::ReviewCommentEditor,
    popup: Rect,
    text_height: usize,
    frame: &mut Frame<'_>,
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
            spans.push(Span::styled(String::new(), Style::new().bg(Color::Black)));
        }
        frame.write_line_with_fallback_style(
            Rect::new(
                popup.x.saturating_add(1),
                popup
                    .y
                    .saturating_add(2)
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                preview_width,
                1,
            ),
            &Line::from_spans(spans),
            Style::new().fg(Color::White).bg(Color::Black),
        );
    }
}

fn render_comment_editor_text(
    editor: &crate::code_review_tui::ReviewCommentEditor,
    popup: Rect,
    text_height: usize,
    frame: &mut Frame<'_>,
) {
    for (index, line) in editor.buffer.text().lines().take(text_height).enumerate() {
        frame.write_line(
            Rect::new(
                popup.x.saturating_add(1),
                popup
                    .y
                    .saturating_add(2)
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                popup.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(line, usize::from(popup.width.saturating_sub(2))),
                Style::new().fg(Color::White).bg(Color::Black),
            )]),
        );
    }
}

fn prompt_popup_height(kind: &ReviewPromptKind, area: Rect) -> u16 {
    match kind {
        ReviewPromptKind::FilePicker
        | ReviewPromptKind::AddSourceKind
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
    frame: &mut Frame<'_>,
) {
    let items = add_source_menu_items();
    for (row, item) in items
        .iter()
        .take(usize::from(height.saturating_sub(3)))
        .enumerate()
    {
        let style = if row == prompt.selected {
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::new().fg(Color::White).bg(Color::Black)
        };
        let text = format!(" {:<16} {}", item.label, item.help);
        frame.write_line(
            Rect::new(
                popup.x.saturating_add(1),
                popup
                    .y
                    .saturating_add(2 + u16::try_from(row).unwrap_or(u16::MAX)),
                popup.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(&text, usize::from(popup.width.saturating_sub(2))),
                style,
            )]),
        );
    }
}

fn render_prompt(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    let Some(prompt) = &app.prompt_state else {
        return;
    };
    let width = area.width.min(80);
    let height = prompt_popup_height(&prompt.kind, area);
    if width < 20 || height < 3 {
        return;
    }
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    let popup = Rect::new(x, y, width, height);
    frame.fill(popup, " ", Style::new().fg(Color::White).bg(Color::Black));
    let title = prompt_title(&prompt.kind);
    frame.write_line(
        Rect::new(popup.x, popup.y, popup.width, 1),
        &Line::from_spans(vec![Span::styled(
            title,
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
    );
    let query = prompt.buffer.text();
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup.y.saturating_add(1),
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(query, usize::from(popup.width.saturating_sub(2))),
            Style::new().fg(Color::White).bg(Color::Black),
        )]),
    );
    match &prompt.kind {
        ReviewPromptKind::AddSourceKind => render_add_source_menu(prompt, popup, height, frame),
        ReviewPromptKind::AddFileSourcePicker | ReviewPromptKind::AddFileRangePathPicker => {
            render_add_repository_file_picker(app, prompt, popup, height, query, frame);
        }
        ReviewPromptKind::AddCommitPicker
        | ReviewPromptKind::AddCommitRangeBasePicker
        | ReviewPromptKind::AddCommitRangeHeadPicker { .. } => {
            render_add_repository_commit_picker(app, prompt, popup, height, query, frame);
        }
        ReviewPromptKind::AddBranchCompareBasePicker
        | ReviewPromptKind::AddBranchCompareHeadPicker { .. } => {
            render_add_repository_branch_picker(app, prompt, popup, height, query, frame);
        }
        ReviewPromptKind::FilePicker => {
            render_file_picker(app, prompt, popup, height, query, frame);
        }
        _ => {}
    }
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup.bottom().saturating_sub(1),
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            prompt_footer_text(&prompt.kind),
            Style::new().fg(Color::Black).bg(Color::Yellow),
        )]),
    );
}

fn render_add_repository_file_picker(
    app: &ReviewApp,
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    query: &str,
    frame: &mut Frame<'_>,
) {
    for (row, path) in app
        .repository_file_picker_matches(query)
        .into_iter()
        .take(usize::from(height.saturating_sub(3)))
        .enumerate()
    {
        render_prompt_choice(row, prompt.selected, popup, &path, frame);
    }
}

fn render_add_repository_commit_picker(
    app: &ReviewApp,
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    query: &str,
    frame: &mut Frame<'_>,
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
        );
    }
}

fn render_add_repository_branch_picker(
    app: &ReviewApp,
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    query: &str,
    frame: &mut Frame<'_>,
) {
    for (row, branch) in app
        .repository_branch_picker_matches(query)
        .into_iter()
        .take(usize::from(height.saturating_sub(3)))
        .enumerate()
    {
        render_prompt_choice(row, prompt.selected, popup, &branch, frame);
    }
}

fn render_file_picker(
    app: &ReviewApp,
    prompt: &crate::code_review_tui::ReviewPromptState,
    popup: Rect,
    height: u16,
    query: &str,
    frame: &mut Frame<'_>,
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
        );
    }
}

fn render_prompt_choice(
    row: usize,
    selected: usize,
    popup: Rect,
    text: &str,
    frame: &mut Frame<'_>,
) {
    let style = if row == selected {
        Style::new().fg(Color::Black).bg(Color::Yellow)
    } else {
        Style::new().fg(Color::White).bg(Color::Black)
    };
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup
                .y
                .saturating_add(2 + u16::try_from(row).unwrap_or(u16::MAX)),
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            truncate_to_display_width(text, usize::from(popup.width.saturating_sub(2))),
            style,
        )]),
    );
}

const fn prompt_footer_text(kind: &ReviewPromptKind) -> &'static str {
    match kind {
        ReviewPromptKind::AddSourceKind => {
            " add source  ↑/↓ choose  enter select  type shortcut: worktree, staged, file, range, branch  esc cancel "
        }
        ReviewPromptKind::AddCommitPicker => {
            " ↑/↓ choose commit  enter add commit source  esc cancel "
        }
        ReviewPromptKind::AddCommitRangeBasePicker => {
            " ↑/↓ choose base commit  enter select  esc cancel "
        }
        ReviewPromptKind::AddCommitRangeHeadPicker { .. } => {
            " ↑/↓ choose head commit  enter add range  esc cancel "
        }
        ReviewPromptKind::AddCommitRangeSource | ReviewPromptKind::AddBranchCompareSource => {
            " enter base..head or base...head  esc cancel "
        }
        ReviewPromptKind::AddBranchCompareBasePicker => {
            " ↑/↓ choose base branch  enter select  esc cancel "
        }
        ReviewPromptKind::AddBranchCompareHeadPicker { .. } => {
            " ↑/↓ choose head branch  enter add compare  esc cancel "
        }
        ReviewPromptKind::AddFileRangeSource => " enter path:start-end  esc cancel ",
        ReviewPromptKind::FilePicker => " ↑/↓ choose  enter open  esc cancel ",
        ReviewPromptKind::AddFileSourcePicker => " ↑/↓ choose  enter add file source  esc cancel ",
        ReviewPromptKind::AddFileRangePathPicker => {
            " ↑/↓ choose file  enter pick file  esc cancel "
        }
        ReviewPromptKind::EditSourceSpec { .. } => " enter update source  esc cancel ",
        _ => " enter submit  esc cancel ",
    }
}

struct RenderedRow {
    line: Line,
    style: Style,
}
