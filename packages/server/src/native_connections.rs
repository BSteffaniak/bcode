//! Native connection application plumbing. Caller authorization precedes this module.
//! Canonical state is always the serving server's user state root, never plugin storage.
#[cfg(test)]
#[path = "native_connection_store_tests.rs"]
mod store_tests;
#[cfg(test)]
mod tests;
use crate::ServerState;
use bcode_harness_models::native_connections::{
    MAX_NATIVE_CONNECTIONS, NATIVE_PREPARATION_INTERFACE, NativeCandidate, NativeConnectionError,
    NativeConnectionLookupRequest, NativeConnectionLookupResponse, NativeConnectionRequest,
    NativeConnectionResponse, NativePreparationResponse, NativePrepareRequest,
};
use bcode_provider_auth::native_connections::NativeConnectionStore;

fn enabled(state: &ServerState, plugin_id: &str) -> Result<(), NativeConnectionError> {
    let manifests = state.plugins.registry();
    let manifest = manifests
        .manifests()
        .get(plugin_id)
        .ok_or(NativeConnectionError::Unavailable)?;
    if !manifest
        .services
        .iter()
        .any(|service| service.interface_id == NATIVE_PREPARATION_INTERFACE)
    {
        return Err(NativeConnectionError::Unavailable);
    }
    bcode_plugin::harness::harness_contribution(manifest)
        .map_err(|_| NativeConnectionError::Unavailable)?;
    Ok(())
}
async fn discover(
    state: &ServerState,
    plugin_id: &str,
) -> Result<Vec<NativeCandidate>, NativeConnectionError> {
    enabled(state, plugin_id)?;
    let response: NativePreparationResponse = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.plugins.invoke_service_json(
            plugin_id,
            NATIVE_PREPARATION_INTERFACE,
            "discover",
            &serde_json::json!({}),
        ),
    )
    .await
    .map_err(|_| NativeConnectionError::Unavailable)?
    .map_err(|_| NativeConnectionError::Unavailable)?;
    match response {
        NativePreparationResponse::Candidates { candidates }
            if candidates.len() <= MAX_NATIVE_CONNECTIONS =>
        {
            let mut ids = std::collections::BTreeSet::new();
            if candidates.iter().any(|candidate| {
                candidate.plugin_id != plugin_id
                    || candidate.validate().is_err()
                    || !ids.insert(&candidate.candidate_id)
            }) {
                return Err(NativeConnectionError::InvalidRequest);
            }
            Ok(candidates)
        }
        NativePreparationResponse::Failed { error } => Err(error),
        _ => Err(NativeConnectionError::InvalidRequest),
    }
}
/// Execute an already-authorized application operation. Approval means explicit broad-local consent.
/// Preparation may run only bounded native compatibility/status probes, never generation/login.
/// # Errors
/// Returned failures are normalized and preserve existing state on unsupported input.
pub async fn execute(
    state: &ServerState,
    request: NativeConnectionRequest,
) -> NativeConnectionResponse {
    match execute_inner(state, request).await {
        Ok(response) => response,
        Err(error) => NativeConnectionResponse::Failed { error },
    }
}
fn approval_time() -> Result<u64, NativeConnectionError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| NativeConnectionError::Unavailable)
}

async fn store_operation<T: Send + 'static>(
    store: &NativeConnectionStore,
    operation: impl FnOnce(NativeConnectionStore) -> Result<T, NativeConnectionError> + Send + 'static,
) -> Result<T, NativeConnectionError> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || operation(store))
        .await
        .map_err(|_| NativeConnectionError::StateUnavailable)?
}

async fn execute_inner(
    state: &ServerState,
    request: NativeConnectionRequest,
) -> Result<NativeConnectionResponse, NativeConnectionError> {
    request
        .validate()
        .map_err(|_| NativeConnectionError::InvalidRequest)?;
    let store = NativeConnectionStore::new(&state.state_root);
    match request {
        NativeConnectionRequest::Discover { plugin_id } => {
            Ok(NativeConnectionResponse::Candidates {
                candidates: discover(state, &plugin_id).await?,
            })
        }
        NativeConnectionRequest::List {} => Ok(NativeConnectionResponse::Records {
            records: store_operation(&store, |store| store.list()).await?,
        }),
        NativeConnectionRequest::Approve {
            candidate,
            expected_revision,
            mutation_id,
        } => {
            // Never accept caller-supplied paths or provenance as authoritative.
            if !discover(state, &candidate.plugin_id)
                .await?
                .contains(&candidate)
            {
                return Err(NativeConnectionError::Conflict);
            }
            Ok(NativeConnectionResponse::Record {
                record: Box::new(
                    store_operation(&store, move |store| {
                        store.approve(
                            &candidate,
                            expected_revision,
                            &mutation_id,
                            approval_time()?,
                        )
                    })
                    .await?,
                ),
            })
        }
        NativeConnectionRequest::Prepare {
            candidate_id,
            expected_revision,
            mutation_id,
        } => prepare_connection(state, &store, candidate_id, expected_revision, mutation_id).await,
        NativeConnectionRequest::Revoke {
            candidate_id,
            expected_revision,
            mutation_id,
        } => Ok(NativeConnectionResponse::Record {
            record: Box::new(
                store_operation(&store, move |store| {
                    store.revoke(&candidate_id, expected_revision, &mutation_id)
                })
                .await?,
            ),
        }),
    }
}
async fn prepare_connection(
    state: &ServerState,
    store: &NativeConnectionStore,
    candidate_id: String,
    expected_revision: u64,
    mutation_id: String,
) -> Result<NativeConnectionResponse, NativeConnectionError> {
    let record = store_operation(store, |store| store.list())
        .await?
        .into_iter()
        .find(|record| record.candidate.candidate_id == candidate_id && record.approved)
        .ok_or(NativeConnectionError::NotApproved)?;
    // Lost publication acknowledgement is retry-safe without running another probe.
    if record.revision != expected_revision {
        if let Some(prepared) = record.prepared {
            return Ok(NativeConnectionResponse::Record {
                record: Box::new(
                    store_operation(store, move |store| {
                        store.publish(&prepared, expected_revision, &mutation_id)
                    })
                    .await?,
                ),
            });
        }
        return Err(NativeConnectionError::Conflict);
    }
    enabled(state, &record.candidate.plugin_id)?;
    let response: NativePreparationResponse = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.plugins.invoke_service_json(
            &record.candidate.plugin_id,
            NATIVE_PREPARATION_INTERFACE,
            "prepare",
            &NativePrepareRequest {
                schema_version: 1,
                request_id: mutation_id.clone(),
                candidate: record.candidate.clone(),
            },
        ),
    )
    .await
    .map_err(|_| NativeConnectionError::Unavailable)?
    .map_err(|_| NativeConnectionError::Unavailable)?;
    match response {
        NativePreparationResponse::Prepared {
            request_id,
            connection,
        } if request_id == mutation_id && connection.candidate == record.candidate => {
            enabled(state, &record.candidate.plugin_id)?;
            Ok(NativeConnectionResponse::Record {
                record: Box::new(
                    store_operation(store, move |store| {
                        store.publish(&connection, expected_revision, &mutation_id)
                    })
                    .await?,
                ),
            })
        }
        NativePreparationResponse::Failed { error } => Err(error),
        _ => Err(NativeConnectionError::InvalidRequest),
    }
}
/// Read-only bridge implementation. `caller_plugin_id` is host-authenticated, not payload data.
/// Catalog callers must be the enabled generic catalog plugin; native plugins resolve only themselves.
/// # Errors
/// Rejects wrong ownership, unavailable state and disabled native declarations.
pub fn lookup(
    state: &ServerState,
    caller_plugin_id: &str,
    request: NativeConnectionLookupRequest,
) -> Result<NativeConnectionLookupResponse, NativeConnectionError> {
    let records = NativeConnectionStore::new(&state.state_root).list()?;
    filter_lookup(state, caller_plugin_id, request, records)
}

fn filter_lookup(
    state: &ServerState,
    caller_plugin_id: &str,
    request: NativeConnectionLookupRequest,
    records: Vec<bcode_harness_models::native_connections::NativeConnectionRecord>,
) -> Result<NativeConnectionLookupResponse, NativeConnectionError> {
    let records = match request {
        NativeConnectionLookupRequest::Catalog {}
            if caller_plugin_id == "bcode.harness"
                && state
                    .plugins
                    .registry()
                    .manifests()
                    .contains_key(caller_plugin_id) =>
        {
            records
                .into_iter()
                .filter(|record| record.approved && record.prepared.is_some())
                .filter(|record| enabled(state, &record.candidate.plugin_id).is_ok())
                .collect()
        }
        NativeConnectionLookupRequest::Resolve { instance_id } => {
            enabled(state, caller_plugin_id)?;
            let record = records
                .into_iter()
                .find(|record| {
                    record.approved
                        && record.candidate.plugin_id == caller_plugin_id
                        && record
                            .prepared
                            .as_ref()
                            .is_some_and(|prepared| prepared.instance.instance_id == instance_id)
                })
                .ok_or(NativeConnectionError::NotApproved)?;
            vec![record]
        }
        NativeConnectionLookupRequest::Catalog {} => {
            return Err(NativeConnectionError::Unavailable);
        }
    };
    Ok(NativeConnectionLookupResponse { records })
}

/// Async host bridge lookup; durable filesystem work runs outside executor threads.
///
/// # Errors
/// Rejects malformed lookup envelopes, unavailable consent, and foreign ownership.
pub async fn resolve_bridge_async(
    state: &ServerState,
    caller: &str,
    request: &bcode_tool::ToolInvocationServiceRequest,
) -> Result<serde_json::Value, String> {
    let request = decode_lookup_bridge(request)?;
    let store = NativeConnectionStore::new(&state.state_root);
    let records = store_operation(&store, |store| store.list())
        .await
        .map_err(|_| "native connection unavailable".to_owned())?;
    let response = filter_lookup(state, caller, request, records)
        .map_err(|_| "native connection unavailable".to_owned())?;
    serde_json::to_value(response).map_err(|_| "native connection unavailable".to_owned())
}

fn decode_lookup_bridge(
    request: &bcode_tool::ToolInvocationServiceRequest,
) -> Result<NativeConnectionLookupRequest, String> {
    use bcode_harness_models::native_connections::{
        MAX_NATIVE_CONNECTION_BYTES, NATIVE_CONNECTION_LOOKUP_INTERFACE,
    };
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("lookup budget exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let invalid = || "invalid native connection lookup".to_owned();
    let valid_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 256
            && value.trim() == value
            && !value.chars().any(char::is_control)
    };
    if request.interface_id != NATIVE_CONNECTION_LOOKUP_INTERFACE
        || request.operation != "lookup"
        || request.route_id.is_some()
        || !valid_id(&request.invocation_id)
        || !valid_id(&request.request_id)
    {
        return Err(invalid());
    }
    serde_json::to_writer(Budget(MAX_NATIVE_CONNECTION_BYTES), &request.payload)
        .map_err(|_| invalid())?;
    let lookup: NativeConnectionLookupRequest =
        serde_json::from_value(request.payload.clone()).map_err(|_| invalid())?;
    if let NativeConnectionLookupRequest::Resolve { instance_id } = &lookup
        && !valid_id(instance_id)
    {
        return Err(invalid());
    }
    Ok(lookup)
}

/// Construct a lookup bridge bound to the exact host-admitted managed instance.
/// Each lookup re-reads consent/publication; it cannot substitute another instance.
#[must_use]
pub fn bridge_for_instance(
    state: std::sync::Arc<ServerState>,
    instance: bcode_harness_models::HarnessInstance,
) -> bcode_plugin::PluginInvocationBridge {
    lookup_bridge(state, instance.plugin_id.clone(), Some(instance))
}

fn lookup_bridge(
    state: std::sync::Arc<ServerState>,
    caller_plugin_id: String,
    expected: Option<bcode_harness_models::HarnessInstance>,
) -> bcode_plugin::PluginInvocationBridge {
    use bcode_harness_models::native_connections::NATIVE_CONNECTION_LOOKUP_INTERFACE;
    use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse};
    use bcode_tool::ToolInvocationServiceResolution;
    bcode_plugin::PluginInvocationBridge::new(move |request, cancellation| {
        if cancellation.is_cancelled() {
            return Err("native connection lookup cancelled".into());
        }
        let ServiceBridgeRequest::InvokeService(request) = request else {
            return Err("unsupported native connection bridge operation".into());
        };
        if request.interface_id != NATIVE_CONNECTION_LOOKUP_INTERFACE
            || request.operation != "lookup"
            || request.route_id.is_some()
        {
            return Ok(ServiceBridgeResponse::Service(
                ToolInvocationServiceResolution::Unsupported,
            ));
        }
        let unavailable = || "native connection lookup unavailable".to_owned();
        let request = decode_lookup_bridge(&request)?;
        if let Some(instance) = &expected {
            match &request {
                NativeConnectionLookupRequest::Resolve { instance_id }
                    if instance_id == &instance.instance_id => {}
                _ => return Err(unavailable()),
            }
        }
        let response = lookup(&state, &caller_plugin_id, request).map_err(|_| unavailable())?;
        if let Some(instance) = &expected
            && (response.records.len() != 1
                || response.records[0]
                    .prepared
                    .as_ref()
                    .is_none_or(|prepared| prepared.instance != *instance))
        {
            return Err(unavailable());
        }
        if cancellation.is_cancelled() {
            return Err("native connection lookup cancelled".into());
        }
        let payload = serde_json::to_value(response).map_err(|_| unavailable())?;
        Ok(ServiceBridgeResponse::Service(
            ToolInvocationServiceResolution::Responded { payload },
        ))
    })
}
