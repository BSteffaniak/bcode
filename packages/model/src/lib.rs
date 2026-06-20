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

/// Operation for model verification.
pub const OP_VERIFY_MODEL: &str = "verify_model";

/// Operation for polling model turn stream events.
pub const OP_POLL_TURN_EVENTS: &str = "poll_turn_events";

/// Operation for cancelling a model turn.
pub const OP_CANCEL_TURN: &str = "cancel_turn";

/// Operation for provider-native web search.
pub const OP_NATIVE_WEB_SEARCH: &str = "native_web_search";

/// Operation for provider turn cleanup.
pub const OP_FINISH_TURN: &str = "finish_turn";

/// Provider-level capability report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub provider_id: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: BTreeSet<ProviderCapability>,
    /// Provider-supported auth scheme identifiers, for example `api_key` or `chatgpt`.
    #[serde(default)]
    pub auth_schemes: BTreeSet<String>,
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
    NativeWebSearch,
    CodeSearch,
}

/// Model listing request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelListRequest {
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    #[serde(default)]
    pub selected_model_id: Option<String>,
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
    #[serde(default)]
    pub reasoning: Option<ModelReasoningInfo>,
    #[serde(default)]
    pub cache: ModelCacheInfo,
    #[serde(default)]
    pub metadata_source: Option<ModelMetadataSource>,
    #[serde(default)]
    pub pricing: Option<ModelPricingInfo>,
    #[serde(default, skip)]
    pub visibility: ModelVisibility,
}

/// Model picker/list visibility metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVisibility {
    #[default]
    Visible,
    Ignored {
        source: ModelVisibilitySource,
        rule: String,
    },
    Unsupported {
        reason: String,
    },
}

/// Source of a model visibility decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVisibilitySource {
    Config,
    State,
    Both,
}

/// Model token pricing metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPricingInfo {
    /// ISO 4217 currency code, for example `USD`.
    pub currency: String,
    /// Pricing unit for each token bucket.
    pub unit: ModelPricingUnit,
    /// Price for uncached input tokens.
    #[serde(default)]
    pub input: Option<ModelTokenPrice>,
    /// Price for cached input tokens.
    #[serde(default)]
    pub cached_input: Option<ModelTokenPrice>,
    /// Price for input tokens written to an explicit prompt cache.
    #[serde(default)]
    pub cache_write_input: Option<ModelTokenPrice>,
    /// Price for generated output tokens.
    #[serde(default)]
    pub output: Option<ModelTokenPrice>,
    /// Price source.
    pub source: ModelPricingSource,
}

/// Model pricing unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPricingUnit {
    /// Prices are expressed per one million tokens.
    PerMillionTokens,
}

/// Price for a token bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTokenPrice {
    /// Price in micros of the pricing currency.
    pub micros: u64,
}

impl ModelTokenPrice {
    /// Construct a token price from currency micros.
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self { micros }
    }
}

/// Source used by a provider to resolve model pricing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPricingSource {
    UserOverride,
    ProviderApi,
    BundledCatalog,
    PatternMatch,
    Unknown,
}

/// Estimated model-call cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCostEstimate {
    /// ISO 4217 currency code.
    pub currency: String,
    /// Total estimated cost in micros of the pricing currency.
    pub total_micros: u64,
    /// Price metadata source used for the estimate.
    pub source: ModelPricingSource,
}

impl ModelPricingInfo {
    /// Estimate cost for provider-reported token usage.
    #[must_use]
    pub fn estimate_cost(&self, usage: &TokenUsage) -> Option<ModelCostEstimate> {
        let cached = usage.cached_input_tokens.unwrap_or_default();
        let cache_write = usage.cache_write_input_tokens.unwrap_or_default();
        let uncached_input = usage.uncached_input_tokens().unwrap_or_default();
        let output = usage.output_tokens.unwrap_or_default();
        let mut total_micros = 0_u64;
        total_micros = total_micros.saturating_add(price_bucket_micros(uncached_input, self.input));
        total_micros = total_micros.saturating_add(price_bucket_micros(cached, self.cached_input));
        total_micros =
            total_micros.saturating_add(price_bucket_micros(cache_write, self.cache_write_input));
        total_micros = total_micros.saturating_add(price_bucket_micros(output, self.output));
        (total_micros > 0).then(|| ModelCostEstimate {
            currency: self.currency.clone(),
            total_micros,
            source: self.source,
        })
    }
}

fn price_bucket_micros(tokens: u32, price: Option<ModelTokenPrice>) -> u64 {
    price.map_or(0, |price| {
        u64::from(tokens).saturating_mul(price.micros) / 1_000_000
    })
}

/// Source used by a provider to resolve model metadata such as token limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMetadataSource {
    ConfigOverride,
    ProviderApi,
    BundledCatalog,
    PatternMatch,
    ProviderDefault,
    Unknown,
}

/// Per-model/provider cache and continuation capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCacheInfo {
    #[serde(default)]
    pub capabilities: BTreeSet<ModelCacheCapability>,
}

/// Provider cache/continuation capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCacheCapability {
    PromptCacheKey,
    AutomaticPrefixCache,
    ExplicitCachePoints,
    CacheUsageReporting,
    PreviousResponseId,
}

/// Source of model reasoning capability metadata.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReasoningCapabilitySource {
    /// No source was provided.
    #[default]
    Unknown,
    /// User or administrator configuration override.
    ConfigOverride,
    /// Provider model metadata explicitly declared the values.
    ProviderMetadata,
    /// Bcode inferred values from a maintained provider/model compatibility table.
    KnownModelTable,
    /// Generic compatibility fallback; the provider may reject unsupported values.
    GenericFallback,
}

/// Per-model reasoning/thinking controls exposed by a provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReasoningInfo {
    /// Provider-native effort values accepted by the model.
    #[serde(default)]
    pub effort_values: Vec<String>,
    /// Provider-native default effort value, when known.
    #[serde(default)]
    pub default_effort: Option<String>,
    /// Whether provider-visible reasoning summaries can be requested.
    #[serde(default)]
    pub visible_summary_supported: bool,
    /// Provider-native summary/detail values accepted by the model.
    #[serde(default)]
    pub summary_values: Vec<String>,
    /// Provider-native default summary/detail value, when known.
    #[serde(default)]
    pub default_summary: Option<String>,
    /// Whether raw provider reasoning text can be requested.
    #[serde(default)]
    pub raw_reasoning_supported: bool,
    /// Source of this capability metadata.
    #[serde(default)]
    pub source: ModelReasoningCapabilitySource,
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
    NativeWebSearch,
    CodeSearch,
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

/// Binary-codec-safe provider-native request option value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderRequestValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl From<serde_json::Value> for ProviderRequestValue {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(value) => Self::Number(value.to_string()),
            serde_json::Value::String(value) => Self::String(value),
            serde_json::Value::Array(values) => {
                Self::Array(values.into_iter().map(Self::from).collect())
            }
            serde_json::Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl From<ProviderRequestValue> for serde_json::Value {
    fn from(value: ProviderRequestValue) -> Self {
        match value {
            ProviderRequestValue::Null => Self::Null,
            ProviderRequestValue::Bool(value) => Self::Bool(value),
            ProviderRequestValue::Number(value) => value
                .parse::<serde_json::Number>()
                .map_or_else(|_| Self::String(value), Self::Number),
            ProviderRequestValue::String(value) => Self::String(value),
            ProviderRequestValue::Array(values) => {
                Self::Array(values.into_iter().map(Self::from).collect())
            }
            ProviderRequestValue::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

/// Auth/security diagnostic surfaced while resolving provider credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthDiagnostic {
    /// Severity level, for example `info`, `warning`, or `error`.
    pub severity: String,
    /// Stable diagnostic code.
    pub code: String,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Optional remediation guidance.
    #[serde(default)]
    pub remediation: Option<String>,
}

/// Provider-neutral authentication material resolved by the client from the selected auth profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthContext {
    /// Auth profile ID selected by model/profile resolution.
    #[serde(default)]
    pub profile: Option<String>,
    /// Auth backend used to materialize the credentials, for example `sshenv`.
    #[serde(default)]
    pub backend: Option<String>,
    /// Provider/plugin-specific auth scheme, for example `api_key` or `chatgpt`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Canonical secret names to secret values, for example `api_key`.
    #[serde(default)]
    pub credentials: BTreeMap<String, ProviderAuthCredential>,
    /// Non-secret auth attributes, for example region/profile/base URL hints.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    /// Optional persistence references for credentials that can be refreshed/updated.
    #[serde(default)]
    pub storage: BTreeMap<String, ProviderAuthStorageRef>,
    /// Non-secret diagnostics produced while materializing auth.
    #[serde(default)]
    pub diagnostics: Vec<ProviderAuthDiagnostic>,
}

/// Secret credential value supplied to a provider plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthCredential {
    pub value: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// Location where a credential can be updated after refresh/login.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthStorageRef {
    pub backend: String,
    pub profile: String,
    pub key: String,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthCandidate {
    /// Auth profile name for this candidate.
    #[serde(default)]
    pub profile: Option<String>,
    /// Semantic auth material for this candidate.
    #[serde(default)]
    pub auth: ProviderAuthContext,
    /// Compatibility environment values scoped to this candidate.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Provider-neutral request context resolved by the host from model/provider profiles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequestContext {
    #[serde(default)]
    pub model_profile: Option<String>,
    #[serde(default)]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub auth_pool: Option<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Semantic auth material resolved from the selected auth profile.
    #[serde(default)]
    pub auth: Option<ProviderAuthContext>,
    /// Ordered semantic auth candidates resolved from the selected auth pool.
    #[serde(default)]
    pub auth_candidates: Vec<ProviderAuthCandidate>,
    /// Provider-native request fields merged into the outbound provider request.
    ///
    /// These values are non-secret provider-specific request options resolved from model profiles
    /// and aliases. Providers should validate/reserve core fields before merging.
    #[serde(default)]
    pub request: BTreeMap<String, ProviderRequestValue>,
    /// Transient client-supplied environment values for provider authentication/configuration.
    ///
    /// These values are carried in-memory from the initiating client connection to the provider
    /// plugin. They must not be persisted to session history or unredacted traces.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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

/// Provider-native web search request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeWebSearchRequest {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub freshness: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub safe_search: Option<String>,
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider-native web search response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeWebSearchResponse {
    pub provider: String,
    #[serde(default)]
    pub results: Vec<NativeWebSearchResult>,
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub message: Option<String>,
}

/// Provider-native web search result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeWebSearchResult {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
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
    /// Provider-private state from prior turns, such as encrypted reasoning continuation payloads.
    #[serde(default)]
    pub provider_state: Option<serde_json::Value>,
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

/// Verify that a provider can answer a tiny request with one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyModelRequest {
    /// Selected model ID to verify.
    pub model_id: String,
    /// Prompt sent to the model.
    pub prompt: String,
    /// Request timeout in seconds.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Model verification response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyModelResponse {
    pub status: VerifyModelStatus,
    #[serde(default)]
    pub latency_ms: Option<u128>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Verification status for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyModelStatus {
    Working,
    Unauthorized,
    NotFound,
    RateLimited,
    Timeout,
    ProviderError,
    NetworkError,
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
    Image {
        image: ImageContent,
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

/// Provider-neutral image content.
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
    #[serde(default)]
    pub reasoning_effort_value: Option<String>,
    #[serde(default)]
    pub reasoning_summary: Option<String>,
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

/// Provider-neutral image reference content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRefContent {
    pub path: String,
    pub mime_type: String,
    #[serde(default)]
    pub metadata: ImageMetadata,
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
    /// Provider reported actual request projection/sending metadata.
    RequestProjection {
        projection: ProviderRequestProjection,
    },
    /// Provider-specific metadata that the host may use for invisible optimization state.
    ProviderMetadata {
        key: String,
        value: String,
    },
    Warning {
        message: String,
    },
    RetryScheduled {
        message: String,
        retry_at_unix: u64,
    },
    Error {
        error: ProviderError,
    },
    TurnFinished {
        stop_reason: StopReason,
    },
    Cancelled,
}

/// Provider-reported request projection metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequestProjection {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub api_shape: Option<String>,
    #[serde(default)]
    pub input_item_count: Option<usize>,
    #[serde(default)]
    pub message_count: Option<usize>,
    #[serde(default)]
    pub original_message_count: Option<usize>,
    #[serde(default)]
    pub sent_message_count: Option<usize>,
    #[serde(default)]
    pub omitted_message_count: Option<usize>,
    #[serde(default)]
    pub cache_point_count: Option<usize>,
    #[serde(default)]
    pub emitted_cache_point_count: Option<usize>,
    #[serde(default)]
    pub dropped_cache_point_count: Option<usize>,
    #[serde(default)]
    pub used_previous_response_id: bool,
    #[serde(default)]
    pub detail: Option<String>,
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
    #[serde(default)]
    pub retry: Option<Box<ProviderRetryHint>>,
}

/// Provider-reported retry timing metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRetryHint {
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
    #[serde(default)]
    pub retry_at_unix: Option<u64>,
    #[serde(default)]
    pub source: Option<String>,
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
    use super::{
        ModelInfo, ModelList, ModelPricingInfo, ModelPricingSource, ModelPricingUnit,
        ModelTokenPrice, ModelVisibility, ModelVisibilitySource, TokenUsage,
    };

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

    #[test]
    fn pricing_estimate_charges_cached_input_separately() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: Some(ModelTokenPrice::from_micros(1_000_000)),
            cached_input: Some(ModelTokenPrice::from_micros(100_000)),
            cache_write_input: Some(ModelTokenPrice::from_micros(1_250_000)),
            output: Some(ModelTokenPrice::from_micros(4_000_000)),
            source: ModelPricingSource::BundledCatalog,
        };
        let usage = TokenUsage {
            input_tokens: Some(2_000_000),
            cached_input_tokens: Some(500_000),
            cache_write_input_tokens: Some(100_000),
            output_tokens: Some(250_000),
            ..TokenUsage::default()
        };

        let cost = pricing.estimate_cost(&usage).expect("cost should estimate");

        assert_eq!(cost.total_micros, 2_675_000);
    }

    #[test]
    fn pricing_estimate_is_unknown_without_priced_buckets() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: None,
            cached_input: None,
            cache_write_input: None,
            output: None,
            source: ModelPricingSource::Unknown,
        };

        assert!(pricing.estimate_cost(&TokenUsage::default()).is_none());
    }

    #[test]
    fn model_list_visibility_is_local_not_serialized() {
        let list = ModelList {
            models: vec![ModelInfo {
                model_id: "hidden".to_string(),
                display_name: "Hidden".to_string(),
                is_default: false,
                context_window: None,
                max_output_tokens: None,
                capabilities: std::collections::BTreeSet::new(),
                reasoning: None,
                cache: super::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                visibility: ModelVisibility::Ignored {
                    source: ModelVisibilitySource::State,
                    rule: "hidden".to_string(),
                },
            }],
        };

        let encoded = serde_json::to_string(&list).expect("model list should encode");
        let decoded: ModelList = serde_json::from_str(&encoded).expect("model list should decode");

        assert_eq!(decoded.models[0].visibility, ModelVisibility::Visible);
    }
}
