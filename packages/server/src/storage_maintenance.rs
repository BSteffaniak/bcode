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
use bcode_session::storage_access::{StorageAccessObservation, observe_access};
use bcode_session_models::SessionId;
use std::sync::Arc;
use std::time::Duration;

/// Run the experimental maintenance worker until daemon shutdown.
/// Not activated by normal startup pending complete safety coordination.
pub async fn run(state: Arc<ServerState>) {
    let Some(root) = state.sessions.session_store_root() else {
        return;
    };
    let mut shutdown = state.subscribe_shutdown();
    let mut interval = tokio::time::interval(Duration::from_mins(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut directories = None;
    let mut pending: std::collections::VecDeque<(SessionId, Option<(String, String)>)> =
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
            pending.extend(ids.into_iter().map(|id| (id, None)));
        }
        if let Some((id, cursor)) = pending.pop_front() {
            match maintain_session(&state, &root, id, cursor).await {
                Ok(Some(next)) => pending.push_back((id, Some(next))),
                Ok(None) => {}
                Err(_) => tracing::debug!(
                    "automatic artifact maintenance deferred: ownership or storage evidence unavailable"
                ),
            }
        }
    }
}

async fn maintain_session(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
    after: Option<(String, String)>,
) -> Result<Option<(String, String)>, String> {
    maintain_session_at(state, root, id, after, super::current_time_ms()).await
}

async fn maintain_session_at(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
    after: Option<(String, String)>,
    now: u64,
) -> Result<Option<(String, String)>, String> {
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
    let path = root.join(id.to_string()).join("storage-access.bin");
    let observation = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path)?;
        observe_access(&mut file)
    })
    .await
    .map_err(|_| "access task failed")?
    .map_err(|_| "access unavailable")?;
    let StorageAccessObservation::Recorded(record) = observation else {
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
    let mut cursor = after;
    {
        let rows = bcode_session::artifact_storage::maintenance_candidates(
            root,
            id,
            cursor.as_ref().map(|(a, r)| (a.as_str(), r.as_str())),
        )
        .await
        .map_err(|_| "references unavailable")?;
        if rows.is_empty() {
            return Ok(None);
        }
        let has_more = rows.len() == 16;
        for (artifact, reference) in rows {
            if state
                .shutdown_requested
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(None);
            }
            cursor = Some((artifact.clone(), reference.clone()));
            let cancellation = ArtifactMaintenanceCancellation::default();
            let mut shutdown = state.subscribe_shutdown();
            let conversion = compress_finalized_artifact_cancellable(
                root,
                id,
                &artifact,
                &reference,
                compression,
                4096,
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
                outcome = &mut conversion => outcome,
            };
            if outcome.is_err() {
                tracing::debug!("automatic artifact candidate deferred");
            }
        }
        Ok(if has_more { cursor } else { None })
    }
}
