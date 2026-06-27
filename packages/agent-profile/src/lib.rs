#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Agent profile service contract types for Bcode.
//!
//! Agent profiles are generic session-scoped operating profiles. Plugins can
//! provide profiles such as `plan`, `build`, `review`, or project-specific
//! agents, along with prompt context and tool-call policy decisions.

use bcode_session_models::SessionId;
use bcode_tool::{ToolDefinition, ToolPolicyMetadata, ToolSideEffect};
use serde::{Deserialize, Serialize};

/// Plugin service interface for agent profile providers.
pub const AGENT_PROFILE_INTERFACE_ID: &str = "bcode.agent-profile/v1";

/// Operation for listing available agent profiles.
pub const OP_LIST_AGENTS: &str = "list_agents";

/// Operation for retrieving prompt/tool context for the active agent profile.
pub const OP_AGENT_CONTEXT: &str = "agent_context";

/// Operation for evaluating a tool call against an agent profile.
pub const OP_EVALUATE_TOOL_CALL: &str = "evaluate_tool_call";

/// Operation for reporting the active policy config source/status.
pub const OP_POLICY_STATUS: &str = "policy_status";

/// Agent profile metadata shown in the TUI and command palette.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Stable profile identifier, e.g. `plan` or `build`.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Optional compact UI badge.
    #[serde(default)]
    pub badge: Option<String>,
    /// Optional UI accent color, encoded as `#RRGGBB`.
    #[serde(default)]
    pub accent: Option<String>,
    /// Optional slash-command aliases.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Whether this profile is the provider's default.
    #[serde(default)]
    pub is_default: bool,
}

/// Response returned by [`OP_LIST_AGENTS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentList {
    /// Available agent profiles.
    pub agents: Vec<AgentInfo>,
}

/// Request for [`OP_AGENT_CONTEXT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContextRequest {
    /// Session ID using the active agent.
    pub session_id: SessionId,
    /// Active agent profile ID.
    pub agent_id: String,
    /// Tool definitions discovered from currently loaded tool provider plugins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_tools: Vec<ToolDefinition>,
}

/// Response returned by [`OP_AGENT_CONTEXT`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContextResponse {
    /// Optional system-prompt suffix contributed by the active agent.
    #[serde(default)]
    pub system_prompt_suffix: Option<String>,
    /// Optional exact list of tool names exposed to the model.
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
}

/// Request for [`OP_EVALUATE_TOOL_CALL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluateToolCallRequest {
    /// Session ID executing the call.
    pub session_id: SessionId,
    /// Active agent profile ID.
    pub agent_id: String,
    /// Tool name requested by the model.
    pub tool_name: String,
    /// Declared side-effect category for the tool.
    pub side_effect: ToolSideEffect,
    /// Declared plugin-owned policy metadata for the tool.
    #[serde(default)]
    pub policy: ToolPolicyMetadata,
    /// Tool arguments.
    pub arguments: serde_json::Value,
    /// Host current working directory for path-boundary policy checks.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Agent policy decision for a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDecision {
    /// Run the tool without an extra prompt.
    Allow,
    /// Ask via Bcode's normal permission prompt path.
    Ask,
    /// Deny the tool call and return the reason to the model.
    Deny,
}

/// Response returned by [`OP_EVALUATE_TOOL_CALL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluateToolCallResponse {
    /// Policy decision.
    pub decision: AgentDecision,
    /// Optional user/model-facing reason.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Agent policy provider status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyStatusResponse {
    /// Human-readable source label.
    pub source: String,
    /// True when the provider is using built-in fallback policy.
    pub using_default: bool,
    /// Enabled tools for the implementation/build agent after policy composition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build_enabled_tools: Vec<String>,
    /// Enabled tools for the planning/read-only agent after policy composition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_enabled_tools: Vec<String>,
    /// Non-fatal degradation diagnostics surfaced by the policy provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}
