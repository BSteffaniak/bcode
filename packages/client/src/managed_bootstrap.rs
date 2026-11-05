//! Provider-independent bootstrap with a sealed managed-only method surface.
//!
//! No raw connection or inner model client is exposed. The daemon still owns authorization;
//! this facade prevents deferred model credentials from becoming an ordinary model fallback.
mod auth_flow;
pub use auth_flow::ManagedSessionAuthFlow;
mod attachment;
pub use attachment::{ManagedSessionCommandHandle, ManagedSessionConnection};

use crate::{BcodeClient, ClientError, ClientRuntimeContext, PluginServiceResponse};
use bcode_ipc::harness::{
    MANAGED_SESSION_INTERFACE_ID, ManagedSessionOperation, ManagedSessionRequest,
};
use serde::Deserialize;
use std::path::PathBuf;

const CONFIGURATION_INTERFACE: &str = "bcode.harness.configuration/v1";
const MAX_CONFIGURATION_REQUEST: usize = 4096;

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

fn validate_route(interface: &str, operation: &str, payload: &[u8]) -> Result<(), ClientError> {
    let invalid = || {
        ClientError::Protocol("managed bootstrap only permits validated managed operations".into())
    };
    if interface == CONFIGURATION_INTERFACE && payload.len() <= MAX_CONFIGURATION_REQUEST {
        return match operation {
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
    if request.operation == ManagedSessionOperation::History {
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
