#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Configuration loading for Bcode.

use bcode_plugin::PluginSelection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Default Bcode config file name.
pub const DEFAULT_CONFIG_FILE_NAME: &str = "bcode.toml";

const DEFAULT_AGENT_PROFILE_PLUGIN_ID: &str = "bcode.default-agents";

/// Top-level Bcode configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BcodeConfig {
    #[serde(default)]
    pub plugins: PluginConfig,
    #[serde(default)]
    pub model: ModelConfig,
    /// Per-agent permission and tool configuration.
    ///
    /// Keys are agent IDs (for example `build`, `plan`). When a key is absent,
    /// the built-in defaults from `bcode_agent_policy::default_config` apply.
    #[serde(default)]
    pub agent: BTreeMap<String, bcode_agent_policy_models::AgentConfig>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub tui: TuiConfig,
}

impl BcodeConfig {
    fn merge(&mut self, next: Self) {
        self.plugins.merge(next.plugins);
        self.model.merge(next.model);
        for (agent_id, agent_config) in next.agent {
            self.agent.insert(agent_id, agent_config);
        }
        self.auth.merge(next.auth);
        self.tui.merge(next.tui);
    }

    /// Resolve the active model profile to a concrete provider/model selection.
    #[must_use]
    pub fn resolved_model_selection(&self) -> ResolvedModelSelection {
        let mut selection = ResolvedModelSelection {
            provider_plugin_id: self.model.provider_plugin_id.clone(),
            model_id: self.model.model_id.clone(),
            model_profile: self.model.profile.clone(),
            auth_profile: None,
            settings: BTreeMap::new(),
        };
        if let Some(profile_name) = &self.model.profile
            && let Some(profile) = self.model.profiles.get(profile_name)
        {
            selection.provider_plugin_id = Some(profile.provider_plugin_id.clone());
            if profile.model_id.is_some() {
                selection.model_id.clone_from(&profile.model_id);
            }
            selection.auth_profile.clone_from(&profile.auth_profile);
            selection.settings = profile.settings.clone();
        }
        if let Some(env_provider) = provider_plugin_id_from_environment() {
            let provider_changed =
                selection.provider_plugin_id.as_deref() != Some(env_provider.as_str());
            selection.provider_plugin_id = Some(env_provider.clone());
            if let Some(model_id) = model_id_from_environment(&env_provider) {
                selection.model_id = Some(model_id);
            } else if provider_changed {
                // Do not pass a persisted model ID for a different provider. Let the selected
                // provider use its own default model when no provider-specific env model exists.
                selection.model_id = None;
            }
            if provider_changed {
                selection.model_profile = None;
                selection.auth_profile = None;
                selection.settings.clear();
            }
        }
        selection
    }
}

/// Return a provider plugin ID explicitly or implicitly selected by environment variables.
#[must_use]
pub fn provider_plugin_id_from_environment() -> Option<String> {
    first_env_value(["BCODE_MODEL_PROVIDER", "BCODE_PROVIDER"])
        .and_then(|value| normalize_provider_plugin_id(&value))
        .or_else(|| bedrock_environment_is_configured().then(|| "bcode.bedrock".to_string()))
}

fn normalize_provider_plugin_id(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "bedrock" | "aws-bedrock" | "aws_bedrock" | "bcode.bedrock" => {
            Some("bcode.bedrock".to_string())
        }
        "openai"
        | "openai-compatible"
        | "openai_compatible"
        | "xai"
        | "grok"
        | "bcode.openai-compatible" => Some("bcode.openai-compatible".to_string()),
        _ => None,
    }
}

fn model_id_from_environment(provider_plugin_id: &str) -> Option<String> {
    match provider_plugin_id {
        "bcode.bedrock" => first_env_value(["BCODE_BEDROCK_MODEL", "BEDROCK_MODEL"]),
        "bcode.openai-compatible" => first_env_value([
            "BCODE_XAI_MODEL",
            "XAI_MODEL",
            "BCODE_OPENAI_MODEL",
            "OPENAI_MODEL",
        ]),
        _ => None,
    }
}

fn first_env_value<const N: usize>(names: [&str; N]) -> Option<String> {
    names.into_iter().find_map(|name| match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    })
}

/// Return true when environment variables imply Bedrock should be selected.
#[must_use]
pub fn bedrock_environment_is_configured() -> bool {
    [
        "AWS_BEARER_TOKEN_BEDROCK",
        "BCODE_BEDROCK_MODEL",
        "BCODE_BEDROCK_MODELS",
        "BCODE_BEDROCK_REGION",
        "BCODE_BEDROCK_AWS_PROFILE",
        "BCODE_BEDROCK_ENDPOINT_URL",
        "BEDROCK_MODEL",
        "BEDROCK_MODELS",
        "BEDROCK_ENDPOINT_URL",
    ]
    .into_iter()
    .any(|name| env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

/// Terminal UI configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiConfig {
    /// Scoped keybindings keyed by key stroke. Values are action IDs.
    #[serde(default)]
    pub keybindings: TuiKeyBindingConfig,
}

impl TuiConfig {
    fn merge(&mut self, next: Self) {
        self.keybindings.merge(next.keybindings);
    }
}

/// Scoped terminal UI keybindings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TuiKeyBindingConfig {
    /// Main chat view bindings.
    pub chat: BTreeMap<String, String>,
    /// Permission prompt bindings.
    pub permission: BTreeMap<String, String>,
    /// Session picker bindings.
    pub session_picker: BTreeMap<String, String>,
    /// Legacy `[tui.keybindings]` action-to-keys entries loaded for compatibility.
    #[serde(skip)]
    pub legacy_actions: BTreeMap<String, Vec<String>>,
}

impl TuiKeyBindingConfig {
    fn merge(&mut self, next: Self) {
        self.chat.extend(next.chat);
        self.permission.extend(next.permission);
        self.session_picker.extend(next.session_picker);
        self.legacy_actions.extend(next.legacy_actions);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chat.is_empty()
            && self.permission.is_empty()
            && self.session_picker.is_empty()
            && self.legacy_actions.is_empty()
    }
}

impl<'de> Deserialize<'de> for TuiKeyBindingConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let raw = BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
        let mut config = Self::default();
        for (key, value) in raw {
            match key.as_str() {
                "chat" => config.chat = parse_tui_keybinding_section(value, "chat")?,
                "permission" => {
                    config.permission = parse_tui_keybinding_section(value, "permission")?;
                }
                "session_picker" | "sessionPicker" => {
                    config.session_picker = parse_tui_keybinding_section(value, "session_picker")?;
                }
                legacy_action => {
                    if let toml::Value::Array(values) = value {
                        let mut keys = Vec::new();
                        for item in values {
                            let toml::Value::String(key) = item else {
                                return Err(D::Error::custom(format!(
                                    "legacy tui keybinding '{legacy_action}' must be an array of strings"
                                )));
                            };
                            keys.push(key);
                        }
                        config
                            .legacy_actions
                            .insert(legacy_action.to_string(), keys);
                    }
                }
            }
        }
        Ok(config)
    }
}

fn parse_tui_keybinding_section<E>(
    value: toml::Value,
    section: &str,
) -> Result<BTreeMap<String, String>, E>
where
    E: serde::de::Error,
{
    let toml::Value::Table(table) = value else {
        return Err(E::custom(format!(
            "tui.keybindings.{section} must be a table of key = action entries"
        )));
    };
    let mut bindings = BTreeMap::new();
    for (key, value) in table {
        let toml::Value::String(action) = value else {
            return Err(E::custom(format!(
                "tui.keybindings.{section}.{key} must be a string action ID"
            )));
        };
        bindings.insert(key, action);
    }
    Ok(bindings)
}

/// Authentication configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub openai: Option<AuthProviderConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, AuthProfileConfig>,
}

impl AuthConfig {
    fn merge(&mut self, next: Self) {
        if next.openai.is_some() {
            self.openai = next.openai;
        }
        self.profiles.extend(next.profiles);
    }
}

/// Generic authentication profile configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthProfileConfig {
    pub backend: String,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
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
    #[serde(default)]
    pub default_thinking_level: Option<bcode_model::ReasoningEffort>,
    #[serde(default)]
    pub max_tool_rounds: Option<u32>,
    #[serde(default)]
    pub prompt_cache: PromptCacheConfig,
    #[serde(default)]
    pub conversation_reuse: ConversationReuseConfig,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ModelProfileConfig>,
}

impl ModelConfig {
    #[must_use]
    pub fn effective_max_tool_rounds(&self) -> Option<u32> {
        self.max_tool_rounds.filter(|rounds| *rounds > 0)
    }

    fn merge(&mut self, next: Self) {
        if next.provider_plugin_id.is_some() {
            self.provider_plugin_id = next.provider_plugin_id;
        }
        if next.model_id.is_some() {
            self.model_id = next.model_id;
        }
        if next.default_thinking_level.is_some() {
            self.default_thinking_level = next.default_thinking_level;
        }
        if next.max_tool_rounds.is_some() {
            self.max_tool_rounds = next.max_tool_rounds;
        }
        if next.prompt_cache != PromptCacheConfig::default() {
            self.prompt_cache = next.prompt_cache;
        }
        if next.conversation_reuse != ConversationReuseConfig::default() {
            self.conversation_reuse = next.conversation_reuse;
        }
        if next.profile.is_some() {
            self.profile = next.profile;
        }
        self.profiles.extend(next.profiles);
    }
}

/// Prompt cache configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheConfig {
    /// Prompt cache mode. Defaults to `auto`.
    #[serde(default)]
    pub mode: bcode_model::PromptCacheMode,
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self {
            mode: bcode_model::PromptCacheMode::Auto,
        }
    }
}

/// Provider-native conversation reuse configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationReuseConfig {
    /// Conversation reuse mode. Defaults to `off` because provider-native retention semantics are provider-specific.
    #[serde(default)]
    pub mode: bcode_model::ConversationReuseMode,
}

/// Generic model provider profile configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProfileConfig {
    pub provider_plugin_id: String,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
}

/// Resolved model selection after applying the active model profile, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedModelSelection {
    pub provider_plugin_id: Option<String>,
    pub model_id: Option<String>,
    pub model_profile: Option<String>,
    pub auth_profile: Option<String>,
    pub settings: BTreeMap<String, String>,
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
        let mut selection = Self::from(&value.plugins);
        let env_provider = provider_plugin_id_from_environment();
        let provider = env_provider.clone().unwrap_or_else(|| {
            value
                .resolved_model_selection()
                .provider_plugin_id
                .unwrap_or_else(|| "bcode.openai-compatible".to_string())
        });
        if selection.enabled.is_empty() {
            for plugin_id in [
                "bcode.filesystem",
                "bcode.shell",
                DEFAULT_AGENT_PROFILE_PLUGIN_ID,
                provider.as_str(),
            ] {
                enable_plugin_unless_disabled(&mut selection, plugin_id);
            }
        } else {
            enable_plugin_unless_disabled(&mut selection, DEFAULT_AGENT_PROFILE_PLUGIN_ID);
            if let Some(env_provider) = env_provider
                && !selection.disabled.contains(&env_provider)
            {
                selection.enabled.insert(env_provider.clone());
                remove_other_model_providers(&mut selection.enabled, &env_provider);
            }
        }
        selection
    }
}

fn enable_plugin_unless_disabled(selection: &mut PluginSelection, plugin_id: &str) {
    if !selection.disabled.contains(plugin_id) {
        selection.enabled.insert(plugin_id.to_string());
    }
}

fn remove_other_model_providers(enabled: &mut BTreeSet<String>, selected_provider: &str) {
    for provider in ["bcode.bedrock", "bcode.openai-compatible"] {
        if provider != selected_provider {
            enabled.remove(provider);
        }
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
    #[error("unknown permission category: {0}")]
    UnknownPermissionCategory(String),
    #[error("unknown permission action: {0}")]
    UnknownPermissionAction(String),
}

/// Upsert a permission rule under `[agent.<agent_id>.permission.<category>]`.
///
/// `category` must be one of `bash`, `read`, `write`, or `edit`.
/// `action` must be one of `allow`, `ask`, or `deny`.
///
/// # Errors
///
/// Returns an error when the config cannot be read, the category or action is
/// unknown, or the updated config cannot be written.
pub fn upsert_agent_permission_rule(
    agent_id: &str,
    category: &str,
    pattern: String,
    action: &str,
) -> Result<PathBuf, ConfigError> {
    let action = parse_action(action)?;
    update_writable_config(|config| {
        insert_agent_permission_rule(&mut config.agent, agent_id, category, pattern, action)
    })
}

/// Configure OpenAI-compatible provider (`OpenAI`, xAI/Grok, etc.) authentication
/// backed by an `sshenv` vault.
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

/// Configure OpenAI-compatible provider authentication mode backed by an `sshenv` vault.
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
        // xAI and other OpenAI-compatibles reuse the same plugin ID + service
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

/// Configure a generic Bedrock model profile using AWS's default credential chain.
///
/// # Errors
///
/// Returns an error when the config cannot be read, updated, or written.
pub fn set_bedrock_model_profile(
    profile: &str,
    model_id: String,
    aws_profile: Option<String>,
    region: Option<String>,
    endpoint_url: Option<&str>,
    model_ids: &[String],
) -> Result<PathBuf, ConfigError> {
    update_writable_config(|config| {
        config.plugins.enabled.insert("bcode.bedrock".to_string());
        config.model.profile = Some(profile.to_string());
        config.model.provider_plugin_id = Some("bcode.bedrock".to_string());
        config.model.model_id = Some(model_id.clone());
        let auth_profile = format!("{profile}-aws");
        let mut settings = BTreeMap::new();
        if let Some(region) = region.clone() {
            settings.insert("region".to_string(), region);
        }
        if let Some(endpoint_url) = endpoint_url {
            settings.insert("endpoint_url".to_string(), endpoint_url.to_string());
        }
        if !model_ids.is_empty() {
            settings.insert("models".to_string(), model_ids.join(","));
        }
        config.model.profiles.insert(
            profile.to_string(),
            ModelProfileConfig {
                provider_plugin_id: "bcode.bedrock".to_string(),
                model_id: Some(model_id),
                auth_profile: Some(auth_profile.clone()),
                settings,
            },
        );
        let mut auth_settings = BTreeMap::new();
        if let Some(aws_profile) = aws_profile {
            auth_settings.insert("profile".to_string(), aws_profile);
        }
        if let Some(region) = region {
            auth_settings.insert("region".to_string(), region);
        }
        config.auth.profiles.insert(
            auth_profile,
            AuthProfileConfig {
                backend: "aws_default_chain".to_string(),
                settings: auth_settings,
            },
        );
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

/// Return the default Bcode state directory.
#[must_use]
pub fn default_state_dir() -> PathBuf {
    if let Ok(path) = env::var("BCODE_STATE_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(state_home) = env::var("XDG_STATE_HOME") {
        return PathBuf::from(state_home).join("bcode");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("bcode");
    }
    env::temp_dir().join("bcode")
}

/// Return the default Bcode auth vault path.
#[must_use]
pub fn default_auth_vault_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_AUTH_VAULT") {
        return PathBuf::from(path);
    }
    default_state_dir().join("auth").join("vault")
}

fn insert_agent_permission_rule(
    agent: &mut BTreeMap<String, bcode_agent_policy_models::AgentConfig>,
    agent_id: &str,
    category: &str,
    pattern: String,
    action: bcode_agent_policy_models::Action,
) -> Result<(), ConfigError> {
    let entry = agent.entry(agent_id.to_string()).or_default();
    let permission = &mut entry.permission;
    let map = match category {
        "bash" => &mut permission.bash,
        "read" => &mut permission.read,
        "write" => &mut permission.write,
        "edit" => &mut permission.edit,
        _ => return Err(ConfigError::UnknownPermissionCategory(category.to_string())),
    };
    map.insert(pattern, action);
    Ok(())
}

fn parse_action(action: &str) -> Result<bcode_agent_policy_models::Action, ConfigError> {
    match action {
        "allow" => Ok(bcode_agent_policy_models::Action::Allow),
        "ask" => Ok(bcode_agent_policy_models::Action::Ask),
        "deny" => Ok(bcode_agent_policy_models::Action::Deny),
        _ => Err(ConfigError::UnknownPermissionAction(action.to_string())),
    }
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
    write_model_toml(&mut output, &config.model);
    write_agents_toml(&mut output, &config.agent);
    write_auth_toml(&mut output, &config.auth);
    write_tui_toml(&mut output, &config.tui);
    output
}

fn write_model_toml(output: &mut String, model: &ModelConfig) {
    if model.provider_plugin_id.is_some()
        || model.model_id.is_some()
        || model.default_thinking_level.is_some()
        || model.max_tool_rounds.is_some()
        || model.profile.is_some()
        || model.prompt_cache != PromptCacheConfig::default()
        || model.conversation_reuse != ConversationReuseConfig::default()
    {
        output.push_str("[model]\n");
        if let Some(profile) = &model.profile {
            writeln!(output, "profile = {}", toml_string(profile))
                .expect("writing to string should not fail");
        }
        if let Some(provider_plugin_id) = &model.provider_plugin_id {
            writeln!(
                output,
                "provider_plugin_id = {}",
                toml_string(provider_plugin_id)
            )
            .expect("writing to string should not fail");
        }
        if let Some(model_id) = &model.model_id {
            writeln!(output, "model_id = {}", toml_string(model_id))
                .expect("writing to string should not fail");
        }
        if let Some(level) = &model.default_thinking_level {
            writeln!(output, "default_thinking_level = \"{level:?}\"")
                .expect("writing to string should not fail");
        }
        if let Some(max_tool_rounds) = model.max_tool_rounds {
            writeln!(output, "max_tool_rounds = {max_tool_rounds}")
                .expect("writing to string should not fail");
        }
        output.push('\n');
    }
    if model.prompt_cache != PromptCacheConfig::default() {
        output.push_str("[model.prompt_cache]\n");
        writeln!(
            output,
            "mode = {}",
            toml_string(prompt_cache_mode_name(model.prompt_cache.mode))
        )
        .expect("writing to string should not fail");
        output.push('\n');
    }
    if model.conversation_reuse != ConversationReuseConfig::default() {
        output.push_str("[model.conversation_reuse]\n");
        writeln!(
            output,
            "mode = {}",
            toml_string(conversation_reuse_mode_name(model.conversation_reuse.mode))
        )
        .expect("writing to string should not fail");
        output.push('\n');
    }
    for (profile_name, profile) in &model.profiles {
        writeln!(output, "[model.profiles.{}]", toml_key(profile_name))
            .expect("writing to string should not fail");
        writeln!(
            output,
            "provider_plugin_id = {}",
            toml_string(&profile.provider_plugin_id)
        )
        .expect("writing to string should not fail");
        if let Some(model_id) = &profile.model_id {
            writeln!(output, "model_id = {}", toml_string(model_id))
                .expect("writing to string should not fail");
        }
        if let Some(auth_profile) = &profile.auth_profile {
            writeln!(output, "auth_profile = {}", toml_string(auth_profile))
                .expect("writing to string should not fail");
        }
        output.push('\n');
        write_string_map_table(
            output,
            &format!("model.profiles.{}.settings", toml_key(profile_name)),
            &profile.settings,
        );
    }
}

fn write_tui_toml(output: &mut String, tui: &TuiConfig) {
    if tui.keybindings.is_empty() {
        return;
    }
    write_tui_keybinding_section(output, "chat", &tui.keybindings.chat);
    write_tui_keybinding_section(output, "permission", &tui.keybindings.permission);
    write_tui_keybinding_section(output, "session_picker", &tui.keybindings.session_picker);
}

fn write_tui_keybinding_section(
    output: &mut String,
    section: &str,
    bindings: &BTreeMap<String, String>,
) {
    if bindings.is_empty() {
        return;
    }
    writeln!(output, "[tui.keybindings.{section}]").expect("writing to string should not fail");
    for (key, action) in bindings {
        writeln!(output, "{} = {}", toml_string(key), toml_string(action))
            .expect("writing to string should not fail");
    }
    output.push('\n');
}

fn write_agents_toml(
    output: &mut String,
    agents: &BTreeMap<String, bcode_agent_policy_models::AgentConfig>,
) {
    for (agent_id, agent) in agents {
        if !agent.tools.is_empty() {
            writeln!(output, "[agent.{}.tools]", toml_table_key(agent_id))
                .expect("writing to string should not fail");
            for (tool, enabled) in &agent.tools {
                writeln!(output, "{} = {}", toml_string(tool), enabled)
                    .expect("writing to string should not fail");
            }
            output.push('\n');
        }

        let permission = &agent.permission;
        let has_permission = !permission.bash.is_empty()
            || !permission.read.is_empty()
            || !permission.write.is_empty()
            || !permission.edit.is_empty()
            || permission.external_directory
                != bcode_agent_policy_models::default_external_directory_action();
        if !has_permission {
            continue;
        }

        writeln!(output, "[agent.{}.permission]", toml_table_key(agent_id))
            .expect("writing to string should not fail");
        if permission.external_directory
            != bcode_agent_policy_models::default_external_directory_action()
        {
            writeln!(
                output,
                "external_directory = {}",
                toml_string(action_name(permission.external_directory))
            )
            .expect("writing to string should not fail");
        }
        write_action_map(output, "bash", &permission.bash);
        write_action_map(output, "read", &permission.read);
        write_action_map(output, "write", &permission.write);
        write_action_map(output, "edit", &permission.edit);
        output.push('\n');
    }
}

fn write_action_map(
    output: &mut String,
    name: &str,
    map: &BTreeMap<String, bcode_agent_policy_models::Action>,
) {
    if map.is_empty() {
        return;
    }
    output.push_str(name);
    output.push_str(" = { ");
    let mut first = true;
    for (pattern, action) in map {
        if !first {
            output.push_str(", ");
        }
        first = false;
        write!(
            output,
            "{} = {}",
            toml_string(pattern),
            toml_string(action_name(*action))
        )
        .expect("writing to string should not fail");
    }
    output.push_str(" }\n");
}

const fn prompt_cache_mode_name(mode: bcode_model::PromptCacheMode) -> &'static str {
    match mode {
        bcode_model::PromptCacheMode::Off => "off",
        bcode_model::PromptCacheMode::Auto => "auto",
        bcode_model::PromptCacheMode::Aggressive => "aggressive",
    }
}

const fn conversation_reuse_mode_name(mode: bcode_model::ConversationReuseMode) -> &'static str {
    match mode {
        bcode_model::ConversationReuseMode::Off => "off",
        bcode_model::ConversationReuseMode::Auto => "auto",
    }
}

const fn action_name(action: bcode_agent_policy_models::Action) -> &'static str {
    match action {
        bcode_agent_policy_models::Action::Allow => "allow",
        bcode_agent_policy_models::Action::Ask => "ask",
        bcode_agent_policy_models::Action::Deny => "deny",
    }
}

fn toml_table_key(key: &str) -> String {
    let needs_quoting = key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if needs_quoting {
        toml_string(key)
    } else {
        key.to_string()
    }
}

fn write_auth_toml(output: &mut String, auth: &AuthConfig) {
    if let Some(openai) = &auth.openai {
        output.push_str("[auth.openai]\n");
        writeln!(output, "backend = {}", toml_string(&openai.backend))
            .expect("writing to string should not fail");
        writeln!(
            output,
            "mode = {}",
            toml_string(auth_mode_name(&openai.mode))
        )
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
    for (profile_name, profile) in &auth.profiles {
        writeln!(output, "[auth.profiles.{}]", toml_key(profile_name))
            .expect("writing to string should not fail");
        writeln!(output, "backend = {}", toml_string(&profile.backend))
            .expect("writing to string should not fail");
        output.push('\n');
        write_string_map_table(
            output,
            &format!("auth.profiles.{}.settings", toml_key(profile_name)),
            &profile.settings,
        );
    }
}

const fn auth_mode_name(mode: &AuthMode) -> &'static str {
    match mode {
        AuthMode::ApiKey => "api_key",
        AuthMode::ChatGpt => "chatgpt",
    }
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

fn write_string_map_table(output: &mut String, table: &str, values: &BTreeMap<String, String>) {
    if values.is_empty() {
        return;
    }
    writeln!(output, "[{table}]").expect("writing to string should not fail");
    for (key, value) in values {
        writeln!(output, "{} = {}", toml_key(key), toml_string(value))
            .expect("writing to string should not fail");
    }
    output.push('\n');
}

fn toml_key(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        value.to_string()
    } else {
        toml_string(value)
    }
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
    use super::{
        BcodeConfig, DEFAULT_AGENT_PROFILE_PLUGIN_ID, PluginSelection, load_config_from_paths,
    };
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

[agent.build.tools]
"shell.run" = true

[agent.build.permission]
external_directory = "ask"
bash = { "cargo *" = "allow", "git push *" = "deny" }
read = { "**" = "allow" }
write = { "target/**" = "allow" }
edit = { "src/**" = "ask" }

[tui.keybindings.chat]
"ctrl+x" = "app.exit"

[tui.keybindings.permission]
"y" = "app.permission.approve"
"escape" = "tui.select.cancel"

[tui.keybindings]
"app.exit" = ["ctrl+d"]
"app.permission.approve" = []
"#,
        )
        .expect("project config should be written");

        let config = load_config_from_paths(&[user, project]).expect("config should load");
        assert!(config.plugins.enabled.contains("example.a"));
        assert!(config.plugins.enabled.contains("example.c"));
        assert!(config.plugins.disabled.contains("example.b"));
        assert!(config.plugins.disabled.contains("example.d"));

        let build = config
            .agent
            .get("build")
            .expect("build agent config should be loaded");
        assert_eq!(
            build.tools.get("shell.run").copied(),
            Some(true),
            "build agent should enable shell.run"
        );
        assert_eq!(
            build.permission.external_directory,
            bcode_agent_policy_models::Action::Ask
        );
        assert_eq!(
            build.permission.bash.get("cargo *").copied(),
            Some(bcode_agent_policy_models::Action::Allow)
        );
        assert_eq!(
            build.permission.bash.get("git push *").copied(),
            Some(bcode_agent_policy_models::Action::Deny)
        );
        assert_eq!(
            build.permission.read.get("**").copied(),
            Some(bcode_agent_policy_models::Action::Allow)
        );
        assert_eq!(
            build.permission.write.get("target/**").copied(),
            Some(bcode_agent_policy_models::Action::Allow)
        );
        assert_eq!(
            build.permission.edit.get("src/**").copied(),
            Some(bcode_agent_policy_models::Action::Ask)
        );
        assert_eq!(
            config.tui.keybindings.chat.get("ctrl+x"),
            Some(&"app.exit".to_string())
        );
        assert_eq!(
            config.tui.keybindings.permission.get("y"),
            Some(&"app.permission.approve".to_string())
        );
        assert_eq!(
            config.tui.keybindings.legacy_actions.get("app.exit"),
            Some(&vec!["ctrl+d".to_string()])
        );
        assert_eq!(
            config
                .tui
                .keybindings
                .legacy_actions
                .get("app.permission.approve"),
            Some(&Vec::new())
        );

        std::fs::remove_dir_all(root).expect("temp root should clean up");
    }

    #[test]
    fn resolves_active_model_profile() {
        let config: BcodeConfig = toml::from_str(
            r#"
[model]
profile = "bedrock-work"

[model.profiles.bedrock-work]
provider_plugin_id = "bcode.bedrock"
model_id = "anthropic.claude-3-5-sonnet-20241022-v2:0"
auth_profile = "aws-work"

[model.profiles.bedrock-work.settings]
region = "us-east-1"

[auth.profiles.aws-work]
backend = "aws_default_chain"

[auth.profiles.aws-work.settings]
profile = "work"
"#,
        )
        .expect("profile config should parse");

        let selection = config.resolved_model_selection();
        assert_eq!(
            selection.provider_plugin_id,
            Some("bcode.bedrock".to_string())
        );
        assert_eq!(
            selection.model_id,
            Some("anthropic.claude-3-5-sonnet-20241022-v2:0".to_string())
        );
        assert_eq!(selection.auth_profile, Some("aws-work".to_string()));
        assert_eq!(
            selection.settings.get("region"),
            Some(&"us-east-1".to_string())
        );
    }

    #[test]
    fn parses_prompt_cache_mode() {
        let config: BcodeConfig = toml::from_str(
            r#"
[model.prompt_cache]
mode = "off"
"#,
        )
        .expect("config should parse");

        assert_eq!(
            config.model.prompt_cache.mode,
            bcode_model::PromptCacheMode::Off
        );
    }

    #[test]
    fn parses_conversation_reuse_mode() {
        let config: BcodeConfig = toml::from_str(
            r#"
[model.conversation_reuse]
mode = "auto"
"#,
        )
        .expect("config should parse");

        assert_eq!(
            config.model.conversation_reuse.mode,
            bcode_model::ConversationReuseMode::Auto
        );
    }

    #[test]
    fn max_tool_rounds_zero_means_unlimited() {
        let config: BcodeConfig = toml::from_str(
            r"
[model]
max_tool_rounds = 0
",
        )
        .expect("config should parse");

        assert_eq!(config.model.effective_max_tool_rounds(), None);
    }

    #[test]
    fn positive_max_tool_rounds_is_effective() {
        let config: BcodeConfig = toml::from_str(
            r"
[model]
max_tool_rounds = 3
",
        )
        .expect("config should parse");

        assert_eq!(config.model.effective_max_tool_rounds(), Some(3));
    }

    #[test]
    fn default_plugin_selection_includes_default_agent_profiles() {
        let config = BcodeConfig::default();
        let plugin_selection = PluginSelection::from(&config);

        assert!(
            plugin_selection
                .enabled
                .contains(DEFAULT_AGENT_PROFILE_PLUGIN_ID)
        );
    }

    #[test]
    fn explicit_plugin_selection_still_includes_default_agent_profiles() {
        let config: BcodeConfig = toml::from_str(
            r#"
[plugins]
enabled = ["bcode.bedrock"]
"#,
        )
        .expect("config should parse");
        let plugin_selection = PluginSelection::from(&config);

        assert!(plugin_selection.enabled.contains("bcode.bedrock"));
        assert!(
            plugin_selection
                .enabled
                .contains(DEFAULT_AGENT_PROFILE_PLUGIN_ID)
        );
    }

    #[test]
    fn default_agent_profiles_can_be_disabled() {
        let config: BcodeConfig = toml::from_str(
            r#"
[plugins]
disabled = ["bcode.default-agents"]
"#,
        )
        .expect("config should parse");
        let plugin_selection = PluginSelection::from(&config);

        assert!(
            !plugin_selection
                .enabled
                .contains(DEFAULT_AGENT_PROFILE_PLUGIN_ID)
        );
    }

    #[test]
    fn bedrock_env_overrides_persisted_openai_provider() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_token = std::env::var_os("AWS_BEARER_TOKEN_BEDROCK");
        let previous_model = std::env::var_os("BCODE_BEDROCK_MODEL");
        unsafe {
            std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", "test-token");
            std::env::remove_var("BCODE_BEDROCK_MODEL");
        }

        let config: BcodeConfig = toml::from_str(
            r#"
[plugins]
enabled = ["bcode.openai-compatible"]

[model]
provider_plugin_id = "bcode.openai-compatible"
model_id = "gpt-4.1-mini"
"#,
        )
        .expect("config should parse");
        let selection = config.resolved_model_selection();
        assert_eq!(
            selection.provider_plugin_id,
            Some("bcode.bedrock".to_string())
        );
        assert_eq!(selection.model_id, None);

        let plugin_selection = PluginSelection::from(&config);
        assert!(plugin_selection.enabled.contains("bcode.bedrock"));
        assert!(
            plugin_selection
                .enabled
                .contains(DEFAULT_AGENT_PROFILE_PLUGIN_ID)
        );
        assert!(!plugin_selection.enabled.contains("bcode.openai-compatible"));

        restore_env("AWS_BEARER_TOKEN_BEDROCK", previous_token);
        restore_env("BCODE_BEDROCK_MODEL", previous_model);
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("bcode-config-test-{nanos}"))
    }
}
