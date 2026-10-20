//! Session operation, mutation, catalog, and event subscriptions.

use crate::{SessionError, SessionEventSubscription, SessionManager, SessionMutationCommitted};
use bcode_session_models::{SessionEvent, SessionId, SessionSummary};
use tokio::sync::broadcast;

impl SessionManager {
    /// Subscribe to admission-bounded observation updates without attaching a client.
    ///
    /// Queue/structural exhaustion terminates this observer only, not persistence
    /// or the existing broadcast feeds. The initial summary is inspected/reserved
    /// before cloning, and shares structural admission with queued/in-flight updates.
    /// Initial durable reads must be decoded under their own bounded read policy;
    /// use `admit_history_event` before retaining an additional source event copy.
    ///
    /// # Errors
    /// Returns an error when the session cannot be opened. Invalid limits are
    /// reported as a terminal `LimitReached` by the returned subscription.
    pub async fn subscribe_session_events_bounded(
        &self,
        session_id: SessionId,
        limits: bcode_session_models::SessionObservationSourceLimits,
    ) -> Result<crate::BoundedSessionEventSubscription, SessionError> {
        self.session_handle(session_id)
            .await?
            .subscribe_events_bounded(limits)
            .await
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
        let handle = self.session_handle(session_id).await?;
        let (session, events, live_events) = handle.subscribe_events().await?;
        Ok(SessionEventSubscription {
            session,
            events,
            live_events,
        })
    }
}
