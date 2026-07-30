//! Durable session mutation and live-event publication behavior.

use crate::{SessionError, SessionManager, actor, current_unix_millis};
use bcode_session_models::{
    ClientId, ModelTurnOutcome, SessionEvent, SessionEventKind, SessionEventProvenance, SessionId,
    SessionLiveEvent, SessionLiveEventKind, SessionTokenUsage, SessionTraceEvent,
};
use std::sync::atomic::Ordering;

impl SessionManager {
    /// Append a user message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the user-message event cannot be persisted
    pub async fn append_user_message(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.append_user_message_with_origin(session_id, client_id, text, None)
            .await
    }

    /// Append a user message carrying optional generic turn-origin metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the user-message event cannot be persisted
    pub async fn append_user_message_with_origin(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
        origin: Option<bcode_session_models::TurnOrigin>,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.admit_turn_result(
            session_id,
            client_id,
            text,
            bcode_session_models::TurnAdmissionMetadata {
                origin,
                ..bcode_session_models::TurnAdmissionMetadata::default()
            },
        )
        .await
        .map(|result| result.events)
    }

    /// Atomically admit an ordinary turn and return its durable receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist, metadata is invalid, or persistence fails.
    pub async fn admit_turn(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<bcode_session_models::TurnAdmission, SessionError> {
        self.admit_turn_with_events(session_id, client_id, text, admission)
            .await
            .map(|(admission, _)| admission)
    }

    /// Atomically admit an ordinary turn and return both its durable receipt and committed events.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist, metadata is invalid, or persistence fails.
    pub async fn admit_turn_with_events(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<
        (
            bcode_session_models::TurnAdmission,
            Vec<bcode_session_models::SessionEvent>,
        ),
        SessionError,
    > {
        self.admit_turn_result(session_id, client_id, text, admission)
            .await
            .map(|result| (result.admission, result.events))
    }

    async fn admit_turn_result(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<actor::TurnAdmissionResult, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let result = handle
            .append_user_message(client_id, text, admission, activity_timestamp_ms)
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        for event in &result.events {
            self.publish_committed_mutation(event.clone(), summary.clone());
        }
        Ok(result)
    }

    /// Append an assistant streaming delta to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_assistant_delta(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AssistantDelta { text })
            .await
    }

    /// Append a complete assistant message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_assistant_message(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AssistantMessage { text })
            .await
    }

    /// Append a complete assistant response segment with stable turn-local identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_assistant_response_segment(
        &self,
        session_id: SessionId,
        turn_id: String,
        segment_id: String,
        segment_order: u32,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::AssistantResponseSegment {
                turn_id,
                segment_id,
                segment_order,
                text,
            },
        )
        .await
    }

    /// Append a complete positioned assistant response segment.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_positioned_assistant_response_segment(
        &self,
        session_id: SessionId,
        turn_id: String,
        output_position: bcode_session_models::TurnOutputPosition,
        segment_id: String,
        segment_order: u32,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::PositionedAssistantResponseSegment {
                turn_id,
                output_position,
                segment_id,
                segment_order,
                text,
            },
        )
        .await
    }

    /// Append one complete terminal provider-reported reasoning activity.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_assistant_reasoning_activity(
        &self,
        session_id: SessionId,
        turn_id: String,
        activity: bcode_session_models::ReasoningActivity,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::AssistantReasoningActivity { turn_id, activity },
        )
        .await
    }

    /// Append one complete positioned terminal reasoning activity.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_positioned_assistant_reasoning_activity(
        &self,
        session_id: SessionId,
        turn_id: String,
        output_position: bcode_session_models::TurnOutputPosition,
        activity: bcode_session_models::ReasoningActivity,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::PositionedAssistantReasoningActivity {
                turn_id,
                output_position,
                activity,
            },
        )
        .await
    }

    /// Publish a live-only event to currently attached session subscribers.
    ///
    /// Live events are not appended to durable history and may be coalesced or
    /// dropped by callers under backpressure. They are intended for high-rate
    /// presentation streams whose final semantic result is recorded separately.
    /// Returns `None` when the session is not loaded or has no active live subscribers.
    pub async fn publish_live_event(
        &self,
        session_id: SessionId,
        event: SessionLiveEventKind,
    ) -> Option<SessionLiveEvent> {
        let handle = {
            let inner = self.inner.lock().await;
            inner.sessions.get(&session_id).cloned()?
        };
        handle.publish_live_event(event).await.ok().flatten()
    }

    /// Append a permission-requested event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_permission_requested(
        &self,
        session_id: SessionId,
        request: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        debug_assert!(matches!(
            request,
            SessionEventKind::PermissionRequested { .. }
        ));
        self.append_event(session_id, request).await
    }

    /// Append a permission-resolved event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_permission_resolved(
        &self,
        session_id: SessionId,
        permission_id: String,
        approved: bool,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            },
        )
        .await
    }

    /// Append a model-changed event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_changed(
        &self,
        session_id: SessionId,
        provider: String,
        model: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ModelChanged { provider, model },
        )
        .await
    }

    /// Append a reasoning-changed event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_reasoning_changed(
        &self,
        session_id: SessionId,
        effort: Option<String>,
        summary: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ReasoningChanged { effort, summary },
        )
        .await
    }

    /// Append an agent-changed event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_agent_changed(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AgentChanged { agent_id })
            .await
    }

    /// Set the current in-memory agent selection for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or is not writable.
    pub async fn set_current_agent(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> Result<(), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.set_current_agent(agent_id).await
    }

    /// Append a model-turn-started event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_turn_started(
        &self,
        session_id: SessionId,
        turn_id: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::ModelTurnStarted { turn_id })
            .await
    }

    /// Append a model-turn-cancel-requested event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_turn_cancel_requested(
        &self,
        session_id: SessionId,
        turn_id: String,
        requested_at_ms: Option<u64>,
        client_id: Option<ClientId>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            },
        )
        .await
    }

    /// Append a model-turn-finished event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_turn_finished(
        &self,
        session_id: SessionId,
        turn_id: String,
        outcome: ModelTurnOutcome,
        message: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            },
        )
        .await
    }

    /// Append provider-neutral token usage to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_usage(
        &self,
        session_id: SessionId,
        turn_id: String,
        usage: SessionTokenUsage,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::ModelUsage { turn_id, usage })
            .await
    }

    /// Append a system message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_system_message(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::SystemMessage { text })
            .await
    }

    /// Append a diagnostic trace event.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_trace_event(
        &self,
        session_id: SessionId,
        trace: SessionTraceEvent,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::TraceEvent {
                trace: Box::new(trace),
            },
        )
        .await
    }

    /// Append an event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the event cannot be persisted
    pub async fn append_event(
        &self,
        session_id: SessionId,
        kind: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle.append_event(kind, activity_timestamp_ms).await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        self.publish_committed_mutation(event.clone(), summary);
        Ok(event)
    }

    /// Append an event with optional source provenance to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the event cannot be persisted
    pub async fn append_event_with_provenance(
        &self,
        session_id: SessionId,
        kind: SessionEventKind,
        provenance: Option<SessionEventProvenance>,
    ) -> Result<SessionEvent, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle
            .append_event_with_provenance(kind, provenance, activity_timestamp_ms)
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        self.publish_committed_mutation(event.clone(), summary);
        Ok(event)
    }

    pub(crate) fn next_activity_timestamp_ms(&self) -> u64 {
        loop {
            let previous = self.activity_clock_ms.load(Ordering::Acquire);
            let next = previous.max(current_unix_millis()).saturating_add(1);
            if self
                .activity_clock_ms
                .compare_exchange(previous, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return next;
            }
        }
    }
}
