//! BMUX backend model picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::model_picker::ModelPickerApp;
use super::picker_render::{render_picker_list, render_picker_panel};

/// Render the model picker.
pub(super) fn render_model_picker(app: &mut ModelPickerApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let inner = render_picker_panel(" Models ", area, frame);
    frame.write_line(
        Rect::new(inner.x, inner.y, inner.width, 1),
        &Line::from_spans(vec![
            Span::styled("Select model", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  Enter selects  Esc cancels"),
        ]),
    );
    let filter = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    TextInput::new(app.filter())
        .placeholder("Filter models")
        .render(filter, frame);
    let status = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        u16::from(inner.height > 0),
    );
    frame.write_line(
        status,
        &Line::from_spans(vec![Span::styled(
            app.status().to_owned(),
            Style::new().fg(Color::BrightBlack),
        )]),
    );
    let list_y = filter.y.saturating_add(2);
    if status.y <= list_y {
        return;
    }
    let list_area = Rect::new(inner.x, list_y, inner.width, status.y - list_y);
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
}
