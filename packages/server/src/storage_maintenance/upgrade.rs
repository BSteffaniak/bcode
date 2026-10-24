//! Maintenance admission policy; execution remains migration-domain owned.

use super::{ServerState, operation_cancellation, skipped};
use crate::session_migration_execution::{
    MaintenanceStorageBlock, MaintenanceStorageClassification,
    classify_session_storage_for_maintenance, start_session_storage_maintenance,
};
use bcode_session_models::{SessionId, SessionOpenTerminalOutcome};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Refuse foreground work without loading an actor or changing access age.
pub(super) async fn foreground_pending(state: &ServerState, id: SessionId) -> bool {
    if let Ok(ownership) = state.sessions.session_ownership_snapshot(id).await
        && (!ownership.is_quiescent() || ownership.database_handle_retained)
    {
        return true;
    }
    if state
        .active_session_namespaces
        .lock()
        .await
        .get(&id)
        .is_some_and(|namespace| namespace.pending_attaches > 0)
        || state
            .attached_client_sessions
            .lock()
            .await
            .values()
            .any(|session| *session == id)
    {
        return true;
    }
    if let Some(runtime) = state.session_runtimes.lock().await.get(&id).cloned()
        && (runtime.queued_followups.load(Ordering::Acquire) > 0
            || runtime.queued_steering.load(Ordering::Acquire) > 0
            || runtime.phase.lock().await.has_active_work()
            || runtime.current_turn.lock().await.is_some())
    {
        return true;
    }
    !state.runtime_work.active_for_session(id).await.is_empty()
        || crate::session_has_active_plugin_invocations(state, id)
}

const fn stage_reason(stage: bcode_session_models::SessionMigrationStage) -> &'static str {
    use bcode_session_models::SessionMigrationStage as Stage;
    match stage {
        Stage::WaitingForOwnership => "upgrade_waiting_for_ownership",
        Stage::InspectingStorage => "classification",
        Stage::PlanningBackup => "backup_planning",
        Stage::CopyingBackup => "backup_copying",
        Stage::VerifyingBackup => "backup_verifying",
        Stage::PreparingSchema => "upgrade_preparing_schema",
        Stage::ReadingCanonicalHistory => "upgrade_reading_history",
        Stage::RebuildingProjections => "upgrade_rebuilding_projections",
        Stage::ValidatingProjections => "upgrade_validating_projections",
        Stage::Committing => "upgrade_committing",
        Stage::ValidatingWriteReadiness => "upgrade_validating_readiness",
        Stage::OpeningSession => "upgrade_opening",
        Stage::Complete => "upgrade_complete",
        Stage::Failed => "upgrade_deferred",
    }
}

const fn terminal_reason(outcome: &SessionOpenTerminalOutcome) -> &'static str {
    use bcode_session_models::SessionOpenFailureKind as Failure;
    match outcome {
        SessionOpenTerminalOutcome::Failed { kind, .. } => match kind {
            Failure::OwnedByOtherDaemon => "ownership_unavailable",
            Failure::WriterIncompatible => "future_writer",
            Failure::ProjectionStale => "projection_stale",
            Failure::RepairRequired => "history_or_corruption",
            Failure::BackupFailed => "backup_failed",
            Failure::MigrationFailed => "upgrade_deferred",
            Failure::NotFound => "storage_missing",
        },
        SessionOpenTerminalOutcome::WriterIncompatible { .. } => "future_writer",
        SessionOpenTerminalOutcome::RepairRequired { .. } => "history_or_corruption",
        SessionOpenTerminalOutcome::DegradedReadOnly { .. } => "unsupported_schema",
        SessionOpenTerminalOutcome::Ready => "upgrade_deferred",
    }
}

pub(super) async fn prepare(
    state: &ServerState,
    root: &Path,
    id: SessionId,
    now: u64,
    timeout_secs: u32,
) -> Result<bool, String> {
    let cancellation = operation_cancellation(state)?;
    if state.shutdown_requested.load(Ordering::SeqCst) {
        return Err("cancelled".into());
    }
    if foreground_pending(state, id).await {
        skipped(state, id, now, "foreground_pending");
        return Ok(false);
    }
    state
        .storage_worker
        .record_attempt(id, now, "running", "classification", None);
    let mut classification_shutdown = state.subscribe_shutdown();
    let classification = tokio::select! {
        result = classify_session_storage_for_maintenance(id, root) => result,
        () = tokio::time::sleep(Duration::from_secs(u64::from(timeout_secs))) => {
            return Err("classification_timeout".into());
        }
        _ = classification_shutdown.recv() => {
            return Err("cancelled".into());
        }
    };
    let writer_epoch = match classification {
        MaintenanceStorageClassification::Current => return Ok(true),
        MaintenanceStorageClassification::SupportedHistorical { writer_epoch } => writer_epoch,
        MaintenanceStorageClassification::Unsupported { reason } => {
            return Err(match reason {
                MaintenanceStorageBlock::Missing => "storage_missing",
                MaintenanceStorageBlock::FutureWriter => "future_writer",
                MaintenanceStorageBlock::UnsupportedEventSchema { .. } => "unsupported_schema",
                MaintenanceStorageBlock::HistoryOrCorruption => "history_or_corruption",
                MaintenanceStorageBlock::Unavailable => "classification_unavailable",
            }
            .into());
        }
    };
    // The known historical upgrade must precede current-only unknown-age initialization.
    // Neither classification nor migration is meaningful user access.
    let epoch = u32::try_from(writer_epoch).map_err(|_| "future_writer")?;
    cancellation
        .check_tracking_admission(root, id)
        .await
        .map_err(|_| "tracking_unavailable")?;
    if foreground_pending(state, id).await {
        skipped(state, id, now, "foreground_pending");
        return Ok(false);
    }
    let admission = start_session_storage_maintenance(
        id,
        root.to_path_buf(),
        epoch,
        &state.session_migrations.operations(),
        state.metrics.clone(),
        cancellation.clone(),
    )
    .await;
    let reason = if admission.admitted {
        "upgrade_admitted"
    } else {
        "upgrade_joined"
    };
    state
        .storage_worker
        .record_attempt(id, now, "running", reason, None);
    state
        .metrics
        .increment_counter(format!("storage.maintenance.reason.{reason}"));
    await_upgrade(state, id, now, timeout_secs, admission).await
}

async fn await_upgrade(
    state: &ServerState,
    id: SessionId,
    now: u64,
    timeout_secs: u32,
    admission: crate::session_migration_execution::MaintenanceMigrationAdmission,
) -> Result<bool, String> {
    let mut updates = admission.operation.subscribe();
    let mut shutdown = state.subscribe_shutdown();
    let deadline = tokio::time::sleep(Duration::from_secs(u64::from(timeout_secs)));
    tokio::pin!(deadline);
    let mut cancelling = state.shutdown_requested.load(Ordering::SeqCst);
    if cancelling && let Some(control) = &admission.control {
        control.cancel();
    }
    loop {
        let terminal_outcome = updates.borrow().outcome.clone();
        if let Some(outcome) = terminal_outcome {
            #[cfg(test)]
            state
                .storage_worker
                .record_terminal(id, admission.operation.snapshot());
            return match outcome {
                SessionOpenTerminalOutcome::Ready if !cancelling => {
                    state
                        .metrics
                        .increment_counter("storage.maintenance.upgraded");
                    state
                        .storage_worker
                        .record_attempt(id, now, "running", "compression", None);
                    Ok(true)
                }
                SessionOpenTerminalOutcome::Ready => Err("cancelled".into()),
                outcome => Err(terminal_reason(&outcome).into()),
            };
        }
        let progress_stage = updates.borrow().progress.stage;
        let reason = stage_reason(progress_stage);
        state
            .storage_worker
            .record_attempt(id, now, "running", reason, None);
        tokio::select! {
            biased;
            _ = shutdown.recv(), if !cancelling => {
                if let Some(control) = &admission.control { control.cancel(); }
                cancelling = true;
            }
            () = &mut deadline, if !cancelling => {
                if let Some(control) = &admission.control { control.cancel(); }
                cancelling = true;
            }
            changed = updates.changed() => {
                changed.map_err(|_| "upgrade_unavailable")?;
            }
        }
        // Never drop an operation waiter while our admitted backup/transaction may still run.
        // Joined foreground work is observed, but its cancellation authority is not borrowed.
    }
}
