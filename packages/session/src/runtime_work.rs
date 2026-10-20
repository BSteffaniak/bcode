//! Session runtime-work projection reads and durable lifecycle events.

use crate::{SessionError, SessionManager, db};
use bcode_session_models::{
    ClientId, RuntimeWorkStatus, SessionEvent, SessionEventKind, SessionId, WorkId,
};

/// Bounded current-format work inspected for explicit interrupted-turn recovery.
pub struct InterruptedRecoveryWork {
    /// Active invocation projections, including one overflow sentinel.
    pub tools: Vec<db::ToolRun>,
    /// Active execution projections, including one overflow sentinel.
    pub work: Vec<db::RuntimeWorkProjection>,
}

impl SessionManager {
    /// Read current runtime-work projections for bounded exact owner identities.
    /// The actor retains its existing connection and validates projection freshness.
    ///
    /// # Errors
    /// Rejects unavailable/stale storage, missing identities, and count/byte budget overflow.
    pub async fn runtime_work_by_ids(
        &self,
        session_id: SessionId,
        ids: Vec<WorkId>,
        max_bytes: usize,
    ) -> Result<Vec<db::RuntimeWorkProjection>, SessionError> {
        if ids.len() > 200
            || !(1..=262_144).contains(&max_bytes)
            || ids.iter().any(|id| id.0.len() > 512)
        {
            return Err(SessionError::UnsupportedProjectionWindow);
        }
        self.session_handle(session_id)
            .await?
            .runtime_work_by_ids(ids, max_bytes)
            .await
    }
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
        let phase = bcode_metrics::startup::phase("session.active_work.acquire_handle");
        let result = self.session_handle(session_id).await;
        phase.finish_result(&result);
        let handle = result?;
        let phase = bcode_metrics::startup::phase("session.active_work.actor_query");
        let result = handle.active_runtime_work().await;
        phase.finish_result(&result);
        result
    }

    /// Read bounded, freshness-checked work for explicit interrupted-turn recovery.
    /// More than 128 tools or 256 work records is returned as an overflow sentinel;
    /// callers must reject overflow rather than treat the page as complete.
    ///
    /// # Errors
    /// Returns missing durable projections, ownership, or storage errors.
    pub async fn interrupted_recovery_work(
        &self,
        session_id: SessionId,
    ) -> Result<InterruptedRecoveryWork, SessionError> {
        self.session_handle(session_id)
            .await?
            .interrupted_recovery_work()
            .await
    }

    /// Probe active work without sleeping on database-open contention.
    ///
    /// Intended for best-effort recovery discovery. Contention remains an error;
    /// callers must defer rather than interpret it as an empty work set.
    /// Existing actor connections and projection freshness checks are unchanged.
    ///
    /// # Errors
    /// Returns normal active-work errors, including immediate database-open contention.
    pub async fn probe_active_runtime_work(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<db::RuntimeWorkProjection>, SessionError> {
        let phase = bcode_metrics::startup::phase("session.active_work.acquire_handle");
        let result = self.session_handle(session_id).await;
        phase.finish_result(&result);
        let handle = result?;
        let phase = bcode_metrics::startup::phase("session.active_work.actor_query");
        let result = handle.active_runtime_work_with_policy(true).await;
        phase.finish_result(&result);
        result
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
        // Bounded read path: the local handle is dropped at the end of this function, which
        // releases the backend connection. It must not close, because closing checkpoints the WAL
        // and would make a normal read mutate canonical storage.
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
