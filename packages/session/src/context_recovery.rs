//! Explicit bounded canonical evidence reads for replacing stale summaries.
//!
//! This is not a normal model-context read and never repairs projections or storage.
//! Callers must hold session ownership and serialize source activation with the session runtime.

use crate::{SessionError, SessionManager, persisted::EventDecodeBudget};
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery, SessionId, SessionSummaryRebuildRequest,
};

/// Failure to obtain complete, generation-pinned canonical recovery evidence.
#[derive(Debug, thiserror::Error)]
pub enum SummaryRecoveryError {
    /// Canonical storage or a required projection is unavailable.
    #[error("canonical summary recovery source unavailable")]
    Session(#[from] SessionError),
    /// The exact requested marker or source generation changed.
    #[error("summary recovery source changed; inspect the current marker and retry explicitly")]
    Conflict,
    /// The explicit operation allowance was invalid or exhausted.
    #[error("summary recovery resource allowance exhausted or invalid; existing marker unchanged")]
    Budget,
    /// Canonical evidence was incomplete or incompatible.
    #[error("canonical summary recovery evidence is incomplete or incompatible")]
    Incomplete,
}

/// Pinned replacement target. Its boundary never moves backwards during reconstruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryRecoveryPin {
    /// Owning canonical session.
    pub session_id: SessionId,
    /// Exact latest marker selected for replacement.
    pub marker_sequence: u64,
    /// Inclusive canonical source boundary covered by the marker.
    pub boundary: u64,
    /// Context epoch at admission, including any model selection after the marker.
    pub context_epoch: u64,
    /// Canonical tail at explicit operation admission.
    pub generation: u64,
}

/// Stateful, aggregate-budgeted recovery cursor; consumed pages need not be retained.
pub struct SummaryRecoveryReader {
    pin: SummaryRecoveryPin,
    next: u64,
    done: bool,
    pages_left: u32,
    events_left: u32,
    decode: Option<EventDecodeBudget>,
}

fn decode_budget(bytes: u32) -> EventDecodeBudget {
    let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
    EventDecodeBudget::new(
        bytes,
        bcode_session_models::retention::ObservationRetentionLimits {
            scalar_capacity_bytes: bytes,
            collection_entries: 262_144,
            value_nodes: 524_288,
            depth: 64,
            copies: 65_536,
        },
    )
}

fn recovery_event(event: SessionEvent) -> Option<SessionEvent> {
    if matches!(
        event.kind,
        SessionEventKind::ContextCompacted { .. }
            | SessionEventKind::ProviderContextCompacted { .. }
    ) {
        return None;
    }
    if matches!(
        crate::db_context::context_history_role(&event.kind),
        crate::db_context::ContextHistoryRole::Excluded
    ) {
        return None;
    }
    Some(crate::submission::context_event(event))
}

impl SummaryRecoveryReader {
    /// Capture an exact latest marker without loading its replaced prefix.
    ///
    /// # Errors
    /// Rejects invalid budgets, a non-current marker, incompatible canonical records,
    /// changed generations, or unavailable storage. Never repairs or reindexes.
    pub async fn begin(
        sessions: &SessionManager,
        request: SessionSummaryRebuildRequest,
    ) -> Result<Self, SummaryRecoveryError> {
        if !request.budget.is_valid() {
            return Err(SummaryRecoveryError::Budget);
        }
        let generation = sessions
            .current_session_generation(request.session_id)
            .await?;
        let context_epoch = sessions.current_context_epoch(request.session_id).await?;
        let (page, decode) = sessions
            .session_history_page_with_decode_budget(
                request.session_id,
                SessionHistoryQuery {
                    cursor: Some(SessionHistoryCursor {
                        sequence: request.expected_marker,
                    }),
                    direction: SessionHistoryDirection::Forward,
                    limit: 1,
                },
                decode_budget(request.budget.max_decoded_bytes),
            )
            .await
            .map_err(|_| SummaryRecoveryError::Incomplete)?;
        let marker = page
            .events
            .first()
            .ok_or(SummaryRecoveryError::Incomplete)?;
        if marker.sequence != request.expected_marker || !page.compatibility_issues.is_empty() {
            return Err(SummaryRecoveryError::Incomplete);
        }
        let boundary = match &marker.kind {
            SessionEventKind::ContextCompacted {
                compacted_through_sequence,
                ..
            }
            | SessionEventKind::ProviderContextCompacted {
                compacted_through_sequence,
                ..
            } if *compacted_through_sequence < marker.sequence => *compacted_through_sequence,
            _ => return Err(SummaryRecoveryError::Incomplete),
        };
        if generation
            != sessions
                .current_session_generation(request.session_id)
                .await?
            || sessions.current_context_epoch(request.session_id).await? != context_epoch
        {
            return Err(SummaryRecoveryError::Conflict);
        }
        let mut reader = Self {
            pin: SummaryRecoveryPin {
                session_id: request.session_id,
                marker_sequence: request.expected_marker,
                boundary,
                context_epoch,
                generation,
            },
            next: 0,
            done: false,
            pages_left: request.budget.max_pages.saturating_sub(1),
            events_left: request.budget.max_events.saturating_sub(1),
            decode: Some(decode),
        };
        let mut next = request
            .expected_marker
            .checked_add(1)
            .ok_or(SummaryRecoveryError::Conflict)?;
        while next <= generation {
            let page = reader.read_page(sessions, next, generation).await?;
            for event in page {
                next = event
                    .sequence
                    .checked_add(1)
                    .ok_or(SummaryRecoveryError::Conflict)?;
                if matches!(
                    event.kind,
                    SessionEventKind::ContextCompacted { .. }
                        | SessionEventKind::ProviderContextCompacted { .. }
                ) {
                    return Err(SummaryRecoveryError::Conflict);
                }
            }
        }
        if generation
            != sessions
                .current_session_generation(request.session_id)
                .await?
        {
            return Err(SummaryRecoveryError::Conflict);
        }
        Ok(reader)
    }

    /// Return the immutable replacement target.
    #[must_use]
    pub const fn pin(&self) -> SummaryRecoveryPin {
        self.pin
    }

    /// Read the next complete bounded canonical page through the original boundary.
    ///
    /// Old summaries and pending submissions are excluded. Activated submissions are converted
    /// at their activation sequence, exactly as in normal model context. Empty pages can advance.
    /// # Errors
    /// Rejects budget exhaustion, sequence gaps, incompatible records or storage failures.
    pub async fn next_page(
        &mut self,
        sessions: &SessionManager,
    ) -> Result<Option<Vec<SessionEvent>>, SummaryRecoveryError> {
        if self.done {
            return Ok(None);
        }
        let page = self
            .read_page(sessions, self.next, self.pin.boundary)
            .await?;
        let mut output = Vec::new();
        for event in page {
            if event.sequence != self.next {
                return Err(SummaryRecoveryError::Incomplete);
            }
            self.next = self
                .next
                .checked_add(1)
                .ok_or(SummaryRecoveryError::Incomplete)?;
            output.extend(recovery_event(event));
        }
        self.done = self.next > self.pin.boundary;
        Ok(Some(output))
    }

    async fn read_page(
        &mut self,
        sessions: &SessionManager,
        start: u64,
        through: u64,
    ) -> Result<Vec<SessionEvent>, SummaryRecoveryError> {
        if self.pages_left == 0 || self.events_left == 0 {
            return Err(SummaryRecoveryError::Budget);
        }
        let count = through
            .saturating_sub(start)
            .saturating_add(1)
            .min(64)
            .min(u64::from(self.events_left));
        let decode = self.decode.take().ok_or(SummaryRecoveryError::Budget)?;
        let (page, decode) = sessions
            .session_history_page_with_decode_budget(
                self.pin.session_id,
                SessionHistoryQuery {
                    cursor: Some(SessionHistoryCursor { sequence: start }),
                    direction: SessionHistoryDirection::Forward,
                    limit: usize::try_from(count).map_err(|_| SummaryRecoveryError::Budget)?,
                },
                decode,
            )
            .await
            .map_err(|_| SummaryRecoveryError::Incomplete)?;
        self.decode = Some(decode);
        self.pages_left -= 1;
        self.events_left = self.events_left.saturating_sub(
            u32::try_from(page.events.len()).map_err(|_| SummaryRecoveryError::Budget)?,
        );
        if page.events.is_empty()
            || !page.compatibility_issues.is_empty()
            || page
                .events
                .first()
                .is_none_or(|event| event.sequence != start)
            || page
                .events
                .last()
                .is_some_and(|event| event.sequence > through)
        {
            return Err(SummaryRecoveryError::Incomplete);
        }
        for pair in page.events.windows(2) {
            if pair[0].sequence.checked_add(1) != Some(pair[1].sequence) {
                return Err(SummaryRecoveryError::Incomplete);
            }
        }
        Ok(page.events)
    }

    /// Recheck the marker and all canonical additions since admission before replacement.
    ///
    /// Provider accounting/traces do not change evidence. New activated source or another marker
    /// rejects the operation. Caller must continue serializing activation until append completes.
    /// # Errors
    /// Rejects incomplete reconstruction, changed evidence, or exhausted validation allowance.
    pub async fn validate_commit(
        &mut self,
        sessions: &SessionManager,
    ) -> Result<(), SummaryRecoveryError> {
        if !self.done {
            return Err(SummaryRecoveryError::Incomplete);
        }
        if sessions.current_context_epoch(self.pin.session_id).await? != self.pin.context_epoch {
            return Err(SummaryRecoveryError::Conflict);
        }
        let tail = sessions
            .current_session_generation(self.pin.session_id)
            .await?;
        let mut next = self
            .pin
            .generation
            .checked_add(1)
            .ok_or(SummaryRecoveryError::Conflict)?;
        while next <= tail {
            let page = self.read_page(sessions, next, tail).await?;
            for event in page {
                next = event
                    .sequence
                    .checked_add(1)
                    .ok_or(SummaryRecoveryError::Conflict)?;
                if !matches!(
                    crate::db_context::context_history_role(&event.kind),
                    crate::db_context::ContextHistoryRole::Excluded
                ) {
                    return Err(SummaryRecoveryError::Conflict);
                }
            }
        }
        if sessions.current_context_epoch(self.pin.session_id).await? != self.pin.context_epoch
            || sessions
                .current_session_generation(self.pin.session_id)
                .await?
                != tail
        {
            return Err(SummaryRecoveryError::Conflict);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::SessionSummaryRebuildBudget;

    #[test]
    fn explicit_budgets_are_nonzero_and_bounded() {
        assert!(SessionSummaryRebuildBudget::default().is_valid());
        let mut budget = SessionSummaryRebuildBudget {
            max_events: 0,
            ..SessionSummaryRebuildBudget::default()
        };
        assert!(!budget.is_valid());
        budget.max_events = u32::MAX;
        assert!(!budget.is_valid());
    }

    #[tokio::test]
    async fn exact_marker_recovery_streams_source_and_preserves_tail() {
        let sessions = SessionManager::default();
        let session = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let activation = bcode_session_models::TurnId("recovery-activation".into());
        let submission = bcode_session_models::PromptSubmission {
            id: bcode_session_models::SubmissionId::for_queued_turn(&activation),
            client_id: bcode_session_models::ClientId::new(),
            text: "Preserve read-only scope".into(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
            requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
            target: bcode_session_models::SubmissionTarget::OwnTurn {
                turn_id: activation.clone(),
            },
            execution_descriptor: None,
        };
        sessions
            .append_event(
                session.id,
                SessionEventKind::SubmissionAccepted {
                    submission: submission.clone(),
                },
            )
            .await
            .unwrap();
        let source = sessions
            .append_event(
                session.id,
                SessionEventKind::SubmissionActivated {
                    submission_id: submission.id,
                    turn_id: activation,
                    client_id: submission.client_id,
                    text: submission.text,
                    admission: submission.admission,
                },
            )
            .await
            .unwrap();
        let marker = sessions
            .append_context_compacted(session.id, "STALE_WRITE_AUTHORITY".into(), source.sequence)
            .await
            .unwrap();
        let tail = sessions
            .append_event(
                session.id,
                SessionEventKind::SystemMessage {
                    text: "Uncompacted tail".into(),
                },
            )
            .await
            .unwrap();
        let request = SessionSummaryRebuildRequest {
            session_id: session.id,
            expected_marker: marker.sequence,
            budget: SessionSummaryRebuildBudget::default(),
        };
        let mut reader = SummaryRecoveryReader::begin(&sessions, request)
            .await
            .unwrap();
        let mut recovered = Vec::new();
        while let Some(page) = reader.next_page(&sessions).await.unwrap() {
            recovered.extend(page);
        }
        assert!(
            recovered
                .iter()
                .any(|event| event.sequence == source.sequence)
        );
        assert!(
            recovered
                .iter()
                .all(|event| event.sequence <= source.sequence)
        );
        assert!(!recovered.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::ContextCompacted { .. }
                | SessionEventKind::ProviderContextCompacted { .. }
        )));
        reader.validate_commit(&sessions).await.unwrap();
        sessions
            .append_context_compacted(
                session.id,
                "Fresh canonical recollection".into(),
                reader.pin().boundary,
            )
            .await
            .unwrap();
        let context = sessions.model_context_events(session.id).await.unwrap();
        assert!(context.iter().any(|event| event.sequence == tail.sequence));
        assert!(
            !context
                .iter()
                .any(|event| event.sequence == marker.sequence)
        );
        assert!(matches!(
            SummaryRecoveryReader::begin(&sessions, request).await,
            Err(SummaryRecoveryError::Conflict)
        ));
    }

    #[tokio::test]
    async fn new_source_or_incomplete_scan_cannot_commit() {
        let sessions = SessionManager::default();
        let session = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let source = sessions
            .append_event(
                session.id,
                SessionEventKind::SystemMessage {
                    text: "canonical".into(),
                },
            )
            .await
            .unwrap();
        let marker = sessions
            .append_context_compacted(session.id, "stale".into(), source.sequence)
            .await
            .unwrap();
        let request = SessionSummaryRebuildRequest {
            session_id: session.id,
            expected_marker: marker.sequence,
            budget: SessionSummaryRebuildBudget::default(),
        };
        let mut reader = SummaryRecoveryReader::begin(&sessions, request)
            .await
            .unwrap();
        assert!(matches!(
            reader.validate_commit(&sessions).await,
            Err(SummaryRecoveryError::Incomplete)
        ));
        while reader.next_page(&sessions).await.unwrap().is_some() {}
        sessions
            .append_event(
                session.id,
                SessionEventKind::SystemMessage {
                    text: "changed source".into(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            reader.validate_commit(&sessions).await,
            Err(SummaryRecoveryError::Conflict)
        ));
        assert_eq!(
            sessions.current_context_epoch(session.id).await.unwrap(),
            marker.sequence
        );
    }

    #[tokio::test]
    async fn exhausted_allowance_and_wrong_marker_do_not_replace_summary() {
        let sessions = SessionManager::default();
        let session = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let source = sessions
            .append_event(
                session.id,
                SessionEventKind::SystemMessage {
                    text: "canonical".into(),
                },
            )
            .await
            .unwrap();
        let marker = sessions
            .append_context_compacted(session.id, "stale".into(), source.sequence)
            .await
            .unwrap();
        let mut request = SessionSummaryRebuildRequest {
            session_id: session.id,
            expected_marker: source.sequence,
            budget: SessionSummaryRebuildBudget::default(),
        };
        assert!(
            SummaryRecoveryReader::begin(&sessions, request)
                .await
                .is_err()
        );
        request.expected_marker = marker.sequence;
        request.budget.max_pages = 1;
        let mut reader = SummaryRecoveryReader::begin(&sessions, request)
            .await
            .unwrap();
        assert!(matches!(
            reader.next_page(&sessions).await,
            Err(SummaryRecoveryError::Budget)
        ));
        assert_eq!(
            sessions.current_context_epoch(session.id).await.unwrap(),
            marker.sequence
        );
    }

    #[test]
    fn pending_acceptance_is_excluded_until_activation() {
        let turn = bcode_session_models::TurnId("pending-turn".into());
        let event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 7,
            timestamp_ms: 0,
            session_id: SessionId::new(),
            provenance: None,
            kind: SessionEventKind::SubmissionAccepted {
                submission: bcode_session_models::PromptSubmission {
                    id: bcode_session_models::SubmissionId::for_queued_turn(&turn),
                    client_id: bcode_session_models::ClientId::new(),
                    text: "not activated".into(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
                    target: bcode_session_models::SubmissionTarget::OwnTurn { turn_id: turn },
                    execution_descriptor: None,
                },
            },
        };
        assert!(recovery_event(event).is_none());
    }

    #[test]
    fn activation_is_user_source_at_activation_sequence() {
        let event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 19,
            timestamp_ms: 0,
            session_id: SessionId::new(),
            provenance: None,
            kind: SessionEventKind::SubmissionActivated {
                submission_id: bcode_session_models::SubmissionId("submission".into()),
                turn_id: bcode_session_models::TurnId("turn".into()),
                client_id: bcode_session_models::ClientId::new(),
                text: "Remain read only".into(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        };
        let recovered = recovery_event(event).unwrap();
        assert_eq!(recovered.sequence, 19);
        assert!(
            matches!(recovered.kind, SessionEventKind::UserMessage { text, .. } if text == "Remain read only")
        );
    }

    #[test]
    fn stale_summary_is_never_recovery_evidence() {
        let event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 10,
            timestamp_ms: 0,
            session_id: SessionId::new(),
            provenance: None,
            kind: SessionEventKind::ContextCompacted {
                summary: "Unrestricted writes allowed".into(),
                compacted_through_sequence: 8,
            },
        };
        assert!(recovery_event(event).is_none());
    }
}
