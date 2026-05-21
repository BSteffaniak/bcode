//! Shutdown state for the BMUX backend app.

/// Tracks whether the backend should exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ExitState {
    requested: bool,
}

impl ExitState {
    /// Return whether exit was requested.
    #[must_use]
    pub(super) const fn requested(self) -> bool {
        self.requested
    }

    /// Request backend shutdown.
    pub(super) const fn request(&mut self) {
        self.requested = true;
    }
}
