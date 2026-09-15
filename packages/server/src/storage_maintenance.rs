//! Bounded daemon-owned automatic artifact maintenance.
//!
//! Scheduling is optional; failures are isolated from canonical session reads and daemon startup.
//! Each tick advances a retained directory iterator by at most sixteen entries and processes one
//! bounded reference page. No canonical history is replayed or search index rebuilt.

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests;

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

/// Run the experimental maintenance worker until daemon shutdown.
/// Not activated by normal startup pending complete safety coordination.
pub async fn run(state: Arc<ServerState>) {
    if !wait_for_tracking_readiness(&state).await {
        return;
    }
    let Some(root) = state.sessions.session_store_root() else {
        return;
    };
    let mut shutdown = state.subscribe_shutdown();
    let cadence = state
        .startup_config
        .session_storage
        .maintenance_interval_secs;
    if cadence == 0 || state.startup_config.session_storage.artifact_timeout_secs == 0 {
        tracing::warn!("automatic artifact maintenance disabled: invalid timing configuration");
        return;
    }
    let mut interval = tokio::time::interval(Duration::from_secs(u64::from(cadence)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut directories = None;
    let mut pending: std::collections::VecDeque<(SessionId, MaintenanceCursor)> =
        std::collections::VecDeque::new();
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
        if pending.is_empty() {
            let result = tokio::task::spawn_blocking({
                let root = root.clone();
                let mut current = directories.take();
                move || -> std::io::Result<_> {
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
                }
            })
            .await;
            let Ok(Ok((next, ids))) = result else {
                tracing::warn!("automatic artifact maintenance discovery unavailable");
                continue;
            };
            directories = next;
            pending.extend(
                ids.into_iter()
                    .map(|id| (id, MaintenanceCursor::Artifacts(None))),
            );
        }
        if let Some((id, cursor)) = pending.pop_front() {
            let outcome = match cursor {
                MaintenanceCursor::Artifacts(after) => maintain_session(&state, &root, id, after)
                    .await
                    .map(|next| {
                        Some(next.map_or(
                            MaintenanceCursor::History {
                                start: 0,
                                through: None,
                            },
                            |key| MaintenanceCursor::Artifacts(Some(key)),
                        ))
                    }),
                MaintenanceCursor::History { start, through } => {
                    maintain_history(&state, &root, id, start, through).await
                }
            };
            match outcome {
                Ok(Some(next)) => pending.push_back((id, next)),
                Ok(None) => {}
                Err(_) => tracing::debug!(
                    "automatic artifact maintenance deferred: ownership or storage evidence unavailable"
                ),
            }
        }
    }
}

async fn wait_for_tracking_readiness(state: &ServerState) -> bool {
    state
        .metrics
        .set_gauge("storage.maintenance.compatibility_ready", 0);
    let mut shutdown = state.subscribe_shutdown();
    if tracking_coverage_ready(state) {
        return true;
    }
    let _ = shutdown.recv().await;
    false
}

fn tracking_coverage_ready(state: &ServerState) -> bool {
    // No compatibility proof is currently installed. Do not infer it from clean registry files:
    // old clients and unregistered fallbacks are not represented by those files.
    let _failed = state
        .storage_tracking_failed
        .load(std::sync::atomic::Ordering::SeqCst);
    false
}

fn operation_cancellation(state: &ServerState) -> Result<ArtifactMaintenanceCancellation, String> {
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
    let cancellation = ArtifactMaintenanceCancellation::default();
    registration
        .as_ref()
        .map_or(Ok(cancellation.clone()), |registration| {
            registration
                .acknowledgement()
                .map(|ack| cancellation.with_acknowledgement(ack))
                .map_err(|_| "unhealthy storage registration".into())
        })
}

async fn maintain_history(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
    start: u64,
    through: Option<u64>,
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
        return Ok(None);
    }
    let now = super::current_time_ms();
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
    reclaim_completed_pass(state, root, id, now, age, policy.minimum_saved_bytes).await;
    Ok(None)
}

async fn reclaim_completed_pass(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
    now: u64,
    age: u64,
    minimum: u64,
) {
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let Ok(cancellation) = operation_cancellation(state) else {
        return;
    };
    match bcode_session::storage_reclamation::reclaim_idle_session_storage_admitted(
        root,
        id,
        now,
        age,
        minimum,
        cancellation,
    )
    .await
    {
        Ok(bcode_session::storage_reclamation::SessionReclamationOutcome::Reclaimed(report)) => {
            state.metrics.add_counter(
                "storage.maintenance.reclaimed_bytes",
                report.reclaimed_bytes(),
            );
        }
        Ok(_) => {}
        Err(_) => tracing::debug!("automatic session reclamation deferred"),
    }
}

async fn maintain_session(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
    after: Option<ArtifactSweepCursor>,
) -> Result<Option<ArtifactSweepCursor>, String> {
    maintain_session_at(state, root, id, after, super::current_time_ms()).await
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
        return Ok(None);
    }
    let config = state.session_config(id).await.session_storage;
    if !config.enabled {
        return Ok(None);
    }
    let access_root = root.to_path_buf();
    let observation = observe_access_for_sweep(access_root, id).await?;
    let StorageAccessObservation::Recorded(record) = observation else {
        bcode_session::artifact_storage::initialize_maintenance_access(root, id, now)
            .await
            .map_err(|_| "access initialization deferred")?;
        return Ok(None);
    };
    let Some(age) = now.checked_sub(record.observed_at_ms) else {
        return Ok(None);
    };
    let light = u64::from(config.light_after_days) * 86_400_000;
    let deep = u64::from(config.deep_after_days) * 86_400_000;
    let (compression, minimum_age) = if age >= deep {
        (ArtifactCompression::Deep, deep)
    } else if age >= light {
        (ArtifactCompression::Light, light)
    } else {
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
            if outcome.is_err() {
                tracing::debug!("automatic artifact candidate deferred");
            }
        }
        if !has_more {
            reclaim_completed_pass(
                state,
                root,
                id,
                now,
                minimum_age,
                config.minimum_saved_bytes,
            )
            .await;
        }
        Ok(ArtifactSweepCursor::continuation(
            cursor,
            page.through_sequence,
            has_more,
        ))
    }
}
