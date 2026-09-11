//! Named configuration contexts resolved before provider or credential materialization.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{AuthConfig, ConfigError, ModelConfig};

/// Declarative contexts. Map keys are stable IDs; labels are presentation only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    /// Explicitly selected stable context ID. No context is selected implicitly.
    #[serde(default)]
    pub active: Option<String>,
    /// Independently scoped model and authentication configuration.
    #[serde(default)]
    pub entries: BTreeMap<String, ContextDefinition>,
}

/// One user-defined context. Global model/auth defaults are not inherited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDefinition {
    /// Human-readable label. Renaming it does not change credential identity.
    #[serde(default)]
    pub label: Option<String>,
    /// Context-local model defaults, profiles, aliases, and policy.
    #[serde(default)]
    pub model: ModelConfig,
    /// Context-local account bindings and pools.
    #[serde(default)]
    pub auth: AuthConfig,
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
    if local.is_empty() || local.starts_with("ctx-") {
        return Err(invalid(
            "context-local reference must not be empty or use the reserved ctx- prefix",
        ));
    }
    Ok(format!("ctx-{}-{context}-{local}", context.len()))
}

fn qualify_option(context: &str, value: &mut Option<String>) -> Result<(), ConfigError> {
    if let Some(name) = value {
        *name = qualify(context, name)?;
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
