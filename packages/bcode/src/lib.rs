#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! High-level Rust SDK facade for Bcode.
//!
//! This crate provides explicit, application-facing types for building AI applications with Bcode.
//! The facade is intentionally small and delegates reusable turn behavior to
//! `bcode_agent_runtime`.

use bcode_agent_permissions::{PermissionAskCallback, ask_callback};
use bcode_agent_policy::active_tools_for;
use bcode_agent_runtime::{
    PermissionPolicyAuthorization, ToolBatchExecutionOutput, TurnGeneration, TurnScope,
};
#[cfg(feature = "embedded-plugins")]
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID, OP_CANCEL_TURN,
    OP_CAPABILITIES, OP_FINISH_TURN, OP_MODELS, OP_POLL_TURN_EVENTS, OP_START_TURN,
    PollTurnEventsRequest, PollTurnEventsResponse, StartTurnResponse,
};
use bcode_plugin_sdk::path::display_from_current_dir;
#[cfg(feature = "embedded-plugins")]
use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse};
pub use bcode_session_models::SessionId;
/// Optional OpenTelemetry and in-process metrics adapters.
pub mod telemetry;
use futures::Stream;
use pin_project_lite::pin_project;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::Instrument as _;

pub use bcode_agent_permissions::{AgentPermissionPolicy, allow_all_agent_policy};
pub use bcode_agent_policy::{Action, AgentConfig, AgentPermissionConfig, PermissionConfig};
pub use bcode_agent_profile::{AgentDecision, EvaluateToolCallResponse};
#[cfg(feature = "testing")]
pub mod testing;
pub use bcode_agent_runtime::{
    AgentLoopStopCondition, AgentLoopStopContext, AgentLoopStopPredicate,
    AgentLoopTerminationReason, AgentRuntime, AgentRuntimeEvent as AgentEvent,
    AgentRuntimeStream as AgentStream, AgentRuntimeStreamItem as AgentStreamItem, AgentTurnRequest,
    AgentTurnResponse, AllowAllPolicy, CancellationToken, DEFAULT_STREAM_BUFFER_CAPACITY,
    InProcessModelProvider, InProcessModelProviderAdapter, InProcessProviderContext,
    InProcessProviderEmitError, InProcessProviderEventSink, InProcessProviderFuture,
    InProcessProviderOutcome, ModelProviderInvoker, PermissionDecision, PermissionPolicy,
    ProviderRoundPlan, ProviderRoundPlanContext, ProviderRoundPlanner, RegisteredTool,
    RuntimeError, RuntimeFuture, RuntimePermissionContext, RuntimePermissionRequest, ToolCatalog,
    ToolExecutionOutput, ToolResultPolicy, ToolResultTransform, ToolRoundObserver, ToolRoundState,
    ToolSource, UnifiedToolCatalog, in_process_provider_error,
};
pub use bcode_agent_runtime::{
    ArtifactCommitGuard, HostTurnEventSink, InvocationArtifactSink, InvocationCapabilities,
    InvocationCapabilityFuture, InvocationExchangeBroker, InvocationInputRouter, InvocationScope,
    InvocationServiceRouter, PreparationScope, ScopedTurnEvent, ToolAuthorizationCoordinator,
    ToolAuthorizationDecision, ToolAuthorizationRequest, ToolInvoker, TurnEventObservability,
    TurnEventPersistence, TurnEventSink,
};
#[cfg(feature = "daemon-client")]
pub use bcode_client::{
    BcodeClient, ClientConnection, ClientError, DaemonAvailability, SessionList,
};
pub use bcode_model::{
    CapabilityScope, CapabilitySource, CapabilitySupport, ContentBlock as ModelContentBlock,
    MediaInputFeature, MessageRole, ModelCostEstimate, ModelFeatureSupport, ModelInfo, ModelList,
    ModelMessage, ModelMetadataSource, ModelParameterKey, ModelParameters, ModelPricingInfo,
    ModelPricingSource, ModelPricingUnit, ModelTokenPrice, ModelTurnRequest,
    NegotiatedFeatureSupport, PromptCacheFeature, ProviderCapabilities, ProviderError,
    ProviderErrorCategory, ProviderErrorSource, ProviderRequestContext, ProviderRequestExtension,
    ProviderRequestProjection, ProviderRetryHint, ProviderTurnEvent, RequestedModelFeature,
    StopReason, StructuredOutputMode, TokenUsage, ToolCall, ToolChoice, ToolChoiceMode, ToolResult,
    ToolResultContent,
};
#[cfg(feature = "evaluation")]
pub mod evaluation;
#[cfg(feature = "openai-compatible-provider")]
pub mod openai {
    //! Typed provider-specific options for the OpenAI-compatible provider.

    pub use bcode_openai_compatible_provider_plugin::{
        OpenAiPromptCacheRetention, OpenAiResponsesRequestOptions, OpenAiServiceTier,
        OpenAiTruncation,
    };
}
pub use bcode_tool::PreparedToolInvocation;
pub use bcode_tool::{
    ListToolsRequest, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifactWriteRequest,
    ToolArtifactWriteResolution, ToolDefinition, ToolExchangeRequest, ToolExchangeResolution,
    ToolExchangeResponsePolicy, ToolExecutionOptions, ToolInvocationDescriptor,
    ToolInvocationInput, ToolInvocationInputResolution, ToolInvocationLifecycleEvent,
    ToolInvocationLifecycleStage, ToolInvocationResponse, ToolInvocationResult,
    ToolInvocationServiceRequest, ToolInvocationServiceResolution, ToolList, ToolPolicyMetadata,
    ToolPreparationRequest, ToolPreparationResponse, ToolSideEffect, ToolUiMetadata,
};

/// Result alias for Bcode SDK operations.
pub type Result<T> = std::result::Result<T, BcodeError>;

/// Provider/model selector used by ergonomic SDK helpers and builders.
///
/// A selector can hold just a model ID (`"gpt-4o-mini"`) or a provider-qualified model string
/// (`"provider:model"`). In embedded plugin mode the provider component maps to the provider
/// plugin ID used by [`AgentBuilder::provider_plugin`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelSelector {
    provider_plugin_id: Option<String>,
    model_id: String,
}

impl ModelSelector {
    /// Create a selector for an unqualified model ID.
    #[must_use]
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            provider_plugin_id: None,
            model_id: model_id.into(),
        }
    }

    /// Create a selector for an explicit provider plugin ID and model ID.
    #[must_use]
    pub fn with_provider(
        provider_plugin_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            provider_plugin_id: Some(provider_plugin_id.into()),
            model_id: model_id.into(),
        }
    }

    /// Parse either `model` or `provider:model` selector text.
    #[must_use]
    pub fn from_text(selector: impl AsRef<str>) -> Self {
        let selector = selector.as_ref();
        selector.split_once(':').map_or_else(
            || Self::new(selector),
            |(provider_plugin_id, model_id)| Self::with_provider(provider_plugin_id, model_id),
        )
    }

    /// Return the provider plugin ID when the selector is provider-qualified.
    #[must_use]
    pub fn provider_plugin_id(&self) -> Option<&str> {
        self.provider_plugin_id.as_deref()
    }

    /// Return the selected model ID.
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

impl From<&str> for ModelSelector {
    fn from(value: &str) -> Self {
        Self::from_text(value)
    }
}

impl From<String> for ModelSelector {
    fn from(value: String) -> Self {
        Self::from_text(value)
    }
}

/// Configurable retry policy for provider invocation failures.
///
/// The policy retries only provider-originated failures. Cancellation, timeout, tool, permission,
/// host-extension, and validation failures remain terminal. Each retry passes through the runtime's
/// canonical cancellation boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_retries: u32,
    base_delay: Duration,
    max_delay: Duration,
    jitter_millis: u64,
}

impl RetryPolicy {
    /// Create a retry policy with a maximum retry count and fixed delay.
    #[must_use]
    pub const fn new(max_retries: u32, delay: Duration) -> Self {
        Self {
            max_retries,
            base_delay: delay,
            max_delay: delay,
            jitter_millis: 0,
        }
    }

    /// Configure exponential backoff capped at `max_delay`.
    #[must_use]
    pub const fn with_max_delay(mut self, max_delay: Duration) -> Self {
        self.max_delay = max_delay;
        self
    }

    /// Configure deterministic bounded jitter in milliseconds.
    ///
    /// Jitter is derived from request identity and attempt, requiring no RNG/global state.
    #[must_use]
    pub const fn with_jitter_millis(mut self, jitter_millis: u64) -> Self {
        self.jitter_millis = jitter_millis;
        self
    }

    /// Return the maximum number of retries after the initial attempt.
    #[must_use]
    pub const fn max_retries(self) -> u32 {
        self.max_retries
    }

    /// Return the fixed delay before each retry.
    #[must_use]
    pub const fn delay(self) -> Duration {
        self.base_delay
    }

    fn retry_delay(&self, context: &ProviderRoundPlanContext<'_>) -> Duration {
        let exponent = context.attempt.saturating_sub(1).min(31);
        let multiplier = 1_u32 << exponent;
        let backoff = self
            .base_delay
            .checked_mul(multiplier)
            .unwrap_or(self.max_delay)
            .min(self.max_delay);
        let hinted = provider_retry_delay(context.previous_failure).unwrap_or(Duration::ZERO);
        let floor = backoff.max(hinted).min(self.max_delay);
        let jitter_bound = self.jitter_millis.min(
            u64::try_from(self.max_delay.saturating_sub(floor).as_millis()).unwrap_or(u64::MAX),
        );
        if jitter_bound == 0 {
            return floor;
        }
        let mut hasher = Sha256::new();
        hasher.update(context.proposed_request.model_id.as_bytes());
        hasher.update(
            context
                .proposed_request
                .provider_plugin_id
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update(context.attempt.to_le_bytes());
        let digest = hasher.finalize();
        let jitter = u64::from_le_bytes(digest[..8].try_into().expect("SHA-256 has eight bytes"))
            % jitter_bound.saturating_add(1);
        floor + Duration::from_millis(jitter)
    }
}

fn provider_retry_delay(failure: Option<&RuntimeError>) -> Option<Duration> {
    let RuntimeError::Provider { error, .. } = failure? else {
        return None;
    };
    let hint = error.retry.as_deref()?;
    let relative = hint.retry_after_ms.map(Duration::from_millis);
    let absolute = hint.retry_at_unix.map(|retry_at| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        Duration::from_secs(retry_at.saturating_sub(now))
    });
    match (relative, absolute) {
        (Some(relative), Some(absolute)) => Some(relative.max(absolute)),
        (Some(delay), None) | (None, Some(delay)) => Some(delay),
        (None, None) => None,
    }
}

fn retryable_provider_failure(failure: &RuntimeError) -> bool {
    match failure {
        RuntimeError::ProviderInvocation(_) => true,
        RuntimeError::Provider { error, .. } => error.retryable,
        _ => false,
    }
}

fn fallbackable_provider_failure(failure: &RuntimeError) -> bool {
    match failure {
        RuntimeError::ProviderInvocation(_) => true,
        RuntimeError::Provider { error, .. } => matches!(
            error.category,
            ProviderErrorCategory::RateLimit
                | ProviderErrorCategory::Network
                | ProviderErrorCategory::Timeout
                | ProviderErrorCategory::ModelNotFound
                | ProviderErrorCategory::UnsupportedFeature
                | ProviderErrorCategory::ProviderInternal
                | ProviderErrorCategory::Overloaded
        ),
        _ => false,
    }
}

impl ProviderRoundPlanner for RetryPolicy {
    fn plan_round<'a>(
        &'a self,
        context: ProviderRoundPlanContext<'a>,
    ) -> RuntimeFuture<'a, ProviderRoundPlan> {
        Box::pin(async move {
            let Some(failure) = context.previous_failure else {
                return Ok(ProviderRoundPlan::Proceed {
                    request: context.proposed_request.clone(),
                });
            };
            let retryable = retryable_provider_failure(failure);
            Ok(if retryable && context.attempt <= self.max_retries {
                ProviderRoundPlan::RetryAfter {
                    request: context.proposed_request.clone(),
                    delay: self.retry_delay(&context),
                }
            } else {
                ProviderRoundPlan::Fail { error: None }
            })
        })
    }
}

/// Ordered fallback provider/model policy for provider invocation failures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FallbackPolicy {
    fallbacks: Vec<ModelSelector>,
}

impl FallbackPolicy {
    /// Create an empty fallback policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a provider/model fallback.
    #[must_use]
    pub fn fallback(mut self, selector: impl Into<ModelSelector>) -> Self {
        self.fallbacks.push(selector.into());
        self
    }

    /// Return configured fallbacks in attempt order.
    #[must_use]
    pub fn fallbacks(&self) -> &[ModelSelector] {
        &self.fallbacks
    }
}

impl ProviderRoundPlanner for FallbackPolicy {
    fn plan_round<'a>(
        &'a self,
        context: ProviderRoundPlanContext<'a>,
    ) -> RuntimeFuture<'a, ProviderRoundPlan> {
        Box::pin(async move {
            let Some(failure) = context.previous_failure else {
                return Ok(ProviderRoundPlan::Proceed {
                    request: context.proposed_request.clone(),
                });
            };
            if !fallbackable_provider_failure(failure) {
                return Ok(ProviderRoundPlan::Fail { error: None });
            }
            let fallback_index = context.attempt.saturating_sub(1) as usize;
            let Some(selector) = self.fallbacks.get(fallback_index) else {
                return Ok(ProviderRoundPlan::Fail { error: None });
            };
            let mut request = context.proposed_request.clone();
            request.provider_plugin_id = selector.provider_plugin_id().map(str::to_string);
            request.model_id = selector.model_id().to_string();
            Ok(ProviderRoundPlan::RetryAfter {
                request,
                delay: Duration::ZERO,
            })
        })
    }
}

/// Application-owned error returned by a typed tool handler.
///
/// `message` and `details` are application-visible through the structured invocation result.
/// Only `model_message` is sent back to the model, so applications can avoid exposing internal or
/// sensitive error details accidentally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolApplicationError<D = serde_json::Value> {
    /// Stable application-defined error code.
    pub code: String,
    /// Detailed application-visible error message.
    pub message: String,
    /// Explicitly safe model-visible error message.
    pub model_message: String,
    /// Typed application-owned diagnostic details.
    pub details: D,
    /// Whether retrying the tool operation may succeed.
    pub retryable: bool,
}

impl<D> ToolApplicationError<D> {
    /// Create a non-retryable typed application error.
    #[must_use]
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        model_message: impl Into<String>,
        details: D,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            model_message: model_message.into(),
            details,
            retryable: false,
        }
    }

    /// Mark whether retrying may succeed.
    #[must_use]
    pub const fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

/// Per-call context supplied to asynchronous typed tools.
///
/// State is available only when the application explicitly supplies it during registration.
#[derive(Debug, Clone)]
pub struct TypedToolContext<S> {
    scope: InvocationScope,
    state: Arc<S>,
    lifecycle_sequence: Arc<AtomicU64>,
}

impl<S> TypedToolContext<S> {
    fn new(scope: InvocationScope, state: Arc<S>) -> Self {
        Self {
            scope,
            state,
            lifecycle_sequence: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Return the canonical invocation ID.
    #[must_use]
    pub fn invocation_id(&self) -> &str {
        self.scope.invocation_id()
    }

    /// Return explicitly supplied application state.
    #[must_use]
    pub fn state(&self) -> &S {
        &self.state
    }

    /// Return cancellation shared with the parent model/tool turn.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.scope.cancellation()
    }

    /// Return whether cancellation has been requested or normal output has closed.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        !self.scope.accepts_work()
    }

    /// Publish a typed, ordered progress event through the canonical invocation event stream.
    ///
    /// # Errors
    ///
    /// Returns an error when `metadata` cannot be serialized.
    pub fn report_progress<M>(
        &self,
        message: impl Into<String>,
        metadata: M,
    ) -> std::result::Result<bool, serde_json::Error>
    where
        M: Serialize,
    {
        let event = ToolInvocationLifecycleEvent {
            invocation_id: self.invocation_id().to_string(),
            sequence: self.lifecycle_sequence.fetch_add(1, Ordering::Relaxed),
            stage: ToolInvocationLifecycleStage::Progress,
            message: Some(message.into()),
            metadata: serde_json::to_value(metadata)?,
        };
        Ok(self.scope.emit_lifecycle(event))
    }
}

/// Model-callable inline tool definition derived from Rust input/output types.
///
/// `I` supplies the provider-visible JSON Schema and is deserialized before the handler runs. `O`
/// is serialized into both the model-visible output and structured tool result. Use
/// [`AgentBuilder::typed_tool`] to register the completed definition.
#[derive(Debug, Clone)]
pub struct TypedTool<I, O> {
    definition: ToolDefinition,
    _types: std::marker::PhantomData<fn(I) -> O>,
}

impl<I, O> TypedTool<I, O>
where
    I: schemars::JsonSchema,
{
    /// Create a read-only typed tool that does not require permission.
    ///
    /// # Panics
    ///
    /// Panics only if schemars emits a schema that cannot be represented as JSON.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            definition: ToolDefinition {
                name: name.into(),
                description: description.into(),
                input_schema: serde_json::to_value(schemars::schema_for!(I))
                    .expect("schemars tool input schema should serialize to JSON"),
                side_effect: ToolSideEffect::ReadOnly,
                requires_permission: false,
                policy: ToolPolicyMetadata::default(),
                ui: ToolUiMetadata::default(),
            },
            _types: std::marker::PhantomData,
        }
    }

    /// Configure the tool's side-effect classification.
    #[must_use]
    pub const fn side_effect(mut self, side_effect: ToolSideEffect) -> Self {
        self.definition.side_effect = side_effect;
        self
    }

    /// Configure whether the tool requires permission before execution.
    #[must_use]
    pub const fn requires_permission(mut self, requires_permission: bool) -> Self {
        self.definition.requires_permission = requires_permission;
        self
    }

    /// Configure plugin-compatible policy metadata for this inline tool.
    #[must_use]
    pub fn policy(mut self, policy: ToolPolicyMetadata) -> Self {
        self.definition.policy = policy;
        self
    }

    /// Configure renderer-neutral UI metadata for this inline tool.
    #[must_use]
    pub fn ui(mut self, ui: ToolUiMetadata) -> Self {
        self.definition.ui = ui;
        self
    }

    /// Return the generated tool definition.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
}

/// Builder for text-generation requests.
///
/// This is the builder-first API for text generation. Thin helper functions such as
/// [`generate_text`] delegate to this type.
#[derive(Debug, Clone)]
pub struct GenerateTextBuilder {
    agent: AgentBuilder,
    prompt: String,
    messages: Vec<ModelMessage>,
    cancellation: CancellationToken,
}

impl Default for GenerateTextBuilder {
    fn default() -> Self {
        Self {
            agent: Agent::builder(),
            prompt: String::new(),
            messages: Vec::new(),
            cancellation: CancellationToken::new(),
        }
    }
}

impl GenerateTextBuilder {
    /// Create a text-generation builder with default agent settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure advanced agent behavior for this request.
    ///
    /// The callback exposes the complete [`AgentBuilder`] for tools, hooks, policy, provider
    /// context, plugin runtime, execution options, and other advanced settings.
    #[must_use]
    pub fn configure_agent<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(AgentBuilder) -> AgentBuilder,
    {
        self.agent = configure(self.agent);
        self
    }

    /// Configure the user prompt.
    #[must_use]
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Configure provider/model selection.
    #[must_use]
    pub fn model(mut self, model: impl Into<ModelSelector>) -> Self {
        self.agent = self.agent.model_selector(model);
        self
    }

    /// Configure the system prompt.
    #[must_use]
    pub fn system(mut self, system_prompt: impl Into<String>) -> Self {
        self.agent = self.agent.system(system_prompt);
        self
    }

    /// Configure prior conversation messages.
    #[must_use]
    pub fn messages(mut self, messages: Vec<ModelMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Append one prior conversation message.
    #[must_use]
    pub fn message(mut self, message: ModelMessage) -> Self {
        self.messages.push(message);
        self
    }

    /// Configure model parameters.
    #[must_use]
    pub fn parameters(mut self, parameters: ModelParameters) -> Self {
        self.agent = self.agent.parameters(parameters);
        self
    }

    /// Add one metadata key/value pair sent to providers.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.agent = self.agent.metadata(key, value);
        self
    }

    /// Configure turn timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.agent = self.agent.timeout(timeout);
        self
    }

    /// Configure cancellation for this request.
    #[must_use]
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Configure how inline tool handler failures affect provider/tool loops.
    #[must_use]
    pub fn tool_failure_policy(mut self, policy: ToolFailurePolicy) -> Self {
        self.agent = self.agent.tool_failure_policy(policy);
        self
    }

    /// Configure provider-neutral model tool-choice behavior.
    #[must_use]
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.agent = self.agent.tool_choice(choice);
        self
    }

    /// Configure maximum tool rounds.
    #[must_use]
    pub fn max_tool_rounds(mut self, max_tool_rounds: u32) -> Self {
        self.agent = self.agent.max_tool_rounds(max_tool_rounds);
        self
    }

    /// Configure maximum consecutive semantically identical tool-call batches.
    #[must_use]
    pub fn max_repeated_tool_batches(mut self, limit: u32) -> Self {
        self.agent = self.agent.max_repeated_tool_batches(limit);
        self
    }

    /// Configure an application-owned successful-loop stop condition.
    #[must_use]
    pub fn stop_when(mut self, condition: impl AgentLoopStopCondition + 'static) -> Self {
        self.agent = self.agent.stop_when(condition);
        self
    }

    /// Register a typed inline SDK tool for this text-generation request.
    #[must_use]
    pub fn typed_tool<I, O, F>(mut self, tool: TypedTool<I, O>, handler: F) -> Self
    where
        I: DeserializeOwned + schemars::JsonSchema + Send + 'static,
        O: Serialize + Send + 'static,
        F: Fn(I) -> std::result::Result<O, String> + Send + Sync + 'static,
    {
        self.agent = self.agent.typed_tool(tool, handler);
        self
    }

    /// Register an inline SDK tool for this text-generation request.
    #[must_use]
    pub fn inline_tool<F>(mut self, definition: ToolDefinition, handler: F) -> Self
    where
        F: Fn(ToolInvocationDescriptor) -> std::result::Result<ToolInvocationResponse, String>
            + Send
            + Sync
            + 'static,
    {
        self.agent = self.agent.inline_tool(definition, handler);
        self
    }

    /// Register a plugin-backed tool definition for this text-generation request.
    #[must_use]
    pub fn plugin_tool(mut self, definition: ToolDefinition, plugin_id: impl Into<String>) -> Self {
        self.agent = self.agent.plugin_tool(definition, plugin_id);
        self
    }

    /// Configure ordered provider/model fallbacks for provider-originated failures.
    #[must_use]
    pub fn fallback_policy(mut self, policy: FallbackPolicy) -> Self {
        self.agent = self.agent.fallback_policy(policy);
        self
    }

    /// Configure fixed-delay retries for provider-originated failures.
    #[must_use]
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.agent = self.agent.retry_policy(policy);
        self
    }

    /// Configure an application-owned response cache for this text-generation request.
    #[must_use]
    pub fn response_cache(mut self, cache: Arc<dyn ModelResponseCache>) -> Self {
        self.agent = self.agent.response_cache(cache);
        self
    }

    /// Append model request/response middleware for this text-generation request.
    #[must_use]
    pub fn middleware_layer<M>(mut self, middleware: M) -> Self
    where
        M: ModelMiddleware + 'static,
    {
        self.agent = self.agent.middleware_layer(middleware);
        self
    }

    /// Run the request with a caller-supplied provider.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, cancellation is requested, the turn times
    /// out, or the provider reports an error.
    pub async fn run<P>(self, provider: &mut P) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        let agent = self.agent.build();
        agent
            .generate_text_with_provider_with_options(
                provider,
                self.prompt,
                None,
                self.messages,
                self.cancellation,
            )
            .await
    }
}

/// Start building a text-generation request.
#[must_use]
pub fn generate_text_builder() -> GenerateTextBuilder {
    GenerateTextBuilder::new()
}

/// Builder for streaming text-generation requests.
///
/// This is the builder-first API for streaming text. Thin helper functions such as
/// [`stream_text`] delegate to this type.
#[derive(Debug, Clone)]
pub struct StreamTextBuilder {
    agent: AgentBuilder,
    prompt: String,
    messages: Vec<ModelMessage>,
    cancellation: CancellationToken,
}

impl Default for StreamTextBuilder {
    fn default() -> Self {
        Self {
            agent: Agent::builder(),
            prompt: String::new(),
            messages: Vec::new(),
            cancellation: CancellationToken::new(),
        }
    }
}

impl StreamTextBuilder {
    /// Create a streaming text builder with default agent settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure advanced agent behavior for this request.
    ///
    /// The callback exposes the complete [`AgentBuilder`] for tools, hooks, policy, provider
    /// context, plugin runtime, execution options, and other advanced settings.
    #[must_use]
    pub fn configure_agent<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(AgentBuilder) -> AgentBuilder,
    {
        self.agent = configure(self.agent);
        self
    }

    /// Configure the user prompt.
    #[must_use]
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Configure provider/model selection.
    #[must_use]
    pub fn model(mut self, model: impl Into<ModelSelector>) -> Self {
        self.agent = self.agent.model_selector(model);
        self
    }

    /// Configure the system prompt.
    #[must_use]
    pub fn system(mut self, system_prompt: impl Into<String>) -> Self {
        self.agent = self.agent.system(system_prompt);
        self
    }

    /// Configure prior conversation messages.
    #[must_use]
    pub fn messages(mut self, messages: Vec<ModelMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Append one prior conversation message.
    #[must_use]
    pub fn message(mut self, message: ModelMessage) -> Self {
        self.messages.push(message);
        self
    }

    /// Configure model parameters.
    #[must_use]
    pub fn parameters(mut self, parameters: ModelParameters) -> Self {
        self.agent = self.agent.parameters(parameters);
        self
    }

    /// Add one metadata key/value pair sent to providers.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.agent = self.agent.metadata(key, value);
        self
    }

    /// Configure turn timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.agent = self.agent.timeout(timeout);
        self
    }

    /// Configure cancellation for this request.
    #[must_use]
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Configure how inline tool handler failures affect provider/tool loops.
    #[must_use]
    pub fn tool_failure_policy(mut self, policy: ToolFailurePolicy) -> Self {
        self.agent = self.agent.tool_failure_policy(policy);
        self
    }

    /// Configure provider-neutral model tool-choice behavior.
    #[must_use]
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.agent = self.agent.tool_choice(choice);
        self
    }

    /// Configure maximum tool rounds.
    #[must_use]
    pub fn max_tool_rounds(mut self, max_tool_rounds: u32) -> Self {
        self.agent = self.agent.max_tool_rounds(max_tool_rounds);
        self
    }

    /// Configure maximum consecutive semantically identical tool-call batches.
    #[must_use]
    pub fn max_repeated_tool_batches(mut self, limit: u32) -> Self {
        self.agent = self.agent.max_repeated_tool_batches(limit);
        self
    }

    /// Configure an application-owned successful-loop stop condition.
    #[must_use]
    pub fn stop_when(mut self, condition: impl AgentLoopStopCondition + 'static) -> Self {
        self.agent = self.agent.stop_when(condition);
        self
    }

    /// Register a typed inline SDK tool for this streaming request.
    #[must_use]
    pub fn typed_tool<I, O, F>(mut self, tool: TypedTool<I, O>, handler: F) -> Self
    where
        I: DeserializeOwned + schemars::JsonSchema + Send + 'static,
        O: Serialize + Send + 'static,
        F: Fn(I) -> std::result::Result<O, String> + Send + Sync + 'static,
    {
        self.agent = self.agent.typed_tool(tool, handler);
        self
    }

    /// Register an inline SDK tool for this streaming request.
    #[must_use]
    pub fn inline_tool<F>(mut self, definition: ToolDefinition, handler: F) -> Self
    where
        F: Fn(ToolInvocationDescriptor) -> std::result::Result<ToolInvocationResponse, String>
            + Send
            + Sync
            + 'static,
    {
        self.agent = self.agent.inline_tool(definition, handler);
        self
    }

    /// Register a plugin-backed tool definition for this streaming request.
    #[must_use]
    pub fn plugin_tool(mut self, definition: ToolDefinition, plugin_id: impl Into<String>) -> Self {
        self.agent = self.agent.plugin_tool(definition, plugin_id);
        self
    }

    /// Configure ordered provider/model fallbacks for this streaming request.
    #[must_use]
    pub fn fallback_policy(mut self, policy: FallbackPolicy) -> Self {
        self.agent = self.agent.fallback_policy(policy);
        self
    }

    /// Configure bounded cancellation-aware retries for this streaming request.
    #[must_use]
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.agent = self.agent.retry_policy(policy);
        self
    }

    /// Append model request/response middleware for this streaming request.
    ///
    /// Request middleware runs before provider startup. Response middleware runs on the terminal
    /// response without buffering or rewriting already emitted deltas.
    #[must_use]
    pub fn middleware_layer<M>(mut self, middleware: M) -> Self
    where
        M: ModelMiddleware + 'static,
    {
        self.agent = self.agent.middleware_layer(middleware);
        self
    }

    /// Run the request with a caller-supplied provider.
    #[must_use]
    pub fn run<P>(self, provider: P) -> TextStream
    where
        P: ModelProviderInvoker + 'static,
    {
        let agent = self.agent.build();
        let request = agent.turn_request_with_structured_output_messages_and_cancellation(
            self.prompt,
            None,
            self.messages,
            self.cancellation,
        );
        TextStream::start(&agent, provider, request)
    }
}

/// Start building a streaming text request.
#[must_use]
pub fn stream_text_builder() -> StreamTextBuilder {
    StreamTextBuilder::new()
}

/// Item produced by the high-level text stream.
#[derive(Debug)]
pub enum TextStreamItem {
    /// Normalized provider/runtime event.
    Event(AgentEvent),
    /// Non-runtime scoped event from tool invocation lifecycle or renderer-neutral contributions.
    ScopedEvent(ScopedTurnEvent),
    /// Final response after response middleware and after-model hooks complete.
    Finished(GenerateTextResponse),
    /// SDK or runtime error that terminated the stream.
    Error(BcodeError),
}

struct TextStreamFinalizer {
    request: AgentTurnRequest,
    context: ModelCallContext,
    middleware: ModelMiddlewareStack,
    hooks: AgentHooks,
    model_pricing: Option<ModelPricingInfo>,
}

pin_project! {
    /// High-level text stream with SDK middleware, hooks, tools, and retry/fallback semantics.
    ///
    /// This adapts the canonical scoped provider/tool stream. Runtime events remain available as
    /// [`TextStreamItem::Event`], while invocation lifecycle and contribution events are retained
    /// as [`TextStreamItem::ScopedEvent`]. Configured response caches are deliberately bypassed
    /// because replaying a completed response cannot reproduce trustworthy stream timing or tool
    /// lifecycle.
    pub struct TextStream {
        #[pin]
        stream: Option<ScopedAgentStream>,
    }
}

impl fmt::Debug for TextStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextStream")
            .field("active", &self.stream.is_some())
            .finish_non_exhaustive()
    }
}

impl TextStream {
    fn start<P>(agent: &Agent, provider: P, request: AgentTurnRequest) -> Self
    where
        P: ModelProviderInvoker + 'static,
    {
        Self {
            stream: Some(agent.stream_request(provider, request)),
        }
    }

    /// Receive the next high-level text stream item.
    ///
    /// This convenience method is equivalent to `futures::StreamExt::next` and does not require
    /// importing the extension trait.
    pub async fn next(&mut self) -> Option<TextStreamItem> {
        let item = self.stream.as_mut()?.next().await?;
        let terminal = matches!(
            item,
            ScopedAgentStreamItem::Finished(_) | ScopedAgentStreamItem::Error(_)
        );
        let item = text_stream_item(item);
        if terminal {
            self.stream = None;
        }
        Some(item)
    }
}

impl Stream for TextStream {
    type Item = TextStreamItem;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let item = match this.stream.as_mut().as_pin_mut() {
            Some(stream) => match stream.poll_next(context) {
                Poll::Ready(Some(item)) => item,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            },
            None => return Poll::Ready(None),
        };
        let terminal = matches!(
            item,
            ScopedAgentStreamItem::Finished(_) | ScopedAgentStreamItem::Error(_)
        );
        let item = text_stream_item(item);
        if terminal {
            this.stream.set(None);
        }
        Poll::Ready(Some(item))
    }
}

fn text_stream_item(item: ScopedAgentStreamItem) -> TextStreamItem {
    match item {
        ScopedAgentStreamItem::Event(ScopedTurnEvent::Runtime(event)) => {
            TextStreamItem::Event(event)
        }
        ScopedAgentStreamItem::Event(event) => TextStreamItem::ScopedEvent(event),
        ScopedAgentStreamItem::Finished(response) => TextStreamItem::Finished(response),
        ScopedAgentStreamItem::Error(error) => TextStreamItem::Error(error),
    }
}

/// Builder for structured object generation requests.
///
/// This is the builder-first API for typed structured output. Thin helper functions such as
/// [`generate_object`] delegate to this type.
#[derive(Debug, Clone)]
pub struct GenerateObjectBuilder<T> {
    agent: AgentBuilder,
    prompt: String,
    messages: Vec<ModelMessage>,
    options: Option<StructuredOutputOptions>,
    cancellation: CancellationToken,
    _output: std::marker::PhantomData<T>,
}

impl<T> Default for GenerateObjectBuilder<T> {
    fn default() -> Self {
        Self {
            agent: Agent::builder(),
            prompt: String::new(),
            messages: Vec::new(),
            options: None,
            cancellation: CancellationToken::new(),
            _output: std::marker::PhantomData,
        }
    }
}

impl<T> GenerateObjectBuilder<T> {
    /// Create a structured object generation builder with default agent settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure advanced agent behavior for this request.
    ///
    /// The callback exposes the complete [`AgentBuilder`] for tools, hooks, policy, provider
    /// context, plugin runtime, execution options, and other advanced settings.
    #[must_use]
    pub fn configure_agent<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(AgentBuilder) -> AgentBuilder,
    {
        self.agent = configure(self.agent);
        self
    }

    /// Configure the user prompt.
    #[must_use]
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Configure provider/model selection.
    #[must_use]
    pub fn model(mut self, model: impl Into<ModelSelector>) -> Self {
        self.agent = self.agent.model_selector(model);
        self
    }

    /// Configure the system prompt.
    #[must_use]
    pub fn system(mut self, system_prompt: impl Into<String>) -> Self {
        self.agent = self.agent.system(system_prompt);
        self
    }

    /// Configure prior conversation messages.
    #[must_use]
    pub fn messages(mut self, messages: Vec<ModelMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Append one prior conversation message.
    #[must_use]
    pub fn message(mut self, message: ModelMessage) -> Self {
        self.messages.push(message);
        self
    }

    /// Configure structured-output options.
    #[must_use]
    pub fn options(mut self, options: StructuredOutputOptions) -> Self {
        self.options = Some(options);
        self
    }

    /// Configure model parameters.
    #[must_use]
    pub fn parameters(mut self, parameters: ModelParameters) -> Self {
        self.agent = self.agent.parameters(parameters);
        self
    }

    /// Add one metadata key/value pair sent to providers.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.agent = self.agent.metadata(key, value);
        self
    }

    /// Configure turn timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.agent = self.agent.timeout(timeout);
        self
    }

    /// Configure cancellation for this request.
    #[must_use]
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Configure ordered provider/model fallbacks for provider-originated failures.
    #[must_use]
    pub fn fallback_policy(mut self, policy: FallbackPolicy) -> Self {
        self.agent = self.agent.fallback_policy(policy);
        self
    }

    /// Configure fixed-delay retries for provider-originated failures.
    #[must_use]
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.agent = self.agent.retry_policy(policy);
        self
    }

    /// Configure an application-owned response cache for this structured request.
    #[must_use]
    pub fn response_cache(mut self, cache: Arc<dyn ModelResponseCache>) -> Self {
        self.agent = self.agent.response_cache(cache);
        self
    }

    /// Append model request/response middleware for this structured request.
    #[must_use]
    pub fn middleware_layer<M>(mut self, middleware: M) -> Self
    where
        M: ModelMiddleware + 'static,
    {
        self.agent = self.agent.middleware_layer(middleware);
        self
    }

    /// Run the request with a caller-supplied provider.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the runtime is cancelled, the provider
    /// reports an error, the model output is not valid JSON, schema validation fails, or decoding
    /// into `T` fails.
    pub async fn run<P>(self, provider: &mut P) -> Result<T>
    where
        T: DeserializeOwned + schemars::JsonSchema,
        P: ModelProviderInvoker,
    {
        let options = self
            .options
            .unwrap_or_else(StructuredOutputOptions::for_type::<T>);
        let agent = self.agent.build();
        agent
            .generate_object_with_provider_and_request_options(
                provider,
                self.prompt,
                options,
                self.messages,
                self.cancellation,
            )
            .await
    }
    /// Run the request with explicit structured-output options and a caller-supplied provider.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the runtime is cancelled, the provider
    /// reports an error, the model output is not valid JSON, schema validation fails, repair
    /// attempts are exhausted, or decoding into `T` fails.
    pub async fn run_with_options<P>(
        self,
        provider: &mut P,
        options: StructuredOutputOptions,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        P: ModelProviderInvoker,
    {
        let agent = self.agent.build();
        agent
            .generate_object_with_provider_and_request_options(
                provider,
                self.prompt,
                options,
                self.messages,
                self.cancellation,
            )
            .await
    }
}

/// Start building a structured object generation request.
#[must_use]
pub fn generate_object_builder<T>() -> GenerateObjectBuilder<T> {
    GenerateObjectBuilder::new()
}

/// Item produced by structured object streaming.
#[derive(Debug)]
pub enum ObjectStreamItem<T> {
    /// Raw assistant text delta from the provider.
    RawDelta(String),
    /// Parsed best-effort JSON object state reconstructed from the accumulated stream buffer.
    ///
    /// Incomplete object/string/array delimiters are closed only when the strict JSON parser
    /// reports an end-of-input error. Syntactically invalid prefixes are never repaired into a
    /// partial value. A value is emitted only when it differs from the prior partial value.
    Partial(serde_json::Value),
    /// Parsed partial JSON object state that currently satisfies the configured schema.
    ///
    /// A value is emitted only when it differs from the prior validated partial value.
    ValidatedPartial(serde_json::Value),
    /// Non-text runtime event forwarded from the underlying model stream.
    Event(AgentEvent),
    /// Non-runtime scoped event forwarded from tool invocation lifecycle or contributions.
    ScopedEvent(ScopedTurnEvent),
    /// Final typed object and the completed runtime response metadata.
    Finished {
        /// Decoded structured object.
        object: T,
        /// Runtime response metadata.
        response: GenerateTextResponse,
    },
    /// Error that terminated object streaming or final decoding.
    Error(BcodeError),
}

pin_project! {
    /// Typed asynchronous stream of structured object events.
    #[derive(Debug)]
    pub struct ObjectStream<T> {
        #[pin]
        stream: Option<TextStream>,
        schema: serde_json::Value,
        buffer: String,
        last_partial: Option<serde_json::Value>,
        last_validated_partial: Option<serde_json::Value>,
        pending: VecDeque<ObjectStreamItem<T>>,
    }
}

impl<T> ObjectStream<T>
where
    T: DeserializeOwned,
{
    fn accept_stream_item(&mut self, item: TextStreamItem) {
        accept_object_stream_item(
            &self.schema,
            &mut self.buffer,
            &mut self.last_partial,
            &mut self.last_validated_partial,
            &mut self.pending,
            item,
        );
    }

    /// Receive the next structured object stream item.
    ///
    /// This convenience method is equivalent to `futures::StreamExt::next` and does not require
    /// importing the extension trait.
    pub async fn next(&mut self) -> Option<ObjectStreamItem<T>> {
        if let Some(item) = self.pending.pop_front() {
            return Some(item);
        }
        loop {
            let item = self.stream.as_mut()?.next().await?;
            self.accept_stream_item(item);
            if let Some(item) = self.pending.pop_front() {
                return Some(item);
            }
        }
    }
}

impl<T> Stream for ObjectStream<T>
where
    T: DeserializeOwned,
{
    type Item = ObjectStreamItem<T>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        if let Some(item) = this.pending.pop_front() {
            return Poll::Ready(Some(item));
        }
        loop {
            let item = match this.stream.as_mut().as_pin_mut() {
                Some(stream) => match stream.poll_next(context) {
                    Poll::Ready(Some(item)) => item,
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Pending => return Poll::Pending,
                },
                None => return Poll::Ready(None),
            };
            accept_object_stream_item(
                this.schema,
                this.buffer,
                this.last_partial,
                this.last_validated_partial,
                this.pending,
                item,
            );
            if let Some(item) = this.pending.pop_front() {
                return Poll::Ready(Some(item));
            }
        }
    }
}

fn accept_object_stream_item<T>(
    schema: &serde_json::Value,
    buffer: &mut String,
    last_partial: &mut Option<serde_json::Value>,
    last_validated_partial: &mut Option<serde_json::Value>,
    pending: &mut VecDeque<ObjectStreamItem<T>>,
    item: TextStreamItem,
) where
    T: DeserializeOwned,
{
    match item {
        TextStreamItem::Event(AgentEvent::TextDelta(delta)) => {
            buffer.push_str(&delta);
            pending.push_back(ObjectStreamItem::RawDelta(delta));
            if let Some(value) = json_value_from_text(buffer) {
                if last_partial.as_ref() != Some(&value) {
                    pending.push_back(ObjectStreamItem::Partial(value.clone()));
                    *last_partial = Some(value.clone());
                }
                if validate_json_schema(schema, &value).is_ok()
                    && last_validated_partial.as_ref() != Some(&value)
                {
                    pending.push_back(ObjectStreamItem::ValidatedPartial(value.clone()));
                    *last_validated_partial = Some(value);
                }
            }
        }
        TextStreamItem::Event(event) => {
            pending.push_back(ObjectStreamItem::Event(event));
        }
        TextStreamItem::ScopedEvent(event) => {
            pending.push_back(ObjectStreamItem::ScopedEvent(event));
        }
        TextStreamItem::Finished(response) => {
            match decode_structured_output(schema, &response.text) {
                Ok(object) => pending.push_back(ObjectStreamItem::Finished { object, response }),
                Err(error) => pending.push_back(ObjectStreamItem::Error(error)),
            }
        }
        TextStreamItem::Error(error) => {
            pending.push_back(ObjectStreamItem::Error(error));
        }
    }
}

/// Builder for streaming structured object generation requests.
#[derive(Debug, Clone)]
pub struct StreamObjectBuilder<T> {
    agent: AgentBuilder,
    prompt: String,
    messages: Vec<ModelMessage>,
    options: Option<StructuredOutputOptions>,
    cancellation: CancellationToken,
    _output: std::marker::PhantomData<T>,
}

impl<T> Default for StreamObjectBuilder<T> {
    fn default() -> Self {
        Self {
            agent: Agent::builder(),
            prompt: String::new(),
            messages: Vec::new(),
            options: None,
            cancellation: CancellationToken::new(),
            _output: std::marker::PhantomData,
        }
    }
}

impl<T> StreamObjectBuilder<T> {
    /// Create a structured object streaming builder with default agent settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure advanced agent behavior for this request.
    ///
    /// The callback exposes the complete [`AgentBuilder`] for tools, hooks, policy, provider
    /// context, plugin runtime, execution options, and other advanced settings.
    #[must_use]
    pub fn configure_agent<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(AgentBuilder) -> AgentBuilder,
    {
        self.agent = configure(self.agent);
        self
    }

    /// Configure the user prompt.
    #[must_use]
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Configure provider/model selection.
    #[must_use]
    pub fn model(mut self, model: impl Into<ModelSelector>) -> Self {
        self.agent = self.agent.model_selector(model);
        self
    }

    /// Configure the system prompt.
    #[must_use]
    pub fn system(mut self, system_prompt: impl Into<String>) -> Self {
        self.agent = self.agent.system(system_prompt);
        self
    }

    /// Configure prior conversation messages.
    #[must_use]
    pub fn messages(mut self, messages: Vec<ModelMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Append one prior conversation message.
    #[must_use]
    pub fn message(mut self, message: ModelMessage) -> Self {
        self.messages.push(message);
        self
    }

    /// Configure structured-output options.
    #[must_use]
    pub fn options(mut self, options: StructuredOutputOptions) -> Self {
        self.options = Some(options);
        self
    }

    /// Configure model parameters.
    #[must_use]
    pub fn parameters(mut self, parameters: ModelParameters) -> Self {
        self.agent = self.agent.parameters(parameters);
        self
    }

    /// Add one metadata key/value pair sent to providers.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.agent = self.agent.metadata(key, value);
        self
    }

    /// Configure turn timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.agent = self.agent.timeout(timeout);
        self
    }

    /// Configure cancellation for this request.
    #[must_use]
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Append model request/response middleware for this structured stream.
    ///
    /// Request middleware runs before provider startup. Response middleware runs on the terminal
    /// response before final schema validation and decoding, without buffering raw deltas.
    #[must_use]
    pub fn middleware_layer<M>(mut self, middleware: M) -> Self
    where
        M: ModelMiddleware + 'static,
    {
        self.agent = self.agent.middleware_layer(middleware);
        self
    }

    /// Run the request with a caller-supplied provider and schema derived from `T`.
    #[must_use]
    pub fn run<P>(self, provider: P) -> ObjectStream<T>
    where
        T: DeserializeOwned + schemars::JsonSchema,
        P: ModelProviderInvoker + 'static,
    {
        self.run_with_options(provider, StructuredOutputOptions::for_type::<T>())
    }

    /// Run the request with explicit structured-output options.
    #[must_use]
    pub fn run_with_options<P>(
        self,
        provider: P,
        options: StructuredOutputOptions,
    ) -> ObjectStream<T>
    where
        T: DeserializeOwned,
        P: ModelProviderInvoker + 'static,
    {
        let prompt = structured_prompt(&self.prompt, &options);
        let StructuredOutputOptions {
            name,
            schema,
            strict,
            max_repairs,
        } = options;
        if max_repairs > 0 {
            return ObjectStream {
                stream: None,
                schema,
                buffer: String::new(),
                last_partial: None,
                last_validated_partial: None,
                pending: VecDeque::from([ObjectStreamItem::Error(
                    BcodeError::StructuredStreamingRepairsUnsupported { max_repairs },
                )]),
            };
        }
        let structured_output = bcode_model::StructuredOutputRequest {
            name,
            schema: schema.clone(),
            strict,
        };
        let agent = self.agent.build();
        let request = agent.turn_request_with_structured_output_messages_and_cancellation(
            prompt,
            Some(structured_output),
            self.messages,
            self.cancellation,
        );
        let stream = TextStream::start(&agent, provider, request);
        ObjectStream {
            stream: Some(stream),
            schema,
            buffer: String::new(),
            last_partial: None,
            last_validated_partial: None,
            pending: VecDeque::new(),
        }
    }
}

/// Start building a structured object streaming request.
#[must_use]
pub fn stream_object_builder<T>() -> StreamObjectBuilder<T> {
    StreamObjectBuilder::new()
}

/// Stream a typed structured object with a caller-supplied provider using default settings.
#[must_use]
pub fn stream_object<T, P>(provider: P, prompt: impl Into<String>) -> ObjectStream<T>
where
    T: DeserializeOwned + schemars::JsonSchema,
    P: ModelProviderInvoker + 'static,
{
    stream_object_builder().prompt(prompt).run(provider)
}

/// Generate text with a caller-supplied provider using default agent settings.
///
/// This is the smallest lean-core text generation helper. It does not launch the TUI, require the
/// daemon, or enable app/bundled-plugin features.
///
/// # Errors
///
/// Returns an error when provider invocation fails, the runtime is cancelled, or the provider
/// reports an error.
pub async fn generate_text<P>(
    provider: &mut P,
    prompt: impl Into<String>,
) -> Result<GenerateTextResponse>
where
    P: ModelProviderInvoker,
{
    Box::pin(generate_text_builder().prompt(prompt).run(provider)).await
}

/// Generate text with prior conversation messages and a caller-supplied provider.
///
/// # Errors
///
/// Returns an error when provider invocation fails, the runtime is cancelled, or the provider
/// reports an error.
pub async fn generate_text_with_messages<P>(
    provider: &mut P,
    messages: Vec<ModelMessage>,
    prompt: impl Into<String>,
) -> Result<GenerateTextResponse>
where
    P: ModelProviderInvoker,
{
    Box::pin(
        generate_text_builder()
            .messages(messages)
            .prompt(prompt)
            .run(provider),
    )
    .await
}

/// Generate text with a caller-supplied provider and cancellation token using default settings.
///
/// # Errors
///
/// Returns an error when provider invocation fails, cancellation is requested, the turn times out,
/// or the provider reports an error.
pub async fn generate_text_with_cancellation<P>(
    provider: &mut P,
    prompt: impl Into<String>,
    cancellation: CancellationToken,
) -> Result<GenerateTextResponse>
where
    P: ModelProviderInvoker,
{
    Box::pin(
        generate_text_builder()
            .prompt(prompt)
            .cancellation(cancellation)
            .run(provider),
    )
    .await
}

/// Stream text with a caller-supplied provider using default agent settings.
///
/// This is the smallest lean-core streaming helper. It returns normalized [`AgentStreamItem`]
/// values and does not launch the TUI, require the daemon, or enable app/bundled-plugin features.
#[must_use]
pub fn stream_text<P>(provider: P, prompt: impl Into<String>) -> TextStream
where
    P: ModelProviderInvoker + 'static,
{
    stream_text_builder().prompt(prompt).run(provider)
}

/// Generate a typed structured object with a caller-supplied provider using default agent settings.
///
/// The helper derives a JSON Schema from `T`, requests structured output, validates the returned
/// JSON locally, and decodes it into `T`.
///
/// # Errors
///
/// Returns an error when provider invocation fails, the runtime is cancelled, the provider reports
/// an error, the model output is not valid JSON, schema validation fails, or decoding into `T`
/// fails.
pub async fn generate_object<T, P>(provider: &mut P, prompt: impl Into<String>) -> Result<T>
where
    T: DeserializeOwned + schemars::JsonSchema,
    P: ModelProviderInvoker,
{
    Box::pin(generate_object_builder().prompt(prompt).run(provider)).await
}

/// Generate a typed structured object with explicit structured-output options.
///
/// # Errors
///
/// Returns an error when provider invocation fails, the runtime is cancelled, the provider reports
/// an error, the model output is not valid JSON, schema validation fails, repair attempts are
/// exhausted, or decoding into `T` fails.
pub async fn generate_object_with_options<T, P>(
    provider: &mut P,
    prompt: impl Into<String>,
    options: StructuredOutputOptions,
) -> Result<T>
where
    T: DeserializeOwned,
    P: ModelProviderInvoker,
{
    Box::pin(
        generate_object_builder()
            .prompt(prompt)
            .run_with_options(provider, options),
    )
    .await
}

/// Generate text with a caller-supplied provider and model selector using default agent settings.
///
/// # Errors
///
/// Returns an error when provider invocation fails, the runtime is cancelled, or the provider
/// reports an error.
pub async fn generate_text_with_model<P>(
    provider: &mut P,
    model: impl Into<ModelSelector>,
    prompt: impl Into<String>,
) -> Result<GenerateTextResponse>
where
    P: ModelProviderInvoker,
{
    Box::pin(
        generate_text_builder()
            .model(model)
            .prompt(prompt)
            .run(provider),
    )
    .await
}

/// Stream text with a caller-supplied provider and model selector using default agent settings.
#[must_use]
pub fn stream_text_with_model<P>(
    provider: P,
    model: impl Into<ModelSelector>,
    prompt: impl Into<String>,
) -> TextStream
where
    P: ModelProviderInvoker + 'static,
{
    stream_text_builder()
        .model(model)
        .prompt(prompt)
        .run(provider)
}

/// Generate a typed structured object with a caller-supplied provider and model selector.
///
/// # Errors
///
/// Returns an error when provider invocation fails, the runtime is cancelled, the provider reports
/// an error, the model output is not valid JSON, schema validation fails, or decoding into `T`
/// fails.
pub async fn generate_object_with_model<T, P>(
    provider: &mut P,
    model: impl Into<ModelSelector>,
    prompt: impl Into<String>,
) -> Result<T>
where
    T: DeserializeOwned + schemars::JsonSchema,
    P: ModelProviderInvoker,
{
    Box::pin(
        generate_object_builder()
            .model(model)
            .prompt(prompt)
            .run(provider),
    )
    .await
}

/// High-level SDK error.
#[derive(Debug, Error)]
pub enum BcodeError {
    /// Agent runtime failed.
    #[error("agent runtime error: {0}")]
    Runtime(#[from] RuntimeError),
    /// No provider is configured for a requested model operation.
    #[error(
        "no model provider is configured; pass a provider to the request, configure an Agent provider factory, or enable `embedded-plugins` and attach a plugin runtime"
    )]
    MissingProvider,
    /// Embedded plugin runtime is required for this operation.
    #[error(
        "embedded plugin runtime is not configured; enable the `embedded-plugins` feature and call `Bcode::builder().plugin_runtime(...)`"
    )]
    MissingPluginRuntime,
    /// Loading Bcode configuration failed.
    #[cfg(feature = "config")]
    #[error("failed to load Bcode provider defaults: {0}")]
    Config(#[from] bcode_config::ConfigError),
    /// Hook callback failed.
    #[error("hook error: {0}")]
    Hook(String),
    /// Response cache lookup, storage, or coordination failed.
    #[error("response cache error: {0}")]
    Cache(String),
    /// Application-owned rate limiter denied a model request.
    #[error("application rate limiter {limiter_id} denied the request: {reason}")]
    RateLimited {
        /// Application-defined limiter identity.
        limiter_id: String,
        /// Actionable denial reason.
        reason: String,
        /// Absolute Unix retry timestamp when known.
        retry_at_unix: Option<u64>,
    },
    /// Application-owned rate limiter failed to evaluate a request.
    #[error("application rate limiter {limiter_id} failed: {message}")]
    RateLimiter {
        /// Application-defined limiter identity.
        limiter_id: String,
        /// Limiter/storage failure detail.
        message: String,
    },
    /// Structured output was invalid or could not be decoded.
    #[error("structured output error: {0}")]
    StructuredOutput(String),
    /// Model output was not valid JSON for structured decoding.
    #[error("structured output invalid JSON: {0}")]
    StructuredInvalidJson(String),
    /// Structured output JSON schema was invalid.
    #[error("structured output invalid schema: {0}")]
    StructuredInvalidSchema(String),
    /// Structured output failed JSON schema validation.
    #[error("structured output schema validation failed: {0}")]
    StructuredSchemaValidation(String),
    /// Structured output could not be deserialized into the requested Rust type.
    #[error("structured output decode failed: {0}")]
    StructuredDecode(String),
    /// Structured output repair attempts were exhausted.
    #[error("structured output repair exhausted: {0}")]
    StructuredRepairExhausted(String),
    /// Structured output repair attempts are not supported for event-native streaming because a
    /// failed final value cannot be retried without retracting already-visible deltas. Configure
    /// zero repairs (the default), or use [`GenerateObjectBuilder`] for buffered repair attempts.
    #[error(
        "structured output streaming does not support {max_repairs} repair attempts; use generate_object_builder for buffered repairs"
    )]
    StructuredStreamingRepairsUnsupported {
        /// Requested number of repair attempts.
        max_repairs: u32,
    },
    /// Session persistence failed.
    #[error("session persistence error: {0}")]
    SessionPersistence(String),
    /// Session state is missing, stale, corrupt, or requires repair.
    #[error("session state requires attention: {0}")]
    SessionState(String),
    /// Memory retrieval or validation failed before provider invocation.
    #[error("memory provider {provider_index} failed ({code})")]
    Memory {
        /// Zero-based configured memory provider.
        provider_index: usize,
        /// Stable failure code without provider payloads.
        code: &'static str,
    },
    /// Explicit memory item failed validation.
    #[error("memory item is invalid ({code})")]
    MemoryValidation {
        /// Stable validation code without memory payloads.
        code: &'static str,
    },
    /// Tool execution failed.
    #[error("tool execution error: {0}")]
    ToolExecution(String),
    /// Provider-specific request extension could not be encoded.
    #[error("provider request extension error: {0}")]
    ProviderExtension(String),
    /// Provider setup or capability discovery failed.
    #[error(
        "provider configuration failed: {0}; verify the provider plugin is enabled and its credentials, endpoint, and model settings are configured"
    )]
    ProviderConfiguration(String),
    /// Plugin loading or execution setup failed.
    #[cfg(feature = "embedded-plugins")]
    #[error("plugin error: {0}")]
    Plugin(#[from] bcode_plugin::PluginLoadError),
}

/// Structured-output generation options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredOutputOptions {
    /// Human-readable object/schema name sent to providers that support native structured output.
    pub name: String,
    /// JSON schema used for provider-native structured output and local validation.
    pub schema: serde_json::Value,
    /// Request strict provider-native schema validation where supported.
    pub strict: bool,
    /// Maximum invalid-output repair attempts after the initial provider response.
    pub max_repairs: u32,
}

impl StructuredOutputOptions {
    /// Build options from a Rust type implementing [`schemars::JsonSchema`].
    ///
    /// # Panics
    ///
    /// Panics only if schemars emits a schema value that cannot be serialized to JSON.
    #[must_use]
    pub fn for_type<T>() -> Self
    where
        T: schemars::JsonSchema,
    {
        let schema = schemars::schema_for!(T);
        Self {
            name: std::any::type_name::<T>()
                .rsplit("::")
                .next()
                .unwrap_or("StructuredOutput")
                .to_string(),
            schema: serde_json::to_value(schema)
                .expect("schemars schema should serialize to JSON value"),
            strict: true,
            max_repairs: 0,
        }
    }

    /// Build options from an explicit JSON schema.
    #[must_use]
    pub fn json_schema(name: impl Into<String>, schema: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            schema,
            strict: true,
            max_repairs: 0,
        }
    }

    /// Configure whether strict provider-native schema validation should be requested.
    #[must_use]
    pub const fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Configure invalid-output repair attempts.
    #[must_use]
    pub const fn with_max_repairs(mut self, max_repairs: u32) -> Self {
        self.max_repairs = max_repairs;
        self
    }
}

/// Context supplied to model-call hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCallContext {
    /// Agent name, when configured.
    pub agent_name: Option<String>,
    /// Selected provider plugin ID, when configured.
    pub provider_plugin_id: Option<String>,
    /// Selected model ID.
    pub model_id: String,
    /// User prompt for the model call.
    pub prompt: String,
}

/// Context supplied after a successful model call.
#[derive(Debug, Clone)]
pub struct ModelCallOutcome {
    /// Generated text response and runtime metadata.
    pub response: GenerateTextResponse,
}

/// Context supplied to tool-call hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallContext {
    /// Agent name, when configured.
    pub agent_name: Option<String>,
    /// Requested tool call.
    pub call: ToolCall,
}

/// Context supplied after a successful tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallOutcome {
    /// Tool execution output including model result and normalized events.
    pub output: ToolExecutionOutput,
}

type ModelBeforeHook = Arc<dyn Fn(&ModelCallContext) -> Result<()> + Send + Sync>;
type ModelAfterHook = Arc<dyn Fn(&ModelCallContext, &ModelCallOutcome) -> Result<()> + Send + Sync>;
type ToolBeforeHook = Arc<dyn Fn(&ToolCallContext) -> Result<()> + Send + Sync>;
type ToolAfterHook = Arc<dyn Fn(&ToolCallContext, &ToolCallOutcome) -> Result<()> + Send + Sync>;

/// Secret-safe deterministic identity for one post-middleware model request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelResponseCacheKey {
    /// Cache-key schema version.
    pub schema_version: u32,
    /// SHA-256 digest of provider/model/config, messages, tools, structured output, and parameters.
    pub digest_hex: String,
}

impl ModelResponseCacheKey {
    /// Derive a key from the complete post-middleware request without retaining prompt contents.
    ///
    /// # Errors
    ///
    /// Returns an error if request identity cannot be serialized.
    pub fn from_request(request: &AgentTurnRequest) -> Result<Self> {
        let mut provider_context = request.provider_context.clone();
        provider_context.env.values_mut().for_each(String::clear);
        if let Some(auth) = &mut provider_context.auth {
            auth.credentials
                .values_mut()
                .for_each(|credential| credential.value.clear());
        }
        for candidate in &mut provider_context.auth_candidates {
            candidate.env.values_mut().for_each(String::clear);
            candidate
                .auth
                .credentials
                .values_mut()
                .for_each(|credential| credential.value.clear());
        }
        let identity = serde_json::json!({
            "schema_version": 1,
            "provider_plugin_id": request.provider_plugin_id,
            "model_id": request.model_id,
            "provider_context": provider_context,
            "system_prompt": request.system_prompt,
            "messages": request.messages,
            "prompt": request.prompt,
            "append_prompt": request.append_prompt,
            "tools": request.tools,
            "tool_call_policy": request.tool_call_policy,
            "structured_output": request.structured_output,
            "parameters": request.parameters,
            "metadata": request.metadata,
            "max_tool_rounds": request.max_tool_rounds,
            "max_repeated_tool_batches": request.max_repeated_tool_batches,
            "timeout_seconds": request.timeout.as_secs(),
            "timeout_subsec_nanos": request.timeout.subsec_nanos(),
            "cache_routing_identity": request.cache_routing_identity,
        });
        let encoded = serde_json::to_vec(&identity).map_err(|error| {
            BcodeError::Cache(format!("failed to encode cache identity: {error}"))
        })?;
        let digest = Sha256::digest(encoded);
        Ok(Self {
            schema_version: 1,
            digest_hex: format!("{digest:x}"),
        })
    }
}

/// Privacy class controlling whether a response may enter shared cache storage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelResponseCachePrivacy {
    /// Request is private to its fully derived identity and may use the configured cache.
    #[default]
    Private,
    /// Application has explicitly approved shared storage under its cache adapter policy.
    Shared,
    /// Bypass lookup and storage entirely.
    NoStore,
}

/// Cache provenance for one SDK response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ModelResponseCacheStatus {
    /// No cache was configured or policy bypassed it.
    #[default]
    Bypassed,
    /// Cache miss; this response was produced by provider/tool execution and stored successfully.
    Stored {
        /// Canonical identity stored by the configured cache.
        key: ModelResponseCacheKey,
    },
    /// Completed response was loaded from cache.
    Hit {
        /// Canonical identity loaded from the configured cache.
        key: ModelResponseCacheKey,
    },
}

/// Application-owned cache adapter for completed non-streaming model responses.
///
/// The adapter owns cache-key construction, expiration, capacity, and storage. It receives the
/// complete post-middleware request, so keys can include provider/model selection, messages,
/// parameters, tools, structured-output schema, and metadata as appropriate.
pub trait ModelResponseCache: Send + Sync {
    /// Return a cached response for this request, or `None` on a cache miss.
    ///
    /// # Errors
    ///
    /// Returns an error when cache lookup fails and the application does not want to continue.
    fn get(&self, request: &AgentTurnRequest) -> Result<Option<GenerateTextResponse>>;

    /// Return privacy behavior for this request.
    #[must_use]
    fn privacy(&self, _request: &AgentTurnRequest) -> ModelResponseCachePrivacy {
        ModelResponseCachePrivacy::Private
    }

    /// Return whether requests advertising tools may be cached.
    ///
    /// Defaults to false because a cache hit skips tool execution and therefore must never suppress
    /// side effects implicitly. Applications opting in must version tool implementations in request
    /// metadata and ensure replay is safe.
    #[must_use]
    fn allow_tool_responses(&self) -> bool {
        false
    }

    /// Store a completed response for this request.
    ///
    /// Responses include complete ordered steps, tool results, provider metadata, and usage. Cache
    /// hits preserve those values; provider usage remains historical and is not billed again.
    ///
    /// # Errors
    ///
    /// Returns an error when cache storage fails and the application treats that failure as
    /// terminal.
    fn put(&self, request: &AgentTurnRequest, response: &GenerateTextResponse) -> Result<()>;

    /// Abort a miss reservation after provider/tool/cache processing fails.
    ///
    /// Adapters implementing single-flight stampede control must wake followers so one can become
    /// the next leader. The compatibility default is a no-op.
    fn abort(&self, _request: &AgentTurnRequest) {}

    /// Invalidate this exact request identity.
    ///
    /// # Errors
    ///
    /// Returns an error when invalidation fails. The compatibility default is a no-op.
    fn invalidate(&self, _request: &AgentTurnRequest) -> Result<()> {
        Ok(())
    }
}

/// Bounded application-owned in-memory response cache with expiration and single-flight misses.
#[derive(Debug)]
pub struct InMemoryModelResponseCache {
    ttl: Duration,
    capacity: std::num::NonZeroUsize,
    single_flight_timeout: Duration,
    allow_tool_responses: bool,
    state: Mutex<InMemoryCacheState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct InMemoryCacheState {
    entries: BTreeMap<ModelResponseCacheKey, InMemoryCacheEntry>,
    in_flight: BTreeMap<ModelResponseCacheKey, Instant>,
    next_sequence: u64,
}

#[derive(Debug, Clone)]
struct InMemoryCacheEntry {
    response: GenerateTextResponse,
    expires_at: Instant,
    sequence: u64,
}

impl InMemoryModelResponseCache {
    /// Create a bounded cache. Both expiration and capacity are mandatory.
    #[must_use]
    pub fn new(ttl: Duration, capacity: std::num::NonZeroUsize) -> Self {
        Self {
            ttl,
            capacity,
            single_flight_timeout: Duration::from_secs(30),
            allow_tool_responses: false,
            state: Mutex::new(InMemoryCacheState::default()),
            changed: Condvar::new(),
        }
    }

    /// Configure how long followers wait before replacing an abandoned miss leader.
    ///
    /// Zero is accepted and disables coalescing rather than permitting an unbounded wait.
    #[must_use]
    pub const fn with_single_flight_timeout(mut self, timeout: Duration) -> Self {
        self.single_flight_timeout = timeout;
        self
    }

    /// Explicitly allow or reject caching requests that advertise tools.
    #[must_use]
    pub const fn with_tool_responses(mut self, allow: bool) -> Self {
        self.allow_tool_responses = allow;
        self
    }

    /// Remove every cached response and wake miss followers.
    ///
    /// # Errors
    ///
    /// Returns an error if cache synchronization was poisoned.
    pub fn invalidate_all(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| BcodeError::Cache(error.to_string()))?;
        state.entries.clear();
        state.in_flight.clear();
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    fn key(request: &AgentTurnRequest) -> Result<ModelResponseCacheKey> {
        ModelResponseCacheKey::from_request(request)
    }
}

impl ModelResponseCache for InMemoryModelResponseCache {
    fn allow_tool_responses(&self) -> bool {
        self.allow_tool_responses
    }

    fn get(&self, request: &AgentTurnRequest) -> Result<Option<GenerateTextResponse>> {
        let key = Self::key(request)?;
        let mut state = self
            .state
            .lock()
            .map_err(|error| BcodeError::Cache(error.to_string()))?;
        loop {
            let now = Instant::now();
            if let Some(entry) = state.entries.get(&key) {
                if entry.expires_at > now {
                    return Ok(Some(entry.response.clone()));
                }
                state.entries.remove(&key);
            }
            let lease_expires = state.in_flight.get(&key).copied();
            if lease_expires.is_none_or(|expires| expires <= now) {
                state
                    .in_flight
                    .insert(key, now + self.single_flight_timeout);
                return Ok(None);
            }
            let wait = lease_expires
                .expect("checked in-flight lease")
                .saturating_duration_since(now);
            let (next_state, _) = self
                .changed
                .wait_timeout(state, wait)
                .map_err(|error| BcodeError::Cache(error.to_string()))?;
            state = next_state;
        }
    }

    fn put(&self, request: &AgentTurnRequest, response: &GenerateTextResponse) -> Result<()> {
        let key = Self::key(request)?;
        let mut state = self
            .state
            .lock()
            .map_err(|error| BcodeError::Cache(error.to_string()))?;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let sequence = state.next_sequence;
        state.entries.insert(
            key.clone(),
            InMemoryCacheEntry {
                response: response.clone(),
                expires_at: Instant::now() + self.ttl,
                sequence,
            },
        );
        state.in_flight.remove(&key);
        while state.entries.len() > self.capacity.get() {
            let oldest = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                state.entries.remove(&oldest);
            }
        }
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    fn abort(&self, request: &AgentTurnRequest) {
        let Ok(key) = Self::key(request) else {
            return;
        };
        if let Ok(mut state) = self.state.lock() {
            state.in_flight.remove(&key);
            drop(state);
            self.changed.notify_all();
        }
    }

    fn invalidate(&self, request: &AgentTurnRequest) -> Result<()> {
        let key = Self::key(request)?;
        let mut state = self
            .state
            .lock()
            .map_err(|error| BcodeError::Cache(error.to_string()))?;
        state.entries.remove(&key);
        state.in_flight.remove(&key);
        drop(state);
        self.changed.notify_all();
        Ok(())
    }
}

fn record_cache_lookup(request: &AgentTurnRequest, cache_hit: bool) {
    tracing::info!(
        target: "bcode::sdk",
        event = "bcode.cache_lookup",
        cache_hit,
        provider_id = request.provider_plugin_id.as_deref().unwrap_or(""),
        model_id = %request.model_id,
    );
}

fn record_cache_bypass(request: &AgentTurnRequest) {
    tracing::info!(
        target: "bcode::sdk",
        event = "bcode.cache_bypass",
        provider_id = request.provider_plugin_id.as_deref().unwrap_or(""),
        model_id = %request.model_id,
    );
}

fn record_cache_store(request: &AgentTurnRequest) {
    tracing::info!(
        target: "bcode::sdk",
        event = "bcode.cache_store",
        provider_id = request.provider_plugin_id.as_deref().unwrap_or(""),
        model_id = %request.model_id,
    );
}

fn record_cost_estimate(
    request: &AgentTurnRequest,
    pricing: Option<&ModelPricingInfo>,
    usage: Option<&TokenUsage>,
) {
    let estimate = pricing
        .zip(usage)
        .and_then(|(pricing, usage)| pricing.estimate_cost(usage));
    let (currency, total_micros, source, cost_available) =
        estimate
            .as_ref()
            .map_or(("", 0, "unavailable", false), |estimate| {
                (
                    estimate.currency.as_str(),
                    estimate.total_micros,
                    pricing_source_label(estimate.source),
                    true,
                )
            });
    tracing::info!(
        target: "bcode::sdk",
        event = "bcode.cost_estimate",
        provider_id = request.provider_plugin_id.as_deref().unwrap_or(""),
        model_id = %request.model_id,
        currency,
        total_micros,
        pricing_source = source,
        cost_available,
        estimated = true,
    );
}

const fn pricing_source_label(source: ModelPricingSource) -> &'static str {
    match source {
        ModelPricingSource::UserOverride => "user_override",
        ModelPricingSource::ProviderApi => "provider_api",
        ModelPricingSource::RemoteCatalog => "remote_catalog",
        ModelPricingSource::BundledCatalog => "bundled_catalog",
        ModelPricingSource::PatternMatch => "pattern_match",
        ModelPricingSource::Unknown => "unknown",
    }
}

async fn response_cache_get(
    cache: Arc<dyn ModelResponseCache>,
    request: AgentTurnRequest,
) -> Result<Option<GenerateTextResponse>> {
    tokio::task::spawn_blocking(move || cache.get(&request))
        .await
        .map_err(|error| BcodeError::Cache(format!("cache lookup task failed: {error}")))?
}

async fn response_cache_put(
    cache: Arc<dyn ModelResponseCache>,
    request: AgentTurnRequest,
    response: GenerateTextResponse,
) -> Result<()> {
    tokio::task::spawn_blocking(move || cache.put(&request, &response))
        .await
        .map_err(|error| BcodeError::Cache(format!("cache storage task failed: {error}")))?
}

async fn response_cache_abort(cache: Arc<dyn ModelResponseCache>, request: AgentTurnRequest) {
    let _ = tokio::task::spawn_blocking(move || cache.abort(&request)).await;
}

/// Typed application-owned model rate-limit decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationRateLimitDecision {
    /// Admit this request.
    Allow,
    /// Reject this request before provider execution.
    Deny {
        /// Actionable application-defined reason.
        reason: String,
        /// Absolute Unix retry timestamp when known.
        retry_at_unix: Option<u64>,
    },
}

/// Storage-agnostic application model rate limiter.
///
/// Implementations own synchronization, persistence, keying, windows, and distributed semantics.
/// The complete request exposes provider/model/config identity and application metadata.
pub trait ApplicationRateLimiter: Send + Sync {
    /// Atomically admit or deny one complete model request.
    ///
    /// # Errors
    ///
    /// Returns an error when limiter storage or evaluation fails. Bcode treats this as terminal and
    /// never starts the provider.
    fn check(
        &self,
        request: &AgentTurnRequest,
    ) -> std::result::Result<ApplicationRateLimitDecision, String>;
}

/// Model middleware backed by an application-owned rate limiter.
#[derive(Clone)]
pub struct RateLimitMiddleware {
    limiter_id: String,
    limiter: Arc<dyn ApplicationRateLimiter>,
}

impl fmt::Debug for RateLimitMiddleware {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RateLimitMiddleware")
            .field("limiter_id", &self.limiter_id)
            .field("limiter", &"<application limiter>")
            .finish()
    }
}

impl RateLimitMiddleware {
    /// Create middleware around an application-owned limiter.
    #[must_use]
    pub fn new(limiter_id: impl Into<String>, limiter: Arc<dyn ApplicationRateLimiter>) -> Self {
        Self {
            limiter_id: limiter_id.into(),
            limiter,
        }
    }
}

impl ModelMiddleware for RateLimitMiddleware {
    fn before_request(&self, request: AgentTurnRequest) -> Result<AgentTurnRequest> {
        match self
            .limiter
            .check(&request)
            .map_err(|message| BcodeError::RateLimiter {
                limiter_id: self.limiter_id.clone(),
                message,
            })? {
            ApplicationRateLimitDecision::Allow => Ok(request),
            ApplicationRateLimitDecision::Deny {
                reason,
                retry_at_unix,
            } => {
                tracing::info!(
                    target: "bcode::sdk",
                    event = "bcode.rate_limit",
                    limiter_id = %self.limiter_id,
                    provider_id = request.provider_plugin_id.as_deref().unwrap_or(""),
                    model_id = %request.model_id,
                    retry_at_unix = retry_at_unix.unwrap_or_default(),
                    reset_available = retry_at_unix.is_some(),
                );
                Err(BcodeError::RateLimited {
                    limiter_id: self.limiter_id.clone(),
                    reason,
                    retry_at_unix,
                })
            }
        }
    }
}

/// Middleware for transport-independent model request and response processing.
///
/// Middleware runs in registration order before provider invocation and in reverse registration
/// order after a successful response. Implementations can enforce budgets/rate limits, redact or
/// transform prompts and messages, attach metadata, inspect usage, populate caches, and emit
/// tracing without depending on TUI or daemon internals.
pub trait ModelMiddleware: Send + Sync {
    /// Transform or reject a complete model request before provider invocation.
    ///
    /// # Errors
    ///
    /// Returns an error to stop the request before provider execution.
    fn before_request(&self, request: AgentTurnRequest) -> Result<AgentTurnRequest> {
        Ok(request)
    }

    /// Inspect, transform, or reject a successful model response.
    ///
    /// `response.text` is the canonical transformed assistant text. After all response middleware
    /// completes, Bcode synchronizes `response.runtime.text` and rebuilds ordered steps from the
    /// retained runtime events plus that canonical text.
    ///
    /// # Errors
    ///
    /// Returns an error to replace the successful response with a middleware failure.
    fn after_response(
        &self,
        _request: &AgentTurnRequest,
        response: GenerateTextResponse,
    ) -> Result<GenerateTextResponse> {
        Ok(response)
    }
}

/// Ordered collection of model middleware.
#[derive(Clone, Default)]
pub struct ModelMiddlewareStack {
    middleware: Vec<Arc<dyn ModelMiddleware>>,
}

impl fmt::Debug for ModelMiddlewareStack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelMiddlewareStack")
            .field("middleware", &self.middleware.len())
            .finish()
    }
}

impl ModelMiddlewareStack {
    /// Create an empty middleware stack.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append middleware to the stack.
    #[must_use]
    pub fn layer<M>(mut self, middleware: M) -> Self
    where
        M: ModelMiddleware + 'static,
    {
        self.middleware.push(Arc::new(middleware));
        self
    }

    /// Append shared middleware to the stack.
    #[must_use]
    pub fn shared(mut self, middleware: Arc<dyn ModelMiddleware>) -> Self {
        self.middleware.push(middleware);
        self
    }

    fn before_request(&self, mut request: AgentTurnRequest) -> Result<AgentTurnRequest> {
        for middleware in &self.middleware {
            request = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                middleware.before_request(request)
            }))
            .map_err(|payload| {
                extension_panic("model middleware before_request", payload.as_ref())
            })??;
        }
        Ok(request)
    }

    fn after_response(
        &self,
        request: &AgentTurnRequest,
        mut response: GenerateTextResponse,
    ) -> Result<GenerateTextResponse> {
        for middleware in self.middleware.iter().rev() {
            response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                middleware.after_response(request, response)
            }))
            .map_err(|payload| {
                extension_panic("model middleware after_response", payload.as_ref())
            })??;
        }
        response.runtime.text.clone_from(&response.text);
        response.steps = generation_steps(&response.runtime);
        Ok(response)
    }
}

/// Typed SDK hook callbacks for logging, tracing, budgets, and safety checks.
#[derive(Clone, Default)]
pub struct AgentHooks {
    before_model: Vec<ModelBeforeHook>,
    after_model: Vec<ModelAfterHook>,
    before_tool: Vec<ToolBeforeHook>,
    after_tool: Vec<ToolAfterHook>,
}

impl fmt::Debug for AgentHooks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentHooks")
            .field("before_model", &self.before_model.len())
            .field("after_model", &self.after_model.len())
            .field("before_tool", &self.before_tool.len())
            .field("after_tool", &self.after_tool.len())
            .finish()
    }
}

impl AgentHooks {
    /// Create an empty hook set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a hook that runs before model calls.
    #[must_use]
    pub fn on_before_model<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ModelCallContext) -> Result<()> + Send + Sync + 'static,
    {
        self.before_model.push(Arc::new(hook));
        self
    }

    /// Add a hook that runs after successful model calls.
    #[must_use]
    pub fn on_after_model<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ModelCallContext, &ModelCallOutcome) -> Result<()> + Send + Sync + 'static,
    {
        self.after_model.push(Arc::new(hook));
        self
    }

    /// Add a hook that runs before tool calls.
    #[must_use]
    pub fn on_before_tool<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ToolCallContext) -> Result<()> + Send + Sync + 'static,
    {
        self.before_tool.push(Arc::new(hook));
        self
    }

    /// Add a hook that runs after successful tool calls.
    #[must_use]
    pub fn on_after_tool<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ToolCallContext, &ToolCallOutcome) -> Result<()> + Send + Sync + 'static,
    {
        self.after_tool.push(Arc::new(hook));
        self
    }

    fn run_before_model(&self, context: &ModelCallContext) -> Result<()> {
        for hook in &self.before_model {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(context)))
                .map_err(|payload| extension_panic("before-model hook", payload.as_ref()))??;
        }
        Ok(())
    }

    fn run_after_model(
        &self,
        context: &ModelCallContext,
        outcome: &ModelCallOutcome,
    ) -> Result<()> {
        for hook in &self.after_model {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(context, outcome)))
                .map_err(|payload| extension_panic("after-model hook", payload.as_ref()))??;
        }
        Ok(())
    }

    fn run_before_tool(&self, context: &ToolCallContext) -> Result<()> {
        for hook in &self.before_tool {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(context)))
                .map_err(|payload| extension_panic("before-tool hook", payload.as_ref()))??;
        }
        Ok(())
    }

    fn run_after_tool(&self, context: &ToolCallContext, outcome: &ToolCallOutcome) -> Result<()> {
        for hook in &self.after_tool {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(context, outcome)))
                .map_err(|payload| extension_panic("after-tool hook", payload.as_ref()))??;
        }
        Ok(())
    }
}

fn extension_panic(boundary: &'static str, payload: &(dyn std::any::Any + Send)) -> BcodeError {
    let detail = payload
        .downcast_ref::<&str>()
        .map_or_else(
            || payload.downcast_ref::<String>().map(String::as_str),
            |message| Some(*message),
        )
        .unwrap_or("non-string panic payload");
    BcodeError::Hook(format!("{boundary} panicked: {detail}"))
}

type InlineToolFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = std::result::Result<ToolInvocationResponse, String>>
            + Send,
    >,
>;
type InlineToolHandler =
    Arc<dyn Fn(ToolInvocationDescriptor, InvocationScope) -> InlineToolFuture + Send + Sync>;
type ProviderFactory = Arc<dyn Fn() -> Box<dyn ModelProviderInvoker> + Send + Sync>;

#[derive(Debug, Default)]
struct DiscardingSdkTurnEventSink;

impl TurnEventSink for DiscardingSdkTurnEventSink {
    fn emit(&self, _event: ScopedTurnEvent) -> bool {
        true
    }
}

/// Explicit policy for renderer-free invocation exchanges.
#[derive(Clone)]
pub enum HeadlessExchangePolicy {
    /// Reject every exchange with a structured failure.
    Reject,
    /// Resolve exchanges with a caller callback.
    Callback(Arc<dyn Fn(ToolExchangeRequest) -> ToolExchangeResolution + Send + Sync>),
    /// Forward exchanges to another broker.
    Forward(Arc<dyn InvocationExchangeBroker>),
    /// Respond to every exchange with the configured opaque payload.
    AutoResponse(serde_json::Value),
}

impl fmt::Debug for HeadlessExchangePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reject => formatter.write_str("Reject"),
            Self::Callback(_) => formatter.write_str("Callback(<callback>)"),
            Self::Forward(_) => formatter.write_str("Forward(<broker>)"),
            Self::AutoResponse(payload) => formatter
                .debug_tuple("AutoResponse")
                .field(payload)
                .finish(),
        }
    }
}

impl InvocationExchangeBroker for HeadlessExchangePolicy {
    fn request(
        &self,
        request: ToolExchangeRequest,
    ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
        match self {
            Self::Reject => Box::pin(async {
                ToolExchangeResolution::Failed {
                    code: "headless_exchange_rejected".to_string(),
                    message: "headless host rejected the invocation exchange".to_string(),
                }
            }),
            Self::Callback(callback) => {
                let resolution = callback(request);
                Box::pin(async move { resolution })
            }
            Self::Forward(broker) => broker.request(request),
            Self::AutoResponse(payload) => {
                let payload = payload.clone();
                Box::pin(async move { ToolExchangeResolution::Responded { payload } })
            }
        }
    }
}

fn finish_text_response(
    finalizer: &mut Option<TextStreamFinalizer>,
    response: AgentTurnResponse,
) -> TextStreamItem {
    let Some(finalizer) = finalizer.take() else {
        let error = BcodeError::Hook(
            "text stream terminal response was finalized more than once".to_string(),
        );
        record_sdk_error(&error);
        return TextStreamItem::Error(error);
    };
    let response = GenerateTextResponse::from(response);
    record_cost_estimate(
        &finalizer.request,
        finalizer.model_pricing.as_ref(),
        response.runtime.usage.as_ref(),
    );
    let response = finalizer
        .middleware
        .after_response(&finalizer.request, response)
        .and_then(|response| {
            finalizer.hooks.run_after_model(
                &finalizer.context,
                &ModelCallOutcome {
                    response: response.clone(),
                },
            )?;
            Ok(response)
        });
    match response {
        Ok(response) => TextStreamItem::Finished(response),
        Err(error) => {
            record_sdk_error(&error);
            TextStreamItem::Error(error)
        }
    }
}

fn record_sdk_error(error: &BcodeError) {
    tracing::info!(
        target: "bcode::sdk",
        event = "bcode.error",
        error_origin = bcode_error_origin(error),
    );
}

const fn bcode_error_origin(error: &BcodeError) -> &'static str {
    match error {
        BcodeError::Runtime(_) => "runtime",
        BcodeError::Hook(_) => "hook",
        BcodeError::Cache(_) => "cache",
        BcodeError::RateLimited { .. } | BcodeError::RateLimiter { .. } => "rate_limit",
        BcodeError::ToolExecution(_) => "tool",
        BcodeError::StructuredOutput(_)
        | BcodeError::StructuredInvalidJson(_)
        | BcodeError::StructuredInvalidSchema(_)
        | BcodeError::StructuredSchemaValidation(_)
        | BcodeError::StructuredDecode(_)
        | BcodeError::StructuredRepairExhausted(_)
        | BcodeError::StructuredStreamingRepairsUnsupported { .. } => "structured_output",
        _ => "sdk",
    }
}

/// Item produced by the generic scoped agent stream.
#[derive(Debug)]
pub enum ScopedAgentStreamItem {
    /// Runtime, invocation lifecycle, or contribution event accepted by the canonical turn scope.
    Event(ScopedTurnEvent),
    /// Completed provider/tool orchestration response.
    Finished(GenerateTextResponse),
    /// Error that terminated provider/tool orchestration.
    Error(BcodeError),
}

pin_project! {
    /// Generic stream for one complete provider/tool turn across every scoped event family.
    pub struct ScopedAgentStream {
        #[pin]
        stream: Option<bcode_agent_runtime::AgentLoopStream>,
        observer: Arc<SdkToolRoundObserver>,
        finalizer: Option<TextStreamFinalizer>,
        pending: Option<ScopedAgentStreamItem>,
    }
}

impl fmt::Debug for ScopedAgentStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopedAgentStream")
            .field("active", &self.stream.is_some())
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl ScopedAgentStream {
    fn map_item(
        &mut self,
        item: bcode_agent_runtime::AgentLoopStreamItem,
    ) -> ScopedAgentStreamItem {
        match item {
            bcode_agent_runtime::AgentLoopStreamItem::Event(event) => {
                ScopedAgentStreamItem::Event(event)
            }
            bcode_agent_runtime::AgentLoopStreamItem::Finished(response) => {
                match finish_text_response(&mut self.finalizer, response) {
                    TextStreamItem::Finished(response) => ScopedAgentStreamItem::Finished(response),
                    TextStreamItem::Error(error) => ScopedAgentStreamItem::Error(error),
                    TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {
                        unreachable!("finalization cannot produce an event")
                    }
                }
            }
            bcode_agent_runtime::AgentLoopStreamItem::Error(error) => {
                self.finalizer = None;
                ScopedAgentStreamItem::Error(
                    self.observer
                        .take_error()
                        .unwrap_or(BcodeError::Runtime(error)),
                )
            }
        }
    }

    /// Receive the next scoped stream item.
    ///
    /// This convenience method is equivalent to `futures::StreamExt::next` and does not require
    /// importing the extension trait.
    pub async fn next(&mut self) -> Option<ScopedAgentStreamItem> {
        if let Some(item) = self.pending.take() {
            return Some(item);
        }
        let item = self.stream.as_mut()?.next().await?;
        let item = self.map_item(item);
        if matches!(
            item,
            ScopedAgentStreamItem::Finished(_) | ScopedAgentStreamItem::Error(_)
        ) {
            self.stream = None;
        }
        Some(item)
    }
}

impl Stream for ScopedAgentStream {
    type Item = ScopedAgentStreamItem;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        if let Some(item) = this.pending.take() {
            return Poll::Ready(Some(item));
        }
        let item = match this.stream.as_mut().as_pin_mut() {
            Some(stream) => match stream.poll_next(context) {
                Poll::Ready(Some(item)) => item,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            },
            None => return Poll::Ready(None),
        };
        let item = map_scoped_stream_item(this.observer, this.finalizer, item);
        if matches!(
            item,
            ScopedAgentStreamItem::Finished(_) | ScopedAgentStreamItem::Error(_)
        ) {
            this.stream.set(None);
        }
        Poll::Ready(Some(item))
    }
}

fn map_scoped_stream_item(
    observer: &SdkToolRoundObserver,
    finalizer: &mut Option<TextStreamFinalizer>,
    item: bcode_agent_runtime::AgentLoopStreamItem,
) -> ScopedAgentStreamItem {
    match item {
        bcode_agent_runtime::AgentLoopStreamItem::Event(event) => {
            ScopedAgentStreamItem::Event(event)
        }
        bcode_agent_runtime::AgentLoopStreamItem::Finished(response) => {
            match finish_text_response(finalizer, response) {
                TextStreamItem::Finished(response) => ScopedAgentStreamItem::Finished(response),
                TextStreamItem::Error(error) => ScopedAgentStreamItem::Error(error),
                TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {
                    unreachable!("finalization cannot produce an event")
                }
            }
        }
        bcode_agent_runtime::AgentLoopStreamItem::Error(error) => {
            *finalizer = None;
            ScopedAgentStreamItem::Error(
                observer.take_error().unwrap_or(BcodeError::Runtime(error)),
            )
        }
    }
}

struct SdkToolRoundObserver {
    agent_name: Option<String>,
    hooks: AgentHooks,
    error: Mutex<Option<BcodeError>>,
}

impl fmt::Debug for SdkToolRoundObserver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SdkToolRoundObserver")
            .field("agent_name", &self.agent_name)
            .field("hooks", &self.hooks)
            .finish_non_exhaustive()
    }
}

impl SdkToolRoundObserver {
    fn new(agent: &Agent) -> Self {
        Self {
            agent_name: agent.name.clone(),
            hooks: agent.hooks.clone(),
            error: Mutex::new(None),
        }
    }
    fn record_error(&self, error: BcodeError) -> RuntimeError {
        let message = error.to_string();
        *self
            .error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
        RuntimeError::HostExtension(message)
    }

    fn take_error(&self) -> Option<BcodeError> {
        self.error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl ToolRoundObserver for SdkToolRoundObserver {
    fn before_tool_batch(&self, calls: &[ToolCall]) -> bcode_agent_runtime::Result<()> {
        for call in calls {
            self.hooks
                .run_before_tool(&ToolCallContext {
                    agent_name: self.agent_name.clone(),
                    call: call.clone(),
                })
                .map_err(|error| self.record_error(error))?;
        }
        Ok(())
    }

    fn after_tool_call(
        &self,
        call: &ToolCall,
        output: &ToolExecutionOutput,
    ) -> bcode_agent_runtime::Result<()> {
        self.hooks
            .run_after_tool(
                &ToolCallContext {
                    agent_name: self.agent_name.clone(),
                    call: call.clone(),
                },
                &ToolCallOutcome {
                    output: output.clone(),
                },
            )
            .map_err(|error| self.record_error(error))
    }
}

fn structured_prompt(prompt: &str, options: &StructuredOutputOptions) -> String {
    format!(
        "{prompt}\n\nReturn only a JSON object that matches this JSON Schema. Do not wrap it in Markdown fences or include explanatory text.\nSchema name: {name}\nSchema:\n{schema}",
        name = options.name,
        schema = options.schema
    )
}

fn repair_prompt(
    original_prompt: &str,
    options: &StructuredOutputOptions,
    invalid_output: &str,
    error: &BcodeError,
) -> String {
    format!(
        "{base}\n\nThe previous response was invalid. Error: {error}\nPrevious response:\n{invalid_output}\n\nReturn a corrected JSON object only.",
        base = structured_prompt(original_prompt, options),
    )
}

fn decode_structured_output<T>(schema: &serde_json::Value, text: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let value = extract_structured_json(text)?;
    validate_json_schema(schema, &value)?;
    serde_json::from_value(value).map_err(|error| BcodeError::StructuredDecode(error.to_string()))
}

fn extract_structured_json(text: &str) -> Result<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => Ok(value),
        Err(error) => {
            let Some(slice) = json_object_slice(text) else {
                return Err(BcodeError::StructuredInvalidJson(format!(
                    "model output was not valid JSON: {error}; output: {text}"
                )));
            };
            serde_json::from_str(slice).map_err(|slice_error| {
                BcodeError::StructuredInvalidJson(format!(
                    "failed to parse JSON object from model output: {slice_error}; output: {text}"
                ))
            })
        }
    }
}

fn json_object_slice(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (start <= end).then_some(&text[start..=end])
}

fn validate_typed_tool_input(
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> std::result::Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("invalid generated typed tool schema: {error}"))?;
    if validator.is_valid(input) {
        return Ok(());
    }
    let errors = validator
        .iter_errors(input)
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "typed tool input failed schema validation: {errors}"
    ))
}

fn typed_tool_success_response<O>(output: O) -> std::result::Result<ToolInvocationResponse, String>
where
    O: Serialize,
{
    let value = serde_json::to_value(output)
        .map_err(|error| format!("failed to serialize typed tool result: {error}"))?;
    let encoded = serde_json::to_string(&value)
        .map_err(|error| format!("failed to encode typed tool result: {error}"))?;
    Ok(ToolInvocationResponse {
        output: encoded.clone(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: Some(ToolInvocationResult::Json { value: encoded }),
    })
}

fn typed_tool_error_response<D>(
    error: ToolApplicationError<D>,
) -> std::result::Result<ToolInvocationResponse, String>
where
    D: Serialize,
{
    let model_message = error.model_message.clone();
    let mut value = serde_json::to_value(error)
        .map_err(|error| format!("failed to serialize typed tool application error: {error}"))?;
    value
        .as_object_mut()
        .expect("ToolApplicationError must serialize as an object")
        .insert(
            "status".to_string(),
            serde_json::Value::String("error".to_string()),
        );
    let encoded = serde_json::to_string(&value)
        .map_err(|error| format!("failed to encode typed tool application error: {error}"))?;
    Ok(ToolInvocationResponse {
        output: model_message,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        result: Some(ToolInvocationResult::Json { value: encoded }),
    })
}

fn json_value_from_text(text: &str) -> Option<serde_json::Value> {
    if let Some(slice) = json_object_slice(text) {
        return serde_json::from_str(slice).ok();
    }
    let start = text.find('{')?;
    let partial = &text[start..];
    let error = serde_json::from_str::<serde_json::Value>(partial).expect_err(
        "a partial structured value without a closing object delimiter cannot be valid JSON",
    );
    if error.classify() != serde_json::error::Category::Eof {
        return None;
    }
    serde_json::from_str(&partial_json_fixer::fix_json(partial)).ok()
}

fn validate_json_schema(schema: &serde_json::Value, value: &serde_json::Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| BcodeError::StructuredInvalidSchema(error.to_string()))?;
    if validator.is_valid(value) {
        Ok(())
    } else {
        let errors = validator
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        Err(BcodeError::StructuredSchemaValidation(errors))
    }
}

fn user_message(text: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text { text: text.into() }],
    }
}

fn assistant_message(text: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::Text { text: text.into() }],
    }
}

fn transcript_messages_from_response(response: &GenerateTextResponse) -> Vec<ModelMessage> {
    let mut messages = Vec::new();
    let mut assistant_content = Vec::new();
    for step in &response.steps {
        match step {
            GenerationStep::Model { text, .. } => {
                if !text.is_empty() {
                    assistant_content.push(ModelContentBlock::Text { text: text.clone() });
                }
            }
            GenerationStep::ToolCall { call, .. } => {
                assistant_content.push(ModelContentBlock::ToolCall { call: call.clone() });
            }
            GenerationStep::ToolResult { result, .. } => {
                if !assistant_content.is_empty() {
                    messages.push(ModelMessage {
                        role: MessageRole::Assistant,
                        content: std::mem::take(&mut assistant_content),
                    });
                }
                messages.push(ModelMessage {
                    role: MessageRole::Tool,
                    content: vec![ModelContentBlock::ToolResult {
                        result: result.clone(),
                    }],
                });
            }
            GenerationStep::FinalResponse { .. } => {}
        }
    }
    if !assistant_content.is_empty() {
        messages.push(ModelMessage {
            role: MessageRole::Assistant,
            content: assistant_content,
        });
    }
    if messages.is_empty() && !response.text.is_empty() {
        messages.push(assistant_message(response.text.clone()));
    }
    messages
}

fn text_from_message(message: &ModelMessage) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ModelContentBlock::Text { text } => Some(text.clone()),
        _ => None,
    })
}

/// Policy for runtime tool invocation failures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolFailurePolicy {
    /// Fail the complete turn immediately.
    #[default]
    FailTurn,
    /// Convert the failure into an error tool result so the model can recover.
    ReturnToModel,
}

#[derive(Clone)]
struct SdkToolInvoker {
    handlers: BTreeMap<String, InlineToolHandler>,
    failure_policy: ToolFailurePolicy,
    #[cfg(feature = "embedded-plugins")]
    session_id: SessionId,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl fmt::Debug for SdkToolInvoker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("SdkToolInvoker");
        debug.field("tools", &self.handlers.keys().collect::<Vec<_>>());
        debug.field("failure_policy", &self.failure_policy);
        #[cfg(feature = "embedded-plugins")]
        debug.field("session_id", &self.session_id);
        #[cfg(feature = "embedded-plugins")]
        debug.field("plugins", &self.plugins.is_some());
        debug.finish()
    }
}

impl ToolInvoker for SdkToolInvoker {
    fn prepare_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        request: &'a ToolPreparationRequest,
        _scope: &'a PreparationScope,
    ) -> RuntimeFuture<'a, ToolPreparationResponse> {
        match &tool.source {
            ToolSource::Inline => {
                let result = bcode_agent_profile::prepare_tool_policy(request, &tool.definition)
                    .map_err(|message| RuntimeError::ToolPreparation {
                        tool_name: request.invocation.tool_name.clone(),
                        message,
                    });
                Box::pin(async move { result })
            }
            ToolSource::Plugin { plugin_id } => {
                #[cfg(feature = "embedded-plugins")]
                {
                    Box::pin(prepare_plugin_tool(
                        self.plugins.as_ref(),
                        plugin_id,
                        request,
                        self.session_id,
                    ))
                }
                #[cfg(not(feature = "embedded-plugins"))]
                {
                    Box::pin(async move {
                        Err(RuntimeError::ToolPreparation {
                            tool_name: request.invocation.tool_name.clone(),
                            message: format!(
                                "plugin-backed preparation for plugin '{plugin_id}' requires embedded-plugins"
                            ),
                        })
                    })
                }
            }
        }
    }

    fn invoke_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        invocation: &'a PreparedToolInvocation,
        scope: &'a InvocationScope,
    ) -> RuntimeFuture<'a, ToolInvocationResponse> {
        let descriptor = invocation.invocation.clone();
        Box::pin(async move {
            match &tool.source {
                ToolSource::Inline => {
                    let handler = self.handlers.get(&tool.definition.name).ok_or_else(|| {
                        RuntimeError::ToolExecution {
                            tool_name: tool.definition.name.clone(),
                            message: "inline tool handler not found".to_string(),
                        }
                    })?;
                    match handler(descriptor, scope.clone()).await {
                        Ok(response) => Ok(response),
                        Err(message) if self.failure_policy == ToolFailurePolicy::ReturnToModel => {
                            Ok(ToolInvocationResponse {
                                output: message,
                                is_error: true,
                                content: Vec::new(),
                                full_output: None,
                                result: None,
                            })
                        }
                        Err(message) => Err(RuntimeError::ToolExecution {
                            tool_name: tool.definition.name.clone(),
                            message,
                        }),
                    }
                }
                ToolSource::Plugin { plugin_id } => {
                    #[cfg(feature = "embedded-plugins")]
                    {
                        let preparation_descriptor = invocation.preparation.descriptor.clone();
                        execute_plugin_tool(
                            self.plugins.as_ref(),
                            plugin_id,
                            &descriptor,
                            preparation_descriptor,
                            scope,
                            self.session_id,
                        )
                        .await
                    }
                    #[cfg(not(feature = "embedded-plugins"))]
                    {
                        Err(RuntimeError::ToolExecution {
                            tool_name: descriptor.tool_name.clone(),
                            message: format!(
                                "plugin-backed tool routing for plugin '{plugin_id}' requires embedded-plugins"
                            ),
                        })
                    }
                }
            }
        })
    }
}

#[cfg(feature = "embedded-plugins")]
async fn prepare_plugin_tool(
    plugins: Option<&bcode_plugin::PluginRuntimeHost>,
    plugin_id: &str,
    request: &ToolPreparationRequest,
    session_id: SessionId,
) -> std::result::Result<ToolPreparationResponse, RuntimeError> {
    let plugins = plugins.ok_or_else(|| RuntimeError::ToolPreparation {
        tool_name: request.invocation.tool_name.clone(),
        message: "embedded plugin runtime is not configured".to_string(),
    })?;
    plugins
        .invoke_service_json_scoped(
            plugin_id,
            bcode_tool::TOOL_SERVICE_INTERFACE_ID,
            bcode_tool::OP_PREPARE_TOOL,
            request,
            bcode_plugin::PluginInvocationScope::session(session_id.to_string())
                .with_work_id(request.invocation.invocation_id.clone()),
        )
        .await
        .map_err(|error| RuntimeError::ToolPreparation {
            tool_name: request.invocation.tool_name.clone(),
            message: error.to_string(),
        })
}

#[cfg(feature = "embedded-plugins")]
async fn execute_plugin_tool(
    plugins: Option<&bcode_plugin::PluginRuntimeHost>,
    plugin_id: &str,
    descriptor: &ToolInvocationDescriptor,
    preparation_descriptor: serde_json::Value,
    scope: &InvocationScope,
    session_id: SessionId,
) -> std::result::Result<ToolInvocationResponse, RuntimeError> {
    let plugins = plugins.ok_or_else(|| RuntimeError::ToolExecution {
        tool_name: descriptor.tool_name.clone(),
        message: "embedded plugin runtime is not configured".to_string(),
    })?;
    let request = bcode_tool::ToolInvocationRequest {
        tool_call_id: descriptor.invocation_id.clone(),
        name: descriptor.tool_name.clone(),
        arguments: descriptor.arguments.clone(),
        preparation_descriptor,
        cwd: None,
        artifact_dir: None,
    };
    let payload = serde_json::to_vec(&request).map_err(|error| RuntimeError::ToolExecution {
        tool_name: descriptor.tool_name.clone(),
        message: error.to_string(),
    })?;
    let plugin_scope = bcode_plugin::PluginInvocationScope::session(session_id.to_string())
        .with_turn_id(scope.turn().turn_id())
        .with_work_id(scope.invocation_id());
    let invocation_scope = scope.clone();
    let handle = tokio::runtime::Handle::current();
    let bridge = bcode_plugin::PluginInvocationBridge::new(move |request, _| {
        handle.block_on(route_plugin_bridge_request(&invocation_scope, request))
    });
    let mut invocation = plugins
        .invoke_service_with_events_and_bridge_scoped(
            plugin_id,
            bcode_tool::TOOL_SERVICE_INTERFACE_ID,
            bcode_tool::OP_INVOKE_TOOL,
            payload,
            plugin_scope,
            Some(bridge),
        )
        .await
        .map_err(|error| RuntimeError::ToolExecution {
            tool_name: descriptor.tool_name.clone(),
            message: error.to_string(),
        })?;
    if !scope.register_cancellation(Arc::new(PluginInvocationCancellation(
        invocation.cancel.clone(),
    ))) {
        invocation.cancel.cancel();
        return Err(RuntimeError::Cancelled);
    }
    loop {
        match invocation
            .next_event()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool_name: descriptor.tool_name.clone(),
                message: error.to_string(),
            })? {
            bcode_plugin::StreamingServiceInvocationEvent::Event(payload) => {
                if let Ok(event) =
                    serde_json::from_slice::<bcode_tool::ToolInvocationLifecycleEvent>(&payload)
                {
                    let _ = scope.emit_lifecycle(event);
                } else if let Ok(event) =
                    serde_json::from_slice::<bcode_tool::ToolContributionEvent>(&payload)
                {
                    let _ = scope.emit_contribution(event);
                }
            }
            bcode_plugin::StreamingServiceInvocationEvent::Response(response) => {
                let response = response.map_err(|error| RuntimeError::ToolExecution {
                    tool_name: descriptor.tool_name.clone(),
                    message: error.to_string(),
                })?;
                return bcode_plugin::decode_service_response(response).map_err(|error| {
                    RuntimeError::ToolExecution {
                        tool_name: descriptor.tool_name.clone(),
                        message: error.to_string(),
                    }
                });
            }
        }
    }
}

#[cfg(feature = "embedded-plugins")]
#[derive(Debug)]
struct PluginInvocationCancellation(bcode_plugin::PluginInvocationCancelHandle);

#[cfg(feature = "embedded-plugins")]
impl bcode_agent_runtime::InvocationCancellation for PluginInvocationCancellation {
    fn request_cancel(&self) {
        self.0.cancel();
    }
}

#[cfg(feature = "embedded-plugins")]
async fn route_plugin_bridge_request(
    scope: &InvocationScope,
    request: ServiceBridgeRequest,
) -> std::result::Result<ServiceBridgeResponse, String> {
    Ok(match request {
        ServiceBridgeRequest::Exchange(request) => {
            ServiceBridgeResponse::Exchange(scope.request_exchange(request).await)
        }
        ServiceBridgeRequest::ReceiveInput {
            invocation_id,
            timeout_ms,
        } => {
            if invocation_id != scope.invocation_id() {
                return Err("input request invocation ID does not match runtime scope".to_string());
            }
            let receive = scope.receive_input();
            ServiceBridgeResponse::Input(if let Some(timeout_ms) = timeout_ms {
                tokio::time::timeout(Duration::from_millis(timeout_ms), receive)
                    .await
                    .unwrap_or(ToolInvocationInputResolution::TimedOut)
            } else {
                receive.await
            })
        }
        ServiceBridgeRequest::InvokeService(request) => {
            ServiceBridgeResponse::Service(scope.invoke_service(request).await)
        }
        ServiceBridgeRequest::WriteArtifact(request) => {
            ServiceBridgeResponse::Artifact(scope.write_artifact(request).await)
        }
    })
}

/// Provider invoker backed by a loaded Bcode plugin runtime.
#[cfg(feature = "embedded-plugins")]
#[derive(Debug, Clone)]
pub struct PluginModelProviderInvoker {
    plugins: bcode_plugin::PluginRuntimeHost,
}

#[cfg(feature = "embedded-plugins")]
impl PluginModelProviderInvoker {
    /// Create a provider invoker from a loaded plugin runtime host.
    #[must_use]
    pub const fn new(plugins: bcode_plugin::PluginRuntimeHost) -> Self {
        Self { plugins }
    }

    fn resolve_provider(
        &self,
        provider_plugin_id: Option<&str>,
    ) -> std::result::Result<String, RuntimeError> {
        provider_plugin_id.map_or_else(
            || {
                self.plugins
                    .registry()
                    .service_registry()
                    .unique_provider(MODEL_PROVIDER_INTERFACE_ID)
                    .map(str::to_string)
                    .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
            },
            |provider_plugin_id| Ok(provider_plugin_id.to_string()),
        )
    }
}

#[cfg(feature = "embedded-plugins")]
impl ModelProviderInvoker for PluginModelProviderInvoker {
    fn start_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async move {
            let provider_plugin_id = self.resolve_provider(provider_plugin_id)?;
            self.plugins
                .invoke_service_json_scoped(
                    &provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_START_TURN,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            let provider_plugin_id = self.resolve_provider(provider_plugin_id)?;
            self.plugins
                .invoke_service_json_scoped(
                    &provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_POLL_TURN_EVENTS,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
        })
    }

    fn cancel_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async move {
            let provider_plugin_id = self.resolve_provider(provider_plugin_id)?;
            self.plugins
                .invoke_service_json_scoped(
                    &provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_CANCEL_TURN,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
        })
    }

    fn finish_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async move {
            let provider_plugin_id = self.resolve_provider(provider_plugin_id)?;
            self.plugins
                .invoke_service_json_scoped(
                    &provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_FINISH_TURN,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
        })
    }
}

/// Source that registered a provider in the SDK registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderRegistrationSource {
    /// Provider registration came from composed configuration.
    Configuration,
    /// Application code registered the provider explicitly.
    Explicit,
    /// Provider discovery registered the provider.
    Discovery,
}

/// Provider registry entry used by the SDK facade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRegistration {
    /// Provider plugin ID.
    pub provider_plugin_id: String,
    /// Provider registration provenance.
    pub source: ProviderRegistrationSource,
    /// Provider capability metadata, when discovered or supplied.
    pub capabilities: Option<ProviderCapabilities>,
    /// Provider model metadata, when discovered or supplied.
    pub models: Option<ModelList>,
}

/// Public provenance for one resolved provider/model choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelSelectionSource {
    /// Base configuration selected the value.
    Config,
    /// A named model profile selected the value.
    Profile {
        /// Selected profile name.
        name: String,
    },
    /// A model alias selected or rewrote the value.
    Alias {
        /// Alias that selected or rewrote the value.
        name: String,
    },
    /// Provider authentication configuration implied the provider.
    AuthConfig,
    /// An exact environment variable selected the value.
    Environment {
        /// Environment variable that selected the value.
        variable: String,
    },
    /// An SDK registry default explicitly selected the value.
    ExplicitRegistration,
    /// An agent/request builder overrode the inherited value.
    PerRequest,
}

/// Winning source for provider and model components of a selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelectionProvenance {
    /// Winning provider source, or `None` for an unqualified model.
    pub provider: Option<ModelSelectionSource>,
    /// Winning model source.
    pub model: Option<ModelSelectionSource>,
}

#[cfg(feature = "config")]
impl From<bcode_config::ModelSelectionSource> for ModelSelectionSource {
    fn from(source: bcode_config::ModelSelectionSource) -> Self {
        match source {
            bcode_config::ModelSelectionSource::Config => Self::Config,
            bcode_config::ModelSelectionSource::Profile { name } => Self::Profile { name },
            bcode_config::ModelSelectionSource::Alias { name } => Self::Alias { name },
            bcode_config::ModelSelectionSource::AuthConfig => Self::AuthConfig,
            bcode_config::ModelSelectionSource::Environment { variable } => {
                Self::Environment { variable }
            }
        }
    }
}

/// Final effective provider/model selection report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelectionReport {
    /// Final effective provider/model selector.
    pub selector: ModelSelector,
    /// Winning source of each selector component.
    pub provenance: ModelSelectionProvenance,
    /// How the selected provider entered the registry, when known.
    pub registration_source: Option<ProviderRegistrationSource>,
    /// Source of selected model metadata, when known.
    pub model_metadata_source: Option<ModelMetadataSource>,
    /// Selected model pricing metadata used for labeled SDK cost estimates, when known.
    pub model_pricing: Option<ModelPricingInfo>,
}

/// Explicit SDK provider registry/default facade.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderRegistration>,
    default_model: Option<ModelSelector>,
    default_provenance: Option<ModelSelectionProvenance>,
}

impl ProviderRegistry {
    /// Create an empty provider registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build provider defaults from an already loaded Bcode configuration.
    ///
    /// Process environment provider/model overrides are applied using the same resolution rules as
    /// the Bcode application.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_config(config: &bcode_config::BcodeConfig) -> Self {
        Self::from_config_environment(config, &bcode_config::ProcessConfigEnvironment)
    }

    /// Build provider defaults from a Bcode configuration and explicit environment.
    ///
    /// This deterministic variant is useful for applications that snapshot their environment and
    /// for tests that must not mutate process-global environment variables.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_config_environment(
        config: &bcode_config::BcodeConfig,
        environment: &impl bcode_config::ConfigEnvironment,
    ) -> Self {
        let selection = config.resolved_model_selection_with_environment(environment);
        Self::from_resolved_model_selection(&selection)
    }

    /// Load Bcode configuration from its normal layered paths and build provider defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if an existing config layer cannot be read, parsed, or composed.
    #[cfg(feature = "config")]
    pub fn load() -> Result<Self> {
        Ok(Self::from_config(&bcode_config::load_config()?))
    }

    #[cfg(feature = "config")]
    fn from_resolved_model_selection(selection: &bcode_config::ResolvedModelSelection) -> Self {
        let mut registry = Self::new();
        if let Some(provider_plugin_id) = selection.provider_plugin_id.as_deref() {
            registry = registry.provider(provider_plugin_id);
            if let Some(registration) = registry.providers.get_mut(provider_plugin_id) {
                registration.source = ProviderRegistrationSource::Configuration;
            }
        }
        if let Some(model_id) = selection.model_id.as_deref() {
            registry.default_model = Some(selection.provider_plugin_id.as_deref().map_or_else(
                || ModelSelector::new(model_id),
                |provider_plugin_id| ModelSelector::with_provider(provider_plugin_id, model_id),
            ));
            registry.default_provenance = Some(ModelSelectionProvenance {
                provider: selection.provider_source.clone().map(Into::into),
                model: selection.model_source.clone().map(Into::into),
            });
        }
        registry
    }

    /// Negotiate one granular feature for a selected provider/model.
    #[must_use]
    pub fn feature_support(
        &self,
        selector: &ModelSelector,
        feature: RequestedModelFeature,
    ) -> NegotiatedFeatureSupport {
        let Some(provider_id) = selector.provider_plugin_id.as_deref() else {
            return NegotiatedFeatureSupport::Unknown {
                scope: CapabilityScope::Provider,
            };
        };
        let Some(registration) = self.providers.get(provider_id) else {
            return NegotiatedFeatureSupport::Unknown {
                scope: CapabilityScope::Provider,
            };
        };
        let Some(provider) = registration.capabilities.as_ref() else {
            return NegotiatedFeatureSupport::Unknown {
                scope: CapabilityScope::Provider,
            };
        };
        let Some(model) = registration.models.as_ref().and_then(|models| {
            models
                .models
                .iter()
                .find(|model| model.model_id == selector.model_id)
        }) else {
            return NegotiatedFeatureSupport::Unknown {
                scope: CapabilityScope::Model,
            };
        };
        provider
            .feature_support
            .negotiate(&model.feature_support, feature)
    }

    /// Return selected provider/model capability support for parallel tool calls.
    #[must_use]
    pub fn parallel_tool_capabilities(
        &self,
        selector: &ModelSelector,
    ) -> bcode_model::ParallelToolCallCapabilities {
        let feature = RequestedModelFeature::ToolChoice(ToolChoiceMode::Parallel);
        let guaranteed = self.feature_support(selector, feature).is_guaranteed();
        bcode_model::ParallelToolCallCapabilities {
            provider: guaranteed,
            model: guaranteed,
            runtime: true,
        }
    }

    /// Register a provider by plugin ID.
    #[must_use]
    pub fn provider(mut self, provider_plugin_id: impl Into<String>) -> Self {
        let provider_plugin_id = provider_plugin_id.into();
        self.providers
            .entry(provider_plugin_id.clone())
            .or_insert_with(|| ProviderRegistration {
                provider_plugin_id,
                source: ProviderRegistrationSource::Explicit,
                capabilities: None,
                models: None,
            });
        self
    }

    /// Mark one provider registration as discovered rather than explicitly supplied.
    #[must_use]
    pub fn discovered_provider(mut self, provider_plugin_id: impl Into<String>) -> Self {
        let provider_plugin_id = provider_plugin_id.into();
        self.providers
            .entry(provider_plugin_id.clone())
            .and_modify(|registration| registration.source = ProviderRegistrationSource::Discovery)
            .or_insert_with(|| ProviderRegistration {
                provider_plugin_id,
                source: ProviderRegistrationSource::Discovery,
                capabilities: None,
                models: None,
            });
        self
    }

    /// Discover and register one embedded provider's capabilities and model metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when either typed provider operation cannot be invoked or decoded.
    #[cfg(feature = "embedded-plugins")]
    pub async fn discover_provider(
        mut self,
        plugins: &bcode_plugin::PluginRuntimeHost,
        provider_plugin_id: impl Into<String>,
        provider_context: ProviderRequestContext,
        selected_model_id: Option<String>,
    ) -> Result<Self> {
        let provider_plugin_id = provider_plugin_id.into();
        let capabilities = plugins
            .invoke_service_json_scoped::<(), ProviderCapabilities>(
                &provider_plugin_id,
                MODEL_PROVIDER_INTERFACE_ID,
                OP_CAPABILITIES,
                &(),
                bcode_plugin::PluginInvocationScope::Global,
            )
            .await
            .map_err(|error| BcodeError::ProviderConfiguration(error.to_string()))?;
        let models = plugins
            .invoke_service_json_scoped::<bcode_model::ModelListRequest, ModelList>(
                &provider_plugin_id,
                MODEL_PROVIDER_INTERFACE_ID,
                OP_MODELS,
                &bcode_model::ModelListRequest {
                    provider_context,
                    selected_model_id,
                },
                bcode_plugin::PluginInvocationScope::Global,
            )
            .await
            .map_err(|error| BcodeError::ProviderConfiguration(error.to_string()))?;
        self = self
            .discovered_provider(&provider_plugin_id)
            .provider_capabilities(capabilities)
            .provider_models(&provider_plugin_id, models);
        if let Some(registration) = self.providers.get_mut(&provider_plugin_id) {
            registration.source = ProviderRegistrationSource::Discovery;
        }
        Ok(self)
    }

    /// Register provider capabilities.
    #[must_use]
    pub fn provider_capabilities(mut self, capabilities: ProviderCapabilities) -> Self {
        let provider_plugin_id = capabilities.provider_id.clone();
        let entry = self
            .providers
            .entry(provider_plugin_id.clone())
            .or_insert_with(|| ProviderRegistration {
                provider_plugin_id,
                source: ProviderRegistrationSource::Explicit,
                capabilities: None,
                models: None,
            });
        entry.capabilities = Some(capabilities);
        self
    }

    /// Register provider model metadata.
    #[must_use]
    pub fn provider_models(
        mut self,
        provider_plugin_id: impl Into<String>,
        models: ModelList,
    ) -> Self {
        let provider_plugin_id = provider_plugin_id.into();
        let entry = self
            .providers
            .entry(provider_plugin_id.clone())
            .or_insert_with(|| ProviderRegistration {
                provider_plugin_id,
                source: ProviderRegistrationSource::Explicit,
                capabilities: None,
                models: None,
            });
        entry.models = Some(models);
        self
    }

    /// Configure the default provider/model selector used by agents built from [`Bcode`].
    #[must_use]
    pub fn default_model(mut self, model: impl Into<ModelSelector>) -> Self {
        let model = model.into();
        let has_provider = model.provider_plugin_id.is_some();
        self.default_model = Some(model);
        self.default_provenance = Some(ModelSelectionProvenance {
            provider: has_provider.then_some(ModelSelectionSource::ExplicitRegistration),
            model: Some(ModelSelectionSource::ExplicitRegistration),
        });
        self
    }

    /// Return the default model selector.
    #[must_use]
    pub const fn default_model_selector(&self) -> Option<&ModelSelector> {
        self.default_model.as_ref()
    }

    /// Return the winning sources for the default provider/model selection.
    #[must_use]
    pub const fn default_selection_provenance(&self) -> Option<&ModelSelectionProvenance> {
        self.default_provenance.as_ref()
    }

    /// Report the effective default provider/model and all available provenance.
    #[must_use]
    pub fn default_selection_report(&self) -> Option<ModelSelectionReport> {
        let selector = self.default_model.clone()?;
        Some(self.selection_report(
            selector,
            self.default_provenance.clone().unwrap_or_default(),
        ))
    }

    /// Report an effective per-request selector and provenance.
    #[must_use]
    pub fn selection_report(
        &self,
        selector: ModelSelector,
        provenance: ModelSelectionProvenance,
    ) -> ModelSelectionReport {
        let registration = selector
            .provider_plugin_id
            .as_deref()
            .and_then(|provider_id| self.providers.get(provider_id));
        let model_metadata_source = registration
            .and_then(|registration| registration.models.as_ref())
            .and_then(|models| {
                models
                    .models
                    .iter()
                    .find(|model| model.model_id == selector.model_id)
            })
            .and_then(|model| model.metadata_source);
        let model_pricing = registration
            .and_then(|registration| registration.models.as_ref())
            .and_then(|models| {
                models
                    .models
                    .iter()
                    .find(|model| model.model_id == selector.model_id)
            })
            .and_then(|model| model.pricing.clone());
        ModelSelectionReport {
            selector,
            provenance,
            registration_source: registration.map(|registration| registration.source),
            model_metadata_source,
            model_pricing,
        }
    }

    /// Return a registered provider by plugin ID.
    #[must_use]
    pub fn provider_registration(&self, provider_plugin_id: &str) -> Option<&ProviderRegistration> {
        self.providers.get(provider_plugin_id)
    }

    /// Return registered provider IDs.
    pub fn provider_ids(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }

    /// Return all registered providers.
    pub fn providers(&self) -> impl Iterator<Item = &ProviderRegistration> {
        self.providers.values()
    }
}

/// Bcode SDK runtime mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BcodeMode {
    /// Run directly in-process without daemon IPC.
    #[default]
    Embedded,
    /// Route operations through a daemon-backed client.
    Daemon,
}

/// Plugin-owned tool discovered from a manifest-declared tool service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPluginTool {
    /// Plugin that owns and executes the tool.
    pub plugin_id: String,
    /// Complete plugin-provided tool definition and metadata.
    pub definition: ToolDefinition,
}

/// Top-level SDK handle.
#[derive(Debug, Clone)]
pub struct Bcode {
    mode: BcodeMode,
    runtime: AgentRuntime,
    provider_registry: ProviderRegistry,
    #[cfg(feature = "daemon-client")]
    daemon_client: Option<BcodeClient>,
    #[cfg(feature = "embedded-plugins")]
    provider: Option<PluginModelProviderInvoker>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl Bcode {
    /// Start building a Bcode SDK handle.
    #[must_use]
    pub fn builder() -> BcodeBuilder {
        BcodeBuilder::default()
    }

    /// Build an SDK handle using provider/model defaults from Bcode's layered configuration.
    ///
    /// Enable the `config` feature to use this constructor. This configures selection defaults;
    /// embedded provider execution still requires `embedded-plugins` and a plugin runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if an existing config layer cannot be read, parsed, or composed.
    #[cfg(feature = "config")]
    pub fn configured() -> Result<Self> {
        Ok(Self::builder().load_provider_defaults()?.build())
    }

    /// Discover one embedded provider into this SDK's registry.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded runtime is configured or typed discovery fails.
    #[cfg(feature = "embedded-plugins")]
    pub async fn discover_provider(
        mut self,
        provider_plugin_id: impl Into<String>,
        provider_context: ProviderRequestContext,
        selected_model_id: Option<String>,
    ) -> Result<Self> {
        let plugins = self
            .plugins
            .as_ref()
            .ok_or(BcodeError::MissingPluginRuntime)?;
        self.provider_registry = self
            .provider_registry
            .discover_provider(
                plugins,
                provider_plugin_id,
                provider_context,
                selected_model_id,
            )
            .await?;
        Ok(self)
    }

    /// Start building an agent attached to this SDK handle.
    #[must_use]
    pub fn agent(&self) -> AgentBuilder {
        let builder = AgentBuilder::default().runtime(self.runtime.clone());
        #[cfg(feature = "embedded-plugins")]
        let builder = if let Some(provider) = self.provider.clone() {
            builder.provider_invoker(provider)
        } else {
            builder
        };
        #[cfg(feature = "embedded-plugins")]
        let builder = if let Some(plugins) = self.plugins.clone() {
            builder.plugin_runtime(plugins)
        } else {
            builder
        };
        if let Some(report) = self.provider_registry.default_selection_report() {
            let selector = report.selector.clone();
            builder.selection_report(report).parallel_tool_capabilities(
                self.provider_registry.parallel_tool_capabilities(&selector),
            )
        } else {
            builder
        }
    }

    /// Return the configured provider registry.
    #[must_use]
    pub const fn provider_registry(&self) -> &ProviderRegistry {
        &self.provider_registry
    }

    /// Return the default model selector from the configured provider registry.
    #[must_use]
    pub const fn default_model_selector(&self) -> Option<&ModelSelector> {
        self.provider_registry.default_model_selector()
    }

    /// Return provenance for the configured default model selection.
    #[must_use]
    pub const fn default_selection_provenance(&self) -> Option<&ModelSelectionProvenance> {
        self.provider_registry.default_selection_provenance()
    }

    /// Return the configured runtime mode.
    #[must_use]
    pub const fn mode(&self) -> BcodeMode {
        self.mode
    }

    /// Return the configured daemon client when daemon-client mode is enabled.
    #[cfg(feature = "daemon-client")]
    #[must_use]
    pub const fn daemon_client(&self) -> Option<&BcodeClient> {
        self.daemon_client.as_ref()
    }
    /// Return provider capabilities for an embedded provider plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded plugin runtime is configured or the provider service
    /// cannot be invoked or decoded.
    #[cfg(feature = "embedded-plugins")]
    pub async fn provider_capabilities(
        &self,
        provider_plugin_id: impl AsRef<str>,
    ) -> Result<ProviderCapabilities> {
        let plugins = self
            .plugins
            .as_ref()
            .ok_or(BcodeError::MissingPluginRuntime)?;
        plugins
            .invoke_service_json_scoped(
                provider_plugin_id.as_ref(),
                MODEL_PROVIDER_INTERFACE_ID,
                OP_CAPABILITIES,
                &serde_json::Value::Null,
                bcode_plugin::PluginInvocationScope::Global,
            )
            .await
            .map_err(|error| BcodeError::ProviderConfiguration(error.to_string()))
    }

    /// Build an agent builder with every manifest-discovered embedded plugin tool registered.
    ///
    /// Definitions and routing IDs come from [`Self::discover_tools`]. Callers can continue
    /// configuring policy, exchanges, hooks, and model selection on the returned builder.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded runtime is configured or tool discovery fails.
    #[cfg(feature = "embedded-plugins")]
    pub async fn agent_with_discovered_tools(&self) -> Result<AgentBuilder> {
        let mut builder = self.agent();
        for tool in self.discover_tools().await? {
            builder = builder.plugin_tool(tool.definition, tool.plugin_id);
        }
        Ok(builder)
    }

    /// Discover tools from every manifest-declared embedded tool service.
    ///
    /// Tool definitions, policy metadata, side-effect classification, permission requirements,
    /// and UI metadata come directly from the owning plugin. Plugins without a declared
    /// `bcode.tool/v1` service are not queried.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded runtime is configured or any declared tool service cannot
    /// be invoked or decoded.
    #[cfg(feature = "embedded-plugins")]
    pub async fn discover_tools(&self) -> Result<Vec<DiscoveredPluginTool>> {
        let plugins = self
            .plugins
            .as_ref()
            .ok_or(BcodeError::MissingPluginRuntime)?;
        let plugin_ids = plugins
            .registry()
            .manifests()
            .values()
            .filter(|manifest| {
                manifest
                    .services
                    .iter()
                    .any(|service| service.interface_id == TOOL_SERVICE_INTERFACE_ID)
            })
            .map(|manifest| manifest.id.clone())
            .collect::<Vec<_>>();
        let mut discovered = Vec::new();
        for plugin_id in plugin_ids {
            let list = plugins
                .invoke_service_json::<_, ToolList>(
                    &plugin_id,
                    TOOL_SERVICE_INTERFACE_ID,
                    OP_LIST_TOOLS,
                    &ListToolsRequest::default(),
                )
                .await
                .map_err(|error| BcodeError::ToolExecution(error.to_string()))?;
            discovered.extend(
                list.tools
                    .into_iter()
                    .map(|definition| DiscoveredPluginTool {
                        plugin_id: plugin_id.clone(),
                        definition,
                    }),
            );
        }
        Ok(discovered)
    }

    /// Return models advertised by an embedded provider plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded plugin runtime is configured or the provider service
    /// cannot be invoked or decoded.
    #[cfg(feature = "embedded-plugins")]
    pub async fn provider_models(&self, provider_plugin_id: impl AsRef<str>) -> Result<ModelList> {
        let plugins = self
            .plugins
            .as_ref()
            .ok_or(BcodeError::MissingPluginRuntime)?;
        plugins
            .invoke_service_json_scoped(
                provider_plugin_id.as_ref(),
                MODEL_PROVIDER_INTERFACE_ID,
                OP_MODELS,
                &bcode_model::ModelListRequest::default(),
                bcode_plugin::PluginInvocationScope::Global,
            )
            .await
            .map_err(|error| BcodeError::ProviderConfiguration(error.to_string()))
    }
}

/// Builder for [`Bcode`].
#[derive(Debug, Clone)]
pub struct BcodeBuilder {
    mode: BcodeMode,
    runtime: AgentRuntime,
    provider_registry: ProviderRegistry,
    #[cfg(feature = "daemon-client")]
    daemon_client: Option<BcodeClient>,
    #[cfg(feature = "embedded-plugins")]
    provider: Option<PluginModelProviderInvoker>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl Default for BcodeBuilder {
    fn default() -> Self {
        Self {
            mode: BcodeMode::Embedded,
            runtime: AgentRuntime::new(),
            provider_registry: ProviderRegistry::new(),
            #[cfg(feature = "daemon-client")]
            daemon_client: None,
            #[cfg(feature = "embedded-plugins")]
            provider: None,
            #[cfg(feature = "embedded-plugins")]
            plugins: None,
        }
    }
}

impl BcodeBuilder {
    /// Configure the runtime mode.
    #[must_use]
    pub const fn mode(mut self, mode: BcodeMode) -> Self {
        self.mode = mode;
        self
    }

    /// Configure the reusable agent runtime used by agents built from this handle.
    #[must_use]
    pub fn runtime(mut self, runtime: AgentRuntime) -> Self {
        self.runtime = runtime;
        self
    }

    /// Configure the provider registry/defaults used by this SDK handle.
    #[must_use]
    pub fn provider_registry(mut self, provider_registry: ProviderRegistry) -> Self {
        self.provider_registry = provider_registry;
        self
    }

    /// Configure provider/model defaults from an already loaded Bcode configuration.
    ///
    /// Process environment provider/model overrides are applied using Bcode's standard resolution
    /// rules. Explicit builder calls made after this method can replace these defaults.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn provider_defaults_from_config(mut self, config: &bcode_config::BcodeConfig) -> Self {
        self.provider_registry = ProviderRegistry::from_config(config);
        self
    }

    /// Configure provider/model defaults from a Bcode configuration and explicit environment.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn provider_defaults_from_config_environment(
        mut self,
        config: &bcode_config::BcodeConfig,
        environment: &impl bcode_config::ConfigEnvironment,
    ) -> Self {
        self.provider_registry = ProviderRegistry::from_config_environment(config, environment);
        self
    }

    /// Load Bcode's layered configuration and configure its provider/model defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if an existing config layer cannot be read, parsed, or composed.
    #[cfg(feature = "config")]
    pub fn load_provider_defaults(mut self) -> Result<Self> {
        self.provider_registry = ProviderRegistry::load()?;
        Ok(self)
    }

    /// Configure the default provider/model selector for agents built from this SDK handle.
    #[must_use]
    pub fn default_model(mut self, model: impl Into<ModelSelector>) -> Self {
        self.provider_registry = self.provider_registry.default_model(model);
        self
    }

    /// Register a provider plugin ID in this SDK handle's provider registry.
    #[must_use]
    pub fn provider(mut self, provider_plugin_id: impl Into<String>) -> Self {
        self.provider_registry = self.provider_registry.provider(provider_plugin_id);
        self
    }

    /// Configure a daemon-backed programmatic client path.
    #[cfg(feature = "daemon-client")]
    #[must_use]
    pub fn daemon_client(mut self, client: BcodeClient) -> Self {
        self.mode = BcodeMode::Daemon;
        self.daemon_client = Some(client);
        self
    }

    /// Configure the default local daemon-backed programmatic client path.
    #[cfg(feature = "daemon-client")]
    #[must_use]
    pub fn default_daemon_client(self) -> Self {
        self.daemon_client(BcodeClient::default_endpoint())
    }

    /// Configure a plugin-backed embedded provider invoker.
    #[cfg(feature = "embedded-plugins")]
    #[must_use]
    pub fn plugin_runtime(mut self, plugins: bcode_plugin::PluginRuntimeHost) -> Self {
        self.provider = Some(PluginModelProviderInvoker::new(plugins.clone()));
        self.plugins = Some(plugins);
        self
    }

    /// Build the SDK handle.
    #[cfg(all(not(feature = "embedded-plugins"), not(feature = "daemon-client")))]
    #[must_use]
    pub fn build(self) -> Bcode {
        Bcode {
            mode: self.mode,
            runtime: self.runtime,
            provider_registry: self.provider_registry,
        }
    }

    /// Build the SDK handle.
    #[cfg(all(not(feature = "embedded-plugins"), feature = "daemon-client"))]
    #[must_use]
    pub fn build(self) -> Bcode {
        let daemon_client = self
            .daemon_client
            .or_else(|| (self.mode == BcodeMode::Daemon).then(BcodeClient::default_endpoint));
        Bcode {
            mode: self.mode,
            runtime: self.runtime,
            provider_registry: self.provider_registry,
            daemon_client,
        }
    }

    /// Build the SDK handle.
    #[cfg(all(feature = "embedded-plugins", not(feature = "daemon-client")))]
    #[must_use]
    pub fn build(self) -> Bcode {
        Bcode {
            mode: self.mode,
            runtime: self.runtime,
            provider_registry: self.provider_registry,
            provider: self.provider,
            plugins: self.plugins,
        }
    }

    /// Build the SDK handle.
    #[cfg(all(feature = "embedded-plugins", feature = "daemon-client"))]
    #[must_use]
    pub fn build(self) -> Bcode {
        let daemon_client = self
            .daemon_client
            .or_else(|| (self.mode == BcodeMode::Daemon).then(BcodeClient::default_endpoint));
        Bcode {
            mode: self.mode,
            runtime: self.runtime,
            provider_registry: self.provider_registry,
            daemon_client,
            provider: self.provider,
            plugins: self.plugins,
        }
    }
}

/// Current renderer-neutral frontend event/snapshot schema version.
pub const FRONTEND_CONTRACT_SCHEMA_VERSION: u32 = 1;

/// Provider/plugin-independent transcript content for frontend snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendContentBlock {
    /// Plain visible text.
    Text {
        /// Visible text content.
        text: String,
    },
    /// Provider-neutral image content.
    Image {
        /// Provider-neutral image content.
        image: bcode_model::ImageContent,
    },
    /// Complete model-requested tool call.
    ToolCall {
        /// Complete tool call.
        call: ToolCall,
    },
    /// Complete model-visible tool result.
    ToolResult {
        /// Complete model-visible tool result.
        result: ToolResult,
    },
}

/// Provider/plugin-independent visible transcript message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendMessage {
    /// Message role.
    pub role: MessageRole,
    /// Visible neutral content. Provider extensions and cache points are omitted.
    pub content: Vec<FrontendContentBlock>,
}

impl From<&ModelMessage> for FrontendMessage {
    fn from(message: &ModelMessage) -> Self {
        Self {
            role: message.role,
            content: message
                .content
                .iter()
                .filter_map(|content| match content {
                    ModelContentBlock::Text { text } => {
                        Some(FrontendContentBlock::Text { text: text.clone() })
                    }
                    ModelContentBlock::Image { image } => Some(FrontendContentBlock::Image {
                        image: image.clone(),
                    }),
                    ModelContentBlock::ToolCall { call } => {
                        Some(FrontendContentBlock::ToolCall { call: call.clone() })
                    }
                    ModelContentBlock::ToolResult { result } => {
                        Some(FrontendContentBlock::ToolResult {
                            result: result.clone(),
                        })
                    }
                    ModelContentBlock::CachePoint { .. }
                    | ModelContentBlock::ProviderExtension { .. } => None,
                })
                .collect(),
        }
    }
}

/// Renderer-neutral event safe for terminal, desktop, web, and service frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FrontendEvent {
    /// Provider turn started.
    TurnStarted,
    /// Visible assistant text delta.
    TextDelta(String),
    /// Visible reasoning delta.
    ReasoningDelta(String),
    /// Tool call began.
    ToolCallStarted {
        /// Provider call ID.
        call_id: String,
        /// Requested tool name.
        name: String,
    },
    /// Incremental tool-call argument data.
    ToolCallDelta {
        /// Provider call ID.
        call_id: String,
        /// Argument delta.
        delta: String,
    },
    /// Complete tool call.
    ToolCallFinished(ToolCall),
    /// Complete model-visible tool result.
    ToolResult(ToolResult),
    /// Provider usage update.
    Usage(TokenUsage),
    /// Provider-confirmed complete request input tokens.
    ExactRequestInputTokens {
        /// Exact token count.
        tokens: u64,
    },
    /// Provider context was compacted.
    ContextCompacted,
    /// Provider retry was scheduled.
    RetryScheduled {
        /// Human-readable retry reason.
        message: String,
        /// Scheduled Unix timestamp.
        retry_at_unix: u64,
    },
    /// Visible warning.
    Warning(String),
    /// Visible provider error.
    Error {
        /// Safe error message.
        message: String,
    },
    /// Turn completed successfully.
    Finished {
        /// Terminal stop reason.
        stop_reason: StopReason,
        /// Final provider usage when available.
        usage: Option<TokenUsage>,
        /// Turn latency in milliseconds.
        latency_ms: u64,
    },
    /// Turn was cancelled.
    Cancelled,
}

impl FrontendEvent {
    /// Project one normalized runtime event, omitting provider metadata and request-projection
    /// internals by construction.
    #[must_use]
    pub fn from_agent_event(event: &AgentEvent) -> Option<Self> {
        match event {
            AgentEvent::TurnStarted => Some(Self::TurnStarted),
            AgentEvent::TextDelta(text) => Some(Self::TextDelta(text.clone())),
            AgentEvent::ReasoningDelta(text) => Some(Self::ReasoningDelta(text.clone())),
            AgentEvent::ToolCallStarted { call_id, name } => Some(Self::ToolCallStarted {
                call_id: call_id.clone(),
                name: name.clone(),
            }),
            AgentEvent::ToolCallDelta { call_id, delta } => Some(Self::ToolCallDelta {
                call_id: call_id.clone(),
                delta: delta.clone(),
            }),
            AgentEvent::ToolCallFinished(call) => Some(Self::ToolCallFinished(call.clone())),
            AgentEvent::ToolResult(result) => Some(Self::ToolResult(result.clone())),
            AgentEvent::Usage(usage) => Some(Self::Usage(usage.clone())),
            AgentEvent::ExactRequestInputTokens(tokens) => Some(Self::ExactRequestInputTokens {
                tokens: tokens.get(),
            }),
            AgentEvent::ContextCompacted => Some(Self::ContextCompacted),
            AgentEvent::RetryScheduled {
                message,
                retry_at_unix,
            } => Some(Self::RetryScheduled {
                message: message.clone(),
                retry_at_unix: *retry_at_unix,
            }),
            AgentEvent::Warning(message) => Some(Self::Warning(message.clone())),
            AgentEvent::ProviderError { message, .. } => Some(Self::Error {
                message: message.clone(),
            }),
            AgentEvent::Finished {
                stop_reason,
                usage,
                latency_ms,
            } => Some(Self::Finished {
                stop_reason: *stop_reason,
                usage: usage.clone(),
                latency_ms: *latency_ms,
            }),
            AgentEvent::Cancelled => Some(Self::Cancelled),
            AgentEvent::RequestProjection(_) | AgentEvent::ProviderMetadata { .. } => None,
        }
    }
}

/// Versioned, correlated frontend event envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendEventEnvelope {
    /// Frontend contract schema version.
    pub schema_version: u32,
    /// Session correlated with this event.
    pub session_id: SessionId,
    /// Turn correlated with this event.
    pub turn_id: String,
    /// Monotonic sequence within the correlated turn cursor.
    pub sequence: u64,
    /// Renderer-neutral event payload.
    pub event: FrontendEvent,
}

/// Sequence allocator and provider-safe projector for one frontend turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendEventCursor {
    session_id: SessionId,
    turn_id: String,
    next_sequence: u64,
}

impl FrontendEventCursor {
    /// Create a cursor at an application-selected resume sequence.
    #[must_use]
    pub fn new(session_id: SessionId, turn_id: impl Into<String>, next_sequence: u64) -> Self {
        Self {
            session_id,
            turn_id: turn_id.into(),
            next_sequence,
        }
    }

    /// Return the next sequence that will be assigned to a projected event.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Project one runtime event. Provider-internal events return `None` and consume no sequence.
    #[must_use]
    pub fn project(&mut self, event: &AgentEvent) -> Option<FrontendEventEnvelope> {
        let event = FrontendEvent::from_agent_event(event)?;
        let envelope = FrontendEventEnvelope {
            schema_version: FRONTEND_CONTRACT_SCHEMA_VERSION,
            session_id: self.session_id,
            turn_id: self.turn_id.clone(),
            sequence: self.next_sequence,
            event,
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        Some(envelope)
    }
}

/// Frontend turn lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendTurnStatus {
    /// The turn may still produce events.
    Active,
    /// The turn completed successfully.
    Completed,
    /// The turn was cancelled.
    Cancelled,
    /// The turn ended with an error.
    Failed,
}

/// Materialized renderer-neutral state for one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendTurnSnapshot {
    /// Correlated turn ID.
    pub turn_id: String,
    /// Current materialized lifecycle status.
    pub status: FrontendTurnStatus,
    /// Concatenated visible assistant text.
    pub text: String,
    /// Concatenated visible reasoning text.
    pub reasoning: String,
    /// Latest cumulative provider usage.
    pub usage: Option<TokenUsage>,
    /// Provider-confirmed exact input context for the request.
    pub exact_request_input_tokens: Option<u64>,
    /// Terminal stop reason when known.
    pub stop_reason: Option<StopReason>,
    /// Terminal latency in milliseconds when known.
    pub latency_ms: Option<u64>,
    /// Complete observed tool calls in event order.
    pub tool_calls: Vec<ToolCall>,
    /// Complete observed tool results in event order.
    pub tool_results: Vec<ToolResult>,
    /// Visible warnings in event order.
    pub warnings: Vec<String>,
    /// Last visible terminal error message.
    pub last_error: Option<String>,
}

impl FrontendTurnSnapshot {
    const fn new(turn_id: String) -> Self {
        Self {
            turn_id,
            status: FrontendTurnStatus::Active,
            text: String::new(),
            reasoning: String::new(),
            usage: None,
            exact_request_input_tokens: None,
            stop_reason: None,
            latency_ms: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            warnings: Vec::new(),
            last_error: None,
        }
    }

    fn apply(&mut self, event: &FrontendEvent) {
        match event {
            FrontendEvent::TurnStarted
            | FrontendEvent::ToolCallStarted { .. }
            | FrontendEvent::ToolCallDelta { .. }
            | FrontendEvent::RetryScheduled { .. }
            | FrontendEvent::ContextCompacted => {}
            FrontendEvent::TextDelta(text) => self.text.push_str(text),
            FrontendEvent::ReasoningDelta(text) => self.reasoning.push_str(text),
            FrontendEvent::ToolCallFinished(call) => self.tool_calls.push(call.clone()),
            FrontendEvent::ToolResult(result) => self.tool_results.push(result.clone()),
            FrontendEvent::Usage(usage) => self.usage = Some(usage.clone()),
            FrontendEvent::ExactRequestInputTokens { tokens } => {
                self.exact_request_input_tokens = Some(*tokens);
            }
            FrontendEvent::Warning(message) => self.warnings.push(message.clone()),
            FrontendEvent::Error { message } => {
                self.last_error = Some(message.clone());
                self.status = FrontendTurnStatus::Failed;
            }
            FrontendEvent::Finished {
                stop_reason,
                usage,
                latency_ms,
            } => {
                self.status = FrontendTurnStatus::Completed;
                self.stop_reason = Some(*stop_reason);
                self.usage.clone_from(usage);
                self.latency_ms = Some(*latency_ms);
            }
            FrontendEvent::Cancelled => {
                self.status = FrontendTurnStatus::Cancelled;
                self.stop_reason = Some(StopReason::Cancelled);
            }
        }
    }
}

/// Result of idempotently applying a frontend event envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendSnapshotApplyOutcome {
    /// The event was new and changed materialized state.
    Applied,
    /// The event exactly matched one already applied at the same sequence.
    Duplicate,
}

/// Validation failure while materializing a frontend snapshot.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrontendSnapshotError {
    /// Envelope uses an unsupported schema version.
    #[error("unsupported frontend schema version {0}")]
    UnsupportedVersion(u32),
    /// Envelope belongs to another session.
    #[error("frontend event belongs to a different session")]
    SessionMismatch,
    /// Envelope skipped the next required sequence.
    #[error("frontend event sequence gap: expected {expected}, received {actual}")]
    SequenceGap {
        /// Next required sequence.
        expected: u64,
        /// Received sequence.
        actual: u64,
    },
    /// Duplicate sequence carried a different payload.
    #[error("frontend duplicate sequence {sequence} has conflicting payload")]
    ConflictingDuplicate {
        /// Conflicting sequence.
        sequence: u64,
    },
    /// Envelope belongs to another active turn.
    #[error("frontend event belongs to turn {actual}, active turn is {expected}")]
    TurnMismatch {
        /// Active turn ID.
        expected: String,
        /// Received turn ID.
        actual: String,
    },
}

/// Serializable renderer-neutral session snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendSessionSnapshot {
    /// Frontend contract schema version.
    pub schema_version: u32,
    /// Materialized session ID.
    pub session_id: SessionId,
    /// Provider-neutral visible transcript.
    pub transcript: Vec<FrontendMessage>,
    /// Active or most recently materialized turn.
    pub active_turn: Option<FrontendTurnSnapshot>,
    /// Next event sequence required by this snapshot.
    pub next_sequence: u64,
    #[serde(default)]
    applied_event_digests: BTreeMap<u64, String>,
}

impl FrontendSessionSnapshot {
    /// Create a snapshot from visible neutral transcript messages.
    #[must_use]
    pub fn new(session_id: SessionId, transcript: &[ModelMessage]) -> Self {
        Self {
            schema_version: FRONTEND_CONTRACT_SCHEMA_VERSION,
            session_id,
            transcript: transcript.iter().map(FrontendMessage::from).collect(),
            active_turn: None,
            next_sequence: 0,
            applied_event_digests: BTreeMap::new(),
        }
    }

    /// Replace the visible transcript using the same provider-safe projection.
    pub fn replace_transcript(&mut self, transcript: &[ModelMessage]) {
        self.transcript = transcript.iter().map(FrontendMessage::from).collect();
    }

    /// Idempotently apply one event, rejecting gaps, conflicting duplicates, and mixed turns.
    ///
    /// # Errors
    ///
    /// Returns a typed contract error for unsupported versions or incoherent delivery.
    pub fn apply_event(
        &mut self,
        envelope: &FrontendEventEnvelope,
    ) -> std::result::Result<FrontendSnapshotApplyOutcome, FrontendSnapshotError> {
        if envelope.schema_version != FRONTEND_CONTRACT_SCHEMA_VERSION {
            return Err(FrontendSnapshotError::UnsupportedVersion(
                envelope.schema_version,
            ));
        }
        if envelope.session_id != self.session_id {
            return Err(FrontendSnapshotError::SessionMismatch);
        }
        let digest = frontend_event_digest(envelope);
        if envelope.sequence < self.next_sequence {
            return if self.applied_event_digests.get(&envelope.sequence) == Some(&digest) {
                Ok(FrontendSnapshotApplyOutcome::Duplicate)
            } else {
                Err(FrontendSnapshotError::ConflictingDuplicate {
                    sequence: envelope.sequence,
                })
            };
        }
        if envelope.sequence > self.next_sequence {
            return Err(FrontendSnapshotError::SequenceGap {
                expected: self.next_sequence,
                actual: envelope.sequence,
            });
        }
        let start_new = self.active_turn.as_ref().is_none_or(|turn| {
            turn.turn_id != envelope.turn_id
                && !matches!(turn.status, FrontendTurnStatus::Active)
                && matches!(envelope.event, FrontendEvent::TurnStarted)
        });
        if start_new {
            self.active_turn = Some(FrontendTurnSnapshot::new(envelope.turn_id.clone()));
        }
        let turn = self
            .active_turn
            .get_or_insert_with(|| FrontendTurnSnapshot::new(envelope.turn_id.clone()));
        if turn.turn_id != envelope.turn_id {
            return Err(FrontendSnapshotError::TurnMismatch {
                expected: turn.turn_id.clone(),
                actual: envelope.turn_id.clone(),
            });
        }
        turn.apply(&envelope.event);
        self.applied_event_digests.insert(envelope.sequence, digest);
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(FrontendSnapshotApplyOutcome::Applied)
    }
}

fn frontend_event_digest(envelope: &FrontendEventEnvelope) -> String {
    let encoded =
        serde_json::to_vec(envelope).expect("serializing a frontend event envelope cannot fail");
    format!("{:x}", Sha256::digest(encoded))
}

/// Current portable SDK session payload schema version.
pub const PERSISTED_SESSION_SCHEMA_VERSION: u32 = 1;

const fn persisted_session_schema_version() -> u32 {
    PERSISTED_SESSION_SCHEMA_VERSION
}

/// On-disk payload used by [`LocalSessionStore`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Portable payload schema version. Older unversioned payloads decode as version 1.
    #[serde(default = "persisted_session_schema_version")]
    pub schema_version: u32,
    /// Session ID associated with the transcript.
    pub session_id: SessionId,
    /// Caller-managed conversation transcript.
    pub messages: Vec<ModelMessage>,
    /// Explicit transcript-persisted memory records. Older payloads default to none.
    #[serde(default)]
    pub memories: Vec<MemoryItem>,
}

/// Persistence adapter for SDK-managed conversation sessions.
///
/// Adapters must serialize concurrent saves for one logical session, atomically replace a complete
/// payload (never expose a partial one), and make successful `save` data visible to later `load`
/// calls. Bcode does not impose a database, locking backend, or async runtime. Applications migrate
/// older/newer schema versions inside their adapter before returning a validated payload.
/// Corrupt, stale, conflicting, or otherwise unusable state should return a descriptive
/// [`BcodeError`] rather than silently discarding data.
pub trait SessionPersistenceAdapter: Send + Sync {
    /// Load a persisted session, or return `Ok(None)` when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when persisted state exists but cannot be read or safely decoded.
    fn load(&self) -> Result<Option<PersistedSession>>;

    /// Save the complete persisted session as one atomic replacement.
    ///
    /// Implementations must serialize concurrent writes for this logical session and must not
    /// return success until a subsequent `load` can observe the complete payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the session cannot be encoded or durably stored by the adapter.
    fn save(&self, session: &PersistedSession) -> Result<()>;
}

fn validate_persisted_session(session: &PersistedSession) -> Result<()> {
    if session.schema_version != PERSISTED_SESSION_SCHEMA_VERSION {
        return Err(BcodeError::SessionState(format!(
            "unsupported SDK session schema version {}; current version is {}; migrate with the application adapter before opening",
            session.schema_version, PERSISTED_SESSION_SCHEMA_VERSION
        )));
    }
    if session.memories.iter().any(|memory| {
        memory.retention != MemoryRetention::SessionTranscript
            || !session.messages.contains(&memory.message)
    }) {
        return Err(BcodeError::SessionState(
            "persisted memory metadata is inconsistent with the visible transcript".to_string(),
        ));
    }
    Ok(())
}

/// Explicit local JSON session store for SDK-managed persistence.
///
/// This adapter uses same-directory temporary-file replacement. Callers must serialize concurrent
/// writers to the same path; the last completed save wins. It is a portable convenience adapter,
/// not a multi-process transactional database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSessionStore {
    path: PathBuf,
}

impl LocalSessionStore {
    /// Create a local session store at an explicit file path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Return the configured store path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Load the session, returning `Ok(None)` when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the session file cannot be read, is stale/empty, or contains corrupt
    /// JSON that requires caller attention.
    pub fn load(&self) -> Result<Option<PersistedSession>> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(BcodeError::SessionPersistence(format!(
                    "failed to read session store {}: {error}",
                    display_from_current_dir(&self.path)
                )));
            }
        };
        if contents.trim().is_empty() {
            return Err(BcodeError::SessionState(format!(
                "session store {} is empty and requires repair or replacement",
                display_from_current_dir(&self.path)
            )));
        }
        let session: PersistedSession = serde_json::from_str(&contents).map_err(|error| {
            BcodeError::SessionState(format!(
                "session store {} is corrupt and requires repair or replacement: {error}",
                display_from_current_dir(&self.path)
            ))
        })?;
        validate_persisted_session(&session)?;
        Ok(Some(session))
    }

    /// Save the complete session payload atomically enough for local SDK use.
    ///
    /// # Errors
    ///
    /// Returns an error when parent directories or files cannot be written.
    pub fn save(&self, session: &PersistedSession) -> Result<()> {
        validate_persisted_session(session)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                BcodeError::SessionPersistence(format!(
                    "failed to create session store directory {}: {error}",
                    display_from_current_dir(parent)
                ))
            })?;
        }
        let encoded = serde_json::to_string_pretty(session).map_err(|error| {
            BcodeError::SessionPersistence(format!("failed to encode session store JSON: {error}"))
        })?;
        let temporary_path = self.path.with_extension("tmp");
        std::fs::write(&temporary_path, encoded).map_err(|error| {
            BcodeError::SessionPersistence(format!(
                "failed to write temporary session store {}: {error}",
                display_from_current_dir(&temporary_path)
            ))
        })?;
        std::fs::rename(&temporary_path, &self.path).map_err(|error| {
            BcodeError::SessionPersistence(format!(
                "failed to replace session store {}: {error}",
                display_from_current_dir(&self.path)
            ))
        })
    }
}

impl SessionPersistenceAdapter for LocalSessionStore {
    fn load(&self) -> Result<Option<PersistedSession>> {
        Self::load(self)
    }

    fn save(&self, session: &PersistedSession) -> Result<()> {
        Self::save(self, session)
    }
}

/// In-memory SDK session state for continuing conversations without persistence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InMemorySession {
    messages: Vec<ModelMessage>,
    persisted_memories: Vec<MemoryItem>,
}

impl InMemorySession {
    /// Create an empty in-memory session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a session from caller-managed model messages.
    #[must_use]
    pub const fn from_messages(messages: Vec<ModelMessage>) -> Self {
        Self {
            messages,
            persisted_memories: Vec::new(),
        }
    }

    /// Restore a transcript and its explicit persisted memory records.
    #[must_use]
    pub const fn from_persisted(
        messages: Vec<ModelMessage>,
        persisted_memories: Vec<MemoryItem>,
    ) -> Self {
        Self {
            messages,
            persisted_memories,
        }
    }

    /// Return the current session transcript.
    #[must_use]
    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    /// Return explicit transcript-persisted memory records.
    #[must_use]
    pub fn persisted_memories(&self) -> &[MemoryItem] {
        &self.persisted_memories
    }

    /// Export the session transcript for caller-managed persistence.
    #[must_use]
    pub fn into_messages(self) -> Vec<ModelMessage> {
        self.messages
    }

    /// Add a message to the session transcript.
    pub fn push_message(&mut self, message: ModelMessage) {
        self.messages.push(message);
    }

    /// Add one explicit transcript-persisted memory record and its visible message.
    pub fn push_persisted_memory(&mut self, memory: MemoryItem) {
        self.messages.push(memory.message.clone());
        self.persisted_memories.push(memory);
    }

    /// Clear the in-memory transcript and explicit persisted-memory metadata.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.persisted_memories.clear();
    }
}

/// Privacy classification for application-owned memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPrivacy {
    /// Memory may be supplied under ordinary application policy.
    #[default]
    Public,
    /// Memory contains private application/user context.
    Private,
    /// Memory contains sensitive context requiring explicit policy opt-in.
    Sensitive,
}

/// Whether memory is ephemeral retrieval context or an explicit transcript entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRetention {
    /// Use only for one model request; never mutate or persist the transcript.
    #[default]
    RequestOnly,
    /// Persist explicitly through [`AgentSession::remember`]; retrieval providers cannot silently
    /// request this retention.
    SessionTranscript,
}

/// Stable application-owned source identity for retrieved memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryProvenance {
    /// Memory provider/index identity.
    pub provider_id: String,
    /// Stable source/document/record identity within that provider.
    pub source_id: String,
}

/// One typed memory item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryItem {
    /// Stable item identity used in reports and diagnostics.
    pub id: String,
    /// Model context contributed by this item.
    pub message: ModelMessage,
    /// Provider-owned relevance score from zero (least) through 1,000 (most).
    pub relevance_millis: u16,
    /// Source provenance.
    pub provenance: MemoryProvenance,
    /// Privacy classification evaluated by [`MemoryPolicy`].
    #[serde(default)]
    pub privacy: MemoryPrivacy,
    /// Request-only retrieval or explicitly persisted transcript memory.
    #[serde(default)]
    pub retention: MemoryRetention,
}

/// Query passed to an application memory provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRetrievalRequest {
    /// Current user query/prompt.
    pub query: String,
    /// Visible transcript before the current user query.
    pub transcript: Vec<ModelMessage>,
}

/// Application-extensible memory retrieval contract.
pub trait MemoryProvider: Send + Sync {
    /// Retrieve candidate memories. Bcode validates, ranks, filters, and bounds the result.
    ///
    /// # Errors
    ///
    /// Returns an error when retrieval fails. [`MemoryFailurePolicy`] determines whether the model
    /// request fails or continues without this provider's items.
    fn retrieve(&self, request: &MemoryRetrievalRequest) -> Result<Vec<MemoryItem>>;
}

/// Behavior when one memory provider fails or returns invalid items.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemoryFailurePolicy {
    /// Fail the model request before provider invocation.
    #[default]
    FailTurn,
    /// Record the failure and continue without that memory provider.
    ContinueWithoutMemory,
}

/// Bounds and privacy policy for retrieved memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPolicy {
    /// Maximum accepted items across all providers.
    pub max_items: usize,
    /// Maximum serialized bytes for one item message.
    pub max_item_bytes: usize,
    /// Maximum serialized bytes across accepted item messages.
    pub max_total_bytes: usize,
    /// Most sensitive privacy class admitted to model context.
    pub max_privacy: MemoryPrivacy,
    /// Failure behavior for retrieval and validation.
    pub failure_policy: MemoryFailurePolicy,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            max_items: 16,
            max_item_bytes: 32 * 1024,
            max_total_bytes: 128 * 1024,
            max_privacy: MemoryPrivacy::Private,
            failure_policy: MemoryFailurePolicy::FailTurn,
        }
    }
}

/// Accepted memory evidence for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievedMemoryEvidence {
    /// Stable memory item identity.
    pub id: String,
    /// Source provenance.
    pub provenance: MemoryProvenance,
    /// Relevance used for ranking.
    pub relevance_millis: u16,
    /// Serialized message size admitted to context.
    pub byte_len: usize,
}

/// Report for the latest memory assembly attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryRetrievalReport {
    /// Accepted items in final relevance order.
    pub accepted: Vec<RetrievedMemoryEvidence>,
    /// Stable non-secret failure/filter diagnostics.
    pub diagnostics: Vec<String>,
    /// Total serialized bytes accepted.
    pub accepted_bytes: usize,
}

/// Application-owned request-context extension for SDK sessions.
///
/// Providers can return summaries, profile context, or application state as normal model messages
/// for the next turn. Returned messages are request context only: they are not appended to or
/// persisted with the visible session transcript. New memory integrations that need relevance,
/// provenance, privacy, bounds, and diagnostics should implement [`MemoryProvider`].
pub trait SessionContextProvider: Send + Sync {
    /// Return context messages for the next turn.
    ///
    /// # Errors
    ///
    /// Returns an error when context retrieval or construction fails.
    fn context_messages(&self, session: &InMemorySession) -> Result<Vec<ModelMessage>>;
}

/// Stateful agent wrapper that keeps conversation history in memory.
#[derive(Clone)]
pub struct AgentSession {
    agent: Agent,
    session: InMemorySession,
    persistence: Option<Arc<dyn SessionPersistenceAdapter>>,
    context_providers: Vec<Arc<dyn SessionContextProvider>>,
    memory_providers: Vec<Arc<dyn MemoryProvider>>,
    memory_policy: MemoryPolicy,
    memory_report: MemoryRetrievalReport,
}

impl fmt::Debug for AgentSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSession")
            .field("agent", &self.agent)
            .field("session", &self.session)
            .field("persistence", &self.persistence.is_some())
            .field("context_providers", &self.context_providers.len())
            .field("memory_providers", &self.memory_providers.len())
            .field("memory_policy", &self.memory_policy)
            .field("memory_report", &self.memory_report)
            .finish()
    }
}

impl AgentSession {
    /// Create a stateful wrapper around an agent and in-memory session.
    #[must_use]
    pub const fn new(agent: Agent, session: InMemorySession) -> Self {
        Self {
            agent,
            session,
            persistence: None,
            context_providers: Vec::new(),
            memory_providers: Vec::new(),
            memory_policy: MemoryPolicy {
                max_items: 16,
                max_item_bytes: 32 * 1024,
                max_total_bytes: 128 * 1024,
                max_privacy: MemoryPrivacy::Private,
                failure_policy: MemoryFailurePolicy::FailTurn,
            },
            memory_report: MemoryRetrievalReport {
                accepted: Vec::new(),
                diagnostics: Vec::new(),
                accepted_bytes: 0,
            },
        }
    }

    /// Attach an application-owned context/memory provider.
    #[must_use]
    pub fn with_context_provider<P>(mut self, provider: P) -> Self
    where
        P: SessionContextProvider + 'static,
    {
        self.context_providers.push(Arc::new(provider));
        self
    }

    /// Attach a shared application-owned context/memory provider.
    #[must_use]
    pub fn with_shared_context_provider(
        mut self,
        provider: Arc<dyn SessionContextProvider>,
    ) -> Self {
        self.context_providers.push(provider);
        self
    }

    /// Attach an application-owned typed memory provider.
    #[must_use]
    pub fn with_memory_provider<P>(mut self, provider: P) -> Self
    where
        P: MemoryProvider + 'static,
    {
        self.memory_providers.push(Arc::new(provider));
        self
    }

    /// Attach a shared typed memory provider.
    #[must_use]
    pub fn with_shared_memory_provider(mut self, provider: Arc<dyn MemoryProvider>) -> Self {
        self.memory_providers.push(provider);
        self
    }

    /// Configure retrieval bounds, privacy, and failure behavior.
    #[must_use]
    pub const fn with_memory_policy(mut self, policy: MemoryPolicy) -> Self {
        self.memory_policy = policy;
        self
    }

    /// Return the latest memory retrieval/filtering report.
    #[must_use]
    pub const fn memory_report(&self) -> &MemoryRetrievalReport {
        &self.memory_report
    }

    /// Explicitly append transcript-persisted memory.
    ///
    /// Unlike retrieval, this mutates the visible transcript and immediately saves through a
    /// configured persistence adapter. Request-only items are rejected to prevent accidental
    /// retention.
    ///
    /// # Errors
    ///
    /// Returns an error for request-only retention, invalid metadata/relevance, oversized messages,
    /// disallowed privacy, or persistence failure.
    pub fn remember(&mut self, memory: MemoryItem) -> Result<()> {
        validate_memory_item(&memory, &self.memory_policy, true)?;
        if let Some(persistence) = &self.persistence {
            let mut messages = self.session.messages.clone();
            messages.push(memory.message.clone());
            let mut memories = self.session.persisted_memories.clone();
            memories.push(memory.clone());
            persistence.save(&PersistedSession {
                schema_version: PERSISTED_SESSION_SCHEMA_VERSION,
                session_id: self.agent.session_id,
                messages,
                memories,
            })?;
        }
        self.session.push_persisted_memory(memory);
        Ok(())
    }

    /// Attach a persistence adapter and save after successful turns.
    #[must_use]
    pub fn with_persistence(mut self, persistence: Arc<dyn SessionPersistenceAdapter>) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Attach an explicit local session store and save after successful turns.
    #[must_use]
    pub fn with_store(self, store: LocalSessionStore) -> Self {
        self.with_persistence(Arc::new(store))
    }

    /// Return the wrapped agent.
    #[must_use]
    pub const fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Return the in-memory session state.
    #[must_use]
    pub const fn session(&self) -> &InMemorySession {
        &self.session
    }

    /// Build a renderer-neutral serializable snapshot of visible session state.
    #[must_use]
    pub fn frontend_snapshot(&self) -> FrontendSessionSnapshot {
        FrontendSessionSnapshot::new(self.agent.session_id, self.session.messages())
    }

    /// Return the configured persistence adapter, when persistence was explicitly enabled.
    #[must_use]
    pub fn persistence(&self) -> Option<&dyn SessionPersistenceAdapter> {
        self.persistence.as_deref()
    }

    /// Return the session payload that can be saved by caller-managed persistence.
    #[must_use]
    pub fn persisted_session(&self) -> PersistedSession {
        PersistedSession {
            schema_version: PERSISTED_SESSION_SCHEMA_VERSION,
            session_id: self.agent.session_id,
            messages: self.session.messages.clone(),
            memories: self.session.persisted_memories.clone(),
        }
    }

    /// Save to the configured persistence adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when this session has no configured adapter or when saving fails.
    pub fn save(&self) -> Result<()> {
        let persistence = self.persistence.as_ref().ok_or_else(|| {
            BcodeError::SessionPersistence(
                "session persistence is not configured for this session".to_string(),
            )
        })?;
        persistence.save(&self.persisted_session())
    }

    /// Export the in-memory session transcript for caller-managed persistence.
    #[must_use]
    pub fn into_messages(self) -> Vec<ModelMessage> {
        self.session.into_messages()
    }

    /// Append a caller-managed message to the session transcript.
    pub fn append_message(&mut self, message: ModelMessage) {
        self.session.push_message(message);
    }

    /// Create an in-memory branch of this session without copying the configured persistence store.
    #[must_use]
    pub fn branch(&self) -> Self {
        Self::new(self.agent.clone(), self.session.clone())
    }

    /// Alias for [`Self::branch`].
    #[must_use]
    pub fn fork(&self) -> Self {
        self.branch()
    }

    fn commit_messages(&mut self, messages: Vec<ModelMessage>) -> Result<()> {
        if let Some(persistence) = &self.persistence {
            persistence.save(&PersistedSession {
                schema_version: PERSISTED_SESSION_SCHEMA_VERSION,
                session_id: self.agent.session_id,
                messages: messages.clone(),
                memories: self.session.persisted_memories.clone(),
            })?;
        }
        self.session.messages = messages;
        Ok(())
    }

    fn request_messages(
        &mut self,
        transcript: &[ModelMessage],
        query: &str,
    ) -> Result<Vec<ModelMessage>> {
        let mut messages = Vec::new();
        for provider in &self.context_providers {
            messages.extend(provider.context_messages(&self.session)?);
        }
        let (memory, report) = retrieve_memory(
            &self.memory_providers,
            &self.memory_policy,
            &MemoryRetrievalRequest {
                query: query.to_string(),
                transcript: transcript.to_vec(),
            },
        )?;
        self.memory_report = report;
        messages.extend(memory.into_iter().map(|item| item.message));
        messages.extend_from_slice(transcript);
        Ok(messages)
    }

    /// Regenerate the response to the last user message.
    ///
    /// The previous assistant continuation after the last user message is removed, the last user
    /// message is kept, and the regenerated assistant message is appended.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no prior user message, the last user message has no text
    /// block, provider invocation fails, the runtime is cancelled, or persistence fails.
    pub async fn regenerate_last_with_provider<P>(
        &mut self,
        provider: &mut P,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        let Some(user_index) = self
            .session
            .messages
            .iter()
            .rposition(|message| message.role == MessageRole::User)
        else {
            return Err(BcodeError::SessionState(
                "cannot regenerate a session without a prior user message".to_string(),
            ));
        };
        let user_message = self.session.messages[user_index].clone();
        let prompt = text_from_message(&user_message).ok_or_else(|| {
            BcodeError::SessionState(
                "cannot regenerate because the last user message has no text block".to_string(),
            )
        })?;
        let transcript = self.session.messages[..user_index].to_vec();
        let prior_messages = self.request_messages(&transcript, &prompt)?;
        let session_span = tracing::info_span!(
            target: "bcode::sdk",
            "bcode.session_turn",
            session_id = %self.agent.session_id,
            provider_id = self.agent.provider_plugin_id.as_deref().unwrap_or(""),
            model_id = %self.agent.model_id,
            operation = "regenerate",
        );
        let response = self
            .agent
            .generate_text_with_provider_and_history(provider, prompt, prior_messages)
            .instrument(session_span)
            .await?;
        let mut updated = self.session.messages[..user_index].to_vec();
        updated.push(user_message);
        updated.extend(transcript_messages_from_response(&response));
        self.commit_messages(updated)?;
        Ok(response)
    }

    /// Send a user message and append the user/assistant exchange to this chat session.
    ///
    /// This is the frontend-oriented alias for [`Self::generate_text_with_provider`].
    ///
    /// # Errors
    ///
    /// Returns an error when context retrieval, provider invocation, cancellation, or persistence
    /// fails.
    pub async fn send<P>(
        &mut self,
        provider: &mut P,
        message: impl Into<String>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider(provider, message).await
    }

    /// Generate text and append user/assistant messages to the in-memory transcript.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the runtime is cancelled, or the provider
    /// reports an error.
    pub async fn generate_text_with_provider<P>(
        &mut self,
        provider: &mut P,
        prompt: impl Into<String>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        let prompt = prompt.into();
        let transcript = self.session.messages.clone();
        let messages = self.request_messages(&transcript, &prompt)?;
        let session_span = tracing::info_span!(
            target: "bcode::sdk",
            "bcode.session_turn",
            session_id = %self.agent.session_id,
            provider_id = self.agent.provider_plugin_id.as_deref().unwrap_or(""),
            model_id = %self.agent.model_id,
            operation = "generate",
        );
        let response = self
            .agent
            .generate_text_with_provider_and_history(provider, prompt.clone(), messages)
            .instrument(session_span)
            .await?;
        let mut updated = self.session.messages.clone();
        updated.push(user_message(prompt));
        updated.extend(transcript_messages_from_response(&response));
        self.commit_messages(updated)?;
        Ok(response)
    }
}

fn validate_memory_item(
    item: &MemoryItem,
    policy: &MemoryPolicy,
    require_persisted: bool,
) -> Result<usize> {
    let code = memory_item_validation_code(item, policy, require_persisted);
    if let Some(code) = code {
        return Err(BcodeError::MemoryValidation { code });
    }
    let bytes = serde_json::to_vec(&item.message)
        .map(|encoded| encoded.len())
        .map_err(|_| BcodeError::MemoryValidation {
            code: "message_serialization_failed",
        })?;
    if bytes > policy.max_item_bytes || bytes > policy.max_total_bytes {
        return Err(BcodeError::MemoryValidation {
            code: "item_too_large",
        });
    }
    Ok(bytes)
}

fn memory_item_validation_code(
    item: &MemoryItem,
    policy: &MemoryPolicy,
    require_persisted: bool,
) -> Option<&'static str> {
    if item.id.trim().is_empty() {
        return Some("empty_item_id");
    }
    if item.provenance.provider_id.trim().is_empty() {
        return Some("empty_provider_id");
    }
    if item.provenance.source_id.trim().is_empty() {
        return Some("empty_source_id");
    }
    if item.relevance_millis > 1_000 {
        return Some("relevance_out_of_range");
    }
    if item.message.content.is_empty() {
        return Some("empty_message");
    }
    if require_persisted && item.retention != MemoryRetention::SessionTranscript {
        return Some("request_only_cannot_be_persisted");
    }
    if !require_persisted && item.retention != MemoryRetention::RequestOnly {
        return Some("provider_cannot_persist_memory");
    }
    if item.privacy > policy.max_privacy {
        return Some("privacy_not_allowed");
    }
    None
}

fn retrieve_memory(
    providers: &[Arc<dyn MemoryProvider>],
    policy: &MemoryPolicy,
    request: &MemoryRetrievalRequest,
) -> Result<(Vec<MemoryItem>, MemoryRetrievalReport)> {
    let mut report = MemoryRetrievalReport::default();
    let mut candidates = Vec::new();
    let mut identities = std::collections::BTreeSet::new();
    for (provider_index, provider) in providers.iter().enumerate() {
        let items = match provider.retrieve(request) {
            Ok(items) => items,
            Err(_) if policy.failure_policy == MemoryFailurePolicy::ContinueWithoutMemory => {
                report
                    .diagnostics
                    .push(format!("provider:{provider_index}:retrieval_failed"));
                continue;
            }
            Err(_) => {
                return Err(BcodeError::Memory {
                    provider_index,
                    code: "retrieval_failed",
                });
            }
        };
        for (item_index, item) in items.into_iter().enumerate() {
            let identity = (item.provenance.provider_id.clone(), item.id.clone());
            let validation_code = memory_item_validation_code(&item, policy, false)
                .or_else(|| (!identities.insert(identity)).then_some("duplicate_item_identity"));
            let bytes = serde_json::to_vec(&item.message)
                .map(|encoded| encoded.len())
                .ok();
            let validation_code = validation_code
                .or_else(|| bytes.is_none().then_some("message_serialization_failed"))
                .or_else(|| {
                    bytes
                        .is_some_and(|bytes| bytes > policy.max_item_bytes)
                        .then_some("item_too_large")
                });
            if let Some(code) = validation_code {
                if policy.failure_policy == MemoryFailurePolicy::FailTurn {
                    return Err(BcodeError::Memory {
                        provider_index,
                        code,
                    });
                }
                report.diagnostics.push(format!(
                    "provider:{provider_index}:item:{item_index}:{code}"
                ));
                continue;
            }
            candidates.push((provider_index, item_index, item, bytes.unwrap_or_default()));
        }
    }
    candidates.sort_by(
        |(left_provider, left_item, left, _), (right_provider, right_item, right, _)| {
            right
                .relevance_millis
                .cmp(&left.relevance_millis)
                .then_with(|| left_provider.cmp(right_provider))
                .then_with(|| left_item.cmp(right_item))
        },
    );
    let mut accepted = Vec::new();
    for (_, _, item, bytes) in candidates {
        if accepted.len() >= policy.max_items {
            report.diagnostics.push("limit:max_items".to_string());
            break;
        }
        if report.accepted_bytes.saturating_add(bytes) > policy.max_total_bytes {
            report
                .diagnostics
                .push(format!("item:{}:total_size_limit", item.id));
            continue;
        }
        report.accepted_bytes = report.accepted_bytes.saturating_add(bytes);
        report.accepted.push(RetrievedMemoryEvidence {
            id: item.id.clone(),
            provenance: item.provenance.clone(),
            relevance_millis: item.relevance_millis,
            byte_len: bytes,
        });
        accepted.push(item);
    }
    Ok((accepted, report))
}

/// Configured agent facade for text generation, tools, streaming, hooks, and sessions.
#[derive(Clone)]
pub struct Agent {
    runtime: AgentRuntime,
    name: Option<String>,
    profile_id: String,
    session_id: SessionId,
    cwd: Option<PathBuf>,
    provider_plugin_id: Option<String>,
    model_id: String,
    selection_provenance: Box<ModelSelectionProvenance>,
    registration_source: Option<ProviderRegistrationSource>,
    model_metadata_source: Option<ModelMetadataSource>,
    model_pricing: Option<ModelPricingInfo>,
    provider_context: ProviderRequestContext,
    system_prompt: Option<String>,
    parameters: ModelParameters,
    metadata: BTreeMap<String, String>,
    timeout: Duration,
    max_tool_rounds: u32,
    max_repeated_tool_batches: u32,
    stop_condition: Option<AgentLoopStopPredicate>,
    execution_options: ToolExecutionOptions,
    tool_choice: ToolChoice,
    parallel_tool_capabilities: bcode_model::ParallelToolCallCapabilities,
    tool_failure_policy: ToolFailurePolicy,
    invocation_capabilities: InvocationCapabilities,
    invocation_event_sink: Arc<dyn TurnEventSink>,
    authorization_coordinator: Option<Arc<dyn ToolAuthorizationCoordinator>>,
    tool_invoker: Option<Arc<dyn ToolInvoker>>,
    provider_factory: Option<ProviderFactory>,
    provider_round_planner: Arc<dyn ProviderRoundPlanner>,
    cache_routing_identity: Option<String>,
    tool_catalog: UnifiedToolCatalog,
    inline_tool_handlers: BTreeMap<String, InlineToolHandler>,
    hooks: AgentHooks,
    middleware: ModelMiddlewareStack,
    response_cache: Option<Arc<dyn ModelResponseCache>>,
    policy_config: AgentConfig,
    permission_policy: Arc<dyn PermissionPolicy>,
    #[cfg(feature = "embedded-plugins")]
    provider: Option<PluginModelProviderInvoker>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Agent");
        debug
            .field("runtime", &self.runtime)
            .field("name", &self.name)
            .field("profile_id", &self.profile_id)
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("provider_plugin_id", &self.provider_plugin_id)
            .field("model_id", &self.model_id)
            .field("selection_provenance", &self.selection_provenance)
            .field("registration_source", &self.registration_source)
            .field("model_metadata_source", &self.model_metadata_source)
            .field("model_pricing", &self.model_pricing)
            .field("provider_context", &self.provider_context)
            .field("system_prompt", &self.system_prompt)
            .field("parameters", &self.parameters)
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("max_repeated_tool_batches", &self.max_repeated_tool_batches)
            .field("stop_condition", &self.stop_condition.is_some())
            .field("execution_options", &self.execution_options)
            .field("tool_choice", &self.tool_choice)
            .field(
                "parallel_tool_capabilities",
                &self.parallel_tool_capabilities,
            )
            .field("tool_failure_policy", &self.tool_failure_policy)
            .field("invocation_capabilities", &self.invocation_capabilities)
            .field("invocation_event_sink", &"<sink>")
            .field(
                "authorization_coordinator",
                &self.authorization_coordinator.is_some(),
            )
            .field("tool_invoker", &self.tool_invoker.is_some())
            .field("provider_factory", &self.provider_factory.is_some())
            .field("provider_round_planner", &"<planner>")
            .field("cache_routing_identity", &self.cache_routing_identity)
            .field("tool_catalog", &self.tool_catalog)
            .field(
                "inline_tool_handlers",
                &self.inline_tool_handlers.keys().collect::<Vec<_>>(),
            )
            .field("hooks", &self.hooks)
            .field("middleware", &self.middleware)
            .field("response_cache", &self.response_cache.is_some())
            .field("policy_config", &self.policy_config)
            .field("permission_policy", &"<policy>");
        #[cfg(feature = "embedded-plugins")]
        debug
            .field("provider", &self.provider)
            .field("plugins", &self.plugins.is_some());
        debug.finish()
    }
}

impl Agent {
    /// Start building an agent.
    #[must_use]
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Generate text using the agent's configured embedded provider.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded provider is configured, provider invocation fails, the
    /// runtime is cancelled, or the provider reports an error.
    #[cfg(feature = "embedded-plugins")]
    pub async fn generate_text(&self, prompt: impl Into<String>) -> Result<GenerateTextResponse> {
        let mut provider: Box<dyn ModelProviderInvoker> =
            self.provider_factory.as_ref().map_or_else(
                || {
                    self.provider
                        .clone()
                        .map(|provider| Box::new(provider) as Box<dyn ModelProviderInvoker>)
                        .ok_or(BcodeError::MissingProvider)
                },
                |factory| Ok(factory()),
            )?;
        self.generate_text_with_provider(&mut provider, prompt)
            .await
    }

    /// Generate text using the configured provider factory.
    ///
    /// # Errors
    ///
    /// Returns an error when no provider factory is configured or provider invocation fails.
    #[cfg(not(feature = "embedded-plugins"))]
    pub async fn generate_text(&self, prompt: impl Into<String>) -> Result<GenerateTextResponse> {
        let mut provider = self
            .provider_factory
            .as_ref()
            .map(|factory| factory())
            .ok_or(BcodeError::MissingProvider)?;
        self.generate_text_with_provider(&mut provider, prompt)
            .await
    }

    /// Run one complete provider/tool turn with a caller-supplied provider.
    ///
    /// Provider-native tool-call batches are automatically authorized, scheduled once, executed,
    /// and returned to the provider in provider order until the provider finishes normally.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation, tool orchestration, cancellation, or timeout
    /// handling fails.
    pub async fn run<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider(provider, prompt).await
    }

    /// Generate text using a caller-supplied provider invoker.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the runtime is cancelled, or the provider
    /// reports an error.
    pub async fn generate_text_with_provider<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider_with_structured_output(provider, prompt, None)
            .await
    }

    /// Generate text using a caller-supplied provider invoker and prior conversation messages.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the runtime is cancelled, or the provider
    /// reports an error.
    pub async fn generate_text_with_provider_and_messages<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        messages: Vec<ModelMessage>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider_and_history(provider, prompt, messages)
            .await
    }

    /// Generate text using a caller-supplied provider invoker and cancellation token.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, cancellation is requested, the turn times
    /// out, or the provider reports an error.
    pub async fn generate_text_with_provider_and_cancellation<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider_with_options(
            provider,
            prompt,
            None,
            Vec::new(),
            cancellation,
        )
        .await
    }

    async fn generate_text_with_provider_and_history<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        messages: Vec<ModelMessage>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider_with_options(
            provider,
            prompt,
            None,
            messages,
            CancellationToken::new(),
        )
        .await
    }

    /// Generate and deserialize a structured object using the agent's embedded provider.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded provider is configured, provider invocation fails,
    /// structured JSON cannot be extracted, schema validation fails, or deserialization fails.
    #[cfg(feature = "embedded-plugins")]
    pub async fn generate_object<T>(&self, prompt: impl Into<String>) -> Result<T>
    where
        T: DeserializeOwned + schemars::JsonSchema,
    {
        let mut provider: Box<dyn ModelProviderInvoker> =
            self.provider_factory.as_ref().map_or_else(
                || {
                    self.provider
                        .clone()
                        .map(|provider| Box::new(provider) as Box<dyn ModelProviderInvoker>)
                        .ok_or(BcodeError::MissingProvider)
                },
                |factory| Ok(factory()),
            )?;
        self.generate_object_with_provider(&mut provider, prompt)
            .await
    }

    /// Generate and deserialize a structured object using the configured provider factory.
    ///
    /// # Errors
    ///
    /// Returns an error when no provider factory is configured, provider invocation fails, or the
    /// structured response is invalid.
    #[cfg(not(feature = "embedded-plugins"))]
    pub async fn generate_object<T>(&self, prompt: impl Into<String>) -> Result<T>
    where
        T: DeserializeOwned + schemars::JsonSchema,
    {
        let mut provider = self
            .provider_factory
            .as_ref()
            .map(|factory| factory())
            .ok_or(BcodeError::MissingProvider)?;
        self.generate_object_with_provider(&mut provider, prompt)
            .await
    }

    /// Generate and deserialize a structured object using a caller-supplied provider invoker.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, structured JSON cannot be extracted,
    /// schema validation fails, or deserialization fails.
    pub async fn generate_object_with_provider<T, P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
    ) -> Result<T>
    where
        T: DeserializeOwned + schemars::JsonSchema,
        P: ModelProviderInvoker,
    {
        self.generate_object_with_provider_and_options(
            provider,
            prompt,
            StructuredOutputOptions::for_type::<T>(),
        )
        .await
    }

    /// Generate and deserialize a structured object using explicit structured-output options.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, structured JSON cannot be extracted,
    /// schema validation fails, or deserialization fails.
    pub async fn generate_object_with_provider_and_options<T, P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        options: StructuredOutputOptions,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        P: ModelProviderInvoker,
    {
        self.generate_object_with_provider_and_request_options(
            provider,
            prompt,
            options,
            Vec::new(),
            CancellationToken::new(),
        )
        .await
    }

    async fn generate_object_with_provider_and_request_options<T, P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        options: StructuredOutputOptions,
        messages: Vec<ModelMessage>,
        cancellation: CancellationToken,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        P: ModelProviderInvoker,
    {
        let prompt = prompt.into();
        let schema = options.schema.clone();
        let structured_output = bcode_model::StructuredOutputRequest {
            name: options.name.clone(),
            schema: schema.clone(),
            strict: options.strict,
        };
        let mut current_prompt = structured_prompt(&prompt, &options);
        let mut last_error = None;
        for attempt in 0..=options.max_repairs {
            let response = self
                .generate_text_with_provider_with_options(
                    provider,
                    current_prompt.clone(),
                    Some(structured_output.clone()),
                    messages.clone(),
                    cancellation.clone(),
                )
                .await?;
            match decode_structured_output(&schema, &response.text) {
                Ok(value) => return Ok(value),
                Err(error) if attempt < options.max_repairs => {
                    current_prompt = repair_prompt(&prompt, &options, &response.text, &error);
                    last_error = Some(error);
                }
                Err(error) if options.max_repairs > 0 => {
                    return Err(BcodeError::StructuredRepairExhausted(format!(
                        "structured output remained invalid after {} repair attempts: {error}",
                        options.max_repairs
                    )));
                }
                Err(error) => return Err(error),
            }
        }
        Err(BcodeError::StructuredRepairExhausted(
            last_error.map_or_else(
                || "structured output repair loop did not run".to_string(),
                |error| {
                    format!(
                        "structured output remained invalid after {} repair attempts: {error}",
                        options.max_repairs
                    )
                },
            ),
        ))
    }

    async fn generate_text_with_provider_with_structured_output<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        structured_output: Option<bcode_model::StructuredOutputRequest>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider_with_options(
            provider,
            prompt,
            structured_output,
            Vec::new(),
            CancellationToken::new(),
        )
        .await
    }

    async fn generate_text_with_provider_with_options<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        structured_output: Option<bcode_model::StructuredOutputRequest>,
        messages: Vec<ModelMessage>,
        cancellation: CancellationToken,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        let prompt = prompt.into();
        let context = self.model_call_context(prompt.clone());
        let request_span = tracing::info_span!(
            target: "bcode::sdk",
            "bcode.model_request",
            session_id = %self.session_id,
            provider_id = self.provider_plugin_id.as_deref().unwrap_or(""),
            model_id = %self.model_id,
            streaming = false,
        );
        async move {
            self.hooks.run_before_model(&context)?;
            let request = self.middleware.before_request(
                self.turn_request_with_structured_output_messages_and_cancellation(
                    prompt,
                    structured_output,
                    messages,
                    cancellation,
                ),
            )?;
            let cache = self.response_cache.as_ref().and_then(|cache| {
                let privacy_allows = cache.privacy(&request) != ModelResponseCachePrivacy::NoStore;
                let tools_allow = request.tools.is_empty() || cache.allow_tool_responses();
                let routing_identified = request.cache_routing_identity.is_some();
                let stopping_identified = request.stop_condition.is_none();
                (privacy_allows && tools_allow && routing_identified && stopping_identified)
                    .then(|| Arc::clone(cache))
            });
            let key = cache
                .as_ref()
                .map(|_| ModelResponseCacheKey::from_request(&request))
                .transpose()?;
            let cached = if let Some(cache) = &cache {
                let response = response_cache_get(Arc::clone(cache), request.clone()).await?;
                record_cache_lookup(&request, response.is_some());
                response
            } else {
                record_cache_bypass(&request);
                None
            };
            let response = if let Some(mut response) = cached {
                response.cache_status = ModelResponseCacheStatus::Hit {
                    key: key.expect("cache key exists for cache hit"),
                };
                response
            } else {
                let runtime_response = self
                    .run_provider_tool_loop(
                        provider,
                        request.clone(),
                        Arc::clone(&self.invocation_event_sink),
                    )
                    .await;
                let runtime_response = match runtime_response {
                    Ok(response) => response,
                    Err(error) => {
                        if let Some(cache) = cache {
                            response_cache_abort(cache, request.clone()).await;
                        }
                        return Err(error);
                    }
                };
                let mut response = GenerateTextResponse::from(runtime_response);
                if let Some(cache) = cache {
                    response.cache_status = ModelResponseCacheStatus::Stored {
                        key: key.expect("cache key exists for cache storage"),
                    };
                    if let Err(error) =
                        response_cache_put(cache.clone(), request.clone(), response.clone()).await
                    {
                        response_cache_abort(cache, request.clone()).await;
                        return Err(error);
                    }
                    record_cache_store(&request);
                }
                response
            };
            record_cost_estimate(
                &request,
                self.model_pricing.as_ref(),
                response.runtime.usage.as_ref(),
            );
            let response = self.middleware.after_response(&request, response)?;
            self.hooks.run_after_model(
                &context,
                &ModelCallOutcome {
                    response: response.clone(),
                },
            )?;
            Ok(response)
        }
        .instrument(request_span)
        .await
    }

    /// Stream text using the agent's configured embedded provider.
    ///
    /// The returned stream yields high-level [`TextStreamItem`] values and does not require the
    /// TUI or daemon when an embedded plugin provider is configured.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded provider is configured.
    #[cfg(feature = "embedded-plugins")]
    pub fn stream_text(&self, prompt: impl Into<String>) -> Result<TextStream> {
        let provider: Box<dyn ModelProviderInvoker> = self.provider_factory.as_ref().map_or_else(
            || {
                self.provider
                    .clone()
                    .map(|provider| Box::new(provider) as Box<dyn ModelProviderInvoker>)
                    .ok_or(BcodeError::MissingProvider)
            },
            |factory| Ok(factory()),
        )?;
        Ok(TextStream::start(
            self,
            provider,
            self.turn_request_with_structured_output_messages_and_cancellation(
                prompt.into(),
                None,
                Vec::new(),
                CancellationToken::new(),
            ),
        ))
    }

    /// Stream text using the configured provider factory.
    ///
    /// # Errors
    ///
    /// Returns an error when no provider factory is configured.
    #[cfg(not(feature = "embedded-plugins"))]
    pub fn stream_text(&self, prompt: impl Into<String>) -> Result<TextStream> {
        let provider = self
            .provider_factory
            .as_ref()
            .map(|factory| factory())
            .ok_or(BcodeError::MissingProvider)?;
        Ok(TextStream::start(
            self,
            provider,
            self.turn_request_with_structured_output_messages_and_cancellation(
                prompt.into(),
                None,
                Vec::new(),
                CancellationToken::new(),
            ),
        ))
    }

    /// Stream text using the agent's configured embedded provider and cancellation token.
    ///
    /// Cancelling the token requests provider cancellation and terminates the stream with a
    /// [`TextStreamItem::Error`] containing [`BcodeError::Runtime`] and
    /// [`RuntimeError::Cancelled`].
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded provider is configured.
    #[cfg(feature = "embedded-plugins")]
    pub fn stream_text_with_cancellation(
        &self,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<TextStream> {
        let provider: Box<dyn ModelProviderInvoker> = self.provider_factory.as_ref().map_or_else(
            || {
                self.provider
                    .clone()
                    .map(|provider| Box::new(provider) as Box<dyn ModelProviderInvoker>)
                    .ok_or(BcodeError::MissingProvider)
            },
            |factory| Ok(factory()),
        )?;
        Ok(TextStream::start(
            self,
            provider,
            self.turn_request_with_cancellation(prompt.into(), cancellation),
        ))
    }

    /// Stream text with cancellation using the configured provider factory.
    ///
    /// # Errors
    ///
    /// Returns an error when no provider factory is configured.
    #[cfg(not(feature = "embedded-plugins"))]
    pub fn stream_text_with_cancellation(
        &self,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<TextStream> {
        let provider = self
            .provider_factory
            .as_ref()
            .map(|factory| factory())
            .ok_or(BcodeError::MissingProvider)?;
        Ok(TextStream::start(
            self,
            provider,
            self.turn_request_with_cancellation(prompt.into(), cancellation),
        ))
    }

    /// Stream text using a caller-supplied provider invoker.
    ///
    /// The returned stream yields text deltas, reasoning deltas, tool-call events, warnings, usage,
    /// a final response, or an error.
    #[must_use]
    pub fn stream_text_with_provider<P>(&self, provider: P, prompt: impl Into<String>) -> TextStream
    where
        P: ModelProviderInvoker + 'static,
    {
        self.stream_text_with_provider_and_cancellation(provider, prompt, CancellationToken::new())
    }

    /// Stream text using a caller-supplied provider invoker and cancellation token.
    ///
    /// Cancelling the token requests provider cancellation and terminates the stream with a
    /// [`TextStreamItem::Error`] containing [`BcodeError::Runtime`] and
    /// [`RuntimeError::Cancelled`].
    #[must_use]
    pub fn stream_text_with_provider_and_cancellation<P>(
        &self,
        provider: P,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> TextStream
    where
        P: ModelProviderInvoker + 'static,
    {
        TextStream::start(
            self,
            provider,
            self.turn_request_with_cancellation(prompt.into(), cancellation),
        )
    }

    /// Stream a complete provider/tool turn through the generic scoped event surface.
    #[must_use]
    pub fn stream<P>(&self, provider: P, prompt: impl Into<String>) -> ScopedAgentStream
    where
        P: ModelProviderInvoker + 'static,
    {
        self.stream_with_cancellation(provider, prompt, CancellationToken::new())
    }

    /// Stream a complete provider/tool turn with explicit cancellation.
    #[must_use]
    pub fn stream_with_cancellation<P>(
        &self,
        provider: P,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> ScopedAgentStream
    where
        P: ModelProviderInvoker + 'static,
    {
        self.stream_request(
            provider,
            self.turn_request_with_cancellation(prompt.into(), cancellation),
        )
    }

    fn stream_request<P>(&self, provider: P, request: AgentTurnRequest) -> ScopedAgentStream
    where
        P: ModelProviderInvoker + 'static,
    {
        let context = self.model_call_context(request.prompt.clone());
        let request_span = tracing::info_span!(
            target: "bcode::sdk",
            "bcode.model_request",
            session_id = %self.session_id,
            provider_id = request.provider_plugin_id.as_deref().unwrap_or(""),
            model_id = %request.model_id,
            streaming = true,
        );
        let _request_enter = request_span.enter();
        let request = self
            .hooks
            .run_before_model(&context)
            .and_then(|()| self.middleware.before_request(request));
        let observer = Arc::new(SdkToolRoundObserver::new(self));
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                return ScopedAgentStream {
                    stream: None,
                    observer,
                    finalizer: None,
                    pending: Some(ScopedAgentStreamItem::Error(error)),
                };
            }
        };
        let invoker: Arc<dyn ToolInvoker> = self.tool_invoker.clone().unwrap_or_else(|| {
            Arc::new(SdkToolInvoker {
                handlers: self.inline_tool_handlers.clone(),
                failure_policy: self.tool_failure_policy,
                #[cfg(feature = "embedded-plugins")]
                session_id: self.session_id,
                #[cfg(feature = "embedded-plugins")]
                plugins: self.plugins.clone(),
            })
        });
        let authorization: Arc<dyn ToolAuthorizationCoordinator> =
            self.authorization_coordinator.clone().unwrap_or_else(|| {
                Arc::new(
                    bcode_agent_runtime::SharedPermissionPolicyAuthorization::new(Arc::clone(
                        &self.permission_policy,
                    )),
                )
            });
        let stream = self.runtime.run_streaming_provider_tool_loop(
            provider,
            request.clone(),
            Arc::new(self.tool_catalog.clone()),
            authorization,
            invoker,
            self.permission_context(),
            Vec::new(),
            self.execution_options,
            Arc::clone(&self.invocation_event_sink),
            self.invocation_capabilities.clone(),
            observer.clone(),
            Arc::clone(&self.provider_round_planner),
        );
        ScopedAgentStream {
            stream: Some(stream),
            observer,
            finalizer: Some(TextStreamFinalizer {
                request,
                context,
                middleware: self.middleware.clone(),
                hooks: self.hooks.clone(),
                model_pricing: self.model_pricing.clone(),
            }),
            pending: None,
        }
    }

    /// Create mutable tool-round state using this agent's configured maximum.
    #[must_use]
    pub const fn tool_round_state(&self) -> ToolRoundState {
        ToolRoundState::new(self.max_tool_rounds)
    }

    /// Execute an ordered tool-call batch as one canonical tool round.
    ///
    /// This is the explicit advanced batch API. It performs one complete-batch authorization pass,
    /// delegates scheduling exactly once to the canonical runtime, and returns per-call results in
    /// provider order.
    ///
    /// # Errors
    ///
    /// Returns an error when the round budget is exhausted or complete-batch authorization fails.
    pub async fn execute_tool_batch_with_round_state(
        &self,
        calls: &[ToolCall],
        rounds: &mut ToolRoundState,
    ) -> Result<ToolBatchExecutionOutput> {
        let scope = TurnScope::with_capabilities(
            format!("sdk-tool-batch:{}", self.session_id),
            TurnGeneration::new(0),
            Arc::clone(&self.invocation_event_sink),
            self.invocation_capabilities.clone(),
        );
        self.execute_tool_batch_in_scope(calls, rounds, &scope)
            .await
    }

    async fn execute_tool_batch_in_scope(
        &self,
        calls: &[ToolCall],
        rounds: &mut ToolRoundState,
        scope: &TurnScope,
    ) -> Result<ToolBatchExecutionOutput> {
        let default_invoker = SdkToolInvoker {
            handlers: self.inline_tool_handlers.clone(),
            failure_policy: self.tool_failure_policy,
            #[cfg(feature = "embedded-plugins")]
            session_id: self.session_id,
            #[cfg(feature = "embedded-plugins")]
            plugins: self.plugins.clone(),
        };
        let invoker = self.tool_invoker.as_deref().unwrap_or(&default_invoker);
        let policy_authorization =
            PermissionPolicyAuthorization::new(self.permission_policy.as_ref());
        let authorization = self
            .authorization_coordinator
            .as_deref()
            .unwrap_or(&policy_authorization);
        self.runtime
            .execute_prepared_tool_batch(
                &self.tool_catalog,
                authorization,
                invoker,
                calls,
                rounds,
                &self.permission_context(),
                self.execution_options,
                scope,
            )
            .await
            .map_err(Into::into)
    }

    /// Execute an ordered tool-call batch using this agent's configured round budget.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute_tool_batch_with_round_state`].
    pub async fn execute_tool_batch(&self, calls: &[ToolCall]) -> Result<ToolBatchExecutionOutput> {
        let mut rounds = self.tool_round_state();
        self.execute_tool_batch_with_round_state(calls, &mut rounds)
            .await
    }

    /// Execute a registered tool call through this agent's unified tool catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the tool is unknown, denied, or its handler fails.
    pub async fn execute_tool_call(&self, call: &ToolCall) -> Result<ToolExecutionOutput> {
        let mut rounds = self.tool_round_state();
        self.execute_tool_call_with_round_state(call, &mut rounds)
            .await
    }

    /// Execute a registered tool call through this agent's canonical batch path and round budget.
    ///
    /// # Errors
    ///
    /// Returns an error when the maximum number of tool rounds is exhausted, the tool is unknown,
    /// denied, or its handler fails.
    pub async fn execute_tool_call_with_round_state(
        &self,
        call: &ToolCall,
        rounds: &mut ToolRoundState,
    ) -> Result<ToolExecutionOutput> {
        let context = ToolCallContext {
            agent_name: self.name.clone(),
            call: call.clone(),
        };
        self.hooks.run_before_tool(&context)?;
        let mut batch = self
            .execute_tool_batch_with_round_state(std::slice::from_ref(call), rounds)
            .await?;
        let output = batch.results.pop().ok_or_else(|| {
            BcodeError::ToolExecution(
                "canonical single-call batch returned no per-call result".to_string(),
            )
        })??;
        self.hooks.run_after_tool(
            &context,
            &ToolCallOutcome {
                output: output.clone(),
            },
        )?;
        Ok(output)
    }

    async fn run_provider_tool_loop<P>(
        &self,
        provider: &mut P,
        request: AgentTurnRequest,
        events: Arc<dyn TurnEventSink>,
    ) -> Result<AgentTurnResponse>
    where
        P: ModelProviderInvoker + ?Sized,
    {
        let mut request = request;
        request.tool_call_policy = self.parallel_tool_capabilities.negotiate(
            request.tool_call_policy.parallel,
            request.tool_call_policy.choice.clone(),
        );
        #[cfg(feature = "embedded-plugins")]
        if let (Some(plugins), Some(provider_plugin_id)) =
            (self.plugins.as_ref(), self.provider_plugin_id.as_deref())
        {
            let provider_capabilities = plugins
                .invoke_service_json_scoped::<(), ProviderCapabilities>(
                    provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    bcode_model::OP_CAPABILITIES,
                    &(),
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .ok();
            let model = plugins
                .invoke_service_json_scoped::<bcode_model::ModelListRequest, ModelList>(
                    provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_MODELS,
                    &bcode_model::ModelListRequest {
                        provider_context: self.provider_context.clone(),
                        selected_model_id: Some(self.model_id.clone()),
                    },
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .ok()
                .and_then(|models| {
                    models
                        .models
                        .into_iter()
                        .find(|model| model.model_id == self.model_id)
                });
            let parallel_feature = RequestedModelFeature::ToolChoice(ToolChoiceMode::Parallel);
            let parallel_guaranteed = provider_capabilities
                .as_ref()
                .zip(model.as_ref())
                .is_some_and(|(provider, model)| {
                    provider
                        .feature_support
                        .negotiate(&model.feature_support, parallel_feature)
                        .is_guaranteed()
                });
            request.tool_call_policy = bcode_model::ParallelToolCallCapabilities {
                provider: parallel_guaranteed,
                model: parallel_guaranteed,
                runtime: true,
            }
            .negotiate(
                self.execution_options.parallel,
                request.tool_call_policy.choice.clone(),
            );
        }
        let default_invoker = SdkToolInvoker {
            handlers: self.inline_tool_handlers.clone(),
            failure_policy: self.tool_failure_policy,
            #[cfg(feature = "embedded-plugins")]
            session_id: self.session_id,
            #[cfg(feature = "embedded-plugins")]
            plugins: self.plugins.clone(),
        };
        let invoker = self.tool_invoker.as_deref().unwrap_or(&default_invoker);
        let policy_authorization =
            PermissionPolicyAuthorization::new(self.permission_policy.as_ref());
        let authorization = self
            .authorization_coordinator
            .as_deref()
            .unwrap_or(&policy_authorization);
        let observer = SdkToolRoundObserver::new(self);
        let result = self
            .runtime
            .run_provider_tool_loop(
                provider,
                request,
                &self.tool_catalog,
                authorization,
                invoker,
                &self.permission_context(),
                &[],
                self.execution_options,
                events,
                self.invocation_capabilities.clone(),
                &observer,
                self.provider_round_planner.as_ref(),
            )
            .await;
        if let Some(error) = observer.take_error() {
            return Err(error);
        }
        result.map_err(Into::into)
    }

    fn permission_context(&self) -> RuntimePermissionContext {
        RuntimePermissionContext {
            session_id: self.session_id,
            agent_id: self.profile_id.clone(),
        }
    }

    fn enabled_tool_definitions(&self) -> Vec<ToolDefinition> {
        let active_tools = active_tools_for(&self.policy_config);
        self.tool_catalog
            .definitions()
            .into_iter()
            .filter(|definition| active_tools.is_empty() || active_tools.contains(&definition.name))
            .collect()
    }

    fn model_call_context(&self, prompt: String) -> ModelCallContext {
        ModelCallContext {
            agent_name: self.name.clone(),
            provider_plugin_id: self.provider_plugin_id.clone(),
            model_id: self.model_id.clone(),
            prompt,
        }
    }

    fn turn_request_with_structured_output_messages_and_cancellation(
        &self,
        prompt: String,
        structured_output: Option<bcode_model::StructuredOutputRequest>,
        messages: Vec<ModelMessage>,
        cancellation: CancellationToken,
    ) -> AgentTurnRequest {
        let mut request = self.turn_request_with_cancellation(prompt, cancellation);
        request.structured_output = structured_output;
        request.messages = messages;
        request
    }

    fn turn_request_with_cancellation(
        &self,
        prompt: String,
        cancellation: CancellationToken,
    ) -> AgentTurnRequest {
        AgentTurnRequest {
            provider_plugin_id: self.provider_plugin_id.clone(),
            model_id: self.model_id.clone(),
            provider_context: self.provider_context.clone(),
            system_prompt: self.system_prompt.clone(),
            messages: Vec::new(),
            prompt,
            append_prompt: true,
            tools: self.enabled_tool_definitions(),
            tool_call_policy: self
                .parallel_tool_capabilities
                .negotiate(self.execution_options.parallel, self.tool_choice.clone()),
            structured_output: None,
            parameters: self.parameters.clone(),
            metadata: self.metadata.clone(),
            timeout: self.timeout,
            max_tool_rounds: self.max_tool_rounds,
            max_repeated_tool_batches: self.max_repeated_tool_batches,
            stop_condition: self.stop_condition.clone(),
            cache_routing_identity: self.cache_routing_identity.clone(),
            cancellation,
        }
    }

    /// Create a frontend-oriented stateful chat session.
    #[must_use]
    pub fn chat(self) -> AgentSession {
        self.session()
    }

    /// Create a stateful in-memory session wrapper for this agent.
    #[must_use]
    pub fn session(self) -> AgentSession {
        AgentSession::new(self, InMemorySession::new())
    }

    /// Create a stateful in-memory session wrapper from caller-managed messages.
    #[must_use]
    pub const fn session_from_messages(self, messages: Vec<ModelMessage>) -> AgentSession {
        AgentSession::new(self, InMemorySession::from_messages(messages))
    }

    /// Create a stateful session backed by an extensible persistence adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when persisted state exists but cannot be read or requires repair.
    pub fn session_with_persistence(
        mut self,
        persistence: Arc<dyn SessionPersistenceAdapter>,
    ) -> Result<AgentSession> {
        let persisted = persistence.load()?;
        let session = if let Some(persisted) = persisted {
            validate_persisted_session(&persisted)?;
            self.session_id = persisted.session_id;
            InMemorySession::from_persisted(persisted.messages, persisted.memories)
        } else {
            InMemorySession::new()
        };
        Ok(AgentSession::new(self, session).with_persistence(persistence))
    }

    /// Create a stateful session backed by an explicit local store.
    ///
    /// # Errors
    ///
    /// Returns an error when the store exists but cannot be read or requires repair.
    pub fn session_with_store(self, store: LocalSessionStore) -> Result<AgentSession> {
        self.session_with_persistence(Arc::new(store))
    }

    /// Return the configured agent name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Return the configured model ID.
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Return provenance for the configured provider/model selection.
    #[must_use]
    pub const fn selection_provenance(&self) -> &ModelSelectionProvenance {
        &self.selection_provenance
    }

    /// Report the final effective selection and all available provenance.
    #[must_use]
    pub fn selection_report(&self) -> ModelSelectionReport {
        ModelSelectionReport {
            selector: self.provider_plugin_id.as_deref().map_or_else(
                || ModelSelector::new(&self.model_id),
                |provider| ModelSelector::with_provider(provider, &self.model_id),
            ),
            provenance: (*self.selection_provenance).clone(),
            registration_source: self.registration_source,
            model_metadata_source: self.model_metadata_source,
            model_pricing: self.model_pricing.clone(),
        }
    }

    /// Decode one configured typed provider request extension.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored extension payload does not match its Rust type.
    pub fn provider_extension<E>(&self) -> Result<Option<E>>
    where
        E: ProviderRequestExtension,
    {
        self.provider_context
            .extension::<E>()
            .map_err(|error| BcodeError::ProviderExtension(error.to_string()))
    }

    /// Return the configured provider request context.
    #[must_use]
    pub const fn provider_context(&self) -> &ProviderRequestContext {
        &self.provider_context
    }
}

/// Builder for [`Agent`].
#[derive(Clone)]
pub struct AgentBuilder {
    runtime: AgentRuntime,
    name: Option<String>,
    profile_id: String,
    session_id: SessionId,
    cwd: Option<PathBuf>,
    provider_plugin_id: Option<String>,
    model_id: Option<String>,
    selection_provenance: Box<ModelSelectionProvenance>,
    registration_source: Option<ProviderRegistrationSource>,
    model_metadata_source: Option<ModelMetadataSource>,
    model_pricing: Option<ModelPricingInfo>,
    provider_context: ProviderRequestContext,
    system_prompt: Option<String>,
    parameters: ModelParameters,
    metadata: BTreeMap<String, String>,
    timeout: Duration,
    max_tool_rounds: u32,
    max_repeated_tool_batches: u32,
    stop_condition: Option<AgentLoopStopPredicate>,
    execution_options: ToolExecutionOptions,
    tool_choice: ToolChoice,
    parallel_tool_capabilities: bcode_model::ParallelToolCallCapabilities,
    tool_failure_policy: ToolFailurePolicy,
    invocation_capabilities: InvocationCapabilities,
    invocation_event_sink: Arc<dyn TurnEventSink>,
    event_persistence: Option<Arc<dyn TurnEventPersistence>>,
    event_observability: Option<Arc<dyn TurnEventObservability>>,
    authorization_coordinator: Option<Arc<dyn ToolAuthorizationCoordinator>>,
    tool_invoker: Option<Arc<dyn ToolInvoker>>,
    provider_factory: Option<ProviderFactory>,
    provider_round_planner: Arc<dyn ProviderRoundPlanner>,
    cache_routing_identity: Option<String>,
    tool_catalog: UnifiedToolCatalog,
    inline_tool_handlers: BTreeMap<String, InlineToolHandler>,
    hooks: AgentHooks,
    middleware: ModelMiddlewareStack,
    response_cache: Option<Arc<dyn ModelResponseCache>>,
    policy_config: AgentConfig,
    permission_ask_callback: Option<PermissionAskCallback>,
    custom_permission_policy: Option<Arc<dyn PermissionPolicy>>,
    #[cfg(feature = "embedded-plugins")]
    provider: Option<PluginModelProviderInvoker>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl fmt::Debug for AgentBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("AgentBuilder");
        debug
            .field("runtime", &self.runtime)
            .field("name", &self.name)
            .field("profile_id", &self.profile_id)
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("provider_plugin_id", &self.provider_plugin_id)
            .field("model_id", &self.model_id)
            .field("selection_provenance", &self.selection_provenance)
            .field("registration_source", &self.registration_source)
            .field("model_metadata_source", &self.model_metadata_source)
            .field("model_pricing", &self.model_pricing)
            .field("provider_context", &self.provider_context)
            .field("system_prompt", &self.system_prompt)
            .field("parameters", &self.parameters)
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("max_repeated_tool_batches", &self.max_repeated_tool_batches)
            .field("stop_condition", &self.stop_condition.is_some())
            .field("execution_options", &self.execution_options)
            .field("tool_choice", &self.tool_choice)
            .field(
                "parallel_tool_capabilities",
                &self.parallel_tool_capabilities,
            )
            .field("tool_failure_policy", &self.tool_failure_policy)
            .field("invocation_capabilities", &self.invocation_capabilities)
            .field("invocation_event_sink", &"<sink>")
            .field("event_persistence", &self.event_persistence.is_some())
            .field("event_observability", &self.event_observability.is_some())
            .field(
                "authorization_coordinator",
                &self.authorization_coordinator.is_some(),
            )
            .field("tool_invoker", &self.tool_invoker.is_some())
            .field("provider_factory", &self.provider_factory.is_some())
            .field("provider_round_planner", &"<planner>")
            .field("cache_routing_identity", &self.cache_routing_identity)
            .field("tool_catalog", &self.tool_catalog)
            .field(
                "inline_tool_handlers",
                &self.inline_tool_handlers.keys().collect::<Vec<_>>(),
            )
            .field("hooks", &self.hooks)
            .field("middleware", &self.middleware)
            .field("response_cache", &self.response_cache.is_some())
            .field("policy_config", &self.policy_config)
            .field(
                "permission_ask_callback",
                &self.permission_ask_callback.is_some(),
            )
            .field(
                "custom_permission_policy",
                &self.custom_permission_policy.is_some(),
            );
        #[cfg(feature = "embedded-plugins")]
        debug
            .field("provider", &self.provider)
            .field("plugins", &self.plugins.is_some());
        debug.finish()
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            runtime: AgentRuntime::new(),
            name: None,
            profile_id: bcode_agent_policy::BUILD_AGENT.to_string(),
            session_id: SessionId::default(),
            cwd: std::env::current_dir().ok(),
            provider_plugin_id: None,
            model_id: None,
            selection_provenance: Box::default(),
            registration_source: None,
            model_metadata_source: None,
            model_pricing: None,
            provider_context: ProviderRequestContext::default(),
            system_prompt: None,
            parameters: ModelParameters::default(),
            metadata: BTreeMap::new(),
            timeout: Duration::from_mins(2),
            max_tool_rounds: 8,
            max_repeated_tool_batches: 2,
            stop_condition: None,
            execution_options: ToolExecutionOptions::default(),
            tool_choice: ToolChoice::Auto,
            parallel_tool_capabilities: bcode_model::ParallelToolCallCapabilities {
                runtime: true,
                ..bcode_model::ParallelToolCallCapabilities::default()
            },
            tool_failure_policy: ToolFailurePolicy::FailTurn,
            invocation_capabilities: InvocationCapabilities::default(),
            invocation_event_sink: Arc::new(DiscardingSdkTurnEventSink),
            event_persistence: None,
            event_observability: None,
            authorization_coordinator: None,
            tool_invoker: None,
            provider_factory: None,
            provider_round_planner: Arc::new(bcode_agent_runtime::NoopProviderRoundPlanner),
            cache_routing_identity: Some("direct".to_string()),
            tool_catalog: UnifiedToolCatalog::new(),
            inline_tool_handlers: BTreeMap::new(),
            hooks: AgentHooks::new(),
            middleware: ModelMiddlewareStack::new(),
            response_cache: None,
            policy_config: bcode_agent_policy::agent_config(
                &bcode_agent_policy::default_config(),
                bcode_agent_policy::BUILD_AGENT,
            ),
            permission_ask_callback: None,
            custom_permission_policy: None,
            #[cfg(feature = "embedded-plugins")]
            provider: None,
            #[cfg(feature = "embedded-plugins")]
            plugins: None,
        }
    }
}

impl AgentBuilder {
    /// Configure the reusable runtime used by this agent.
    #[must_use]
    pub fn runtime(mut self, runtime: AgentRuntime) -> Self {
        self.runtime = runtime;
        self
    }

    /// Configure the embedded provider invoker for this agent.
    #[cfg(feature = "embedded-plugins")]
    #[must_use]
    pub fn provider_invoker(mut self, provider: PluginModelProviderInvoker) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Configure the embedded plugin runtime for provider and plugin-backed tool calls.
    #[cfg(feature = "embedded-plugins")]
    #[must_use]
    pub fn plugin_runtime(mut self, plugins: bcode_plugin::PluginRuntimeHost) -> Self {
        self.provider = Some(PluginModelProviderInvoker::new(plugins.clone()));
        self.plugins = Some(plugins);
        self
    }

    /// Configure provider/model capability support used for parallel tool-call negotiation.
    #[must_use]
    pub const fn parallel_tool_capabilities(
        mut self,
        capabilities: bcode_model::ParallelToolCallCapabilities,
    ) -> Self {
        self.parallel_tool_capabilities = capabilities;
        self
    }

    /// Configure a human-readable agent name.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Configure the selected model ID.
    #[must_use]
    pub fn model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = Some(model_id.into());
        self.selection_provenance.model = Some(ModelSelectionSource::PerRequest);
        self.model_metadata_source = None;
        self.model_pricing = None;
        self.parallel_tool_capabilities.model = false;
        self
    }

    /// Configure pricing metadata for the selected model.
    ///
    /// Cost observations derived from this metadata are always labeled as estimates and retain
    /// currency and pricing-source provenance.
    #[must_use]
    pub fn model_pricing(mut self, pricing: ModelPricingInfo) -> Self {
        self.model_pricing = Some(pricing);
        self
    }

    /// Configure provider/model selection from a [`ModelSelector`].
    #[must_use]
    pub fn model_selector(mut self, selector: impl Into<ModelSelector>) -> Self {
        let selector = selector.into();
        self.provider_plugin_id = selector.provider_plugin_id;
        self.model_id = Some(selector.model_id);
        self.selection_provenance.model = Some(ModelSelectionSource::PerRequest);
        self.model_metadata_source = None;
        self.model_pricing = None;
        if self.provider_plugin_id.is_some() {
            self.selection_provenance.provider = Some(ModelSelectionSource::PerRequest);
        }
        self.registration_source = None;
        self.parallel_tool_capabilities.provider = false;
        self.parallel_tool_capabilities.model = false;
        self
    }

    /// Configure a specific provider plugin ID.
    #[must_use]
    pub fn provider_plugin(mut self, provider_plugin_id: impl Into<String>) -> Self {
        self.provider_plugin_id = Some(provider_plugin_id.into());
        self.selection_provenance.provider = Some(ModelSelectionSource::PerRequest);
        self.registration_source = None;
        self.model_metadata_source = None;
        self.model_pricing = None;
        self.parallel_tool_capabilities.provider = false;
        self.parallel_tool_capabilities.model = false;
        self
    }

    /// Configure provenance for a provider/model selection supplied by an SDK registry.
    #[must_use]
    pub fn selection_provenance(mut self, provenance: ModelSelectionProvenance) -> Self {
        self.selection_provenance = Box::new(provenance);
        self
    }

    /// Configure a complete registry-derived selection report.
    #[must_use]
    pub fn selection_report(mut self, report: ModelSelectionReport) -> Self {
        self.provider_plugin_id = report.selector.provider_plugin_id;
        self.model_id = Some(report.selector.model_id);
        self.selection_provenance = Box::new(report.provenance);
        self.registration_source = report.registration_source;
        self.model_metadata_source = report.model_metadata_source;
        self.model_pricing = report.model_pricing;
        self
    }

    /// Configure provider request context.
    #[must_use]
    pub fn provider_context(mut self, provider_context: ProviderRequestContext) -> Self {
        self.provider_context = provider_context;
        self
    }

    /// Configure one typed provider-specific request extension.
    ///
    /// The extension is scoped to its owning provider and serialized through the canonical model
    /// request. Providers reject extensions sent to the wrong provider or API surface.
    ///
    /// # Errors
    ///
    /// Returns an error when the extension cannot be serialized.
    pub fn provider_extension<E>(mut self, extension: &E) -> Result<Self>
    where
        E: ProviderRequestExtension,
    {
        self.provider_context
            .set_extension(extension)
            .map_err(|error| BcodeError::ProviderExtension(error.to_string()))?;
        Ok(self)
    }

    /// Configure the system prompt.
    #[must_use]
    pub fn system(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(system_prompt.into());
        self
    }

    /// Configure model parameters.
    #[must_use]
    pub fn parameters(mut self, parameters: ModelParameters) -> Self {
        self.parameters = parameters;
        self
    }

    /// Add one metadata key/value pair sent to providers.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Configure turn timeout.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Configure maximum tool rounds.
    #[must_use]
    pub const fn max_tool_rounds(mut self, max_tool_rounds: u32) -> Self {
        self.max_tool_rounds = max_tool_rounds;
        self
    }

    /// Configure maximum consecutive semantically identical tool-call batches.
    #[must_use]
    pub const fn max_repeated_tool_batches(mut self, max_repeated_tool_batches: u32) -> Self {
        self.max_repeated_tool_batches = max_repeated_tool_batches;
        self
    }

    /// Stop a successful loop after a completed provider round when `condition` returns true.
    ///
    /// A stop on a tool-call round happens before tools execute. It is reported as
    /// [`AgentLoopTerminationReason::StopCondition`].
    #[must_use]
    pub fn stop_when(mut self, condition: impl AgentLoopStopCondition + 'static) -> Self {
        self.stop_condition = Some(AgentLoopStopPredicate::new(condition));
        self
    }

    /// Configure how inline tool handler failures affect provider/tool loops.
    #[must_use]
    pub const fn tool_failure_policy(mut self, policy: ToolFailurePolicy) -> Self {
        self.tool_failure_policy = policy;
        self
    }

    /// Configure provider-neutral model tool-choice behavior.
    #[must_use]
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = choice;
        self
    }

    /// Configure canonical tool batch scheduling and execution options.
    #[must_use]
    pub const fn execution_options(mut self, options: ToolExecutionOptions) -> Self {
        self.execution_options = options;
        self
    }

    /// Configure the invocation exchange broker.
    #[must_use]
    pub fn exchange_broker(mut self, broker: Arc<dyn InvocationExchangeBroker>) -> Self {
        self.invocation_capabilities = self.invocation_capabilities.with_exchange_broker(broker);
        self
    }

    /// Configure explicit renderer-free exchange behavior.
    #[must_use]
    pub fn headless_exchange_policy(mut self, policy: HeadlessExchangePolicy) -> Self {
        self.invocation_capabilities = self
            .invocation_capabilities
            .with_exchange_broker(Arc::new(policy));
        self
    }

    /// Configure the invocation input router.
    #[must_use]
    pub fn input_router(mut self, router: Arc<dyn InvocationInputRouter>) -> Self {
        self.invocation_capabilities = self.invocation_capabilities.with_input_router(router);
        self
    }

    /// Configure the nested invocation service router.
    #[must_use]
    pub fn service_router(mut self, router: Arc<dyn InvocationServiceRouter>) -> Self {
        self.invocation_capabilities = self.invocation_capabilities.with_service_router(router);
        self
    }

    /// Configure the invocation artifact sink.
    #[must_use]
    pub fn artifact_sink(mut self, sink: Arc<dyn InvocationArtifactSink>) -> Self {
        self.invocation_capabilities = self.invocation_capabilities.with_artifact_sink(sink);
        self
    }

    /// Configure the nonblocking scoped invocation event sink.
    #[must_use]
    pub fn invocation_event_sink(mut self, sink: Arc<dyn TurnEventSink>) -> Self {
        self.invocation_event_sink = sink;
        self
    }

    /// Configure neutral runtime-event persistence admission.
    #[must_use]
    pub fn event_persistence(mut self, persistence: Arc<dyn TurnEventPersistence>) -> Self {
        self.event_persistence = Some(persistence);
        self
    }

    /// Configure neutral runtime-event observability.
    #[must_use]
    pub fn event_observability(mut self, observability: Arc<dyn TurnEventObservability>) -> Self {
        self.event_observability = Some(observability);
        self
    }

    /// Configure the complete-batch authorization coordinator.
    #[must_use]
    pub fn authorization_coordinator(
        mut self,
        coordinator: Arc<dyn ToolAuthorizationCoordinator>,
    ) -> Self {
        self.authorization_coordinator = Some(coordinator);
        self
    }

    /// Configure a canonical tool invoker registry/adapter.
    #[must_use]
    pub fn tool_invoker(mut self, invoker: Arc<dyn ToolInvoker>) -> Self {
        self.tool_invoker = Some(invoker);
        self
    }

    /// Configure a provider factory used by the agent's default generation and streaming methods.
    #[must_use]
    pub fn provider_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn() -> Box<dyn ModelProviderInvoker> + Send + Sync + 'static,
    {
        self.provider_factory = Some(Arc::new(factory));
        self
    }

    /// Configure provider retry, recovery, compaction, and request rebuilding policy.
    ///
    /// Custom planners disable response caching until [`Self::cache_routing_identity`] supplies a
    /// stable versioned identity for their effective routing behavior.
    #[must_use]
    pub fn provider_round_planner(mut self, planner: Arc<dyn ProviderRoundPlanner>) -> Self {
        self.provider_round_planner = planner;
        self.cache_routing_identity = None;
        self
    }

    /// Identify custom retry/fallback/routing behavior for safe response-cache keys.
    #[must_use]
    pub fn cache_routing_identity(mut self, identity: impl Into<String>) -> Self {
        self.cache_routing_identity = Some(identity.into());
        self
    }

    /// Configure fixed-delay retry behavior for provider-originated failures.
    #[must_use]
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.provider_round_planner = Arc::new(policy);
        self.cache_routing_identity = Some(format!("retry:{policy:?}"));
        self
    }

    /// Configure ordered provider/model fallbacks for provider-originated failures.
    #[must_use]
    pub fn fallback_policy(mut self, policy: FallbackPolicy) -> Self {
        self.cache_routing_identity = Some(format!("fallback:{policy:?}"));
        self.provider_round_planner = Arc::new(policy);
        self
    }

    /// Register a typed inline SDK tool.
    ///
    /// The input schema is derived from `I`. Invocation arguments are decoded before the handler
    /// runs, and the `O` result is serialized for both model-visible text and structured consumers.
    #[must_use]
    pub fn typed_tool<I, O, F>(self, tool: TypedTool<I, O>, handler: F) -> Self
    where
        I: DeserializeOwned + schemars::JsonSchema + Send + 'static,
        O: Serialize + Send + 'static,
        F: Fn(I) -> std::result::Result<O, String> + Send + Sync + 'static,
    {
        let schema = tool.definition.input_schema.clone();
        self.inline_tool(tool.definition, move |request| {
            validate_typed_tool_input(&schema, &request.arguments)?;
            let input = serde_json::from_value(request.arguments)
                .map_err(|error| format!("invalid typed tool arguments: {error}"))?;
            let output = handler(input)?;
            typed_tool_success_response(output)
        })
    }

    /// Register an asynchronous typed tool with cancellation, progress, and per-call context.
    ///
    /// Application errors remain structured for consumers while only their explicit
    /// `model_message` is returned to the model.
    #[must_use]
    pub fn typed_tool_async<I, O, D, F, Fut>(self, tool: TypedTool<I, O>, handler: F) -> Self
    where
        I: DeserializeOwned + schemars::JsonSchema + Send + 'static,
        O: Serialize + Send + 'static,
        D: Serialize + Send + 'static,
        F: Fn(I, TypedToolContext<()>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<O, ToolApplicationError<D>>>
            + Send
            + 'static,
    {
        self.typed_tool_with_state(tool, Arc::new(()), handler)
    }

    /// Register an asynchronous typed tool with explicitly supplied application state.
    ///
    /// Input is validated against the generated schema before deserialization. The handler gets
    /// canonical cancellation, ordered progress reporting, invocation identity, and read-only
    /// shared access to `state`.
    #[must_use]
    pub fn typed_tool_with_state<I, O, D, S, F, Fut>(
        self,
        tool: TypedTool<I, O>,
        state: Arc<S>,
        handler: F,
    ) -> Self
    where
        I: DeserializeOwned + schemars::JsonSchema + Send + 'static,
        O: Serialize + Send + 'static,
        D: Serialize + Send + 'static,
        S: Send + Sync + 'static,
        F: Fn(I, TypedToolContext<S>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<O, ToolApplicationError<D>>>
            + Send
            + 'static,
    {
        let schema = tool.definition.input_schema.clone();
        let handler = Arc::new(handler);
        self.scoped_inline_tool(tool.definition, move |request, scope| {
            let state = Arc::clone(&state);
            let handler = Arc::clone(&handler);
            let schema = schema.clone();
            async move {
                validate_typed_tool_input(&schema, &request.arguments)?;
                let input = serde_json::from_value(request.arguments)
                    .map_err(|error| format!("invalid typed tool arguments: {error}"))?;
                let context = TypedToolContext::new(scope, state);
                match handler(input, context).await {
                    Ok(output) => typed_tool_success_response(output),
                    Err(error) => typed_tool_error_response(error),
                }
            }
        })
    }

    /// Register an inline SDK tool.
    ///
    /// The supplied definition is exposed to providers as a normal [`ToolDefinition`], while the
    /// handler executes in-process when the runtime routes a matching tool call.
    #[must_use]
    pub fn inline_tool<F>(mut self, definition: ToolDefinition, handler: F) -> Self
    where
        F: Fn(ToolInvocationDescriptor) -> std::result::Result<ToolInvocationResponse, String>
            + Send
            + Sync
            + 'static,
    {
        let name = definition.name.clone();
        self.tool_catalog.insert(RegisteredTool::inline(definition));
        self.inline_tool_handlers.insert(
            name,
            Arc::new(move |request, _scope| {
                let result = handler(request);
                Box::pin(async move { result })
            }),
        );
        self
    }

    /// Register an asynchronous inline SDK tool that receives its canonical invocation scope.
    #[must_use]
    pub fn scoped_inline_tool<F, Fut>(mut self, definition: ToolDefinition, handler: F) -> Self
    where
        F: Fn(ToolInvocationDescriptor, InvocationScope) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<ToolInvocationResponse, String>>
            + Send
            + 'static,
    {
        let name = definition.name.clone();
        self.tool_catalog.insert(RegisteredTool::inline(definition));
        self.inline_tool_handlers.insert(
            name,
            Arc::new(move |request, scope| Box::pin(handler(request, scope))),
        );
        self
    }

    /// Register a plugin-backed tool definition with routing metadata.
    #[must_use]
    pub fn plugin_tool(mut self, definition: ToolDefinition, plugin_id: impl Into<String>) -> Self {
        self.tool_catalog
            .insert(RegisteredTool::plugin(definition, plugin_id));
        self
    }

    /// Configure an application-owned response cache for non-streaming model requests.
    #[must_use]
    pub fn response_cache(mut self, cache: Arc<dyn ModelResponseCache>) -> Self {
        self.response_cache = Some(cache);
        self
    }

    /// Configure model request/response middleware.
    #[must_use]
    pub fn middleware(mut self, middleware: ModelMiddlewareStack) -> Self {
        self.middleware = middleware;
        self
    }

    /// Append one model request/response middleware layer.
    #[must_use]
    pub fn middleware_layer<M>(mut self, middleware: M) -> Self
    where
        M: ModelMiddleware + 'static,
    {
        self.middleware = self.middleware.layer(middleware);
        self
    }

    /// Configure hooks for model and tool calls.
    #[must_use]
    pub fn hooks(mut self, hooks: AgentHooks) -> Self {
        self.hooks = hooks;
        self
    }

    /// Add a hook that runs before model calls.
    #[must_use]
    pub fn on_before_model<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ModelCallContext) -> Result<()> + Send + Sync + 'static,
    {
        self.hooks = self.hooks.on_before_model(hook);
        self
    }

    /// Add a hook that runs after successful model calls.
    #[must_use]
    pub fn on_after_model<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ModelCallContext, &ModelCallOutcome) -> Result<()> + Send + Sync + 'static,
    {
        self.hooks = self.hooks.on_after_model(hook);
        self
    }

    /// Add a hook that runs before tool calls.
    #[must_use]
    pub fn on_before_tool<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ToolCallContext) -> Result<()> + Send + Sync + 'static,
    {
        self.hooks = self.hooks.on_before_tool(hook);
        self
    }

    /// Add a hook that runs after successful tool calls.
    #[must_use]
    pub fn on_after_tool<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ToolCallContext, &ToolCallOutcome) -> Result<()> + Send + Sync + 'static,
    {
        self.hooks = self.hooks.on_after_tool(hook);
        self
    }

    /// Configure the active agent/profile ID used by shared policy evaluation.
    #[must_use]
    pub fn agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.profile_id = agent_id.into();
        self.policy_config = bcode_agent_policy::agent_config(
            &bcode_agent_policy::default_config(),
            &self.profile_id,
        );
        self.custom_permission_policy = None;
        self
    }

    /// Configure the session ID used by shared policy evaluation.
    #[must_use]
    pub const fn session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = session_id;
        self
    }

    /// Configure the working directory used by shared path-boundary policy evaluation.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Configure a resolved agent policy from the shared Bcode permission model.
    #[must_use]
    pub fn agent_config(mut self, config: AgentConfig) -> Self {
        self.policy_config = config;
        self.custom_permission_policy = None;
        self
    }

    /// Configure a resolved agent policy and ask callback from the shared Bcode permission model.
    #[must_use]
    pub fn agent_config_with_ask<F>(mut self, config: AgentConfig, callback: F) -> Self
    where
        F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
            + Send
            + Sync
            + 'static,
    {
        self.policy_config = config;
        self.permission_ask_callback = Some(ask_callback(callback));
        self.custom_permission_policy = None;
        self
    }

    /// Configure a multi-agent policy config from the shared Bcode permission model.
    #[must_use]
    pub fn agent_permission_config(mut self, config: &AgentPermissionConfig) -> Self {
        self.policy_config = bcode_agent_policy::agent_config(config, &self.profile_id);
        self.custom_permission_policy = None;
        self
    }

    /// Configure a multi-agent policy config and ask callback from the shared Bcode permission model.
    #[must_use]
    pub fn agent_permission_config_with_ask<F>(
        mut self,
        config: &AgentPermissionConfig,
        callback: F,
    ) -> Self
    where
        F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
            + Send
            + Sync
            + 'static,
    {
        self.policy_config = bcode_agent_policy::agent_config(config, &self.profile_id);
        self.permission_ask_callback = Some(ask_callback(callback));
        self.custom_permission_policy = None;
        self
    }

    /// Configure a callback used only when shared policy returns `ask`.
    #[must_use]
    pub fn on_permission_request<F>(mut self, callback: F) -> Self
    where
        F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
            + Send
            + Sync
            + 'static,
    {
        self.permission_ask_callback = Some(ask_callback(callback));
        self
    }

    /// Configure a custom permission policy implementation.
    #[must_use]
    pub fn custom_permission_policy<P>(mut self, policy: P) -> Self
    where
        P: PermissionPolicy + 'static,
    {
        self.custom_permission_policy = Some(Arc::new(policy));
        self
    }

    /// Discover plugin-backed tools from the configured embedded plugin runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when no plugin runtime is configured or a plugin tool service fails.
    #[cfg(feature = "embedded-plugins")]
    pub async fn discover_plugin_tools(mut self) -> Result<Self> {
        let plugins = self
            .plugins
            .clone()
            .ok_or(BcodeError::MissingPluginRuntime)?;
        let registry = plugins.registry().service_registry();
        let Some(providers) = registry.providers_for(bcode_tool::TOOL_SERVICE_INTERFACE_ID) else {
            return Ok(self);
        };
        for plugin_id in providers {
            let list: bcode_tool::ToolList = plugins
                .invoke_service_json_scoped(
                    plugin_id,
                    bcode_tool::TOOL_SERVICE_INTERFACE_ID,
                    bcode_tool::OP_LIST_TOOLS,
                    &bcode_tool::ListToolsRequest::default(),
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| BcodeError::ToolExecution(error.to_string()))?;
            for definition in list.tools {
                self.tool_catalog
                    .insert(RegisteredTool::plugin(definition, plugin_id.clone()));
            }
        }
        Ok(self)
    }

    fn permission_policy(&self) -> Arc<dyn PermissionPolicy> {
        self.custom_permission_policy.clone().unwrap_or_else(|| {
            Arc::new(
                AgentPermissionPolicy::new(self.policy_config.clone())
                    .with_working_directory(self.cwd.clone().unwrap_or_else(|| PathBuf::from(".")))
                    .with_ask_callback(self.permission_ask_callback.clone()),
            )
        })
    }

    /// Build the agent.
    #[must_use]
    pub fn build(self) -> Agent {
        let permission_policy = self.permission_policy();
        let invocation_event_sink: Arc<dyn TurnEventSink> =
            if self.event_persistence.is_some() || self.event_observability.is_some() {
                let mut sink = HostTurnEventSink::new(Arc::clone(&self.invocation_event_sink));
                if let Some(persistence) = self.event_persistence.clone() {
                    sink = sink.with_persistence(persistence);
                }
                if let Some(observability) = self.event_observability.clone() {
                    sink = sink.with_observability(observability);
                }
                Arc::new(sink)
            } else {
                Arc::clone(&self.invocation_event_sink)
            };
        Agent {
            runtime: self.runtime,
            name: self.name,
            profile_id: self.profile_id,
            session_id: self.session_id,
            cwd: self.cwd,
            provider_plugin_id: self.provider_plugin_id,
            model_id: self.model_id.unwrap_or_default(),
            selection_provenance: self.selection_provenance,
            registration_source: self.registration_source,
            model_metadata_source: self.model_metadata_source,
            model_pricing: self.model_pricing,
            provider_context: self.provider_context,
            system_prompt: self.system_prompt,
            parameters: self.parameters,
            metadata: self.metadata,
            timeout: self.timeout,
            max_tool_rounds: self.max_tool_rounds,
            max_repeated_tool_batches: self.max_repeated_tool_batches,
            stop_condition: self.stop_condition,
            execution_options: self.execution_options,
            tool_choice: self.tool_choice,
            parallel_tool_capabilities: self.parallel_tool_capabilities,
            tool_failure_policy: self.tool_failure_policy,
            invocation_capabilities: self.invocation_capabilities,
            invocation_event_sink,
            authorization_coordinator: self.authorization_coordinator,
            tool_invoker: self.tool_invoker,
            provider_factory: self.provider_factory,
            provider_round_planner: self.provider_round_planner,
            cache_routing_identity: self.cache_routing_identity,
            tool_catalog: self.tool_catalog,
            inline_tool_handlers: self.inline_tool_handlers,
            hooks: self.hooks,
            middleware: self.middleware,
            response_cache: self.response_cache,
            policy_config: self.policy_config,
            permission_policy,
            #[cfg(feature = "embedded-plugins")]
            provider: self.provider,
            #[cfg(feature = "embedded-plugins")]
            plugins: self.plugins,
        }
    }
}

/// Request for text generation.
#[derive(Debug, Clone)]
pub struct GenerateTextRequest {
    /// User prompt.
    pub prompt: String,
}

/// One normalized step in a completed multi-step generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GenerationStep {
    /// One provider model round, including accumulated text/reasoning and its latest usage.
    Model {
        /// Zero-based provider round.
        round: u32,
        /// Assistant text emitted during this model round.
        text: String,
        /// Reasoning text emitted during this model round.
        reasoning: String,
        /// Latest token usage emitted during this model round.
        usage: Option<TokenUsage>,
        /// Ordered non-content runtime metadata emitted during this model round.
        metadata: Vec<AgentEvent>,
    },
    /// A complete model-requested tool call.
    ToolCall {
        /// Zero-based provider round that requested the call.
        round: u32,
        /// Provider-supplied tool call.
        call: ToolCall,
    },
    /// A model-visible tool result produced by the runtime.
    ToolResult {
        /// Zero-based provider round that requested the tool.
        round: u32,
        /// Tool result sent back to the provider.
        result: ToolResult,
    },
    /// Final completed response summary.
    FinalResponse {
        /// Final accumulated assistant text.
        text: String,
        /// Provider-reported stop reason.
        stop_reason: Option<StopReason>,
        /// Why the successful loop stopped.
        termination_reason: AgentLoopTerminationReason,
        /// Total generation latency in milliseconds.
        latency_ms: u64,
    },
}

/// Response from text generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateTextResponse {
    /// Generated assistant text.
    pub text: String,
    /// Ordered normalized model/tool/final steps.
    pub steps: Vec<GenerationStep>,
    /// Response-cache provenance. Cached usage remains historical provider usage.
    #[serde(default)]
    pub cache_status: ModelResponseCacheStatus,
    /// Runtime response containing metadata and events.
    pub runtime: AgentTurnResponse,
}

impl From<AgentTurnResponse> for GenerateTextResponse {
    fn from(runtime: AgentTurnResponse) -> Self {
        Self {
            text: runtime.text.clone(),
            steps: generation_steps(&runtime),
            cache_status: ModelResponseCacheStatus::Bypassed,
            runtime,
        }
    }
}

fn generation_steps(runtime: &AgentTurnResponse) -> Vec<GenerationStep> {
    struct ModelRound {
        round: u32,
        started: bool,
        text: String,
        reasoning: String,
        usage: Option<TokenUsage>,
        metadata: Vec<AgentEvent>,
    }

    fn flush_model(steps: &mut Vec<GenerationStep>, model: &mut ModelRound) {
        if model.started {
            steps.push(GenerationStep::Model {
                round: model.round,
                text: std::mem::take(&mut model.text),
                reasoning: std::mem::take(&mut model.reasoning),
                usage: model.usage.take(),
                metadata: std::mem::take(&mut model.metadata),
            });
            model.started = false;
        }
    }

    let mut steps = Vec::new();
    let mut model = ModelRound {
        round: 0,
        started: false,
        text: String::new(),
        reasoning: String::new(),
        usage: None,
        metadata: Vec::new(),
    };
    for event in &runtime.events {
        match event {
            AgentEvent::TurnStarted => {
                if !model.started {
                    model.round = steps
                        .iter()
                        .filter_map(|step| match step {
                            GenerationStep::Model { round, .. } => Some(round.saturating_add(1)),
                            _ => None,
                        })
                        .max()
                        .unwrap_or_default();
                    model.started = true;
                }
            }
            AgentEvent::TextDelta(delta) => {
                model.started = true;
                model.text.push_str(delta);
            }
            AgentEvent::ReasoningDelta(delta) => {
                model.started = true;
                model.reasoning.push_str(delta);
            }
            AgentEvent::Usage(usage) => {
                model.started = true;
                model.usage = Some(usage.clone());
            }
            AgentEvent::ToolCallFinished(call) => {
                flush_model(&mut steps, &mut model);
                steps.push(GenerationStep::ToolCall {
                    round: model.round,
                    call: call.clone(),
                });
            }
            AgentEvent::ToolResult(result) => steps.push(GenerationStep::ToolResult {
                round: model.round,
                result: result.clone(),
            }),
            event @ (AgentEvent::ExactRequestInputTokens(_)
            | AgentEvent::RequestProjection(_)
            | AgentEvent::ContextCompacted
            | AgentEvent::ProviderMetadata { .. }
            | AgentEvent::RetryScheduled { .. }
            | AgentEvent::Warning(_)
            | AgentEvent::ProviderError { .. }) => {
                model.started = true;
                model.metadata.push(event.clone());
            }
            AgentEvent::ToolCallStarted { .. }
            | AgentEvent::ToolCallDelta { .. }
            | AgentEvent::Finished { .. }
            | AgentEvent::Cancelled => {}
        }
    }
    flush_model(&mut steps, &mut model);
    steps.push(GenerationStep::FinalResponse {
        text: runtime.text.clone(),
        stop_reason: runtime.stop_reason,
        termination_reason: runtime.termination_reason,
        latency_ms: runtime.latency_ms,
    });
    steps
}
