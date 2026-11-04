#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Per-plugin configuration for bcode hosts.
//!
//! A host loading plugins gives each one its configuration section from
//! `bcode.toml` (with secret references left as references), a private state
//! directory, and a resolver that supplies the credentials the plugin owns
//! (from `[auth]` profiles, runtime auth bindings such as `bcode auth login exa`,
//! or explicit `env`/`sshenv` references) when a service is invoked. The
//! daemon and embedded SDK hosts both use [`resolve_plugin_configs`], so a
//! plugin behaves the same wherever it runs.

use std::path::{Path, PathBuf};

/// Configuration for every known plugin, statically bundled and discovered.
///
/// Each gets its `bcode.toml` section with secret references kept as
/// references, a redacted copy for diagnostics, a state directory under
/// `state_base`, and a resolver that supplies the credentials the plugin owns
/// at invocation time. This is what the daemon gives plugins; embedded hosts
/// use it so plugins behave the same in both.
#[must_use]
pub fn resolve_plugin_configs(
    config: &bcode_config::BcodeConfig,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    state_base: &Path,
) -> std::collections::BTreeMap<String, bcode_plugin::ResolvedPluginConfig> {
    let mut manifests = std::collections::BTreeMap::new();
    for plugin in static_plugins {
        if let Ok(manifest) = toml::from_str::<bcode_plugin::PluginManifest>(plugin.manifest_toml) {
            manifests.insert(manifest.id.clone(), manifest);
        }
    }
    if let Ok(discovered) = bcode_plugin::discover_plugins() {
        for plugin in discovered {
            manifests
                .entry(plugin.manifest.id.clone())
                .or_insert(plugin.manifest);
        }
    }
    manifests
        .values()
        .map(|manifest| {
            let raw = resolved_plugin_config_value(config, manifest);
            let delivered = sanitize_plugin_config_value(&raw);
            let redacted = redact_plugin_config_value(&raw);
            let plugin_id = manifest.id.clone();
            let config = config.clone();
            let resolver_source = raw;
            let resolver: bcode_plugin::PluginSecretResolver =
                std::sync::Arc::new(move |_interface_id, _operation, _delivered_config| {
                    resolve_invocation_secrets(&plugin_id, &config, &resolver_source)
                });
            let plugin_config = bcode_plugin::ResolvedPluginConfig::new(delivered, redacted)
                .with_state_root(plugin_state_root(state_base, &manifest.id))
                .with_secret_resolver(resolver);
            (manifest.id.clone(), plugin_config)
        })
        .collect()
}

/// The state directory for `plugin_id` under `base`, with the id
/// percent-encoded so every plugin gets one safe path component.
#[must_use]
pub fn plugin_state_root(base: &Path, plugin_id: &str) -> PathBuf {
    let encoded_id = plugin_id
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
                vec![char::from(byte)].into_iter()
            } else {
                format!("%{byte:02X}")
                    .chars()
                    .collect::<Vec<_>>()
                    .into_iter()
            }
        })
        .collect::<String>();
    base.join(encoded_id)
}

fn resolved_plugin_config_value(
    config: &bcode_config::BcodeConfig,
    manifest: &bcode_plugin::PluginManifest,
) -> serde_json::Value {
    let mut value = serde_json::Value::Object(serde_json::Map::new());
    if let Some(section) = manifest
        .config
        .as_ref()
        .and_then(|config| config.section.as_deref())
        && section == "web_search"
    {
        merge_json_values(&mut value, toml_value_to_json(&config.web_search));
    }
    if let Some(plugin_value) = config.plugins.config.get(&manifest.id) {
        merge_json_values(&mut value, toml_value_to_json(plugin_value));
    }
    value
}

/// Plugin configuration as delivered to the plugin: secret references stay
/// references (resolved only at invocation time).
#[must_use]
pub fn sanitize_plugin_config_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) if secret_ref_backend(map).is_some() => {
            serde_json::Value::Object(map.clone())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), sanitize_plugin_config_value(value)))
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(sanitize_plugin_config_value).collect())
        }
        value => value.clone(),
    }
}

fn canonical_plugin_secret_id(plugin_id: &str, provider_id: &str, credential_id: &str) -> String {
    format!("{plugin_id}/{provider_id}/{credential_id}")
}

/// Credentials `plugin_id` owns, keyed `plugin/provider/credential`.
///
/// They come from explicit secret references in its configuration, `[auth]`
/// profiles it owns, and runtime auth bindings (such as `bcode auth login
/// exa`). They are read live, so rotated credentials apply to the next
/// invocation.
///
/// # Errors
///
/// * A secret reference is malformed or its backend is unsupported.
/// * A referenced `sshenv` profile or credential cannot be read.
/// * An auth profile or runtime binding's ownership does not match the plugin.
pub fn resolve_invocation_secrets(
    plugin_id: &str,
    config: &bcode_config::BcodeConfig,
    value: &serde_json::Value,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let runtime = bcode_config::load_runtime_auth_subscriptions();
    resolve_invocation_secrets_with_runtime(plugin_id, config, value, &runtime)
}

/// [`resolve_invocation_secrets`] with explicit runtime auth bindings.
///
/// # Errors
///
/// * The same conditions as [`resolve_invocation_secrets`].
/// * A runtime binding references a missing profile.
pub fn resolve_invocation_secrets_with_runtime(
    plugin_id: &str,
    config: &bcode_config::BcodeConfig,
    value: &serde_json::Value,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut secrets = std::collections::BTreeMap::new();
    collect_explicit_secret_refs(plugin_id, value, &mut Vec::new(), &mut secrets)?;
    for (provider_id, binding) in &config.auth.bindings {
        if let Some(profile_name) = binding.profile.as_deref().or(Some(provider_id.as_str()))
            && let Some(profile) = config.auth.profiles.get(profile_name)
            && profile.owner_plugin_id.as_deref() == Some(plugin_id)
        {
            collect_profile_secrets(plugin_id, provider_id, profile_name, profile, &mut secrets)?;
        }
    }
    for (provider_id, binding) in &runtime.bindings {
        if binding.owner_plugin_id != plugin_id {
            continue;
        }
        if config.auth.bindings.contains_key(provider_id)
            || config.auth.profiles.contains_key(&binding.profile)
        {
            continue;
        }
        let Some(profile) = runtime.profiles.get(&binding.profile) else {
            return Err(format!(
                "runtime auth binding '{provider_id}' references missing profile '{}'",
                binding.profile
            ));
        };
        if profile.owner_plugin_id != plugin_id || profile.provider_id != *provider_id {
            return Err(format!(
                "runtime auth binding '{provider_id}' has inconsistent plugin/provider ownership"
            ));
        }
        let config_profile = bcode_config::AuthProfileConfig {
            backend: profile.backend.clone(),
            provider_id: Some(profile.provider_id.clone()),
            owner_plugin_id: Some(profile.owner_plugin_id.clone()),
            scheme: Some(profile.scheme.clone()),
            map: profile.map.clone(),
            settings: std::collections::BTreeMap::from([
                ("profile".to_owned(), profile.storage_profile.clone()),
                ("vault".to_owned(), profile.vault.display().to_string()),
            ]),
        };
        collect_profile_secrets(
            plugin_id,
            provider_id,
            &binding.profile,
            &config_profile,
            &mut secrets,
        )?;
    }
    Ok(secrets)
}

fn collect_profile_secrets(
    plugin_id: &str,
    provider_id: &str,
    profile_name: &str,
    profile: &bcode_config::AuthProfileConfig,
    secrets: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    if profile.owner_plugin_id.as_deref() != Some(plugin_id)
        || profile.provider_id.as_deref() != Some(provider_id)
    {
        return Err(format!(
            "auth profile '{profile_name}' ownership does not match plugin/provider"
        ));
    }
    let resolved = bcode_provider_auth::resolve_auth_profile(profile_name, profile);
    for (credential_id, mapping) in &profile.map {
        let source_key = mapping.env.as_ref().or(mapping.key.as_ref());
        if let Some(source_key) = source_key
            && let Ok(value) = std::env::var(source_key)
            && !value.trim().is_empty()
        {
            secrets
                .entry(canonical_plugin_secret_id(
                    plugin_id,
                    provider_id,
                    credential_id,
                ))
                .or_insert(value);
        }
    }
    for (credential_id, credential) in resolved.auth.credentials {
        let id = canonical_plugin_secret_id(plugin_id, provider_id, &credential_id);
        secrets.entry(id).or_insert(credential.value);
    }
    Ok(())
}

fn collect_explicit_secret_refs(
    plugin_id: &str,
    value: &serde_json::Value,
    path: &mut Vec<String>,
    secrets: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    match value {
        serde_json::Value::Object(map) if secret_ref_backend(map).is_some() => {
            let provider_id = path.iter().rev().nth(1).map_or("config", String::as_str);
            let credential_id = path.last().map_or("secret", String::as_str);
            if let Some(secret) = resolve_secret_ref(map)? {
                secrets.insert(
                    canonical_plugin_secret_id(plugin_id, provider_id, credential_id),
                    secret,
                );
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                path.push(key.clone());
                collect_explicit_secret_refs(plugin_id, value, path, secrets)?;
                path.pop();
            }
            Ok(())
        }
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                path.push(index.to_string());
                collect_explicit_secret_refs(plugin_id, value, path, secrets)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn secret_ref_backend(map: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    match map.get("backend").and_then(serde_json::Value::as_str) {
        Some("env" | "sshenv") => map.get("backend").and_then(serde_json::Value::as_str),
        _ => None,
    }
}

fn resolve_secret_ref(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<String>, String> {
    let backend = map
        .get("backend")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "secret reference has no backend".to_owned())?;
    match backend {
        "env" => {
            let name = map
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "environment secret reference has no name".to_owned())?;
            Ok(std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty()))
        }
        "sshenv" => resolve_sshenv_secret_ref(map).map(Some),
        other => Err(format!("unsupported secret backend '{other}'")),
    }
}

fn resolve_sshenv_secret_ref(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    let profile = map
        .get("profile")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "sshenv secret reference has no profile".to_owned())?;
    let key = map
        .get("key")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(profile);
    let vault = map
        .get("vault")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            bcode_config::default_auth_vault_path,
            std::path::PathBuf::from,
        );
    let store = sshenv_vault::SshenvStore::new(
        sshenv_vault::SshenvStoreConfig::new(vault.clone()).with_private_key_paths(
            bcode_provider_auth::security::vault_private_key_paths(&vault),
        ),
    );
    store
        .get_profile(profile)
        .map_err(|error| format!("failed to read sshenv profile '{profile}': {error}"))?
        .and_then(|profile_env| profile_env.get(key).map(|value| value.as_str().to_string()))
        .ok_or_else(|| format!("sshenv profile '{profile}' has no credential '{key}'"))
}

fn toml_value_to_json(value: &toml::Value) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

fn merge_json_values(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge_json_values(base.entry(key).or_insert(serde_json::Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// A copy of plugin configuration with credential-looking values redacted
/// (secret references are kept: they are not secrets).
#[must_use]
pub fn redact_plugin_config_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) if secret_ref_backend(map).is_some() => {
            // Secret references are non-secret configuration. Preserve them so status and
            // remediation can explain the selected backend/profile without exposing a value.
            serde_json::Value::Object(map.clone())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| {
                    if value.as_object().and_then(secret_ref_backend).is_some() {
                        (key.clone(), value.clone())
                    } else if key.to_ascii_lowercase().contains("key")
                        || key.to_ascii_lowercase().contains("secret")
                        || key.to_ascii_lowercase().contains("token")
                    {
                        (
                            key.clone(),
                            serde_json::Value::String("<redacted>".to_string()),
                        )
                    } else {
                        (key.clone(), redact_plugin_config_value(value))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_plugin_config_value).collect())
        }
        value => value.clone(),
    }
}
