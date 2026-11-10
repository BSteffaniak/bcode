//! Explicit, reviewed configuration edits preserving unrelated TOML and comments.

use crate::{BcodeConfig, ConfigError};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// An exact-file edit preview. The original contents fence concurrent changes.
///
/// This type intentionally has no Debug or serialization implementation: existing
/// configuration may contain secrets and must not become public preview data.
pub struct ConfigEdit {
    path: PathBuf,
    original: Option<String>,
    updated: String,
}

impl ConfigEdit {
    /// Destination selected for this explicit edit.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Apply only if the reviewed source is unchanged. Writes use atomic replacement.
    ///
    /// # Errors
    /// Returns an error on concurrent modification, lock failure, or I/O failure.
    pub fn apply(self) -> Result<PathBuf, ConfigError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("Configuration parent is unavailable"))?;
        std::fs::create_dir_all(parent).map_err(|source| io_error(&self.path, source))?;
        let lock_path = self.path.with_extension("toml.lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| io_error(&lock_path, source))?;
        lock.lock().map_err(|source| io_error(&lock_path, source))?;
        if read_optional(&self.path)? != self.original {
            return Err(invalid(
                "Configuration changed since review; review the changes again",
            ));
        }
        atomic_replace(&self.path, self.updated.as_bytes())
            .map_err(|source| io_error(&self.path, source))?;
        Ok(self.path)
    }
}

/// Replace a file with synced contents on the same filesystem. Caller owns locking.
pub(crate) fn atomic_replace(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file parent is unavailable",
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    bcode_confined_fs::persist(temporary, path)?;
    bcode_confined_fs::sync_directory_path(parent)?;
    Ok(())
}

/// Plan one explicit non-secret TOML field edit or override removal.
///
/// Paths are key segments, not a dot-separated expression (plugin IDs may contain dots).
/// Interactive presentation and authentication storage have dedicated APIs and are rejected.
///
/// # Errors
/// Returns an error for an unsupported field, malformed TOML, or invalid configuration.
pub fn plan_field_edit(
    path: PathBuf,
    keys: &[String],
    value: Option<toml::Value>,
) -> Result<ConfigEdit, ConfigError> {
    if keys.first().is_some_and(|key| key == "contexts") {
        let selection = keys.len() == 2 && keys[1] == "active";
        let valid = selection
            || (keys.len() == 5
                && keys[1] == "entries"
                && keys[3] == "model"
                && keys[4] == "profile");
        if !valid {
            return Err(invalid(
                "Only context-local model profile selection is supported by this editor",
            ));
        }
        if selection {
            let Some(toml::Value::String(context)) = &value else {
                return Err(invalid("Select a context ID explicitly"));
            };
            crate::contexts::qualify(context, "validation")?;
        } else {
            crate::contexts::qualify(&keys[2], "validation")?;
        }
        if !matches!(&value, Some(toml::Value::String(name)) if !name.trim().is_empty()) {
            return Err(invalid("Select a non-empty context-local model profile"));
        }
        return plan_validated_field_edit(path, keys, value);
    }
    if keys.is_empty()
        || keys.iter().any(String::is_empty)
        || matches!(
            keys[0].as_str(),
            "tui" | "presentation" | "auth" | "composition"
        )
    {
        return Err(invalid(
            "Use the dedicated presentation or authentication editor for this field",
        ));
    }
    let root: toml::Value = toml::Value::try_from(BcodeConfig::default())
        .map_err(|_| invalid("Configuration schema is unavailable"))?;
    if root.get(&keys[0]).is_none() {
        return Err(invalid("Unknown configuration section"));
    }
    let allowed = match keys[0].as_str() {
        "model" => {
            keys.len() == 2
                && matches!(
                    keys[1].as_str(),
                    "profile" | "model_id" | "provider_plugin_id" | "auth_profile" | "auth_pool"
                )
        }
        "onboarding" => {
            keys.len() == 2 && matches!(keys[1].as_str(), "credential_discovery" | "open_browser")
        }
        "plugins" | "skills" => {
            keys.len() == 2 && matches!(keys[1].as_str(), "enabled" | "disabled")
        }
        _ => false,
    };
    if !allowed {
        return Err(invalid("This field requires a dedicated domain editor"));
    }
    plan_validated_field_edit(path, keys, value)
}

/// Plan an explicit execution default without changing provider or authentication settings.
///
/// This does not establish catalog membership, readiness, or permission to execute. The caller
/// must present the exact route/account choice for approval; managed admission still applies.
/// Passing `None` removes only this preference and restores ordinary provider default behavior.
///
/// # Errors
/// Rejects invalid references, malformed configuration, and inaccessible destinations.
pub fn plan_execution_preference(
    path: PathBuf,
    preference: Option<&crate::ExecutionPreference>,
) -> Result<ConfigEdit, ConfigError> {
    let value = preference
        .map(|preference| {
            preference.validate().map_err(invalid)?;
            toml::Value::try_from(preference).map_err(|_| invalid("Invalid execution preference"))
        })
        .transpose()?;
    let keys = ["execution", "default_selection"].map(str::to_owned);
    plan_validated_field_edit(path, &keys, value)
}

/// Plan one reviewed plugin settings replacement, fenced against stale discovery/setup input.
///
/// Plugin-owned typed preparation supplies `settings`; this editor does not interpret native
/// protocols or credentials. `expected` must equal the destination's current plugin settings
/// (`None` means absent). Other plugins, provider defaults, and auth bindings are untouched.
///
/// # Errors
/// Rejects invalid plugin IDs, non-table settings, stale input, malformed files, or I/O failure.
pub fn plan_plugin_configuration(
    path: PathBuf,
    plugin_id: &str,
    expected: Option<&toml::Value>,
    settings: &toml::Value,
) -> Result<ConfigEdit, ConfigError> {
    if plugin_id.is_empty()
        || plugin_id.len() > 256
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || !settings.is_table()
    {
        return Err(invalid("Invalid plugin configuration target"));
    }
    let original = read_optional(&path)?;
    let parsed: toml::Value = toml::from_str(original.as_deref().unwrap_or(""))
        .map_err(|_| invalid("Existing configuration cannot be edited safely"))?;
    let current = parsed
        .get("plugins")
        .and_then(|value| value.get("config"))
        .and_then(|value| value.get(plugin_id));
    if current != expected {
        return Err(invalid(
            "Plugin configuration changed since setup preparation",
        ));
    }
    let keys = ["plugins", "config", plugin_id].map(str::to_owned);
    let edit = plan_validated_field_edit(path, &keys, Some(settings.clone()))?;
    if edit.original != original {
        return Err(invalid(
            "Configuration changed during plugin setup; review again",
        ));
    }
    Ok(edit)
}

/// Reviewed edit retaining the current configuration policy/provenance fence.
/// No configuration contents are exposed through Debug or serialization.
pub struct PolicyConfigEdit {
    edit: ConfigEdit,
    reference: crate::recovery::ConfigRecoveryReference,
    allowed_roots: Vec<PathBuf>,
}

impl PolicyConfigEdit {
    /// Exact destination verified against retained load-time provenance.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.edit.path()
    }

    /// Explicitly commit after review, rejecting changed policy or destination contents.
    ///
    /// # Errors
    /// Returns an error for changed policy, concurrent edits, locking, or I/O failures.
    pub fn apply(self) -> Result<PathBuf, ConfigError> {
        crate::recovery::resolve_with_roots(&self.reference, &self.allowed_roots)
            .map_err(|_| invalid("Configuration policy changed since review; prepare again"))?;
        self.edit.apply()
    }
}

/// Plan remembering a default in the current, verified single-file configuration context.
///
/// # Errors
/// Rejects inline, layered, context-scoped, stale, unconfined, or otherwise unreproducible
/// policy. Explicit multi-file/context editing requires a separate caller-owned workflow.
pub fn plan_execution_preference_with_policy(
    effective: &BcodeConfig,
    reference: &crate::recovery::ConfigRecoveryReference,
    allowed_roots: &[PathBuf],
    preference: Option<&crate::ExecutionPreference>,
) -> Result<PolicyConfigEdit, ConfigError> {
    let path = policy_edit_path(effective, reference, allowed_roots)?;
    let edit = plan_execution_preference(path, preference)?;
    Ok(PolicyConfigEdit {
        edit,
        reference: reference.clone(),
        allowed_roots: allowed_roots.to_vec(),
    })
}

/// Plan reviewed plugin setup in the same policy-fenced destination as execution preferences.
///
/// `expected` is the destination's current plugin settings, not merged effective settings.
/// This changes no provider selection, authentication profile, pool, or binding.
///
/// # Errors
/// Rejects unsupported policy, stale settings, malformed configuration, or inaccessible files.
pub fn plan_plugin_configuration_with_policy(
    effective: &BcodeConfig,
    reference: &crate::recovery::ConfigRecoveryReference,
    allowed_roots: &[PathBuf],
    plugin_id: &str,
    expected: Option<&toml::Value>,
    settings: &toml::Value,
) -> Result<PolicyConfigEdit, ConfigError> {
    let path = policy_edit_path(effective, reference, allowed_roots)?;
    let edit = plan_plugin_configuration(path, plugin_id, expected, settings)?;
    Ok(PolicyConfigEdit {
        edit,
        reference: reference.clone(),
        allowed_roots: allowed_roots.to_vec(),
    })
}

/// One reviewed plugin replacement in an atomic execution setup edit.
/// Settings are produced by plugin-owned preparation, never interpreted as native protocol here.
pub struct PluginConfigurationUpdate<'a> {
    /// Exact plugin whose settings are being reviewed.
    pub plugin_id: &'a str,
    /// Current destination settings (`None` requires absence).
    pub expected: Option<&'a toml::Value>,
    /// Complete proposed settings, retaining unrelated instance entries.
    pub settings: &'a toml::Value,
}

/// Plan one atomic file replacement for plugin setup and the remembered execution default.
///
/// Pass both adapter and generic catalog updates in `updates`. All expected values and the
/// current configuration provenance are checked before any write. The returned edit requires
/// explicit approval/application. No provider defaults or auth profiles/pools/bindings change.
/// Native preparation must be revalidated immediately before application by the caller.
///
/// # Errors
/// Rejects unsupported/stale policy, duplicate targets, invalid plugin IDs/settings, stale
/// expected values, invalid preference, malformed files, or I/O errors.
pub fn plan_execution_setup_with_policy(
    effective: &BcodeConfig,
    reference: &crate::recovery::ConfigRecoveryReference,
    allowed_roots: &[PathBuf],
    updates: &[PluginConfigurationUpdate<'_>],
    preference: &crate::ExecutionPreference,
) -> Result<PolicyConfigEdit, ConfigError> {
    plan_execution_setup(
        effective,
        reference,
        allowed_roots,
        updates,
        Some(preference),
    )
}

/// Plan native instance setup without changing the remembered model or provider selection.
/// All plugin updates share one provenance-fenced atomic replacement.
///
/// # Errors
/// Rejects stale configuration, unsupported edit policy or conflicting plugin settings.
pub fn plan_native_setup_with_policy(
    effective: &BcodeConfig,
    reference: &crate::recovery::ConfigRecoveryReference,
    allowed_roots: &[PathBuf],
    updates: &[PluginConfigurationUpdate<'_>],
) -> Result<PolicyConfigEdit, ConfigError> {
    plan_execution_setup(effective, reference, allowed_roots, updates, None)
}

fn plan_execution_setup(
    effective: &BcodeConfig,
    reference: &crate::recovery::ConfigRecoveryReference,
    allowed_roots: &[PathBuf],
    updates: &[PluginConfigurationUpdate<'_>],
    preference: Option<&crate::ExecutionPreference>,
) -> Result<PolicyConfigEdit, ConfigError> {
    let path = policy_edit_path(effective, reference, allowed_roots)?;
    if let Some(preference) = preference {
        preference.validate().map_err(invalid)?;
    }
    if updates.is_empty() || updates.len() > 16 {
        return Err(invalid("Invalid execution setup update count"));
    }
    let original = read_optional(&path)?;
    let parsed: toml::Value = toml::from_str(original.as_deref().unwrap_or(""))
        .map_err(|_| invalid("Existing configuration cannot be edited safely"))?;
    let mut document = original
        .as_deref()
        .unwrap_or("")
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid("Existing configuration cannot be edited safely"))?;
    let mut targets = std::collections::BTreeSet::new();
    for update in updates {
        if update.plugin_id.is_empty()
            || update.plugin_id.len() > 256
            || !update
                .plugin_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            || !update.settings.is_table()
            || !targets.insert(update.plugin_id)
        {
            return Err(invalid("Invalid or duplicate plugin configuration target"));
        }
        let current = parsed
            .get("plugins")
            .and_then(|v| v.get("config"))
            .and_then(|v| v.get(update.plugin_id));
        if current != update.expected {
            return Err(invalid(
                "Plugin configuration changed since setup preparation",
            ));
        }
        set_setup_value(
            &mut document,
            &["plugins", "config", update.plugin_id],
            update.settings,
        )?;
    }
    if let Some(preference) = preference {
        let preference = toml::Value::try_from(preference)
            .map_err(|_| invalid("Invalid execution preference"))?;
        set_setup_value(
            &mut document,
            &["execution", "default_selection"],
            &preference,
        )?;
    }
    let rendered = document.to_string();
    let mut resolved: toml::Value =
        toml::from_str(&rendered).map_err(|_| invalid("Invalid execution setup configuration"))?;
    crate::shared_pools::expand(&mut resolved, &path)?;
    resolved
        .try_into::<BcodeConfig>()
        .map_err(|_| invalid("Invalid execution setup configuration"))?;
    // Recheck provenance after constructing the entire proposal, not just at entry.
    policy_edit_path(effective, reference, allowed_roots)?;
    Ok(PolicyConfigEdit {
        edit: ConfigEdit {
            path,
            original,
            updated: rendered,
        },
        reference: reference.clone(),
        allowed_roots: allowed_roots.to_vec(),
    })
}

fn set_setup_value(
    document: &mut toml_edit::DocumentMut,
    keys: &[&str],
    value: &toml::Value,
) -> Result<(), ConfigError> {
    let mut table = document.as_table_mut();
    for key in &keys[..keys.len() - 1] {
        if !table.contains_key(key) {
            table.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        table = table
            .get_mut(key)
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or_else(|| invalid("Configuration setup path is not a table"))?;
    }
    let encoded = format!("value = {value}");
    let parsed = encoded
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid("Invalid setup TOML value"))?;
    table.insert(keys[keys.len() - 1], parsed["value"].clone());
    Ok(())
}

fn policy_edit_path(
    effective: &BcodeConfig,
    reference: &crate::recovery::ConfigRecoveryReference,
    allowed_roots: &[PathBuf],
) -> Result<PathBuf, ConfigError> {
    let replayed = crate::recovery::resolve_with_roots(reference, allowed_roots)
        .map_err(|_| invalid("Current configuration policy cannot be verified for editing"))?;
    if crate::recovery::execution_fingerprint(effective)
        .map_err(|_| invalid("Current configuration policy cannot be verified for editing"))?
        != reference.execution_digest
        || effective.active_context.is_some()
        || replayed.active_context.is_some()
        || reference.sources.active_context.is_some()
        || reference.sources.inline_overrides
    {
        return Err(invalid(
            "Current context requires explicit configuration editing",
        ));
    }
    let sources = &reference.sources;
    let path = edit_destination(sources, &reference.source_presence).ok_or_else(|| {
        invalid("Several configuration files are active; edit the intended file explicitly")
    })?;
    Ok(if path.is_absolute() {
        path.clone()
    } else {
        sources.cwd.join(path)
    })
}

/// The one file a reviewed edit may change, chosen without guessing between active layers.
///
/// * An exclusive file is the only layer.
/// * Otherwise the single present layer is edited; nothing it could be shadowed by exists.
/// * With no layer present, an explicit (base, environment or command-line) file is created
///   when exactly one was named; with none named, the first discovered (user) file is.
/// * Several present layers, or several named-but-absent explicit files, are refused.
fn edit_destination<'a>(
    sources: &'a crate::recovery::ConfigRecoverySources,
    presence: &[bool],
) -> Option<&'a PathBuf> {
    if let Some(path) = &sources.exclusive_config_path {
        return Some(path);
    }
    let explicit: Vec<&PathBuf> = sources
        .base_config_path
        .iter()
        .chain(sources.env_config_path.iter())
        .chain(sources.cli_config_path.iter())
        .collect();
    // Presence follows replay order: base, discovered, environment, command line.
    let layers: Vec<&PathBuf> = sources
        .base_config_path
        .iter()
        .chain(sources.discovered_paths.iter())
        .chain(sources.env_config_path.iter())
        .chain(sources.cli_config_path.iter())
        .collect();
    if layers.len() != presence.len() {
        return None;
    }
    let mut present = layers
        .iter()
        .zip(presence)
        .filter_map(|(path, present)| present.then_some(*path));
    match (present.next(), present.next()) {
        (Some(path), None) => Some(path),
        (Some(_), Some(_)) => None,
        (None, _) => match explicit.as_slice() {
            [path] => Some(path),
            [] => sources.discovered_paths.first(),
            _ => None,
        },
    }
}

fn plan_validated_field_edit(
    path: PathBuf,
    keys: &[String],
    value: Option<toml::Value>,
) -> Result<ConfigEdit, ConfigError> {
    let original = read_optional(&path)?;
    let mut document = original
        .as_deref()
        .unwrap_or("")
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid("Existing configuration cannot be edited safely"))?;
    let mut table = document.as_table_mut();
    for key in &keys[..keys.len() - 1] {
        if !table.contains_key(key) {
            table.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        table = table
            .get_mut(key)
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or_else(|| invalid("Configuration path is not a table"))?;
    }
    let key = &keys[keys.len() - 1];
    if let Some(value) = value {
        let encoded = format!("value = {value}");
        let parsed = encoded
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| invalid("Invalid TOML value"))?;
        table.insert(key, parsed["value"].clone());
    } else {
        table.remove(key);
    }
    let updated = document.to_string();
    let mut resolved: toml::Value = toml::from_str(&updated)
        .map_err(|_| invalid("Edited configuration fails schema validation"))?;
    crate::shared_pools::expand(&mut resolved, &path)?;
    resolved
        .try_into::<BcodeConfig>()
        .map_err(|_| invalid("Edited configuration fails schema validation"))?;
    Ok(ConfigEdit {
        path,
        original,
        updated,
    })
}

/// Plan creation of an empty user-defined context without selecting it or copying credentials.
///
/// # Errors
/// Rejects invalid IDs, duplicate definitions, malformed files, and inaccessible destinations.
pub fn plan_context_creation(
    path: PathBuf,
    context: &str,
    label: &str,
) -> Result<ConfigEdit, ConfigError> {
    crate::contexts::qualify(context, "validation")?;
    let original = read_optional(&path)?;
    let parsed: toml::Value = toml::from_str(original.as_deref().unwrap_or(""))
        .map_err(|_| invalid("Existing configuration cannot be edited safely"))?;
    if parsed
        .get("contexts")
        .and_then(|value| value.get("entries"))
        .and_then(|value| value.get(context))
        .is_some()
    {
        return Err(invalid(
            "Context already exists; creation cannot overwrite its configuration",
        ));
    }
    let keys = ["contexts", "entries", context, "label"].map(str::to_owned);
    let edit = plan_validated_field_edit(path, &keys, Some(toml::Value::String(label.to_owned())))?;
    // Preserve the original version used for duplicate detection across the second read.
    if edit.original != original {
        return Err(invalid(
            "Configuration changed during context creation; review again",
        ));
    }
    Ok(edit)
}

/// Plan a context-local account declaration without writing credentials or changing bindings.
///
/// # Errors
/// Rejects invalid local IDs, duplicate declarations, or malformed destination files.
pub fn plan_context_auth_profile(
    path: PathBuf,
    context: &str,
    local: &str,
    profile: &crate::AuthProfileConfig,
) -> Result<ConfigEdit, ConfigError> {
    crate::contexts::qualify(context, local)?;
    let original = read_optional(&path)?;
    let parsed: toml::Value = toml::from_str(original.as_deref().unwrap_or(""))
        .map_err(|_| invalid("Existing configuration cannot be edited safely"))?;
    if parsed
        .get("contexts")
        .and_then(|v| v.get("entries"))
        .and_then(|v| v.get(context))
        .and_then(|v| v.get("auth"))
        .and_then(|v| v.get("profiles"))
        .and_then(|v| v.get(local))
        .is_some()
    {
        return Err(invalid(
            "Context account already exists; declaration cannot overwrite it",
        ));
    }
    let value =
        toml::Value::try_from(profile).map_err(|_| invalid("Invalid account declaration"))?;
    let keys = ["contexts", "entries", context, "auth", "profiles", local].map(str::to_owned);
    let edit = plan_validated_field_edit(path, &keys, Some(value))?;
    if edit.original != original {
        return Err(invalid(
            "Configuration changed during declaration; review again",
        ));
    }
    Ok(edit)
}

/// Plan a complete default model selection in one atomic file replacement.
/// The caller must resolve the model through the catalog before presenting this edit.
///
/// # Errors
/// Returns an error for empty selection, invalid TOML, or inaccessible configuration.
pub fn plan_model_selection(
    path: PathBuf,
    provider_plugin_id: &str,
    model_id: &str,
) -> Result<ConfigEdit, ConfigError> {
    plan_scoped_model_selection(path, None, provider_plugin_id, model_id, None)
}

/// Plan provider, model, and optional local account selection as one reviewed edit.
/// Context selection never modifies global model defaults or sibling contexts.
///
/// # Errors
/// Rejects invalid context/account IDs, empty targets, and malformed configuration.
pub fn plan_scoped_model_selection(
    path: PathBuf,
    context: Option<&str>,
    provider_plugin_id: &str,
    model_id: &str,
    auth_profile: Option<&str>,
) -> Result<ConfigEdit, ConfigError> {
    if let Some(context) = context {
        crate::contexts::qualify(context, auth_profile.unwrap_or("validation"))?;
    }
    if provider_plugin_id.is_empty() || model_id.is_empty() {
        return Err(invalid("Provider and model are required"));
    }
    plan_model_target_edit(
        path,
        context,
        provider_plugin_id,
        Some(model_id),
        auth_profile,
    )
}

fn plan_model_target_edit(
    path: PathBuf,
    context: Option<&str>,
    provider_plugin_id: &str,
    model_id: Option<&str>,
    auth_profile: Option<&str>,
) -> Result<ConfigEdit, ConfigError> {
    let original = read_optional(&path)?;
    let mut document = original
        .as_deref()
        .unwrap_or("")
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid("Existing configuration cannot be edited safely"))?;
    let keys = context.map_or_else(
        || vec!["model"],
        |context| vec!["contexts", "entries", context, "model"],
    );
    let mut model = document.as_table_mut();
    for key in keys {
        if !model.contains_key(key) {
            model.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        model = model
            .get_mut(key)
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or_else(|| invalid("Model selection path must be a table"))?;
    }
    model.insert("provider_plugin_id", toml_edit::value(provider_plugin_id));
    if let Some(model_id) = model_id {
        model.insert("model_id", toml_edit::value(model_id));
    } else {
        model.remove("model_id");
    }
    model.remove("profile");
    if let Some(profile) = auth_profile {
        model.insert("auth_profile", toml_edit::value(profile));
        model.remove("auth_pool");
    }
    // An explicitly selected account replaces pool routing; otherwise references are preserved.
    let updated = document.to_string();
    toml::from_str::<BcodeConfig>(&updated).map_err(|_| invalid("Invalid model configuration"))?;
    Ok(ConfigEdit {
        path,
        original,
        updated,
    })
}

/// Plan a context-local account/provider selection, clearing model and pool overrides.
/// The account must already exist in the caller's effective context configuration.
///
/// # Errors
/// Rejects invalid IDs, unavailable accounts, and malformed destination files.
pub fn plan_context_account_selection(
    path: PathBuf,
    config: &BcodeConfig,
    local: &str,
) -> Result<ConfigEdit, ConfigError> {
    let discovery = crate::contexts::prepare_model_discovery(config, Some(local))?;
    plan_model_target_edit(
        path,
        Some(&discovery.context),
        &discovery.provider_plugin_id,
        None,
        Some(local),
    )
}

fn read_optional(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(path, error)),
    }
}
fn invalid(message: &str) -> ConfigError {
    ConfigError::Composition {
        message: message.to_owned(),
    }
}
fn io_error(path: &Path, source: std::io::Error) -> ConfigError {
    ConfigError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_setup_preserves_selection_and_rejects_stale_atomic_proposal() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = "# retain comment\n[model]\nmodel_id = 'unchanged'\n";
        std::fs::write(&path, original).unwrap();
        let effective = crate::load_config_from_paths(std::slice::from_ref(&path)).unwrap();
        let sources = crate::recovery::ConfigRecoverySources {
            cwd: temp.path().to_owned(),
            exclusive_config_path: Some(path.clone()),
            ..Default::default()
        };
        let roots = vec![temp.path().to_owned()];
        let reference =
            crate::recovery::capture_with_sources(&effective, &sources, &roots).unwrap();
        let adapter: toml::Value =
            toml::from_str("[instances.primary]\nexecutable = '/native'\n").unwrap();
        let catalog: toml::Value = toml::from_str("enabled = true\n[instances]\n").unwrap();
        let updates = [
            PluginConfigurationUpdate {
                plugin_id: "example.native",
                expected: None,
                settings: &adapter,
            },
            PluginConfigurationUpdate {
                plugin_id: "bcode.harness",
                expected: None,
                settings: &catalog,
            },
        ];
        let edit = plan_native_setup_with_policy(&effective, &reference, &roots, &updates).unwrap();
        let stale =
            plan_native_setup_with_policy(&effective, &reference, &roots, &updates).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        edit.apply().unwrap();
        assert!(stale.apply().is_err());
        let reopened = crate::load_config_from_paths(std::slice::from_ref(&path)).unwrap();
        assert_eq!(reopened.model, effective.model);
        assert_eq!(reopened.execution, effective.execution);
        assert_eq!(reopened.plugins.config["example.native"], adapter);
        assert_eq!(reopened.plugins.config["bcode.harness"], catalog);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("# retain comment")
        );
    }

    #[test]
    fn atomic_execution_setup_updates_both_plugins_and_default_in_one_fenced_write() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let original = "# keep\n[model]\nmodel_id = 'ordinary'\nauth_pool = 'existing'\n[plugins.config.other]\nkeep = true\n";
        std::fs::write(&path, original).unwrap();
        let effective = crate::load_config_from_paths(std::slice::from_ref(&path)).unwrap();
        let sources = crate::recovery::ConfigRecoverySources {
            cwd: temp.path().to_owned(),
            exclusive_config_path: Some(path.clone()),
            ..Default::default()
        };
        let roots = vec![temp.path().to_owned()];
        let reference =
            crate::recovery::capture_with_sources(&effective, &sources, &roots).unwrap();
        let adapter: toml::Value =
            toml::from_str("[instances.primary]\nexecutable = '/native'\n").unwrap();
        let catalog: toml::Value =
            toml::from_str("enabled = true\ndefault_instance = 'primary'\n").unwrap();
        let updates = [
            PluginConfigurationUpdate {
                plugin_id: "example.native",
                expected: None,
                settings: &adapter,
            },
            PluginConfigurationUpdate {
                plugin_id: "bcode.harness",
                expected: None,
                settings: &catalog,
            },
        ];
        let preference = crate::ExecutionPreference {
            model_id: "observed-model".into(),
            route: bcode_model::execution::ExecutionRoute::ManagedHarness {
                instance_id: "primary".into(),
                plugin_id: "example.native".into(),
            },
            account_reference: Some("native-account".into()),
        };
        let first =
            plan_execution_setup_with_policy(&effective, &reference, &roots, &updates, &preference)
                .unwrap();
        let stale =
            plan_execution_setup_with_policy(&effective, &reference, &roots, &updates, &preference)
                .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        first.apply().unwrap();
        assert!(stale.apply().is_err());
        let text = std::fs::read_to_string(&path).unwrap();
        let config: BcodeConfig = toml::from_str(&text).unwrap();
        assert_eq!(config.execution.default_selection, Some(preference));
        assert_eq!(config.plugins.config["example.native"], adapter);
        assert_eq!(config.plugins.config["bcode.harness"], catalog);
        assert_eq!(config.model, effective.model);
        assert_eq!(config.auth, effective.auth);
        assert_eq!(
            config.plugins.config["other"],
            effective.plugins.config["other"]
        );
        assert!(text.starts_with("# keep"));
    }

    #[test]
    fn policy_edit_requires_current_single_file_and_rejects_changed_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        std::fs::write(&path, "[model]\nmodel_id = 'ordinary'\n").unwrap();
        let effective = crate::load_config_from_paths(std::slice::from_ref(&path)).unwrap();
        let sources = crate::recovery::ConfigRecoverySources {
            cwd: temp.path().to_owned(),
            exclusive_config_path: Some(path.clone()),
            ..Default::default()
        };
        let roots = vec![temp.path().to_owned()];
        let reference =
            crate::recovery::capture_with_sources(&effective, &sources, &roots).unwrap();
        let edit =
            plan_execution_preference_with_policy(&effective, &reference, &roots, None).unwrap();
        assert_eq!(edit.path(), path);
        std::fs::write(&path, "[model]\nmodel_id = 'changed'\n").unwrap();
        assert!(edit.apply().is_err());
        assert!(
            plan_execution_preference_with_policy(&effective, &reference, &roots, None).is_err()
        );
        let mut layered = reference;
        layered.sources.exclusive_config_path = None;
        layered.sources.discovered_paths = vec![path, temp.path().join("optional.toml")];
        assert!(plan_execution_preference_with_policy(&effective, &layered, &roots, None).is_err());
    }

    #[test]
    fn discovered_layers_edit_the_single_present_file_or_create_the_user_file() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user/bcode.toml");
        let project = temp.path().join("project/bcode.toml");
        let hidden = temp.path().join("project/.bcode/bcode.toml");
        std::fs::create_dir_all(temp.path().join("user")).unwrap();
        std::fs::create_dir_all(temp.path().join("project/.bcode")).unwrap();
        let discovered = vec![user.clone(), project.clone(), hidden];
        let sources = crate::recovery::ConfigRecoverySources {
            cwd: temp.path().join("project"),
            discovered_paths: discovered.clone(),
            ..Default::default()
        };
        let roots = vec![temp.path().to_owned()];
        // Isolated state: a remembered startup choice on the host must not leak in.
        let environment = crate::ConfigEnvironmentSnapshot::isolated(temp.path());
        let plan = || {
            let effective = crate::load_config_from_paths_with_overrides_and_environment(
                &discovered,
                &crate::ConfigLoadOverrides::default(),
                &environment,
            )
            .unwrap();
            let reference =
                crate::recovery::capture_with_sources(&effective, &sources, &roots).unwrap();
            plan_execution_preference_with_policy(&effective, &reference, &roots, None)
        };

        // First run: no file exists yet, so the user file is created.
        assert_eq!(plan().unwrap().path(), user);
        // One present layer is edited wherever it lives.
        std::fs::write(&project, "[model]\nmodel_id = 'project'\n").unwrap();
        assert_eq!(plan().unwrap().path(), project);
        // Two present layers are ambiguous.
        std::fs::write(&user, "[model]\nmodel_id = 'user'\n").unwrap();
        assert!(plan().is_err());
    }

    #[test]
    fn execution_preference_is_reviewed_clearable_and_preserves_provider_auth() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let original = "# keep\n[model]\nmodel_id = 'ordinary'\nprovider_plugin_id = 'ordinary.provider'\nauth_pool = 'existing-pool'\n[plugins.config.other]\nkeep = true\n";
        std::fs::write(&path, original).unwrap();
        let preference = crate::ExecutionPreference {
            model_id: "discovered-model".into(),
            route: bcode_model::execution::ExecutionRoute::ManagedHarness {
                instance_id: "primary".into(),
                plugin_id: "example.native".into(),
            },
            account_reference: Some("native-account".into()),
        };
        let edit = plan_execution_preference(path.clone(), Some(&preference)).unwrap();
        let stale = plan_execution_preference(path.clone(), Some(&preference)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        edit.apply().unwrap();
        assert!(stale.apply().is_err());
        let config: crate::BcodeConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.execution.default_selection, Some(preference));
        let before: toml::Value = toml::from_str(original).unwrap();
        let after: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(before["model"], after["model"]);
        assert_eq!(before["plugins"], after["plugins"]);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("# keep")
        );
        plan_execution_preference(path.clone(), None)
            .unwrap()
            .apply()
            .unwrap();
        let cleared: crate::BcodeConfig =
            toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(cleared.execution.default_selection.is_none());
        assert_eq!(cleared.model, config.model);
    }

    #[test]
    fn plugin_setup_is_scoped_and_checks_preparation_and_apply_fences() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let original =
            "# keep\n[model]\nmodel_id = 'ordinary'\n[plugins.config.other]\nkeep = true\n";
        std::fs::write(&path, original).unwrap();
        let settings: toml::Value = toml::from_str("enabled = true\n[instances]\n").unwrap();
        let first =
            plan_plugin_configuration(path.clone(), "example.native", None, &settings).unwrap();
        let stale =
            plan_plugin_configuration(path.clone(), "example.native", None, &settings).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        first.apply().unwrap();
        assert!(stale.apply().is_err());
        assert!(
            plan_plugin_configuration(path.clone(), "example.native", None, &settings).is_err()
        );
        let after: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["plugins"]["config"]["example.native"], settings);
        assert_eq!(after["model"]["model_id"].as_str(), Some("ordinary"));
        assert_eq!(
            after["plugins"]["config"]["other"]["keep"].as_bool(),
            Some(true)
        );
        assert!(
            plan_plugin_configuration(path, "example.native", Some(&settings), &settings).is_ok()
        );
    }

    #[test]
    fn context_account_declaration_is_reviewed_and_uses_isolated_storage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        std::fs::write(
            &path,
            "[contexts]\nactive = 'custom'\n[contexts.entries.custom]\n",
        )
        .unwrap();
        let profile = crate::AuthProfileConfig {
            backend: "sshenv".to_owned(),
            provider_id: Some("example".to_owned()),
            owner_plugin_id: Some("example.plugin".to_owned()),
            scheme: Some("oauth".to_owned()),
            ..Default::default()
        };
        let edit = plan_context_auth_profile(path.clone(), "custom", "account", &profile).unwrap();
        let before = crate::load_config_from_paths(std::slice::from_ref(&path)).unwrap();
        assert!(before.auth.profiles.is_empty());
        edit.apply().unwrap();
        assert!(plan_context_auth_profile(path.clone(), "custom", "account", &profile).is_err());
        let loaded = crate::load_config_from_paths(&[path]).unwrap();
        let id = crate::contexts::qualify("custom", "account").unwrap();
        assert_eq!(loaded.auth.profiles[&id].settings["profile"], id);
        assert!(loaded.auth.bindings.is_empty());
    }

    #[test]
    fn context_creation_is_reviewed_nonselecting_and_conflict_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let first = plan_context_creation(path.clone(), "custom", "Custom label").unwrap();
        let stale = plan_context_creation(path.clone(), "other", "Other").unwrap();
        assert!(!path.exists());
        first.apply().unwrap();
        assert!(stale.apply().is_err());
        assert!(plan_context_creation(path.clone(), "custom", "Replacement").is_err());
        let config = crate::load_config_from_paths(&[path]).unwrap();
        assert!(config.active_context.is_none());
        let contexts = config.contexts.unwrap();
        assert!(contexts.entries["custom"].auth.profiles.is_empty());
        assert_eq!(
            contexts.entries["custom"].label.as_deref(),
            Some("Custom label")
        );
    }

    #[test]
    fn explicit_context_selection_edit_preserves_definitions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        std::fs::write(&path, "[contexts]\nactive = 'alpha'\n[contexts.entries.alpha]\nlabel = 'First'\n[contexts.entries.beta]\nlabel = 'Second'\n").unwrap();
        let edit = plan_field_edit(
            path.clone(),
            &["contexts".to_owned(), "active".to_owned()],
            Some(toml::Value::String("beta".to_owned())),
        )
        .unwrap();
        edit.apply().unwrap();
        let config = crate::load_config_from_paths(&[path]).unwrap();
        assert_eq!(config.active_context.as_deref(), Some("beta"));
        assert_eq!(config.contexts.unwrap().entries.len(), 2);
    }

    #[test]
    fn context_model_edit_preserves_global_and_sibling_selections() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let original = "# preserved\n[model]\nprofile = 'global'\n[contexts.entries.alpha.model]\nprofile = 'before'\n[contexts.entries.beta.model]\nprofile = 'sibling'\n";
        std::fs::write(&path, original).unwrap();
        let keys = ["contexts", "entries", "alpha", "model", "profile"].map(str::to_owned);
        let edit = plan_field_edit(
            path.clone(),
            &keys,
            Some(toml::Value::String("after".to_owned())),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        edit.apply().unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["model"]["profile"].as_str(), Some("global"));
        assert_eq!(
            parsed["contexts"]["entries"]["alpha"]["model"]["profile"].as_str(),
            Some("after")
        );
        assert_eq!(
            parsed["contexts"]["entries"]["beta"]["model"]["profile"].as_str(),
            Some("sibling")
        );
        assert!(text.starts_with("# preserved"));
    }

    #[test]
    fn preserves_comments_and_rejects_stale_review() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        std::fs::write(&path, "# keep me\n[model]\nmodel_id = 'old'\n").unwrap();
        let keys = vec!["model".to_owned(), "model_id".to_owned()];
        let first = plan_field_edit(
            path.clone(),
            &keys,
            Some(toml::Value::String("first".to_owned())),
        )
        .unwrap();
        let stale = plan_field_edit(
            path.clone(),
            &keys,
            Some(toml::Value::String("stale".to_owned())),
        )
        .unwrap();
        first.apply().unwrap();
        assert!(stale.apply().is_err());
        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.contains("# keep me"));
        assert!(contents.contains("first"));
    }
    #[test]
    fn model_selection_is_one_reviewed_edit_and_preserves_other_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        std::fs::write(&path, "# preserved\n[model]\nprofile = 'old'\nauth_profile = 'account'\n[onboarding]\ncredential_discovery = false\n").unwrap();
        plan_model_selection(path.clone(), "bcode.provider", "model-name")
            .unwrap()
            .apply()
            .unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        let config: BcodeConfig = toml::from_str(&contents).unwrap();
        assert_eq!(
            config.model.provider_plugin_id.as_deref(),
            Some("bcode.provider")
        );
        assert_eq!(config.model.model_id.as_deref(), Some("model-name"));
        assert_eq!(config.model.auth_profile.as_deref(), Some("account"));
        assert!(config.model.profile.is_none());
        assert!(!config.onboarding.credential_discovery);
        assert!(contents.contains("# preserved"));
    }

    #[test]
    fn presentation_and_auth_require_domain_editors() {
        for section in ["auth", "tui", "presentation"] {
            assert!(plan_field_edit(PathBuf::from("unused"), &[section.to_owned()], None).is_err());
        }
    }
}
