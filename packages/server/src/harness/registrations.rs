//! Native registration application operations; storage and merge policy remain plugin-owned.

use super::ManagedHarnessError;
use bcode_harness_models::{RegistrationResolution, RegistrationSnapshot};
use bcode_plugin::{PluginInvocationScope, PluginServiceConfigView};
use std::{collections::BTreeMap, sync::Arc};

const PLUGIN: &str = "bcode.harness";
const INTERFACE: &str = "bcode.harness.configuration/v1";

pub(super) async fn call<T: serde::de::DeserializeOwned>(
    view: &PluginServiceConfigView,
    operation: &str,
    request: &(impl serde::Serialize + Sync),
) -> Result<T, ManagedHarnessError> {
    let payload = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
    let response = view
        .invoke_service_with_bridge_scoped(
            PLUGIN,
            INTERFACE,
            operation,
            payload,
            PluginInvocationScope::Global,
            None,
        )
        .await
        .map_err(|_| ManagedHarnessError::Unavailable)?;
    if response.error.is_some() {
        return Err(ManagedHarnessError::Conflict);
    }
    serde_json::from_slice(&response.payload).map_err(|_| ManagedHarnessError::Invalid)
}

pub async fn resolution(
    view: &PluginServiceConfigView,
) -> Result<RegistrationResolution, ManagedHarnessError> {
    let configurations = view.configuration_values();
    call(
        view,
        "registrations_resolve",
        &bcode_harness_models::RegistrationResolveRequest {
            adapter_configurations: configurations
                .into_iter()
                .filter(|(id, _)| id != PLUGIN)
                .collect(),
        },
    )
    .await
}

pub(super) async fn effective(
    view: PluginServiceConfigView,
) -> Result<PluginServiceConfigView, ManagedHarnessError> {
    if !view.configuration_values().contains_key(PLUGIN) {
        return Ok(view);
    }
    let resolved = resolution(&view).await?;
    let admitted = view.configuration_values();
    // Resolution cannot enable plugins absent from the host-admitted runtime.
    let values: BTreeMap<_, _> = resolved
        .configurations
        .into_iter()
        .filter(|(id, _)| admitted.contains_key(id))
        .collect();
    view.with_configuration_values(values)
        .map_err(|_| ManagedHarnessError::Invalid)
}

pub async fn snapshot(
    view: &PluginServiceConfigView,
) -> Result<RegistrationSnapshot, ManagedHarnessError> {
    call(view, "registrations_snapshot", &serde_json::json!({})).await
}

pub async fn commit(
    state: &Arc<crate::ServerState>,
    client: bcode_session_models::ClientId,
    query: bcode_ipc::harness::ManagedRegistrationCommitRequest,
) -> Result<RegistrationSnapshot, ManagedHarnessError> {
    let context = state
        .client_runtime_context(client)
        .await
        .ok_or(ManagedHarnessError::StaleOwner)?;
    let sources = context
        .config_launch
        .as_ref()
        .and_then(|launch| launch.recovery_sources.as_ref())
        .ok_or(ManagedHarnessError::Invalid)?;
    let supplied = context
        .effective_config_toml
        .as_deref()
        .ok_or(ManagedHarnessError::Invalid)?;
    let config = bcode_config::decode_effective_config(supplied)
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let authority = state
        .startup_config_authority
        .as_ref()
        .ok_or(ManagedHarnessError::Invalid)?;
    authority
        .verify_workspace(&config, sources)
        .map_err(super::configuration_views::source_mismatch)?;
    let view = super::configuration_views::declarative_for_client(state, client).await?;
    let native =
        super::transport::HarnessPluginTransport::with_view(Arc::clone(state), view.clone())
            .prepare_candidate(&query.prepare)
            .await?;
    native
        .validate_for(&query.prepare, &query.prepare.candidate.plugin_id)
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let bcode_harness_models::HarnessPrepareCandidateResponseKind::Prepared(prepared) = native.kind
    else {
        return Err(ManagedHarnessError::Unavailable);
    };
    if prepared.instance != query.expected_registration.instance
        || prepared.adapter_settings != query.expected_registration.adapter_settings
    {
        return Err(ManagedHarnessError::Conflict);
    }
    let previous = view.configuration_values();
    drop(view);
    // Re-check the authenticated source immediately before commit. Neither UI payload nor
    // preparation grants authority to substitute a different configuration or installation.
    authority
        .verify_workspace(&config, sources)
        .map_err(super::configuration_views::source_mismatch)?;
    let current = super::configuration_views::declarative_for_client(state, client).await?;
    if current.configuration_values() != previous {
        return Err(ManagedHarnessError::Conflict);
    }
    let result = call(
        &current,
        "registrations_commit",
        &bcode_harness_models::RegistrationCommit {
            expected_revision: query.expected_revision,
            registration: query.expected_registration,
        },
    )
    .await;
    drop(current);
    result
}
