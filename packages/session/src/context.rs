//! Session model-context state and compaction/occupancy events.

use crate::{SessionError, SessionManager};
use bcode_session_models::{
    ProviderContextSnapshot, RequestContextObservation, RequestContextOccupancy, SessionEvent,
    SessionEventKind, SessionId,
};

impl SessionManager {
    /// Return the current context generation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_context_epoch(&self, session_id: SessionId) -> Result<u64, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_context_epoch().await
    }

    /// Return authoritative current context occupancy with a bounded projection lookup.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist, or a projection error
    /// when the occupancy read model is not trustworthy.
    pub async fn current_context_occupancy(
        &self,
        session_id: SessionId,
    ) -> Result<Option<RequestContextOccupancy>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_context_occupancy().await
    }

    /// Return the model-visible session events, starting at the latest compaction when possible.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn model_context_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.model_context_events().await
    }

    /// Append a context-compaction summary to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_context_compacted(
        &self,
        session_id: SessionId,
        summary: String,
        compacted_through_sequence: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            },
        )
        .await
    }

    /// Append a provider-native context compaction boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_provider_context_compacted(
        &self,
        session_id: SessionId,
        snapshot: ProviderContextSnapshot,
        compacted_through_sequence: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            },
        )
        .await
    }

    /// Append a context occupancy observation.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_request_context_observed(
        &self,
        session_id: SessionId,
        observation: RequestContextObservation,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::RequestContextObserved { observation },
        )
        .await
    }
}
