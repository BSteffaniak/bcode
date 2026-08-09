//! TUI command palette rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::palette::CommandPalette;
use bmux_tui::prelude::StatefulWidget;
use bmux_tui_components::picker_frame::{PickerFrame, PickerFramePolicy, PickerFrameStyles};

use super::command_palette::BmuxCommandPalette;
use super::render::TuiTheme;

/// Render a command palette overlay.
pub fn render_palette(palette: &mut BmuxCommandPalette, frame: &mut Frame<'_>, theme: TuiTheme) {
    let layout = command_palette_frame()
        .styles(command_palette_styles(theme))
        .render(frame.area(), frame);
    let items = palette.cloned_items(theme.muted);
    let widget = CommandPalette::new(&items).empty("No matching commands");
    widget.render(layout.list, frame, palette.state_mut());
}

/// Return the command palette's interactive list area.
#[must_use]
pub fn palette_list_area(area: Rect) -> Rect {
    command_palette_frame().layout(area).list
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
