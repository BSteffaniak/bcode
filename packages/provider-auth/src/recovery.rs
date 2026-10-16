//! Re-materialization of verified named provider policy without credential storage.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bcode_config::{BcodeConfig, ConfigEnvironmentSnapshot, ResolvedModelSelection};
use bcode_model::ProviderRequestContext;

/// Resolve original model/profile/pool policy from verified configuration only.
///
/// Runtime subscription metadata and credentials use the existing provider-owned
/// routes. This function does not persist credentials or mutate allowed pool
/// membership. Callers must verify resolved provider/model against their durable
/// submission descriptor through normal catalog resolution before dispatch.
///
/// # Errors
/// Returns a secret-safe error for missing named policy, mismatched selections,
/// request-only auth, or unavailable runtime credential metadata.
pub fn resolve(
    verified: &BcodeConfig,
    model_profile: Option<&str>,
    auth_profile: Option<&str>,
    auth_pool: Option<&str>,
) -> Result<ProviderRequestContext, &'static str> {
    let selection = selection(verified, model_profile, auth_profile, auth_pool)?;
    // Do not invoke startup remembered-environment fallback: replayed config
    // already established the exact durable declarative execution policy.
    let context = crate::try_resolve_with_registry_loader(
        crate::ProviderRequestContextResolution {
            config: verified,
            selection,
        },
        bcode_config::try_load_runtime_auth_subscriptions,
    )
    .map_err(|_| "recovery auth metadata is unavailable")?;
    if context.auth.as_ref().is_some_and(|auth| {
        auth.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == "error")
    }) {
        return Err("recovery credentials are unavailable");
    }
    Ok(context)
}

/// Verify that live request policy is reproducible from durable configuration.
///
/// Credential values may
/// rotate; source identity, storage routes and allowed pool policy may not.
/// Provider API-surface catalog adaptation is validated by the application.
///
/// # Errors
/// Returns a secret-safe error when named resolution fails or transient fields
/// cannot be reproduced. Never includes request values in the error.
pub fn capture_policy(
    verified: &BcodeConfig,
    original: &ProviderRequestContext,
) -> Result<(), &'static str> {
    let recovered = resolve(
        verified,
        original.model_profile.as_deref(),
        original.auth_profile.as_deref(),
        original.auth_pool.as_deref(),
    )?;
    compare_policy(original, &recovered)
}

fn compare_policy(
    original: &ProviderRequestContext,
    recovered: &ProviderRequestContext,
) -> Result<(), &'static str> {
    fn normalize(context: &ProviderRequestContext) -> ProviderRequestContext {
        let mut context = context.clone();
        let mut secrets = BTreeMap::new();
        if let Some(auth) = context.auth.as_ref() {
            for (name, credential) in &auth.credentials {
                secrets.insert(credential.value.clone(), name.clone());
            }
            for auth in context
                .auth_candidates
                .iter()
                .map(|candidate| &candidate.auth)
            {
                for (name, credential) in &auth.credentials {
                    secrets.insert(credential.value.clone(), name.clone());
                }
            }
        }
        for value in context.env.values_mut().chain(
            context
                .auth_candidates
                .iter_mut()
                .flat_map(|candidate| candidate.env.values_mut()),
        ) {
            if let Some(name) = secrets.get(value) {
                *value = format!("credential:{name}");
            }
        }
        for auth in context.auth.iter_mut().chain(
            context
                .auth_candidates
                .iter_mut()
                .map(|candidate| &mut candidate.auth),
        ) {
            for credential in auth.credentials.values_mut() {
                credential.value.clear();
            }
            auth.diagnostics.clear();
        }
        // These are catalog/provider runtime projections, not declarative
        // overrides. Exact catalog identity is checked by the backend.
        context.api_surface = None;
        context.auth_pool_selection_reason = None;
        context
    }
    if normalize(original) != normalize(recovered) {
        return Err("recovery unavailable: transient provider policy cannot be reproduced");
    }
    Ok(())
}

fn selection(
    verified: &BcodeConfig,
    model_profile: Option<&str>,
    auth_profile: Option<&str>,
    auth_pool: Option<&str>,
) -> Result<ResolvedModelSelection, &'static str> {
    let selection = if let Some(profile) = model_profile {
        verified
            .resolved_model_profile(profile)
            .ok_or("recovery model profile is unavailable")?
    } else {
        verified.resolved_model_selection_with_environment(&ConfigEnvironmentSnapshot::new(
            BTreeMap::new(),
            PathBuf::from("."),
        ))
    };
    selection
        .validate_selection()
        .map_err(|_| "recovery model selection is incomplete")?;
    if selection.model_profile.as_deref() != model_profile
        || selection.auth_profile.as_deref() != auth_profile
        || selection.auth_pool.as_deref() != auth_pool
    {
        return Err("recovery named selection differs from verified configuration");
    }
    if auth_profile == Some("environment") {
        return Err("request-only environment credentials cannot be recovered");
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BcodeConfig {
        let mut config = BcodeConfig::default();
        config.model.provider_plugin_id = Some("owned.provider".to_owned());
        config.model.model_id = Some("model".to_owned());
        config.model.auth_profile = Some("account".to_owned());
        config.model.auth_pool = Some("allowed".to_owned());
        config.auth.profiles.insert(
            "account".to_owned(),
            bcode_config::AuthProfileConfig {
                backend: "test".to_owned(),
                provider_id: Some("owned.provider".to_owned()),
                owner_plugin_id: Some("owned.provider".to_owned()),
                ..Default::default()
            },
        );
        config.auth.pools.insert(
            "allowed".to_owned(),
            bcode_config::AuthPoolConfig {
                provider_plugin_id: Some("owned.provider".to_owned()),
                profiles: vec!["account".to_owned()],
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn changed_selection_cannot_expand_pool_policy() {
        let config = config();
        assert!(selection(&config, None, Some("account"), Some("allowed")).is_ok());
        assert!(selection(&config, None, Some("other"), Some("allowed")).is_err());
        assert!(selection(&config, None, Some("account"), Some("other")).is_err());
        assert!(selection(&config, Some("missing"), Some("account"), Some("allowed")).is_err());
    }

    #[test]
    fn credential_refresh_uses_same_named_policy() {
        let config = config();
        let before = bcode_config::recovery::execution_fingerprint(&config).unwrap();
        let selection = selection(&config, None, Some("account"), Some("allowed")).unwrap();
        let mut generation = 0;
        let mut materialize = || {
            crate::resolve_provider_request_context_with_resolver(
                crate::ProviderRequestContextResolution {
                    config: &config,
                    selection: selection.clone(),
                },
                &bcode_config::RuntimeAuthSubscriptions::default(),
                |name, _profile| {
                    generation += 1;
                    crate::ResolvedProviderAuth {
                        auth: bcode_model::ProviderAuthContext {
                            profile: Some(name.to_owned()),
                            credentials: BTreeMap::from([(
                                "api_key".to_owned(),
                                bcode_model::ProviderAuthCredential {
                                    value: format!("refreshed-{generation}"),
                                    source: None,
                                },
                            )]),
                            ..Default::default()
                        },
                        env: BTreeMap::new(),
                    }
                },
            )
        };
        let first = materialize();
        let second = materialize();
        assert_ne!(first.auth, second.auth);
        assert_eq!(first.auth_pool, second.auth_pool);
        assert_eq!(
            before,
            bcode_config::recovery::execution_fingerprint(&config).unwrap()
        );
        assert_eq!(config.auth.pools["allowed"].profiles, vec!["account"]);
        assert!(compare_policy(&first, &second).is_ok());
        let mut transient = first.clone();
        transient
            .settings
            .insert("private".to_owned(), "secret-marker".to_owned());
        assert!(compare_policy(&transient, &second).is_err());
        transient = first.clone();
        transient
            .env
            .insert("TRANSIENT".to_owned(), "secret-marker".to_owned());
        let error = compare_policy(&transient, &second).unwrap_err();
        assert!(!error.contains("secret-marker"));
        transient = first;
        transient.auth_pool = Some("expanded".to_owned());
        assert!(compare_policy(&transient, &second).is_err());
    }
}
