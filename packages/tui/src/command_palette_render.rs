//! TUI command palette rendering.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::PaintCx;
use bmux_tui_components::picker_frame::{
    PickerFrame, PickerFrameComponent, PickerFramePolicy, PickerFrameStyles,
};

use super::command_palette::BmuxCommandPalette;
use super::render::TuiTheme;

/// Render a command palette overlay.
pub fn render_palette(
    palette: &mut BmuxCommandPalette,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    let shell = command_palette_frame().styles(command_palette_styles(theme));
    let query = std::cell::RefCell::new(palette.query_mut().clone());
    let policy = super::text_input_flow::single_line_policy();
    let input =
        bmux_tui_components::text_input::TextInputComponent::new("commands.input", &query, &policy)
            .style(theme.text)
            .selection_style(theme.selection)
            .focused(true);
    let component = PickerFrameComponent::new("commands", shell, TextBlock::new("")).input(input);
    let layout = component.layout(
        Constraints::tight(Size::new(frame.area().width, frame.area().height)),
        &mut LayoutCx::new(),
    );
    component.paint(&layout, frame);
    let panel = &layout.children[0];
    let list = panel.node.children.last().expect("picker list");
    let area = Rect::new(
        panel.x + list.x,
        u16::try_from(panel.y + list.y).unwrap_or(u16::MAX),
        list.node.size.width,
        u16::try_from(list.node.size.height).unwrap_or(u16::MAX),
    );
    let items = palette.visible_items(theme.muted);
    super::picker_render::render_picker_list(
        &items,
        palette.render_state(area.height),
        area,
        frame,
        theme,
    );
    let input = &panel.node.children[0];
    let input_area = Rect::new(
        panel.x + input.x,
        u16::try_from(panel.y + input.y).unwrap_or(u16::MAX),
        input.node.size.width,
        u16::try_from(input.node.size.height).unwrap_or(u16::MAX),
    );
    drop(component);
    *palette.query_mut() = query.into_inner();
    palette.query_mut().set_content_area(input_area, &policy);
}

/// Return the command palette's interactive list area.
#[must_use]
pub fn palette_list_area(area: Rect) -> Rect {
    let component =
        PickerFrameComponent::new("commands", command_palette_frame(), TextBlock::new(""));
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    let panel = &layout.children[0];
    let list = panel.node.children.last().expect("picker list");
    Rect::new(
        area.x + panel.x + list.x,
        area.y + u16::try_from(panel.y + list.y).unwrap_or(u16::MAX),
        list.node.size.width,
        u16::try_from(list.node.size.height).unwrap_or(u16::MAX),
    )
}

const fn command_palette_frame() -> PickerFrame<'static> {
    PickerFrame::new().title(" Commands ").policy(
        PickerFramePolicy::palette()
            .placement(bmux_tui_components::picker_frame::PickerFramePlacement::UpperThird)
            .max_size(Size::new(72, 12)),
    )
}

const fn command_palette_styles(theme: TuiTheme) -> PickerFrameStyles {
    PickerFrameStyles {
        border: theme.focused,
        background: theme.raised,
        header: theme.raised,
        input: theme.raised,
        list: theme.raised,
        footer: theme.raised,
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::geometry::Rect;

    use super::palette_list_area;

    #[test]
    fn palette_hit_area_comes_from_picker_frame_layout() {
        assert_eq!(
            palette_list_area(Rect::new(0, 0, 80, 24)),
            Rect::new(6, 8, 68, 6)
        );
    }
}
