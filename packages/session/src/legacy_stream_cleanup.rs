//! One-time cleanup for historically persisted live tool-stream payloads.
//!
//! This module is maintenance-only. Normal session open, attach, history, and append paths must
//! never call it.

use crate::{db, lease, persisted};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, LegacyTransientStreamKind, SessionEvent,
    SessionEventKind, SessionEventProvenance, SessionId, ToolInvocationStreamEvent,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use switchy::database::query::FilterableQuery as _;
use switchy::database::{DatabaseError, DatabaseValue};
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
    Lease(#[from] lease::SessionLeaseError),
    #[error("filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
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
    sequence: u64,
    payload: String,
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
/// updates eligible rows in one transaction, strictly validates every event, and compacts the DB.
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
    let (total, replacements) = scan_replacements(&session_db, session_id, &mut progress).await?;
    drop(session_db);

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
    let session_db = db::SessionDb::open_turso(session_id, db_path).await?;
    let transaction = session_db.database().begin_transaction().await?;
    for replacement in replacements {
        transaction
            .update("events")
            .value(
                "schema_version",
                i64::from(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            )
            .value("payload", replacement.payload.clone())
            .where_eq(
                "event_seq",
                DatabaseValue::Int64(i64::try_from(replacement.sequence).unwrap_or(i64::MAX)),
            )
            .execute(&*transaction)
            .await?;
    }
    transaction.commit().await?;
    progress(CleanupProgress::PhaseChanged {
        phase: CleanupPhase::Validating,
    });
    let validated = session_db.all_events_strict().await?;
    if validated.len() != total {
        return Err(LegacyStreamCleanupError::SequenceMismatch {
            session_id,
            row_sequence: u64::MAX,
        });
    }
    progress(CleanupProgress::PhaseChanged {
        phase: CleanupPhase::Compacting,
    });
    let draft = session_db.session_composer_draft().await?;
    drop(session_db);
    rebuild_compact_database(root, session_id, &validated, draft.as_deref()).await?;
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
    session_db: &db::SessionDb,
    session_id: SessionId,
    progress: &mut impl FnMut(CleanupProgress),
) -> Result<(usize, Vec<Replacement>), LegacyStreamCleanupError> {
    const PAGE_SIZE: usize = 256;
    let Some(last_sequence) = session_db.last_event_sequence().await? else {
        return Ok((0, Vec::new()));
    };
    let total = usize::try_from(last_sequence.saturating_add(1)).unwrap_or(usize::MAX);
    let mut replacements = Vec::new();
    let mut processed = 0_usize;
    while processed < total {
        let rows = session_db
            .database()
            .select("events")
            .columns(&["event_seq", "payload"])
            .where_gt(
                "event_seq",
                DatabaseValue::Int64(i64::try_from(processed).unwrap_or(i64::MAX) - 1),
            )
            .sort("event_seq", switchy::database::query::SortDirection::Asc)
            .limit(PAGE_SIZE)
            .execute(session_db.database())
            .await?;
        if rows.is_empty() {
            return Err(LegacyStreamCleanupError::SequenceMismatch {
                session_id,
                row_sequence: u64::try_from(processed).unwrap_or(u64::MAX),
            });
        }
        for row in rows {
            let expected_sequence = u64::try_from(processed).unwrap_or(u64::MAX);
            let sequence = row
                .get("event_seq")
                .and_then(|value| value.as_i64())
                .and_then(|value| u64::try_from(value).ok())
                .ok_or(LegacyStreamCleanupError::SequenceMismatch {
                    session_id,
                    row_sequence: expected_sequence,
                })?;
            if sequence != expected_sequence {
                return Err(LegacyStreamCleanupError::SequenceMismatch {
                    session_id,
                    row_sequence: sequence,
                });
            }
            let payload = row
                .get("payload")
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .ok_or(LegacyStreamCleanupError::SequenceMismatch {
                    session_id,
                    row_sequence: sequence,
                })?;
            collect_replacement(session_id, sequence, &payload, &mut replacements)?;
            processed += 1;
            progress(CleanupProgress::EventsProcessed { processed, total });
        }
    }
    Ok((total, replacements))
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
) -> Result<(), LegacyStreamCleanupError> {
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
            sequence,
            before_bytes: payload.len(),
            after_bytes: replacement.len(),
            payload: replacement,
        });
    } else {
        persisted::decode_session_event(payload)
            .map_err(|source| LegacyStreamCleanupError::StrictDecode { sequence, source })?;
    }
    Ok(())
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
    Ok(destination)
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
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
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

        let reopened = db::SessionDb::open_turso_in_root(session_id, &root)
            .await
            .expect("reopen session");
        let events = reopened.all_events_strict().await.expect("strict history");
        assert_eq!(events[0].sequence, 0);
        assert!(matches!(
            &events[0].kind,
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
