//! Daemon-owned ephemeral bridge lifecycle. Poll native queue briefly, release its exclusive
//! plugin lock, then execute ordinary tools. Never await a tool while holding native I/O locks.
#[cfg(test)]
mod tests;
use super::{ManagedHarnessError, ManagedToolBridge, ManagedToolEndpoint};
use crate::{ServerState, TurnCancelState, harness_dispatch::ManagedCaller};
use bcode_harness_models::{
    HarnessRequestKind, MANAGED_TOOL_BRIDGE_VERSION, MANAGED_TOOL_QUEUE_INTERFACE_ID,
    MAX_MANAGED_TOOL_MESSAGE_BYTES, MAX_MANAGED_TOOL_QUEUE, ManagedToolOperation,
    ManagedToolOutcome, ManagedToolQueueRequest, ManagedToolQueueResponse, ManagedToolQueueScope,
    ManagedToolRequest, ManagedToolResponse,
};
use bcode_ipc::harness::ManagedSessionCall;
use bcode_session_models::{SessionId, TurnExecutionOptions};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Default)]
pub struct ToolQueueHost {
    live: Mutex<BTreeMap<SessionId, Arc<LiveQueue>>>,
}
struct LiveQueue {
    principal: String,
    scope: ManagedToolQueueScope,
    cancellation: Arc<TurnCancelState>,
    endpoint: ManagedToolEndpoint,
}
// A Bind future may be dropped after native state changed but before its acknowledgement.
// Keep exact-scope cleanup armed across that await; cancellation never leaves a live SDK grant.
struct PendingBind {
    state: std::sync::Weak<ServerState>,
    plugin: String,
    scope: Option<ManagedToolQueueScope>,
    cancellation: Arc<TurnCancelState>,
}
impl PendingBind {
    async fn bind(
        state: &Arc<ServerState>,
        plugin: &str,
        scope: &ManagedToolQueueScope,
        cancellation: &Arc<TurnCancelState>,
    ) -> Result<Self, ManagedHarnessError> {
        let mut binding = Self {
            state: Arc::downgrade(state),
            plugin: plugin.to_owned(),
            scope: Some(scope.clone()),
            cancellation: Arc::clone(cancellation),
        };
        let bound = queue_call(
            state,
            plugin,
            &ManagedToolQueueRequest::Bind {
                scope: scope.clone(),
            },
        )
        .await;
        if !matches!(bound, Ok(ManagedToolQueueResponse::Bound)) {
            let category = match &bound {
                Ok(ManagedToolQueueResponse::Rejected) => "bind_rejected",
                Ok(_) => "bind_unexpected_response",
                Err(ManagedHarnessError::Deadline) => "bind_timeout",
                Err(ManagedHarnessError::Invalid) => "bind_invalid_response",
                Err(_) => "bind_unavailable",
            };
            tracing::warn!(target: "bcode_server::harness", category, "managed tool queue binding cleanup");
            // Native binding may have succeeded even if its acknowledgement failed.
            cancellation.close();
            let _ = queue_call(
                state,
                plugin,
                &ManagedToolQueueRequest::Revoke {
                    scope: scope.clone(),
                },
            )
            .await;
            binding.scope = None;
            return Err(bound.err().unwrap_or(ManagedHarnessError::Unavailable));
        }
        Ok(binding)
    }
}
impl Drop for PendingBind {
    fn drop(&mut self) {
        let Some(scope) = self.scope.take() else {
            return;
        };
        tracing::warn!(target: "bcode_server::harness", category = "bind_future_dropped", "managed tool queue binding cleanup");
        self.cancellation.close();
        let state = self.state.clone();
        let plugin = self.plugin.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(state) = state.upgrade() {
                    let _ = queue_call(&state, &plugin, &ManagedToolQueueRequest::Revoke { scope })
                        .await;
                }
            });
        }
    }
}
impl ToolQueueHost {
    /// Called only after normal admission and canonical retained-result exclusion.
    pub async fn install(
        &self,
        state: &Arc<ServerState>,
        caller: &ManagedCaller,
        call: &ManagedSessionCall,
    ) -> Result<Option<ManagedToolQueueScope>, ManagedHarnessError> {
        // Open has only Configured state here: no native prepared target exists yet.
        // The inert SDK server is installed at native launch; issue authority only on a
        // fresh authorized Submit, after ordinary Open canonically publishes its target.
        if !queue_eligible(state, caller, call) {
            return Ok(None);
        }
        let plugin = &call.harness.instance.plugin_id;
        let session = super::admission::target(call)?;
        let snapshot = state
            .sessions
            .session_external_harness_state(session)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::StaleOwner)?;
        let principal = match caller {
            ManagedCaller::Client(client) => format!("client:{client}"),
            ManagedCaller::Workflow {
                run_id,
                dispatch_identity,
                ..
            } => format!("workflow:{run_id}:{dispatch_identity}"),
            ManagedCaller::Invocation { .. } => return Ok(None),
        };
        let mut live = self.live.lock().await;
        if let Some(existing) = live.get(&session) {
            if existing.principal == principal
                && existing.scope.generation == snapshot.owner.generation
                && !existing.cancellation.is_cancelled()
                && endpoint_live(&existing.endpoint).await
            {
                return Ok(Some(existing.scope.clone())); // Exact retained grant remains authorized.
            }
            tracing::debug!(target: "bcode_server::harness", category = "grant_replacement", "managed tool queue revocation requested");
            existing.cancellation.close();
            // Exact revoke is bounded and never executes tool effects. Native SDK server stays
            // installed but unavailable until the new authenticated principal is bound.
            let _ = queue_call(
                state,
                plugin,
                &ManagedToolQueueRequest::Revoke {
                    scope: existing.scope.clone(),
                },
            )
            .await?;
        }
        live.remove(&session);
        live.retain(|_, entry| !entry.cancellation.is_cancelled());
        if live.len() >= 128 {
            return Err(ManagedHarnessError::Unavailable);
        }
        let ceiling = admitted_ceiling(state, caller, call).await?;
        let copy = serde_json::from_value(
            serde_json::to_value(call).map_err(|_| ManagedHarnessError::Invalid)?,
        )
        .map_err(|_| ManagedHarnessError::Invalid)?;
        let cancellation = Arc::new(TurnCancelState::default());
        let (endpoint, driver) = ManagedToolBridge::issue(
            Arc::clone(state),
            caller.clone(),
            copy,
            ceiling,
            Arc::clone(&cancellation),
        )
        .await
        .map_err(|_| ManagedHarnessError::StaleOwner)?;
        let scope = ManagedToolQueueScope {
            session_id: session.to_string(),
            generation: snapshot.owner.generation,
            bridge_id: uuid::Uuid::new_v4().to_string(),
        };
        let installed_scope = scope.clone();
        let mut binding = PendingBind::bind(state, plugin, &scope, &cancellation).await?;
        let entry = Arc::new(LiveQueue {
            principal,
            scope,
            cancellation,
            endpoint: endpoint.clone(),
        });
        live.insert(session, Arc::clone(&entry));
        tracing::debug!(target: "bcode_server::harness", category = "bind_adopted", "managed tool queue binding lifecycle");
        binding.scope = None;
        drop(binding);
        drop(live);
        let state = Arc::downgrade(state);
        let plugin = plugin.clone();
        tokio::spawn(async move {
            // Driver is polled concurrently with the queue pump, never under a plugin call.
            tokio::join!(
                Box::new(driver).serve(),
                pump(state, plugin, entry, endpoint)
            );
        });
        Ok(Some(installed_scope))
    }

    /// Revoke failed Submit authority only if its exact scope is still current.
    pub async fn revoke_exact(&self, state: &ServerState, scope: &ManagedToolQueueScope) {
        let Ok(session) = scope.session_id.parse::<SessionId>() else {
            return;
        };
        let retired = {
            let mut live = self.live.lock().await;
            if !live
                .get(&session)
                .is_some_and(|entry| entry.scope == *scope)
            {
                return;
            }
            live.remove(&session)
        };
        if let Some(live) = retired {
            tracing::warn!(target: "bcode_server::harness", category = "failed_submit_exact_scope", "managed tool queue revocation requested");
            live.cancellation.close();
            if let Ok(Some(snapshot)) = state.sessions.session_external_harness_state(session).await
            {
                let _ = queue_call(
                    state,
                    &snapshot.instance.plugin_id,
                    &ManagedToolQueueRequest::Revoke {
                        scope: live.scope.clone(),
                    },
                )
                .await;
            }
        }
    }

    pub async fn revoke(&self, state: &ServerState, session: SessionId) {
        let retired = self.live.lock().await.remove(&session);
        if let Some(live) = retired {
            tracing::warn!(target: "bcode_server::harness", category = "explicit_session_revoke", "managed tool queue revocation requested");
            live.cancellation.close();
            // Revoke at native queue before a later principal can bind. The old pump's delayed
            // revoke has an old bridge ID and cannot revoke the new queue.
            if let Ok(Some(snapshot)) = state.sessions.session_external_harness_state(session).await
            {
                let _ = queue_call(
                    state,
                    &snapshot.instance.plugin_id,
                    &ManagedToolQueueRequest::Revoke {
                        scope: live.scope.clone(),
                    },
                )
                .await;
            }
        }
    }
    pub async fn shutdown(&self) {
        let retired = std::mem::take(&mut *self.live.lock().await);
        for live in retired.into_values() {
            tracing::debug!(target: "bcode_server::harness", category = "host_shutdown", "managed tool queue revocation requested");
            live.cancellation.close();
        }
    }
}

fn queue_eligible(state: &ServerState, caller: &ManagedCaller, call: &ManagedSessionCall) -> bool {
    matches!(
        caller,
        ManagedCaller::Client(_) | ManagedCaller::Workflow { .. }
    ) && matches!(call.harness.kind, HarnessRequestKind::Submit { .. })
        && state
            .plugins
            .registry()
            .manifests()
            .get(&call.harness.instance.plugin_id)
            .is_some_and(|manifest| {
                manifest
                    .services
                    .iter()
                    .any(|service| service.interface_id == MANAGED_TOOL_QUEUE_INTERFACE_ID)
            })
}

async fn admitted_ceiling(
    state: &ServerState,
    caller: &ManagedCaller,
    call: &ManagedSessionCall,
) -> Result<TurnExecutionOptions, ManagedHarnessError> {
    match caller {
        ManagedCaller::Workflow {
            run_id,
            dispatch_identity,
            session_id,
            plugin_id,
        } => crate::harness_workflow::managed_tool_ceiling(
            state,
            run_id,
            dispatch_identity,
            *session_id,
            plugin_id,
            call,
        )
        .await
        .map_err(|_| ManagedHarnessError::StaleOwner),
        ManagedCaller::Client(_) => Ok(TurnExecutionOptions::default()),
        ManagedCaller::Invocation { .. } => Err(ManagedHarnessError::Unsupported),
    }
}

async fn queue_call(
    state: &ServerState,
    plugin: &str,
    request: &ManagedToolQueueRequest,
) -> Result<ManagedToolQueueResponse, ManagedHarnessError> {
    request
        .validate()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let payload = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        state.plugins.invoke_service_scoped(
            plugin,
            MANAGED_TOOL_QUEUE_INTERFACE_ID,
            request.operation(),
            payload,
            bcode_plugin::PluginInvocationScope::session(request.scope().session_id.clone()),
        ),
    )
    .await;
    let operation = request.operation();
    let category = match &result {
        Err(_) => Some("timeout"),
        Ok(Err(_)) => Some("invocation_unavailable"),
        Ok(Ok(response)) if response.error.is_some() => Some("service_error"),
        Ok(Ok(response)) if response.payload.len() > MAX_MANAGED_TOOL_MESSAGE_BYTES => {
            Some("oversized_response")
        }
        _ => None,
    };
    if let Some(category) = category {
        tracing::warn!(target: "bcode_server::harness", operation, category, "managed tool queue service failed");
    }
    let result = result
        .map_err(|_| ManagedHarnessError::Deadline)?
        .map_err(|_| ManagedHarnessError::Unavailable)?;
    if result.payload.len() > MAX_MANAGED_TOOL_MESSAGE_BYTES || result.error.is_some() {
        return Err(ManagedHarnessError::Invalid);
    }
    serde_json::from_slice(&result.payload).map_err(|_| {
        tracing::warn!(target: "bcode_server::harness", operation, category = "invalid_response_shape", "managed tool queue service failed");
        ManagedHarnessError::Invalid
    })
}

async fn endpoint_live(endpoint: &ManagedToolEndpoint) -> bool {
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        endpoint.request(ManagedToolRequest {
            version: MANAGED_TOOL_BRIDGE_VERSION,
            request_id: "host-liveness".into(),
            operation: ManagedToolOperation::Inspect {
                invocation_id: "host-liveness-probe".into(),
            },
        }),
    )
    .await;
    let category = match result {
        Ok(Ok(ManagedToolResponse {
            outcome: ManagedToolOutcome::Missing,
            ..
        })) => return true,
        Ok(Ok(ManagedToolResponse {
            outcome: ManagedToolOutcome::Denied,
            ..
        })) => "denied",
        Ok(Ok(ManagedToolResponse {
            outcome: ManagedToolOutcome::Rejected,
            ..
        })) => "rejected",
        Ok(Ok(_)) => "unexpected_outcome",
        Ok(Err(_)) => "endpoint_unavailable",
        Err(_) => "timeout",
    };
    tracing::debug!(target: "bcode_server::harness", category, "managed tool queue liveness failed");
    false
}

async fn pump(
    state: std::sync::Weak<ServerState>,
    plugin: String,
    live: Arc<LiveQueue>,
    endpoint: ManagedToolEndpoint,
) {
    use futures::StreamExt;
    type Pending = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ManagedToolResponse, String>> + Send>,
    >;
    let mut pending = futures::stream::FuturesUnordered::<Pending>::new();
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    let stopped = loop {
        tokio::select! {
            biased;
            () = live.cancellation.cancelled() => break "cancelled",
            Some(result) = pending.next(), if !pending.is_empty() => {
                let Some(state) = state.upgrade() else { break "state_unavailable"; };
                let Ok(response) = result else { break "endpoint_request_failed"; };
                if !matches!(queue_call(&state, &plugin, &ManagedToolQueueRequest::Complete { scope: live.scope.clone(), response }).await,
                    Ok(ManagedToolQueueResponse::Completed)) { break "completion_rejected"; }
            }
            _ = interval.tick() => {
                let Some(state) = state.upgrade() else { break "state_unavailable"; };
                if state.shutdown_requested.load(std::sync::atomic::Ordering::SeqCst) { break "shutdown"; }
                // Probe the endpoint each tick: durable generation, caller bootstrap and workflow
                // receipt/cancellation are revalidated even when native has no pending requests.
                if !endpoint_live(&endpoint).await { break "endpoint_not_live"; }
                if pending.len() >= MAX_MANAGED_TOOL_QUEUE { continue; }
                let Ok(ManagedToolQueueResponse::Requests { requests }) = queue_call(&state, &plugin,
                    &ManagedToolQueueRequest::Drain { scope: live.scope.clone() }).await else { break "drain_rejected"; };
                if requests.len() > MAX_MANAGED_TOOL_QUEUE || pending.len() + requests.len() > MAX_MANAGED_TOOL_QUEUE { break "capacity"; }
                for request in requests {
                    let endpoint = endpoint.clone();
                    pending.push(Box::pin(async move { endpoint.request(request).await }));
                }
            }
        }
    };
    tracing::debug!(target: "bcode_server::harness", category = stopped, "managed tool queue pump stopped");
    live.cancellation.close();
    // Dropping receivers is not settlement or replay authority; canonical started claims remain.
    drop(pending);
    if let Some(state) = state.upgrade() {
        let _ = queue_call(
            &state,
            &plugin,
            &ManagedToolQueueRequest::Revoke {
                scope: live.scope.clone(),
            },
        )
        .await;
    }
}
