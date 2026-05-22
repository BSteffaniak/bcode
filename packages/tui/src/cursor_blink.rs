//! Cursor blink state for the TUI composer.

use std::time::Instant;

use super::IDLE_REDRAW_INTERVAL;

/// Composer cursor blink state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorBlink {
    visible: bool,
    last_toggle: Instant,
}

impl CursorBlink {
    /// Create visible cursor blink state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            visible: true,
            last_toggle: Instant::now(),
        }
    }

    /// Return whether the cursor should be visible.
    #[must_use]
    pub const fn visible(&self) -> bool {
        self.visible
    }

    /// Reset cursor blink state after input.
    pub fn wake(&mut self) {
        self.visible = true;
        self.last_toggle = Instant::now();
    }

    /// Advance time-based cursor blink state.
    pub fn tick(&mut self) -> bool {
        if self.last_toggle.elapsed() < IDLE_REDRAW_INTERVAL {
            return false;
        }
        self.visible = !self.visible;
        self.last_toggle = Instant::now();
        true
    }
}
