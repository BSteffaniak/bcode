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
        scheme: Some(profile.scheme.clone()),
        map: BTreeMap::new(),
        settings: BTreeMap::from([
            ("provider".to_string(), profile.provider.clone()),
            ("profile".to_string(), profile.storage_profile.clone()),
            ("vault".to_string(), profile.vault.display().to_string()),
            ("mode".to_string(), "chatgpt".to_string()),
        ]),
    }
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
    fn mapped_api_key_uses_canonical_credential_name() {
        let profile = bcode_config::AuthProfileConfig {
            backend: "sshenv".to_string(),
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
