#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Agent profile service contract types for Bcode.
//!
//! Agent profiles are generic session-scoped operating profiles. Plugins can
//! provide profiles such as `plan`, `build`, `review`, or project-specific
//! agents, along with prompt context and tool-call policy decisions.

use bcode_session_models::SessionId;
use bcode_tool::{
    ToolAuthorizationFact, ToolDefinition, ToolPreparationRequest, ToolPreparationResponse,
    ToolSideEffect,
};
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

/// Namespace for the standard owner-produced agent policy authorization fact.
pub const TOOL_POLICY_AUTHORIZATION_NAMESPACE: &str = "bcode.agent-policy.tool";

/// Current schema version for [`ToolPolicyAuthorizationMetadata`].
pub const TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION: u32 = 1;

/// Action for invoking a tool under the standard agent policy authorization fact.
pub const TOOL_POLICY_AUTHORIZATION_ACTION_INVOKE: &str = "invoke";

/// Tool-owner-produced agent policy operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolPolicyOperation {
    /// Evaluate shell command policy for an owner-extracted command.
    Command { command: Option<String> },
    /// Evaluate web policy for an owner-extracted URL.
    Web { url: Option<String> },
    /// Evaluate read policy for owner-extracted paths.
    Read { paths: Vec<String> },
    /// Evaluate write/edit policy for owner-extracted paths.
    Write {
        paths: Vec<String>,
        category: String,
    },
    /// Read-only operation requiring no domain-specific evaluation.
    ReadOnly,
    /// Mutating operation requiring explicit tool enablement/permission.
    Mutating,
}

/// Tool-owner-produced metadata consumed only by the agent policy adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicyAuthorizationMetadata {
    /// Whether the owner requires explicit permission absent a stronger policy decision.
    pub requires_permission: bool,
    /// Tool aliases used for profile enablement lookup.
    pub aliases: Vec<String>,
    /// Owner-extracted policy operation and resources.
    pub operation: ToolPolicyOperation,
}

impl ToolPolicyAuthorizationMetadata {
    /// Project the owner operation to the legacy side-effect field still consumed by session and
    /// skill compatibility paths.
    #[must_use]
    pub const fn legacy_side_effect(&self) -> ToolSideEffect {
        match self.operation {
            ToolPolicyOperation::Command { .. } => ToolSideEffect::ExecuteProcess,
            ToolPolicyOperation::Write { .. } | ToolPolicyOperation::Mutating => {
                ToolSideEffect::WriteFiles
            }
            ToolPolicyOperation::Web { .. }
            | ToolPolicyOperation::Read { .. }
            | ToolPolicyOperation::ReadOnly => ToolSideEffect::ReadOnly,
        }
    }

    /// Project aliases and category to legacy skill-policy metadata.
    #[must_use]
    pub fn legacy_policy_metadata(&self) -> bcode_tool::ToolPolicyMetadata {
        bcode_tool::ToolPolicyMetadata {
            aliases: self.aliases.clone(),
            permission_category: match &self.operation {
                ToolPolicyOperation::Command { .. } => Some("command".to_string()),
                ToolPolicyOperation::Web { .. } => Some("web".to_string()),
                ToolPolicyOperation::Read { .. } => Some("read".to_string()),
                ToolPolicyOperation::Write { category, .. } => Some(category.clone()),
                ToolPolicyOperation::ReadOnly | ToolPolicyOperation::Mutating => None,
            },
            ..bcode_tool::ToolPolicyMetadata::default()
        }
    }
}

fn extracted_argument(
    request: &ToolPreparationRequest,
    definition: &ToolDefinition,
    kind: bcode_tool::ToolArgumentKind,
) -> Option<String> {
    definition
        .policy
        .argument_extractors
        .iter()
        .filter(|extractor| extractor.kind == kind)
        .find_map(|extractor| {
            request
                .invocation
                .arguments
                .get(&extractor.argument)
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        })
}

fn extracted_paths(
    request: &ToolPreparationRequest,
    definition: &ToolDefinition,
    kind: bcode_tool::ToolArgumentKind,
) -> Vec<String> {
    definition
        .policy
        .argument_extractors
        .iter()
        .filter(|extractor| extractor.kind == kind)
        .flat_map(|extractor| {
            let value = request.invocation.arguments.get(&extractor.argument);
            value
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
                .into_iter()
                .chain(
                    value
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|entry| {
                            entry.as_str().map(ToString::to_string).or_else(|| {
                                entry
                                    .get("path")
                                    .and_then(serde_json::Value::as_str)
                                    .map(ToString::to_string)
                            })
                        }),
                )
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn prepared_policy_operation(
    request: &ToolPreparationRequest,
    definition: &ToolDefinition,
) -> ToolPolicyOperation {
    let extractors = &definition.policy.argument_extractors;
    let command = extracted_argument(request, definition, bcode_tool::ToolArgumentKind::Command);
    if extractors
        .iter()
        .any(|extractor| extractor.kind == bcode_tool::ToolArgumentKind::Command)
    {
        return ToolPolicyOperation::Command { command };
    }
    let url = extracted_argument(request, definition, bcode_tool::ToolArgumentKind::Url);
    if url.is_some() {
        return ToolPolicyOperation::Web { url };
    }
    let write_paths = extracted_paths(request, definition, bcode_tool::ToolArgumentKind::WritePath);
    if !write_paths.is_empty() {
        return ToolPolicyOperation::Write {
            paths: write_paths,
            category: definition
                .policy
                .permission_category
                .clone()
                .unwrap_or_else(|| "write".to_string()),
        };
    }
    let read_paths = extracted_paths(request, definition, bcode_tool::ToolArgumentKind::ReadPath);
    if !read_paths.is_empty() {
        return ToolPolicyOperation::Read { paths: read_paths };
    }
    if extractors
        .iter()
        .any(|extractor| extractor.kind == bcode_tool::ToolArgumentKind::Url)
    {
        return ToolPolicyOperation::Web { url: None };
    }
    match definition.side_effect {
        ToolSideEffect::ReadOnly => ToolPolicyOperation::ReadOnly,
        ToolSideEffect::WriteFiles | ToolSideEffect::ExecuteProcess => {
            ToolPolicyOperation::Mutating
        }
    }
}

/// Prepare the standard agent-policy fact for one owner-supplied tool definition.
///
/// # Errors
///
/// Returns an error when the requested tool name does not match `definition` or the fact metadata
/// cannot be encoded.
pub fn prepare_tool_policy(
    request: &ToolPreparationRequest,
    definition: &ToolDefinition,
) -> Result<ToolPreparationResponse, String> {
    if request.invocation.tool_name != definition.name {
        return Err(format!(
            "tool not found during preparation: {}",
            request.invocation.tool_name
        ));
    }
    let aliases = std::iter::once(definition.name.clone())
        .chain(definition.policy.permission_category.iter().cloned())
        .chain(definition.policy.aliases.iter().cloned())
        .collect();
    let operation = prepared_policy_operation(request, definition);
    let metadata = ToolPolicyAuthorizationMetadata {
        requires_permission: definition.requires_permission,
        aliases,
        operation,
    };
    Ok(ToolPreparationResponse {
        authorization: vec![ToolAuthorizationFact {
            namespace: TOOL_POLICY_AUTHORIZATION_NAMESPACE.to_string(),
            schema_version: TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION,
            action: TOOL_POLICY_AUTHORIZATION_ACTION_INVOKE.to_string(),
            resource: Some(definition.name.clone()),
            metadata: serde_json::to_value(metadata).map_err(|error| error.to_string())?,
        }],
        descriptor: serde_json::Value::Null,
    })
}

/// Decode and validate the standard owner-produced agent policy fact.
///
/// # Errors
///
/// Returns an error when the fact is missing, duplicated, malformed, or names a different tool.
pub fn tool_policy_authorization_metadata(
    facts: &[ToolAuthorizationFact],
    tool_name: &str,
) -> Result<ToolPolicyAuthorizationMetadata, String> {
    let mut matching = facts.iter().filter(|fact| {
        fact.namespace == TOOL_POLICY_AUTHORIZATION_NAMESPACE
            && fact.schema_version == TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION
            && fact.action == TOOL_POLICY_AUTHORIZATION_ACTION_INVOKE
    });
    let fact = matching
        .next()
        .ok_or_else(|| "tool owner omitted the standard policy authorization fact".to_string())?;
    if matching.next().is_some() {
        return Err("tool owner emitted duplicate standard policy authorization facts".to_string());
    }
    if fact.resource.as_deref() != Some(tool_name) {
        return Err(
            "authorization fact resource does not match the correlated tool call".to_string(),
        );
    }
    serde_json::from_value(fact.metadata.clone())
        .map_err(|error| format!("invalid standard policy authorization fact: {error}"))
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
    /// Tool-owner-produced operation and resources.
    pub operation: ToolPolicyOperation,
    /// Tool aliases used for profile enablement lookup.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Whether the tool owner requires explicit permission absent a stronger decision.
    #[serde(default)]
    pub requires_permission: bool,
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
