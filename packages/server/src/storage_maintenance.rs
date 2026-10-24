//! Bounded daemon-owned automatic artifact maintenance.
//!
//! Scheduling is optional; failures are isolated from canonical session reads and daemon startup.
//! Each tick advances a retained directory iterator by at most sixteen entries and processes one
//! bounded reference page. Current storage never replays history or rebuilds search indexes;
//! supported historical storage delegates exclusive, backed-up conversion to the migration domain.

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod admission_tests;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod fail_closed_tests;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(crate) mod historical_tests;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod initialization_tests;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests;

mod scheduler;
mod upgrade;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod worker_historical_tests;

use super::ServerState;
use bcode_session::artifact_compression::ArtifactCompression;
use bcode_session::artifact_storage::{
    ArtifactMaintenanceCancellation, compress_finalized_artifact_cancellable,
};
use bcode_session::storage_access::{StorageAccessObservation, observe_session_access};
use bcode_session_models::SessionId;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArtifactSweepCursor {
    after: (String, String),
    through_sequence: u64,
}

impl ArtifactSweepCursor {
    fn continuation(
        after: Option<(String, String)>,
        through_sequence: u64,
        has_more: bool,
    ) -> Option<Self> {
        if has_more {
            after.map(|after| Self {
                after,
                through_sequence,
            })
        } else {
            None
        }
    }
}

#[derive(Clone)]
enum MaintenanceCursor {
    Artifacts(Option<ArtifactSweepCursor>),
    History { start: u64, through: Option<u64> },
}

/// Run bounded maintenance until daemon shutdown.
/// Requires healthy current-version registration under the coordinated clean-break rollout.
pub async fn run(state: Arc<ServerState>) {
    run_with_clock(state, super::current_time_ms).await;
}

async fn run_with_clock(state: Arc<ServerState>, now_ms: fn() -> u64) {
    if !tracking_readiness(&state) {
        return;
    }
    let Some(root) = state.sessions.session_store_root() else {
        return;
    };
    let mut shutdown = state.subscribe_shutdown();
    let policy = &state.startup_config.session_storage;
    let cadence = policy.maintenance_interval_secs;
    if cadence == 0 || policy.artifact_timeout_secs == 0 {
        tracing::warn!("automatic artifact maintenance disabled: invalid timing configuration");
        return;
    }
    let mut interval = tokio::time::interval(Duration::from_secs(u64::from(cadence)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut directories = None;
    let mut scheduler = scheduler::Scheduler::default();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => return,
            _ = interval.tick() => {},
        }
        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        if !state.startup_config.session_storage.enabled {
            continue;
        }
        if scheduler.needs_discovery() {
            let result = discover_sessions(root.clone(), directories.take()).await;
            if let Ok(Ok((next, ids))) = result {
                directories = next;
                scheduler.discover(ids, now_ms());
            } else {
                state
                    .metrics
                    .increment_counter("storage.maintenance.reason.discovery_unavailable");
                tracing::warn!("automatic artifact maintenance discovery unavailable");
                // Discovery failure must not starve already-admitted page continuations.
            }
        }
        if let Some((id, cursor)) = scheduler.next() {
            let now = now_ms();
            state
                .storage_worker
                .record_attempt(id, now, "running", "compression", None);
            let outcome = match cursor {
                MaintenanceCursor::Artifacts(after) => {
                    maintain_session_at(&state, &root, id, after, now_ms())
                        .await
                        .map(|next| {
                            if state
                                .storage_worker
                                .latest_outcome(id)
                                .is_some_and(|(outcome, _)| outcome == "skipped")
                            {
                                return None;
                            }
                            Some(next.map_or(
                                MaintenanceCursor::History {
                                    start: 0,
                                    through: None,
                                },
                                |key| MaintenanceCursor::Artifacts(Some(key)),
                            ))
                        })
                }
                MaintenanceCursor::History { start, through } => {
                    maintain_history(&state, &root, id, start, through, now_ms()).await
                }
            };
            let (label, reason) = match &outcome {
                Ok(Some(_)) => ("continued", "page_complete"),
                Ok(None) => state
                    .storage_worker
                    .latest_outcome(id)
                    .filter(|(outcome, _)| *outcome == "skipped")
                    .unwrap_or(("completed", "sweep_complete")),
                Err(error) => ("deferred", normalized_reason(error)),
            };
            let next_retry = scheduler.finish(id, outcome, now_ms());
            state
                .storage_worker
                .record_attempt(id, now, label, reason, next_retry);
            state
                .metrics
                .increment_counter(format!("storage.maintenance.outcome.{label}"));
            state
                .metrics
                .increment_counter(format!("storage.maintenance.reason.{reason}"));
        }
    }
}

type DiscoveredSessions = (Option<std::fs::ReadDir>, Vec<SessionId>);

async fn discover_sessions(
    root: std::path::PathBuf,
    mut current: Option<std::fs::ReadDir>,
) -> Result<std::io::Result<DiscoveredSessions>, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        if current.is_none() {
            current = Some(std::fs::read_dir(root)?);
        }
        let mut ids = Vec::new();
        for _ in 0..16 {
            let Some(entry) = current.as_mut().and_then(Iterator::next) else {
                current = None;
                break;
            };
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
                && let Ok(id) = name.parse::<SessionId>()
            {
                ids.push(id);
            }
        }
        Ok((current, ids))
    })
    .await
}

fn normalized_reason(error: &str) -> &'static str {
    match error {
        "storage tracking failed"
        | "registration lock unavailable"
        | "storage daemon registration unavailable"
        | "unhealthy storage registration"
        | "tracking_unavailable" => "tracking_unavailable",
        "access task failed"
        | "access unavailable"
        | "history access task"
        | "history access unavailable" => "access_unavailable",
        "access initialization deferred" => "access_initialization_deferred",
        "references unavailable" => "references_unavailable",
        "projection_stale" => "projection_stale",
        "backup_failed" => "backup_failed",
        "storage_missing" => "storage_missing",
        "future_writer" => "future_writer",
        "unsupported_schema" => "unsupported_schema",
        "history_or_corruption" => "history_or_corruption",
        "classification_unavailable" => "classification_unavailable",
        "upgrade_deferred" => "upgrade_deferred",
        "upgrade_unavailable" => "upgrade_unavailable",
        "cancelled" => "cancelled",
        "ownership_unavailable" => "ownership_unavailable",
        "artifact_compression_deferred" => "artifact_compression_deferred",
        "history compression deferred" => "history_compression_deferred",
        _ => "storage_unavailable",
    }
}

fn skipped(state: &ServerState, id: SessionId, now: u64, reason: &'static str) {
    state
        .storage_worker
        .record_attempt(id, now, "skipped", reason, None);
}

fn tracking_readiness(state: &ServerState) -> bool {
    let ready = !state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::SeqCst)
        && operation_cancellation(state).is_ok();
    state
        .metrics
        .set_gauge("storage.maintenance.compatibility_ready", i64::from(ready));
    // The clean-break rollout excludes legacy participants operationally. Current-version
    // registration is still mandatory, and each operation rechecks tracking health and obtains
    // durable maintenance admission. A failed startup registration never gains offline authority.
    ready
}

pub(super) fn operation_cancellation(
    state: &ServerState,
) -> Result<ArtifactMaintenanceCancellation, String> {
    if state
        .storage_tracking_failed
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err("storage tracking failed".into());
    }
    let registration = state
        .storage_daemon_registration
        .lock()
        .map_err(|_| "registration lock unavailable")?;
    registration
        .as_ref()
        .ok_or_else(|| "storage daemon registration unavailable".to_owned())?
        .acknowledgement()
        .map(|ack| ArtifactMaintenanceCancellation::default().with_acknowledgement(ack))
        .map_err(|_| "unhealthy storage registration".into())
}

async fn maintain_history(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
    start: u64,
    through: Option<u64>,
    now: u64,
) -> Result<Option<MaintenanceCursor>, String> {
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::SeqCst)
        || !state
            .session_catalog
            .ambiguous_location_ids(id)
            .await
            .is_empty()
    {
        return Ok(None);
    }
    let policy = state.session_config(id).await.session_storage;
    if !policy.enabled {
        skipped(state, id, now, "policy_disabled");
        return Ok(None);
    }
    if upgrade::foreground_pending(state, id).await {
        skipped(state, id, now, "foreground_pending");
        return Ok(None);
    }
    let access_root = root.to_path_buf();
    let observation = tokio::task::spawn_blocking(move || observe_session_access(&access_root, id))
        .await
        .map_err(|_| "history access task")?
        .map_err(|_| "history access unavailable")?;
    let StorageAccessObservation::Recorded(access) = observation else {
        return Ok(None);
    };
    let Some(elapsed) = now.checked_sub(access.observed_at_ms) else {
        return Ok(None);
    };
    let light = u64::from(policy.light_after_days) * 86_400_000;
    let deep = u64::from(policy.deep_after_days) * 86_400_000;
    let (level, age) = if elapsed >= deep {
        (12, deep)
    } else if elapsed >= light {
        (1, light)
    } else {
        return Ok(None);
    };
    let cancellation = operation_cancellation(state)?;
    let mut shutdown = state.subscribe_shutdown();
    let work = bcode_session::history_compression::compress_history_page_through(
        root,
        id,
        start,
        level,
        Some((now, age)),
        cancellation.clone(),
        through,
    );
    tokio::pin!(work);
    let page = tokio::select! {
        biased;
        _ = shutdown.recv() => { cancellation.cancel(); work.await }
        () = tokio::time::sleep(Duration::from_secs(u64::from(policy.artifact_timeout_secs))) => { cancellation.cancel(); work.await }
        result = &mut work => result,
    }.map_err(|_| "history compression deferred")?;
    state.metrics.add_counter(
        "storage.maintenance.history_payload_bytes_saved",
        page.saved_bytes,
    );
    if let Some(next) = page.next_sequence {
        return Ok(Some(MaintenanceCursor::History {
            start: next,
            through: page.through_sequence,
        }));
    }
    Ok(None)
}

async fn observe_access_for_sweep(
    root: std::path::PathBuf,
    id: SessionId,
) -> Result<StorageAccessObservation, String> {
    tokio::task::spawn_blocking(move || observe_session_access(&root, id))
        .await
        .map_err(|_| "access task failed")?
        .map_err(|_| "access unavailable".into())
}

async fn maintain_session_at(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
    after: Option<ArtifactSweepCursor>,
    now: u64,
) -> Result<Option<ArtifactSweepCursor>, String> {
    if !state
        .session_catalog
        .ambiguous_location_ids(id)
        .await
        .is_empty()
    {
        skipped(state, id, now, "ambiguous_location");
        return Ok(None);
    }
    let config = state.session_config(id).await.session_storage;
    if !config.enabled {
        skipped(state, id, now, "policy_disabled");
        return Ok(None);
    }
    // Validate optional age metadata before any upgrade. Missing age is conservative and
    // supported, but a corrupt/future record is not permission to mutate canonical storage.
    let access_root = root.to_path_buf();
    let observation = observe_access_for_sweep(access_root, id).await?;
    if !upgrade::prepare(state, root, id, now, config.artifact_timeout_secs).await? {
        return Ok(None);
    }
    let StorageAccessObservation::Recorded(record) = observation else {
        initialize_access(state, root, id, now, config.artifact_timeout_secs).await?;
        skipped(state, id, now, "access_initialized");
        return Ok(None);
    };
    let Some(age) = now.checked_sub(record.observed_at_ms) else {
        skipped(state, id, now, "clock_before_access");
        return Ok(None);
    };
    let light = u64::from(config.light_after_days) * 86_400_000;
    let deep = u64::from(config.deep_after_days) * 86_400_000;
    let (compression, minimum_age) = if age >= deep {
        (ArtifactCompression::Deep, deep)
    } else if age >= light {
        (ArtifactCompression::Light, light)
    } else {
        skipped(state, id, now, "recent_access");
        return Ok(None);
    };
    let mut cursor = after.as_ref().map(|cursor| cursor.after.clone());
    {
        let page = bcode_session::artifact_storage::maintenance_candidates_through(
            root,
            id,
            cursor.as_ref().map(|(a, r)| (a.as_str(), r.as_str())),
            after.as_ref().map(|cursor| cursor.through_sequence),
        )
        .await
        .map_err(|_| "references unavailable")?;
        let has_more = page.references.len() == 16;
        for (artifact, reference) in page.references {
            if state
                .shutdown_requested
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(None);
            }
            cursor = Some((artifact.clone(), reference.clone()));
            let cancellation = operation_cancellation(state)?;
            let mut shutdown = state.subscribe_shutdown();
            let conversion = compress_finalized_artifact_cancellable(
                root,
                id,
                &artifact,
                &reference,
                compression,
                config.minimum_saved_bytes,
                Some((now, minimum_age)),
                cancellation.clone(),
            );
            tokio::pin!(conversion);
            let outcome = tokio::select! {
                biased;
                _ = shutdown.recv() => {
                    cancellation.cancel();
                    // A blocking codec task cannot be aborted by dropping its async waiter.
                    // Keep awaiting so the maintenance fence outlives actual IO completion.
                    conversion.await
                }
                () = tokio::time::sleep(Duration::from_secs(u64::from(config.artifact_timeout_secs))) => {
                    cancellation.cancel();
                    conversion.await
                }
                outcome = &mut conversion => outcome,
            };
            record_artifact_outcome(state, id, now, outcome)?;
        }
        Ok(ArtifactSweepCursor::continuation(
            cursor,
            page.through_sequence,
            has_more,
        ))
    }
}

fn record_artifact_outcome(
    state: &ServerState,
    id: SessionId,
    now: u64,
    outcome: std::io::Result<bcode_session::artifact_storage::ArtifactStorageOutcome>,
) -> Result<(), String> {
    match outcome {
        Ok(bcode_session::artifact_storage::ArtifactStorageOutcome::Compressed {
            saved_bytes,
            ..
        }) => {
            state
                .metrics
                .add_counter("storage.maintenance.artifact_bytes_saved", saved_bytes);
            state
                .metrics
                .increment_counter("storage.maintenance.artifact_compressed");
        }
        Ok(bcode_session::artifact_storage::ArtifactStorageOutcome::Unchanged) => {
            state
                .metrics
                .increment_counter("storage.maintenance.artifact_unchanged");
        }
        Err(error) => {
            state
                .metrics
                .increment_counter("storage.maintenance.artifact_deferred");
            // Preserve error categories only; storage paths are not diagnostic API values.
            let reason = match error.kind() {
                std::io::ErrorKind::Interrupted => return Err("cancelled".into()),
                std::io::ErrorKind::WouldBlock => "ownership_unavailable",
                std::io::ErrorKind::NotFound => "artifact_missing",
                _ => "artifact_compression_deferred",
            };
            state
                .metrics
                .increment_counter(format!("storage.maintenance.reason.{reason}"));
            state
                .storage_worker
                .record_attempt(id, now, "partial", reason, None);
            // One missing/busy artifact must not starve the rest of the finite page
            // or history compression. A later sweep retries it with fresh evidence.
        }
    }
    Ok(())
}

async fn initialize_access(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
    now: u64,
    timeout_secs: u32,
) -> Result<(), String> {
    let cancellation = operation_cancellation(state)?;
    let mut shutdown = state.subscribe_shutdown();
    let initialize = bcode_session::artifact_storage::initialize_maintenance_access_cancellable(
        root,
        id,
        now,
        cancellation.clone(),
    );
    tokio::pin!(initialize);
    tokio::select! {
        biased;
        _ = shutdown.recv() => { cancellation.cancel(); initialize.await }
        () = tokio::time::sleep(Duration::from_secs(u64::from(timeout_secs))) => {
            cancellation.cancel(); initialize.await
        }
        result = &mut initialize => result,
    }
    .map_err(|_| "access initialization deferred")?;

    Ok(())
}
