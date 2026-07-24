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
pub const TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION: u32 = 2;

/// Action for invoking a tool under the standard agent policy authorization fact.
pub const TOOL_POLICY_AUTHORIZATION_ACTION_INVOKE: &str = "invoke";

/// Tool-owner-produced agent policy operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolPolicyOperation {
    /// Evaluate shell command policy using owner-produced structured analysis.
    Command {
        /// Original command retained for diagnostics and schema compatibility checks.
        command: Option<String>,
        /// Complete or explicitly incomplete analysis produced by the shell owner.
        analysis: Option<bcode_shell_command_analysis_models::ShellAnalysis>,
        /// Fatal owner-side analysis failure. Exactly one of `analysis` or `analysis_error` is
        /// expected for a well-formed shell authorization fact.
        analysis_error: Option<bcode_shell_command_analysis_models::ShellAnalysisError>,
    },
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

/// Tool-owner-produced policy identity used for selector matching.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicyIdentity {
    /// Backward-compatible or user-facing aliases that may enable this tool.
    pub aliases: Vec<String>,
    /// Source-ecosystem aliases this tool provider declares it can satisfy.
    pub compatibility_aliases: Vec<bcode_tool::ToolCompatibilityAlias>,
    /// Declarative capabilities this tool provides for policy matching.
    pub capabilities: Vec<String>,
    /// Permission category used by policy providers for grouped rules.
    pub permission_category: Option<String>,
}

/// Tool-owner-produced preparation policy encoded into the standard authorization fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicyPreparation {
    /// Whether this invocation requires explicit permission absent a stronger policy decision.
    pub requires_permission: bool,
    /// Owner-declared policy identity.
    pub identity: ToolPolicyIdentity,
    /// Owner-extracted operation and resources.
    pub operation: ToolPolicyOperation,
}

impl ToolPolicyPreparation {
    /// Create an owner preparation policy with no aliases, capabilities, or permission category.
    #[must_use]
    pub fn new(requires_permission: bool, operation: ToolPolicyOperation) -> Self {
        Self {
            requires_permission,
            identity: ToolPolicyIdentity::default(),
            operation,
        }
    }

    /// Attach owner-declared policy identity.
    #[must_use]
    pub fn with_identity(mut self, identity: ToolPolicyIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Create an explicitly permission-free read-only policy.
    #[must_use]
    pub fn read_only() -> Self {
        Self::new(false, ToolPolicyOperation::ReadOnly)
    }
}

/// Tool-owner-produced metadata consumed only by the agent policy adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicyAuthorizationMetadata {
    /// Whether the owner requires explicit permission absent a stronger policy decision.
    pub requires_permission: bool,
    /// Tool aliases used for policy selector matching.
    pub aliases: Vec<String>,
    /// Source-ecosystem aliases used for policy selector matching.
    pub compatibility_aliases: Vec<bcode_tool::ToolCompatibilityAlias>,
    /// Owner-declared capabilities used for policy selector matching.
    pub capabilities: Vec<String>,
    /// Owner-declared permission category.
    pub permission_category: Option<String>,
    /// Owner-extracted policy operation and resources.
    pub operation: ToolPolicyOperation,
}

impl ToolPolicyAuthorizationMetadata {
    /// Return whether this operation is read-only.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        matches!(
            self.operation,
            ToolPolicyOperation::Web { .. }
                | ToolPolicyOperation::Read { .. }
                | ToolPolicyOperation::ReadOnly
        )
    }

    /// Return a stable operation class for diagnostics and tracing.
    #[must_use]
    pub const fn operation_name(&self) -> &'static str {
        match self.operation {
            ToolPolicyOperation::Command { .. } => "command",
            ToolPolicyOperation::Web { .. } => "web",
            ToolPolicyOperation::Read { .. } => "read",
            ToolPolicyOperation::Write { .. } => "write",
            ToolPolicyOperation::ReadOnly => "read_only",
            ToolPolicyOperation::Mutating => "mutating",
        }
    }
}

/// Prepare authorization using a tool-owner-supplied operation and identity.
///
/// This helper centralizes the standard authorization envelope while leaving domain extraction and
/// analysis entirely with the tool owner.
///
/// # Errors
///
/// Returns an error when the requested tool name does not match `definition` or the authorization
/// metadata cannot be encoded.
pub fn prepare_tool_policy_with_operation(
    request: &ToolPreparationRequest,
    definition: &ToolDefinition,
    requires_permission: bool,
    identity: ToolPolicyIdentity,
    operation: ToolPolicyOperation,
) -> Result<ToolPreparationResponse, String> {
    prepare_tool_policy(
        request,
        definition,
        ToolPolicyPreparation {
            requires_permission,
            identity,
            operation,
        },
    )
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
    preparation: ToolPolicyPreparation,
) -> Result<ToolPreparationResponse, String> {
    if request.invocation.tool_name != definition.name {
        return Err(format!(
            "tool not found during preparation: {}",
            request.invocation.tool_name
        ));
    }
    let metadata = ToolPolicyAuthorizationMetadata {
        requires_permission: preparation.requires_permission,
        aliases: preparation.identity.aliases,
        compatibility_aliases: preparation.identity.compatibility_aliases,
        capabilities: preparation.identity.capabilities,
        permission_category: preparation.identity.permission_category,
        operation: preparation.operation,
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
    let mut namespace_facts = facts
        .iter()
        .filter(|fact| fact.namespace == TOOL_POLICY_AUTHORIZATION_NAMESPACE);
    let first = namespace_facts
        .next()
        .ok_or_else(|| "tool owner omitted the standard policy authorization fact".to_string())?;
    if namespace_facts.next().is_some() {
        return Err("tool owner emitted duplicate standard policy authorization facts".to_string());
    }
    if first.schema_version != TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION {
        return Err(format!(
            "unsupported standard policy authorization schema version {}; expected {}",
            first.schema_version, TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION
        ));
    }
    if first.action != TOOL_POLICY_AUTHORIZATION_ACTION_INVOKE {
        return Err("standard policy authorization fact has an unsupported action".to_string());
    }
    let fact = first;
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
