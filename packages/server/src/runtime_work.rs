use bcode_ipc::RuntimeWorkSnapshot;
use bcode_plugin::PluginInvocationCancelHandle;
use bcode_session_models::{RuntimeWorkKind, RuntimeWorkStatus, SessionId, WorkId};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::TurnCancelState;

/// Server-owned cancellation handle for active runtime work.
#[derive(Debug, Clone)]
pub enum CancellationHandle {
    /// Cancel a whole session/model turn.
    SessionTurn(Arc<TurnCancelState>),
    /// Cancel an in-flight plugin invocation.
    PluginInvocation(PluginInvocationCancelHandle),
    /// Cancel a Ralph autonomous runner.
    RalphRun {
        /// Store containing the run record.
        store: bcode_ralph::RalphStateStore,
        /// Run ID to mark cancellation-requested.
        run_id: String,
    },
    /// Test/no-op cancellation hook.
    #[cfg(test)]
    Test(Arc<std::sync::atomic::AtomicUsize>),
    /// Test cancellation hook that never returns.
    #[cfg(test)]
    TestBlocked(Arc<tokio::sync::Notify>),
}

impl CancellationHandle {
    /// Request cancellation through the underlying handle.
    pub async fn cancel(&self) {
        match self {
            Self::SessionTurn(cancel_state) => cancel_state.cancel().await,
            Self::PluginInvocation(cancel) => cancel.cancel(),
            Self::RalphRun { store, run_id } => {
                let _ = store.request_run_cancel(run_id);
            }
            #[cfg(test)]
            Self::Test(count) => {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            #[cfg(test)]
            Self::TestBlocked(blocked) => blocked.notified().await,
        }
    }

    /// Runtime work with a handle is cancellable.
    pub const fn is_cancellable(&self) -> bool {
        match self {
            Self::SessionTurn(_) | Self::PluginInvocation(_) | Self::RalphRun { .. } => true,
            #[cfg(test)]
            Self::Test(_) | Self::TestBlocked(_) => true,
        }
    }
}

/// Specification for starting runtime work.
#[derive(Debug, Clone)]
pub struct RuntimeWorkSpec {
    pub work_id: WorkId,
    pub kind: RuntimeWorkKind,
    pub label: String,
    pub tool_call_id: Option<String>,
    pub plugin_id: Option<String>,
    pub service_interface: Option<String>,
    pub operation: Option<String>,
    pub parent_work_id: Option<WorkId>,
    pub cancellation: CancellationHandle,
}

impl RuntimeWorkSpec {
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(
        work_id: WorkId,
        kind: RuntimeWorkKind,
        label: String,
        cancellation: CancellationHandle,
    ) -> Self {
        Self {
            work_id,
            kind,
            label,
            tool_call_id: None,
            plugin_id: None,
            service_interface: None,
            operation: None,
            parent_work_id: None,
            cancellation,
        }
    }

    pub fn with_tool_call_id(mut self, tool_call_id: Option<String>) -> Self {
        self.tool_call_id = tool_call_id;
        self
    }

    pub fn with_parent_work_id(mut self, parent_work_id: Option<WorkId>) -> Self {
        self.parent_work_id = parent_work_id;
        self
    }
}

#[derive(Debug, Clone)]
struct ActiveRuntimeWork {
    spec: RuntimeWorkSpec,
    cancelled: bool,
}

/// Central server registry and cancellation router for active runtime work.
#[derive(Debug, Default)]
pub struct RuntimeWorkManager {
    active: Mutex<BTreeMap<(SessionId, WorkId), ActiveRuntimeWork>>,
}

impl RuntimeWorkManager {
    /// Register active work and return whether it should be advertised as cancellable.
    pub async fn start(&self, session_id: SessionId, spec: RuntimeWorkSpec) -> bool {
        let cancellable = spec.cancellation.is_cancellable();
        self.active.lock().await.insert(
            (session_id, spec.work_id.clone()),
            ActiveRuntimeWork {
                spec,
                cancelled: false,
            },
        );
        cancellable
    }

    /// Replace the cancellation handle for existing active work.
    pub async fn replace_cancellation(
        &self,
        session_id: SessionId,
        work_id: &WorkId,
        cancellation: CancellationHandle,
    ) -> bool {
        let mut active = self.active.lock().await;
        let replaced = if let Some(work) = active.get_mut(&(session_id, work_id.clone())) {
            work.spec.cancellation = cancellation;
            true
        } else {
            false
        };
        drop(active);
        replaced
    }

    /// Cancel active work by exact ID and return every work item newly signalled.
    pub async fn cancel_with_children(
        &self,
        session_id: SessionId,
        work_id: &WorkId,
    ) -> Vec<WorkId> {
        let mut active = self.active.lock().await;
        if !active.contains_key(&(session_id, work_id.clone())) {
            return Vec::new();
        }
        let mut pending = vec![work_id.clone()];
        let mut cancellations = Vec::new();
        let mut cancelled_work_ids = Vec::new();
        while let Some(next_work_id) = pending.pop() {
            let key = (session_id, next_work_id.clone());
            let Some(work) = active.get_mut(&key) else {
                continue;
            };
            if work.cancelled {
                continue;
            }
            work.cancelled = true;
            cancelled_work_ids.push(next_work_id.clone());
            cancellations.push(work.spec.cancellation.clone());
            let children = active
                .iter()
                .filter(|((child_session_id, _), child)| {
                    *child_session_id == session_id
                        && child.spec.parent_work_id.as_ref() == Some(&next_work_id)
                })
                .map(|((_, child_work_id), _)| child_work_id.clone())
                .collect::<Vec<_>>();
            pending.extend(children);
        }
        drop(active);
        for cancellation in cancellations {
            tokio::spawn(async move {
                cancellation.cancel().await;
            });
        }
        cancelled_work_ids
    }

    /// Finish work and remove it from the active registry.
    pub async fn finish(&self, session_id: SessionId, work_id: &WorkId) {
        self.active
            .lock()
            .await
            .remove(&(session_id, work_id.clone()));
    }

    /// Return active work snapshots for a session.
    pub async fn active_for_session(&self, session_id: SessionId) -> Vec<RuntimeWorkSnapshot> {
        self.active
            .lock()
            .await
            .iter()
            .filter(|((active_session_id, _), _)| *active_session_id == session_id)
            .map(|((_, work_id), work)| RuntimeWorkSnapshot {
                work_id: work_id.clone(),
                kind: work.spec.kind,
                label: work.spec.label.clone(),
                tool_call_id: work.spec.tool_call_id.clone(),
                status: if work.cancelled {
                    RuntimeWorkStatus::Cancelling
                } else {
                    RuntimeWorkStatus::Running
                },
                cancellable: work.spec.cancellation.is_cancellable(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn wait_for_count(count: &AtomicUsize, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while count.load(Ordering::SeqCst) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached cancellation should be dispatched");
    }

    fn test_spec(work_id: &str, count: Arc<AtomicUsize>) -> RuntimeWorkSpec {
        RuntimeWorkSpec::new(
            WorkId::new(work_id),
            RuntimeWorkKind::Tool,
            work_id.to_string(),
            CancellationHandle::Test(count),
        )
    }

    #[tokio::test]
    async fn start_cancel_finish_tracks_active_work() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let count = Arc::new(AtomicUsize::new(0));
        let work_id = WorkId::new("work");

        assert!(
            manager
                .start(session_id, test_spec("work", Arc::clone(&count)))
                .await
        );
        assert_eq!(manager.active_for_session(session_id).await.len(), 1);
        assert!(
            !manager
                .cancel_with_children(session_id, &work_id)
                .await
                .is_empty()
        );
        wait_for_count(&count, 1).await;
        assert_eq!(
            manager.active_for_session(session_id).await[0].status,
            RuntimeWorkStatus::Cancelling
        );
        manager.finish(session_id, &work_id).await;
        assert!(manager.active_for_session(session_id).await.is_empty());
    }

    #[tokio::test]
    async fn duplicate_cancel_only_signals_once() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let count = Arc::new(AtomicUsize::new(0));
        let work_id = WorkId::new("work");

        manager
            .start(session_id, test_spec("work", Arc::clone(&count)))
            .await;
        assert!(
            !manager
                .cancel_with_children(session_id, &work_id)
                .await
                .is_empty()
        );
        assert!(
            manager
                .cancel_with_children(session_id, &work_id)
                .await
                .is_empty()
        );
        wait_for_count(&count, 1).await;
    }

    #[tokio::test]
    async fn replace_cancellation_changes_signal_target() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let work_id = WorkId::new("work");

        manager
            .start(session_id, test_spec("work", Arc::clone(&first)))
            .await;
        assert!(
            manager
                .replace_cancellation(
                    session_id,
                    &work_id,
                    CancellationHandle::Test(Arc::clone(&second))
                )
                .await
        );
        assert!(
            !manager
                .cancel_with_children(session_id, &work_id)
                .await
                .is_empty()
        );
        assert_eq!(first.load(Ordering::SeqCst), 0);
        wait_for_count(&second, 1).await;
    }
    #[tokio::test]
    async fn cancelling_parent_cancels_child() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let parent_count = Arc::new(AtomicUsize::new(0));
        let child_count = Arc::new(AtomicUsize::new(0));
        let parent_id = WorkId::new("parent");
        let child_id = WorkId::new("child");

        manager
            .start(session_id, test_spec("parent", Arc::clone(&parent_count)))
            .await;
        manager
            .start(
                session_id,
                test_spec("child", Arc::clone(&child_count))
                    .with_parent_work_id(Some(parent_id.clone())),
            )
            .await;

        assert!(
            !manager
                .cancel_with_children(session_id, &parent_id)
                .await
                .is_empty()
        );
        wait_for_count(&parent_count, 1).await;
        wait_for_count(&child_count, 1).await;
        assert_eq!(
            manager
                .active_for_session(session_id)
                .await
                .into_iter()
                .find(|work| work.work_id == child_id)
                .expect("child remains active until finished")
                .status,
            RuntimeWorkStatus::Cancelling
        );
    }
    #[tokio::test]
    async fn non_returning_cleanup_cannot_delay_local_cancellation() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let work_id = WorkId::new("blocked");
        manager
            .start(
                session_id,
                RuntimeWorkSpec::new(
                    work_id.clone(),
                    RuntimeWorkKind::Tool,
                    "blocked cleanup".to_string(),
                    CancellationHandle::TestBlocked(Arc::new(tokio::sync::Notify::new())),
                ),
            )
            .await;

        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            manager.cancel_with_children(session_id, &work_id),
        )
        .await
        .expect("local cancellation must not await cleanup");

        assert_eq!(cancelled, vec![work_id]);
        assert_eq!(
            manager.active_for_session(session_id).await[0].status,
            RuntimeWorkStatus::Cancelling
        );
    }
}
