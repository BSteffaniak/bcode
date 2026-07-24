#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Session mutations intentionally hold the manager lock while updating in-memory
// state and appending the corresponding event so summaries/history/fanout stay
// consistent in this first implementation.
#![allow(clippy::significant_drop_tightening)]

//! Session lifecycle, attachment management, and append-only event history.
//!
//! Model context is a bounded projection of canonical events. Local and provider-native
//! compaction markers are equivalent boundaries: the newest marker is selected by its own event
//! sequence, while its `compacted_through_sequence` identifies the canonical prefix it replaces.
//! Normal model-context reads return that marker plus later semantic events without replaying or
//! repairing the complete event log.

pub mod legacy_storage;
pub use legacy_storage::{
    LegacyStorageInspectionReport, LegacyStorageRecoveryReport,
    inspect_accidental_epoch_session_root, recover_accidental_epoch_session_root,
};

mod actor;
pub mod db;
pub mod lease;
pub mod legacy_stream_cleanup;
mod migration_operation;
pub mod persisted;
mod persisted_legacy;
pub mod projection;
pub mod repair;
pub mod semantic_migration;
mod store_executor;

use actor::{AttachMode, SessionHandle};
use bcode_metrics::{MetricLabels, MetricsRegistry};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ExecutionSessionContextMode,
    ExecutionSessionProvenance, ModelTurnOutcome, ProjectionWindow, ProjectionWindowRequest,
    SessionEvent, SessionEventKind, SessionEventProvenance, SessionForkKind, SessionForkResult,
    SessionForkSummary, SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery,
    SessionId, SessionImportSummary, SessionInputHistoryEntry, SessionLiveEvent,
    SessionLiveEventKind, SessionMigrationProgress, SessionMigrationStage, SessionOpenFailureKind,
    SessionOpenOperationId, SessionOpenOperationSnapshot, SessionOpenTerminalOutcome,
    SessionSummary, SessionTitleSource, SessionTokenUsage, SessionTraceEvent, SessionVisibility,
};
use lease::{SessionLeaseGuard, SessionLeaseOwnerContext};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use store_executor::SessionStoreExecutor;
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, watch};
use tokio::task::spawn_blocking;

/// Return the stable kind name when a session event is live-only and must not be persisted.
const fn live_only_session_event_kind(kind: &SessionEventKind) -> Option<&'static str> {
    match kind {
        SessionEventKind::ToolInvocationStream { .. } => Some("tool_invocation_stream"),
        SessionEventKind::ToolContribution { event }
            if matches!(
                event.persistence,
                bcode_session_models::ToolContributionPersistence::Transient
            ) =>
        {
            Some("tool_contribution")
        }
        _ => None,
    }
}

const MAX_DURABLE_GENERIC_EVENT_BYTES: usize = 64 * 1024;

fn ensure_durable_session_event_kind(
    kind: &SessionEventKind,
    metrics: Option<&MetricsRegistry>,
) -> Result<(), SessionError> {
    if matches!(kind, SessionEventKind::LegacyEvent { .. }) {
        return Err(SessionError::EventSerialization(
            "historical compatibility events cannot be appended".to_owned(),
        ));
    }
    if let Some(event_kind) = live_only_session_event_kind(kind) {
        if let Some(metrics) = metrics {
            metrics.increment_counter("session.event.live_persistence_rejected");
        }
        return Err(SessionError::LiveEventPersistenceRejected { event_kind });
    }
    if matches!(kind, SessionEventKind::ToolContribution { .. }) {
        let payload_bytes = serde_json::to_vec(kind)
            .map_err(|error| SessionError::EventSerialization(error.to_string()))?
            .len();
        if payload_bytes > MAX_DURABLE_GENERIC_EVENT_BYTES {
            if let Some(metrics) = metrics {
                metrics.increment_counter("session.event.oversized_persistence_rejected");
                metrics.record_histogram(
                    "session.event.rejected_payload_bytes",
                    u64::try_from(payload_bytes).unwrap_or(u64::MAX),
                );
            }
            return Err(SessionError::DurableEventPayloadTooLarge {
                event_kind: "tool_contribution",
                payload_bytes,
                max_bytes: MAX_DURABLE_GENERIC_EVENT_BYTES,
            });
        }
    }
    Ok(())
}

fn record_session_event_domain_metrics(metrics: &MetricsRegistry, event: &SessionEvent) {
    if let Ok(payload) = serde_json::to_vec(event) {
        metrics.record_histogram("session.event.payload_bytes", payload.len() as u64);
    }
    if matches!(
        event.kind,
        SessionEventKind::UserMessage { .. }
            | SessionEventKind::AssistantMessage { .. }
            | SessionEventKind::ToolCallRequested { .. }
            | SessionEventKind::ToolCallFinished { .. }
            | SessionEventKind::SystemMessage { .. }
            | SessionEventKind::WorkingDirectoryChanged { .. }
            | SessionEventKind::ContextCompacted { .. }
            | SessionEventKind::ProviderContextCompacted { .. }
            | SessionEventKind::RequestContextObserved { .. }
    ) {
        metrics.increment_counter("session.event.semantic_rows");
    }
    match &event.kind {
        SessionEventKind::ToolCallFinished {
            semantic_result: Some(bcode_session_models::ToolInvocationResult::Artifact { artifact }),
            ..
        } => {
            metrics.add_counter(
                "session.event.artifact_references",
                u64::try_from(artifact.refs.len()).unwrap_or(u64::MAX),
            );
        }
        SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::ProviderContextCompacted { .. } => {
            metrics.increment_counter("session.event.compaction_boundaries");
        }
        _ => {}
    }
}

const MIGRATION_PROGRESS_BYTE_INTERVAL: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
struct MigrationProgressReporter {
    operation: Arc<migration_operation::SessionMigrationOperation>,
    last_reported: Arc<StdMutex<Option<(SessionMigrationStage, u64)>>>,
}

impl MigrationProgressReporter {
    fn new(operation: Arc<migration_operation::SessionMigrationOperation>) -> Self {
        Self {
            operation,
            last_reported: Arc::new(StdMutex::new(None)),
        }
    }

    fn stage(&self, stage: SessionMigrationStage, message: impl Into<String>) {
        self.operation.publish_progress(SessionMigrationProgress {
            stage,
            completed_units: None,
            total_units: None,
            unit: None,
            message: message.into(),
        });
    }

    fn determinate(
        &self,
        stage: SessionMigrationStage,
        completed: u64,
        total: u64,
        unit: bcode_session_models::SessionMigrationProgressUnit,
        message: impl Into<String>,
        interval: u64,
    ) {
        let should_publish = completed == 0
            || completed == total
            || self.last_reported.lock().is_ok_and(|mut last| {
                let publish = last.is_none_or(|(last_stage, last_completed)| {
                    last_stage != stage || completed.saturating_sub(last_completed) >= interval
                });
                if publish {
                    *last = Some((stage, completed));
                }
                publish
            });
        if should_publish {
            self.operation.publish_progress(SessionMigrationProgress {
                stage,
                completed_units: Some(completed),
                total_units: Some(total),
                unit: Some(unit),
                message: message.into(),
            });
        }
    }

    fn backup_verified(&self, path: PathBuf) {
        self.operation.publish_backup_path(path);
    }
}

fn ensure_loaded_metric_labels(result: &str) -> MetricLabels {
    let mut labels = MetricLabels::new();
    labels.insert("result".to_owned(), result.to_owned());
    labels
}

const MIGRATION_BACKUP_BUFFER_BYTES: usize = 64 * 1024;

async fn create_verified_migration_backup(
    root: &Path,
    session_id: SessionId,
    writer_epoch: u64,
    metrics: &MetricsRegistry,
    progress: Option<&MigrationProgressReporter>,
) -> Result<PathBuf, SessionError> {
    let source = root.join(session_id.to_string());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let destination = root
        .parent()
        .unwrap_or(root)
        .join("session-migration-backups")
        .join(format!("{timestamp}-{session_id}-epoch-{writer_epoch}"));
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "session_id": session_id,
        "source_writer_epoch": writer_epoch,
        "target_writer_epoch": db::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
        "created_at_ms": u64::try_from(timestamp).unwrap_or(u64::MAX),
    }))
    .map_err(|error| SessionError::MigrationBackup {
        session_id,
        reason: error.to_string(),
    })?;
    let backup_destination = destination.clone();
    let backup_progress = progress.cloned();
    let result = spawn_blocking(move || {
        create_verified_migration_backup_blocking(
            &source,
            &backup_destination,
            &manifest,
            MigrationBackupCopyFault::None,
            backup_progress.as_ref(),
        )
    })
    .await
    .map_err(|error| SessionError::MigrationBackup {
        session_id,
        reason: format!("backup worker failed: {error}"),
    })?
    .map_err(|error| SessionError::MigrationBackup {
        session_id,
        reason: error.to_string(),
    })?;
    metrics.record_histogram(
        "session.migration.backup.plan_duration_ms",
        duration_millis(result.plan_duration),
    );
    metrics.record_histogram(
        "session.migration.backup.copy_duration_ms",
        duration_millis(result.copy_duration),
    );
    metrics.record_histogram(
        "session.migration.backup.verify_duration_ms",
        duration_millis(result.verify_duration),
    );
    metrics.add_counter("session.migration.backup.files_total", result.files);
    metrics.add_counter("session.migration.backup.bytes_total", result.bytes);
    Ok(destination)
}

#[derive(Debug)]
struct MigrationBackupResult {
    files: u64,
    bytes: u64,
    plan_duration: std::time::Duration,
    copy_duration: std::time::Duration,
    verify_duration: std::time::Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigrationBackupCopyFault {
    None,
    #[cfg(test)]
    PermissionDenied,
    #[cfg(test)]
    ShortWriteAfter(u64),
}

fn create_verified_migration_backup_blocking(
    source: &Path,
    destination: &Path,
    manifest: &[u8],
    fault: MigrationBackupCopyFault,
    progress: Option<&MigrationProgressReporter>,
) -> std::io::Result<MigrationBackupResult> {
    if destination.exists() {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "backup destination already exists: {}",
                destination.display()
            ),
        ));
    }
    let result = (|| {
        let started = Instant::now();
        let files = migration_backup_files(source, source)?;
        let plan_duration = started.elapsed();
        let bytes = files
            .iter()
            .fold(0_u64, |total, file| total.saturating_add(file.bytes));
        if let Some(progress) = progress {
            let file_count = u64::try_from(files.len()).unwrap_or(u64::MAX);
            progress.determinate(
                SessionMigrationStage::PlanningBackup,
                file_count,
                file_count,
                bcode_session_models::SessionMigrationProgressUnit::Files,
                "Planned retained backup",
                1,
            );
            progress.determinate(
                SessionMigrationStage::CopyingBackup,
                0,
                bytes,
                bcode_session_models::SessionMigrationProgressUnit::Bytes,
                "Copying retained backup",
                MIGRATION_PROGRESS_BYTE_INTERVAL,
            );
        }
        fs::create_dir_all(destination)?;
        let started = Instant::now();
        let source_hashes =
            copy_and_hash_backup_files(source, destination, &files, fault, progress, bytes)?;
        let copy_duration = started.elapsed();
        if let Some(progress) = progress {
            progress.determinate(
                SessionMigrationStage::VerifyingBackup,
                0,
                bytes,
                bcode_session_models::SessionMigrationProgressUnit::Bytes,
                "Verifying retained backup",
                MIGRATION_PROGRESS_BYTE_INTERVAL,
            );
        }
        let started = Instant::now();
        verify_backup_files(destination, &files, &source_hashes, progress, bytes)?;
        let verify_duration = started.elapsed();
        fs::write(destination.join("migration-backup.json"), manifest)?;
        Ok(MigrationBackupResult {
            files: u64::try_from(files.len()).unwrap_or(u64::MAX),
            bytes,
            plan_duration,
            copy_duration,
            verify_duration,
        })
    })();
    if result.is_err() && destination.exists() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

#[derive(Debug)]
struct MigrationBackupFile {
    relative_path: PathBuf,
    bytes: u64,
}

fn migration_backup_files(
    root: &Path,
    directory: &Path,
) -> std::io::Result<Vec<MigrationBackupFile>> {
    let mut files = Vec::new();
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            files.extend(migration_backup_files(root, &path)?);
        } else {
            files.push(MigrationBackupFile {
                relative_path: path
                    .strip_prefix(root)
                    .map_err(std::io::Error::other)?
                    .to_path_buf(),
                bytes: entry.metadata()?.len(),
            });
        }
    }
    Ok(files)
}

fn copy_and_hash_backup_files(
    source: &Path,
    destination: &Path,
    files: &[MigrationBackupFile],
    fault: MigrationBackupCopyFault,
    progress: Option<&MigrationProgressReporter>,
    total_bytes: u64,
) -> std::io::Result<BTreeMap<PathBuf, [u8; 32]>> {
    #[cfg(not(test))]
    let _ = fault;
    #[cfg(test)]
    if matches!(fault, MigrationBackupCopyFault::PermissionDenied) {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "injected backup permission failure",
        ));
    }
    let mut total_written = 0_u64;
    let mut source_digests = BTreeMap::new();
    for file in files {
        let source_path = source.join(&file.relative_path);
        let destination_path = destination.join(&file.relative_path);
        fs::create_dir_all(
            destination_path
                .parent()
                .ok_or_else(|| std::io::Error::other("backup destination file has no parent"))?,
        )?;
        let mut reader =
            BufReader::with_capacity(MIGRATION_BACKUP_BUFFER_BYTES, File::open(&source_path)?);
        let destination_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination_path)?;
        let mut writer = BufWriter::with_capacity(MIGRATION_BACKUP_BUFFER_BYTES, destination_file);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; MIGRATION_BACKUP_BUFFER_BYTES];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            #[cfg(test)]
            if let MigrationBackupCopyFault::ShortWriteAfter(limit) = fault
                && total_written.saturating_add(u64::try_from(read).unwrap_or(u64::MAX)) > limit
            {
                let allowed = usize::try_from(limit.saturating_sub(total_written))
                    .unwrap_or(usize::MAX)
                    .min(read);
                if allowed > 0 {
                    writer.write_all(&buffer[..allowed])?;
                }
                writer.flush()?;
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "injected short backup write",
                ));
            }
            writer.write_all(&buffer[..read])?;
            total_written = total_written.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            if let Some(progress) = progress {
                progress.determinate(
                    SessionMigrationStage::CopyingBackup,
                    total_written,
                    total_bytes,
                    bcode_session_models::SessionMigrationProgressUnit::Bytes,
                    "Copying retained backup",
                    MIGRATION_PROGRESS_BYTE_INTERVAL,
                );
            }
            hasher.update(&buffer[..read]);
        }
        writer.flush()?;
        fs::set_permissions(&destination_path, fs::metadata(&source_path)?.permissions())?;
        source_digests.insert(file.relative_path.clone(), hasher.finalize().into());
    }
    Ok(source_digests)
}

fn verify_backup_files(
    destination: &Path,
    files: &[MigrationBackupFile],
    source_hashes: &BTreeMap<PathBuf, [u8; 32]>,
    progress: Option<&MigrationProgressReporter>,
    total_bytes: u64,
) -> std::io::Result<()> {
    let mut verified_bytes = 0_u64;
    for file in files {
        let destination_path = destination.join(&file.relative_path);
        if fs::metadata(&destination_path)?.len() != file.bytes {
            return Err(std::io::Error::other(format!(
                "backup length verification failed for {}",
                file.relative_path.display()
            )));
        }
        let actual = hash_file(&destination_path)?;
        if source_hashes.get(&file.relative_path) != Some(&actual) {
            return Err(std::io::Error::other(format!(
                "backup hash verification failed for {}",
                file.relative_path.display()
            )));
        }
        verified_bytes = verified_bytes.saturating_add(file.bytes);
        if let Some(progress) = progress {
            progress.determinate(
                SessionMigrationStage::VerifyingBackup,
                verified_bytes,
                total_bytes,
                bcode_session_models::SessionMigrationProgressUnit::Bytes,
                "Verifying retained backup",
                MIGRATION_PROGRESS_BYTE_INTERVAL,
            );
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut reader = BufReader::with_capacity(MIGRATION_BACKUP_BUFFER_BYTES, File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; MIGRATION_BACKUP_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn terminal_session_open_snapshot(
    session_id: SessionId,
    outcome: SessionOpenTerminalOutcome,
    message: String,
) -> SessionOpenOperationSnapshot {
    SessionOpenOperationSnapshot {
        operation_id: SessionOpenOperationId::new(),
        revision: 0,
        session_id,
        source_writer_epoch: None,
        target_writer_epoch: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
        progress: SessionMigrationProgress {
            stage: if matches!(outcome, SessionOpenTerminalOutcome::Ready) {
                SessionMigrationStage::Complete
            } else {
                SessionMigrationStage::Failed
            },
            completed_units: None,
            total_units: None,
            unit: None,
            message,
        },
        outcome: Some(outcome),
        backup_path: None,
    }
}

fn ready_session_open_snapshot(session_id: SessionId) -> SessionOpenOperationSnapshot {
    terminal_session_open_snapshot(
        session_id,
        SessionOpenTerminalOutcome::Ready,
        "Session storage is ready".to_owned(),
    )
}

fn migrating_session_open_snapshot(
    session_id: SessionId,
    writer_epoch: u64,
) -> SessionOpenOperationSnapshot {
    SessionOpenOperationSnapshot {
        operation_id: SessionOpenOperationId::new(),
        revision: 0,
        session_id,
        source_writer_epoch: Some(writer_epoch),
        target_writer_epoch: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
        progress: SessionMigrationProgress {
            stage: SessionMigrationStage::WaitingForOwnership,
            completed_units: None,
            total_units: None,
            unit: None,
            message: "Waiting for exclusive session ownership".to_owned(),
        },
        outcome: None,
        backup_path: None,
    }
}

fn session_open_failure_outcome(error: &SessionError) -> SessionOpenTerminalOutcome {
    match error {
        SessionError::Db(db::SessionDbError::CompatibilityDegraded { issue_count }) => {
            return SessionOpenTerminalOutcome::DegradedReadOnly {
                issue_count: *issue_count,
            };
        }
        SessionError::Db(db::SessionDbError::WriterIncompatible { actual, expected }) => {
            return SessionOpenTerminalOutcome::WriterIncompatible {
                actual: *actual,
                expected: *expected,
            };
        }
        SessionError::ProjectionStale { .. }
        | SessionError::Db(
            db::SessionDbError::InvalidCanonicalSequence { .. }
            | db::SessionDbError::InvalidRow { .. }
            | db::SessionDbError::PersistedEvent(_),
        ) => {
            return SessionOpenTerminalOutcome::RepairRequired {
                reason: error.to_string(),
            };
        }
        _ => {}
    }
    let kind = match error {
        SessionError::NotFound(_) => SessionOpenFailureKind::NotFound,
        SessionError::MigrationBackup { .. } => SessionOpenFailureKind::BackupFailed,
        SessionError::Lease(lease::SessionLeaseError::OwnedByOtherDaemon { .. }) => {
            SessionOpenFailureKind::OwnedByOtherDaemon
        }
        _ => SessionOpenFailureKind::MigrationFailed,
    };
    SessionOpenTerminalOutcome::Failed {
        kind,
        message: error.to_string(),
        backup_path: None,
    }
}

fn record_ensure_loaded_duration(metrics: &MetricsRegistry, result: &str, elapsed_ms: u64) {
    metrics.record_histogram_with_labels(
        "session.manager.ensure_loaded.duration_ms",
        elapsed_ms,
        ensure_loaded_metric_labels(result),
    );
}

/// Runtime model and reasoning selections restored from a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionRuntimeSelection {
    /// Session-specific agent id, when explicitly selected.
    pub agent_id: Option<String>,
    /// Session-specific provider plugin id, when explicitly selected.
    pub provider_plugin_id: Option<String>,
    /// Session-specific model id, when explicitly selected.
    pub model_id: Option<String>,
    /// Session-specific reasoning effort, when explicitly selected.
    pub reasoning_effort: Option<String>,
    /// Session-specific reasoning summary, when explicitly selected.
    pub reasoning_summary: Option<String>,
}

/// Return a shared-session execution target after enforcing explicitly sequential admission.
///
/// # Errors
///
/// Returns an error when provenance is not shared-sequential or does not identify `parent`.
pub fn shared_execution_session(
    parent: SessionId,
    provenance: &ExecutionSessionProvenance,
) -> Result<SessionId, SessionError> {
    validate_execution_session_provenance(Some(provenance))?;
    if provenance.context_mode != ExecutionSessionContextMode::SharedSequential
        || provenance.parent_session_id != parent
    {
        return Err(SessionError::InvalidExecutionSessionProvenance(
            "shared execution must target its declared parent session".to_string(),
        ));
    }
    Ok(parent)
}

const MAX_EXECUTION_PROVENANCE_ID_BYTES: usize = 512;

fn validate_execution_session_provenance(
    provenance: Option<&ExecutionSessionProvenance>,
) -> Result<(), SessionError> {
    let Some(provenance) = provenance else {
        return Ok(());
    };
    for (label, value) in [
        ("owner", provenance.owner.as_str()),
        ("run_id", provenance.run_id.as_str()),
        ("node_id", provenance.node_id.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > MAX_EXECUTION_PROVENANCE_ID_BYTES {
            return Err(SessionError::InvalidExecutionSessionProvenance(format!(
                "{label} must contain 1..={MAX_EXECUTION_PROVENANCE_ID_BYTES} bytes"
            )));
        }
    }
    if provenance.attempt == 0 {
        return Err(SessionError::InvalidExecutionSessionProvenance(
            "attempt must be greater than zero".to_string(),
        ));
    }
    match (provenance.context_mode, provenance.parent_generation) {
        (ExecutionSessionContextMode::FixedGenerationFork, None) => {
            return Err(SessionError::InvalidExecutionSessionProvenance(
                "fixed-generation fork requires parent_generation".to_string(),
            ));
        }
        (
            ExecutionSessionContextMode::FreshIsolated
            | ExecutionSessionContextMode::SharedSequential,
            Some(_),
        ) => {
            return Err(SessionError::InvalidExecutionSessionProvenance(
                "parent_generation is valid only for fixed-generation fork".to_string(),
            ));
        }
        _ => {}
    }
    Ok(())
}

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
    /// A live-only event was passed to the durable append boundary.
    #[error("live-only session event cannot be persisted: {event_kind}")]
    LiveEventPersistenceRejected { event_kind: &'static str },
    /// A bounded durable event exceeded its event-kind-specific payload limit.
    #[error(
        "durable session event payload is too large: {event_kind} payload={payload_bytes} max={max_bytes}"
    )]
    DurableEventPayloadTooLarge {
        event_kind: &'static str,
        payload_bytes: usize,
        max_bytes: usize,
    },
    /// A durable event could not be measured before persistence.
    #[error("session event serialization failed before persistence: {0}")]
    EventSerialization(String),
    #[error("unsupported session projection window request")]
    UnsupportedProjectionWindow,
    #[error(
        "session DB projection is stale: {session_id} {projection} checkpoint={checkpoint:?} expected={expected}"
    )]
    ProjectionStale {
        session_id: SessionId,
        projection: &'static str,
        checkpoint: Option<u64>,
        expected: u64,
    },
    /// Turn admission metadata is invalid.
    #[error(transparent)]
    TurnAdmission(#[from] bcode_session_models::TurnAdmissionMetadataError),
    /// Session storage is a known legacy generation that requires an explicit maintenance migration.
    #[error(
        "session storage migration required: writer epoch {actual}, expected {expected}; run an explicit session migration/reindex command"
    )]
    StorageMigrationRequired { actual: u64, expected: u64 },
    /// A verified pre-migration backup could not be created.
    #[error("session migration backup failed for {session_id}: {reason}")]
    MigrationBackup {
        session_id: SessionId,
        reason: String,
    },
    /// Session database error: {0}
    #[error("session database error: {0}")]
    Db(#[from] db::SessionDbError),
    /// Session database is unavailable for this operation.
    #[error("session database is unavailable: {0}")]
    DbUnavailable(SessionId),
    /// Selected fork prompt could not be found.
    #[error("selected fork prompt not found in session {session_id}: sequence {sequence}")]
    ForkPromptNotFound {
        session_id: SessionId,
        sequence: u64,
    },
    #[error(
        "session generation changed before clone snapshot: {session_id} expected={expected} current={current}"
    )]
    CloneGenerationChanged {
        session_id: SessionId,
        expected: u64,
        current: u64,
    },
    /// Background execution-session provenance is malformed or inconsistent.
    #[error("invalid execution session provenance: {0}")]
    InvalidExecutionSessionProvenance(String),
    /// Session is owned by another daemon or cannot be leased.
    #[error(transparent)]
    Lease(#[from] lease::SessionLeaseError),
}

/// Errors returned by the session store.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("blocking session store task failed: {0}")]
    BlockingTask(#[from] tokio::task::JoinError),
    #[error("session catalog load failed: {0}")]
    CatalogLoad(String),
    #[error(transparent)]
    Lease(#[from] lease::SessionLeaseError),
}

/// Filesystem-rooted session store for DB-backed session histories.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    pub(crate) metrics: MetricsRegistry,
    lease_owner: SessionLeaseOwnerContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionFormatMarker {
    family: String,
    epoch: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionManifest {
    schema_version: u32,
    session_format: SessionFormatMarker,
    summary: SessionSummary,
}

/// Current bounded session-manifest metadata schema.
pub const SESSION_MANIFEST_SCHEMA_VERSION: u32 = 2;
/// Stable family identifier for canonical Bcode session stores.
pub const SESSION_FORMAT_FAMILY: &str = "bcode.session";
/// Current canonical session format epoch.
pub const CURRENT_SESSION_FORMAT_EPOCH: u32 = 2;

impl SessionStore {
    /// Create an event store rooted at the provided directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            metrics: MetricsRegistry::default(),
            lease_owner: SessionLeaseOwnerContext::default(),
        }
    }

    /// Create an event store rooted at the provided directory with metrics instrumentation.
    #[must_use]
    pub fn with_metrics(root: impl Into<PathBuf>, metrics: MetricsRegistry) -> Self {
        Self {
            root: root.into(),
            metrics,
            lease_owner: SessionLeaseOwnerContext::default(),
        }
    }

    fn load_catalog(&self) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let mut summaries = if self.catalog_db_path().exists() {
            match self.load_global_catalog_summaries() {
                Ok(summaries) => summaries,
                Err(error) => {
                    eprintln!("ignoring unreadable derived session catalog: {error}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        summaries.extend(self.load_session_manifests()?);
        summaries.extend(self.discover_canonical_session_summaries()?);
        match self.load_legacy_catalog_summaries() {
            Ok(legacy) => summaries.extend(legacy),
            Err(error) => eprintln!("ignoring unreadable legacy session catalog: {error}"),
        }
        summaries.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
        });
        summaries.dedup_by_key(|summary| summary.id);

        let mut sessions = BTreeMap::new();
        for summary in summaries {
            let summary = match self.load_session_manifest(summary.id) {
                Ok(Some(manifest_summary)) => manifest_summary,
                Ok(None) => summary,
                Err(error) => {
                    eprintln!(
                        "using canonical fallback for session {} with unreadable manifest metadata: {error}",
                        summary.id
                    );
                    summary
                }
            };
            sessions.insert(summary.id, SessionState::from_catalog_summary(summary));
        }
        Ok(sessions)
    }

    fn backfill_catalog(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = self.load_session_manifests()?;
        summaries.extend(self.discover_canonical_session_summaries()?);
        match self.load_legacy_catalog_summaries() {
            Ok(legacy) => summaries.extend(legacy),
            Err(error) => eprintln!("ignoring unreadable legacy session catalog: {error}"),
        }
        summaries.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
        });
        summaries.dedup_by_key(|summary| summary.id);
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at_ms));
        if summaries.is_empty() {
            return Ok(summaries);
        }

        self.write_global_catalog_summaries(&summaries)?;
        Ok(summaries)
    }

    fn discover_canonical_session_summaries(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = Vec::new();
        if !self.root.exists() {
            return Ok(summaries);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let Some(session_id) = canonical_session_id_from_dir(&path) else {
                continue;
            };
            if !db::session_db_path(&self.root, session_id).exists() {
                continue;
            }
            summaries.push(SessionSummary {
                id: session_id,
                name: None,
                explicit_name: None,
                derived_title: None,
                title_source: SessionTitleSource::EmptyDraft,
                client_count: 0,
                created_at_ms: 0,
                updated_at_ms: 0,
                working_directory: self.root.clone(),
                import: None,
                fork: None,
                execution: None,
            });
        }
        Ok(summaries)
    }

    fn load_session_manifests(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = Vec::new();
        if !self.root.exists() {
            return Ok(summaries);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let Some(session_id) = canonical_session_id_from_dir(&path) else {
                continue;
            };
            match self.load_session_manifest(session_id) {
                Ok(Some(summary)) => summaries.push(summary),
                Ok(None) => {}
                Err(error) => {
                    eprintln!("skipping unreadable session manifest {session_id}: {error}");
                }
            }
        }
        Ok(summaries)
    }

    fn load_session_manifest(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSummary>, SessionStoreError> {
        let path = self.session_manifest_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read(&path)?;
        let value: serde_json::Value = serde_json::from_slice(&contents)
            .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
        let schema_version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64);
        if schema_version == Some(1) {
            let summary: SessionSummary =
                serde_json::from_value(value.get("summary").cloned().ok_or_else(|| {
                    SessionStoreError::CatalogLoad(
                        "legacy session manifest is missing its summary".to_owned(),
                    )
                })?)
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            if summary.id != session_id {
                return Err(SessionStoreError::CatalogLoad(format!(
                    "session manifest id mismatch: expected {session_id}, found {}",
                    summary.id
                )));
            }
            return Ok(Some(summary));
        }
        if schema_version != Some(u64::from(SESSION_MANIFEST_SCHEMA_VERSION)) {
            return Err(SessionStoreError::CatalogLoad(format!(
                "unsupported session manifest schema version {schema_version:?}"
            )));
        }
        let format_family = value
            .pointer("/session_format/family")
            .and_then(serde_json::Value::as_str);
        let format_epoch = value
            .pointer("/session_format/epoch")
            .and_then(serde_json::Value::as_u64);
        if format_family != Some(SESSION_FORMAT_FAMILY)
            || format_epoch != Some(u64::from(CURRENT_SESSION_FORMAT_EPOCH))
        {
            return Err(SessionStoreError::CatalogLoad(format!(
                "unsupported session format family={format_family:?} epoch={format_epoch:?}"
            )));
        }
        let manifest: SessionManifest = serde_json::from_value(value)
            .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
        if manifest.schema_version != SESSION_MANIFEST_SCHEMA_VERSION {
            return Err(SessionStoreError::CatalogLoad(format!(
                "unsupported session manifest schema version {}",
                manifest.schema_version
            )));
        }
        if manifest.session_format.family != SESSION_FORMAT_FAMILY
            || manifest.session_format.epoch != CURRENT_SESSION_FORMAT_EPOCH
        {
            return Err(SessionStoreError::CatalogLoad(format!(
                "unsupported session format family={} epoch={}",
                manifest.session_format.family, manifest.session_format.epoch
            )));
        }
        if manifest.summary.id != session_id {
            return Err(SessionStoreError::CatalogLoad(format!(
                "session manifest id mismatch: expected {session_id}, found {}",
                manifest.summary.id
            )));
        }
        Ok(Some(manifest.summary))
    }

    fn load_legacy_catalog_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if self.catalog_namespace().is_none() || !db::global_catalog_db_path(&self.root).exists() {
            return Ok(Vec::new());
        }
        Self::load_catalog_summaries_at_path(db::global_catalog_db_path(&self.root))
    }

    fn load_catalog_summaries_at_path(
        path: PathBuf,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            runtime.block_on(async move {
                let catalog = db::GlobalSessionDb::open_turso_without_catalog_lock(&path)
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                catalog
                    .list_sessions()
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))
            })
        })
        .join()
        .map_err(|_| SessionStoreError::CatalogLoad("catalog loader panicked".to_string()))?
    }

    fn write_global_catalog_summaries(
        &self,
        summaries: &[SessionSummary],
    ) -> Result<(), SessionStoreError> {
        let root = self.root.clone();
        let namespace = self.catalog_namespace();
        let summaries = summaries.to_vec();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            runtime.block_on(async move {
                let catalog = match namespace.as_deref() {
                    Some(namespace) => {
                        db::GlobalSessionDb::open_turso_in_root_namespace(&root, namespace).await
                    }
                    None => db::GlobalSessionDb::open_turso_in_root(&root).await,
                }
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                for summary in summaries {
                    catalog
                        .upsert_session(&summary, &db::session_db_path(&root, summary.id))
                        .await
                        .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                }
                Ok(())
            })
        })
        .join()
        .map_err(|_| SessionStoreError::CatalogLoad("catalog writer panicked".to_string()))?
    }

    pub(crate) fn write_session_manifest(
        &self,
        summary: &SessionSummary,
    ) -> Result<(), SessionStoreError> {
        let path = self.session_manifest_path(summary.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut summary = summary.clone();
        summary.client_count = 0;
        let manifest = SessionManifest {
            schema_version: SESSION_MANIFEST_SCHEMA_VERSION,
            session_format: SessionFormatMarker {
                family: SESSION_FORMAT_FAMILY.to_owned(),
                epoch: CURRENT_SESSION_FORMAT_EPOCH,
            },
            summary,
        };
        let temp_path = path.with_extension("json.tmp");
        fs::write(
            &temp_path,
            serde_json::to_vec_pretty(&manifest)
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?,
        )?;
        fs::rename(&temp_path, path)?;
        Ok(())
    }

    fn session_manifest_path(&self, session_id: SessionId) -> PathBuf {
        db::session_dir_path(&self.root, session_id).join("manifest.json")
    }

    fn catalog_namespace(&self) -> Option<String> {
        self.lease_owner
            .build_fingerprint
            .as_deref()
            .map(safe_catalog_namespace)
    }

    fn catalog_db_path(&self) -> PathBuf {
        self.catalog_namespace().map_or_else(
            || db::global_catalog_db_path(&self.root),
            |namespace| db::namespaced_catalog_db_path(&self.root, &namespace),
        )
    }

    fn load_global_catalog_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let root = self.root.clone();
        let namespace = self.catalog_namespace();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            runtime.block_on(async move {
                let catalog = match namespace.as_deref() {
                    Some(namespace) => {
                        db::GlobalSessionDb::open_turso_in_root_namespace(&root, namespace).await
                    }
                    None => db::GlobalSessionDb::open_turso_in_root(&root).await,
                }
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                catalog
                    .list_sessions()
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))
            })
        })
        .join()
        .map_err(|_| SessionStoreError::CatalogLoad("global catalog loader panicked".to_string()))?
    }

    pub(crate) fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn with_lease_owner(mut self, lease_owner: SessionLeaseOwnerContext) -> Self {
        self.lease_owner = lease_owner;
        self
    }

    pub(crate) const fn lease_owner(&self) -> &SessionLeaseOwnerContext {
        &self.lease_owner
    }
}

/// Input for appending a tool-call request event.
#[derive(Debug, Clone, Default)]
pub struct AppendToolCallRequestedInput {
    /// Provider tool call identifier.
    pub tool_call_id: String,
    /// Tool name requested by the model.
    pub tool_name: String,
    /// Raw JSON arguments requested by the model.
    pub arguments_json: String,
    /// Producer plugin id, when known.
    pub producer_plugin_id: Option<String>,
    /// Working directory captured for this invocation.
    pub working_directory: Option<std::path::PathBuf>,
    /// Plugin-owned request visual captured at request time.
    pub request_visual: Option<bcode_session_models::PluginVisualDescriptor>,
    /// Legacy request presentation metadata retained for old sessions/imports.
    pub legacy_request_presentation:
        Option<bcode_session_models::LegacyToolRequestPresentationMetadata>,
}

/// In-memory session manager with optional DB-backed persistence.
#[derive(Debug, Clone)]
pub struct SessionManager {
    inner: Arc<Mutex<SessionManagerInner>>,
    store: Option<SessionStoreExecutor>,
    activity_clock_ms: Arc<AtomicU64>,
    catalog_status_tx: watch::Sender<CatalogLoadStatus>,
    catalog_status_rx: watch::Receiver<CatalogLoadStatus>,
    mutation_tx: broadcast::Sender<SessionMutationCommitted>,
    migration_operations: migration_operation::SessionMigrationOperations,
    metrics: MetricsRegistry,
}

#[derive(Debug, Default)]
struct SessionManagerInner {
    sessions: BTreeMap<SessionId, SessionHandle>,
    leases: BTreeMap<SessionId, SessionLeaseGuard>,
    load_gates: BTreeMap<SessionId, Arc<Mutex<()>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionLoadStatusKind {
    Current,
    SummaryOnly,
}

enum SessionLeaseLoadOutcome {
    Acquired(Box<SessionLeaseGuard>),
    Retry,
}

/// Current asynchronous catalog discovery status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum CatalogLoadStatus {
    NotStarted,
    Loading,
    Loaded,
    Failed(String),
}

/// First-class session health for normal runtime UX.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionHealth {
    /// DB-backed session is ready for normal runtime access.
    Ready,
    /// Session is inspectable but contains semantically opaque history and is read-only.
    DegradedReadOnly { issue_count: u64 },
    /// Session storage requires a different writer epoch.
    WriterIncompatible { actual: Option<u64>, expected: u64 },
    /// A DB read model is missing or stale.
    ProjectionStale {
        projection: &'static str,
        checkpoint: Option<u64>,
        expected: u64,
    },
    /// Session storage exists but cannot be safely used without repair.
    RepairRequired { reason: String },
    /// No DB-backed session exists for the id.
    NotFound,
}

async fn compatibility_health(db: &db::SessionDb, expected: u64) -> Option<SessionHealth> {
    match db.session_compatibility_status().await {
        Ok(db::SessionCompatibilityStatus::Compatible { .. }) => None,
        Ok(db::SessionCompatibilityStatus::Degraded { issue_count, .. }) => {
            Some(SessionHealth::DegradedReadOnly { issue_count })
        }
        Ok(db::SessionCompatibilityStatus::Missing) => Some(SessionHealth::ProjectionStale {
            projection: "session_compatibility",
            checkpoint: None,
            expected,
        }),
        Ok(db::SessionCompatibilityStatus::Stale {
            checkpoint,
            expected,
        }) => Some(SessionHealth::ProjectionStale {
            projection: "session_compatibility",
            checkpoint: Some(checkpoint),
            expected,
        }),
        Ok(db::SessionCompatibilityStatus::Incompatible { actual, expected }) => {
            Some(SessionHealth::RepairRequired {
                reason: format!(
                    "unsupported session compatibility projection schema {actual}; expected {expected}"
                ),
            })
        }
        Err(error) => Some(SessionHealth::RepairRequired {
            reason: error.to_string(),
        }),
    }
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
    reasoning_effort: Option<String>,
    reasoning_summary: Option<String>,
    current_agent: Option<String>,
    latest_compaction_sequence: Option<u64>,
    context_epoch: u64,
    context_occupancy: Option<bcode_session_models::RequestContextOccupancy>,
    turn_receipts: BTreeMap<(String, String), bcode_session_models::TurnReceipt>,
    total_metered_tokens: u64,
    load_status: SessionLoadStatusKind,
    sender: broadcast::Sender<SessionEvent>,
    live_events: SessionLiveEventBroker,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionLiveEventBroker {
    sender: broadcast::Sender<SessionLiveEvent>,
    published: Arc<AtomicU64>,
    dropped_no_receivers: Arc<AtomicU64>,
}

impl SessionLiveEventBroker {
    fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender,
            published: Arc::new(AtomicU64::new(0)),
            dropped_no_receivers: Arc::new(AtomicU64::new(0)),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<SessionLiveEvent> {
        self.sender.subscribe()
    }

    fn publish(&self, event: SessionLiveEvent) -> Option<SessionLiveEvent> {
        if self.sender.receiver_count() == 0 {
            self.dropped_no_receivers.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let _ = self.sender.send(event.clone());
        self.published.fetch_add(1, Ordering::Relaxed);
        Some(event)
    }
}

/// Native catalog entry with maintenance/access metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCatalogEntry {
    pub summary: SessionSummary,
    pub load_status: SessionCatalogLoadStatus,
}

/// Session load status for catalog/status reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCatalogLoadStatus {
    Current,
    SummaryOnly,
}

impl SessionCatalogEntry {
    fn from_snapshot(snapshot: actor::SessionSnapshot) -> Self {
        Self {
            summary: snapshot.summary,
            load_status: match snapshot.load_status {
                SessionLoadStatusKind::Current => SessionCatalogLoadStatus::Current,
                SessionLoadStatusKind::SummaryOnly => SessionCatalogLoadStatus::SummaryOnly,
            },
        }
    }
}

#[derive(Debug)]
pub struct SessionAttachment {
    pub session: SessionSummary,
    pub history: Vec<SessionEvent>,
    pub input_history: Vec<SessionInputHistoryEntry>,
    pub events: broadcast::Receiver<SessionEvent>,
    pub live_events: broadcast::Receiver<SessionLiveEvent>,
}

/// Non-mutating event subscription for a session.
#[derive(Debug)]
pub struct SessionEventSubscription {
    pub session: SessionSummary,
    pub events: broadcast::Receiver<SessionEvent>,
    pub live_events: broadcast::Receiver<SessionLiveEvent>,
}

/// Notification emitted after a durable session mutation is committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMutationCommitted {
    pub session_id: SessionId,
    pub event: SessionEvent,
    pub summary: SessionSummary,
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
            activity_clock_ms: Arc::new(AtomicU64::new(current_unix_millis())),
            catalog_status_tx,
            catalog_status_rx,
            mutation_tx: broadcast::channel(1024).0,
            migration_operations: migration_operation::SessionMigrationOperations::default(),
            metrics: MetricsRegistry::default(),
        }
    }
}

struct OwnedLegacyMigration<'a> {
    session_id: SessionId,
    root: &'a Path,
    writer_epoch: u64,
    maintenance: &'a lease::SessionMaintenanceGuard,
    write: &'a lease::SessionWriteGuard,
    started: &'a bcode_metrics::MetricsTimer,
    progress: Option<&'a MigrationProgressReporter>,
}

impl SessionManager {
    /// Create a session manager backed by a session store root.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent(root: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        Self::persistent_with_metrics(root, MetricsRegistry::default())
    }

    /// Create a session manager backed by a session store root with metrics instrumentation.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent_with_metrics(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
    ) -> Result<Self, SessionStoreError> {
        let store = SessionStore::with_metrics(root, metrics);
        let sessions = store.load_catalog()?;
        Ok(Self::from_store(store, sessions, true))
    }

    /// Create a session manager backed by a session store root with lease owner metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent_with_metrics_and_lease_owner(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
        lease_owner: SessionLeaseOwnerContext,
    ) -> Result<Self, SessionStoreError> {
        let store = SessionStore::with_metrics(root, metrics).with_lease_owner(lease_owner);
        let sessions = store.load_catalog()?;
        Ok(Self::from_store(store, sessions, true))
    }

    /// Create a session manager whose catalog is loaded on demand.
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
        let store = SessionStore::with_metrics(root, metrics);
        Self::from_store(store, BTreeMap::new(), false)
    }

    /// Create a lazy persistent session manager with lease owner metadata.
    #[must_use]
    pub fn persistent_lazy_with_metrics_and_lease_owner(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
        lease_owner: SessionLeaseOwnerContext,
    ) -> Self {
        let store = SessionStore::with_metrics(root, metrics).with_lease_owner(lease_owner);
        Self::from_store(store, BTreeMap::new(), false)
    }

    fn from_store(
        store: SessionStore,
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
        let (mutation_tx, _) = broadcast::channel(1024);
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
                leases: BTreeMap::new(),
                load_gates: BTreeMap::new(),
            })),
            store: Some(executor),
            activity_clock_ms: Arc::new(AtomicU64::new(current_unix_millis())),
            catalog_status_tx,
            catalog_status_rx,
            mutation_tx,
            migration_operations: migration_operation::SessionMigrationOperations::default(),
            metrics,
        }
    }

    /// Start or join server-owned preparation for opening one persistent session.
    ///
    /// Current storage returns a terminal ready snapshot without spawning work. Known legacy
    /// storage starts one detached, per-session migration operation that survives observer loss.
    ///
    /// # Errors
    ///
    /// Returns an error if bounded storage classification fails or the session does not exist.
    pub async fn prepare_session_open(
        &self,
        session_id: SessionId,
    ) -> Result<SessionOpenOperationSnapshot, SessionError> {
        let Some(store) = &self.store else {
            return if self.inner.lock().await.sessions.contains_key(&session_id) {
                Ok(ready_session_open_snapshot(session_id))
            } else {
                Err(SessionError::NotFound(session_id))
            };
        };
        let root = store.root_path();
        if !db::session_db_path(&root, session_id).exists() {
            return Err(SessionError::NotFound(session_id));
        }
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        let compatibility = match db.storage_compatibility().await {
            Ok(compatibility) => compatibility,
            Err(db::SessionDbError::WriterIncompatible { actual, expected }) => {
                return Ok(terminal_session_open_snapshot(
                    session_id,
                    SessionOpenTerminalOutcome::WriterIncompatible { actual, expected },
                    format!(
                        "Session writer epoch {actual:?} is incompatible with expected epoch {expected}"
                    ),
                ));
            }
            Err(error) => {
                return Ok(terminal_session_open_snapshot(
                    session_id,
                    SessionOpenTerminalOutcome::RepairRequired {
                        reason: error.to_string(),
                    },
                    "Session storage requires repair".to_owned(),
                ));
            }
        };
        if matches!(
            compatibility,
            db::SessionStorageCompatibility::Current { .. }
        ) {
            let (outcome, message) = match db.session_compatibility_status().await? {
                db::SessionCompatibilityStatus::Compatible { .. } => {
                    return Ok(ready_session_open_snapshot(session_id));
                }
                db::SessionCompatibilityStatus::Degraded { issue_count, .. } => (
                    SessionOpenTerminalOutcome::DegradedReadOnly { issue_count },
                    format!(
                        "Session history contains {issue_count} unsupported event(s) and is read-only"
                    ),
                ),
                db::SessionCompatibilityStatus::Missing => (
                    SessionOpenTerminalOutcome::RepairRequired {
                        reason: "session compatibility projection is missing".to_owned(),
                    },
                    "Session compatibility projection is missing".to_owned(),
                ),
                db::SessionCompatibilityStatus::Stale {
                    checkpoint,
                    expected,
                } => (
                    SessionOpenTerminalOutcome::RepairRequired {
                        reason: format!(
                            "session compatibility projection is stale: checkpoint {checkpoint}, expected {expected}"
                        ),
                    },
                    "Session compatibility projection is stale".to_owned(),
                ),
                db::SessionCompatibilityStatus::Incompatible { actual, expected } => (
                    SessionOpenTerminalOutcome::RepairRequired {
                        reason: format!(
                            "session compatibility projection schema {actual} is incompatible with expected schema {expected}"
                        ),
                    },
                    "Session compatibility projection is incompatible".to_owned(),
                ),
            };
            return Ok(terminal_session_open_snapshot(session_id, outcome, message));
        }
        let db::SessionStorageCompatibility::KnownLegacy { writer_epoch } = compatibility else {
            unreachable!("current compatibility returned above")
        };
        let initial = migrating_session_open_snapshot(session_id, writer_epoch);
        let manager = self.clone();
        let operation = self
            .migration_operations
            .start_or_join(initial, move |operation| async move {
                let reporter = MigrationProgressReporter::new(operation);
                match manager
                    .ensure_session_loaded_with_progress(session_id, Some(&reporter))
                    .await
                {
                    Ok(()) => SessionOpenTerminalOutcome::Ready,
                    Err(error) => session_open_failure_outcome(&error),
                }
            })
            .await;
        Ok(operation.snapshot())
    }

    /// Return one operation snapshot when both session and operation identities match.
    pub async fn session_open_operation(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Option<SessionOpenOperationSnapshot> {
        self.migration_operations
            .get(session_id, operation_id)
            .await
            .map(|operation| operation.snapshot())
    }

    /// Subscribe to one matching session-open operation.
    pub async fn subscribe_session_open_operation(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Option<watch::Receiver<SessionOpenOperationSnapshot>> {
        self.migration_operations
            .get(session_id, operation_id)
            .await
            .map(|operation| operation.subscribe())
    }

    #[cfg(test)]
    async fn session_open_operation_history(
        &self,
        session_id: SessionId,
        operation_id: SessionOpenOperationId,
    ) -> Vec<SessionOpenOperationSnapshot> {
        self.migration_operations
            .get(session_id, operation_id)
            .await
            .map_or_else(Vec::new, |operation| operation.history())
    }

    /// Return the number of migrations currently running.
    pub async fn active_session_migration_count(&self) -> usize {
        self.migration_operations.active_count().await
    }

    /// Subscribe to committed durable session mutations.
    #[must_use]
    pub fn subscribe_mutations(&self) -> broadcast::Receiver<SessionMutationCommitted> {
        self.mutation_tx.subscribe()
    }

    fn publish_committed_mutation(&self, event: SessionEvent, summary: SessionSummary) {
        let _ = self.mutation_tx.send(SessionMutationCommitted {
            session_id: event.session_id,
            event,
            summary,
        });
    }

    /// Return the persistent session store root, when this manager is store-backed.
    #[must_use]
    pub fn session_store_root(&self) -> Option<PathBuf> {
        self.store.as_ref().map(SessionStoreExecutor::root_path)
    }

    async fn load_db_session_state(
        &self,
        session_id: SessionId,
        db: &db::SessionDb,
    ) -> Result<SessionState, SessionError> {
        let Some(db_state) = db.session_state().await? else {
            return Err(SessionError::ProjectionStale {
                session_id,
                projection: "session_state",
                checkpoint: None,
                expected: db.last_event_sequence().await?.unwrap_or(0),
            });
        };
        let expected_last_sequence = db
            .last_event_sequence()
            .await?
            .unwrap_or(db_state.last_event_seq);
        if db_state.last_event_seq < expected_last_sequence {
            return Err(SessionError::ProjectionStale {
                session_id,
                projection: "session_state",
                checkpoint: Some(db_state.last_event_seq),
                expected: expected_last_sequence,
            });
        }
        let activity_bounds = db.activity_bounds().await?;
        let created_at_ms = activity_bounds
            .map(|(created_at_ms, _)| created_at_ms)
            .or(db_state.updated_at_ms)
            .unwrap_or_else(current_unix_millis);
        let updated_at_ms = db_state
            .updated_at_ms
            .or_else(|| activity_bounds.map(|(_, updated_at_ms)| updated_at_ms))
            .unwrap_or(created_at_ms);
        Ok(SessionState::from_db_state(
            db_state,
            created_at_ms,
            updated_at_ms,
        ))
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

    async fn session_load_gate(&self, session_id: SessionId) -> Arc<Mutex<()>> {
        Arc::clone(
            self.inner
                .lock()
                .await
                .load_gates
                .entry(session_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    async fn ensure_session_loaded(&self, session_id: SessionId) -> Result<(), SessionError> {
        self.ensure_session_loaded_with_progress(session_id, None)
            .await
    }

    async fn ensure_session_loaded_with_progress(
        &self,
        session_id: SessionId,
        progress: Option<&MigrationProgressReporter>,
    ) -> Result<(), SessionError> {
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        self.ensure_session_loaded_inner(session_id, progress).await
    }

    async fn ensure_session_loaded_inner(
        &self,
        session_id: SessionId,
        progress: Option<&MigrationProgressReporter>,
    ) -> Result<(), SessionError> {
        let total_timer = self.metrics.timer();
        let cached_handle = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = cached_handle {
            return self
                .ensure_cached_session_loaded(session_id, handle, total_timer, progress)
                .await;
        }
        let Some(store) = &self.store else {
            record_ensure_loaded_duration(&self.metrics, "missing", total_timer.elapsed_ms());
            return Err(SessionError::NotFound(session_id));
        };
        if db::session_db_path(&store.root_path(), session_id).exists() {
            self.load_persistent_session(session_id, store, total_timer, progress)
                .await?;
            return Ok(());
        }
        record_ensure_loaded_duration(&self.metrics, "missing", total_timer.elapsed_ms());
        Err(SessionError::NotFound(session_id))
    }

    async fn ensure_cached_session_loaded(
        &self,
        session_id: SessionId,
        handle: SessionHandle,
        total_timer: bcode_metrics::MetricsTimer,
        progress: Option<&MigrationProgressReporter>,
    ) -> Result<(), SessionError> {
        let Some(store) = &self.store else {
            record_ensure_loaded_duration(&self.metrics, "cached", total_timer.elapsed_ms());
            return Ok(());
        };
        if !db::session_db_path(&store.root_path(), session_id).exists() {
            record_ensure_loaded_duration(&self.metrics, "cached", total_timer.elapsed_ms());
            return Ok(());
        }
        let snapshot = handle.snapshot();
        let inserted_lease = self
            .acquire_missing_session_lease(session_id, store, progress)
            .await?;
        let refreshed_summary = snapshot.load_status == SessionLoadStatusKind::SummaryOnly;
        if refreshed_summary {
            let result = self
                .refresh_summary_session(session_id, store, &handle)
                .await;
            if result.is_err() && inserted_lease {
                self.inner.lock().await.leases.remove(&session_id);
            }
            result?;
        }
        record_ensure_loaded_duration(
            &self.metrics,
            if refreshed_summary {
                "summary_refreshed"
            } else {
                "cached"
            },
            total_timer.elapsed_ms(),
        );
        Ok(())
    }

    async fn acquire_session_lease_for_load(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        progress: Option<&MigrationProgressReporter>,
    ) -> Result<SessionLeaseGuard, SessionError> {
        use db::SessionStorageCompatibility::{Current, KnownLegacy};

        let root = store.root_path();
        for attempt in 0..3_u8 {
            let db = db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
            if let Some(progress) = progress {
                progress.stage(
                    SessionMigrationStage::InspectingStorage,
                    "Inspecting session storage",
                );
            }
            let compatibility = db.storage_compatibility().await?;
            drop(db);
            let outcome = match compatibility {
                Current { .. } => {
                    self.acquire_current_session_lease(session_id, store, &root)
                        .await?
                }
                KnownLegacy { writer_epoch } => {
                    if progress.is_none() {
                        return Err(SessionError::StorageMigrationRequired {
                            actual: writer_epoch,
                            expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
                        });
                    }
                    self.migrate_legacy_session_for_load(
                        session_id,
                        store,
                        &root,
                        writer_epoch,
                        attempt,
                        progress,
                    )
                    .await?
                }
            };
            if let SessionLeaseLoadOutcome::Acquired(lease) = outcome {
                return Ok(*lease);
            }
        }
        Err(db::SessionDbError::MigrationHistoryIncompatible {
            reason: "session storage changed repeatedly while acquiring ownership".to_owned(),
        }
        .into())
    }

    async fn acquire_current_session_lease(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        root: &Path,
    ) -> Result<SessionLeaseLoadOutcome, SessionError> {
        use db::SessionStorageCompatibility::{Current, KnownLegacy};

        let lease = lease::acquire_session_lease(root, session_id, store.lease_owner())?;
        let rechecked = db::SessionDb::open_existing_turso_in_root(session_id, root)
            .await?
            .storage_compatibility()
            .await?;
        match rechecked {
            Current { .. } => Ok(SessionLeaseLoadOutcome::Acquired(Box::new(lease))),
            KnownLegacy { .. } => {
                drop(lease);
                self.metrics
                    .increment_counter("session.manager.storage_migration.race_retry_total");
                Ok(SessionLeaseLoadOutcome::Retry)
            }
        }
    }

    async fn migrate_legacy_session_for_load(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        root: &Path,
        writer_epoch: u64,
        attempt: u8,
        progress: Option<&MigrationProgressReporter>,
    ) -> Result<SessionLeaseLoadOutcome, SessionError> {
        use db::SessionStorageCompatibility::{Current, KnownLegacy};

        let started = self.metrics.timer();
        self.metrics
            .increment_counter("session.manager.storage_migration.attempted_total");
        tracing::info!(
            target: "bcode_session::migration",
            %session_id,
            writer_epoch,
            target_writer_epoch = db::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
            "attempting automatic legacy session migration"
        );
        let ownership_timer = self.metrics.timer();
        if let Some(progress) = progress {
            progress.stage(
                SessionMigrationStage::WaitingForOwnership,
                "Waiting for exclusive session ownership",
            );
        }
        let maintenance = match lease::acquire_session_maintenance_guard(root, session_id) {
            Ok(maintenance) => maintenance,
            Err(error @ lease::SessionLeaseError::OwnedByOtherDaemon { .. }) => {
                self.metrics
                    .increment_counter("session.manager.storage_migration.blocked_owner_total");
                let rechecked = db::SessionDb::open_existing_turso_in_root(session_id, root)
                    .await?
                    .storage_compatibility()
                    .await?;
                if matches!(rechecked, Current { .. }) && attempt < 2 {
                    self.metrics
                        .increment_counter("session.manager.storage_migration.race_retry_total");
                    return Ok(SessionLeaseLoadOutcome::Retry);
                }
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        self.metrics.record_histogram(
            "session.migration.ownership_duration_ms",
            ownership_timer.elapsed_ms(),
        );
        let write = lease::acquire_maintenance_session_write_lock(&maintenance, root, session_id)?;
        let rechecked = db::SessionDb::open_existing_turso_in_root(session_id, root)
            .await?
            .storage_compatibility()
            .await?;
        if matches!(rechecked, KnownLegacy { .. }) {
            self.migrate_owned_legacy_storage(OwnedLegacyMigration {
                session_id,
                root,
                writer_epoch,
                maintenance: &maintenance,
                write: &write,
                started: &started,
                progress,
            })
            .await?;
        }
        drop(write);
        Ok(SessionLeaseLoadOutcome::Acquired(Box::new(
            lease::transition_session_maintenance_to_lease(
                maintenance,
                root,
                session_id,
                store.lease_owner(),
            )?,
        )))
    }

    async fn migrate_owned_legacy_storage(
        &self,
        migration: OwnedLegacyMigration<'_>,
    ) -> Result<(), SessionError> {
        let OwnedLegacyMigration {
            session_id,
            root,
            writer_epoch,
            maintenance,
            write,
            started,
            progress,
        } = migration;
        let backup_path = create_verified_migration_backup(
            root,
            session_id,
            writer_epoch,
            &self.metrics,
            progress,
        )
        .await?;
        if let Some(progress) = progress {
            progress.backup_verified(backup_path.clone());
            progress.stage(
                SessionMigrationStage::PreparingSchema,
                "Preparing session storage schema",
            );
        }
        tracing::info!(
            target: "bcode_session::migration",
            %session_id,
            backup_path = %backup_path.display(),
            "verified pre-migration session backup"
        );
        let db_progress = progress.map(|progress| {
            let progress = progress.clone();
            Arc::new(move |update| progress.operation.publish_progress(update))
                as db::SessionMigrationProgressCallback
        });
        let migrated = db::SessionDb::migrate_turso_in_root_observed(
            session_id,
            root,
            maintenance,
            write,
            self.metrics.clone(),
            db_progress,
        )
        .await
        .inspect_err(|error| {
            self.metrics
                .increment_counter("session.manager.storage_migration.failed_total");
            tracing::warn!(
                target: "bcode_session::migration",
                %session_id,
                %error,
                "automatic legacy session migration failed"
            );
        })?;
        let readiness_timer = self.metrics.timer();
        if let Some(progress) = progress {
            progress.stage(
                SessionMigrationStage::ValidatingWriteReadiness,
                "Validating session write readiness",
            );
        }
        migrated.validate_write_readiness().await?;
        self.metrics.record_histogram(
            "session.migration.write_readiness_duration_ms",
            readiness_timer.elapsed_ms(),
        );
        drop(migrated);
        self.metrics
            .increment_counter("session.manager.storage_migration.completed_total");
        self.metrics.record_histogram(
            "session.manager.storage_migration.duration_ms",
            started.elapsed_ms(),
        );
        tracing::info!(
            target: "bcode_session::migration",
            %session_id,
            writer_epoch,
            target_writer_epoch = db::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
            duration_ms = started.elapsed_ms(),
            "automatic legacy session migration completed"
        );
        Ok(())
    }

    async fn acquire_missing_session_lease(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        progress: Option<&MigrationProgressReporter>,
    ) -> Result<bool, SessionError> {
        if self.inner.lock().await.leases.contains_key(&session_id) {
            return Ok(false);
        }
        let lease = self
            .acquire_session_lease_for_load(session_id, store, progress)
            .await?;
        let mut inner = self.inner.lock().await;
        if let std::collections::btree_map::Entry::Vacant(entry) = inner.leases.entry(session_id) {
            entry.insert(lease);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn refresh_summary_session(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        handle: &SessionHandle,
    ) -> Result<(), SessionError> {
        let db_open_timer = self.metrics.timer();
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        if let db::SessionCompatibilityStatus::Degraded { issue_count, .. } =
            db.session_compatibility_status().await?
        {
            return Err(db::SessionDbError::CompatibilityDegraded { issue_count }.into());
        }
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.summary_refresh_db_open_duration_ms",
            db_open_timer.elapsed_ms(),
        );
        let state_load_timer = self.metrics.timer();
        let state = self.load_db_session_state(session_id, &db).await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.summary_refresh_state_load_duration_ms",
            state_load_timer.elapsed_ms(),
        );
        let replace_timer = self.metrics.timer();
        handle.replace_state(state).await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.summary_refresh_replace_state_duration_ms",
            replace_timer.elapsed_ms(),
        );
        Ok(())
    }

    async fn load_persistent_session(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        total_timer: bcode_metrics::MetricsTimer,
        progress: Option<&MigrationProgressReporter>,
    ) -> Result<(), SessionError> {
        let load_timer = self.metrics.timer();
        let lease_timer = self.metrics.timer();
        let lease = self
            .acquire_session_lease_for_load(session_id, store, progress)
            .await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.lease_acquire_duration_ms",
            lease_timer.elapsed_ms(),
        );
        let db_open_timer = self.metrics.timer();
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        if let db::SessionCompatibilityStatus::Degraded { issue_count, .. } =
            db.session_compatibility_status().await?
        {
            return Err(db::SessionDbError::CompatibilityDegraded { issue_count }.into());
        }
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.db_open_duration_ms",
            db_open_timer.elapsed_ms(),
        );
        let state_load_timer = self.metrics.timer();
        let state = self.load_db_session_state(session_id, &db).await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.state_load_duration_ms",
            state_load_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.load_db_session_duration_ms",
            load_timer.elapsed_ms(),
        );
        let insert_timer = self.metrics.timer();
        let mut inner = self.inner.lock().await;
        inner
            .sessions
            .insert(session_id, SessionHandle::new(state, Some(store.clone())));
        inner.leases.insert(session_id, lease);
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.insert_handle_duration_ms",
            insert_timer.elapsed_ms(),
        );
        record_ensure_loaded_duration(&self.metrics, "db_loaded", total_timer.elapsed_ms());
        Ok(())
    }

    async fn release_persistent_idle_session_resources(&self, session_id: SessionId) {
        if self.store.is_some() {
            let _ = self.release_idle_session_resources(session_id).await;
        }
    }

    /// Return the current persistent catalog discovery status.
    #[must_use]
    pub fn catalog_status(&self) -> CatalogLoadStatus {
        self.catalog_status_rx.borrow().clone()
    }

    /// Subscribe to persistent catalog status changes.
    #[must_use]
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

    /// Backfill the current catalog DB from bounded legacy summary sources.
    ///
    /// This scans manifest sidecars and the legacy global catalog DB, but does not open per-session
    /// DBs or replay event logs.
    ///
    /// # Errors
    ///
    /// Returns an error if catalog backfill fails.
    pub async fn backfill_catalog(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let Some(store) = self.store.clone() else {
            return Ok(Vec::new());
        };
        let summaries = store.backfill_catalog().await?;
        if summaries.is_empty() {
            return Ok(summaries);
        }
        let mut inner = self.inner.lock().await;
        for summary in &summaries {
            inner.sessions.entry(summary.id).or_insert_with(|| {
                SessionHandle::new(
                    SessionState::from_catalog_summary(summary.clone()),
                    Some(store.clone()),
                )
            });
        }
        Ok(summaries)
    }

    /// Return first-class health for one session without event-log replay or repair.
    pub async fn session_health(&self, session_id: SessionId) -> SessionHealth {
        let Some(store) = &self.store else {
            return if self.inner.lock().await.sessions.contains_key(&session_id) {
                SessionHealth::Ready
            } else {
                SessionHealth::NotFound
            };
        };
        let root = store.root_path();
        let db_path = db::session_db_path(&root, session_id);
        if !db_path.exists() {
            return SessionHealth::NotFound;
        }
        let db = match db::SessionDb::open_existing_turso_in_root(session_id, &root).await {
            Ok(db) => db,
            Err(error) => {
                return SessionHealth::RepairRequired {
                    reason: error.to_string(),
                };
            }
        };
        let expected_writer_epoch = u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH);
        match db.storage_compatibility().await {
            Ok(db::SessionStorageCompatibility::Current { .. }) => {}
            Ok(db::SessionStorageCompatibility::KnownLegacy { writer_epoch }) => {
                return SessionHealth::WriterIncompatible {
                    actual: Some(writer_epoch),
                    expected: expected_writer_epoch,
                };
            }
            Err(db::SessionDbError::WriterIncompatible { actual, expected }) => {
                return SessionHealth::WriterIncompatible { actual, expected };
            }
            Err(error) => {
                return SessionHealth::RepairRequired {
                    reason: error.to_string(),
                };
            }
        }
        let expected = match db.last_event_sequence().await {
            Ok(Some(sequence)) => sequence,
            Ok(None) => 0,
            Err(error) => {
                return SessionHealth::RepairRequired {
                    reason: error.to_string(),
                };
            }
        };
        if let Some(health) = compatibility_health(&db, expected).await {
            return health;
        }
        let session_state = match db.session_state().await {
            Ok(Some(state)) if state.last_event_seq >= expected => state,
            Ok(Some(state)) => {
                return SessionHealth::ProjectionStale {
                    projection: "session_state",
                    checkpoint: Some(state.last_event_seq),
                    expected,
                };
            }
            Ok(None) => {
                return SessionHealth::ProjectionStale {
                    projection: "session_state",
                    checkpoint: None,
                    expected,
                };
            }
            Err(error) => {
                return SessionHealth::RepairRequired {
                    reason: error.to_string(),
                };
            }
        };
        debug_assert!(session_state.last_event_seq >= expected);
        match db
            .materialized_projection_checkpoint(db::MaterializedProjection::ArtifactReferences)
            .await
        {
            Ok(Some(checkpoint)) if checkpoint == expected => SessionHealth::Ready,
            Ok(checkpoint) => SessionHealth::ProjectionStale {
                projection: "artifact_references",
                checkpoint,
                expected,
            },
            Err(error) => SessionHealth::RepairRequired {
                reason: error.to_string(),
            },
        }
    }

    /// Require this session to be ready for a durable turn-admission append.
    ///
    /// # Errors
    ///
    /// Returns a session-specific lease, writer-contract, projection, or database error before
    /// user input is persisted.
    pub async fn require_write_readiness(&self, session_id: SessionId) -> Result<(), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.validate_write_readiness().await
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
        self.create_session_with_execution(name, working_directory, None)
            .await
    }

    async fn create_session_with_execution(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        execution: Option<ExecutionSessionProvenance>,
    ) -> Result<SessionSummary, SessionError> {
        validate_execution_session_provenance(execution.as_ref())?;
        if let Some(provenance) = &execution {
            match provenance.context_mode {
                ExecutionSessionContextMode::FreshIsolated => {}
                ExecutionSessionContextMode::FixedGenerationFork => {
                    return self
                        .clone_execution_session_at_generation(
                            provenance.clone(),
                            name,
                            working_directory,
                        )
                        .await;
                }
                ExecutionSessionContextMode::SharedSequential => {
                    return Err(SessionError::InvalidExecutionSessionProvenance(
                        "shared-sequential execution reuses the parent session and must not create a child"
                            .to_string(),
                    ));
                }
            }
        }
        self.create_session_record(name, working_directory, execution)
            .await
    }

    async fn create_session_record(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        execution: Option<ExecutionSessionProvenance>,
    ) -> Result<SessionSummary, SessionError> {
        let started_at = std::time::Instant::now();
        self.metrics
            .increment_counter("session.manager.create.total");
        let working_directory = normalize_working_directory(&working_directory);
        let id = SessionId::new();
        let (sender, _) = broadcast::channel(512);
        let live_events = SessionLiveEventBroker::new(512);
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
            fork: None,
            execution: execution.map(|provenance| {
                Box::new(bcode_session_models::ExecutionSessionSummary {
                    provenance,
                    visibility: SessionVisibility::Background,
                })
            }),
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
            reasoning_effort: None,
            reasoning_summary: None,
            current_agent: None,
            latest_compaction_sequence: None,
            context_epoch: 0,
            context_occupancy: None,
            turn_receipts: BTreeMap::new(),
            total_metered_tokens: 0,
            load_status: SessionLoadStatusKind::Current,
            sender,
            live_events,
        };
        let lease = self
            .store
            .as_ref()
            .map(|store| lease::acquire_session_lease(&store.root_path(), id, store.lease_owner()))
            .transpose()?;
        let handle = SessionHandle::new(state, self.store.clone());
        let event = handle
            .append_event(
                SessionEventKind::SessionCreated {
                    name,
                    working_directory,
                },
                now_ms,
            )
            .await?;
        let execution_event = if let Some(execution) = summary.execution.clone() {
            Some(
                handle
                    .append_event(
                        SessionEventKind::ExecutionSessionCreated {
                            provenance: Box::new(execution.provenance),
                            visibility: execution.visibility,
                        },
                        now_ms,
                    )
                    .await?,
            )
        } else {
            None
        };
        {
            let mut inner = self.inner.lock().await;
            inner.sessions.insert(id, handle);
            if let Some(lease) = lease {
                inner.leases.insert(id, lease);
            }
        }
        self.release_persistent_idle_session_resources(id).await;
        self.publish_committed_mutation(event, summary.clone());
        if let Some(event) = execution_event {
            self.publish_committed_mutation(event, summary.clone());
        }
        self.metrics
            .record_histogram("session.manager.create.duration_ms", elapsed_ms(started_at));
        Ok(summary)
    }

    async fn clone_execution_session_at_generation(
        &self,
        provenance: ExecutionSessionProvenance,
        name: Option<String>,
        working_directory: PathBuf,
    ) -> Result<SessionSummary, SessionError> {
        let expected = provenance
            .parent_generation
            .expect("validated fixed-generation provenance has a generation");
        let source = self.session_summary(provenance.parent_session_id).await?;
        let expected_working_directory = normalize_working_directory(&working_directory);
        if normalize_working_directory(&source.working_directory) != expected_working_directory {
            return Err(SessionError::InvalidExecutionSessionProvenance(
                "fixed-generation child working directory must match its parent or declared worktree"
                    .to_string(),
            ));
        }
        let events = self.session_history(provenance.parent_session_id).await?;
        let current = events.last().map_or(0, |event| event.sequence);
        if current != expected {
            return Err(SessionError::CloneGenerationChanged {
                session_id: provenance.parent_session_id,
                expected,
                current,
            });
        }
        let marker = SessionEventKind::SessionForked {
            source_session_id: provenance.parent_session_id,
            source_title: Some(source.display_title().to_string()),
            source_cutoff_sequence: events.last().map(|event| event.sequence),
            source_prompt_sequence: None,
            forked_at_ms: self.next_activity_timestamp_ms(),
            kind: SessionForkKind::Clone,
        };
        let session = self
            .copy_session_events_with_execution(
                name,
                working_directory,
                events,
                marker,
                Some(provenance),
            )
            .await?;
        Ok(session)
    }

    /// Create a fresh isolated background execution session.
    ///
    /// The child inherits the parent's normalized working directory unless `working_directory`
    /// names an explicit worktree.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent is unavailable, provenance is invalid, or persistence
    /// fails.
    pub async fn create_fresh_execution_session(
        &self,
        name: Option<String>,
        mut provenance: ExecutionSessionProvenance,
        working_directory: Option<PathBuf>,
    ) -> Result<SessionSummary, SessionError> {
        provenance.context_mode = ExecutionSessionContextMode::FreshIsolated;
        provenance.parent_generation = None;
        let parent = self.session_summary(provenance.parent_session_id).await?;
        let working_directory =
            working_directory.unwrap_or_else(|| parent.working_directory.clone());
        self.create_session_with_execution(name, working_directory, Some(provenance))
            .await
    }

    /// Clone a background execution session from one exact parent generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent generation changed, provenance is invalid, the requested
    /// directory is inconsistent, or persistence fails.
    pub async fn create_fixed_generation_execution_session(
        &self,
        name: Option<String>,
        mut provenance: ExecutionSessionProvenance,
        parent_generation: u64,
        working_directory: Option<PathBuf>,
    ) -> Result<SessionSummary, SessionError> {
        provenance.context_mode = ExecutionSessionContextMode::FixedGenerationFork;
        provenance.parent_generation = Some(parent_generation);
        let parent = self.session_summary(provenance.parent_session_id).await?;
        let working_directory =
            working_directory.unwrap_or_else(|| parent.working_directory.clone());
        self.create_session_with_execution(name, working_directory, Some(provenance))
            .await
    }

    /// Set or clear a persisted composer draft for a session.
    ///
    /// Empty text clears the persisted draft without appending a session event.
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or the draft cannot be written.
    pub async fn set_session_composer_draft(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle
            .set_composer_draft(text, self.next_activity_timestamp_ms())
            .await
    }

    /// Return a persisted composer draft for a session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or the draft cannot be read.
    pub async fn session_composer_draft(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.composer_draft().await
    }

    /// Set or clear a launch-cwd-scoped draft-session composer draft.
    ///
    /// Empty text clears the persisted draft without creating a session.
    ///
    /// # Errors
    ///
    /// Returns an error if the draft cannot be written.
    pub async fn set_draft_session_composer_draft(
        &self,
        launch_working_directory: PathBuf,
        text: String,
    ) -> Result<(), SessionError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let launch_working_directory = normalize_working_directory(&launch_working_directory);
        let db = db::GlobalSessionDb::open_turso_in_root(&store.root_path()).await?;
        db.set_draft_session_composer_draft(
            &launch_working_directory,
            &text,
            self.next_activity_timestamp_ms(),
        )
        .await?;
        Ok(())
    }

    /// Return a launch-cwd-scoped draft-session composer draft.
    ///
    /// # Errors
    ///
    /// Returns an error if the draft cannot be read.
    pub async fn draft_session_composer_draft(
        &self,
        launch_working_directory: PathBuf,
    ) -> Result<Option<String>, SessionError> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let launch_working_directory = normalize_working_directory(&launch_working_directory);
        let db = db::GlobalSessionDb::open_turso_in_root(&store.root_path()).await?;
        Ok(db
            .draft_session_composer_draft(&launch_working_directory)
            .await?)
    }

    /// List known sessions from the session catalog.
    pub async fn list_sessions(&self, working_directory: &Path) -> Vec<SessionSummary> {
        self.list_sessions_with_background(working_directory, false)
            .await
    }

    /// List sessions for a working directory, optionally including background execution sessions.
    pub async fn list_sessions_with_background(
        &self,
        working_directory: &Path,
        include_background: bool,
    ) -> Vec<SessionSummary> {
        self.start_catalog_load();
        let sessions = self.cached_sessions(working_directory).await;
        if include_background {
            sessions
        } else {
            sessions
                .into_iter()
                .filter(SessionSummary::is_picker_visible)
                .collect()
        }
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

    /// List all already-loaded sessions, including inspectable background execution sessions.
    pub async fn all_session_summaries(&self) -> Vec<SessionSummary> {
        self.all_session_catalog_entries()
            .await
            .into_iter()
            .map(|entry| entry.summary)
            .collect()
    }

    pub async fn all_session_catalog_entries(&self) -> Vec<SessionCatalogEntry> {
        self.start_catalog_load();
        let handles = {
            let inner = self.inner.lock().await;
            inner.sessions.values().cloned().collect::<Vec<_>>()
        };
        handles
            .into_iter()
            .map(|handle| SessionCatalogEntry::from_snapshot(handle.snapshot()))
            .collect()
    }

    /// Return true once the persistent session catalog has been discovered.
    #[must_use]
    pub fn catalog_loaded(&self) -> bool {
        matches!(self.catalog_status(), CatalogLoadStatus::Loaded)
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
        self.ensure_session_loaded(session_id).await?;
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
        self.release_persistent_idle_session_resources(session_id)
            .await;
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
        self.ensure_session_loaded(session_id).await?;
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
        self.release_persistent_idle_session_resources(session_id)
            .await;
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
    /// * the persistent session data cannot be removed
    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionError> {
        let handle = self.session_handle(session_id).await?;
        let session = handle.summary().await?;
        if handle.client_count() != 0 {
            return Err(SessionError::ConnectedClients(session_id));
        }
        let _lease = {
            let mut inner = self.inner.lock().await;
            inner
                .sessions
                .remove(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            let lease = inner.leases.remove(&session_id);
            inner.load_gates.remove(&session_id);
            lease
        };
        if let Some(store) = &self.store {
            let catalog = match store
                .lease_owner()
                .build_fingerprint
                .as_deref()
                .map(safe_catalog_namespace)
            {
                Some(namespace) => {
                    db::GlobalSessionDb::open_turso_in_root_namespace(
                        &store.root_path(),
                        &namespace,
                    )
                    .await
                }
                None => db::GlobalSessionDb::open_turso_in_root(&store.root_path()).await,
            };
            if let Ok(catalog) = catalog
                && let Err(error) = catalog.delete_session(session_id).await
            {
                eprintln!("failed to remove session from scoped catalog: {error}");
            }
            let session_dir = db::session_dir_path(&store.root_path(), session_id);
            if session_dir.exists() {
                match std::fs::remove_dir_all(&session_dir) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(SessionStoreError::Io(error).into()),
                }
            }
        }
        handle.shutdown().await?;
        Ok(session)
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

    /// Return the complete durable event history for explicit export/debug/history commands only.
    ///
    /// This API performs a full canonical event read. Do not call it from normal UI, attach,
    /// prompt/model-context, catalog, or background maintenance paths. Use bounded pages,
    /// projection windows, or typed read models for runtime flows.
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
        let Some(store) = &self.store else {
            return Err(SessionError::NotFound(session_id));
        };
        let db_path = db::session_db_path(&store.root_path(), session_id);
        if !db_path.exists() {
            return Err(SessionError::NotFound(session_id));
        }
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        Ok(db.history_page(query).await?)
    }

    /// Return canonical plugin status-note events for one stable note identity.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn plugin_status_note_events(
        &self,
        session_id: SessionId,
        plugin_id: &str,
        note_id: &str,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let Some(store) = &self.store else {
            return Err(SessionError::NotFound(session_id));
        };
        let db_path = db::session_db_path(&store.root_path(), session_id);
        if !db_path.exists() {
            return Err(SessionError::NotFound(session_id));
        }
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        Ok(db.plugin_status_note_events(plugin_id, note_id).await?)
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
        let projection_window = match handle.projection_window(request.clone()).await {
            Ok(window) => {
                self.metrics
                    .increment_counter("session.manager.projection_window.fast_path_total");
                Ok(window)
            }
            Err(SessionError::UnsupportedProjectionWindow) => {
                self.metrics
                    .increment_counter("session.manager.projection_window.fallback_total");
                self.projection_window_from_recent_history(session_id, request)
                    .await
            }
            Err(error) => Err(error),
        }?;
        Ok(projection_window)
    }

    async fn projection_window_from_recent_history(
        &self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        let limit = request.limits.max_events_scanned.max(1);
        let page = self
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    limit,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await?;
        crate::projection::projection_window_from_events(&page.events, &request)
            .ok_or(SessionError::UnsupportedProjectionWindow)
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

    /// Fork a session from a selected user prompt into a new session.
    ///
    /// The selected prompt is returned as draft text and is not appended to the new session.
    ///
    /// # Errors
    ///
    /// Returns an error when the source session does not exist, the prompt cannot be found,
    /// or the copied events cannot be persisted.
    pub async fn fork_session_from_prompt(
        &self,
        source_session_id: SessionId,
        prompt_sequence: u64,
        name: Option<String>,
    ) -> Result<SessionForkResult, SessionError> {
        let source = self.session_summary(source_session_id).await?;
        let events = self.session_history(source_session_id).await?;
        let Some(prompt_event) = events
            .iter()
            .find(|event| event.sequence == prompt_sequence)
        else {
            return Err(SessionError::ForkPromptNotFound {
                session_id: source_session_id,
                sequence: prompt_sequence,
            });
        };
        let SessionEventKind::UserMessage { text: draft, .. } = &prompt_event.kind else {
            return Err(SessionError::ForkPromptNotFound {
                session_id: source_session_id,
                sequence: prompt_sequence,
            });
        };
        let copied_events = events
            .iter()
            .filter(|event| event.sequence < prompt_sequence)
            .cloned()
            .collect::<Vec<_>>();
        let source_title = Some(source.display_title().to_string());
        let forked_at_ms = self.next_activity_timestamp_ms();
        let fork_name = normalize_session_name(name)
            .or_else(|| Some(format!("[fork] {}", source.display_title())));
        let session = self
            .copy_session_events(
                fork_name,
                source.working_directory,
                copied_events,
                SessionEventKind::SessionForked {
                    source_session_id,
                    source_title,
                    source_cutoff_sequence: prompt_sequence.checked_sub(1),
                    source_prompt_sequence: Some(prompt_sequence),
                    forked_at_ms,
                    kind: SessionForkKind::Fork,
                },
            )
            .await?;
        Ok(SessionForkResult {
            session,
            draft: Some(draft.clone()),
        })
    }

    /// Clone a session's complete event history into a new session.
    ///
    /// # Errors
    ///
    /// Returns an error when the source session does not exist or the copied events cannot be
    /// persisted.
    pub async fn clone_session(
        &self,
        source_session_id: SessionId,
        name: Option<String>,
    ) -> Result<SessionForkResult, SessionError> {
        self.clone_session_at_generation(source_session_id, name, None)
            .await
    }

    /// Clone a session's complete history if its snapshot matches an expected generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the source session does not exist, the source generation differs
    /// from `expected_generation`, or copied events cannot be persisted.
    pub async fn clone_session_at_generation(
        &self,
        source_session_id: SessionId,
        name: Option<String>,
        expected_generation: Option<u64>,
    ) -> Result<SessionForkResult, SessionError> {
        let events = self.session_history(source_session_id).await?;
        let source_cutoff_sequence = events.last().map_or(0, |event| event.sequence);
        if let Some(expected) = expected_generation
            && source_cutoff_sequence != expected
        {
            return Err(SessionError::CloneGenerationChanged {
                session_id: source_session_id,
                expected,
                current: source_cutoff_sequence,
            });
        }
        let source = self.session_summary(source_session_id).await?;
        if let Some(expected) = expected_generation {
            let current = self
                .session_history_page(
                    source_session_id,
                    SessionHistoryQuery {
                        cursor: None,
                        limit: 1,
                        direction: SessionHistoryDirection::Backward,
                    },
                )
                .await?
                .events
                .first()
                .map_or(0, |event| event.sequence);
            if current != expected {
                return Err(SessionError::CloneGenerationChanged {
                    session_id: source_session_id,
                    expected,
                    current,
                });
            }
        }
        let source_title = Some(source.display_title().to_string());
        let source_cutoff_sequence = events.last().map(|event| event.sequence);
        let forked_at_ms = self.next_activity_timestamp_ms();
        let clone_name = normalize_session_name(name)
            .or_else(|| Some(format!("[clone] {}", source.display_title())));
        let session = self
            .copy_session_events(
                clone_name,
                source.working_directory,
                events,
                SessionEventKind::SessionForked {
                    source_session_id,
                    source_title,
                    source_cutoff_sequence,
                    source_prompt_sequence: None,
                    forked_at_ms,
                    kind: SessionForkKind::Clone,
                },
            )
            .await?;
        Ok(SessionForkResult {
            session,
            draft: None,
        })
    }

    async fn copy_session_events(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        events: Vec<SessionEvent>,
        marker: SessionEventKind,
    ) -> Result<SessionSummary, SessionError> {
        self.copy_session_events_with_execution(name, working_directory, events, marker, None)
            .await
    }

    async fn copy_session_events_with_execution(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        events: Vec<SessionEvent>,
        marker: SessionEventKind,
        execution: Option<ExecutionSessionProvenance>,
    ) -> Result<SessionSummary, SessionError> {
        let session = self
            .create_session_record(name, working_directory, execution)
            .await?;
        let handle = self.session_handle(session.id).await?;
        let mut sequence_map = BTreeMap::new();
        for event in events {
            if !is_copyable_fork_event(&event.kind) {
                continue;
            }
            let kind = rewrite_copied_event_kind(event.kind.clone(), &sequence_map);
            let copied = handle
                .append_event_with_provenance(
                    kind,
                    Some(copy_event_provenance(&event)),
                    self.next_activity_timestamp_ms(),
                )
                .await?;
            sequence_map.insert(event.sequence, copied.sequence);
        }
        let marker_event = handle
            .append_event(marker.clone(), self.next_activity_timestamp_ms())
            .await?;
        let mut summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session.id)
            .await;
        summary.fork = session_fork_summary_from_marker(&marker);
        self.publish_committed_mutation(marker_event, summary.clone());
        Ok(summary)
    }

    /// Return active tool runs from the DB read model.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist, or
    /// [`SessionError::ProjectionStale`] when the DB projection is not current.
    pub async fn active_tool_runs(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<db::ToolRun>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.active_tool_runs().await
    }

    /// Return active runtime-work rows through the session actor's DB connection.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist, or
    /// [`SessionError::ProjectionStale`] when the DB projection is not current.
    pub async fn active_runtime_work(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<db::RuntimeWorkProjection>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.active_runtime_work().await
    }

    /// Return latest runtime-work rows from the DB read model.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist, or
    /// [`SessionError::ProjectionStale`] when the DB projection is not current.
    pub async fn runtime_work_history(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<db::RuntimeWorkProjection>, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::DbUnavailable(session_id))?;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        let expected_last_sequence = db.last_event_sequence().await?.unwrap_or(0);
        let checkpoint = db
            .materialized_projection_checkpoint(db::MaterializedProjection::RuntimeWork)
            .await?;
        if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
            return Ok(db.runtime_work_history(limit).await?);
        }
        Err(SessionError::ProjectionStale {
            session_id,
            projection: "runtime_work",
            checkpoint,
            expected: expected_last_sequence,
        })
    }

    /// Return the current context generation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_context_epoch(&self, session_id: SessionId) -> Result<u64, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_context_epoch().await
    }

    /// Return authoritative current context occupancy with a bounded projection lookup.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist, or a projection error
    /// when the occupancy read model is not trustworthy.
    pub async fn current_context_occupancy(
        &self,
        session_id: SessionId,
    ) -> Result<Option<bcode_session_models::RequestContextOccupancy>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_context_occupancy().await
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

    /// Return the latest session-specific runtime selection state.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_runtime_selection(
        &self,
        session_id: SessionId,
    ) -> Result<SessionRuntimeSelection, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_runtime_selection().await
    }

    /// Return the latest session-specific model selection if one has been set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_model_selection(
        &self,
        session_id: SessionId,
    ) -> Result<(Option<String>, Option<String>), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_model_selection().await
    }

    /// Return the latest session-specific reasoning selection if one has been set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_reasoning_selection(
        &self,
        session_id: SessionId,
    ) -> Result<(Option<String>, Option<String>), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_reasoning_selection().await
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

    /// Subscribe to a session's committed/live events without registering as an attached client.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist.
    pub async fn subscribe_session_events(
        &self,
        session_id: SessionId,
    ) -> Result<SessionEventSubscription, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let (session, events, live_events) = handle.subscribe_events().await?;
        Ok(SessionEventSubscription {
            session,
            events,
            live_events,
        })
    }

    /// Attach a client to an existing session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist.
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
        let attach_timer = self.metrics.timer();
        let result = handle.attach(client_id, AttachMode::Full).await;
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
    /// Returns an error when the session does not exist.
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
        let attach_timer = self.metrics.timer();
        let result = handle.attach(client_id, AttachMode::Recent { limit }).await;
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
    /// * the projection request is not supported
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
        let projection_window = match handle.projection_window(request.clone()).await {
            Ok(window) => {
                self.metrics
                    .increment_counter("session.manager.attach_projection_window.fast_path_total");
                window
            }
            Err(SessionError::UnsupportedProjectionWindow) => {
                self.metrics
                    .increment_counter("session.manager.attach_projection_window.fallback_total");
                self.projection_window_from_recent_history(session_id, request)
                    .await?
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
        let attach_timer = self.metrics.timer();
        let mut attachment = handle
            .attach(client_id, AttachMode::ProjectionWindow { history })
            .await?;
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
    /// Returns an error when the detach command cannot be delivered.
    pub async fn detach_session(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<bool, SessionError> {
        let Ok(handle) = self.session_handle(session_id).await else {
            return Ok(false);
        };
        handle.detach(client_id).await
    }

    /// Release cached per-session resources when no clients remain attached.
    ///
    /// The session stays visible through its lightweight summary, and its compatibility lease
    /// remains held for the loaded actor lifetime. Only cached database/event state is released;
    /// this prevents an incompatible daemon from claiming the session between idle operations.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session actor is unavailable.
    pub async fn release_idle_session_resources(
        &self,
        session_id: SessionId,
    ) -> Result<bool, SessionError> {
        let started_at = Instant::now();
        let handle = self
            .inner
            .lock()
            .await
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or(SessionError::NotFound(session_id))?;
        let released = handle.release_idle_resources().await?;
        self.metrics.record_histogram(
            "session.manager.release_idle.duration_ms",
            elapsed_ms(started_at),
        );
        if released {
            self.metrics
                .increment_counter("session.manager.release_idle.released_total");
        }
        Ok(released)
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
        self.append_user_message_with_origin(session_id, client_id, text, None)
            .await
    }

    /// Append a user message carrying optional generic turn-origin metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the user-message event cannot be persisted
    pub async fn append_user_message_with_origin(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
        origin: Option<bcode_session_models::TurnOrigin>,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.admit_turn_result(
            session_id,
            client_id,
            text,
            bcode_session_models::TurnAdmissionMetadata {
                origin,
                ..bcode_session_models::TurnAdmissionMetadata::default()
            },
        )
        .await
        .map(|result| result.events)
    }

    /// Atomically admit an ordinary turn and return its durable receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist, metadata is invalid, or persistence fails.
    pub async fn admit_turn(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<bcode_session_models::TurnAdmission, SessionError> {
        self.admit_turn_with_events(session_id, client_id, text, admission)
            .await
            .map(|(admission, _)| admission)
    }

    /// Atomically admit an ordinary turn and return both its durable receipt and committed events.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist, metadata is invalid, or persistence fails.
    pub async fn admit_turn_with_events(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<
        (
            bcode_session_models::TurnAdmission,
            Vec<bcode_session_models::SessionEvent>,
        ),
        SessionError,
    > {
        self.admit_turn_result(session_id, client_id, text, admission)
            .await
            .map(|result| (result.admission, result.events))
    }

    async fn admit_turn_result(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<actor::TurnAdmissionResult, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let result = handle
            .append_user_message(client_id, text, admission, activity_timestamp_ms)
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        for event in &result.events {
            self.publish_committed_mutation(event.clone(), summary.clone());
        }
        Ok(result)
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
        input: AppendToolCallRequestedInput,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ToolCallRequested {
                tool_call_id: input.tool_call_id,
                tool_name: input.tool_name,
                arguments_json: input.arguments_json,
                producer_plugin_id: input.producer_plugin_id,
                working_directory: input.working_directory,
                request_visual: input.request_visual,
                legacy_request_presentation: input.legacy_request_presentation,
            },
        )
        .await
    }

    /// Append a generic terminal invocation result record to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_tool_invocation_result(
        &self,
        session_id: SessionId,
        record: bcode_session_models::ToolInvocationResultRecord,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ToolInvocationResultRecorded { record },
        )
        .await
    }

    /// Append an interactive tool request event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_interactive_tool_request_created(
        &self,
        session_id: SessionId,
        event: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, event).await
    }

    /// Append an interactive tool resolution event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_interactive_tool_request_resolved(
        &self,
        session_id: SessionId,
        event: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, event).await
    }

    /// Publish a live-only event to currently attached session subscribers.
    ///
    /// Live events are not appended to durable history and may be coalesced or
    /// dropped by callers under backpressure. They are intended for high-rate
    /// presentation streams whose final semantic result is recorded separately.
    /// Returns `None` when the session is not loaded or has no active live subscribers.
    pub async fn publish_live_event(
        &self,
        session_id: SessionId,
        event: SessionLiveEventKind,
    ) -> Option<SessionLiveEvent> {
        let handle = {
            let inner = self.inner.lock().await;
            inner.sessions.get(&session_id).cloned()?
        };
        handle.publish_live_event(event).await.ok().flatten()
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
        work_id: bcode_session_models::WorkId,
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
        work_id: bcode_session_models::WorkId,
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
        request: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        debug_assert!(matches!(
            request,
            SessionEventKind::PermissionRequested { .. }
        ));
        self.append_event(session_id, request).await
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

    /// Append a reasoning-changed event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_reasoning_changed(
        &self,
        session_id: SessionId,
        effort: Option<String>,
        summary: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ReasoningChanged { effort, summary },
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

    /// Set the current in-memory agent selection for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or is not writable.
    pub async fn set_current_agent(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> Result<(), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.set_current_agent(agent_id).await
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

    /// Append a provider-native context compaction boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_provider_context_compacted(
        &self,
        session_id: SessionId,
        snapshot: bcode_session_models::ProviderContextSnapshot,
        compacted_through_sequence: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            },
        )
        .await
    }

    /// Append a context occupancy observation.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_request_context_observed(
        &self,
        session_id: SessionId,
        observation: bcode_session_models::RequestContextObservation,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::RequestContextObserved { observation },
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

    /// Resolve one finalized generic artifact reference through its bounded projection.
    ///
    /// # Errors
    ///
    /// Returns an error when the session database is unavailable, the projection is stale, or the
    /// projected row cannot be read.
    pub async fn finalized_artifact_reference(
        &self,
        session_id: SessionId,
        artifact_id: &str,
        reference_key: &str,
    ) -> Result<Option<db::FinalizedArtifactReference>, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::DbUnavailable(session_id))?;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        let reference = db
            .finalized_artifact_reference(artifact_id, reference_key)
            .await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        Ok(reference)
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
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle.append_event(kind, activity_timestamp_ms).await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        self.publish_committed_mutation(event.clone(), summary);
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
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle
            .append_event_with_provenance(kind, provenance, activity_timestamp_ms)
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        self.publish_committed_mutation(event.clone(), summary);
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
    pub(crate) fn from_catalog_summary(summary: SessionSummary) -> Self {
        let (sender, _) = broadcast::channel(512);
        let live_events = SessionLiveEventBroker::new(512);
        let working_directory = normalize_working_directory(&summary.working_directory);
        Self {
            summary,
            working_directory,
            clients: BTreeSet::new(),
            events: None,
            next_sequence: 0,
            event_count: 0,
            has_user_message: false,
            current_provider: None,
            current_model: None,
            reasoning_effort: None,
            reasoning_summary: None,
            current_agent: None,
            latest_compaction_sequence: None,
            context_epoch: 0,
            context_occupancy: None,
            turn_receipts: BTreeMap::new(),
            total_metered_tokens: 0,
            load_status: SessionLoadStatusKind::SummaryOnly,
            sender,
            live_events,
        }
    }

    pub(crate) fn from_db_state(
        state: db::SessionDbState,
        created_at_ms: u64,
        updated_at_ms: u64,
    ) -> Self {
        let (sender, _) = broadcast::channel(512);
        let live_events = SessionLiveEventBroker::new(512);
        let working_directory = normalize_working_directory(&state.working_directory);
        let title_source = if state.title.is_some() {
            SessionTitleSource::Explicit
        } else {
            SessionTitleSource::EmptyDraft
        };
        Self {
            summary: SessionSummary {
                id: state.session_id,
                name: state.title.clone(),
                explicit_name: state.title,
                derived_title: None,
                title_source,
                client_count: 0,
                created_at_ms,
                updated_at_ms,
                working_directory: working_directory.clone(),
                import: None,
                fork: None,
                execution: state.execution.map(|provenance| {
                    Box::new(bcode_session_models::ExecutionSessionSummary {
                        provenance,
                        visibility: state.visibility,
                    })
                }),
            },
            working_directory,
            clients: BTreeSet::new(),
            events: None,
            next_sequence: state.last_event_seq.saturating_add(1),
            event_count: usize::try_from(state.last_event_seq.saturating_add(1))
                .unwrap_or(usize::MAX),
            has_user_message: state.has_user_message,
            current_provider: state.current_provider,
            current_model: state.current_model,
            reasoning_effort: state.reasoning_effort,
            reasoning_summary: state.reasoning_summary,
            current_agent: state.current_agent,
            latest_compaction_sequence: state.latest_compaction_sequence,
            context_epoch: state.latest_compaction_sequence.unwrap_or_default(),
            context_occupancy: None,
            turn_receipts: BTreeMap::new(),
            total_metered_tokens: 0,
            load_status: SessionLoadStatusKind::Current,
            sender,
            live_events,
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

    const fn build_next_event(&self, kind: SessionEventKind, timestamp_ms: u64) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.next_sequence,
            timestamp_ms,
            session_id: self.summary.id,
            provenance: None,
            kind,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply_persisted_event(&mut self, event: SessionEvent, activity_timestamp_ms: u64) {
        self.summary.updated_at_ms = activity_timestamp_ms;
        self.next_sequence += 1;
        self.event_count = self.event_count.saturating_add(1);
        match &event.kind {
            SessionEventKind::ExecutionSessionCreated {
                provenance,
                visibility,
            } => {
                self.summary.execution =
                    Some(Box::new(bcode_session_models::ExecutionSessionSummary {
                        provenance: (**provenance).clone(),
                        visibility: *visibility,
                    }));
            }
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
            SessionEventKind::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            } => {
                self.summary.fork = Some(SessionForkSummary {
                    source_session_id: *source_session_id,
                    source_title: source_title.clone(),
                    source_cutoff_sequence: *source_cutoff_sequence,
                    source_prompt_sequence: *source_prompt_sequence,
                    forked_at_ms: *forked_at_ms,
                    kind: *kind,
                });
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
                self.context_epoch = event.sequence;
                self.context_occupancy = None;
            }
            SessionEventKind::ReasoningChanged { effort, summary } => {
                self.reasoning_effort.clone_from(effort);
                self.reasoning_summary.clone_from(summary);
            }
            SessionEventKind::AgentChanged { agent_id } => {
                self.current_agent = Some(agent_id.clone());
            }
            SessionEventKind::ContextCompacted {
                compacted_through_sequence,
                ..
            }
            | SessionEventKind::ProviderContextCompacted {
                compacted_through_sequence,
                ..
            } => {
                self.latest_compaction_sequence = Some(*compacted_through_sequence);
                self.context_epoch = event.sequence;
                self.context_occupancy = None;
            }
            SessionEventKind::RequestContextObserved { observation } => {
                self.context_occupancy = bcode_session_models::RequestContextOccupancy::reconcile(
                    self.context_occupancy.as_ref(),
                    self.context_epoch,
                    event.sequence,
                    observation.clone(),
                );
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

fn session_fork_summary_from_marker(marker: &SessionEventKind) -> Option<SessionForkSummary> {
    if let SessionEventKind::SessionForked {
        source_session_id,
        source_title,
        source_cutoff_sequence,
        source_prompt_sequence,
        forked_at_ms,
        kind,
    } = marker
    {
        Some(SessionForkSummary {
            source_session_id: *source_session_id,
            source_title: source_title.clone(),
            source_cutoff_sequence: *source_cutoff_sequence,
            source_prompt_sequence: *source_prompt_sequence,
            forked_at_ms: *forked_at_ms,
            kind: *kind,
        })
    } else {
        None
    }
}

fn copy_event_provenance(event: &SessionEvent) -> SessionEventProvenance {
    let source_locator = format!(
        "bcode://session/{}/event/{}",
        event.session_id, event.sequence
    );
    SessionEventProvenance {
        source_event_id: Some(event.sequence.to_string()),
        source_timestamp_ms: None,
        source_locator: Some(source_locator),
    }
}

const fn is_copyable_fork_event(kind: &SessionEventKind) -> bool {
    !matches!(
        kind,
        SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ClientAttached { .. }
            | SessionEventKind::ClientDetached { .. }
            | SessionEventKind::SessionForked { .. }
    )
}

fn rewrite_copied_event_kind(
    kind: SessionEventKind,
    sequence_map: &BTreeMap<u64, u64>,
) -> SessionEventKind {
    match kind {
        SessionEventKind::ContextCompacted {
            summary,
            compacted_through_sequence,
        } => SessionEventKind::ContextCompacted {
            summary,
            compacted_through_sequence: sequence_map
                .get(&compacted_through_sequence)
                .copied()
                .unwrap_or(compacted_through_sequence),
        },
        SessionEventKind::ProviderContextCompacted {
            snapshot,
            compacted_through_sequence,
        } => SessionEventKind::ProviderContextCompacted {
            snapshot,
            compacted_through_sequence: sequence_map
                .get(&compacted_through_sequence)
                .copied()
                .unwrap_or(compacted_through_sequence),
        },
        other => other,
    }
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
                    timestamp_ms: event.timestamp_ms,
                    text: text.clone(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn model_context_events_from_history(history: &[SessionEvent]) -> Vec<SessionEvent> {
    let latest_compaction = history
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                SessionEventKind::ContextCompacted { .. }
                    | SessionEventKind::ProviderContextCompacted { .. }
            )
        })
        .max_by_key(|event| event.sequence);
    let Some(marker) = latest_compaction else {
        return history.to_vec();
    };
    let compacted_through_sequence = match &marker.kind {
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        }
        | SessionEventKind::ProviderContextCompacted {
            compacted_through_sequence,
            ..
        } => *compacted_through_sequence,
        _ => unreachable!("marker selection accepts only compaction events"),
    };
    let mut retained = history
        .iter()
        .filter(|event| event.sequence > compacted_through_sequence)
        .filter(|event| event.sequence != marker.sequence)
        .filter(|event| {
            !matches!(
                event.kind,
                SessionEventKind::ContextCompacted { .. }
                    | SessionEventKind::ProviderContextCompacted { .. }
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    retained.sort_by_key(|event| event.sequence);
    std::iter::once(marker.clone()).chain(retained).collect()
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn canonical_session_id_from_dir(path: &Path) -> Option<SessionId> {
    path.is_dir()
        .then(|| path.file_name()?.to_str()?.parse::<SessionId>().ok())
        .flatten()
}

fn safe_catalog_namespace(value: &str) -> String {
    let namespace = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if namespace.is_empty() {
        "unknown".to_string()
    } else {
        namespace
    }
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

#[cfg(test)]
mod tests {
    use super::{
        AppendToolCallRequestedInput, CURRENT_SESSION_FORMAT_EPOCH,
        MAX_DURABLE_GENERIC_EVENT_BYTES, MIGRATION_BACKUP_BUFFER_BYTES, MigrationBackupCopyFault,
        SESSION_FORMAT_FAMILY, SESSION_MANIFEST_SCHEMA_VERSION, SessionCatalogLoadStatus,
        SessionError, SessionHealth, SessionLeaseOwnerContext, SessionLoadStatusKind,
        SessionManager, SessionMigrationStage, SessionOpenFailureKind, SessionOpenOperationId,
        SessionOpenTerminalOutcome, SessionStore, copy_and_hash_backup_files,
        create_verified_migration_backup_blocking, db, lease, migration_backup_files, persisted,
        shared_execution_session, verify_backup_files,
    };
    use bcode_metrics::MetricsRegistry;
    use bcode_session_models::{
        ExecutionSessionContextMode, ExecutionSessionProvenance, SessionVisibility,
    };
    use std::time::Duration;
    use switchy::database::query::FilterableQuery;

    fn session_database_files(
        root: &std::path::Path,
        session_id: SessionId,
    ) -> Vec<(String, Vec<u8>)> {
        let path = db::session_db_path(root, session_id);
        let file_name = path
            .file_name()
            .expect("database filename")
            .to_string_lossy();
        let mut files = std::fs::read_dir(path.parent().expect("database parent"))
            .expect("database directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(file_name.as_ref())
            })
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).expect("database bytes"),
                )
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    #[tokio::test]
    async fn session_health_is_byte_for_byte_non_mutating() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("health immutability".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should create");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let before = session_database_files(&root, session.id);

        assert_eq!(
            manager.session_health(session.id).await,
            SessionHealth::Ready
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = session_database_files(&root, session.id);
        assert_eq!(
            after, before,
            "session health must not mutate DB or sidecars"
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn session_health_reports_incompatible_storage_writer_epoch() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("writer health".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let db = db::SessionDb::open_turso_in_root(session.id, &root)
            .await
            .expect("open session db");
        let future_epoch = u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH).saturating_add(1);
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::DatabaseValue::Int64(
                    i64::try_from(future_epoch).expect("epoch fits"),
                ),
            )
            .execute(db.database())
            .await
            .expect("set future writer epoch");

        assert_eq!(
            manager.session_health(session.id).await,
            SessionHealth::WriterIncompatible {
                actual: Some(future_epoch),
                expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            }
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn bounded_history_does_not_require_runtime_lease_or_writer_compatibility() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("read-only incompatible history".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session should create");
        let db = db::SessionDb::open_turso_in_root(session.id, &root)
            .await
            .expect("open session db");
        let future_epoch = u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH).saturating_add(1);
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::DatabaseValue::Int64(
                    i64::try_from(future_epoch).expect("epoch fits"),
                ),
            )
            .execute(db.database())
            .await
            .expect("set future writer epoch");
        manager
            .inner
            .lock()
            .await
            .sessions
            .remove(&session.id)
            .expect("remove cached actor handle");
        manager.inner.lock().await.leases.remove(&session.id);

        let page = manager
            .session_history_page(
                session.id,
                SessionHistoryQuery {
                    cursor: None,
                    direction: bcode_session_models::SessionHistoryDirection::Forward,
                    limit: 10,
                },
            )
            .await
            .expect("bounded history should remain inspectable");
        assert_eq!(page.events.len(), 1);
        assert!(matches!(
            page.events[0].kind,
            SessionEventKind::SessionCreated { .. }
        ));
        assert!(matches!(
            manager.ensure_session_loaded(session.id).await,
            Err(SessionError::Db(
                db::SessionDbError::WriterIncompatible { .. }
            ))
        ));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn migrated_opaque_session_health_is_degraded_and_attach_fails_closed() {
        let root = unique_temp_dir();
        let session_id = SessionId::new();
        let db = db::SessionDb::open_turso_in_root(session_id, &root)
            .await
            .expect("open session DB");
        db.append_event(&bcode_session_models::SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::SessionCreated {
                name: Some("opaque health".to_owned()),
                working_directory: test_working_directory(),
            },
        })
        .await
        .expect("append session created");
        db.database()
            .insert("events")
            .value("event_seq", switchy::database::DatabaseValue::Int64(1))
            .value("event_type", "future_event_kind")
            .value(
                "schema_version",
                switchy::database::DatabaseValue::Int32(i32::from(
                    CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                )),
            )
            .value("created_at_ms", switchy::database::DatabaseValue::Int64(2))
            .value(
                "payload",
                serde_json::json!({
                    "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    "sequence": 1,
                    "timestamp_ms": 2,
                    "session_id": session_id,
                    "kind": { "future_event_kind": { "value": true } }
                })
                .to_string(),
            )
            .execute(db.database())
            .await
            .expect("insert opaque event");
        db.database()
            .update("session_storage_contract")
            .value("writer_epoch", switchy::database::DatabaseValue::Int64(3))
            .where_eq("contract_id", switchy::database::DatabaseValue::Int32(1))
            .execute(db.database())
            .await
            .expect("mark epoch three");
        db.database()
            .delete("__bcode_session_migrations")
            .where_in(
                "id",
                vec![
                    switchy::database::DatabaseValue::String(
                        "028_session_compatibility_state".to_owned(),
                    ),
                    switchy::database::DatabaseValue::String(
                        "029_session_compatibility_issues".to_owned(),
                    ),
                    switchy::database::DatabaseValue::String(
                        "030_session_state_visibility_column".to_owned(),
                    ),
                    switchy::database::DatabaseValue::String(
                        "031_session_state_execution_provenance_column".to_owned(),
                    ),
                ],
            )
            .execute(db.database())
            .await
            .expect("remove epoch four migrations");
        db.database()
            .exec_raw("ALTER TABLE session_state DROP COLUMN execution_provenance")
            .await
            .expect("drop execution provenance");
        db.database()
            .exec_raw("ALTER TABLE session_state DROP COLUMN visibility")
            .await
            .expect("drop visibility");
        db.database()
            .exec_raw("DROP TABLE session_compatibility_issues")
            .await
            .expect("drop issues");
        db.database()
            .exec_raw("DROP TABLE session_compatibility_state")
            .await
            .expect("drop state");
        drop(db);

        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let prepared = manager
            .prepare_session_open(session_id)
            .await
            .expect("degraded preparation should start");
        let mut preparation = manager
            .subscribe_session_open_operation(session_id, prepared.operation_id)
            .await
            .expect("degraded preparation subscription");
        let prepared = preparation
            .wait_for(|snapshot| snapshot.outcome.is_some())
            .await
            .expect("terminal degraded preparation")
            .clone();
        assert_eq!(
            prepared.outcome,
            Some(SessionOpenTerminalOutcome::DegradedReadOnly { issue_count: 1 })
        );
        assert_eq!(manager.active_session_migration_count().await, 0);
        assert!(matches!(
            manager
                .attach_session_recent(session_id, ClientId::new(), 10)
                .await,
            Err(SessionError::Db(
                db::SessionDbError::CompatibilityDegraded { issue_count: 1 }
            ))
        ));
        assert_eq!(
            manager.session_health(session_id).await,
            SessionHealth::DegradedReadOnly { issue_count: 1 }
        );
        let page = manager
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    direction: bcode_session_models::SessionHistoryDirection::Forward,
                    limit: 10,
                },
            )
            .await
            .expect("bounded history should remain available");
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.compatibility_issues.len(), 1);

        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("migrated session DB should open");
        db.database()
            .update("events")
            .value("payload", "not valid JSON")
            .where_eq("event_seq", switchy::database::DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("opaque canonical payload should corrupt");
        drop(db);
        drop(manager);

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let attach = restored
            .attach_session_recent(session_id, ClientId::new(), 10)
            .await;
        assert!(
            matches!(
                attach,
                Err(SessionError::Db(
                    db::SessionDbError::CompatibilityDegraded { issue_count: 1 }
                ))
            ),
            "unexpected degraded attach result: {attach:?}"
        );
        assert_eq!(
            restored.session_health(session_id).await,
            SessionHealth::DegradedReadOnly { issue_count: 1 }
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn session_health_reports_missing_artifact_projection_as_stale() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should create");
        let session = manager
            .create_session(Some("health".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        assert_eq!(
            manager.session_health(session.id).await,
            SessionHealth::Ready
        );

        let db = db::SessionDb::open_turso_in_root(session.id, &root)
            .await
            .expect("session DB should open");
        db.database()
            .delete("projection_checkpoints")
            .where_eq("projection_name", "artifact_references")
            .execute(db.database())
            .await
            .expect("remove checkpoint");

        assert_eq!(
            manager.session_health(session.id).await,
            SessionHealth::ProjectionStale {
                projection: "artifact_references",
                checkpoint: None,
                expected: 0,
            }
        );
    }

    #[tokio::test]
    async fn durable_boundary_rejects_tool_stream_status_regardless_of_payload_size() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("stream-boundary".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        for message in [
            "status".to_owned(),
            "x".repeat(MAX_DURABLE_GENERIC_EVENT_BYTES),
        ] {
            let error = manager
                .append_event(
                    session.id,
                    SessionEventKind::ToolInvocationStream {
                        event: ToolInvocationStreamEvent::Status {
                            tool_call_id: "call".to_owned(),
                            sequence: 1,
                            message,
                        },
                    },
                )
                .await
                .expect_err("tool stream status must remain live-only");
            assert!(matches!(
                error,
                SessionError::LiveEventPersistenceRejected {
                    event_kind: "tool_invocation_stream"
                }
            ));
        }

        let large_semantic_message = "y".repeat(MAX_DURABLE_GENERIC_EVENT_BYTES + 1);
        manager
            .append_event(
                session.id,
                SessionEventKind::AssistantMessage {
                    text: large_semantic_message.clone(),
                },
            )
            .await
            .expect("semantic message is governed by its own domain, not the stream limit");
        assert!(
            manager
                .session_history(session.id)
                .await
                .expect("history")
                .iter()
                .any(|event| matches!(
                    &event.kind,
                    SessionEventKind::AssistantMessage { text } if text == &large_semantic_message
                ))
        );
    }

    #[tokio::test]
    async fn durable_boundary_rejects_live_only_tool_stream_events() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("live-boundary".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        let variants = [
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call".to_owned(),
                stream: bcode_session_models::ToolOutputStream::Pty,
                sequence: 1,
                text: "live".to_owned(),
                byte_len: 4,
            },
            ToolInvocationStreamEvent::VisualUpdate {
                tool_call_id: "call".to_owned(),
                sequence: 2,
                visual: bcode_session_models::PluginVisualDescriptor {
                    visual_id: None,
                    producer_plugin_id: Some("fixture.plugin".to_owned()),
                    schema: "fixture.visual".to_owned(),
                    schema_version: 1,
                    title: None,
                    subtitle: None,
                    payload: serde_json::json!({"cumulative": "live"}),
                },
                streaming: true,
            },
            ToolInvocationStreamEvent::ArtifactUpdate {
                tool_call_id: "call".to_owned(),
                sequence: 3,
                artifact_id: "artifact".to_owned(),
                reference_key: "recording".to_owned(),
                producer_plugin_id: "fixture.plugin".to_owned(),
                schema: "fixture.recording".to_owned(),
                schema_version: 1,
                content_type: Some("application/octet-stream".to_owned()),
                storage_uri: "recording.bin".to_owned(),
                committed_bytes: 4,
                revision: 1,
                availability: None,
                finalized: false,
            },
            ToolInvocationStreamEvent::LegacyPresentation {
                tool_call_id: "call".to_owned(),
                sequence: 4,
                presentation: bcode_session_models::LegacyToolPresentationEvent::Card(
                    bcode_session_models::LegacyToolCardPresentation {
                        target: bcode_session_models::LegacyToolPresentationTarget::Result,
                        title: "legacy".to_owned(),
                        subtitle: None,
                        sections: Vec::new(),
                    },
                ),
            },
            ToolInvocationStreamEvent::Status {
                tool_call_id: "call".to_owned(),
                sequence: 5,
                message: "legacy status".to_owned(),
            },
            ToolInvocationStreamEvent::Started {
                tool_call_id: "call".to_owned(),
                tool_name: "fixture.tool".to_owned(),
                sequence: 6,
                terminal: false,
                columns: None,
                rows: None,
                started_at_ms: None,
            },
            ToolInvocationStreamEvent::Finished {
                tool_call_id: "call".to_owned(),
                sequence: 7,
                is_error: false,
                finished_at_ms: None,
            },
        ];

        for event in variants {
            let error = manager
                .append_event(session.id, SessionEventKind::ToolInvocationStream { event })
                .await
                .expect_err("live-only event must be rejected");
            assert!(matches!(
                error,
                SessionError::LiveEventPersistenceRejected {
                    event_kind: "tool_invocation_stream"
                }
            ));
        }

        let history = manager
            .session_history(session.id)
            .await
            .expect("history should read");
        assert!(
            !history
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::ToolInvocationStream { .. }))
        );
    }

    #[tokio::test]
    async fn durable_boundary_blocks_thousands_of_cumulative_visual_snapshots() {
        const SNAPSHOTS: usize = 4_096;
        let manager = SessionManager::default();
        let session = manager
            .create_session(
                Some("visual-growth-guard".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session should create");
        let mut cumulative = String::new();
        for sequence in 1..=SNAPSHOTS {
            cumulative.push('x');
            let error = manager
                .append_event(
                    session.id,
                    SessionEventKind::ToolInvocationStream {
                        event: ToolInvocationStreamEvent::VisualUpdate {
                            tool_call_id: "shell-call".to_owned(),
                            sequence: u64::try_from(sequence).expect("sequence"),
                            visual: bcode_session_models::PluginVisualDescriptor {
                                visual_id: None,
                                producer_plugin_id: Some("fixture.shell".to_owned()),
                                schema: "fixture.shell".to_owned(),
                                schema_version: 1,
                                title: None,
                                subtitle: None,
                                payload: serde_json::json!({"output": cumulative}),
                            },
                            streaming: true,
                        },
                    },
                )
                .await
                .expect_err("every cumulative visual snapshot must be rejected");
            assert!(matches!(
                error,
                SessionError::LiveEventPersistenceRejected {
                    event_kind: "tool_invocation_stream"
                }
            ));
        }

        let history = manager
            .session_history(session.id)
            .await
            .expect("history should read");
        assert_eq!(history.len(), 1, "only session creation may be durable");
        assert_eq!(
            serde_json::to_vec(&history)
                .expect("history encoding")
                .len(),
            serde_json::to_vec(&history[..1])
                .expect("creation encoding")
                .len(),
            "canonical payload bytes must be independent of visual snapshot count"
        );
    }

    #[tokio::test]
    async fn reading_legacy_stream_events_does_not_rewrite_session_storage() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should create");
        let session = manager
            .create_session(Some("legacy-stream".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        let legacy_event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id: session.id,
            provenance: None,
            kind: SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "legacy-call".to_owned(),
                    stream: bcode_session_models::ToolOutputStream::Pty,
                    sequence: 1,
                    text: "legacy persisted bytes".to_owned(),
                    byte_len: 22,
                },
            },
        };
        let db = db::SessionDb::open_turso_in_root(session.id, &root)
            .await
            .expect("session DB should open");
        db.append_event(&legacy_event)
            .await
            .expect("legacy fixture should append below the current durable boundary");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let before = session_database_files(&root, session.id);

        let history = manager
            .session_history(session.id)
            .await
            .expect("history should read");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta { text, .. }
            } if text == "legacy persisted bytes"
        )));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = session_database_files(&root, session.id);

        assert_eq!(after, before);
    }

    async fn persistent_artifact_session_bytes(
        root: &std::path::Path,
        artifact_bytes: u64,
        transient_updates: usize,
    ) -> u64 {
        let manager = SessionManager::persistent(root).expect("manager should create");
        let session = manager
            .create_session(Some("artifact-size".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        let _attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");
        manager
            .append_tool_call_requested(
                session.id,
                AppendToolCallRequestedInput {
                    tool_call_id: "call-1".to_owned(),
                    producer_plugin_id: Some("fixture.plugin".to_owned()),
                    tool_name: "fixture.run".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                    request_visual: None,
                    legacy_request_presentation: None,
                },
            )
            .await
            .expect("request should append");
        for sequence in 0..transient_updates {
            manager
                .publish_live_event(
                    session.id,
                    SessionLiveEventKind::ToolOutputDelta {
                        event: ToolInvocationStreamEvent::OutputDelta {
                            tool_call_id: "call-1".to_owned(),
                            stream: bcode_session_models::ToolOutputStream::Pty,
                            sequence: u64::try_from(sequence).expect("sequence"),
                            text: "x".repeat(4_096),
                            byte_len: 4_096,
                        },
                    },
                )
                .await
                .expect("transient output should publish");
        }
        manager
            .append_event(
                session.id,
                SessionEventKind::ToolCallFinished {
                    tool_call_id: "call-1".to_owned(),
                    result: "bounded result".to_owned(),
                    is_error: false,
                    output: None,
                    semantic_result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(bcode_session_models::ToolArtifact {
                            artifact_id: "artifact-1".to_owned(),
                            producer_plugin_id: "fixture.plugin".to_owned(),
                            schema: "fixture.artifact".to_owned(),
                            schema_version: 1,
                            tool_call_id: Some("call-1".to_owned()),
                            title: None,
                            metadata: serde_json::Value::Null,
                            refs: vec![bcode_session_models::ToolArtifactRef {
                                key: "complete_output".to_owned(),
                                content_type: Some("application/octet-stream".to_owned()),
                                storage_uri: Some("file:///external/artifact".to_owned()),
                                byte_len: Some(artifact_bytes),
                                metadata: None,
                            }],
                        }),
                    }),
                },
            )
            .await
            .expect("completion should append");
        drop(manager);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let path = db::session_db_path(root, session.id);
        let file_name = path.file_name().expect("database filename");
        std::fs::read_dir(path.parent().expect("database parent"))
            .expect("database directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(file_name.to_string_lossy().as_ref())
            })
            .filter_map(|entry| entry.metadata().ok())
            .map(|metadata| metadata.len())
            .sum()
    }

    #[tokio::test]
    async fn session_database_growth_is_independent_of_artifact_volume_and_transient_updates() {
        let low_root = unique_temp_dir();
        let high_root = unique_temp_dir();
        let low = persistent_artifact_session_bytes(&low_root, 100_000, 1).await;
        let high = persistent_artifact_session_bytes(&high_root, 900_000, 1_000).await;

        assert_eq!(low, high, "low={low} high={high}");
    }

    #[test]
    fn domain_metrics_count_payload_semantics_artifacts_and_compaction_boundaries() {
        let metrics = MetricsRegistry::in_memory();
        let session_id = SessionId::new();
        let artifact = bcode_session_models::ToolArtifact {
            artifact_id: "artifact".to_owned(),
            producer_plugin_id: "plugin".to_owned(),
            schema: "schema".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call".to_owned()),
            title: None,
            metadata: serde_json::Value::Null,
            refs: vec![bcode_session_models::ToolArtifactRef {
                key: "recording".to_owned(),
                content_type: None,
                storage_uri: Some("artifact://recording".to_owned()),
                byte_len: Some(12),
                metadata: None,
            }],
        };
        let events = [
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolCallFinished {
                    tool_call_id: "call".to_owned(),
                    result: "done".to_owned(),
                    is_error: false,
                    output: None,
                    semantic_result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(artifact),
                    }),
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 2,
                timestamp_ms: 2,
                session_id,
                provenance: None,
                kind: SessionEventKind::ContextCompacted {
                    summary: "summary".to_owned(),
                    compacted_through_sequence: 1,
                },
            },
        ];
        for event in &events {
            super::record_session_event_domain_metrics(&metrics, event);
        }

        let snapshot = metrics.snapshot();
        assert_eq!(
            snapshot.counters.get("session.event.semantic_rows"),
            Some(&2)
        );
        assert_eq!(
            snapshot.counters.get("session.event.artifact_references"),
            Some(&1)
        );
        assert_eq!(
            snapshot.counters.get("session.event.compaction_boundaries"),
            Some(&1)
        );
        assert_eq!(
            snapshot
                .histograms
                .get("session.event.payload_bytes")
                .map(|histogram| histogram.count),
            Some(2)
        );
    }

    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ProjectionWindowAnchor,
        ProjectionWindowDirection, ProjectionWindowLimits, ProjectionWindowRequest,
        ProjectionWindowTarget, ProviderContextSnapshot, ProviderContextSnapshotOrigin,
        ProviderStreamEvent, RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind,
        SessionEventProvenance, SessionForkKind, SessionHistoryDirection, SessionHistoryQuery,
        SessionId, SessionLiveEvent, SessionLiveEventKind, SessionProjectionKind,
        SessionTraceEvent, SessionTracePayload, SessionTracePhase, ToolInvocationResult,
        ToolInvocationStreamEvent, ToolOutputStream, TraceBlobRef, WorkId,
    };
    use bcode_skill_models::{SkillActivationMode, SkillId};
    use serde::Serialize;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn test_session_event(
        session_id: SessionId,
        sequence: u64,
        kind: SessionEventKind,
    ) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        }
    }

    fn provider_snapshot() -> ProviderContextSnapshot {
        ProviderContextSnapshot {
            format_version: 1,
            request_fingerprint: None,
            request_id: None,
            provider_plugin_id: "provider".to_string(),
            model_id: "model".to_string(),
            compatibility_key: "surface".to_string(),
            auth_profile: None,
            origin: ProviderContextSnapshotOrigin::Explicit,
            messages_json: "[]".to_string(),
            portable_summary: "portable".to_string(),
        }
    }

    #[test]
    fn in_memory_projection_selects_newest_marker_by_sequence_not_storage_order() {
        let id = SessionId::new();
        let history = vec![
            test_session_event(
                id,
                8,
                SessionEventKind::ContextCompacted {
                    summary: "newest".to_string(),
                    compacted_through_sequence: 2,
                },
            ),
            test_session_event(
                id,
                4,
                SessionEventKind::ProviderContextCompacted {
                    snapshot: provider_snapshot(),
                    compacted_through_sequence: 1,
                },
            ),
            test_session_event(
                id,
                5,
                SessionEventKind::AssistantMessage {
                    text: "retained".to_string(),
                },
            ),
        ];
        let projected = super::model_context_events_from_history(&history);
        assert_eq!(
            projected
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![8, 5]
        );
    }

    #[test]
    fn copied_local_boundary_is_rewritten_to_destination_sequence() {
        let rewritten = super::rewrite_copied_event_kind(
            SessionEventKind::ContextCompacted {
                summary: "summary".to_string(),
                compacted_through_sequence: 10,
            },
            &BTreeMap::from([(10, 4)]),
        );
        assert!(matches!(
            rewritten,
            SessionEventKind::ContextCompacted {
                compacted_through_sequence: 4,
                ..
            }
        ));
    }

    #[test]
    fn copied_provider_boundary_is_rewritten_to_destination_sequence() {
        let rewritten = super::rewrite_copied_event_kind(
            SessionEventKind::ProviderContextCompacted {
                snapshot: provider_snapshot(),
                compacted_through_sequence: 10,
            },
            &BTreeMap::from([(10, 4)]),
        );
        assert!(matches!(
            rewritten,
            SessionEventKind::ProviderContextCompacted {
                compacted_through_sequence: 4,
                ..
            }
        ));
    }

    #[test]
    fn fork_cut_before_boundary_contains_no_future_marker() {
        let id = SessionId::new();
        let history = vec![
            test_session_event(
                id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "old".to_string(),
                },
            ),
            test_session_event(
                id,
                3,
                SessionEventKind::ContextCompacted {
                    summary: "summary".to_string(),
                    compacted_through_sequence: 1,
                },
            ),
        ];
        let forked = history
            .into_iter()
            .filter(|event| event.sequence < 2)
            .collect::<Vec<_>>();
        assert!(
            !super::model_context_events_from_history(&forked)
                .iter()
                .any(|event| matches!(
                    event.kind,
                    SessionEventKind::ContextCompacted { .. }
                        | SessionEventKind::ProviderContextCompacted { .. }
                ))
        );
    }

    #[test]
    fn fork_cut_after_boundary_preserves_marker_and_retained_tail() {
        let id = SessionId::new();
        let history = vec![
            test_session_event(
                id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "old".to_string(),
                },
            ),
            test_session_event(
                id,
                3,
                SessionEventKind::ContextCompacted {
                    summary: "summary".to_string(),
                    compacted_through_sequence: 1,
                },
            ),
            test_session_event(
                id,
                4,
                SessionEventKind::AssistantMessage {
                    text: "tail".to_string(),
                },
            ),
        ];
        let forked = history
            .into_iter()
            .filter(|event| event.sequence < 5)
            .collect::<Vec<_>>();
        assert_eq!(
            super::model_context_events_from_history(&forked)
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    #[tokio::test]
    async fn live_assistant_text_delta_is_not_persisted() {
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

        manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantTextDelta {
                    turn_id: "turn-1".to_string(),
                    text: "live text".to_string(),
                },
            )
            .await
            .expect("live event should publish");

        let received = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive live event");
        assert_eq!(
            received.kind,
            SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_string(),
                text: "live text".to_string(),
            }
        );
        let persisted = manager
            .session_history(session.id)
            .await
            .expect("history should read");
        assert!(
            !persisted
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::AssistantDelta { .. }))
        );
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[tokio::test]
    async fn live_assistant_reasoning_delta_is_not_persisted() {
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

        manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantReasoningDelta {
                    turn_id: "turn-1".to_string(),
                    text: "live reasoning".to_string(),
                },
            )
            .await
            .expect("live event should publish");

        let received = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive live event");
        assert_eq!(
            received.kind,
            SessionLiveEventKind::AssistantReasoningDelta {
                turn_id: "turn-1".to_string(),
                text: "live reasoning".to_string(),
            }
        );
        let persisted = manager
            .session_history(session.id)
            .await
            .expect("history should read");
        assert!(
            !persisted.iter().any(|event| matches!(
                event.kind,
                SessionEventKind::AssistantReasoningDelta { .. }
            ))
        );
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[tokio::test]
    async fn live_tool_output_delta_is_not_persisted() {
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
            .publish_live_event(
                session.id,
                SessionLiveEventKind::ToolOutputDelta {
                    event: stream_event.clone(),
                },
            )
            .await
            .expect("live event should publish");
        let received = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive live event");
        assert_eq!(
            received.kind,
            SessionLiveEventKind::ToolOutputDelta {
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

    #[tokio::test]
    async fn persisted_semantic_result_session_reopens_and_attaches() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should create");
            let session = manager
                .create_session(
                    Some("semantic reopen".to_string()),
                    test_working_directory(),
                )
                .await
                .expect("session should create");
            manager
                .append_tool_call_requested(
                    session.id,
                    crate::AppendToolCallRequestedInput {
                        tool_call_id: "call-1".to_string(),
                        tool_name: "shell.run".to_string(),
                        arguments_json: "{}".to_string(),
                        ..crate::AppendToolCallRequestedInput::default()
                    },
                )
                .await
                .expect("request should append");
            manager
                .append_event(
                    session.id,
                    SessionEventKind::ToolCallFinished {
                        tool_call_id: "call-1".to_string(),
                        result: "legacy fallback".to_string(),
                        is_error: false,
                        output: None,
                        semantic_result: Some(ToolInvocationResult::Artifact {
                            artifact: Box::new(bcode_session_models::ToolArtifact {
                                artifact_id: "call-1-shell-run".to_string(),
                                producer_plugin_id: "test.shell".to_string(),
                                schema: "test.shell-artifact".to_string(),
                                schema_version: 1,
                                tool_call_id: Some("call-1".to_string()),
                                title: Some("Shell run".to_string()),
                                metadata: serde_json::json!({
                                    "mode": "terminal",
                                    "exit_code": 0,
                                    "timed_out": false,
                                    "cancelled": false,
                                    "duration_ms": null,
                                    "output_tail": "hello\n",
                                    "output_truncated": false,
                                    "output_bytes": 6,
                                    "retained_output_bytes": 6,
                                    "columns": 120,
                                    "rows": 30,
                                }),
                                refs: Vec::new(),
                            }),
                        }),
                    },
                )
                .await
                .expect("finish should append");
            session.id
        };

        let reopened = SessionManager::persistent(&root).expect("manager should reopen");
        let attachment = reopened
            .attach_session(session_id, ClientId::new())
            .await
            .expect("session should attach after reopen");

        assert!(attachment.history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolCallFinished {
                semantic_result: Some(ToolInvocationResult::Artifact { artifact }),
                ..
            } if artifact.schema == "test.shell-artifact"
                && artifact.metadata["mode"] == "terminal"
                && artifact.metadata["output_tail"] == "hello\n"
        )));
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[tokio::test]
    async fn old_persisted_session_without_semantic_result_reopens_and_attaches() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should create");
            let session = manager
                .create_session(Some("legacy reopen".to_string()), test_working_directory())
                .await
                .expect("session should create");
            manager
                .append_event(
                    session.id,
                    SessionEventKind::ToolCallFinished {
                        tool_call_id: "call-legacy".to_string(),
                        result: "legacy result".to_string(),
                        is_error: false,
                        output: None,
                        semantic_result: None,
                    },
                )
                .await
                .expect("legacy finish should append");
            session.id
        };

        let reopened = SessionManager::persistent(&root).expect("manager should reopen");
        let attachment = reopened
            .attach_session(session_id, ClientId::new())
            .await
            .expect("legacy session should attach after reopen");

        assert!(attachment.history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                semantic_result: None,
                ..
            } if tool_call_id == "call-legacy" && result == "legacy result"
        )));
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[test]
    fn live_event_broker_drops_without_receivers_and_tracks_publish_counts() {
        let broker = super::SessionLiveEventBroker::new(4);
        let session_id = SessionId::new();
        let event = SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_string(),
                text: "hello".to_string(),
            },
        };

        assert_eq!(broker.publish(event.clone()), None);
        assert_eq!(broker.published.load(Ordering::Relaxed), 0);
        assert_eq!(broker.dropped_no_receivers.load(Ordering::Relaxed), 1);

        let mut receiver = broker.subscribe();
        assert_eq!(broker.publish(event.clone()), Some(event.clone()));
        assert_eq!(broker.published.load(Ordering::Relaxed), 1);
        assert_eq!(broker.dropped_no_receivers.load(Ordering::Relaxed), 1);
        assert_eq!(
            receiver.try_recv().expect("event should be available"),
            event
        );
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
            timestamp_ms: 1,
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
            timestamp_ms: 1,
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
                "persisted SessionEventKind tag changed for {name}; append new variants only or add compatibility decoding plus binary fixtures"
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
                "persisted SessionTracePhase tag changed for {name}; append new variants only or add compatibility decoding plus binary fixtures"
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
                "persisted SessionTracePayload tag changed for {name}; append new variants only or add compatibility decoding plus binary fixtures"
            );
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
                crate::AppendToolCallRequestedInput {
                    tool_call_id: "tool-1".to_string(),
                    tool_name: "read".to_string(),
                    arguments_json: r#"{"path":"README.md"}"#.to_string(),
                    ..crate::AppendToolCallRequestedInput::default()
                },
            )
            .await
            .expect("tool request should append");
        manager
            .append_event(
                session.id,
                SessionEventKind::ToolCallFinished {
                    tool_call_id: "tool-1".to_string(),
                    result: "ok".to_string(),
                    is_error: false,
                    output: None,
                    semantic_result: None,
                },
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
        let runtime_selection = restored
            .current_runtime_selection(session.id)
            .await
            .expect("runtime selection should restore");
        assert_eq!(runtime_selection.agent_id.as_deref(), Some("plan"));
        assert_eq!(
            runtime_selection.provider_plugin_id.as_deref(),
            Some("provider")
        );
        assert_eq!(runtime_selection.model_id.as_deref(), Some("model"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn detached_preparation_migrates_legacy_storage_before_exclusive_load() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should initialize");
            let session = manager
                .create_session(Some("legacy".to_owned()), test_working_directory())
                .await
                .expect("session should create");
            let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
                .await
                .expect("fixture database should open");
            db.database()
                .update("session_storage_contract")
                .value(
                    "writer_epoch",
                    switchy::database::DatabaseValue::Int64(i64::from(
                        db::LEGACY_SESSION_STORAGE_WRITER_EPOCH,
                    )),
                )
                .execute(db.database())
                .await
                .expect("writer epoch should become legacy");
            session.id
        };

        let manager = SessionManager::persistent(&root).expect("manager should reopen");
        let before_preparation = manager.require_write_readiness(session_id).await;
        assert!(
            matches!(
                before_preparation,
                Err(SessionError::StorageMigrationRequired { .. }
                    | SessionError::Db(db::SessionDbError::WriterIncompatible { .. }))
            ),
            "unexpected pre-preparation write result: {before_preparation:?}"
        );
        let terminal = prepare_session_until_terminal(&manager, session_id).await;
        assert_eq!(terminal.outcome, Some(SessionOpenTerminalOutcome::Ready));
        manager
            .require_write_readiness(session_id)
            .await
            .expect("prepared legacy session should be writable");
        manager
            .attach_session_recent(session_id, ClientId::new(), 16)
            .await
            .expect("migrated session should attach");
        manager
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    limit: 16,
                    direction: bcode_session_models::SessionHistoryDirection::Backward,
                },
            )
            .await
            .expect("migrated history should load");
        manager
            .model_context_events(session_id)
            .await
            .expect("migrated model context should load");
        let migrated = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("migrated database should open");
        assert_eq!(
            migrated.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        let backup_root = root
            .parent()
            .expect("session root parent")
            .join("session-migration-backups");
        let backups = std::fs::read_dir(&backup_root)
            .expect("migration backup directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("migration backup entries");
        assert_eq!(backups.len(), 1);
        let backup = backups[0].path();
        assert!(backup.join("session.db").is_file());
        assert!(backup.join("migration-backup.json").is_file());
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(backup.join("migration-backup.json")).expect("backup manifest"),
        )
        .expect("manifest JSON");
        assert_eq!(manifest["session_id"], session_id.to_string());
        assert_eq!(
            manifest["target_writer_epoch"],
            db::CURRENT_SESSION_STORAGE_WRITER_EPOCH
        );
        std::fs::remove_dir_all(state_root).expect("state root should clean up");
    }

    #[tokio::test]
    async fn failed_migration_backup_prevents_every_storage_mutation() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should initialize");
            let session = manager
                .create_session(Some("blocked backup".to_owned()), test_working_directory())
                .await
                .expect("session should create");
            let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
                .await
                .expect("fixture database should open");
            db.database()
                .update("session_storage_contract")
                .value("writer_epoch", switchy::database::DatabaseValue::Int64(3))
                .execute(db.database())
                .await
                .expect("mark epoch three");
            session.id
        };
        let backup_root = root
            .parent()
            .expect("session root parent")
            .join("session-migration-backups");
        std::fs::write(&backup_root, b"block backup directory").expect("create backup blocker");

        let manager = SessionManager::persistent(&root).expect("manager should reopen");
        let terminal = prepare_session_until_terminal(&manager, session_id).await;
        assert!(matches!(
            terminal.outcome,
            Some(SessionOpenTerminalOutcome::Failed {
                kind: SessionOpenFailureKind::BackupFailed,
                ..
            })
        ));
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("database should reopen");
        assert_eq!(db.storage_writer_epoch().await.expect("writer epoch"), 3);
        assert_eq!(db.last_event_sequence().await.expect("tail"), Some(0));
        assert_eq!(
            db.session_compatibility_status()
                .await
                .expect("compatibility status"),
            db::SessionCompatibilityStatus::Compatible { checkpoint: 0 }
        );
        std::fs::remove_file(backup_root).expect("backup blocker should clean up");
        std::fs::remove_dir_all(state_root).expect("state root should clean up");
    }

    #[derive(Debug, Clone, Copy)]
    enum MigrationBenchmarkProfile {
        Small,
        Medium,
        Large,
    }

    impl MigrationBenchmarkProfile {
        const fn event_count(self) -> usize {
            match self {
                Self::Small => 100,
                Self::Medium => 5_000,
                Self::Large => 50_000,
            }
        }

        const fn name(self) -> &'static str {
            match self {
                Self::Small => "small",
                Self::Medium => "medium",
                Self::Large => "large-50k",
            }
        }
    }

    async fn generate_legacy_migration_benchmark_store(
        root: &std::path::Path,
        profile: MigrationBenchmarkProfile,
    ) -> SessionId {
        let session_id = SessionId::new();
        let db = db::SessionDb::open_turso_in_root(session_id, root)
            .await
            .expect("benchmark DB");
        let tx = db
            .database()
            .begin_transaction()
            .await
            .expect("benchmark transaction");
        for sequence in 0..profile.event_count() {
            let sequence = u64::try_from(sequence).expect("benchmark sequence fits");
            let kind = if sequence == 0 {
                SessionEventKind::SessionCreated {
                    name: Some(format!("migration benchmark {}", profile.name())),
                    working_directory: test_working_directory(),
                }
            } else if sequence % 2 == 0 {
                SessionEventKind::AssistantMessage {
                    text: format!("synthetic assistant message {sequence}"),
                }
            } else {
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: format!("synthetic user message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                }
            };
            let event = SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence,
                timestamp_ms: sequence,
                session_id,
                provenance: None,
                kind,
            };
            let payload = persisted::encode_session_event(&event).expect("benchmark event encode");
            let event_type = match event.kind {
                SessionEventKind::SessionCreated { .. } => "session_created",
                SessionEventKind::UserMessage { .. } => "user_message",
                SessionEventKind::AssistantMessage { .. } => "assistant_message",
                _ => unreachable!("benchmark generator uses three event kinds"),
            };
            tx.insert("events")
                .value(
                    "event_seq",
                    switchy::database::DatabaseValue::Int64(
                        i64::try_from(sequence).expect("benchmark sequence fits i64"),
                    ),
                )
                .value("event_type", event_type)
                .value(
                    "schema_version",
                    switchy::database::DatabaseValue::Int32(i32::from(
                        CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    )),
                )
                .value(
                    "created_at_ms",
                    switchy::database::DatabaseValue::Int64(
                        i64::try_from(sequence).expect("benchmark timestamp fits i64"),
                    ),
                )
                .value("payload", payload)
                .execute(&*tx)
                .await
                .expect("benchmark canonical insert");
        }
        tx.update("session_storage_contract")
            .value("writer_epoch", switchy::database::DatabaseValue::Int64(3))
            .where_eq("contract_id", switchy::database::DatabaseValue::Int32(1))
            .execute(&*tx)
            .await
            .expect("legacy writer epoch");
        tx.commit().await.expect("benchmark transaction commit");
        drop(db);
        session_id
    }

    async fn decoded_log_high_water_proxy(root: &std::path::Path, session_id: SessionId) -> u64 {
        let db = db::SessionDb::open_existing_turso_in_root(session_id, root)
            .await
            .expect("benchmark DB for memory proxy");
        let rows = db
            .database()
            .select("events")
            .columns(&["payload"])
            .execute(db.database())
            .await
            .expect("benchmark canonical payloads");
        let payload_bytes = rows.iter().fold(0_u64, |total, row| {
            let bytes = match row.get("payload") {
                Some(switchy::database::DatabaseValue::String(payload)) => payload.len(),
                _ => panic!("benchmark canonical payload missing"),
            };
            total.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX))
        });
        payload_bytes.saturating_add(
            u64::try_from(rows.len())
                .unwrap_or(u64::MAX)
                .saturating_mul(
                    u64::try_from(std::mem::size_of::<SessionEvent>()).unwrap_or(u64::MAX),
                ),
        )
    }

    async fn prepare_session_until_terminal(
        manager: &SessionManager,
        session_id: SessionId,
    ) -> bcode_session_models::SessionOpenOperationSnapshot {
        let initial = manager
            .prepare_session_open(session_id)
            .await
            .expect("start session preparation");
        if initial.outcome.is_some() {
            return initial;
        }
        let mut receiver = manager
            .subscribe_session_open_operation(session_id, initial.operation_id)
            .await
            .expect("preparation subscription");
        receiver
            .wait_for(|snapshot| snapshot.outcome.is_some())
            .await
            .expect("terminal preparation")
            .clone()
    }

    async fn assert_legacy_preparation_repair_required(
        manager: &SessionManager,
        session_id: SessionId,
        expected_reason: &str,
    ) -> bcode_session_models::SessionOpenOperationSnapshot {
        let initial = manager
            .prepare_session_open(session_id)
            .await
            .expect("start legacy preparation");
        let mut receiver = manager
            .subscribe_session_open_operation(session_id, initial.operation_id)
            .await
            .expect("preparation subscription");
        let terminal = receiver
            .wait_for(|snapshot| snapshot.outcome.is_some())
            .await
            .expect("terminal preparation")
            .clone();
        assert!(
            matches!(
                terminal.outcome,
                Some(SessionOpenTerminalOutcome::RepairRequired { ref reason })
                    if reason.contains(expected_reason)
            ),
            "unexpected repair outcome: {:?}",
            terminal.outcome
        );
        let history = manager
            .session_open_operation_history(session_id, initial.operation_id)
            .await;
        assert!(history.iter().any(|snapshot| {
            snapshot.progress.stage == SessionMigrationStage::ReadingCanonicalHistory
        }));
        assert!(terminal.backup_path.is_some());
        terminal
    }

    #[tokio::test]
    async fn malformed_legacy_json_preparation_requires_repair_at_canonical_read() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("legacy DB");
        db.database()
            .update("events")
            .value("payload", "not valid JSON")
            .where_eq("event_seq", switchy::database::DatabaseValue::Int64(50))
            .execute(db.database())
            .await
            .expect("corrupt canonical payload");
        drop(db);
        let manager = SessionManager::persistent(&root).expect("manager");

        assert_legacy_preparation_repair_required(&manager, session_id, "expected ident at line 1")
            .await;
        std::fs::remove_dir_all(state_root).expect("state root cleanup");
    }

    #[tokio::test]
    async fn mismatched_session_identity_preparation_requires_repair_at_canonical_read() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("legacy DB");
        let row = db
            .database()
            .select("events")
            .columns(&["payload"])
            .where_eq("event_seq", switchy::database::DatabaseValue::Int64(50))
            .execute_first(db.database())
            .await
            .expect("canonical row")
            .expect("canonical event");
        let payload = row
            .get("payload")
            .expect("canonical payload")
            .as_str()
            .expect("canonical payload string")
            .to_owned();
        let mut payload: serde_json::Value =
            serde_json::from_str(&payload).expect("canonical payload JSON");
        payload["session_id"] =
            serde_json::to_value(SessionId::new()).expect("mismatched session id JSON");
        db.database()
            .update("events")
            .value("payload", payload.to_string())
            .where_eq("event_seq", switchy::database::DatabaseValue::Int64(50))
            .execute(db.database())
            .await
            .expect("replace canonical identity");
        drop(db);
        let manager = SessionManager::persistent(&root).expect("manager");

        assert_legacy_preparation_repair_required(&manager, session_id, "events.session_id").await;
        std::fs::remove_dir_all(state_root).expect("state root cleanup");
    }

    #[tokio::test]
    async fn legacy_sequence_gap_preparation_requires_repair_at_canonical_read() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("legacy DB");
        db.database()
            .delete("events")
            .where_eq("event_seq", switchy::database::DatabaseValue::Int64(50))
            .execute(db.database())
            .await
            .expect("delete canonical event");
        drop(db);
        let manager = SessionManager::persistent(&root).expect("manager");

        assert_legacy_preparation_repair_required(&manager, session_id, "canonical event sequence")
            .await;
        std::fs::remove_dir_all(state_root).expect("state root cleanup");
    }

    #[tokio::test]
    async fn terminal_operation_cleanup_preserves_migrated_durable_state() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let mut manager = SessionManager::persistent(&root).expect("manager");
        manager.migration_operations =
            crate::migration_operation::SessionMigrationOperations::new(Duration::ZERO, 1);

        let terminal = prepare_session_until_terminal(&manager, session_id).await;
        assert_eq!(terminal.outcome, Some(SessionOpenTerminalOutcome::Ready));
        assert!(
            manager
                .session_open_operation(session_id, terminal.operation_id)
                .await
                .is_none(),
            "zero-retention cleanup should remove terminal observer metadata"
        );

        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("migrated DB after operation cleanup");
        assert_eq!(
            db.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        drop(db);
        let page = manager
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    direction: SessionHistoryDirection::Forward,
                    limit: 10,
                },
            )
            .await
            .expect("bounded history after operation cleanup");
        assert_eq!(page.events.len(), 10);
        std::fs::remove_dir_all(state_root).expect("state root cleanup");
    }

    #[tokio::test]
    async fn direct_attach_cannot_own_legacy_migration_lifetime() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let manager = SessionManager::persistent(&root).expect("manager");

        let attach = manager
            .attach_session_recent(session_id, ClientId::new(), 16)
            .await;

        assert!(matches!(
            attach,
            Err(SessionError::StorageMigrationRequired {
                actual: 3,
                expected: 4,
            })
        ));
        assert_eq!(manager.active_session_migration_count().await, 0);
        let durable_epoch = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("unchanged legacy DB")
            .storage_writer_epoch()
            .await
            .expect("legacy writer epoch");
        assert_eq!(durable_epoch, 3);
        std::fs::remove_dir_all(state_root).expect("state root cleanup");
    }

    #[tokio::test]
    async fn current_session_with_compatibility_issues_prepares_degraded_read_only() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let session = manager
            .create_session(Some("degraded".to_owned()), test_working_directory())
            .await
            .expect("current session");
        let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("session DB");
        let sequence = db
            .last_event_sequence()
            .await
            .expect("canonical tail")
            .expect("created session event");
        db.database()
            .insert("session_compatibility_issues")
            .value(
                "event_seq",
                switchy::database::DatabaseValue::Int64(i64::try_from(sequence).expect("sequence")),
            )
            .value("event_kind", "unknown_fixture_event")
            .value(
                "event_schema_version",
                switchy::database::DatabaseValue::Int32(1),
            )
            .value("compatibility", "unknown_event_kind")
            .value("remediation", "upgrade Bcode")
            .execute(db.database())
            .await
            .expect("compatibility issue");
        drop(db);

        let snapshot = manager
            .prepare_session_open(session.id)
            .await
            .expect("prepare degraded session");

        assert_eq!(
            snapshot.outcome,
            Some(SessionOpenTerminalOutcome::DegradedReadOnly { issue_count: 1 })
        );
        assert_eq!(manager.active_session_migration_count().await, 0);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn future_writer_preparation_is_incompatible_without_migration() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let session = manager
            .create_session(Some("future".to_owned()), test_working_directory())
            .await
            .expect("current session");
        let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("session DB");
        db.database()
            .update("session_storage_contract")
            .value("writer_epoch", switchy::database::DatabaseValue::Int64(99))
            .where_eq("contract_id", switchy::database::DatabaseValue::Int32(1))
            .execute(db.database())
            .await
            .expect("future writer epoch");
        drop(db);

        let snapshot = manager
            .prepare_session_open(session.id)
            .await
            .expect("classify future writer");

        assert_eq!(
            snapshot.outcome,
            Some(SessionOpenTerminalOutcome::WriterIncompatible {
                actual: Some(99),
                expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            })
        );
        assert_eq!(manager.active_session_migration_count().await, 0);
        let durable_epoch = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("unchanged future session DB")
            .storage_writer_epoch()
            .await
            .expect("future writer epoch after classification");
        assert_eq!(durable_epoch, 99);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn stale_compatibility_projection_prepares_repair_required() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let session = manager
            .create_session(Some("stale".to_owned()), test_working_directory())
            .await
            .expect("current session");
        let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("session DB");
        let stale_checkpoint = db
            .last_event_sequence()
            .await
            .expect("canonical tail")
            .expect("created session event")
            .saturating_add(1);
        db.database()
            .update("session_compatibility_state")
            .value(
                "last_event_seq",
                switchy::database::DatabaseValue::Int64(
                    i64::try_from(stale_checkpoint).expect("checkpoint"),
                ),
            )
            .where_eq("projection_id", switchy::database::DatabaseValue::Int32(1))
            .execute(db.database())
            .await
            .expect("stale checkpoint");
        drop(db);

        let snapshot = manager
            .prepare_session_open(session.id)
            .await
            .expect("classify stale projection");

        assert!(matches!(
            snapshot.outcome,
            Some(SessionOpenTerminalOutcome::RepairRequired { ref reason })
                if reason.contains("stale")
        ));
        assert_eq!(manager.active_session_migration_count().await, 0);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn current_session_preparation_is_immediately_ready_without_operation() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let session = manager
            .create_session(Some("current".to_owned()), test_working_directory())
            .await
            .expect("current session");

        let snapshot = manager
            .prepare_session_open(session.id)
            .await
            .expect("prepare current session");

        assert_eq!(snapshot.outcome, Some(SessionOpenTerminalOutcome::Ready));
        assert_eq!(snapshot.progress.stage, SessionMigrationStage::Complete);
        assert_eq!(manager.active_session_migration_count().await, 0);
        assert!(
            manager
                .session_open_operation(session.id, snapshot.operation_id)
                .await
                .is_none(),
            "current storage must not allocate a migration operation"
        );
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    fn assert_successful_migration_progress(
        snapshots: &[bcode_session_models::SessionOpenOperationSnapshot],
        terminal: &bcode_session_models::SessionOpenOperationSnapshot,
        expected_backup_bytes: u64,
    ) {
        assert_eq!(terminal.outcome, Some(SessionOpenTerminalOutcome::Ready));
        assert_eq!(terminal.source_writer_epoch, Some(3));
        assert_eq!(
            terminal.target_writer_epoch,
            u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        assert!(
            terminal.backup_path.is_some(),
            "terminal snapshot lost backup path: {terminal:?}"
        );
        assert_eq!(
            snapshots
                .iter()
                .filter(|snapshot| snapshot.outcome.is_some())
                .count(),
            1
        );
        assert!(
            snapshots.len() < 30,
            "progress updates must remain throttled"
        );
        for pair in snapshots.windows(2) {
            assert!(pair[0].revision < pair[1].revision);
            assert!(pair[0].progress.stage <= pair[1].progress.stage);
            if pair[0].progress.stage == pair[1].progress.stage {
                assert!(pair[0].progress.completed_units <= pair[1].progress.completed_units);
            }
        }
        for snapshot in snapshots {
            assert!(
                snapshot
                    .progress
                    .completed_units
                    .zip(snapshot.progress.total_units)
                    .is_none_or(|(completed, total)| completed <= total)
            );
            if snapshot.backup_path.is_some() {
                assert!(
                    snapshot.progress.stage > SessionMigrationStage::VerifyingBackup
                        || (snapshot.progress.stage == SessionMigrationStage::VerifyingBackup
                            && snapshot.progress.completed_units == snapshot.progress.total_units)
                );
            }
        }
        for stage in [
            SessionMigrationStage::PlanningBackup,
            SessionMigrationStage::CopyingBackup,
            SessionMigrationStage::VerifyingBackup,
            SessionMigrationStage::ReadingCanonicalHistory,
            SessionMigrationStage::RebuildingProjections,
            SessionMigrationStage::ValidatingProjections,
            SessionMigrationStage::Committing,
            SessionMigrationStage::ValidatingWriteReadiness,
            SessionMigrationStage::Complete,
        ] {
            assert!(
                snapshots
                    .iter()
                    .any(|snapshot| snapshot.progress.stage == stage),
                "missing migration progress stage {stage:?}"
            );
        }
        for stage in [
            SessionMigrationStage::CopyingBackup,
            SessionMigrationStage::VerifyingBackup,
        ] {
            assert!(snapshots.iter().any(|snapshot| {
                snapshot.progress.stage == stage
                    && snapshot.progress.completed_units == Some(expected_backup_bytes)
                    && snapshot.progress.total_units == Some(expected_backup_bytes)
            }));
        }
        for stage in [
            SessionMigrationStage::ReadingCanonicalHistory,
            SessionMigrationStage::RebuildingProjections,
        ] {
            assert!(snapshots.iter().any(|snapshot| {
                snapshot.progress.stage == stage
                    && snapshot.progress.completed_units == Some(100)
                    && snapshot.progress.total_units == Some(100)
            }));
        }
    }

    #[tokio::test]
    async fn concurrent_preparation_joins_one_detached_legacy_migration() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let expected_backup_bytes =
            session_database_files(&root, session_id)
                .iter()
                .fold(0_u64, |total, (_, bytes)| {
                    total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                });
        let manager = SessionManager::persistent(&root).expect("manager");

        let (first, second) = tokio::join!(
            manager.prepare_session_open(session_id),
            manager.prepare_session_open(session_id),
        );
        let first = first.expect("first preparation");
        let second = second.expect("second preparation");
        assert_eq!(first.operation_id, second.operation_id);
        assert_eq!(manager.active_session_migration_count().await, 1);
        let operation_id = first.operation_id;
        let unrelated_session_id = SessionId::new();
        assert!(
            manager
                .session_open_operation(unrelated_session_id, operation_id)
                .await
                .is_none()
        );
        assert!(
            manager
                .subscribe_session_open_operation(unrelated_session_id, operation_id)
                .await
                .is_none()
        );
        drop(first);
        drop(second);

        let mut receiver = manager
            .subscribe_session_open_operation(session_id, operation_id)
            .await
            .expect("operation subscription");
        let terminal = receiver
            .wait_for(|snapshot| snapshot.outcome.is_some())
            .await
            .expect("terminal migration snapshot")
            .clone();
        let snapshots = manager
            .session_open_operation_history(session_id, operation_id)
            .await;
        assert_successful_migration_progress(&snapshots, &terminal, expected_backup_bytes);
        assert_eq!(manager.active_session_migration_count().await, 0);
        assert_eq!(
            manager
                .session_open_operation(session_id, operation_id)
                .await
                .expect("terminal snapshot retained"),
            terminal
        );
        assert!(
            manager
                .session_open_operation(session_id, SessionOpenOperationId::new())
                .await
                .is_none()
        );
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("migrated DB");
        assert_eq!(
            db.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        std::fs::remove_dir_all(state_root).expect("state root cleanup");
    }

    #[tokio::test]
    async fn detached_preparation_reports_structured_backup_failure_without_mutation() {
        let state_root = unique_temp_dir();
        let root = state_root.join("sessions");
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let backup_root = state_root.join("session-migration-backups");
        std::fs::write(&backup_root, b"block backup directory").expect("backup blocker");
        let manager = SessionManager::persistent(&root).expect("manager");
        let initial = manager
            .prepare_session_open(session_id)
            .await
            .expect("prepare legacy session");
        let mut receiver = manager
            .subscribe_session_open_operation(session_id, initial.operation_id)
            .await
            .expect("operation subscription");
        let terminal = receiver
            .wait_for(|snapshot| snapshot.outcome.is_some())
            .await
            .expect("terminal failure snapshot")
            .clone();

        assert!(matches!(
            terminal.outcome,
            Some(SessionOpenTerminalOutcome::Failed {
                kind: SessionOpenFailureKind::BackupFailed,
                backup_path: None,
                ..
            })
        ));
        assert_eq!(terminal.progress.stage, SessionMigrationStage::Failed);
        assert!(terminal.backup_path.is_none());
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("legacy DB");
        assert_eq!(db.storage_writer_epoch().await.expect("writer epoch"), 3);
        std::fs::remove_file(backup_root).expect("backup blocker cleanup");
        std::fs::remove_dir_all(state_root).expect("state root cleanup");
    }

    #[tokio::test]
    async fn generated_migration_benchmark_store_is_contiguous_and_legacy() {
        let root = unique_temp_dir();
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("generated DB");
        assert_eq!(db.last_event_sequence().await.expect("tail"), Some(99));
        assert_eq!(db.storage_writer_epoch().await.expect("epoch"), 3);
        assert_eq!(db.all_events_strict().await.expect("history").len(), 100);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    #[ignore = "manual generated legacy-session decoded-log memory proxy"]
    async fn benchmark_generated_legacy_session_memory_proxies() {
        for profile in [
            MigrationBenchmarkProfile::Small,
            MigrationBenchmarkProfile::Medium,
            MigrationBenchmarkProfile::Large,
        ] {
            let root = unique_temp_dir();
            let session_id = generate_legacy_migration_benchmark_store(&root, profile).await;
            let proxy = decoded_log_high_water_proxy(&root, session_id).await;
            eprintln!(
                "migration_memory_proxy profile={} events={} decoded_log_bytes={proxy}",
                profile.name(),
                profile.event_count(),
            );
            std::fs::remove_dir_all(root).expect("memory proxy cleanup");
        }
    }

    #[tokio::test]
    #[ignore = "manual generated legacy-session migration benchmark"]
    async fn benchmark_generated_legacy_session_migrations() {
        let profiles = match std::env::var("BCODE_MIGRATION_BENCHMARK_PROFILE").as_deref() {
            Ok("small") => vec![MigrationBenchmarkProfile::Small],
            Ok("medium") => vec![MigrationBenchmarkProfile::Medium],
            Ok("large") => vec![MigrationBenchmarkProfile::Large],
            Ok(other) => panic!("unknown BCODE_MIGRATION_BENCHMARK_PROFILE {other}"),
            Err(std::env::VarError::NotPresent) => vec![
                MigrationBenchmarkProfile::Small,
                MigrationBenchmarkProfile::Medium,
                MigrationBenchmarkProfile::Large,
            ],
            Err(error) => panic!("invalid BCODE_MIGRATION_BENCHMARK_PROFILE: {error}"),
        };
        for profile in profiles {
            let state_root = unique_temp_dir();
            let root = state_root.join("sessions");
            let session_id = generate_legacy_migration_benchmark_store(&root, profile).await;
            let metrics = MetricsRegistry::in_memory();
            let started = std::time::Instant::now();
            let manager = SessionManager::persistent_with_metrics(&root, metrics.clone())
                .expect("benchmark manager");
            let terminal = prepare_session_until_terminal(&manager, session_id).await;
            assert_eq!(terminal.outcome, Some(SessionOpenTerminalOutcome::Ready));
            let elapsed = started.elapsed();
            let storage_bytes = session_database_files(&root, session_id).iter().fold(
                0_u64,
                |total, (_, bytes)| {
                    total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                },
            );
            let report = metrics.report();
            let histogram_sum = |name: &str| {
                report
                    .snapshot
                    .histograms
                    .get(name)
                    .map_or(0, |histogram| histogram.sum)
            };
            eprintln!(
                "migration_benchmark profile={} events={} storage_bytes={} total_ms={} backup_plan_ms={} backup_copy_ms={} backup_verify_ms={} schema_ms={} decode_ms={} reproject_ms={} projector_materialized_ms={} projector_model_context_ms={} projector_occupancy_ms={} projector_receipt_ms={} projector_compatibility_ms={} validate_ms={} commit_ms={} readiness_ms={}",
                profile.name(),
                profile.event_count(),
                storage_bytes,
                elapsed.as_millis(),
                histogram_sum("session.migration.backup.plan_duration_ms"),
                histogram_sum("session.migration.backup.copy_duration_ms"),
                histogram_sum("session.migration.backup.verify_duration_ms"),
                histogram_sum("session.migration.schema_duration_ms"),
                histogram_sum("session.migration.canonical_decode_duration_ms"),
                histogram_sum("session.migration.projection_rebuild_duration_ms"),
                histogram_sum("session.migration.projector.materialized_duration_ms"),
                histogram_sum("session.migration.projector.model_context_duration_ms"),
                histogram_sum("session.migration.projector.context_occupancy_duration_ms"),
                histogram_sum("session.migration.projector.turn_receipt_duration_ms"),
                histogram_sum("session.migration.projector.compatibility_duration_ms"),
                histogram_sum("session.migration.validation_duration_ms"),
                histogram_sum("session.migration.commit_duration_ms"),
                histogram_sum("session.migration.write_readiness_duration_ms"),
            );
            drop(manager);
            std::fs::remove_dir_all(state_root).expect("benchmark cleanup");
        }
    }

    #[test]
    fn streaming_migration_backup_handles_nested_empty_and_large_files() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let source = temp_dir.path().join("source");
        let destination = temp_dir.path().join("backup");
        std::fs::create_dir_all(source.join("nested")).expect("nested source");
        std::fs::write(source.join("empty"), []).expect("empty file");
        std::fs::write(source.join("nested/small"), b"small").expect("small file");
        let large = vec![0x5a; MIGRATION_BACKUP_BUFFER_BYTES * 3 + 17];
        std::fs::write(source.join("large"), &large).expect("large file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                source.join("nested/small"),
                std::fs::Permissions::from_mode(0o640),
            )
            .expect("source permissions");
        }

        let result = create_verified_migration_backup_blocking(
            &source,
            &destination,
            br#"{"manifest":true}"#,
            MigrationBackupCopyFault::None,
            None,
        )
        .expect("backup should succeed");

        assert_eq!(result.files, 3);
        assert_eq!(
            result.bytes,
            u64::try_from(large.len() + b"small".len()).expect("fixture bytes fit")
        );
        assert_eq!(
            std::fs::read(destination.join("large")).expect("large backup"),
            large
        );
        assert_eq!(
            std::fs::read(destination.join("nested/small")).expect("small backup"),
            b"small"
        );
        assert!(
            std::fs::read(destination.join("empty"))
                .expect("empty backup")
                .is_empty()
        );
        assert_eq!(
            std::fs::read(destination.join("migration-backup.json")).expect("manifest"),
            br#"{"manifest":true}"#
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(destination.join("nested/small"))
                    .expect("backup metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn streaming_migration_backup_refuses_conflicts_and_cleans_failed_copy() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let source = temp_dir.path().join("source");
        let destination = temp_dir.path().join("backup");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(source.join("file"), b"content").expect("source file");
        std::fs::create_dir_all(&destination).expect("destination conflict");
        assert_eq!(
            create_verified_migration_backup_blocking(
                &source,
                &destination,
                b"manifest",
                MigrationBackupCopyFault::None,
                None,
            )
            .expect_err("existing destination must fail")
            .kind(),
            std::io::ErrorKind::AlreadyExists
        );

        std::fs::remove_dir_all(&destination).expect("remove conflict");
        let files = migration_backup_files(&source, &source).expect("backup plan");
        std::fs::create_dir_all(&destination).expect("destination");
        let source_hashes = copy_and_hash_backup_files(
            &source,
            &destination,
            &files,
            MigrationBackupCopyFault::None,
            None,
            7,
        )
        .expect("copy");
        std::fs::write(destination.join("file"), b"corrupt").expect("corrupt backup");
        assert!(
            verify_backup_files(&destination, &files, &source_hashes, None, 7)
                .expect_err("hash mismatch must fail")
                .to_string()
                .contains("verification failed")
        );

        std::fs::remove_dir_all(&destination).expect("remove corrupt backup");
        std::fs::create_dir(source.join("migration-backup.json")).expect("manifest-path directory");
        std::fs::write(source.join("migration-backup.json/child"), b"child")
            .expect("manifest-path child");
        assert!(
            create_verified_migration_backup_blocking(
                &source,
                &destination,
                b"manifest",
                MigrationBackupCopyFault::None,
                None,
            )
            .is_err()
        );
        assert!(
            !destination.exists(),
            "failed backup must clean its incomplete destination"
        );
    }

    #[test]
    fn migration_backup_faults_are_deterministic_and_cleanup_partial_output() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let source = temp_dir.path().join("source");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(
            source.join("large"),
            vec![0x7f; MIGRATION_BACKUP_BUFFER_BYTES * 2],
        )
        .expect("source file");

        for (index, fault, expected_kind) in [
            (
                0,
                MigrationBackupCopyFault::PermissionDenied,
                std::io::ErrorKind::PermissionDenied,
            ),
            (
                1,
                MigrationBackupCopyFault::ShortWriteAfter(17),
                std::io::ErrorKind::WriteZero,
            ),
        ] {
            let destination = temp_dir.path().join(format!("backup-{index}"));
            let error = create_verified_migration_backup_blocking(
                &source,
                &destination,
                b"manifest",
                fault,
                None,
            )
            .expect_err("injected backup failure");
            assert_eq!(error.kind(), expected_kind);
            assert!(
                !destination.exists(),
                "injected failure must clean incomplete destination"
            );
        }
    }

    #[tokio::test]
    async fn legacy_storage_migration_replays_presentation_diff_sections() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should initialize");
            let session = manager
                .create_session(Some("legacy diff".to_owned()), test_working_directory())
                .await
                .expect("session should create");
            manager
                .append_event(
                    session.id,
                    SessionEventKind::ToolInvocationStream {
                        event: ToolInvocationStreamEvent::Status {
                            tool_call_id: "call-1".to_owned(),
                            sequence: 1,
                            message: "running".to_owned(),
                        },
                    },
                )
                .await
                .expect("durable stream event should append");
            let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
                .await
                .expect("fixture database should open");
            let payload = serde_json::json!({
                "schema_version": 25,
                "sequence": 1,
                "timestamp_ms": 1,
                "session_id": session.id,
                "provenance": null,
                "kind": {
                    "tool_invocation_stream": {
                        "event": {
                            "presentation": {
                                "tool_call_id": "call-1",
                                "sequence": 1,
                                "presentation": {
                                    "card": {
                                        "target": "preview",
                                        "title": "Edit preview",
                                        "sections": [{
                                            "type": "diff",
                                            "path": "/tmp/file.rs",
                                            "old_text": "before",
                                            "new_text": "after"
                                        }]
                                    }
                                }
                            }
                        }
                    }
                }
            })
            .to_string();
            db.database()
                .update("events")
                .value("payload", switchy::database::DatabaseValue::String(payload))
                .where_eq("event_seq", switchy::database::DatabaseValue::Int64(1))
                .execute(db.database())
                .await
                .expect("legacy diff payload should replace status payload");
            db.database()
                .update("session_storage_contract")
                .value(
                    "writer_epoch",
                    switchy::database::DatabaseValue::Int64(i64::from(
                        db::LEGACY_SESSION_STORAGE_WRITER_EPOCH,
                    )),
                )
                .execute(db.database())
                .await
                .expect("writer epoch should become legacy");
            session.id
        };

        let manager = SessionManager::persistent(&root).expect("manager should reopen");
        let terminal = prepare_session_until_terminal(&manager, session_id).await;
        assert_eq!(terminal.outcome, Some(SessionOpenTerminalOutcome::Ready));
        manager
            .require_write_readiness(session_id)
            .await
            .expect("prepared legacy diff session should be writable");
        let migrated = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("migrated database should open");
        assert_eq!(
            migrated.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        assert!(matches!(
            migrated
                .all_events_strict()
                .await
                .expect("migrated history should decode")[1]
                .kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation { .. }
            }
        ));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn detached_preparation_migrates_missing_legacy_contract_before_load() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should initialize");
            let session = manager
                .create_session(
                    Some("tableless legacy".to_owned()),
                    test_working_directory(),
                )
                .await
                .expect("session should create");
            let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
                .await
                .expect("fixture database should open");
            db.database()
                .delete("__bcode_session_migrations")
                .where_in(
                    "id",
                    vec![
                        switchy::database::DatabaseValue::String(
                            "026_session_storage_contract_table".to_owned(),
                        ),
                        switchy::database::DatabaseValue::String(
                            "027_initialize_session_storage_contract".to_owned(),
                        ),
                        switchy::database::DatabaseValue::String(
                            "028_session_compatibility_state".to_owned(),
                        ),
                        switchy::database::DatabaseValue::String(
                            "029_session_compatibility_issues".to_owned(),
                        ),
                        switchy::database::DatabaseValue::String(
                            "030_session_state_visibility_column".to_owned(),
                        ),
                        switchy::database::DatabaseValue::String(
                            "031_session_state_execution_provenance_column".to_owned(),
                        ),
                    ],
                )
                .execute(db.database())
                .await
                .expect("contract migrations should be removed");
            db.database()
                .exec_raw("ALTER TABLE session_state DROP COLUMN execution_provenance")
                .await
                .expect("execution provenance column should be removed");
            db.database()
                .exec_raw("ALTER TABLE session_state DROP COLUMN visibility")
                .await
                .expect("visibility column should be removed");
            db.database()
                .exec_raw("DROP TABLE session_storage_contract")
                .await
                .expect("contract table should be removed");
            session.id
        };

        let manager = SessionManager::persistent(&root).expect("manager should reopen");
        let terminal = prepare_session_until_terminal(&manager, session_id).await;
        assert_eq!(terminal.outcome, Some(SessionOpenTerminalOutcome::Ready));
        manager
            .require_write_readiness(session_id)
            .await
            .expect("prepared tableless legacy session should be writable");
        let migrated = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("migrated database should open");
        assert_eq!(
            migrated.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        assert!(matches!(
            migrated
                .storage_compatibility()
                .await
                .expect("compatibility"),
            db::SessionStorageCompatibility::Current { .. }
        ));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }
    #[tokio::test]
    async fn concurrent_preparations_share_one_detached_legacy_migration() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should initialize");
            let session = manager
                .create_session(
                    Some("concurrent legacy".to_owned()),
                    test_working_directory(),
                )
                .await
                .expect("session should create");
            let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
                .await
                .expect("database should open");
            db.database()
                .update("session_storage_contract")
                .value(
                    "writer_epoch",
                    switchy::database::DatabaseValue::Int64(i64::from(
                        db::LEGACY_SESSION_STORAGE_WRITER_EPOCH,
                    )),
                )
                .execute(db.database())
                .await
                .expect("writer epoch should become legacy");
            session.id
        };
        tokio::time::sleep(Duration::from_millis(50)).await;

        let metrics = MetricsRegistry::default();
        let restored = SessionManager::persistent_with_metrics(&root, metrics.clone())
            .expect("manager should restore");
        let (first, second) = tokio::join!(
            restored.prepare_session_open(session_id),
            restored.prepare_session_open(session_id)
        );
        let first = first.expect("first preparation");
        let second = second.expect("second preparation");
        assert_eq!(first.operation_id, second.operation_id);
        let terminal = prepare_session_until_terminal(&restored, session_id).await;
        assert_eq!(terminal.outcome, Some(SessionOpenTerminalOutcome::Ready));
        assert_eq!(
            metrics
                .snapshot()
                .counters
                .get("session.manager.storage_migration.completed_total"),
            Some(&1)
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn active_owner_blocks_detached_legacy_session_migration() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("owned legacy".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("legacy fixture database should open");
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::DatabaseValue::Int64(i64::from(
                    db::LEGACY_SESSION_STORAGE_WRITER_EPOCH,
                )),
            )
            .execute(db.database())
            .await
            .expect("writer epoch should become legacy");
        drop(db);

        let contender = SessionManager::persistent(&root).expect("contender should initialize");
        let terminal = prepare_session_until_terminal(&contender, session.id).await;
        assert!(matches!(
            terminal.outcome,
            Some(SessionOpenTerminalOutcome::Failed {
                kind: SessionOpenFailureKind::OwnedByOtherDaemon,
                ..
            })
        ));
        let unchanged = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("legacy database should remain readable");
        assert_eq!(
            unchanged
                .storage_writer_epoch()
                .await
                .expect("writer epoch"),
            u64::from(db::LEGACY_SESSION_STORAGE_WRITER_EPOCH)
        );
        drop(unchanged);
        drop(contender);
        drop(manager);
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }
    #[tokio::test]
    async fn write_readiness_uses_actor_connection_before_followup_append() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("followup".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "first".to_string())
            .await
            .expect("first message should append");

        manager
            .set_session_composer_draft(session.id, "draft".to_string())
            .await
            .expect("draft should persist on actor connection");
        assert_eq!(
            manager
                .session_composer_draft(session.id)
                .await
                .expect("draft should load"),
            Some("draft".to_string())
        );
        manager
            .require_write_readiness(session.id)
            .await
            .expect("followup should be ready");
        manager
            .append_user_message(session.id, ClientId::new(), "second".to_string())
            .await
            .expect("followup should append");

        let history = manager
            .session_history(session.id)
            .await
            .expect("history should load");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "second"
        )));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn fork_session_from_prompt_copies_history_before_prompt_and_returns_draft() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("source".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_model_changed(session.id, "provider".to_string(), "model".to_string())
            .await
            .expect("model should append");
        manager
            .append_user_message(session.id, ClientId::new(), "first prompt".to_string())
            .await
            .expect("first prompt should append");
        manager
            .append_assistant_message(session.id, "first response".to_string())
            .await
            .expect("assistant response should append");
        let second_prompt = manager
            .append_user_message(session.id, ClientId::new(), "second prompt".to_string())
            .await
            .expect("second prompt should append")
            .into_iter()
            .find(|event| matches!(event.kind, SessionEventKind::UserMessage { .. }))
            .expect("user message event should exist");
        manager
            .append_assistant_message(session.id, "second response".to_string())
            .await
            .expect("second response should append");

        let result = manager
            .fork_session_from_prompt(session.id, second_prompt.sequence, None)
            .await
            .expect("session should fork");

        assert_ne!(result.session.id, session.id);
        assert_eq!(result.session.name.as_deref(), Some("[fork] source"));
        assert_eq!(result.draft.as_deref(), Some("second prompt"));
        assert_eq!(
            result.session.fork.as_ref().map(|fork| fork.kind),
            Some(SessionForkKind::Fork)
        );
        assert_eq!(
            result
                .session
                .fork
                .as_ref()
                .and_then(|fork| fork.source_prompt_sequence),
            Some(second_prompt.sequence)
        );

        let fork_history = manager
            .session_history(result.session.id)
            .await
            .expect("fork history should load");
        assert!(fork_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelChanged { provider, model }
                if provider == "provider" && model == "model"
        )));
        assert!(fork_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "first prompt"
        )));
        assert!(fork_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantMessage { text } if text == "first response"
        )));
        assert!(!fork_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "second prompt"
        )));
        assert!(fork_history.iter().any(|event| {
            matches!(
                &event.kind,
                SessionEventKind::SessionForked {
                    source_session_id,
                    kind: SessionForkKind::Fork,
                    ..
                } if *source_session_id == session.id
            )
        }));
        let copied = fork_history
            .iter()
            .find(|event| matches!(event.kind, SessionEventKind::AssistantMessage { .. }))
            .expect("copied assistant message should exist");
        assert!(copied.provenance.is_some());

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let restored_sessions = restored.list_sessions(&test_working_directory()).await;
        let restored_fork = restored_sessions
            .iter()
            .find(|summary| summary.id == result.session.id)
            .expect("fork should be listed after restore");
        assert_eq!(
            restored_fork.fork.as_ref().map(|fork| fork.kind),
            Some(SessionForkKind::Fork)
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn clone_session_at_generation_rejects_stale_snapshot_without_creating_clone() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let source = manager
            .create_session(Some("source".to_owned()), test_working_directory())
            .await
            .expect("source session");
        manager
            .append_user_message(source.id, ClientId::new(), "prompt".to_owned())
            .await
            .expect("prompt");
        let generation = manager
            .session_history(source.id)
            .await
            .expect("history")
            .last()
            .expect("source event")
            .sequence;
        manager
            .append_assistant_message(source.id, "changed".to_owned())
            .await
            .expect("source change");
        let session_count = manager.list_sessions(&test_working_directory()).await.len();

        let error = manager
            .clone_session_at_generation(source.id, None, Some(generation))
            .await
            .expect_err("stale generation must fail");
        assert!(matches!(
            error,
            SessionError::CloneGenerationChanged {
                session_id,
                expected,
                current,
            } if session_id == source.id && expected == generation && current > expected
        ));
        assert_eq!(
            manager.list_sessions(&test_working_directory()).await.len(),
            session_count,
            "a rejected snapshot must not leave a clone behind"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn clone_session_at_generation_copies_exact_accepted_snapshot() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let source = manager
            .create_session(Some("source".to_owned()), test_working_directory())
            .await
            .expect("source session");
        manager
            .append_user_message(source.id, ClientId::new(), "prompt".to_owned())
            .await
            .expect("prompt");
        let source_history = manager.session_history(source.id).await.expect("history");
        let generation = source_history.last().expect("source event").sequence;

        let clone = manager
            .clone_session_at_generation(source.id, None, Some(generation))
            .await
            .expect("matching generation should clone");
        assert_eq!(
            clone
                .session
                .fork
                .as_ref()
                .and_then(|fork| fork.source_cutoff_sequence),
            Some(generation)
        );
        let clone_history = manager
            .session_history(clone.session.id)
            .await
            .expect("clone history");
        let generation_string = generation.to_string();
        assert!(clone_history.iter().any(|event| {
            event.provenance.as_ref().is_some_and(|provenance| {
                provenance.source_event_id.as_deref() == Some(generation_string.as_str())
            })
        }));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn clone_session_copies_full_history_and_records_provenance() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("source".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "prompt".to_string())
            .await
            .expect("prompt should append");
        manager
            .append_assistant_message(session.id, "response".to_string())
            .await
            .expect("response should append");

        let result = manager
            .clone_session(session.id, None)
            .await
            .expect("session should clone");

        assert_ne!(result.session.id, session.id);
        assert_eq!(result.session.name.as_deref(), Some("[clone] source"));
        assert_eq!(result.draft, None);
        assert_eq!(
            result.session.fork.as_ref().map(|fork| fork.kind),
            Some(SessionForkKind::Clone)
        );

        let clone_history = manager
            .session_history(result.session.id)
            .await
            .expect("clone history should load");
        assert!(clone_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "prompt"
        )));
        assert!(clone_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantMessage { text } if text == "response"
        )));
        assert!(clone_history.iter().any(|event| {
            matches!(
                &event.kind,
                SessionEventKind::SessionForked {
                    source_session_id,
                    kind: SessionForkKind::Clone,
                    ..
                } if *source_session_id == session.id
            )
        }));
        assert!(
            clone_history
                .iter()
                .all(|event| event.session_id == result.session.id)
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn clone_before_any_boundary_preserves_uncompacted_context() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let source = manager
            .create_session(Some("source".to_string()), test_working_directory())
            .await
            .expect("source");
        manager
            .append_user_message(source.id, ClientId::new(), "prompt".to_string())
            .await
            .expect("prompt");
        manager
            .append_assistant_message(source.id, "response".to_string())
            .await
            .expect("response");
        let clone = manager.clone_session(source.id, None).await.expect("clone");
        let context = manager
            .model_context_events(clone.session.id)
            .await
            .expect("context");
        assert!(context.iter().any(|event| matches!(&event.kind, SessionEventKind::UserMessage { text, .. } if text == "prompt")));
        assert!(context.iter().any(|event| matches!(&event.kind, SessionEventKind::AssistantMessage { text } if text == "response")));
        assert!(!context.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::ContextCompacted { .. }
                | SessionEventKind::ProviderContextCompacted { .. }
        )));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn clone_after_provider_boundary_preserves_rewritten_canonical_context() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let source = manager
            .create_session(Some("source".to_string()), test_working_directory())
            .await
            .expect("source");
        let old = manager
            .append_assistant_message(source.id, "old".to_string())
            .await
            .expect("old");
        manager
            .append_event(
                source.id,
                SessionEventKind::ProviderContextCompacted {
                    snapshot: provider_snapshot(),
                    compacted_through_sequence: old.sequence,
                },
            )
            .await
            .expect("boundary");
        manager
            .append_assistant_message(source.id, "tail".to_string())
            .await
            .expect("tail");
        let clone = manager.clone_session(source.id, None).await.expect("clone");
        let context = manager
            .model_context_events(clone.session.id)
            .await
            .expect("context");
        assert_eq!(
            context
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    SessionEventKind::ProviderContextCompacted { .. }
                ))
                .count(),
            1
        );
        assert!(!context.iter().any(|event| matches!(&event.kind, SessionEventKind::AssistantMessage { text } if text == "old")));
        assert!(context.iter().any(|event| matches!(&event.kind, SessionEventKind::AssistantMessage { text } if text == "tail")));
        std::fs::remove_dir_all(root).expect("cleanup");
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
    async fn attach_uses_db_input_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("db input history".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "hello".to_owned())
            .await
            .expect("message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let attachment = restored
            .attach_session_recent(session.id, ClientId::new(), 16)
            .await
            .expect("attach should use DB projections");

        assert_eq!(attachment.input_history.len(), 1);
        let entry = &attachment.input_history[0];
        assert_eq!(entry.sequence, 1);
        assert!(entry.timestamp_ms > 0);
        assert_eq!(entry.text, "hello");

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn release_idle_session_resources_drops_loaded_state_but_retains_lease() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                build_fingerprint: Some("current-test-build".to_string()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("manager should initialize");
        let session = manager
            .create_session(Some("idle".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let client_id = ClientId::new();
        manager
            .attach_session_recent(session.id, client_id, 8)
            .await
            .expect("session should attach");

        assert!(
            !manager
                .release_idle_session_resources(session.id)
                .await
                .expect("release should check clients"),
            "attached sessions should not release resources"
        );

        manager
            .detach_session(session.id, client_id)
            .await
            .expect("session should detach");
        assert!(
            manager
                .release_idle_session_resources(session.id)
                .await
                .expect("idle resources should release")
        );

        assert!(
            manager.inner.lock().await.leases.contains_key(&session.id),
            "idle resource release must retain compatibility ownership"
        );

        let incompatible = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                storage_writer_epoch: Some(
                    crate::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH.saturating_add(1),
                ),
                build_fingerprint: Some("incompatible-test-build".to_string()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("incompatible manager should initialize lazily");
        assert!(matches!(
            incompatible.ensure_session_loaded(session.id).await,
            Err(SessionError::Lease(
                crate::lease::SessionLeaseError::OwnedByOtherDaemon { .. }
            ))
        ));

        manager
            .append_user_message(session.id, ClientId::new(), "after release".to_owned())
            .await
            .expect("released session should reload on next use");
        let history = manager
            .session_history(session.id)
            .await
            .expect("history should load after release");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "after release"
        )));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn dropping_persistent_manager_releases_loaded_session_lease() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent_with_metrics_and_lease_owner(
                &root,
                MetricsRegistry::default(),
                SessionLeaseOwnerContext {
                    storage_writer_epoch: Some(7),
                    build_fingerprint: Some("first-build".to_string()),
                    ..SessionLeaseOwnerContext::default()
                },
            )
            .expect("first manager");
            let session = manager
                .create_session(Some("lease release".to_string()), test_working_directory())
                .await
                .expect("session should create");
            assert!(manager.inner.lock().await.leases.contains_key(&session.id));
            session.id
        };

        let next = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                storage_writer_epoch: Some(8),
                build_fingerprint: Some("next-build".to_string()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("next manager");
        next.ensure_session_loaded(session_id)
            .await
            .expect("manager drop must release lease");
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_model_context_uses_relevant_canonical_db_events_without_checkpoint() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("model context".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "first".to_owned())
            .await
            .expect("first message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let appended = restored
            .append_user_message(session.id, ClientId::new(), "carry on".to_owned())
            .await
            .expect("carry-on message should append");
        let user_sequence = appended
            .last()
            .expect("user event should be returned")
            .sequence;
        restored
            .append_model_turn_started(session.id, "turn-1".to_owned())
            .await
            .expect("turn start should append");

        let context = restored
            .model_context_events(session.id)
            .await
            .expect("model context should load from canonical DB events");

        assert!(context.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. }
                if event.sequence == user_sequence && text == "carry on"
        )));
        assert!(context.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelTurnStarted { turn_id } if turn_id == "turn-1"
        )));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_session_events_range_reads_inclusive_sequences_from_db() {
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
    async fn restored_projection_windows_page_bidirectionally_without_overlap() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("projection pages".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        for index in 0..6 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }
        let request = |anchor, direction| ProjectionWindowRequest {
            projection: SessionProjectionKind::Transcript,
            anchor,
            direction,
            target: ProjectionWindowTarget {
                min_items: Some(2),
                min_estimated_rows: None,
                min_bytes: None,
                width_columns: Some(80),
            },
            limits: ProjectionWindowLimits {
                max_items: 2,
                max_events_scanned: 8,
                max_bytes: 4096,
            },
        };

        let latest = manager
            .session_projection_window(
                session.id,
                request(
                    ProjectionWindowAnchor::Latest,
                    ProjectionWindowDirection::Backward,
                ),
            )
            .await
            .expect("latest window");
        assert_eq!(
            latest.source_range,
            Some(bcode_session_models::ProjectionSourceRange {
                start_sequence: 5,
                end_sequence: 6,
            })
        );
        assert!(latest.has_older);
        assert!(!latest.has_newer);

        let older = manager
            .session_projection_window(
                session.id,
                request(
                    ProjectionWindowAnchor::BeforeSequence(5),
                    ProjectionWindowDirection::Backward,
                ),
            )
            .await
            .expect("older window");
        assert_eq!(
            older.source_range,
            Some(bcode_session_models::ProjectionSourceRange {
                start_sequence: 3,
                end_sequence: 4,
            })
        );
        assert!(older.has_older);
        assert!(older.has_newer);

        let newer = manager
            .session_projection_window(
                session.id,
                request(
                    ProjectionWindowAnchor::AfterSequence(4),
                    ProjectionWindowDirection::Forward,
                ),
            )
            .await
            .expect("newer window");
        assert_eq!(newer.source_range, latest.source_range);
        assert!(newer.has_older);
        assert!(!newer.has_newer);

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
    #[allow(clippy::too_many_lines)]
    async fn catalog_discovers_every_migration_fixture_without_mutation() {
        let root = unique_temp_dir();
        let fixtures = [
            (
                "future-schema-v40.json",
                include_str!("../fixtures/migrations/future-schema-v40.json"),
            ),
            (
                "interactive-tool-request-created-v32.json",
                include_str!("../fixtures/migrations/interactive-tool-request-created-v32.json"),
            ),
            (
                "interactive-tool-request-resolved-v32.json",
                include_str!("../fixtures/migrations/interactive-tool-request-resolved-v32.json"),
            ),
            (
                "interactive-tool-request-unresolved-v32.json",
                include_str!("../fixtures/migrations/interactive-tool-request-unresolved-v32.json"),
            ),
            (
                "malformed-json-v38.json",
                include_str!("../fixtures/migrations/malformed-json-v38.json"),
            ),
            (
                "mismatched-session-id-v38.json",
                include_str!("../fixtures/migrations/mismatched-session-id-v38.json"),
            ),
            (
                "plugin-automation-turn-finished-v29.json",
                include_str!("../fixtures/migrations/plugin-automation-turn-finished-v29.json"),
            ),
            (
                "plugin-automation-turn-started-v29.json",
                include_str!("../fixtures/migrations/plugin-automation-turn-started-v29.json"),
            ),
            (
                "plugin-status-note-v29.json",
                include_str!("../fixtures/migrations/plugin-status-note-v29.json"),
            ),
            (
                "tool-presentation-diff-v25.json",
                include_str!("../fixtures/migrations/tool-presentation-diff-v25.json"),
            ),
            (
                "unknown-future-event-kind-v38.json",
                include_str!("../fixtures/migrations/unknown-future-event-kind-v38.json"),
            ),
            (
                "unknown-old-event-kind-v32.json",
                include_str!("../fixtures/migrations/unknown-old-event-kind-v32.json"),
            ),
            (
                "mixed-interactive-history-v32-v35.jsonl",
                include_str!("../fixtures/migrations/mixed-interactive-history-v32-v35.jsonl"),
            ),
            (
                "sequence-gap-v38.jsonl",
                include_str!("../fixtures/migrations/sequence-gap-v38.jsonl"),
            ),
        ];
        let mut expected = BTreeMap::new();

        for (fixture_name, fixture) in fixtures {
            let session_id = SessionId::new();
            let db = db::SessionDb::open_turso_in_root(session_id, &root)
                .await
                .expect("fixture session DB should initialize");
            for (line_index, payload) in fixture.lines().enumerate() {
                let parsed = serde_json::from_str::<serde_json::Value>(payload).ok();
                let sequence = parsed
                    .as_ref()
                    .and_then(|value| value["sequence"].as_u64())
                    .unwrap_or_else(|| u64::try_from(line_index).expect("line index should fit"));
                let event_type = parsed
                    .as_ref()
                    .and_then(|value| value["kind"].as_object())
                    .and_then(|kind| kind.keys().next())
                    .map_or_else(|| "malformed_fixture".to_owned(), Clone::clone);
                let schema_version = parsed
                    .as_ref()
                    .and_then(|value| value["schema_version"].as_i64())
                    .and_then(|version| i32::try_from(version).ok())
                    .unwrap_or_else(|| i32::from(CURRENT_SESSION_EVENT_SCHEMA_VERSION));
                let created_at_ms = parsed
                    .as_ref()
                    .and_then(|value| value["timestamp_ms"].as_i64())
                    .unwrap_or(0);
                db.database()
                    .insert("events")
                    .value(
                        "event_seq",
                        switchy::database::DatabaseValue::Int64(
                            i64::try_from(sequence).expect("sequence should fit"),
                        ),
                    )
                    .value("event_type", event_type)
                    .value(
                        "schema_version",
                        switchy::database::DatabaseValue::Int32(schema_version),
                    )
                    .value(
                        "created_at_ms",
                        switchy::database::DatabaseValue::Int64(created_at_ms),
                    )
                    .value("payload", payload)
                    .execute(db.database())
                    .await
                    .unwrap_or_else(|error| {
                        panic!("fixture {fixture_name} row should insert: {error}")
                    });
            }
            drop(db);
            std::thread::sleep(Duration::from_millis(20));
            expected.insert(
                session_id,
                (fixture_name, session_database_files(&root, session_id)),
            );
        }

        let catalog = SessionStore::new(&root)
            .load_catalog()
            .expect("fixture catalog should load");
        assert_eq!(catalog.len(), expected.len());
        for (session_id, (fixture_name, before)) in expected {
            let state = catalog
                .get(&session_id)
                .unwrap_or_else(|| panic!("fixture {fixture_name} should be discoverable"));
            assert_eq!(state.load_status, SessionLoadStatusKind::SummaryOnly);
            assert_eq!(
                session_database_files(&root, session_id),
                before,
                "catalog discovery must not mutate fixture {fixture_name}"
            );
        }

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn mixed_legacy_fixture_is_discoverable_migrates_and_preserves_bounded_history() {
        let root = unique_temp_dir();
        let session_id = SessionId::new();
        let db = db::SessionDb::open_turso_in_root(session_id, &root)
            .await
            .expect("session DB should initialize");
        db.append_event(&bcode_session_models::SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::SessionCreated {
                name: Some("mixed fixture".to_owned()),
                working_directory: test_working_directory(),
            },
        })
        .await
        .expect("session-created event should append");

        for (offset, fixture) in
            include_str!("../fixtures/migrations/mixed-interactive-history-v32-v35.jsonl")
                .lines()
                .enumerate()
        {
            let sequence = u64::try_from(offset).expect("fixture offset should fit") + 1;
            let mut payload: serde_json::Value =
                serde_json::from_str(fixture).expect("fixture should be valid JSON");
            let schema_version = payload["schema_version"]
                .as_u64()
                .and_then(|version| i32::try_from(version).ok())
                .expect("fixture schema should fit");
            let event_type = payload["kind"]
                .as_object()
                .and_then(|kind| kind.keys().next())
                .expect("fixture should have one event kind")
                .clone();
            payload["sequence"] = sequence.into();
            payload["session_id"] = serde_json::to_value(session_id).expect("session ID JSON");
            db.database()
                .insert("events")
                .value(
                    "event_seq",
                    switchy::database::DatabaseValue::Int64(
                        i64::try_from(sequence).expect("sequence should fit"),
                    ),
                )
                .value("event_type", event_type)
                .value(
                    "schema_version",
                    switchy::database::DatabaseValue::Int32(schema_version),
                )
                .value(
                    "created_at_ms",
                    switchy::database::DatabaseValue::Int64(
                        payload["timestamp_ms"]
                            .as_i64()
                            .expect("fixture timestamp should fit"),
                    ),
                )
                .value("payload", payload.to_string())
                .execute(db.database())
                .await
                .expect("fixture event should insert");
        }
        let mut unresolved: serde_json::Value = serde_json::from_str(include_str!(
            "../fixtures/migrations/interactive-tool-request-unresolved-v32.json"
        ))
        .expect("unresolved fixture JSON");
        unresolved["sequence"] = 4_u64.into();
        unresolved["session_id"] =
            serde_json::to_value(session_id).expect("unresolved session ID JSON");
        db.database()
            .insert("events")
            .value("event_seq", switchy::database::DatabaseValue::Int64(4))
            .value("event_type", "interactive_tool_request_created")
            .value(
                "schema_version",
                switchy::database::DatabaseValue::Int32(32),
            )
            .value(
                "created_at_ms",
                switchy::database::DatabaseValue::Int64(
                    unresolved["timestamp_ms"].as_i64().expect("timestamp"),
                ),
            )
            .value("payload", unresolved.to_string())
            .execute(db.database())
            .await
            .expect("unresolved fixture event should insert");
        db.database()
            .update("session_storage_contract")
            .value("writer_epoch", switchy::database::DatabaseValue::Int64(3))
            .where_eq("contract_id", switchy::database::DatabaseValue::Int32(1))
            .execute(db.database())
            .await
            .expect("fixture store should represent epoch three");
        db.database()
            .delete("__bcode_session_migrations")
            .where_in(
                "id",
                vec![
                    switchy::database::DatabaseValue::String(
                        "028_session_compatibility_state".to_owned(),
                    ),
                    switchy::database::DatabaseValue::String(
                        "029_session_compatibility_issues".to_owned(),
                    ),
                    switchy::database::DatabaseValue::String(
                        "030_session_state_visibility_column".to_owned(),
                    ),
                    switchy::database::DatabaseValue::String(
                        "031_session_state_execution_provenance_column".to_owned(),
                    ),
                ],
            )
            .execute(db.database())
            .await
            .expect("epoch-four migration ledger should clear");
        db.database()
            .exec_raw("ALTER TABLE session_state DROP COLUMN execution_provenance")
            .await
            .expect("execution provenance column should drop");
        db.database()
            .exec_raw("ALTER TABLE session_state DROP COLUMN visibility")
            .await
            .expect("visibility column should drop");
        db.database()
            .exec_raw("DROP TABLE session_compatibility_issues")
            .await
            .expect("compatibility issues should drop");
        db.database()
            .exec_raw("DROP TABLE session_compatibility_state")
            .await
            .expect("compatibility state should drop");
        drop(db);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let before_discovery = session_database_files(&root, session_id);
        let metrics = MetricsRegistry::in_memory();
        let manager = SessionManager::persistent_with_metrics(&root, metrics.clone())
            .expect("manager should initialize");
        let entry = manager
            .all_session_catalog_entries()
            .await
            .into_iter()
            .find(|entry| entry.summary.id == session_id)
            .expect("fixture session should be discovered");
        assert_eq!(entry.load_status, SessionCatalogLoadStatus::SummaryOnly);
        assert_eq!(
            manager.session_health(session_id).await,
            SessionHealth::WriterIncompatible {
                actual: Some(3),
                expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            }
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            session_database_files(&root, session_id),
            before_discovery,
            "catalog discovery and health classification must not mutate fixture storage"
        );

        let terminal = prepare_session_until_terminal(&manager, session_id).await;
        assert_eq!(terminal.outcome, Some(SessionOpenTerminalOutcome::Ready));
        let attachment = manager
            .attach_session_recent(session_id, ClientId::new(), 10)
            .await
            .expect("prepared known-legacy fixture should attach");
        assert_eq!(
            attachment
                .history
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![3],
            "inert legacy events must not be fabricated as transcript entries"
        );
        let page = manager
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    direction: SessionHistoryDirection::Forward,
                    limit: 10,
                },
            )
            .await
            .expect("bounded canonical history should load");
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert!(matches!(
            &page.events[1].kind,
            SessionEventKind::LegacyEvent { event_type, .. }
                if event_type == "interactive_tool_request_created"
        ));
        assert!(matches!(
            &page.events[2].kind,
            SessionEventKind::LegacyEvent { event_type, .. }
                if event_type == "interactive_tool_request_resolved"
        ));
        assert!(matches!(
            &page.events[3].kind,
            SessionEventKind::AssistantMessage { text }
                if text == "history continued under schema 35"
        ));
        assert!(matches!(
            &page.events[4].kind,
            SessionEventKind::LegacyEvent { event_type, .. }
                if event_type == "interactive_tool_request_created"
        ));
        assert_eq!(
            manager.session_health(session_id).await,
            SessionHealth::Ready
        );
        let migrated = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("migrated fixture DB should open");
        assert_eq!(
            migrated
                .last_event_sequence()
                .await
                .expect("canonical tail"),
            Some(4)
        );
        assert!(
            migrated
                .active_runtime_work()
                .await
                .expect("runtime projection")
                .is_empty()
        );
        assert!(
            migrated
                .active_tool_runs()
                .await
                .expect("tool projection")
                .is_empty()
        );
        let model_context = manager
            .model_context_events(session_id)
            .await
            .expect("bounded migrated model context");
        assert_eq!(model_context.len(), 1);
        assert!(matches!(
            &model_context[0].kind,
            SessionEventKind::AssistantMessage { text }
                if text == "history continued under schema 35"
        ));
        let report = metrics.report();
        for metric in [
            "session.migration.ownership_duration_ms",
            "session.migration.backup.plan_duration_ms",
            "session.migration.backup.copy_duration_ms",
            "session.migration.backup.verify_duration_ms",
            "session.migration.schema_duration_ms",
            "session.migration.canonical_decode_duration_ms",
            "session.migration.projection_rebuild_duration_ms",
            "session.migration.projector.materialized_duration_ms",
            "session.migration.projector.model_context_duration_ms",
            "session.migration.projector.context_occupancy_duration_ms",
            "session.migration.projector.turn_receipt_duration_ms",
            "session.migration.projector.compatibility_duration_ms",
            "session.migration.validation_duration_ms",
            "session.migration.commit_duration_ms",
            "session.migration.write_readiness_duration_ms",
        ] {
            assert!(
                report.snapshot.histograms.contains_key(metric),
                "missing migration stage metric {metric}"
            );
        }
        assert_eq!(
            report
                .snapshot
                .counters
                .get("session.migration.canonical_events_total"),
            Some(&5)
        );
        assert_eq!(
            report
                .snapshot
                .counters
                .get("session.migration.projected_events_total"),
            Some(&5)
        );
        assert!(
            report
                .snapshot
                .counters
                .get("session.migration.backup.files_total")
                .is_some_and(|files| *files > 0)
        );
        assert!(
            report
                .snapshot
                .counters
                .get("session.migration.backup.bytes_total")
                .is_some_and(|bytes| *bytes > 0)
        );
        drop(migrated);
        drop(manager);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn normal_open_does_not_decode_canonical_events() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("decode free open".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "canonical payload".to_owned())
            .await
            .expect("message should append");

        let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("session DB should open");
        db.database()
            .update("events")
            .value("payload", "not valid JSON")
            .where_eq("event_seq", switchy::database::DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("canonical payload should corrupt");
        drop(db);
        manager
            .inner
            .lock()
            .await
            .sessions
            .remove(&session.id)
            .expect("cached actor should exist");
        manager.inner.lock().await.leases.remove(&session.id);

        let summary = manager
            .session_summary(session.id)
            .await
            .expect("normal open must use derived state without decoding canonical events");
        assert_eq!(summary.name.as_deref(), Some("decode free open"));

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
    async fn concurrent_same_session_appends_across_managers_have_contiguous_sequences() {
        let root = unique_temp_dir();
        let creator = SessionManager::persistent(&root).expect("manager should initialize");
        let session = creator
            .create_session(Some("cross-manager".to_string()), test_working_directory())
            .await
            .expect("session should create");
        drop(creator);

        let first = std::sync::Arc::new(
            SessionManager::persistent(&root).expect("first manager should restore"),
        );
        let second = std::sync::Arc::new(
            SessionManager::persistent(&root).expect("second manager should restore"),
        );

        let mut tasks = Vec::new();
        for index in 0..16 {
            let manager = if index % 2 == 0 {
                std::sync::Arc::clone(&first)
            } else {
                std::sync::Arc::clone(&second)
            };
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
    async fn provider_snapshot_opaque_context_survives_manager_restart() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("opaque restart".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let snapshot = bcode_session_models::ProviderContextSnapshot {
            format_version: 7,
            request_fingerprint: Some("fingerprint".to_string()),
            request_id: Some("request".to_string()),
            provider_plugin_id: "provider".to_string(),
            model_id: "model".to_string(),
            compatibility_key: "surface".to_string(),
            auth_profile: Some("profile".to_string()),
            origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
            messages_json: r#"[{"opaque":"ciphertext"}]"#.to_string(),
            portable_summary: "portable fallback".to_string(),
        };
        manager
            .append_provider_context_compacted(session.id, snapshot.clone(), 0)
            .await
            .expect("snapshot should persist");
        drop(manager);

        let restored = SessionManager::persistent_lazy(&root);
        let context = restored
            .model_context_events(session.id)
            .await
            .expect("context should reload");

        assert!(context.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ProviderContextCompacted { snapshot: actual, .. }
                if actual == &snapshot
        )));
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
    async fn concurrent_duplicate_turn_admission_is_atomic() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("idempotency".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let metadata = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: None,
                display_label: None,
            }),
            idempotency_key: Some("operation-1".to_string()),
            ..bcode_session_models::TurnAdmissionMetadata::default()
        };

        let first = manager.admit_turn(
            session.id,
            ClientId::new(),
            "first".to_string(),
            metadata.clone(),
        );
        let second =
            manager.admit_turn(session.id, ClientId::new(), "second".to_string(), metadata);
        let (first, second) = tokio::join!(first, second);
        let first = first.expect("first concurrent admission");
        let second = second.expect("second concurrent admission");

        let accepted = [&first, &second]
            .into_iter()
            .filter(|admission| {
                matches!(admission, bcode_session_models::TurnAdmission::Accepted(_))
            })
            .count();
        let existing = [&first, &second]
            .into_iter()
            .filter(|admission| {
                matches!(admission, bcode_session_models::TurnAdmission::Existing(_))
            })
            .count();
        assert_eq!((accepted, existing), (1, 1));
        let first_receipt = match first {
            bcode_session_models::TurnAdmission::Accepted(receipt)
            | bcode_session_models::TurnAdmission::Existing(receipt) => receipt,
            other => panic!("unexpected admission: {other:?}"),
        };
        let second_receipt = match second {
            bcode_session_models::TurnAdmission::Accepted(receipt)
            | bcode_session_models::TurnAdmission::Existing(receipt) => receipt,
            other => panic!("unexpected admission: {other:?}"),
        };
        assert_eq!(first_receipt, second_receipt);
    }

    #[tokio::test]
    async fn idempotent_turn_admission_returns_existing_receipt_without_duplicate_event() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("idempotency".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let metadata = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: Some("run-1".to_string()),
                display_label: Some("Background pass 1".to_string()),
            }),
            priority: bcode_session_models::TurnPriority::Background,
            idempotency_key: Some("operation-1".to_string()),
            execution: bcode_session_models::TurnExecutionOptions {
                tools: bcode_session_models::TurnToolPolicy::Disabled,
                ..bcode_session_models::TurnExecutionOptions::default()
            },
        };

        let first = manager
            .admit_turn(
                session.id,
                ClientId::new(),
                "prompt".to_string(),
                metadata.clone(),
            )
            .await
            .expect("first admission should succeed");
        let duplicate = manager
            .admit_turn(
                session.id,
                ClientId::new(),
                "different text must not append".to_string(),
                metadata,
            )
            .await
            .expect("duplicate admission should succeed");

        let bcode_session_models::TurnAdmission::Accepted(receipt) = first else {
            panic!("first admission should be accepted");
        };
        assert_eq!(
            duplicate,
            bcode_session_models::TurnAdmission::Existing(receipt)
        );
        let history = manager.session_history(session.id).await.expect("history");
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::UserMessage { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn persisted_idempotent_turn_receipt_survives_manager_restart() {
        let root = unique_temp_dir();
        let session_id;
        let expected;
        let metadata = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: None,
                display_label: None,
            }),
            idempotency_key: Some("operation-1".to_string()),
            ..bcode_session_models::TurnAdmissionMetadata::default()
        };
        {
            let manager = SessionManager::persistent(&root).expect("manager");
            let session = manager
                .create_session(Some("idempotency".to_string()), test_working_directory())
                .await
                .expect("session");
            session_id = session.id;
            expected = manager
                .admit_turn(
                    session_id,
                    ClientId::new(),
                    "prompt".to_string(),
                    metadata.clone(),
                )
                .await
                .expect("admission");
        }

        let restored = SessionManager::persistent(&root).expect("restored manager");
        let duplicate = restored
            .admit_turn(
                session_id,
                ClientId::new(),
                "different".to_string(),
                metadata,
            )
            .await
            .expect("duplicate");
        let bcode_session_models::TurnAdmission::Accepted(receipt) = expected else {
            panic!("first admission should be accepted");
        };
        assert_eq!(
            duplicate,
            bcode_session_models::TurnAdmission::Existing(receipt)
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn generic_turn_origin_is_persisted_on_the_ordinary_user_message_path() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("origin".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let origin = bcode_session_models::TurnOrigin {
            producer: "test.producer".to_string(),
            correlation_id: Some("operation-1".to_string()),
            display_label: Some("Background pass 1".to_string()),
        };

        let events = manager
            .append_user_message_with_origin(
                session.id,
                ClientId::new(),
                "ordinary prompt".to_string(),
                Some(origin.clone()),
            )
            .await
            .expect("message should append");

        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(SessionEventKind::UserMessage {
                text,
                admission:
                    bcode_session_models::TurnAdmissionMetadata {
                        origin: Some(actual),
                        ..
                    },
                ..
            }) if text == "ordinary prompt" && actual == &origin
        ));
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
    async fn catalog_listing_remains_lease_free_with_an_incompatible_live_owner() {
        let root = unique_temp_dir();
        let writer = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                storage_writer_epoch: Some(lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
                build_fingerprint: Some("current-writer".to_owned()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("writer manager should initialize");
        let session = writer
            .create_session(Some("catalog-only".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        assert_eq!(
            lease::active_session_owners(&root, session.id)
                .expect("owners should be readable")
                .len(),
            1,
            "the loaded writer should hold exactly one session lease"
        );

        let passive_reader = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                storage_writer_epoch: Some(lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH - 1),
                build_fingerprint: Some("incompatible-passive-reader".to_owned()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("catalog loading must not acquire a session lease");
        assert!(
            passive_reader
                .all_session_summaries()
                .await
                .iter()
                .any(|summary| summary.id == session.id),
            "passive catalog listing should discover the owned session"
        );
        assert_eq!(
            lease::active_session_owners(&root, session.id)
                .expect("owners should remain readable")
                .len(),
            1,
            "passive discovery must not create an owner record"
        );

        drop(passive_reader);
        drop(writer);
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn canonical_database_remains_visible_without_manifest_or_catalog() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent_with_metrics_and_lease_owner(
                &root,
                MetricsRegistry::default(),
                SessionLeaseOwnerContext {
                    build_fingerprint: Some("discovery-build".to_owned()),
                    ..SessionLeaseOwnerContext::default()
                },
            )
            .expect("manager should initialize");
            manager
                .create_session(Some("canonical".to_owned()), test_working_directory())
                .await
                .expect("session")
                .id
        };
        std::fs::remove_file(db::session_dir_path(&root, session_id).join("manifest.json"))
            .expect("remove manifest");
        std::fs::remove_dir_all(root.join("catalogs")).expect("remove catalogs");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        assert!(
            restored
                .all_session_summaries()
                .await
                .iter()
                .any(|session| session.id == session_id),
            "canonical session directory must not be hidden by missing caches"
        );
        restored
            .require_write_readiness(session_id)
            .await
            .expect("canonical database should load");
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn unreadable_manifest_and_catalog_cache_do_not_hide_canonical_database() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent_with_metrics_and_lease_owner(
                &root,
                MetricsRegistry::default(),
                SessionLeaseOwnerContext {
                    build_fingerprint: Some("corrupt-cache".to_owned()),
                    ..SessionLeaseOwnerContext::default()
                },
            )
            .expect("manager should initialize");
            manager
                .create_session(Some("canonical".to_owned()), test_working_directory())
                .await
                .expect("session")
                .id
        };
        std::fs::write(
            db::session_dir_path(&root, session_id).join("manifest.json"),
            b"not valid JSON",
        )
        .expect("corrupt manifest");
        let catalogs = root.join("catalogs");
        std::fs::remove_dir_all(&catalogs).expect("remove catalogs");
        let catalog = db::namespaced_catalog_db_path(&root, "corrupt-cache");
        std::fs::create_dir_all(catalog.parent().expect("catalog parent"))
            .expect("create catalog parent");
        std::fs::write(&catalog, b"not a database").expect("corrupt catalog");

        let restored = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                build_fingerprint: Some("corrupt-cache".to_owned()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("derived catalog damage must not fail discovery");
        let entry = restored
            .all_session_catalog_entries()
            .await
            .into_iter()
            .find(|entry| entry.summary.id == session_id)
            .expect("canonical session should remain visible");
        assert_eq!(entry.load_status, SessionCatalogLoadStatus::SummaryOnly);
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn bounded_manifest_lists_known_legacy_format_without_opening_session_database() {
        let root = unique_temp_dir();
        let session_id = SessionId::new();
        let session_dir = db::session_dir_path(&root, session_id);
        std::fs::create_dir_all(&session_dir).expect("session dir");
        std::fs::write(
            session_dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": 1,
                "summary": {
                    "id": session_id,
                    "name": "legacy session",
                    "explicit_name": "legacy session",
                    "derived_title": null,
                    "title_source": "explicit",
                    "client_count": 0,
                    "created_at_ms": 1,
                    "updated_at_ms": 2,
                    "working_directory": root,
                    "import": null,
                    "fork": null
                }
            })
            .to_string(),
        )
        .expect("old manifest");
        let legacy_db_path = db::session_db_path(&root, session_id);
        std::fs::write(&legacy_db_path, b"not a database").expect("database sentinel");
        let current_session_id = SessionId::new();
        let current_session_dir = db::session_dir_path(&root, current_session_id);
        std::fs::create_dir_all(&current_session_dir).expect("current session dir");
        std::fs::write(
            current_session_dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": SESSION_MANIFEST_SCHEMA_VERSION,
                "session_format": {
                    "family": SESSION_FORMAT_FAMILY,
                    "epoch": CURRENT_SESSION_FORMAT_EPOCH
                },
                "summary": {
                    "id": current_session_id,
                    "name": "current session",
                    "explicit_name": "current session",
                    "derived_title": null,
                    "title_source": "explicit",
                    "client_count": 0,
                    "created_at_ms": 3,
                    "updated_at_ms": 4,
                    "working_directory": root,
                    "import": null,
                    "fork": null
                }
            })
            .to_string(),
        )
        .expect("current manifest");
        let current_db_path = db::session_db_path(&root, current_session_id);
        std::fs::write(&current_db_path, b"also not a database").expect("database sentinel");
        let store = SessionStore::new(&root);
        let summary = store
            .load_session_manifest(session_id)
            .expect("known legacy manifest should load")
            .expect("manifest summary");
        assert_eq!(summary.id, session_id);
        assert_eq!(summary.display_title(), "legacy session");
        let catalog = store.load_catalog().expect("bounded catalog load");
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains_key(&session_id));
        assert!(catalog.contains_key(&current_session_id));

        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        manager
            .wait_catalog_loaded()
            .await
            .expect("catalog should load");
        let sessions = manager.list_sessions(&root).await;
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|summary| summary.id == session_id));
        assert!(
            sessions
                .iter()
                .any(|summary| summary.id == current_session_id)
        );
        let entries = manager.all_session_catalog_entries().await;
        assert!(
            entries
                .iter()
                .all(|entry| matches!(entry.load_status, SessionCatalogLoadStatus::SummaryOnly))
        );
        assert_eq!(
            std::fs::read(&legacy_db_path).expect("legacy database sentinel"),
            b"not a database",
            "passive listing must not open or mutate the legacy database"
        );
        assert_eq!(
            std::fs::read(&current_db_path).expect("current database sentinel"),
            b"also not a database",
            "passive listing must not open or mutate the current database"
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[test]
    fn bounded_manifest_rejects_unknown_and_inconsistent_metadata() {
        let root = unique_temp_dir();
        let future_session_id = SessionId::new();
        let future_session_dir = db::session_dir_path(&root, future_session_id);
        std::fs::create_dir_all(&future_session_dir).expect("future session dir");
        std::fs::write(
            future_session_dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": SESSION_MANIFEST_SCHEMA_VERSION + 1,
                "summary": { "id": future_session_id }
            })
            .to_string(),
        )
        .expect("future manifest");
        let future_db_path = db::session_db_path(&root, future_session_id);
        std::fs::write(&future_db_path, b"future database sentinel").expect("database sentinel");

        let mismatched_session_id = SessionId::new();
        let mismatched_session_dir = db::session_dir_path(&root, mismatched_session_id);
        std::fs::create_dir_all(&mismatched_session_dir).expect("mismatched session dir");
        std::fs::write(
            mismatched_session_dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": 1,
                "summary": {
                    "id": SessionId::new(),
                    "name": null,
                    "client_count": 0,
                    "created_at_ms": 0,
                    "updated_at_ms": 0,
                    "working_directory": root
                }
            })
            .to_string(),
        )
        .expect("mismatched manifest");
        let mismatched_db_path = db::session_db_path(&root, mismatched_session_id);
        std::fs::write(&mismatched_db_path, b"mismatched database sentinel")
            .expect("database sentinel");

        let store = SessionStore::new(&root);
        assert!(
            store
                .load_session_manifest(future_session_id)
                .expect_err("future manifest should fail closed")
                .to_string()
                .contains("unsupported session manifest schema version")
        );
        assert!(
            store
                .load_session_manifest(mismatched_session_id)
                .expect_err("mismatched manifest should fail closed")
                .to_string()
                .contains("session manifest id mismatch")
        );
        let catalog = store.load_catalog().expect("bounded catalog load");
        assert_eq!(catalog.len(), 2);
        for session_id in [future_session_id, mismatched_session_id] {
            let state = catalog
                .get(&session_id)
                .expect("canonical fallback should remain visible");
            assert_eq!(state.load_status, SessionLoadStatusKind::SummaryOnly);
            assert_eq!(state.summary.id, session_id);
            assert_eq!(state.summary.name, None);
        }
        assert_eq!(
            std::fs::read(&future_db_path).expect("future database sentinel"),
            b"future database sentinel"
        );
        assert_eq!(
            std::fs::read(&mismatched_db_path).expect("mismatched database sentinel"),
            b"mismatched database sentinel"
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn persistent_sessions_write_manifest_and_scoped_catalog() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                build_fingerprint: Some("test-build".to_string()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("manager should initialize");
        let session = manager
            .create_session(Some("manifested".to_string()), test_working_directory())
            .await
            .expect("session should create");

        let manifest_path = root.join(session.id.to_string()).join("manifest.json");
        assert!(manifest_path.exists());
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest should read"))
                .expect("manifest should decode");
        assert_eq!(manifest["schema_version"], SESSION_MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest["session_format"]["family"], SESSION_FORMAT_FAMILY);
        assert_eq!(
            manifest["session_format"]["epoch"],
            CURRENT_SESSION_FORMAT_EPOCH
        );
        assert!(
            db::namespaced_catalog_db_path(&root, "test-build").exists(),
            "catalog should be build-scoped"
        );
        assert!(
            !db::global_catalog_db_path(&root).exists(),
            "build-scoped managers should not create the legacy shared catalog"
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn catalog_load_uses_global_catalog_without_opening_session_db() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("global catalog".to_string()), test_working_directory())
            .await
            .expect("session should create");
        drop(manager);
        let session_db = db::session_db_path(&root, session.id);
        let hidden_db = session_db.with_extension("db.hidden");
        std::fs::rename(&session_db, &hidden_db).expect("hide session db");

        let restored = SessionManager::persistent_lazy(&root);
        restored.start_catalog_load();
        let mut status = restored.subscribe_catalog_status();
        loop {
            if matches!(*status.borrow(), super::CatalogLoadStatus::Loaded) {
                break;
            }
            status.changed().await.expect("status should change");
        }
        let sessions = restored.cached_sessions(&test_working_directory()).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        assert_eq!(sessions[0].name.as_deref(), Some("global catalog"));

        std::fs::rename(hidden_db, session_db).expect("restore session db");
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

    #[tokio::test]
    async fn transient_contribution_is_rejected_before_durable_append() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("persistent manager");
        let session = manager
            .create_session(
                Some("transient contribution".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session");
        let result = manager
            .append_event(
                session.id,
                SessionEventKind::ToolContribution {
                    event: bcode_session_models::ToolContributionEvent {
                        invocation_id: "call".to_owned(),
                        contribution_id: "transient".to_owned(),
                        sequence: 1,
                        producer_id: "producer".to_owned(),
                        schema: "example.transient".to_owned(),
                        schema_version: 1,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Transient,
                        artifact: None,
                        payload: serde_json::json!({"must_not_persist": true}),
                    },
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(SessionError::LiveEventPersistenceRejected {
                event_kind: "tool_contribution"
            })
        ));
        assert!(
            !manager
                .session_history(session.id)
                .await
                .expect("history")
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::ToolContribution { .. }))
        );
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn unknown_durable_contribution_replays_opaquely() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("persistent manager");
        let session = manager
            .create_session(
                Some("opaque contribution".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session");
        let contribution = bcode_session_models::ToolContributionEvent {
            invocation_id: "call".to_owned(),
            contribution_id: "opaque".to_owned(),
            sequence: 9,
            producer_id: "future.producer".to_owned(),
            schema: "future.unknown/schema".to_owned(),
            schema_version: 4_294_967_000,
            operation: bcode_session_models::ToolContributionOperation::Append,
            persistence: bcode_session_models::ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({"nested": [1, {"future": true}], "number": 1.25}),
        };
        manager
            .append_event(
                session.id,
                SessionEventKind::ToolContribution {
                    event: contribution.clone(),
                },
            )
            .await
            .expect("durable contribution append");
        drop(manager);

        let restored = SessionManager::persistent(&root).expect("restore manager");
        let replayed = restored
            .session_history(session.id)
            .await
            .expect("replayed history")
            .into_iter()
            .find_map(|event| match event.kind {
                SessionEventKind::ToolContribution { event } => Some(event),
                _ => None,
            })
            .expect("durable contribution");
        assert_eq!(replayed, contribution);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
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
    async fn lazy_catalog_ignores_uncataloged_db_session() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let good = manager
            .create_session(Some("good".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let bad_id = SessionId::new();
        let bad_dir = root.join(bad_id.to_string());
        std::fs::create_dir_all(&bad_dir).expect("bad session dir should create");
        let bad_path = bad_dir.join("session.db");
        std::fs::File::create(&bad_path)
            .expect("bad session DB should create")
            .write_all(&[1_u8])
            .expect("bad session DB should write");

        let restored = SessionManager::persistent_lazy(&root);
        restored
            .wait_catalog_loaded()
            .await
            .expect("catalog load should not inspect uncataloged session DBs");
        let sessions = restored.cached_sessions(&test_working_directory()).await;
        assert!(sessions.iter().any(|session| session.id == good.id));
        assert!(!sessions.iter().any(|session| session.id == bad_id));

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
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
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
                    producer_plugin_id: None,
                    tool_name: "tool".to_string(),
                    arguments_json: "{}".to_string(),
                    working_directory: None,
                    request_visual: None,
                    legacy_request_presentation: None,
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
                    semantic_result: None,
                },
            ),
            (
                8,
                "PermissionRequested",
                SessionEventKind::PermissionRequested {
                    permission_id: "permission".to_string(),
                    tool_call_id: "call".to_string(),
                    producer_plugin_id: None,
                    tool_name: "tool".to_string(),
                    arguments_json: "{}".to_string(),
                    legacy_request_presentation: None,
                    batch: None,
                    policy_source: None,
                    policy_reason: None,
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
                    source: None,
                    preview: None,
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
                    work_id: WorkId::new("work"),
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
                    work_id: WorkId::new("work"),
                    requested_at_ms: Some(2),
                    client_id: Some(client_id),
                },
            ),
            (
                29,
                "RuntimeWorkFinished",
                SessionEventKind::RuntimeWorkFinished {
                    work_id: WorkId::new("work"),
                    status: RuntimeWorkStatus::Completed,
                    finished_at_ms: Some(3),
                    message: None,
                },
            ),
            (
                30,
                "RuntimeWorkProgress",
                SessionEventKind::RuntimeWorkProgress {
                    work_id: WorkId::new("work"),
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
            (
                35,
                "SessionForked",
                SessionEventKind::SessionForked {
                    source_session_id: SessionId::new(),
                    source_title: Some("source".to_string()),
                    source_cutoff_sequence: Some(2),
                    source_prompt_sequence: Some(3),
                    forked_at_ms: 1,
                    kind: SessionForkKind::Fork,
                },
            ),
            (
                36,
                "RalphLifecycle",
                SessionEventKind::RalphLifecycle {
                    loop_name: "loop".to_string(),
                    state_dir: test_working_directory(),
                    kind: "started".to_string(),
                    message: "message".to_string(),
                    occurred_at_ms: 1,
                },
            ),
            (
                37,
                "ReasoningChanged",
                SessionEventKind::ReasoningChanged {
                    effort: Some("medium".to_string()),
                    summary: Some("auto".to_string()),
                },
            ),
            (
                40,
                "ProviderContextCompacted",
                SessionEventKind::ProviderContextCompacted {
                    snapshot: bcode_session_models::ProviderContextSnapshot {
                        provider_plugin_id: "provider".to_string(),
                        model_id: "model".to_string(),
                        auth_profile: None,
                        format_version: 1,
                        compatibility_key: "key".to_string(),
                        messages_json: "[]".to_string(),
                        portable_summary: "summary".to_string(),
                        origin:
                            bcode_session_models::ProviderContextSnapshotOrigin::ProviderManaged,
                        request_id: None,
                        request_fingerprint: None,
                    },
                    compacted_through_sequence: 1,
                },
            ),
            (
                41,
                "RequestContextObserved",
                SessionEventKind::RequestContextObserved {
                    observation: bcode_session_models::RequestContextObservation {
                        request: bcode_session_models::ModelRequestIdentity {
                            provider_plugin_id: "provider".to_string(),
                            requested_model_id: None,
                            effective_model_id: "model".to_string(),
                            request_id: "request".to_string(),
                            model_turn_id: "turn".to_string(),
                            round: 0,
                            request_fingerprint: "fingerprint".to_string(),
                            effective_auth_profile: None,
                            context_format_version: None,
                            compatibility_key: None,
                            context_epoch: 0,
                        },
                        context_through_sequence: 1,
                        context_tokens: bcode_session_models::RequestContextTokenCount::Estimated(
                            1,
                        ),
                        local_estimate: bcode_session_models::LocalContextEstimate {
                            tokens: 1,
                            algorithm_version: 1,
                        },
                    },
                },
            ),
            (
                42,
                "PluginStatusNote",
                SessionEventKind::PluginStatusNote {
                    plugin_id: "plugin".to_string(),
                    note_id: "note".to_string(),
                    text: "status".to_string(),
                    metadata: BTreeMap::new(),
                },
            ),
            (
                43,
                "LegacyEvent",
                SessionEventKind::LegacyEvent {
                    event_type: "legacy".to_string(),
                    payload: serde_json::Value::Null,
                },
            ),
            (
                44,
                "ToolInvocationLifecycle",
                SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: "call".to_string(),
                        sequence: 1,
                        stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                        message: None,
                        metadata: serde_json::Value::Null,
                    },
                },
            ),
            (
                45,
                "ToolContribution",
                SessionEventKind::ToolContribution {
                    event: bcode_session_models::ToolContributionEvent {
                        invocation_id: "call".to_string(),
                        contribution_id: "surface".to_string(),
                        sequence: 1,
                        producer_id: "producer".to_string(),
                        schema: "example.unknown".to_string(),
                        schema_version: 7,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Durable,
                        artifact: None,
                        payload: serde_json::json!({"opaque": true}),
                    },
                },
            ),
            (
                38,
                "ToolExchangeRequested",
                SessionEventKind::ToolExchangeRequested {
                    request: bcode_session_models::ToolExchangeRequest {
                        invocation_id: "call".to_string(),
                        exchange_id: "question".to_string(),
                        producer_id: "producer".to_string(),
                        schema: "example.question".to_string(),
                        schema_version: 1,
                        payload: serde_json::json!({"opaque": "request"}),
                        response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                    },
                },
            ),
            (
                39,
                "ToolExchangeResolved",
                SessionEventKind::ToolExchangeResolved {
                    event: bcode_session_models::ToolExchangeResolutionEvent {
                        invocation_id: "call".to_string(),
                        exchange_id: "question".to_string(),
                        resolution: bcode_session_models::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({"opaque": "response"}),
                        },
                    },
                },
            ),
            (
                46,
                "ToolContributionResultRecorded",
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call".to_owned(),
                        model_output: "done".to_owned(),
                        is_error: false,
                        result: None,
                    },
                },
            ),
            (
                47,
                "ToolContributionPlaced",
                SessionEventKind::ToolContributionPlaced {
                    envelope: bcode_session_models::ToolContributionEnvelope::new(
                        bcode_session_models::ToolContributionPlacement::Request,
                        bcode_session_models::ToolContributionEvent {
                            invocation_id: "call".to_owned(),
                            contribution_id: "request".to_owned(),
                            sequence: 1,
                            producer_id: "producer".to_owned(),
                            schema: "example.request".to_owned(),
                            schema_version: 1,
                            operation: bcode_session_models::ToolContributionOperation::Upsert,
                            persistence: bcode_session_models::ToolContributionPersistence::Durable,
                            artifact: None,
                            payload: serde_json::json!({"path": "src/lib.rs"}),
                        },
                    ),
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
            (
                19,
                "ContextCompactionDiagnostic",
                SessionTracePhase::ContextCompactionDiagnostic,
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

    #[tokio::test]
    async fn background_execution_sessions_are_hidden_but_inspectable() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        let provenance = ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review-a".to_string(),
            attempt: 1,
            parent_session_id: parent.id,
            context_mode: ExecutionSessionContextMode::FreshIsolated,
            parent_generation: None,
        };
        let child = manager
            .create_fresh_execution_session(Some("review".to_string()), provenance.clone(), None)
            .await
            .expect("child");

        assert_eq!(
            child.execution.as_ref().expect("execution").visibility,
            SessionVisibility::Background
        );
        assert_eq!(
            child
                .execution
                .as_ref()
                .map(|execution| &execution.provenance),
            Some(&provenance)
        );
        assert_eq!(child.working_directory, parent.working_directory);
        assert_eq!(
            manager.list_sessions(&parent.working_directory).await,
            vec![parent]
        );
        assert!(
            manager
                .list_sessions_with_background(&child.working_directory, true)
                .await
                .iter()
                .any(|summary| summary.id == child.id)
        );
        assert_eq!(
            manager
                .session_summary(child.id)
                .await
                .expect("direct inspect")
                .id,
            child.id
        );
    }

    #[tokio::test]
    async fn two_fixed_generation_reviewers_have_independent_transcripts() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        manager
            .append_event(
                parent.id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "shared snapshot".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
            .await
            .expect("snapshot prompt");
        let generation = manager
            .session_history(parent.id)
            .await
            .expect("history")
            .last()
            .expect("event")
            .sequence;
        let create = |node_id: &str| ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: node_id.to_string(),
            attempt: 1,
            parent_session_id: parent.id,
            context_mode: ExecutionSessionContextMode::FixedGenerationFork,
            parent_generation: Some(generation),
        };
        let (left, right) = tokio::join!(
            manager.create_fixed_generation_execution_session(
                Some("left".to_string()),
                create("review-left"),
                generation,
                None,
            ),
            manager.create_fixed_generation_execution_session(
                Some("right".to_string()),
                create("review-right"),
                generation,
                None,
            ),
        );
        let left = left.expect("left");
        let right = right.expect("right");
        assert_ne!(left.id, right.id);
        assert_eq!(left.working_directory, right.working_directory);
        let left_history = manager
            .session_history(left.id)
            .await
            .expect("left history");
        let right_history = manager
            .session_history(right.id)
            .await
            .expect("right history");
        let inherited = |events: &[SessionEvent]| {
            events
                .iter()
                .filter_map(|event| match &event.kind {
                    SessionEventKind::UserMessage { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(inherited(&left_history), ["shared snapshot".to_string()]);
        assert_eq!(inherited(&right_history), ["shared snapshot".to_string()]);

        manager
            .append_event(
                left.id,
                SessionEventKind::SystemMessage {
                    text: "left-only".to_string(),
                },
            )
            .await
            .expect("left note");
        assert!(
            !manager
                .session_history(right.id)
                .await
                .expect("right history")
                .iter()
                .any(|event| matches!(
                    &event.kind,
                    SessionEventKind::SystemMessage { text } if text == "left-only"
                ))
        );
    }

    #[tokio::test]
    async fn background_execution_provenance_survives_persistent_restore() {
        let root = unique_temp_dir();
        let child_id;
        {
            let manager = SessionManager::persistent(&root).expect("manager");
            let parent = manager
                .create_session(Some("parent".to_string()), test_working_directory())
                .await
                .expect("parent");
            let child = manager
                .create_fresh_execution_session(
                    Some("review".to_string()),
                    ExecutionSessionProvenance {
                        owner: "workflow".to_string(),
                        run_id: "run-1".to_string(),
                        node_id: "review".to_string(),
                        attempt: 1,
                        parent_session_id: parent.id,
                        context_mode: ExecutionSessionContextMode::FreshIsolated,
                        parent_generation: None,
                    },
                    None,
                )
                .await
                .expect("child");
            child_id = child.id;
        }

        let restored = SessionManager::persistent(&root).expect("restored");
        let summary = restored
            .session_summary(child_id)
            .await
            .expect("inspect restored child");
        assert_eq!(
            summary.execution.as_ref().expect("execution").visibility,
            SessionVisibility::Background
        );
        assert!(summary.execution.as_ref().is_some_and(|execution| {
            execution.provenance.owner == "workflow"
                && execution.provenance.run_id == "run-1"
                && execution.provenance.node_id == "review"
        }));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn fixed_generation_execution_session_copies_exact_parent_snapshot() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        manager
            .append_event(
                parent.id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "snapshot".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
            .await
            .expect("prompt");
        let generation = manager
            .session_history(parent.id)
            .await
            .expect("history")
            .last()
            .expect("event")
            .sequence;
        let provenance = ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review-a".to_string(),
            attempt: 1,
            parent_session_id: parent.id,
            context_mode: ExecutionSessionContextMode::FixedGenerationFork,
            parent_generation: Some(generation),
        };
        let child = manager
            .create_fixed_generation_execution_session(
                Some("review".to_string()),
                provenance.clone(),
                generation,
                None,
            )
            .await
            .expect("fixed clone");

        assert_eq!(
            child
                .execution
                .as_ref()
                .map(|execution| &execution.provenance),
            Some(&provenance)
        );
        assert_eq!(child.working_directory, parent.working_directory);
        let child_history = manager
            .session_history(child.id)
            .await
            .expect("child history");
        assert!(child_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "snapshot"
        )));

        manager
            .append_event(
                parent.id,
                SessionEventKind::SystemMessage {
                    text: "later".to_string(),
                },
            )
            .await
            .expect("later event");
        let stale = manager
            .create_fixed_generation_execution_session(
                Some("stale".to_string()),
                ExecutionSessionProvenance {
                    node_id: "review-b".to_string(),
                    ..child.execution.expect("execution").provenance
                },
                generation,
                None,
            )
            .await
            .expect_err("stale generation rejected");
        assert!(matches!(stale, SessionError::CloneGenerationChanged { .. }));
    }

    #[test]
    fn shared_execution_requires_declared_parent_and_no_child() {
        let parent = SessionId::new();
        let provenance = ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: "sequential".to_string(),
            attempt: 1,
            parent_session_id: parent,
            context_mode: ExecutionSessionContextMode::SharedSequential,
            parent_generation: None,
        };
        assert_eq!(
            shared_execution_session(parent, &provenance).expect("shared parent"),
            parent
        );
        assert!(shared_execution_session(SessionId::new(), &provenance).is_err());
    }

    fn encoded_variant_tag(value: &impl Serialize) -> u32 {
        let bytes = bmux_codec::to_positional_vec(value).expect("value should encode");
        let (tag, _) = bmux_codec::varint::decode_u32(&bytes).expect("variant tag should decode");
        tag
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
