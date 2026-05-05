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
    PromptCaching,
    ConversationReuse,
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
    PromptCaching,
}

/// User-facing thinking / reasoning effort level for models that support it.
/// Maps to provider-specific parameters (e.g. `reasoning_effort` or budget).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// Provider configuration validation request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateConfigRequest {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

/// Provider-neutral request context resolved by the host from model/provider profiles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequestContext {
    #[serde(default)]
    pub model_profile: Option<String>,
    #[serde(default)]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
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
    /// Selected model ID. Empty means the provider should use its configured default.
    pub model_id: String,
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    #[serde(default)]
    pub system_prompt: Option<String>,
    pub messages: Vec<ModelMessage>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub parameters: ModelParameters,
    #[serde(default)]
    pub prompt_cache: PromptCacheHints,
    #[serde(default)]
    pub conversation_reuse: ConversationReuseHints,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider-neutral conversation reuse hints for provider-native continuation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationReuseHints {
    /// Provider-native conversation reuse mode selected by the host.
    #[serde(default)]
    pub mode: ConversationReuseMode,
    /// Stable key for the reusable provider conversation state.
    #[serde(default)]
    pub key: Option<String>,
    /// Provider response/turn ID to continue from, when available.
    #[serde(default)]
    pub previous_provider_response_id: Option<String>,
    /// First message index not covered by `previous_provider_response_id`.
    #[serde(default)]
    pub new_messages_start_index: Option<usize>,
}

/// Provider-native conversation reuse policy level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationReuseMode {
    #[default]
    Off,
    Auto,
}

impl ConversationReuseMode {
    /// Return whether provider-native continuation may be used.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Provider-neutral prompt cache hints for a model turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheHints {
    /// Prompt cache mode selected by the host.
    #[serde(default)]
    pub mode: PromptCacheMode,
    /// Whether the stable system prompt prefix should end with a provider cache point.
    #[serde(default)]
    pub cache_system_prompt: bool,
    /// Whether provider tool definitions should end with a provider cache point.
    #[serde(default)]
    pub cache_tools: bool,
}

/// Prompt caching policy level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    Off,
    #[default]
    Auto,
    Aggressive,
}

impl PromptCacheMode {
    /// Return whether provider cache hints should be emitted.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Return whether conversation-prefix cache points should be emitted.
    #[must_use]
    pub const fn cache_conversation_prefix(self) -> bool {
        matches!(self, Self::Aggressive)
    }
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
    Text {
        text: String,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        result: ToolResult,
    },
    /// Provider-neutral cache point hint. Providers that do not support explicit prompt caching should ignore it.
    CachePoint {
        hint: PromptCachePoint,
    },
    ProviderExtension {
        value: serde_json::Value,
    },
}

/// Provider-neutral prompt cache point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCachePoint {
    /// Optional provider-neutral label for diagnostics.
    #[serde(default)]
    pub label: Option<String>,
    /// Optional provider-neutral TTL in seconds.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
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
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
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
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStarted {
        call_id: String,
        name: String,
    },
    ToolCallDelta {
        call_id: String,
        delta: String,
    },
    ToolCallFinished {
        call: ToolCall,
    },
    Usage {
        usage: TokenUsage,
    },
    /// Provider-specific metadata that the host may use for invisible optimization state.
    ProviderMetadata {
        key: String,
        value: String,
    },
    Warning {
        message: String,
    },
    Error {
        error: ProviderError,
    },
    TurnFinished {
        stop_reason: StopReason,
    },
    Cancelled,
}

/// Provider-neutral token usage metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens supplied to the model for this turn or provider round.
    #[serde(default)]
    pub input_tokens: Option<u32>,
    /// Tokens generated by the model for this turn or provider round.
    #[serde(default)]
    pub output_tokens: Option<u32>,
    /// Provider-reported total tokens, when available.
    #[serde(default)]
    pub total_tokens: Option<u32>,
    /// Input tokens served from a provider cache, when available.
    #[serde(default)]
    pub cached_input_tokens: Option<u32>,
    /// Input tokens written to a provider prompt cache, when available.
    #[serde(default)]
    pub cache_write_input_tokens: Option<u32>,
    /// Reasoning tokens reported separately by a provider, when available.
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl TokenUsage {
    /// Return the most reliable total token count for spend/session metering.
    ///
    /// Uses the provider-reported total when present; otherwise sums input and
    /// output tokens. Cached and reasoning token fields are metadata and are not
    /// added separately because providers commonly include them in input/output
    /// totals.
    #[must_use]
    pub fn metered_total_tokens(&self) -> Option<u32> {
        self.total_tokens.or_else(|| {
            let input = self.input_tokens.unwrap_or_default();
            let output = self.output_tokens.unwrap_or_default();
            (self.input_tokens.is_some() || self.output_tokens.is_some())
                .then_some(input.saturating_add(output))
        })
    }

    /// Return the token count that best represents current context pressure.
    #[must_use]
    pub const fn context_input_tokens(&self) -> Option<u32> {
        self.input_tokens
    }

    /// Return uncached input tokens when both input and cached counts are known.
    #[must_use]
    pub const fn uncached_input_tokens(&self) -> Option<u32> {
        match (self.input_tokens, self.cached_input_tokens) {
            (Some(input), Some(cached)) => Some(input.saturating_sub(cached)),
            _ => self.input_tokens,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::TokenUsage;

    #[test]
    fn token_usage_prefers_provider_reported_total() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(20),
            ..TokenUsage::default()
        };

        assert_eq!(usage.metered_total_tokens(), Some(20));
    }

    #[test]
    fn token_usage_falls_back_to_input_plus_output() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..TokenUsage::default()
        };

        assert_eq!(usage.metered_total_tokens(), Some(15));
    }
}
