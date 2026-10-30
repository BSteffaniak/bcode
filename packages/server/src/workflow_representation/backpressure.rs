//! Bounded backpressure for replay-safe typed persistence operations, never external dispatch.
use super::representation::RepresentationError;

pub(super) async fn retry<T, F, Fut>(operation: F) -> Result<T, RepresentationError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RepresentationError>>,
{
    retry_observed(None, operation).await
}

/// Observe normalized Busy events in the existing daemon registry, without per-run liveness state.
pub(super) async fn retry_observed<T, F, Fut>(
    metrics: Option<&bcode_metrics::MetricsRegistry>,
    mut operation: F,
) -> Result<T, RepresentationError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RepresentationError>>,
{
    let mut retries = 0_u32;
    loop {
        match operation().await {
            Err(RepresentationError::Database(switchy_database::DatabaseError::Busy))
                if retries < 8 =>
            {
                // Log only the normalized category, never SQL, identities or payloads.
                tracing::debug!("workflow persistence busy; applying bounded async backpressure");
                if let Some(metrics) = metrics {
                    metrics.increment_counter("workflow.persistence.busy_retries");
                }
                retries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(10 * u64::from(retries))).await;
            }
            Err(RepresentationError::Database(switchy_database::DatabaseError::Busy)) => {
                if let Some(metrics) = metrics {
                    metrics.increment_counter("workflow.persistence.busy_exhausted");
                }
                return Err(RepresentationError::Database(
                    switchy_database::DatabaseError::Busy,
                ));
            }
            result => return result,
        }
    }
}

/// Coordinate only local short persistence operations for the same canonical database.
/// Weak entries do not retain a workflow-domain lease or block another process/artifact.
pub(super) fn write_queue(
    path: &std::path::Path,
) -> std::io::Result<std::sync::Arc<tokio::sync::Mutex<()>>> {
    type Queues =
        std::collections::BTreeMap<std::path::PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>;
    static QUEUES: std::sync::OnceLock<std::sync::Mutex<Queues>> = std::sync::OnceLock::new();
    let path = path.canonicalize()?;
    let mut queues = QUEUES
        .get_or_init(|| std::sync::Mutex::new(Queues::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    queues.retain(|_, queue| queue.strong_count() > 0);
    if let Some(queue) = queues.get(&path).and_then(std::sync::Weak::upgrade) {
        return Ok(queue);
    }
    let queue = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    queues.insert(path, std::sync::Arc::downgrade(&queue));
    drop(queues);
    Ok(queue)
}

pub(super) async fn queued<T, F, Fut>(
    path: &std::path::Path,
    operation: F,
) -> Result<T, RepresentationError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RepresentationError>>,
{
    let queue = write_queue(path).map_err(|_| RepresentationError::MaintenanceRequired)?;
    let _guard = queue.lock().await;
    retry(operation).await
}

/// Observe the actual queue guard; telemetry adds no lock or alternate ownership registry.
pub(super) struct ObservedWriteGuard<'a> {
    guard: Option<tokio::sync::MutexGuard<'a, ()>>,
    held: bcode_metrics::MetricsSpan,
    capture: Option<crate::xray_workflow::Boundary>,
}

impl Drop for ObservedWriteGuard<'_> {
    fn drop(&mut self) {
        // Publish release only after relinquishing the real execution guard.
        drop(self.guard.take());
        if let Some(capture) = self.capture.take() {
            capture.finish("released");
        }
        self.held.finish_with_outcome(&"released");
    }
}

/// Measure acquisition separately from actual possession, including cancelled waiters.
/// The caller supplies the original lock future (including any fixture Pending observer).
pub(super) async fn observe_write_lock<'a>(
    metrics: &bcode_metrics::MetricsRegistry,
    labels: bcode_metrics::MetricLabels,
    lock: impl std::future::Future<Output = tokio::sync::MutexGuard<'a, ()>>,
) -> ObservedWriteGuard<'a> {
    metrics.record_event("workflow.writer_queue.wait_started", 1, labels.clone());
    let wait = metrics
        .span("workflow.writer_queue.wait")
        .labels(labels.clone());
    let context = bcode_xray::CaptureContext::current();
    let capture = crate::xray_workflow::Boundary::new(context.clone(), "writer_queue.wait");
    let guard = lock.await;
    capture.finish("acquired");
    wait.finish_ok();
    metrics.record_event("workflow.writer_queue.acquired", 1, labels.clone());
    ObservedWriteGuard {
        guard: Some(guard),
        held: metrics.span("workflow.writer_queue.held").labels(labels),
        capture: Some(crate::xray_workflow::Boundary::new(
            context,
            "writer_queue.held",
        )),
    }
}

pub(super) async fn queued_observed<T, F, Fut>(
    path: &std::path::Path,
    metrics: &bcode_metrics::MetricsRegistry,
    labels: bcode_metrics::MetricLabels,
    operation: F,
) -> Result<T, RepresentationError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RepresentationError>>,
{
    let queue = write_queue(path).map_err(|_| RepresentationError::MaintenanceRequired)?;
    let _guard = observe_write_lock(metrics, labels, queue.lock()).await;
    retry_observed(Some(metrics), operation).await
}

pub(super) fn normalize(error: RepresentationError) -> super::ServerError {
    if matches!(
        error,
        RepresentationError::Database(switchy_database::DatabaseError::Busy)
    ) {
        super::ServerError::WorkflowStorageBusy
    } else {
        drop(error);
        super::unavailable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn xray_observes_real_wait_and_release_without_taking_queue_ownership() {
        use std::future::Future as _;
        let hub = bcode_xray::XrayHub::default();
        let mut observation = hub
            .observe(bcode_xray_models::XrayObserveRequest {
                schema_version: 1,
                session_id: "queue-session".into(),
                deep_capture: false,
            })
            .expect("observe");
        let _ready = observation.recv().await.expect("ready");
        let context = hub.context(bcode_xray_models::XrayCorrelation {
            session_id: "queue-session".into(),
            turn_id: Some("origin-turn".into()),
            run_id: Some("origin-run".into()),
            ..bcode_xray_models::XrayCorrelation::default()
        });
        let queue = tokio::sync::Mutex::new(());
        let holder = queue.lock().await;
        let metrics = bcode_metrics::MetricsRegistry::default();
        let mut waiter = Box::pin(context.scope(observe_write_lock(
            &metrics,
            std::collections::BTreeMap::new(),
            queue.lock(),
        )));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                waiter.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        drop(waiter);
        let started = observation.recv().await.expect("wait start");
        let unfinished = observation.recv().await.expect("wait dropped");
        assert_eq!(started.event.name, "writer_queue.wait");
        assert_eq!(unfinished.event.data["outcome"], "unfinished");
        assert_eq!(
            unfinished.event.correlation.turn_id.as_deref(),
            Some("origin-turn")
        );
        assert!(
            queue.try_lock().is_err(),
            "observer did not release real holder"
        );
        drop(holder);
        let held = context
            .scope(observe_write_lock(
                &metrics,
                std::collections::BTreeMap::new(),
                queue.lock(),
            ))
            .await;
        drop(held);
        let mut outcomes = Vec::new();
        for _ in 0..4 {
            let event = observation.recv().await.expect("acquisition event").event;
            outcomes.push((event.name, event.data["outcome"].clone()));
        }
        assert_eq!(
            outcomes[1],
            ("writer_queue.wait".into(), serde_json::json!("acquired"))
        );
        assert_eq!(
            outcomes[3],
            ("writer_queue.held".into(), serde_json::json!("released"))
        );
        assert!(
            queue.try_lock().is_ok(),
            "capture must not retain the real guard"
        );
    }

    #[tokio::test]
    async fn observed_queue_separates_wait_from_holder_and_cancelled_waiter() {
        use std::future::Future as _;
        let file = tempfile::NamedTempFile::new().expect("queue file");
        let queue = write_queue(file.path()).expect("queue");
        let metrics = bcode_metrics::MetricsRegistry::default();
        let labels = |operation: &str| {
            std::collections::BTreeMap::from([("operation".into(), operation.into())])
        };
        let held = observe_write_lock(&metrics, labels("holder"), queue.lock()).await;
        let mut waiter = Box::pin(observe_write_lock(&metrics, labels("waiter"), queue.lock()));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                waiter.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        assert!(
            !metrics
                .report()
                .events
                .iter()
                .any(|event| event.name == "workflow.writer_queue.acquired"
                    && event.labels.get("operation").map(String::as_str) == Some("waiter"))
        );
        drop(waiter);
        assert!(
            queue.try_lock().is_err(),
            "waiter cancellation cannot release holder"
        );
        assert!(
            metrics
                .report()
                .events
                .iter()
                .any(|event| event.name == "workflow.writer_queue.wait.total"
                    && event.labels.get("operation").map(String::as_str) == Some("waiter")
                    && event.labels.get("outcome").map(String::as_str) == Some("unfinished"))
        );
        let mut successor = Box::pin(observe_write_lock(
            &metrics,
            labels("successor"),
            queue.lock(),
        ));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                successor.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        drop(held);
        let acquired = successor.await;
        assert!(queue.try_lock().is_err());
        drop(acquired);
        assert!(queue.try_lock().is_ok());
        let events = metrics.report().events;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.name == "workflow.writer_queue.held.total")
                .count(),
            2
        );
        assert!(
            events
                .iter()
                .filter(|event| event.name == "workflow.writer_queue.held.total")
                .all(|event| event.labels.get("outcome").map(String::as_str) == Some("released"))
        );
    }

    #[tokio::test]
    async fn cancelled_waiter_never_executes_and_holder_release_unblocks_same_path() {
        let file = tempfile::NamedTempFile::new().expect("file");
        let queue = write_queue(file.path()).expect("queue");
        let guard = queue.lock().await;
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = invoked.clone();
        let path = file.path().to_path_buf();
        let waiter = tokio::spawn(async move {
            queued(&path, || {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Ok(()))
            })
            .await
        });
        tokio::task::yield_now().await;
        waiter.abort();
        assert!(waiter.await.expect_err("cancelled").is_cancelled());
        assert!(!invoked.load(std::sync::atomic::Ordering::SeqCst));
        drop(guard);
        queued(file.path(), || std::future::ready(Ok(())))
            .await
            .expect("released queue");
        assert!(
            queue.try_lock().is_ok(),
            "no guard retained after operation"
        );
    }

    #[tokio::test]
    async fn observed_busy_counts_retries_without_claiming_current_pressure() {
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let calls = std::cell::Cell::new(0);
        let result = retry_observed(Some(&metrics), || {
            calls.set(calls.get() + 1);
            std::future::ready(if calls.get() < 3 {
                Err(RepresentationError::Database(
                    switchy_database::DatabaseError::Busy,
                ))
            } else {
                Ok(())
            })
        })
        .await;
        assert!(result.is_ok());
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.counters["workflow.persistence.busy_retries"], 2);
        assert!(
            !snapshot
                .counters
                .contains_key("workflow.persistence.busy_exhausted")
        );
        let result: Result<(), _> = retry_observed(Some(&metrics), || {
            std::future::ready(Err(RepresentationError::Database(
                switchy_database::DatabaseError::Busy,
            )))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            metrics.snapshot().counters["workflow.persistence.busy_exhausted"],
            1
        );
    }

    #[tokio::test]
    async fn busy_retries_are_bounded_and_nonbusy_failures_are_not_replayed() {
        let calls = std::cell::Cell::new(0);
        let result: Result<(), _> = retry(|| {
            calls.set(calls.get() + 1);
            std::future::ready(Err(RepresentationError::Database(
                switchy_database::DatabaseError::Busy,
            )))
        })
        .await;
        assert!(matches!(
            result,
            Err(RepresentationError::Database(
                switchy_database::DatabaseError::Busy
            ))
        ));
        assert_eq!(calls.get(), 9);
        calls.set(0);
        let result: Result<(), _> = retry(|| {
            calls.set(calls.get() + 1);
            std::future::ready(Err(RepresentationError::AuthorityConflict))
        })
        .await;
        assert!(matches!(
            result,
            Err(RepresentationError::AuthorityConflict)
        ));
        assert_eq!(calls.get(), 1);
    }
}
