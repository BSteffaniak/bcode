//! Causal observation for workflow integration tests, never a scheduling substitute.
use crate::ServerState;

/// Subscribe before inspecting authoritative state. Notifications are hints to reread it;
/// they are neither completion evidence nor permission to dispatch or settle work.
pub struct Progress {
    driver: tokio::sync::watch::Receiver<()>,
    runtime: tokio::sync::watch::Receiver<()>,
    sessions: tokio::sync::broadcast::Receiver<bcode_session::SessionMutationCommitted>,
    interactions: tokio::sync::broadcast::Receiver<bcode_session_models::SessionId>,
}

impl Progress {
    pub(crate) fn subscribe(state: &ServerState) -> Self {
        Self {
            driver: state.workflow_test_progress.subscribe(),
            runtime: state.runtime_work.progress.subscribe(),
            sessions: state.sessions.subscribe_mutations(),
            interactions: state.interaction_invalidations.subscribe(),
        }
    }

    /// Consume our own completed scheduler pass before rereading canonical state.
    /// External session/receipt/runtime notifications remain pending.
    pub(crate) fn drove(&mut self) {
        self.driver.borrow_and_update();
    }

    pub(crate) async fn changed(&mut self) {
        tokio::select! {
            result = self.driver.changed() => result.expect("workflow observer remains open"),
            result = self.runtime.changed() => result.expect("runtime observer remains open"),
            result = self.sessions.recv() => match result {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                Err(error) => panic!("session observer closed: {error}"),
            },
            result = self.interactions.recv() => match result {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                Err(error) => panic!("interaction observer closed: {error}"),
            },
        }
    }
}

/// Stop and join a test-owned driver without claiming any work was settled.
pub async fn stop_driver(state: &ServerState) {
    let task = state
        .workflow_driver_task
        .lock()
        .await
        .take()
        .expect("driver");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn progress_retains_change_between_inspection_and_wait() {
    let state = crate::tests::test_server_state(crate::SessionManager::default());
    let mut progress = Progress::subscribe(&state);
    state.workflow_test_progress.send_replace(());
    drop(state);
    // The acknowledgement is already present; this does not depend on a task winning a race.
    assert!(progress.driver.has_changed().unwrap());
    progress.changed().await;
    assert!(!progress.driver.has_changed().unwrap());
}
