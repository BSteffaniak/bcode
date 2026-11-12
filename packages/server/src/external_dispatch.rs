//! Dispatch-local external authentication. Retained selections contain recipes, never credentials.

use super::{ModelTurnRequest, ServerState};
use std::path::Path;

const UNAVAILABLE: &str = "external authentication unavailable; no fallback permitted";

/// Copy a selected recipe without invoking ambient/profile credential resolution.
pub fn recipe(
    selection: &bcode_config::ResolvedModelSelection,
) -> bcode_model::ProviderRequestContext {
    bcode_model::ProviderRequestContext {
        model_profile: selection.model_profile.clone(),
        auth_profile: selection.auth_profile.clone(),
        auth_pool: selection.auth_pool.clone(),
        settings: selection.settings.clone(),
        request: selection.request.clone(),
        ..Default::default()
    }
}

/// Validate the complete unmaterialized context, not just fields in `ResolvedModelSelection`.
fn selection(
    provider: Option<&str>,
    request: &ModelTurnRequest,
) -> Result<bcode_config::ResolvedModelSelection, String> {
    let context = &request.provider_context;
    if context.auth.is_some()
        || !context.auth_candidates.is_empty()
        || !context.env.is_empty()
        || context.auth_profile.is_some()
        || context.auth_pool.is_some()
        || !context.request.is_empty()
    {
        return Err(UNAVAILABLE.into());
    }
    Ok(bcode_config::ResolvedModelSelection {
        provider_plugin_id: Some(provider.ok_or(UNAVAILABLE)?.to_owned()),
        model_id: Some(request.model_id.clone()),
        model_profile: context.model_profile.clone(),
        settings: context.settings.clone(),
        ..Default::default()
    })
}

pub async fn context_for_dispatch(
    state: &ServerState,
    provider: Option<&str>,
    request: &ModelTurnRequest,
    home: &Path,
) -> Result<Option<bcode_model::ProviderRequestContext>, String> {
    let Some(binding) = request
        .provider_context
        .settings
        .get("external_auth_binding")
    else {
        return Ok(None);
    };
    let selection = selection(provider, request)?;
    let provider = selection.provider_plugin_id.as_deref().ok_or(UNAVAILABLE)?;
    let store = bcode_provider_auth::external_auth::ExternalAuthStore::new(&state.state_root);
    let grant = store
        .list()
        .map_err(|_| UNAVAILABLE)?
        .into_iter()
        .find(|grant| grant.binding_id == *binding)
        .ok_or(UNAVAILABLE)?;
    if grant.target.plugin_id != provider {
        return Err(UNAVAILABLE.into());
    }
    let registered = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if !registered.iter().any(|entry| {
        entry.plugin_id == provider && entry.contribution.provider_id == grant.target.provider_id
    }) {
        return Err(UNAVAILABLE.into());
    }
    let choices: bcode_model::StartupModelChoicesResponse = state
        .plugins
        .invoke_service_json(
            provider,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_STARTUP_MODEL_CHOICES,
            &bcode_model::StartupRecommendationRequest {
                auth_provider_id: grant.target.provider_id,
                method_id: grant.target.method_id,
            },
        )
        .await
        .map_err(|_| UNAVAILABLE)?;
    let enabled = registered
        .iter()
        .map(|entry| (entry.plugin_id.as_str(), &entry.contribution))
        .collect::<Vec<_>>();
    let mut context = bcode_provider_auth::startup::resolve_external_execution(
        &selection,
        bcode_provider_auth::startup::ExternalExecutionContext {
            home,
            store: &store,
            enabled: &enabled,
            // This boundary has no authenticated application identity. Broad consent only.
            application_id: None,
            provider_choices: &choices.choices,
        },
    )
    .map_err(|_| UNAVAILABLE)?
    .ok_or(UNAVAILABLE)?;
    // Model routing is host-owned, not an ambient fallback. Preserve it after validation.
    context.api_surface = request.provider_context.api_surface;
    // Dispatch-local marker disables credential writeback on the provider bridge.
    context
        .settings
        .insert("external_auth_binding".into(), binding.clone());
    Ok(Some(context))
}

#[cfg(test)]
mod tests;
