//! One-time cleanup for historically persisted live tool-stream payloads.
//!
//! This module is maintenance-only. Normal session open, attach, history, and append paths must
//! never call it.

use crate::{db, lease, persisted};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, LegacyTransientStreamKind, SessionEvent,
    SessionEventKind, SessionEventProvenance, SessionId, ToolInvocationStreamEvent,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use switchy::database::DatabaseError;
use thiserror::Error;

/// Whether cleanup only reports changes or applies them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupMode {
    /// Inspect canonical events without writing files.
    DryRun,
    /// Back up and replace eligible payloads.
    Apply,
}

/// A coarse cleanup phase suitable for CLI progress rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupPhase {
    Scanning,
    CreatingBackup,
    ReplacingEvents,
    Validating,
    Compacting,
}

/// Progress emitted by one session cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupProgress {
    PhaseChanged { phase: CleanupPhase },
    EventsProcessed { processed: usize, total: usize },
}

/// Result of cleaning one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOutcome {
    WouldClean,
    Cleaned,
    Unchanged,
    Skipped,
}

/// Report for one inspected session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionCleanupReport {
    pub session_id: SessionId,
    pub outcome: CleanupOutcome,
    pub events_scanned: usize,
    pub events_pruned: usize,
    pub payload_bytes_before: u64,
    pub payload_bytes_after: u64,
    pub database_bytes_before: u64,
    pub database_bytes_after: u64,
    pub backup_path: Option<PathBuf>,
    pub note: Option<String>,
}

/// Errors returned by legacy stream cleanup.
#[derive(Debug, Error)]
pub enum LegacyStreamCleanupError {
    #[error(transparent)]
    Database(#[from] db::SessionDbError),
    #[error(transparent)]
    DatabaseOperation(#[from] DatabaseError),
    #[error(transparent)]
    PersistedEvent(#[from] persisted::PersistedSessionEventError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    BlockingTask(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Lease(#[from] lease::SessionLeaseError),
    #[error("filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("backup verification failed between {source_path} and {destination}: {reason}")]
    BackupVerification {
        source_path: PathBuf,
        destination: PathBuf,
        reason: String,
    },
    #[error("event #{sequence} failed strict persisted decoding: {source}")]
    StrictDecode {
        sequence: u64,
        source: persisted::PersistedSessionEventError,
    },
    #[error("session {session_id} contains a mismatched event at row #{row_sequence}")]
    SequenceMismatch {
        session_id: SessionId,
        row_sequence: u64,
    },
}

#[derive(Debug)]
struct Replacement {
    before_bytes: usize,
    after_bytes: usize,
}

/// Discover session ids beneath a session-store root.
///
/// # Errors
///
/// Returns an error when the root cannot be read.
pub fn discover_session_ids(root: &Path) -> Result<Vec<SessionId>, LegacyStreamCleanupError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    let entries = fs::read_dir(root).map_err(|source| LegacyStreamCleanupError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| LegacyStreamCleanupError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<SessionId>().ok())
        {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// Inspect or clean one session's historically persisted transient stream payloads.
///
/// Apply mode refuses sessions with any live owner, creates a complete session-directory backup,
/// builds a compact replacement database from transformed events, validates it, and installs it.
///
/// # Errors
///
/// Returns an error on ownership, strict decoding, backup, database, validation, or compaction
/// failure. A backup is retained when failure occurs after backup creation.
pub async fn cleanup_session(
    root: &Path,
    session_id: SessionId,
    mode: CleanupMode,
    mut progress: impl FnMut(CleanupProgress),
) -> Result<SessionCleanupReport, LegacyStreamCleanupError> {
    let db_path = db::session_db_path(root, session_id);
    if !db_path.exists() {
        return Ok(SessionCleanupReport {
            session_id,
            outcome: CleanupOutcome::Skipped,
            events_scanned: 0,
            events_pruned: 0,
            payload_bytes_before: 0,
            payload_bytes_after: 0,
            database_bytes_before: 0,
            database_bytes_after: 0,
            backup_path: None,
            note: Some("session database does not exist".to_owned()),
        });
    }

    let _maintenance = match mode {
        CleanupMode::DryRun => None,
        CleanupMode::Apply => Some(lease::acquire_session_maintenance_guard(root, session_id)?),
    };
    progress(CleanupProgress::PhaseChanged {
        phase: CleanupPhase::Scanning,
    });
    let database_bytes_before = database_family_bytes(&db_path)?;
    let session_db = db::SessionDb::open_turso(session_id, &db_path).await?;
    let draft = session_db.session_composer_draft().await?;
    drop(session_db);
    let (total, replacements, transformed_events) =
        scan_replacements(&db_path, session_id, &mut progress).await?;

    let payload_bytes_before = replacements
        .iter()
        .map(|item| u64::try_from(item.before_bytes).unwrap_or(u64::MAX))
        .sum();
    let payload_bytes_after = replacements
        .iter()
        .map(|item| u64::try_from(item.after_bytes).unwrap_or(u64::MAX))
        .sum();
    if replacements.is_empty() {
        return Ok(SessionCleanupReport {
            session_id,
            outcome: CleanupOutcome::Unchanged,
            events_scanned: total,
            events_pruned: 0,
            payload_bytes_before,
            payload_bytes_after,
            database_bytes_before,
            database_bytes_after: database_bytes_before,
            backup_path: None,
            note: None,
        });
    }
    if mode == CleanupMode::DryRun {
        return Ok(SessionCleanupReport {
            session_id,
            outcome: CleanupOutcome::WouldClean,
            events_scanned: total,
            events_pruned: replacements.len(),
            payload_bytes_before,
            payload_bytes_after,
            database_bytes_before,
            database_bytes_after: database_bytes_before,
            backup_path: None,
            note: None,
        });
    }

    apply_replacements(
        root,
        session_id,
        &db_path,
        total,
        &replacements,
        &transformed_events,
        draft.as_deref(),
        &mut progress,
        payload_bytes_before,
        payload_bytes_after,
        database_bytes_before,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn apply_replacements(
    root: &Path,
    session_id: SessionId,
    db_path: &Path,
    total: usize,
    replacements: &[Replacement],
    transformed_events: &[SessionEvent],
    draft: Option<&str>,
    progress: &mut impl FnMut(CleanupProgress),
    payload_bytes_before: u64,
    payload_bytes_after: u64,
    database_bytes_before: u64,
) -> Result<SessionCleanupReport, LegacyStreamCleanupError> {
    progress(CleanupProgress::PhaseChanged {
        phase: CleanupPhase::CreatingBackup,
    });
    let backup_path = create_backup(root, session_id)?;
    progress(CleanupProgress::PhaseChanged {
        phase: CleanupPhase::ReplacingEvents,
    });
    if transformed_events.len() != total {
        return Err(LegacyStreamCleanupError::SequenceMismatch {
            session_id,
            row_sequence: u64::MAX,
        });
    }
    progress(CleanupProgress::PhaseChanged {
        phase: CleanupPhase::Validating,
    });
    progress(CleanupProgress::PhaseChanged {
        phase: CleanupPhase::Compacting,
    });
    rebuild_compact_database(root, session_id, transformed_events, draft).await?;
    Ok(SessionCleanupReport {
        session_id,
        outcome: CleanupOutcome::Cleaned,
        events_scanned: total,
        events_pruned: replacements.len(),
        payload_bytes_before,
        payload_bytes_after,
        database_bytes_before,
        database_bytes_after: database_family_bytes(db_path)?,
        backup_path: Some(backup_path),
        note: None,
    })
}

async fn scan_replacements(
    db_path: &Path,
    session_id: SessionId,
    progress: &mut impl FnMut(CleanupProgress),
) -> Result<(usize, Vec<Replacement>, Vec<SessionEvent>), LegacyStreamCleanupError> {
    let path = db_path.to_path_buf();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut task =
        tokio::task::spawn_blocking(move || scan_replacements_blocking(&path, session_id, &sender));
    loop {
        tokio::select! {
            result = &mut task => {
                while let Ok(event) = receiver.try_recv() {
                    progress(event);
                }
                return result?;
            }
            Some(event) = receiver.recv() => progress(event),
        }
    }
}

fn scan_replacements_blocking(
    db_path: &Path,
    session_id: SessionId,
    progress: &tokio::sync::mpsc::UnboundedSender<CleanupProgress>,
) -> Result<(usize, Vec<Replacement>, Vec<SessionEvent>), LegacyStreamCleanupError> {
    let connection = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let last_sequence = connection.query_row("SELECT MAX(event_seq) FROM events", [], |row| {
        row.get::<_, Option<i64>>(0)
    })?;
    let Some(last_sequence) = last_sequence else {
        return Ok((0, Vec::new(), Vec::new()));
    };
    let total = usize::try_from(last_sequence.saturating_add(1)).unwrap_or(usize::MAX);
    let _ = progress.send(CleanupProgress::EventsProcessed {
        processed: 0,
        total,
    });
    let mut statement =
        connection.prepare("SELECT event_seq, payload FROM events ORDER BY event_seq ASC")?;
    let mut rows = statement.query([])?;
    let mut replacements = Vec::new();
    let mut transformed_events = Vec::with_capacity(total);
    let mut processed = 0_usize;
    while let Some(row) = rows.next()? {
        let sequence = u64::try_from(row.get::<_, i64>(0)?).unwrap_or(u64::MAX);
        let expected = u64::try_from(processed).unwrap_or(u64::MAX);
        if sequence != expected {
            return Err(LegacyStreamCleanupError::SequenceMismatch {
                session_id,
                row_sequence: sequence,
            });
        }
        let payload = row.get::<_, String>(1)?;
        transformed_events.push(collect_replacement(
            session_id,
            sequence,
            &payload,
            &mut replacements,
        )?);
        processed += 1;
        let _ = progress.send(CleanupProgress::EventsProcessed { processed, total });
    }
    if processed != total {
        return Err(LegacyStreamCleanupError::SequenceMismatch {
            session_id,
            row_sequence: u64::try_from(processed).unwrap_or(u64::MAX),
        });
    }
    Ok((total, replacements, transformed_events))
}

#[derive(Deserialize)]
struct LightweightEventEnvelope {
    sequence: u64,
    timestamp_ms: u64,
    session_id: SessionId,
    #[serde(default)]
    provenance: Option<SessionEventProvenance>,
    #[serde(rename = "kind")]
    _kind: serde::de::IgnoredAny,
}

fn collect_replacement(
    session_id: SessionId,
    sequence: u64,
    payload: &str,
    replacements: &mut Vec<Replacement>,
) -> Result<SessionEvent, LegacyStreamCleanupError> {
    if let Some((tool_call_id, original_kind)) = raw_transient_kind(payload) {
        let envelope = serde_json::from_str::<LightweightEventEnvelope>(payload)?;
        if envelope.sequence != sequence || envelope.session_id != session_id {
            return Err(LegacyStreamCleanupError::SequenceMismatch {
                session_id,
                row_sequence: sequence,
            });
        }
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: envelope.timestamp_ms,
            session_id,
            provenance: envelope.provenance,
            kind: SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyTransientPruned {
                    tool_call_id,
                    original_kind,
                },
            },
        };
        let replacement = persisted::encode_session_event(&event)?;
        replacements.push(Replacement {
            before_bytes: payload.len(),
            after_bytes: replacement.len(),
        });
        Ok(event)
    } else {
        persisted::decode_session_event(payload)
            .map_err(|source| LegacyStreamCleanupError::StrictDecode { sequence, source })
    }
}

fn raw_transient_kind(payload: &str) -> Option<(String, LegacyTransientStreamKind)> {
    if !payload.contains("\"kind\":{\"tool_invocation_stream\":{\"event\":{") {
        return None;
    }
    let (variant, original_kind) = [
        ("\"output_delta\":", LegacyTransientStreamKind::OutputDelta),
        (
            "\"visual_update\":",
            LegacyTransientStreamKind::VisualUpdate,
        ),
        (
            "\"artifact_update\":",
            LegacyTransientStreamKind::ArtifactUpdate,
        ),
        (
            "\"presentation\":",
            LegacyTransientStreamKind::LegacyPresentation,
        ),
    ]
    .into_iter()
    .find(|(variant, _)| payload.contains(variant))?;
    let variant_tail = payload.split_once(variant)?.1;
    let tool_call_tail = variant_tail.split_once("\"tool_call_id\"")?.1;
    let string_value = tool_call_tail.split_once(':')?.1.trim_start();
    let mut deserializer = serde_json::Deserializer::from_str(string_value);
    let tool_call_id = String::deserialize(&mut deserializer).ok()?;
    Some((tool_call_id, original_kind))
}

async fn rebuild_compact_database(
    root: &Path,
    session_id: SessionId,
    events: &[bcode_session_models::SessionEvent],
    draft: Option<&str>,
) -> Result<(), LegacyStreamCleanupError> {
    let session_dir = root.join(session_id.to_string());
    let compact_root = session_dir.join(".legacy-stream-cleanup-compact");
    remove_dir_if_exists(&compact_root)?;
    let compact_db = db::SessionDb::open_turso_in_root(session_id, &compact_root).await?;
    for event in events {
        compact_db.append_event(event).await?;
    }
    if compact_db.all_events_strict().await?.len() != events.len() {
        return Err(LegacyStreamCleanupError::SequenceMismatch {
            session_id,
            row_sequence: u64::MAX,
        });
    }
    if let Some(draft) = draft {
        compact_db
            .set_session_composer_draft(draft, unix_time_millis())
            .await?;
    }
    let compact_path = db::session_db_path(&compact_root, session_id);
    drop(compact_db);

    let validation = db::SessionDb::open_turso(session_id, &compact_path).await?;
    if validation.all_events_strict().await?.len() != events.len() {
        return Err(LegacyStreamCleanupError::SequenceMismatch {
            session_id,
            row_sequence: u64::MAX,
        });
    }
    drop(validation);

    let target = db::session_db_path(root, session_id);
    for suffix in [None, Some("wal"), Some("shm"), Some("tshm")] {
        let source = db_family_member(&compact_path, suffix);
        let destination = db_family_member(&target, suffix);
        remove_file_if_exists(&destination)?;
        if source.exists() {
            fs::rename(&source, &destination).map_err(|source_error| {
                LegacyStreamCleanupError::Io {
                    path: destination,
                    source: source_error,
                }
            })?;
        }
    }
    remove_dir_if_exists(&compact_root)?;
    let installed = db::SessionDb::open_turso(session_id, &target).await?;
    if installed.all_events_strict().await?.len() != events.len() {
        return Err(LegacyStreamCleanupError::SequenceMismatch {
            session_id,
            row_sequence: u64::MAX,
        });
    }
    Ok(())
}

fn db_family_member(path: &Path, suffix: Option<&str>) -> PathBuf {
    suffix.map_or_else(
        || path.to_path_buf(),
        |suffix| PathBuf::from(format!("{}-{suffix}", path.display())),
    )
}

fn remove_file_if_exists(path: &Path) -> Result<(), LegacyStreamCleanupError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LegacyStreamCleanupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<(), LegacyStreamCleanupError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LegacyStreamCleanupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn create_backup(root: &Path, session_id: SessionId) -> Result<PathBuf, LegacyStreamCleanupError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let destination = root
        .parent()
        .unwrap_or(root)
        .join("legacy-stream-cleanup-backups")
        .join(format!("{timestamp}-{session_id}"));
    copy_dir_recursive(&root.join(session_id.to_string()), &destination)?;
    verify_directory_copy(&root.join(session_id.to_string()), &destination)?;
    Ok(destination)
}

fn verify_directory_copy(
    source: &Path,
    destination: &Path,
) -> Result<(), LegacyStreamCleanupError> {
    let mut source_entries = fs::read_dir(source)
        .map_err(|source_error| LegacyStreamCleanupError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source_error| LegacyStreamCleanupError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?;
    source_entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut destination_entries = fs::read_dir(destination)
        .map_err(|source_error| LegacyStreamCleanupError::Io {
            path: destination.to_path_buf(),
            source: source_error,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source_error| LegacyStreamCleanupError::Io {
            path: destination.to_path_buf(),
            source: source_error,
        })?;
    destination_entries.sort_by_key(std::fs::DirEntry::file_name);
    if source_entries.len() != destination_entries.len()
        || source_entries
            .iter()
            .zip(&destination_entries)
            .any(|(source, destination)| source.file_name() != destination.file_name())
    {
        return Err(LegacyStreamCleanupError::BackupVerification {
            source_path: source.to_path_buf(),
            destination: destination.to_path_buf(),
            reason: "directory entries differ".to_owned(),
        });
    }
    for (source_entry, destination_entry) in source_entries.iter().zip(&destination_entries) {
        let source_path = source_entry.path();
        let destination_path = destination_entry.path();
        let source_type =
            source_entry
                .file_type()
                .map_err(|source_error| LegacyStreamCleanupError::Io {
                    path: source_path.clone(),
                    source: source_error,
                })?;
        let destination_type =
            destination_entry
                .file_type()
                .map_err(|source_error| LegacyStreamCleanupError::Io {
                    path: destination_path.clone(),
                    source: source_error,
                })?;
        if source_type.is_dir() != destination_type.is_dir()
            || source_type.is_file() != destination_type.is_file()
        {
            return Err(LegacyStreamCleanupError::BackupVerification {
                source_path,
                destination: destination_path,
                reason: "entry types differ".to_owned(),
            });
        }
        if source_type.is_dir() {
            verify_directory_copy(&source_path, &destination_path)?;
        } else if source_type.is_file() {
            verify_file_copy(&source_path, &destination_path)?;
        } else {
            return Err(LegacyStreamCleanupError::BackupVerification {
                source_path,
                destination: destination_path,
                reason: "unsupported non-file entry".to_owned(),
            });
        }
    }
    Ok(())
}

fn verify_file_copy(source: &Path, destination: &Path) -> Result<(), LegacyStreamCleanupError> {
    use std::io::Read as _;

    let source_length = fs::metadata(source)
        .map_err(|source_error| LegacyStreamCleanupError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?
        .len();
    let destination_length = fs::metadata(destination)
        .map_err(|source_error| LegacyStreamCleanupError::Io {
            path: destination.to_path_buf(),
            source: source_error,
        })?
        .len();
    if source_length != destination_length {
        return Err(LegacyStreamCleanupError::BackupVerification {
            source_path: source.to_path_buf(),
            destination: destination.to_path_buf(),
            reason: "file lengths differ".to_owned(),
        });
    }
    let mut source_file =
        fs::File::open(source).map_err(|source_error| LegacyStreamCleanupError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?;
    let mut destination_file =
        fs::File::open(destination).map_err(|source_error| LegacyStreamCleanupError::Io {
            path: destination.to_path_buf(),
            source: source_error,
        })?;
    let mut source_buffer = vec![0_u8; 64 * 1024];
    let mut destination_buffer = vec![0_u8; 64 * 1024];
    loop {
        let source_read = source_file
            .read(&mut source_buffer)
            .map_err(|source_error| LegacyStreamCleanupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        let destination_read =
            destination_file
                .read(&mut destination_buffer)
                .map_err(|source_error| LegacyStreamCleanupError::Io {
                    path: destination.to_path_buf(),
                    source: source_error,
                })?;
        if source_read != destination_read
            || source_buffer[..source_read] != destination_buffer[..source_read]
        {
            return Err(LegacyStreamCleanupError::BackupVerification {
                source_path: source.to_path_buf(),
                destination: destination.to_path_buf(),
                reason: "file contents differ".to_owned(),
            });
        }
        if source_read == 0 {
            return Ok(());
        }
    }
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), LegacyStreamCleanupError> {
    fs::create_dir_all(destination).map_err(|source| LegacyStreamCleanupError::Io {
        path: destination.to_path_buf(),
        source,
    })?;
    let entries = fs::read_dir(source).map_err(|source_error| LegacyStreamCleanupError::Io {
        path: source.to_path_buf(),
        source: source_error,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source_error| LegacyStreamCleanupError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?;
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|source_error| {
                LegacyStreamCleanupError::Io {
                    path: entry.path(),
                    source: source_error,
                }
            })?;
        }
    }
    Ok(())
}

fn database_family_bytes(path: &Path) -> Result<u64, LegacyStreamCleanupError> {
    let mut total = 0_u64;
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
        PathBuf::from(format!("{}-tshm", path.display())),
    ] {
        match fs::metadata(&candidate) {
            Ok(metadata) => total = total.saturating_add(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LegacyStreamCleanupError::Io {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        SessionEvent, SessionEventKind, ToolInvocationStreamEvent, ToolOutputStream,
    };

    #[tokio::test]
    async fn cleanup_is_backed_up_sequence_preserving_and_idempotent() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("sessions");
        let session_id = SessionId::new();
        let database = db::SessionDb::open_turso_in_root(session_id, &root)
            .await
            .expect("open session");
        let semantic_event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::AssistantMessage {
                text: "semantic event must survive cleanup".to_owned(),
            },
        };
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-1".to_owned(),
                    stream: ToolOutputStream::Pty,
                    sequence: 1,
                    text: "large".repeat(10_000),
                    byte_len: 50_000,
                },
            },
        };
        database
            .append_event(&semantic_event)
            .await
            .expect("append semantic event");
        database
            .append_event(&event)
            .await
            .expect("append legacy event");
        drop(database);

        let dry_run = cleanup_session(&root, session_id, CleanupMode::DryRun, |_| {})
            .await
            .expect("dry run");
        assert_eq!(dry_run.outcome, CleanupOutcome::WouldClean);
        assert_eq!(dry_run.events_pruned, 1);
        assert!(dry_run.backup_path.is_none());

        let applied = cleanup_session(&root, session_id, CleanupMode::Apply, |_| {})
            .await
            .expect("apply cleanup");
        assert_eq!(applied.outcome, CleanupOutcome::Cleaned);
        let backup = applied.backup_path.expect("backup path");
        assert!(backup.join("session.db").exists());
        verify_directory_copy(&root.join(session_id.to_string()), &backup)
            .expect_err("installed compact database should differ from source backup");
        let backup_events = db::SessionDb::open_turso(session_id, &backup.join("session.db"))
            .await
            .expect("open verified backup")
            .all_events_strict()
            .await
            .expect("read verified backup");
        assert_eq!(backup_events, vec![semantic_event.clone(), event]);

        let reopened = db::SessionDb::open_turso_in_root(session_id, &root)
            .await
            .expect("reopen session");
        let events = reopened.all_events_strict().await.expect("strict history");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], semantic_event);
        assert_eq!(events[1].sequence, 1);
        assert!(matches!(
            &events[1].kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyTransientPruned {
                    tool_call_id,
                    original_kind: LegacyTransientStreamKind::OutputDelta,
                }
            } if tool_call_id == "call-1"
        ));
        drop(reopened);

        let second = cleanup_session(&root, session_id, CleanupMode::Apply, |_| {})
            .await
            .expect("second cleanup");
        assert_eq!(second.outcome, CleanupOutcome::Unchanged);
        assert!(second.backup_path.is_none());
    }
}
