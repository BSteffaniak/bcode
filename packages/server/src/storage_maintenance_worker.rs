//! Lifecycle-owned automatic maintenance task.
//!
//! The task is started only after its caller establishes complete tracking-health coverage. This
//! lifecycle wrapper never treats spawning a task as proof of compatible reader participation.

use super::ServerState;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Owns one worker across repeated start/stop calls, including cancelled shutdown waiters.
#[derive(Debug, Default)]
pub struct StorageMaintenanceWorker {
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl StorageMaintenanceWorker {
    /// Start at most one worker after the caller verifies tracking-health admission.
    ///
    /// No worker is started once shutdown has been requested or when scheduling is disabled.
    pub async fn start(&self, state: Arc<ServerState>) {
        let mut task = self.task.lock().await;
        if task.is_some()
            || state
                .shutdown_requested
                .load(std::sync::atomic::Ordering::SeqCst)
            || !state.startup_config.session_storage.enabled
        {
            return;
        }
        *task = Some(tokio::spawn(super::storage_maintenance::run(state)));
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
