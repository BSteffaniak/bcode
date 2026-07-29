use crate::{
    MigrationPlan, MigrationPlanError, MigrationPlanService, SessionMigrationOperation,
    SessionMigrationOperations,
};
use bcode_session_models::{SessionId, SessionOpenOperationId};
use std::sync::Arc;
use tokio::sync::watch;

/// Server-composed service for migration planning and reconnectable operation activity.
///
/// Migration execution adapters may share the operation registry, but the composition root owns
/// this service and uses it for progress lookup and shutdown activity.
#[derive(Debug, Clone, Default)]
pub struct SessionMigrationService {
    planner: MigrationPlanService,
    operations: SessionMigrationOperations,
}

impl SessionMigrationService {
    /// Create a service with an explicit operation registry.
    #[must_use]
    pub const fn new(operations: SessionMigrationOperations) -> Self {
        Self {
            planner: MigrationPlanService,
            operations,
        }
    }

    /// Resolve a complete migration plan for a source writer epoch.
    ///
    /// # Errors
    ///
    /// Returns an error when no safe monotonic path reaches the current writer.
    pub fn plan(&self, source_writer_epoch: u32) -> Result<MigrationPlan, MigrationPlanError> {
        self.planner.plan(source_writer_epoch)
    }

    /// Return the shared operation registry used by the execution adapter.
    #[must_use]
    pub fn operations(&self) -> SessionMigrationOperations {
        self.operations.clone()
    }

    /// Subscribe to a reconnectable operation when both identities match.
    pub async fn subscribe(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Option<watch::Receiver<bcode_session_models::SessionOpenOperationSnapshot>> {
        self.operations
            .get(session_id, operation_id)
            .await
            .map(|operation| operation.subscribe())
    }

    /// Return an operation when both identities match.
    pub async fn operation(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Option<Arc<SessionMigrationOperation>> {
        self.operations.get(session_id, operation_id).await
    }

    /// Return whether one session currently has a running migration operation.
    pub async fn is_active(&self, session_id: SessionId) -> bool {
        self.operations.is_active(session_id).await
    }

    /// Count currently running migration operations.
    pub async fn active_count(&self) -> usize {
        self.operations.active_count().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        SessionMigrationProgress, SessionMigrationStage, SessionOpenOperationSnapshot,
        SessionOpenTerminalOutcome,
    };

    fn snapshot(session_id: SessionId) -> SessionOpenOperationSnapshot {
        SessionOpenOperationSnapshot {
            operation_id: SessionOpenOperationId::new(),
            revision: 0,
            session_id,
            source_writer_epoch: Some(4),
            target_writer_epoch: 5,
            progress: SessionMigrationProgress {
                stage: SessionMigrationStage::WaitingForOwnership,
                completed_units: None,
                total_units: None,
                unit: None,
                message: "waiting".to_owned(),
            },
            outcome: None,
            backup_path: None,
        }
    }

    #[tokio::test]
    async fn service_owns_shared_activity_and_reconnect_lookup() {
        let service = SessionMigrationService::default();
        let session_id = SessionId::new();
        let initial = snapshot(session_id);
        let operation_id = initial.operation_id;
        let blocker = Arc::new(tokio::sync::Notify::new());
        let task_blocker = Arc::clone(&blocker);
        let operation = service
            .operations()
            .start_or_join(initial, move |_| async move {
                task_blocker.notified().await;
                SessionOpenTerminalOutcome::Ready
            })
            .await;

        assert_eq!(service.active_count().await, 1);
        assert!(service.is_active(session_id).await);
        let receiver = service
            .subscribe(session_id, operation_id)
            .await
            .expect("reconnectable operation");
        assert_eq!(receiver.borrow().operation_id, operation_id);
        blocker.notify_one();
        let mut receiver = operation.subscribe();
        receiver
            .wait_for(|snapshot| snapshot.outcome.is_some())
            .await
            .expect("terminal operation");
        assert_eq!(service.active_count().await, 0);
        assert!(!service.is_active(session_id).await);
    }
}
