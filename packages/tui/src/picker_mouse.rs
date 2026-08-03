//! Shared picker mouse helpers for the TUI.

use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};

/// Resolve a command palette row from a mouse down event within committed palette geometry.
#[must_use]
pub fn command_palette_row_in_area(
    mouse: MouseEvent,
    area: bmux_tui::geometry::Rect,
) -> Option<usize> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    let inner = area.inset(bmux_tui::geometry::Insets::new(2, 2, 2, 2));
    if !inner.contains(mouse.position) {
        return None;
    }
    Some(usize::from(mouse.position.y.saturating_sub(inner.y)))
}

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
