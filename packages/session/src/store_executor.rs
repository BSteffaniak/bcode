//! Async adapter for blocking session store operations.

use super::{
    SessionEventStore, SessionMigrationReport, SessionState, SessionStoreError, index,
    spawn_blocking,
};
use bcode_session_models::{SessionEvent, SessionId, SessionSummary};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

#[allow(dead_code)]
pub struct PersistedSessionMetadata {
    pub summary: SessionSummary,
    pub working_directory: PathBuf,
    pub next_sequence: u64,
    pub event_count: usize,
    pub has_user_message: bool,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub current_agent: Option<String>,
    pub latest_compaction_sequence: Option<u64>,
    pub total_metered_tokens: u64,
    pub index_issues: Vec<index::SessionIndexIssue>,
}

#[allow(dead_code)]
impl PersistedSessionMetadata {
    pub fn from_state(state: &SessionState) -> Self {
        Self {
            summary: state.summary(),
            working_directory: state.working_directory.clone(),
            next_sequence: state.next_sequence,
            event_count: state.event_count,
            has_user_message: state.has_user_message,
            current_provider: state.current_provider.clone(),
            current_model: state.current_model.clone(),
            current_agent: state.current_agent.clone(),
            latest_compaction_sequence: state.latest_compaction_sequence,
            total_metered_tokens: state.total_metered_tokens,
            index_issues: state.index_issues.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionStoreExecutor {
    store: SessionEventStore,
}

impl SessionStoreExecutor {
    pub fn root_path(&self) -> PathBuf {
        self.store.root().to_path_buf()
    }

    pub const fn new(store: SessionEventStore) -> Self {
        Self { store }
    }

    pub(crate) fn metrics(&self) -> bcode_metrics::MetricsRegistry {
        self.store.metrics.clone()
    }

    pub async fn load_catalog(
        &self,
    ) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.load_catalog()).await?
    }

    pub async fn load_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionState>, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.load_session.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.load_session(session_id);
            store
                .metrics
                .record_histogram("session.store.load_session.duration_ms", timer.elapsed_ms());
            result
        })
        .await?
    }

    pub async fn migrate_event_log_to_current(
        &self,
        session_id: SessionId,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.migrate_event_log.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.migrate_event_log_to_current(session_id);
            store.metrics.record_histogram(
                "session.store.migrate_event_log.duration_ms",
                timer.elapsed_ms(),
            );
            result
        })
        .await?
    }

    pub async fn delete(&self, session_id: SessionId) -> Result<(), SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.delete(session_id)).await?
    }

    #[allow(dead_code)]
    pub async fn append_event_frame(&self, event: SessionEvent) -> Result<(), SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.append_event_frame.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.append(&event).map(|_entry| ());
            store.metrics.record_histogram(
                "session.store.append_event_frame.duration_ms",
                timer.elapsed_ms(),
            );
            result
        })
        .await?
    }

    #[allow(dead_code)]
    pub async fn write_metadata_index(
        &self,
        metadata: PersistedSessionMetadata,
    ) -> Result<(), SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.write_metadata_index.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.write_metadata_index(&metadata);
            store.metrics.record_histogram(
                "session.store.write_metadata_index.duration_ms",
                timer.elapsed_ms(),
            );
            result
        })
        .await?
    }

    pub async fn read_legacy_events_for_migration(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.read_legacy_events_for_migration.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.read_legacy_events_for_migration(session_id);
            if let Ok(events) = &result {
                store.metrics.record_histogram(
                    "session.store.read_legacy_events_for_migration.event_count",
                    usize_to_u64(events.len()),
                );
            }
            store.metrics.record_histogram(
                "session.store.read_legacy_events_for_migration.duration_ms",
                timer.elapsed_ms(),
            );
            result
        })
        .await?
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
