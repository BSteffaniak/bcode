//! Pending composer submission state for TUI rendering.

/// Pending user message not yet confirmed by the session stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSubmission {
    id: bcode_session_models::SubmissionId,
    text: String,
    state: PendingSubmissionState,
}

impl PendingSubmission {
    /// Create a pending submission in the sending state.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self {
            id: bcode_session_models::SubmissionId(
                bcode_session_models::SessionId::new().to_string(),
            ),
            text,
            state: PendingSubmissionState::Sending,
        }
    }

    /// A submission held by the daemon under `id`, as reported after a restart.
    #[must_use]
    pub const fn queued(id: bcode_session_models::SubmissionId, text: String) -> Self {
        Self {
            id,
            text,
            state: PendingSubmissionState::Queued {
                queue_position: None,
            },
        }
    }

    /// Stable submission identity.
    #[must_use]
    pub const fn id(&self) -> &bcode_session_models::SubmissionId {
        &self.id
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

/// Status label of a pending submission (`sending`, `sent`, `queued #N`).
#[must_use]
pub fn pending_label(state: PendingSubmissionState) -> String {
    match state {
        PendingSubmissionState::Sending => "sending".to_owned(),
        PendingSubmissionState::Sent => "sent".to_owned(),
        PendingSubmissionState::Queued { queue_position } => queue_position.map_or_else(
            || "queued".to_owned(),
            |position| format!("queued #{position}"),
        ),
    }
}
