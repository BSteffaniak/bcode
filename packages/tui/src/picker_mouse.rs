//! Shared picker mouse helpers for the TUI.

use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};

/// Resolve a command palette row from a mouse down event within its rendered list area.
#[must_use]
pub fn command_palette_row_in_area(
    mouse: MouseEvent,
    list_area: bmux_tui::geometry::Rect,
) -> Option<usize> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    if !list_area.contains(mouse.position) {
        return None;
    }
    Some(usize::from(mouse.position.y.saturating_sub(list_area.y)))
}

/// Resolve a picker list row from a mouse down event.
#[must_use]
pub fn picker_row_from_mouse(mouse: MouseEvent) -> Option<usize> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    usize::from(mouse.position.y).checked_sub(5)
}
