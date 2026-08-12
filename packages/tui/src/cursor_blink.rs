//! Cursor blink state for the TUI composer.

use std::time::Instant;

use super::CURSOR_BLINK_INTERVAL;
use super::invalidation::{InvalidationKey, InvalidationRequest};

/// Composer cursor blink state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorBlink {
    visible: bool,
    last_toggle: Instant,
}

impl CursorBlink {
    const INVALIDATION_KEY: &'static str = "composer-cursor";

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

    /// Return this component's next requested invalidation.
    #[must_use]
    pub fn invalidation_request(&self) -> InvalidationRequest {
        InvalidationRequest::new(
            InvalidationKey::new(Self::INVALIDATION_KEY),
            self.last_toggle + CURSOR_BLINK_INTERVAL,
        )
    }

    /// Handle a due invalidation key.
    pub fn handle_invalidation(&mut self, key: &InvalidationKey, now: Instant) -> bool {
        if key.as_str() != Self::INVALIDATION_KEY {
            return false;
        }
        self.toggle(now);
        true
    }

    const fn toggle(&mut self, now: Instant) {
        self.visible = !self.visible;
        self.last_toggle = now;
    }
}
