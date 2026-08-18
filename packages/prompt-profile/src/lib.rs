#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Typed service contract for model-scoped prompt profiles.

use std::collections::BTreeMap;

use bcode_model::ModelApiSurface;
use bcode_session_models::SessionId;
use bcode_tool::ToolDefinition;
use serde::{Deserialize, Serialize};

/// Plugin service interface for prompt-profile providers.
pub const PROMPT_PROFILE_INTERFACE_ID: &str = "bcode.prompt-profile/v1";
/// Operation that resolves the prompt profile for one model target.
pub const OP_RESOLVE_PROMPT_PROFILE: &str = "resolve_prompt_profile";

/// Canonical model facts supplied by the host to a profile provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptModelTarget {
    /// Selected model-provider plugin ID.
    pub provider_plugin_id: String,
    /// Catalog provider ID used for model resolution.
    pub catalog_provider_id: String,
    /// Stable catalog entry ID, when the model is catalog-known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_entry_id: Option<String>,
    /// Model ID requested by the user or configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model_id: Option<String>,
    /// Effective provider-native model ID used for this turn.
    pub effective_model_id: String,
    /// Catalog model family, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Catalog API surface, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_surface: Option<ModelApiSurface>,
}

/// Request for [`OP_RESOLVE_PROMPT_PROFILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvePromptProfileRequest {
    /// Session receiving the profile.
    pub session_id: SessionId,
    /// Active agent profile ID.
    pub agent_id: String,
    /// Canonical model target facts.
    pub target: PromptModelTarget,
    /// Model-facing tool catalog available for this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    /// Fully resolved declarative configuration encoded as TOML by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_config_toml: Option<Box<String>>,
}

/// Text-composition behavior for a profile override.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextOverrideMode {
    /// Add text after the existing value.
    #[default]
    Append,
    /// Add text before the existing value.
    Prepend,
    /// Replace the existing value.
    Replace,
}

/// Model-facing tool-description override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptionOverride {
    /// Composition behavior.
    #[serde(default)]
    pub mode: TextOverrideMode,
    /// Text to compose with the existing description.
    pub text: String,
}

/// Resolved prompt-profile response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptProfileResponse {
    /// Highest-precedence system-prompt replacement, when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_replacement: Option<String>,
    /// System-prompt additions before the stable base, in layer order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_prompt_prepends: Vec<String>,
    /// System-prompt additions after the stable base, in layer order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_prompt_appends: Vec<String>,
    /// Tool-description overrides keyed by exact tool name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_description_overrides: BTreeMap<String, Vec<ToolDescriptionOverride>>,
    /// Profile layers that contributed to this response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_layers: Vec<String>,
    /// Bounded provider diagnostics safe for request tracing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_round_trips_through_json() {
        let response = PromptProfileResponse {
            system_prompt_replacement: None,
            system_prompt_prepends: Vec::new(),
            system_prompt_appends: vec!["Use complete tool output.".to_string()],
            tool_description_overrides: BTreeMap::from([(
                "shell.run".to_string(),
                vec![ToolDescriptionOverride {
                    mode: TextOverrideMode::Append,
                    text: "Do not self-truncate output.".to_string(),
                }],
            )]),
            applied_layers: vec!["catalog_entry:anthropic.claude-opus-5".to_string()],
            diagnostics: Vec::new(),
        };
        let encoded = serde_json::to_string(&response).expect("encode response");
        let decoded = serde_json::from_str(&encoded).expect("decode response");
        assert_eq!(response, decoded);
    }
}
