//! Lifecycle-owned automatic maintenance task.
//!
//! The task is started only after its caller establishes complete tracking-health coverage. This
//! lifecycle wrapper never treats spawning a task as proof of compatible reader participation.

use super::ServerState;
use bcode_session_models::{SessionId, StorageMaintenanceStatus};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

const MAX_OUTCOMES: usize = 256;

#[derive(Debug, Default)]
struct Attempt {
    at_ms: u64,
    outcome: &'static str,
    reason: &'static str,
    next_retry_ms: Option<u64>,
    #[cfg(test)]
    terminal: Option<bcode_session_models::SessionOpenOperationSnapshot>,
    partial_reason: Option<&'static str>,
}

/// Owns one worker across repeated start/stop calls, including cancelled shutdown waiters.
#[derive(Debug, Default)]
pub struct StorageMaintenanceWorker {
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    enabled: AtomicBool,
    attempts: StdMutex<BTreeMap<SessionId, Attempt>>,
}

struct Running(Arc<AtomicBool>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl StorageMaintenanceWorker {
    /// Return bounded, best-effort diagnostics, never migration or ownership authority.
    #[must_use]
    pub fn status(&self, id: SessionId) -> StorageMaintenanceStatus {
        let worker_running = self.running.load(Ordering::SeqCst);
        let attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let attempt = attempts.get(&id);
        let status = StorageMaintenanceStatus {
            enabled: self.enabled.load(Ordering::SeqCst),
            worker_running,
            last_attempt_ms: attempt.map(|attempt| attempt.at_ms),
            outcome: attempt.map(|attempt| attempt.outcome.to_owned()),
            reason: attempt.map(|attempt| attempt.reason.to_owned()),
            next_retry_ms: attempt.and_then(|attempt| attempt.next_retry_ms),
        };
        drop(attempts);
        status
    }

    #[cfg(test)]
    pub(super) fn record_terminal(
        &self,
        id: SessionId,
        terminal: bcode_session_models::SessionOpenOperationSnapshot,
    ) {
        if let Some(attempt) = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&id)
        {
            attempt.terminal = Some(terminal);
        }
    }

    #[cfg(test)]
    pub(super) fn terminal(
        &self,
        id: SessionId,
    ) -> Option<bcode_session_models::SessionOpenOperationSnapshot> {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .and_then(|attempt| attempt.terminal.clone())
    }

    pub(super) fn latest_outcome(&self, id: SessionId) -> Option<(&'static str, &'static str)> {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .map(|attempt| (attempt.outcome, attempt.reason))
    }

    pub(super) fn record_attempt(
        &self,
        id: SessionId,
        at_ms: u64,
        outcome: &'static str,
        reason: &'static str,
        next_retry_ms: Option<u64>,
    ) {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if attempts.len() == MAX_OUTCOMES
            && !attempts.contains_key(&id)
            && let Some(oldest) = attempts
                .iter()
                .min_by_key(|(_, attempt)| attempt.at_ms)
                .map(|(id, _)| *id)
        {
            attempts.remove(&oldest);
        }
        #[cfg(test)]
        let terminal = attempts
            .get(&id)
            .and_then(|attempt| attempt.terminal.clone());
        let partial_reason = if outcome == "running" && reason == "classification" {
            None
        } else if outcome == "partial" {
            Some(reason)
        } else {
            attempts.get(&id).and_then(|attempt| attempt.partial_reason)
        };
        let (outcome, reason) = if matches!(outcome, "continued" | "completed") {
            partial_reason.map_or((outcome, reason), |reason| ("partial", reason))
        } else {
            (outcome, reason)
        };
        attempts.insert(
            id,
            Attempt {
                at_ms,
                outcome,
                reason,
                next_retry_ms,
                #[cfg(test)]
                terminal,
                partial_reason,
            },
        );
    }

    /// Start at most one worker after the caller verifies tracking-health admission.
    ///
    /// No worker is started once shutdown has been requested or when scheduling is disabled.
    /// A finished task is joined before replacement so an exited worker is restartable.
    pub async fn start(&self, state: Arc<ServerState>) {
        self.enabled.store(
            state.startup_config.session_storage.enabled,
            Ordering::SeqCst,
        );
        let mut task = self.task.lock().await;
        if task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(completed) = task.take()
        {
            let _ = completed.await;
        }
        if task.is_some()
            || state
                .shutdown_requested
                .load(std::sync::atomic::Ordering::SeqCst)
            || !state.startup_config.session_storage.enabled
        {
            return;
        }
        self.running.store(true, Ordering::SeqCst);
        let running = Running(Arc::clone(&self.running));
        *task = Some(tokio::spawn(async move {
            let _running = running;
            super::storage_maintenance::run(state).await;
        }));
    }

    /// Wait for shutdown completion without abandoning task ownership if this future is cancelled.
    ///
    /// The caller must request daemon shutdown first. An in-flight conversion is cooperatively
    /// cancelled by the worker and fully awaited before its durable authority is released.
    ///
    /// # Errors
    /// Returns a task failure. The completed handle is removed only after join resolves.
    pub async fn stop(&self) -> Result<(), tokio::task::JoinError> {
        let mut task = self.task.lock().await;
        let result = if let Some(handle) = task.as_mut() {
            handle.await
        } else {
            Ok(())
        };
        task.take();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_are_bounded_latest_attempts_and_not_raw_failures() {
        let worker = StorageMaintenanceWorker::default();
        let old = SessionId::new();
        worker.record_attempt(old, 1, "deferred", "ownership_unavailable", Some(30_001));
        let status = worker.status(old);
        assert_eq!(status.reason.as_deref(), Some("ownership_unavailable"));
        assert_eq!(status.next_retry_ms, Some(30_001));
        assert!(!status.worker_running);
        for at in 2..=257 {
            worker.record_attempt(SessionId::new(), at, "completed", "sweep_complete", None);
        }
        assert!(worker.status(old).last_attempt_ms.is_none());
        assert_eq!(
            worker.attempts.lock().expect("attempts").len(),
            MAX_OUTCOMES
        );
        let latest = SessionId::new();
        worker.record_attempt(latest, 258, "running", "classification", None);
        worker.record_attempt(latest, 258, "running", "upgrade_admitted", None);
        worker.record_attempt(latest, 259, "completed", "sweep_complete", Some(300_259));
        assert_eq!(worker.status(latest).outcome.as_deref(), Some("completed"));
        assert_eq!(worker.status(latest).last_attempt_ms, Some(259));
    }

    #[tokio::test]
    async fn storage_usage_rejects_missing_canonical_storage_before_backup_budget_exhaustion() {
        let root = tempfile::tempdir().expect("root");
        let sessions = root.path().join("sessions");
        std::fs::create_dir(&sessions).expect("sessions");
        let manager = bcode_session::SessionManager::persistent_lazy(&sessions);
        let state = crate::tests::test_server_state(manager);
        let backups = root.path().join("session-migration-backups");
        std::fs::create_dir(&backups).expect("backups");
        std::fs::create_dir(backups.join("unrelated")).expect("unrelated entry");
        assert!(
            crate::session_operations::storage_usage(&state, SessionId::new(), 1)
                .await
                .is_err()
        );
    }

    #[test]
    fn partial_artifact_failure_survives_history_completion_until_a_new_sweep() {
        let worker = StorageMaintenanceWorker::default();
        let id = SessionId::new();
        worker.record_attempt(id, 1, "partial", "artifact_missing", None);
        worker.record_attempt(id, 2, "continued", "page_complete", None);
        worker.record_attempt(id, 3, "running", "compression", None);
        worker.record_attempt(id, 4, "completed", "sweep_complete", Some(10));
        let status = worker.status(id);
        assert_eq!(status.outcome.as_deref(), Some("partial"));
        assert_eq!(status.reason.as_deref(), Some("artifact_missing"));
        worker.record_attempt(id, 10, "running", "classification", None);
        worker.record_attempt(id, 11, "completed", "sweep_complete", None);
        assert_eq!(worker.status(id).outcome.as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn cancelled_stop_retains_worker_until_actual_completion() {
        let worker = StorageMaintenanceWorker::default();
        let (complete, done) = tokio::sync::oneshot::channel();
        *worker.task.lock().await = Some(tokio::spawn(async move {
            let _ = done.await;
        }));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), worker.stop())
                .await
                .is_err()
        );
        assert!(worker.task.lock().await.is_some());
        complete.send(()).expect("complete");
        worker.stop().await.expect("join");
        assert!(worker.task.lock().await.is_none());
        worker.stop().await.expect("idempotent");
    }

    #[tokio::test]
    async fn completed_worker_can_restart_and_reports_actual_liveness() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let worker = StorageMaintenanceWorker::default();
        *worker.task.lock().await = Some(tokio::spawn(async {}));
        tokio::task::yield_now().await;
        worker.start(Arc::clone(&state)).await;
        let id = SessionId::new();
        assert!(worker.status(id).enabled);
        state.request_shutdown();
        drop(state);
        worker.stop().await.expect("join replacement");
        assert!(!worker.status(id).worker_running);
    }

    #[tokio::test]
    async fn start_is_idempotent_and_shutdown_prevents_restart() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let worker = StorageMaintenanceWorker::default();
        worker.start(Arc::clone(&state)).await;
        let first = worker.task.lock().await.as_ref().expect("started").id();
        worker.start(Arc::clone(&state)).await;
        assert_eq!(
            worker.task.lock().await.as_ref().expect("same task").id(),
            first
        );
        state.request_shutdown();
        worker.stop().await.expect("stop");
        worker.start(Arc::clone(&state)).await;
        drop(state);
        assert!(worker.task.lock().await.is_none());
    }
}
