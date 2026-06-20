//! Pending composer submission state for TUI rendering.

/// Pending user message not yet confirmed by the session stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSubmission {
    text: String,
    state: PendingSubmissionState,
}

impl PendingSubmission {
    /// Create a pending submission in the sending state.
    #[must_use]
    pub const fn new(text: String) -> Self {
        Self {
            text,
            state: PendingSubmissionState::Sending,
        }
    }

    /// Mark the submission as queued.
    pub const fn mark_queued(&mut self, queue_position: Option<u32>) {
        self.state = PendingSubmissionState::Queued { queue_position };
    }

    /// Return pending text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Return pending state.
    #[must_use]
    pub const fn state(&self) -> PendingSubmissionState {
        self.state
    }
}

/// Pending user message state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSubmissionState {
    /// Client request is in flight.
    Sending,
    /// Server queued the request.
    Queued {
        /// Server-reported queue position.
        queue_position: Option<u32>,
    },
}
