//! Bounded, latest-value persistence for derived session catalog summaries.

use crate::{SessionId, db};
use bcode_metrics::MetricsRegistry;
use bcode_session_models::SessionSummary;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, oneshot};

const CATALOG_FLUSH_DELAY: Duration = Duration::from_millis(500);
const CATALOG_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Bounded coordinator that persists only the latest derived summary for each session.
#[derive(Debug, Clone)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct CatalogUpdateCoordinator {
    shared: Arc<CatalogUpdateShared>,
}

#[derive(Debug)]
struct CatalogUpdateShared {
    root: PathBuf,
    metrics: MetricsRegistry,
    state: Mutex<CatalogUpdateState>,
    notify: Notify,
    worker_started: AtomicBool,
    #[cfg(test)]
    fail_next_flush_before_commit: AtomicBool,
}

#[derive(Debug, Default)]
struct CatalogUpdateState {
    pending: BTreeMap<SessionId, SessionSummary>,
    tombstones: BTreeMap<SessionId, u64>,
    pending_deletes: BTreeMap<SessionId, u64>,
    flush_waiters: Vec<oneshot::Sender<()>>,
    started: bool,
    shutting_down: bool,
}

impl CatalogUpdateCoordinator {
    /// Create a coordinator for one canonical session-store root.
    #[must_use]
    pub(crate) fn new(root: PathBuf, metrics: MetricsRegistry) -> Self {
        Self {
            shared: Arc::new(CatalogUpdateShared {
                root,
                metrics,
                state: Mutex::new(CatalogUpdateState::default()),
                notify: Notify::new(),
                worker_started: AtomicBool::new(false),
                #[cfg(test)]
                fail_next_flush_before_commit: AtomicBool::new(false),
            }),
        }
    }

    /// Start the single background persistence worker.
    pub(crate) fn start(&self) {
        if self.shared.worker_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move { shared.run().await });
    }

    #[cfg(test)]
    pub(super) fn fail_next_flush_before_commit(&self) {
        self.shared
            .fail_next_flush_before_commit
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(super) async fn pending_len(&self) -> usize {
        self.shared.state.lock().await.pending.len()
    }

    #[cfg(test)]
    pub(super) async fn tombstone_len(&self) -> usize {
        self.shared.state.lock().await.pending_deletes.len()
    }

    pub(super) async fn schedule(&self, summary: SessionSummary) {
        self.start();
        let mut state = self.shared.state.lock().await;
        if state.shutting_down {
            return;
        }
        if state
            .tombstones
            .get(&summary.id)
            .is_some_and(|deleted_at| *deleted_at >= summary.updated_at_ms)
        {
            self.shared
                .metrics
                .increment_counter("session.catalog.tombstone_skip_total");
            return;
        }
        let should_replace = state
            .pending
            .get(&summary.id)
            .is_none_or(|pending| pending.updated_at_ms <= summary.updated_at_ms);
        if should_replace {
            if state.pending.insert(summary.id, summary).is_some() {
                self.shared
                    .metrics
                    .increment_counter("session.catalog.coalesced_total");
            }
            self.shared.metrics.set_gauge(
                "session.catalog.pending_sessions",
                i64::try_from(state.pending.len()).unwrap_or(i64::MAX),
            );
            self.shared
                .metrics
                .increment_counter("session.catalog.scheduled_total");
        }
        if !state.started {
            state.started = true;
            drop(state);
            self.shared.notify.notify_one();
        }
    }

    pub(super) async fn delete(&self, session_id: SessionId, updated_at_ms: u64) {
        self.start();
        let mut state = self.shared.state.lock().await;
        state.pending.remove(&session_id);
        state
            .tombstones
            .entry(session_id)
            .and_modify(|existing| *existing = (*existing).max(updated_at_ms))
            .or_insert(updated_at_ms);
        state
            .pending_deletes
            .entry(session_id)
            .and_modify(|existing| *existing = (*existing).max(updated_at_ms))
            .or_insert(updated_at_ms);
        self.shared.metrics.set_gauge(
            "session.catalog.pending_sessions",
            i64::try_from(state.pending.len()).unwrap_or(i64::MAX),
        );
        if !state.started {
            state.started = true;
            drop(state);
            self.shared.notify.notify_one();
        }
    }

    pub(super) async fn flush(&self) {
        self.start();
        let (sender, receiver) = oneshot::channel();
        let mut state = self.shared.state.lock().await;
        state.flush_waiters.push(sender);
        state.started = true;
        drop(state);
        self.shared.notify.notify_one();
        let _ = receiver.await;
    }

    pub(super) async fn shutdown(&self) {
        self.start();
        let (sender, receiver) = oneshot::channel();
        let mut state = self.shared.state.lock().await;
        state.shutting_down = true;
        state.flush_waiters.push(sender);
        state.started = true;
        drop(state);
        self.shared.notify.notify_one();
        let _ = receiver.await;
    }
}

impl CatalogUpdateShared {
    #[allow(clippy::too_many_lines)]
    async fn run(self: Arc<Self>) {
        loop {
            self.notify.notified().await;
            let flush_immediately = !self.state.lock().await.flush_waiters.is_empty();
            if !flush_immediately {
                tokio::time::sleep(CATALOG_FLUSH_DELAY).await;
            }
            loop {
                let (summaries, deleted, waiters, shutting_down) = {
                    let mut state = self.state.lock().await;
                    state.started = false;
                    let drained = (
                        std::mem::take(&mut state.pending),
                        std::mem::take(&mut state.pending_deletes),
                        std::mem::take(&mut state.flush_waiters),
                        state.shutting_down,
                    );
                    self.metrics
                        .set_gauge("session.catalog.pending_sessions", 0);
                    drained
                };

                if summaries.is_empty() && deleted.is_empty() {
                    for waiter in waiters {
                        let _ = waiter.send(());
                    }
                    if shutting_down {
                        return;
                    }
                    break;
                }

                let persist_started = Instant::now();
                match self.persist(&summaries, &deleted).await {
                    Ok(()) => {
                        self.metrics
                            .increment_counter("session.catalog.flush_total");
                        self.metrics.record_histogram(
                            "session.catalog.flush_duration_ms",
                            u64::try_from(persist_started.elapsed().as_millis())
                                .unwrap_or(u64::MAX),
                        );
                        self.metrics.record_histogram(
                            "session.catalog.flush_summaries",
                            u64::try_from(summaries.len()).unwrap_or(u64::MAX),
                        );
                        self.metrics.record_histogram(
                            "session.catalog.flush_deletions",
                            u64::try_from(deleted.len()).unwrap_or(u64::MAX),
                        );
                        for waiter in waiters {
                            let _ = waiter.send(());
                        }
                        if shutting_down {
                            let state = self.state.lock().await;
                            if state.pending.is_empty() && state.pending_deletes.is_empty() {
                                drop(state);
                                self.metrics
                                    .set_gauge("session.catalog.pending_sessions", 0);
                                return;
                            }
                        }
                        break;
                    }
                    Err(CatalogFlushError::AfterCommit(error)) => {
                        self.metrics
                            .increment_counter("session.catalog.close_error_total");
                        tracing::warn!(%error, "session catalog batch committed but close failed");
                        for waiter in waiters {
                            let _ = waiter.send(());
                        }
                        if shutting_down {
                            let state = self.state.lock().await;
                            if state.pending.is_empty() && state.pending_deletes.is_empty() {
                                drop(state);
                                self.metrics
                                    .set_gauge("session.catalog.pending_sessions", 0);
                                return;
                            }
                        }
                        break;
                    }
                    Err(error @ CatalogFlushError::BeforeCommit(_)) => {
                        self.metrics
                            .increment_counter("session.catalog.flush_error_total");
                        self.metrics
                            .increment_counter("session.catalog.retry_total");
                        tracing::warn!(%error, "failed to flush derived session catalog");
                        let mut state = self.state.lock().await;
                        for (session_id, summary) in summaries {
                            if state
                                .tombstones
                                .get(&session_id)
                                .is_none_or(|deleted_at| *deleted_at < summary.updated_at_ms)
                                && state.pending.get(&session_id).is_none_or(|pending| {
                                    pending.updated_at_ms <= summary.updated_at_ms
                                })
                            {
                                state.pending.insert(session_id, summary);
                            }
                        }
                        for (session_id, deleted_at) in deleted {
                            state.pending.remove(&session_id);
                            state
                                .pending_deletes
                                .entry(session_id)
                                .and_modify(|existing| *existing = (*existing).max(deleted_at))
                                .or_insert(deleted_at);
                            state
                                .tombstones
                                .entry(session_id)
                                .and_modify(|existing| *existing = (*existing).max(deleted_at))
                                .or_insert(deleted_at);
                        }
                        state.flush_waiters.extend(waiters);
                        self.metrics.set_gauge(
                            "session.catalog.pending_sessions",
                            i64::try_from(state.pending.len()).unwrap_or(i64::MAX),
                        );
                        drop(state);
                        tokio::time::sleep(CATALOG_RETRY_DELAY).await;
                    }
                }
            }
        }
    }

    async fn persist(
        &self,
        summaries: &BTreeMap<SessionId, SessionSummary>,
        deleted: &BTreeMap<SessionId, u64>,
    ) -> Result<(), CatalogFlushError> {
        #[cfg(test)]
        if self
            .fail_next_flush_before_commit
            .swap(false, Ordering::SeqCst)
        {
            return Err(CatalogFlushError::BeforeCommit(
                crate::db::SessionDbError::Database(switchy::database::DatabaseError::QueryFailed(
                    "injected catalog flush failure".to_owned(),
                )),
            ));
        }
        let catalog = match db::GlobalSessionDb::open_existing_turso_in_root_observed(
            &self.root,
            self.metrics.clone(),
        )
        .await
        {
            Ok(catalog) => catalog,
            Err(crate::db::SessionDbError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                db::GlobalSessionDb::initialize_turso_in_root_observed(
                    &self.root,
                    self.metrics.clone(),
                )
                .await
                .map_err(CatalogFlushError::BeforeCommit)?
            }
            Err(error) => {
                self.metrics
                    .increment_counter("session.catalog.schema_degraded_total");
                return Err(CatalogFlushError::BeforeCommit(error));
            }
        };
        let summaries = summaries.values().cloned().collect::<Vec<_>>();
        let write_result = catalog
            .apply_catalog_batch(&self.root, &summaries, deleted.keys().copied())
            .await;
        let close_result = catalog.close().await;
        match write_result {
            Err(error) => Err(CatalogFlushError::BeforeCommit(error)),
            Ok(()) => close_result.map_err(CatalogFlushError::AfterCommit),
        }
    }
}

#[derive(Debug)]
enum CatalogFlushError {
    BeforeCommit(crate::db::SessionDbError),
    AfterCommit(crate::db::SessionDbError),
}

impl std::fmt::Display for CatalogFlushError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeCommit(error) => {
                write!(formatter, "catalog flush failed before commit: {error}")
            }
            Self::AfterCommit(error) => {
                write!(formatter, "catalog close failed after commit: {error}")
            }
        }
    }
}
