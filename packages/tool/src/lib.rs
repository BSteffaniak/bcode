#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model-callable tool contract types for Bcode.

use serde::{Deserialize, Serialize};
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
    /// Permission category used by policy providers for grouped rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_category: Option<String>,
    /// Argument paths that policy providers may inspect without knowing tool-specific schemas.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argument_extractors: Vec<ToolArgumentExtractor>,
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
    pub presentation: Option<ToolInvocationPresentation>,
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

/// Bounded UI presentation metadata returned by a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationPresentation {
    /// Pseudo-terminal execution result.
    Terminal {
        exit_code: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        output: String,
        output_truncated: bool,
        output_bytes: Option<u64>,
        retained_output_bytes: Option<u64>,
        columns: u16,
        rows: u16,
    },
    /// Filesystem write/edit result.
    FileChange {
        tool_name: String,
        summary: String,
        path: Option<String>,
    },
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
