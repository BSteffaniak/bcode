//! Shared picker mouse helpers for the BMUX backend.

use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};

/// Resolve a picker list row from a mouse down event.
#[must_use]
pub(super) fn picker_row_from_mouse(mouse: MouseEvent) -> Option<usize> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return None,
    }
    usize::from(mouse.position.y).checked_sub(5)
}
