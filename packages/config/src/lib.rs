#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Configuration loading for Bcode.

use bcode_plugin::PluginSelection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Default Bcode config file name.
pub const DEFAULT_CONFIG_FILE_NAME: &str = "bcode.toml";

/// Top-level Bcode configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BcodeConfig {
    #[serde(default)]
    pub plugins: PluginConfig,
    #[serde(default)]
    pub model: ModelConfig,
}

impl BcodeConfig {
    fn merge(&mut self, next: Self) {
        self.plugins.merge(next.plugins);
        self.model.merge(next.model);
    }
}

/// Model selection configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub model_id: Option<String>,
}

impl ModelConfig {
    fn merge(&mut self, next: Self) {
        if next.model_id.is_some() {
            self.model_id = next.model_id;
        }
    }
}

/// Plugin configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfig {
    #[serde(default)]
    pub enabled: BTreeSet<String>,
    #[serde(default)]
    pub disabled: BTreeSet<String>,
}

impl PluginConfig {
    fn merge(&mut self, next: Self) {
        self.enabled.extend(next.enabled);
        self.disabled.extend(next.disabled);
    }
}

impl From<&PluginConfig> for PluginSelection {
    fn from(value: &PluginConfig) -> Self {
        Self {
            enabled: value.enabled.clone(),
            disabled: value.disabled.clone(),
        }
    }
}

impl From<&BcodeConfig> for PluginSelection {
    fn from(value: &BcodeConfig) -> Self {
        Self::from(&value.plugins)
    }
}

/// Errors returned by config loading.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O error while reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

/// Return default config paths in merge order.
#[must_use]
pub fn default_config_paths() -> Vec<PathBuf> {
    if let Ok(path) = env::var("BCODE_CONFIG") {
        return vec![PathBuf::from(path)];
    }

    let mut paths = Vec::new();
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        paths.push(
            PathBuf::from(config_home)
                .join("bcode")
                .join(DEFAULT_CONFIG_FILE_NAME),
        );
    } else if let Ok(home) = env::var("HOME") {
        paths.push(
            PathBuf::from(home)
                .join(".config")
                .join("bcode")
                .join(DEFAULT_CONFIG_FILE_NAME),
        );
    }
    if let Ok(current_dir) = env::current_dir() {
        paths.push(current_dir.join(".bcode").join(DEFAULT_CONFIG_FILE_NAME));
    }
    paths
}

/// Load configuration from default paths.
///
/// # Errors
///
/// Returns an error if an existing config file cannot be read or parsed.
pub fn load_config() -> Result<BcodeConfig, ConfigError> {
    load_config_from_paths(&default_config_paths())
}

/// Load and merge configuration from the provided paths.
///
/// Missing paths are ignored. Existing files are merged in the order provided.
///
/// # Errors
///
/// Returns an error if an existing config file cannot be read or parsed.
pub fn load_config_from_paths(paths: &[PathBuf]) -> Result<BcodeConfig, ConfigError> {
    let mut config = BcodeConfig::default();
    for path in paths {
        if !path.exists() {
            continue;
        }
        config.merge(read_config(path)?);
    }
    Ok(config)
}

fn read_config(path: &Path) -> Result<BcodeConfig, ConfigError> {
    let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::load_config_from_paths;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn merges_plugin_selection_from_existing_files() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let user = root.join("user.toml");
        let project = root.join("project.toml");
        std::fs::write(
            &user,
            r#"
[plugins]
enabled = ["example.a"]
disabled = ["example.b"]
"#,
        )
        .expect("user config should be written");
        std::fs::write(
            &project,
            r#"
[plugins]
enabled = ["example.c"]
disabled = ["example.d"]
"#,
        )
        .expect("project config should be written");

        let config = load_config_from_paths(&[user, project]).expect("config should load");
        assert!(config.plugins.enabled.contains("example.a"));
        assert!(config.plugins.enabled.contains("example.c"));
        assert!(config.plugins.disabled.contains("example.b"));
        assert!(config.plugins.disabled.contains("example.d"));

        std::fs::remove_dir_all(root).expect("temp root should clean up");
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("bcode-config-test-{nanos}"))
    }
}
