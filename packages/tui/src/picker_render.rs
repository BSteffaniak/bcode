//! Shared rendering helpers for TUI pickers.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui_components::picker_frame::{
    PickerFrame, PickerFrameComponent, PickerFramePolicy, PickerFrameStyles,
};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListState, SelectableListStyles,
};
use bmux_tui_components::text_input::TextInputComponent;
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
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) -> Option<(Rect, u16)> {
    let area = Rect::new(0, 0, frame.area().width, frame.area().height);
    if area.is_empty() {
        return None;
    }

    let inner = render_picker_panel(title, area, frame, theme);
    frame.write_line_with_fallback_style(
        LocalRect::terminal(Rect::new(inner.x, inner.y, inner.width, 1)),
        header,
        theme.text,
    );
    let input_area = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    input.set_content_area(input_area, &text_input_flow::single_line_policy());
    paint_picker_input(input, input_area, placeholder, frame, theme);
    Some((inner, input_area.y.saturating_add(2)))
}

/// Paint a measured picker editor and retain its terminal input area.
pub fn paint_picker_input(
    input: &mut TextInputState,
    input_area: Rect,
    placeholder: &str,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    let retained = std::cell::RefCell::new(input.clone());
    let policy = text_input_flow::single_line_policy();
    let editor = TextInputComponent::new("picker.input", &retained, &policy)
        .style(theme.text)
        .selection_style(theme.selection)
        .placeholder(placeholder, theme.muted)
        .focused(true);
    let layout = editor.layout(Constraints::tight(input_area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(input_area.x),
        i64::from(input_area.y),
        LocalRect::new(0, 0, input_area.width, input_area.height),
        |cx| editor.paint(&layout, cx),
    );
    *input = retained.into_inner();
    input.set_content_area(input_area, &policy);
}

/// Render a standard picker status line and return its row.
pub fn render_picker_status(
    inner: Rect,
    text: &str,
    style: Style,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) -> u16 {
    let y = inner.bottom().saturating_sub(1);
    frame.write_line_with_fallback_style(
        LocalRect::terminal(Rect::new(
            inner.x,
            y,
            inner.width,
            u16::from(inner.height > 0),
        )),
        &Line::from_spans(vec![Span::styled(text.to_owned(), style)]),
        theme.text,
    );
    y
}

/// Return list area between a picker content row and bottom row.
pub const fn picker_list_area(inner: Rect, list_y: u16, bottom_y: u16) -> Option<Rect> {
    if bottom_y > list_y {
        Some(Rect::new(inner.x, list_y, inner.width, bottom_y - list_y))
    } else {
        None
    }
}

/// Render a standard picker panel and return its inner area.
pub fn render_picker_panel(
    title: &'static str,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) -> Rect {
    let shell = PickerFrame::new()
        .title(title)
        .policy(PickerFramePolicy {
            chrome: true,
            background: true,
            header: false,
            input: false,
            footer: false,
            margin: Insets::all(0),
            padding: Insets::new(1, 1, 1, 1),
            min_size: Size::new(area.width, area.height),
            max_size: Size::new(area.width, area.height),
            placement: bmux_tui_components::picker_frame::PickerFramePlacement::Center,
        })
        .styles(PickerFrameStyles {
            border: theme.border,
            background: theme.raised,
            header: theme.text,
            input: theme.text,
            list: theme.raised,
            footer: theme.text,
        });
    let component = PickerFrameComponent::new("picker", shell, TextBlock::new(""));
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
    let panel = &layout.children[0];
    let child = &panel.node.children[0];
    Rect::new(
        area.x.saturating_add(panel.x).saturating_add(child.x),
        area.y
            .saturating_add(u16::try_from(panel.y + child.y).unwrap_or(u16::MAX)),
        child.node.size.width,
        u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
    )
}

/// Render a standard selectable list using caller-synchronized render state.
pub fn render_picker_list(
    items: &[Line],
    state: &SelectableListState,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    let rows = items
        .iter()
        .enumerate()
        .map(|(i, line)| SelectableListItem::rich(i.to_string(), line.clone()))
        .collect::<Vec<_>>();
    SelectableList::new(&rows)
        .styles(SelectableListStyles {
            normal: theme.text,
            focused: theme.selection,
            selected: theme.selection,
            hovered: theme.focused,
            pressed: theme.selection,
            disabled: theme.muted,
            background: theme.raised,
            scrollbar: bmux_tui_components::scrollbar::ScrollbarStyles::default(),
        })
        .paint(area, state, theme.text, frame);
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::render_picker_panel;
    use crate::render::TuiTheme;

    #[test]
    fn picker_list_area_rejects_inverted_bounds_without_underflow() {
        assert_eq!(super::picker_list_area(Rect::new(2, 2, 20, 4), 4, 3), None);
        assert_eq!(
            super::picker_list_area(Rect::new(2, 2, 20, 4), 3, 5),
            Some(Rect::new(2, 3, 20, 2))
        );
    }

    #[test]
    fn picker_panel_chrome_tracks_terminal_native_dark_and_light_themes() {
        let mut observed = Vec::new();
        for theme_id in ["terminal-native", "bcode-dark", "bcode-light"] {
            let theme = TuiTheme::for_theme_id(theme_id);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 8));
            let mut frame = Frame::new(&mut buffer);
            let inner = render_picker_panel(
                " Picker ",
                frame.area(),
                &mut bmux_tui::paint::PaintCx::new(&mut frame),
                theme,
            );
            assert_eq!(inner, Rect::new(2, 2, 20, 4));
            assert_eq!(
                frame.buffer().get(Point::new(0, 0)).expect("border").style,
                theme.raised.patch(theme.border),
                "{theme_id} border"
            );
            assert_eq!(
                frame.buffer().get(Point::new(1, 1)).expect("body").style,
                theme.raised,
                "{theme_id} body"
            );
            observed.push((theme_id, theme.raised, theme.border));
        }

        assert!(observed[0].1.bg.is_none());
        assert_ne!(observed[1].1, observed[2].1);
        assert_ne!(observed[1].2, observed[2].2);
    }
}
