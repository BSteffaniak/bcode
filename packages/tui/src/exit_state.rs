//! Shutdown state for the TUI app.

/// Tracks whether the TUI should exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExitState {
    requested: bool,
}

impl ExitState {
    /// Return whether exit was requested.
    #[must_use]
    pub const fn requested(self) -> bool {
        self.requested
    }

    /// Request TUI shutdown.
    pub const fn request(&mut self) {
        self.requested = true;
    }
}
