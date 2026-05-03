#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Configuration loading for Bcode.

use bcode_plugin::PluginSelection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
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
    #[serde(default)]
    pub permissions: PermissionConfig,
    #[serde(default)]
    pub auth: AuthConfig,
}

impl BcodeConfig {
    fn merge(&mut self, next: Self) {
        self.plugins.merge(next.plugins);
        self.model.merge(next.model);
        self.permissions.merge(next.permissions);
        self.auth.merge(next.auth);
    }
}

/// Authentication configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub openai: Option<AuthProviderConfig>,
}

impl AuthConfig {
    fn merge(&mut self, next: Self) {
        if next.openai.is_some() {
            self.openai = next.openai;
        }
    }
}

/// Per-provider authentication configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthProviderConfig {
    pub backend: String,
    #[serde(default)]
    pub mode: AuthMode,
    pub profile: String,
    #[serde(default)]
    pub vault: Option<PathBuf>,
}

/// Authentication mode for a provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// `OpenAI` platform API key authentication.
    #[default]
    ApiKey,
    /// `ChatGPT` subscription authentication for `Codex` models.
    #[serde(rename = "chatgpt", alias = "chat_gpt")]
    ChatGpt,
}

/// Model selection configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
}

impl ModelConfig {
    fn merge(&mut self, next: Self) {
        if next.provider_plugin_id.is_some() {
            self.provider_plugin_id = next.provider_plugin_id;
        }
        if next.model_id.is_some() {
            self.model_id = next.model_id;
        }
    }
}

/// Permission policy configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionConfig {
    #[serde(default)]
    pub allow_tools: BTreeSet<String>,
    #[serde(default)]
    pub deny_tools: BTreeSet<String>,
    #[serde(default)]
    pub allow_shell_command_prefixes: BTreeSet<String>,
    #[serde(default)]
    pub deny_shell_command_prefixes: BTreeSet<String>,
    #[serde(default)]
    pub allow_path_prefixes: BTreeSet<String>,
    #[serde(default)]
    pub deny_path_prefixes: BTreeSet<String>,
}

impl PermissionConfig {
    fn merge(&mut self, next: Self) {
        self.allow_tools.extend(next.allow_tools);
        self.deny_tools.extend(next.deny_tools);
        self.allow_shell_command_prefixes
            .extend(next.allow_shell_command_prefixes);
        self.deny_shell_command_prefixes
            .extend(next.deny_shell_command_prefixes);
        self.allow_path_prefixes.extend(next.allow_path_prefixes);
        self.deny_path_prefixes.extend(next.deny_path_prefixes);
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
    #[error("unknown permission rule kind: {0}")]
    UnknownPermissionRule(String),
}

/// Add a permission rule to the default writable config file.
///
/// # Errors
///
/// Returns an error when the config cannot be read, updated, or written.
pub fn add_permission_rule(kind: &str, value: String) -> Result<PathBuf, ConfigError> {
    update_writable_config(|config| insert_permission_rule(&mut config.permissions, kind, value))
}

/// Configure `OpenAI` authentication backed by an `sshenv` vault.
///
/// # Errors
///
/// Returns an error when the config cannot be read, updated, or written.
pub fn set_openai_sshenv_auth(
    profile: String,
    vault: PathBuf,
    model_id: Option<String>,
) -> Result<PathBuf, ConfigError> {
    set_openai_sshenv_auth_mode(profile, vault, model_id, AuthMode::ApiKey)
}

/// Configure `OpenAI` authentication mode backed by an `sshenv` vault.
///
/// # Errors
///
/// Returns an error when the config cannot be read, updated, or written.
pub fn set_openai_sshenv_auth_mode(
    profile: String,
    vault: PathBuf,
    model_id: Option<String>,
    mode: AuthMode,
) -> Result<PathBuf, ConfigError> {
    update_writable_config(|config| {
        config
            .plugins
            .enabled
            .insert("bcode.openai-compatible".to_string());
        config.model.provider_plugin_id = Some("bcode.openai-compatible".to_string());
        if let Some(model_id) = model_id {
            config.model.model_id = Some(model_id);
        }
        config.auth.openai = Some(AuthProviderConfig {
            backend: "sshenv".to_string(),
            mode,
            profile,
            vault: Some(vault),
        });
        Ok(())
    })
}

fn update_writable_config(
    update: impl FnOnce(&mut BcodeConfig) -> Result<(), ConfigError>,
) -> Result<PathBuf, ConfigError> {
    let path = writable_config_path();
    let mut config = if path.exists() {
        read_config(&path)?
    } else {
        BcodeConfig::default()
    };
    update(&mut config)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, config_to_toml(&config)).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Return the default Bcode auth vault path.
#[must_use]
pub fn default_auth_vault_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_AUTH_VAULT") {
        return PathBuf::from(path);
    }
    if let Ok(state_home) = env::var("XDG_STATE_HOME") {
        return PathBuf::from(state_home)
            .join("bcode")
            .join("auth")
            .join("vault");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("bcode")
            .join("auth")
            .join("vault");
    }
    env::temp_dir().join("bcode").join("auth").join("vault")
}

fn insert_permission_rule(
    permissions: &mut PermissionConfig,
    kind: &str,
    value: String,
) -> Result<(), ConfigError> {
    match kind {
        "allow_tool" => permissions.allow_tools.insert(value),
        "deny_tool" => permissions.deny_tools.insert(value),
        "allow_shell_command_prefix" => permissions.allow_shell_command_prefixes.insert(value),
        "deny_shell_command_prefix" => permissions.deny_shell_command_prefixes.insert(value),
        "allow_path_prefix" => permissions.allow_path_prefixes.insert(value),
        "deny_path_prefix" => permissions.deny_path_prefixes.insert(value),
        _ => return Err(ConfigError::UnknownPermissionRule(kind.to_string())),
    };
    Ok(())
}

fn writable_config_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_CONFIG") {
        return PathBuf::from(path);
    }
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home)
            .join("bcode")
            .join(DEFAULT_CONFIG_FILE_NAME);
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("bcode")
            .join(DEFAULT_CONFIG_FILE_NAME);
    }
    env::temp_dir().join(DEFAULT_CONFIG_FILE_NAME)
}

fn config_to_toml(config: &BcodeConfig) -> String {
    let mut output = String::new();
    write_plugins_toml(&mut output, &config.plugins);
    if config.model.provider_plugin_id.is_some() || config.model.model_id.is_some() {
        output.push_str("[model]\n");
        if let Some(provider_plugin_id) = &config.model.provider_plugin_id {
            writeln!(
                output,
                "provider_plugin_id = {}",
                toml_string(provider_plugin_id)
            )
            .expect("writing to string should not fail");
        }
        if let Some(model_id) = &config.model.model_id {
            writeln!(output, "model_id = {}", toml_string(model_id))
                .expect("writing to string should not fail");
        }
        output.push('\n');
    }
    write_permissions_toml(&mut output, &config.permissions);
    write_auth_toml(&mut output, &config.auth);
    output
}

fn write_permissions_toml(output: &mut String, permissions: &PermissionConfig) {
    if permissions == &PermissionConfig::default() {
        return;
    }
    output.push_str("[permissions]\n");
    write_string_set(output, "allow_tools", &permissions.allow_tools);
    write_string_set(output, "deny_tools", &permissions.deny_tools);
    write_string_set(
        output,
        "allow_shell_command_prefixes",
        &permissions.allow_shell_command_prefixes,
    );
    write_string_set(
        output,
        "deny_shell_command_prefixes",
        &permissions.deny_shell_command_prefixes,
    );
    write_string_set(
        output,
        "allow_path_prefixes",
        &permissions.allow_path_prefixes,
    );
    write_string_set(
        output,
        "deny_path_prefixes",
        &permissions.deny_path_prefixes,
    );
    output.push('\n');
}

fn write_auth_toml(output: &mut String, auth: &AuthConfig) {
    let Some(openai) = &auth.openai else {
        return;
    };
    output.push_str("[auth.openai]\n");
    writeln!(output, "backend = {}", toml_string(&openai.backend))
        .expect("writing to string should not fail");
    writeln!(output, "profile = {}", toml_string(&openai.profile))
        .expect("writing to string should not fail");
    if let Some(vault) = &openai.vault {
        writeln!(
            output,
            "vault = {}",
            toml_string(&vault.display().to_string())
        )
        .expect("writing to string should not fail");
    }
    output.push('\n');
}

fn write_plugins_toml(output: &mut String, plugins: &PluginConfig) {
    if plugins == &PluginConfig::default() {
        return;
    }
    output.push_str("[plugins]\n");
    write_string_set(output, "enabled", &plugins.enabled);
    write_string_set(output, "disabled", &plugins.disabled);
    output.push('\n');
}

fn write_string_set(output: &mut String, key: &str, values: &BTreeSet<String>) {
    if values.is_empty() {
        return;
    }
    let values = values
        .iter()
        .map(|value| toml_string(value))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(output, "{key} = [{values}]").expect("writing to string should not fail");
}

fn toml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
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

[permissions]
allow_tools = ["filesystem.read"]
deny_tools = ["shell.run"]
allow_shell_command_prefixes = ["git status"]
deny_shell_command_prefixes = ["rm -rf"]
allow_path_prefixes = ["/tmp/project"]
deny_path_prefixes = ["/tmp/project/target"]
"#,
        )
        .expect("project config should be written");

        let config = load_config_from_paths(&[user, project]).expect("config should load");
        assert!(config.plugins.enabled.contains("example.a"));
        assert!(config.plugins.enabled.contains("example.c"));
        assert!(config.plugins.disabled.contains("example.b"));
        assert!(config.plugins.disabled.contains("example.d"));
        assert!(config.permissions.allow_tools.contains("filesystem.read"));
        assert!(config.permissions.deny_tools.contains("shell.run"));
        assert!(
            config
                .permissions
                .allow_shell_command_prefixes
                .contains("git status")
        );
        assert!(
            config
                .permissions
                .deny_shell_command_prefixes
                .contains("rm -rf")
        );
        assert!(
            config
                .permissions
                .allow_path_prefixes
                .contains("/tmp/project")
        );
        assert!(
            config
                .permissions
                .deny_path_prefixes
                .contains("/tmp/project/target")
        );

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
