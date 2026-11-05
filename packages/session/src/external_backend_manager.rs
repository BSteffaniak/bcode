use super::{SessionDb, SessionEvent, SessionId};
use bcode_session_models::external_backend::{
    ExternalBackendEnvelope, ExternalBackendPage, ExternalBackendQuery, ExternalBackendRecord,
};

impl crate::SessionManager {
    /// Persist sanitized managed-backend evidence through the serialized ownership-fenced writer.
    /// Exact retries return their original event. This never delivers/replays a native operation.
    /// # Errors
    /// Rejects ephemeral sessions, conflicts, stale ownership/generation, invalid transitions,
    /// missing canonical storage and unavailable projections.
    pub async fn append_external_backend_record(
        &self,
        session_id: SessionId,
        record: ExternalBackendRecord,
    ) -> Result<SessionEvent, crate::SessionError> {
        if record.session_id() != session_id.to_string() {
            return Err(crate::SessionError::EventSerialization(
                "external record session mismatch".into(),
            ));
        }
        let timestamp_ms = self.next_activity_timestamp_ms();
        let kind = ExternalBackendEnvelope { version: 1, record }
            .into_event_kind()
            .map_err(crate::SessionError::EventSerialization)?;
        self.session_handle(session_id)
            .await?
            .append_event(kind, timestamp_ms)
            .await
    }

    /// Bounded indexed canonical external history; no native installation or plugin is required.
    /// # Errors
    /// Rejects ephemeral/missing sessions, invalid bounds and damaged/stale projections.
    pub async fn session_external_backend_page(
        &self,
        session_id: SessionId,
        query: ExternalBackendQuery,
    ) -> Result<ExternalBackendPage, crate::SessionError> {
        query
            .validate()
            .map_err(crate::SessionError::EventSerialization)?;
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        let cached = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = cached {
            return handle.external_backend_page(query).await;
        }
        let store = self
            .store
            .as_ref()
            .ok_or(crate::SessionError::DbUnavailable(session_id))?;
        let db = SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        Ok(db.external_backend_page(&query).await?)
    }

    /// Read the latest binding as canonical evidence, independently of resume availability.
    /// # Errors
    /// Rejects unavailable canonical storage and stale/missing projections without repairing them.
    pub async fn session_external_backend_binding(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionEvent>, crate::SessionError> {
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        let cached = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = cached {
            return handle.external_backend_binding().await;
        }
        let store = self
            .store
            .as_ref()
            .ok_or(crate::SessionError::DbUnavailable(session_id))?;
        let db = SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        Ok(db.external_backend_binding().await?)
    }
}
