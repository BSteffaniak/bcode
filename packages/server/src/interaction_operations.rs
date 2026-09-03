//! Transport-neutral application operations for permissions and pending tool exchanges.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Mutex;

use super::{
    ClientId, PendingPermission, PendingPermissionBatch, PendingToolExchange, ServerState,
    SessionId, SkillToolDecision, SkillToolDecisionKey, ToolExchangeResolution, TurnCancelState,
    current_time_ms, publish_session_event,
};
use bcode_ipc::PendingToolExchangeSummary;

/// Application-level failure while resolving a pending tool exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveToolExchangeError {
    /// The requesting client did not advertise a compatible exchange adapter.
    IncompatibleConsumer,
    /// The serialized resolution is malformed or uses an unknown variant.
    InvalidResolution,
}

impl ResolveToolExchangeError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::IncompatibleConsumer => "incompatible_exchange_consumer",
            Self::InvalidResolution => "invalid_exchange_resolution",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::IncompatibleConsumer => "client did not advertise a compatible exchange adapter",
            Self::InvalidResolution => "tool exchange resolution is malformed or unsupported",
        }
    }
}

async fn append_permission_requested_event(
    state: &ServerState,
    session_id: SessionId,
    request: bcode_session_models::SessionEventKind,
) {
    match state
        .sessions
        .append_permission_requested(session_id, request)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => tracing::warn!("failed to append permission request: {error}"),
    }
}

async fn append_permission_resolved_event(
    state: &ServerState,
    session_id: SessionId,
    permission_id: String,
    approved: bool,
) {
    match state
        .sessions
        .append_permission_resolved(session_id, permission_id, approved)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => tracing::warn!("failed to append permission result: {error}"),
    }
}

impl PendingPermissionBatch {
    /// Create one unresolved batch state.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            decision: Mutex::const_new(None),
        }
    }
}

/// Return a remembered skill-tool permission decision when user state is available.
pub fn remembered_skill_tool_decision(key: &SkillToolDecisionKey) -> Option<SkillToolDecision> {
    bcode_settings::SettingsStore::default()
        .skill_tool_decisions()
        .ok()
        .and_then(|state| state.decision_for(key).map(|entry| entry.decision))
}

fn remember_skill_tool_decision(key: SkillToolDecisionKey, decision: SkillToolDecision) {
    let store = bcode_settings::SettingsStore::default();
    let Ok(mut state) = store.skill_tool_decisions() else {
        return;
    };
    state.upsert(bcode_skill_models::SkillToolDecisionEntry {
        key,
        decision,
        remembered_at_ms: current_time_ms(),
        reason: Some("remembered from permission dialog".to_owned()),
    });
    let _ = store.save_skill_tool_decisions(&state, current_time_ms());
}

/// Allocate the next process-local pending permission identity.
pub async fn next_permission_id(state: &ServerState) -> String {
    let mut next = state.next_permission_id.lock().await;
    let permission_id = format!("perm-{}", *next);
    *next += 1;
    permission_id
}

/// Allocate the next process-local permission batch identity.
pub async fn next_permission_batch_id(state: &ServerState) -> String {
    let mut next = state.next_permission_batch_id.lock().await;
    let batch_id = format!("permission-batch-{}", *next);
    *next += 1;
    batch_id
}

/// Scoped registration for one permission batch.
pub struct PendingPermissionBatchRegistration {
    batches: Arc<StdMutex<BTreeMap<String, Arc<PendingPermissionBatch>>>>,
    batch_id: String,
}

impl PendingPermissionBatchRegistration {
    /// Allocate and register one unresolved batch until this guard is dropped.
    pub async fn allocate(state: &ServerState, session_id: SessionId) -> (String, Self) {
        let batch_id = next_permission_batch_id(state).await;
        let registration = Self::register(state, batch_id.clone(), session_id);
        (batch_id, registration)
    }

    /// Register one unresolved batch until this guard is dropped.
    #[must_use]
    pub fn register(state: &ServerState, batch_id: String, session_id: SessionId) -> Self {
        let batch = Arc::new(PendingPermissionBatch::new(session_id));
        state
            .pending_permission_batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(batch_id.clone(), batch);
        Self {
            batches: Arc::clone(&state.pending_permission_batches),
            batch_id,
        }
    }
}

impl Drop for PendingPermissionBatchRegistration {
    fn drop(&mut self) {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.batch_id);
    }
}

/// Persist one normalized agent permission rule through the configuration owner.
///
/// # Errors
///
/// Returns a normalized message when the rule fields or configuration write are invalid.
pub fn add_permission_rule(
    agent_id: &str,
    category: &str,
    pattern: String,
    action: &str,
) -> Result<PathBuf, String> {
    bcode_config::upsert_agent_permission_rule(agent_id, category, pattern, action)
        .map_err(|error| error.to_string())
}

/// Register one pending permission while preserving its optional batch-decision latch.
///
/// Returns the already-latched decision when the batch has already resolved, or denial when the
/// referenced batch is no longer active.
pub async fn register_pending_permission(
    state: &ServerState,
    pending: &PendingPermission,
    event: bcode_session_models::SessionEventKind,
) -> Result<(), bool> {
    let batch_state = pending.summary.batch.as_ref().and_then(|batch| {
        state
            .pending_permission_batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&batch.batch_id)
            .cloned()
    });
    if pending.summary.batch.is_some() && batch_state.is_none() {
        return Err(false);
    }
    let batch_decision = if let Some(batch_state) = batch_state.as_ref() {
        let decision = batch_state.decision.lock().await;
        if let Some(decision) = *decision {
            return Err(decision);
        }
        Some(decision)
    } else {
        None
    };
    state
        .pending_permissions
        .lock()
        .await
        .insert(pending.summary.permission_id.clone(), pending.clone());
    append_permission_requested_event(state, pending.summary.session_id, event).await;
    drop(batch_decision);
    Ok(())
}

/// Read one current permission-batch decision for focused verification.
#[cfg(test)]
pub async fn permission_batch_decision(state: &ServerState, batch_id: &str) -> Option<bool> {
    let batch = state
        .pending_permission_batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(batch_id)
        .cloned()?;
    *batch.decision.lock().await
}

/// Return current pending permission summaries without transport framing.
pub async fn list_permissions(state: &ServerState) -> Vec<bcode_ipc::PermissionSummary> {
    let mut permissions = state
        .pending_permissions
        .lock()
        .await
        .values()
        .map(|permission| permission.summary.clone())
        .collect::<Vec<_>>();
    permissions.sort_by(|left, right| left.permission_id.cmp(&right.permission_id));
    permissions
}

/// Resolve one pending permission without transport framing.
pub async fn resolve_permission(
    state: &ServerState,
    permission_id: &str,
    approved: bool,
    remember: bool,
) -> bool {
    let Some(permission) = take_pending_permission_for_individual(state, permission_id).await
    else {
        return false;
    };
    complete_pending_permission(state, permission, approved, remember).await;
    true
}

pub async fn complete_pending_permission(
    state: &ServerState,
    permission: PendingPermission,
    approved: bool,
    remember: bool,
) {
    if remember && let Some(key) = permission.skill_decision_key.clone() {
        remember_skill_tool_decision(
            key,
            if approved {
                SkillToolDecision::Allow
            } else {
                SkillToolDecision::Deny
            },
        );
    }
    *permission.decision.lock().await = Some(approved);
    permission.notify.notify_waiters();
    append_permission_resolved_event(
        state,
        permission.summary.session_id,
        permission.summary.permission_id,
        approved,
    )
    .await;
}

pub async fn take_pending_permission_for_individual(
    state: &ServerState,
    permission_id: &str,
) -> Option<PendingPermission> {
    let candidate = state
        .pending_permissions
        .lock()
        .await
        .get(permission_id)
        .cloned()?;
    let Some(correlation) = candidate.summary.batch.as_ref() else {
        return state.pending_permissions.lock().await.remove(permission_id);
    };
    let batch = state
        .pending_permission_batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&correlation.batch_id)
        .cloned();
    let Some(batch) = batch else {
        state.pending_permissions.lock().await.remove(permission_id);
        return None;
    };
    let batch_decision = batch.decision.lock().await;
    if batch_decision.is_some() {
        return None;
    }
    let permission = state.pending_permissions.lock().await.remove(permission_id);
    drop(batch_decision);
    permission
}

/// Deny one pending permission only when this caller wins removal from canonical pending state.
pub async fn cancel_pending_permission(state: &ServerState, permission_id: &str) -> bool {
    let permission = state.pending_permissions.lock().await.remove(permission_id);
    let Some(permission) = permission else {
        return false;
    };
    complete_pending_permission(state, permission, false, false).await;
    true
}

/// Deny and complete every pending permission for one cancelled session.
pub async fn cancel_pending_permissions_for_session(state: &ServerState, session_id: SessionId) {
    let batches = state
        .pending_permission_batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter(|batch| batch.session_id == session_id)
        .cloned()
        .collect::<Vec<_>>();
    for batch in batches {
        let mut decision = batch.decision.lock().await;
        if decision.is_none() {
            *decision = Some(false);
        }
    }

    let permissions = {
        let mut pending = state.pending_permissions.lock().await;
        let permission_ids = pending
            .iter()
            .filter(|(_, permission)| permission.summary.session_id == session_id)
            .map(|(permission_id, _)| permission_id.clone())
            .collect::<Vec<_>>();
        let mut permissions = Vec::with_capacity(permission_ids.len());
        for permission_id in permission_ids {
            if let Some(permission) = pending.remove(&permission_id) {
                permissions.push(permission);
            }
        }
        permissions
    };
    for permission in permissions {
        complete_pending_permission(state, permission, false, false).await;
    }
}

/// Resolve one complete pending permission batch without transport framing.
pub async fn resolve_permission_batch(
    state: &ServerState,
    batch_id: &str,
    approved: bool,
) -> usize {
    let Some(batch) = state
        .pending_permission_batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(batch_id)
        .cloned()
    else {
        return 0;
    };
    let mut batch_decision = batch.decision.lock().await;
    if batch_decision.is_some() {
        return 0;
    }
    *batch_decision = Some(approved);
    drop(batch_decision);

    let permissions = {
        let permission_ids = {
            let pending = state.pending_permissions.lock().await;
            pending
                .iter()
                .filter(|(_, permission)| {
                    permission
                        .summary
                        .batch
                        .as_ref()
                        .is_some_and(|batch| batch.batch_id == batch_id)
                })
                .map(|(permission_id, _)| permission_id.clone())
                .collect::<Vec<_>>()
        };
        let mut pending = state.pending_permissions.lock().await;
        let mut permissions = Vec::with_capacity(permission_ids.len());
        for permission_id in permission_ids {
            if let Some(permission) = pending.remove(&permission_id) {
                permissions.push(permission);
            }
        }
        permissions
    };
    let resolved = permissions.len();
    for permission in permissions {
        complete_pending_permission(state, permission, approved, false).await;
    }
    resolved
}

/// Validate and execute one complete server-owned tool exchange lifecycle.
///
/// Invocation identity is checked before registration, and absence of any compatible consumer is
/// surfaced without creating pending state.
///
/// # Errors
///
/// Returns a normalized failure when the exchange identity is already pending.
pub async fn execute_tool_exchange(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: &str,
    request: &bcode_session_models::ToolExchangeRequest,
    cancel_state: &TurnCancelState,
) -> Result<ToolExchangeResolution, String> {
    if request.invocation_id != tool_call_id {
        return Ok(ToolExchangeResolution::Failed {
            code: "invocation_id_mismatch".to_owned(),
            message: "exchange does not belong to the active tool invocation".to_owned(),
        });
    }
    if !has_exchange_consumer(state, request).await {
        return Ok(ToolExchangeResolution::NoCompatibleConsumer);
    }
    request_tool_exchange(state, session_id, request, cancel_state).await
}

/// Register, recheck consumer availability, and await one tool exchange terminal outcome.
///
/// # Errors
///
/// Returns a normalized failure when the exchange identity is already pending.
pub async fn request_tool_exchange(
    state: &ServerState,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
    cancel_state: &TurnCancelState,
) -> Result<ToolExchangeResolution, String> {
    let (resolution, notify) = register_pending_tool_exchange(state, session_id, request).await?;
    if !has_exchange_consumer(state, request).await {
        return Ok(abort_tool_exchange(
            state,
            &request.exchange_id,
            ToolExchangeResolution::ConsumerDetached,
        )
        .await);
    }
    Ok(wait_for_tool_exchange_resolution(
        state,
        &request.exchange_id,
        &resolution,
        &notify,
        cancel_state,
    )
    .await)
}

/// Return whether one connected client advertised a compatible exchange adapter.
pub async fn client_supports_exchange(
    state: &ServerState,
    client_id: ClientId,
    request: &bcode_session_models::ToolExchangeRequest,
) -> bool {
    state
        .client_runtime_contexts
        .lock()
        .await
        .get(&client_id)
        .is_some_and(|context| {
            context.interaction_adapters.iter().any(|adapter| {
                adapter.supports(&request.schema, request.schema_version)
                    && request.producer_id == adapter.producer_id
            })
        })
}

/// Return whether any connected client advertised a compatible exchange adapter.
pub async fn has_exchange_consumer(
    state: &ServerState,
    request: &bcode_session_models::ToolExchangeRequest,
) -> bool {
    state
        .client_runtime_contexts
        .lock()
        .await
        .values()
        .any(|context| {
            context.interaction_adapters.iter().any(|adapter| {
                adapter.supports(&request.schema, request.schema_version)
                    && request.producer_id == adapter.producer_id
            })
        })
}

/// Register one pending exchange and return its shared resolution state.
///
/// # Errors
///
/// Returns a normalized failure when the exchange identity is already pending.
pub async fn register_pending_tool_exchange(
    state: &ServerState,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
) -> Result<
    (
        Arc<Mutex<Option<ToolExchangeResolution>>>,
        Arc<tokio::sync::Notify>,
    ),
    String,
> {
    let pending = PendingToolExchange {
        summary: PendingToolExchangeSummary {
            session_id,
            request: request.clone(),
        },
        resolution: Arc::new(Mutex::new(None)),
        notify: Arc::new(tokio::sync::Notify::new()),
    };
    let resolution = Arc::clone(&pending.resolution);
    let notify = Arc::clone(&pending.notify);
    let mut exchanges = state.pending_tool_exchanges.lock().await;
    if exchanges.contains_key(&request.exchange_id) {
        return Err(format!(
            "duplicate interactive tool request id: {}",
            request.exchange_id
        ));
    }
    exchanges.insert(request.exchange_id.clone(), pending);
    drop(exchanges);
    Ok((resolution, notify))
}

/// Resolve pending exchanges that no longer have any compatible connected consumer.
pub async fn resolve_exchanges_without_consumers(state: &ServerState) {
    let contexts = state.client_runtime_contexts.lock().await;
    let mut pending = state.pending_tool_exchanges.lock().await;
    let entries = std::mem::take(&mut *pending);
    let (retained, detached): (BTreeMap<_, _>, BTreeMap<_, _>) =
        entries.into_iter().partition(|(_, exchange)| {
            contexts.values().any(|context| {
                context.interaction_adapters.iter().any(|adapter| {
                    adapter.producer_id == exchange.summary.request.producer_id
                        && adapter.supports(
                            &exchange.summary.request.schema,
                            exchange.summary.request.schema_version,
                        )
                })
            })
        });
    *pending = retained;
    drop(contexts);
    drop(pending);
    for exchange in detached.into_values() {
        *exchange.resolution.lock().await = Some(ToolExchangeResolution::ConsumerDetached);
        exchange.notify.notify_waiters();
    }
}

/// Return the current bounded pending tool-exchange summaries.
pub async fn list_pending_tool_exchanges(state: &ServerState) -> Vec<PendingToolExchangeSummary> {
    let mut exchanges = state
        .pending_tool_exchanges
        .lock()
        .await
        .values()
        .map(|request| request.summary.clone())
        .collect::<Vec<_>>();
    exchanges.sort_by(|left, right| left.request.exchange_id.cmp(&right.request.exchange_id));
    exchanges
}

/// Wait for one pending exchange resolution or abort it when the owning turn is cancelled.
pub async fn wait_for_tool_exchange_resolution(
    state: &ServerState,
    interaction_id: &str,
    resolution_slot: &Arc<Mutex<Option<ToolExchangeResolution>>>,
    notify: &Arc<tokio::sync::Notify>,
    cancel_state: &TurnCancelState,
) -> ToolExchangeResolution {
    loop {
        // Register before reading so a resolution committed between the read and the wait is
        // not lost: `notify_waiters` only wakes already-registered waiters.
        let notified = notify.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        let value = resolution_slot.lock().await.clone();
        if let Some(resolution) = value {
            return resolution;
        }
        tokio::select! {
            () = &mut notified => {}
            () = cancel_state.cancelled() => {
                return abort_tool_exchange(
                    state,
                    interaction_id,
                    ToolExchangeResolution::Cancelled,
                )
                .await;
            }
        }
    }
}

/// Remove one pending exchange because its owning workflow cancelled or lost its consumer.
pub async fn abort_tool_exchange(
    state: &ServerState,
    interaction_id: &str,
    resolution: ToolExchangeResolution,
) -> ToolExchangeResolution {
    state
        .pending_tool_exchanges
        .lock()
        .await
        .remove(interaction_id);
    resolution
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

const MAX_TOOL_EXCHANGE_RESOLUTION_BYTES: usize = 64 * 1024;

/// Decode one serialized client tool-exchange resolution without exposing serde failures.
pub fn decode_tool_exchange_resolution(
    resolution_json: serde_json::Value,
) -> Result<ToolExchangeResolution, ResolveToolExchangeError> {
    let encoded = serde_json::to_vec(&resolution_json)
        .map_err(|_| ResolveToolExchangeError::InvalidResolution)?;
    if encoded.len() > MAX_TOOL_EXCHANGE_RESOLUTION_BYTES {
        return Err(ResolveToolExchangeError::InvalidResolution);
    }
    let resolution = serde_json::from_value(resolution_json)
        .map_err(|_| ResolveToolExchangeError::InvalidResolution)?;
    match resolution {
        ToolExchangeResolution::Responded { .. } | ToolExchangeResolution::Cancelled => {
            Ok(resolution)
        }
        ToolExchangeResolution::TimedOut
        | ToolExchangeResolution::NoCompatibleConsumer
        | ToolExchangeResolution::ConsumerDetached
        | ToolExchangeResolution::Failed { .. } => Err(ResolveToolExchangeError::InvalidResolution),
    }
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
    if !client_supports_exchange(state, client_id, &pending.summary.request).await {
        return Err(ResolveToolExchangeError::IncompatibleConsumer);
    }
    Ok(complete_pending_tool_exchange(state, interaction_id, resolution).await)
}
