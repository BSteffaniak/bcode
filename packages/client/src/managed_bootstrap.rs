//! Provider-independent bootstrap with a sealed managed-only method surface.
//!
//! No raw connection or inner model client is exposed. The daemon still owns authorization;
//! this facade prevents deferred model credentials from becoming an ordinary model fallback.
mod auth_flow;
pub use auth_flow::ManagedSessionAuthFlow;
mod attachment;
#[cfg(test)]
mod execution_tests;
#[cfg(test)]
mod setup_tests;
pub use attachment::{ManagedSessionCommandHandle, ManagedSessionConnection};

use crate::{BcodeClient, ClientError, ClientRuntimeContext, PluginServiceResponse};
use bcode_ipc::harness::{
    MANAGED_SESSION_INTERFACE_ID, ManagedSessionOperation, ManagedSessionRequest,
};
use serde::Deserialize;
use std::path::PathBuf;

const CONFIGURATION_INTERFACE: &str = "bcode.harness.configuration/v1";
const MAX_CONFIGURATION_REQUEST: usize = 4096;
// Matches the generic configuration service's bounded, nonsecret setup proposal budget.
const MAX_SETUP_REQUEST: usize = 256 * 1024;

/// Managed-only client retaining the normal config, endpoint, state-root and handshake pipeline.
///
/// This client cannot be converted to a model client or raw connection. Construct a normal
/// `BcodeClient` to materialize model authentication before ordinary model operations.
///
/// ```compile_fail
/// let client = bcode_client::ManagedSessionClient::default_endpoint();
/// // Deferred authentication never exposes an ordinary model connection.
/// let connection = client.connect("ordinary-model-caller");
/// ```
pub struct ManagedSessionClient {
    inner: BcodeClient,
}

impl ManagedSessionClient {
    /// Load the complete current configuration while deferring only model credential materialization.
    /// Configuration/context/profile/auth metadata failures remain connection-blocking errors.
    #[must_use]
    pub fn default_endpoint() -> Self {
        let context = (|| {
            let (config, launch) = crate::load_current_client_config()?;
            let mut context = configured_context(&config, crate::current_working_directory())?;
            context.config_launch = Some(Box::new(launch));
            Ok(context)
        })();
        Self {
            inner: BcodeClient::default_endpoint_with_context(context),
        }
    }

    /// Discover terminal-auth metadata for one explicitly selected canonical session/instance.
    ///
    /// Uses the existing session command binding and exact plugin service declaration selector.
    /// Metadata is not readiness or permission; this performs no login, attachment or native launch.
    /// Consumes this client, like `session_auth_flow`; use a separately configured managed client
    /// when opening the subsequent sealed auth flow.
    /// # Errors
    /// Returns canonical binding, explicit instance mismatch, discovery or ambiguous-service errors.
    pub async fn session_terminal_auth_interface(
        self,
        session_id: bcode_session_models::SessionId,
        instance_id: &str,
    ) -> Result<Option<String>, ClientError> {
        let commands = ManagedSessionCommandHandle {
            client: std::sync::Arc::new(self),
            session_id,
            command_connection: std::sync::Arc::default(),
        };
        let binding = commands.binding().await?;
        if binding.instance.instance_id != instance_id {
            return Err(ClientError::Protocol(
                "terminal auth metadata requires the exact canonical managed instance".into(),
            ));
        }
        commands.terminal_auth_interface().await
    }

    /// Revalidate a selected installation and obtain a plugin-owned configuration proposal.
    /// This performs no configuration commit, login or session creation.
    /// # Errors
    /// Rejects changed candidates, unavailable services and uncorrelated native proposals.
    pub async fn prepare_candidate(
        &self,
        request: bcode_harness_models::HarnessPrepareCandidateRequest,
    ) -> Result<bcode_harness_models::HarnessPrepareCandidateResponse, ClientError> {
        request
            .validate()
            .map_err(|_| ClientError::Protocol("invalid candidate preparation".into()))?;
        let response: bcode_harness_models::HarnessPrepareCandidateResponse = self
            .execution_request(ManagedSessionOperation::PrepareCandidate, &request)
            .await?;
        response
            .validate_for(&request, &request.candidate.plugin_id)
            .map_err(|_| ClientError::Protocol("invalid candidate preparation response".into()))?;
        Ok(response)
    }

    /// Inspect enabled plugins' native installation candidates without instance configuration.
    /// No executable paths, environment overrides, login or provider credentials are accepted.
    /// # Errors
    /// Rejects invalid correlation, excessive fanout and malformed plugin-owned candidates.
    pub async fn discover_candidates(
        &self,
        request: bcode_ipc::harness::ManagedCandidatesRequest,
    ) -> Result<bcode_ipc::harness::ManagedCandidatesResponse, ClientError> {
        request
            .validate()
            .map_err(|message| ClientError::Protocol(message.into()))?;
        let response: bcode_ipc::harness::ManagedCandidatesResponse = self
            .execution_request(ManagedSessionOperation::DiscoverCandidates, &request)
            .await?;
        if response.request_id != request.request_id || response.plugins.len() > 16 {
            return Err(ClientError::Protocol(
                "invalid candidate discovery response".into(),
            ));
        }
        let native = bcode_harness_models::HarnessCandidatesRequest {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: request.request_id,
        };
        let mut plugins = std::collections::BTreeSet::new();
        for plugin in &response.plugins {
            if !plugins.insert(&plugin.plugin_id)
                || plugin.validate_for(&native, &plugin.plugin_id).is_err()
            {
                return Err(ClientError::Protocol(
                    "invalid candidate producer or correlation".into(),
                ));
            }
        }
        Ok(response)
    }

    /// Discover exact execution members without materializing an unrelated provider account.
    /// Native advertisements retain unknown entitlement/readiness unless independently proven.
    /// # Errors
    /// Rejects invalid requests, mismatched responses, transport and application failures.
    pub async fn discover_execution_catalog(
        &self,
        request: bcode_ipc::harness::ManagedExecutionCatalogRequest,
    ) -> Result<bcode_ipc::harness::ManagedExecutionCatalog, ClientError> {
        request
            .validate()
            .map_err(|message| ClientError::Protocol(message.into()))?;
        let response: bcode_ipc::harness::ManagedExecutionCatalog = self
            .execution_request(ManagedSessionOperation::ExecutionCatalog, &request)
            .await?;
        if response.request_id != request.request_id
            || response.models.len() > 16 * bcode_harness_models::MAX_HARNESS_MODELS + 4096
            || response.issues.len() > 17
        {
            return Err(ClientError::Protocol(
                "invalid execution catalog response".into(),
            ));
        }
        Ok(response)
    }

    /// Start an exact selected managed session through canonical host admission only.
    /// No legacy Open, provider fallback or local native invocation is attempted on failure.
    /// # Errors
    /// Returns unsupported while selected-launch persistence is unpublished; also rejects
    /// invalid requests, unavailable readiness and uncorrelated responses.
    pub async fn start_selected_session(
        &self,
        request: bcode_ipc::harness::ManagedSelectedStartRequest,
    ) -> Result<bcode_ipc::harness::ManagedSelectedStartResponse, ClientError> {
        request
            .validate()
            .map_err(|message| ClientError::Protocol(message.into()))?;
        let response: bcode_ipc::harness::ManagedSelectedStartResponse = self
            .execution_request(ManagedSessionOperation::StartSelected, &request)
            .await?;
        let native = bcode_harness_models::HarnessModelsRequest {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: request.open.open_id.clone(),
            instance: request.instance.clone(),
            kind: bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                open: Box::new(request.open),
                selection: bcode_harness_models::HarnessModelSelection {
                    model_id: request.selection.model_id.clone(),
                },
            },
        };
        response
            .response
            .validate_for(&native)
            .map_err(|_| ClientError::Protocol("invalid selected start response".into()))?;
        if response.selection.requested.model_id != request.selection.model_id
            || response.selection.requested.route != request.selection.route
            || response.selection.requested.account_reference != request.selection.account_reference
            || response.selection.target.route
                != request
                    .selection
                    .route
                    .clone()
                    .ok_or_else(|| ClientError::Protocol("selected route is missing".into()))?
            || response.selection.target.model.model_id != request.selection.model_id
            || matches!(
                &response.response.kind,
                bcode_harness_models::HarnessModelsResponseKind::Failure(_)
            )
        {
            return Err(ClientError::Protocol(
                "selected start response changed selection".into(),
            ));
        }
        if let bcode_harness_models::HarnessModelsResponseKind::Selected { evidence, .. } =
            &response.response.kind
            && response.selection.target.confirmed_model_id != evidence.effective_model_id
        {
            return Err(ClientError::Protocol(
                "selected response changed effective-model evidence".into(),
            ));
        }
        let kind_matches = matches!(
            &response.response.kind,
            bcode_harness_models::HarnessModelsResponseKind::Selected { .. }
        );
        if !kind_matches {
            return Err(ClientError::Protocol(
                "selected start did not acknowledge selection".into(),
            ));
        }
        Ok(response)
    }

    async fn execution_request<T: serde::Serialize + Sync, R: serde::de::DeserializeOwned>(
        &self,
        operation: ManagedSessionOperation,
        request: &T,
    ) -> Result<R, ClientError> {
        let envelope = ManagedSessionRequest {
            schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
            operation,
            request_json: serde_json::to_string(request)
                .map_err(|_| ClientError::Protocol("invalid execution request".into()))?,
        };
        let payload = serde_json::to_vec(&envelope)
            .map_err(|_| ClientError::Protocol("invalid execution envelope".into()))?;
        let response = self
            .call_plugin_service(
                MANAGED_SESSION_INTERFACE_ID.into(),
                operation.as_str().into(),
                payload,
            )
            .await?;
        if response.error.is_some()
            || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
        {
            return Err(ClientError::Protocol(
                "execution application request failed".into(),
            ));
        }
        serde_json::from_slice(&response.payload)
            .map_err(|_| ClientError::Protocol("invalid execution application response".into()))
    }

    /// Invoke only managed-session operations or configured-instance discovery/preparation.
    /// No generic plugin/model service route is exposed through deferred authentication.
    ///
    /// # Errors
    /// Rejects unsupported routes or malformed managed requests before connecting. Also returns
    /// normal configuration, artifact/state-location, handshake and daemon application failures.
    pub async fn call_plugin_service(
        &self,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> Result<PluginServiceResponse, ClientError> {
        validate_route(&interface_id, &operation, &payload)?;
        self.inner
            .call_plugin_service(interface_id, operation, payload)
            .await
    }
}

fn configured_context(
    config: &bcode_config::BcodeConfig,
    working_directory: PathBuf,
) -> Result<ClientRuntimeContext, ClientError> {
    // Metadata validation is provider-auth-owned. Missing credentials are not metadata, but
    // unknown accounts, malformed pools and invalid composition must never be swallowed.
    let selection = config.resolved_model_selection();
    if config
        .model
        .profile
        .as_ref()
        .is_some_and(|name| config.resolved_model_profile(name).is_none())
    {
        return Err(ClientError::Protocol(
            "selected model profile is unavailable".into(),
        ));
    }
    bcode_provider_auth::inspect_auth_selection(
        &bcode_provider_auth::ProviderRequestContextResolution {
            config,
            selection: selection.clone(),
        },
    )
    .map_err(|_| {
        ClientError::Protocol("authentication selection metadata could not be verified".into())
    })?;
    // Validate remembered-source references too, without copying or resolving credential values.
    bcode_provider_auth::startup::selected_environment_for_selection(config, &selection).map_err(
        |_| ClientError::Protocol("remembered environment connection is inconsistent".into()),
    )?;
    let effective = bcode_config::encode_effective_config(config)
        .map_err(|_| ClientError::Protocol("client configuration could not be encoded".into()))?;
    Ok(ClientRuntimeContext {
        working_directory: Some(working_directory),
        effective_config_toml: Some(Box::new(effective)),
        selected_provider_plugin_id: selection.provider_plugin_id,
        selected_model_id: selection.model_id,
        requested_model_id: selection.selected_model_id,
        // This context is used exclusively by the sealed managed operation surface. Full
        // declarative config remains available for instance/plugin policy and secret custody.
        ..ClientRuntimeContext::default()
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogRequest {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareRequest {
    selection: Selection,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Selection {
    Default {},
    Instance { instance_id: String },
}

// Local serialized contract for configuration/v1 prepare_setup. The native contracts remain
// domain-owned; the client must not depend on the generic plugin implementation crate.
#[derive(serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeSetupRequest {
    request: bcode_harness_models::HarnessPrepareCandidateRequest,
    response: bcode_harness_models::HarnessPrepareCandidateResponse,
    adapter_configuration: serde_json::Value,
    make_default: bool,
}

// Portable native structs are forward-compatible elsewhere. This sealed bootstrap route is
// deliberately strict: reject fields that deserialization would silently discard. Opaque native
// adapter settings retain all their fields when serialized and are not interpreted by the client.
fn contains_unknown_fields(input: &serde_json::Value, typed: &serde_json::Value) -> bool {
    match (input, typed) {
        (serde_json::Value::Object(input), serde_json::Value::Object(typed)) => {
            input.iter().any(|(key, value)| {
                typed
                    .get(key)
                    .is_none_or(|known| contains_unknown_fields(value, known))
            })
        }
        (serde_json::Value::Array(input), serde_json::Value::Array(typed)) => {
            input.len() != typed.len()
                || input
                    .iter()
                    .zip(typed)
                    .any(|(value, known)| contains_unknown_fields(value, known))
        }
        _ => false,
    }
}

fn validate_native_setup(payload: &[u8]) -> Result<(), ClientError> {
    let invalid = || ClientError::Protocol("invalid native setup proposal".into());
    let request: NativeSetupRequest = serde_json::from_slice(payload).map_err(|_| invalid())?;
    let input: serde_json::Value = serde_json::from_slice(payload).map_err(|_| invalid())?;
    let typed = serde_json::to_value(&request).map_err(|_| invalid())?;
    request.request.validate().map_err(|_| invalid())?;
    request
        .response
        .validate_for(&request.request, &request.request.candidate.plugin_id)
        .map_err(|_| invalid())?;
    if contains_unknown_fields(&input, &typed)
        || !(request.adapter_configuration.is_null() || request.adapter_configuration.is_object())
        || !matches!(
            request.response.kind,
            bcode_harness_models::HarnessPrepareCandidateResponseKind::Prepared(_)
        )
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_route(interface: &str, operation: &str, payload: &[u8]) -> Result<(), ClientError> {
    let invalid = || {
        ClientError::Protocol("managed bootstrap only permits validated managed operations".into())
    };
    if interface == CONFIGURATION_INTERFACE {
        let budget = if operation == "prepare_setup" {
            MAX_SETUP_REQUEST
        } else {
            MAX_CONFIGURATION_REQUEST
        };
        if payload.len() > budget {
            return Err(invalid());
        }
        return match operation {
            "prepare_setup" => validate_native_setup(payload),
            "catalog" => serde_json::from_slice::<CatalogRequest>(payload)
                .map(|_| ())
                .map_err(|_| invalid()),
            "prepare" => {
                let request: PrepareRequest =
                    serde_json::from_slice(payload).map_err(|_| invalid())?;
                match request.selection {
                    Selection::Default {} => Ok(()),
                    Selection::Instance { instance_id }
                        if !instance_id.is_empty()
                            && instance_id.len() <= 256
                            && instance_id.trim() == instance_id
                            && !instance_id.chars().any(char::is_control) =>
                    {
                        Ok(())
                    }
                    Selection::Instance { .. } => Err(invalid()),
                }
            }
            _ => Err(invalid()),
        };
    }
    if interface != MANAGED_SESSION_INTERFACE_ID
        || payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
    {
        return Err(invalid());
    }
    let request: ManagedSessionRequest = serde_json::from_slice(payload).map_err(|_| invalid())?;
    request.validate_envelope().map_err(|_| invalid())?;
    if operation != request.operation.as_str() {
        return Err(invalid());
    }
    if request.operation == ManagedSessionOperation::PrepareCandidate {
        request.decode_prepare_candidate().map_err(|_| invalid())?;
    } else if request.operation == ManagedSessionOperation::DiscoverCandidates {
        let query: bcode_ipc::harness::ManagedCandidatesRequest =
            serde_json::from_str(&request.request_json).map_err(|_| invalid())?;
        query.validate().map_err(|_| invalid())?;
    } else if request.operation == ManagedSessionOperation::ExecutionCatalog {
        request.decode_execution_catalog().map_err(|_| invalid())?;
    } else if request.operation == ManagedSessionOperation::StartSelected {
        request.decode_selected_start().map_err(|_| invalid())?;
    } else if request.operation == ManagedSessionOperation::History {
        request.decode_history().map_err(|_| invalid())?;
    } else if request.operation == ManagedSessionOperation::Recover {
        request.decode_recovery().map_err(|_| invalid())?;
    } else if request.operation.is_auth() {
        // Auth material is still admitted exclusively through the existing managed auth boundary.
        let _: bcode_ipc::harness::ManagedSessionAuthRequest =
            serde_json::from_str(&request.request_json).map_err(|_| invalid())?;
    } else {
        request.decode_call().map_err(|_| invalid())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
