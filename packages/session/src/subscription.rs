//! Session operation, mutation, catalog, and event subscriptions.

use crate::{SessionError, SessionEventSubscription, SessionManager, SessionMutationCommitted};
use bcode_session_models::{SessionEvent, SessionId, SessionSummary};
use tokio::sync::broadcast;

impl SessionManager {
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
        let handle = self.session_handle(session_id).await?;
        let (session, events, live_events) = handle.subscribe_events().await?;
        Ok(SessionEventSubscription {
            session,
            events,
            live_events,
        })
    }
}
