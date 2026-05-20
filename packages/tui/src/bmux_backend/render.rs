//! BMUX backend rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_block::{TextBlock, TextWrap};

use super::app::BmuxApp;

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

    let body_height = composer.y.saturating_sub(area.y.saturating_add(1));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    render_body(body, frame);
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

fn render_body(area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    TextBlock::new("BMUX backend skeleton is running. Composer input is local-only until parity migration continues.")
        .wrap(TextWrap::Character)
        .render(area.inset(Insets::all(1)), frame);
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
        .placeholder("Type here; Enter clears local draft")
        .render(panel.inner_area(area), frame);
}
