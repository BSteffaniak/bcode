//! Rendering for full-screen code review mode.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::truncate_to_display_width;

use super::code_review::{ReviewApp, ReviewFile, ReviewLine, ReviewLineKind, sidebar_width};

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
        render_files(app, file_area, frame);
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
    render_diff(app, diff_area, frame);

    if app.help_visible {
        render_help(area, frame);
    }
    if app.comment_editor.is_some() {
        render_comment_editor(app, area, frame);
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
    let (hunk, hunk_total) = app.hunk_position();
    let drafts = app.draft_comment_count();
    let draft_label = if drafts == 0 {
        String::new()
    } else {
        format!("  💬 {drafts} draft")
    };
    let text = format!(
        " bcode review  {}  {}  File {}  Hunk {}/{}{}  +{} -{} ",
        app.review.title,
        file_label,
        file_position,
        hunk,
        hunk_total,
        draft_label,
        app.review.additions,
        app.review.deletions
    );
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
            if let Some(preview) = app.selected_draft_preview() {
                let linked = app
                    .selected_draft_session_id()
                    .map_or(String::new(), |_| "  🤖 session linked".to_string());
                return format!(" {preview}{linked}  a ask/follow up  o open  e edit  D delete latest draft ");
            }
            format!(
                " j/k scroll  n/p file  J/K hunk  c comment  v range  a ask Bcode  o open session  e edit  D delete draft  b sidebar:{sidebar}  ? {help}  q exit "
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

fn render_files(app: &mut ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let visible_rows = usize::from(area.height);
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

fn render_diff(app: &ReviewApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
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
            if app.has_draft_comment_at(app.selected_file, index) {
                line.spans
                    .insert(0, Span::styled("💬", Style::new().fg(Color::Yellow)));
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

fn rendered_rows(file: &ReviewFile) -> Vec<RenderedRow> {
    let mut rows = Vec::new();
    for hunk in &file.hunks {
        let heading = hunk.heading.as_deref().unwrap_or_default();
        rows.push(RenderedRow {
            line: Line::from_spans(vec![Span::styled(
                format!(
                    "@@ -{},{} +{},{} @@ {}",
                    hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count, heading
                ),
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            )]),
            style: Style::new().fg(Color::Magenta),
        });
        rows.extend(hunk.lines.iter().map(render_diff_line));
    }
    rows
}

fn render_diff_line(line: &ReviewLine) -> RenderedRow {
    let (marker, style) = match line.kind {
        ReviewLineKind::Context => (' ', Style::new().fg(Color::White)),
        ReviewLineKind::Added => ('+', Style::new().fg(Color::Green)),
        ReviewLineKind::Removed => ('-', Style::new().fg(Color::Red)),
    };
    let old = line
        .old_line
        .map_or_else(|| "    ".to_string(), |line| format!("{line:>4}"));
    let new = line
        .new_line
        .map_or_else(|| "    ".to_string(), |line| format!("{line:>4}"));
    RenderedRow {
        line: Line::from_spans(vec![
            Span::styled(
                format!(" {old} {new} "),
                Style::new().fg(Color::BrightBlack),
            ),
            Span::styled(marker.to_string(), style),
            Span::styled(line.content.clone(), style),
        ]),
        style,
    }
}

fn render_help(area: Rect, frame: &mut Frame<'_>) {
    let width = area.width.min(68);
    let height = 17;
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
    let lines = [
        " Code Review Help",
        "",
        " j/k or arrows       scroll diff",
        " n/p or left/right   next/previous file",
        " J/K                 next/previous hunk",
        " g/G                 top/bottom of file diff",
        " b                   toggle file sidebar",
        " mouse wheel         scroll diff",
        " click file          open file",
        " c                   create draft comment",
        " v                   select/clear line range",
        " a                   ask Bcode about selected line",
        " o                   open linked Bcode session",
        " e                   edit latest draft on line",
        " D                   delete latest draft on line",
        " ?                   toggle this help",
        " q or esc            exit review",
    ];
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

struct RenderedRow {
    line: Line,
    style: Style,
}
