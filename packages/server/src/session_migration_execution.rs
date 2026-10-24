//! Current migration-target execution adapter.
//!
//! Historical planning, normalization, backup policy, validation, and operation state remain owned
//! by `bcode_session_migration`. This adapter supplies the current database and lease capabilities
//! needed to execute one already-owned migration.

use bcode_metrics::{MetricsRegistry, MetricsTimer};
use bcode_session::{SessionError, db, lease};
use bcode_session_models::{
    SessionEvent, SessionId, SessionMigrationStage, SessionOpenOperationId,
};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bounded maintenance eligibility; never implies cold-storage eligibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceStorageClassification {
    /// Current storage; does not initialize or classify access age.
    Current,
    /// Released storage requiring owned canonical verification before conversion.
    SupportedHistorical {
        /// Observed durable source writer epoch.
        writer_epoch: u64,
    },
    /// Not eligible for automatic migration.
    Unsupported {
        /// Fail-closed classification.
        reason: MaintenanceStorageBlock,
    },
}

/// Fail-closed reason for withholding automatic upgrades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceStorageBlock {
    /// No canonical database exists.
    Missing,
    /// A newer writer contract must not be modified by this build.
    FutureWriter,
    /// An unreleased canonical schema was found.
    UnsupportedEventSchema {
        /// Observed unsupported schema.
        schema: u16,
    },
    /// Unverifiable ledger, envelope or storage evidence requires explicit diagnosis.
    HistoryOrCorruption,
    /// Inspection or graceful database closure failed.
    Unavailable,
}

/// Classify without canonical replay or current-only access-age initialization.
pub async fn classify_session_storage_for_maintenance(
    session_id: SessionId,
    root: &Path,
) -> MaintenanceStorageClassification {
    if !db::session_db_path(root, session_id)
        .try_exists()
        .unwrap_or(true)
    {
        return MaintenanceStorageClassification::Unsupported {
            reason: MaintenanceStorageBlock::Missing,
        };
    }
    let source = match db::SessionDb::open_existing_turso_in_root(session_id, root).await {
        Ok(source) => source,
        Err(error) => return blocked_storage(&error),
    };
    let result = classify_open_storage(&source).await;
    if source.database().close().await.is_err() {
        return MaintenanceStorageClassification::Unsupported {
            reason: MaintenanceStorageBlock::Unavailable,
        };
    }
    result.unwrap_or_else(|error| blocked_storage(&error))
}

fn blocked_storage(error: &db::SessionDbError) -> MaintenanceStorageClassification {
    let reason = match error {
        db::SessionDbError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            MaintenanceStorageBlock::Missing
        }
        db::SessionDbError::WriterIncompatible {
            actual: Some(actual),
            expected,
        } if actual > expected => MaintenanceStorageBlock::FutureWriter,
        db::SessionDbError::MigrationHistoryIncompatible { .. }
        | db::SessionDbError::InvalidData(_)
        | db::SessionDbError::InvalidRow { .. }
        | db::SessionDbError::PersistedEvent(_) => MaintenanceStorageBlock::HistoryOrCorruption,
        _ => MaintenanceStorageBlock::Unavailable,
    };
    MaintenanceStorageClassification::Unsupported { reason }
}

async fn classify_open_storage(
    source: &db::SessionDb,
) -> Result<MaintenanceStorageClassification, db::SessionDbError> {
    let compatibility = source.storage_compatibility().await?;
    if let db::SessionStorageCompatibility::KnownLegacy { writer_epoch } = compatibility {
        return Ok(MaintenanceStorageClassification::SupportedHistorical { writer_epoch });
    }
    let mut cursor = 0;
    let mut historical = None;
    loop {
        let page = source.event_schema_page(cursor).await?;
        for &(sequence, schema) in &page {
            if schema != bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION {
                if !bcode_session_migration::is_released_historical_event_schema(schema) {
                    return Ok(MaintenanceStorageClassification::Unsupported {
                        reason: MaintenanceStorageBlock::UnsupportedEventSchema { schema },
                    });
                }
                historical = Some(schema);
            }
            cursor = sequence
                .checked_add(1)
                .ok_or_else(|| db::SessionDbError::InvalidData("schema cursor overflow".into()))?;
        }
        if page.len() < 256 {
            break;
        }
        tokio::task::yield_now().await;
    }
    let schema = historical;
    Ok(match compatibility {
        db::SessionStorageCompatibility::Current { writer_epoch } if schema.is_some() => {
            MaintenanceStorageClassification::SupportedHistorical { writer_epoch }
        }
        db::SessionStorageCompatibility::Current { .. } => {
            MaintenanceStorageClassification::Current
        }
        db::SessionStorageCompatibility::KnownLegacy { writer_epoch } => {
            MaintenanceStorageClassification::SupportedHistorical { writer_epoch }
        }
    })
}

/// Cooperative cancellation; request then await the shared operation's terminal snapshot.
#[derive(Debug, Clone, Default)]
pub struct MaintenanceMigrationControl(
    bcode_session::artifact_storage::ArtifactMaintenanceCancellation,
);

impl MaintenanceMigrationControl {
    /// Request cancellation at the next safe boundary; never abort in-flight database/backup I/O.
    pub fn cancel(&self) {
        self.0.cancel();
    }
    fn is_cancelled(&self) -> bool {
        self.0.check().is_err()
    }
}

/// Admission acknowledgement for the same registry used by foreground opens.
pub struct MaintenanceMigrationAdmission {
    /// Shared tracked operation; terminal publication acknowledges settled I/O and ownership.
    pub operation: Arc<bcode_session_migration::SessionMigrationOperation>,
    /// True only when this invocation installed the runner.
    pub admitted: bool,
    /// Present only when this caller admitted the runner; joined work cannot be cancelled here.
    pub control: Option<MaintenanceMigrationControl>,
}

/// Start or join tracked maintenance without creating an actor or acquiring a runtime lease.
pub async fn start_session_storage_maintenance(
    session_id: SessionId,
    root: PathBuf,
    writer_epoch: u32,
    operations: &bcode_session_migration::SessionMigrationOperations,
    metrics: MetricsRegistry,
    cancellation: bcode_session::artifact_storage::ArtifactMaintenanceCancellation,
) -> MaintenanceMigrationAdmission {
    let control = MaintenanceMigrationControl(cancellation);
    let task_control = control.clone();
    let initial = super::migrating_session_open_snapshot(session_id, writer_epoch);
    let (operation, admitted) = operations
        .start_or_join_with_admission(initial, move |operation| async move {
            let progress = MigrationExecutionProgress::new(operation.clone());
            match migrate_session_storage_for_maintenance(
                session_id,
                &root,
                &progress,
                &metrics,
                &task_control,
            )
            .await
            {
                Ok(()) => bcode_session_models::SessionOpenTerminalOutcome::Ready,
                Err(error) => {
                    if matches!(&error, SessionError::DerivationIo(error) if matches!(error.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock)) {
                        operation.retry_after_completion();
                    }
                    super::session_migration_failure_outcome(&error)
                }
            }
        })
        .await;
    MaintenanceMigrationAdmission {
        operation,
        admitted,
        control: admitted.then_some(control),
    }
}

/// Prepare storage under exclusive ownership, then close every database before releasing guards.
///
/// Cancellation after mutation starts is settled by finishing the transaction, not task abortion.
/// # Errors
/// Fails closed on cancellation, ownership, classification, backup, conversion or validation errors.
pub async fn migrate_session_storage_for_maintenance(
    session_id: SessionId,
    root: &Path,
    progress: &MigrationExecutionProgress,
    metrics: &MetricsRegistry,
    control: &MaintenanceMigrationControl,
) -> Result<(), SessionError> {
    check_maintenance_cancelled(control)?;
    let started = metrics.timer();
    let maintenance = lease::acquire_session_maintenance_guard(root, session_id)?;
    let write = lease::acquire_maintenance_session_write_lock(&maintenance, root, session_id)?;
    let admission = control.0.admit_tracking(root, session_id).await?;
    check_maintenance_cancelled(control)?;
    let source = db::SessionDb::open_existing_turso_in_root(session_id, root).await?;
    let classification = classify_open_storage(&source).await;
    let closed = source.database().close().await;
    let classification = classification?;
    closed.map_err(db::SessionDbError::from)?;
    check_maintenance_cancelled(control)?;
    match classification {
        MaintenanceStorageClassification::SupportedHistorical { writer_epoch } => {
            execute_owned_legacy_storage(
                OwnedLegacyMigration {
                    session_id,
                    root,
                    writer_epoch,
                    maintenance: &maintenance,
                    write: &write,
                    started: &started,
                    progress: Some(progress.clone()),
                    control: Some(control),
                },
                metrics,
            )
            .await?;
        }
        MaintenanceStorageClassification::Current => {}
        MaintenanceStorageClassification::Unsupported { reason } => {
            return Err(SessionError::Db(db::SessionDbError::InvalidData(format!(
                "maintenance migration refused: {reason:?}"
            ))));
        }
    }
    let current = db::SessionDb::open_existing_turso_in_root(session_id, root).await?;
    let validated = async {
        current.validate_write_readiness().await?;
        let evidence = migration_source_evidence(&current).await?;
        if let Some(error) = evidence.classification_error {
            return Err(error);
        }
        if !evidence.converted_events.is_empty() || !evidence.retired_known_events.is_empty() {
            return Err(SessionError::Db(db::SessionDbError::InvalidData(
                "maintenance target still contains historical canonical events".to_owned(),
            )));
        }
        Ok::<(), SessionError>(())
    }
    .await;
    let closed = current.database().close().await;
    validated?;
    closed.map_err(db::SessionDbError::from)?;
    drop(current);
    drop(write);
    drop(maintenance);
    drop(admission);
    Ok(())
}

fn check_maintenance_cancelled(control: &MaintenanceMigrationControl) -> Result<(), SessionError> {
    if control.is_cancelled() {
        return Err(SessionError::DerivationIo(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "maintenance migration cancelled before mutation",
        )));
    }
    Ok(())
}

use bcode_session_migration::SessionMigrationProgressReporter as MigrationExecutionProgress;

#[cfg(test)]
mod tests;

async fn migration_source_evidence(
    db: &db::SessionDb,
) -> Result<bcode_session_migration::MigrationSourceEvidence<SessionError>, SessionError> {
    let mut digest = Sha256::new();
    let mut summary = bcode_session_migration::CanonicalNormalizationSummary::default();
    let mut classification_error = None;
    let mut cursor = 0_u64;
    let mut event_count = 0_u64;
    let mut classified_event_count = 0_u64;
    let mut event_tail = None;
    loop {
        let page = db
            .canonical_rows_page(cursor, 256)
            .await
            .map_err(SessionError::from)?;
        if page.is_empty() {
            break;
        }
        for row in page {
            if classification_error.is_none() && row.sequence != event_count {
                classification_error = Some(SessionError::Db(
                    db::SessionDbError::InvalidCanonicalSequence {
                        expected: event_count,
                        actual: row.sequence,
                    },
                ));
            }
            digest.update(
                u64::try_from(row.payload.len())
                    .unwrap_or(u64::MAX)
                    .to_le_bytes(),
            );
            digest.update(row.payload.as_bytes());
            if classification_error.is_none() {
                match bcode_session_migration::normalize_canonical_event(
                    &row.payload,
                    bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    // Historical decoding belongs to the migration domain. The current-only
                    // persistence decoder rejects the envelope before released-family conversion
                    // can preserve representation-compatible historical event kinds.
                    |payload| {
                        serde_json::from_str::<SessionEvent>(payload)
                            .map_err(|error| error.to_string())
                    },
                ) {
                    Ok(normalized)
                        if normalized.event.sequence == row.sequence
                            && normalized.event.session_id == db.session_id() =>
                    {
                        summary.record(&normalized);
                        classified_event_count = classified_event_count.saturating_add(1);
                    }
                    Ok(_) => {
                        classification_error = Some(SessionError::Db(
                            db::SessionDbError::InvalidCanonicalSequence {
                                expected: row.sequence,
                                actual: normalized_sequence(&row.payload).unwrap_or(u64::MAX),
                            },
                        ));
                    }
                    Err(error) => {
                        classification_error = Some(SessionError::Db(
                            db::SessionDbError::MigrationHistoryIncompatible {
                                reason: error.to_string(),
                            },
                        ));
                    }
                }
            }
            event_count = event_count.saturating_add(1);
            event_tail = Some(row.sequence);
            let Some(next) = row.sequence.checked_add(1) else {
                break;
            };
            cursor = next;
        }
        if event_tail == Some(u64::MAX) {
            break;
        }
    }
    let (converted_events, retired_known_events) = summary.into_counts();
    Ok(bcode_session_migration::MigrationSourceEvidence {
        canonical: bcode_session_migration::MigrationBackupCanonicalEvidence {
            classified_event_count,
            event_count,
            event_tail,
            payload_digest_sha256: hex::encode(digest.finalize()),
        },
        converted_events,
        retired_known_events,
        classification_error,
    })
}

fn normalized_sequence(payload: &str) -> Option<u64> {
    serde_json::from_str::<SessionEvent>(payload)
        .ok()
        .map(|event| event.sequence)
}

#[cfg(target_os = "linux")]
fn backup_space_preflight() -> bcode_session_migration::BackupSpacePreflight {
    Arc::new(|bytes, copy_required, path| {
        use std::os::unix::ffi::OsStrExt as _;
        let path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: the path is NUL-terminated and stats points to writable statvfs storage.
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: successful statvfs initialized the complete structure.
        let stats = unsafe { stats.assume_init() };
        let available = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
        // Retain room for the target transaction and journal as well as the verified copy.
        let required = u128::from(bytes)
            .saturating_mul(if copy_required { 3 } else { 2 })
            .saturating_add(16 * 1024 * 1024);
        if available < required {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "insufficient space for verified migration backup and target transaction",
            ));
        }
        Ok(())
    })
}

async fn create_verified_migration_backup(
    root: &Path,
    session_id: SessionId,
    writer_epoch: u64,
    operation_id: SessionOpenOperationId,
    source_evidence: bcode_session_migration::MigrationSourceEvidence<SessionError>,
    metrics: &MetricsRegistry,
    progress: Option<&MigrationExecutionProgress>,
) -> Result<PathBuf, SessionError> {
    let backup_progress = progress.cloned().map(|progress| {
        Arc::new(
            move |update: bcode_session_models::SessionMigrationProgress| {
                progress.publish(update);
            },
        ) as bcode_session_migration::BackupProgressCallback
    });
    let request = bcode_session_migration::build_migration_backup_request(
        bcode_session_migration::MigrationBackupRequestPlan {
            sessions_root: root.to_path_buf(),
            session_id,
            operation_id: operation_id.to_string(),
            source_writer_epoch: writer_epoch,
            canonical_source: source_evidence.canonical,
            converted_events: source_evidence.converted_events,
            retired_known_events: source_evidence.retired_known_events,
        },
    )
    .map_err(|error| SessionError::MigrationBackup {
        session_id,
        reason: error.to_string(),
    })?;
    #[cfg(target_os = "linux")]
    let preflight = Some(backup_space_preflight());
    #[cfg(not(target_os = "linux"))]
    let preflight = None;
    let result = bcode_session_migration::create_retained_migration_backup_with_preflight(
        request,
        backup_progress,
        preflight,
    )
    .await
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
    Ok(result.path)
}

fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Current capabilities and operation context for one exclusively owned migration.
struct OwnedLegacyMigration<'a> {
    pub(crate) session_id: SessionId,
    pub(crate) root: &'a Path,
    pub(crate) writer_epoch: u64,
    pub(crate) maintenance: &'a lease::SessionMaintenanceGuard,
    pub(crate) write: &'a lease::SessionWriteGuard,
    pub(crate) started: &'a MetricsTimer,
    pub(crate) progress: Option<MigrationExecutionProgress>,
    pub(crate) control: Option<&'a MaintenanceMigrationControl>,
}

/// Acquire exclusive maintenance ownership and migrate storage when it remains legacy.
///
/// # Errors
///
/// Returns an error when ownership, backup, migration, or strict validation fails.
pub async fn migrate_owned_session_storage(
    session_id: SessionId,
    root: &Path,
    _writer_epoch: u64,
    progress: &MigrationExecutionProgress,
    metrics: &MetricsRegistry,
    lease_owner: &lease::SessionLeaseOwnerContext,
) -> Result<lease::SessionLeaseGuard, SessionError> {
    let started = metrics.timer();
    metrics.increment_counter("session.manager.storage_migration.attempted_total");
    progress.stage(
        SessionMigrationStage::WaitingForOwnership,
        "Waiting for exclusive session ownership",
    );
    let ownership_timer = metrics.timer();
    let maintenance = lease::acquire_session_maintenance_guard(root, session_id)?;
    metrics.record_histogram(
        "session.migration.ownership_duration_ms",
        ownership_timer.elapsed_ms(),
    );
    let write = lease::acquire_maintenance_session_write_lock(&maintenance, root, session_id)?;
    let source = db::SessionDb::open_existing_turso_in_root(session_id, root).await?;
    let classification = classify_open_storage(&source).await;
    let closed = source.database().close().await;
    let classification = classification?;
    closed.map_err(db::SessionDbError::from)?;
    let writer_epoch = match classification {
        MaintenanceStorageClassification::SupportedHistorical { writer_epoch } => {
            Some(writer_epoch)
        }
        MaintenanceStorageClassification::Current => None,
        MaintenanceStorageClassification::Unsupported { reason } => {
            return Err(SessionError::Db(db::SessionDbError::InvalidData(format!(
                "foreground migration refused: {reason:?}"
            ))));
        }
    };
    if let Some(writer_epoch) = writer_epoch {
        execute_owned_legacy_storage(
            OwnedLegacyMigration {
                session_id,
                root,
                writer_epoch,
                maintenance: &maintenance,
                write: &write,
                started: &started,
                progress: Some(progress.clone()),
                control: None,
            },
            metrics,
        )
        .await?;
    }
    drop(write);
    let lease =
        lease::transition_session_maintenance_to_lease(maintenance, root, session_id, lease_owner)?;
    let current = db::SessionDb::open_existing_turso_in_root(session_id, root).await?;
    let validated = current.validate_write_readiness().await;
    let closed = current.database().close().await;
    validated?;
    closed.map_err(db::SessionDbError::from)?;
    Ok(lease)
}

/// Execute one exclusively owned historical migration through the current target API.
///
/// # Errors
///
/// Returns an error if source evidence cannot be read, a verified backup cannot be retained,
/// canonical normalization or target projection fails, or strict write readiness is not reached.
#[allow(clippy::too_many_lines)]
async fn execute_owned_legacy_storage(
    migration: OwnedLegacyMigration<'_>,
    metrics: &MetricsRegistry,
) -> Result<(), SessionError> {
    let OwnedLegacyMigration {
        session_id,
        root,
        writer_epoch,
        maintenance,
        write,
        started,
        progress,
        control,
    } = migration;
    let operation_id = progress.as_ref().map_or_else(
        SessionOpenOperationId::new,
        MigrationExecutionProgress::operation_id,
    );
    let source = db::SessionDb::open_existing_turso_in_root(session_id, root).await?;
    let evidence_result = migration_source_evidence(&source).await;
    let closed = source.database().close().await;
    let mut source_evidence = evidence_result?;
    closed.map_err(db::SessionDbError::from)?;
    drop(source);
    let classification_error = source_evidence.classification_error.take();
    let backup_path = create_verified_migration_backup(
        root,
        session_id,
        writer_epoch,
        operation_id,
        source_evidence,
        metrics,
        progress.as_ref(),
    )
    .await?;
    if let Some(progress) = &progress {
        progress.backup_verified(backup_path.clone());
        progress.stage(
            SessionMigrationStage::PreparingSchema,
            "Preparing session storage schema",
        );
    }
    if let Some(control) = control {
        check_maintenance_cancelled(control)?;
    }
    if let Some(error) = classification_error {
        if let Some(progress) = &progress {
            progress.stage(
                SessionMigrationStage::ReadingCanonicalHistory,
                "Canonical source history requires repair",
            );
        }
        return Err(error);
    }
    tracing::info!(
        target: "bcode_session::migration",
        %session_id,
        backup_path = %backup_path.display(),
        "verified pre-migration session backup"
    );
    let db_progress = progress.as_ref().map(|progress| {
        let progress = progress.clone();
        Arc::new(move |update| progress.publish(update)) as db::SessionMigrationProgressCallback
    });
    let migrated = db::SessionDb::migrate_turso_in_root_observed_with_operation(
        session_id,
        root,
        maintenance,
        write,
        metrics.clone(),
        db_progress,
        Some(operation_id),
        bcode_session_migration_target::MigrationPolicyCallbacks {
            normalize: Arc::new(|row| {
                bcode_session_migration::normalize_canonical_row(row)
                    .map_err(|error| error.to_string())
            }),
            build_receipt: Arc::new(bcode_session_migration::build_target_receipt),
        },
    )
    .await
    .inspect_err(|error| {
        metrics.increment_counter("session.manager.storage_migration.failed_total");
        tracing::warn!(
            target: "bcode_session::migration",
            %session_id,
            %error,
            "automatic legacy session migration failed"
        );
    })?;
    let readiness_timer = metrics.timer();
    if let Some(progress) = &progress {
        progress.stage(
            SessionMigrationStage::ValidatingWriteReadiness,
            "Validating session write readiness",
        );
    }
    let validated = migrated.validate_write_readiness().await;
    let closed = migrated.database().close().await;
    validated?;
    closed.map_err(db::SessionDbError::from)?;
    metrics.record_histogram(
        "session.migration.write_readiness_duration_ms",
        readiness_timer.elapsed_ms(),
    );
    drop(migrated);
    metrics.increment_counter("session.manager.storage_migration.completed_total");
    metrics.record_histogram(
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
