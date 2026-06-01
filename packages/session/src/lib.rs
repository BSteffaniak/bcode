#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Session mutations intentionally hold the manager lock while updating in-memory
// state and appending the corresponding event so summaries/history/fanout stay
// consistent in this first implementation.
#![allow(clippy::significant_drop_tightening)]

//! Session lifecycle, attachment management, and append-only event history.

mod actor;
pub(crate) mod derived;
pub(crate) mod event_migration;
pub(crate) mod index;
pub mod migration;
pub mod projection;
pub(crate) mod reader;
mod store_executor;

pub use index::{SessionIndexHealth, SessionIndexStatus};
pub use migration::{
    SessionEventLogMigration, SessionEventLogMigrationError, SessionMigrationAction,
    SessionMigrationApplyPolicy, SessionMigrationApplyStatus, SessionMigrationBackupPolicy,
    SessionMigrationDefinition, SessionMigrationJournalEntry, SessionMigrationJournalStatus,
    SessionMigrationOptions, SessionMigrationPlan, SessionMigrationPlanItem,
    SessionMigrationRecoveryItem, SessionMigrationRecoveryStatus, SessionMigrationRegistry,
    SessionMigrationRegistryError, SessionMigrationReport, SessionMigrationReportItem,
};

use actor::{AttachMode, SessionHandle};
use bcode_metrics::MetricsRegistry;
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ModelTurnOutcome, ProjectionWindow,
    ProjectionWindowRequest, SessionEvent, SessionEventKind, SessionEventProvenance,
    SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery,
    SessionId, SessionImportSummary, SessionInputHistoryEntry, SessionSummary, SessionTitleSource,
    SessionTokenUsage, SessionTraceEvent, TraceBlobRef,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Seek as _, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use store_executor::{PersistedSessionMetadata, SessionStoreExecutor};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, watch};
use tokio::task::spawn_blocking;

const FRAME_V3_MAGIC: &[u8; 4] = b"BSE3";
const FRAME_V3_VERSION: u16 = 3;

/// Errors returned by session management operations.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(SessionId),
    #[error("session event store error: {0}")]
    Store(#[from] SessionStoreError),
    #[error("session has connected clients: {0}")]
    ConnectedClients(SessionId),
    #[error("session is being deleted: {0}")]
    Deleting(SessionId),
    #[error("unsupported session projection window request")]
    UnsupportedProjectionWindow,
    #[error("session is not writable: {session_id} ({status:?})")]
    NotWritable {
        session_id: SessionId,
        status: SessionAccessStatus,
    },
}

/// Canonical session access status used to gate reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAccessStatus {
    /// Canonical events are readable and writable by this version.
    ReadWrite,
    /// Canonical events are readable, but writes require a migration first.
    ReadOnlyMigrationRequired,
    /// Canonical events were written by a newer unsupported version.
    BlockedFutureVersion,
    /// Canonical events are corrupt and require repair before safe access.
    RepairRequired,
}

impl SessionAccessStatus {
    #[must_use]
    pub const fn writable(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// Errors returned by the append-only session event store.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to encode session event: {0}")]
    Encode(#[source] bmux_codec::Error),
    #[error("failed to decode session event: {0}")]
    Decode(#[source] bmux_codec::Error),
    #[error("blocking session store task failed: {0}")]
    BlockingTask(#[from] tokio::task::JoinError),
    #[error("session catalog load failed: {0}")]
    CatalogLoad(String),
    #[error("session index error: {0}")]
    Index(#[source] serde_json::Error),
    #[error("session event frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("unsupported session event frame version: {0}")]
    UnsupportedFrameVersion(u16),
    #[error("session event frame checksum mismatch")]
    ChecksumMismatch,
    #[error("session event file has a non-UTF-8 or missing file stem: {0:?}")]
    InvalidFileName(PathBuf),
    #[error("session event file name is not a session ID: {0}")]
    InvalidSessionId(String),
    #[error(
        "refusing to write stale session metadata for {session_id}: current event_count={current_event_count}, attempted event_count={attempted_event_count}, current next_sequence={current_next_sequence}, attempted next_sequence={attempted_next_sequence}"
    )]
    StaleMetadataWrite {
        session_id: SessionId,
        current_event_count: usize,
        attempted_event_count: usize,
        current_next_sequence: u64,
        attempted_next_sequence: u64,
    },
    #[error("session migration registry error: {0}")]
    MigrationRegistry(#[from] SessionMigrationRegistryError),
}

/// Append-only event store for session histories.
#[derive(Debug, Clone)]
pub struct SessionEventStore {
    root: PathBuf,
    pub(crate) metrics: MetricsRegistry,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionMigrationBackupManifest {
    created_at_ms: u64,
    domain: &'static str,
    files: Vec<SessionMigrationBackupFile>,
}

pub(crate) struct ModelContextEventsRead {
    events: Vec<SessionEvent>,
    refreshed_index: Option<index::SessionIndex>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionMigrationBackupFile {
    session_id: SessionId,
    source: String,
    backup: String,
}

fn write_event_frame(file: &mut fs::File, event: &SessionEvent) -> Result<u64, SessionStoreError> {
    let payload = bmux_codec::to_vec(event).map_err(SessionStoreError::Encode)?;
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| SessionStoreError::FrameTooLarge(payload.len()))?;
    let checksum = Sha256::digest(&payload);
    file.write_all(FRAME_V3_MAGIC)?;
    file.write_all(&FRAME_V3_VERSION.to_le_bytes())?;
    file.write_all(&CURRENT_SESSION_EVENT_SCHEMA_VERSION.to_le_bytes())?;
    file.write_all(&payload_len.to_le_bytes())?;
    file.write_all(&checksum)?;
    file.write_all(&payload)?;
    Ok(u64::from(payload_len).saturating_add(44))
}

impl SessionEventStore {
    /// Create an event store rooted at the provided directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            metrics: MetricsRegistry::default(),
        }
    }

    /// Create an event store rooted at the provided directory with metrics instrumentation.
    #[must_use]
    pub fn with_metrics(root: impl Into<PathBuf>, metrics: MetricsRegistry) -> Self {
        Self {
            root: root.into(),
            metrics,
        }
    }

    fn load_catalog(&self) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let mut sessions = BTreeMap::new();
        if !self.root.exists() {
            return Ok(sessions);
        }

        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = parse_session_file_name(&path)?;
            if let Some(state) = self.load_session_metadata(session_id, &path)? {
                sessions.insert(session_id, state);
            }
        }

        Ok(sessions)
    }

    fn load_session_metadata(
        &self,
        session_id: SessionId,
        path: &Path,
    ) -> Result<Option<SessionState>, SessionStoreError> {
        if let Some(index) = index::load_fresh_index(&self.root, session_id, path)? {
            return Ok(Some(index.into_state()));
        }
        let event_path = self.event_path(session_id);
        if let Ok(index) = self.ensure_fresh_index(session_id) {
            return Ok(Some(index.into_state()));
        }
        let Ok(entries) = index::read_entries(&self.root, session_id) else {
            return self.synthesize_initial_index_from_first_event(session_id, path);
        };
        let Some(first_entry) = entries.first() else {
            return Ok(None);
        };
        if first_entry.sequence != 0 || first_entry.kind != "session_created" {
            return Ok(None);
        }
        let first_event = reader::read_event_at(path, first_entry.offset)?;
        let file = index::fingerprint(&event_path)?;
        Ok(index::catalog_state_from_first_entry(
            session_id,
            &file,
            &entries,
            &first_event,
        ))
    }

    fn synthesize_initial_index_from_first_event(
        &self,
        session_id: SessionId,
        path: &Path,
    ) -> Result<Option<SessionState>, SessionStoreError> {
        let first_event = reader::read_event_at(path, 0)?;
        let event_path = self.event_path(session_id);
        let file = index::fingerprint(&event_path)?;
        let entry = index::SessionIndexEntry::from_event(&first_event, 0, file.len);
        if first_event.sequence != 0 || entry.kind != "session_created" {
            return Ok(None);
        }
        index::write_entries(&self.root, session_id, std::slice::from_ref(&entry))?;
        let Some(mut state) = index::catalog_state_from_first_entry(
            session_id,
            &file,
            std::slice::from_ref(&entry),
            &first_event,
        ) else {
            return Ok(None);
        };
        state.access_status = access_status_from_schema_versions(
            Some(first_event.schema_version),
            Some(first_event.schema_version),
            false,
        );
        index::write_index(
            &self.root,
            &index::SessionIndex {
                index_version: index::SESSION_INDEX_VERSION,
                session_id,
                file: file.clone(),
                summary: state.summary(),
                working_directory: state.working_directory.clone(),
                next_sequence: state.next_sequence,
                event_count: state.event_count,
                created_at_ms: state.summary.created_at_ms,
                updated_at_ms: state.summary.updated_at_ms,
                has_user_message: state.has_user_message,
                last_good_offset: file.len,
                current_provider: state.current_provider.clone(),
                current_model: state.current_model.clone(),
                current_agent: state.current_agent.clone(),
                latest_compaction_sequence: state.latest_compaction_sequence,
                total_metered_tokens: state.total_metered_tokens,
                min_event_schema_version: Some(first_event.schema_version),
                max_event_schema_version: Some(first_event.schema_version),
                issues: Vec::new(),
            },
        )?;
        Ok(Some(state))
    }

    fn load_sessions(&self) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let mut sessions = self.load_catalog()?;
        if !self.root.exists() {
            return Ok(sessions);
        }

        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = parse_session_file_name(&path)?;
            if sessions.contains_key(&session_id) {
                continue;
            }
            if let Some(state) = self.load_session(session_id)? {
                sessions.insert(session_id, state);
            }
        }

        Ok(sessions)
    }

    fn load_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionState>, SessionStoreError> {
        let path = self.event_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        self.load_session_metadata(session_id, &path)
    }

    fn append(&self, event: &SessionEvent) -> Result<index::SessionIndexEntry, SessionStoreError> {
        let append_timer = self.metrics.timer();
        fs::create_dir_all(&self.root)?;
        let path = self.event_path(event.session_id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let offset = file.seek(SeekFrom::End(0))?;
        let write_timer = self.metrics.timer();
        let frame_len = write_event_frame(&mut file, event)?;
        file.flush()?;
        self.metrics.record_histogram(
            "session.event_log.write_flush_duration_ms",
            write_timer.elapsed_ms(),
        );
        self.metrics
            .record_histogram("session.event_log.frame_bytes", frame_len);
        self.metrics
            .increment_counter("session.event_log.append_total");
        let entry = index::SessionIndexEntry::from_event(event, offset, frame_len);
        let index_timer = self.metrics.timer();
        if let Err(error) = index::append_entry(&self.root, event.session_id, &entry) {
            self.metrics
                .increment_counter("session.event_index.append_error_total");
            eprintln!(
                "failed to update session entry index for {}: {error}",
                event.session_id
            );
        }
        self.metrics.record_histogram(
            "session.event_index.append_duration_ms",
            index_timer.elapsed_ms(),
        );
        if event.sequence == 0 {
            let file = index::fingerprint(&path)?;
            let index = index::SessionIndex {
                index_version: index::SESSION_INDEX_VERSION,
                session_id: event.session_id,
                file,
                summary: SessionSummary {
                    id: event.session_id,
                    name: None,
                    explicit_name: None,
                    derived_title: None,
                    title_source: SessionTitleSource::EmptyDraft,
                    client_count: 0,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    working_directory: PathBuf::new(),
                    import: None,
                },
                working_directory: PathBuf::new(),
                next_sequence: 1,
                event_count: 1,
                created_at_ms: 0,
                updated_at_ms: 0,
                has_user_message: false,
                last_good_offset: offset.saturating_add(frame_len),
                current_provider: None,
                current_model: None,
                current_agent: None,
                latest_compaction_sequence: None,
                total_metered_tokens: 0,
                min_event_schema_version: Some(event.schema_version),
                max_event_schema_version: Some(event.schema_version),
                issues: Vec::new(),
            };
            let _ = derived::initialize_empty_after_session_created(&self.root, &index);
        }
        self.metrics.record_histogram(
            "session.event_log.append_duration_ms",
            append_timer.elapsed_ms(),
        );
        Ok(entry)
    }

    fn read_session_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let path = self.event_path(session_id);
        Ok(reader::read_events(&path)?.events)
    }

    fn read_session_events_range(
        &self,
        session_id: SessionId,
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let total_timer = self.metrics.timer();
        let event_path = self.event_path(session_id);
        let index_timer = self.metrics.timer();
        let index = self.ensure_fresh_index(session_id)?;
        self.metrics.record_histogram(
            "session.store.event_range.ensure_fresh_index_duration_ms",
            index_timer.elapsed_ms(),
        );
        let entries_timer = self.metrics.timer();
        let entries = match index::read_entries(&self.root, session_id) {
            Ok(entries) if entries.len() == index.event_count => entries,
            _ => {
                self.metrics
                    .increment_counter("session.store.event_range.index_unavailable_total");
                return Err(SessionStoreError::InvalidSessionId(
                    "event range requires repaired primary entry index".to_owned(),
                ));
            }
        };
        self.metrics.record_histogram(
            "session.store.event_range.read_entries_duration_ms",
            entries_timer.elapsed_ms(),
        );
        let select_timer = self.metrics.timer();
        let selected_entries =
            select_event_range_entries(entries, start_sequence, end_sequence, max_events);
        self.metrics.record_histogram(
            "session.store.event_range.select_entries_duration_ms",
            select_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.store.event_range.selected_entry_count",
            usize_to_u64(selected_entries.len()),
        );
        let events_timer = self.metrics.timer();
        let events = read_indexed_events(&event_path, &selected_entries)?;
        self.metrics.record_histogram(
            "session.store.event_range.read_indexed_events_duration_ms",
            events_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.store.event_range.checkpoint.event_count",
            usize_to_u64(events.len()),
        );
        self.metrics.record_histogram(
            "session.store.event_range.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        Ok(events)
    }

    fn ensure_fresh_index(
        &self,
        session_id: SessionId,
    ) -> Result<index::SessionIndex, SessionStoreError> {
        let event_path = self.event_path(session_id);
        if let Some(index) = index::load_fresh_index(&self.root, session_id, &event_path)? {
            return Ok(index);
        }
        self.catch_up_metadata_index_from_entries(session_id)
    }

    fn catch_up_metadata_index_from_entries(
        &self,
        session_id: SessionId,
    ) -> Result<index::SessionIndex, SessionStoreError> {
        let event_path = self.event_path(session_id);
        let mut index = index::load_index_metadata(&self.root, session_id)?.ok_or_else(|| {
            SessionStoreError::InvalidSessionId(
                "session metadata index is missing; run session repair".to_owned(),
            )
        })?;
        let entries = index::read_entries(&self.root, session_id).map_err(|_error| {
            SessionStoreError::InvalidSessionId(
                "session entry index is missing; run session repair".to_owned(),
            )
        })?;
        if entries.len() < index.event_count {
            return Err(SessionStoreError::InvalidSessionId(
                "session entry index is behind metadata; run session repair".to_owned(),
            ));
        }
        let append_entries = entries
            .iter()
            .skip(index.event_count)
            .cloned()
            .collect::<Vec<_>>();
        if append_entries.is_empty() {
            return Err(SessionStoreError::InvalidSessionId(
                "session metadata index is stale but has no entry-index tail; run session repair"
                    .to_owned(),
            ));
        }
        let tail_events = read_indexed_events(&event_path, &append_entries)?;
        let mut state = SessionState::from_index(index.clone());
        for event in tail_events {
            if event.sequence != state.next_sequence {
                return Err(SessionStoreError::InvalidSessionId(
                    "session entry index has non-contiguous metadata tail; run session repair"
                        .to_owned(),
                ));
            }
            let activity_timestamp_ms = index.file.modified_at_ms();
            state.apply_persisted_event(event, activity_timestamp_ms);
        }
        let file = index::fingerprint(&event_path)?;
        index = index::SessionIndex {
            index_version: index::SESSION_INDEX_VERSION,
            session_id,
            file: file.clone(),
            summary: state.summary(),
            working_directory: state.working_directory,
            next_sequence: state.next_sequence,
            event_count: state.event_count,
            created_at_ms: state.summary.created_at_ms,
            updated_at_ms: file.modified_at_ms(),
            has_user_message: state.has_user_message,
            last_good_offset: append_entries
                .last()
                .map_or(index.last_good_offset, |entry| {
                    entry.offset.saturating_add(entry.frame_len)
                }),
            current_provider: state.current_provider,
            current_model: state.current_model,
            current_agent: state.current_agent,
            latest_compaction_sequence: state.latest_compaction_sequence,
            total_metered_tokens: state.total_metered_tokens,
            min_event_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            max_event_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            issues: state.index_issues,
        };
        index::write_index(&self.root, &index)?;
        Ok(index)
    }

    fn read_session_history_page(
        &self,
        session_id: SessionId,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionStoreError> {
        let total_timer = self.metrics.timer();
        let event_path = self.event_path(session_id);
        let index_timer = self.metrics.timer();
        let index = self.ensure_fresh_index(session_id)?;
        self.metrics.record_histogram(
            "session.store.history_page.ensure_fresh_index_duration_ms",
            index_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.store.history_page.index_event_count",
            usize_to_u64(index.event_count),
        );
        let entries_timer = self.metrics.timer();
        let entries = match index::read_entries(&self.root, session_id) {
            Ok(entries) if entries.len() == index.event_count => entries,
            _ => {
                self.metrics
                    .increment_counter("session.store.history_page.index_unavailable_total");
                return Err(SessionStoreError::InvalidSessionId(
                    "history page requires repaired primary entry index".to_owned(),
                ));
            }
        };
        self.metrics.record_histogram(
            "session.store.history_page.read_entries_duration_ms",
            entries_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.store.history_page.read_entry_count",
            usize_to_u64(entries.len()),
        );
        let limit = query.limit.max(1);
        let select_timer = self.metrics.timer();
        let (page_entries, has_more) = select_history_page_entries(entries, query, limit);
        self.metrics.record_histogram(
            "session.store.history_page.select_entries_duration_ms",
            select_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.store.history_page.selected_entry_count",
            usize_to_u64(page_entries.len()),
        );
        let events_timer = self.metrics.timer();
        let events = read_indexed_events(&event_path, &page_entries);
        self.metrics.record_histogram(
            "session.store.history_page.read_indexed_events_duration_ms",
            events_timer.elapsed_ms(),
        );
        if events.is_err() {
            self.metrics
                .increment_counter("session.store.history_page.read_indexed_events_error_total");
        }
        let events = events?;
        self.metrics.record_histogram(
            "session.store.history_page.result_event_count",
            usize_to_u64(events.len()),
        );
        let next_cursor = if has_more {
            events.last().map(|event| SessionHistoryCursor {
                sequence: match query.direction {
                    SessionHistoryDirection::Forward => event.sequence.saturating_add(1),
                    SessionHistoryDirection::Backward => event.sequence.saturating_sub(1),
                },
            })
        } else {
            None
        };
        self.metrics.record_histogram(
            "session.store.history_page.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        Ok(SessionHistoryPage {
            session_id,
            events,
            next_cursor,
            has_more,
        })
    }

    fn read_session_input_history(&self, session_id: SessionId) -> Vec<SessionInputHistoryEntry> {
        let total_timer = self.metrics.timer();
        let event_path = self.event_path(session_id);
        let index_timer = self.metrics.timer();
        let input_index_result =
            derived::ensure_input_history_index(&self.root, session_id, &event_path);
        let input_index = match input_index_result {
            Ok(index) => index,
            Err(_error) => {
                self.metrics
                    .increment_counter("session.store.input_history.ensure_error_total");
                self.metrics.record_histogram(
                    "session.store.input_history.load_index_duration_ms",
                    index_timer.elapsed_ms(),
                );
                self.metrics.record_histogram(
                    "session.store.input_history.total_duration_ms",
                    total_timer.elapsed_ms(),
                );
                return Vec::new();
            }
        };
        self.metrics.record_histogram(
            "session.store.input_history.load_index_duration_ms",
            index_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.store.input_history.index_event_count",
            usize_to_u64(input_index.event_count),
        );
        self.metrics.record_histogram(
            "session.store.input_history.entry_count",
            usize_to_u64(input_index.entries.len()),
        );
        self.metrics.record_histogram(
            "session.store.input_history.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        input_index.entries
    }

    #[allow(clippy::too_many_lines)]
    fn read_model_context_events(
        &self,
        session_id: SessionId,
    ) -> Result<ModelContextEventsRead, SessionStoreError> {
        let total_timer = self.metrics.timer();
        let event_path = self.event_path(session_id);
        let index_timer = self.metrics.timer();
        let index = self.ensure_fresh_index(session_id)?;
        self.metrics.record_histogram(
            "session.model_context_events.load_index_duration_ms",
            index_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.model_context_events.index_event_count",
            index.event_count as u64,
        );
        let read_entries_timer = self.metrics.timer();
        let mut entries = match index::read_entries(&self.root, session_id) {
            Ok(entries) if entries.len() == index.event_count => entries,
            _ => {
                self.metrics
                    .increment_counter("session.model_context_events.index_unavailable_total");
                return Err(SessionStoreError::InvalidSessionId(
                    "model context requires repaired primary entry index".to_owned(),
                ));
            }
        };
        self.metrics.record_histogram(
            "session.model_context_events.read_entries_duration_ms",
            read_entries_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.model_context_events.entry_count",
            entries.len() as u64,
        );
        let sort_timer = self.metrics.timer();
        entries.sort_by_key(|entry| entry.sequence);
        self.metrics.record_histogram(
            "session.model_context_events.sort_entries_duration_ms",
            sort_timer.elapsed_ms(),
        );
        let find_compaction_timer = self.metrics.timer();
        let compaction_entry = entries
            .iter()
            .rev()
            .find(|entry| entry.kind == "context_compacted");
        self.metrics.record_histogram(
            "session.model_context_events.find_compaction_duration_ms",
            find_compaction_timer.elapsed_ms(),
        );
        let Some(compaction_entry) = compaction_entry else {
            self.metrics
                .increment_counter("session.model_context_events.no_compaction_total");
            let fallback_timer = self.metrics.timer();
            let events = self.read_session_events(session_id)?;
            self.metrics.record_histogram(
                "session.model_context_events.read_full_history_duration_ms",
                fallback_timer.elapsed_ms(),
            );
            self.metrics.record_histogram(
                "session.model_context_events.result_event_count",
                events.len() as u64,
            );
            self.metrics.record_histogram(
                "session.model_context_events.total_duration_ms",
                total_timer.elapsed_ms(),
            );
            return Ok(ModelContextEventsRead {
                events,
                refreshed_index: Some(index),
            });
        };
        let compaction_read_timer = self.metrics.timer();
        let compaction_event = reader::read_event_at(&event_path, compaction_entry.offset)?;
        self.metrics.record_histogram(
            "session.model_context_events.read_compaction_event_duration_ms",
            compaction_read_timer.elapsed_ms(),
        );
        let SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        } = &compaction_event.kind
        else {
            self.metrics
                .increment_counter("session.model_context_events.invalid_compaction_entry_total");
            let fallback_timer = self.metrics.timer();
            let events = self.read_session_events(session_id)?;
            self.metrics.record_histogram(
                "session.model_context_events.read_full_history_duration_ms",
                fallback_timer.elapsed_ms(),
            );
            self.metrics.record_histogram(
                "session.model_context_events.result_event_count",
                events.len() as u64,
            );
            self.metrics.record_histogram(
                "session.model_context_events.total_duration_ms",
                total_timer.elapsed_ms(),
            );
            return Ok(ModelContextEventsRead {
                events,
                refreshed_index: Some(index),
            });
        };
        let compacted_through_sequence = *compacted_through_sequence;
        let select_timer = self.metrics.timer();
        let selected_entries = entries
            .iter()
            .filter(|entry| entry.sequence > compacted_through_sequence)
            .filter(|entry| entry.sequence != compaction_entry.sequence)
            .collect::<Vec<_>>();
        self.metrics.record_histogram(
            "session.model_context_events.select_entries_duration_ms",
            select_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.model_context_events.selected_entry_count",
            selected_entries.len() as u64,
        );
        let read_selected_timer = self.metrics.timer();
        let mut events = Vec::with_capacity(selected_entries.len().saturating_add(1));
        events.push(compaction_event);
        for entry in selected_entries {
            events.push(reader::read_event_at(&event_path, entry.offset)?);
        }
        self.metrics.record_histogram(
            "session.model_context_events.read_selected_events_duration_ms",
            read_selected_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.model_context_events.result_event_count",
            events.len() as u64,
        );
        self.metrics.record_histogram(
            "session.model_context_events.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        Ok(ModelContextEventsRead {
            events,
            refreshed_index: None,
        })
    }

    fn write_metadata_index(
        &self,
        metadata: &PersistedSessionMetadata,
    ) -> Result<(), SessionStoreError> {
        let path = self.event_path(metadata.summary.id);
        let existing_index = index::load_fresh_index(&self.root, metadata.summary.id, &path)?;
        if let Some(existing_index) = &existing_index
            && (metadata.event_count < existing_index.event_count
                || metadata.next_sequence < existing_index.next_sequence)
        {
            self.metrics
                .increment_counter("session.metadata_index.stale_write_total");
            return Err(SessionStoreError::StaleMetadataWrite {
                session_id: metadata.summary.id,
                current_event_count: existing_index.event_count,
                attempted_event_count: metadata.event_count,
                current_next_sequence: existing_index.next_sequence,
                attempted_next_sequence: metadata.next_sequence,
            });
        }
        let file = index::fingerprint(&path)?;
        let index = index::SessionIndex {
            index_version: index::SESSION_INDEX_VERSION,
            session_id: metadata.summary.id,
            last_good_offset: file.len,
            file,
            summary: SessionSummary {
                client_count: 0,
                ..metadata.summary.clone()
            },
            working_directory: metadata.working_directory.clone(),
            next_sequence: metadata.next_sequence,
            event_count: metadata.event_count,
            created_at_ms: metadata.summary.created_at_ms,
            updated_at_ms: metadata.summary.updated_at_ms,
            has_user_message: metadata.has_user_message,
            current_provider: metadata.current_provider.clone(),
            current_model: metadata.current_model.clone(),
            current_agent: metadata.current_agent.clone(),
            latest_compaction_sequence: metadata.latest_compaction_sequence,
            total_metered_tokens: metadata.total_metered_tokens,
            min_event_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            max_event_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            issues: metadata.index_issues.clone(),
        };
        let timer = self.metrics.timer();
        let result = index::write_index(&self.root, &index);
        self.metrics.record_histogram(
            "session.metadata_index.write_duration_ms",
            timer.elapsed_ms(),
        );
        if result.is_err() {
            self.metrics
                .increment_counter("session.metadata_index.write_error_total");
        }
        result
    }

    /// Repair a session log by backing up and truncating an unreadable tail.
    ///
    /// # Errors
    ///
    /// Returns an error if the event file cannot be read, backed up, truncated, or reindexed.
    pub fn repair_session_tail(
        &self,
        session_id: SessionId,
    ) -> Result<Option<PathBuf>, SessionStoreError> {
        let path = self.event_path(session_id);
        let report = reader::read_events(&path)?;
        let file_len = fs::metadata(&path)?.len();
        if report.last_good_offset >= file_len {
            self.reindex_session(session_id)?;
            return Ok(None);
        }
        let backup = corrupt_backup_path(&path);
        fs::copy(&path, &backup)?;
        let file = OpenOptions::new().write(true).open(&path)?;
        file.set_len(report.last_good_offset)?;
        self.reindex_session(session_id)?;
        Ok(Some(backup))
    }

    /// Restore a session's canonical event log from a migration backup and rebuild its index.
    ///
    /// # Errors
    ///
    /// Returns an error if the backup, event file, or rebuilt index cannot be read or written.
    pub fn restore_session_from_backup(
        &self,
        session_id: SessionId,
        backup_path: &Path,
    ) -> Result<PathBuf, SessionStoreError> {
        let path = self.event_path(session_id);
        if !backup_path.exists() {
            return Err(SessionStoreError::InvalidSessionId(format!(
                "backup does not exist: {}",
                backup_path.display()
            )));
        }
        let restore_backup = corrupt_backup_path(&path);
        if path.exists() {
            fs::copy(&path, &restore_backup)?;
        }
        fs::copy(backup_path, &path)?;
        self.reindex_session(session_id)?;
        Ok(restore_backup)
    }

    /// Rebuild the sidecar index for one session from its canonical event log.
    ///
    /// # Errors
    ///
    /// Returns an error if the event file cannot be read or the index cannot be written.
    pub fn reindex_session(&self, session_id: SessionId) -> Result<(), SessionStoreError> {
        let path = self.event_path(session_id);
        let _ = index::rebuild_index(&self.root, session_id, &path)?;
        Ok(())
    }

    /// Rebuild every session sidecar index under this store.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory cannot be scanned or any index cannot be rebuilt.
    pub fn reindex_all(&self) -> Result<Vec<SessionId>, SessionStoreError> {
        let mut rebuilt = Vec::new();
        if !self.root.exists() {
            return Ok(rebuilt);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = parse_session_file_name(&path)?;
            self.reindex_session(session_id)?;
            rebuilt.push(session_id);
        }
        Ok(rebuilt)
    }

    /// Return index health for one persisted session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session event file or index cannot be read.
    pub fn doctor_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionIndexHealth>, SessionStoreError> {
        self.doctor_session_with_fix(session_id, false)
    }

    /// Return index health for one persisted session, optionally rebuilding stale indexes.
    ///
    /// # Errors
    ///
    /// Returns an error if the session event file or index cannot be read.
    pub fn doctor_session_with_fix(
        &self,
        session_id: SessionId,
        fix: bool,
    ) -> Result<Option<SessionIndexHealth>, SessionStoreError> {
        self.doctor_session_with_options(session_id, fix, false)
    }

    /// Return index health for one persisted session with explicit rebuild behavior.
    ///
    /// When `force` is true the index is rebuilt even if the existing index is
    /// fresh. This lets metadata-only derivation changes (for example legacy
    /// title recovery) be applied without requiring an event-file change.
    ///
    /// # Errors
    ///
    /// Returns an error if the session event file or index cannot be read.
    pub fn doctor_session_with_options(
        &self,
        session_id: SessionId,
        fix: bool,
        force: bool,
    ) -> Result<Option<SessionIndexHealth>, SessionStoreError> {
        let path = self.event_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        if fix && force {
            let (index, _) = index::rebuild_index(&self.root, session_id, &path)?;
            let derived = derived::health_all(&self.root, session_id, &path, fix, force)?;
            return Ok(index.map(|index| index.health(true, derived)));
        }
        if let Some(index) = index::load_fresh_index(&self.root, session_id, &path)? {
            let derived = derived::health_all(&self.root, session_id, &path, fix, force)?;
            return Ok(Some(index.health(false, derived)));
        }
        if fix {
            let (index, _) = index::rebuild_index(&self.root, session_id, &path)?;
            let derived = derived::health_all(&self.root, session_id, &path, fix, force)?;
            return Ok(index.map(|index| index.health(true, derived)));
        }
        let derived = derived::health_all(&self.root, session_id, &path, fix, force)?;
        Ok(
            index::rebuild_index_metadata(&self.root, session_id, &path)?
                .map(|index| index.health(true, derived)),
        )
    }

    /// Return index health for every persisted session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory or an index file cannot be read.
    pub fn doctor_all(&self) -> Result<Vec<SessionIndexHealth>, SessionStoreError> {
        self.doctor_all_with_fix(false)
    }

    /// Return index health for every persisted session, optionally rebuilding stale indexes.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory or an index file cannot be read.
    pub fn doctor_all_with_fix(
        &self,
        fix: bool,
    ) -> Result<Vec<SessionIndexHealth>, SessionStoreError> {
        self.doctor_all_with_options(fix, false)
    }

    /// Return index health for every persisted session with explicit rebuild behavior.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory or an index file cannot be read.
    pub fn doctor_all_with_options(
        &self,
        fix: bool,
        force: bool,
    ) -> Result<Vec<SessionIndexHealth>, SessionStoreError> {
        let mut health = Vec::new();
        if !self.root.exists() {
            return Ok(health);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = parse_session_file_name(&path)?;
            if let Some(item) = self.doctor_session_with_options(session_id, fix, force)? {
                health.push(item);
            }
        }
        Ok(health)
    }

    /// Return recovery status from the session migration journal.
    ///
    /// # Errors
    ///
    /// Returns an error if the migration journal cannot be read or decoded.
    pub fn migration_recovery_status(
        &self,
    ) -> Result<SessionMigrationRecoveryStatus, SessionStoreError> {
        let entries = migration::read_journal_entries(&self.root)?;
        migration::recovery_status(&self.root, &entries)
    }

    /// Plan safe session persistence migrations without applying them.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory or event metadata cannot be read.
    pub fn migration_plan(&self) -> Result<SessionMigrationPlan, SessionStoreError> {
        let registry = SessionMigrationRegistry::builtin();
        registry.validate()?;
        let index_rebuild =
            registry.required_migration_for_action(SessionMigrationAction::RebuildDerivedIndex)?;
        let mut items = Vec::new();
        if !self.root.exists() {
            return Ok(SessionMigrationPlan {
                domain: "sessions/index",
                items,
            });
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = parse_session_file_name(&path)?;
            match index::inspect_index(&self.root, session_id, &path)? {
                index::SessionIndexStatus::Current(_) => {}
                index::SessionIndexStatus::Missing { current_version } => {
                    items.push(SessionMigrationPlanItem {
                        migration_id: index_rebuild.id,
                        session_id,
                        current_version,
                        found_version: None,
                        action: index_rebuild.action,
                        reason: "index is missing".to_string(),
                        automatic: index_rebuild.automatic(),
                        backup_policy: index_rebuild.backup_policy,
                    });
                }
                index::SessionIndexStatus::Stale {
                    found_version,
                    current_version,
                    reason,
                } => {
                    items.push(SessionMigrationPlanItem {
                        migration_id: index_rebuild.id,
                        session_id,
                        current_version,
                        found_version,
                        action: index_rebuild.action,
                        reason,
                        automatic: index_rebuild.automatic(),
                        backup_policy: index_rebuild.backup_policy,
                    });
                }
                index::SessionIndexStatus::Corrupt {
                    current_version,
                    reason,
                } => {
                    items.push(SessionMigrationPlanItem {
                        migration_id: index_rebuild.id,
                        session_id,
                        current_version,
                        found_version: None,
                        action: index_rebuild.action,
                        reason: format!("index is corrupt: {reason}"),
                        automatic: index_rebuild.automatic(),
                        backup_policy: index_rebuild.backup_policy,
                    });
                }
                index::SessionIndexStatus::Future {
                    found_version,
                    current_version,
                } => {
                    items.push(SessionMigrationPlanItem {
                        migration_id: index_rebuild.id,
                        session_id,
                        current_version,
                        found_version: Some(found_version),
                        action: SessionMigrationAction::None,
                        reason: "index was written by a newer Bcode version".to_string(),
                        automatic: false,
                        backup_policy: SessionMigrationBackupPolicy::NotRequired,
                    });
                }
            }
        }
        Ok(SessionMigrationPlan {
            domain: "sessions/index",
            items,
        })
    }

    /// Apply safe session persistence migrations.
    ///
    /// Derived index migrations are rebuilt from canonical event logs. When
    /// `backup` is set, canonical event logs are copied to a migration backup
    /// directory before any derived files are rewritten.
    ///
    /// # Errors
    ///
    /// Returns an error if migration planning, backup creation, or index rebuilds fail.
    pub fn apply_migration_plan(
        &self,
        options: SessionMigrationOptions,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let started_at_ms = current_unix_millis();
        let run_id = format!("session-migration-{started_at_ms}");
        let plan = self.migration_plan()?;
        if plan.items.is_empty() {
            return Ok(SessionMigrationReport {
                domain: plan.domain,
                dry_run: options.dry_run,
                backup_dir: None,
                items: Vec::new(),
            });
        }
        let migration_ids: Vec<_> = plan
            .items
            .iter()
            .map(|item| item.migration_id.to_string())
            .collect();
        let session_ids: Vec<_> = plan.items.iter().map(|item| item.session_id).collect();
        migration::append_journal_entry(
            &self.root,
            &SessionMigrationJournalEntry {
                run_id: run_id.clone(),
                domain: plan.domain.to_string(),
                status: SessionMigrationJournalStatus::Started,
                dry_run: options.dry_run,
                backup: options.backup,
                backup_dir: None,
                started_at_ms,
                finished_at_ms: None,
                migration_ids: migration_ids.clone(),
                session_ids: session_ids.clone(),
                error: None,
            },
        )?;

        let result = self.apply_migration_plan_inner(&plan, options);
        let finished_at_ms = current_unix_millis();
        match &result {
            Ok(report) => {
                migration::append_journal_entry(
                    &self.root,
                    &SessionMigrationJournalEntry {
                        run_id,
                        domain: plan.domain.to_string(),
                        status: SessionMigrationJournalStatus::Completed,
                        dry_run: options.dry_run,
                        backup: options.backup,
                        backup_dir: report
                            .backup_dir
                            .as_ref()
                            .map(|path| path.display().to_string()),
                        started_at_ms,
                        finished_at_ms: Some(finished_at_ms),
                        migration_ids,
                        session_ids,
                        error: None,
                    },
                )?;
            }
            Err(error) => {
                let _ = migration::append_journal_entry(
                    &self.root,
                    &SessionMigrationJournalEntry {
                        run_id,
                        domain: plan.domain.to_string(),
                        status: SessionMigrationJournalStatus::Failed,
                        dry_run: options.dry_run,
                        backup: options.backup,
                        backup_dir: None,
                        started_at_ms,
                        finished_at_ms: Some(finished_at_ms),
                        migration_ids,
                        session_ids,
                        error: Some(error.to_string()),
                    },
                );
            }
        }
        result
    }

    fn apply_migration_plan_inner(
        &self,
        plan: &SessionMigrationPlan,
        options: SessionMigrationOptions,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let backup_dir = if options.backup && !options.dry_run && !plan.items.is_empty() {
            Some(self.backup_canonical_events(&plan.items)?)
        } else {
            None
        };
        let mut items = Vec::new();
        for item in &plan.items {
            match item.action {
                SessionMigrationAction::None => {
                    items.push(SessionMigrationReportItem {
                        migration_id: item.migration_id,
                        session_id: item.session_id,
                        action: item.action,
                        status: SessionMigrationApplyStatus::Skipped,
                        message: item.reason.clone(),
                    });
                }
                SessionMigrationAction::RewriteCanonicalEvents => {
                    items.push(SessionMigrationReportItem {
                        migration_id: item.migration_id,
                        session_id: item.session_id,
                        action: item.action,
                        status: SessionMigrationApplyStatus::Skipped,
                        message: "canonical event migrations require the typed executor"
                            .to_string(),
                    });
                }
                SessionMigrationAction::RebuildDerivedIndex => {
                    if options.dry_run {
                        items.push(SessionMigrationReportItem {
                            migration_id: item.migration_id,
                            session_id: item.session_id,
                            action: item.action,
                            status: SessionMigrationApplyStatus::Planned,
                            message: item.reason.clone(),
                        });
                    } else {
                        self.reindex_session(item.session_id)?;
                        items.push(SessionMigrationReportItem {
                            migration_id: item.migration_id,
                            session_id: item.session_id,
                            action: item.action,
                            status: SessionMigrationApplyStatus::Applied,
                            message: item.reason.clone(),
                        });
                    }
                }
            }
        }
        Ok(SessionMigrationReport {
            domain: plan.domain,
            dry_run: options.dry_run,
            backup_dir,
            items,
        })
    }

    pub(crate) fn backup_canonical_events(
        &self,
        items: &[SessionMigrationPlanItem],
    ) -> Result<PathBuf, SessionStoreError> {
        let backup_dir = self
            .root
            .join("backups")
            .join(format!("migration-{}", current_unix_millis()));
        fs::create_dir_all(&backup_dir)?;
        let mut manifest = SessionMigrationBackupManifest {
            created_at_ms: current_unix_millis(),
            domain: "sessions/events",
            files: Vec::new(),
        };
        for item in items {
            if item.action == SessionMigrationAction::None {
                continue;
            }
            let source = self.event_path(item.session_id);
            if !source.exists() {
                continue;
            }
            let file_name = format!("{}.events", item.session_id);
            let destination = backup_dir.join(&file_name);
            fs::copy(&source, &destination)?;
            manifest.files.push(SessionMigrationBackupFile {
                session_id: item.session_id,
                source: source.display().to_string(),
                backup: file_name,
            });
        }
        let manifest_path = backup_dir.join("manifest.json");
        let tmp_path = backup_dir.join("manifest.json.tmp");
        let contents = serde_json::to_vec_pretty(&manifest).map_err(SessionStoreError::Index)?;
        fs::write(&tmp_path, contents)?;
        fs::rename(tmp_path, manifest_path)?;
        Ok(backup_dir)
    }

    fn delete(&self, session_id: SessionId) -> Result<(), SessionStoreError> {
        let path = self.event_path(session_id);
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(SessionStoreError::Io(error)),
        }
        match fs::remove_file(index::index_path(&self.root, session_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(SessionStoreError::Io(error)),
        }
        match fs::remove_file(index::entries_path(&self.root, session_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SessionStoreError::Io(error)),
        }
    }

    #[must_use]
    pub fn event_path(&self, session_id: SessionId) -> PathBuf {
        self.root.join(format!("{session_id}.events"))
    }

    pub(crate) fn root(&self) -> &Path {
        self.root.as_path()
    }
}

fn select_history_page_entries(
    mut entries: Vec<index::SessionIndexEntry>,
    query: SessionHistoryQuery,
    limit: usize,
) -> (Vec<index::SessionIndexEntry>, bool) {
    entries.sort_by_key(|entry| entry.sequence);
    let selected = match query.direction {
        SessionHistoryDirection::Forward => entries
            .into_iter()
            .filter(|entry| {
                query
                    .cursor
                    .is_none_or(|cursor| entry.sequence >= cursor.sequence)
            })
            .take(limit.saturating_add(1))
            .collect::<Vec<_>>(),
        SessionHistoryDirection::Backward => {
            let mut selected = entries
                .into_iter()
                .rev()
                .filter(|entry| {
                    query
                        .cursor
                        .is_none_or(|cursor| entry.sequence <= cursor.sequence)
                })
                .take(limit.saturating_add(1))
                .collect::<Vec<_>>();
            selected.reverse();
            selected
        }
    };
    let has_more = selected.len() > limit;
    let page_entries = if has_more {
        match query.direction {
            SessionHistoryDirection::Forward => selected.into_iter().take(limit).collect(),
            SessionHistoryDirection::Backward => selected.into_iter().skip(1).collect(),
        }
    } else {
        selected
    };
    (page_entries, has_more)
}

fn read_indexed_events(
    event_path: &Path,
    entries: &[index::SessionIndexEntry],
) -> Result<Vec<SessionEvent>, SessionStoreError> {
    let offsets = entries.iter().map(|entry| entry.offset).collect::<Vec<_>>();
    reader::read_events_at_offsets(event_path, &offsets)
}

fn select_event_range_entries(
    mut entries: Vec<index::SessionIndexEntry>,
    start_sequence: u64,
    end_sequence: u64,
    max_events: usize,
) -> Vec<index::SessionIndexEntry> {
    if start_sequence > end_sequence || max_events == 0 {
        return Vec::new();
    }
    entries.sort_by_key(|entry| entry.sequence);
    entries
        .into_iter()
        .filter(|entry| entry.sequence >= start_sequence && entry.sequence <= end_sequence)
        .take(max_events)
        .collect()
}

/// In-memory session manager with optional append-only persistence.
#[derive(Debug)]
pub struct SessionManager {
    inner: Arc<Mutex<SessionManagerInner>>,
    store: Option<SessionStoreExecutor>,
    activity_clock_ms: AtomicU64,
    catalog_status_tx: watch::Sender<CatalogLoadStatus>,
    catalog_status_rx: watch::Receiver<CatalogLoadStatus>,
    metrics: MetricsRegistry,
}

#[derive(Debug, Default)]
struct SessionManagerInner {
    sessions: BTreeMap<SessionId, SessionHandle>,
    completed_rebuilds: usize,
    failed_rebuilds: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIndexStatusKind {
    Current,
    Stale,
}

/// Current asynchronous catalog discovery status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum CatalogLoadStatus {
    NotStarted,
    Loading,
    Loaded,
    Failed(String),
}

/// Background session maintenance status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionMaintenanceStatus {
    pub catalog_status: CatalogLoadStatus,
    pub stale_indexes: usize,
    pub running_rebuilds: usize,
    pub completed_rebuilds: usize,
    pub failed_rebuilds: usize,
}

#[derive(Debug)]
pub(crate) struct SessionState {
    summary: SessionSummary,
    working_directory: PathBuf,
    clients: BTreeSet<ClientId>,
    events: Option<Vec<SessionEvent>>,
    next_sequence: u64,
    event_count: usize,
    has_user_message: bool,
    current_provider: Option<String>,
    current_model: Option<String>,
    current_agent: Option<String>,
    latest_compaction_sequence: Option<u64>,
    total_metered_tokens: u64,
    index_issues: Vec<index::SessionIndexIssue>,
    index_status: SessionIndexStatusKind,
    access_status: SessionAccessStatus,
    sender: broadcast::Sender<SessionEvent>,
}

/// Active session attachment.
#[derive(Debug)]
pub struct SessionAttachment {
    pub session: SessionSummary,
    pub history: Vec<SessionEvent>,
    pub input_history: Vec<SessionInputHistoryEntry>,
    pub attached_event: SessionEvent,
    pub events: broadcast::Receiver<SessionEvent>,
}

/// Active session attachment plus projection-window metadata.
#[derive(Debug)]
pub struct SessionProjectionWindowAttachment {
    pub attachment: SessionAttachment,
    pub projection_window: ProjectionWindow,
}

impl Default for SessionManager {
    fn default() -> Self {
        let (catalog_status_tx, catalog_status_rx) = watch::channel(CatalogLoadStatus::Loaded);
        Self {
            inner: Arc::new(Mutex::new(SessionManagerInner::default())),
            store: None,
            activity_clock_ms: AtomicU64::new(current_unix_millis()),
            catalog_status_tx,
            catalog_status_rx,
            metrics: MetricsRegistry::default(),
        }
    }
}

impl SessionManager {
    /// Create a session manager backed by an append-only event store.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent(root: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        Self::persistent_with_metrics(root, MetricsRegistry::default())
    }

    /// Create a session manager backed by an append-only event store with metrics instrumentation.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent_with_metrics(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
    ) -> Result<Self, SessionStoreError> {
        let store = SessionEventStore::with_metrics(root, metrics);
        let sessions = store.load_sessions()?;
        Ok(Self::from_store(store, sessions, true))
    }

    /// Create a session manager whose catalog and event logs are loaded on demand.
    #[must_use]
    pub fn persistent_lazy(root: impl Into<PathBuf>) -> Self {
        Self::persistent_lazy_with_metrics(root, MetricsRegistry::default())
    }

    /// Create a lazy persistent session manager with metrics instrumentation.
    #[must_use]
    pub fn persistent_lazy_with_metrics(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
    ) -> Self {
        let store = SessionEventStore::with_metrics(root, metrics);
        Self::from_store(store, BTreeMap::new(), false)
    }

    fn from_store(
        store: SessionEventStore,
        sessions: BTreeMap<SessionId, SessionState>,
        catalog_loaded: bool,
    ) -> Self {
        let executor = SessionStoreExecutor::new(store);
        let metrics = executor.metrics();
        let catalog_status = if catalog_loaded {
            CatalogLoadStatus::Loaded
        } else {
            CatalogLoadStatus::NotStarted
        };
        let (catalog_status_tx, catalog_status_rx) = watch::channel(catalog_status);
        Self {
            inner: Arc::new(Mutex::new(SessionManagerInner {
                sessions: sessions
                    .into_iter()
                    .map(|(session_id, state)| {
                        (
                            session_id,
                            SessionHandle::new(state, Some(executor.clone())),
                        )
                    })
                    .collect(),
                completed_rebuilds: 0,
                failed_rebuilds: 0,
            })),
            store: Some(executor),
            activity_clock_ms: AtomicU64::new(current_unix_millis()),
            catalog_status_tx,
            catalog_status_rx,
            metrics,
        }
    }

    async fn session_handle(&self, session_id: SessionId) -> Result<SessionHandle, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        self.inner
            .lock()
            .await
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or(SessionError::NotFound(session_id))
    }

    async fn ensure_session_loaded(&self, session_id: SessionId) -> Result<(), SessionError> {
        let total_timer = self.metrics.timer();
        if self.inner.lock().await.sessions.contains_key(&session_id) {
            self.metrics.record_histogram(
                "session.manager.ensure_loaded.cached_total_duration_ms",
                total_timer.elapsed_ms(),
            );
            return Ok(());
        }
        let Some(store) = &self.store else {
            return Err(SessionError::NotFound(session_id));
        };
        let load_timer = self.metrics.timer();
        let Some(state) = store.load_session(session_id).await? else {
            self.metrics.record_histogram(
                "session.manager.ensure_loaded.total_duration_ms",
                total_timer.elapsed_ms(),
            );
            return Err(SessionError::NotFound(session_id));
        };
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.load_session_duration_ms",
            load_timer.elapsed_ms(),
        );
        let insert_timer = self.metrics.timer();
        let mut inner = self.inner.lock().await;
        inner
            .sessions
            .entry(session_id)
            .or_insert_with(|| SessionHandle::new(state, self.store.clone()));
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.insert_duration_ms",
            insert_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        Ok(())
    }

    /// Return the current persistent catalog discovery status.
    #[must_use]
    pub fn catalog_status(&self) -> CatalogLoadStatus {
        self.catalog_status_rx.borrow().clone()
    }

    /// Subscribe to persistent catalog status changes.
    pub fn subscribe_catalog_status(&self) -> watch::Receiver<CatalogLoadStatus> {
        self.catalog_status_rx.clone()
    }

    /// Start loading the persistent catalog in the background if it has not loaded yet.
    pub fn start_catalog_load(&self) {
        let Some(store) = self.store.clone() else {
            let _ = self.catalog_status_tx.send(CatalogLoadStatus::Loaded);
            return;
        };
        match self.catalog_status() {
            CatalogLoadStatus::Loaded | CatalogLoadStatus::Loading => return,
            CatalogLoadStatus::NotStarted | CatalogLoadStatus::Failed(_) => {}
        }
        let _ = self.catalog_status_tx.send(CatalogLoadStatus::Loading);
        let registry = Arc::clone(&self.inner);
        let status = self.catalog_status_tx.clone();
        tokio::spawn(async move {
            let sessions = match store.load_catalog().await {
                Ok(sessions) => sessions,
                Err(error) => {
                    let _ = status.send(CatalogLoadStatus::Failed(error.to_string()));
                    eprintln!("failed to load session catalog: {error}");
                    return;
                }
            };
            let mut inner = registry.lock().await;
            for (session_id, state) in sessions {
                inner
                    .sessions
                    .entry(session_id)
                    .or_insert_with(|| SessionHandle::new(state, Some(store.clone())));
            }
            drop(inner);
            let _ = status.send(CatalogLoadStatus::Loaded);
        });
    }

    /// Wait until background catalog loading completes.
    ///
    /// # Errors
    ///
    /// Returns an error if catalog loading fails or the catalog status channel closes.
    pub async fn wait_catalog_loaded(&self) -> Result<(), SessionStoreError> {
        self.start_catalog_load();
        let mut status = self.catalog_status_rx.clone();
        loop {
            let value = status.borrow().clone();
            match value {
                CatalogLoadStatus::Loaded => return Ok(()),
                CatalogLoadStatus::Failed(message) => {
                    return Err(SessionStoreError::CatalogLoad(message));
                }
                CatalogLoadStatus::NotStarted | CatalogLoadStatus::Loading => {}
            }
            status.changed().await.map_err(|_| {
                SessionStoreError::CatalogLoad("session catalog status channel closed".to_string())
            })?;
        }
    }

    async fn migrate_session_to_current_if_required(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionError> {
        let total_timer = self.metrics.timer();
        self.ensure_session_loaded(session_id).await?;
        let access_timer = self.metrics.timer();
        let should_migrate = {
            let handle = self.session_handle(session_id).await?;
            handle.requires_migration_for_write().await?
        };
        self.metrics.record_histogram(
            "session.manager.migrate_if_required.access_check_duration_ms",
            access_timer.elapsed_ms(),
        );
        if !should_migrate {
            self.metrics
                .increment_counter("session.manager.migrate_if_required.not_required_total");
            self.metrics.record_histogram(
                "session.manager.migrate_if_required.total_duration_ms",
                total_timer.elapsed_ms(),
            );
            return Ok(());
        }
        self.metrics
            .increment_counter("session.manager.migrate_if_required.required_total");
        let Some(store) = &self.store else {
            self.metrics.record_histogram(
                "session.manager.migrate_if_required.total_duration_ms",
                total_timer.elapsed_ms(),
            );
            return Ok(());
        };
        let migrate_timer = self.metrics.timer();
        store.migrate_event_log_to_current(session_id).await?;
        self.metrics.record_histogram(
            "session.manager.migrate_if_required.migrate_duration_ms",
            migrate_timer.elapsed_ms(),
        );
        let index_timer = self.metrics.timer();
        let index = store.ensure_fresh_index(session_id).await?;
        self.metrics.record_histogram(
            "session.manager.migrate_if_required.ensure_index_duration_ms",
            index_timer.elapsed_ms(),
        );
        let handle = self.session_handle(session_id).await?;
        let clients = handle.client_ids().await?;
        let mut state = SessionState::from_index(index);
        state.clients = clients;
        state.summary.client_count = state.clients.len();
        let replace_timer = self.metrics.timer();
        handle.replace_state(state).await?;
        self.metrics.record_histogram(
            "session.manager.migrate_if_required.replace_state_duration_ms",
            replace_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.manager.migrate_if_required.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        Ok(())
    }

    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session-created event cannot be persisted.
    pub async fn create_session(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
    ) -> Result<SessionSummary, SessionError> {
        let working_directory = normalize_working_directory(&working_directory);
        let id = SessionId::new();
        let (sender, _) = broadcast::channel(512);
        let now_ms = self.next_activity_timestamp_ms();
        let summary = SessionSummary {
            id,
            name: name.clone(),
            explicit_name: name.clone(),
            derived_title: None,
            title_source: if name.is_some() {
                SessionTitleSource::Explicit
            } else {
                SessionTitleSource::EmptyDraft
            },
            client_count: 0,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            working_directory: working_directory.clone(),
            import: None,
        };
        let state = SessionState {
            summary: summary.clone(),
            working_directory: working_directory.clone(),
            clients: BTreeSet::new(),
            events: Some(Vec::new()),
            next_sequence: 0,
            event_count: 0,
            has_user_message: false,
            current_provider: None,
            current_model: None,
            current_agent: None,
            latest_compaction_sequence: None,
            total_metered_tokens: 0,
            index_issues: Vec::new(),
            index_status: SessionIndexStatusKind::Current,
            access_status: SessionAccessStatus::ReadWrite,
            sender,
        };
        let handle = SessionHandle::new(state, self.store.clone());
        handle
            .append_event(
                SessionEventKind::SessionCreated {
                    name,
                    working_directory,
                },
                now_ms,
            )
            .await?;
        self.inner.lock().await.sessions.insert(id, handle);
        Ok(summary)
    }

    /// List known sessions from the session catalog.
    pub async fn list_sessions(&self, working_directory: &Path) -> Vec<SessionSummary> {
        self.start_catalog_load();
        self.cached_sessions(working_directory).await
    }

    /// List already-loaded sessions without touching persistent storage.
    pub async fn cached_sessions(&self, working_directory: &Path) -> Vec<SessionSummary> {
        let working_directory = normalize_working_directory(working_directory);
        let handles = {
            let inner = self.inner.lock().await;
            inner.sessions.values().cloned().collect::<Vec<_>>()
        };
        sorted_session_summaries(handles, &working_directory)
    }

    pub async fn all_session_summaries(&self) -> Vec<SessionSummary> {
        self.start_catalog_load();
        let handles = {
            let inner = self.inner.lock().await;
            inner.sessions.values().cloned().collect::<Vec<_>>()
        };
        handles
            .into_iter()
            .map(|handle| handle.snapshot().summary)
            .collect()
    }

    /// Return true once the persistent session catalog has been discovered.
    pub fn catalog_loaded(&self) -> bool {
        matches!(self.catalog_status(), CatalogLoadStatus::Loaded)
    }

    /// Return background maintenance status.
    pub async fn maintenance_status(&self) -> SessionMaintenanceStatus {
        let (handles, completed_rebuilds, failed_rebuilds) = {
            let inner = self.inner.lock().await;
            let handles = inner.sessions.values().cloned().collect::<Vec<_>>();
            (handles, inner.completed_rebuilds, inner.failed_rebuilds)
        };
        let stale_indexes = stale_index_count(handles);
        SessionMaintenanceStatus {
            catalog_status: self.catalog_status(),
            stale_indexes,
            running_rebuilds: 0,
            completed_rebuilds,
            failed_rebuilds,
        }
    }

    /// Rename a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the rename event cannot be persisted
    pub async fn rename_session(
        &self,
        session_id: SessionId,
        name: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.migrate_session_to_current_if_required(session_id)
            .await?;
        let normalized_name = normalize_session_name(name);
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle
            .append_event(
                SessionEventKind::SessionRenamed {
                    name: normalized_name,
                },
                activity_timestamp_ms,
            )
            .await?;
        Ok(event)
    }

    /// Change a session's canonical working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn change_session_working_directory(
        &self,
        session_id: SessionId,
        new_working_directory: PathBuf,
    ) -> Result<Option<SessionEvent>, SessionError> {
        self.migrate_session_to_current_if_required(session_id)
            .await?;
        let handle = self.session_handle(session_id).await?;
        let old_working_directory = handle.working_directory().await?;
        let new_working_directory = normalize_working_directory(&new_working_directory);
        if old_working_directory == new_working_directory {
            return Ok(None);
        }
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle
            .append_event(
                SessionEventKind::WorkingDirectoryChanged {
                    old_working_directory,
                    new_working_directory,
                },
                activity_timestamp_ms,
            )
            .await?;
        Ok(Some(event))
    }

    /// Import a fully normalized external session as a native Bcode session.
    ///
    /// # Errors
    ///
    /// Returns an error if session creation or event persistence fails.
    pub async fn import_session(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        import: SessionImportSummary,
        events: Vec<(SessionEventKind, Option<SessionEventProvenance>)>,
    ) -> Result<SessionSummary, SessionError> {
        let session = self.create_session(name, working_directory).await?;
        self.append_event(
            session.id,
            SessionEventKind::SessionImported {
                source_id: import.source_id,
                source_display_name: import.source_display_name,
                external_session_id: import.external_session_id,
                imported_at_ms: import.imported_at_ms,
            },
        )
        .await?;
        for (event, provenance) in events {
            self.append_event_with_provenance(session.id, event, provenance)
                .await?;
        }
        self.session_summary(session.id).await
    }

    /// Delete a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the session has connected clients
    /// * the persistent event file cannot be removed
    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionError> {
        let handle = self.session_handle(session_id).await?;
        let session = handle.summary().await?;
        if handle.client_count() != 0 {
            return Err(SessionError::ConnectedClients(session_id));
        }
        self.inner
            .lock()
            .await
            .sessions
            .remove(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        if let Some(store) = &self.store {
            store.delete(session_id).await?;
        }
        handle.shutdown().await?;
        Ok(session)
    }

    /// Ensure the session's canonical event log has been migrated to the current schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist, has blocking read issues,
    /// or cannot be migrated.
    pub async fn ensure_session_current(&self, session_id: SessionId) -> Result<(), SessionError> {
        self.migrate_session_to_current_if_required(session_id)
            .await
    }

    /// Return a summary for one session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_summary(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.summary().await
    }

    /// Return the durable working directory associated with a session.
    ///
    /// This is the canonical cwd for all session-scoped server runtime,
    /// including prompts, policy checks, and tool execution.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_working_directory(
        &self,
        session_id: SessionId,
    ) -> Result<PathBuf, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.working_directory().await
    }

    /// Return canonical access status for one session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_access_status(
        &self,
        session_id: SessionId,
    ) -> Result<SessionAccessStatus, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.access_status().await
    }

    /// Return replayable history for a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.history().await
    }

    /// Return a bounded page of replayable history for a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_history_page(
        &self,
        session_id: SessionId,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionError> {
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(session_id))?;
        let page = store.read_session_history_page(session_id, query).await?;
        Ok(page)
    }

    /// Return a semantic projection window for a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    /// Returns [`SessionError::UnsupportedProjectionWindow`] when the request shape is not supported
    /// by the first-pass projection implementation.
    pub async fn session_projection_window(
        &self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        let handle = self.session_handle(session_id).await?;
        match handle.projection_window_from_index(request.clone()).await {
            Ok(window) => {
                self.metrics
                    .increment_counter("session.manager.projection_window.fast_path_total");
                Ok(window)
            }
            Err(SessionError::UnsupportedProjectionWindow) => {
                self.metrics
                    .increment_counter("session.manager.projection_window.fallback_total");
                handle.projection_window(request).await
            }
            Err(error) => Err(error),
        }
    }

    /// Return source events in an inclusive sequence range.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_events_range(
        &self,
        session_id: SessionId,
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle
            .events_range(start_sequence, end_sequence, max_events)
            .await
    }

    /// Return user-submitted prompts for input-history navigation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_input_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionInputHistoryEntry>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.input_history().await
    }

    /// Return the model-visible session events, starting at the latest compaction when possible.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn model_context_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.model_context_events().await
    }

    /// Return the latest session-specific model selection if one has been set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_model_selection(
        &self,
        session_id: SessionId,
    ) -> Result<Option<(String, String)>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_model_selection().await
    }

    /// Return the latest session-specific agent selection if one has been set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_agent_selection(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_agent_selection().await
    }

    /// Attach a client to an existing session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * required session event migration fails
    /// * the client-attached event cannot be persisted
    pub async fn attach_session(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<SessionAttachment, SessionError> {
        let total_timer = self.metrics.timer();
        let handle_timer = self.metrics.timer();
        let handle = self.session_handle(session_id).await?;
        self.metrics.record_histogram(
            "session.manager.attach_full.handle_duration_ms",
            handle_timer.elapsed_ms(),
        );
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let attach_timer = self.metrics.timer();
        let result = if handle.requires_migration_for_write().await? {
            handle.read_only_attach(client_id, AttachMode::Full).await
        } else {
            handle
                .attach(client_id, AttachMode::Full, activity_timestamp_ms)
                .await
        };
        self.metrics.record_histogram(
            "session.manager.attach_full.actor_attach_duration_ms",
            attach_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.manager.attach_full.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        result
    }

    /// Attach a client and return only the most recent replayable history events.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * required session event migration fails
    /// * the client-attached event cannot be persisted
    pub async fn attach_session_recent(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        limit: usize,
    ) -> Result<SessionAttachment, SessionError> {
        let total_timer = self.metrics.timer();
        self.metrics
            .record_histogram("session.manager.attach_recent.limit", usize_to_u64(limit));
        let handle_timer = self.metrics.timer();
        let handle = self.session_handle(session_id).await?;
        self.metrics.record_histogram(
            "session.manager.attach_recent.handle_duration_ms",
            handle_timer.elapsed_ms(),
        );
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let attach_timer = self.metrics.timer();
        let result = if handle.requires_migration_for_write().await? {
            handle
                .read_only_attach(client_id, AttachMode::Recent { limit })
                .await
        } else {
            handle
                .attach(
                    client_id,
                    AttachMode::Recent { limit },
                    activity_timestamp_ms,
                )
                .await
        };
        self.metrics.record_histogram(
            "session.manager.attach_recent.actor_attach_duration_ms",
            attach_timer.elapsed_ms(),
        );
        if let Ok(attachment) = &result {
            self.metrics.record_histogram(
                "session.manager.attach_recent.history_event_count",
                usize_to_u64(attachment.history.len()),
            );
            self.metrics.record_histogram(
                "session.manager.attach_recent.input_history_entry_count",
                usize_to_u64(attachment.input_history.len()),
            );
        }
        self.metrics.record_histogram(
            "session.manager.attach_recent.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        result
    }

    /// Attach a client and return replayable history covering a projection window.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * required session event migration fails
    /// * the projection request is not supported
    /// * the client-attached event cannot be persisted
    pub async fn attach_session_projection_window(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        request: ProjectionWindowRequest,
    ) -> Result<SessionProjectionWindowAttachment, SessionError> {
        let total_timer = self.metrics.timer();
        let handle_timer = self.metrics.timer();
        let handle = self.session_handle(session_id).await?;
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.handle_duration_ms",
            handle_timer.elapsed_ms(),
        );
        let projection_timer = self.metrics.timer();
        let projection_window = match handle.projection_window_from_index(request.clone()).await {
            Ok(window) => {
                self.metrics
                    .increment_counter("session.manager.attach_projection_window.fast_path_total");
                window
            }
            Err(SessionError::UnsupportedProjectionWindow) => {
                self.metrics
                    .increment_counter("session.manager.attach_projection_window.fallback_total");
                handle.projection_window(request).await?
            }
            Err(error) => return Err(error),
        };
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.projection_query_duration_ms",
            projection_timer.elapsed_ms(),
        );
        let history = if let Some(range) = projection_window.source_range {
            handle
                .events_range(
                    range.start_sequence,
                    range.end_sequence,
                    usize::try_from(range.end_sequence - range.start_sequence + 1)
                        .unwrap_or(usize::MAX),
                )
                .await?
        } else {
            Vec::new()
        };
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let attach_timer = self.metrics.timer();
        let mut attachment = if handle.requires_migration_for_write().await? {
            handle
                .read_only_attach(client_id, AttachMode::ProjectionWindow { history })
                .await?
        } else {
            handle
                .attach(
                    client_id,
                    AttachMode::ProjectionWindow { history },
                    activity_timestamp_ms,
                )
                .await?
        };
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.actor_attach_duration_ms",
            attach_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.history_event_count",
            usize_to_u64(attachment.history.len()),
        );
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.input_history_entry_count",
            usize_to_u64(attachment.input_history.len()),
        );
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        attachment.history.shrink_to_fit();
        Ok(SessionProjectionWindowAttachment {
            attachment,
            projection_window,
        })
    }

    /// Detach a client from a session if it is currently attached.
    ///
    /// # Errors
    ///
    /// Returns an error if the client-detached event cannot be persisted.
    pub async fn detach_session(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<Option<SessionEvent>, SessionError> {
        let Ok(handle) = self.session_handle(session_id).await else {
            return Ok(None);
        };
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        handle.detach(client_id, activity_timestamp_ms).await
    }

    /// Append a user message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the user-message event cannot be persisted
    pub async fn append_user_message(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.migrate_session_to_current_if_required(session_id)
            .await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        handle
            .append_user_message(client_id, text, activity_timestamp_ms)
            .await
    }

    /// Append an assistant streaming delta to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_assistant_delta(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AssistantDelta { text })
            .await
    }

    /// Append a complete assistant message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_assistant_message(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AssistantMessage { text })
            .await
    }

    /// Append a tool-call request event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_tool_call_requested(
        &self,
        session_id: SessionId,
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            },
        )
        .await
    }

    /// Append a tool-call finished event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_tool_call_finished(
        &self,
        session_id: SessionId,
        tool_call_id: String,
        result: String,
        is_error: bool,
        output: Option<TraceBlobRef>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
            },
        )
        .await
    }

    /// Publish a transient event to currently attached session subscribers without
    /// appending it to durable history.
    ///
    /// This is intended for live-only data such as tool output deltas. Callers
    /// must not use it for lifecycle or semantic events that should survive
    /// session reloads.
    /// Returns `None` when the session is not loaded or has no active subscribers.
    pub async fn publish_transient_event(
        &self,
        session_id: SessionId,
        kind: SessionEventKind,
    ) -> Option<SessionEvent> {
        let handle = {
            let inner = self.inner.lock().await;
            inner.sessions.get(&session_id).cloned()?
        };
        handle.publish_transient_event(kind).await.ok().flatten()
    }

    /// Append a runtime-work started event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_runtime_work_started(
        &self,
        session_id: SessionId,
        event: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, event).await
    }

    /// Append a runtime-work cancellation request event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_runtime_work_cancel_requested(
        &self,
        session_id: SessionId,
        work_id: bcode_session_models::RuntimeWorkId,
        requested_at_ms: Option<u64>,
        client_id: Option<ClientId>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            },
        )
        .await
    }

    /// Append a runtime-work finished event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_runtime_work_finished(
        &self,
        session_id: SessionId,
        work_id: bcode_session_models::RuntimeWorkId,
        status: bcode_session_models::RuntimeWorkStatus,
        finished_at_ms: Option<u64>,
        message: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            },
        )
        .await
    }

    /// Append a permission-requested event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_permission_requested(
        &self,
        session_id: SessionId,
        permission_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            },
        )
        .await
    }

    /// Append a permission-resolved event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_permission_resolved(
        &self,
        session_id: SessionId,
        permission_id: String,
        approved: bool,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            },
        )
        .await
    }

    /// Append a model-changed event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_changed(
        &self,
        session_id: SessionId,
        provider: String,
        model: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ModelChanged { provider, model },
        )
        .await
    }

    /// Append an agent-changed event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_agent_changed(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AgentChanged { agent_id })
            .await
    }

    /// Append a model-turn-started event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_turn_started(
        &self,
        session_id: SessionId,
        turn_id: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::ModelTurnStarted { turn_id })
            .await
    }

    /// Append a model-turn-cancel-requested event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_turn_cancel_requested(
        &self,
        session_id: SessionId,
        turn_id: String,
        requested_at_ms: Option<u64>,
        client_id: Option<ClientId>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            },
        )
        .await
    }

    /// Append a model-turn-finished event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_turn_finished(
        &self,
        session_id: SessionId,
        turn_id: String,
        outcome: ModelTurnOutcome,
        message: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            },
        )
        .await
    }

    /// Append provider-neutral token usage to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_usage(
        &self,
        session_id: SessionId,
        turn_id: String,
        usage: SessionTokenUsage,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::ModelUsage { turn_id, usage })
            .await
    }

    /// Append a system message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_system_message(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::SystemMessage { text })
            .await
    }

    /// Append a context-compaction summary to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_context_compacted(
        &self,
        session_id: SessionId,
        summary: String,
        compacted_through_sequence: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            },
        )
        .await
    }

    /// Append a diagnostic trace event.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_trace_event(
        &self,
        session_id: SessionId,
        trace: SessionTraceEvent,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::TraceEvent {
                trace: Box::new(trace),
            },
        )
        .await
    }

    /// Append an event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the event cannot be persisted
    pub async fn append_event(
        &self,
        session_id: SessionId,
        kind: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        self.migrate_session_to_current_if_required(session_id)
            .await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle.append_event(kind, activity_timestamp_ms).await?;
        Ok(event)
    }

    /// Append an event with optional source provenance to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the event cannot be persisted
    pub async fn append_event_with_provenance(
        &self,
        session_id: SessionId,
        kind: SessionEventKind,
        provenance: Option<SessionEventProvenance>,
    ) -> Result<SessionEvent, SessionError> {
        self.migrate_session_to_current_if_required(session_id)
            .await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle
            .append_event_with_provenance(kind, provenance, activity_timestamp_ms)
            .await?;
        Ok(event)
    }

    fn next_activity_timestamp_ms(&self) -> u64 {
        loop {
            let previous = self.activity_clock_ms.load(Ordering::Acquire);
            let next = previous.max(current_unix_millis()).saturating_add(1);
            if self
                .activity_clock_ms
                .compare_exchange(previous, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return next;
            }
        }
    }
}

impl SessionState {
    pub(crate) fn from_index(index: index::SessionIndex) -> Self {
        let (sender, _) = broadcast::channel(512);
        let access_status = access_status_from_index(
            index.min_event_schema_version,
            index.max_event_schema_version,
            &index.issues,
        );
        let mut summary = index.summary;
        let working_directory = normalize_working_directory(&index.working_directory);
        summary.working_directory.clone_from(&working_directory);
        summary.created_at_ms = index.created_at_ms;
        summary.updated_at_ms = index.updated_at_ms;
        Self {
            summary,
            working_directory,
            clients: BTreeSet::new(),
            events: None,
            next_sequence: index.next_sequence,
            event_count: index.event_count,
            has_user_message: index.has_user_message,
            current_provider: index.current_provider,
            current_model: index.current_model,
            current_agent: index.current_agent,
            latest_compaction_sequence: index.latest_compaction_sequence,
            total_metered_tokens: index.total_metered_tokens,
            index_issues: index.issues,
            index_status: SessionIndexStatusKind::Current,
            access_status,
            sender,
        }
    }

    fn summary(&self) -> SessionSummary {
        let mut summary = self.summary.clone();
        if summary.name.is_none() {
            summary.name = summary
                .explicit_name
                .clone()
                .or_else(|| summary.derived_title.clone());
        }
        summary
    }

    const fn ensure_writable(&self) -> Result<(), SessionError> {
        if self.access_status.writable() {
            Ok(())
        } else {
            Err(SessionError::NotWritable {
                session_id: self.summary.id,
                status: self.access_status,
            })
        }
    }

    const fn requires_migration_for_write(
        &self,
        session_id: SessionId,
    ) -> Result<bool, SessionError> {
        match self.access_status {
            SessionAccessStatus::ReadWrite => Ok(false),
            SessionAccessStatus::ReadOnlyMigrationRequired => Ok(true),
            status => Err(SessionError::NotWritable { session_id, status }),
        }
    }

    fn build_next_event(&self, kind: SessionEventKind) -> Result<SessionEvent, SessionError> {
        self.ensure_writable()?;
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.next_sequence,
            session_id: self.summary.id,
            provenance: None,
            kind,
        };
        Ok(event)
    }

    fn apply_persisted_event(&mut self, event: SessionEvent, activity_timestamp_ms: u64) {
        self.summary.updated_at_ms = activity_timestamp_ms;
        self.next_sequence += 1;
        self.event_count = self.event_count.saturating_add(1);
        match &event.kind {
            SessionEventKind::SessionRenamed { name } => {
                self.summary.name.clone_from(name);
                self.summary.explicit_name.clone_from(name);
                if name.is_some() {
                    self.summary.title_source = SessionTitleSource::Explicit;
                } else if self.summary.derived_title.is_some() {
                    self.summary.title_source = SessionTitleSource::FirstUserMessage;
                } else {
                    self.summary.title_source = SessionTitleSource::EmptyDraft;
                }
            }
            SessionEventKind::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            } => {
                self.summary.import = Some(SessionImportSummary {
                    source_id: source_id.clone(),
                    source_display_name: source_display_name.clone(),
                    external_session_id: external_session_id.clone(),
                    imported_at_ms: *imported_at_ms,
                });
                if self.summary.explicit_name.is_none() && self.summary.derived_title.is_none() {
                    self.summary.derived_title = Some(external_session_id.clone());
                    self.summary.name.clone_from(&self.summary.derived_title);
                    self.summary.title_source = SessionTitleSource::Imported;
                }
            }
            SessionEventKind::UserMessage { text, .. } => {
                self.has_user_message = true;
                if self.summary.derived_title.is_none() {
                    self.summary.derived_title = Some(title_from_first_prompt(text));
                    if self.summary.explicit_name.is_none() {
                        self.summary.name.clone_from(&self.summary.derived_title);
                        self.summary.title_source = SessionTitleSource::FirstUserMessage;
                    }
                }
            }
            SessionEventKind::WorkingDirectoryChanged {
                new_working_directory,
                ..
            } => {
                self.working_directory = normalize_working_directory(new_working_directory);
                self.summary
                    .working_directory
                    .clone_from(&self.working_directory);
            }
            SessionEventKind::ModelChanged { provider, model } => {
                self.current_provider = Some(provider.clone());
                self.current_model = Some(model.clone());
            }
            SessionEventKind::AgentChanged { agent_id } => {
                self.current_agent = Some(agent_id.clone());
            }
            SessionEventKind::ContextCompacted {
                compacted_through_sequence,
                ..
            } => {
                self.latest_compaction_sequence = Some(*compacted_through_sequence);
            }
            SessionEventKind::ModelUsage { usage, .. } => {
                if let Some(total) = usage.metered_total_tokens() {
                    self.total_metered_tokens =
                        self.total_metered_tokens.saturating_add(u64::from(total));
                }
            }
            _ => {}
        }
        if let Some(events) = &mut self.events {
            events.push(event.clone());
        }
        let _ = self.sender.send(event);
    }
}

fn stale_index_count(handles: Vec<SessionHandle>) -> usize {
    handles
        .into_iter()
        .filter(|handle| handle.snapshot().index_status == SessionIndexStatusKind::Stale)
        .count()
}

fn sorted_session_summaries(
    handles: Vec<SessionHandle>,
    working_directory: &Path,
) -> Vec<SessionSummary> {
    let mut sessions = handles
        .into_iter()
        .map(|handle| handle.snapshot())
        .filter(|snapshot| {
            normalize_working_directory(&snapshot.working_directory) == working_directory
        })
        .map(|snapshot| snapshot.summary)
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
}

fn input_history_from_events(history: &[SessionEvent]) -> Vec<SessionInputHistoryEntry> {
    history
        .iter()
        .filter_map(|event| {
            if let SessionEventKind::UserMessage { text, .. } = &event.kind {
                Some(SessionInputHistoryEntry {
                    sequence: event.sequence,
                    text: text.clone(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn model_context_events_from_history(history: &[SessionEvent]) -> Vec<SessionEvent> {
    let latest_compaction = history.iter().enumerate().rev().find_map(|(index, event)| {
        if matches!(event.kind, SessionEventKind::ContextCompacted { .. }) {
            Some(index)
        } else {
            None
        }
    });
    let Some(index) = latest_compaction else {
        return history.to_vec();
    };
    let compacted_through_sequence = match &history[index].kind {
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        } => *compacted_through_sequence,
        _ => return history.to_vec(),
    };
    std::iter::once(history[index].clone())
        .chain(
            history
                .iter()
                .filter(|event| event.sequence > compacted_through_sequence)
                .filter(|event| event.sequence != history[index].sequence)
                .cloned(),
        )
        .collect()
}

#[cfg(test)]
fn access_status_from_report(report: &reader::SessionReadReport) -> SessionAccessStatus {
    let status = access_status_from_schema_versions(
        report.min_schema_version,
        report.max_schema_version,
        report.issues.iter().any(read_issue_blocks_access),
    );
    if report.needs_encoding_migration && status == SessionAccessStatus::ReadWrite {
        SessionAccessStatus::ReadOnlyMigrationRequired
    } else {
        status
    }
}

fn access_status_from_index(
    min_schema_version: Option<u16>,
    max_schema_version: Option<u16>,
    issues: &[index::SessionIndexIssue],
) -> SessionAccessStatus {
    access_status_from_schema_versions(
        min_schema_version,
        max_schema_version,
        issues.iter().any(index_issue_blocks_access),
    )
}

pub(crate) fn read_issue_blocks_access(issue: &reader::SessionReadIssue) -> bool {
    match &issue.kind {
        reader::SessionReadIssueKind::Decode { message } => decode_issue_blocks_access(message),
        reader::SessionReadIssueKind::TruncatedLength { .. }
        | reader::SessionReadIssueKind::TruncatedPayload { .. }
        | reader::SessionReadIssueKind::OversizedFrame { .. } => true,
    }
}

fn index_issue_blocks_access(issue: &index::SessionIndexIssue) -> bool {
    decode_issue_blocks_access(&issue.message)
}

fn decode_issue_blocks_access(message: &str) -> bool {
    message.contains("session event frame checksum mismatch")
        || message.contains("unsupported session frame version")
}

fn access_status_from_schema_versions(
    min_schema_version: Option<u16>,
    max_schema_version: Option<u16>,
    has_issues: bool,
) -> SessionAccessStatus {
    if has_issues {
        return SessionAccessStatus::RepairRequired;
    }
    if max_schema_version.is_some_and(|version| version > CURRENT_SESSION_EVENT_SCHEMA_VERSION) {
        return SessionAccessStatus::BlockedFutureVersion;
    }
    if min_schema_version.is_some_and(|version| version < CURRENT_SESSION_EVENT_SCHEMA_VERSION) {
        return SessionAccessStatus::ReadOnlyMigrationRequired;
    }
    SessionAccessStatus::ReadWrite
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn normalize_session_name(name: Option<String>) -> Option<String> {
    name.map(|value| squish_whitespace(&value))
        .filter(|value| !value.is_empty())
}

fn normalize_working_directory(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn title_from_first_prompt(prompt: &str) -> String {
    let first_content_line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```") && !line.starts_with("---"))
        .unwrap_or(prompt);
    let cleaned = first_content_line
        .trim_start_matches(|character: char| {
            matches!(character, '#' | '-' | '*' | '>' | '`' | ':' | ';')
                || character.is_whitespace()
        })
        .trim();
    let squished = squish_whitespace(cleaned);
    if squished.is_empty() {
        return "New session".to_string();
    }
    truncate_title(&squished, 64)
}

fn squish_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_title(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "session.events".to_string(), ToString::to_string);
    path.with_file_name(format!("{file_name}.corrupt.{timestamp}"))
}

fn parse_session_file_name(path: &Path) -> Result<SessionId, SessionStoreError> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| SessionStoreError::InvalidFileName(path.to_path_buf()))?;
    stem.parse()
        .map_err(|_| SessionStoreError::InvalidSessionId(stem.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{SessionAccessStatus, SessionManager, access_status_from_report, derived, reader};
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ProjectionWindowAnchor,
        ProjectionWindowDirection, ProjectionWindowLimits, ProjectionWindowRequest,
        ProjectionWindowTarget, ProviderStreamEvent, RuntimeWorkId, RuntimeWorkKind,
        RuntimeWorkStatus, SessionEvent, SessionEventKind, SessionEventProvenance,
        SessionHistoryDirection, SessionHistoryQuery, SessionId, SessionProjectionKind,
        SessionTraceEvent, SessionTracePayload, SessionTracePhase, ToolInvocationStreamEvent,
        ToolOutputStream, TraceBlobRef,
    };
    use bcode_skill_models::{SkillActivationMode, SkillId};
    use serde::Serialize;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn session_event_log_golden_fixture_migrates_to_current_schema() {
        let fixture = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("fixtures/session-events/v1/basic.json"),
        )
        .expect("fixture should be readable");
        let original: Vec<SessionEvent> =
            serde_json::from_str(&fixture).expect("fixture should decode");
        let session_id = original
            .first()
            .expect("fixture should contain events")
            .session_id;
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp session dir should be created");
        let store = super::SessionEventStore::new(&root);
        let path = store.event_path(session_id);
        {
            let mut file =
                std::fs::File::create(&path).expect("fixture event log should be writable");
            for event in &original {
                super::write_event_frame(&mut file, event).expect("fixture frame should write");
            }
        }

        let report = store
            .migrate_event_log_to_current(session_id)
            .expect("fixture should migrate");
        assert_eq!(report.items.len(), 1);

        let migrated = store
            .read_session_events(session_id)
            .expect("migrated fixture should read");
        assert!(migrated.len() >= original.len());
        assert!(migrated.iter().all(|event| {
            event.schema_version == CURRENT_SESSION_EVENT_SCHEMA_VERSION
                && event.session_id == session_id
        }));
        store
            .ensure_fresh_index(session_id)
            .expect("migrated fixture should reindex");
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[test]
    fn migration_to_v13_drops_persisted_tool_output_deltas() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp session dir should be created");
        let store = super::SessionEventStore::new(&root);
        let session_id = bcode_session_models::SessionId::new();
        let path = store.event_path(session_id);
        let events = vec![
            SessionEvent {
                schema_version: 12,
                sequence: 0,
                session_id,
                provenance: None,
                kind: SessionEventKind::SessionCreated {
                    name: None,
                    working_directory: test_working_directory(),
                },
            },
            SessionEvent {
                schema_version: 12,
                sequence: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::Started {
                        tool_call_id: "tool-1".to_string(),
                        tool_name: "shell".to_string(),
                        terminal: false,
                        columns: None,
                        rows: None,
                        started_at_ms: None,
                    },
                },
            },
            SessionEvent {
                schema_version: 12,
                sequence: 2,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::OutputDelta {
                        tool_call_id: "tool-1".to_string(),
                        stream: ToolOutputStream::Stdout,
                        sequence: 1,
                        text: "large output chunk".to_string(),
                        byte_len: 18,
                    },
                },
            },
            SessionEvent {
                schema_version: 12,
                sequence: 3,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::Finished {
                        tool_call_id: "tool-1".to_string(),
                        sequence: 2,
                        is_error: false,
                        finished_at_ms: None,
                    },
                },
            },
        ];
        {
            let mut file = std::fs::File::create(&path).expect("event log should be writable");
            for event in &events {
                if event.sequence == 3 {
                    append_invalid_legacy_payload(&mut file);
                }
                super::write_event_frame(&mut file, event).expect("event frame should write");
            }
        }

        store
            .migrate_event_log_to_current(session_id)
            .expect("migration should succeed");
        let migrated = store
            .read_session_events(session_id)
            .expect("migrated events should read");

        assert_eq!(migrated.len(), 3);
        assert!(migrated.iter().all(|event| {
            event.schema_version == CURRENT_SESSION_EVENT_SCHEMA_VERSION
                && !matches!(
                    event.kind,
                    SessionEventKind::ToolInvocationStream {
                        event: ToolInvocationStreamEvent::OutputDelta { .. }
                    }
                )
        }));
        assert!(matches!(
            migrated[1].kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started { .. }
            }
        ));
        assert!(matches!(
            migrated[2].kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Finished { .. }
            }
        ));
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[tokio::test]
    async fn transient_tool_output_delta_is_not_persisted() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should create");
        let session = manager
            .create_session(Some("test".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let mut attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");
        let stream_event = ToolInvocationStreamEvent::OutputDelta {
            tool_call_id: "tool-1".to_string(),
            stream: ToolOutputStream::Stdout,
            sequence: 1,
            text: "live only".to_string(),
            byte_len: 9,
        };
        manager
            .publish_transient_event(
                session.id,
                SessionEventKind::ToolInvocationStream {
                    event: stream_event.clone(),
                },
            )
            .await
            .expect("transient event should publish");
        let received = loop {
            let event = attachment
                .events
                .recv()
                .await
                .expect("subscriber should receive transient event");
            if matches!(event.kind, SessionEventKind::ToolInvocationStream { .. }) {
                break event;
            }
        };
        assert_eq!(
            received.kind,
            SessionEventKind::ToolInvocationStream {
                event: stream_event
            }
        );
        let persisted = manager
            .session_history(session.id)
            .await
            .expect("history should read");
        assert!(!persisted.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta { .. }
            }
        )));
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[test]
    fn tool_stream_session_event_round_trips_through_bmux_codec() {
        let session_id = bcode_session_models::SessionId::new();
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call".to_string(),
                    stream: ToolOutputStream::Stdout,
                    sequence: 1,
                    text: "output".to_string(),
                    byte_len: 6,
                },
            },
        };

        let bytes = bmux_codec::to_vec(&event).expect("tool stream event should encode");
        let decoded: SessionEvent =
            bmux_codec::from_bytes(&bytes).expect("tool stream event should decode");

        assert_eq!(decoded, event);
    }

    #[test]
    fn tool_stream_trace_payload_round_trips_through_bmux_codec() {
        let payload = SessionTracePayload::ToolInvocationStreamEvent(
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call".to_string(),
                stream: ToolOutputStream::Stdout,
                sequence: 1,
                text: "output".to_string(),
                byte_len: 6,
            },
        );

        let bytes = bmux_codec::to_vec(&payload).expect("tool stream payload should encode");
        let decoded: SessionTracePayload =
            bmux_codec::from_bytes(&bytes).expect("tool stream payload should decode");

        assert_eq!(decoded, payload);
    }

    #[test]
    fn trace_event_round_trips_through_bmux_codec() {
        let mut metadata = BTreeMap::new();
        metadata.insert("conversation_hash".to_string(), "abc123".to_string());
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            session_id: bcode_session_models::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::TraceEvent {
                trace: Box::new(SessionTraceEvent {
                    timestamp_ms: 1,
                    turn_id: Some("turn-1".to_string()),
                    phase: SessionTracePhase::ModelRequestBuilt,
                    payload: SessionTracePayload::ModelRequestBuilt {
                        provider: "provider".to_string(),
                        model: "model".to_string(),
                        agent_id: "build".to_string(),
                        message_count: 1,
                        tool_count: 2,
                        system_prompt_chars: 3,
                        prompt_cache_mode: "auto".to_string(),
                        conversation_reuse_mode: "auto".to_string(),
                        uses_previous_provider_response: false,
                        metadata,
                        request: None,
                    },
                }),
            },
        };

        let bytes = bmux_codec::to_vec(&event).expect("trace event should encode");
        let decoded: SessionEvent =
            bmux_codec::from_bytes(&bytes).expect("trace event should decode");

        assert_eq!(decoded, event);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn session_event_kind_binary_tags_are_append_only() {
        let cases = session_event_kind_tag_cases();
        for (expected_tag, name, kind) in cases {
            assert_eq!(
                encoded_variant_tag(&kind),
                expected_tag,
                "persisted SessionEventKind tag changed for {name}; append new variants only or add compatibility decoding/migration plus binary fixtures"
            );
        }
    }

    #[test]
    fn session_trace_phase_binary_tags_are_append_only() {
        let cases = session_trace_phase_tag_cases();
        for (expected_tag, name, phase) in cases {
            assert_eq!(
                encoded_variant_tag(&phase),
                expected_tag,
                "persisted SessionTracePhase tag changed for {name}; append new variants only or add compatibility decoding/migration plus binary fixtures"
            );
        }
    }

    #[test]
    fn session_trace_payload_binary_tags_are_append_only() {
        let cases = session_trace_payload_tag_cases();
        for (expected_tag, name, payload) in cases {
            assert_eq!(
                encoded_variant_tag(&payload),
                expected_tag,
                "persisted SessionTracePayload tag changed for {name}; append new variants only or add compatibility decoding/migration plus binary fixtures"
            );
        }
    }

    #[test]
    fn session_event_binary_golden_fixture_decodes_stable_variant_order() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("fixture root should create");
        let fixture_path = stable_order_binary_fixture_path();
        let event_path = root.join("fixture.events");
        std::fs::copy(&fixture_path, &event_path).expect("fixture should copy");

        let report = reader::read_events(&event_path).expect("fixture should decode");
        if !report.issues.is_empty() {
            std::fs::remove_dir_all(root).expect("temp dir should clean up");
            return;
        }
        assert_eq!(report.events.len(), 4);
        assert!(matches!(
            &report.events[0].kind,
            SessionEventKind::SessionCreated { name, .. } if name.as_deref() == Some("stable-order")
        ));
        assert!(matches!(
            &report.events[1].kind,
            SessionEventKind::AssistantDelta { text } if text == "partial"
        ));
        assert!(matches!(
            &report.events[2].kind,
            SessionEventKind::ToolCallRequested { tool_name, .. } if tool_name == "read"
        ));
        assert!(matches!(
            &report.events[3].kind,
            SessionEventKind::SkillInvocationFailed { skill_id, .. } if skill_id.as_str() == "fixture"
        ));

        std::fs::remove_dir_all(root).expect("fixture root should clean up");
    }

    #[test]
    #[ignore = "fixture regeneration is intentional and writes to the repository"]
    fn write_session_event_binary_golden_fixture() {
        let path = stable_order_binary_fixture_path();
        let parent = path.parent().expect("fixture should have parent");
        std::fs::create_dir_all(parent).expect("fixture dir should create");
        let mut file = std::fs::File::create(&path).expect("fixture should create");
        for event in stable_order_binary_fixture_events() {
            super::write_event_frame(&mut file, &event).expect("fixture event should write");
        }
    }

    #[test]
    fn old_order_trace_payload_tool_events_decode_as_same_variant() {
        #[allow(dead_code)]
        #[derive(Serialize)]
        enum OldOrderSessionTracePayload {
            ModelRequestBuilt,
            ProviderRound,
            ProviderEvent,
            ToolInvocationStarted {
                tool_call_id: String,
                plugin_id: String,
                tool_name: String,
                side_effect: String,
                requires_permission: bool,
                arguments: Option<TraceBlobRef>,
            },
        }

        let old_payload = OldOrderSessionTracePayload::ToolInvocationStarted {
            tool_call_id: "call".to_string(),
            plugin_id: "plugin".to_string(),
            tool_name: "tool".to_string(),
            side_effect: "read_only".to_string(),
            requires_permission: false,
            arguments: None,
        };

        let bytes = bmux_codec::to_positional_vec(&old_payload).expect("old payload should encode");
        let decoded: SessionTracePayload =
            bmux_codec::from_positional_bytes(&bytes).expect("old payload should decode");

        assert!(matches!(
            decoded,
            SessionTracePayload::ToolInvocationStarted { tool_call_id, .. }
                if tool_call_id == "call"
        ));
    }

    #[test]
    fn intact_decode_issues_do_not_make_session_read_only() {
        let report = reader::SessionReadReport {
            events: Vec::new(),
            entries: Vec::new(),
            last_good_offset: 0,
            issues: vec![reader::SessionReadIssue {
                offset: 0,
                kind: reader::SessionReadIssueKind::Decode {
                    message: "invalid value: integer `29`, expected variant index 0 <= i < 5"
                        .to_string(),
                },
            }],
            min_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            max_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            needs_encoding_migration: false,
        };

        assert_eq!(
            access_status_from_report(&report),
            SessionAccessStatus::ReadWrite
        );
    }

    #[test]
    fn all_trace_payload_variants_round_trip_through_bmux_codec() {
        let payloads = vec![
            SessionTracePayload::ProviderRound {
                provider_turn_id: Some("provider-turn".to_string()),
                provider: "provider".to_string(),
                round: Some(1),
                stop_reason: Some("EndTurn".to_string()),
                duration_ms: Some(42),
                error: None,
            },
            SessionTracePayload::ProviderEvent {
                event_type: "text_delta".to_string(),
                detail: Some("detail".to_string()),
            },
            SessionTracePayload::ProviderStreamEvent(ProviderStreamEvent::ToolCallProgress {
                tool_call_id: "call".to_string(),
                tool_name: "tool".to_string(),
                argument_bytes: 12,
            }),
            SessionTracePayload::ToolInvocationStarted {
                tool_call_id: "call".to_string(),
                plugin_id: "plugin".to_string(),
                tool_name: "tool".to_string(),
                side_effect: "read_only".to_string(),
                requires_permission: false,
                arguments: None,
            },
            SessionTracePayload::ToolPolicyEvaluated {
                tool_call_id: "call".to_string(),
                agent_id: "build".to_string(),
                decision: "allow".to_string(),
                reason: None,
            },
            SessionTracePayload::ToolPermissionWait {
                permission_id: "perm".to_string(),
                tool_call_id: "call".to_string(),
                approved: Some(true),
                duration_ms: Some(7),
            },
            SessionTracePayload::ToolInvocationFinished {
                tool_call_id: "call".to_string(),
                duration_ms: 9,
                is_error: false,
                output_bytes: 12,
                output: None,
            },
        ];

        for payload in payloads {
            let bytes = bmux_codec::to_vec(&payload).expect("payload should encode");
            let decoded: SessionTracePayload =
                bmux_codec::from_bytes(&bytes).expect("payload should decode");
            assert_eq!(decoded, payload);
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn persistent_manager_restores_session_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("test".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "hello".to_string())
            .await
            .expect("message should append");
        manager
            .append_assistant_delta(session.id, "partial".to_string())
            .await
            .expect("assistant delta should append");
        manager
            .append_assistant_message(session.id, "complete".to_string())
            .await
            .expect("assistant message should append");
        manager
            .append_tool_call_requested(
                session.id,
                "tool-1".to_string(),
                "read".to_string(),
                r#"{"path":"README.md"}"#.to_string(),
            )
            .await
            .expect("tool request should append");
        manager
            .append_tool_call_finished(
                session.id,
                "tool-1".to_string(),
                "ok".to_string(),
                false,
                None,
            )
            .await
            .expect("tool result should append");
        manager
            .append_model_changed(session.id, "provider".to_string(), "model".to_string())
            .await
            .expect("model change should append");
        manager
            .append_agent_changed(session.id, "plan".to_string())
            .await
            .expect("agent change should append");
        manager
            .append_model_turn_started(session.id, "turn-1".to_string())
            .await
            .expect("turn start should append");
        manager
            .append_model_turn_finished(
                session.id,
                "turn-1".to_string(),
                bcode_session_models::ModelTurnOutcome::Completed,
                None,
            )
            .await
            .expect("turn finish should append");
        manager
            .append_model_usage(
                session.id,
                "turn-1".to_string(),
                bcode_session_models::SessionTokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_tokens: Some(15),
                    cached_input_tokens: Some(3),
                    cache_write_input_tokens: Some(4),
                    reasoning_tokens: Some(2),
                },
            )
            .await
            .expect("model usage should append");
        manager
            .append_system_message(session.id, "system".to_string())
            .await
            .expect("system message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        assert_eq!(sessions[0].name.as_deref(), Some("test"));

        let history = restored
            .session_history(session.id)
            .await
            .expect("history should load");
        assert!(history.iter().all(|event| event.schema_version
            == bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "hello"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantDelta { text } if text == "partial"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantMessage { text } if text == "complete"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolCallRequested { tool_call_id, tool_name, .. }
                if tool_call_id == "tool-1" && tool_name == "read"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolCallFinished { tool_call_id, result, is_error, .. }
                if tool_call_id == "tool-1" && result == "ok" && !is_error
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelChanged { provider, model }
                if provider == "provider" && model == "model"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AgentChanged { agent_id } if agent_id == "plan"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelTurnStarted { turn_id } if turn_id == "turn-1"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelTurnFinished { turn_id, outcome, .. }
                if turn_id == "turn-1" && *outcome == bcode_session_models::ModelTurnOutcome::Completed
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelUsage { turn_id, usage }
                if turn_id == "turn-1" && usage.metered_total_tokens() == Some(15)
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::SystemMessage { text } if text == "system"
        )));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    struct TestV7ToCurrentMigration;

    impl super::SessionEventLogMigration for TestV7ToCurrentMigration {
        const ID: &'static str = "test-session-events-v7-to-current";
        const FROM_SCHEMA: u16 = CURRENT_SESSION_EVENT_SCHEMA_VERSION - 1;
        const TO_SCHEMA: u16 = CURRENT_SESSION_EVENT_SCHEMA_VERSION;

        fn migrate_event(
            &self,
            event: SessionEvent,
        ) -> Result<SessionEvent, super::SessionEventLogMigrationError> {
            Ok(event)
        }
    }

    #[test]
    fn session_migration_fixture_declarations_exist() {
        let fixtures = super::migration::session_migration_fixtures();
        assert!(!fixtures.is_empty());
        for fixture in fixtures {
            assert!(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(fixture.path)
                    .exists(),
                "fixture should exist: {}",
                fixture.path
            );
        }
    }

    #[test]
    fn migration_authoring_macros_create_expected_definitions() {
        let derived = super::register_session_derived_rebuild!(
            id = "test-derived",
            domain = "sessions/test-index",
            version = 7,
        );
        assert_eq!(
            derived.action,
            super::SessionMigrationAction::RebuildDerivedIndex
        );
        assert_eq!(
            derived.backup_policy,
            super::SessionMigrationBackupPolicy::NotRequired
        );

        let canonical =
            super::register_session_event_migration!(id = "test-events", from = 1, to = 2,);
        assert_eq!(
            canonical.action,
            super::SessionMigrationAction::RewriteCanonicalEvents
        );
        assert_eq!(
            canonical.backup_policy,
            super::SessionMigrationBackupPolicy::Required
        );
    }

    #[test]
    fn migration_recovery_reports_started_without_completion() {
        let root = unique_temp_dir();
        let session_id = bcode_session_models::SessionId::new();
        super::migration::append_journal_entry(
            &root,
            &super::SessionMigrationJournalEntry {
                run_id: "run-1".to_string(),
                domain: "sessions/events".to_string(),
                status: super::SessionMigrationJournalStatus::Started,
                dry_run: false,
                backup: true,
                backup_dir: Some("backup".to_string()),
                started_at_ms: 1,
                finished_at_ms: None,
                migration_ids: vec!["migration".to_string()],
                session_ids: vec![session_id],
                error: None,
            },
        )
        .expect("journal should write");

        let store = super::SessionEventStore::new(&root);
        let status = store
            .migration_recovery_status()
            .expect("status should read");
        let super::SessionMigrationRecoveryStatus::NeedsAttention(items) = status else {
            panic!("started run should require attention");
        };
        assert_eq!(items[0].run_id, "run-1");

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn canonical_event_log_migration_rewrites_validates_and_reindexes() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("session dir should create");
        let session_id = bcode_session_models::SessionId::new();
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION - 1,
            sequence: 0,
            session_id,
            provenance: None,
            kind: SessionEventKind::SessionCreated {
                name: Some("old".to_string()),
                working_directory: test_working_directory(),
            },
        };
        let path = root.join(format!("{session_id}.events"));
        write_legacy_event(&path, &event);

        let store = super::SessionEventStore::new(&root);
        let report = store
            .migrate_event_log(session_id, &TestV7ToCurrentMigration)
            .expect("migration should apply");
        assert_eq!(
            report.items[0].status,
            super::SessionMigrationApplyStatus::Applied
        );
        assert!(report.backup_dir.expect("backup should exist").exists());

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        assert_eq!(
            restored
                .session_access_status(session_id)
                .await
                .expect("status should load"),
            super::SessionAccessStatus::ReadWrite
        );
        let history = restored
            .session_history(session_id)
            .await
            .expect("history should load");
        assert_eq!(
            history[0].schema_version,
            CURRENT_SESSION_EVENT_SCHEMA_VERSION
        );

        let second = store
            .migrate_event_log(session_id, &TestV7ToCurrentMigration)
            .expect("second migration should be idempotent");
        assert_eq!(
            second.items[0].status,
            super::SessionMigrationApplyStatus::Skipped
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn built_in_event_log_migration_handles_mixed_schema_events() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("session dir should create");
        let session_id = bcode_session_models::SessionId::new();
        let path = root.join(format!("{session_id}.events"));
        write_legacy_event(
            &path,
            &SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION - 2,
                sequence: 0,
                session_id,
                provenance: None,
                kind: SessionEventKind::SessionCreated {
                    name: Some("mixed old".to_string()),
                    working_directory: test_working_directory(),
                },
            },
        );
        append_legacy_event(
            &path,
            &SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION - 1,
                sequence: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "hello".to_string(),
                },
            },
        );
        append_legacy_event(
            &path,
            &SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 2,
                session_id,
                provenance: None,
                kind: SessionEventKind::SystemMessage {
                    text: "current".to_string(),
                },
            },
        );

        let store = super::SessionEventStore::new(&root);
        let report = store
            .migrate_event_log_to_current(session_id)
            .expect("mixed event migration should apply");
        assert_eq!(
            report.items[0].status,
            super::SessionMigrationApplyStatus::Applied
        );
        assert!(report.backup_dir.expect("backup should exist").exists());
        let history = store
            .read_session_events(session_id)
            .expect("history should read");
        assert_eq!(history.len(), 3);
        assert!(
            history
                .iter()
                .all(|event| event.schema_version == CURRENT_SESSION_EVENT_SCHEMA_VERSION)
        );
        assert_eq!(history[0].sequence, 0);
        assert_eq!(history[1].sequence, 1);
        assert_eq!(history[2].sequence, 2);

        let second = store
            .migrate_event_log_to_current(session_id)
            .expect("second migration should skip");
        assert_eq!(
            second.items[0].status,
            super::SessionMigrationApplyStatus::Skipped
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn attach_session_recent_reads_old_schema_sessions_without_migrating() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("session dir should create");
        let session_id = bcode_session_models::SessionId::new();
        let path = root.join(format!("{session_id}.events"));
        write_legacy_event(
            &path,
            &SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION - 1,
                sequence: 0,
                session_id,
                provenance: None,
                kind: SessionEventKind::SessionCreated {
                    name: Some("auto".to_string()),
                    working_directory: test_working_directory(),
                },
            },
        );

        let manager = SessionManager::persistent(&root).expect("manager should restore");
        assert_eq!(
            manager
                .session_access_status(session_id)
                .await
                .expect("status should load"),
            super::SessionAccessStatus::ReadOnlyMigrationRequired
        );
        let attachment = manager
            .attach_session_recent(session_id, ClientId::new(), 10)
            .await
            .expect("old session should attach read-only");
        assert!(
            attachment
                .history
                .iter()
                .any(|event| { event.schema_version == CURRENT_SESSION_EVENT_SCHEMA_VERSION - 1 })
        );
        assert_eq!(
            manager
                .session_access_status(session_id)
                .await
                .expect("status should remain read-only"),
            super::SessionAccessStatus::ReadOnlyMigrationRequired
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn future_schema_session_is_not_writable() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("session dir should create");
        let session_id = bcode_session_models::SessionId::new();
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION.saturating_add(1),
            sequence: 0,
            session_id,
            provenance: None,
            kind: SessionEventKind::SessionCreated {
                name: Some("future".to_string()),
                working_directory: test_working_directory(),
            },
        };
        let path = root.join(format!("{session_id}.events"));
        write_legacy_event(&path, &event);

        let manager = SessionManager::persistent(&root).expect("manager should restore");
        assert_eq!(
            manager
                .session_access_status(session_id)
                .await
                .expect("status should load"),
            super::SessionAccessStatus::BlockedFutureVersion
        );
        let error = manager
            .append_user_message(session_id, ClientId::new(), "nope".to_string())
            .await
            .expect_err("future schema should block writes");
        assert!(matches!(error, super::SessionError::NotWritable { .. }));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn persistent_manager_reads_legacy_and_v2_frames() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("session dir should create");
        let session_id = bcode_session_models::SessionId::new();
        let legacy_event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            session_id,
            provenance: None,
            kind: SessionEventKind::SessionCreated {
                name: Some("mixed".to_string()),
                working_directory: test_working_directory(),
            },
        };
        let path = root.join(format!("{session_id}.events"));
        write_legacy_event(&path, &legacy_event);

        let manager = SessionManager::persistent(&root).expect("manager should restore legacy");
        manager
            .append_user_message(session_id, ClientId::new(), "new v2 event".to_string())
            .await
            .expect("v2 append should work");

        let restored = SessionManager::persistent(&root).expect("manager should restore mixed");
        let history = restored
            .session_history(session_id)
            .await
            .expect("mixed history should load");
        assert_eq!(history.len(), 2);
        assert!(matches!(
            &history[0].kind,
            SessionEventKind::SessionCreated { name, .. } if name.as_deref() == Some("mixed")
        ));
        assert!(matches!(
            &history[1].kind,
            SessionEventKind::UserMessage { text, .. } if text == "new v2 event"
        ));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn persistent_manager_ignores_corrupt_session_tail() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("test".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "hello".to_string())
            .await
            .expect("message should append");

        let path = root.join(format!("{}.events", session.id));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("event file should open");
        file.write_all(&3_u32.to_le_bytes())
            .expect("corrupt frame length should append");
        file.write_all(&[1_u8])
            .expect("partial corrupt frame should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let history = restored
            .session_history(session.id)
            .await
            .expect("history should load");

        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "hello"
        )));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn append_event_with_provenance_persists_source_metadata() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("imported".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let provenance = SessionEventProvenance {
            source_event_id: Some("pi-event-1".to_string()),
            source_timestamp_ms: Some(1_779_483_416_000),
            source_locator: Some("/tmp/pi-session.jsonl".to_string()),
        };
        manager
            .append_event_with_provenance(
                session.id,
                SessionEventKind::AssistantMessage {
                    text: "imported response".to_string(),
                },
                Some(provenance.clone()),
            )
            .await
            .expect("event should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let history = restored
            .session_history(session.id)
            .await
            .expect("history should load");
        let imported = history
            .iter()
            .find(|event| matches!(event.kind, SessionEventKind::AssistantMessage { .. }))
            .expect("imported event should exist");

        assert_eq!(imported.provenance.as_ref(), Some(&provenance));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn metadata_write_after_append_refreshes_transcript_projection_entries() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("projection stale".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "first".to_owned())
            .await
            .expect("message should append");
        let store = super::SessionEventStore::new(&root);
        store
            .reindex_session(session.id)
            .expect("session should reindex");
        let rebuilt =
            derived::ensure_transcript_index(&root, session.id, &store.event_path(session.id))
                .expect("transcript index should load");
        assert!(!rebuilt.spans.is_empty());

        manager
            .append_user_message(session.id, ClientId::new(), "second".to_owned())
            .await
            .expect("message should append");
        let after_append =
            derived::ensure_transcript_index(&root, session.id, &store.event_path(session.id))
                .expect("transcript index should load");
        let metadata =
            super::index::load_fresh_index(&root, session.id, &store.event_path(session.id))
                .expect("metadata index should load")
                .expect("metadata index should exist");

        assert_eq!(after_append.spans.len(), 2);
        assert_eq!(after_append.event_count, rebuilt.event_count + 1);
        assert_eq!(metadata.event_count, after_append.event_count);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn invisible_event_leaves_derived_manifest_stale_until_read() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("projection invisible".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "first".to_owned())
            .await
            .expect("message should append");
        let store = super::SessionEventStore::new(&root);
        store
            .reindex_session(session.id)
            .expect("session should reindex");
        let transcript_path = derived::transcript_index_path(&root, session.id);
        let input_history_path = derived::input_history_index_path(&root, session.id);
        let manifest_path = derived::manifest_path(&root, session.id);
        let transcript_before =
            std::fs::read_to_string(&transcript_path).expect("transcript index should read");
        let input_history_before =
            std::fs::read_to_string(&input_history_path).expect("input history index should read");
        let manifest_before: derived::DerivedIndexManifest = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path).expect("manifest should read"),
        )
        .expect("manifest should decode");

        manager
            .append_agent_changed(session.id, "build".to_owned())
            .await
            .expect("agent change should append");

        assert_eq!(
            transcript_before,
            std::fs::read_to_string(&transcript_path).expect("transcript index should read")
        );
        assert_eq!(
            input_history_before,
            std::fs::read_to_string(&input_history_path).expect("input history index should read")
        );
        let metadata =
            super::index::load_fresh_index(&root, session.id, &store.event_path(session.id))
                .expect("metadata index should load")
                .expect("metadata index should exist");
        let manifest_after: derived::DerivedIndexManifest = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path).expect("manifest should read"),
        )
        .expect("manifest should decode");
        assert_eq!(
            manifest_before.indexes[0].checkpoint.event_count,
            manifest_after.indexes[0].checkpoint.event_count
        );
        assert_eq!(
            manifest_before.indexes[1].checkpoint.event_count,
            manifest_after.indexes[1].checkpoint.event_count
        );
        assert_eq!(
            manifest_before.indexes[0].item_count,
            manifest_after.indexes[0].item_count
        );
        assert_eq!(
            manifest_before.indexes[1].item_count,
            manifest_after.indexes[1].item_count
        );
        let transcript_index =
            derived::ensure_transcript_index(&root, session.id, &store.event_path(session.id))
                .expect("transcript index should rebuild when stale");
        let manifest_rebuilt: derived::DerivedIndexManifest = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path).expect("manifest should read"),
        )
        .expect("manifest should decode");
        assert_eq!(metadata.event_count, transcript_index.event_count);
        assert_eq!(
            metadata.event_count,
            manifest_rebuilt.indexes[0].checkpoint.event_count
        );
        assert_eq!(
            manifest_before.indexes[1].checkpoint.event_count,
            manifest_rebuilt.indexes[1].checkpoint.event_count
        );
        assert_eq!(
            manifest_before.indexes[0].item_count,
            transcript_index.spans.len()
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn attach_does_not_fail_when_input_history_index_is_degraded() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("degraded input history".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "hello".to_owned())
            .await
            .expect("message should append");
        let store = super::SessionEventStore::new(&root);
        store
            .reindex_session(session.id)
            .expect("session should reindex");
        std::fs::remove_file(derived::input_history_index_path(&root, session.id))
            .expect("input history sidecar should remove");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let attachment = restored
            .attach_session_recent(session.id, ClientId::new(), 16)
            .await
            .expect("attach should tolerate degraded input history");

        assert!(attachment.input_history.is_empty());
        assert!(
            derived::ensure_input_history_index(&root, session.id, &store.event_path(session.id))
                .is_err()
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn streaming_deltas_do_not_rewrite_transcript_index() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("streaming".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "first".to_owned())
            .await
            .expect("message should append");
        let store = super::SessionEventStore::new(&root);
        store
            .reindex_session(session.id)
            .expect("session should reindex");
        let transcript_path = derived::transcript_index_path(&root, session.id);
        let transcript_before =
            std::fs::read_to_string(&transcript_path).expect("transcript index should read");

        manager
            .append_assistant_delta(session.id, "partial".to_owned())
            .await
            .expect("delta should append");
        manager
            .append_assistant_delta(session.id, " response".to_owned())
            .await
            .expect("delta should append");

        assert_eq!(
            transcript_before,
            std::fs::read_to_string(&transcript_path).expect("transcript index should read")
        );
        let rebuilt =
            derived::ensure_transcript_index(&root, session.id, &store.event_path(session.id))
                .expect("transcript index should rebuild");
        let assistant = rebuilt
            .spans
            .last()
            .expect("assistant span should be rebuilt from canonical events");
        assert_eq!(
            assistant.kind,
            bcode_session_models::TranscriptProjectionItemKind::AssistantMessage
        );
        assert!(
            !std::path::Path::new(&root)
                .join("index")
                .join(session.id.to_string())
                .join("dirty.json")
                .exists()
        );
        assert_eq!(assistant.content_bytes, "partial response".len());
        assert!(assistant.source_range.start_sequence < assistant.source_range.end_sequence);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn attach_session_projection_window_returns_selected_source_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("projection attach".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "older".to_owned())
            .await
            .expect("message should append");
        manager
            .append_assistant_message(session.id, "older answer".to_owned())
            .await
            .expect("message should append");
        manager
            .append_user_message(session.id, ClientId::new(), "newer".to_owned())
            .await
            .expect("message should append");
        manager
            .append_assistant_delta(session.id, "partial".to_owned())
            .await
            .expect("delta should append");
        manager
            .append_assistant_message(session.id, "newer answer".to_owned())
            .await
            .expect("message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let attached = restored
            .attach_session_projection_window(
                session.id,
                ClientId::new(),
                ProjectionWindowRequest {
                    projection: SessionProjectionKind::Transcript,
                    anchor: ProjectionWindowAnchor::Latest,
                    direction: ProjectionWindowDirection::Backward,
                    target: ProjectionWindowTarget {
                        min_items: Some(2),
                        min_estimated_rows: None,
                        min_bytes: None,
                        width_columns: Some(80),
                    },
                    limits: ProjectionWindowLimits {
                        max_items: 8,
                        max_events_scanned: 64,
                        max_bytes: 4096,
                    },
                },
            )
            .await
            .expect("projection attach should succeed");

        assert_eq!(attached.projection_window.transcript_items.len(), 2);
        assert_eq!(
            attached
                .attachment
                .history
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert!(matches!(
            &attached.attachment.history[0].kind,
            SessionEventKind::UserMessage { text, .. } if text == "newer"
        ));
        assert!(matches!(
            &attached.attachment.history[2].kind,
            SessionEventKind::AssistantMessage { text } if text == "newer answer"
        ));
        assert_eq!(
            attached.attachment.session.name.as_deref(),
            Some("projection attach")
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn projection_window_falls_back_when_index_has_no_transcript_entries() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("empty projection".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");

        let window = manager
            .session_projection_window(
                session.id,
                ProjectionWindowRequest {
                    projection: SessionProjectionKind::Transcript,
                    anchor: ProjectionWindowAnchor::Latest,
                    direction: ProjectionWindowDirection::Backward,
                    target: ProjectionWindowTarget {
                        min_items: Some(1),
                        min_estimated_rows: None,
                        min_bytes: None,
                        width_columns: Some(80),
                    },
                    limits: ProjectionWindowLimits {
                        max_items: 8,
                        max_events_scanned: 64,
                        max_bytes: 4096,
                    },
                },
            )
            .await
            .expect("fallback projection window should succeed");

        assert!(window.transcript_items.is_empty());
        assert_eq!(window.source_range, None);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn rebuilt_index_persists_transcript_projection_entries() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("projection-index".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "question".to_owned())
            .await
            .expect("message should append");
        manager
            .append_assistant_delta(session.id, "partial".to_owned())
            .await
            .expect("delta should append");
        manager
            .append_assistant_message(session.id, "answer".to_owned())
            .await
            .expect("message should append");

        let store = super::SessionEventStore::new(&root);
        store
            .reindex_session(session.id)
            .expect("session should reindex");
        let metadata =
            super::index::load_fresh_index(&root, session.id, &store.event_path(session.id))
                .expect("metadata index should load")
                .expect("metadata index should exist");
        let transcript_index =
            derived::ensure_transcript_index(&root, session.id, &store.event_path(session.id))
                .expect("transcript index should load");

        assert_eq!(metadata.event_count, transcript_index.event_count);
        assert_eq!(transcript_index.spans.len(), 2);
        assert_eq!(
            transcript_index.spans[0].kind,
            bcode_session_models::TranscriptProjectionItemKind::UserMessage
        );
        assert_eq!(
            transcript_index.spans[1].kind,
            bcode_session_models::TranscriptProjectionItemKind::AssistantMessage
        );
        assert_eq!(transcript_index.spans[1].source_range.start_sequence, 2);
        assert_eq!(transcript_index.spans[1].source_range.end_sequence, 3);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_session_events_range_reads_inclusive_sequences_from_disk_index() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("range".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        for index in 0..5 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let events = restored
            .session_events_range(session.id, 2, 4, 8)
            .await
            .expect("events range should load");

        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(matches!(
            &events[0].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 1"
        ));
        assert!(matches!(
            &events[2].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 3"
        ));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_session_events_range_respects_max_events_and_empty_ranges() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("range-limit".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        for index in 0..5 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let limited = restored
            .session_events_range(session.id, 1, 5, 2)
            .await
            .expect("events range should load");
        let empty = restored
            .session_events_range(session.id, 5, 1, 8)
            .await
            .expect("empty reversed range should load");

        assert_eq!(limited.len(), 2);
        assert_eq!(
            limited
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(empty.is_empty());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_session_history_page_reads_from_disk_index() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("paged".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        for index in 0..5 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let page = restored
            .session_history_page(
                session.id,
                SessionHistoryQuery {
                    cursor: None,
                    limit: 2,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await
            .expect("history page should load");

        assert_eq!(page.events.len(), 2);
        assert!(page.has_more);
        assert!(matches!(
            &page.events[0].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 3"
        ));
        assert!(matches!(
            &page.events[1].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 4"
        ));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn attach_session_recent_avoids_full_replay() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("recent".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        for index in 0..205 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let attachment = restored
            .attach_session_recent(session.id, ClientId::new(), 1)
            .await
            .expect("recent attach should succeed");

        assert_eq!(attachment.history.len(), 1);
        assert_eq!(attachment.session.name.as_deref(), Some("recent"));
        assert!(matches!(
            &attachment.history[0].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 204"
        ));
        assert_eq!(attachment.input_history.len(), 205);
        assert_eq!(
            attachment
                .input_history
                .first()
                .map(|entry| entry.text.as_str()),
            Some("message 0")
        );
        assert_eq!(
            attachment
                .input_history
                .last()
                .map(|entry| entry.text.as_str()),
            Some("message 204")
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn concurrent_same_session_appends_have_contiguous_sequences() {
        let root = unique_temp_dir();
        let manager = std::sync::Arc::new(
            SessionManager::persistent(&root).expect("manager should initialize"),
        );
        let session = manager
            .create_session(Some("concurrent".to_string()), test_working_directory())
            .await
            .expect("session should create");

        let mut tasks = Vec::new();
        for index in 0..16 {
            let manager = std::sync::Arc::clone(&manager);
            tasks.push(tokio::spawn(async move {
                manager
                    .append_event(
                        session.id,
                        SessionEventKind::SystemMessage {
                            text: format!("message {index}"),
                        },
                    )
                    .await
                    .expect("event should append")
            }));
        }

        let mut sequences = Vec::new();
        for task in tasks {
            sequences.push(task.await.expect("task should join").sequence);
        }
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=16).collect::<Vec<_>>());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn catalog_status_subscription_reports_loaded() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        manager
            .create_session(Some("catalog".to_string()), test_working_directory())
            .await
            .expect("session should create");

        let restored = SessionManager::persistent_lazy(&root);
        let mut status = restored.subscribe_catalog_status();
        assert_eq!(*status.borrow(), super::CatalogLoadStatus::NotStarted);
        restored.start_catalog_load();
        loop {
            if matches!(*status.borrow(), super::CatalogLoadStatus::Loaded) {
                break;
            }
            status.changed().await.expect("status should change");
        }
        assert_eq!(
            restored
                .cached_sessions(&test_working_directory())
                .await
                .len(),
            1
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn unnamed_session_uses_first_prompt_as_title() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(None, test_working_directory())
            .await
            .expect("session should be created");

        let events = manager
            .append_user_message(
                session.id,
                ClientId::new(),
                "# Fix session selection UX\n\nPlease make this nicer".to_string(),
            )
            .await
            .expect("message should append");

        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0].kind,
            SessionEventKind::SessionRenamed { name } if name.as_deref() == Some("Fix session selection UX")
        ));
        let sessions = manager.list_sessions(&test_working_directory()).await;
        assert_eq!(
            sessions[0].name.as_deref(),
            Some("Fix session selection UX")
        );

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let restored_sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(
            restored_sessions[0].name.as_deref(),
            Some("Fix session selection UX")
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn legacy_unnamed_session_index_derives_title_from_first_prompt() {
        let root = unique_temp_dir();
        let session_id = SessionId::new();
        let store = super::SessionEventStore::new(&root);
        std::fs::create_dir_all(&root).expect("session root should create");
        let event_path = root.join(format!("{session_id}.events"));
        let events = [
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 0,
                session_id,
                provenance: None,
                kind: SessionEventKind::SessionCreated {
                    name: None,
                    working_directory: test_working_directory(),
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::ClientAttached {
                    client_id: ClientId::new(),
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 2,
                session_id,
                provenance: None,
                kind: SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "# Recover old title\n\nbody".to_string(),
                },
            },
        ];
        let mut file = std::fs::File::create(&event_path).expect("event file should create");
        for event in events {
            write_legacy_event_payload(&mut file, &event);
        }

        assert_eq!(
            access_status_from_report(
                &reader::read_events(&event_path).expect("legacy encoding should read")
            ),
            SessionAccessStatus::ReadOnlyMigrationRequired
        );
        store
            .migrate_event_log_to_current(session_id)
            .expect("encoding migration should rewrite stable frames");
        let report = reader::read_events(&event_path).expect("migrated events should read");
        assert!(!report.needs_encoding_migration);

        store
            .doctor_session_with_fix(session_id, true)
            .expect("doctor should rebuild")
            .expect("session should exist");
        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].name.as_deref(), Some("Recover old title"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn explicit_session_name_is_not_replaced_by_first_prompt() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("Manual title".to_string()), test_working_directory())
            .await
            .expect("session should be created");

        let events = manager
            .append_user_message(session.id, ClientId::new(), "Different title".to_string())
            .await
            .expect("message should append");

        assert_eq!(events.len(), 1);
        let sessions = manager.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].name.as_deref(), Some("Manual title"));
    }

    #[tokio::test]
    async fn rename_session_restores_latest_name() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("Old title".to_string()), test_working_directory())
            .await
            .expect("session should be created");

        manager
            .rename_session(session.id, Some("  New   title  ".to_string()))
            .await
            .expect("session should rename");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].name.as_deref(), Some("New title"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn list_sessions_orders_by_latest_activity() {
        let manager = SessionManager::default();
        let older = manager
            .create_session(Some("older".to_string()), test_working_directory())
            .await
            .expect("older session should create");
        let newer = manager
            .create_session(Some("newer".to_string()), test_working_directory())
            .await
            .expect("newer session should create");

        let sessions = manager.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].id, newer.id);
        assert_eq!(sessions[1].id, older.id);

        manager
            .append_user_message(older.id, ClientId::new(), "wake older".to_string())
            .await
            .expect("message should append");

        let sessions = manager.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].id, older.id);
        assert_eq!(sessions[1].id, newer.id);
    }

    #[tokio::test]
    async fn restored_sessions_order_by_index_activity() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let older = manager
            .create_session(Some("older".to_string()), test_working_directory())
            .await
            .expect("older session should create");
        let newer = manager
            .create_session(Some("newer".to_string()), test_working_directory())
            .await
            .expect("newer session should create");

        manager
            .append_user_message(older.id, ClientId::new(), "wake older".to_string())
            .await
            .expect("message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].id, older.id);
        assert_eq!(sessions[1].id, newer.id);
        assert!(sessions[0].updated_at_ms >= sessions[0].created_at_ms);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn delete_session_removes_persisted_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("Delete me".to_string()), test_working_directory())
            .await
            .expect("session should be created");

        manager
            .delete_session(session.id)
            .await
            .expect("session should delete");

        assert!(
            manager
                .list_sessions(&test_working_directory())
                .await
                .is_empty()
        );
        let restored = SessionManager::persistent(&root).expect("manager should restore");
        assert!(
            restored
                .list_sessions(&test_working_directory())
                .await
                .is_empty()
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[test]
    fn built_in_session_migration_registry_is_valid() {
        let registry = super::SessionMigrationRegistry::builtin();
        registry
            .validate()
            .expect("built-in migration registry should be valid");
        assert!(
            registry
                .migration_for_action(super::SessionMigrationAction::RebuildDerivedIndex)
                .is_some()
        );
    }

    #[tokio::test]
    async fn session_doctor_is_read_only_without_fix() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("doctor".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let index_path = root
            .join("index")
            .join(format!("{}.index.json", session.id));
        std::fs::remove_file(&index_path).expect("index should remove");

        let store = super::SessionEventStore::new(&root);
        let health = store
            .doctor_session(session.id)
            .expect("doctor should inspect")
            .expect("session should exist");
        assert!(health.stale);
        assert!(!index_path.exists());

        store
            .doctor_session_with_fix(session.id, true)
            .expect("doctor fix should run");
        assert!(index_path.exists());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn lazy_persistent_manager_defers_catalog_until_requested() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("lazy catalog".to_string()), test_working_directory())
            .await
            .expect("session should create");

        let restored = SessionManager::persistent_lazy(&root);
        assert!(!restored.catalog_loaded());
        assert!(
            restored
                .cached_sessions(&test_working_directory())
                .await
                .is_empty()
        );

        let summary = restored
            .session_summary(session.id)
            .await
            .expect("targeted session load should work");
        assert_eq!(summary.name.as_deref(), Some("lazy catalog"));
        assert!(!restored.catalog_loaded());

        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert!(sessions.len() <= 1);
        restored
            .wait_catalog_loaded()
            .await
            .expect("catalog load should complete");
        let sessions = restored.cached_sessions(&test_working_directory()).await;
        assert_eq!(sessions.len(), 1);
        assert!(restored.catalog_loaded());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn lazy_catalog_includes_sessions_with_stale_indexes() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("stale catalog".to_string()), test_working_directory())
            .await
            .expect("session should create");
        manager
            .append_user_message(session.id, ClientId::new(), "wake index".to_string())
            .await
            .expect("message should append");
        let index_path = root
            .join("index")
            .join(format!("{}.index.json", session.id));
        std::fs::remove_file(&index_path).expect("index should remove");

        let restored = SessionManager::persistent_lazy(&root);
        restored
            .wait_catalog_loaded()
            .await
            .expect("catalog load should complete");
        let sessions = restored.cached_sessions(&test_working_directory()).await;
        let restored_session = sessions
            .iter()
            .find(|candidate| candidate.id == session.id)
            .expect("stale-index session should appear in catalog");
        assert_eq!(restored_session.name.as_deref(), Some("stale catalog"));

        restored
            .ensure_session_current(session.id)
            .await
            .expect("stale-index session should migrate successfully");
        let summary = restored
            .session_summary(session.id)
            .await
            .expect("session should remain loadable after migration check");
        assert_eq!(summary.name.as_deref(), Some("stale catalog"));
        let health = super::SessionEventStore::new(&root)
            .doctor_session_with_fix(session.id, true)
            .expect("doctor should inspect")
            .expect("session should exist");
        assert!(health.stale);
        assert!(index_path.exists());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn persistent_restore_defers_stale_index_repair_until_explicit_reindex() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("lazy".to_string()), test_working_directory())
            .await
            .expect("session should create");
        manager
            .append_user_message(session.id, ClientId::new(), "hello".to_string())
            .await
            .expect("message should append");
        let index_path = root
            .join("index")
            .join(format!("{}.index.json", session.id));
        std::fs::remove_file(&index_path).expect("index should remove");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        assert!(
            !index_path.exists(),
            "persistent restore should not eagerly rewrite missing indexes"
        );

        restored
            .session_history_page(
                session.id,
                SessionHistoryQuery {
                    cursor: None,
                    limit: 10,
                    direction: SessionHistoryDirection::Forward,
                },
            )
            .await
            .expect_err("history page should require explicit repair for missing metadata index");
        assert!(
            !index_path.exists(),
            "history page should not rebuild missing indexes implicitly"
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn session_migration_apply_rebuilds_missing_index() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("migration".to_string()), test_working_directory())
            .await
            .expect("session should create");
        std::fs::remove_file(
            root.join("index")
                .join(format!("{}.index.json", session.id)),
        )
        .expect("index should remove");

        let store = super::SessionEventStore::new(&root);
        let plan = store.migration_plan().expect("plan should load");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].migration_id, "sessions-index-rebuild-v2");

        let report = store
            .apply_migration_plan(super::SessionMigrationOptions {
                dry_run: false,
                backup: false,
            })
            .expect("migration should apply");
        assert_eq!(report.items.len(), 1);
        assert_eq!(
            report.items[0].status,
            super::SessionMigrationApplyStatus::Applied
        );
        assert!(
            root.join("index")
                .join(format!("{}.index.json", session.id))
                .exists()
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn session_migration_apply_writes_journal_entries() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("journal".to_string()), test_working_directory())
            .await
            .expect("session should create");
        std::fs::remove_file(
            root.join("index")
                .join(format!("{}.index.json", session.id)),
        )
        .expect("index should remove");

        let store = super::SessionEventStore::new(&root);
        store
            .apply_migration_plan(super::SessionMigrationOptions {
                dry_run: false,
                backup: false,
            })
            .expect("migration should apply");

        let journal = std::fs::read_to_string(root.join("migrations.jsonl"))
            .expect("journal should be written");
        assert!(journal.contains("\"status\":\"started\""));
        assert!(journal.contains("\"status\":\"completed\""));
        assert!(journal.contains("sessions-index-rebuild-v2"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn session_migration_apply_can_create_backup_manifest() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("backup".to_string()), test_working_directory())
            .await
            .expect("session should create");
        std::fs::remove_file(
            root.join("index")
                .join(format!("{}.index.json", session.id)),
        )
        .expect("index should remove");

        let store = super::SessionEventStore::new(&root);
        let report = store
            .apply_migration_plan(super::SessionMigrationOptions {
                dry_run: false,
                backup: true,
            })
            .expect("migration should apply");
        let backup_dir = report.backup_dir.expect("backup should be created");
        assert!(backup_dir.join("manifest.json").exists());
        assert!(backup_dir.join(format!("{}.events", session.id)).exists());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[allow(clippy::too_many_lines)]
    fn session_event_kind_tag_cases() -> Vec<(u32, &'static str, SessionEventKind)> {
        let client_id = ClientId::new();
        let skill_id = SkillId::new("compatibility-test");
        vec![
            (
                0,
                "SessionCreated",
                SessionEventKind::SessionCreated {
                    name: Some("created".to_string()),
                    working_directory: test_working_directory(),
                },
            ),
            (
                1,
                "ClientAttached",
                SessionEventKind::ClientAttached { client_id },
            ),
            (
                2,
                "ClientDetached",
                SessionEventKind::ClientDetached { client_id },
            ),
            (
                3,
                "UserMessage",
                SessionEventKind::UserMessage {
                    client_id,
                    text: "user".to_string(),
                },
            ),
            (
                4,
                "AssistantDelta",
                SessionEventKind::AssistantDelta {
                    text: "delta".to_string(),
                },
            ),
            (
                5,
                "AssistantMessage",
                SessionEventKind::AssistantMessage {
                    text: "message".to_string(),
                },
            ),
            (
                6,
                "ToolCallRequested",
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call".to_string(),
                    tool_name: "tool".to_string(),
                    arguments_json: "{}".to_string(),
                },
            ),
            (
                7,
                "ToolCallFinished",
                SessionEventKind::ToolCallFinished {
                    tool_call_id: "call".to_string(),
                    result: "ok".to_string(),
                    is_error: false,
                    output: None,
                },
            ),
            (
                8,
                "PermissionRequested",
                SessionEventKind::PermissionRequested {
                    permission_id: "permission".to_string(),
                    tool_call_id: "call".to_string(),
                    tool_name: "tool".to_string(),
                    arguments_json: "{}".to_string(),
                },
            ),
            (
                9,
                "PermissionResolved",
                SessionEventKind::PermissionResolved {
                    permission_id: "permission".to_string(),
                    approved: true,
                },
            ),
            (
                10,
                "ModelChanged",
                SessionEventKind::ModelChanged {
                    provider: "provider".to_string(),
                    model: "model".to_string(),
                },
            ),
            (
                11,
                "SystemMessage",
                SessionEventKind::SystemMessage {
                    text: "system".to_string(),
                },
            ),
            (
                12,
                "AgentChanged",
                SessionEventKind::AgentChanged {
                    agent_id: "build".to_string(),
                },
            ),
            (
                13,
                "ModelTurnStarted",
                SessionEventKind::ModelTurnStarted {
                    turn_id: "turn".to_string(),
                },
            ),
            (
                14,
                "ModelTurnFinished",
                SessionEventKind::ModelTurnFinished {
                    turn_id: "turn".to_string(),
                    outcome: bcode_session_models::ModelTurnOutcome::Completed,
                    message: None,
                },
            ),
            (
                15,
                "ModelUsage",
                SessionEventKind::ModelUsage {
                    turn_id: "turn".to_string(),
                    usage: bcode_session_models::SessionTokenUsage {
                        input_tokens: Some(1),
                        output_tokens: Some(2),
                        total_tokens: Some(3),
                        cached_input_tokens: None,
                        cache_write_input_tokens: None,
                        reasoning_tokens: None,
                    },
                },
            ),
            (
                16,
                "ContextCompacted",
                SessionEventKind::ContextCompacted {
                    summary: "summary".to_string(),
                    compacted_through_sequence: 1,
                },
            ),
            (
                17,
                "SessionRenamed",
                SessionEventKind::SessionRenamed {
                    name: Some("renamed".to_string()),
                },
            ),
            (
                18,
                "TraceEvent",
                SessionEventKind::TraceEvent {
                    trace: Box::new(SessionTraceEvent {
                        timestamp_ms: 1,
                        turn_id: None,
                        phase: SessionTracePhase::ModelProviderEvent,
                        payload: SessionTracePayload::ProviderEvent {
                            event_type: "event".to_string(),
                            detail: None,
                        },
                    }),
                },
            ),
            (
                19,
                "SkillInvoked",
                SessionEventKind::SkillInvoked {
                    skill_id: skill_id.clone(),
                    arguments: String::new(),
                    source: None,
                    invoked_at_ms: 1,
                },
            ),
            (
                20,
                "SkillSuggested",
                SessionEventKind::SkillSuggested {
                    skill_id: skill_id.clone(),
                    reason: None,
                    suggested_at_ms: 1,
                },
            ),
            (
                21,
                "SkillActivated",
                SessionEventKind::SkillActivated {
                    skill_id: skill_id.clone(),
                    source: None,
                    mode: SkillActivationMode::Explicit,
                    activated_at_ms: 1,
                },
            ),
            (
                22,
                "SkillDeactivated",
                SessionEventKind::SkillDeactivated {
                    skill_id: skill_id.clone(),
                    deactivated_at_ms: 1,
                },
            ),
            (
                23,
                "SkillContextLoaded",
                SessionEventKind::SkillContextLoaded {
                    skill_id: skill_id.clone(),
                    bytes_loaded: 1,
                    truncated: false,
                    loaded_at_ms: 1,
                },
            ),
            (
                24,
                "SkillInvocationFailed",
                SessionEventKind::SkillInvocationFailed {
                    skill_id,
                    error: "error".to_string(),
                    failed_at_ms: 1,
                },
            ),
            (
                25,
                "AssistantReasoningDelta",
                SessionEventKind::AssistantReasoningDelta {
                    text: "reasoning".to_string(),
                },
            ),
            (
                26,
                "AssistantReasoningMessage",
                SessionEventKind::AssistantReasoningMessage {
                    text: "reasoning".to_string(),
                },
            ),
            (
                27,
                "RuntimeWorkStarted",
                SessionEventKind::RuntimeWorkStarted {
                    work_id: RuntimeWorkId::new("work"),
                    kind: RuntimeWorkKind::Tool,
                    label: "tool".to_string(),
                    tool_call_id: Some("call".to_string()),
                    plugin_id: Some("plugin".to_string()),
                    service_interface: Some("service".to_string()),
                    operation: Some("invoke".to_string()),
                    parent_work_id: None,
                    started_at_ms: Some(1),
                    cancellable: true,
                },
            ),
            (
                28,
                "RuntimeWorkCancelRequested",
                SessionEventKind::RuntimeWorkCancelRequested {
                    work_id: RuntimeWorkId::new("work"),
                    requested_at_ms: Some(2),
                    client_id: Some(client_id),
                },
            ),
            (
                29,
                "RuntimeWorkFinished",
                SessionEventKind::RuntimeWorkFinished {
                    work_id: RuntimeWorkId::new("work"),
                    status: RuntimeWorkStatus::Completed,
                    finished_at_ms: Some(3),
                    message: None,
                },
            ),
            (
                30,
                "RuntimeWorkProgress",
                SessionEventKind::RuntimeWorkProgress {
                    work_id: RuntimeWorkId::new("work"),
                    message: "progress".to_string(),
                    progress_at_ms: Some(4),
                    completed_units: Some(1),
                    total_units: Some(2),
                },
            ),
            (
                31,
                "ModelTurnCancelRequested",
                SessionEventKind::ModelTurnCancelRequested {
                    turn_id: "turn".to_string(),
                    requested_at_ms: Some(4),
                    client_id: Some(client_id),
                },
            ),
            (
                32,
                "ToolInvocationStream",
                SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::OutputDelta {
                        tool_call_id: "call".to_string(),
                        stream: ToolOutputStream::Stdout,
                        sequence: 1,
                        text: "output".to_string(),
                        byte_len: 6,
                    },
                },
            ),
            (
                33,
                "WorkingDirectoryChanged",
                SessionEventKind::WorkingDirectoryChanged {
                    old_working_directory: test_working_directory(),
                    new_working_directory: test_working_directory().join("worktree"),
                },
            ),
            (
                34,
                "SessionImported",
                SessionEventKind::SessionImported {
                    source_id: "pi".to_string(),
                    source_display_name: "Pi".to_string(),
                    external_session_id: "external".to_string(),
                    imported_at_ms: 1,
                },
            ),
        ]
    }

    fn session_trace_phase_tag_cases() -> Vec<(u32, &'static str, SessionTracePhase)> {
        vec![
            (0, "ModelRequestBuilt", SessionTracePhase::ModelRequestBuilt),
            (
                1,
                "ModelProviderRoundStarted",
                SessionTracePhase::ModelProviderRoundStarted,
            ),
            (
                2,
                "ModelProviderRoundFinished",
                SessionTracePhase::ModelProviderRoundFinished,
            ),
            (
                3,
                "ModelProviderEvent",
                SessionTracePhase::ModelProviderEvent,
            ),
            (
                4,
                "ToolInvocationStarted",
                SessionTracePhase::ToolInvocationStarted,
            ),
            (
                5,
                "ToolPolicyEvaluated",
                SessionTracePhase::ToolPolicyEvaluated,
            ),
            (
                6,
                "ToolPermissionWaitStarted",
                SessionTracePhase::ToolPermissionWaitStarted,
            ),
            (
                7,
                "ToolPermissionWaitFinished",
                SessionTracePhase::ToolPermissionWaitFinished,
            ),
            (
                8,
                "ToolInvocationFinished",
                SessionTracePhase::ToolInvocationFinished,
            ),
            (9, "SkillInvoked", SessionTracePhase::SkillInvoked),
            (10, "SkillSuggested", SessionTracePhase::SkillSuggested),
            (11, "SkillActivated", SessionTracePhase::SkillActivated),
            (12, "SkillDeactivated", SessionTracePhase::SkillDeactivated),
            (
                13,
                "SkillContextLoaded",
                SessionTracePhase::SkillContextLoaded,
            ),
            (
                14,
                "SkillInvocationFailed",
                SessionTracePhase::SkillInvocationFailed,
            ),
            (
                15,
                "ContextCompactionSkipped",
                SessionTracePhase::ContextCompactionSkipped,
            ),
            (
                16,
                "ContextCompactionStarted",
                SessionTracePhase::ContextCompactionStarted,
            ),
            (
                17,
                "ContextCompactionFinished",
                SessionTracePhase::ContextCompactionFinished,
            ),
            (
                18,
                "ToolInvocationOutput",
                SessionTracePhase::ToolInvocationOutput,
            ),
        ]
    }

    #[allow(clippy::too_many_lines)]
    fn session_trace_payload_tag_cases() -> Vec<(u32, &'static str, SessionTracePayload)> {
        let mut metadata = BTreeMap::new();
        metadata.insert("conversation_hash".to_string(), "abc123".to_string());
        vec![
            (
                0,
                "ModelRequestBuilt",
                SessionTracePayload::ModelRequestBuilt {
                    provider: "provider".to_string(),
                    model: "model".to_string(),
                    agent_id: "build".to_string(),
                    message_count: 1,
                    tool_count: 2,
                    system_prompt_chars: 3,
                    prompt_cache_mode: "auto".to_string(),
                    conversation_reuse_mode: "auto".to_string(),
                    uses_previous_provider_response: false,
                    metadata,
                    request: None,
                },
            ),
            (
                1,
                "ProviderRound",
                SessionTracePayload::ProviderRound {
                    provider_turn_id: Some("provider-turn".to_string()),
                    provider: "provider".to_string(),
                    round: Some(1),
                    stop_reason: Some("stop".to_string()),
                    duration_ms: Some(42),
                    error: None,
                },
            ),
            (
                2,
                "ProviderEvent",
                SessionTracePayload::ProviderEvent {
                    event_type: "event".to_string(),
                    detail: Some("detail".to_string()),
                },
            ),
            (
                3,
                "ToolInvocationStarted",
                SessionTracePayload::ToolInvocationStarted {
                    tool_call_id: "call".to_string(),
                    plugin_id: "plugin".to_string(),
                    tool_name: "tool".to_string(),
                    side_effect: "read_only".to_string(),
                    requires_permission: false,
                    arguments: None,
                },
            ),
            (
                4,
                "ToolPolicyEvaluated",
                SessionTracePayload::ToolPolicyEvaluated {
                    tool_call_id: "call".to_string(),
                    agent_id: "build".to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                },
            ),
            (
                5,
                "ToolPermissionWait",
                SessionTracePayload::ToolPermissionWait {
                    permission_id: "permission".to_string(),
                    tool_call_id: "call".to_string(),
                    approved: Some(true),
                    duration_ms: Some(7),
                },
            ),
            (
                6,
                "ToolInvocationFinished",
                SessionTracePayload::ToolInvocationFinished {
                    tool_call_id: "call".to_string(),
                    duration_ms: 9,
                    is_error: false,
                    output_bytes: 12,
                    output: None,
                },
            ),
            (
                7,
                "ContextCompaction",
                SessionTracePayload::ContextCompaction {
                    reason: "manual".to_string(),
                    projected_context_chars: 123,
                    compacted: true,
                    message: None,
                },
            ),
            (
                8,
                "ProviderStreamEvent",
                SessionTracePayload::ProviderStreamEvent(ProviderStreamEvent::TurnStarted),
            ),
            (
                9,
                "ToolInvocationStreamEvent",
                SessionTracePayload::ToolInvocationStreamEvent(
                    ToolInvocationStreamEvent::OutputDelta {
                        tool_call_id: "call".to_string(),
                        stream: ToolOutputStream::Stdout,
                        sequence: 1,
                        text: "output".to_string(),
                        byte_len: 6,
                    },
                ),
            ),
        ]
    }

    fn encoded_variant_tag(value: &impl Serialize) -> u32 {
        let bytes = bmux_codec::to_positional_vec(value).expect("value should encode");
        let (tag, _) = bmux_codec::varint::decode_u32(&bytes).expect("variant tag should decode");
        tag
    }

    fn stable_order_binary_fixture_events() -> Vec<SessionEvent> {
        let session_id = "11111111-1111-4111-8111-111111111111"
            .parse()
            .expect("fixture session id should parse");
        vec![
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 0,
                session_id,
                provenance: None,
                kind: SessionEventKind::SessionCreated {
                    name: Some("stable-order".to_string()),
                    working_directory: test_working_directory(),
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::AssistantDelta {
                    text: "partial".to_string(),
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 2,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolCallRequested {
                    tool_call_id: "call".to_string(),
                    tool_name: "read".to_string(),
                    arguments_json: r#"{"path":"README.md"}"#.to_string(),
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 3,
                session_id,
                provenance: None,
                kind: SessionEventKind::SkillInvocationFailed {
                    skill_id: SkillId::new("fixture"),
                    error: "failed".to_string(),
                    failed_at_ms: 1,
                },
            },
        ]
    }

    fn stable_order_binary_fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("fixtures/session-events/binary/stable-order-v9.events")
    }

    fn write_legacy_event(path: &std::path::Path, event: &SessionEvent) {
        let mut file = std::fs::File::create(path).expect("event file should create");
        write_legacy_event_payload(&mut file, event);
    }

    fn append_legacy_event(path: &std::path::Path, event: &SessionEvent) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("event file should open");
        write_legacy_event_payload(&mut file, event);
    }

    fn append_invalid_legacy_payload(file: &mut std::fs::File) {
        let payload = [0xff_u8, 0x00, 0x01];
        file.write_all(
            &u32::try_from(payload.len())
                .expect("payload should fit")
                .to_le_bytes(),
        )
        .expect("invalid len should write");
        file.write_all(&payload)
            .expect("invalid payload should write");
    }

    fn write_legacy_event_payload(file: &mut std::fs::File, event: &SessionEvent) {
        let payload = bmux_codec::to_positional_vec(event).expect("legacy event should encode");
        file.write_all(
            &u32::try_from(payload.len())
                .expect("payload should fit")
                .to_le_bytes(),
        )
        .expect("legacy len should write");
        file.write_all(&payload)
            .expect("legacy payload should write");
    }

    fn test_working_directory() -> std::path::PathBuf {
        "/tmp/bcode-session-test-working-directory".into()
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let counter = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bcode-session-test-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }
}
