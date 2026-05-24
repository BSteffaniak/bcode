#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Session mutations intentionally hold the manager lock while updating in-memory
// state and appending the corresponding event so summaries/history/fanout stay
// consistent in this first implementation.
#![allow(clippy::significant_drop_tightening)]

//! Session lifecycle, attachment management, and append-only event history.

pub(crate) mod event_migration;
pub(crate) mod index;
pub mod migration;
pub(crate) mod reader;

pub use index::{SessionIndexHealth, SessionIndexStatus};
pub use migration::{
    SessionEventLogMigration, SessionEventLogMigrationError, SessionMigrationAction,
    SessionMigrationApplyPolicy, SessionMigrationApplyStatus, SessionMigrationBackupPolicy,
    SessionMigrationDefinition, SessionMigrationJournalEntry, SessionMigrationJournalStatus,
    SessionMigrationOptions, SessionMigrationPlan, SessionMigrationPlanItem,
    SessionMigrationRecoveryItem, SessionMigrationRecoveryStatus, SessionMigrationRegistry,
    SessionMigrationRegistryError, SessionMigrationReport, SessionMigrationReportItem,
};

use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ModelTurnOutcome, SessionEvent,
    SessionEventKind, SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage,
    SessionHistoryQuery, SessionId, SessionInputHistoryEntry, SessionSummary, SessionTokenUsage,
    SessionTraceEvent,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Seek as _, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

const FRAME_V2_MAGIC: &[u8; 4] = b"BSE2";
const FRAME_V2_VERSION: u16 = 2;

/// Errors returned by session management operations.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(SessionId),
    #[error("session event store error: {0}")]
    Store(#[from] SessionStoreError),
    #[error("session has connected clients: {0}")]
    ConnectedClients(SessionId),
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
    #[error("session migration registry error: {0}")]
    MigrationRegistry(#[from] SessionMigrationRegistryError),
}

/// Append-only event store for session histories.
#[derive(Debug, Clone)]
pub struct SessionEventStore {
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionMigrationBackupManifest {
    created_at_ms: u64,
    domain: &'static str,
    files: Vec<SessionMigrationBackupFile>,
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
    file.write_all(FRAME_V2_MAGIC)?;
    file.write_all(&FRAME_V2_VERSION.to_le_bytes())?;
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
        Self { root: root.into() }
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
            if let Some(index) = index::load_fresh_index(&self.root, session_id, &path)? {
                sessions.insert(session_id, index.into_state());
            }
        }

        Ok(sessions)
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
        if let Some(index) = index::load_fresh_index(&self.root, session_id, &path)? {
            return Ok(Some(index.into_state()));
        }
        let Some(index) = index::rebuild_index_metadata(&self.root, session_id, &path)? else {
            return Ok(None);
        };
        let mut state = index.into_state();
        state.index_status = SessionIndexStatusKind::Stale;
        state.access_status = self.inspect_access_status(session_id)?;
        Ok(Some(state))
    }

    fn append(&self, event: &SessionEvent) -> Result<index::SessionIndexEntry, SessionStoreError> {
        fs::create_dir_all(&self.root)?;
        let path = self.event_path(event.session_id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let offset = file.seek(SeekFrom::End(0))?;
        let frame_len = write_event_frame(&mut file, event)?;
        file.flush()?;
        let entry = index::SessionIndexEntry::from_event(event, offset, frame_len);
        if let Err(error) = index::append_entry(&self.root, event.session_id, &entry) {
            eprintln!(
                "failed to update session entry index for {}: {error}",
                event.session_id
            );
        }
        Ok(entry)
    }

    fn read_session_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let path = self.event_path(session_id);
        Ok(reader::read_events(&path)?.events)
    }

    fn inspect_access_status(
        &self,
        session_id: SessionId,
    ) -> Result<SessionAccessStatus, SessionStoreError> {
        let path = self.event_path(session_id);
        let report = reader::read_events(&path)?;
        Ok(access_status_from_report(&report))
    }

    fn ensure_fresh_index(
        &self,
        session_id: SessionId,
    ) -> Result<index::SessionIndex, SessionStoreError> {
        let event_path = self.event_path(session_id);
        match index::load_fresh_index(&self.root, session_id, &event_path)? {
            Some(index) => Ok(index),
            None => index::rebuild_index(&self.root, session_id, &event_path)?
                .0
                .ok_or_else(|| {
                    SessionStoreError::InvalidSessionId(format!("empty session log: {session_id}"))
                }),
        }
    }

    fn read_session_history_page(
        &self,
        session_id: SessionId,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionStoreError> {
        let event_path = self.event_path(session_id);
        let index = self.ensure_fresh_index(session_id)?;
        let entries = match index::read_entries(&self.root, session_id) {
            Ok(entries) if entries.len() == index.event_count => entries,
            _ => {
                let _ = index::rebuild_index(&self.root, session_id, &event_path)?;
                index::read_entries(&self.root, session_id)?
            }
        };
        let limit = query.limit.max(1);
        let (page_entries, mut has_more) = select_history_page_entries(entries, query, limit);
        let mut events = read_indexed_events(&event_path, &page_entries);
        if events.is_err() {
            let _ = index::rebuild_index(&self.root, session_id, &event_path)?;
            let rebuilt_index = self.ensure_fresh_index(session_id)?;
            let rebuilt_entries = index::read_entries(&self.root, session_id)?;
            if rebuilt_entries.len() != rebuilt_index.event_count {
                return Err(SessionStoreError::InvalidSessionId(format!(
                    "rebuilt session index entry count mismatch for {session_id}"
                )));
            }
            let (rebuilt_page_entries, rebuilt_has_more) =
                select_history_page_entries(rebuilt_entries, query, limit);
            has_more = rebuilt_has_more;
            events = read_indexed_events(&event_path, &rebuilt_page_entries);
        }
        let events = events?;
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
        Ok(SessionHistoryPage {
            session_id,
            events,
            next_cursor,
            has_more,
        })
    }

    fn read_session_input_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionInputHistoryEntry>, SessionStoreError> {
        let event_path = self.event_path(session_id);
        let index =
            if let Some(index) = index::load_fresh_index(&self.root, session_id, &event_path)? {
                index
            } else {
                let (index, events) = index::rebuild_index(&self.root, session_id, &event_path)?;
                let Some(index) = index else {
                    return Ok(input_history_from_events(&events));
                };
                index
            };
        let entries = match index::read_entries(&self.root, session_id) {
            Ok(entries) if entries.len() == index.event_count => entries,
            _ => {
                let (_, events) = index::rebuild_index(&self.root, session_id, &event_path)?;
                return Ok(input_history_from_events(&events));
            }
        };
        let mut input_history = Vec::new();
        for entry in entries {
            if entry.kind != "user_message" {
                continue;
            }
            let event = reader::read_event_at(&event_path, entry.offset)?;
            if let SessionEventKind::UserMessage { text, .. } = event.kind {
                input_history.push(SessionInputHistoryEntry {
                    sequence: event.sequence,
                    text,
                });
            }
        }
        Ok(input_history)
    }

    fn read_model_context_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let event_path = self.event_path(session_id);
        let index = match index::load_fresh_index(&self.root, session_id, &event_path)? {
            Some(index) => index,
            None => index::rebuild_index(&self.root, session_id, &event_path)?
                .0
                .ok_or_else(|| {
                    SessionStoreError::InvalidSessionId(format!("empty session log: {session_id}"))
                })?,
        };
        let mut entries = match index::read_entries(&self.root, session_id) {
            Ok(entries) if entries.len() == index.event_count => entries,
            _ => {
                let _ = index::rebuild_index(&self.root, session_id, &event_path)?;
                index::read_entries(&self.root, session_id)?
            }
        };
        entries.sort_by_key(|entry| entry.sequence);
        let Some(compaction_entry) = entries
            .iter()
            .rev()
            .find(|entry| entry.kind == "context_compacted")
        else {
            return self.read_session_events(session_id);
        };
        let compaction_event = reader::read_event_at(&event_path, compaction_entry.offset)?;
        let compacted_through_sequence = match &compaction_event.kind {
            SessionEventKind::ContextCompacted {
                compacted_through_sequence,
                ..
            } => *compacted_through_sequence,
            _ => return self.read_session_events(session_id),
        };
        let mut events = vec![compaction_event];
        for entry in entries
            .iter()
            .filter(|entry| entry.sequence > compacted_through_sequence)
        {
            if entry.sequence == compaction_entry.sequence {
                continue;
            }
            events.push(reader::read_event_at(&event_path, entry.offset)?);
        }
        Ok(events)
    }

    fn write_state_index(&self, state: &SessionState) -> Result<(), SessionStoreError> {
        let path = self.event_path(state.summary.id);
        let file = index::fingerprint(&path)?;
        let index = index::SessionIndex {
            index_version: index::SESSION_INDEX_VERSION,
            session_id: state.summary.id,
            last_good_offset: file.len,
            file,
            summary: SessionSummary {
                client_count: 0,
                ..state.summary.clone()
            },
            working_directory: state.working_directory.clone(),
            next_sequence: state.next_sequence,
            event_count: state.event_count,
            created_at_ms: state.summary.created_at_ms,
            updated_at_ms: state.summary.updated_at_ms,
            has_user_message: state.has_user_message,
            current_provider: state.current_provider.clone(),
            current_model: state.current_model.clone(),
            current_agent: state.current_agent.clone(),
            latest_compaction_sequence: state.latest_compaction_sequence,
            total_metered_tokens: state.total_metered_tokens,
            min_event_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            max_event_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            issues: state.index_issues.clone(),
        };
        index::write_index(&self.root, &index)
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
        let path = self.event_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        if let Some(index) = index::load_fresh_index(&self.root, session_id, &path)? {
            return Ok(Some(index.health(false)));
        }
        if fix {
            let (index, _) = index::rebuild_index(&self.root, session_id, &path)?;
            Ok(index.map(|index| index.health(true)))
        } else {
            Ok(
                index::rebuild_index_metadata(&self.root, session_id, &path)?
                    .map(|index| index.health(true)),
            )
        }
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
            if let Some(item) = self.doctor_session_with_fix(session_id, fix)? {
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

    pub(crate) fn event_path(&self, session_id: SessionId) -> PathBuf {
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
    entries
        .iter()
        .map(|entry| reader::read_event_at(event_path, entry.offset))
        .collect()
}

/// In-memory session manager with optional append-only persistence.
#[derive(Debug, Default)]
pub struct SessionManager {
    inner: Mutex<SessionManagerInner>,
    store: Option<SessionEventStore>,
}

#[derive(Debug, Default)]
struct SessionManagerInner {
    sessions: BTreeMap<SessionId, SessionState>,
    catalog_loaded: bool,
    activity_clock_ms: u64,
    index_rebuilds: BTreeMap<SessionId, JoinHandle<()>>,
    completed_rebuilds: usize,
    failed_rebuilds: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIndexStatusKind {
    Current,
    Stale,
}

/// Background session maintenance status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SessionMaintenanceStatus {
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
    pub history: Vec<SessionEvent>,
    pub input_history: Vec<SessionInputHistoryEntry>,
    pub attached_event: SessionEvent,
    pub events: broadcast::Receiver<SessionEvent>,
}

impl SessionManager {
    /// Create a session manager backed by an append-only event store.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent(root: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let store = SessionEventStore::new(root);
        store.migrate_all_event_logs_to_current()?;
        let sessions = store.load_sessions()?;
        Ok(Self::from_store(store, sessions, true))
    }

    /// Create a session manager whose catalog and event logs are loaded on demand.
    #[must_use]
    pub fn persistent_lazy(root: impl Into<PathBuf>) -> Self {
        let store = SessionEventStore::new(root);
        Self::from_store(store, BTreeMap::new(), false)
    }

    fn from_store(
        store: SessionEventStore,
        sessions: BTreeMap<SessionId, SessionState>,
        catalog_loaded: bool,
    ) -> Self {
        Self {
            inner: Mutex::new(SessionManagerInner {
                sessions,
                catalog_loaded,
                activity_clock_ms: current_unix_millis(),
                index_rebuilds: BTreeMap::new(),
                completed_rebuilds: 0,
                failed_rebuilds: 0,
            }),
            store: Some(store),
        }
    }

    async fn ensure_session_loaded(&self, session_id: SessionId) -> Result<(), SessionError> {
        if self.inner.lock().await.sessions.contains_key(&session_id) {
            return Ok(());
        }
        let Some(store) = &self.store else {
            return Err(SessionError::NotFound(session_id));
        };
        let Some(state) = store.load_session(session_id)? else {
            return Err(SessionError::NotFound(session_id));
        };
        let mut inner = self.inner.lock().await;
        inner.sessions.entry(session_id).or_insert(state);
        Ok(())
    }

    async fn ensure_catalog_loaded(&self) -> Result<(), SessionStoreError> {
        if self.inner.lock().await.catalog_loaded {
            return Ok(());
        }
        let Some(store) = &self.store else {
            self.inner.lock().await.catalog_loaded = true;
            return Ok(());
        };
        let sessions = store.load_catalog()?;
        let mut inner = self.inner.lock().await;
        for (session_id, state) in sessions {
            inner.sessions.entry(session_id).or_insert(state);
        }
        inner.catalog_loaded = true;
        Ok(())
    }

    async fn migrate_session_to_current_if_required(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let should_migrate = {
            let inner = self.inner.lock().await;
            let state = inner
                .sessions
                .get(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            match state.access_status {
                SessionAccessStatus::ReadWrite => false,
                SessionAccessStatus::ReadOnlyMigrationRequired => true,
                status => {
                    return Err(SessionError::NotWritable { session_id, status });
                }
            }
        };
        if !should_migrate {
            return Ok(());
        }
        let Some(store) = &self.store else {
            return Ok(());
        };
        store.migrate_event_log_to_current(session_id)?;
        let index = store.ensure_fresh_index(session_id)?;
        let mut inner = self.inner.lock().await;
        let clients = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?
            .clients
            .clone();
        let mut state = SessionState::from_index(index);
        state.clients = clients;
        state.summary.client_count = state.clients.len();
        inner.sessions.insert(session_id, state);
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
        let mut inner = self.inner.lock().await;
        let id = SessionId::new();
        let (sender, _) = broadcast::channel(512);
        let now_ms = inner.next_activity_timestamp_ms();
        let summary = SessionSummary {
            id,
            name: name.clone(),
            client_count: 0,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            working_directory: working_directory.clone(),
        };
        let mut state = SessionState {
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
        state.push_event(
            SessionEventKind::SessionCreated {
                name,
                working_directory,
            },
            self.store.as_ref(),
            now_ms,
        )?;
        inner.sessions.insert(id, state);
        Ok(summary)
    }

    /// List known sessions from the session catalog.
    pub async fn list_sessions(&self, working_directory: &Path) -> Vec<SessionSummary> {
        if let Err(error) = self.ensure_catalog_loaded().await {
            eprintln!("failed to load session catalog: {error}");
        }
        self.cached_sessions(working_directory).await
    }

    /// List already-loaded sessions without touching persistent storage.
    pub async fn cached_sessions(&self, working_directory: &Path) -> Vec<SessionSummary> {
        let working_directory = normalize_working_directory(working_directory);
        let inner = self.inner.lock().await;
        sorted_session_summaries(&inner.sessions, &working_directory)
    }

    /// Return true once the persistent session catalog has been discovered.
    pub async fn catalog_loaded(&self) -> bool {
        self.inner.lock().await.catalog_loaded
    }

    /// Return background maintenance status.
    pub async fn maintenance_status(&self) -> SessionMaintenanceStatus {
        let mut inner = self.inner.lock().await;
        inner.collect_finished_rebuilds();
        inner.maintenance_status()
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
        let event = {
            let mut inner = self.inner.lock().await;
            let activity_timestamp_ms = inner.next_activity_timestamp_ms();
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            state.ensure_writable()?;
            state.summary.name.clone_from(&normalized_name);
            state.push_event(
                SessionEventKind::SessionRenamed {
                    name: normalized_name,
                },
                self.store.as_ref(),
                activity_timestamp_ms,
            )?
        };
        Ok(event)
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
        self.ensure_session_loaded(session_id).await?;
        let mut inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        if !state.clients.is_empty() {
            return Err(SessionError::ConnectedClients(session_id));
        }
        if let Some(store) = &self.store {
            store.delete(session_id)?;
        }
        let removed = inner
            .sessions
            .remove(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(removed.summary)
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
        self.ensure_session_loaded(session_id).await?;
        let inner = self.inner.lock().await;
        inner
            .sessions
            .get(&session_id)
            .map(SessionState::summary)
            .ok_or(SessionError::NotFound(session_id))
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
        self.ensure_session_loaded(session_id).await?;
        let inner = self.inner.lock().await;
        inner
            .sessions
            .get(&session_id)
            .map(|state| state.working_directory.clone())
            .ok_or(SessionError::NotFound(session_id))
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
        self.ensure_session_loaded(session_id).await?;
        let inner = self.inner.lock().await;
        inner
            .sessions
            .get(&session_id)
            .map(|state| state.access_status)
            .ok_or(SessionError::NotFound(session_id))
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
        self.ensure_session_loaded(session_id).await?;
        let inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        if let Some(events) = &state.events {
            return Ok(events.clone());
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(store.read_session_events(session_id)?)
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
        self.ensure_session_loaded(session_id).await?;
        let should_mark_current = {
            let inner = self.inner.lock().await;
            let state = inner
                .sessions
                .get(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            if let Some(events) = &state.events {
                return Ok(history_page_from_events(session_id, events.clone(), query));
            }
            state.index_status == SessionIndexStatusKind::Stale
        };
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(session_id))?;
        let page = store.read_session_history_page(session_id, query)?;
        if should_mark_current {
            self.inner.lock().await.mark_index_current(session_id);
        }
        Ok(page)
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
        self.ensure_session_loaded(session_id).await?;
        let inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        if let Some(events) = &state.events {
            return Ok(input_history_from_events(events));
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(store.read_session_input_history(session_id)?)
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
        self.ensure_session_loaded(session_id).await?;
        let inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        if let Some(events) = &state.events {
            return Ok(model_context_events_from_history(events));
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(store.read_model_context_events(session_id)?)
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
        self.ensure_session_loaded(session_id).await?;
        let inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(state
            .current_provider
            .clone()
            .zip(state.current_model.clone()))
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
        self.ensure_session_loaded(session_id).await?;
        let inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(state.current_agent.clone())
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
        self.migrate_session_to_current_if_required(session_id)
            .await?;
        let attachment = {
            let mut inner = self.inner.lock().await;
            let activity_timestamp_ms = inner.next_activity_timestamp_ms();
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            let history = if let Some(events) = &state.events {
                events.clone()
            } else {
                let store = self
                    .store
                    .as_ref()
                    .ok_or(SessionError::NotFound(session_id))?;
                store.read_session_events(session_id)?
            };
            let input_history = input_history_from_events(&history);
            state.ensure_writable()?;
            state.clients.insert(client_id);
            state.summary.client_count = state.clients.len();
            let events = state.sender.subscribe();
            let attached_event = state.push_event(
                SessionEventKind::ClientAttached { client_id },
                self.store.as_ref(),
                activity_timestamp_ms,
            )?;
            SessionAttachment {
                history,
                input_history,
                attached_event,
                events,
            }
        };
        Ok(attachment)
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
        self.migrate_session_to_current_if_required(session_id)
            .await?;
        let history = self
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    limit,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await?
            .events;
        let input_history = self.session_input_history(session_id).await?;
        let attachment = {
            let mut inner = self.inner.lock().await;
            let activity_timestamp_ms = inner.next_activity_timestamp_ms();
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            state.ensure_writable()?;
            state.clients.insert(client_id);
            state.summary.client_count = state.clients.len();
            let events = state.sender.subscribe();
            let attached_event = state.push_event(
                SessionEventKind::ClientAttached { client_id },
                self.store.as_ref(),
                activity_timestamp_ms,
            )?;
            SessionAttachment {
                history,
                input_history,
                attached_event,
                events,
            }
        };
        Ok(attachment)
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
        self.ensure_session_loaded(session_id).await?;
        let mut inner = self.inner.lock().await;
        let activity_timestamp_ms = inner.next_activity_timestamp_ms();
        let Some(state) = inner.sessions.get_mut(&session_id) else {
            return Ok(None);
        };
        state.ensure_writable()?;
        if state.clients.remove(&client_id) {
            state.summary.client_count = state.clients.len();
            return Ok(Some(state.push_event(
                SessionEventKind::ClientDetached { client_id },
                self.store.as_ref(),
                activity_timestamp_ms,
            )?));
        }
        Ok(None)
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
        let events = {
            let mut inner = self.inner.lock().await;
            let activity_timestamp_ms = inner.next_activity_timestamp_ms();
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            state.ensure_writable()?;
            let mut events = Vec::new();
            if state.summary.name.is_none() && !state.has_user_message {
                let title = title_from_first_prompt(&text);
                state.summary.name = Some(title.clone());
                events.push(state.push_event(
                    SessionEventKind::SessionRenamed { name: Some(title) },
                    self.store.as_ref(),
                    activity_timestamp_ms,
                )?);
            }
            events.push(state.push_event(
                SessionEventKind::UserMessage { client_id, text },
                self.store.as_ref(),
                activity_timestamp_ms,
            )?);
            events
        };
        Ok(events)
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
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
            },
        )
        .await
    }

    /// Publish a transient event to currently attached session subscribers without
    /// appending it to durable history.
    ///
    /// Returns `None` when the session is not loaded or has no active subscribers.
    pub async fn publish_transient_event(
        &self,
        session_id: SessionId,
        kind: SessionEventKind,
    ) -> Option<SessionEvent> {
        let inner = self.inner.lock().await;
        let state = inner.sessions.get(&session_id)?;
        if state.sender.receiver_count() == 0 {
            return None;
        }
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: state.next_sequence,
            session_id,
            kind,
        };
        let _ = state.sender.send(event.clone());
        Some(event)
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
        let event = {
            let mut inner = self.inner.lock().await;
            let activity_timestamp_ms = inner.next_activity_timestamp_ms();
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            state.ensure_writable()?;
            state.push_event(kind, self.store.as_ref(), activity_timestamp_ms)?
        };
        Ok(event)
    }
}

impl SessionManagerInner {
    fn mark_index_current(&mut self, session_id: SessionId) {
        if let Some(state) = self.sessions.get_mut(&session_id) {
            state.index_status = SessionIndexStatusKind::Current;
        }
        self.collect_finished_rebuilds();
    }

    fn collect_finished_rebuilds(&mut self) {
        let finished = self
            .index_rebuilds
            .iter()
            .filter_map(|(session_id, handle)| handle.is_finished().then_some(*session_id))
            .collect::<Vec<_>>();
        for session_id in finished {
            if let Some(handle) = self.index_rebuilds.remove(&session_id) {
                drop(handle);
                self.completed_rebuilds = self.completed_rebuilds.saturating_add(1);
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.index_status = SessionIndexStatusKind::Current;
                }
            }
        }
    }

    fn maintenance_status(&self) -> SessionMaintenanceStatus {
        SessionMaintenanceStatus {
            stale_indexes: self
                .sessions
                .values()
                .filter(|state| state.index_status == SessionIndexStatusKind::Stale)
                .count(),
            running_rebuilds: self.index_rebuilds.len(),
            completed_rebuilds: self.completed_rebuilds,
            failed_rebuilds: self.failed_rebuilds,
        }
    }

    fn next_activity_timestamp_ms(&mut self) -> u64 {
        let now_ms = current_unix_millis();
        self.activity_clock_ms = self.activity_clock_ms.max(now_ms).saturating_add(1);
        self.activity_clock_ms
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
        self.summary.clone()
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

    fn push_event(
        &mut self,
        kind: SessionEventKind,
        store: Option<&SessionEventStore>,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionStoreError> {
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.next_sequence,
            session_id: self.summary.id,
            kind,
        };
        if let Some(store) = store {
            store.append(&event)?;
        }
        self.summary.updated_at_ms = activity_timestamp_ms;
        self.next_sequence += 1;
        self.event_count = self.event_count.saturating_add(1);
        match &event.kind {
            SessionEventKind::UserMessage { .. } => self.has_user_message = true,
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
        if let Some(store) = store {
            match store.write_state_index(self) {
                Ok(()) => self.index_status = SessionIndexStatusKind::Current,
                Err(error) => eprintln!(
                    "failed to update session index for {}: {error}",
                    self.summary.id
                ),
            }
        }
        let _ = self.sender.send(event.clone());
        Ok(event)
    }
}

fn sorted_session_summaries(
    sessions: &BTreeMap<SessionId, SessionState>,
    working_directory: &Path,
) -> Vec<SessionSummary> {
    let mut sessions = sessions
        .values()
        .filter(|state| normalize_working_directory(&state.working_directory) == working_directory)
        .map(SessionState::summary)
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

fn history_page_from_events(
    session_id: SessionId,
    history: Vec<SessionEvent>,
    query: SessionHistoryQuery,
) -> SessionHistoryPage {
    let limit = query.limit.max(1);
    let events = match query.direction {
        SessionHistoryDirection::Forward => history
            .into_iter()
            .filter(|event| {
                query
                    .cursor
                    .is_none_or(|cursor| event.sequence >= cursor.sequence)
            })
            .take(limit.saturating_add(1))
            .collect::<Vec<_>>(),
        SessionHistoryDirection::Backward => {
            let mut events = history
                .into_iter()
                .rev()
                .filter(|event| {
                    query
                        .cursor
                        .is_none_or(|cursor| event.sequence <= cursor.sequence)
                })
                .take(limit.saturating_add(1))
                .collect::<Vec<_>>();
            events.reverse();
            events
        }
    };
    let has_more = events.len() > limit;
    let page_events = if has_more {
        match query.direction {
            SessionHistoryDirection::Forward => events.into_iter().take(limit).collect(),
            SessionHistoryDirection::Backward => events.into_iter().skip(1).collect(),
        }
    } else {
        events
    };
    let next_cursor = if has_more {
        page_events.last().map(|event| SessionHistoryCursor {
            sequence: match query.direction {
                SessionHistoryDirection::Forward => event.sequence.saturating_add(1),
                SessionHistoryDirection::Backward => event.sequence.saturating_sub(1),
            },
        })
    } else {
        None
    };
    SessionHistoryPage {
        session_id,
        events: page_events,
        next_cursor,
        has_more,
    }
}

fn access_status_from_report(report: &reader::SessionReadReport) -> SessionAccessStatus {
    access_status_from_schema_versions(
        report.min_schema_version,
        report.max_schema_version,
        report.issues.iter().any(read_issue_blocks_access),
    )
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
    use super::{SessionAccessStatus, SessionManager, access_status_from_report, reader};
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ProviderStreamEvent, RuntimeWorkId,
        RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind,
        SessionHistoryDirection, SessionHistoryQuery, SessionTraceEvent, SessionTracePayload,
        SessionTracePhase, ToolInvocationStreamEvent, ToolOutputStream, TraceBlobRef,
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
                kind: SessionEventKind::SessionCreated {
                    name: None,
                    working_directory: test_working_directory(),
                },
            },
            SessionEvent {
                schema_version: 12,
                sequence: 1,
                session_id,
                kind: SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::Started {
                        tool_call_id: "tool-1".to_string(),
                        tool_name: "shell".to_string(),
                        terminal: false,
                        columns: None,
                        rows: None,
                    },
                },
            },
            SessionEvent {
                schema_version: 12,
                sequence: 2,
                session_id,
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
                kind: SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::Finished {
                        tool_call_id: "tool-1".to_string(),
                        sequence: 2,
                        is_error: false,
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

        let bytes = bmux_codec::to_vec(&old_payload).expect("old payload should encode");
        let decoded: SessionTracePayload =
            bmux_codec::from_bytes(&bytes).expect("old payload should decode");

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
                chunk_count: 2,
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
            .append_tool_call_finished(session.id, "tool-1".to_string(), "ok".to_string(), false)
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
            SessionEventKind::ToolCallFinished { tool_call_id, result, is_error }
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
    async fn attach_session_recent_lazy_migrates_old_schema_sessions() {
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
            super::SessionAccessStatus::ReadWrite
        );
        let attachment = manager
            .attach_session_recent(session_id, ClientId::new(), 10)
            .await
            .expect("old session should migrate lazily on attach");
        assert!(
            attachment
                .history
                .iter()
                .all(|event| event.schema_version == CURRENT_SESSION_EVENT_SCHEMA_VERSION)
        );
        assert_eq!(
            manager
                .session_access_status(session_id)
                .await
                .expect("status should update"),
            super::SessionAccessStatus::ReadWrite
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
        for index in 0..4 {
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
        assert!(matches!(
            &attachment.history[0].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 3"
        ));
        assert_eq!(
            attachment
                .input_history
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            vec!["message 0", "message 1", "message 2", "message 3"]
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
        assert!(!restored.catalog_loaded().await);
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
        assert!(!restored.catalog_loaded().await);

        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions.len(), 1);
        assert!(restored.catalog_loaded().await);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn persistent_restore_defers_stale_index_rebuild_until_access() {
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
            .expect("history page should rebuild lazily");
        assert!(index_path.exists());

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
        let bytes = bmux_codec::to_vec(value).expect("value should encode");
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
                kind: SessionEventKind::SessionCreated {
                    name: Some("stable-order".to_string()),
                    working_directory: test_working_directory(),
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                session_id,
                kind: SessionEventKind::AssistantDelta {
                    text: "partial".to_string(),
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 2,
                session_id,
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
        let payload = bmux_codec::to_vec(event).expect("legacy event should encode");
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
