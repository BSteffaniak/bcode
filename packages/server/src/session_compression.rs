//! Explicit page-based compression; shared session primitives retain execution authority.
#[cfg(test)]
mod tests;
use super::ServerState;
use bcode_session::artifact_storage::{
    ArtifactStorageOutcome, compress_finalized_artifact_cancellable, maintenance_candidates_through,
};
use bcode_session::storage_access::{StorageAccessObservation, observe_session_access};
use bcode_session_models::{
    StorageCompressionCursor as Cursor, StorageCompressionDisposition as Disposition,
    StorageCompressionFailure as Failure, StorageCompressionRequest, StorageCompressionResult,
    StorageCompressionTier,
};

pub async fn compress_page(
    state: &ServerState,
    request: StorageCompressionRequest,
) -> Result<StorageCompressionResult, &'static str> {
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err("daemon is shutting down");
    }
    if request.as_of_ms > super::current_time_ms() || request.minimum_age_ms == Some(0) {
        return Err("invalid compression cutoff or age");
    }
    let mut result = StorageCompressionResult {
        session_id: request.session_id,
        disposition: Disposition::Unavailable,
        artifact_bytes_saved: 0,
        history_payload_bytes_saved: 0,
        failures: 0,
        changed: false,
        failure: Some(Failure::StorageUnavailable),
        artifact_failure: None,
        next: None,
    };
    let Some(root) = state.sessions.session_store_root() else {
        return Ok(result);
    };
    if !state
        .session_catalog
        .ambiguous_location_ids(request.session_id)
        .await
        .is_empty()
    {
        return Ok(result);
    }
    if let Some(age) = request.minimum_age_ms {
        let path = root.clone();
        let id = request.session_id;
        let observation =
            tokio::task::spawn_blocking(move || observe_session_access(&path, id)).await;
        match observation {
            Ok(Ok(StorageAccessObservation::Recorded(record))) => {
                if request
                    .as_of_ms
                    .checked_sub(record.observed_at_ms)
                    .is_none_or(|elapsed| elapsed < age)
                {
                    result.failure = None;
                    result.disposition = Disposition::Recent;
                    return Ok(result);
                }
            }
            Ok(Ok(StorageAccessObservation::Unknown)) => {
                result.failure = None;
                result.disposition = Disposition::UnknownAge;
                return Ok(result);
            }
            _ => return Ok(result),
        }
    }
    result.failure = Some(Failure::CandidateInspectionFailed);
    if request.dry_run {
        // Candidate inspection takes exclusive session ownership but never initializes tracking.
        if maintenance_candidates_through(&root, request.session_id, None, None)
            .await
            .is_ok()
        {
            result.failure = None;
            result.disposition = Disposition::Eligible;
        }
        return Ok(result);
    }
    execute_page(state, &root, request, result).await
}

const fn compression_level(tier: StorageCompressionTier) -> i32 {
    match tier {
        StorageCompressionTier::Light => 1,
        StorageCompressionTier::Deep => 12,
    }
}

async fn preflight(
    state: &ServerState,
    root: &std::path::Path,
    id: bcode_session_models::SessionId,
) -> Result<bcode_session::artifact_storage::ArtifactMaintenanceCancellation, Failure> {
    let cancellation = super::storage_maintenance::operation_cancellation(state)
        .map_err(|_| Failure::TrackingUnavailable)?;
    cancellation
        .check_tracking_admission(root, id)
        .await
        .map_err(|e| admission_failure(&e))?;
    Ok(cancellation)
}

async fn execute_page(
    state: &ServerState,
    root: &std::path::Path,
    request: StorageCompressionRequest,
    mut result: StorageCompressionResult,
) -> Result<StorageCompressionResult, &'static str> {
    let cancellation = match preflight(state, root, request.session_id).await {
        Ok(cancellation) => cancellation,
        Err(reason) => return Ok(failed(result, reason)),
    };
    let age = request.minimum_age_ms.map(|age| (request.as_of_ms, age));
    let tier = artifact_tier(request.tier);
    let cursor = request.cursor.unwrap_or(Cursor::Artifacts {
        after: None,
        through: None,
    });
    match cursor {
        Cursor::Artifacts { after, through } => {
            let Ok(page) = maintenance_candidates_through(
                root,
                request.session_id,
                after.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
                through,
            )
            .await
            else {
                return Ok(result);
            };
            // One reference per request bounds cancellation and transport work. Discovery is at most 16.
            if let Some((artifact, reference)) = page.references.first() {
                let work = compress_finalized_artifact_cancellable(
                    root,
                    request.session_id,
                    artifact,
                    reference,
                    tier,
                    4096,
                    age,
                    cancellation.clone(),
                );
                tokio::pin!(work);
                let mut shutdown = state.subscribe_shutdown();
                let mut timed_out = false;
                let outcome = tokio::select! {
                    biased;
                    _ = shutdown.recv() => { cancellation.cancel(); work.await }
                    () = tokio::time::sleep(std::time::Duration::from_secs(10)) => { timed_out = true; cancellation.cancel(); work.await }
                    outcome = &mut work => outcome,
                };
                match outcome {
                    Ok(ArtifactStorageOutcome::Compressed { saved_bytes, .. }) => {
                        result.artifact_bytes_saved = saved_bytes;
                    }
                    Ok(ArtifactStorageOutcome::Unchanged) => {}
                    Err(error) => {
                        result.artifact_failure = Some(artifact_failure_context(
                            artifact, reference, &error, timed_out,
                        ));
                        return Ok(failed(result, Failure::ArtifactFailed));
                    }
                }
                result.next = Some(Cursor::Artifacts {
                    after: Some((artifact.clone(), reference.clone())),
                    through: Some(page.through_sequence),
                });
            } else {
                result.next = Some(Cursor::History {
                    start: 0,
                    through: page.through_sequence,
                });
            }
        }
        Cursor::History { start, through } => {
            let level = compression_level(request.tier);
            let work = bcode_session::history_compression::compress_history_page_through(
                root,
                request.session_id,
                start,
                level,
                age,
                cancellation.clone(),
                Some(through),
            );
            tokio::pin!(work);
            let mut shutdown = state.subscribe_shutdown();
            let outcome = tokio::select! {
                biased;
                _ = shutdown.recv() => { cancellation.cancel(); work.await }
                () = tokio::time::sleep(std::time::Duration::from_secs(10)) => { cancellation.cancel(); work.await }
                outcome = &mut work => outcome,
            };
            if let Ok(page) = outcome {
                result.history_payload_bytes_saved = page.saved_bytes;
                result.next = page
                    .next_sequence
                    .map(|start| Cursor::History { start, through });
            } else {
                return Ok(failed(result, Failure::HistoryFailed));
            }
        }
    }
    result.changed = result.artifact_bytes_saved > 0 || result.history_payload_bytes_saved > 0;
    result.failure = None;
    result.disposition = Disposition::Processed;
    Ok(result)
}

fn failed(mut result: StorageCompressionResult, reason: Failure) -> StorageCompressionResult {
    result.disposition = Disposition::Unavailable;
    result.failures += 1;
    result.failure = Some(reason);
    result.next = None;
    result
}

fn admission_failure(error: &std::io::Error) -> Failure {
    match error.kind() {
        std::io::ErrorKind::WouldBlock => Failure::AdmissionBusy,
        std::io::ErrorKind::PermissionDenied
            if error.get_ref().is_some_and(|e| {
                e.is::<bcode_session::storage_admission::UnacknowledgedStorageDaemon>()
            }) =>
        {
            Failure::UnacknowledgedDaemon
        }
        _ => Failure::AdmissionUnavailable,
    }
}

const fn artifact_tier(
    tier: StorageCompressionTier,
) -> bcode_session::artifact_compression::ArtifactCompression {
    match tier {
        StorageCompressionTier::Light => {
            bcode_session::artifact_compression::ArtifactCompression::Light
        }
        StorageCompressionTier::Deep => {
            bcode_session::artifact_compression::ArtifactCompression::Deep
        }
    }
}

fn artifact_failure_context(
    artifact: &str,
    reference: &str,
    error: &std::io::Error,
    timed_out: bool,
) -> bcode_session_models::StorageArtifactFailure {
    bcode_session_models::StorageArtifactFailure {
        artifact_id: artifact.chars().take(256).collect(),
        reference_key: reference.chars().take(256).collect(),
        reason: if timed_out {
            bcode_session_models::ArtifactCompressionFailureReason::Timeout
        } else {
            bcode_session::artifact_storage::artifact_failure_reason(error)
        },
    }
}
