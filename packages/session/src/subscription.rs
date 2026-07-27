//! Session operation, mutation, catalog, and event subscriptions.

use crate::{SessionError, SessionEventSubscription, SessionManager, SessionMutationCommitted};
use bcode_session_models::{
    SessionEvent, SessionId, SessionOpenOperationId, SessionOpenOperationSnapshot, SessionSummary,
};
use tokio::sync::{broadcast, watch};

impl SessionManager {
    /// Return one operation snapshot when both session and operation identities match.
    pub async fn session_open_operation(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Option<SessionOpenOperationSnapshot> {
        self.migration_operations
            .get(session_id, operation_id)
            .await
            .map(|operation| operation.snapshot())
    }

    /// Subscribe to one matching session-open operation.
    pub async fn subscribe_session_open_operation(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Option<watch::Receiver<SessionOpenOperationSnapshot>> {
        self.migration_operations
            .get(session_id, operation_id)
            .await
            .map(|operation| operation.subscribe())
    }

    #[cfg(test)]
    pub(crate) async fn session_open_operation_history(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Vec<SessionOpenOperationSnapshot> {
        self.migration_operations
            .get(session_id, operation_id)
            .await
            .map_or_else(Vec::new, |operation| operation.history())
    }

    /// Return the number of migrations currently running.
    pub async fn active_session_migration_count(&self) -> usize {
        self.migration_operations.active_count().await
    }

    /// Subscribe to committed durable session mutations.
    #[must_use]
    pub fn subscribe_mutations(&self) -> broadcast::Receiver<SessionMutationCommitted> {
        self.mutation_tx.subscribe()
    }

    pub(crate) fn publish_committed_mutation(&self, event: SessionEvent, summary: SessionSummary) {
        let _ = self.mutation_tx.send(SessionMutationCommitted {
            session_id: event.session_id,
            event,
            summary,
        });
    }

    /// Subscribe to a session's committed/live events without registering as an attached client.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist.
    pub async fn subscribe_session_events(
        &self,
        session_id: SessionId,
    ) -> Result<SessionEventSubscription, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let (session, events, live_events) = handle.subscribe_events().await?;
        Ok(SessionEventSubscription {
            session,
            events,
            live_events,
        })
    }
}
