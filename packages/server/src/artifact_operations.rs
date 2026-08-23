//! Transport-neutral application operations for session-owned artifacts.

use super::ServerState;

/// Read one bounded, confined range from a finalized or active session artifact.
pub async fn read_range(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    artifact_id: &str,
    reference_key: &str,
    offset: u64,
    length: u32,
) -> Result<bcode_ipc::SessionArtifactRange, String> {
    super::read_session_artifact_range(
        state,
        session_id,
        artifact_id,
        reference_key,
        offset,
        length,
    )
    .await
}

/// Return a stable public error code for one secret-safe artifact read failure.
#[must_use]
pub fn error_code(error: &str) -> &'static str {
    if error.contains("was not found in the finalized projection") {
        "artifact_not_found"
    } else if error.contains("artifact reference has no storage URI")
        || error.contains("artifact reference is unavailable")
        || error.contains("artifact reference is incomplete")
        || error.contains("artifact file is unavailable")
        || error.contains("No such file or directory")
    {
        "artifact_unavailable"
    } else {
        "artifact_read_failed"
    }
}
