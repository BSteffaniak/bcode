//! Pending composer submission state for TUI rendering.

/// Pending user message not yet confirmed by the session stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSubmission {
    text: String,
    reveal_on_acceptance: bool,
    state: PendingSubmissionState,
}

impl PendingSubmission {
    /// Create a pending submission in the sending state.
    #[must_use]
    pub const fn new(text: String) -> Self {
        Self {
            text,
            reveal_on_acceptance: true,
            state: PendingSubmissionState::Sending,
        }
    }

    /// Supersede the positioning associated with this submission, but retain its data.
    pub const fn cancel_reveal(&mut self) {
        self.reveal_on_acceptance = false;
    }

    /// Whether acceptance still owns a pending navigation intent.
    #[must_use]
    pub const fn reveals_on_acceptance(&self) -> bool {
        self.reveal_on_acceptance
    }

    /// Mark the submission as queued.
    pub const fn mark_queued(&mut self, queue_position: Option<u32>) {
        self.state = PendingSubmissionState::Queued { queue_position };
    }

    /// Mark the submission as sent.
    pub const fn mark_sent(&mut self) {
        self.state = PendingSubmissionState::Sent;
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
    /// Server accepted the request immediately.
    Sent,
    /// Server queued the request.
    Queued {
        /// Server-reported queue position.
        queue_position: Option<u32>,
    },
}
