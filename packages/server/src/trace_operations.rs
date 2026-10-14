//! Transport-neutral, explicit diagnostic access to session-owned trace blobs.

use super::ServerState;
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, MAX_SESSION_TRACE_BLOB_BYTES,
    ReadSessionTraceBlobRequest, SESSION_TRACE_EXPORT_VERSION, SessionEvent, SessionEventKind,
    SessionTraceBlobRange, SessionTracePayload, TraceBlobRef,
};
use std::path::Path;

#[cfg(test)]
mod tests;

/// Secret-safe trace read failure. No private path, payload or internal error is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadTraceBlobError {
    /// Unknown operation/event version or unsupported storage/reference representation.
    #[error("trace export representation is unsupported")]
    Unsupported,
    /// Zero/oversized length, overflowing range, or offset beyond retained EOF.
    #[error("invalid trace byte range")]
    InvalidRange,
    /// No retained reference exists at the exact canonical event sequence.
    #[error("retained trace reference was not found")]
    NotFound,
    /// Session storage admission or bounded canonical lookup failed.
    #[error("session trace is unavailable; check session storage status")]
    Unavailable,
    /// Referenced content no longer exists.
    #[error("retained trace content is missing")]
    Missing,
    /// Reference identity, confinement, byte count, hash or storage is damaged.
    #[error("retained trace content failed verification")]
    Damaged,
    /// The entire retained blob exceeds the bounded verification budget.
    #[error("retained trace content exceeds the export verification limit")]
    TooLarge,
}

impl ReadTraceBlobError {
    /// Stable transport-independent public error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unsupported => "trace_export_unsupported",
            Self::InvalidRange => "invalid_trace_range",
            Self::NotFound => "trace_reference_not_found",
            Self::Unavailable => "trace_unavailable",
            Self::Missing => "trace_content_missing",
            Self::Damaged => "trace_content_damaged",
            Self::TooLarge => "trace_export_too_large",
        }
    }
}

/// Resolve one retained reference from canonical history and return verified bounded bytes.
///
/// Caller must authorize explicit sensitive diagnostic access to the selected session.
/// This operation does not enable capture, replay history, repair state or guess missing inputs.
///
/// # Errors
/// Returns a secret-safe error for unsupported versions, invalid ranges, unavailable canonical
/// history, absent references, unsafe paths, missing/damaged content or oversized captures.
pub async fn read_range(
    state: &ServerState,
    request: ReadSessionTraceBlobRequest,
) -> Result<SessionTraceBlobRange, ReadTraceBlobError> {
    validate_request(&request)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let mut admission = super::storage_read_admission::RegisteredStorageRead::for_session(
        state,
        request.session_id,
    )
    .await
    .map_err(|_| ReadTraceBlobError::Unavailable)?;
    let result = async {
        let events = state
            .sessions
            .session_events_range(request.session_id, request.sequence, request.sequence, 1)
            .await
            .map_err(|_| ReadTraceBlobError::Unavailable)?;
        let event = events.first().ok_or(ReadTraceBlobError::NotFound)?;
        let reference = retained_reference(event, &request)?;
        let root = state.trace_store.root.clone();
        let read_request = request.clone();
        // A cancelled future must not release storage admission while blocking IO still runs.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let held_admission = admission.take();
            let (result, held_admission) = tokio::task::spawn_blocking(move || {
                let result = read_verified(&root, &read_request, &reference);
                (result, held_admission)
            })
            .await
            .map_err(|_| ReadTraceBlobError::Unavailable)?;
            admission = held_admission;
            result
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        tokio::task::spawn_blocking(move || read_verified(&root, &read_request, &reference))
            .await
            .map_err(|_| ReadTraceBlobError::Unavailable)?
    }
    .await;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let result =
        super::storage_read_admission::RegisteredStorageRead::finish_failed(&mut admission, result)
            .await;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(admission) = admission {
        admission.finish_history(state, request.session_id).await;
    }
    result
}

fn validate_request(request: &ReadSessionTraceBlobRequest) -> Result<(), ReadTraceBlobError> {
    if request.version != SESSION_TRACE_EXPORT_VERSION {
        return Err(ReadTraceBlobError::Unsupported);
    }
    if request.length == 0
        || request.length > MAX_SESSION_TRACE_BLOB_BYTES
        || request
            .offset
            .checked_add(u64::from(request.length))
            .is_none()
    {
        return Err(ReadTraceBlobError::InvalidRange);
    }
    Ok(())
}

fn retained_reference(
    event: &SessionEvent,
    request: &ReadSessionTraceBlobRequest,
) -> Result<TraceBlobRef, ReadTraceBlobError> {
    if event.session_id != request.session_id || event.sequence != request.sequence {
        return Err(ReadTraceBlobError::Damaged);
    }
    if event.schema_version != CURRENT_SESSION_EVENT_SCHEMA_VERSION {
        return Err(ReadTraceBlobError::Unsupported);
    }
    let SessionEventKind::TraceEvent { trace } = &event.kind else {
        return Err(ReadTraceBlobError::NotFound);
    };
    if let SessionTracePayload::ProviderEvent { event_type, detail } = &trace.payload
        && event_type == bcode_session_models::REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE
    {
        let reference = bcode_session_models::RequestContextObservationReference::parse(
            event_type,
            detail.as_deref().ok_or(ReadTraceBlobError::Damaged)?,
        )
        .map_err(|_| ReadTraceBlobError::Unsupported)?;
        if trace.turn_id.as_ref().is_none_or(String::is_empty)
            || reference.model_request_sequence >= event.sequence
        {
            return Err(ReadTraceBlobError::Damaged);
        }
        return Ok(reference.blob);
    }
    match &trace.payload {
        SessionTracePayload::ModelRequestBuilt { request, .. } => request.as_ref(),
        SessionTracePayload::ToolInvocationStarted { arguments, .. } => arguments.as_ref(),
        SessionTracePayload::ToolInvocationFinished { output, .. } => output.as_ref(),
        _ => None,
    }
    .cloned()
    .ok_or(ReadTraceBlobError::NotFound)
}

fn validate_reference(
    request: &ReadSessionTraceBlobRequest,
    reference: &TraceBlobRef,
) -> Result<(), ReadTraceBlobError> {
    // Bound all reference strings before parsing. Only the writer's exact flat layout is accepted.
    if reference.path.len() > 512
        || reference.sha256.len() != 64
        || !reference
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ReadTraceBlobError::Damaged);
    }
    let extension = match reference.content_type.as_str() {
        "application/json" => "json",
        "text/plain" => "txt",
        _ => return Err(ReadTraceBlobError::Unsupported),
    };
    let session = request.session_id.to_string();
    let parts: Vec<_> = reference.path.split('/').collect();
    if parts.len() != 3 || parts[0] != session || parts[1] != "blobs" {
        return Err(ReadTraceBlobError::Damaged);
    }
    let suffix = format!("-{}.{}", reference.sha256, extension);
    let name = parts[2]
        .strip_suffix(&suffix)
        .ok_or(ReadTraceBlobError::Damaged)?;
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ReadTraceBlobError::Damaged);
    }
    if reference.byte_len > u64::from(MAX_SESSION_TRACE_BLOB_BYTES) {
        return Err(ReadTraceBlobError::TooLarge);
    }
    if request.offset > reference.byte_len {
        return Err(ReadTraceBlobError::InvalidRange);
    }
    Ok(())
}

fn read_verified(
    root: &Path,
    request: &ReadSessionTraceBlobRequest,
    reference: &TraceBlobRef,
) -> Result<SessionTraceBlobRange, ReadTraceBlobError> {
    validate_request(request)?;
    validate_reference(request, reference)?;
    if root.as_os_str().is_empty() {
        return Err(ReadTraceBlobError::Unavailable);
    }
    // The existing artifact domain pins descendants with no-follow descriptor-relative opens.
    // Do not substitute canonicalize + ordinary File::open on other platforms.
    #[cfg(not(unix))]
    return Err(ReadTraceBlobError::Unsupported);
    #[cfg(unix)]
    {
        use sha2::{Digest as _, Sha256};

        let (total, bytes) = bcode_session::artifact_storage::read_artifact_range(
            root,
            &root.join(&reference.path),
            0,
            MAX_SESSION_TRACE_BLOB_BYTES,
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ReadTraceBlobError::Missing
            } else {
                ReadTraceBlobError::Damaged
            }
        })?;
        if total != reference.byte_len
            || u64::try_from(bytes.len()).ok() != Some(total)
            || hex::encode(Sha256::digest(&bytes)) != reference.sha256
        {
            return Err(ReadTraceBlobError::Damaged);
        }
        let start =
            usize::try_from(request.offset).map_err(|_| ReadTraceBlobError::InvalidRange)?;
        let count =
            usize::try_from(request.length).map_err(|_| ReadTraceBlobError::InvalidRange)?;
        let end = start.saturating_add(count).min(bytes.len());
        Ok(SessionTraceBlobRange {
            version: SESSION_TRACE_EXPORT_VERSION,
            session_id: request.session_id,
            sequence: request.sequence,
            sha256: reference.sha256.clone(),
            content_type: reference.content_type.clone(),
            byte_len: total,
            redaction: reference.redaction,
            completeness: reference.completeness,
            offset: request.offset,
            bytes: bytes[start..end].to_vec(),
            eof: end == bytes.len(),
        })
    }
}
