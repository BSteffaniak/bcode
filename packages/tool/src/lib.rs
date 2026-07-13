#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model-callable tool contract types for Bcode.

pub mod interaction;

pub use interaction::{
    InteractionControlId, InteractionController, InteractionInput, InteractionNavigation,
    InteractionOutput, InteractionValue,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Plugin service interface for model-callable tools.
pub const TOOL_SERVICE_INTERFACE_ID: &str = "bcode.tool/v1";

/// Operation for listing tools provided by a plugin.
pub const OP_LIST_TOOLS: &str = "list_tools";

/// Operation for invoking a tool.
pub const OP_INVOKE_TOOL: &str = "invoke_tool";

/// Operation for resuming a suspended interactive tool invocation.
pub const OP_RESUME_INTERACTIVE_TOOL: &str = "resume_interactive_tool";

/// List tools request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListToolsRequest {}

/// List tools response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolList {
    pub tools: Vec<ToolDefinition>,
}

/// Model-callable tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub side_effect: ToolSideEffect,
    #[serde(default)]
    pub requires_permission: bool,
    #[serde(default)]
    pub policy: ToolPolicyMetadata,
    #[serde(default)]
    pub ui: ToolUiMetadata,
}

/// Plugin-owned policy metadata for a model-callable tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicyMetadata {
    /// Backward-compatible or user-facing aliases that may enable this tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Source-ecosystem aliases this tool provider declares it can satisfy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compatibility_aliases: Vec<ToolCompatibilityAlias>,
    /// Declarative capabilities this tool provides for policy matching.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Permission category used by policy providers for grouped rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_category: Option<String>,
    /// Argument paths that policy providers may inspect without knowing tool-specific schemas.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argument_extractors: Vec<ToolArgumentExtractor>,
}

/// Compatibility alias declared by the tool provider that owns the real tool.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ToolCompatibilityAlias {
    /// Source ecosystem that defined the alias, such as `claude` or `opencode`.
    pub ecosystem: String,
    /// Tool name used by that ecosystem.
    pub name: String,
}

impl ToolCompatibilityAlias {
    /// Create a compatibility alias.
    #[must_use]
    pub fn new(ecosystem: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            ecosystem: ecosystem.into(),
            name: name.into(),
        }
    }
}

/// Unresolved tool reference from a policy source such as skill frontmatter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnresolvedToolReference {
    /// Raw tool reference without source-ecosystem context.
    Raw { value: String },
    /// Compatibility alias scoped to a source ecosystem.
    CompatibilityAlias { ecosystem: String, name: String },
}

impl UnresolvedToolReference {
    /// Create a raw unresolved reference.
    #[must_use]
    pub fn raw(value: impl Into<String>) -> Self {
        Self::Raw {
            value: value.into(),
        }
    }

    /// Create an ecosystem-scoped compatibility alias.
    #[must_use]
    pub fn compatibility_alias(ecosystem: impl Into<String>, name: impl Into<String>) -> Self {
        Self::CompatibilityAlias {
            ecosystem: ecosystem.into(),
            name: name.into(),
        }
    }
}

/// Strict selector produced by resolving an unresolved tool reference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolvedToolSelector {
    /// Exact model-callable tool name.
    ToolName { name: String },
    /// Policy alias declared by exactly one tool provider.
    Alias { alias: String },
    /// Compatibility alias declared by exactly one tool provider.
    CompatibilityAlias { ecosystem: String, name: String },
    /// Permission category.
    PermissionCategory { category: String },
    /// Declarative tool capability.
    Capability { capability: String },
}

/// Candidate returned for ambiguous tool reference resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResolutionCandidate {
    /// Candidate model-callable tool name.
    pub tool_name: String,
    /// Human-readable reason this candidate matched.
    pub matched_by: String,
}

/// Resolution result for a tool reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolReferenceResolution {
    /// The reference resolved to a strict selector.
    Resolved { selector: ResolvedToolSelector },
    /// The reference matched multiple possible tools.
    Ambiguous {
        reference: UnresolvedToolReference,
        candidates: Vec<ToolResolutionCandidate>,
    },
    /// The reference did not match any known tool metadata.
    Unknown { reference: UnresolvedToolReference },
}

/// Resolve an unresolved tool reference against available tool definitions.
#[must_use]
pub fn resolve_tool_reference(
    reference: &UnresolvedToolReference,
    tools: &[ToolDefinition],
) -> ToolReferenceResolution {
    if let Some(selector) = explicit_selector(reference) {
        return ToolReferenceResolution::Resolved { selector };
    }

    let candidates = candidates_for_reference(reference, tools);
    match candidates.as_slice() {
        [] => ToolReferenceResolution::Unknown {
            reference: reference.clone(),
        },
        [candidate] => ToolReferenceResolution::Resolved {
            selector: selector_for_candidate(reference, candidate),
        },
        _ => ToolReferenceResolution::Ambiguous {
            reference: reference.clone(),
            candidates,
        },
    }
}

fn explicit_selector(reference: &UnresolvedToolReference) -> Option<ResolvedToolSelector> {
    let value = match reference {
        UnresolvedToolReference::Raw { value } => value,
        UnresolvedToolReference::CompatibilityAlias { .. } => return None,
    };
    value
        .strip_prefix("category:")
        .map(|category| ResolvedToolSelector::PermissionCategory {
            category: category.to_string(),
        })
        .or_else(|| {
            value
                .strip_prefix("capability:")
                .map(|capability| ResolvedToolSelector::Capability {
                    capability: capability.to_string(),
                })
        })
}

fn candidates_for_reference(
    reference: &UnresolvedToolReference,
    tools: &[ToolDefinition],
) -> Vec<ToolResolutionCandidate> {
    let mut candidates = BTreeMap::<String, ToolResolutionCandidate>::new();
    match reference {
        UnresolvedToolReference::Raw { value } => {
            for tool in tools {
                if tool.name == *value {
                    candidates.insert(
                        tool.name.clone(),
                        ToolResolutionCandidate {
                            tool_name: tool.name.clone(),
                            matched_by: "tool_name".to_string(),
                        },
                    );
                }
                if tool.policy.aliases.iter().any(|alias| alias == value) {
                    candidates.insert(
                        tool.name.clone(),
                        ToolResolutionCandidate {
                            tool_name: tool.name.clone(),
                            matched_by: format!("alias:{value}"),
                        },
                    );
                }
                if tool
                    .policy
                    .capabilities
                    .iter()
                    .any(|capability| capability == value)
                {
                    candidates.insert(
                        tool.name.clone(),
                        ToolResolutionCandidate {
                            tool_name: tool.name.clone(),
                            matched_by: format!("capability:{value}"),
                        },
                    );
                }
            }
        }
        UnresolvedToolReference::CompatibilityAlias { ecosystem, name } => {
            for tool in tools {
                if tool.policy.compatibility_aliases.iter().any(|alias| {
                    alias.ecosystem.eq_ignore_ascii_case(ecosystem) && alias.name == *name
                }) {
                    candidates.insert(
                        tool.name.clone(),
                        ToolResolutionCandidate {
                            tool_name: tool.name.clone(),
                            matched_by: format!("compatibility_alias:{ecosystem}:{name}"),
                        },
                    );
                }
            }
        }
    }
    candidates.into_values().collect()
}

fn selector_for_candidate(
    reference: &UnresolvedToolReference,
    candidate: &ToolResolutionCandidate,
) -> ResolvedToolSelector {
    match reference {
        UnresolvedToolReference::Raw { value } if candidate.matched_by == "tool_name" => {
            ResolvedToolSelector::ToolName {
                name: value.clone(),
            }
        }
        UnresolvedToolReference::Raw { value } if candidate.matched_by.starts_with("alias:") => {
            ResolvedToolSelector::Alias {
                alias: value.clone(),
            }
        }
        UnresolvedToolReference::Raw { value } => ResolvedToolSelector::Capability {
            capability: value.clone(),
        },
        UnresolvedToolReference::CompatibilityAlias { ecosystem, name } => {
            ResolvedToolSelector::CompatibilityAlias {
                ecosystem: ecosystem.clone(),
                name: name.clone(),
            }
        }
    }
}

/// Plugin-owned argument extraction metadata for policy providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArgumentExtractor {
    /// Logical argument kind.
    pub kind: ToolArgumentKind,
    /// Top-level JSON argument name to inspect.
    pub argument: String,
}

/// Logical argument kind used by policy providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolArgumentKind {
    Command,
    ReadPath,
    WritePath,
    Url,
}

/// Plugin-owned UI metadata for a model-callable tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUiMetadata {
    /// Short activity label suitable for progress/status displays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_label: Option<String>,
    /// Declarative request visual metadata for streamed and persisted tool arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_visual: Option<ToolPluginVisualMetadata>,
}

/// A generic argument field selector for plugin-owned presentation payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolVisualPayloadSelector {
    /// Candidate top-level JSON argument names, in priority order.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Literal fallback value when no field is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<serde_json::Value>,
    /// Whether this selector must resolve before the payload can be emitted.
    #[serde(default)]
    pub required: bool,
}

/// Plugin-owned presentation descriptor metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPluginVisualMetadata {
    /// Producer-owned schema identifier.
    pub schema: String,
    /// Producer-owned schema version.
    pub schema_version: u32,
    /// Producer plugin id for adapter routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_plugin_id: Option<String>,
    /// Optional human-readable fallback title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable fallback subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Payload keys mapped to tool argument fields/literals.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub payload: std::collections::BTreeMap<String, ToolVisualPayloadSelector>,
}

/// Side-effect category for a model-callable tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSideEffect {
    #[default]
    ReadOnly,
    WriteFiles,
    ExecuteProcess,
}

/// Versioned opaque action routed to an active plugin-owned invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInvocationAction {
    /// Plugin that owns interpretation of this action.
    pub producer_plugin_id: String,
    /// Plugin-owned action payload schema.
    pub schema: String,
    /// Version of the plugin-owned payload schema.
    pub schema_version: u32,
    /// Opaque plugin-owned payload.
    pub payload: serde_json::Value,
}

/// Tool invocation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationRequest {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Canonical session working directory for this invocation.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Host-managed artifact directory for this invocation/session.
    #[serde(default)]
    pub artifact_dir: Option<PathBuf>,
    /// Optional host-managed cancellation file. Tools should stop work when this path exists.
    #[serde(default)]
    pub cancellation_path: Option<PathBuf>,
    /// Optional host-managed append-only control stream for an active invocation.
    #[serde(default)]
    pub invocation_action_path: Option<PathBuf>,
}

/// Plugin-owned visual update emitted while a tool invocation is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStreamVisualUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_id: Option<String>,
    pub producer_plugin_id: Option<String>,
    pub schema: String,
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub payload: serde_json::Value,
}

/// Incremental event emitted while a tool invocation is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInvocationStreamEvent {
    /// Tool execution has started inside the provider plugin.
    Started {
        tool_call_id: String,
        tool_name: String,
        #[serde(default)]
        sequence: u64,
        #[serde(default)]
        terminal: bool,
        #[serde(default)]
        columns: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        started_at_ms: Option<u64>,
    },
    /// A chunk of live tool output is available.
    OutputDelta {
        tool_call_id: String,
        stream: ToolOutputStream,
        sequence: u64,
        text: String,
        #[serde(default)]
        byte_len: usize,
    },
    /// Plugin-owned visual update for transcript rendering.
    VisualUpdate {
        tool_call_id: String,
        sequence: u64,
        visual: ToolStreamVisualUpdate,
        #[serde(default)]
        streaming: bool,
    },
    /// A user-visible status line or progress update.
    Status {
        tool_call_id: String,
        #[serde(default)]
        sequence: u64,
        message: String,
    },
    /// Tool has finished; full result follows through normal invoke response.
    Finished {
        tool_call_id: String,
        #[serde(default)]
        sequence: u64,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        finished_at_ms: Option<u64>,
    },
}

/// Logical output stream for an incremental tool output chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
    Pty,
}

/// Optional stream event sink callback supplied by the host for long-running tools.
pub type ToolStreamEventSink<'a> =
    &'a mut dyn FnMut(ToolInvocationStreamEvent) -> Result<(), String>;

/// Tool invocation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationResponse {
    pub output: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolResultContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_action: Option<ToolInvocationHostAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolInvocationResult>,
}

/// Opaque artifact produced by a tool plugin and rendered by visual adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArtifact {
    pub artifact_id: String,
    pub producer_plugin_id: String,
    pub schema: String,
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<ToolArtifactRef>,
}

/// Reference to plugin-owned artifact bytes or structured sidecar data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArtifactRef {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Host action requested by a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInvocationHostAction {
    HostModelNativeWebSearch(HostModelNativeWebSearchRequest),
    InteractiveToolRequest(InteractiveToolRequest),
}

/// Generic request for host-owned interactive tool UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractiveToolRequest {
    pub interaction_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_kind: Option<String>,
    pub surface_kind: String,
    #[serde(default)]
    pub request: serde_json::Value,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub turn_behavior: InteractiveToolTurnBehavior,
    #[serde(default)]
    pub render_target: InteractiveToolRenderTarget,
}

/// How an interactive tool request affects the active model turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractiveToolTurnBehavior {
    #[default]
    AwaitBeforeContinuing,
    CompleteTurnWithPendingInteraction,
}

/// Core-understood resolution for an interactive tool request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractiveToolResolution {
    Submitted {
        payload: serde_json::Value,
    },
    Aborted {
        reason: InteractiveToolAbortReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// Infrastructure-level reason an interactive tool request could not be submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractiveToolAbortReason {
    UserDismissed,
    TurnCancelled,
    ClientDetached,
    Timeout,
    UnsupportedSurface,
    HostError,
}

/// Request payload for resuming a suspended interactive tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractiveToolResumeRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub interaction_id: String,
    pub original_arguments: serde_json::Value,
    pub interactive_request: InteractiveToolRequest,
    pub resolution: InteractiveToolResolution,
}

/// Host render target for an interactive tool request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractiveToolRenderTarget {
    #[default]
    TranscriptToolCall,
}

/// Host-side model-native web search request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostModelNativeWebSearchRequest {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_search: Option<String>,
}

/// Semantic tool result values that UI layers can render without parsing text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInvocationResult {
    Text { text: String },
    Json { value: String },
    Artifact { artifact: Box<ToolArtifact> },
}

/// Structured model-visible tool result content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text { text: String },
    Image { image: ImageContent },
    ImageRef { image: ImageRefContent },
}

/// Model-visible image reference returned by a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRefContent {
    pub path: String,
    pub mime_type: String,
    #[serde(default)]
    pub metadata: ImageMetadata,
}

/// Model-visible image content returned by a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    pub mime_type: String,
    pub data_base64: String,
    #[serde(default)]
    pub metadata: ImageMetadata,
}

/// Image metadata useful for diagnostics and transcript display.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMetadata {
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub byte_len: Option<u64>,
    #[serde(default)]
    pub source_path: Option<String>,
}
