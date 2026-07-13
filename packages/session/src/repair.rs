//! Explicit Bcode-native session database repair utilities.
//!
//! These routines are maintenance-only. Normal catalog/open/attach/history paths must not call
//! them because they may inspect and mutate WAL sidecar files after creating backups.

use crate::{db, lease};
use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_models::SessionId;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Options for explicit session/catalog repair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepairOptions {
    /// Report planned actions without mutating files or acquiring write leases.
    pub dry_run: bool,
}

/// Target repaired or inspected by a repair operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairTarget {
    /// A per-session database.
    Session { session_id: SessionId },
    /// The global session catalog database.
    Catalog,
}

/// Overall repair status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStatus {
    /// The database opened through Bcode's Turso stack without repair.
    Ok,
    /// The operation would make changes, but dry-run mode skipped them.
    WouldRepair,
    /// A safe repair was applied and the database opened through Bcode's Turso stack.
    Repaired,
    /// Another daemon owns the session.
    RefusedOwnedElsewhere,
    /// The issue is not a clearly safe stale-sidecar or truncated-tail repair.
    ManualRequired,
}

/// One repair action, either planned or applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairAction {
    /// Stable action kind.
    pub kind: String,
    /// Human-readable detail.
    pub detail: String,
}

/// Report emitted by repair/doctor operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairReport {
    /// Repair target.
    pub target: RepairTarget,
    /// Main database path.
    pub db_path: PathBuf,
    /// Final status.
    pub status: RepairStatus,
    /// Backup directory if one was created.
    pub backup_path: Option<PathBuf>,
    /// Initial Bcode/Turso validation error, if any.
    pub initial_error: Option<String>,
    /// Final Bcode/Turso validation error, if any.
    pub final_error: Option<String>,
    /// Actions that were planned or applied.
    pub actions: Vec<RepairAction>,
    /// Human-readable notes.
    pub notes: Vec<String>,
}

impl RepairReport {
    const fn new(target: RepairTarget, db_path: PathBuf) -> Self {
        Self {
            target,
            db_path,
            status: RepairStatus::ManualRequired,
            backup_path: None,
            initial_error: None,
            final_error: None,
            actions: Vec::new(),
            notes: Vec::new(),
        }
    }
}

/// Errors returned by explicit repair operations.
#[derive(Debug, Error)]
pub enum SessionRepairError {
    /// Filesystem operation failed.
    #[error("repair I/O error at {}: {source}", path.display())]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Original I/O error.
        source: std::io::Error,
    },
    /// Database validation failed unexpectedly.
    #[error(transparent)]
    Db(#[from] db::SessionDbError),
    /// Session lease acquisition failed.
    #[error(transparent)]
    Lease(#[from] lease::SessionLeaseError),
    /// Report serialization failed.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

/// Diagnose a per-session database without mutating files.
///
/// # Errors
///
/// Returns an error if filesystem inspection fails.
pub async fn doctor_session(
    root: &Path,
    session_id: SessionId,
) -> Result<RepairReport, SessionRepairError> {
    repair_session(root, session_id, RepairOptions { dry_run: true }).await
}

/// Explicitly repair a per-session database when the problem is a safe WAL sidecar/tail issue.
///
/// # Errors
///
/// Returns an error if filesystem operations, backup creation, or lease acquisition fail.
pub async fn repair_session(
    root: &Path,
    session_id: SessionId,
    options: RepairOptions,
) -> Result<RepairReport, SessionRepairError> {
    let db_path = db::session_db_path(root, session_id);
    let mut report = RepairReport::new(RepairTarget::Session { session_id }, db_path.clone());
    if !db_path.exists() {
        report.status = RepairStatus::ManualRequired;
        report
            .notes
            .push("session database does not exist".to_string());
        return Ok(report);
    }

    let _lease = if options.dry_run {
        None
    } else {
        match lease::acquire_session_lease(
            root,
            session_id,
            &lease::SessionLeaseOwnerContext::default(),
        ) {
            Ok(lease) => Some(lease),
            Err(lease::SessionLeaseError::OwnedByOtherDaemon { .. }) => {
                report.status = RepairStatus::RefusedOwnedElsewhere;
                report.final_error = Some("session is owned by another daemon".to_string());
                return Ok(report);
            }
            Err(error) => return Err(error.into()),
        }
    };

    let initial_error = match validate_session_db(root, session_id).await {
        Ok(()) => {
            report.status = RepairStatus::Ok;
            report
                .notes
                .push(model_context_projection_note(root, session_id).await?);
            return Ok(report);
        }
        Err(error) => error.to_string(),
    };
    report.initial_error = Some(initial_error.clone());
    repair_db_files(
        root,
        &db_path,
        &mut report,
        options,
        || validate_session_db(root, session_id),
        Some(session_id),
        &initial_error,
    )
    .await?;
    write_final_report(&report)?;
    Ok(report)
}

/// Diagnose the global catalog database without mutating files.
///
/// # Errors
///
/// Returns an error if filesystem inspection fails.
pub async fn doctor_catalog(root: &Path) -> Result<RepairReport, SessionRepairError> {
    repair_catalog(root, RepairOptions { dry_run: true }).await
}

/// Explicitly repair the global catalog database when the problem is a safe WAL sidecar/tail issue.
///
/// # Errors
///
/// Returns an error if filesystem operations or backup creation fail.
pub async fn repair_catalog(
    root: &Path,
    options: RepairOptions,
) -> Result<RepairReport, SessionRepairError> {
    let db_path = db::global_catalog_db_path(root);
    let mut report = RepairReport::new(RepairTarget::Catalog, db_path.clone());
    if !db_path.exists() {
        report.status = RepairStatus::ManualRequired;
        report
            .notes
            .push("catalog database does not exist".to_string());
        return Ok(report);
    }

    let _catalog_lock = if options.dry_run {
        None
    } else {
        Some(lease::acquire_catalog_lock(root)?)
    };

    let initial_error = match validate_catalog_db(root).await {
        Ok(()) => {
            report.status = RepairStatus::Ok;
            return Ok(report);
        }
        Err(error) => error.to_string(),
    };
    report.initial_error = Some(initial_error.clone());
    repair_db_files(
        root,
        &db_path,
        &mut report,
        options,
        || validate_catalog_db(root),
        None,
        &initial_error,
    )
    .await?;
    write_final_report(&report)?;
    Ok(report)
}

async fn repair_db_files<Fut>(
    root: &Path,
    db_path: &Path,
    report: &mut RepairReport,
    options: RepairOptions,
    validate: impl Fn() -> Fut,
    session_id: Option<SessionId>,
    initial_error: &str,
) -> Result<(), SessionRepairError>
where
    Fut: std::future::Future<Output = Result<(), db::SessionDbError>>,
{
    let short_read = initial_error
        .to_ascii_lowercase()
        .contains("short read on wal frame");
    if !short_read {
        report.status = RepairStatus::ManualRequired;
        report.final_error = Some(initial_error.to_string());
        report
            .notes
            .push("initial error is not a recognized WAL short-read repair case".to_string());
        return Ok(());
    }

    let backup_path = if options.dry_run {
        None
    } else {
        Some(create_backup(root, db_path, session_id, report)?)
    };
    report.backup_path = backup_path;

    add_action(
        report,
        "remove_wal_indexes",
        "remove stale -shm and -tshm files before retrying Turso open",
    );
    if !options.dry_run {
        remove_if_exists(&sidecar_path(db_path, "shm"))?;
        remove_if_exists(&sidecar_path(db_path, "tshm"))?;
        if validate().await.is_ok() {
            report.status = RepairStatus::Repaired;
            return Ok(());
        }
    }

    let Some(wal_repair) = plan_wal_tail_repair(db_path)? else {
        report.status = if options.dry_run {
            RepairStatus::WouldRepair
        } else {
            RepairStatus::ManualRequired
        };
        report.final_error = Some(initial_error.to_string());
        report.notes.push(
            "WAL does not contain a clearly incomplete final frame; manual repair required if stale sidecar removal is insufficient"
                .to_string(),
        );
        return Ok(());
    };

    add_action(
        report,
        "truncate_wal_tail",
        &format!(
            "truncate {} from {} to {} bytes to remove incomplete final frame",
            display_from_current_dir(&wal_repair.path),
            wal_repair.current_len,
            wal_repair.truncate_len
        ),
    );
    if options.dry_run {
        report.status = RepairStatus::WouldRepair;
        return Ok(());
    }

    truncate_file(&wal_repair.path, wal_repair.truncate_len)?;
    match validate().await {
        Ok(()) => {
            report.status = RepairStatus::Repaired;
            Ok(())
        }
        Err(error) => {
            report.status = RepairStatus::ManualRequired;
            report.final_error = Some(error.to_string());
            Ok(())
        }
    }
}

async fn model_context_projection_note(
    root: &Path,
    session_id: SessionId,
) -> Result<String, db::SessionDbError> {
    let session_db = db::SessionDb::open_turso_in_root(session_id, root).await?;
    Ok(match session_db.model_context_projection_status().await? {
        db::ModelContextProjectionStatus::Missing =>
            "model-context projection is missing; exact compatibility reads are active until explicit reindex".to_string(),
        db::ModelContextProjectionStatus::Fresh { checkpoint } =>
            format!("model-context projection is fresh through event #{checkpoint}"),
        db::ModelContextProjectionStatus::Stale { checkpoint, expected } => format!(
            "model-context projection is stale at event #{checkpoint}; canonical history ends at #{expected}; explicit reindex is required"
        ),
        db::ModelContextProjectionStatus::Incompatible { actual, expected } => format!(
            "model-context projection schema {actual} is incompatible with expected schema {expected}; explicit reindex is required"
        ),
    })
}

async fn validate_session_db(root: &Path, session_id: SessionId) -> Result<(), db::SessionDbError> {
    let session_db = db::SessionDb::open_turso_in_root(session_id, root).await?;
    let _ = session_db.last_event_sequence().await?;
    let _ = session_db.all_events_strict().await?;
    let _ = session_db.model_context_events().await?;
    Ok(())
}

async fn validate_catalog_db(root: &Path) -> Result<(), db::SessionDbError> {
    let catalog =
        db::GlobalSessionDb::open_turso_without_catalog_lock(&db::global_catalog_db_path(root))
            .await?;
    let _ = catalog.list_sessions().await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WalTailRepair {
    path: PathBuf,
    current_len: u64,
    truncate_len: u64,
}

fn plan_wal_tail_repair(db_path: &Path) -> Result<Option<WalTailRepair>, SessionRepairError> {
    let wal_path = sidecar_path(db_path, "wal");
    if !wal_path.exists() {
        return Ok(None);
    }
    let mut file = File::open(&wal_path).map_err(|source| SessionRepairError::Io {
        path: wal_path.clone(),
        source,
    })?;
    let current_len = file_len(&wal_path)?;
    if current_len <= 32 {
        return Ok(None);
    }
    let mut header = [0_u8; 32];
    file.read_exact(&mut header)
        .map_err(|source| SessionRepairError::Io {
            path: wal_path.clone(),
            source,
        })?;
    let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if !matches!(magic, 0x377f_0682 | 0x377f_0683) {
        return Ok(None);
    }
    let page_size = u64::from(u32::from_be_bytes([
        header[8], header[9], header[10], header[11],
    ]));
    let page_size = if page_size == 0 { 65_536 } else { page_size };
    let frame_size = 24_u64.saturating_add(page_size);
    if frame_size <= 24 {
        return Ok(None);
    }
    let frame_bytes = current_len.saturating_sub(32);
    let partial = frame_bytes % frame_size;
    if partial == 0 {
        return Ok(None);
    }
    let truncate_len = current_len.saturating_sub(partial);
    Ok(Some(WalTailRepair {
        path: wal_path,
        current_len,
        truncate_len,
    }))
}

fn write_final_report(report: &RepairReport) -> Result<(), SessionRepairError> {
    let Some(backup_path) = &report.backup_path else {
        return Ok(());
    };
    let report_path = backup_path.join("repair-report-after.json");
    fs::write(&report_path, serde_json::to_vec_pretty(report)?).map_err(|source| {
        SessionRepairError::Io {
            path: report_path,
            source,
        }
    })
}

fn create_backup(
    root: &Path,
    db_path: &Path,
    session_id: Option<SessionId>,
    report: &RepairReport,
) -> Result<PathBuf, SessionRepairError> {
    let backup_root = root.parent().unwrap_or(root).join("repairs").join(format!(
        "{}-{}",
        unix_time_millis(),
        backup_label(session_id)
    ));
    fs::create_dir_all(&backup_root).map_err(|source| SessionRepairError::Io {
        path: backup_root.clone(),
        source,
    })?;

    if let Some(session_id) = session_id {
        let session_dir = root.join(session_id.to_string());
        if session_dir.exists() {
            copy_dir_recursive(&session_dir, &backup_root.join("session"))?;
        }
    } else {
        copy_db_family(db_path, &backup_root)?;
    }

    let report_path = backup_root.join("repair-report-before.json");
    fs::write(&report_path, serde_json::to_vec_pretty(report)?).map_err(|source| {
        SessionRepairError::Io {
            path: report_path,
            source,
        }
    })?;
    Ok(backup_root)
}

fn copy_db_family(db_path: &Path, backup_root: &Path) -> Result<(), SessionRepairError> {
    for path in [
        db_path.to_path_buf(),
        sidecar_path(db_path, "wal"),
        sidecar_path(db_path, "shm"),
        sidecar_path(db_path, "tshm"),
    ] {
        if path.exists() {
            let Some(name) = path.file_name() else {
                continue;
            };
            copy_file(&path, &backup_root.join(name))?;
        }
    }
    Ok(())
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), SessionRepairError> {
    fs::create_dir_all(destination).map_err(|source_error| SessionRepairError::Io {
        path: destination.to_path_buf(),
        source: source_error,
    })?;
    for entry in fs::read_dir(source).map_err(|source_error| SessionRepairError::Io {
        path: source.to_path_buf(),
        source: source_error,
    })? {
        let entry = entry.map_err(|source_error| SessionRepairError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            copy_file(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), SessionRepairError> {
    fs::copy(source, destination).map_err(|source_error| SessionRepairError::Io {
        path: source.to_path_buf(),
        source: source_error,
    })?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), SessionRepairError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SessionRepairError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn truncate_file(path: &Path, len: u64) -> Result<(), SessionRepairError> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| SessionRepairError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.seek(SeekFrom::Start(len))
        .map_err(|source| SessionRepairError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.set_len(len).map_err(|source| SessionRepairError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.flush().map_err(|source| SessionRepairError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn file_len(path: &Path) -> Result<u64, SessionRepairError> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|source| SessionRepairError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", db_path.display()))
}

fn add_action(report: &mut RepairReport, kind: &str, detail: &str) {
    report.actions.push(RepairAction {
        kind: kind.to_string(),
        detail: detail.to_string(),
    });
}

fn backup_label(session_id: Option<SessionId>) -> String {
    session_id.map_or_else(|| "catalog".to_string(), |id| id.to_string())
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn destructive_repair_dry_run_is_non_mutating_and_execution_creates_backup() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("sessions").join("fixture.db");
        std::fs::create_dir_all(db_path.parent().expect("database parent"))
            .expect("database directory");
        std::fs::write(&db_path, b"database-before-repair").expect("database file");
        let shm_path = sidecar_path(&db_path, "shm");
        let tshm_path = sidecar_path(&db_path, "tshm");
        std::fs::write(&shm_path, b"stale-shm").expect("shm sidecar");
        std::fs::write(&tshm_path, b"stale-tshm").expect("tshm sidecar");
        let before = [
            std::fs::read(&db_path).expect("database bytes"),
            std::fs::read(&shm_path).expect("shm bytes"),
            std::fs::read(&tshm_path).expect("tshm bytes"),
        ];
        let initial_error = "short read on WAL frame";

        let mut dry_report = RepairReport::new(RepairTarget::Catalog, db_path.clone());
        repair_db_files(
            temp_dir.path(),
            &db_path,
            &mut dry_report,
            RepairOptions { dry_run: true },
            || async { Ok(()) },
            None,
            initial_error,
        )
        .await
        .expect("dry run");
        assert_eq!(dry_report.status, RepairStatus::WouldRepair);
        assert_eq!(dry_report.backup_path, None);
        assert!(!dry_report.actions.is_empty());
        assert_eq!(std::fs::read(&db_path).expect("database bytes"), before[0]);
        assert_eq!(std::fs::read(&shm_path).expect("shm bytes"), before[1]);
        assert_eq!(std::fs::read(&tshm_path).expect("tshm bytes"), before[2]);

        let mut repair_report = RepairReport::new(RepairTarget::Catalog, db_path.clone());
        repair_db_files(
            temp_dir.path(),
            &db_path,
            &mut repair_report,
            RepairOptions { dry_run: false },
            || async { Ok(()) },
            None,
            initial_error,
        )
        .await
        .expect("repair execution");
        assert_eq!(repair_report.status, RepairStatus::Repaired);
        let backup = repair_report.backup_path.expect("backup path");
        assert_eq!(
            std::fs::read(backup.join("fixture.db")).expect("backup database"),
            before[0]
        );
        assert_eq!(
            std::fs::read(backup.join("fixture.db-shm")).expect("backup shm"),
            before[1]
        );
        assert_eq!(
            std::fs::read(backup.join("fixture.db-tshm")).expect("backup tshm"),
            before[2]
        );
        assert!(!shm_path.exists());
        assert!(!tshm_path.exists());
    }

    #[tokio::test]
    async fn doctor_session_reports_stale_model_context_projection() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let session_db = db::SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        session_db
            .append_event(&session_event(
                session_id,
                0,
                bcode_session_models::SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "projected".to_string(),
                },
            ))
            .await
            .expect("append projected event");
        insert_raw_event(
            &session_db,
            session_id,
            1,
            "assistant_message",
            serde_json::json!({
                "schema_version": bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                "sequence": 1,
                "timestamp_ms": 1,
                "session_id": session_id,
                "provenance": null,
                "kind": { "assistant_message": { "text": "unprojected" } }
            }),
        )
        .await;

        let report = doctor_session(temp_dir.path(), session_id)
            .await
            .expect("doctor should report");
        assert_eq!(report.status, RepairStatus::ManualRequired);
        assert!(report.initial_error.as_deref().is_some_and(|error| {
            error.contains("model-context projection is stale")
                && error.contains("checkpoint #0")
                && error.contains("ends at #1")
        }));
        assert!(report.actions.is_empty());
    }

    #[tokio::test]
    async fn doctor_session_reports_fresh_model_context_projection() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let session_db = db::SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        session_db
            .append_event(&session_event(
                session_id,
                0,
                bcode_session_models::SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "hello".to_string(),
                },
            ))
            .await
            .expect("append projected event");

        let report = doctor_session(temp_dir.path(), session_id)
            .await
            .expect("doctor should report");
        assert_eq!(report.status, RepairStatus::Ok);
        assert!(
            report
                .notes
                .iter()
                .any(|note| { note == "model-context projection is fresh through event #0" })
        );
    }

    #[tokio::test]
    async fn doctor_session_reports_future_and_corrupt_persisted_events_without_mutation() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let session_db = db::SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        session_db
            .append_event(&session_event(
                session_id,
                0,
                bcode_session_models::SessionEventKind::SessionCreated {
                    name: Some("strict".to_string()),
                    working_directory: temp_dir.path().to_path_buf(),
                },
            ))
            .await
            .expect("append valid event");
        insert_raw_event(
            &session_db,
            session_id,
            1,
            "future_event_kind",
            serde_json::json!({
                "schema_version": bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                "sequence": 1,
                "session_id": session_id,
                "kind": { "future_event_kind": { "value": true } }
            }),
        )
        .await;
        insert_raw_event(
            &session_db,
            session_id,
            2,
            "tool_call_finished",
            serde_json::json!({
                "schema_version": bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                "sequence": 2,
                "session_id": session_id,
                "kind": { "tool_call_finished": { "result": "missing call id" } }
            }),
        )
        .await;

        let degraded_history = session_db
            .all_events()
            .await
            .expect("normal history should degrade");
        assert_eq!(degraded_history.len(), 1);

        let report = doctor_session(temp_dir.path(), session_id)
            .await
            .expect("doctor should report");

        assert_eq!(report.status, RepairStatus::ManualRequired);
        let initial_error = report
            .initial_error
            .as_deref()
            .expect("strict validation error should be reported");
        assert!(
            initial_error.contains("unsupported persisted session event kind future_event_kind"),
            "unexpected initial error: {initial_error}"
        );
        assert_eq!(report.backup_path, None);
        assert!(report.actions.is_empty());

        let raw_rows = session_db
            .database()
            .select("events")
            .columns(&["event_seq"])
            .execute(session_db.database())
            .await
            .expect("raw rows remain queryable");
        assert_eq!(raw_rows.len(), 3);
    }

    fn session_event(
        session_id: SessionId,
        sequence: u64,
        kind: bcode_session_models::SessionEventKind,
    ) -> bcode_session_models::SessionEvent {
        bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: unix_time_millis(),
            session_id,
            provenance: None,
            kind,
        }
    }

    async fn insert_raw_event(
        session_db: &db::SessionDb,
        session_id: SessionId,
        sequence: u64,
        event_type: &str,
        payload: serde_json::Value,
    ) {
        session_db
            .database()
            .insert("events")
            .value(
                "event_seq",
                switchy::database::DatabaseValue::Int64(
                    i64::try_from(sequence).unwrap_or(i64::MAX),
                ),
            )
            .value("event_type", event_type)
            .value(
                "schema_version",
                switchy::database::DatabaseValue::Int32(i32::from(
                    bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                )),
            )
            .value(
                "created_at_ms",
                switchy::database::DatabaseValue::Int64(
                    i64::try_from(sequence).unwrap_or(i64::MAX),
                ),
            )
            .value("payload", payload.to_string())
            .execute(session_db.database())
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "insert raw event {sequence} for session {session_id} should succeed: {error}"
                );
            });
    }

    #[test]
    fn plans_truncating_only_incomplete_final_wal_frame() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("session.db");
        fs::write(&db_path, []).expect("db placeholder");
        write_wal(&sidecar_path(&db_path, "wal"), 4_096, 2, 17);

        let repair = plan_wal_tail_repair(&db_path)
            .expect("plan should inspect")
            .expect("partial frame should be repairable");

        assert_eq!(repair.current_len, 32 + ((24 + 4_096) * 2) + 17);
        assert_eq!(repair.truncate_len, 32 + ((24 + 4_096) * 2));
    }

    #[test]
    fn complete_wal_frames_do_not_plan_truncation() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("session.db");
        fs::write(&db_path, []).expect("db placeholder");
        write_wal(&sidecar_path(&db_path, "wal"), 4_096, 2, 0);

        assert_eq!(
            plan_wal_tail_repair(&db_path).expect("plan should inspect"),
            None
        );
    }

    #[test]
    fn unknown_wal_magic_does_not_plan_truncation() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("session.db");
        fs::write(&db_path, []).expect("db placeholder");
        let wal_path = sidecar_path(&db_path, "wal");
        let mut file = File::create(&wal_path).expect("wal create");
        file.write_all(&[0_u8; 32]).expect("header");
        file.write_all(&[1_u8; 7]).expect("partial");

        assert_eq!(
            plan_wal_tail_repair(&db_path).expect("plan should inspect"),
            None
        );
    }

    fn write_wal(path: &Path, page_size: u32, complete_frames: usize, partial_bytes: usize) {
        let mut header = [0_u8; 32];
        header[0..4].copy_from_slice(&0x377f_0682_u32.to_be_bytes());
        header[8..12].copy_from_slice(&page_size.to_be_bytes());
        let mut file = File::create(path).expect("wal create");
        file.write_all(&header).expect("header");
        let frame_size = 24 + usize::try_from(page_size).expect("page size");
        for _ in 0..complete_frames {
            file.write_all(&vec![0_u8; frame_size]).expect("frame");
        }
        file.write_all(&vec![1_u8; partial_bytes]).expect("partial");
    }
}
