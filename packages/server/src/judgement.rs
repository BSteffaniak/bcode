//! Non-conversational judgement application routing.
//!
//! The application resolves credentials, identity and capabilities before invoking the plugin.
//! Provider service failures are untrusted and never returned verbatim to a client.

use bcode_model::judgement::{self, ModelList, ProviderRequest, Request, Response};
use bcode_plugin::PluginInvocationScope;

use crate::ServerState;

const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const JUDGEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn invoke_judgement_model(
    state: &ServerState,
    provider_plugin_id: &str,
    auth_profile: &str,
    request: Request,
) -> Result<Response, &'static str> {
    judgement::validate_request(&request)?;
    if provider_plugin_id.is_empty()
        || !state
            .plugins
            .registry()
            .manifests()
            .get(provider_plugin_id)
            .is_some_and(|manifest| {
                manifest
                    .services
                    .iter()
                    .any(|service| service.interface_id == judgement::INTERFACE_ID)
            })
    {
        return Err("judgement provider is not available");
    }
    let provider_context = if auth_profile.is_empty() {
        let registered = state
            .plugins
            .auth_provider(provider_plugin_id)
            .filter(|entry| entry.plugin_id == provider_plugin_id)
            .ok_or("judgement provider environment authentication is unavailable")?;
        bcode_provider_auth::resolve_declared_environment_context(&registered.contribution)?
    } else {
        bcode_provider_auth::resolve_explicit_profile_context(
            &state.startup_config,
            provider_plugin_id,
            auth_profile,
        )?
    };
    if provider_context.auth.is_none() {
        return Err("judgement provider credentials are unavailable");
    }
    let listing: ModelList = state
        .plugins
        .invoke_service_json_scoped_with_timeout(
            provider_plugin_id,
            judgement::INTERFACE_ID,
            judgement::OP_MODELS,
            &provider_context,
            PluginInvocationScope::Global,
            DISCOVERY_TIMEOUT,
        )
        .await
        .map_err(|_| "judgement model discovery failed")?;
    if listing.models.len() > 4096 {
        return Err("judgement model listing exceeds size limit");
    }
    let resolved = state
        .model_catalog
        .resolve_judgement_request(provider_plugin_id, &request, &listing)
        .await?;
    let model = judgement::select_supported_model(&resolved, &listing)?;
    let envelope = ProviderRequest {
        judgement: resolved.clone(),
        provider_context,
    };
    let result: Response = state
        .plugins
        .invoke_service_json_scoped_with_timeout(
            provider_plugin_id,
            judgement::INTERFACE_ID,
            judgement::OP_JUDGE,
            &envelope,
            PluginInvocationScope::Global,
            JUDGEMENT_TIMEOUT,
        )
        .await
        .map_err(|_| "judgement provider invocation failed")?;
    judgement::validate_response(&resolved, model, &result)?;
    Ok(result)
}
