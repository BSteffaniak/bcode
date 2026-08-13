#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model-callable tool contract types for Bcode.

pub mod contracts;
pub mod interaction;

pub use bcode_tool_models::{
    ToolContributionArtifact, ToolContributionEnvelope, ToolContributionEvent,
    ToolContributionOperation, ToolContributionPersistence, ToolContributionPlacement,
    ToolExchangeRequest, ToolExchangeResolution, ToolExchangeResolutionEvent,
    ToolExchangeResponsePolicy, ToolInvocationInput, ToolInvocationInputResolution,
    ToolInvocationLifecycleEvent, ToolInvocationLifecycleStage, ToolPresentationIdentity,
    ToolPresentationRetention, ToolPresentationScopeState, ToolPresentationUpdate,
    ToolPresentationUpdateError, ToolPresentationUpdateScope,
};
pub use contracts::{
    PreparedToolInvocation, TOOL_ARTIFACT_CONTEXT_SCHEMA, TOOL_ARTIFACT_CONTEXT_SCHEMA_VERSION,
    TOOL_INVOCATION_SERVICE_ROUTES_SCHEMA, TOOL_WORKSPACE_CONTEXT_SCHEMA,
    TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION, ToolArtifactWriteRequest, ToolArtifactWriteResolution,
    ToolAuthorizationFact, ToolExecutionOptions, ToolHostContextEntry, ToolInvocationDescriptor,
    ToolInvocationServiceRequest, ToolInvocationServiceResolution, ToolInvocationServiceRoute,
    ToolPreparationRequest, ToolPreparationResponse,
};

pub use interaction::{
    InteractionControlId, InteractionController, InteractionInput, InteractionNavigation,
    InteractionOutput, InteractionValue,
};
use serde::{Deserialize, Serialize};

/// Plugin service interface for model-callable tools.
pub const TOOL_SERVICE_INTERFACE_ID: &str = "bcode.tool/v1";

/// Operation for listing tools provided by a plugin.
pub const OP_LIST_TOOLS: &str = "list_tools";

/// Operation for preparing a tool without performing side effects.
pub const OP_PREPARE_TOOL: &str = "prepare_tool";

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

/// Tool invocation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationRequest {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Opaque tool-owner-produced descriptor returned by preparation.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub preparation_descriptor: serde_json::Value,
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_key: Option<String>,
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
