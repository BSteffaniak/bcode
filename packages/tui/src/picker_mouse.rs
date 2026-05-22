//! Shared picker mouse helpers for the TUI.

use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};

/// Resolve a command palette row from a mouse down event.
#[must_use]
pub fn command_palette_row_from_mouse(mouse: MouseEvent) -> Option<usize> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    usize::from(mouse.position.y).checked_sub(3)
}

/// Resolve a picker list row from a mouse down event.
#[must_use]
pub fn picker_row_from_mouse(mouse: MouseEvent) -> Option<usize> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    usize::from(mouse.position.y).checked_sub(5)
}
