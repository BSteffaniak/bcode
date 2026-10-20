//! Caller-bound non-owning session progress observations.

use std::future::Future;

/// Extensible secret-safe progress failure.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionProgressApplicationFailure {
    /// Stable diagnostic category; unknown codes do not imply success.
    pub code: String,
    /// Normalized public diagnostic.
    pub message: String,
}

/// Read ephemeral progress without acquiring execution authority or replaying history.
pub trait SessionProgressApplication: Sync {
    /// Read current owner-scoped progress; unavailable coverage is not idleness.
    /// # Errors
    /// Rejects unavailable callers, locations, or unsupported state.
    fn session_progress(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> impl Future<
        Output = Result<crate::SessionProgressSnapshot, SessionProgressApplicationFailure>,
    > + Send;
}
