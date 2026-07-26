//! Current migration-target execution adapter.
//!
//! Historical planning, normalization, backup policy, validation, and operation state remain owned
//! by `bcode_session_migration`. This adapter supplies the current database and lease capabilities
//! needed to execute one already-owned migration.

use crate::{SessionError, db, lease};
use bcode_metrics::{MetricsRegistry, MetricsTimer};
use bcode_session_models::{SessionId, SessionMigrationStage, SessionOpenOperationId};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use bcode_session_migration::SessionMigrationProgressReporter as MigrationExecutionProgress;

async fn create_verified_migration_backup(
    root: &Path,
    session_id: SessionId,
    writer_epoch: u64,
    operation_id: SessionOpenOperationId,
    source_evidence: db::MigrationSourceEvidence,
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
pub struct OwnedLegacyMigration<'a> {
    pub(crate) session_id: SessionId,
    pub(crate) root: &'a Path,
    pub(crate) writer_epoch: u64,
    pub(crate) maintenance: &'a lease::SessionMaintenanceGuard,
    pub(crate) write: &'a lease::SessionWriteGuard,
    pub(crate) started: &'a MetricsTimer,
    pub(crate) progress: Option<MigrationExecutionProgress>,
}

/// Execute one exclusively owned historical migration through the current target API.
///
/// # Errors
///
/// Returns an error if source evidence cannot be read, a verified backup cannot be retained,
/// canonical normalization or target projection fails, or strict write readiness is not reached.
#[allow(clippy::too_many_lines)]
pub async fn execute_owned_legacy_storage(
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
    let mut source_evidence = source.migration_source_evidence().await?;
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
        return Err(error.into());
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
