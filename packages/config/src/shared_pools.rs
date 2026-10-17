//! Explicit shared pool policy composition. No account or credential discovery.

use crate::ConfigError;
use std::path::{Path, PathBuf};

pub mod migration;

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Composition {
        message: message.into(),
    }
}

/// Expand a pool reference relative to its declaring file, before layer composition.
pub(crate) fn expand(raw: &mut toml::Value, source: &Path) -> Result<(), ConfigError> {
    let Some(reference) = raw
        .as_table_mut()
        .and_then(|table| table.remove("auth_pool_config"))
    else {
        return Ok(());
    };
    let reference = reference
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid("auth_pool_config must be a nonempty file path"))?;
    let path = PathBuf::from(reference);
    let path = if path.is_absolute() {
        path
    } else {
        source.parent().unwrap_or_else(|| Path::new(".")).join(path)
    };
    let shared = toml::from_str(&read(&path)?)
        .map_err(|_| invalid("Shared pool policy is not valid TOML"))?;
    let pools = pools(&shared)?;
    let root = raw
        .as_table_mut()
        .ok_or_else(|| invalid("configuration must be a table"))?;
    let auth = root
        .entry("auth")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| invalid("auth must be a table"))?;
    let local = auth
        .entry("pools")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| invalid("auth.pools must be a table"))?;
    for (name, value) in pools {
        if local.contains_key(name) {
            return Err(invalid(format!(
                "pool '{name}' is defined both locally and in shared policy"
            )));
        }
        local.insert(name.clone(), value.clone());
    }
    Ok(())
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })
}

fn pools(raw: &toml::Value) -> Result<&toml::Table, ConfigError> {
    let root = raw
        .as_table()
        .ok_or_else(|| invalid("shared pool policy must be a table"))?;
    let auth = root
        .get("auth")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| invalid("shared pool policy requires [auth.pools]"))?;
    if root.get("schema_version").and_then(toml::Value::as_integer) != Some(1) {
        return Err(invalid(
            "Unsupported shared pool policy version; expected schema_version = 1",
        ));
    }
    if root.len() != 2 || auth.len() != 1 {
        return Err(invalid(
            "shared pool policy may contain only [auth.pools]; accounts, contexts and imports are forbidden",
        ));
    }
    let pools = auth
        .get("pools")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| invalid("shared pool policy requires [auth.pools]"))?;
    for value in pools.values() {
        value
            .clone()
            .try_into::<crate::AuthPoolConfig>()
            .map_err(|_| invalid("invalid shared auth pool policy"))?;
        // Reject unknown fields rather than losing them during migration.
        let table = value
            .as_table()
            .ok_or_else(|| invalid("pool must be a table"))?;
        for (section, allowed) in [
            (
                "priming",
                &[
                    "enabled",
                    "include_primary",
                    "reprime_after",
                    "provider_windows",
                    "fallback_reprime_after",
                    "required_windows",
                ][..],
            ),
            (
                "quota",
                &["unknown_cooldown", "rate_limit_cooldown", "weekly_cooldown"][..],
            ),
        ] {
            if let Some(nested) = table.get(section).and_then(toml::Value::as_table)
                && nested.keys().any(|key| !allowed.contains(&key.as_str()))
            {
                return Err(invalid("Unknown nested shared pool policy field"));
            }
        }
        if table.keys().any(|key| {
            !matches!(
                key.as_str(),
                "provider_plugin_id"
                    | "automatic_provider"
                    | "automatic_group"
                    | "strategy"
                    | "preferred_profile"
                    | "profiles"
                    | "priming"
                    | "quota"
            )
        }) {
            return Err(invalid("unknown shared auth pool field"));
        }
    }
    Ok(pools)
}

#[cfg(test)]
mod tests;
