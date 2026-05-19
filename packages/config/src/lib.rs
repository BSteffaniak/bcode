#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Configuration loading for Bcode.

use bcode_plugin::PluginSelection;
use bcode_skill_models::SkillId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Default Bcode config file name.
pub const DEFAULT_CONFIG_FILE_NAME: &str = "bcode.toml";

const DEFAULT_AGENT_PROFILE_PLUGIN_ID: &str = "bcode.default-agents";
const DEFAULT_FILESYSTEM_PLUGIN_ID: &str = "bcode.filesystem";
const DEFAULT_SHELL_PLUGIN_ID: &str = "bcode.shell";
const DEFAULT_MODEL_PROVIDER_PLUGIN_ID: &str = "bcode.openai-compatible";
const DEFAULT_CORE_PLUGIN_IDS: &[&str] = &[
    DEFAULT_FILESYSTEM_PLUGIN_ID,
    DEFAULT_SHELL_PLUGIN_ID,
    DEFAULT_AGENT_PROFILE_PLUGIN_ID,
];

struct ProviderEnvironmentSpec {
    plugin_id: &'static str,
    aliases: &'static [&'static str],
    signal_env_vars: &'static [&'static str],
    model_env_vars: &'static [&'static str],
    config_auth_is_configured: fn(&BcodeConfig) -> bool,
}

const PROVIDER_ENVIRONMENT_SPECS: &[ProviderEnvironmentSpec] = &[
    ProviderEnvironmentSpec {
        plugin_id: "bcode.bedrock",
        aliases: &["bedrock", "aws-bedrock", "aws_bedrock", "bcode.bedrock"],
        signal_env_vars: &[
            "AWS_BEARER_TOKEN_BEDROCK",
            "BCODE_BEDROCK_MODEL",
            "BCODE_BEDROCK_MODELS",
            "BCODE_BEDROCK_REGION",
            "BCODE_BEDROCK_AWS_PROFILE",
            "BCODE_BEDROCK_ENDPOINT_URL",
            "BEDROCK_MODEL",
            "BEDROCK_MODELS",
            "BEDROCK_ENDPOINT_URL",
        ],
        model_env_vars: &["BCODE_BEDROCK_MODEL", "BEDROCK_MODEL"],
        config_auth_is_configured: no_config_auth_signal,
    },
    ProviderEnvironmentSpec {
        plugin_id: "bcode.openai-compatible",
        aliases: &[
            "openai",
            "openai-compatible",
            "openai_compatible",
            "xai",
            "grok",
            "bcode.openai-compatible",
        ],
        signal_env_vars: &[
            "BCODE_XAI_API_KEY",
            "XAI_API_KEY",
            "BCODE_XAI_MODEL",
            "XAI_MODEL",
            "BCODE_XAI_MODELS",
            "XAI_MODELS",
            "BCODE_XAI_BASE_URL",
            "XAI_BASE_URL",
            "BCODE_OPENAI_API_KEY",
            "OPENAI_API_KEY",
            "BCODE_OPENAI_MODEL",
            "OPENAI_MODEL",
            "BCODE_OPENAI_MODELS",
            "OPENAI_MODELS",
            "BCODE_OPENAI_BASE_URL",
            "OPENAI_BASE_URL",
            "BCODE_OPENAI_CODEX_ACCESS_TOKEN",
            "BCODE_OPENAI_CODEX_REFRESH_TOKEN",
            "BCODE_OPENAI_CODEX_ID_TOKEN",
        ],
        model_env_vars: &[
            "BCODE_XAI_MODEL",
            "XAI_MODEL",
            "BCODE_OPENAI_MODEL",
            "OPENAI_MODEL",
        ],
        config_auth_is_configured: openai_config_auth_is_configured,
    },
];

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
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
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
        self.observability.merge(next.observability);
        self.skills.merge(next.skills);
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
        if selection.provider_plugin_id.is_none()
            && let Some(config_provider) = self.provider_plugin_id_from_config_auth()
        {
            selection.provider_plugin_id = Some(config_provider);
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
        if let Some(provider_plugin_id) = &selection.provider_plugin_id
            && let Some(model_id) = model_id_from_environment(provider_plugin_id)
        {
            selection.model_id = Some(model_id);
        }
        selection
    }

    fn provider_plugin_id_from_config_auth(&self) -> Option<String> {
        PROVIDER_ENVIRONMENT_SPECS
            .iter()
            .find(|spec| (spec.config_auth_is_configured)(self))
            .map(|spec| spec.plugin_id.to_string())
    }
}

const fn no_config_auth_signal(_config: &BcodeConfig) -> bool {
    false
}

fn openai_config_auth_is_configured(config: &BcodeConfig) -> bool {
    config
        .auth
        .openai
        .as_ref()
        .is_some_and(|auth| auth.backend == "sshenv")
}

/// Return a provider plugin ID explicitly or implicitly selected by environment variables.
#[must_use]
pub fn provider_plugin_id_from_environment() -> Option<String> {
    first_env_value(["BCODE_MODEL_PROVIDER", "BCODE_PROVIDER"])
        .and_then(|value| normalize_provider_plugin_id(&value))
        .or_else(implicit_provider_plugin_id_from_environment)
}

fn implicit_provider_plugin_id_from_environment() -> Option<String> {
    PROVIDER_ENVIRONMENT_SPECS.iter().find_map(|spec| {
        first_env_value_from_slice(spec.signal_env_vars).map(|_| spec.plugin_id.to_string())
    })
}

fn normalize_provider_plugin_id(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    PROVIDER_ENVIRONMENT_SPECS
        .iter()
        .find(|spec| spec.aliases.contains(&value.as_str()))
        .map(|spec| spec.plugin_id.to_string())
}

fn model_id_from_environment(provider_plugin_id: &str) -> Option<String> {
    PROVIDER_ENVIRONMENT_SPECS
        .iter()
        .find(|spec| spec.plugin_id == provider_plugin_id)
        .and_then(|spec| first_env_value_from_slice(spec.model_env_vars))
}

fn first_env_value<const N: usize>(names: [&str; N]) -> Option<String> {
    first_env_value_from_slice(&names)
}

fn first_env_value_from_slice(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    })
}

/// Return true when environment variables imply Bedrock should be selected.
#[must_use]
pub fn bedrock_environment_is_configured() -> bool {
    PROVIDER_ENVIRONMENT_SPECS
        .iter()
        .find(|spec| spec.plugin_id == "bcode.bedrock")
        .is_some_and(|spec| first_env_value_from_slice(spec.signal_env_vars).is_some())
}

/// Skill discovery and activation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SkillsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub auto_activate: SkillAutoActivateMode,
    #[serde(default = "default_true")]
    pub include_repo_skills: bool,
    #[serde(default = "default_true")]
    pub include_user_skills: bool,
    #[serde(default = "default_true")]
    pub include_compat_claude_skills: bool,
    #[serde(default = "default_skill_context_bytes")]
    pub max_context_bytes: usize,
    #[serde(default = "default_skill_file_bytes")]
    pub max_skill_file_bytes: u64,
    #[serde(default = "default_skill_resource_file_bytes")]
    pub max_resource_file_bytes: u64,
    #[serde(default)]
    pub follow_symlinks: bool,
    #[serde(default)]
    pub sources: SkillSourceConfig,
    #[serde(default)]
    pub disabled: DisabledSkillsConfig,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_activate: SkillAutoActivateMode::Suggest,
            include_repo_skills: true,
            include_user_skills: true,
            include_compat_claude_skills: true,
            max_context_bytes: default_skill_context_bytes(),
            max_skill_file_bytes: default_skill_file_bytes(),
            max_resource_file_bytes: default_skill_resource_file_bytes(),
            follow_symlinks: false,
            sources: SkillSourceConfig::default(),
            disabled: DisabledSkillsConfig::default(),
        }
    }
}

impl SkillsConfig {
    fn merge(&mut self, next: Self) {
        *self = next;
    }

    /// Return disabled skill IDs in registry form.
    #[must_use]
    pub fn disabled_skill_ids(&self) -> BTreeSet<SkillId> {
        self.disabled
            .ids
            .iter()
            .cloned()
            .map(SkillId::new)
            .collect()
    }
}

/// Skill auto-activation behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillAutoActivateMode {
    Off,
    #[default]
    Suggest,
    On,
}

/// Additional skill source paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSourceConfig {
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

/// Disabled skill IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisabledSkillsConfig {
    #[serde(default)]
    pub ids: BTreeSet<String>,
}

const fn default_skill_context_bytes() -> usize {
    24 * 1024
}

const fn default_skill_file_bytes() -> u64 {
    256 * 1024
}

const fn default_skill_resource_file_bytes() -> u64 {
    1024 * 1024
}

const fn default_true() -> bool {
    true
}

/// Session observability configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    /// Trace detail level.
    #[serde(default)]
    pub level: ObservabilityLevel,
    /// Persist full provider-neutral model requests as trace blobs.
    #[serde(default)]
    pub persist_model_requests: bool,
    /// Persist tool arguments and outputs as trace blobs.
    #[serde(default = "default_true")]
    pub persist_tool_io: bool,
    /// Maximum bytes to keep for a single trace blob.
    #[serde(default = "default_max_trace_blob_bytes")]
    pub max_blob_bytes: usize,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            level: ObservabilityLevel::Standard,
            persist_model_requests: false,
            persist_tool_io: true,
            max_blob_bytes: default_max_trace_blob_bytes(),
        }
    }
}

impl ObservabilityConfig {
    fn merge(&mut self, next: Self) {
        if next != Self::default() {
            *self = next;
        }
    }

    /// Return true when diagnostic trace events should be persisted.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        !matches!(self.level, ObservabilityLevel::Off)
    }

    /// Return true when debug-level details should be persisted.
    #[must_use]
    pub const fn debug_enabled(&self) -> bool {
        matches!(
            self.level,
            ObservabilityLevel::Debug | ObservabilityLevel::Raw
        )
    }
}

/// Session observability detail level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservabilityLevel {
    Off,
    #[default]
    Standard,
    Debug,
    Raw,
}

const fn default_max_trace_blob_bytes() -> usize {
    10 * 1024 * 1024
}

/// Terminal UI configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiConfig {
    /// Scoped keybindings keyed by key stroke. Values are action IDs.
    #[serde(default)]
    pub keybindings: TuiKeyBindingConfig,
    /// Mouse interaction configuration.
    #[serde(default)]
    pub mouse: TuiMouseConfig,
}

impl TuiConfig {
    fn merge(&mut self, next: Self) {
        self.keybindings.merge(next.keybindings);
        self.mouse = next.mouse;
    }
}

/// Terminal UI mouse interaction configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiMouseConfig {
    /// Maximum milliseconds between clicks in the same sequence.
    #[serde(default = "default_mouse_multi_click_ms")]
    pub multi_click_ms: u64,
    /// Maximum terminal-cell distance between clicks in the same sequence.
    #[serde(default)]
    pub multi_click_max_distance: u16,
    /// Double-click selection behavior.
    #[serde(default)]
    pub double_click_select: TuiMouseClickSelection,
    /// Triple-click selection behavior.
    #[serde(default = "default_triple_click_select")]
    pub triple_click_select: TuiMouseClickSelection,
}

impl Default for TuiMouseConfig {
    fn default() -> Self {
        Self {
            multi_click_ms: default_mouse_multi_click_ms(),
            multi_click_max_distance: 0,
            double_click_select: TuiMouseClickSelection::Word,
            triple_click_select: default_triple_click_select(),
        }
    }
}

const fn default_mouse_multi_click_ms() -> u64 {
    500
}

const fn default_triple_click_select() -> TuiMouseClickSelection {
    TuiMouseClickSelection::All
}

/// Selection behavior for a mouse click count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiMouseClickSelection {
    /// Do not select on this click count.
    Disabled,
    /// Select a word.
    #[default]
    Word,
    /// Select the current line.
    Line,
    /// Select the whole editable buffer.
    All,
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
    pub tool_output: ToolOutputConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
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
        if next.tool_output != ToolOutputConfig::default() {
            self.tool_output = next.tool_output;
        }
        if next.streaming != StreamingConfig::default() {
            self.streaming = next.streaming;
        }
        if next.compaction != CompactionConfig::default() {
            self.compaction = next.compaction;
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

/// Tool output context policy for future model turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputConfig {
    /// Maximum characters of each tool result included directly in model context.
    #[serde(default = "default_tool_output_context_chars")]
    pub context_chars: usize,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            context_chars: default_tool_output_context_chars(),
        }
    }
}

/// Provider streaming progress and timeout configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamingConfig {
    /// Seconds without meaningful provider progress before Bcode shows a warning.
    #[serde(default = "default_streaming_no_progress_warning_secs")]
    pub no_progress_warning_secs: u64,
    /// Seconds without meaningful provider progress before Bcode times out the turn.
    #[serde(default = "default_streaming_no_progress_timeout_secs")]
    pub no_progress_timeout_secs: u64,
    /// Minimum streamed argument bytes between visible progress updates.
    #[serde(default = "default_streaming_progress_event_interval_bytes")]
    pub progress_event_interval_bytes: usize,
    /// Minimum seconds between visible progress updates.
    #[serde(default = "default_streaming_progress_event_interval_secs")]
    pub progress_event_interval_secs: u64,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            no_progress_warning_secs: default_streaming_no_progress_warning_secs(),
            no_progress_timeout_secs: default_streaming_no_progress_timeout_secs(),
            progress_event_interval_bytes: default_streaming_progress_event_interval_bytes(),
            progress_event_interval_secs: default_streaming_progress_event_interval_secs(),
        }
    }
}

/// Automatic context compaction configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Automatic compaction mode. Defaults to `auto`.
    #[serde(default)]
    pub mode: CompactionMode,
    /// Projected conversation character count that triggers automatic compaction.
    #[serde(default = "default_auto_compaction_context_chars")]
    pub context_chars: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            mode: CompactionMode::OnOverflow,
            context_chars: default_auto_compaction_context_chars(),
        }
    }
}

/// Automatic context compaction mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// Disable automatic compaction entirely. Manual compaction remains available.
    Off,
    /// Compact only after the provider reports a context-length overflow.
    #[default]
    OnOverflow,
    /// Compact before model turns when the projected context character threshold is exceeded.
    Proactive,
    /// Compact proactively and also recover from provider context-length overflows.
    ProactiveAndOverflow,
    /// Legacy spelling for automatic compaction.
    Auto,
}

impl CompactionMode {
    /// Return whether proactive threshold-based compaction may run.
    #[must_use]
    pub const fn is_proactive_enabled(self) -> bool {
        matches!(
            self,
            Self::Auto | Self::Proactive | Self::ProactiveAndOverflow
        )
    }

    /// Return whether provider context-length overflow should trigger compaction and retry.
    #[must_use]
    pub const fn is_overflow_recovery_enabled(self) -> bool {
        matches!(
            self,
            Self::Auto | Self::OnOverflow | Self::ProactiveAndOverflow
        )
    }
}

const fn default_tool_output_context_chars() -> usize {
    4_000
}

const fn default_streaming_no_progress_warning_secs() -> u64 {
    30
}

const fn default_streaming_no_progress_timeout_secs() -> u64 {
    300
}

const fn default_streaming_progress_event_interval_bytes() -> usize {
    256 * 1024
}

const fn default_streaming_progress_event_interval_secs() -> u64 {
    2
}

const fn default_auto_compaction_context_chars() -> usize {
    120_000
}

/// Provider-native conversation reuse configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationReuseConfig {
    /// Conversation reuse mode. Defaults to `auto` so providers can use native continuation when supported.
    #[serde(default = "default_conversation_reuse_mode")]
    pub mode: bcode_model::ConversationReuseMode,
}

impl Default for ConversationReuseConfig {
    fn default() -> Self {
        Self {
            mode: default_conversation_reuse_mode(),
        }
    }
}

const fn default_conversation_reuse_mode() -> bcode_model::ConversationReuseMode {
    bcode_model::ConversationReuseMode::Auto
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
        let had_explicit_enabled_plugins = !selection.enabled.is_empty();
        let env_provider = provider_plugin_id_from_environment();
        let resolved_provider = value.resolved_model_selection().provider_plugin_id;
        let provider = env_provider
            .clone()
            .or_else(|| resolved_provider.clone())
            .unwrap_or_else(|| DEFAULT_MODEL_PROVIDER_PLUGIN_ID.to_string());

        enable_default_core_plugins(&mut selection);
        if !had_explicit_enabled_plugins {
            enable_plugin_unless_disabled(&mut selection, &provider);
        } else if let Some(env_provider) = env_provider {
            enable_plugin_unless_disabled(&mut selection, &env_provider);
            remove_other_model_providers(&mut selection.enabled, &env_provider);
        } else if let Some(resolved_provider) = resolved_provider {
            enable_plugin_unless_disabled(&mut selection, &resolved_provider);
        }
        selection
    }
}

fn enable_default_core_plugins(selection: &mut PluginSelection) {
    for plugin_id in DEFAULT_CORE_PLUGIN_IDS {
        enable_plugin_unless_disabled(selection, plugin_id);
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

/// Upsert a permission rule under `[agent.<agent_id>.permission.<category>]` in
/// the runtime permissions state file.
///
/// Runtime rules live in `$BCODE_STATE_DIR/permissions.toml` (or the XDG state
/// directory) rather than `bcode.toml`, so declarative configuration (for
/// example a read-only Nix-managed `bcode.toml`) is never touched. When merged
/// at load time, state-file rules win over same-pattern rules declared in
/// `bcode.toml`.
///
/// `category` must be one of `bash`, `read`, `write`, or `edit`.
/// `action` must be one of `allow`, `ask`, or `deny`.
///
/// # Errors
///
/// Returns an error when the state file cannot be read, the category or action
/// is unknown, or the updated file cannot be written.
pub fn upsert_agent_permission_rule(
    agent_id: &str,
    category: &str,
    pattern: String,
    action: &str,
) -> Result<PathBuf, ConfigError> {
    let action = parse_action(action)?;
    update_permissions_state(|agents| {
        insert_agent_permission_rule(agents, agent_id, category, pattern, action)
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

/// Return the default runtime permissions state file path.
///
/// Runtime "always allow" / "always deny" clicks from the TUI and
/// `bcode permission add` invocations persist to this path instead of the
/// user's `bcode.toml`, so declarative configuration (for example a Nix-managed
/// read-only `bcode.toml`) is never mutated at runtime.
///
/// Resolution precedence:
///
/// * `$BCODE_PERMISSIONS_STATE` if set.
/// * Otherwise `<default_state_dir>/permissions.toml`.
#[must_use]
pub fn default_permissions_state_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_PERMISSIONS_STATE") {
        return PathBuf::from(path);
    }
    default_state_dir().join("permissions.toml")
}

/// Load the runtime permissions state file.
///
/// Returns an empty agent map when the file does not exist. The file uses the
/// same `[agent.<id>.permission.<category>]` schema as `bcode.toml`, so rules
/// can be promoted to declarative config by copying entries verbatim.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_permissions_state()
-> Result<BTreeMap<String, bcode_agent_policy_models::AgentConfig>, ConfigError> {
    load_permissions_state_from(&default_permissions_state_path())
}

/// Load the runtime permissions state file from an explicit path.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_permissions_state_from(
    path: &Path,
) -> Result<BTreeMap<String, bcode_agent_policy_models::AgentConfig>, ConfigError> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Reuse the full config parser so the `[agent.<id>]` shape matches exactly.
    let config: BcodeConfig = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(config.agent)
}

/// Merge a state-file agent map over a declarative agent map.
///
/// State entries win per `(agent, category, pattern)`: a rule appearing in the
/// state map replaces the same-pattern rule in the declarative map. Patterns
/// present only in the declarative map survive untouched. Tool enablement
/// (`[agent.<id>.tools]`) and `external_directory` fields also take the state
/// value when set in the state map.
pub fn merge_agent_configs(
    base: &mut BTreeMap<String, bcode_agent_policy_models::AgentConfig>,
    overlay: BTreeMap<String, bcode_agent_policy_models::AgentConfig>,
) {
    for (agent_id, overlay_agent) in overlay {
        let entry = base.entry(agent_id).or_default();
        for (tool, enabled) in overlay_agent.tools {
            entry.tools.insert(tool, enabled);
        }
        // External directory: treat a non-default state value as an override.
        let overlay_external = overlay_agent.permission.external_directory;
        if overlay_external != bcode_agent_policy_models::default_external_directory_action() {
            entry.permission.external_directory = overlay_external;
        }
        for (pattern, action) in overlay_agent.permission.bash {
            entry.permission.bash.insert(pattern, action);
        }
        for (pattern, action) in overlay_agent.permission.read {
            entry.permission.read.insert(pattern, action);
        }
        for (pattern, action) in overlay_agent.permission.write {
            entry.permission.write.insert(pattern, action);
        }
        for (pattern, action) in overlay_agent.permission.edit {
            entry.permission.edit.insert(pattern, action);
        }
    }
}

fn update_permissions_state(
    update: impl FnOnce(
        &mut BTreeMap<String, bcode_agent_policy_models::AgentConfig>,
    ) -> Result<(), ConfigError>,
) -> Result<PathBuf, ConfigError> {
    let path = default_permissions_state_path();
    let mut agents = if path.exists() {
        load_permissions_state_from(&path)?
    } else {
        BTreeMap::new()
    };
    update(&mut agents)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, permissions_state_to_toml(&agents)).map_err(|source| {
        ConfigError::Io {
            path: path.clone(),
            source,
        }
    })?;
    Ok(path)
}

fn permissions_state_to_toml(
    agents: &BTreeMap<String, bcode_agent_policy_models::AgentConfig>,
) -> String {
    let mut output = String::new();
    output.push_str("# Bcode runtime permissions state. Managed automatically by\n");
    output.push_str("# `bcode permission add` and the TUI always-allow prompts.\n");
    output.push_str("# Entries here win over same-pattern rules in bcode.toml.\n\n");
    write_agents_toml(&mut output, agents);
    output
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
    write_observability_toml(&mut output, &config.observability);
    write_skills_toml(&mut output, &config.skills);
    write_tui_toml(&mut output, &config.tui);
    output
}

fn write_model_compaction_toml(output: &mut String, compaction: &CompactionConfig) {
    if compaction == &CompactionConfig::default() {
        return;
    }
    output.push_str("[model.compaction]\n");
    writeln!(
        output,
        "mode = {}",
        toml_string(compaction_mode_name(compaction.mode))
    )
    .expect("writing to string should not fail");
    writeln!(output, "context_chars = {}", compaction.context_chars)
        .expect("writing to string should not fail");
    output.push('\n');
}

fn write_model_tool_output_toml(output: &mut String, tool_output: &ToolOutputConfig) {
    if tool_output == &ToolOutputConfig::default() {
        return;
    }
    output.push_str("[model.tool_output]\n");
    writeln!(output, "context_chars = {}", tool_output.context_chars)
        .expect("writing to string should not fail");
    output.push('\n');
}

fn write_model_streaming_toml(output: &mut String, streaming: &StreamingConfig) {
    if streaming == &StreamingConfig::default() {
        return;
    }
    output.push_str("[model.streaming]\n");
    writeln!(
        output,
        "no_progress_warning_secs = {}",
        streaming.no_progress_warning_secs
    )
    .expect("writing to string should not fail");
    writeln!(
        output,
        "no_progress_timeout_secs = {}",
        streaming.no_progress_timeout_secs
    )
    .expect("writing to string should not fail");
    writeln!(
        output,
        "progress_event_interval_bytes = {}",
        streaming.progress_event_interval_bytes
    )
    .expect("writing to string should not fail");
    writeln!(
        output,
        "progress_event_interval_secs = {}",
        streaming.progress_event_interval_secs
    )
    .expect("writing to string should not fail");
    output.push('\n');
}

fn write_model_toml(output: &mut String, model: &ModelConfig) {
    if model.provider_plugin_id.is_some()
        || model.model_id.is_some()
        || model.default_thinking_level.is_some()
        || model.max_tool_rounds.is_some()
        || model.profile.is_some()
        || model.prompt_cache != PromptCacheConfig::default()
        || model.conversation_reuse != ConversationReuseConfig::default()
        || model.tool_output != ToolOutputConfig::default()
        || model.streaming != StreamingConfig::default()
        || model.compaction != CompactionConfig::default()
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
    write_model_tool_output_toml(output, &model.tool_output);
    write_model_streaming_toml(output, &model.streaming);
    if model.compaction != CompactionConfig::default() {
        write_model_compaction_toml(output, &model.compaction);
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

const fn compaction_mode_name(mode: CompactionMode) -> &'static str {
    match mode {
        CompactionMode::Off => "off",
        CompactionMode::OnOverflow => "on_overflow",
        CompactionMode::Proactive => "proactive",
        CompactionMode::ProactiveAndOverflow => "proactive_and_overflow",
        CompactionMode::Auto => "auto",
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

fn write_skills_toml(output: &mut String, skills: &SkillsConfig) {
    if skills == &SkillsConfig::default() {
        return;
    }
    output.push_str("[skills]\n");
    if !skills.enabled {
        output.push_str("enabled = false\n");
    }
    if skills.auto_activate != SkillAutoActivateMode::Suggest {
        writeln!(
            output,
            "auto_activate = {}",
            toml_string(skill_auto_activate_mode_name(skills.auto_activate))
        )
        .expect("write to string");
    }
    if !skills.include_repo_skills {
        output.push_str("include_repo_skills = false\n");
    }
    if !skills.include_user_skills {
        output.push_str("include_user_skills = false\n");
    }
    if !skills.include_compat_claude_skills {
        output.push_str("include_compat_claude_skills = false\n");
    }
    if skills.max_context_bytes != default_skill_context_bytes() {
        writeln!(output, "max_context_bytes = {}", skills.max_context_bytes)
            .expect("write to string");
    }
    if skills.max_skill_file_bytes != default_skill_file_bytes() {
        writeln!(
            output,
            "max_skill_file_bytes = {}",
            skills.max_skill_file_bytes
        )
        .expect("write to string");
    }
    if skills.max_resource_file_bytes != default_skill_resource_file_bytes() {
        writeln!(
            output,
            "max_resource_file_bytes = {}",
            skills.max_resource_file_bytes
        )
        .expect("write to string");
    }
    if skills.follow_symlinks {
        output.push_str("follow_symlinks = true\n");
    }
    output.push('\n');

    if !skills.sources.paths.is_empty() {
        output.push_str("[skills.sources]\npaths = [");
        for (index, path) in skills.sources.paths.iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            output.push_str(&toml_string(&path.to_string_lossy()));
        }
        output.push_str("]\n\n");
    }

    if !skills.disabled.ids.is_empty() {
        output.push_str("[skills.disabled]\n");
        write_string_set(output, "ids", &skills.disabled.ids);
        output.push('\n');
    }
}

const fn skill_auto_activate_mode_name(mode: SkillAutoActivateMode) -> &'static str {
    match mode {
        SkillAutoActivateMode::Off => "off",
        SkillAutoActivateMode::Suggest => "suggest",
        SkillAutoActivateMode::On => "on",
    }
}

fn write_observability_toml(output: &mut String, observability: &ObservabilityConfig) {
    if observability == &ObservabilityConfig::default() {
        return;
    }
    output.push_str("[observability]\n");
    writeln!(
        output,
        "level = {}",
        toml_string(observability_level_name(observability.level))
    )
    .expect("writing to string should not fail");
    if observability.persist_model_requests {
        output.push_str("persist_model_requests = true\n");
    }
    if !observability.persist_tool_io {
        output.push_str("persist_tool_io = false\n");
    }
    if observability.max_blob_bytes != default_max_trace_blob_bytes() {
        writeln!(output, "max_blob_bytes = {}", observability.max_blob_bytes)
            .expect("writing to string should not fail");
    }
    output.push('\n');
}

const fn observability_level_name(level: ObservabilityLevel) -> &'static str {
    match level {
        ObservabilityLevel::Off => "off",
        ObservabilityLevel::Standard => "standard",
        ObservabilityLevel::Debug => "debug",
        ObservabilityLevel::Raw => "raw",
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
        BcodeConfig, CompactionMode, DEFAULT_AGENT_PROFILE_PLUGIN_ID, DEFAULT_FILESYSTEM_PLUGIN_ID,
        DEFAULT_SHELL_PLUGIN_ID, PluginSelection, default_permissions_state_path,
        load_config_from_paths, load_permissions_state_from, merge_agent_configs,
        upsert_agent_permission_rule,
    };
    use bcode_agent_policy_models::{Action, AgentConfig, PermissionConfig};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn tui_mouse_config_loads_from_toml() {
        let config: BcodeConfig = toml::from_str(
            r#"
[tui.mouse]
multi_click_ms = 300
multi_click_max_distance = 1
double_click_select = "word"
triple_click_select = "all"
"#,
        )
        .expect("config should parse");

        assert_eq!(config.tui.mouse.multi_click_ms, 300);
        assert_eq!(config.tui.mouse.multi_click_max_distance, 1);
        assert_eq!(
            config.tui.mouse.double_click_select,
            super::TuiMouseClickSelection::Word
        );
        assert_eq!(
            config.tui.mouse.triple_click_select,
            super::TuiMouseClickSelection::All
        );
    }

    fn assert_default_core_plugins_enabled(plugin_selection: &PluginSelection) {
        assert!(
            plugin_selection
                .enabled
                .contains(DEFAULT_FILESYSTEM_PLUGIN_ID)
        );
        assert!(plugin_selection.enabled.contains(DEFAULT_SHELL_PLUGIN_ID));
        assert!(
            plugin_selection
                .enabled
                .contains(DEFAULT_AGENT_PROFILE_PLUGIN_ID)
        );
    }

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
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
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

        restore_provider_env(previous_env);
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
    fn defaults_conversation_reuse_to_auto() {
        let config = BcodeConfig::default();

        assert_eq!(
            config.model.conversation_reuse.mode,
            bcode_model::ConversationReuseMode::Auto
        );
    }

    #[test]
    fn parses_conversation_reuse_mode() {
        let config: BcodeConfig = toml::from_str(
            r#"
[model.conversation_reuse]
mode = "off"
"#,
        )
        .expect("config should parse");

        assert_eq!(
            config.model.conversation_reuse.mode,
            bcode_model::ConversationReuseMode::Off
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
    fn default_model_limits_are_unlimited_and_context_policies_enabled() {
        let config = BcodeConfig::default();

        assert_eq!(config.model.effective_max_tool_rounds(), None);
        assert_eq!(config.model.tool_output.context_chars, 4_000);
        assert_eq!(config.model.compaction.mode, CompactionMode::OnOverflow);
        assert_eq!(config.model.compaction.context_chars, 120_000);
    }

    #[test]
    fn parses_tool_output_context_limit() {
        let config: BcodeConfig = toml::from_str(
            r"
[model.tool_output]
context_chars = 1200
",
        )
        .expect("config should parse");

        assert_eq!(config.model.tool_output.context_chars, 1_200);
    }

    #[test]
    fn parses_auto_compaction_config() {
        let config: BcodeConfig = toml::from_str(
            r#"
[model.compaction]
mode = "off"
context_chars = 90000
"#,
        )
        .expect("config should parse");

        assert_eq!(config.model.compaction.mode, CompactionMode::Off);
        assert_eq!(config.model.compaction.context_chars, 90_000);
    }

    #[test]
    fn parses_new_compaction_modes() {
        for (mode, expected) in [
            ("on_overflow", CompactionMode::OnOverflow),
            ("proactive", CompactionMode::Proactive),
            (
                "proactive_and_overflow",
                CompactionMode::ProactiveAndOverflow,
            ),
            ("auto", CompactionMode::Auto),
        ] {
            let config: BcodeConfig = toml::from_str(&format!(
                r#"
[model.compaction]
mode = "{mode}"
"#
            ))
            .expect("config should parse");

            assert_eq!(config.model.compaction.mode, expected);
        }
    }

    #[test]
    fn default_plugin_selection_includes_default_core_plugins() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        let config = BcodeConfig::default();
        let plugin_selection = PluginSelection::from(&config);

        assert_default_core_plugins_enabled(&plugin_selection);

        restore_provider_env(previous_env);
    }

    #[test]
    fn explicit_plugin_selection_still_includes_default_core_plugins() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        let config: BcodeConfig = toml::from_str(
            r#"
[plugins]
enabled = ["bcode.openai-compatible"]
"#,
        )
        .expect("config should parse");
        let plugin_selection = PluginSelection::from(&config);

        assert!(plugin_selection.enabled.contains("bcode.openai-compatible"));
        assert_default_core_plugins_enabled(&plugin_selection);

        restore_provider_env(previous_env);
    }

    #[test]
    fn default_agent_profiles_can_be_disabled() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
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
        assert!(
            plugin_selection
                .enabled
                .contains(DEFAULT_FILESYSTEM_PLUGIN_ID)
        );
        assert!(plugin_selection.enabled.contains(DEFAULT_SHELL_PLUGIN_ID));

        restore_provider_env(previous_env);
    }

    #[test]
    fn default_tool_plugins_can_be_disabled() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        let config: BcodeConfig = toml::from_str(
            r#"
[plugins]
enabled = ["bcode.openai-compatible"]
disabled = ["bcode.shell"]
"#,
        )
        .expect("config should parse");
        let plugin_selection = PluginSelection::from(&config);

        assert!(
            plugin_selection
                .enabled
                .contains(DEFAULT_FILESYSTEM_PLUGIN_ID)
        );
        assert!(
            plugin_selection
                .enabled
                .contains(DEFAULT_AGENT_PROFILE_PLUGIN_ID)
        );
        assert!(!plugin_selection.enabled.contains(DEFAULT_SHELL_PLUGIN_ID));

        restore_provider_env(previous_env);
    }

    #[test]
    fn bedrock_env_overrides_persisted_openai_provider() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        unsafe {
            std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", "test-token");
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
        assert_default_core_plugins_enabled(&plugin_selection);
        assert!(!plugin_selection.enabled.contains("bcode.openai-compatible"));

        restore_provider_env(previous_env);
    }

    #[test]
    fn openai_env_overrides_persisted_bedrock_provider() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }

        let config: BcodeConfig = toml::from_str(
            r#"
[plugins]
enabled = ["bcode.bedrock"]

[model]
provider_plugin_id = "bcode.bedrock"
model_id = "anthropic.claude-test"
"#,
        )
        .expect("config should parse");
        let selection = config.resolved_model_selection();
        assert_eq!(
            selection.provider_plugin_id,
            Some("bcode.openai-compatible".to_string())
        );
        assert_eq!(selection.model_id, None);

        let plugin_selection = PluginSelection::from(&config);
        assert!(plugin_selection.enabled.contains("bcode.openai-compatible"));
        assert_default_core_plugins_enabled(&plugin_selection);
        assert!(!plugin_selection.enabled.contains("bcode.bedrock"));

        restore_provider_env(previous_env);
    }

    #[test]
    fn explicit_provider_env_wins_over_provider_specific_env() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        unsafe {
            std::env::set_var("BCODE_PROVIDER", "bedrock");
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }

        let config = BcodeConfig::default();
        let selection = config.resolved_model_selection();
        assert_eq!(
            selection.provider_plugin_id,
            Some("bcode.bedrock".to_string())
        );
        assert_eq!(selection.model_id, None);

        restore_provider_env(previous_env);
    }

    #[test]
    fn provider_env_model_overrides_same_provider_config_model() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        unsafe {
            std::env::set_var("BCODE_OPENAI_MODEL", "env-model");
        }

        let config: BcodeConfig = toml::from_str(
            r#"
[model]
provider_plugin_id = "bcode.openai-compatible"
model_id = "config-model"
"#,
        )
        .expect("config should parse");
        let selection = config.resolved_model_selection();
        assert_eq!(
            selection.provider_plugin_id,
            Some("bcode.openai-compatible".to_string())
        );
        assert_eq!(selection.model_id, Some("env-model".to_string()));

        restore_provider_env(previous_env);
    }

    #[test]
    fn same_provider_config_model_survives_without_env_model() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }

        let config: BcodeConfig = toml::from_str(
            r#"
[model]
provider_plugin_id = "bcode.openai-compatible"
model_id = "config-model"
"#,
        )
        .expect("config should parse");
        let selection = config.resolved_model_selection();
        assert_eq!(
            selection.provider_plugin_id,
            Some("bcode.openai-compatible".to_string())
        );
        assert_eq!(selection.model_id, Some("config-model".to_string()));

        restore_provider_env(previous_env);
    }

    #[test]
    fn config_auth_selects_openai_provider_when_model_provider_is_omitted() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();

        let config: BcodeConfig = toml::from_str(
            r#"
[auth.openai]
backend = "sshenv"
profile = "openai-max"
mode = "chatgpt"
"#,
        )
        .expect("config should parse");
        let selection = config.resolved_model_selection();
        assert_eq!(
            selection.provider_plugin_id,
            Some("bcode.openai-compatible".to_string())
        );

        restore_provider_env(previous_env);
    }

    #[test]
    fn upsert_permission_rule_writes_state_file_only() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let state_path = root.join("permissions.toml");
        let config_path = root.join("bcode.toml");
        std::fs::write(
            &config_path,
            r#"[agent.build.permission]
bash = { "cargo *" = "allow" }
"#,
        )
        .expect("declarative config should be written");

        let previous_state = std::env::var_os("BCODE_PERMISSIONS_STATE");
        let previous_config = std::env::var_os("BCODE_CONFIG");
        unsafe {
            std::env::set_var("BCODE_PERMISSIONS_STATE", &state_path);
            std::env::set_var("BCODE_CONFIG", &config_path);
        }

        let written =
            upsert_agent_permission_rule("build", "bash", "echo hello".to_string(), "allow")
                .expect("state write should succeed");

        assert_eq!(written, state_path);
        assert_eq!(default_permissions_state_path(), state_path);

        let declarative_after = std::fs::read_to_string(&config_path)
            .expect("declarative config should still be readable");
        assert!(
            declarative_after.contains("cargo *"),
            "declarative bcode.toml must not be rewritten by runtime rule upsert"
        );
        assert!(
            !declarative_after.contains("echo hello"),
            "runtime rule must not leak into declarative bcode.toml"
        );

        let state_after =
            std::fs::read_to_string(&state_path).expect("state file should be written");
        assert!(state_after.contains("echo hello"));

        let loaded = load_permissions_state_from(&state_path).expect("state file should load");
        assert_eq!(
            loaded
                .get("build")
                .and_then(|agent| agent.permission.bash.get("echo hello").copied()),
            Some(Action::Allow)
        );

        restore_env("BCODE_PERMISSIONS_STATE", previous_state);
        restore_env("BCODE_CONFIG", previous_config);
    }

    #[test]
    fn merge_agent_configs_state_wins_on_same_pattern() {
        let mut base = BTreeMap::from([(
            "build".to_string(),
            AgentConfig {
                tools: BTreeMap::new(),
                permission: PermissionConfig {
                    bash: BTreeMap::from([
                        ("cargo *".to_string(), Action::Allow),
                        ("git push *".to_string(), Action::Deny),
                    ]),
                    ..PermissionConfig::default()
                },
            },
        )]);
        let overlay = BTreeMap::from([(
            "build".to_string(),
            AgentConfig {
                tools: BTreeMap::new(),
                permission: PermissionConfig {
                    bash: BTreeMap::from([
                        // Flip the declarative allow to deny; that is the
                        // "state wins" contract the user signed up for.
                        ("cargo *".to_string(), Action::Deny),
                        // Add a brand-new pattern.
                        ("echo *".to_string(), Action::Allow),
                    ]),
                    ..PermissionConfig::default()
                },
            },
        )]);

        merge_agent_configs(&mut base, overlay);

        let build = base.get("build").expect("build agent should exist");
        assert_eq!(
            build.permission.bash.get("cargo *").copied(),
            Some(Action::Deny)
        );
        assert_eq!(
            build.permission.bash.get("git push *").copied(),
            Some(Action::Deny),
            "declarative-only rules survive the merge"
        );
        assert_eq!(
            build.permission.bash.get("echo *").copied(),
            Some(Action::Allow)
        );
    }

    #[test]
    fn merge_agent_configs_state_only_agent_is_added() {
        let mut base: BTreeMap<String, AgentConfig> = BTreeMap::new();
        let overlay = BTreeMap::from([(
            "scratch".to_string(),
            AgentConfig {
                tools: BTreeMap::from([("shell.run".to_string(), true)]),
                permission: PermissionConfig {
                    bash: BTreeMap::from([("*".to_string(), Action::Ask)]),
                    ..PermissionConfig::default()
                },
            },
        )]);

        merge_agent_configs(&mut base, overlay);

        let scratch = base.get("scratch").expect("scratch agent should be added");
        assert_eq!(scratch.tools.get("shell.run").copied(), Some(true));
        assert_eq!(scratch.permission.bash.get("*").copied(), Some(Action::Ask));
    }

    #[test]
    fn load_permissions_state_missing_file_returns_empty() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let missing = root.join("nonexistent.toml");
        let loaded = load_permissions_state_from(&missing).expect("missing file should be ok");
        assert!(loaded.is_empty());
    }

    const TEST_PROVIDER_ENV_NAMES: &[&str] = &[
        "BCODE_MODEL_PROVIDER",
        "BCODE_PROVIDER",
        "AWS_BEARER_TOKEN_BEDROCK",
        "BCODE_BEDROCK_MODEL",
        "BCODE_BEDROCK_MODELS",
        "BCODE_BEDROCK_REGION",
        "BCODE_BEDROCK_AWS_PROFILE",
        "BCODE_BEDROCK_ENDPOINT_URL",
        "BEDROCK_MODEL",
        "BEDROCK_MODELS",
        "BEDROCK_ENDPOINT_URL",
        "BCODE_XAI_API_KEY",
        "XAI_API_KEY",
        "BCODE_XAI_MODEL",
        "XAI_MODEL",
        "BCODE_XAI_MODELS",
        "XAI_MODELS",
        "BCODE_XAI_BASE_URL",
        "XAI_BASE_URL",
        "BCODE_OPENAI_API_KEY",
        "OPENAI_API_KEY",
        "BCODE_OPENAI_MODEL",
        "OPENAI_MODEL",
        "BCODE_OPENAI_MODELS",
        "OPENAI_MODELS",
        "BCODE_OPENAI_BASE_URL",
        "OPENAI_BASE_URL",
        "BCODE_OPENAI_CODEX_ACCESS_TOKEN",
        "BCODE_OPENAI_CODEX_REFRESH_TOKEN",
        "BCODE_OPENAI_CODEX_ID_TOKEN",
    ];

    fn clear_provider_env() -> Vec<(&'static str, Option<std::ffi::OsString>)> {
        let previous = TEST_PROVIDER_ENV_NAMES
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect::<Vec<_>>();
        unsafe {
            for name in TEST_PROVIDER_ENV_NAMES {
                std::env::remove_var(name);
            }
        }
        previous
    }

    fn restore_provider_env(previous: Vec<(&'static str, Option<std::ffi::OsString>)>) {
        for (name, value) in previous {
            restore_env(name, value);
        }
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
