//! Request-scoped observations of child publication work; never persisted.
use bcode_session_models::SessionId;
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

/// Existing operations within fresh child publication. Nested stages overlap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildCreationStage {
    ParentIntent,
    MaintenanceOwnership,
    Staging,
    DatabaseConnect,
    DatabaseConfiguration,
    SchemaInitialization,
    StorageContract,
    CreationEvents,
    ExecutionEvent,
    Validation,
    StateLoad,
    DatabaseClose,
    Manifest,
    RenameAndSync,
    Publication,
    CatalogFlush,
    CatalogSchedule,
}

/// One completed operation, without event payloads or filesystem paths.
#[derive(Clone, Copy, Debug)]
pub struct ChildCreationObservation {
    /// Session whose operation was observed; parent database work retains parent identity.
    pub session_id: SessionId,
    /// Operation that completed, failed, or was dropped.
    pub stage: ChildCreationStage,
    /// Wall-clock duration; nested operations are not additive.
    pub elapsed: Duration,
}

/// A nonblocking observer. Callbacks must not mutate session state or perform I/O.
pub type ChildCreationObserver = Arc<dyn Fn(ChildCreationObservation) + Send + Sync>;

tokio::task_local! {
    static OBSERVER: ChildCreationObserver;
}

/// Observe this future only. The caller must retain ownership of authoritative work.
/// Spawned tasks do not inherit the observer. No history, migration, or repair is initiated.
pub async fn observe_child_creation<T>(
    observer: ChildCreationObserver,
    future: impl Future<Output = T>,
) -> T {
    OBSERVER.scope(observer, future).await
}

pub(crate) struct Timer {
    observation: ChildCreationObservation,
    started: Instant,
    observer: ChildCreationObserver,
}

pub(crate) fn timer(session_id: SessionId, stage: ChildCreationStage) -> Option<Timer> {
    OBSERVER
        .try_with(|observer| Timer {
            observation: ChildCreationObservation {
                session_id,
                stage,
                elapsed: Duration::ZERO,
            },
            started: Instant::now(),
            observer: Arc::clone(observer),
        })
        .ok()
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.observation.elapsed = self.started.elapsed();
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.observer)(self.observation);
        }))
        .is_err()
        {
            tracing::warn!("child creation observer failed; authoritative operation continues");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn child_creation_observer_panic_does_not_abort_operation() {
        let observer: ChildCreationObserver = Arc::new(|_| panic!("observer fault"));
        let completed = observe_child_creation(observer, async {
            drop(timer(SessionId::new(), ChildCreationStage::Staging));
            true
        })
        .await;
        assert!(completed);
    }

    #[tokio::test]
    async fn child_creation_observer_is_request_scoped() {
        let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&observations);
        let observer: ChildCreationObserver =
            Arc::new(move |value| recorded.lock().unwrap().push(value));
        let id = SessionId::new();
        assert!(timer(id, ChildCreationStage::Staging).is_none());
        observe_child_creation(observer, async {
            drop(timer(id, ChildCreationStage::Staging));
            tokio::spawn(async move {
                assert!(timer(id, ChildCreationStage::Staging).is_none());
            })
            .await
            .unwrap();
        })
        .await;
        assert!(timer(id, ChildCreationStage::Staging).is_none());
        assert_eq!(observations.lock().unwrap().len(), 1);
    }
}
