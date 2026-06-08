//! Rendering for full-screen code review mode.

use bcode_code_review_models::{ReviewSourceKind, ReviewSurfaceKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::truncate_to_display_width;

use super::code_review::{
    ReviewApp, ReviewFile, ReviewLineKind, ReviewPromptKind, ReviewPublishState, ReviewSidebarMode,
    add_source_menu_items, sidebar_width,
};
use super::code_review_display::{
    ReviewDisplayBuilder, ReviewDisplayRow, ReviewDisplayRowSource, ReviewDisplaySegment,
    ReviewDisplayTextRole,
};

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
    if app.ux_mode == super::code_review::ReviewUxMode::Build {
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
    let surface_kind = app
        .review
        .surfaces()
        .get(app.selected_file)
        .map_or("diff", |surface| match surface.kind {
            ReviewSurfaceKind::Diff => "diff",
            ReviewSurfaceKind::File => "file",
        });
    let text = if app.ux_mode == super::code_review::ReviewUxMode::Build {
        let workspace = &app.workspace;
        format!(
            " bcode review build  {}  {} included source(s)  {} file(s) ",
            workspace.title,
            workspace
                .sources
                .iter()
                .filter(|source| source.included)
                .count(),
            app.review.files.len()
        )
    } else if app.review.is_repository_review() {
        format!(
            " bcode review  {}  {}  File {}  Surface {}  Line {}{} ",
            app.review.title,
            file_label,
            file_position,
            surface_kind,
            app.selected_diff_line.saturating_add(1),
            draft_label
        )
    } else {
        let (hunk, hunk_total) = app.hunk_position();
        format!(
            " bcode review  {}  {}  File {}  Surface {}  Hunk {}/{}{}  +{} -{} ",
            app.review.title,
            file_label,
            file_position,
            surface_kind,
            hunk,
            hunk_total,
            draft_label,
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
                return format!(" {preview}{linked}  a ask/follow up  o open  e edit  D delete latest draft ");
            }
            if app.sidebar_mode == ReviewSidebarMode::Threads && app.sidebar_visible {
                return app.selected_thread_preview().unwrap_or_else(|| {
                    " j/k thread  Enter jump  x publish  a ask/follow up  o open  e edit  D delete  t files  ? help ".to_string()
                });
            }
            if app.ux_mode == super::code_review::ReviewUxMode::Build {
                let source_hint = if app.workspace.sources.is_empty() {
                    "no sources yet — press A to add one"
                } else {
                    "j/k move  space include/exclude  A add source  r rename  [/] reorder  - remove"
                };
                return format!(
                    " build mode  {source_hint}  m review mode  f picker  ? help  q exit "
                );
            }
            if app.review.is_repository_review() {
                return format!(
                    " j/k move  enter open/toggle  ←/→ collapse/expand  f picker  : line  / search  n/N next/prev  c comment  v range  x publish  a ask Bcode  t sidebar-tab  b sidebar:{sidebar}  ? {help}  q exit "
                );
            }
            format!(
                " j/k scroll  n/p file  J/K hunk  c comment  v range  x publish  a ask Bcode  o open session  e edit  D delete draft  t sidebar-tab  b sidebar:{sidebar}  ? {help}  q exit "
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
            && app.ux_mode == super::code_review::ReviewUxMode::Build
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
        ReviewSourceKind::CommitRange { .. } => "range",
        ReviewSourceKind::BranchCompare { .. } => "branch",
        ReviewSourceKind::File { .. } => "file",
        ReviewSourceKind::FileRange { .. } => "file-range",
        ReviewSourceKind::Repository => "repo",
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
                app.draft_comment_count_for_file(index),
                line_area,
                frame,
            );
        }
    }
}

fn render_file_tree(app: &mut ReviewApp, area: Rect, frame: &mut Frame<'_>, visible_rows: usize) {
    let rows = app.file_tree_rows();
    let selected_row = app.selected_tree_row.min(rows.len().saturating_sub(1));
    app.selected_tree_row = selected_row;
    if selected_row < app.file_scroll {
        app.file_scroll = selected_row;
    }
    if selected_row >= app.file_scroll.saturating_add(visible_rows) {
        app.file_scroll = selected_row.saturating_sub(visible_rows.saturating_sub(1));
    }
    for row in 0..visible_rows {
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        let index = app.file_scroll.saturating_add(row);
        let line_area = Rect::new(area.x, y, area.width, 1);
        let Some(tree_row) = rows.get(index) else {
            continue;
        };
        match tree_row {
            super::code_review::ReviewFileTreeRow::Directory { path, depth } => {
                let selected = index == selected_row;
                let style = if selected {
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
            super::code_review::ReviewFileTreeRow::File { index, depth } => {
                if let Some(file) = app.review.files.get(*index) {
                    render_file_tree_file_row(
                        file,
                        *index == app.selected_file,
                        app.draft_comment_count_for_file(*index),
                        *depth,
                        line_area,
                        frame,
                    );
                }
            }
        }
    }
}

fn render_file_tree_file_row(
    file: &ReviewFile,
    selected: bool,
    draft_comments: usize,
    depth: usize,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let style = if selected {
        Style::new().fg(Color::Black).bg(Color::White)
    } else {
        Style::new().fg(Color::White).bg(Color::Black)
    };
    let path = std::path::Path::new(file.display_path());
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| file.display_path());
    let comments = if draft_comments == 0 {
        String::new()
    } else {
        format!(" 💬{draft_comments}")
    };
    let text = format!(" {}  {name}{comments}", "  ".repeat(depth));
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
    let threads = app.thread_summaries();
    let visible_rows = usize::from(area.height);
    if threads.is_empty() {
        frame.write_line(
            area,
            &Line::from_spans(vec![Span::styled(
                " no review threads",
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
            let marker = if thread.session_id.is_some() {
                "🤖💬"
            } else {
                "💬"
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
            let text = format!(
                " {marker} {} {line_label} x{}  {body}",
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
    let path_width = usize::from(area.width)
        .saturating_sub(counts.len())
        .saturating_sub(3);
    let path = truncate_to_display_width(file.display_path(), path_width);
    let line = Line::from_spans(vec![
        Span::raw(" "),
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

fn render_build_workspace(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let workspace = &app.workspace;
    let surfaces = app.review.surfaces();
    let mut rows = Vec::new();
    rows.push((
        "Review workspace".to_string(),
        format!(": {}", workspace.title),
        false,
    ));
    rows.push((String::new(), String::new(), false));
    rows.push(("Included sources".to_string(), String::new(), false));
    for source in &workspace.sources {
        let marker = if source.included { "✓" } else { " " };
        rows.push((format!("  [{marker}]"), source.label.clone(), true));
    }
    rows.push((String::new(), String::new(), false));
    rows.push(("Review surfaces".to_string(), String::new(), false));
    for surface in &surfaces {
        let kind = match surface.kind {
            ReviewSurfaceKind::Diff => "diff",
            ReviewSurfaceKind::File => "file",
        };
        rows.push((format!("  {kind:4}"), surface.path.clone(), true));
    }
    rows.push((String::new(), String::new(), false));
    rows.push((
        "toggle-space   + add selected file   A add source   r rename   [/] reorder   - remove source   m review mode"
            .to_string(),
        String::new(),
        false,
    ));

    let mut selectable_index = 0usize;
    for (row, (prefix, text, selectable)) in
        rows.into_iter().take(usize::from(area.height)).enumerate()
    {
        let selected = selectable && selectable_index == app.selected_build_row;
        if selectable {
            selectable_index = selectable_index.saturating_add(1);
        }
        let style = if selected {
            Style::new().fg(Color::Black).bg(Color::Yellow)
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

fn render_diff(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    if selected_surface_kind(app) == Some(ReviewSurfaceKind::File) {
        render_materialized_file_surface(app, area, frame);
        return;
    }
    if app.review.is_repository_review() {
        render_repository_file(app, area, frame);
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
    let rows = rendered_rows(file);
    if rows.is_empty() {
        render_empty(area, "No textual changes", frame);
        return;
    }
    let visible = usize::from(area.height);
    for row in 0..visible {
        let index = app.diff_scroll.saturating_add(row);
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        let row_area = Rect::new(area.x, y, area.width, 1);
        if let Some(rendered) = rows.get(index) {
            let mut line = rendered.line.clone();
            if let Some(marker) = app.draft_marker_at(app.selected_file, index) {
                line.spans
                    .insert(0, Span::styled(marker, Style::new().fg(Color::Yellow)));
            }
            let (line, style) = if index == app.selected_diff_line {
                (selected_line(&line), rendered.style.bg(Color::BrightBlack))
            } else if app.is_row_in_range_selection(app.selected_file, index) {
                (selected_line(&line), rendered.style.bg(Color::Blue))
            } else {
                (line, rendered.style)
            };
            frame.write_line_with_fallback_style(row_area, &line, style);
        }
    }
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
    let rows = file_surface_rows(file);
    if rows.is_empty() {
        render_empty(area, "No file content", frame);
        return;
    }
    let visible = usize::from(area.height);
    for row in 0..visible {
        let index = app.diff_scroll.saturating_add(row);
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        let Some((line_number, content)) = rows.get(index) else {
            break;
        };
        let mut style = if index == app.selected_diff_line {
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else if app.is_row_in_range_selection(app.selected_file, index) {
            Style::new().fg(Color::White).bg(Color::Blue)
        } else if app.has_draft_comment_at(app.selected_file, index) {
            Style::new().fg(Color::White).bg(Color::BrightBlack)
        } else {
            Style::new()
        };
        let line_number =
            line_number.map_or_else(|| "      ".to_string(), |number| format!("{number:>5} "));
        let mut line = Line::from_spans(vec![
            Span::styled(line_number, Style::new().fg(Color::BrightBlack)),
            Span::styled(content.clone(), style),
        ]);
        if let Some(marker) = app.draft_marker_at(app.selected_file, index) {
            line.spans
                .insert(0, Span::styled(marker, Style::new().fg(Color::Yellow)));
            style = style.bg(style.bg.unwrap_or(Color::BrightBlack));
        }
        frame.write_line_with_fallback_style(Rect::new(area.x, y, area.width, 1), &line, style);
    }
}

fn file_surface_rows(file: &ReviewFile) -> Vec<(Option<u32>, String)> {
    materialized_file_surface_rows(file)
}

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
    let Some(file) = app.selected_file_data() else {
        render_empty(area, "No files", frame);
        return;
    };
    let path = file.display_path();
    let Some(cached) = app.file_cache.get(path) else {
        render_empty(area, "Loading file…", frame);
        return;
    };
    if let Some(reason) = &cached.unavailable_reason {
        render_empty(area, reason, frame);
        return;
    }
    let visible = usize::from(area.height);
    for row in 0..visible {
        let index = app.diff_scroll.saturating_add(row);
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        let Some(content) = cached.line(index) else {
            break;
        };
        let mut style = if index == app.selected_diff_line {
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else if app.is_row_in_range_selection(app.selected_file, index) {
            Style::new().fg(Color::White).bg(Color::Blue)
        } else if app.has_draft_comment_at(app.selected_file, index) {
            Style::new().fg(Color::White).bg(Color::BrightBlack)
        } else {
            Style::new()
        };
        let line_number = format!("{:>5} ", index.saturating_add(1));
        let mut line = Line::from_spans(vec![
            Span::styled(line_number, Style::new().fg(Color::BrightBlack)),
            Span::styled(content.to_string(), style),
        ]);
        if let Some(marker) = app.draft_marker_at(app.selected_file, index) {
            line.spans
                .insert(0, Span::styled(marker, Style::new().fg(Color::Yellow)));
            style = style.bg(style.bg.unwrap_or(Color::BrightBlack));
        }
        frame.write_line_with_fallback_style(
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
            &line,
            style,
        );
    }
}

fn rendered_rows(file: &ReviewFile) -> Vec<RenderedRow> {
    ReviewDisplayBuilder::new()
        .syntax_highlighting(true)
        .build_file(file)
        .rows
        .iter()
        .map(render_display_row)
        .collect()
}

fn render_display_row(row: &ReviewDisplayRow) -> RenderedRow {
    match row.source {
        ReviewDisplayRowSource::HunkHeader => RenderedRow {
            line: Line::from_spans(
                row.segments
                    .iter()
                    .map(render_display_segment)
                    .collect::<Vec<_>>(),
            ),
            style: row_style(row.source),
        },
        ReviewDisplayRowSource::Context
        | ReviewDisplayRowSource::Added
        | ReviewDisplayRowSource::Removed => {
            let old = row
                .old_line
                .map_or_else(|| "    ".to_string(), |line| format!("{line:>4}"));
            let new = row
                .new_line
                .map_or_else(|| "    ".to_string(), |line| format!("{line:>4}"));
            let marker_style = row_style(row.source);
            let marker = row.source.diff_marker().unwrap_or(' ');
            let mut spans = vec![
                Span::styled(
                    format!(" {old} {new} "),
                    Style::new().fg(Color::BrightBlack),
                ),
                Span::styled(marker.to_string(), marker_style),
            ];
            spans.extend(row.segments.iter().map(render_display_segment));
            RenderedRow {
                line: Line::from_spans(spans),
                style: marker_style,
            }
        }
    }
}

fn render_display_segment(segment: &ReviewDisplaySegment) -> Span {
    Span::styled(segment.text.clone(), style_for_segment(segment))
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
        ReviewDisplayTextRole::Code => Style::new().fg(Color::White),
        ReviewDisplayTextRole::Syntax(style) => syntax_style_to_tui(*style),
        ReviewDisplayTextRole::DiffContext
        | ReviewDisplayTextRole::DiffAdded
        | ReviewDisplayTextRole::DiffRemoved => Style::new(),
        ReviewDisplayTextRole::HunkHeader => {
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
        }
    }
}

const fn row_style(source: ReviewDisplayRowSource) -> Style {
    match source {
        ReviewDisplayRowSource::HunkHeader => Style::new().fg(Color::Magenta),
        ReviewDisplayRowSource::Context => Style::new().fg(Color::White),
        ReviewDisplayRowSource::Added => Style::new().fg(Color::Green),
        ReviewDisplayRowSource::Removed => Style::new().fg(Color::Red),
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
    let build_lines = [
        " Build Review Help",
        "",
        " m                   switch to review mode",
        " j/k or arrows       move selection",
        " space              include/exclude selected source",
        " +                  add selected file source",
        " A                  add source by kind/path/range",
        " r                  rename selected source",
        " [/]                move selected source up/down",
        " -                  remove selected source",
        " f or ctrl-p         fuzzy file picker",
        " enter               inspect/open selected item",
        " t                   cycle included/repo/threads/sources",
        " b                   toggle sidebar",
        " ?                   toggle this help",
        " q or esc            exit review",
    ];
    let repo_lines = [
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
        " t                   cycle included/repo/threads/sources",
        " b                   toggle sidebar",
        " ?                   toggle this help",
        " q or esc            exit review",
    ];
    let diff_lines = [
        " Code Review Help",
        "",
        " j/k or arrows       scroll diff",
        " n/p                 next/previous file",
        " J/K                 next/previous hunk",
        " g/G                 top/bottom of file diff",
        " b                   toggle sidebar",
        " t                   cycle included/repo/threads/sources",
        " mouse wheel         scroll diff",
        " click file          open file",
        " c                   create draft comment",
        " x                   publish/export review",
        " v                   select/clear line range",
        " a                   ask Bcode about selected line",
        " o                   open linked Bcode session",
        " e                   edit latest draft on line",
        " D                   delete latest draft on line",
        " ?                   toggle this help",
        " q or esc            exit review",
    ];
    let lines: &[&str] = if app.ux_mode == super::code_review::ReviewUxMode::Build {
        &build_lines
    } else if app.review.is_repository_review() {
        &repo_lines
    } else {
        &diff_lines
    };
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
        } => render_publish_preview(publisher_id, preview, *scroll, popup, frame),
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
    options: &[super::code_review::ReviewPublishOption],
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
            " Publisher options  Enter preview  Tab next  Esc cancel ",
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
        let text = format!(" {}: {}", option.label, option.value);
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
            format!(" Preview {publisher_id}  Enter submit  Esc cancel "),
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
            " Draft comment ",
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
            " enter/ctrl+s save  esc cancel ",
            Style::new().fg(Color::Black).bg(Color::Yellow),
        )]),
    );
}

fn prompt_popup_height(kind: ReviewPromptKind, area: Rect) -> u16 {
    match kind {
        ReviewPromptKind::FilePicker | ReviewPromptKind::AddSourceKind => area.height.min(16),
        ReviewPromptKind::JumpToLine
        | ReviewPromptKind::FileSearch
        | ReviewPromptKind::AddCommitSource
        | ReviewPromptKind::AddCommitRangeSource
        | ReviewPromptKind::AddBranchCompareSource
        | ReviewPromptKind::AddFileSource
        | ReviewPromptKind::AddFileRangeSource
        | ReviewPromptKind::RenameSource => area.height.min(5),
    }
}

const fn prompt_title(kind: ReviewPromptKind) -> &'static str {
    match kind {
        ReviewPromptKind::FilePicker => " Open file ",
        ReviewPromptKind::JumpToLine => " Jump to line ",
        ReviewPromptKind::FileSearch => " Search file ",
        ReviewPromptKind::AddSourceKind => " Add source ",
        ReviewPromptKind::AddCommitSource => " Add commit ",
        ReviewPromptKind::AddCommitRangeSource => " Add range ",
        ReviewPromptKind::AddBranchCompareSource => " Add branch compare ",
        ReviewPromptKind::AddFileSource => " Add file ",
        ReviewPromptKind::AddFileRangeSource => " Add file range ",
        ReviewPromptKind::RenameSource => " Rename source ",
    }
}

fn render_add_source_menu(
    prompt: &super::code_review::ReviewPromptState,
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
    let height = prompt_popup_height(prompt.kind, area);
    if width < 20 || height < 3 {
        return;
    }
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    let popup = Rect::new(x, y, width, height);
    frame.fill(popup, " ", Style::new().fg(Color::White).bg(Color::Black));
    let title = prompt_title(prompt.kind);
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
    if prompt.kind == ReviewPromptKind::AddSourceKind {
        render_add_source_menu(prompt, popup, height, frame);
    }
    if prompt.kind == ReviewPromptKind::FilePicker {
        let matches = app.file_picker_matches(query);
        for (row, index) in matches
            .into_iter()
            .take(usize::from(height.saturating_sub(3)))
            .enumerate()
        {
            let style = if row == prompt.selected {
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
                    truncate_to_display_width(
                        app.review.files[index].display_path(),
                        usize::from(popup.width.saturating_sub(2)),
                    ),
                    style,
                )]),
            );
        }
    }
    frame.write_line(
        Rect::new(
            popup.x.saturating_add(1),
            popup.bottom().saturating_sub(1),
            popup.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            prompt_footer_text(prompt.kind),
            Style::new().fg(Color::Black).bg(Color::Yellow),
        )]),
    );
}

const fn prompt_footer_text(kind: ReviewPromptKind) -> &'static str {
    match kind {
        ReviewPromptKind::AddSourceKind => {
            " add source  ↑/↓ choose  enter select  type shortcut: worktree, staged, file, range, branch  esc cancel "
        }
        ReviewPromptKind::AddCommitRangeSource | ReviewPromptKind::AddBranchCompareSource => {
            " enter base..head or base...head  esc cancel "
        }
        ReviewPromptKind::AddFileRangeSource => " enter path:start-end  esc cancel ",
        ReviewPromptKind::FilePicker => " ↑/↓ choose  enter open  esc cancel ",
        _ => " enter submit  esc cancel ",
    }
}

struct RenderedRow {
    line: Line,
    style: Style,
}
