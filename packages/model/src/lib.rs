#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model provider service contract types for Bcode.

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Plugin service interface for model providers.
pub const MODEL_PROVIDER_INTERFACE_ID: &str = "bcode.model-provider/v1";

/// Operation for provider capability discovery.
pub const OP_CAPABILITIES: &str = "capabilities";

/// Operation for model listing.
pub const OP_MODELS: &str = "models";

/// Operation for validating provider configuration.
pub const OP_VALIDATE_CONFIG: &str = "validate_config";

/// Operation for starting a model turn.
pub const OP_START_TURN: &str = "start_turn";

/// Operation for polling model turn stream events.
pub const OP_POLL_TURN_EVENTS: &str = "poll_turn_events";

/// Operation for cancelling a model turn.
pub const OP_CANCEL_TURN: &str = "cancel_turn";

/// Operation for provider turn cleanup.
pub const OP_FINISH_TURN: &str = "finish_turn";

/// Provider-level capability report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub provider_id: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: BTreeSet<ProviderCapability>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider-level capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    Streaming,
    Tools,
    Cancellation,
    JsonMode,
}

/// Model listing response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelList {
    pub models: Vec<ModelInfo>,
}

/// Model metadata exposed by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model_id: String,
    pub display_name: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub capabilities: BTreeSet<ModelCapability>,
}

/// Per-model capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    StreamingText,
    ToolCalls,
    ParallelToolCalls,
    JsonMode,
    Reasoning,
    ImageInput,
}

/// Provider configuration validation request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateConfigRequest {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

/// Provider configuration validation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateConfigResponse {
    pub valid: bool,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Start a provider model turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnRequest {
    pub session_id: SessionId,
    pub turn_id: String,
    pub model_id: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
    pub messages: Vec<ModelMessage>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub parameters: ModelParameters,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider response after starting a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartTurnResponse {
    pub provider_turn_id: String,
}

/// Poll queued provider turn events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollTurnEventsRequest {
    pub provider_turn_id: String,
}

/// Provider turn event batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollTurnEventsResponse {
    pub events: Vec<ProviderTurnEvent>,
}

/// Cancel an active provider turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelTurnRequest {
    pub provider_turn_id: String,
}

/// Finish or clean up a provider turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishTurnRequest {
    pub provider_turn_id: String,
}

/// Empty acknowledgement response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckResponse {}

/// Model message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Provider-neutral content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolCall { call: ToolCall },
    ToolResult { result: ToolResult },
    ProviderExtension { value: serde_json::Value },
}

/// Model parameters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelParameters {
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub reasoning_budget_tokens: Option<u32>,
}

/// Tool definition supplied to a provider.
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

/// Tool call emitted by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Tool result supplied back to a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
}

/// Normalized provider stream event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderTurnEvent {
    TurnStarted,
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallStarted { call_id: String, name: String },
    ToolCallDelta { call_id: String, delta: String },
    ToolCallFinished { call: ToolCall },
    Usage { usage: TokenUsage },
    Warning { message: String },
    Error { error: ProviderError },
    TurnFinished { stop_reason: StopReason },
    Cancelled,
}

/// Token usage metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    pub output_tokens: Option<u32>,
    #[serde(default)]
    pub cached_input_tokens: Option<u32>,
}

/// Provider turn stop reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolCall,
    MaxTokens,
    StopSequence,
    Cancelled,
    Error,
}

/// Structured provider error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub code: String,
    pub category: ProviderErrorCategory,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub provider_message: Option<String>,
}

/// Provider error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorCategory {
    Config,
    Auth,
    RateLimit,
    Network,
    Timeout,
    ModelNotFound,
    ContextLength,
    InvalidRequest,
    UnsupportedFeature,
    ProviderInternal,
    Cancelled,
}
