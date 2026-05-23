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
}

/// Incremental event emitted while a tool invocation is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInvocationStreamEvent {
    /// Tool execution has started inside the provider plugin.
    Started {
        tool_call_id: String,
        tool_name: String,
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
    /// Human-readable progress status from a long-running tool.
    Status {
        tool_call_id: String,
        sequence: u64,
        message: String,
    },
    /// Tool execution has finished inside the provider plugin.
    Finished {
        tool_call_id: String,
        sequence: u64,
        is_error: bool,
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

/// Tool invocation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationResponse {
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub content: Vec<ToolResultContent>,
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
