//! Cursor blink state for the BMUX backend composer.

use std::time::Instant;

use super::IDLE_REDRAW_INTERVAL;

/// Composer cursor blink state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CursorBlink {
    visible: bool,
    last_toggle: Instant,
}

impl CursorBlink {
    /// Create visible cursor blink state.
    #[must_use]
    pub(super) fn new() -> Self {
        Self {
            visible: true,
            last_toggle: Instant::now(),
        }
    }

    /// Return whether the cursor should be visible.
    #[must_use]
    pub(super) const fn visible(&self) -> bool {
        self.visible
    }

    /// Reset cursor blink state after input.
    pub(super) fn wake(&mut self) {
        self.visible = true;
        self.last_toggle = Instant::now();
    }

    /// Advance time-based cursor blink state.
    pub(super) fn tick(&mut self) -> bool {
        if self.last_toggle.elapsed() < IDLE_REDRAW_INTERVAL {
            return false;
        }
        self.visible = !self.visible;
        self.last_toggle = Instant::now();
        true
    }
}
