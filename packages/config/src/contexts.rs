//! Named configuration contexts resolved before provider or credential materialization.

use std::collections::BTreeMap;

use hyperchad_docs_config_derive::ConfigDoc;
use serde::{Deserialize, Serialize};

use crate::{AuthConfig, ConfigError, ModelConfig};

/// Declarative contexts. Map keys are stable IDs; labels are presentation only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "contexts")]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    /// Explicitly selected stable context ID. No context is selected implicitly.
    #[serde(default)]
    pub active: Option<String>,
    /// Independently scoped model and authentication configuration.
    #[serde(default)]
    #[config_doc(nested, map_key = "<context-id>")]
    pub entries: BTreeMap<String, ContextDefinition>,
}

/// One user-defined context. Global model/auth defaults are not inherited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "context_definition")]
#[serde(deny_unknown_fields)]
pub struct ContextDefinition {
    /// Human-readable label. Renaming it does not change credential identity.
    #[serde(default)]
    pub label: Option<String>,
    /// Context-local model defaults, profiles, aliases, and policy.
    #[serde(default)]
    #[config_doc(nested)]
    pub model: ModelConfig,
    /// Context-local account bindings and pools.
    #[serde(default)]
    #[config_doc(nested)]
    pub auth: AuthConfig,
}

/// Configuration prepared for discovery using one explicitly selected context account.
pub struct ContextModelDiscovery {
    /// Resolved request configuration; no ambient layers are loaded during preparation.
    pub config: crate::BcodeConfig,
    /// Stable context ID.
    pub context: String,
    /// Local account name for an eventual reviewed model-selection edit.
    pub account: String,
    /// Registered provider plugin declared by the account.
    pub provider_plugin_id: String,
}

/// Prepare discovery from a context snapshot and optional local account selection.
/// Uses the selected model's account, or the sole declared account, when no override is supplied.
///
/// # Errors
/// Rejects absent contexts/accounts, ambiguous account selection, and missing ownership.
pub fn prepare_model_discovery(
    config: &crate::BcodeConfig,
    account: Option<&str>,
) -> Result<ContextModelDiscovery, ConfigError> {
    let context = config
        .active_context
        .as_deref()
        .ok_or_else(|| invalid("select a context before discovering models"))?;
    let definition = config
        .contexts
        .as_ref()
        .and_then(|contexts| contexts.entries.get(context))
        .ok_or_else(|| invalid("selected context definition is missing"))?;
    let selected = definition
        .model
        .profile
        .as_ref()
        .and_then(|name| definition.model.profiles.get(name))
        .and_then(|profile| profile.auth_profile.as_deref())
        .or(definition.model.auth_profile.as_deref());
    let account = account.or(selected).or_else(|| {
        (definition.auth.profiles.len() == 1).then(|| definition.auth.profiles.keys().next().map(String::as_str)).flatten()
    }).ok_or_else(|| invalid("select an account before discovering models; multiple accounts are not chosen automatically"))?;
    let profile = definition
        .auth
        .profiles
        .get(account)
        .ok_or_else(|| invalid("selected discovery account is not declared"))?;
    let provider = profile
        .owner_plugin_id
        .as_deref()
        .filter(|owner| !owner.trim().is_empty())
        .ok_or_else(|| invalid("discovery account provider ownership is missing"))?;
    let mut snapshot = config.clone();
    let model = &mut snapshot
        .contexts
        .as_mut()
        .ok_or_else(|| invalid("missing contexts"))?
        .entries
        .get_mut(context)
        .ok_or_else(|| invalid("missing context"))?
        .model;
    model.provider_plugin_id = Some(provider.to_owned());
    model.auth_profile = Some(account.to_owned());
    model.profile = None;
    model.auth_pool = None;
    let mut value =
        toml::Value::try_from(snapshot).map_err(|_| invalid("cannot encode discovery snapshot"))?;
    resolve(&mut value)?;
    let config = crate::validate_config_value(value, "context discovery")?;
    Ok(ContextModelDiscovery {
        config,
        context: context.to_owned(),
        account: account.to_owned(),
        provider_plugin_id: provider.to_owned(),
    })
}

fn invalid(message: &str) -> ConfigError {
    ConfigError::Composition {
        message: message.to_owned(),
    }
}

fn validate_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty()
        || id.len() > 48
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
    {
        return Err(invalid(
            "context IDs require 1–48 lowercase ASCII letters, digits, hyphens, or underscores",
        ));
    }
    Ok(())
}

/// Qualify a local account or pool name with a stable context ID.
///
/// Length framing prevents ambiguous concatenation. This is an identity, never a path.
///
/// # Errors
/// Returns an error for an invalid context ID or empty local name.
pub fn qualify(context: &str, local: &str) -> Result<String, ConfigError> {
    validate_id(context)?;
    if local.is_empty()
        || local.starts_with("ctx-")
        || !local.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(invalid(
            "context-local account and pool IDs require lowercase ASCII letters, digits, dots, hyphens, or underscores and must not use the reserved ctx- prefix",
        ));
    }
    let qualified = format!("ctx-{}-{context}-{local}", context.len());
    if qualified.len() > 64 {
        return Err(invalid(
            "qualified context account or pool ID exceeds the 64-byte authentication contract limit",
        ));
    }
    Ok(qualified)
}

fn qualify_option(context: &str, value: &mut Option<String>) -> Result<(), ConfigError> {
    if let Some(name) = value {
        *name = qualify(context, name)?;
    }
    Ok(())
}

pub(crate) fn validate_effective(config: &crate::BcodeConfig) -> Result<(), ConfigError> {
    let declared = config
        .contexts
        .as_ref()
        .and_then(|contexts| contexts.active.as_deref());
    if declared != config.active_context.as_deref() {
        return Err(invalid(
            "effective context identity disagrees with selected context",
        ));
    }
    if let Some(active) = declared {
        validate_id(active)?;
        let mut resolved =
            toml::Value::try_from(config).map_err(|_| invalid("invalid effective context"))?;
        resolve(&mut resolved)?;
        let expected: crate::BcodeConfig = resolved
            .try_into()
            .map_err(|_| invalid("invalid resolved context"))?;
        if expected.auth != config.auth || expected.model != config.model {
            return Err(invalid(
                "effective model or auth configuration disagrees with selected context",
            ));
        }
    }
    Ok(())
}

pub(crate) fn apply_profile_override(
    value: &mut toml::Value,
    overrides: &crate::ConfigLoadOverrides,
) -> Result<(), ConfigError> {
    let Some(active) = value
        .get("active_context")
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(());
    };
    let mut selected = None;
    for raw in [
        overrides.env_config_toml.as_deref(),
        overrides.cli_config_toml.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let patch: toml::Value =
            toml::from_str(raw).map_err(|_| invalid("invalid model profile override"))?;
        if let Some(profile) = patch.get("model").and_then(|model| model.get("profile")) {
            selected = Some(profile.clone());
        }
    }
    if let Some(profile) = selected {
        value["contexts"]["entries"][&active]["model"]
            .as_table_mut()
            .ok_or_else(|| invalid("selected context model must be a table"))?
            .insert("profile".to_owned(), profile);
        resolve(value)?;
    }
    Ok(())
}

pub(crate) fn resolve(value: &mut toml::Value) -> Result<(), ConfigError> {
    let Some(raw) = value.get("contexts") else {
        return Ok(());
    };
    let contexts: ContextConfig = raw
        .clone()
        .try_into()
        .map_err(|_| invalid("invalid context definitions"))?;
    for id in contexts.entries.keys() {
        validate_id(id)?;
    }
    let Some(active) = contexts.active.as_deref() else {
        return Ok(());
    };
    validate_id(active)?;
    let definition = contexts.entries.get(active).ok_or_else(|| {
        invalid("selected context is not defined; no default context was substituted")
    })?;
    let mut auth = definition.auth.clone();
    if auth.openai.is_some() {
        return Err(invalid(
            "contexts require explicit auth profiles rather than the legacy OpenAI shortcut",
        ));
    }
    let mut profiles = BTreeMap::new();
    for (local, mut profile) in auth.profiles {
        let qualified = qualify(active, &local)?;
        // An explicit storage profile is an intentional credential reference. Otherwise
        // separate contexts must never address the same default vault entry.
        if profile.backend == "sshenv" {
            profile
                .settings
                .entry("profile".to_owned())
                .or_insert_with(|| qualified.clone());
        }
        profiles.insert(qualified, profile);
    }
    auth.profiles = profiles;
    for binding in auth.bindings.values_mut() {
        qualify_option(active, &mut binding.profile)?;
    }
    let mut pools = BTreeMap::new();
    for (local, mut pool) in auth.pools {
        qualify_option(active, &mut pool.preferred_profile)?;
        for profile in &mut pool.profiles {
            *profile = qualify(active, profile)?;
        }
        pools.insert(qualify(active, &local)?, pool);
    }
    auth.pools = pools;
    let mut model = definition.model.clone();
    qualify_option(active, &mut model.auth_profile)?;
    qualify_option(active, &mut model.auth_pool)?;
    for profile in model.profiles.values_mut() {
        qualify_option(active, &mut profile.auth_profile)?;
        qualify_option(active, &mut profile.auth_pool)?;
    }
    let root = value
        .as_table_mut()
        .ok_or_else(|| invalid("configuration must be a table"))?;
    root.insert(
        "model".to_owned(),
        toml::Value::try_from(model)
            .map_err(|_| invalid("cannot resolve context model configuration"))?,
    );
    root.insert(
        "auth".to_owned(),
        toml::Value::try_from(auth)
            .map_err(|_| invalid("cannot resolve context auth configuration"))?,
    );
    root.insert(
        "active_context".to_owned(),
        toml::Value::String(active.to_owned()),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(active: &str) -> crate::BcodeConfig {
        let raw: toml::Value = toml::from_str(&format!(
            r#"
[contexts]
active = "{active}"
[contexts.entries.alpha.model]
profile = "fast"
[contexts.entries.alpha.model.profiles.fast]
provider_plugin_id = "example.plugin"
model_id = "model-a"
auth_profile = "account"
auth_pool = "pool"
[contexts.entries.alpha.auth.profiles.account]
backend = "sshenv"
provider_id = "example"
owner_plugin_id = "example.plugin"
scheme = "oauth"
[contexts.entries.alpha.auth.pools.pool]
profiles = ["account"]
[contexts.entries.beta.model]
auth_profile = "account"
model_id = "model-b"
provider_plugin_id = "example.plugin"
[contexts.entries.beta.auth.profiles.account]
backend = "sshenv"
provider_id = "example"
owner_plugin_id = "example.plugin"
scheme = "oauth"
"#
        ))
        .unwrap();
        let (resolved, _) = crate::resolve_composed_config_value(&raw).unwrap();
        crate::validate_config_value(resolved, "test context").unwrap()
    }

    #[test]
    fn local_names_are_isolated_and_transport_preserves_resolution() {
        let alpha = fixture("alpha");
        let beta = fixture("beta");
        let a = qualify("alpha", "account").unwrap();
        let b = qualify("beta", "account").unwrap();
        assert_ne!(a, b);
        assert_eq!(alpha.auth.profiles[&a].settings["profile"], a);
        assert_eq!(beta.auth.profiles[&b].settings["profile"], b);
        assert!(!alpha.auth.profiles.contains_key(&b));
        assert_eq!(
            alpha
                .resolved_model_profile("fast")
                .unwrap()
                .auth_profile
                .as_deref(),
            Some(a.as_str())
        );
        let decoded =
            crate::decode_effective_config(&crate::encode_effective_config(&alpha).unwrap())
                .unwrap();
        assert_eq!(decoded.auth, alpha.auth);
        assert_eq!(decoded.active_context.as_deref(), Some("alpha"));
    }

    #[test]
    fn switching_contexts_replaces_selection_without_requalifying_transport() {
        let mut alpha = fixture("alpha");
        alpha.contexts.as_mut().unwrap().active = Some("beta".to_owned());
        let raw = toml::Value::try_from(&alpha).unwrap();
        let (resolved, _) = crate::resolve_composed_config_value(&raw).unwrap();
        let beta = crate::validate_config_value(resolved, "switch").unwrap();
        assert_eq!(beta.active_context.as_deref(), Some("beta"));
        assert_eq!(beta.model.model_id.as_deref(), Some("model-b"));
        assert!(
            !beta
                .auth
                .profiles
                .contains_key(&qualify("alpha", "account").unwrap())
        );
        assert_eq!(
            crate::decode_effective_config(&crate::encode_effective_config(&beta).unwrap())
                .unwrap()
                .auth,
            beta.auth
        );
    }

    #[test]
    fn explicit_storage_sharing_survives_context_resolution() {
        let mut config = fixture("alpha");
        let contexts = config.contexts.as_mut().unwrap();
        for definition in contexts.entries.values_mut() {
            definition
                .auth
                .profiles
                .get_mut("account")
                .unwrap()
                .settings
                .insert("profile".to_owned(), "shared-storage".to_owned());
        }
        for context in ["alpha", "beta"] {
            config.contexts.as_mut().unwrap().active = Some(context.to_owned());
            let (resolved, _) =
                crate::resolve_composed_config_value(&toml::Value::try_from(&config).unwrap())
                    .unwrap();
            let selected = crate::validate_config_value(resolved, "shared").unwrap();
            assert_eq!(
                selected.auth.profiles[&qualify(context, "account").unwrap()].settings["profile"],
                "shared-storage"
            );
        }
    }

    #[test]
    fn profile_override_targets_active_context_and_transport_rejects_mismatch() {
        let config = fixture("alpha");
        let mut value = toml::Value::try_from(&config).unwrap();
        let overrides = crate::ConfigLoadOverrides::default()
            .with_cli_config_toml(Some("[model]\nprofile = 'other'".to_owned()));
        apply_profile_override(&mut value, &overrides).unwrap();
        let changed: crate::BcodeConfig = value.try_into().unwrap();
        assert_eq!(changed.model.profile.as_deref(), Some("other"));
        assert_eq!(
            changed.contexts.as_ref().unwrap().entries["alpha"]
                .model
                .profile
                .as_deref(),
            Some("other")
        );
        assert!(
            crate::decode_effective_config(&crate::encode_effective_config(&changed).unwrap())
                .is_ok()
        );
        let mut forged = config;
        forged.active_context = Some("beta".to_owned());
        assert!(
            crate::decode_effective_config(&crate::encode_effective_config(&forged).unwrap())
                .is_err()
        );
        forged.active_context = Some("alpha".to_owned());
        forged.auth.profiles.clear();
        assert!(
            crate::decode_effective_config(&crate::encode_effective_config(&forged).unwrap())
                .is_err()
        );
    }

    #[test]
    fn discovery_uses_selected_account_and_does_not_reload_ambient_layers() {
        let mut config = fixture("alpha");
        let definition = config
            .contexts
            .as_mut()
            .unwrap()
            .entries
            .get_mut("alpha")
            .unwrap();
        let mut other = definition.auth.profiles["account"].clone();
        other.owner_plugin_id = Some("other.plugin".to_owned());
        definition.auth.profiles.insert("second".to_owned(), other);
        let discovery = prepare_model_discovery(&config, None).unwrap();
        assert_eq!(discovery.account, "account");
        assert_eq!(discovery.provider_plugin_id, "example.plugin");
        assert!(discovery.config.model.auth_pool.is_none());
        assert_eq!(
            discovery.config.model.auth_profile.as_deref(),
            Some(qualify("alpha", "account").unwrap().as_str())
        );
        assert!(
            crate::decode_effective_config(
                &crate::encode_effective_config(&discovery.config).unwrap()
            )
            .is_ok()
        );
        let selected = prepare_model_discovery(&config, Some("second")).unwrap();
        assert_eq!(selected.provider_plugin_id, "other.plugin");
        assert_eq!(
            config.contexts.as_ref().unwrap().entries["alpha"]
                .model
                .profile
                .as_deref(),
            Some("fast")
        );
        let definition = config
            .contexts
            .as_mut()
            .unwrap()
            .entries
            .get_mut("alpha")
            .unwrap();
        definition.model.profile = None;
        definition.model.auth_profile = None;
        assert!(prepare_model_discovery(&config, None).is_err());
        assert!(prepare_model_discovery(&config, Some("missing")).is_err());
    }

    #[test]
    fn qualified_names_fit_auth_contract_and_reject_unusable_local_ids() {
        for local in [
            "",
            "has space",
            "../escape",
            "UPPER",
            "ctx-foreign",
            "emoji-🙂",
        ] {
            assert!(qualify("alpha", local).is_err());
        }
        assert!(qualify("alpha", &"a".repeat(64)).is_err());
        let maximum = qualify(&"a".repeat(48), "12345678").unwrap();
        assert_eq!(maximum.len(), 64);
        assert!(qualify(&"a".repeat(48), "123456789").is_err());
    }

    #[test]
    fn missing_context_and_invalid_ids_do_not_fallback() {
        for raw in [
            "[contexts]\nactive = 'missing'",
            "[contexts.entries.'../outside']",
        ] {
            let value = toml::from_str(raw).unwrap();
            assert!(crate::resolve_composed_config_value(&value).is_err());
        }
        assert_ne!(qualify("a", "b-c").unwrap(), qualify("a-b", "c").unwrap());
    }
}
