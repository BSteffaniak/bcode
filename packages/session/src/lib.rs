#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Session mutations intentionally hold the manager lock while updating in-memory
// state and appending the corresponding event so summaries/history/fanout stay
// consistent in this first implementation.
#![allow(clippy::significant_drop_tightening)]

//! Session lifecycle, attachment management, and append-only event history.

pub(crate) mod index;
pub mod migration;
pub(crate) mod reader;

pub use index::{SessionIndexHealth, SessionIndexStatus};
pub use migration::{
    SessionEventLogMigration, SessionEventLogMigrationError, SessionMigrationAction,
    SessionMigrationApplyPolicy, SessionMigrationApplyStatus, SessionMigrationBackupPolicy,
    SessionMigrationDefinition, SessionMigrationJournalEntry, SessionMigrationJournalStatus,
    SessionMigrationOptions, SessionMigrationPlan, SessionMigrationPlanItem,
    SessionMigrationRegistry, SessionMigrationRegistryError, SessionMigrationReport,
    SessionMigrationReportItem,
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

    fn load_sessions(&self) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
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
            } else if let Some(index) =
                index::rebuild_index_metadata(&self.root, session_id, &path)?
            {
                let mut state = index.into_state();
                state.index_status = SessionIndexStatusKind::Stale;
                state.access_status = self.inspect_access_status(session_id)?;
                sessions.insert(session_id, state);
            }
        }

        Ok(sessions)
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
        let mut entries = match index::read_entries(&self.root, session_id) {
            Ok(entries) if entries.len() == index.event_count => entries,
            _ => {
                let _ = index::rebuild_index(&self.root, session_id, &event_path)?;
                index::read_entries(&self.root, session_id)?
            }
        };
        entries.sort_by_key(|entry| entry.sequence);
        let limit = query.limit.max(1);
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
        let mut events = Vec::with_capacity(page_entries.len());
        for entry in &page_entries {
            events.push(reader::read_event_at(&event_path, entry.offset)?);
        }
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
        let migration_ids: Vec<_> = plan.items.iter().map(|item| item.migration_id).collect();
        let session_ids: Vec<_> = plan.items.iter().map(|item| item.session_id).collect();
        migration::append_journal_entry(
            &self.root,
            &SessionMigrationJournalEntry {
                run_id: run_id.clone(),
                domain: plan.domain,
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
                        domain: plan.domain,
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
                        domain: plan.domain,
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

    /// Rewrite a canonical session event log through a registered event migration.
    ///
    /// The executor owns backup, temp writes, validation, atomic replacement, and
    /// derived index rebuild. The migration implementation only transforms events.
    ///
    /// # Errors
    ///
    /// Returns an error if the log cannot be read, backed up, migrated, validated,
    /// atomically replaced, or reindexed.
    pub fn migrate_event_log<M: SessionEventLogMigration>(
        &self,
        session_id: SessionId,
        migration: &M,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let started_at_ms = current_unix_millis();
        let run_id = format!("session-event-migration-{started_at_ms}");
        let migration_id = M::ID;
        let session_ids = vec![session_id];
        let migration_ids = vec![migration_id];
        migration::append_journal_entry(
            &self.root,
            &SessionMigrationJournalEntry {
                run_id: run_id.clone(),
                domain: "sessions/events",
                status: SessionMigrationJournalStatus::Started,
                dry_run: false,
                backup: true,
                backup_dir: None,
                started_at_ms,
                finished_at_ms: None,
                migration_ids: migration_ids.clone(),
                session_ids: session_ids.clone(),
                error: None,
            },
        )?;

        let result = self.migrate_event_log_inner(session_id, migration_id, migration);
        let finished_at_ms = current_unix_millis();
        match &result {
            Ok(report) => {
                migration::append_journal_entry(
                    &self.root,
                    &SessionMigrationJournalEntry {
                        run_id,
                        domain: "sessions/events",
                        status: SessionMigrationJournalStatus::Completed,
                        dry_run: false,
                        backup: true,
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
                        domain: "sessions/events",
                        status: SessionMigrationJournalStatus::Failed,
                        dry_run: false,
                        backup: true,
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

    fn migrate_event_log_inner<M: SessionEventLogMigration>(
        &self,
        session_id: SessionId,
        migration_id: &'static str,
        migration: &M,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let path = self.event_path(session_id);
        let report = reader::read_events(&path)?;
        if report.max_schema_version == Some(M::TO_SCHEMA)
            && report.min_schema_version == Some(M::TO_SCHEMA)
        {
            return Ok(SessionMigrationReport {
                domain: "sessions/events",
                dry_run: false,
                backup_dir: None,
                items: vec![SessionMigrationReportItem {
                    migration_id,
                    session_id,
                    action: SessionMigrationAction::RewriteCanonicalEvents,
                    status: SessionMigrationApplyStatus::Skipped,
                    message: "already at target schema".to_string(),
                }],
            });
        }
        if report.max_schema_version != Some(M::FROM_SCHEMA)
            || report.min_schema_version != Some(M::FROM_SCHEMA)
        {
            return Err(SessionStoreError::InvalidSessionId(format!(
                "session {session_id} schema range {:?}..{:?} does not match migration {migration_id} {}->{}",
                report.min_schema_version,
                report.max_schema_version,
                M::FROM_SCHEMA,
                M::TO_SCHEMA
            )));
        }

        let plan_item = SessionMigrationPlanItem {
            migration_id,
            session_id,
            current_version: M::TO_SCHEMA,
            found_version: Some(M::FROM_SCHEMA),
            action: SessionMigrationAction::RewriteCanonicalEvents,
            reason: format!(
                "canonical event migration {}->{}",
                M::FROM_SCHEMA,
                M::TO_SCHEMA
            ),
            automatic: false,
            backup_policy: SessionMigrationBackupPolicy::Required,
        };
        let backup_dir = self.backup_canonical_events(&[plan_item])?;
        let tmp_path = path.with_extension("events.tmp");
        let mut tmp = fs::File::create(&tmp_path)?;
        let mut migrated_events = Vec::with_capacity(report.events.len());
        for mut event in report.events {
            event = migration
                .migrate_event(event)
                .map_err(|error| SessionStoreError::InvalidSessionId(error.to_string()))?;
            event.schema_version = M::TO_SCHEMA;
            write_event_frame(&mut tmp, &event)?;
            migrated_events.push(event);
        }
        tmp.flush()?;
        drop(tmp);
        let validation = reader::read_events(&tmp_path)?;
        if validation.events.len() != migrated_events.len()
            || validation.min_schema_version != Some(M::TO_SCHEMA)
            || validation.max_schema_version != Some(M::TO_SCHEMA)
            || !validation.issues.is_empty()
        {
            return Err(SessionStoreError::InvalidSessionId(format!(
                "migrated session log validation failed for {session_id}"
            )));
        }
        fs::rename(&tmp_path, &path)?;
        self.reindex_session(session_id)?;
        Ok(SessionMigrationReport {
            domain: "sessions/events",
            dry_run: false,
            backup_dir: Some(backup_dir),
            items: vec![SessionMigrationReportItem {
                migration_id,
                session_id,
                action: SessionMigrationAction::RewriteCanonicalEvents,
                status: SessionMigrationApplyStatus::Applied,
                message: format!(
                    "migrated canonical events {}->{}",
                    M::FROM_SCHEMA,
                    M::TO_SCHEMA
                ),
            }],
        })
    }

    fn backup_canonical_events(
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

    fn event_path(&self, session_id: SessionId) -> PathBuf {
        self.root.join(format!("{session_id}.events"))
    }
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
    activity_clock_ms: u64,
    index_rebuilds: BTreeMap<SessionId, JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIndexStatusKind {
    Current,
    Stale,
}

#[derive(Debug)]
pub(crate) struct SessionState {
    summary: SessionSummary,
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
        let sessions = store.load_sessions()?;
        Ok(Self {
            inner: Mutex::new(SessionManagerInner {
                sessions,
                activity_clock_ms: current_unix_millis(),
                index_rebuilds: BTreeMap::new(),
            }),
            store: Some(store),
        })
    }

    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session-created event cannot be persisted.
    pub async fn create_session(
        &self,
        name: Option<String>,
    ) -> Result<SessionSummary, SessionError> {
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
        };
        let mut state = SessionState {
            summary: summary.clone(),
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
            SessionEventKind::SessionCreated { name },
            self.store.as_ref(),
            now_ms,
        )?;
        inner.sessions.insert(id, state);
        Ok(summary)
    }

    /// List known sessions.
    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        let mut inner = self.inner.lock().await;
        inner.schedule_stale_index_rebuilds(self.store.as_ref());
        let mut sessions: Vec<_> = inner.sessions.values().map(SessionState::summary).collect();
        sessions.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        sessions
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

    /// Return a summary for one session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_summary(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionError> {
        let inner = self.inner.lock().await;
        inner
            .sessions
            .get(&session_id)
            .map(SessionState::summary)
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
    /// * the client-attached event cannot be persisted
    pub async fn attach_session(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<SessionAttachment, SessionError> {
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
    /// * the client-attached event cannot be persisted
    pub async fn attach_session_recent(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        limit: usize,
    ) -> Result<SessionAttachment, SessionError> {
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
        if self
            .index_rebuilds
            .get(&session_id)
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            self.index_rebuilds.remove(&session_id);
        }
    }

    fn schedule_index_rebuild(&mut self, store: Option<&SessionEventStore>, session_id: SessionId) {
        let Some(store) = store.cloned() else {
            return;
        };
        if self
            .sessions
            .get(&session_id)
            .is_none_or(|state| state.index_status == SessionIndexStatusKind::Current)
        {
            return;
        }
        if self
            .index_rebuilds
            .get(&session_id)
            .is_some_and(|handle| !handle.is_finished())
        {
            return;
        }
        let handle = tokio::spawn(async move {
            if let Err(error) = store.reindex_session(session_id) {
                eprintln!("failed to rebuild session index for {session_id}: {error}");
            }
        });
        self.index_rebuilds.insert(session_id, handle);
    }

    fn schedule_stale_index_rebuilds(&mut self, store: Option<&SessionEventStore>) {
        let session_ids = self
            .sessions
            .iter()
            .filter_map(|(session_id, state)| {
                (state.index_status == SessionIndexStatusKind::Stale).then_some(*session_id)
            })
            .collect::<Vec<_>>();
        for session_id in session_ids {
            self.schedule_index_rebuild(store, session_id);
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
        let access_status = access_status_from_schema_versions(
            index.min_event_schema_version,
            index.max_event_schema_version,
            !index.issues.is_empty(),
        );
        let mut summary = index.summary;
        summary.created_at_ms = index.created_at_ms;
        summary.updated_at_ms = index.updated_at_ms;
        Self {
            summary,
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

const fn access_status_from_report(report: &reader::SessionReadReport) -> SessionAccessStatus {
    access_status_from_schema_versions(
        report.min_schema_version,
        report.max_schema_version,
        !report.issues.is_empty(),
    )
}

const fn access_status_from_schema_versions(
    _min_schema_version: Option<u16>,
    max_schema_version: Option<u16>,
    has_issues: bool,
) -> SessionAccessStatus {
    if has_issues {
        return SessionAccessStatus::RepairRequired;
    }
    match max_schema_version {
        Some(version) if version > CURRENT_SESSION_EVENT_SCHEMA_VERSION => {
            SessionAccessStatus::BlockedFutureVersion
        }
        Some(version) if version < CURRENT_SESSION_EVENT_SCHEMA_VERSION => {
            SessionAccessStatus::ReadOnlyMigrationRequired
        }
        Some(_) | None => SessionAccessStatus::ReadWrite,
    }
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
    use super::SessionManager;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, SessionEvent, SessionEventKind,
        SessionHistoryDirection, SessionHistoryQuery, SessionTraceEvent, SessionTracePayload,
        SessionTracePhase,
    };
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

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
            .create_session(Some("test".to_string()))
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
        let sessions = restored.list_sessions().await;
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
            SessionEventKind::SessionCreated { name } if name.as_deref() == Some("mixed")
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
            .create_session(Some("test".to_string()))
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
            .create_session(Some("paged".to_string()))
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
            .create_session(Some("recent".to_string()))
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
            .create_session(None)
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
        let sessions = manager.list_sessions().await;
        assert_eq!(
            sessions[0].name.as_deref(),
            Some("Fix session selection UX")
        );

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let restored_sessions = restored.list_sessions().await;
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
            .create_session(Some("Manual title".to_string()))
            .await
            .expect("session should be created");

        let events = manager
            .append_user_message(session.id, ClientId::new(), "Different title".to_string())
            .await
            .expect("message should append");

        assert_eq!(events.len(), 1);
        let sessions = manager.list_sessions().await;
        assert_eq!(sessions[0].name.as_deref(), Some("Manual title"));
    }

    #[tokio::test]
    async fn rename_session_restores_latest_name() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("Old title".to_string()))
            .await
            .expect("session should be created");

        manager
            .rename_session(session.id, Some("  New   title  ".to_string()))
            .await
            .expect("session should rename");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions().await;
        assert_eq!(sessions[0].name.as_deref(), Some("New title"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn list_sessions_orders_by_latest_activity() {
        let manager = SessionManager::default();
        let older = manager
            .create_session(Some("older".to_string()))
            .await
            .expect("older session should create");
        let newer = manager
            .create_session(Some("newer".to_string()))
            .await
            .expect("newer session should create");

        let sessions = manager.list_sessions().await;
        assert_eq!(sessions[0].id, newer.id);
        assert_eq!(sessions[1].id, older.id);

        manager
            .append_user_message(older.id, ClientId::new(), "wake older".to_string())
            .await
            .expect("message should append");

        let sessions = manager.list_sessions().await;
        assert_eq!(sessions[0].id, older.id);
        assert_eq!(sessions[1].id, newer.id);
    }

    #[tokio::test]
    async fn restored_sessions_order_by_index_activity() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let older = manager
            .create_session(Some("older".to_string()))
            .await
            .expect("older session should create");
        let newer = manager
            .create_session(Some("newer".to_string()))
            .await
            .expect("newer session should create");

        manager
            .append_user_message(older.id, ClientId::new(), "wake older".to_string())
            .await
            .expect("message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions().await;
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
            .create_session(Some("Delete me".to_string()))
            .await
            .expect("session should be created");

        manager
            .delete_session(session.id)
            .await
            .expect("session should delete");

        assert!(manager.list_sessions().await.is_empty());
        let restored = SessionManager::persistent(&root).expect("manager should restore");
        assert!(restored.list_sessions().await.is_empty());

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
            .create_session(Some("doctor".to_string()))
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
    async fn persistent_restore_defers_stale_index_rebuild_until_access() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("lazy".to_string()))
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
            .create_session(Some("migration".to_string()))
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
            .create_session(Some("journal".to_string()))
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
            .create_session(Some("backup".to_string()))
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

    fn write_legacy_event(path: &std::path::Path, event: &SessionEvent) {
        let mut file = std::fs::File::create(path).expect("event file should create");
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
