//! Async adapter for blocking session store operations.

use super::{
    SessionEventStore, SessionMigrationReport, SessionState, SessionStoreError, index,
    spawn_blocking,
};
use bcode_session_models::{
    SessionEvent, SessionHistoryPage, SessionHistoryQuery, SessionId, SessionInputHistoryEntry,
    SessionSummary,
};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

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

    pub async fn ensure_fresh_index(
        &self,
        session_id: SessionId,
    ) -> Result<index::SessionIndex, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.ensure_fresh_index.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.ensure_fresh_index(session_id);
            store.metrics.record_histogram(
                "session.store.ensure_fresh_index.duration_ms",
                timer.elapsed_ms(),
            );
            result
        })
        .await?
    }

    pub async fn ensure_transcript_index(
        &self,
        session_id: SessionId,
    ) -> Result<crate::derived::TranscriptIndex, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.ensure_transcript_index.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let event_path = store.event_path(session_id);
            let result =
                crate::derived::ensure_transcript_index(&store.root, session_id, &event_path);
            store.metrics.record_histogram(
                "session.store.ensure_transcript_index.duration_ms",
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

    pub async fn append_event_frame(&self, event: SessionEvent) -> Result<(), SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.append_event_frame.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.append(&event).map(|_| ());
            store.metrics.record_histogram(
                "session.store.append_event_frame.duration_ms",
                timer.elapsed_ms(),
            );
            result
        })
        .await?
    }

    pub async fn write_metadata_index(
        &self,
        metadata: PersistedSessionMetadata,
        appended_event: Option<SessionEvent>,
    ) -> Result<(), SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.write_metadata_index.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.write_metadata_index(&metadata, appended_event.as_ref());
            store.metrics.record_histogram(
                "session.store.write_metadata_index.duration_ms",
                timer.elapsed_ms(),
            );
            result
        })
        .await?
    }

    pub async fn read_session_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.read_events.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.read_session_events(session_id);
            if let Ok(events) = &result {
                store.metrics.record_histogram(
                    "session.store.read_events.event_count",
                    usize_to_u64(events.len()),
                );
            }
            store
                .metrics
                .record_histogram("session.store.read_events.duration_ms", timer.elapsed_ms());
            result
        })
        .await?
    }

    pub async fn read_session_history_page(
        &self,
        session_id: SessionId,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.history_page.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.read_session_history_page(session_id, query);
            if let Ok(page) = &result {
                store.metrics.record_histogram(
                    "session.store.history_page.event_count",
                    usize_to_u64(page.events.len()),
                );
            }
            store
                .metrics
                .record_histogram("session.store.history_page.duration_ms", timer.elapsed_ms());
            result
        })
        .await?
    }

    pub async fn read_session_events_range(
        &self,
        session_id: SessionId,
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.event_range.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.read_session_events_range(
                session_id,
                start_sequence,
                end_sequence,
                max_events,
            );
            if let Ok(events) = &result {
                store.metrics.record_histogram(
                    "session.store.event_range.result_event_count",
                    usize_to_u64(events.len()),
                );
            }
            store
                .metrics
                .record_histogram("session.store.event_range.duration_ms", timer.elapsed_ms());
            result
        })
        .await?
    }

    pub async fn read_session_input_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionInputHistoryEntry>, SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.store.input_history.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let timer = store.metrics.timer();
            let result = store.read_session_input_history(session_id);
            if let Ok(input_history) = &result {
                store.metrics.record_histogram(
                    "session.store.input_history.entry_count",
                    usize_to_u64(input_history.len()),
                );
            }
            store.metrics.record_histogram(
                "session.store.input_history.duration_ms",
                timer.elapsed_ms(),
            );
            result
        })
        .await?
    }

    pub async fn read_model_context_events(
        &self,
        session_id: SessionId,
    ) -> Result<(Vec<SessionEvent>, Option<SessionState>), SessionStoreError> {
        let queued_at = Instant::now();
        let store = self.store.clone();
        spawn_blocking(move || {
            store.metrics.record_histogram(
                "session.model_context_events.blocking_queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
            let read = store.read_model_context_events(session_id)?;
            Ok((
                read.events,
                read.refreshed_index.map(SessionState::from_index),
            ))
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
