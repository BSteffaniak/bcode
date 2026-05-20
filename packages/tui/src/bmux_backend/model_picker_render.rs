//! BMUX backend model picker rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::list::List;
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::model_picker::ModelPickerApp;

/// Render the model picker.
pub(super) fn render_model_picker(app: &mut ModelPickerApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(" Models ")
        .padding(Insets::new(1, 1, 1, 1));
    panel.render(area, frame);
    let inner = panel.inner_area(area);
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
    let mut state = *app.list_state_mut();
    state.ensure_selected_visible(list_area.height, items.len());
    List::new(&items)
        .highlight_symbol("> ")
        .render(list_area, frame, &mut state);
    *app.list_state_mut() = state;
}
