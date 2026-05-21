//! BMUX backend provider picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::Modifier;

use super::picker_render::{render_picker_list, render_picker_panel};
use super::provider_picker::ProviderPickerApp;

/// Render the provider picker.
pub(super) fn render_provider_picker(app: &mut ProviderPickerApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let inner = render_picker_panel(" Providers ", area, frame);
    frame.write_line(
        Rect::new(inner.x, inner.y, inner.width, 1),
        &Line::from_spans(vec![
            Span::styled(
                "Select model provider",
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  Enter selects  Esc cancels"),
        ]),
    );
    let filter = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    TextInput::new(app.filter())
        .placeholder("Filter providers")
        .render(filter, frame);
    let list_y = filter.y.saturating_add(2);
    if inner.bottom() <= list_y {
        return;
    }
    let list_area = Rect::new(inner.x, list_y, inner.width, inner.bottom() - list_y);
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
}
