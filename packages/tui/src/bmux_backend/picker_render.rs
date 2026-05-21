//! Shared rendering helpers for BMUX backend pickers.

use bmux_text_edit::TextEditBuffer;
use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::list::{List, ListItem, ListState};
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::Color;

/// Render standard picker panel chrome and return `(inner_area, list_start_y)`.
pub(super) fn render_picker_chrome(
    title: &'static str,
    header: &Line,
    input: &TextEditBuffer,
    placeholder: &'static str,
    frame: &mut Frame<'_>,
) -> Option<(Rect, u16)> {
    let area = frame.area();
    if area.is_empty() {
        return None;
    }

    let inner = render_picker_panel(title, area, frame);
    frame.write_line(Rect::new(inner.x, inner.y, inner.width, 1), header);
    let input_area = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    TextInput::new(input)
        .placeholder(placeholder)
        .render(input_area, frame);
    Some((inner, input_area.y.saturating_add(2)))
}

/// Render a standard picker status line and return its row.
pub(super) fn render_picker_status(
    inner: Rect,
    text: &str,
    style: Style,
    frame: &mut Frame<'_>,
) -> u16 {
    let y = inner.bottom().saturating_sub(1);
    frame.write_line(
        Rect::new(inner.x, y, inner.width, u16::from(inner.height > 0)),
        &Line::from_spans(vec![Span::styled(text.to_owned(), style)]),
    );
    y
}

/// Return list area between a picker content row and bottom row.
pub(super) fn picker_list_area(inner: Rect, list_y: u16, bottom_y: u16) -> Option<Rect> {
    (bottom_y > list_y).then_some(Rect::new(inner.x, list_y, inner.width, bottom_y - list_y))
}

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
