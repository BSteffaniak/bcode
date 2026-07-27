//! Session runtime-work projection reads and durable lifecycle events.

use crate::{SessionError, SessionManager, db};
use bcode_session_models::{
    ClientId, RuntimeWorkStatus, SessionEvent, SessionEventKind, SessionId, WorkId,
};

impl SessionManager {
    /// Return active runtime-work rows through the session actor's DB connection.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist, or
    /// [`SessionError::ProjectionStale`] when the DB projection is not current.
    pub async fn active_runtime_work(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<db::RuntimeWorkProjection>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.active_runtime_work().await
    }

    /// Return latest runtime-work rows from the DB read model.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist, or
    /// [`SessionError::ProjectionStale`] when the DB projection is not current.
    pub async fn runtime_work_history(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<db::RuntimeWorkProjection>, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::DbUnavailable(session_id))?;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        let expected_last_sequence = db.last_event_sequence().await?.unwrap_or(0);
        let checkpoint = db
            .materialized_projection_checkpoint(db::MaterializedProjection::RuntimeWork)
            .await?;
        if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
            return Ok(db.runtime_work_history(limit).await?);
        }
        Err(SessionError::ProjectionStale {
            session_id,
            projection: "runtime_work",
            checkpoint,
            expected: expected_last_sequence,
        })
    }

    /// Append a runtime-work started event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_runtime_work_started(
        &self,
        session_id: SessionId,
        event: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, event).await
    }

    /// Append a runtime-work cancellation request event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_runtime_work_cancel_requested(
        &self,
        session_id: SessionId,
        work_id: WorkId,
        requested_at_ms: Option<u64>,
        client_id: Option<ClientId>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            },
        )
        .await
    }

    /// Append a runtime-work finished event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_runtime_work_finished(
        &self,
        session_id: SessionId,
        work_id: WorkId,
        status: RuntimeWorkStatus,
        finished_at_ms: Option<u64>,
        message: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            },
        )
        .await
    }
}
