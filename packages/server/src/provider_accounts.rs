//! Caller-scoped, secret-free account model discovery and exact selection routing.

use bcode_model::{ModelList, ProviderAccountModels, ProviderRequestContext};
use bcode_session_models::{ClientId, SessionId};

use crate::ServerState;

#[cfg(test)]
mod tests;

const MAX_ACCOUNTS: usize = 64;
const MAX_MODELS: usize = 512;

pub async fn caller_config(
    state: &ServerState,
    client: ClientId,
) -> Result<bcode_config::BcodeConfig, String> {
    let runtime = state.client_runtime_context(client).await;
    crate::submission_admission_config(state, runtime.as_ref())
        .map_err(|_| "caller configuration is unavailable".into())
}

fn metadata(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    provider: &str,
    account: &str,
) -> Result<bcode_provider_auth::ResolvedAuthProfile, String> {
    if account.trim().is_empty() || account.len() > 256 {
        return Err("invalid account reference".into());
    }
    let resolved = bcode_provider_auth::resolve_owned_profile_metadata(config, provider, account)
        .map_err(str::to_owned)?;
    if resolved.profile_name != account {
        return Err("account reference is outside the selected context".into());
    }
    let registered = state
        .plugins
        .auth_provider_registry()
        .get(&resolved.provider_id)
        .filter(|entry| entry.plugin_id == provider)
        .ok_or("account provider is unavailable")?;
    let method = registered
        .contribution
        .methods
        .iter()
        .find(|method| Some(method.method_id()) == resolved.profile.scheme.as_deref())
        .ok_or("account method is unavailable")?;
    // Validate lifecycle ownership without unlocking the vault in the model picker.
    bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
        &resolved,
        &resolved.provider_id,
        provider,
        method,
    )
    .map_err(|_| "account credentials are unavailable")?;
    let vault = resolved.profile.settings.get("vault").map_or_else(
        bcode_config::default_auth_vault_path,
        std::path::PathBuf::from,
    );
    if !std::fs::metadata(vault).is_ok_and(|metadata| metadata.is_file()) {
        return Err("account credentials are unavailable".into());
    }
    Ok(resolved)
}

type AccountRouteFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(ProviderRequestContext, ModelList), String>>
            + Send
            + 'a,
    >,
>;

pub fn route<'a>(
    state: &'a ServerState,
    config: &'a bcode_config::BcodeConfig,
    provider: &'a str,
    account: &'a str,
    selected_model: Option<&'a str>,
) -> AccountRouteFuture<'a> {
    Box::pin(route_inner(
        state,
        config,
        provider,
        account,
        selected_model,
    ))
}

async fn route_inner(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    provider: &str,
    account: &str,
    selected_model: Option<&str>,
) -> Result<(ProviderRequestContext, ModelList), String> {
    let resolved = metadata(state, config, provider, account)?;
    let choices = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state
            .plugins
            .invoke_service_json::<_, bcode_model::StartupModelChoicesResponse>(
                provider,
                bcode_model::MODEL_PROVIDER_INTERFACE_ID,
                bcode_model::OP_STARTUP_MODEL_CHOICES,
                &bcode_model::StartupRecommendationRequest {
                    auth_provider_id: resolved.provider_id.clone(),
                    method_id: resolved
                        .profile
                        .scheme
                        .ok_or("account method is unavailable")?,
                },
            ),
    )
    .await
    .map_err(|_| "account model choices timed out")?
    .map_err(|_| "account model choices are unavailable")?;
    if choices.choices.len() > MAX_MODELS {
        return Err("account model choices exceed the source limit".into());
    }
    let choices = choices
        .choices
        .into_iter()
        .take(MAX_MODELS)
        .filter(|choice| choice.provider_plugin_id == provider)
        .collect::<Vec<_>>();
    let choice = choices
        .iter()
        .find(|choice| selected_model.is_none_or(|model| choice.model_id == model))
        .ok_or("model is unavailable for the selected account")?;
    let context = ProviderRequestContext {
        account_reference: Some(account.to_owned()),
        auth_profile: Some(account.to_owned()),
        settings: choice.settings.clone(),
        ..Default::default()
    };
    // Membership and routing come from the provider's offline choices. The owning catalog
    // enriches the bounded list; no ambient endpoint, model profile or credential is inherited.
    let catalog_provider = choice
        .settings
        .get("catalog_provider_id")
        .cloned()
        .unwrap_or(resolved.provider_id);
    let list = ModelList {
        models: choices
            .iter()
            .map(|choice| bcode_model::ModelInfo {
                model_id: choice.model_id.clone(),
                display_name: choice.model_id.clone(),
                is_default: false,
                context_window: None,
                max_output_tokens: None,
                max_image_input_base64_bytes: None,
                capabilities: std::collections::BTreeSet::default(),
                feature_support: bcode_model::ModelFeatureSupport::default(),
                reasoning: None,
                cache: bcode_model::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                api_surface: None,
                visibility: bcode_model::ModelVisibility::default(),
            })
            .collect(),
        catalog: bcode_model::ModelCatalogHints {
            policy: bcode_model::ModelCatalogPolicy::EnrichOnly {
                provider_id: catalog_provider,
                target: None,
                authority: bcode_model::ModelListAuthority::Authoritative,
            },
            ..Default::default()
        },
    };
    let list = state
        .model_catalog
        .resolve_selection(list, selected_model, None)
        .await;
    Ok((context, list))
}

fn require_discovery_budget(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
) -> Result<(), String> {
    // Reject oversized sources explicitly before enumerating or cloning candidate names.
    let unavailable = || {
        "account discovery exceeds the 64-account source limit; narrow the configuration context"
            .to_owned()
    };
    if config.auth.profiles.len() > MAX_ACCOUNTS
        || runtime.profiles.len() > MAX_ACCOUNTS
        || runtime.pools.len() > MAX_ACCOUNTS
    {
        return Err(unavailable());
    }
    let mut total = config.auth.profiles.len() + runtime.profiles.len();
    for pool in runtime.pools.values() {
        total = total.saturating_add(pool.profiles.len());
        if total > MAX_ACCOUNTS {
            return Err(unavailable());
        }
    }
    if total > MAX_ACCOUNTS {
        return Err(unavailable());
    }
    Ok(())
}

pub async fn discover(
    state: &ServerState,
    client: ClientId,
) -> Result<Vec<ProviderAccountModels>, String> {
    let config = caller_config(state, client).await?;
    let runtime = if config.active_context.is_some() {
        bcode_config::RuntimeAuthSubscriptions::default()
    } else {
        bcode_config::try_load_runtime_auth_subscriptions()
            .map_err(|_| "account registry is unavailable")?
    };
    require_discovery_budget(&config, &runtime)?;
    if state.plugins.auth_provider_registry().len() > MAX_ACCOUNTS {
        return Err("account discovery exceeds the provider limit".into());
    }
    let mut result = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut inspected = 0;
    for registered in state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .take(MAX_ACCOUNTS)
    {
        for resolved in bcode_provider_auth::discover_provider_profiles_bounded(
            &config,
            &registered.contribution.provider_id,
            &registered.plugin_id,
            &runtime,
            MAX_ACCOUNTS.saturating_sub(inspected),
        )
        .into_values()
        .filter_map(Result::ok)
        {
            if inspected >= MAX_ACCOUNTS {
                return Ok(result);
            }
            inspected += 1;
            if tokio::time::Instant::now() >= deadline {
                return Err("account discovery timed out; results are incomplete".into());
            }
            let resolved_models = tokio::time::timeout_at(
                deadline,
                route(
                    state,
                    &config,
                    &registered.plugin_id,
                    &resolved.profile_name,
                    None,
                ),
            )
            .await;
            let (models, unavailable) = match resolved_models {
                Ok(Ok((_, models))) => (models, false),
                _ => (
                    ModelList {
                        models: Vec::new(),
                        catalog: bcode_model::ModelCatalogHints::default(),
                    },
                    true,
                ),
            };
            result.push(ProviderAccountModels {
                display_name: format!(
                    "{} · {}{}",
                    registered.contribution.display_name,
                    resolved.profile_name,
                    if unavailable { " (unavailable)" } else { "" }
                ),
                account_reference: resolved.profile_name,
                provider_plugin_id: registered.plugin_id.clone(),
                models,
            });
        }
    }
    Ok(result)
}

pub async fn validate(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    provider: &str,
    model: &str,
    account: &str,
) -> Result<ProviderRequestContext, String> {
    let (mut context, catalog) = route(state, config, provider, account, Some(model)).await?;
    let selected = catalog
        .models
        .iter()
        .find(|entry| entry.model_id == model)
        .ok_or("model is unavailable for the selected account")?;
    context.api_surface = selected.api_surface;
    Ok(context)
}

fn validate_credentials(
    method: &bcode_provider_auth_models::AuthMethodContribution,
    credentials: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let complete = match method {
        bcode_provider_auth_models::AuthMethodContribution::SecretFields { fields, .. } => {
            fields.iter().all(|field| {
                credentials
                    .get(&field.credential_id)
                    .map_or(field.optional, |value| {
                        !value.trim().is_empty() && field.validation.validate_secret(value).is_ok()
                    })
            })
        }
        bcode_provider_auth_models::AuthMethodContribution::Interactive {
            credentials: required,
            ..
        } => required.iter().all(|field| {
            credentials
                .get(&field.credential_id)
                .is_some_and(|value| !value.trim().is_empty())
        }),
    };
    if credentials.is_empty() || !complete {
        return Err("account credentials are unavailable".into());
    }
    Ok(())
}

/// Materialize exactly the selected account at execution; never inherit a pool or transient auth.
pub fn materialize<'a>(
    state: &'a ServerState,
    config: &'a bcode_config::BcodeConfig,
    provider: &'a str,
    model: &'a str,
    account: &'a str,
) -> impl std::future::Future<Output = Result<ProviderRequestContext, String>> + Send + 'a {
    Box::pin(materialize_inner(state, config, provider, model, account))
}

async fn materialize_inner(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    provider: &str,
    model: &str,
    account: &str,
) -> Result<ProviderRequestContext, String> {
    let mut context = validate(state, config, provider, model, account).await?;
    let metadata = metadata(state, config, provider, account)?;
    let registered = state
        .plugins
        .auth_provider_registry()
        .get(&metadata.provider_id)
        .filter(|registered| registered.plugin_id == provider)
        .ok_or("account provider is unavailable")?;
    let method = registered
        .contribution
        .methods
        .iter()
        .find(|method| Some(method.method_id()) == metadata.profile.scheme.as_deref())
        .ok_or("account method is unavailable")?;
    let credentials = bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
        &metadata,
        &metadata.provider_id,
        provider,
        method,
    )
    .and_then(|lifecycle| lifecycle.read())
    .map_err(|_| "account credentials are unavailable")?;
    validate_credentials(method, &credentials)?;
    // Read only the selected vault, never the mapped process environment fallback.
    context.auth = Some(bcode_model::ProviderAuthContext {
        profile: Some(account.to_owned()),
        backend: Some(metadata.profile.backend.clone()),
        scheme: metadata.profile.scheme.clone(),
        storage: credentials
            .keys()
            .map(|name| {
                let key = metadata
                    .profile
                    .map
                    .get(name)
                    .and_then(|mapping| mapping.key.as_ref().or(mapping.env.as_ref()))
                    .cloned()
                    .or_else(|| match method {
                        bcode_provider_auth_models::AuthMethodContribution::SecretFields {
                            fields,
                            ..
                        } => fields
                            .iter()
                            .find(|field| field.credential_id == *name)
                            .map(|field| field.storage_key.clone()),
                        bcode_provider_auth_models::AuthMethodContribution::Interactive {
                            credentials,
                            ..
                        } => credentials
                            .iter()
                            .find(|field| field.credential_id == *name)
                            .map(|field| field.storage_key.clone()),
                    })
                    .unwrap_or_else(|| name.clone());
                (
                    name.clone(),
                    bcode_model::ProviderAuthStorageRef {
                        backend: metadata.profile.backend.clone(),
                        profile: metadata
                            .profile
                            .settings
                            .get("profile")
                            .cloned()
                            .unwrap_or_else(|| account.to_owned()),
                        key,
                        vault: metadata.profile.settings.get("vault").cloned(),
                    },
                )
            })
            .collect(),
        credentials: credentials
            .into_iter()
            .map(|(name, value)| {
                (
                    name,
                    bcode_model::ProviderAuthCredential {
                        value,
                        source: None,
                    },
                )
            })
            .collect(),
        ..Default::default()
    });
    Ok(context)
}

/// Keep invocation-specific non-auth semantics while replacing all account routing/auth policy.
pub fn preserve_request_context(
    context: &mut ProviderRequestContext,
    original: &ProviderRequestContext,
) {
    context.request.clone_from(&original.request);
    context.api_surface = context.api_surface.or(original.api_surface);
}

pub async fn set(
    state: &ServerState,
    session: SessionId,
    config: &bcode_config::BcodeConfig,
    provider: Option<String>,
    model: String,
    account: String,
) -> Result<(), String> {
    let provider = provider.ok_or("an explicit provider is required for an account")?;
    // Explicit selection preflights the complete owned credential set, then discards it.
    // Discovery never performs this unlock; no credentials enter the model-change event.
    let mut preflight = materialize(state, config, &provider, &model, &account).await?;
    if let Some(auth) = &mut preflight.auth {
        for credential in auth.credentials.values_mut() {
            zeroize::Zeroize::zeroize(&mut credential.value);
        }
    }
    drop(preflight);
    if let Some(skill) = crate::required_model_active_skill(state, session).await
        && crate::required_model_override_behavior(&config.skills.model_policy, &skill)
            == bcode_config::SkillRequiredModelOverride::Deny
    {
        return Err("an active skill requires its model".into());
    }
    let event = state
        .sessions
        .append_event(
            session,
            bcode_session_models::SessionEventKind::ModelChanged {
                provider,
                model,
                selection_source: bcode_session_models::ModelSelectionSource::UserExplicit,
                external_auth_binding: None,
                account_reference: Some(account),
            },
        )
        .await
        .map_err(|_| "session selection could not be persisted")?;
    state
        .session_configs
        .lock()
        .await
        .insert(session, config.clone());
    state.session_model_selections.lock().await.remove(&session);
    state
        .session_model_selection_origins
        .lock()
        .await
        .remove(&session);
    crate::publish_session_event(state, &event).await;
    Ok(())
}
