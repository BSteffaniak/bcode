#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model-callable tool contract types for Bcode.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Plugin service interface for model-callable tools.
pub const TOOL_SERVICE_INTERFACE_ID: &str = "bcode.tool/v1";

/// Operation for listing tools provided by a plugin.
pub const OP_LIST_TOOLS: &str = "list_tools";

/// Operation for invoking a tool.
pub const OP_INVOKE_TOOL: &str = "invoke_tool";

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
    /// Declarative request presentation metadata for permission prompts and transcripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_presentation: Option<ToolRequestPresentationMetadata>,
    /// Declarative live argument preview metadata for streamed tool arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_argument_preview: Option<ToolLiveArgumentPreviewMetadata>,
}

/// Declarative live argument preview metadata owned by a tool provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolLiveArgumentPreviewMetadata {
    /// File edit/write style preview.
    FileEdit {
        /// Candidate path fields.
        #[serde(default)]
        path_fields: Vec<String>,
        /// Candidate old-text fields.
        #[serde(default)]
        old_text_fields: Vec<String>,
        /// Candidate new-text/content fields.
        new_text_fields: Vec<String>,
        /// Plugin-owned live preview title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview_title: Option<String>,
        /// Plugin-owned streaming status template.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        streaming_status: Option<String>,
    },
    /// Shell command style preview.
    ShellCommand {
        /// Command field.
        command_field: String,
        /// Optional cwd field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd_field: Option<String>,
        /// Plugin-owned live preview title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview_title: Option<String>,
        /// Plugin-owned streaming status template.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        streaming_status: Option<String>,
    },
    /// Query/key-value style preview.
    Query {
        /// Candidate display fields.
        fields: Vec<String>,
        /// Plugin-owned live preview title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview_title: Option<String>,
        /// Plugin-owned streaming status template.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        streaming_status: Option<String>,
    },
}

/// Declarative request presentation metadata owned by a tool provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequestPresentationMetadata {
    /// Human-readable request title.
    pub title: String,
    /// Ordered argument fields that should be shown in request summaries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ToolPresentationField>,
}

/// Declarative presentation metadata for one request argument field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPresentationField {
    /// Human-readable field label.
    pub label: String,
    /// Top-level JSON argument name to display.
    pub argument: String,
    /// Field rendering hint for generic UI presentation.
    pub kind: ToolPresentationFieldKind,
    /// Whether the field may be omitted from the request arguments.
    #[serde(default)]
    pub optional: bool,
}

/// Generic UI presentation hint for request argument fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentationFieldKind {
    /// Plain text value.
    Text,
    /// File or directory path value.
    Path,
    /// URL value.
    Url,
    /// Shell or process command value.
    Command,
    /// Boolean value.
    Boolean,
    /// Integer count or limit value.
    Count,
    /// Millisecond duration value.
    DurationMs,
    /// JSON value with no more specific semantic hint.
    Json,
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
    /// A user-visible status line or progress update.
    Status {
        tool_call_id: String,
        #[serde(default)]
        sequence: u64,
        message: String,
    },
    /// Plugin-owned presentation update.
    Presentation {
        tool_call_id: String,
        #[serde(default)]
        sequence: u64,
        presentation: ToolPresentationEvent,
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

/// Plugin-owned presentation update for a running tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolPresentationEvent {
    /// Status text for an activity, preview, or result target.
    Status(ToolStatusPresentation),
    /// Card-style structured presentation.
    Card(ToolCardPresentation),
    /// Progress update.
    Progress(ToolProgressPresentation),
    /// Clear a previous presentation target.
    Clear { target: ToolPresentationTarget },
}

/// Tool presentation target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentationTarget {
    Activity,
    Preview,
    Result,
}

/// Presentation severity/level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentationLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// Tool status presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStatusPresentation {
    pub target: ToolPresentationTarget,
    pub text: String,
    #[serde(default = "default_presentation_level")]
    pub level: ToolPresentationLevel,
}

/// Tool progress presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProgressPresentation {
    pub target: ToolPresentationTarget,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(default = "default_presentation_level")]
    pub level: ToolPresentationLevel,
}

/// Tool card presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCardPresentation {
    pub target: ToolPresentationTarget,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ToolPresentationSection>,
}

/// Generic section in a tool presentation card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolPresentationSection {
    Text {
        label: Option<String>,
        text: String,
    },
    Fields {
        fields: Vec<ToolPresentationFieldValue>,
    },
    Diff {
        path: Option<String>,
        old_text: String,
        new_text: String,
    },
    Terminal {
        output: String,
        columns: u16,
        rows: u16,
    },
}

/// Label/value field for a presentation section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPresentationFieldValue {
    pub label: String,
    pub value: String,
}

const fn default_presentation_level() -> ToolPresentationLevel {
    ToolPresentationLevel::Info
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

/// Typed host action requested by a tool plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInvocationHostAction {
    HostModelNativeWebSearch(HostModelNativeWebSearchRequest),
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
    ShellRun { result: ShellRunResult },
    FileChange { result: FileChangeResult },
}

/// Semantic shell execution result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ShellRunResult {
    /// Terminal-backed execution with a single bounded output stream.
    Terminal {
        exit_code: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        output_tail: String,
        output_truncated: bool,
        output_bytes: Option<u64>,
        retained_output_bytes: Option<u64>,
        columns: u16,
        rows: u16,
    },
    /// Non-terminal execution with separately captured streams.
    Captured {
        exit_code: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
        stdout_bytes: Option<u64>,
        stderr_bytes: Option<u64>,
    },
}

/// Semantic filesystem change result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeResult {
    pub tool_name: String,
    pub summary: String,
    pub path: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::{
        ToolPresentationField, ToolPresentationFieldKind, ToolRequestPresentationMetadata,
        ToolUiMetadata,
    };

    #[test]
    fn request_presentation_metadata_round_trips() {
        let metadata = ToolUiMetadata {
            activity_label: Some("running".to_string()),
            live_argument_preview: None,
            request_presentation: Some(ToolRequestPresentationMetadata {
                title: "Run command".to_string(),
                fields: vec![ToolPresentationField {
                    label: "Command".to_string(),
                    argument: "command".to_string(),
                    kind: ToolPresentationFieldKind::Command,
                    optional: false,
                }],
            }),
        };

        let encoded = serde_json::to_string(&metadata).expect("metadata encodes");
        let decoded = serde_json::from_str::<ToolUiMetadata>(&encoded).expect("metadata decodes");

        assert_eq!(decoded, metadata);
    }
}
