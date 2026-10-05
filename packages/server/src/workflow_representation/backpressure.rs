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
