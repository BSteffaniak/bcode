//! Session tool-run reads, durable tool results, and finalized artifact lookup.

use crate::{AppendToolCallRequestedInput, SessionError, SessionManager, db};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId, ToolInvocationResultRecord};

impl SessionManager {
    /// Return active tool runs from the DB read model.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist, or
    /// [`SessionError::ProjectionStale`] when the DB projection is not current.
    pub async fn active_tool_runs(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<db::ToolRun>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.active_tool_runs().await
    }

    /// Append a tool-call request event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_tool_call_requested(
        &self,
        session_id: SessionId,
        input: AppendToolCallRequestedInput,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ToolCallRequested {
                tool_call_id: input.tool_call_id,
                tool_name: input.tool_name,
                arguments_json: input.arguments_json,
                producer_plugin_id: input.producer_plugin_id,
                working_directory: input.working_directory,
            },
        )
        .await
    }

    /// Append a generic terminal invocation result record to a session.
    ///
    /// Repeating an append for an invocation that already has a terminal result is idempotent: the
    /// original durable event is returned and no duplicate terminal record is written.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_tool_invocation_result(
        &self,
        session_id: SessionId,
        record: ToolInvocationResultRecord,
    ) -> Result<SessionEvent, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle
            .append_tool_invocation_result(record, activity_timestamp_ms)
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        self.publish_committed_mutation(event.clone(), summary);
        Ok(event)
    }

    /// Resolve one finalized generic artifact reference through its bounded projection.
    ///
    /// # Errors
    ///
    /// Returns an error when the session database is unavailable, the projection is stale, or the
    /// projected row cannot be read.
    pub async fn finalized_artifact_reference(
        &self,
        session_id: SessionId,
        artifact_id: &str,
        reference_key: &str,
    ) -> Result<Option<db::FinalizedArtifactReference>, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::DbUnavailable(session_id))?;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        let reference = db
            .finalized_artifact_reference(artifact_id, reference_key)
            .await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        Ok(reference)
    }
}
