//! BMUX backend provider picker rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::list::List;
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::provider_picker::ProviderPickerApp;

/// Render the provider picker.
pub(super) fn render_provider_picker(app: &mut ProviderPickerApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(" Providers ")
        .padding(Insets::new(1, 1, 1, 1));
    panel.render(area, frame);
    let inner = panel.inner_area(area);
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
    let mut state = *app.list_state_mut();
    state.ensure_selected_visible(list_area.height, items.len());
    List::new(&items)
        .highlight_symbol("> ")
        .render(list_area, frame, &mut state);
    *app.list_state_mut() = state;
}
