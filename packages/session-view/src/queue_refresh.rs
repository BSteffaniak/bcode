//! Queue-observation invalidation shared by every renderer host.

/// Whether a canonical lifecycle event invalidates the independently loaded queue observation.
#[must_use]
pub const fn submission_queue_needs_refresh(kind: &bcode_session_models::SessionEventKind) -> bool {
    use bcode_session_models::SessionEventKind as Event;
    matches!(
        kind,
        Event::SubmissionAccepted { .. }
            | Event::SubmissionActivated { .. }
            | Event::SubmissionCancelled { .. }
            | Event::SubmissionPendingCancelled { .. }
            | Event::SubmissionRecoveryAuthorized { .. }
            | Event::SubmissionContextObserved { .. }
            | Event::ModelTurnStarted { .. }
            | Event::ModelTurnFinished { .. }
    )
}
