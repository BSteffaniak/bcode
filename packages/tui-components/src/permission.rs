//! Bcode permission-dialog presentation recipe.

use bmux_tui::geometry::{Insets, Size};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

/// Build the standard Bcode permission-request modal shell.
#[must_use]
pub fn permission_modal(theme: ModalTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(48, 12), Size::new(100, 24), Insets::all(4)),
        theme,
    )
    .title(" Permission requested ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::geometry::Rect;
    use bmux_tui::prelude::Style;

    #[test]
    fn permission_modal_remains_bounded_in_small_terminals() {
        let terminal = Rect::new(0, 0, 40, 10);
        let theme = ModalTheme::new(
            Style::new(),
            Style::new(),
            Style::new(),
            Style::new(),
            Style::new(),
            Style::new(),
        );
        let area = permission_modal(theme).panel_area(terminal);
        assert!(area.width <= terminal.width);
        assert!(area.height <= terminal.height);
    }
}
