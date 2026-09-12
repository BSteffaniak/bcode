//! Bounded daemon-owned automatic artifact maintenance.
//!
//! Scheduling is optional; failures are isolated from canonical session reads and daemon startup.
//! Each tick advances a retained directory iterator by at most sixteen entries and processes one
//! bounded reference page. No canonical history is replayed or search index rebuilt.

use super::ServerState;
use bcode_session::artifact_compression::ArtifactCompression;
use bcode_session::artifact_storage::compress_finalized_artifact_with_age;
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
        for id in ids {
            if state
                .shutdown_requested
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            if maintain_session(&state, &root, id).await.is_err() {
                tracing::debug!(
                    "automatic artifact maintenance deferred: ownership or storage evidence unavailable"
                );
            }
        }
    }
}

async fn maintain_session(
    state: &ServerState,
    root: &std::path::Path,
    id: SessionId,
) -> Result<(), String> {
    if !state
        .session_catalog
        .ambiguous_location_ids(id)
        .await
        .is_empty()
    {
        return Ok(());
    }
    let config = state.session_config(id).await.session_storage;
    if !config.enabled {
        return Ok(());
    }
    let now = super::current_time_ms();
    let path = root.join(id.to_string()).join("storage-access.bin");
    let observation = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path)?;
        observe_access(&mut file)
    })
    .await
    .map_err(|_| "access task failed")?
    .map_err(|_| "access unavailable")?;
    let StorageAccessObservation::Recorded(record) = observation else {
        return Ok(());
    };
    let Some(age) = now.checked_sub(record.observed_at_ms) else {
        return Ok(());
    };
    let light = u64::from(config.light_after_days) * 86_400_000;
    let deep = u64::from(config.deep_after_days) * 86_400_000;
    let (compression, minimum_age) = if age >= deep {
        (ArtifactCompression::Deep, deep)
    } else if age >= light {
        (ArtifactCompression::Light, light)
    } else {
        return Ok(());
    };
    // Candidate discovery is read-only and bounded. Publication rechecks compatibility, projection,
    // completeness, access age, and ownership independently under the maintenance fence.
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root)
        .await
        .map_err(|_| "database unavailable")?;
    let result = db.database().query_raw("SELECT artifact_id, reference_key FROM artifact_references WHERE complete = 1 AND availability = 'complete' ORDER BY artifact_id, reference_key LIMIT 16").await;
    db.database()
        .close()
        .await
        .map_err(|_| "database close failed")?;
    let rows = result.map_err(|_| "references unavailable")?;
    for row in rows {
        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            break;
        }
        let artifact = row
            .get("artifact_id")
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or("invalid artifact")?;
        let reference = row
            .get("reference_key")
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or("invalid reference")?;
        compress_finalized_artifact_with_age(
            root,
            id,
            &artifact,
            &reference,
            compression,
            4096,
            Some((now, minimum_age)),
        )
        .await
        .map_err(|_| "compression deferred")?;
    }
    Ok(())
}
