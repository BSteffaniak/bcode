//! BMUX backend rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_block::{TextBlock, TextWrap};

use super::app::{BmuxApp, TranscriptItem};

/// Render one BMUX backend frame.
pub(super) fn render(app: &BmuxApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    render_header(app, header, frame);

    let composer_height = area.height.clamp(3, 6);
    let composer = Rect::new(
        area.x,
        area.bottom().saturating_sub(composer_height),
        area.width,
        composer_height,
    );
    render_composer(app, composer, frame);

    let body_height = composer.y.saturating_sub(area.y.saturating_add(2));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    render_body(app, body, frame);

    let status = Rect::new(
        area.x,
        composer.y.saturating_sub(1),
        area.width,
        u16::from(composer.y > area.y.saturating_add(1)),
    );
    render_status(app, status, frame);
}

fn render_header(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    let session = app
        .session_id()
        .map_or_else(|| String::from("new session"), |id| id.to_string());
    let line = Line::from_spans(vec![
        Span::styled("Bcode BMUX TUI", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(session, Style::new().fg(Color::BrightBlack)),
        Span::raw("  Esc/Ctrl-C exits"),
    ]);
    frame.write_line(area, &line);
}

fn render_body(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    if app.transcript().is_empty() {
        TextBlock::new(
            "BMUX backend is attached. Composer submissions are sent to the active Bcode session; live transcript events will appear here.",
        )
        .wrap(TextWrap::Character)
        .render(area.inset(Insets::all(1)), frame);
        return;
    }

    let transcript = app.transcript();
    let visible_items = usize::from(area.height).max(1);
    let start = transcript.len().saturating_sub(visible_items);
    let mut y = area.y;
    for item in &transcript[start..] {
        if y >= area.bottom() {
            break;
        }
        y = render_transcript_item(
            item,
            Rect::new(area.x, y, area.width, area.bottom() - y),
            frame,
        );
    }
}

fn render_transcript_item(item: &TranscriptItem, area: Rect, frame: &mut Frame<'_>) -> u16 {
    if area.is_empty() {
        return area.y;
    }
    let role_style = role_style(item.role());
    let marker = if item.streaming() { " …" } else { "" };
    let line = Line::from_spans(vec![
        Span::styled(format!("{}{}", item.role(), marker), role_style),
        Span::raw(": "),
        Span::raw(first_line(item.text())),
    ]);
    frame.write_line(Rect::new(area.x, area.y, area.width, 1), &line);

    let mut y = area.y.saturating_add(1);
    for continuation in item.text().lines().skip(1) {
        if y >= area.bottom() {
            break;
        }
        frame.write_line(
            Rect::new(area.x.saturating_add(2), y, area.width.saturating_sub(2), 1),
            &Line::from_spans(vec![Span::raw(continuation.to_owned())]),
        );
        y = y.saturating_add(1);
    }
    y
}

fn render_status(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let line = Line::from_spans(vec![Span::styled(
        app.status().to_owned(),
        Style::new().fg(Color::BrightBlack),
    )]);
    frame.write_line(area, &line);
}

fn render_composer(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(" Composer ")
        .padding(Insets::new(0, 1, 0, 1));
    panel.render(area, frame);
    TextInput::new(app.composer())
        .placeholder("Type here; Enter sends")
        .cursor_visible(app.cursor_visible())
        .render(panel.inner_area(area), frame);
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .map_or_else(String::new, ToOwned::to_owned)
}

fn role_style(role: &str) -> Style {
    match role {
        "You" => Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        "Assistant" => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        "Tool" => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        "Tool error" | "Skill error" => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        "Permission" => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        "Reasoning" => Style::new().fg(Color::BrightBlack),
        _ => Style::new()
            .fg(Color::BrightBlack)
            .add_modifier(Modifier::BOLD),
    }
}
