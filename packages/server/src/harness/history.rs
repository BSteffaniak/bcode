//! Plugin-independent canonical history. Native poll cursors are never history cursors.
use super::ManagedHarnessError;
use bcode_harness_models::HarnessObservationPage;
use bcode_session_models::SessionId;
use std::sync::Arc;

/// Read bounded indexed normalized history without loading or invoking a native plugin.
/// The owning application must admit the selected-location read before calling this method.
/// # Errors
/// Rejects excessive bounds, malformed versioned cursors and degraded canonical projections.
pub async fn canonical_history(
    state: &Arc<crate::ServerState>,
    session_id: SessionId,
    after: Option<String>,
    limit: u16,
) -> Result<HarnessObservationPage, ManagedHarnessError> {
    if limit == 0
        || usize::from(limit) > bcode_harness_models::MAX_HARNESS_OBSERVATIONS
        || after
            .as_ref()
            .is_some_and(|cursor| cursor.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES)
    {
        return Err(ManagedHarnessError::Invalid);
    }
    state
        .sessions
        .external_harness_history(session_id, after, limit)
        .await
        .map_err(|_| ManagedHarnessError::Storage)
}
