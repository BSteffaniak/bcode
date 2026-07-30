#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Generic provider authentication materialization.
//!
//! This crate resolves declarative `auth.profiles.*` config into semantic auth
//! material for provider plugins, plus compatibility env values for providers
//! that still consume environment-shaped credentials.

pub mod auth_pool_routing;
pub mod auth_pool_state;
pub mod lifecycle;
pub mod security;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Request for resolving a provider request context from model and auth config.
#[derive(Debug, Clone)]
pub struct ProviderRequestContextResolution<'a> {
    pub config: &'a bcode_config::BcodeConfig,
    pub selection: bcode_config::ResolvedModelSelection,
}

/// Resolve model selection plus auth profile/pool config into provider request context.
///
/// This is the canonical host-side materialization path for provider auth. Callers should pass the
/// returned context to provider plugins instead of asking plugins to rediscover config profiles.
#[must_use]
pub fn resolve_provider_request_context(
    request: ProviderRequestContextResolution<'_>,
) -> bcode_model::ProviderRequestContext {
    let mut context = bcode_model::ProviderRequestContext {
        model_profile: request.selection.model_profile,
        auth_profile: request.selection.auth_profile.clone(),
        auth_pool: request.selection.auth_pool.clone(),
        auth_pool_routing: selected_auth_pool_routing(
            request.config,
            request.selection.auth_pool.as_deref(),
        ),
        auth_pool_selection_reason: None,
        settings: request.selection.settings,
        auth: None,
        auth_candidates: Vec::new(),
        request: request.selection.request,
        env: BTreeMap::new(),
    };

    if let Some(auth_profile_name) = request.selection.auth_profile.as_deref()
        && let Some(auth_profile) = request.config.auth.profiles.get(auth_profile_name)
    {
        let resolved = resolve_auth_profile(auth_profile_name, auth_profile);
        context.env = resolved.env;
        context.auth = Some(resolved.auth);
    }

    if let Some(auth_pool_name) = request.selection.auth_pool.as_deref() {
        let mut candidates = Vec::new();
        let mut seen = BTreeSet::new();
        if let Some(auth_profile_name) = request.selection.auth_profile.as_deref() {
            push_config_auth_candidate(
                request.config,
                auth_profile_name,
                &mut candidates,
                &mut seen,
            );
        }
        if let Some(auth_pool) = request.config.auth.pools.get(auth_pool_name) {
            for profile_name in &auth_pool.profiles {
                push_config_auth_candidate(
                    request.config,
                    profile_name,
                    &mut candidates,
                    &mut seen,
                );
            }
        }
        let registry = bcode_config::load_runtime_auth_subscriptions();
        if let Some(pool) = registry.pools.get(auth_pool_name) {
            for profile in &pool.profiles {
                if !seen.insert(profile.auth_profile.clone()) {
                    continue;
                }
                let auth_profile = runtime_subscription_auth_profile_config(profile);
                let resolved = resolve_auth_profile(&profile.auth_profile, &auth_profile);
                candidates.push(bcode_model::ProviderAuthCandidate {
                    profile: Some(profile.auth_profile.clone()),
                    auth: resolved.auth,
                    env: resolved.env,
                });
            }
        }
        context.auth_candidates = candidates;
    }

    context
}

fn push_config_auth_candidate(
    config: &bcode_config::BcodeConfig,
    auth_profile_name: &str,
    candidates: &mut Vec<bcode_model::ProviderAuthCandidate>,
    seen: &mut BTreeSet<String>,
) {
    if !seen.insert(auth_profile_name.to_string()) {
        return;
    }
    if let Some(auth_profile) = config.auth.profiles.get(auth_profile_name) {
        let resolved = resolve_auth_profile(auth_profile_name, auth_profile);
        candidates.push(bcode_model::ProviderAuthCandidate {
            profile: Some(auth_profile_name.to_string()),
            auth: resolved.auth,
            env: resolved.env,
        });
    }
}

fn runtime_subscription_auth_profile_config(
    profile: &bcode_config::RuntimeAuthSubscriptionProfile,
) -> bcode_config::AuthProfileConfig {
    bcode_config::AuthProfileConfig {
        backend: "sshenv".to_string(),
        provider_id: Some(profile.provider.clone()),
        owner_plugin_id: profile.owner_plugin_id.clone(),
        scheme: Some(profile.scheme.clone()),
        map: profile.map.clone(),
        settings: {
            let mut settings = BTreeMap::from([
                ("provider".to_string(), profile.provider.clone()),
                ("profile".to_string(), profile.storage_profile.clone()),
                ("vault".to_string(), profile.vault.display().to_string()),
                ("mode".to_string(), profile.scheme.clone()),
            ]);
            if let Some(device_seal) = &profile.device_seal {
                settings.insert("device_seal".to_owned(), device_seal.clone());
            }
            settings
        },
    }
}

/// Resolved provider-owned authentication profile metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAuthProfile {
    pub profile_name: String,
    pub provider_id: String,
    pub owner_plugin_id: String,
    pub profile: bcode_config::AuthProfileConfig,
    pub source: AuthProfileSource,
}

/// Source selected by generic provider-to-profile resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProfileSource {
    Declarative,
    Runtime,
}

/// Generic provider-to-profile resolution failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthProfileResolutionError {
    #[error("auth provider and plugin IDs must not be empty")]
    InvalidOwner,
    #[error("auth profile '{profile}' is not configured for provider '{provider_id}'")]
    MissingProfile {
        provider_id: String,
        profile: String,
    },
    #[error("auth profile '{profile}' belongs to provider '{actual}', not '{expected}'")]
    ProviderMismatch {
        profile: String,
        expected: String,
        actual: String,
    },
    #[error("auth profile '{profile}' belongs to plugin '{actual}', not '{expected}'")]
    OwnerMismatch {
        profile: String,
        expected: String,
        actual: String,
    },
}

/// Resolve an auth profile for one registered provider with declarative precedence.
///
/// An explicit profile wins over bindings. Otherwise a declarative binding is used, then a
/// same-named declarative profile, then a runtime binding/profile. Runtime metadata never
/// overrides a declarative profile of the same name.
///
/// # Errors
///
/// Returns an error for missing profiles or provider/plugin ownership mismatch.
pub fn resolve_auth_provider_profile(
    config: &bcode_config::BcodeConfig,
    provider_id: &str,
    owner_plugin_id: &str,
    explicit_profile: Option<&str>,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
) -> Result<ResolvedAuthProfile, AuthProfileResolutionError> {
    if provider_id.trim().is_empty() || owner_plugin_id.trim().is_empty() {
        return Err(AuthProfileResolutionError::InvalidOwner);
    }
    let declarative_binding = config
        .auth
        .bindings
        .get(provider_id)
        .and_then(|binding| binding.profile.as_deref());
    let runtime_binding = runtime.bindings.get(provider_id);
    let profile_name = explicit_profile
        .or(declarative_binding)
        .or_else(|| {
            config
                .auth
                .profiles
                .contains_key(provider_id)
                .then_some(provider_id)
        })
        .or_else(|| runtime_binding.map(|binding| binding.profile.as_str()))
        .unwrap_or(provider_id);

    if let Some(profile) = config.auth.profiles.get(profile_name) {
        validate_auth_profile_ownership(profile_name, profile, provider_id, owner_plugin_id)?;
        return Ok(ResolvedAuthProfile {
            profile_name: profile_name.to_string(),
            provider_id: provider_id.to_string(),
            owner_plugin_id: owner_plugin_id.to_string(),
            profile: profile.clone(),
            source: AuthProfileSource::Declarative,
        });
    }

    let runtime_profile = runtime.profiles.get(profile_name).ok_or_else(|| {
        AuthProfileResolutionError::MissingProfile {
            provider_id: provider_id.to_string(),
            profile: profile_name.to_string(),
        }
    })?;
    if runtime_profile.provider_id != provider_id {
        return Err(AuthProfileResolutionError::ProviderMismatch {
            profile: profile_name.to_string(),
            expected: provider_id.to_string(),
            actual: runtime_profile.provider_id.clone(),
        });
    }
    if runtime_profile.owner_plugin_id != owner_plugin_id {
        return Err(AuthProfileResolutionError::OwnerMismatch {
            profile: profile_name.to_string(),
            expected: owner_plugin_id.to_string(),
            actual: runtime_profile.owner_plugin_id.clone(),
        });
    }
    Ok(ResolvedAuthProfile {
        profile_name: profile_name.to_string(),
        provider_id: provider_id.to_string(),
        owner_plugin_id: owner_plugin_id.to_string(),
        profile: bcode_config::AuthProfileConfig {
            backend: runtime_profile.backend.clone(),
            provider_id: Some(provider_id.to_string()),
            owner_plugin_id: Some(owner_plugin_id.to_string()),
            scheme: Some(runtime_profile.scheme.clone()),
            map: runtime_profile.map.clone(),
            settings: {
                let mut settings = BTreeMap::from([
                    (
                        "profile".to_string(),
                        runtime_profile.storage_profile.clone(),
                    ),
                    (
                        "vault".to_string(),
                        runtime_profile.vault.display().to_string(),
                    ),
                ]);
                if let Some(device_seal) = &runtime_profile.device_seal {
                    settings.insert("device_seal".to_owned(), device_seal.clone());
                }
                settings
            },
        },
        source: AuthProfileSource::Runtime,
    })
}

fn validate_auth_profile_ownership(
    profile_name: &str,
    profile: &bcode_config::AuthProfileConfig,
    provider_id: &str,
    owner_plugin_id: &str,
) -> Result<(), AuthProfileResolutionError> {
    if let Some(actual) = &profile.provider_id
        && actual != provider_id
    {
        return Err(AuthProfileResolutionError::ProviderMismatch {
            profile: profile_name.to_string(),
            expected: provider_id.to_string(),
            actual: actual.clone(),
        });
    }
    if let Some(actual) = &profile.owner_plugin_id
        && actual != owner_plugin_id
    {
        return Err(AuthProfileResolutionError::OwnerMismatch {
            profile: profile_name.to_string(),
            expected: owner_plugin_id.to_string(),
            actual: actual.clone(),
        });
    }
    Ok(())
}

/// Auth material and compatibility environment resolved for a selected profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedProviderAuth {
    pub auth: bcode_model::ProviderAuthContext,
    pub env: BTreeMap<String, String>,
}

/// Resolve one configured auth profile.
#[must_use]
pub fn resolve_auth_profile(
    auth_profile_name: &str,
    auth_profile: &bcode_config::AuthProfileConfig,
) -> ResolvedProviderAuth {
    let mut env = BTreeMap::new();
    let mut storage_profile = auth_profile_name.to_string();
    let mut storage_vault = None;

    let mut diagnostics = Vec::new();
    match auth_profile.backend.as_str() {
        "sshenv" => {
            let vault = auth_profile
                .settings
                .get("vault")
                .map_or_else(bcode_config::default_auth_vault_path, PathBuf::from);
            let profile = auth_profile
                .settings
                .get("profile")
                .map_or(auth_profile_name, String::as_str);
            storage_profile = profile.to_string();
            storage_vault = Some(vault.display().to_string());
            let options = security::device_seal_options_for_auth_profile(auth_profile);
            let recipient_key = auth_profile
                .settings
                .get("recipient_key")
                .map(String::as_str)
                .map_or_else(
                    || security::ensure_vault_recipient_key(&vault).ok(),
                    |key| Some(key.to_string()),
                );
            let report = security::reconcile_auth_vault_security_report_with_options(
                &vault,
                profile,
                options,
                recipient_key.as_deref(),
            );
            diagnostics.extend(report.diagnostics);
            match security::read_auth_vault_profile(&vault, profile) {
                Ok(Some(profile_env)) => {
                    for (key, value) in profile_env {
                        env.entry(key).or_insert(value);
                    }
                }
                Ok(None) => {}
                Err(error) => diagnostics.push(security::AuthSecurityDiagnostic {
                    severity: security::AuthSecurityDiagnosticSeverity::Warning,
                    code: "auth_vault_profile_unavailable".to_string(),
                    message: error,
                    remediation: Some(
                        "Run `bcode login` to recreate this profile using the Bcode-managed per-vault key."
                            .to_string(),
                    ),
                }),
            }
            merge_metadata_env(auth_profile, profile, &vault, &mut env);
            merge_mapped_process_env(auth_profile, &mut env);
            merge_settings_env(auth_profile, &mut env);
        }
        "aws" | "aws_default_chain" => merge_settings_env(auth_profile, &mut env),
        _ => {}
    }

    let auth = provider_auth_context(
        auth_profile_name,
        auth_profile,
        &storage_profile,
        storage_vault.as_deref(),
        &env,
        diagnostics,
    );
    ResolvedProviderAuth { auth, env }
}

fn merge_metadata_env(
    auth_profile: &bcode_config::AuthProfileConfig,
    profile: &str,
    vault: &std::path::Path,
    env: &mut BTreeMap<String, String>,
) {
    match auth_profile.settings.get("provider").map(String::as_str) {
        Some("openai") => {
            env.entry("BCODE_OPENAI_AUTH_PROFILE".to_string())
                .or_insert_with(|| profile.to_string());
            env.entry("BCODE_OPENAI_AUTH_VAULT".to_string())
                .or_insert_with(|| vault.display().to_string());
        }
        Some("xai" | "grok") => {
            env.entry("BCODE_XAI_AUTH_PROFILE".to_string())
                .or_insert_with(|| profile.to_string());
            env.entry("BCODE_XAI_AUTH_VAULT".to_string())
                .or_insert_with(|| vault.display().to_string());
        }
        _ => {}
    }
}

fn merge_mapped_process_env(
    auth_profile: &bcode_config::AuthProfileConfig,
    env: &mut BTreeMap<String, String>,
) {
    for source_key in auth_credential_source_keys(auth_profile).values() {
        if let Ok(value) = std::env::var(source_key)
            && !value.trim().is_empty()
        {
            env.entry(source_key.clone()).or_insert(value);
        }
    }
}

fn merge_settings_env(
    auth_profile: &bcode_config::AuthProfileConfig,
    env: &mut BTreeMap<String, String>,
) {
    for (key, value) in &auth_profile.settings {
        if let Some(env_key) = key.strip_prefix("env.") {
            env.entry(env_key.to_string())
                .or_insert_with(|| value.clone());
        }
    }
    match auth_profile.settings.get("provider").map(String::as_str) {
        Some("openai") => {
            copy_setting_to_env(auth_profile, env, "mode", "BCODE_OPENAI_AUTH_MODE");
            copy_setting_to_env(auth_profile, env, "base_url", "BCODE_OPENAI_BASE_URL");
        }
        Some("xai" | "grok") => {
            copy_setting_to_env(auth_profile, env, "base_url", "BCODE_XAI_BASE_URL");
        }
        Some("aws" | "bedrock") => {
            copy_setting_to_env(auth_profile, env, "profile", "AWS_PROFILE");
            copy_setting_to_env(auth_profile, env, "profile", "BCODE_BEDROCK_AWS_PROFILE");
            copy_setting_to_env(auth_profile, env, "region", "AWS_REGION");
            copy_setting_to_env(auth_profile, env, "region", "BCODE_BEDROCK_REGION");
            copy_setting_to_env(
                auth_profile,
                env,
                "endpoint_url",
                "BCODE_BEDROCK_ENDPOINT_URL",
            );
        }
        _ => {}
    }
}

fn copy_setting_to_env(
    auth_profile: &bcode_config::AuthProfileConfig,
    env: &mut BTreeMap<String, String>,
    setting_key: &str,
    env_key: &str,
) {
    if let Some(value) = auth_profile.settings.get(setting_key) {
        env.entry(env_key.to_string())
            .or_insert_with(|| value.clone());
    }
}

fn provider_auth_context(
    auth_profile_name: &str,
    auth_profile: &bcode_config::AuthProfileConfig,
    storage_profile: &str,
    storage_vault: Option<&str>,
    env: &BTreeMap<String, String>,
    diagnostics: Vec<security::AuthSecurityDiagnostic>,
) -> bcode_model::ProviderAuthContext {
    let source_keys = auth_credential_source_keys(auth_profile);
    let credentials = source_keys
        .iter()
        .filter_map(|(credential, source_key)| {
            env.get(source_key)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    (
                        credential.clone(),
                        bcode_model::ProviderAuthCredential {
                            value: value.clone(),
                            source: Some(source_key.clone()),
                        },
                    )
                })
        })
        .collect::<BTreeMap<_, _>>();
    let storage = source_keys
        .into_iter()
        .map(|(credential, source_key)| {
            (
                credential,
                bcode_model::ProviderAuthStorageRef {
                    backend: auth_profile.backend.clone(),
                    profile: storage_profile.to_string(),
                    key: source_key,
                    vault: storage_vault.map(ToString::to_string),
                },
            )
        })
        .collect();
    bcode_model::ProviderAuthContext {
        profile: Some(auth_profile_name.to_string()),
        backend: Some(auth_profile.backend.clone()),
        scheme: auth_profile
            .scheme
            .clone()
            .or_else(|| auth_profile.settings.get("mode").cloned())
            .or_else(|| (!credentials.is_empty()).then(|| "api_key".to_string())),
        credentials,
        attributes: auth_profile.settings.clone(),
        storage,
        diagnostics: diagnostics
            .into_iter()
            .map(|diagnostic| bcode_model::ProviderAuthDiagnostic {
                severity: diagnostic.severity.as_str().to_string(),
                code: diagnostic.code,
                message: diagnostic.message,
                remediation: diagnostic.remediation,
            })
            .collect(),
    }
}

fn auth_credential_source_keys(
    auth_profile: &bcode_config::AuthProfileConfig,
) -> BTreeMap<String, String> {
    let mut source_keys = auth_profile
        .map
        .iter()
        .filter_map(|(credential, mapping)| {
            mapping
                .env
                .as_ref()
                .or(mapping.key.as_ref())
                .filter(|key| !key.trim().is_empty())
                .map(|key| (credential.clone(), key.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    if let Some(api_key_env) = auth_profile
        .settings
        .get("api_key_env")
        .filter(|value| !value.trim().is_empty())
    {
        source_keys
            .entry("api_key".to_string())
            .or_insert_with(|| api_key_env.clone());
    }
    if matches!(
        auth_profile.settings.get("provider").map(String::as_str),
        Some("aws" | "bedrock")
    ) {
        for (credential, key) in [
            ("access_key_id", "AWS_ACCESS_KEY_ID"),
            ("secret_access_key", "AWS_SECRET_ACCESS_KEY"),
            ("session_token", "AWS_SESSION_TOKEN"),
            ("bearer_token", "AWS_BEARER_TOKEN_BEDROCK"),
        ] {
            source_keys
                .entry(credential.to_string())
                .or_insert_with(|| key.to_string());
        }
    }
    if auth_profile
        .settings
        .get("mode")
        .is_some_and(|mode| mode == "chatgpt")
    {
        for (credential, key) in [
            ("access_token", "BCODE_OPENAI_CODEX_ACCESS_TOKEN"),
            ("refresh_token", "BCODE_OPENAI_CODEX_REFRESH_TOKEN"),
            ("id_token", "BCODE_OPENAI_CODEX_ID_TOKEN"),
            ("expires_at", "BCODE_OPENAI_CODEX_EXPIRES_AT"),
            ("account_id", "BCODE_OPENAI_CODEX_ACCOUNT_ID"),
        ] {
            source_keys
                .entry(credential.to_string())
                .or_insert_with(|| key.to_string());
        }
    }
    source_keys
}

fn selected_auth_pool_routing(
    config: &bcode_config::BcodeConfig,
    auth_pool: Option<&str>,
) -> bcode_model::ProviderAuthPoolRouting {
    let Some(auth_pool) = auth_pool else {
        return bcode_model::ProviderAuthPoolRouting::default();
    };
    let Some(pool) = config.auth.pools.get(auth_pool) else {
        return bcode_model::ProviderAuthPoolRouting::default();
    };
    let provider_plugin_id = pool.provider_plugin_id.as_deref();
    let mut required_windows = pool.priming.required_windows.clone();
    apply_default_priming_required_windows(auth_pool, provider_plugin_id, &mut required_windows);
    bcode_model::ProviderAuthPoolRouting {
        strategy: Some(match pool.strategy {
            bcode_config::AuthPoolStrategy::Failover => "failover".to_string(),
            bcode_config::AuthPoolStrategy::RoundRobin => "round_robin".to_string(),
        }),
        priming_enabled: pool.priming.enabled,
        priming_include_primary: pool.priming.include_primary,
        priming_reprime_after: pool.priming.reprime_after.clone(),
        priming_provider_windows: pool.priming.provider_windows,
        priming_fallback_reprime_after: pool.priming.fallback_reprime_after.clone(),
        priming_required_windows: required_windows,
    }
}

fn apply_default_priming_required_windows(
    pool: &str,
    provider_plugin_id: Option<&str>,
    required_windows: &mut BTreeMap<String, Vec<String>>,
) {
    if !required_windows.is_empty() {
        return;
    }
    if pool == "openai" || provider_plugin_id == Some("bcode.openai-compatible") {
        required_windows.insert(
            "codex".to_string(),
            vec!["primary".to_string(), "secondary".to_string()],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarative_binding_precedes_runtime_and_enforces_ownership() {
        let declarative_profile = bcode_config::AuthProfileConfig {
            backend: "sshenv".to_owned(),
            provider_id: Some("exa".to_owned()),
            owner_plugin_id: Some("bcode.web-search".to_owned()),
            scheme: Some("api_key".to_owned()),
            map: BTreeMap::new(),
            settings: BTreeMap::new(),
        };
        let config = bcode_config::BcodeConfig {
            auth: bcode_config::AuthConfig {
                profiles: BTreeMap::from([("exa-work".to_owned(), declarative_profile)]),
                bindings: BTreeMap::from([(
                    "exa".to_owned(),
                    bcode_config::AuthBindingConfig {
                        profile: Some("exa-work".to_owned()),
                    },
                )]),
                ..bcode_config::AuthConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let runtime = bcode_config::RuntimeAuthSubscriptions {
            bindings: BTreeMap::from([(
                "exa".to_owned(),
                bcode_config::RuntimeAuthBinding {
                    profile: "runtime-exa".to_owned(),
                    owner_plugin_id: "bcode.web-search".to_owned(),
                },
            )]),
            ..bcode_config::RuntimeAuthSubscriptions::default()
        };

        let resolved =
            resolve_auth_provider_profile(&config, "exa", "bcode.web-search", None, &runtime)
                .expect("declarative binding resolves");
        assert_eq!(resolved.profile_name, "exa-work");
        assert_eq!(resolved.source, AuthProfileSource::Declarative);
        assert!(matches!(
            resolve_auth_provider_profile(&config, "exa", "bcode.other", None, &runtime),
            Err(AuthProfileResolutionError::OwnerMismatch { .. })
        ));
    }

    #[test]
    fn runtime_binding_resolves_only_without_declarative_profile() {
        let runtime = bcode_config::RuntimeAuthSubscriptions {
            bindings: BTreeMap::from([(
                "exa".to_owned(),
                bcode_config::RuntimeAuthBinding {
                    profile: "exa".to_owned(),
                    owner_plugin_id: "bcode.web-search".to_owned(),
                },
            )]),
            profiles: BTreeMap::from([(
                "exa".to_owned(),
                bcode_config::RuntimeAuthProfile {
                    provider_id: "exa".to_owned(),
                    owner_plugin_id: "bcode.web-search".to_owned(),
                    backend: "sshenv".to_owned(),
                    scheme: "api_key".to_owned(),
                    storage_profile: "exa".to_owned(),
                    vault: PathBuf::from("/vault"),
                    map: BTreeMap::from([(
                        "api_key".to_owned(),
                        bcode_config::AuthCredentialMapping {
                            env: None,
                            key: Some("TEST_PROVIDER_API_KEY".to_owned()),
                        },
                    )]),
                    device_seal: Some("off".to_owned()),
                },
            )]),
            ..bcode_config::RuntimeAuthSubscriptions::default()
        };

        let resolved = resolve_auth_provider_profile(
            &bcode_config::BcodeConfig::default(),
            "exa",
            "bcode.web-search",
            None,
            &runtime,
        )
        .expect("runtime profile resolves");
        assert_eq!(resolved.source, AuthProfileSource::Runtime);
        assert_eq!(resolved.profile.provider_id.as_deref(), Some("exa"));
        assert_eq!(
            resolved
                .profile
                .settings
                .get("device_seal")
                .map(String::as_str),
            Some("off")
        );
        assert_eq!(
            resolved
                .profile
                .map
                .get("api_key")
                .and_then(|mapping| mapping.key.as_deref()),
            Some("TEST_PROVIDER_API_KEY")
        );
    }

    #[test]
    fn mapped_api_key_uses_canonical_credential_name() {
        let profile = bcode_config::AuthProfileConfig {
            backend: "sshenv".to_string(),
            provider_id: None,
            owner_plugin_id: None,
            scheme: Some("api_key".to_string()),
            map: BTreeMap::from([(
                "api_key".to_string(),
                bcode_config::AuthCredentialMapping {
                    env: Some("TEST_PROVIDER_KEY".to_string()),
                    key: None,
                },
            )]),
            settings: BTreeMap::new(),
        };
        unsafe {
            std::env::set_var("TEST_PROVIDER_KEY", "secret");
        }
        let resolved = resolve_auth_profile("test", &profile);
        unsafe {
            std::env::remove_var("TEST_PROVIDER_KEY");
        }
        assert_eq!(
            resolved
                .auth
                .credentials
                .get("api_key")
                .map(|credential| credential.value.as_str()),
            Some("secret")
        );
        assert_eq!(
            resolved
                .auth
                .storage
                .get("api_key")
                .map(|storage| storage.key.as_str()),
            Some("TEST_PROVIDER_KEY")
        );
    }

    #[test]
    fn openai_pool_priming_uses_codex_window_defaults() {
        let config = bcode_config::BcodeConfig {
            auth: bcode_config::AuthConfig {
                pools: BTreeMap::from([(
                    "openai".to_string(),
                    bcode_config::AuthPoolConfig {
                        provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                        priming: bcode_config::AuthPoolPrimingConfig {
                            enabled: true,
                            ..bcode_config::AuthPoolPrimingConfig::default()
                        },
                        ..bcode_config::AuthPoolConfig::default()
                    },
                )]),
                ..bcode_config::AuthConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };

        let routing = selected_auth_pool_routing(&config, Some("openai"));

        assert!(routing.priming_enabled);
        assert_eq!(
            routing.priming_required_windows.get("codex"),
            Some(&vec!["primary".to_string(), "secondary".to_string()])
        );
    }

    #[test]
    fn explicit_priming_windows_override_openai_defaults() {
        let config = bcode_config::BcodeConfig {
            auth: bcode_config::AuthConfig {
                pools: BTreeMap::from([(
                    "openai".to_string(),
                    bcode_config::AuthPoolConfig {
                        provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                        priming: bcode_config::AuthPoolPrimingConfig {
                            required_windows: BTreeMap::from([(
                                "custom".to_string(),
                                vec!["daily".to_string()],
                            )]),
                            ..bcode_config::AuthPoolPrimingConfig::default()
                        },
                        ..bcode_config::AuthPoolConfig::default()
                    },
                )]),
                ..bcode_config::AuthConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };

        let routing = selected_auth_pool_routing(&config, Some("openai"));

        assert_eq!(
            routing.priming_required_windows,
            BTreeMap::from([("custom".to_string(), vec!["daily".to_string()])])
        );
    }
}
