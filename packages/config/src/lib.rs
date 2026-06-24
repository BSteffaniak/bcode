#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Configuration loading for Bcode.

use bcode_plugin::PluginSelection;
use bcode_skill_models::SkillId;
pub use hyperchad_docs_config::{ConfigDocSchema, FieldDoc, NestedFieldDoc};
use hyperchad_docs_config_derive::{ConfigDoc, ConfigDocEnum};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};
use thiserror::Error;

/// Default Bcode config file name.
pub const DEFAULT_CONFIG_FILE_NAME: &str = "bcode.toml";
/// Environment variable containing a config file path overlay.
pub const BCODE_CONFIG_ENV: &str = "BCODE_CONFIG";
/// Environment variable containing raw TOML config overlay data.
pub const BCODE_CONFIG_TOML_ENV: &str = "BCODE_CONFIG_TOML";
/// Environment variable selecting the active model profile.
pub const BCODE_MODEL_PROFILE_ENV: &str = "BCODE_MODEL_PROFILE";
/// Environment variable selecting the active auth profile for this client.
pub const BCODE_AUTH_PROFILE_ENV: &str = "BCODE_AUTH_PROFILE";

const DEFAULT_MODEL_PROVIDER_PLUGIN_ID: &str = "bcode.openai-compatible";
const DEFAULT_MODEL_PROVIDER_PLUGIN_IDS: &[&str] = &["bcode.openai-compatible", "bcode.bedrock"];

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BcodeConfig {
    #[serde(default, skip_serializing)]
    pub composition: CompositionConfig,
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
    pub system_prompt: SystemPromptConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(default)]
    pub session_import: SessionImportConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub worktree: WorktreeConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default = "empty_toml_table")]
    pub web_search: toml::Value,
}

impl Default for BcodeConfig {
    fn default() -> Self {
        Self {
            composition: CompositionConfig::default(),
            plugins: PluginConfig::default(),
            model: ModelConfig::default(),
            agent: BTreeMap::new(),
            auth: AuthConfig::default(),
            observability: ObservabilityConfig::default(),
            skills: SkillsConfig::default(),
            system_prompt: SystemPromptConfig::default(),
            tui: TuiConfig::default(),
            session_import: SessionImportConfig::default(),
            daemon: DaemonConfig::default(),
            worktree: WorktreeConfig::default(),
            tools: ToolsConfig::default(),
            web_search: empty_toml_table(),
        }
    }
}

impl ConfigDocSchema for BcodeConfig {
    fn section_name() -> &'static str {
        "bcode"
    }

    fn section_description() -> &'static str {
        "Top-level Bcode configuration."
    }

    fn field_docs() -> Vec<FieldDoc> {
        vec![
            section_doc(
                "composition",
                "Config composition metadata and profile selection.",
                composition_field_docs(),
            ),
            section_doc(
                "plugins",
                "Bundled and external plugin selection.",
                plugin_field_docs(),
            ),
            section_doc(
                "model",
                "Model provider, profile, alias, metadata, retry, and compaction settings.",
                model_field_docs(),
            ),
            section_doc(
                "agent",
                "Per-agent permission and tool policy configuration.",
                agent_field_docs(),
            ),
            section_doc(
                "auth",
                "Provider authentication profiles, pools, and runtime subscription behavior.",
                auth_field_docs(),
            ),
            schema_section_doc::<ObservabilityConfig>(
                "observability",
                "Logging, tracing, and telemetry controls.",
            ),
            schema_section_doc::<SkillsConfig>(
                "skills",
                "Skill discovery, activation, source, disabled-skill, and prompt catalog settings.",
            ),
            schema_section_doc::<SystemPromptConfig>(
                "system_prompt",
                "System prompt mode and section controls.",
            ),
            section_doc(
                "tui",
                "Terminal UI behavior and appearance.",
                tui_field_docs(),
            ),
            schema_section_doc::<SessionImportConfig>(
                "session_import",
                "External session import plugin settings.",
            ),
            schema_section_doc::<DaemonConfig>(
                "daemon",
                "Daemon lifecycle and connection settings.",
            ),
            schema_section_doc::<WorktreeConfig>(
                "worktree",
                "Worktree creation and naming defaults.",
            ),
            schema_section_doc::<ToolsConfig>(
                "tools",
                "Built-in tool behavior and environment controls.",
            ),
            section_doc(
                "web_search",
                "Provider-specific web search plugin configuration.",
                web_search_field_docs(),
            ),
        ]
    }

    fn default_values() -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

const fn section_doc(
    toml_key: &'static str,
    description: &'static str,
    fields: Vec<FieldDoc>,
) -> FieldDoc {
    FieldDoc {
        toml_key,
        type_display: "table",
        description,
        enum_values: None,
        nested: Some(NestedFieldDoc::Inline {
            fields,
            defaults: BTreeMap::new(),
        }),
    }
}

fn schema_section_doc<T: ConfigDocSchema>(
    toml_key: &'static str,
    description: &'static str,
) -> FieldDoc {
    section_doc(toml_key, description, T::field_docs())
}

const fn config_field(
    toml_key: &'static str,
    type_display: &'static str,
    description: &'static str,
) -> FieldDoc {
    FieldDoc {
        toml_key,
        type_display,
        description,
        enum_values: None,
        nested: None,
    }
}

const fn enum_field(
    toml_key: &'static str,
    type_display: &'static str,
    description: &'static str,
    values: &'static [&'static str],
) -> FieldDoc {
    FieldDoc {
        toml_key,
        type_display,
        description,
        enum_values: Some(values),
        nested: None,
    }
}

const fn nested_field(
    toml_key: &'static str,
    description: &'static str,
    fields: Vec<FieldDoc>,
) -> FieldDoc {
    FieldDoc {
        toml_key,
        type_display: "table",
        description,
        enum_values: None,
        nested: Some(NestedFieldDoc::Inline {
            fields,
            defaults: BTreeMap::new(),
        }),
    }
}

const fn map_field(
    toml_key: &'static str,
    description: &'static str,
    key_placeholder: &'static str,
    value_fields: Vec<FieldDoc>,
) -> FieldDoc {
    FieldDoc {
        toml_key,
        type_display: "map",
        description,
        enum_values: None,
        nested: Some(NestedFieldDoc::Map {
            key_placeholder,
            value_fields,
            value_defaults: BTreeMap::new(),
        }),
    }
}

fn composition_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field(
            "active_profile",
            "string",
            "Active composition profile id used when `profile:active` appears in the layer order.",
        ),
        config_field(
            "layer_order",
            "array<string>",
            "Explicit config layer precedence order. Supported entries include `defaults`, `config`, `profile:active`, and `profile:<id>`.",
        ),
        map_field(
            "profiles",
            "Named composition profiles that can override the base config.",
            "<profile>",
            vec![
                config_field(
                    "description",
                    "string",
                    "Human-readable profile description.",
                ),
                config_field("config", "table", "Profile-local config overlay."),
            ],
        ),
    ]
}

fn plugin_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field(
            "external_dirs",
            "array<string>",
            "Directories scanned for external plugin manifests.",
        ),
        config_field(
            "disabled",
            "array<string>",
            "Plugin ids disabled even if bundled or discovered.",
        ),
        map_field(
            "bundled",
            "Per-plugin bundled plugin selection overrides.",
            "<plugin-id>",
            vec![
                enum_field(
                    "mode",
                    "enum",
                    "Plugin selection mode.",
                    &["auto", "enabled", "disabled"],
                ),
                config_field(
                    "path",
                    "string",
                    "Optional explicit plugin path for this selection.",
                ),
            ],
        ),
    ]
}

fn model_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field(
            "provider_plugin_id",
            "string",
            "Default model provider plugin id.",
        ),
        config_field("model_id", "string", "Default provider-specific model id."),
        config_field(
            "profile",
            "string",
            "Active model profile name selected from `model.profiles`.",
        ),
        map_field(
            "profiles",
            "Named model profiles for provider/model/auth/request overrides.",
            "<profile>",
            model_profile_field_docs(),
        ),
        map_field(
            "aliases",
            "Named model aliases resolved before provider selection.",
            "<alias>",
            vec![
                config_field(
                    "provider_plugin_id",
                    "string",
                    "Provider plugin id selected by the alias.",
                ),
                config_field(
                    "model_id",
                    "string",
                    "Provider-specific model id selected by the alias.",
                ),
                config_field(
                    "profile",
                    "string",
                    "Optional model profile selected by the alias.",
                ),
            ],
        ),
        map_field(
            "metadata",
            "Provider/model metadata keyed by provider and model id.",
            "<provider-or-model>",
            vec![
                config_field("label", "string", "Human-readable display label."),
                config_field(
                    "context_window",
                    "integer",
                    "Approximate context window token count.",
                ),
                config_field(
                    "max_output_tokens",
                    "integer",
                    "Maximum output token count.",
                ),
            ],
        ),
        nested_field(
            "reasoning",
            "Default reasoning request controls.",
            reasoning_field_docs(),
        ),
        nested_field(
            "request",
            "Default provider request overrides.",
            request_field_docs(),
        ),
        nested_field("retry", "Provider retry behavior.", retry_field_docs()),
        nested_field(
            "compaction",
            "Conversation compaction behavior.",
            compaction_field_docs(),
        ),
        nested_field(
            "conversation_reuse",
            "Conversation reuse behavior.",
            conversation_reuse_field_docs(),
        ),
    ]
}

fn model_profile_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field(
            "provider_plugin_id",
            "string",
            "Provider plugin id for this profile.",
        ),
        config_field(
            "model_id",
            "string",
            "Provider-specific model id for this profile.",
        ),
        config_field(
            "auth_profile",
            "string",
            "Auth profile name selected from `auth.profiles`.",
        ),
        config_field(
            "auth_pool",
            "string",
            "Auth pool name selected from `auth.pools`.",
        ),
        config_field(
            "settings",
            "table",
            "Provider-specific persistent settings.",
        ),
        config_field(
            "request.temperature",
            "number",
            "Provider-specific sampling temperature override.",
        ),
        config_field(
            "request.top_p",
            "number",
            "Provider-specific nucleus sampling probability override.",
        ),
        config_field(
            "request.max_tokens",
            "integer",
            "Provider-specific maximum response token override.",
        ),
        config_field(
            "reasoning.effort",
            "string",
            "Provider-specific reasoning effort value.",
        ),
        config_field(
            "reasoning.summary",
            "string",
            "Provider-specific reasoning summary value.",
        ),
    ]
}

fn reasoning_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field(
            "effort",
            "string",
            "Provider-specific reasoning effort value.",
        ),
        config_field(
            "summary",
            "string",
            "Provider-specific reasoning summary value.",
        ),
    ]
}

fn request_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field("temperature", "number", "Sampling temperature override."),
        config_field("top_p", "number", "Nucleus sampling probability override."),
        config_field("max_tokens", "integer", "Maximum response token override."),
    ]
}

fn retry_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field(
            "max_attempts",
            "integer",
            "Maximum provider request attempts.",
        ),
        config_field(
            "initial_backoff_ms",
            "integer",
            "Initial retry backoff in milliseconds.",
        ),
        config_field(
            "max_backoff_ms",
            "integer",
            "Maximum retry backoff in milliseconds.",
        ),
    ]
}

fn compaction_field_docs() -> Vec<FieldDoc> {
    vec![
        enum_field(
            "mode",
            "enum",
            "Conversation compaction mode.",
            &["off", "auto"],
        ),
        config_field(
            "target_tokens",
            "integer",
            "Target token budget after compaction.",
        ),
        config_field(
            "trigger_tokens",
            "integer",
            "Token budget threshold that triggers compaction.",
        ),
    ]
}

fn conversation_reuse_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field(
            "enabled",
            "bool",
            "Whether compatible previous conversation context may be reused.",
        ),
        config_field(
            "max_age_seconds",
            "integer",
            "Maximum age for reusable conversation context.",
        ),
    ]
}

fn agent_field_docs() -> Vec<FieldDoc> {
    vec![map_field(
        "<agent-id>",
        "Configuration for a named agent profile such as `build` or `plan`.",
        "<agent-id>",
        vec![
            map_field(
                "tools",
                "Per-tool permission policy keyed by tool name.",
                "<tool-name>",
                vec![
                    enum_field(
                        "default",
                        "enum",
                        "Default action for the tool.",
                        &["allow", "ask", "deny"],
                    ),
                    enum_field(
                        "read",
                        "enum",
                        "Read action for the tool.",
                        &["allow", "ask", "deny"],
                    ),
                    enum_field(
                        "write",
                        "enum",
                        "Write action for the tool.",
                        &["allow", "ask", "deny"],
                    ),
                    enum_field(
                        "execute",
                        "enum",
                        "Execute action for the tool.",
                        &["allow", "ask", "deny"],
                    ),
                ],
            ),
            nested_field(
                "permissions",
                "Fallback permission actions by operation class.",
                vec![
                    enum_field(
                        "read",
                        "enum",
                        "Default read permission.",
                        &["allow", "ask", "deny"],
                    ),
                    enum_field(
                        "write",
                        "enum",
                        "Default write permission.",
                        &["allow", "ask", "deny"],
                    ),
                    enum_field(
                        "execute",
                        "enum",
                        "Default execute permission.",
                        &["allow", "ask", "deny"],
                    ),
                    enum_field(
                        "network",
                        "enum",
                        "Default network permission.",
                        &["allow", "ask", "deny"],
                    ),
                ],
            ),
        ],
    )]
}

fn auth_field_docs() -> Vec<FieldDoc> {
    vec![
        config_field(
            "active_profile",
            "string",
            "Default auth profile selected when no environment override is present.",
        ),
        map_field(
            "profiles",
            "Named provider auth profiles.",
            "<profile>",
            vec![
                config_field(
                    "provider_plugin_id",
                    "string",
                    "Provider plugin id this auth profile applies to.",
                ),
                config_field("mode", "string", "Provider-specific authentication mode."),
                config_field("api_key", "string", "Provider API key or token value."),
                config_field(
                    "api_key_env",
                    "string",
                    "Environment variable containing the provider API key.",
                ),
                config_field("base_url", "string", "Provider base URL override."),
                config_field(
                    "extra",
                    "table",
                    "Provider-specific additional auth settings.",
                ),
            ],
        ),
        map_field(
            "pools",
            "Named auth profile pools used for failover or load distribution.",
            "<pool>",
            vec![
                config_field(
                    "profiles",
                    "array<string>",
                    "Auth profile names included in this pool.",
                ),
                enum_field(
                    "strategy",
                    "enum",
                    "Pool selection strategy.",
                    &["round_robin", "sticky", "random"],
                ),
            ],
        ),
        nested_field(
            "runtime_subscriptions",
            "Runtime subscription auth settings.",
            vec![map_field(
                "pools",
                "Runtime subscription pools keyed by provider/plugin-specific pool id.",
                "<pool>",
                vec![
                    enum_field(
                        "strategy",
                        "enum",
                        "Runtime pool strategy.",
                        &["round_robin", "sticky", "random"],
                    ),
                    map_field(
                        "profiles",
                        "Runtime subscription profiles.",
                        "<profile>",
                        vec![
                            config_field("provider_plugin_id", "string", "Provider plugin id."),
                            config_field(
                                "auth_profile",
                                "string",
                                "Config auth profile name to use.",
                            ),
                        ],
                    ),
                ],
            )],
        ),
    ]
}

fn tui_field_docs() -> Vec<FieldDoc> {
    vec![
        enum_field(
            "thinking",
            "enum",
            "How model thinking/reasoning output is displayed.",
            &["hidden", "summary", "expanded"],
        ),
        enum_field(
            "mouse_click_selection",
            "enum",
            "Mouse click selection behavior.",
            &["off", "single", "double"],
        ),
        nested_field(
            "accent_transition",
            "Accent color transition behavior.",
            vec![
                enum_field(
                    "mode",
                    "enum",
                    "Accent transition mode.",
                    &["off", "instant", "animated"],
                ),
                enum_field(
                    "curve",
                    "enum",
                    "Accent transition curve.",
                    &["linear", "ease_in", "ease_out", "ease_in_out"],
                ),
                config_field(
                    "duration_ms",
                    "integer",
                    "Transition duration in milliseconds.",
                ),
            ],
        ),
    ]
}

fn web_search_field_docs() -> Vec<FieldDoc> {
    vec![config_field(
        "<provider-key>",
        "value",
        "Provider-specific web search plugin option. Supported keys depend on the enabled web-search plugin.",
    )]
}

fn empty_toml_table() -> toml::Value {
    toml::Value::Table(toml::Table::new())
}

impl BcodeConfig {
    /// Resolve the active model profile to a concrete provider/model selection.
    #[must_use]
    pub fn resolved_model_selection(&self) -> ResolvedModelSelection {
        let mut selection = ResolvedModelSelection {
            provider_plugin_id: self.model.provider_plugin_id.clone(),
            model_id: self.model.model_id.clone(),
            selected_model_id: self.model.model_id.clone(),
            model_profile: self.model.profile.clone(),
            auth_profile: None,
            auth_pool: None,
            settings: BTreeMap::new(),
            request: BTreeMap::new(),
            reasoning: self.model.reasoning.clone(),
        };
        if let Some(profile_name) = &self.model.profile
            && let Some(profile) = self.model.profiles.get(profile_name)
        {
            selection.provider_plugin_id = Some(profile.provider_plugin_id.clone());
            if profile.model_id.is_some() {
                selection.model_id.clone_from(&profile.model_id);
            }
            selection.auth_profile.clone_from(&profile.auth_profile);
            selection.auth_pool.clone_from(&profile.auth_pool);
            selection.settings = profile.settings.clone();
            selection.request = provider_request_values_from_json(&profile.request);
            selection.reasoning = merge_reasoning_config(&self.model.reasoning, &profile.reasoning);
        }
        self.apply_model_alias(&mut selection);
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
                selection.auth_pool = None;
                selection.settings.clear();
            }
        }
        if let Some(provider_plugin_id) = &selection.provider_plugin_id
            && let Some(model_id) = model_id_from_environment(provider_plugin_id)
        {
            selection.model_id = Some(model_id);
        }
        self.apply_model_metadata_override(&mut selection);
        selection
    }

    fn apply_model_metadata_override(&self, selection: &mut ResolvedModelSelection) {
        let Some(model_id) = selection.model_id.as_deref() else {
            return;
        };
        let Some(metadata) = self.model.metadata.get(model_id) else {
            return;
        };
        if let Some(provider_plugin_id) = metadata.provider_plugin_id.as_deref()
            && selection.provider_plugin_id.as_deref() != Some(provider_plugin_id)
        {
            return;
        }
        if let Some(context_window) = metadata.context_window {
            selection.settings.insert(
                format!("model_metadata.{model_id}.context_window"),
                context_window.to_string(),
            );
        }
        if let Some(max_output_tokens) = metadata.max_output_tokens {
            selection.settings.insert(
                format!("model_metadata.{model_id}.max_output_tokens"),
                max_output_tokens.to_string(),
            );
        }
        if let Some(reasoning) = reasoning_capabilities_from_config(&metadata.reasoning) {
            insert_model_reasoning_settings(&mut selection.settings, model_id, &reasoning);
        }
    }

    fn apply_model_alias(&self, selection: &mut ResolvedModelSelection) {
        let Some(selected_model_id) = selection.model_id.clone() else {
            return;
        };
        let Some(alias) = self.model.aliases.get(&selected_model_id) else {
            selection.selected_model_id = Some(selected_model_id);
            return;
        };
        selection.selected_model_id = Some(selected_model_id);
        if let Some(provider_plugin_id) = &alias.provider_plugin_id {
            selection.provider_plugin_id = Some(provider_plugin_id.clone());
        }
        selection.model_id = Some(alias.model_id.clone());
        let mut request = provider_request_values_from_json(&alias.request);
        request.extend(selection.request.clone());
        selection.request = request;
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

fn provider_request_values_from_json(
    values: &BTreeMap<String, serde_json::Value>,
) -> BTreeMap<String, bcode_model::ProviderRequestValue> {
    values
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                bcode_model::ProviderRequestValue::from(value.clone()),
            )
        })
        .collect()
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

/// Config composition metadata and profile selection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompositionConfig {
    /// Active profile id to apply when `profile:active` appears in the layer order.
    pub active_profile: Option<String>,
    /// Explicit layer precedence order. Supported values are `defaults`, `config`,
    /// `profile:active`, and `profile:<id>`.
    pub layer_order: Vec<String>,
    /// User-defined composition profiles keyed by profile id.
    pub profiles: BTreeMap<String, CompositionProfile>,
}

/// Reusable config profile patch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompositionProfile {
    /// Parent profiles applied left-to-right before this profile patch.
    pub extends: Vec<String>,
    /// Raw partial `BcodeConfig` TOML patch.
    pub patch: toml::Table,
}

/// Config load resolution details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionResolution {
    /// Profile selected by composition, if any.
    pub selected_profile: Option<String>,
    /// Effective layer order after defaulting.
    pub layer_order: Vec<String>,
    /// Profile ids available while resolving composition.
    pub available_profiles: Vec<String>,
}

/// Config loading overrides layered above discovered config files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLoadOverrides {
    /// Optional base layer merged below discovered config files.
    pub base_config_path: Option<PathBuf>,
    /// Config file path from environment or caller-provided equivalent.
    pub env_config_path: Option<PathBuf>,
    /// Raw TOML config data from environment or caller-provided equivalent.
    pub env_config_toml: Option<String>,
    /// Config file path from CLI arguments.
    pub cli_config_path: Option<PathBuf>,
    /// Raw TOML config data synthesized from CLI arguments.
    pub cli_config_toml: Option<String>,
}

impl ConfigLoadOverrides {
    /// Build overrides from `BCODE_CONFIG`, `BCODE_CONFIG_TOML`, and optional CLI values.
    #[must_use]
    pub fn from_env_with_cli(
        cli_config_path: Option<PathBuf>,
        cli_config_toml: Option<String>,
    ) -> Self {
        Self {
            base_config_path: None,
            env_config_path: env::var_os(BCODE_CONFIG_ENV).map(PathBuf::from),
            env_config_toml: merge_config_toml_overrides(
                env::var(BCODE_CONFIG_TOML_ENV).ok(),
                env::var(BCODE_MODEL_PROFILE_ENV)
                    .ok()
                    .filter(|profile| !profile.trim().is_empty())
                    .map(|profile| model_profile_override_toml(&profile)),
            ),
            cli_config_path,
            cli_config_toml,
        }
    }

    /// Return true when no override layers are configured.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.base_config_path.is_none()
            && self.env_config_path.is_none()
            && self.env_config_toml.is_none()
            && self.cli_config_path.is_none()
            && self.cli_config_toml.is_none()
    }

    /// Set base config path.
    #[must_use]
    pub fn with_base_config_path(mut self, path: Option<PathBuf>) -> Self {
        self.base_config_path = path;
        self
    }

    /// Fluent setter for the CLI raw TOML override.
    #[must_use]
    pub fn with_cli_config_toml(mut self, toml: Option<String>) -> Self {
        self.cli_config_toml = merge_config_toml_overrides(self.cli_config_toml, toml);
        self
    }
}

/// Build a TOML override selecting a model profile.
#[must_use]
pub fn model_profile_override_toml(profile: &str) -> String {
    format!("[model]\nprofile = {}\n", toml_string(profile))
}

/// Build a TOML override for worktree base ref.
#[must_use]
pub fn worktree_base_ref_override_toml(base_ref: WorktreeBaseRefConfig) -> String {
    let value = match base_ref {
        WorktreeBaseRefConfig::Auto => "auto",
        WorktreeBaseRefConfig::DefaultBranch => "default_branch",
        WorktreeBaseRefConfig::Head => "head",
    };
    format!("[worktree]\nbase_ref = {}\n", toml_string(value))
}

fn merge_config_toml_overrides(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(mut left), Some(right)) => {
            if !left.ends_with('\n') {
                left.push('\n');
            }
            left.push_str(&right);
            Some(left)
        }
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn process_config_overrides() -> &'static std::sync::RwLock<Option<ConfigLoadOverrides>> {
    static OVERRIDES: std::sync::OnceLock<std::sync::RwLock<Option<ConfigLoadOverrides>>> =
        std::sync::OnceLock::new();
    OVERRIDES.get_or_init(|| std::sync::RwLock::new(None))
}

/// Guard that restores prior process-scoped config load overrides when dropped.
#[derive(Debug)]
pub struct ConfigOverrideGuard {
    previous: Option<ConfigLoadOverrides>,
}

impl Drop for ConfigOverrideGuard {
    fn drop(&mut self) {
        let mut guard = process_config_overrides()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (*guard).clone_from(&self.previous);
    }
}

/// Apply process-scoped config load overrides until the returned guard is dropped.
#[must_use]
pub fn push_process_config_overrides(overrides: ConfigLoadOverrides) -> ConfigOverrideGuard {
    let mut guard = process_config_overrides()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = guard.clone();
    *guard = Some(overrides);
    drop(guard);
    ConfigOverrideGuard { previous }
}

fn canonical_profile_id(profile_id: &str) -> String {
    profile_id.trim().to_ascii_lowercase()
}

/// Recursively merge TOML config values.
///
/// Tables are merged key-by-key. Non-table overlay values replace the base
/// value at the same path.
pub fn merge_config_values(base: &mut toml::Value, overlay: toml::Value) {
    merge_toml_value(base, overlay);
}

fn merge_toml_value(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) => {
            for (key, value) in overlay_table {
                if let Some(existing) = base_table.get_mut(&key) {
                    merge_toml_value(existing, value);
                } else {
                    base_table.insert(key, value);
                }
            }
        }
        (base_slot, overlay_value) => *base_slot = overlay_value,
    }
}

fn parse_composition_config(root: &toml::Table) -> Result<CompositionConfig, ConfigError> {
    let Some(value) = root.get("composition") else {
        return Ok(CompositionConfig::default());
    };
    value
        .clone()
        .try_into()
        .map_err(|source| ConfigError::Composition {
            message: format!("invalid [composition] config: {source}"),
        })
}

fn resolve_profile_patch(
    requested_profile_id: &str,
    profiles: &BTreeMap<String, CompositionProfile>,
) -> Result<toml::Table, ConfigError> {
    fn resolve_inner(
        requested_profile_id: &str,
        profiles: &BTreeMap<String, CompositionProfile>,
        stack: &mut Vec<String>,
        cache: &mut BTreeMap<String, toml::Table>,
    ) -> Result<toml::Table, ConfigError> {
        let canonical_id = canonical_profile_id(requested_profile_id);
        if let Some(resolved) = cache.get(&canonical_id) {
            return Ok(resolved.clone());
        }
        if stack.contains(&canonical_id) {
            let mut cycle = stack.clone();
            cycle.push(canonical_id.clone());
            return Err(ConfigError::Composition {
                message: format!("profile inheritance cycle detected: {}", cycle.join(" -> ")),
            });
        }
        let Some(profile) = profiles.get(&canonical_id) else {
            return Err(ConfigError::Composition {
                message: format!(
                    "profile '{requested_profile_id}' is not defined (known profiles: {})",
                    profiles.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            });
        };

        stack.push(canonical_id.clone());
        let mut resolved = toml::Table::new();
        for parent_id in &profile.extends {
            let parent_patch = resolve_inner(parent_id, profiles, stack, cache)?;
            let mut merged = toml::Value::Table(resolved);
            merge_toml_value(&mut merged, toml::Value::Table(parent_patch));
            resolved = merged.as_table().cloned().unwrap_or_default();
        }
        let mut merged = toml::Value::Table(resolved);
        merge_toml_value(&mut merged, toml::Value::Table(profile.patch.clone()));
        let resolved = merged.as_table().cloned().unwrap_or_default();
        stack.pop();

        cache.insert(canonical_id, resolved.clone());
        Ok(resolved)
    }

    resolve_inner(
        requested_profile_id,
        profiles,
        &mut Vec::new(),
        &mut BTreeMap::new(),
    )
}

fn resolve_composed_config_value(
    raw: &toml::Value,
) -> Result<(toml::Value, CompositionResolution), ConfigError> {
    let mut raw_table = raw
        .as_table()
        .cloned()
        .ok_or_else(|| ConfigError::Composition {
            message: "config root must be a table".to_string(),
        })?;
    let composition = parse_composition_config(&raw_table)?;
    raw_table.remove("composition");

    let mut profiles = BTreeMap::new();
    for (profile_id, profile) in composition.profiles {
        let canonical_id = canonical_profile_id(&profile_id);
        if canonical_id.is_empty() {
            return Err(ConfigError::Composition {
                message: "composition profile id must not be empty".to_string(),
            });
        }
        profiles.insert(canonical_id, profile);
    }

    let active_profile = composition
        .active_profile
        .as_deref()
        .map(canonical_profile_id);
    let layer_order = if composition.layer_order.is_empty() {
        if active_profile.is_some() {
            vec![
                "defaults".to_string(),
                "profile:active".to_string(),
                "config".to_string(),
            ]
        } else {
            vec!["defaults".to_string(), "config".to_string()]
        }
    } else {
        composition.layer_order
    };

    let mut resolved = toml::Value::try_from(BcodeConfig::default()).map_err(|source| {
        ConfigError::Composition {
            message: format!("failed to serialize default config: {source}"),
        }
    })?;

    for layer in &layer_order {
        match layer.as_str() {
            "defaults" => {}
            "config" => merge_toml_value(&mut resolved, toml::Value::Table(raw_table.clone())),
            "profile:active" => {
                let Some(active_profile) = active_profile.as_deref() else {
                    return Err(ConfigError::Composition {
                        message: "layer 'profile:active' requires composition.active_profile"
                            .to_string(),
                    });
                };
                let patch = resolve_profile_patch(active_profile, &profiles)?;
                merge_toml_value(&mut resolved, toml::Value::Table(patch));
            }
            _ if layer.starts_with("profile:") => {
                let profile_id = layer.trim_start_matches("profile:");
                let patch = resolve_profile_patch(profile_id, &profiles)?;
                merge_toml_value(&mut resolved, toml::Value::Table(patch));
            }
            unknown => {
                return Err(ConfigError::Composition {
                    message: format!("unknown composition layer '{unknown}'"),
                });
            }
        }
    }

    let mut available_profiles = profiles.keys().cloned().collect::<Vec<_>>();
    available_profiles.sort();
    Ok((
        resolved,
        CompositionResolution {
            selected_profile: active_profile,
            layer_order,
            available_profiles,
        },
    ))
}

/// Return true when environment variables imply Bedrock should be selected.
#[must_use]
pub fn bedrock_environment_is_configured() -> bool {
    PROVIDER_ENVIRONMENT_SPECS
        .iter()
        .find(|spec| spec.plugin_id == "bcode.bedrock")
        .is_some_and(|spec| first_env_value_from_slice(spec.signal_env_vars).is_some())
}

/// System prompt assembly configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "system_prompt")]
pub struct SystemPromptConfig {
    /// Base system prompt mode.
    #[serde(default)]
    pub mode: SystemPromptMode,
    /// Replacement system prompt text when replacement mode is active.
    #[serde(default)]
    pub text: Option<String>,
    /// Toggleable built-in system prompt sections.
    #[config_doc(nested)]
    #[serde(default)]
    pub sections: SystemPromptSectionsConfig,
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            mode: SystemPromptMode::Default,
            text: None,
            sections: SystemPromptSectionsConfig::default(),
        }
    }
}

/// Base system prompt mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum SystemPromptMode {
    #[default]
    Default,
    Replace,
}

/// Toggleable system prompt sections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[allow(clippy::struct_excessive_bools)]
#[config_doc(section = "sections")]
pub struct SystemPromptSectionsConfig {
    /// Include static repository context.
    #[serde(default = "default_true")]
    pub repository_context: bool,
    /// Include dynamic repository context.
    #[serde(default = "default_true")]
    pub dynamic_repository_context: bool,
    /// Include agent-specific suffix text.
    #[serde(default = "default_true")]
    pub agent_suffix: bool,
    /// Include the skill catalog.
    #[serde(default = "default_true")]
    pub skill_catalog: bool,
}

impl Default for SystemPromptSectionsConfig {
    fn default() -> Self {
        Self {
            repository_context: true,
            dynamic_repository_context: true,
            agent_suffix: true,
            skill_catalog: true,
        }
    }
}

/// Skill discovery and activation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[allow(clippy::struct_excessive_bools)]
#[config_doc(section = "skills")]
pub struct SkillsConfig {
    /// Whether skill discovery and activation are enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Skill auto-activation behavior.
    #[serde(default)]
    pub auto_activate: SkillAutoActivateMode,
    /// Whether repository-local skills are discovered.
    #[serde(default = "default_true")]
    pub include_repo_skills: bool,
    /// Whether generic repository skills are discovered.
    #[serde(default = "default_true")]
    pub include_generic_repo_skills: bool,
    /// Whether skills from user configuration roots are discovered.
    #[serde(default = "default_true")]
    pub include_user_skills: bool,
    /// Whether Claude-compatible skill layouts are discovered.
    #[serde(default = "default_true")]
    pub include_compat_claude_skills: bool,
    /// Maximum bytes of skill context that may be included in prompts.
    #[serde(default = "default_skill_context_bytes")]
    pub max_context_bytes: usize,
    /// Maximum bytes read from a single skill definition file.
    #[serde(default = "default_skill_file_bytes")]
    pub max_skill_file_bytes: u64,
    /// Maximum bytes read from a single skill resource file.
    #[serde(default = "default_skill_resource_file_bytes")]
    pub max_resource_file_bytes: u64,
    /// Whether symlinks are followed while discovering skill files.
    #[serde(default)]
    pub follow_symlinks: bool,
    /// Additional skill source paths.
    #[config_doc(nested)]
    #[serde(default)]
    pub sources: SkillSourceConfig,
    /// Disabled skill IDs.
    #[config_doc(nested)]
    #[serde(default)]
    pub disabled: DisabledSkillsConfig,
    /// Skill prompt catalog configuration.
    #[config_doc(nested)]
    #[serde(default)]
    pub prompt: SkillPromptConfig,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_activate: SkillAutoActivateMode::Suggest,
            include_repo_skills: true,
            include_generic_repo_skills: true,
            include_user_skills: true,
            include_compat_claude_skills: true,
            max_context_bytes: default_skill_context_bytes(),
            max_skill_file_bytes: default_skill_file_bytes(),
            max_resource_file_bytes: default_skill_resource_file_bytes(),
            follow_symlinks: true,
            sources: SkillSourceConfig::default(),
            disabled: DisabledSkillsConfig::default(),
            prompt: SkillPromptConfig::default(),
        }
    }
}

impl SkillsConfig {
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum SkillAutoActivateMode {
    Off,
    #[default]
    Suggest,
    On,
}

/// Skill prompt catalog configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "prompt")]
pub struct SkillPromptConfig {
    /// Catalog rendering mode.
    #[serde(default)]
    pub catalog: SkillPromptCatalogMode,
    /// Maximum skill catalog bytes included in prompts.
    #[serde(default = "default_skill_prompt_catalog_bytes")]
    pub max_bytes: usize,
    /// Maximum skill description characters included in prompts.
    #[serde(default = "default_skill_prompt_description_chars")]
    pub max_description_chars: usize,
    /// Whether skill source paths are included in the prompt catalog.
    #[serde(default = "default_true")]
    pub include_sources: bool,
    /// Whether skill keywords are included in the prompt catalog.
    #[serde(default)]
    pub include_keywords: bool,
}

impl Default for SkillPromptConfig {
    fn default() -> Self {
        Self {
            catalog: SkillPromptCatalogMode::Summary,
            max_bytes: default_skill_prompt_catalog_bytes(),
            max_description_chars: default_skill_prompt_description_chars(),
            include_sources: true,
            include_keywords: false,
        }
    }
}

/// Skill prompt catalog rendering mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum SkillPromptCatalogMode {
    Off,
    NamesOnly,
    #[default]
    Summary,
}

const fn default_skill_prompt_catalog_bytes() -> usize {
    8 * 1024
}

const fn default_skill_prompt_description_chars() -> usize {
    240
}

/// Additional skill source paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "sources")]
pub struct SkillSourceConfig {
    /// Additional filesystem roots scanned for skills.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

/// Disabled skill IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "disabled")]
pub struct DisabledSkillsConfig {
    /// Skill IDs disabled by configuration.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "observability")]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
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

/// Session import configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "session_import")]
pub struct SessionImportConfig {
    /// Whether session import plugins are enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whether import sources are discovered when the server starts.
    #[serde(default = "default_true")]
    pub auto_discover_on_startup: bool,
    /// Whether already-imported external sessions are hidden from import candidates.
    #[serde(default = "default_true")]
    pub hide_already_imported: bool,
    /// PI session import configuration.
    #[config_doc(nested)]
    #[serde(default)]
    pub pi: PiSessionImportConfig,
    /// `OpenCode` session import configuration.
    #[config_doc(nested)]
    #[serde(default)]
    pub opencode: OpenCodeSessionImportConfig,
}

impl Default for SessionImportConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_discover_on_startup: true,
            hide_already_imported: true,
            pi: PiSessionImportConfig::default(),
            opencode: OpenCodeSessionImportConfig::default(),
        }
    }
}

/// Daemon lifecycle configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "daemon")]
pub struct DaemonConfig {
    /// Shut down background daemon processes after they have been idle.
    #[serde(default = "default_true")]
    pub idle_shutdown: bool,
    /// Idle grace period in seconds before a background daemon exits.
    #[serde(default = "default_daemon_idle_shutdown_after_secs")]
    pub idle_shutdown_after_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_shutdown: true,
            idle_shutdown_after_secs: default_daemon_idle_shutdown_after_secs(),
        }
    }
}

const fn default_daemon_idle_shutdown_after_secs() -> u64 {
    15 * 60
}

/// Pi session import configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "pi")]
pub struct PiSessionImportConfig {
    /// Whether PI session import is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Additional PI session roots to scan.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Path selection mode for PI session import.
    #[serde(default)]
    pub path_mode: SessionImportPathMode,
}

impl Default for PiSessionImportConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: Vec::new(),
            path_mode: SessionImportPathMode::DefaultsAndCustom,
        }
    }
}

/// `OpenCode` session import configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "opencode")]
pub struct OpenCodeSessionImportConfig {
    /// Whether `OpenCode` session import is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Additional `OpenCode` session roots to scan.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Path selection mode for `OpenCode` session import.
    #[serde(default)]
    pub path_mode: SessionImportPathMode,
}

impl Default for OpenCodeSessionImportConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: Vec::new(),
            path_mode: SessionImportPathMode::DefaultsAndCustom,
        }
    }
}

/// Path selection mode for a session import source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum SessionImportPathMode {
    DefaultsOnly,
    CustomOnly,
    #[default]
    DefaultsAndCustom,
}

/// Worktree configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "worktree")]
pub struct WorktreeConfig {
    /// Root directory for Bcode-created worktrees. Relative paths resolve from the repository's
    /// main worktree root.
    #[serde(default = "default_worktree_root")]
    pub root: PathBuf,
    /// Prefix used when deriving new branch names.
    #[serde(default = "default_worktree_branch_prefix")]
    pub branch_prefix: String,
    /// Default base ref strategy for new worktrees.
    #[serde(default)]
    pub base_ref: WorktreeBaseRefConfig,
    /// Automatic worktree setup configuration.
    #[config_doc(nested)]
    #[serde(default)]
    pub setup: WorktreeSetupConfig,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            root: default_worktree_root(),
            branch_prefix: default_worktree_branch_prefix(),
            base_ref: WorktreeBaseRefConfig::default(),
            setup: WorktreeSetupConfig::default(),
        }
    }
}

/// Configured strategy for choosing the base ref for newly-created worktrees.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeBaseRefConfig {
    /// Use context-sensitive defaults.
    #[default]
    Auto,
    /// Use the repository default branch when possible.
    DefaultBranch,
    /// Use the current checkout's `HEAD`.
    Head,
}

/// Automatic worktree setup configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "setup")]
pub struct WorktreeSetupConfig {
    /// Whether setup should run automatically after creating a worktree.
    #[serde(default = "default_worktree_setup_enabled")]
    pub enabled: bool,
    /// Setup profile name for future profile-aware setup flows.
    #[serde(default)]
    pub profile: Option<String>,
}

impl Default for WorktreeSetupConfig {
    fn default() -> Self {
        Self {
            enabled: default_worktree_setup_enabled(),
            profile: None,
        }
    }
}

fn default_worktree_root() -> PathBuf {
    PathBuf::from(".bcode").join("worktrees")
}

fn default_worktree_branch_prefix() -> String {
    "bcode/".to_string()
}

const fn default_worktree_setup_enabled() -> bool {
    true
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
    /// Provider-exposed reasoning / thinking display configuration.
    #[serde(default)]
    pub thinking: TuiThinkingConfig,
    /// Theme rendering configuration.
    #[serde(default)]
    pub theme: TuiThemeConfig,
    /// Inline diff preview rendering configuration.
    #[serde(default)]
    pub inline_diff: TuiInlineDiffConfig,
}

/// Duration/easing curve for terminal UI accent color transitions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum TuiAccentTransitionCurve {
    /// Constant-speed transition.
    Linear,
    /// Cubic slow-start transition.
    EaseIn,
    /// Cubic fast-start transition.
    #[default]
    EaseOut,
    /// Cubic slow-start and slow-end transition.
    EaseInOut,
}

/// Terminal UI theme rendering configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "theme")]
pub struct TuiThemeConfig {
    /// How accent color changes should be applied.
    #[serde(default)]
    pub accent_transition: TuiAccentTransitionMode,
    /// Duration of accent color transitions in milliseconds.
    #[serde(default = "default_tui_accent_transition_ms")]
    pub accent_transition_ms: u64,
    /// Easing curve used for accent color transitions.
    #[serde(default)]
    pub accent_transition_curve: TuiAccentTransitionCurve,
}

impl TuiThemeConfig {
    /// Return the effective accent transition duration in milliseconds.
    #[must_use]
    pub const fn effective_accent_transition_ms(self) -> u64 {
        if matches!(self.accent_transition, TuiAccentTransitionMode::Immediate) {
            0
        } else {
            self.accent_transition_ms
        }
    }
}

impl Default for TuiThemeConfig {
    fn default() -> Self {
        Self {
            accent_transition: TuiAccentTransitionMode::Transition,
            accent_transition_ms: default_tui_accent_transition_ms(),
            accent_transition_curve: TuiAccentTransitionCurve::EaseOut,
        }
    }
}

const fn default_tui_accent_transition_ms() -> u64 {
    220
}

/// Terminal UI accent color transition behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum TuiAccentTransitionMode {
    /// Apply accent color changes immediately.
    Immediate,
    /// Animate accent color changes over the configured duration.
    #[default]
    Transition,
}

/// Terminal UI inline diff preview rendering configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "inline_diff")]
pub struct TuiInlineDiffConfig {
    /// Maximum inline diff card width in terminal columns.
    ///
    /// When unset, inline diff cards use the available transcript width.
    #[serde(default)]
    pub max_width: Option<usize>,
}

/// Terminal UI mouse interaction configuration.
/// Terminal UI configuration for provider-exposed reasoning / thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "thinking")]
pub struct TuiThinkingConfig {
    /// Whether provider-exposed reasoning should be shown in the TUI.
    #[serde(default)]
    pub show: bool,
    /// How provider-exposed reasoning should be rendered.
    #[serde(default)]
    pub mode: TuiThinkingMode,
}

impl Default for TuiThinkingConfig {
    fn default() -> Self {
        Self {
            show: true,
            mode: TuiThinkingMode::Summary,
        }
    }
}

/// Tool execution configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "tools")]
pub struct ToolsConfig {
    /// Shell tool configuration.
    #[config_doc(nested)]
    #[serde(default)]
    pub shell: ShellToolConfig,
}

/// Shell tool configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "shell")]
pub struct ShellToolConfig {
    /// Environment resolution configuration for shell commands.
    #[config_doc(nested)]
    #[serde(default)]
    pub env: ShellToolEnvConfig,
    /// Maximum bytes retained per stdout/stderr stream from non-terminal shell commands.
    #[serde(default = "default_shell_max_output_bytes")]
    pub max_output_bytes: usize,
    /// Maximum bytes included inline per stdout/stderr stream in model-visible shell results.
    #[serde(default = "default_shell_inline_output_bytes")]
    pub inline_output_bytes: usize,
}

impl Default for ShellToolConfig {
    fn default() -> Self {
        Self {
            env: ShellToolEnvConfig::default(),
            max_output_bytes: default_shell_max_output_bytes(),
            inline_output_bytes: default_shell_inline_output_bytes(),
        }
    }
}

const fn default_shell_max_output_bytes() -> usize {
    10 * 1024 * 1024
}

const fn default_shell_inline_output_bytes() -> usize {
    16 * 1024
}

/// Shell tool environment resolution configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "env")]
pub struct ShellToolEnvConfig {
    /// Environment resolver mode.
    #[serde(default)]
    pub mode: ShellToolEnvMode,
    /// Fallback behavior when `auto` detects an environment manager but cannot apply it.
    #[serde(default)]
    pub auto_fallback: ShellToolEnvAutoFallback,
}

impl Default for ShellToolEnvConfig {
    fn default() -> Self {
        Self {
            mode: ShellToolEnvMode::Auto,
            auto_fallback: ShellToolEnvAutoFallback::Error,
        }
    }
}

/// Shell tool environment resolver mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum ShellToolEnvMode {
    /// Automatically detect project environment managers.
    #[default]
    Auto,
    /// Inherit the daemon process environment.
    Inherit,
    /// Use direnv when running shell commands.
    Direnv,
}

/// Shell tool auto environment fallback behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum ShellToolEnvAutoFallback {
    /// Return an actionable error when auto-detected environment setup cannot run.
    #[default]
    Error,
    /// Fall back to the daemon process environment.
    Inherit,
}

/// Terminal UI thinking display mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum TuiThinkingMode {
    /// Show provider reasoning summaries when available.
    #[default]
    Summary,
    /// Show raw provider reasoning when available.
    Raw,
}

/// Terminal UI mouse interaction configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "mouse")]
pub struct TuiMouseConfig {
    /// Terminal rows to scroll for each terminal mouse-wheel event.
    #[serde(default = "default_tui_mouse_scroll_rows")]
    pub scroll_rows: usize,
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

impl TuiMouseConfig {
    /// Return the effective terminal rows to scroll for each wheel event.
    #[must_use]
    pub const fn effective_scroll_rows(self) -> usize {
        if self.scroll_rows == 0 {
            1
        } else {
            self.scroll_rows
        }
    }
}

impl Default for TuiMouseConfig {
    fn default() -> Self {
        Self {
            scroll_rows: default_tui_mouse_scroll_rows(),
            multi_click_ms: default_mouse_multi_click_ms(),
            multi_click_max_distance: 0,
            double_click_select: TuiMouseClickSelection::Word,
            triple_click_select: default_triple_click_select(),
        }
    }
}

const fn default_tui_mouse_scroll_rows() -> usize {
    3
}

const fn default_mouse_multi_click_ms() -> u64 {
    500
}

const fn default_triple_click_select() -> TuiMouseClickSelection {
    TuiMouseClickSelection::All
}

/// Selection behavior for a mouse click count.
/// Terminal UI click selection behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
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
    #[serde(default)]
    pub pools: BTreeMap<String, AuthPoolConfig>,
}

/// Ordered provider auth profiles that can satisfy the same model/provider request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPoolConfig {
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    #[serde(default)]
    pub strategy: AuthPoolStrategy,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub quota: AuthPoolQuotaConfig,
}

/// Auth pool selection strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPoolStrategy {
    /// Use the first healthy profile and fail over when provider-owned quota detection requires it.
    #[default]
    Failover,
}

/// Provider-specific quota/cooldown policy hints for an auth pool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPoolQuotaConfig {
    #[serde(default)]
    pub unknown_cooldown: Option<String>,
    #[serde(default)]
    pub rate_limit_cooldown: Option<String>,
    #[serde(default)]
    pub weekly_cooldown: Option<String>,
}

/// Generic authentication profile configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthProfileConfig {
    pub backend: String,
    /// Optional provider/plugin auth scheme, for example `api_key` or `chatgpt`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Canonical credential-to-source mappings.
    ///
    /// Example: `map.api_key.env = "OPENROUTER_API_KEY"` reads/stores the canonical
    /// `api_key` credential from `OPENROUTER_API_KEY` in the selected auth backend.
    #[serde(default)]
    pub map: BTreeMap<String, AuthCredentialMapping>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
}

/// Mapping from a canonical auth credential name to a backend/environment key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCredentialMapping {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
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
    pub reasoning: ReasoningConfig,
    #[serde(default)]
    pub max_tool_rounds: Option<u32>,
    #[serde(default)]
    pub context_strategy: ContextStrategyConfig,
    #[serde(default)]
    pub prompt_cache: PromptCacheConfig,
    #[serde(default)]
    pub conversation_reuse: ConversationReuseConfig,
    #[serde(default)]
    pub tool_output: ToolOutputConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub retry: ModelRetryConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ModelProfileConfig>,
    #[serde(default)]
    pub aliases: BTreeMap<String, ModelAliasConfig>,
    #[serde(default)]
    pub metadata: BTreeMap<String, ModelMetadataConfig>,
    #[serde(default)]
    pub ignored: BTreeMap<String, ModelIgnoreConfig>,
}

/// Declarative or state-backed model ignore rules for one provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIgnoreConfig {
    #[serde(default)]
    pub models: BTreeSet<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// Source that caused a model to be ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelIgnoreSource {
    Config,
    State,
    Both,
}

/// Matched model ignore rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelIgnoreMatch {
    pub source: ModelIgnoreSource,
    pub rule: String,
}

/// Effective model ignore rules after unioning declarative config and runtime state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveModelIgnoreRules {
    pub config: ModelIgnoreConfig,
    pub state: ModelIgnoreConfig,
}

impl EffectiveModelIgnoreRules {
    #[must_use]
    pub fn is_ignored(&self, model_id: &str) -> Option<ModelIgnoreMatch> {
        let config_rule = ignore_rule_match(&self.config, model_id);
        let state_rule = ignore_rule_match(&self.state, model_id);
        match (config_rule, state_rule) {
            (Some(config), Some(state)) => Some(ModelIgnoreMatch {
                source: ModelIgnoreSource::Both,
                rule: format!("{config}; {state}"),
            }),
            (Some(rule), None) => Some(ModelIgnoreMatch {
                source: ModelIgnoreSource::Config,
                rule,
            }),
            (None, Some(rule)) => Some(ModelIgnoreMatch {
                source: ModelIgnoreSource::State,
                rule,
            }),
            (None, None) => None,
        }
    }
}

fn ignore_rule_match(rules: &ModelIgnoreConfig, model_id: &str) -> Option<String> {
    if rules.models.contains(model_id) {
        return Some(model_id.to_string());
    }
    rules
        .patterns
        .iter()
        .find(|pattern| model_id.contains(pattern.as_str()))
        .map(|pattern| format!("*{pattern}*"))
}

impl ModelConfig {
    #[must_use]
    pub fn effective_max_tool_rounds(&self) -> Option<u32> {
        self.max_tool_rounds.filter(|rounds| *rounds > 0)
    }

    #[must_use]
    pub const fn effective_prompt_cache_mode(&self) -> bcode_model::PromptCacheMode {
        match self.context_strategy.mode {
            ContextStrategyMode::ProviderReuse => self.prompt_cache.mode,
            ContextStrategyMode::ExplicitCachedTranscript => {
                bcode_model::PromptCacheMode::Aggressive
            }
        }
    }

    #[must_use]
    pub const fn effective_conversation_reuse_mode(&self) -> bcode_model::ConversationReuseMode {
        match self.context_strategy.mode {
            ContextStrategyMode::ProviderReuse => self.conversation_reuse.mode,
            ContextStrategyMode::ExplicitCachedTranscript => {
                bcode_model::ConversationReuseMode::Off
            }
        }
    }
}

/// High-level model context management strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextStrategyConfig {
    /// Context strategy mode. Defaults to provider-native continuation where available.
    #[serde(default)]
    pub mode: ContextStrategyMode,
}

impl Default for ContextStrategyConfig {
    fn default() -> Self {
        Self {
            mode: ContextStrategyMode::ProviderReuse,
        }
    }
}

/// High-level context strategy mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategyMode {
    /// Prefer provider-native continuation/state reuse when supported.
    #[default]
    ProviderReuse,
    /// Resend explicit transcript context, using prompt cache hints aggressively.
    ExplicitCachedTranscript,
}

/// Reasoning / thinking request configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub effort_values: Vec<String>,
    #[serde(default)]
    pub summary_values: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub default_summary: Option<String>,
    #[serde(default)]
    pub visible_summary_supported: Option<bool>,
    #[serde(default)]
    pub raw_reasoning_supported: Option<bool>,
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
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            no_progress_warning_secs: default_streaming_no_progress_warning_secs(),
            no_progress_timeout_secs: default_streaming_no_progress_timeout_secs(),
        }
    }
}

/// Model provider retry configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRetryConfig {
    /// Enable automatic model-provider retries.
    #[serde(default = "default_model_retry_enabled")]
    pub enabled: bool,
    /// Enable built-in provider overload retries.
    #[serde(default = "default_overload_retry_enabled")]
    pub overload_enabled: bool,
    /// Maximum automatic retry attempts for provider overload errors.
    #[serde(default = "default_max_overload_retries")]
    pub max_overload_retries: u8,
    /// Initial overload retry delay in milliseconds.
    #[serde(default = "default_overload_initial_delay_ms")]
    pub overload_initial_delay_ms: u64,
    /// Maximum overload retry delay in milliseconds.
    #[serde(default = "default_overload_max_delay_ms")]
    pub overload_max_delay_ms: u64,
    /// Enable recoverable error patterns imported from the remote model catalog.
    #[serde(default = "default_remote_catalog_retry_rules_enabled")]
    pub remote_catalog_rules_enabled: bool,
    /// Custom provider-error retry rules.
    #[serde(default)]
    pub rules: Vec<ModelRetryRuleConfig>,
}

const fn default_remote_catalog_retry_rules_enabled() -> bool {
    true
}

pub type ModelRetryRuleConfig = bcode_model::ProviderRetryRule;
pub type ModelRetryRuleMatchConfig = bcode_model::ProviderRetryRuleMatch;

impl Default for ModelRetryConfig {
    fn default() -> Self {
        Self {
            enabled: default_model_retry_enabled(),
            overload_enabled: default_overload_retry_enabled(),
            max_overload_retries: default_max_overload_retries(),
            overload_initial_delay_ms: default_overload_initial_delay_ms(),
            overload_max_delay_ms: default_overload_max_delay_ms(),
            remote_catalog_rules_enabled: default_remote_catalog_retry_rules_enabled(),
            rules: Vec::new(),
        }
    }
}

const fn default_model_retry_enabled() -> bool {
    true
}

const fn default_overload_retry_enabled() -> bool {
    true
}

const fn default_max_overload_retries() -> u8 {
    5
}

const fn default_overload_initial_delay_ms() -> u64 {
    2_000
}

const fn default_overload_max_delay_ms() -> u64 {
    30_000
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
    pub auth_pool: Option<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    #[serde(default)]
    pub reasoning: ReasoningConfig,
    #[serde(default)]
    pub request: BTreeMap<String, serde_json::Value>,
}

/// Generic model alias resolved before provider requests are built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAliasConfig {
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    pub model_id: String,
    #[serde(default)]
    pub request: BTreeMap<String, serde_json::Value>,
}

/// User-provided model metadata override.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMetadataConfig {
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning: ReasoningConfig,
}

/// Resolved model selection after applying the active model profile, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedModelSelection {
    pub provider_plugin_id: Option<String>,
    pub model_id: Option<String>,
    pub selected_model_id: Option<String>,
    pub model_profile: Option<String>,
    pub auth_profile: Option<String>,
    pub auth_pool: Option<String>,
    pub settings: BTreeMap<String, String>,
    pub request: BTreeMap<String, bcode_model::ProviderRequestValue>,
    pub reasoning: ReasoningConfig,
}

/// Plugin configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginConfig {
    #[serde(default)]
    pub enabled: BTreeSet<String>,
    #[serde(default)]
    pub disabled: BTreeSet<String>,
    #[serde(default)]
    pub config: BTreeMap<String, toml::Value>,
}

fn merge_reasoning_config(base: &ReasoningConfig, overlay: &ReasoningConfig) -> ReasoningConfig {
    ReasoningConfig {
        effort: overlay.effort.clone().or_else(|| base.effort.clone()),
        summary: overlay.summary.clone().or_else(|| base.summary.clone()),
        effort_values: if overlay.effort_values.is_empty() {
            base.effort_values.clone()
        } else {
            overlay.effort_values.clone()
        },
        summary_values: if overlay.summary_values.is_empty() {
            base.summary_values.clone()
        } else {
            overlay.summary_values.clone()
        },
        default_effort: overlay
            .default_effort
            .clone()
            .or_else(|| base.default_effort.clone()),
        default_summary: overlay
            .default_summary
            .clone()
            .or_else(|| base.default_summary.clone()),
        visible_summary_supported: overlay
            .visible_summary_supported
            .or(base.visible_summary_supported),
        raw_reasoning_supported: overlay
            .raw_reasoning_supported
            .or(base.raw_reasoning_supported),
    }
}

fn reasoning_capabilities_from_config(
    reasoning: &ReasoningConfig,
) -> Option<bcode_model::ModelReasoningInfo> {
    (!reasoning.effort_values.is_empty()
        || !reasoning.summary_values.is_empty()
        || reasoning.default_effort.is_some()
        || reasoning.default_summary.is_some()
        || reasoning.visible_summary_supported.is_some()
        || reasoning.raw_reasoning_supported.is_some())
    .then(|| bcode_model::ModelReasoningInfo {
        effort_values: reasoning.effort_values.clone(),
        default_effort: reasoning.default_effort.clone(),
        visible_summary_supported: reasoning.visible_summary_supported.unwrap_or_default(),
        summary_values: reasoning.summary_values.clone(),
        default_summary: reasoning.default_summary.clone(),
        raw_reasoning_supported: reasoning.raw_reasoning_supported.unwrap_or_default(),
        source: bcode_model::ModelReasoningCapabilitySource::ConfigOverride,
    })
}

fn insert_model_reasoning_settings(
    settings: &mut BTreeMap<String, String>,
    model_id: &str,
    reasoning: &bcode_model::ModelReasoningInfo,
) {
    if !reasoning.effort_values.is_empty() {
        settings.insert(
            format!("model_metadata.{model_id}.reasoning.effort_values"),
            reasoning.effort_values.join(","),
        );
    }
    if let Some(default_effort) = &reasoning.default_effort {
        settings.insert(
            format!("model_metadata.{model_id}.reasoning.default_effort"),
            default_effort.clone(),
        );
    }
    settings.insert(
        format!("model_metadata.{model_id}.reasoning.visible_summary_supported"),
        reasoning.visible_summary_supported.to_string(),
    );
    if !reasoning.summary_values.is_empty() {
        settings.insert(
            format!("model_metadata.{model_id}.reasoning.summary_values"),
            reasoning.summary_values.join(","),
        );
    }
    if let Some(default_summary) = &reasoning.default_summary {
        settings.insert(
            format!("model_metadata.{model_id}.reasoning.default_summary"),
            default_summary.clone(),
        );
    }
    settings.insert(
        format!("model_metadata.{model_id}.reasoning.raw_reasoning_supported"),
        reasoning.raw_reasoning_supported.to_string(),
    );
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
        plugin_selection_with_default_plugin_ids(value, std::iter::empty::<&str>())
    }
}

/// Resolve plugin selection using caller-provided distribution/bundle default plugin IDs.
#[must_use]
pub fn plugin_selection_with_default_plugin_ids<I, S>(
    value: &BcodeConfig,
    default_plugin_ids: I,
) -> PluginSelection
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut selection = PluginSelection::from(&value.plugins);
    let had_explicit_enabled_plugins = !selection.enabled.is_empty();
    let env_provider = provider_plugin_id_from_environment();
    let resolved_provider = value.resolved_model_selection().provider_plugin_id;
    let provider = env_provider
        .clone()
        .or_else(|| resolved_provider.clone())
        .unwrap_or_else(|| DEFAULT_MODEL_PROVIDER_PLUGIN_ID.to_string());

    for plugin_id in default_plugin_ids {
        enable_plugin_unless_disabled(&mut selection, plugin_id.as_ref());
    }
    enable_default_model_provider_plugins(&mut selection);
    if !had_explicit_enabled_plugins {
        enable_plugin_unless_disabled(&mut selection, &provider);
    } else if let Some(env_provider) = env_provider {
        enable_plugin_unless_disabled(&mut selection, &env_provider);
    } else if let Some(resolved_provider) = resolved_provider {
        enable_plugin_unless_disabled(&mut selection, &resolved_provider);
    }
    selection
}

fn enable_default_model_provider_plugins(selection: &mut PluginSelection) {
    for plugin_id in DEFAULT_MODEL_PROVIDER_PLUGIN_IDS {
        enable_plugin_unless_disabled(selection, plugin_id);
    }
}

fn enable_plugin_unless_disabled(selection: &mut PluginSelection, plugin_id: &str) {
    if !selection.disabled.contains(plugin_id) {
        selection.enabled.insert(plugin_id.to_string());
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
    #[error("composition error: {message}")]
    Composition { message: String },
    #[error("unknown permission category: {0}")]
    UnknownPermissionCategory(String),
    #[error("unknown permission action: {0}")]
    UnknownPermissionAction(String),
    #[error(
        "removed shorthand tool ID at agent.{agent_id}.tools.{tool_id}; use exact tool ID {replacement} instead"
    )]
    RemovedShorthandToolId {
        agent_id: String,
        tool_id: String,
        replacement: &'static str,
    },
}

fn validate_config(config: &BcodeConfig) -> Result<(), ConfigError> {
    for (agent_id, agent) in &config.agent {
        for tool_id in agent.tools.keys() {
            if let Some(replacement) = removed_shorthand_tool_replacement(tool_id) {
                return Err(ConfigError::RemovedShorthandToolId {
                    agent_id: agent_id.clone(),
                    tool_id: tool_id.clone(),
                    replacement,
                });
            }
        }
    }
    Ok(())
}

fn removed_shorthand_tool_replacement(tool_id: &str) -> Option<&'static str> {
    match tool_id {
        "bash" => Some("shell.run"),
        "read" => Some("filesystem.read"),
        "grep" => Some("filesystem.grep"),
        "find" => Some("filesystem.find"),
        "ls" => Some("filesystem.list"),
        "stat" => Some("filesystem.stat"),
        "write" => Some("filesystem.write"),
        "edit" => Some("filesystem.edit"),
        "worktree.read" => Some("worktree.list"),
        _ => None,
    }
}

fn validate_config_value(value: toml::Value, context: &str) -> Result<BcodeConfig, ConfigError> {
    let config = value
        .try_into()
        .map_err(|source| ConfigError::Composition {
            message: format!("failed to deserialize {context}: {source}"),
        })?;
    validate_config(&config)?;
    Ok(config)
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
/// `category` must be one of `bash`, `read`, `write`, `edit`, or `web`.
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
    set_openai_compatible_sshenv_auth_mode("openai", profile, vault, model_id, mode, None)
}

/// Configure an OpenAI-compatible provider auth profile backed by an `sshenv` vault.
///
/// # Errors
///
/// Returns an error when the config cannot be read, updated, or written.
pub fn set_openai_compatible_sshenv_auth_mode(
    provider: &str,
    profile: String,
    vault: PathBuf,
    model_id: Option<String>,
    mode: AuthMode,
    base_url: Option<&str>,
) -> Result<PathBuf, ConfigError> {
    update_writable_config(|config| {
        let vault_setting = vault.display().to_string();
        let mode_setting = auth_mode_setting(&mode);
        config
            .plugins
            .enabled
            .insert("bcode.openai-compatible".to_string());
        config.model.provider_plugin_id = Some("bcode.openai-compatible".to_string());
        // xAI and other OpenAI-compatibles reuse the same plugin ID + service
        if let Some(model_id) = model_id {
            config.model.model_id = Some(model_id);
        }
        let auth_map = openai_compatible_auth_map(provider, &mode);
        config.auth.openai = Some(AuthProviderConfig {
            backend: "sshenv".to_string(),
            mode,
            profile: profile.clone(),
            vault: Some(vault),
        });
        let mut settings = BTreeMap::new();
        settings.insert("provider".to_string(), provider.to_string());
        settings.insert("profile".to_string(), profile.clone());
        settings.insert("vault".to_string(), vault_setting);
        settings.insert("mode".to_string(), mode_setting.to_string());
        if let Some(base_url) = base_url {
            settings.insert("base_url".to_string(), base_url.to_string());
        }
        config.auth.profiles.insert(
            profile.clone(),
            AuthProfileConfig {
                backend: "sshenv".to_string(),
                scheme: Some(mode_setting.to_string()),
                map: auth_map,
                settings,
            },
        );
        if let Some(model_id) = config.model.model_id.clone() {
            config
                .model
                .profiles
                .entry(profile.clone())
                .or_insert_with(|| ModelProfileConfig {
                    provider_plugin_id: "bcode.openai-compatible".to_string(),
                    model_id: Some(model_id),
                    auth_profile: Some(profile),
                    auth_pool: None,
                    settings: BTreeMap::new(),
                    reasoning: ReasoningConfig::default(),
                    request: BTreeMap::new(),
                });
        }
        Ok(())
    })
}

/// Configure an `OpenAI` `ChatGPT` subscription auth profile and add it to a failover auth pool.
///
/// # Errors
///
/// Returns an error when the config cannot be read, updated, or written.
pub fn add_openai_chatgpt_subscription_auth(
    pool: &str,
    profile: &str,
    vault: &Path,
    model_id: Option<String>,
) -> Result<PathBuf, ConfigError> {
    update_writable_config(|config| {
        let vault_setting = vault.display().to_string();
        config
            .plugins
            .enabled
            .insert("bcode.openai-compatible".to_string());
        config.model.provider_plugin_id = Some("bcode.openai-compatible".to_string());
        if let Some(model_id) = model_id {
            config.model.model_id = Some(model_id);
        }
        let mut settings = BTreeMap::new();
        settings.insert("provider".to_string(), "openai".to_string());
        settings.insert("profile".to_string(), profile.to_string());
        settings.insert("vault".to_string(), vault_setting);
        settings.insert("mode".to_string(), "chatgpt".to_string());
        config.auth.profiles.insert(
            profile.to_string(),
            AuthProfileConfig {
                backend: "sshenv".to_string(),
                scheme: Some("chatgpt".to_string()),
                map: openai_compatible_auth_map("openai", &AuthMode::ChatGpt),
                settings,
            },
        );
        let auth_pool = config
            .auth
            .pools
            .entry(pool.to_string())
            .or_insert_with(|| AuthPoolConfig {
                provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                strategy: AuthPoolStrategy::Failover,
                profiles: Vec::new(),
                quota: AuthPoolQuotaConfig::default(),
            });
        auth_pool.provider_plugin_id = Some("bcode.openai-compatible".to_string());
        auth_pool.strategy = AuthPoolStrategy::Failover;
        if !auth_pool
            .profiles
            .iter()
            .any(|candidate| candidate == profile)
        {
            auth_pool.profiles.push(profile.to_string());
        }
        if let Some(model_id) = config.model.model_id.clone() {
            config
                .model
                .profiles
                .entry(pool.to_string())
                .and_modify(|model_profile| {
                    model_profile.auth_profile = None;
                    model_profile.auth_pool = Some(pool.to_string());
                })
                .or_insert_with(|| ModelProfileConfig {
                    provider_plugin_id: "bcode.openai-compatible".to_string(),
                    model_id: Some(model_id),
                    auth_profile: None,
                    auth_pool: Some(pool.to_string()),
                    settings: BTreeMap::new(),
                    reasoning: ReasoningConfig::default(),
                    request: BTreeMap::new(),
                });
        }
        Ok(())
    })
}

fn openai_compatible_auth_map(
    provider: &str,
    mode: &AuthMode,
) -> BTreeMap<String, AuthCredentialMapping> {
    match mode {
        AuthMode::ApiKey => BTreeMap::from([(
            "api_key".to_string(),
            AuthCredentialMapping {
                env: Some(match provider {
                    "xai" | "grok" => "BCODE_XAI_API_KEY".to_string(),
                    _ => "BCODE_OPENAI_API_KEY".to_string(),
                }),
                key: None,
            },
        )]),
        AuthMode::ChatGpt => BTreeMap::from([
            (
                "access_token".to_string(),
                AuthCredentialMapping {
                    env: Some("BCODE_OPENAI_CODEX_ACCESS_TOKEN".to_string()),
                    key: None,
                },
            ),
            (
                "refresh_token".to_string(),
                AuthCredentialMapping {
                    env: Some("BCODE_OPENAI_CODEX_REFRESH_TOKEN".to_string()),
                    key: None,
                },
            ),
            (
                "id_token".to_string(),
                AuthCredentialMapping {
                    env: Some("BCODE_OPENAI_CODEX_ID_TOKEN".to_string()),
                    key: None,
                },
            ),
            (
                "expires_at".to_string(),
                AuthCredentialMapping {
                    env: Some("BCODE_OPENAI_CODEX_EXPIRES_AT".to_string()),
                    key: None,
                },
            ),
            (
                "account_id".to_string(),
                AuthCredentialMapping {
                    env: Some("BCODE_OPENAI_CODEX_ACCOUNT_ID".to_string()),
                    key: None,
                },
            ),
        ]),
    }
}

const fn auth_mode_setting(mode: &AuthMode) -> &'static str {
    match mode {
        AuthMode::ApiKey => "api_key",
        AuthMode::ChatGpt => "chatgpt",
    }
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
                auth_pool: None,
                settings,
                reasoning: ReasoningConfig::default(),
                request: BTreeMap::new(),
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
                scheme: Some("aws_default_chain".to_string()),
                settings: auth_settings,
                ..AuthProfileConfig::default()
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

/// Non-secret runtime subscription registry for provider logins that should not mutate declarative config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAuthSubscriptions {
    #[serde(default)]
    pub pools: BTreeMap<String, RuntimeAuthSubscriptionPool>,
}

/// Runtime subscriptions associated with one logical auth pool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAuthSubscriptionPool {
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    #[serde(default)]
    pub profiles: Vec<RuntimeAuthSubscriptionProfile>,
}

/// Runtime subscription profile metadata. Secret values remain in the auth vault.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAuthSubscriptionProfile {
    pub auth_profile: String,
    pub storage_profile: String,
    pub vault: PathBuf,
    pub provider: String,
    pub scheme: String,
}

/// Return the runtime auth subscription registry path.
#[must_use]
pub fn runtime_auth_subscriptions_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_AUTH_SUBSCRIPTIONS") {
        return PathBuf::from(path);
    }
    default_state_dir().join("auth").join("subscriptions.json")
}

/// Load runtime auth subscriptions from user state.
#[must_use]
pub fn load_runtime_auth_subscriptions() -> RuntimeAuthSubscriptions {
    let path = runtime_auth_subscriptions_path();
    let Ok(contents) = fs::read_to_string(path) else {
        return RuntimeAuthSubscriptions::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Register a runtime auth subscription without mutating declarative config.
///
/// # Errors
///
/// Returns an error when the registry cannot be written.
pub fn register_runtime_auth_subscription(
    pool: &str,
    profile: RuntimeAuthSubscriptionProfile,
) -> Result<PathBuf, ConfigError> {
    let path = runtime_auth_subscriptions_path();
    let mut registry = load_runtime_auth_subscriptions();
    let pool_entry =
        registry
            .pools
            .entry(pool.to_string())
            .or_insert_with(|| RuntimeAuthSubscriptionPool {
                provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                profiles: Vec::new(),
            });
    pool_entry.provider_plugin_id = Some("bcode.openai-compatible".to_string());
    if let Some(existing) = pool_entry
        .profiles
        .iter_mut()
        .find(|existing| existing.auth_profile == profile.auth_profile)
    {
        *existing = profile;
    } else {
        pool_entry.profiles.push(profile);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let contents =
        serde_json::to_string_pretty(&registry).map_err(|source| ConfigError::Composition {
            message: format!("failed to serialize runtime auth subscriptions: {source}"),
        })?;
    fs::write(&path, contents).map_err(|source| ConfigError::Io {
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

/// Return the default runtime model ignores state file path.
#[must_use]
pub fn default_model_ignores_state_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_MODEL_IGNORES_STATE") {
        return PathBuf::from(path);
    }
    default_state_dir().join("model-ignores.toml")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ModelIgnoresState {
    #[serde(default)]
    providers: BTreeMap<String, ModelIgnoreConfig>,
}

/// Load runtime model ignore rules.
///
/// # Errors
///
/// Returns an error when the state file exists but cannot be read or parsed.
pub fn load_model_ignores_state() -> Result<BTreeMap<String, ModelIgnoreConfig>, ConfigError> {
    load_model_ignores_state_from(&default_model_ignores_state_path())
}

/// Load runtime model ignore rules from a specific path.
///
/// # Errors
///
/// Returns an error when the state file exists but cannot be read or parsed.
pub fn load_model_ignores_state_from(
    path: &Path,
) -> Result<BTreeMap<String, ModelIgnoreConfig>, ConfigError> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let state =
        toml::from_str::<ModelIgnoresState>(&raw).map_err(|source| ConfigError::Composition {
            message: format!("failed to parse {}: {source}", path.display()),
        })?;
    Ok(state.providers)
}

/// Return effective model ignore rules for a provider.
///
/// # Errors
///
/// Returns an error when config or state cannot be loaded.
pub fn effective_model_ignore_rules(
    provider_plugin_id: &str,
) -> Result<EffectiveModelIgnoreRules, ConfigError> {
    let config = load_config()?;
    let state = load_model_ignores_state()?;
    Ok(EffectiveModelIgnoreRules {
        config: config
            .model
            .ignored
            .get(provider_plugin_id)
            .cloned()
            .unwrap_or_default(),
        state: state.get(provider_plugin_id).cloned().unwrap_or_default(),
    })
}

/// Add a model to runtime ignores for a provider.
///
/// # Errors
///
/// Returns an error when state cannot be read or written.
pub fn ignore_model_in_state(
    provider_plugin_id: &str,
    model_id: String,
) -> Result<PathBuf, ConfigError> {
    update_model_ignores_state(|providers| {
        providers
            .entry(provider_plugin_id.to_string())
            .or_default()
            .models
            .insert(model_id);
        Ok(())
    })
}

/// Remove a model from runtime ignores for a provider.
///
/// # Errors
///
/// Returns an error when state cannot be read or written.
pub fn unignore_model_in_state(
    provider_plugin_id: &str,
    model_id: &str,
) -> Result<PathBuf, ConfigError> {
    update_model_ignores_state(|providers| {
        if let Some(rules) = providers.get_mut(provider_plugin_id) {
            rules.models.remove(model_id);
        }
        Ok(())
    })
}

fn update_model_ignores_state(
    update: impl FnOnce(&mut BTreeMap<String, ModelIgnoreConfig>) -> Result<(), ConfigError>,
) -> Result<PathBuf, ConfigError> {
    let path = default_model_ignores_state_path();
    let mut providers = if path.exists() {
        load_model_ignores_state_from(&path)?
    } else {
        BTreeMap::new()
    };
    update(&mut providers)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, model_ignores_state_to_toml(&providers)).map_err(|source| {
        ConfigError::Io {
            path: path.clone(),
            source,
        }
    })?;
    Ok(path)
}

fn model_ignores_state_to_toml(providers: &BTreeMap<String, ModelIgnoreConfig>) -> String {
    let mut output = String::new();
    output.push_str(
        "# Bcode runtime model ignore state. Managed automatically by the TUI and CLI.\n",
    );
    output.push_str(
        "# Declarative [model.ignored] rules in bcode.toml are unioned with this file.\n\n",
    );
    for (provider, rules) in providers {
        let escaped_provider = provider.replace('"', "\\\"");
        let _ = writeln!(output, "[providers.\"{escaped_provider}\"]");
        write_model_ignore_string_set(&mut output, "models", &rules.models);
        write_string_slice(&mut output, "patterns", &rules.patterns);
        output.push('\n');
    }
    output
}

fn write_model_ignore_string_set(output: &mut String, key: &str, values: &BTreeSet<String>) {
    let values = values.iter().cloned().collect::<Vec<_>>();
    write_string_slice(output, key, &values);
}

fn write_string_slice(output: &mut String, key: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let escaped = values
        .iter()
        .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(output, "{key} = [{escaped}]");
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

/// Load runtime permissions state as a raw TOML config value.
///
/// Missing state files are represented as `None`. Existing files keep their raw
/// shape so composition can recursively merge future agent config fields without
/// typed field-by-field merge logic.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_permissions_state_value() -> Result<Option<toml::Value>, ConfigError> {
    load_permissions_state_value_from(&default_permissions_state_path())
}

/// Load runtime permissions state from an explicit path as a raw TOML config value.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_permissions_state_value_from(path: &Path) -> Result<Option<toml::Value>, ConfigError> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(load_toml_file(path)?))
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
    let config: BcodeConfig =
        toml::from_str(&contents).map_err(|source| ConfigError::Composition {
            message: format!(
                "failed to parse permissions state {}: {source}",
                path.display()
            ),
        })?;
    Ok(config.agent)
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
        "web" => &mut permission.web,
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

/// Return the default Bcode config directory.
#[must_use]
pub fn default_config_dir() -> PathBuf {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join("bcode");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".config").join("bcode");
    }
    env::temp_dir().join("bcode")
}

fn writable_config_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_CONFIG") {
        return PathBuf::from(path);
    }
    default_config_dir().join(DEFAULT_CONFIG_FILE_NAME)
}

fn config_to_toml(config: &BcodeConfig) -> String {
    let mut output = String::new();
    write_plugins_toml(&mut output, &config.plugins);
    write_model_toml(&mut output, &config.model);
    write_agents_toml(&mut output, &config.agent);
    write_auth_toml(&mut output, &config.auth);
    write_observability_toml(&mut output, &config.observability);
    write_skills_toml(&mut output, &config.skills);
    write_system_prompt_toml(&mut output, &config.system_prompt);
    write_tui_toml(&mut output, &config.tui);
    write_domain_toml(&mut output, "web_search", &config.web_search);
    output
}

fn write_domain_toml(output: &mut String, section: &str, value: &toml::Value) {
    let Some(table) = value.as_table() else {
        return;
    };
    if table.is_empty() {
        return;
    }
    writeln!(output, "[{}]", toml_table_key(section)).expect("writing to string should not fail");
    for (key, value) in table {
        write_toml_value(output, key, value);
    }
    output.push('\n');
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

fn write_model_retry_toml(output: &mut String, retry: &ModelRetryConfig) {
    if retry == &ModelRetryConfig::default() {
        return;
    }
    output.push_str("[model.retry]\n");
    if retry.enabled != default_model_retry_enabled() {
        writeln!(output, "enabled = {}", retry.enabled).expect("writing to string should not fail");
    }
    if retry.overload_enabled != default_overload_retry_enabled() {
        writeln!(output, "overload_enabled = {}", retry.overload_enabled)
            .expect("writing to string should not fail");
    }
    writeln!(
        output,
        "max_overload_retries = {}",
        retry.max_overload_retries
    )
    .expect("writing to string should not fail");
    writeln!(
        output,
        "overload_initial_delay_ms = {}",
        retry.overload_initial_delay_ms
    )
    .expect("writing to string should not fail");
    writeln!(
        output,
        "overload_max_delay_ms = {}",
        retry.overload_max_delay_ms
    )
    .expect("writing to string should not fail");
    if retry.remote_catalog_rules_enabled != default_remote_catalog_retry_rules_enabled() {
        writeln!(
            output,
            "remote_catalog_rules_enabled = {}",
            retry.remote_catalog_rules_enabled
        )
        .expect("writing to string should not fail");
    }
    output.push('\n');
    for rule in &retry.rules {
        write_model_retry_rule_toml(output, rule);
    }
}

fn write_model_retry_rule_toml(output: &mut String, rule: &ModelRetryRuleConfig) {
    output.push_str("[[model.retry.rules]]\n");
    writeln!(output, "id = {}", toml_string(&rule.id)).expect("writing to string should not fail");
    if let Some(enabled) = rule.enabled {
        writeln!(output, "enabled = {enabled}").expect("writing to string should not fail");
    }
    write_optional_string(
        output,
        "provider_plugin_id",
        rule.provider_plugin_id.as_ref(),
    );
    write_optional_string(
        output,
        "provider_plugin_id_contains",
        rule.provider_plugin_id_contains.as_ref(),
    );
    write_optional_string(output, "model_id", rule.model_id.as_ref());
    write_optional_string(output, "model_id_contains", rule.model_id_contains.as_ref());
    if let Some(max_retries) = rule.max_retries {
        writeln!(output, "max_retries = {max_retries}").expect("writing to string should not fail");
    }
    if let Some(initial_delay_ms) = rule.initial_delay_ms {
        writeln!(output, "initial_delay_ms = {initial_delay_ms}")
            .expect("writing to string should not fail");
    }
    if let Some(max_delay_ms) = rule.max_delay_ms {
        writeln!(output, "max_delay_ms = {max_delay_ms}")
            .expect("writing to string should not fail");
    }
    if let Some(use_provider_retry_hint) = rule.use_provider_retry_hint {
        writeln!(
            output,
            "use_provider_retry_hint = {use_provider_retry_hint}"
        )
        .expect("writing to string should not fail");
    }
    output.push('\n');
    output.push_str("[model.retry.rules.match]\n");
    write_model_retry_rule_match_toml(output, &rule.r#match);
    output.push('\n');
}

fn write_model_retry_rule_match_toml(output: &mut String, matcher: &ModelRetryRuleMatchConfig) {
    if let Some(category) = matcher.category {
        writeln!(
            output,
            "category = {}",
            toml_string(provider_error_category_name(category))
        )
        .expect("writing to string should not fail");
    }
    write_optional_string(output, "code", matcher.code.as_ref());
    write_optional_string(output, "message_equals", matcher.message_equals.as_ref());
    write_optional_string(
        output,
        "message_contains",
        matcher.message_contains.as_ref(),
    );
    write_optional_string(
        output,
        "provider_message_equals",
        matcher.provider_message_equals.as_ref(),
    );
    write_optional_string(
        output,
        "provider_message_contains",
        matcher.provider_message_contains.as_ref(),
    );
}

const fn provider_error_category_name(
    category: bcode_model::ProviderErrorCategory,
) -> &'static str {
    match category {
        bcode_model::ProviderErrorCategory::Auth => "auth",
        bcode_model::ProviderErrorCategory::Config => "config",
        bcode_model::ProviderErrorCategory::InvalidRequest => "invalid_request",
        bcode_model::ProviderErrorCategory::ModelNotFound => "model_not_found",
        bcode_model::ProviderErrorCategory::ContextLength => "context_length",
        bcode_model::ProviderErrorCategory::Network => "network",
        bcode_model::ProviderErrorCategory::Timeout => "timeout",
        bcode_model::ProviderErrorCategory::RateLimit => "rate_limit",
        bcode_model::ProviderErrorCategory::UnsupportedFeature => "unsupported_feature",
        bcode_model::ProviderErrorCategory::ProviderInternal => "provider_internal",
        bcode_model::ProviderErrorCategory::Overloaded => "overloaded",
        bcode_model::ProviderErrorCategory::Cancelled => "cancelled",
    }
}

fn write_optional_string(output: &mut String, key: &str, value: Option<&String>) {
    if let Some(value) = value {
        writeln!(output, "{key} = {}", toml_string(value))
            .expect("writing to string should not fail");
    }
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
    output.push('\n');
}

fn write_model_toml(output: &mut String, model: &ModelConfig) {
    if model.provider_plugin_id.is_some()
        || model.model_id.is_some()
        || model.default_thinking_level.is_some()
        || model.reasoning != ReasoningConfig::default()
        || model.max_tool_rounds.is_some()
        || model.profile.is_some()
        || !model.aliases.is_empty()
        || !model.metadata.is_empty()
        || model.context_strategy != ContextStrategyConfig::default()
        || model.prompt_cache != PromptCacheConfig::default()
        || model.conversation_reuse != ConversationReuseConfig::default()
        || model.tool_output != ToolOutputConfig::default()
        || model.streaming != StreamingConfig::default()
        || model.retry != ModelRetryConfig::default()
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
        write_reasoning_inline_toml(output, &model.reasoning);
        if let Some(max_tool_rounds) = model.max_tool_rounds {
            writeln!(output, "max_tool_rounds = {max_tool_rounds}")
                .expect("writing to string should not fail");
        }
        output.push('\n');
    }
    if model.context_strategy != ContextStrategyConfig::default() {
        output.push_str("[model.context_strategy]\n");
        writeln!(
            output,
            "mode = {}",
            toml_string(context_strategy_mode_name(model.context_strategy.mode))
        )
        .expect("writing to string should not fail");
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
    write_model_retry_toml(output, &model.retry);
    if model.compaction != CompactionConfig::default() {
        write_model_compaction_toml(output, &model.compaction);
    }
    write_model_profiles_toml(output, &model.profiles);
    write_model_aliases_toml(output, &model.aliases);
    write_model_metadata_toml(output, &model.metadata);
}

fn write_reasoning_inline_toml(output: &mut String, reasoning: &ReasoningConfig) {
    if let Some(effort) = &reasoning.effort {
        writeln!(output, "reasoning_effort = {}", toml_string(effort))
            .expect("writing to string should not fail");
    }
    if let Some(summary) = &reasoning.summary {
        writeln!(output, "reasoning_summary = {}", toml_string(summary))
            .expect("writing to string should not fail");
    }
    if !reasoning.effort_values.is_empty() {
        write_string_array(output, "reasoning_effort_values", &reasoning.effort_values);
    }
    if !reasoning.summary_values.is_empty() {
        write_string_array(
            output,
            "reasoning_summary_values",
            &reasoning.summary_values,
        );
    }
    if let Some(default_effort) = &reasoning.default_effort {
        writeln!(
            output,
            "reasoning_default_effort = {}",
            toml_string(default_effort)
        )
        .expect("writing to string should not fail");
    }
    if let Some(default_summary) = &reasoning.default_summary {
        writeln!(
            output,
            "reasoning_default_summary = {}",
            toml_string(default_summary)
        )
        .expect("writing to string should not fail");
    }
    if let Some(supported) = reasoning.visible_summary_supported {
        writeln!(output, "reasoning_visible_summary_supported = {supported}")
            .expect("writing to string should not fail");
    }
    if let Some(supported) = reasoning.raw_reasoning_supported {
        writeln!(output, "reasoning_raw_supported = {supported}")
            .expect("writing to string should not fail");
    }
}

fn write_string_array(output: &mut String, key: &str, values: &[String]) {
    let rendered = values
        .iter()
        .map(|value| toml_string(value))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(output, "{key} = [{rendered}]").expect("writing to string should not fail");
}

fn write_model_profiles_toml(output: &mut String, profiles: &BTreeMap<String, ModelProfileConfig>) {
    for (profile_name, profile) in profiles {
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
        write_reasoning_inline_toml(output, &profile.reasoning);
        output.push('\n');
        write_string_map_table(
            output,
            &format!("model.profiles.{}.settings", toml_key(profile_name)),
            &profile.settings,
        );
        write_json_map_table(
            output,
            &format!("model.profiles.{}.request", toml_key(profile_name)),
            &profile.request,
        );
    }
}

fn write_model_aliases_toml(output: &mut String, aliases: &BTreeMap<String, ModelAliasConfig>) {
    for (alias_name, alias) in aliases {
        writeln!(output, "[model.aliases.{}]", toml_key(alias_name))
            .expect("writing to string should not fail");
        if let Some(provider_plugin_id) = &alias.provider_plugin_id {
            writeln!(
                output,
                "provider_plugin_id = {}",
                toml_string(provider_plugin_id)
            )
            .expect("writing to string should not fail");
        }
        writeln!(output, "model_id = {}", toml_string(&alias.model_id))
            .expect("writing to string should not fail");
        output.push('\n');
        write_json_map_table(
            output,
            &format!("model.aliases.{}.request", toml_key(alias_name)),
            &alias.request,
        );
    }
}

fn write_model_metadata_toml(
    output: &mut String,
    metadata: &BTreeMap<String, ModelMetadataConfig>,
) {
    for (model_id, metadata) in metadata {
        writeln!(output, "[model.metadata.{}]", toml_key(model_id))
            .expect("writing to string should not fail");
        if let Some(provider_plugin_id) = &metadata.provider_plugin_id {
            writeln!(
                output,
                "provider_plugin_id = {}",
                toml_string(provider_plugin_id)
            )
            .expect("writing to string should not fail");
        }
        if let Some(context_window) = metadata.context_window {
            writeln!(output, "context_window = {context_window}")
                .expect("writing to string should not fail");
        }
        if let Some(max_output_tokens) = metadata.max_output_tokens {
            writeln!(output, "max_output_tokens = {max_output_tokens}")
                .expect("writing to string should not fail");
        }
        output.push('\n');
    }
}

fn write_system_prompt_toml(output: &mut String, system_prompt: &SystemPromptConfig) {
    if system_prompt == &SystemPromptConfig::default() {
        return;
    }
    output.push_str("[system_prompt]\n");
    if system_prompt.mode != SystemPromptMode::Default {
        writeln!(
            output,
            "mode = {}",
            toml_string(system_prompt_mode_name(system_prompt.mode))
        )
        .expect("write to string");
    }
    if let Some(text) = &system_prompt.text {
        writeln!(output, "text = {}", toml_string(text)).expect("write to string");
    }
    output.push('\n');
    if system_prompt.sections != SystemPromptSectionsConfig::default() {
        output.push_str("[system_prompt.sections]\n");
        if !system_prompt.sections.repository_context {
            output.push_str("repository_context = false\n");
        }
        if !system_prompt.sections.dynamic_repository_context {
            output.push_str("dynamic_repository_context = false\n");
        }
        if !system_prompt.sections.agent_suffix {
            output.push_str("agent_suffix = false\n");
        }
        if !system_prompt.sections.skill_catalog {
            output.push_str("skill_catalog = false\n");
        }
        output.push('\n');
    }
}

const fn system_prompt_mode_name(mode: SystemPromptMode) -> &'static str {
    match mode {
        SystemPromptMode::Default => "default",
        SystemPromptMode::Replace => "replace",
    }
}

fn write_tui_toml(output: &mut String, tui: &TuiConfig) {
    if !tui.keybindings.is_empty() {
        write_tui_keybinding_section(output, "chat", &tui.keybindings.chat);
        write_tui_keybinding_section(output, "permission", &tui.keybindings.permission);
        write_tui_keybinding_section(output, "session_picker", &tui.keybindings.session_picker);
    }
    write_tui_mouse_toml(output, &tui.mouse);
    write_tui_inline_diff_toml(output, &tui.inline_diff);
    writeln!(output, "[tui.thinking]").expect("writing to string should not fail");
    writeln!(output, "show = {}", tui.thinking.show).expect("writing to string should not fail");
    writeln!(
        output,
        "mode = \"{}\"",
        tui_thinking_mode_name(tui.thinking.mode)
    )
    .expect("writing to string should not fail");
}

const fn tui_thinking_mode_name(mode: TuiThinkingMode) -> &'static str {
    match mode {
        TuiThinkingMode::Summary => "summary",
        TuiThinkingMode::Raw => "raw",
    }
}

fn write_tui_inline_diff_toml(output: &mut String, inline_diff: &TuiInlineDiffConfig) {
    if inline_diff == &TuiInlineDiffConfig::default() {
        return;
    }
    writeln!(output, "[tui.inline_diff]").expect("writing to string should not fail");
    if let Some(max_width) = inline_diff.max_width {
        writeln!(output, "max_width = {max_width}").expect("writing to string should not fail");
    }
    output.push('\n');
}

fn write_tui_mouse_toml(output: &mut String, mouse: &TuiMouseConfig) {
    if mouse == &TuiMouseConfig::default() {
        return;
    }
    writeln!(output, "[tui.mouse]").expect("writing to string should not fail");
    writeln!(output, "scroll_rows = {}", mouse.scroll_rows)
        .expect("writing to string should not fail");
    writeln!(output, "multi_click_ms = {}", mouse.multi_click_ms)
        .expect("writing to string should not fail");
    writeln!(
        output,
        "multi_click_max_distance = {}",
        mouse.multi_click_max_distance
    )
    .expect("writing to string should not fail");
    writeln!(
        output,
        "double_click_select = {}",
        toml_string(tui_mouse_click_selection_name(mouse.double_click_select))
    )
    .expect("writing to string should not fail");
    writeln!(
        output,
        "triple_click_select = {}",
        toml_string(tui_mouse_click_selection_name(mouse.triple_click_select))
    )
    .expect("writing to string should not fail");
    output.push('\n');
}

const fn tui_mouse_click_selection_name(selection: TuiMouseClickSelection) -> &'static str {
    match selection {
        TuiMouseClickSelection::Disabled => "disabled",
        TuiMouseClickSelection::Word => "word",
        TuiMouseClickSelection::Line => "line",
        TuiMouseClickSelection::All => "all",
    }
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
            || !permission.web.is_empty()
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
        write_action_map(output, "web", &permission.web);
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

const fn context_strategy_mode_name(mode: ContextStrategyMode) -> &'static str {
    match mode {
        ContextStrategyMode::ProviderReuse => "provider_reuse",
        ContextStrategyMode::ExplicitCachedTranscript => "explicit_cached_transcript",
    }
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
        if let Some(scheme) = &profile.scheme {
            writeln!(output, "scheme = {}", toml_string(scheme))
                .expect("writing to string should not fail");
        }
        output.push('\n');
        write_auth_mapping_tables(output, profile_name, &profile.map);
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
    if !skills.include_generic_repo_skills {
        output.push_str("include_generic_repo_skills = false\n");
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
    if !skills.follow_symlinks {
        output.push_str("follow_symlinks = false\n");
    }
    output.push('\n');

    if skills.prompt != SkillPromptConfig::default() {
        output.push_str("[skills.prompt]\n");
        if skills.prompt.catalog != SkillPromptCatalogMode::Summary {
            writeln!(
                output,
                "catalog = {}",
                toml_string(skill_prompt_catalog_mode_name(skills.prompt.catalog))
            )
            .expect("write to string");
        }
        if skills.prompt.max_bytes != default_skill_prompt_catalog_bytes() {
            writeln!(output, "max_bytes = {}", skills.prompt.max_bytes).expect("write to string");
        }
        if skills.prompt.max_description_chars != default_skill_prompt_description_chars() {
            writeln!(
                output,
                "max_description_chars = {}",
                skills.prompt.max_description_chars
            )
            .expect("write to string");
        }
        if !skills.prompt.include_sources {
            output.push_str("include_sources = false\n");
        }
        if skills.prompt.include_keywords {
            output.push_str("include_keywords = true\n");
        }
        output.push('\n');
    }

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

const fn skill_prompt_catalog_mode_name(mode: SkillPromptCatalogMode) -> &'static str {
    match mode {
        SkillPromptCatalogMode::Off => "off",
        SkillPromptCatalogMode::NamesOnly => "names_only",
        SkillPromptCatalogMode::Summary => "summary",
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
    for (plugin_id, value) in &plugins.config {
        if let Some(table) = value.as_table() {
            writeln!(output, "[plugins.config.{}]", toml_table_key(plugin_id))
                .expect("writing to string should not fail");
            for (key, value) in table {
                write_toml_value(output, key, value);
            }
            output.push('\n');
        }
    }
}

fn write_toml_value(output: &mut String, key: &str, value: &toml::Value) {
    let encoded = toml_value_literal(value);
    writeln!(output, "{} = {}", toml_key(key), encoded.trim())
        .expect("writing to string should not fail");
}

fn toml_value_literal(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => toml_string(value),
        toml::Value::Integer(value) => value.to_string(),
        toml::Value::Float(value) => value.to_string(),
        toml::Value::Boolean(value) => value.to_string(),
        toml::Value::Datetime(value) => value.to_string(),
        toml::Value::Array(values) => {
            let values = values
                .iter()
                .map(toml_value_literal)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{values}]")
        }
        toml::Value::Table(_) => "{}".to_string(),
    }
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

fn write_auth_mapping_tables(
    output: &mut String,
    profile_name: &str,
    values: &BTreeMap<String, AuthCredentialMapping>,
) {
    if values.is_empty() {
        return;
    }
    for (credential, mapping) in values {
        writeln!(
            output,
            "[auth.profiles.{}.map.{}]",
            toml_key(profile_name),
            toml_key(credential)
        )
        .expect("writing to string should not fail");
        if let Some(env) = &mapping.env {
            writeln!(output, "env = {}", toml_string(env))
                .expect("writing to string should not fail");
        }
        if let Some(key) = &mapping.key {
            writeln!(output, "key = {}", toml_string(key))
                .expect("writing to string should not fail");
        }
        output.push('\n');
    }
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

fn write_json_map_table(
    output: &mut String,
    table: &str,
    values: &BTreeMap<String, serde_json::Value>,
) {
    if values.is_empty() {
        return;
    }
    writeln!(output, "[{table}]").expect("writing to string should not fail");
    for (key, value) in values {
        write_json_toml_value(output, table, key, value);
    }
    output.push('\n');
}

fn write_json_toml_value(output: &mut String, table: &str, key: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(values) => {
            output.push('\n');
            writeln!(output, "[{table}.{}]", toml_key(key))
                .expect("writing to string should not fail");
            for (child_key, child_value) in values {
                write_json_toml_value(
                    output,
                    &format!("{table}.{}", toml_key(key)),
                    child_key,
                    child_value,
                );
            }
        }
        serde_json::Value::Array(values) => {
            let values = values
                .iter()
                .map(json_toml_inline_value)
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(output, "{} = [{values}]", toml_key(key))
                .expect("writing to string should not fail");
        }
        _ => writeln!(
            output,
            "{} = {}",
            toml_key(key),
            json_toml_inline_value(value)
        )
        .expect("writing to string should not fail"),
    }
}

fn json_toml_inline_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "\"\"".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => toml_string(value),
        serde_json::Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(json_toml_inline_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        serde_json::Value::Object(_) => toml_string(&value.to_string()),
    }
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
    default_config_paths_from(&env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Return default config paths in merge order for a starting directory.
#[must_use]
pub fn default_config_paths_from(start: &Path) -> Vec<PathBuf> {
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
    let root = discover_config_root(start).unwrap_or_else(|| start.to_path_buf());
    paths.push(root.join(DEFAULT_CONFIG_FILE_NAME));
    paths.push(root.join(".bcode").join(DEFAULT_CONFIG_FILE_NAME));
    paths
}

fn discover_config_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Load configuration from default paths.
///
/// # Errors
///
/// Returns an error if an existing config layer cannot be read, parsed, or composed.
pub fn load_config() -> Result<BcodeConfig, ConfigError> {
    load_config_from_paths_with_overrides(
        &default_config_paths(),
        &ConfigLoadOverrides::from_env_with_cli(None, None),
    )
}

/// Load configuration from default paths with explicit overrides.
///
/// # Errors
///
/// Returns an error if an existing config layer cannot be read, parsed, or composed.
pub fn load_config_with_overrides(
    overrides: &ConfigLoadOverrides,
) -> Result<BcodeConfig, ConfigError> {
    load_config_from_paths_with_overrides(&default_config_paths(), overrides)
}

/// Load and merge configuration from the provided paths.
///
/// Missing paths are ignored. Existing files are merged in the order provided.
/// Process-scoped overrides are honored when present.
///
/// # Errors
///
/// Returns an error if an existing config layer cannot be read, parsed, or composed.
pub fn load_config_from_paths(paths: &[PathBuf]) -> Result<BcodeConfig, ConfigError> {
    let process_overrides = process_config_overrides()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    process_overrides.as_ref().map_or_else(
        || load_config_from_paths_with_overrides(paths, &ConfigLoadOverrides::default()),
        |overrides| load_config_from_paths_with_overrides(paths, overrides),
    )
}

/// Load composed raw configuration from default paths.
///
/// # Errors
///
/// Returns an error if an existing config layer cannot be read, parsed, or composed.
pub fn load_composed_config_value() -> Result<toml::Value, ConfigError> {
    load_composed_config_value_with_overrides(&ConfigLoadOverrides::from_env_with_cli(None, None))
}

/// Load composed raw configuration from default paths with explicit overrides.
///
/// # Errors
///
/// Returns an error if an existing config layer cannot be read, parsed, or composed.
pub fn load_composed_config_value_with_overrides(
    overrides: &ConfigLoadOverrides,
) -> Result<toml::Value, ConfigError> {
    let raw = merged_raw_config_value_with_overrides(&default_config_paths(), overrides)?;
    let (resolved, _resolution) = resolve_composed_config_value(&raw)?;
    Ok(resolved)
}

/// Load and merge configuration from paths with explicit override layers.
///
/// Precedence is: base config, provided paths, env config file, env raw TOML,
/// CLI config file, CLI raw TOML.
///
/// # Errors
///
/// Returns an error if an existing config layer cannot be read, parsed, or composed.
pub fn load_config_from_paths_with_overrides(
    paths: &[PathBuf],
    overrides: &ConfigLoadOverrides,
) -> Result<BcodeConfig, ConfigError> {
    let raw = merged_raw_config_value_with_overrides(paths, overrides)?;
    let (resolved, _resolution) = resolve_composed_config_value(&raw)?;
    validate_config_value(resolved, "composed config")
}

fn merged_raw_config_value_with_overrides(
    paths: &[PathBuf],
    overrides: &ConfigLoadOverrides,
) -> Result<toml::Value, ConfigError> {
    let mut merged = toml::Value::Table(toml::Table::new());

    if let Some(path) = overrides.base_config_path.as_ref()
        && path.exists()
    {
        merge_toml_value(&mut merged, load_toml_file(path)?);
    }

    for path in paths {
        if path.exists() {
            merge_toml_value(&mut merged, load_toml_file(path)?);
        }
    }

    if let Some(path) = overrides.env_config_path.as_ref() {
        let path = resolve_config_override_path(path);
        if !path.exists() {
            return Err(ConfigError::Io {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "environment config path not found",
                ),
            });
        }
        merge_toml_value(&mut merged, load_toml_file(&path)?);
    }

    if let Some(raw) = overrides.env_config_toml.as_deref() {
        merge_toml_value(&mut merged, parse_raw_toml_config(raw, "env")?);
    }

    if let Some(path) = overrides.cli_config_path.as_ref() {
        let path = resolve_config_override_path(path);
        if !path.exists() {
            return Err(ConfigError::Io {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "CLI config path not found",
                ),
            });
        }
        merge_toml_value(&mut merged, load_toml_file(&path)?);
    }

    if let Some(raw) = overrides.cli_config_toml.as_deref() {
        merge_toml_value(&mut merged, parse_raw_toml_config(raw, "cli")?);
    }

    Ok(merged)
}

fn resolve_config_override_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
}

fn parse_raw_toml_config(raw: &str, source_name: &str) -> Result<toml::Value, ConfigError> {
    toml::from_str(raw).map_err(|source| ConfigError::Composition {
        message: format!("failed to parse {source_name} raw config TOML: {source}"),
    })
}

fn load_toml_file(path: &Path) -> Result<toml::Value, ConfigError> {
    let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&contents).map_err(|source| ConfigError::Composition {
        message: format!("failed to parse config {}: {source}", path.display()),
    })
}

fn read_config(path: &Path) -> Result<BcodeConfig, ConfigError> {
    let raw = load_toml_file(path)?;
    let (resolved, _resolution) = resolve_composed_config_value(&raw)?;
    let context = format!("config {}", path.display());
    validate_config_value(resolved, &context)
}

#[cfg(test)]
mod tests {
    use super::{
        BcodeConfig, CompactionMode, ConfigDocSchema, ConfigError, ConfigLoadOverrides,
        ContextStrategyMode, NestedFieldDoc, TuiAccentTransitionCurve, TuiMouseConfig,
        default_config_paths_from, default_permissions_state_path, load_config_from_paths,
        load_config_from_paths_with_overrides, load_permissions_state_from, merge_config_values,
        plugin_selection_with_default_plugin_ids, upsert_agent_permission_rule,
    };
    use bcode_agent_policy_models::Action;
    use bcode_plugin::PluginSelection;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_CODE_REVIEW_PLUGIN_ID: &str = "bcode.code_review";
    const TEST_AGENT_PROFILE_PLUGIN_ID: &str = "bcode.default-agents";
    const TEST_PI_SESSION_IMPORT_PLUGIN_ID: &str = "bcode.pi-session-import";
    const TEST_DOCUMENT_PLUGIN_ID: &str = "bcode.example-document";
    const TEST_FILESYSTEM_PLUGIN_ID: &str = "bcode.example-filesystem";
    const TEST_GIT_PLUGIN_ID: &str = "bcode.example-git";
    const TEST_SHELL_PLUGIN_ID: &str = "bcode.example-shell";
    const TEST_WEB_SEARCH_PLUGIN_ID: &str = "bcode.example-web-search";
    const TEST_WORKTREE_PLUGIN_ID: &str = "bcode.example-worktree";
    const TEST_DEFAULT_CORE_PLUGIN_IDS: &[&str] = &[
        TEST_CODE_REVIEW_PLUGIN_ID,
        TEST_DOCUMENT_PLUGIN_ID,
        TEST_FILESYSTEM_PLUGIN_ID,
        TEST_GIT_PLUGIN_ID,
        TEST_SHELL_PLUGIN_ID,
        TEST_WEB_SEARCH_PLUGIN_ID,
        TEST_WORKTREE_PLUGIN_ID,
        TEST_AGENT_PROFILE_PLUGIN_ID,
        TEST_PI_SESSION_IMPORT_PLUGIN_ID,
    ];

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn stable_config_doc_sections_are_derive_backed() {
        for (section, keys) in [
            ("system_prompt", &["mode", "text", "sections"][..]),
            ("skills", &["enabled", "auto_activate", "prompt"]),
            (
                "observability",
                &["level", "persist_tool_io", "max_blob_bytes"],
            ),
            ("daemon", &["idle_shutdown", "idle_shutdown_after_secs"]),
            ("worktree", &["root", "branch_prefix", "setup"]),
            ("tools", &["shell"]),
            (
                "session_import",
                &["enabled", "auto_discover_on_startup", "pi", "opencode"],
            ),
        ] {
            let root_fields = BcodeConfig::field_docs();
            let field = root_fields
                .iter()
                .find(|field| field.toml_key == section)
                .unwrap_or_else(|| panic!("missing root config doc section: {section}"));
            let Some(NestedFieldDoc::Inline { fields, .. }) = &field.nested else {
                panic!("root config doc section {section} is not inline nested");
            };

            for key in keys {
                assert!(
                    fields.iter().any(|field| field.toml_key == *key),
                    "missing derived key {section}.{key}"
                );
            }
        }

        assert_eq!(
            super::SystemPromptMode::config_doc_values(),
            &["default", "replace"]
        );
        assert_eq!(
            super::SkillAutoActivateMode::config_doc_values(),
            &["off", "suggest", "on"]
        );
        assert_eq!(
            super::ShellToolEnvMode::config_doc_values(),
            &["auto", "inherit", "direnv"]
        );
    }

    #[test]
    fn root_config_doc_schema_documents_major_nested_sections() {
        let fields = BcodeConfig::field_docs();

        for section in ["model", "auth", "agent", "skills", "system_prompt", "tools"] {
            assert!(
                fields.iter().any(|field| field.toml_key == section),
                "missing root config doc section: {section}"
            );
        }
    }

    #[test]
    fn removed_shorthand_agent_tool_ids_are_rejected() {
        for (tool_id, replacement) in [
            ("bash", "shell.run"),
            ("read", "filesystem.read"),
            ("grep", "filesystem.grep"),
            ("find", "filesystem.find"),
            ("ls", "filesystem.list"),
            ("stat", "filesystem.stat"),
            ("write", "filesystem.write"),
            ("edit", "filesystem.edit"),
            ("worktree.read", "worktree.list"),
        ] {
            let result = load_config_from_paths_with_overrides(
                &[],
                &ConfigLoadOverrides::from_env_with_cli(
                    None,
                    Some(format!("[agent.plan.tools]\n\"{tool_id}\" = true\n")),
                ),
            );

            assert!(
                matches!(
                    result,
                    Err(ConfigError::RemovedShorthandToolId {
                        agent_id,
                        tool_id: actual_tool_id,
                        replacement: actual_replacement,
                    }) if agent_id == "plan" && actual_tool_id == tool_id && actual_replacement == replacement
                ),
                "{tool_id} should be rejected with replacement {replacement}"
            );
        }
    }

    #[test]
    fn auth_pool_config_loads_from_toml_and_resolves_model_profile() {
        let config: BcodeConfig = toml::from_str(
            r#"
[auth.profiles.openai]
backend = "sshenv"
scheme = "chatgpt"

[auth.profiles.openai-2]
backend = "sshenv"
scheme = "chatgpt"

[auth.pools.openai]
provider_plugin_id = "bcode.openai-compatible"
strategy = "failover"
profiles = ["openai", "openai-2"]

[model.profiles.openai]
provider_plugin_id = "bcode.openai-compatible"
model_id = "gpt-5.5"
auth_pool = "openai"
"#,
        )
        .expect("config should parse");

        let pool = config
            .auth
            .pools
            .get("openai")
            .expect("auth pool should parse");
        assert_eq!(pool.profiles, vec!["openai", "openai-2"]);
        assert_eq!(
            pool.provider_plugin_id.as_deref(),
            Some("bcode.openai-compatible")
        );

        let mut config = config;
        config.model.profile = Some("openai".to_string());
        let selection = config.resolved_model_selection();
        assert_eq!(selection.auth_pool.as_deref(), Some("openai"));
        assert_eq!(selection.auth_profile, None);
    }

    #[test]
    fn model_retry_config_loads_from_toml() {
        let config: BcodeConfig = toml::from_str(
            r#"
[model.retry]
max_overload_retries = 3
overload_initial_delay_ms = 1000
overload_max_delay_ms = 10000
remote_catalog_rules_enabled = false

[[model.retry.rules]]
id = "unsupported-content-type"
provider_plugin_id = "bcode.openai-compatible"
model_id_contains = "claude"
max_retries = 2
initial_delay_ms = 500
max_delay_ms = 4000
use_provider_retry_hint = false

[model.retry.rules.match]
code = "http_400"
message_contains = "Unsupported content type"
"#,
        )
        .expect("config should parse");

        assert_eq!(config.model.retry.max_overload_retries, 3);
        assert_eq!(config.model.retry.overload_initial_delay_ms, 1_000);
        assert_eq!(config.model.retry.overload_max_delay_ms, 10_000);
        assert!(!config.model.retry.remote_catalog_rules_enabled);
        let rule = config
            .model
            .retry
            .rules
            .first()
            .expect("custom retry rule should parse");
        assert_eq!(rule.id, "unsupported-content-type");
        assert_eq!(
            rule.provider_plugin_id.as_deref(),
            Some("bcode.openai-compatible")
        );
        assert_eq!(rule.model_id_contains.as_deref(), Some("claude"));
        assert_eq!(rule.max_retries, Some(2));
        assert_eq!(rule.initial_delay_ms, Some(500));
        assert_eq!(rule.max_delay_ms, Some(4_000));
        assert_eq!(rule.use_provider_retry_hint, Some(false));
        assert_eq!(rule.r#match.code.as_deref(), Some("http_400"));
        assert_eq!(
            rule.r#match.message_contains.as_deref(),
            Some("Unsupported content type")
        );
    }

    #[test]
    fn tui_mouse_config_loads_from_toml() {
        let config: BcodeConfig = toml::from_str(
            r#"
[tui.mouse]
scroll_rows = 4
multi_click_ms = 300
multi_click_max_distance = 1
double_click_select = "word"
triple_click_select = "all"
"#,
        )
        .expect("config should parse");

        assert_eq!(config.tui.mouse.scroll_rows, 4);
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

    #[test]
    fn tui_theme_transition_curve_loads_from_toml() {
        let config: BcodeConfig = toml::from_str(
            r#"
[tui.theme]
accent_transition = "transition"
accent_transition_ms = 180
accent_transition_curve = "ease_in_out"
"#,
        )
        .expect("config should parse");

        assert_eq!(config.tui.theme.accent_transition_ms, 180);
        assert_eq!(
            config.tui.theme.accent_transition_curve,
            TuiAccentTransitionCurve::EaseInOut
        );
    }

    #[test]
    fn tui_theme_transition_curve_defaults_to_ease_out() {
        assert_eq!(
            BcodeConfig::default().tui.theme.accent_transition_curve,
            TuiAccentTransitionCurve::EaseOut
        );
    }

    #[test]
    fn tui_mouse_scroll_rows_defaults_and_clamps_zero() {
        assert_eq!(TuiMouseConfig::default().scroll_rows, 3);
        assert_eq!(TuiMouseConfig::default().effective_scroll_rows(), 3);
        assert_eq!(
            TuiMouseConfig {
                scroll_rows: 0,
                ..TuiMouseConfig::default()
            }
            .effective_scroll_rows(),
            1
        );
    }

    fn assert_default_core_plugins_enabled(plugin_selection: &PluginSelection) {
        assert!(
            plugin_selection
                .enabled
                .contains(TEST_CODE_REVIEW_PLUGIN_ID)
        );
        assert!(plugin_selection.enabled.contains(TEST_DOCUMENT_PLUGIN_ID));
        assert!(plugin_selection.enabled.contains(TEST_FILESYSTEM_PLUGIN_ID));
        assert!(plugin_selection.enabled.contains(TEST_GIT_PLUGIN_ID));
        assert!(plugin_selection.enabled.contains(TEST_SHELL_PLUGIN_ID));
        assert!(plugin_selection.enabled.contains(TEST_WEB_SEARCH_PLUGIN_ID));
        assert!(
            plugin_selection
                .enabled
                .contains(TEST_AGENT_PROFILE_PLUGIN_ID)
        );
        assert!(
            plugin_selection
                .enabled
                .contains(TEST_PI_SESSION_IMPORT_PLUGIN_ID)
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
"example.tool" = true

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
        assert!(!config.plugins.enabled.contains("example.a"));
        assert!(config.plugins.enabled.contains("example.c"));
        assert!(!config.plugins.disabled.contains("example.b"));
        assert!(config.plugins.disabled.contains("example.d"));

        let build = config
            .agent
            .get("build")
            .expect("build agent config should be loaded");
        assert_eq!(
            build.tools.get("example.tool").copied(),
            Some(true),
            "build agent should enable example.tool"
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
    fn resolves_model_alias_request_options() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        let config: BcodeConfig = toml::from_str(
            r#"
[model]
profile = "openai-fast"

[model.profiles.openai-fast]
provider_plugin_id = "bcode.openai-compatible"
model_id = "gpt-5.5-fast"

[model.profiles.openai-fast.request]
custom_boolean = true

[model.aliases."gpt-5.5-fast"]
provider_plugin_id = "bcode.openai-compatible"
model_id = "gpt-5.5"

[model.aliases."gpt-5.5-fast".request]
service_tier = "priority"
custom_boolean = false
"#,
        )
        .expect("alias config should parse");

        let selection = config.resolved_model_selection();
        assert_eq!(selection.selected_model_id.as_deref(), Some("gpt-5.5-fast"));
        assert_eq!(selection.model_id.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            selection.request.get("service_tier"),
            Some(&bcode_model::ProviderRequestValue::from(serde_json::json!(
                "priority"
            )))
        );
        assert_eq!(
            selection.request.get("custom_boolean"),
            Some(&bcode_model::ProviderRequestValue::from(serde_json::json!(
                true
            ))),
            "profile request options override alias defaults"
        );

        restore_provider_env(previous_env);
    }

    #[test]
    fn parses_context_strategy_mode() {
        let config: BcodeConfig = toml::from_str(
            r#"
[model.context_strategy]
mode = "explicit_cached_transcript"
"#,
        )
        .expect("config should parse");

        assert_eq!(
            config.model.context_strategy.mode,
            ContextStrategyMode::ExplicitCachedTranscript
        );
        assert_eq!(
            config.model.effective_prompt_cache_mode(),
            bcode_model::PromptCacheMode::Aggressive
        );
        assert_eq!(
            config.model.effective_conversation_reuse_mode(),
            bcode_model::ConversationReuseMode::Off
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
        let plugin_selection =
            plugin_selection_with_default_plugin_ids(&config, TEST_DEFAULT_CORE_PLUGIN_IDS);

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
        let plugin_selection =
            plugin_selection_with_default_plugin_ids(&config, TEST_DEFAULT_CORE_PLUGIN_IDS);

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
        let plugin_selection =
            plugin_selection_with_default_plugin_ids(&config, TEST_DEFAULT_CORE_PLUGIN_IDS);

        assert!(
            !plugin_selection
                .enabled
                .contains(TEST_AGENT_PROFILE_PLUGIN_ID)
        );
        assert!(plugin_selection.enabled.contains(TEST_FILESYSTEM_PLUGIN_ID));
        assert!(plugin_selection.enabled.contains(TEST_SHELL_PLUGIN_ID));

        restore_provider_env(previous_env);
    }

    #[test]
    fn default_code_review_can_be_disabled() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        let config: BcodeConfig = toml::from_str(
            r#"
[plugins]
disabled = ["bcode.code_review"]
"#,
        )
        .expect("config should parse");
        let plugin_selection =
            plugin_selection_with_default_plugin_ids(&config, TEST_DEFAULT_CORE_PLUGIN_IDS);

        assert!(
            !plugin_selection
                .enabled
                .contains(TEST_CODE_REVIEW_PLUGIN_ID)
        );
        assert!(plugin_selection.enabled.contains(TEST_FILESYSTEM_PLUGIN_ID));

        restore_provider_env(previous_env);
    }

    #[test]
    fn default_pi_session_import_can_be_disabled() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let previous_env = clear_provider_env();
        let config: BcodeConfig = toml::from_str(
            r#"
[plugins]
disabled = ["bcode.pi-session-import"]
"#,
        )
        .expect("config should parse");
        let plugin_selection =
            plugin_selection_with_default_plugin_ids(&config, TEST_DEFAULT_CORE_PLUGIN_IDS);

        assert!(
            !plugin_selection
                .enabled
                .contains(TEST_PI_SESSION_IMPORT_PLUGIN_ID)
        );
        assert!(plugin_selection.enabled.contains(TEST_FILESYSTEM_PLUGIN_ID));

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
disabled = ["bcode.example-shell"]
"#,
        )
        .expect("config should parse");
        let plugin_selection =
            plugin_selection_with_default_plugin_ids(&config, TEST_DEFAULT_CORE_PLUGIN_IDS);

        assert!(plugin_selection.enabled.contains(TEST_FILESYSTEM_PLUGIN_ID));
        assert!(
            plugin_selection
                .enabled
                .contains(TEST_AGENT_PROFILE_PLUGIN_ID)
        );
        assert!(!plugin_selection.enabled.contains(TEST_SHELL_PLUGIN_ID));

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

        let plugin_selection =
            plugin_selection_with_default_plugin_ids(&config, TEST_DEFAULT_CORE_PLUGIN_IDS);
        assert!(plugin_selection.enabled.contains("bcode.bedrock"));
        assert!(plugin_selection.enabled.contains("bcode.openai-compatible"));
        assert_default_core_plugins_enabled(&plugin_selection);

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

        let plugin_selection =
            plugin_selection_with_default_plugin_ids(&config, TEST_DEFAULT_CORE_PLUGIN_IDS);
        assert!(plugin_selection.enabled.contains("bcode.openai-compatible"));
        assert!(plugin_selection.enabled.contains("bcode.bedrock"));
        assert_default_core_plugins_enabled(&plugin_selection);

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
    fn raw_agent_config_merge_recurses_key_by_key() {
        let mut base: toml::Value = toml::from_str(
            r##"
[agent.build]
accent = "#22d3ee"

[agent.build.tools]
"example.read" = true
"example.write" = true

[agent.build.permission.bash]
"cargo *" = "allow"
"git push *" = "deny"
"##,
        )
        .expect("base TOML should parse");
        let overlay: toml::Value = toml::from_str(
            r##"
[agent.build]
accent = "#6b7280"

[agent.build.tools]
"example.write" = false
"example.run" = true

[agent.build.permission.bash]
"cargo *" = "deny"
"echo *" = "allow"
"##,
        )
        .expect("overlay TOML should parse");

        merge_config_values(&mut base, overlay);
        let build = base
            .get("agent")
            .and_then(toml::Value::as_table)
            .and_then(|agents| agents.get("build"))
            .and_then(toml::Value::as_table)
            .expect("build agent table should exist");
        let tools = build
            .get("tools")
            .and_then(toml::Value::as_table)
            .expect("tools table should exist");
        let bash_rules = build
            .get("permission")
            .and_then(toml::Value::as_table)
            .and_then(|permission| permission.get("bash"))
            .and_then(toml::Value::as_table)
            .expect("bash permission table should exist");

        assert_eq!(
            build.get("accent").and_then(toml::Value::as_str),
            Some("#6b7280")
        );
        assert_eq!(
            tools.get("example.read").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            tools.get("example.write").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            tools.get("example.run").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            bash_rules.get("cargo *").and_then(toml::Value::as_str),
            Some("deny")
        );
        assert_eq!(
            bash_rules.get("git push *").and_then(toml::Value::as_str),
            Some("deny")
        );
        assert_eq!(
            bash_rules.get("echo *").and_then(toml::Value::as_str),
            Some("allow")
        );
    }

    #[test]
    fn raw_agent_config_merge_adds_state_only_agent() {
        let mut base = toml::Value::Table(toml::Table::new());
        let overlay: toml::Value = toml::from_str(
            r##"
[agent.scratch]
accent = "#abcdef"

[agent.scratch.tools]
"example.run" = true

[agent.scratch.permission.bash]
"*" = "ask"
"##,
        )
        .expect("overlay TOML should parse");

        merge_config_values(&mut base, overlay);

        let scratch = base
            .get("agent")
            .and_then(toml::Value::as_table)
            .and_then(|agents| agents.get("scratch"))
            .and_then(toml::Value::as_table)
            .expect("scratch agent should be added");
        assert_eq!(
            scratch.get("accent").and_then(toml::Value::as_str),
            Some("#abcdef")
        );
        assert_eq!(
            scratch
                .get("tools")
                .and_then(toml::Value::as_table)
                .and_then(|tools| tools.get("example.run"))
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            scratch
                .get("permission")
                .and_then(toml::Value::as_table)
                .and_then(|permission| permission.get("bash"))
                .and_then(toml::Value::as_table)
                .and_then(|bash| bash.get("*"))
                .and_then(toml::Value::as_str),
            Some("ask")
        );
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

    #[test]
    fn composition_profile_deep_merges_and_arrays_replace() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let config_path = root.join("bcode.toml");
        std::fs::write(
            &config_path,
            r#"
[composition]
active_profile = "dev"

[composition.profiles.base.patch.plugins]
enabled = ["base"]

[composition.profiles.base.patch.agent.build.tools]
"example.run" = false
"example.read" = true

[composition.profiles.dev]
extends = ["base"]

[composition.profiles.dev.patch.plugins]
enabled = ["dev"]

[composition.profiles.dev.patch.agent.build.tools]
"example.run" = true

[agent.build.permission]
read = { "**" = "allow" }
"#,
        )
        .expect("config should be written");

        let config = load_config_from_paths(&[config_path]).expect("config should load");
        assert_eq!(
            config.plugins.enabled.iter().cloned().collect::<Vec<_>>(),
            vec!["dev".to_string()],
            "arrays replace rather than concatenate"
        );
        let build = config.agent.get("build").expect("build agent should exist");
        assert_eq!(build.tools.get("example.run"), Some(&true));
        assert_eq!(build.tools.get("example.read"), Some(&true));
        assert_eq!(
            build.permission.read.get("**"),
            Some(&bcode_agent_policy_models::Action::Allow)
        );
    }

    #[test]
    fn composition_layer_order_can_make_profile_override_config() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let config_path = root.join("bcode.toml");
        std::fs::write(
            &config_path,
            r#"
[composition]
active_profile = "override"
layer_order = ["defaults", "config", "profile:active"]

[composition.profiles.override.patch.model]
max_tool_rounds = 9

[model]
max_tool_rounds = 3
"#,
        )
        .expect("config should be written");

        let config = load_config_from_paths(&[config_path]).expect("config should load");
        assert_eq!(config.model.max_tool_rounds, Some(9));
    }

    #[test]
    fn composition_rejects_unknown_profile_and_cycles() {
        let unknown = toml::from_str(
            r#"
[composition]
active_profile = "missing"
"#,
        )
        .expect("raw toml should parse");
        assert!(super::resolve_composed_config_value(&unknown).is_err());

        let cycle = toml::from_str(
            r#"
[composition]
active_profile = "a"

[composition.profiles.a]
extends = ["b"]

[composition.profiles.b]
extends = ["a"]
"#,
        )
        .expect("raw toml should parse");
        assert!(super::resolve_composed_config_value(&cycle).is_err());
    }

    #[test]
    fn explicit_override_layers_apply_after_paths() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let base = root.join("base.toml");
        let user = root.join("user.toml");
        let env_path = root.join("env.toml");
        let cli_path = root.join("cli.toml");
        std::fs::write(&base, "[model]\nmax_tool_rounds = 1\n").expect("base should be written");
        std::fs::write(&user, "[model]\nmax_tool_rounds = 2\n").expect("user should be written");
        std::fs::write(&env_path, "[model]\nmax_tool_rounds = 3\n").expect("env should be written");
        std::fs::write(&cli_path, "[model]\nmax_tool_rounds = 5\n").expect("cli should be written");

        let config = load_config_from_paths_with_overrides(
            &[user],
            &ConfigLoadOverrides {
                base_config_path: Some(base),
                env_config_path: Some(env_path),
                env_config_toml: Some("[model]\nmax_tool_rounds = 4\n".to_string()),
                cli_config_path: Some(cli_path),
                cli_config_toml: Some("[model]\nmax_tool_rounds = 6\n".to_string()),
            },
        )
        .expect("config should load");

        assert_eq!(config.model.max_tool_rounds, Some(6));
    }

    #[test]
    fn default_config_paths_include_repo_local_layers() {
        let root = unique_temp_dir();
        let nested = root.join("src").join("bin");
        std::fs::create_dir_all(root.join(".git")).expect("git dir should be created");
        std::fs::create_dir_all(&nested).expect("nested dir should be created");

        let paths = default_config_paths_from(&nested);

        assert!(paths.contains(&root.join("bcode.toml")));
        assert!(paths.contains(&root.join(".bcode").join("bcode.toml")));
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("bcode-config-test-{nanos}"))
    }
}
