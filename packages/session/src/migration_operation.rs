//! Coordination for server-owned session-open preparation operations.

use bcode_session_models::{
    SessionId, SessionMigrationProgress, SessionMigrationStage, SessionOpenOperationId,
    SessionOpenOperationSnapshot, SessionOpenTerminalOutcome,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, watch};

const DEFAULT_TERMINAL_RETENTION: Duration = Duration::from_mins(10);
const DEFAULT_MAX_TERMINAL_OPERATIONS: usize = 128;

#[derive(Debug)]
pub struct SessionMigrationOperation {
    snapshots: watch::Sender<SessionOpenOperationSnapshot>,
    publication: std::sync::Mutex<()>,
    completed_at: Mutex<Option<Instant>>,
    #[cfg(test)]
    history: std::sync::Mutex<Vec<SessionOpenOperationSnapshot>>,
}

impl SessionMigrationOperation {
    fn new(snapshot: SessionOpenOperationSnapshot) -> Self {
        Self {
            snapshots: watch::channel(snapshot).0,
            publication: std::sync::Mutex::new(()),
            completed_at: Mutex::new(None),
            #[cfg(test)]
            history: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> SessionOpenOperationSnapshot {
        self.snapshots.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<SessionOpenOperationSnapshot> {
        self.snapshots.subscribe()
    }

    pub fn publish(&self, snapshot: SessionOpenOperationSnapshot) {
        #[cfg(test)]
        if let Ok(mut history) = self.history.lock() {
            history.push(snapshot.clone());
        }
        self.snapshots.send_replace(snapshot);
    }

    #[cfg(test)]
    pub fn history(&self) -> Vec<SessionOpenOperationSnapshot> {
        self.history
            .lock()
            .map_or_else(|_| Vec::new(), |history| history.clone())
    }

    pub fn publish_progress(&self, progress: SessionMigrationProgress) {
        let Ok(_publication) = self.publication.lock() else {
            return;
        };
        let current = self.snapshot();
        if current.outcome.is_some() || progress.stage < current.progress.stage {
            return;
        }
        if progress.stage == current.progress.stage
            && current.progress.completed_units.is_some()
            && progress.completed_units < current.progress.completed_units
        {
            return;
        }
        if progress
            .completed_units
            .zip(progress.total_units)
            .is_some_and(|(completed, total)| completed > total)
        {
            return;
        }
        let mut next = current;
        next.revision = next.revision.saturating_add(1);
        next.progress = progress;
        self.publish(next);
    }

    pub fn publish_backup_path(&self, backup_path: std::path::PathBuf) {
        let Ok(_publication) = self.publication.lock() else {
            return;
        };
        let mut next = self.snapshot();
        if next.outcome.is_some() {
            return;
        }
        next.revision = next.revision.saturating_add(1);
        next.backup_path = Some(backup_path);
        self.publish(next);
    }

    async fn complete(&self, outcome: SessionOpenTerminalOutcome) {
        {
            let Ok(_publication) = self.publication.lock() else {
                return;
            };
            let mut snapshot = self.snapshot();
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.outcome = Some(outcome);
            self.publish(snapshot);
        }
        *self.completed_at.lock().await = Some(Instant::now());
    }

    async fn completed_at(&self) -> Option<Instant> {
        *self.completed_at.lock().await
    }

    fn is_terminal(&self) -> bool {
        self.snapshots.borrow().outcome.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct SessionMigrationOperations {
    entries: Arc<Mutex<BTreeMap<SessionId, Arc<SessionMigrationOperation>>>>,
    terminal_retention: Duration,
    max_terminal_operations: usize,
}

impl Default for SessionMigrationOperations {
    fn default() -> Self {
        Self::new(DEFAULT_TERMINAL_RETENTION, DEFAULT_MAX_TERMINAL_OPERATIONS)
    }
}

impl SessionMigrationOperations {
    pub(super) fn new(terminal_retention: Duration, max_terminal_operations: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            terminal_retention,
            max_terminal_operations,
        }
    }

    pub async fn start_or_join<F, Fut>(
        &self,
        initial: SessionOpenOperationSnapshot,
        run: F,
    ) -> Arc<SessionMigrationOperation>
    where
        F: FnOnce(Arc<SessionMigrationOperation>) -> Fut + Send + 'static,
        Fut: Future<Output = SessionOpenTerminalOutcome> + Send + 'static,
    {
        self.prune().await;
        let session_id = initial.session_id;
        let operation = {
            let mut entries = self.entries.lock().await;
            if let Some(existing) = entries.get(&session_id) {
                return Arc::clone(existing);
            }
            let operation = Arc::new(SessionMigrationOperation::new(initial));
            entries.insert(session_id, Arc::clone(&operation));
            operation
        };
        let task_operation = Arc::clone(&operation);
        tokio::spawn(async move {
            let outcome = run(Arc::clone(&task_operation)).await;
            let terminal_stage = if matches!(outcome, SessionOpenTerminalOutcome::Ready) {
                SessionMigrationStage::Complete
            } else {
                SessionMigrationStage::Failed
            };
            task_operation.publish_progress(SessionMigrationProgress {
                stage: terminal_stage,
                completed_units: None,
                total_units: None,
                unit: None,
                message: if matches!(outcome, SessionOpenTerminalOutcome::Ready) {
                    "Session storage is ready".to_owned()
                } else {
                    "Session preparation failed".to_owned()
                },
            });
            task_operation.complete(outcome).await;
        });
        operation
    }

    pub async fn get(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Option<Arc<SessionMigrationOperation>> {
        self.prune().await;
        self.entries
            .lock()
            .await
            .get(&session_id)
            .filter(|operation| operation.snapshot().operation_id == operation_id)
            .cloned()
    }

    pub async fn active_count(&self) -> usize {
        self.entries
            .lock()
            .await
            .values()
            .filter(|operation| !operation.is_terminal())
            .count()
    }

    async fn prune(&self) {
        let now = Instant::now();
        let snapshot = self
            .entries
            .lock()
            .await
            .iter()
            .map(|(session_id, operation)| (*session_id, Arc::clone(operation)))
            .collect::<Vec<_>>();
        let mut completed = Vec::new();
        for (session_id, operation) in snapshot {
            if operation.is_terminal()
                && let Some(completed_at) = operation.completed_at().await
            {
                completed.push((session_id, completed_at));
            }
        }
        completed.sort_by_key(|(_, completed_at)| *completed_at);
        let overflow = completed.len().saturating_sub(self.max_terminal_operations);
        let mut remove = completed
            .iter()
            .filter(|(_, completed_at)| {
                now.duration_since(*completed_at) >= self.terminal_retention
            })
            .map(|(session_id, _)| *session_id)
            .collect::<Vec<_>>();
        remove.extend(
            completed
                .iter()
                .take(overflow)
                .map(|(session_id, _)| *session_id),
        );
        remove.sort_unstable();
        remove.dedup();
        let mut entries = self.entries.lock().await;
        for session_id in remove {
            if entries
                .get(&session_id)
                .is_some_and(|operation| operation.is_terminal())
            {
                entries.remove(&session_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        SessionMigrationProgress, SessionMigrationStage, SessionOpenFailureKind,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn snapshot(session_id: SessionId) -> SessionOpenOperationSnapshot {
        SessionOpenOperationSnapshot {
            operation_id: SessionOpenOperationId::new(),
            revision: 0,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage: SessionMigrationStage::WaitingForOwnership,
                completed_units: None,
                total_units: None,
                unit: None,
                message: "Waiting for exclusive ownership".to_owned(),
            },
            outcome: None,
            backup_path: None,
        }
    }

    #[tokio::test]
    async fn concurrent_starts_join_one_running_operation() {
        let operations = SessionMigrationOperations::default();
        let session_id = SessionId::new();
        let initial = snapshot(session_id);
        let operation_id = initial.operation_id;
        let runs = Arc::new(AtomicUsize::new(0));
        let blocker = Arc::new(tokio::sync::Notify::new());
        let first_runs = Arc::clone(&runs);
        let first_blocker = Arc::clone(&blocker);
        let first = operations
            .start_or_join(initial, move |_| async move {
                first_runs.fetch_add(1, Ordering::SeqCst);
                first_blocker.notified().await;
                SessionOpenTerminalOutcome::Ready
            })
            .await;
        let second_runs = Arc::clone(&runs);
        let second = operations
            .start_or_join(snapshot(session_id), move |_| async move {
                second_runs.fetch_add(1, Ordering::SeqCst);
                SessionOpenTerminalOutcome::Failed {
                    kind: SessionOpenFailureKind::MigrationFailed,
                    message: "must not run".to_owned(),
                    backup_path: None,
                }
            })
            .await;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.snapshot().operation_id, operation_id);
        assert_eq!(operations.active_count().await, 1);
        blocker.notify_one();
        let mut receiver = first.subscribe();
        receiver
            .wait_for(|state| state.outcome.is_some())
            .await
            .expect("terminal snapshot");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(operations.active_count().await, 0);
    }

    #[tokio::test]
    async fn pruning_is_bounded_and_never_removes_running_operations() {
        let operations = SessionMigrationOperations::new(Duration::ZERO, 1);
        let running_session = SessionId::new();
        let blocker = Arc::new(tokio::sync::Notify::new());
        let task_blocker = Arc::clone(&blocker);
        let running = operations
            .start_or_join(snapshot(running_session), move |_| async move {
                task_blocker.notified().await;
                SessionOpenTerminalOutcome::Ready
            })
            .await;
        for _ in 0..3 {
            let session_id = SessionId::new();
            let completed = operations
                .start_or_join(snapshot(session_id), |_| async {
                    SessionOpenTerminalOutcome::Ready
                })
                .await;
            let mut receiver = completed.subscribe();
            receiver
                .wait_for(|state| state.outcome.is_some())
                .await
                .expect("terminal snapshot");
        }
        operations.prune().await;
        assert_eq!(operations.active_count().await, 1);
        assert!(
            operations
                .get(running_session, running.snapshot().operation_id)
                .await
                .is_some()
        );
        assert!(operations.entries.lock().await.len() <= 2);
        blocker.notify_one();
    }
}
