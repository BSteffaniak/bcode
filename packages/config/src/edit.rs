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
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| io_error(parent, source))?;
        temporary
            .write_all(self.updated.as_bytes())
            .map_err(|source| io_error(&self.path, source))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|source| io_error(&self.path, source))?;
        temporary
            .persist(&self.path)
            .map_err(|error| io_error(&self.path, error.error))?;
        Ok(self.path)
    }
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
    toml::from_str::<BcodeConfig>(&updated)
        .map_err(|_| invalid("Edited configuration fails schema validation"))?;
    Ok(ConfigEdit {
        path,
        original,
        updated,
    })
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
    if provider_plugin_id.is_empty() || model_id.is_empty() {
        return Err(invalid("Provider and model are required"));
    }
    let original = read_optional(&path)?;
    let mut document = original
        .as_deref()
        .unwrap_or("")
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid("Existing configuration cannot be edited safely"))?;
    if !document.contains_key("model") {
        document["model"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let model = document["model"]
        .as_table_mut()
        .ok_or_else(|| invalid("model must be a table"))?;
    model.insert("provider_plugin_id", toml_edit::value(provider_plugin_id));
    model.insert("model_id", toml_edit::value(model_id));
    model.remove("profile");
    // Auth profile/pool references are preserved and must be reconciled before launch.
    let updated = document.to_string();
    toml::from_str::<BcodeConfig>(&updated).map_err(|_| invalid("Invalid model configuration"))?;
    Ok(ConfigEdit {
        path,
        original,
        updated,
    })
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
