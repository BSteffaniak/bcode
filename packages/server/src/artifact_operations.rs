//! Transport-neutral application operations for session-owned artifacts.

use super::ServerState;
use bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES;

/// Public failure while reading a session artifact range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadArtifactError {
    /// The requested byte count is zero or exceeds the bounded read limit.
    #[error("artifact range length must be between 1 and {MAX_SESSION_ARTIFACT_RANGE_BYTES} bytes")]
    InvalidLength,
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
            Self::InvalidLength => "invalid_artifact_range_length",
            Self::NotFound => "artifact_not_found",
            Self::Unavailable => "artifact_unavailable",
            Self::Failed => "artifact_read_failed",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub fn message(self) -> String {
        self.to_string()
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
) -> Result<bcode_session_models::SessionArtifactRange, ReadArtifactError> {
    if length == 0 || length > super::MAX_ARTIFACT_RANGE_BYTES {
        return Err(ReadArtifactError::InvalidLength);
    }
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

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn invalid_lengths_fail_before_session_lookup() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        for length in [0, super::super::MAX_ARTIFACT_RANGE_BYTES + 1, u32::MAX] {
            let error = super::read_range(
                &state,
                bcode_session_models::SessionId::new(),
                "missing-artifact",
                "missing-reference",
                0,
                length,
            )
            .await
            .expect_err("invalid length");
            assert_eq!(error, super::ReadArtifactError::InvalidLength);
            assert_eq!(error.code(), "invalid_artifact_range_length");
            assert_eq!(error.to_string(), error.message());
        }
        drop(state);
    }
}
