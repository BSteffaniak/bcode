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

use bcode_session_migration::SessionMigrationProgressReporter as MigrationExecutionProgress;

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
            payload_digest_sha256: format!("{:x}", digest.finalize()),
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
    let result =
        bcode_session_migration::create_retained_migration_backup(request, backup_progress)
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
}

/// Acquire exclusive maintenance ownership and migrate storage when it remains legacy.
///
/// # Errors
///
/// Returns an error when ownership, backup, migration, or strict validation fails.
pub async fn migrate_owned_session_storage(
    session_id: SessionId,
    root: &Path,
    writer_epoch: u64,
    progress: &MigrationExecutionProgress,
    metrics: &MetricsRegistry,
) -> Result<(), SessionError> {
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
    let compatibility = db::SessionDb::open_existing_turso_in_root(session_id, root)
        .await?
        .storage_compatibility()
        .await?;
    if matches!(
        compatibility,
        db::SessionStorageCompatibility::KnownLegacy { .. }
    ) {
        execute_owned_legacy_storage(
            OwnedLegacyMigration {
                session_id,
                root,
                writer_epoch,
                maintenance: &maintenance,
                write: &write,
                started: &started,
                progress: Some(progress.clone()),
            },
            metrics,
        )
        .await?;
    }
    drop(write);
    drop(maintenance);
    let current = db::SessionDb::open_existing_turso_in_root(session_id, root).await?;
    current.validate_write_readiness().await?;
    Ok(())
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
    } = migration;
    let operation_id = progress.as_ref().map_or_else(
        SessionOpenOperationId::new,
        MigrationExecutionProgress::operation_id,
    );
    let source = db::SessionDb::open_existing_turso_in_root(session_id, root).await?;
    let mut source_evidence = migration_source_evidence(&source).await?;
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
    migrated.validate_write_readiness().await?;
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
