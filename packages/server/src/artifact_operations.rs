//! Transport-neutral application operations for session-owned artifacts.

use super::ServerState;

/// Public failure while reading a session artifact range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadArtifactError {
    /// The requested artifact or reference does not exist.
    #[error("artifact was not found")]
    NotFound,
    /// The artifact exists but is not currently readable.
    #[error("artifact is unavailable")]
    Unavailable,
    /// The read failed for another secret-safe reason.
    #[error("artifact read failed")]
    Failed,
}

impl ReadArtifactError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "artifact_not_found",
            Self::Unavailable => "artifact_unavailable",
            Self::Failed => "artifact_read_failed",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotFound => "artifact was not found",
            Self::Unavailable => "artifact is unavailable",
            Self::Failed => "artifact read failed",
        }
    }
}

/// Read one bounded, confined range from a finalized or active session artifact.
pub async fn read_range(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    artifact_id: &str,
    reference_key: &str,
    offset: u64,
    length: u32,
) -> Result<bcode_ipc::SessionArtifactRange, ReadArtifactError> {
    super::read_session_artifact_range(
        state,
        session_id,
        artifact_id,
        reference_key,
        offset,
        length,
    )
    .await
    .map_err(|error| classify_internal_error(&error))
}

/// Classify one internal artifact read failure without exposing its text.
#[must_use]
pub fn classify_internal_error(error: &str) -> ReadArtifactError {
    if error.contains("was not found in the finalized projection") {
        ReadArtifactError::NotFound
    } else if error.contains("artifact reference has no storage URI")
        || error.contains("artifact reference is unavailable")
        || error.contains("artifact reference is incomplete")
        || error.contains("artifact file is unavailable")
        || error.contains("No such file or directory")
    {
        ReadArtifactError::Unavailable
    } else {
        ReadArtifactError::Failed
    }
}
