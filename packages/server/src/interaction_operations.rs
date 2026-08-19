//! Transport-neutral application operations for pending tool exchanges.

use super::{ClientId, ServerState, ToolExchangeResolution};
use bcode_ipc::PendingToolExchangeSummary;

/// Application-level failure while resolving a pending tool exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveToolExchangeError {
    /// The requesting client did not advertise a compatible exchange adapter.
    IncompatibleConsumer,
}

/// Return current pending permission summaries without transport framing.
pub async fn list_permissions(state: &ServerState) -> Vec<bcode_ipc::PermissionSummary> {
    state
        .pending_permissions
        .lock()
        .await
        .values()
        .map(|permission| permission.summary.clone())
        .collect()
}

/// Resolve one pending permission without transport framing.
pub async fn resolve_permission(
    state: &ServerState,
    permission_id: &str,
    approved: bool,
    remember: bool,
) -> bool {
    let Some(permission) =
        super::take_pending_permission_for_individual(state, permission_id).await
    else {
        return false;
    };
    super::resolve_pending_permission(state, permission, approved, remember).await;
    true
}

/// Resolve one complete pending permission batch without transport framing.
pub async fn resolve_permission_batch(
    state: &ServerState,
    batch_id: &str,
    approved: bool,
) -> usize {
    super::resolve_permission_batch_operation(state, batch_id, approved).await
}

/// Return the current bounded pending tool-exchange summaries.
pub async fn list_pending_tool_exchanges(state: &ServerState) -> Vec<PendingToolExchangeSummary> {
    state
        .pending_tool_exchanges
        .lock()
        .await
        .values()
        .map(|request| request.summary.clone())
        .collect()
}

/// Complete a known pending exchange from an authoritative internal consumer.
pub async fn complete_pending_tool_exchange(
    state: &ServerState,
    interaction_id: &str,
    resolution: ToolExchangeResolution,
) -> bool {
    let request = state
        .pending_tool_exchanges
        .lock()
        .await
        .remove(interaction_id);
    let Some(request) = request else {
        return false;
    };
    *request.resolution.lock().await = Some(resolution);
    request.notify.notify_waiters();
    true
}

/// Resolve one pending exchange through canonical server-owned interaction state.
///
/// Returns `Ok(false)` when the exchange is already terminal or otherwise no longer pending.
pub async fn resolve_tool_exchange(
    state: &ServerState,
    client_id: ClientId,
    interaction_id: &str,
    resolution: ToolExchangeResolution,
) -> Result<bool, ResolveToolExchangeError> {
    let pending = state
        .pending_tool_exchanges
        .lock()
        .await
        .get(interaction_id)
        .cloned();
    let Some(pending) = pending else {
        return Ok(false);
    };
    if !state
        .client_supports_exchange(client_id, &pending.summary.request)
        .await
    {
        return Err(ResolveToolExchangeError::IncompatibleConsumer);
    }
    Ok(complete_pending_tool_exchange(state, interaction_id, resolution).await)
}
