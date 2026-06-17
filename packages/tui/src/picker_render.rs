//! Shared rendering helpers for TUI pickers.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::list::{List, ListItem, ListState};
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputState;

use super::render::TuiTheme;
use super::text_input_flow;

const PICKER_BG: Color = Color::Black;

const fn picker_style() -> Style {
    Style::new().bg(PICKER_BG)
}

/// Return the standard picker opaque base style.
pub const fn picker_base_style() -> Style {
    picker_style()
}

/// Render standard picker panel chrome and return `(inner_area, list_start_y)`.
pub fn render_picker_chrome(
    title: &'static str,
    header: &Line,
    input: &mut TextInputState,
    placeholder: &'static str,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) -> Option<(Rect, u16)> {
    let area = frame.area();
    if area.is_empty() {
        return None;
    }

    let inner = render_picker_panel(title, area, frame, theme);
    frame.write_line_with_fallback_style(
        Rect::new(inner.x, inner.y, inner.width, 1),
        header,
        picker_style(),
    );
    let input_area = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    input.set_content_area(input_area, &text_input_flow::single_line_policy());
    TextInput::new(input.buffer())
        .style(picker_style())
        .selection_style(Style::new().fg(Color::Black).bg(Color::Yellow))
        .placeholder(placeholder)
        .placeholder_style(Style::new().fg(Color::BrightBlack).bg(PICKER_BG))
        .vertical_scroll(input.vertical_scroll())
        .render(input_area, frame);
    Some((inner, input_area.y.saturating_add(2)))
}

/// Render a standard picker status line and return its row.
pub fn render_picker_status(inner: Rect, text: &str, style: Style, frame: &mut Frame<'_>) -> u16 {
    let y = inner.bottom().saturating_sub(1);
    frame.write_line_with_fallback_style(
        Rect::new(inner.x, y, inner.width, u16::from(inner.height > 0)),
        &Line::from_spans(vec![Span::styled(text.to_owned(), style)]),
        picker_style(),
    );
    y
}

/// Return list area between a picker content row and bottom row.
pub fn picker_list_area(inner: Rect, list_y: u16, bottom_y: u16) -> Option<Rect> {
    (bottom_y > list_y).then_some(Rect::new(inner.x, list_y, inner.width, bottom_y - list_y))
}

/// Render a standard picker panel and return its inner area.
pub fn render_picker_panel(
    title: &'static str,
    area: Rect,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) -> Rect {
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(theme.accent).bg(PICKER_BG)))
        .title(title)
        .padding(Insets::new(1, 1, 1, 1))
        .background(picker_style());
    panel.render(area, frame);
    panel.inner_area(area)
}

/// Render a standard selectable list and persist scroll state.
pub fn render_picker_list(
    items: &[ListItem],
    state: &mut ListState,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let mut render_state = *state;
    render_state.ensure_selected_visible(area.height, items.len());
    List::new(items)
        .style(picker_style())
        .selected_style(
            Style::new()
                .fg(Color::White)
                .bg(Color::Rgb(38, 52, 64))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ")
        .render(area, frame, &mut render_state);
    *state = render_state;
}
