//! Shared rendering helpers for BMUX backend pickers.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::list::{List, ListItem, ListState};
use bmux_tui::prelude::{StatefulWidget, Style, Widget};
use bmux_tui::style::Color;

/// Render a standard picker panel and return its inner area.
pub(super) fn render_picker_panel(title: &'static str, area: Rect, frame: &mut Frame<'_>) -> Rect {
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(title)
        .padding(Insets::new(1, 1, 1, 1));
    panel.render(area, frame);
    panel.inner_area(area)
}

/// Render a standard selectable list and persist scroll state.
pub(super) fn render_picker_list(
    items: &[ListItem],
    state: &mut ListState,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let mut render_state = *state;
    render_state.ensure_selected_visible(area.height, items.len());
    List::new(items)
        .highlight_symbol("> ")
        .render(area, frame, &mut render_state);
    *state = render_state;
}
