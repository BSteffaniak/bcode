//! Shared rendering helpers for TUI pickers.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::list::{List, ListItem, ListState};
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui_components::text_input::TextInputState;

use super::render::TuiTheme;
use super::text_input_flow;

/// Return the standard picker base style.
#[must_use]
pub const fn picker_base_style(theme: TuiTheme) -> Style {
    theme.text
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
        theme.text,
    );
    let input_area = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    input.set_content_area(input_area, &text_input_flow::single_line_policy());
    TextInput::new(input.buffer())
        .style(theme.text)
        .selection_style(theme.selection)
        .placeholder(placeholder)
        .placeholder_style(theme.muted)
        .vertical_scroll(input.vertical_scroll())
        .render(input_area, frame);
    Some((inner, input_area.y.saturating_add(2)))
}

/// Render a standard picker status line and return its row.
pub fn render_picker_status(
    inner: Rect,
    text: &str,
    style: Style,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) -> u16 {
    let y = inner.bottom().saturating_sub(1);
    frame.write_line_with_fallback_style(
        Rect::new(inner.x, y, inner.width, u16::from(inner.height > 0)),
        &Line::from_spans(vec![Span::styled(text.to_owned(), style)]),
        theme.text,
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
        .border(Border::single().style(theme.border))
        .title(title)
        .padding(Insets::new(1, 1, 1, 1))
        .background(theme.text);
    panel.render(area, frame);
    panel.inner_area(area)
}

/// Render a standard selectable list and persist scroll state.
pub fn render_picker_list(
    items: &[ListItem],
    state: &mut ListState,
    area: Rect,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let mut render_state = *state;
    render_state.ensure_selected_visible(area.height, items.len());
    List::new(items)
        .style(theme.text)
        .selected_style(theme.selection)
        .highlight_symbol("> ")
        .render(area, frame, &mut render_state);
    *state = render_state;
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::render_picker_panel;
    use crate::render::TuiTheme;

    #[test]
    fn picker_panel_chrome_tracks_terminal_native_dark_and_light_themes() {
        let mut observed = Vec::new();
        for theme_id in ["terminal-native", "bcode-dark", "bcode-light"] {
            let theme = TuiTheme::for_theme_id(theme_id);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 8));
            let mut frame = Frame::new(&mut buffer);
            let inner = render_picker_panel(" Picker ", frame.area(), &mut frame, theme);
            assert_eq!(inner, Rect::new(2, 2, 20, 4));
            assert_eq!(
                frame.buffer().get(Point::new(0, 0)).expect("border").style,
                theme.border,
                "{theme_id} border"
            );
            assert_eq!(
                frame.buffer().get(Point::new(1, 1)).expect("body").style,
                theme.text,
                "{theme_id} body"
            );
            observed.push((theme_id, theme.text, theme.border));
        }

        assert!(observed[0].1.bg.is_none());
        assert_ne!(observed[1].1, observed[2].1);
        assert_ne!(observed[1].2, observed[2].2);
    }
}
