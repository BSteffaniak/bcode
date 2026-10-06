//! Non-conversational judgement application routing.
//!
//! The application resolves credentials, identity and capabilities before invoking the plugin.
//! Provider service failures are untrusted and never returned verbatim to a client.

use bcode_model::judgement::{self, ModelList, ProviderRequest, Request, Response};
use bcode_plugin::PluginInvocationScope;

use crate::ServerState;

#[path = "judgement_optional.rs"]
mod optional;
pub use optional::OptionalJudgementAccess;

/// Cache-only bridge suitable for credential-refresh closures.
pub type JudgementInvalidationHandle = optional::JudgementInvalidationHandle;

const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const JUDGEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn validate_setup_selection(
    state: &ServerState,
    selection: &judgement::SetupSelection,
) -> bool {
    if selection.provider_plugin_id.len() > 256
        || selection.model_id.len() > 256
        || selection.auth_profile.len() > 256
        || selection.provider_plugin_id.is_empty()
        || selection.model_id.is_empty()
    {
        return false;
    }
    let Some(manifest) = state
        .plugins
        .registry()
        .manifests()
        .get(&selection.provider_plugin_id)
    else {
        return false;
    };
    if !manifest
        .services
        .iter()
        .any(|service| service.interface_id == judgement::INTERFACE_ID)
    {
        return false;
    }
    let matching: Vec<_> = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .filter(|provider| provider.plugin_id == selection.provider_plugin_id)
        .collect();
    if matching.len() != 1 {
        return false;
    }
    let provider = matching[0];
    let context = if selection.auth_profile.is_empty() {
        bcode_provider_auth::resolve_declared_environment_context(&provider.contribution)
    } else {
        bcode_provider_auth::resolve_owned_profile_context(
            &state.startup_config,
            &provider.contribution.provider_id,
            &selection.provider_plugin_id,
            &selection.auth_profile,
        )
    };
    let Ok(context) = context else {
        return false;
    };
    if context
        .auth
        .as_ref()
        .is_none_or(|auth| auth.credentials.is_empty())
    {
        return false;
    }
    let listing: Result<ModelList, _> = state
        .plugins
        .invoke_service_json_scoped_with_timeout(
            &selection.provider_plugin_id,
            judgement::INTERFACE_ID,
            judgement::OP_MODELS,
            &context,
            PluginInvocationScope::Global,
            DISCOVERY_TIMEOUT,
        )
        .await;
    listing.is_ok_and(|list| {
        list.provider_id == selection.provider_plugin_id
            && list
                .question_kinds
                .contains(&judgement::QuestionKind::YesNo)
            && list.models.iter().any(|model| {
                model.model_id == selection.model_id
                    && model
                        .question_kinds
                        .contains(&judgement::QuestionKind::YesNo)
            })
    })
}

pub async fn setup_catalog(state: &ServerState) -> judgement::SetupCatalog {
    setup_catalog_with_config(state, &state.startup_config).await
}

async fn setup_catalog_with_config(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
) -> judgement::SetupCatalog {
    let mut providers = Vec::new();
    for (plugin_id, manifest) in state.plugins.registry().manifests() {
        if providers.len() >= 32 {
            break;
        }
        if !manifest
            .services
            .iter()
            .any(|service| service.interface_id == judgement::INTERFACE_ID)
        {
            continue;
        }
        if let Some(provider) = setup_provider(state, config, plugin_id).await {
            providers.push(provider);
            if serde_json::to_vec(&judgement::SetupCatalog {
                providers: providers.clone(),
            })
            .map_or(true, |bytes| bytes.len() > 240_000)
            {
                providers.pop();
            }
        }
    }
    judgement::SetupCatalog { providers }
}

fn setup_profiles(
    config: &bcode_config::BcodeConfig,
    plugin_id: &str,
    provider_id: &str,
) -> Vec<String> {
    let mut profiles = Vec::new();
    let mut names: std::collections::BTreeSet<String> =
        config.auth.profiles.keys().cloned().collect();
    let runtime = if config.active_context.is_none() {
        bcode_config::try_load_runtime_auth_subscriptions().ok()
    } else {
        None
    };
    if config.active_context.is_none() && runtime.is_none() {
        return Vec::new();
    }
    if let Some(runtime) = runtime.as_ref() {
        names.extend(runtime.profiles.keys().cloned());
    }
    for name in names.into_iter().take(256) {
        if profiles.len() >= 32 {
            break;
        }
        if name.len() > 256 {
            continue;
        }
        let empty = bcode_config::RuntimeAuthSubscriptions::default();
        let registry = runtime.as_ref().unwrap_or(&empty);
        if bcode_provider_auth::resolve_auth_provider_profile(
            config,
            provider_id,
            plugin_id,
            Some(&name),
            registry,
        )
        .is_ok()
            && bcode_provider_auth::resolve_owned_profile_context(
                config,
                provider_id,
                plugin_id,
                &name,
            )
            .is_ok()
        {
            profiles.push(name);
        }
    }
    profiles
}

async fn setup_provider(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    plugin_id: &str,
) -> Option<judgement::SetupProvider> {
    let matching: Vec<_> = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .filter(|auth| auth.plugin_id == plugin_id)
        .collect();
    if matching.len() != 1 {
        return None;
    }
    let auth = matching[0];
    let profiles = setup_profiles(config, plugin_id, &auth.contribution.provider_id);
    let environment_available =
        bcode_provider_auth::resolve_declared_environment_context(&auth.contribution).is_ok();
    let context = profiles
        .first()
        .and_then(|name| {
            bcode_provider_auth::resolve_owned_profile_context(
                config,
                &auth.contribution.provider_id,
                plugin_id,
                name,
            )
            .ok()
        })
        .or_else(|| {
            environment_available
                .then(|| {
                    bcode_provider_auth::resolve_declared_environment_context(&auth.contribution)
                        .ok()
                })
                .flatten()
        });
    // Model discovery is optional: an unavailable auth account still permits static
    // capability discovery, but does not permit judgement execution.
    let discovery_context = context.unwrap_or_default();
    let listing: Result<ModelList, _> = state
        .plugins
        .invoke_service_json_scoped_with_timeout(
            plugin_id,
            judgement::INTERFACE_ID,
            judgement::OP_MODELS,
            &discovery_context,
            PluginInvocationScope::Global,
            DISCOVERY_TIMEOUT,
        )
        .await;
    let (models, unavailable_reason) = match listing {
        Ok(list)
            if list.models.len() <= 64
                && list.models.iter().all(|model| model.model_id.len() <= 256)
                && list.provider_id == plugin_id =>
        {
            (
                list.models
                    .into_iter()
                    .filter(|model| {
                        model
                            .question_kinds
                            .contains(&judgement::QuestionKind::YesNo)
                            && list
                                .question_kinds
                                .contains(&judgement::QuestionKind::YesNo)
                    })
                    .collect(),
                None,
            )
        }
        _ => (
            Vec::new(),
            Some("model discovery unavailable; refresh or inspect provider setup".into()),
        ),
    };
    let unavailable_reason = if profiles.is_empty() && !environment_available {
        Some(format!(
            "No usable account; run bcode auth login {}",
            auth.contribution.provider_id
        ))
    } else {
        unavailable_reason
    };
    Some(judgement::SetupProvider {
        plugin_id: plugin_id.into(),
        display_name: auth.contribution.display_name.chars().take(128).collect(),
        auth_provider_id: auth.contribution.provider_id.clone(),
        profiles,
        environment_available,
        models,
        unavailable_reason,
    })
}

pub async fn invoke_judgement_model(
    state: &ServerState,
    provider_plugin_id: &str,
    auth_profile: &str,
    request: Request,
) -> Result<Response, &'static str> {
    invoke_judgement_model_with_config(
        state,
        &state.startup_config,
        provider_plugin_id,
        auth_profile,
        request,
    )
    .await
}

async fn invoke_judgement_model_with_config(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    provider_plugin_id: &str,
    auth_profile: &str,
    request: Request,
) -> Result<Response, &'static str> {
    judgement::validate_request(&request)?;
    if provider_plugin_id.is_empty()
        || provider_plugin_id.len() > 256
        || auth_profile.len() > 256
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
    let mut auth_providers = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .filter(|entry| entry.plugin_id == provider_plugin_id);
    let registered = auth_providers
        .next()
        .ok_or("judgement provider authentication is unavailable")?;
    if auth_providers.next().is_some() {
        return Err("judgement provider authentication is ambiguous");
    }
    let provider_context = if auth_profile.is_empty() {
        bcode_provider_auth::resolve_declared_environment_context(&registered.contribution)?
    } else {
        bcode_provider_auth::resolve_owned_profile_context(
            config,
            &registered.contribution.provider_id,
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
