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
    AgentRuntime, PermissionPolicyAuthorization, ToolBatchExecutionOutput, TurnGeneration,
    TurnScope,
};
#[cfg(feature = "embedded-plugins")]
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    ModelTurnRequest, OP_CANCEL_TURN, OP_CAPABILITIES, OP_FINISH_TURN, OP_MODELS,
    OP_POLL_TURN_EVENTS, OP_START_TURN, PollTurnEventsRequest, PollTurnEventsResponse,
    StartTurnResponse,
};
use bcode_model::{ModelParameters, ProviderRequestContext};
use bcode_plugin_sdk::path::display_from_current_dir;
#[cfg(feature = "embedded-plugins")]
use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse};
use bcode_session_models::SessionId;
use futures::Stream;
use pin_project_lite::pin_project;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;

pub use bcode_agent_permissions::{AgentPermissionPolicy, allow_all_agent_policy};
pub use bcode_agent_policy::{Action, AgentConfig, AgentPermissionConfig, PermissionConfig};
pub use bcode_agent_profile::{AgentDecision, EvaluateToolCallResponse};
pub use bcode_agent_runtime::{
    AgentRuntimeEvent as AgentEvent, AgentRuntimeStream as AgentStream,
    AgentRuntimeStreamItem as AgentStreamItem, AgentTurnRequest, AgentTurnResponse, AllowAllPolicy,
    CancellationToken, DEFAULT_STREAM_BUFFER_CAPACITY, ModelProviderInvoker, PermissionDecision,
    PermissionPolicy, ProviderRoundPlan, ProviderRoundPlanContext, ProviderRoundPlanner,
    RegisteredTool, RuntimeError, RuntimeFuture, RuntimePermissionContext,
    RuntimePermissionRequest, ToolCatalog, ToolExecutionOutput, ToolRoundObserver, ToolRoundState,
    ToolSource, UnifiedToolCatalog,
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
    ContentBlock as ModelContentBlock, MessageRole, ModelInfo, ModelList, ModelMessage,
    ProviderCapabilities, ProviderTurnEvent, StopReason, TokenUsage, ToolCall, ToolChoice,
    ToolResult,
};
pub use bcode_tool::PreparedToolInvocation;
pub use bcode_tool::{
    ListToolsRequest, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifactWriteRequest,
    ToolArtifactWriteResolution, ToolDefinition, ToolExchangeRequest, ToolExchangeResolution,
    ToolExchangeResponsePolicy, ToolExecutionOptions, ToolInvocationDescriptor,
    ToolInvocationInput, ToolInvocationInputResolution, ToolInvocationResponse,
    ToolInvocationResult, ToolInvocationServiceRequest, ToolInvocationServiceResolution, ToolList,
    ToolPolicyMetadata, ToolPreparationRequest, ToolPreparationResponse, ToolSideEffect,
    ToolUiMetadata,
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
    delay: Duration,
}

impl RetryPolicy {
    /// Create a retry policy with a maximum retry count and fixed delay.
    #[must_use]
    pub const fn new(max_retries: u32, delay: Duration) -> Self {
        Self { max_retries, delay }
    }

    /// Return the maximum number of retries after the initial attempt.
    #[must_use]
    pub const fn max_retries(self) -> u32 {
        self.max_retries
    }

    /// Return the fixed delay before each retry.
    #[must_use]
    pub const fn delay(self) -> Duration {
        self.delay
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
            let retryable = matches!(
                failure,
                RuntimeError::ProviderInvocation(_) | RuntimeError::Provider { .. }
            );
            Ok(if retryable && context.attempt <= self.max_retries {
                ProviderRoundPlan::RetryAfter {
                    request: context.proposed_request.clone(),
                    delay: self.delay,
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
            if !matches!(
                failure,
                RuntimeError::ProviderInvocation(_) | RuntimeError::Provider { .. }
            ) {
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
    generate_text_builder().prompt(prompt).run(provider).await
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
    generate_text_builder()
        .messages(messages)
        .prompt(prompt)
        .run(provider)
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
    generate_text_builder()
        .prompt(prompt)
        .cancellation(cancellation)
        .run(provider)
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
    generate_text_builder()
        .model(model)
        .prompt(prompt)
        .run(provider)
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
    /// Tool execution failed.
    #[error("tool execution error: {0}")]
    ToolExecution(String),
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

    /// Store a completed response for this request.
    ///
    /// # Errors
    ///
    /// Returns an error when cache storage fails and the application treats that failure as
    /// terminal.
    fn put(&self, request: &AgentTurnRequest, response: &GenerateTextResponse) -> Result<()>;
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
            request = middleware.before_request(request)?;
        }
        Ok(request)
    }

    fn after_response(
        &self,
        request: &AgentTurnRequest,
        mut response: GenerateTextResponse,
    ) -> Result<GenerateTextResponse> {
        for middleware in self.middleware.iter().rev() {
            response = middleware.after_response(request, response)?;
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
            hook(context)?;
        }
        Ok(())
    }

    fn run_after_model(
        &self,
        context: &ModelCallContext,
        outcome: &ModelCallOutcome,
    ) -> Result<()> {
        for hook in &self.after_model {
            hook(context, outcome)?;
        }
        Ok(())
    }

    fn run_before_tool(&self, context: &ToolCallContext) -> Result<()> {
        for hook in &self.before_tool {
            hook(context)?;
        }
        Ok(())
    }

    fn run_after_tool(&self, context: &ToolCallContext, outcome: &ToolCallOutcome) -> Result<()> {
        for hook in &self.after_tool {
            hook(context, outcome)?;
        }
        Ok(())
    }
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
        return TextStreamItem::Error(BcodeError::Hook(
            "text stream terminal response was finalized more than once".to_string(),
        ));
    };
    let response = GenerateTextResponse::from(response);
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
        Err(error) => TextStreamItem::Error(error),
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
                                host_action: None,
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
                        execute_plugin_tool(
                            self.plugins.as_ref(),
                            plugin_id,
                            &descriptor,
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

/// Provider registry entry used by the SDK facade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRegistration {
    /// Provider plugin ID.
    pub provider_plugin_id: String,
    /// Provider capability metadata, when discovered or supplied.
    pub capabilities: Option<ProviderCapabilities>,
    /// Provider model metadata, when discovered or supplied.
    pub models: Option<ModelList>,
}

/// Explicit SDK provider registry/default facade.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderRegistration>,
    default_model: Option<ModelSelector>,
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
        }
        if let Some(model_id) = selection.model_id.as_deref() {
            registry.default_model = Some(selection.provider_plugin_id.as_deref().map_or_else(
                || ModelSelector::new(model_id),
                |provider_plugin_id| ModelSelector::with_provider(provider_plugin_id, model_id),
            ));
        }
        registry
    }

    /// Return selected provider/model capability support for parallel tool calls.
    #[must_use]
    pub fn parallel_tool_capabilities(
        &self,
        selector: &ModelSelector,
    ) -> bcode_model::ParallelToolCallCapabilities {
        let Some(provider_id) = selector.provider_plugin_id.as_deref() else {
            return bcode_model::ParallelToolCallCapabilities::default();
        };
        let Some(registration) = self.providers.get(provider_id) else {
            return bcode_model::ParallelToolCallCapabilities::default();
        };
        let provider = registration
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| {
                capabilities
                    .capabilities
                    .contains(&bcode_model::ProviderCapability::ParallelToolCalls)
            });
        let model = registration.models.as_ref().is_some_and(|models| {
            models.models.iter().any(|model| {
                model.model_id == selector.model_id
                    && model
                        .capabilities
                        .contains(&bcode_model::ModelCapability::ParallelToolCalls)
            })
        });
        bcode_model::ParallelToolCallCapabilities {
            provider,
            model,
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
                capabilities: None,
                models: None,
            });
        self
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
                capabilities: None,
                models: None,
            });
        entry.models = Some(models);
        self
    }

    /// Configure the default provider/model selector used by agents built from [`Bcode`].
    #[must_use]
    pub fn default_model(mut self, model: impl Into<ModelSelector>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Return the default model selector.
    #[must_use]
    pub const fn default_model_selector(&self) -> Option<&ModelSelector> {
        self.default_model.as_ref()
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
        if let Some(selector) = self.provider_registry.default_model_selector() {
            builder
                .model_selector(selector.clone())
                .parallel_tool_capabilities(
                    self.provider_registry.parallel_tool_capabilities(selector),
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

/// On-disk payload used by [`LocalSessionStore`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Session ID associated with the transcript.
    pub session_id: SessionId,
    /// Caller-managed conversation transcript.
    pub messages: Vec<ModelMessage>,
}

/// Persistence adapter for SDK-managed conversation sessions.
///
/// Adapters own their storage behavior and should return `Ok(None)` when no persisted session
/// exists. Corrupt, stale, or otherwise unusable state should return a descriptive [`BcodeError`]
/// rather than silently discarding data.
pub trait SessionPersistenceAdapter: Send + Sync {
    /// Load a persisted session, or return `Ok(None)` when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when persisted state exists but cannot be read or safely decoded.
    fn load(&self) -> Result<Option<PersistedSession>>;

    /// Save the complete persisted session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session cannot be encoded or durably stored by the adapter.
    fn save(&self, session: &PersistedSession) -> Result<()>;
}

/// Explicit local JSON session store for SDK-managed persistence.
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
        serde_json::from_str(&contents).map(Some).map_err(|error| {
            BcodeError::SessionState(format!(
                "session store {} is corrupt and requires repair or replacement: {error}",
                display_from_current_dir(&self.path)
            ))
        })
    }

    /// Save the complete session payload atomically enough for local SDK use.
    ///
    /// # Errors
    ///
    /// Returns an error when parent directories or files cannot be written.
    pub fn save(&self, session: &PersistedSession) -> Result<()> {
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
        Self { messages }
    }

    /// Return the current session transcript.
    #[must_use]
    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
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

    /// Clear the in-memory transcript.
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

/// Application-owned context/memory extension for SDK sessions.
///
/// Providers can retrieve memory, summaries, user profile context, or application state and return
/// normal model messages for the next turn. Returned messages are request context only: they are not
/// appended to or persisted with the visible session transcript.
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
}

impl fmt::Debug for AgentSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSession")
            .field("agent", &self.agent)
            .field("session", &self.session)
            .field("persistence", &self.persistence.is_some())
            .field("context_providers", &self.context_providers.len())
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

    /// Return the configured persistence adapter, when persistence was explicitly enabled.
    #[must_use]
    pub fn persistence(&self) -> Option<&dyn SessionPersistenceAdapter> {
        self.persistence.as_deref()
    }

    /// Return the session payload that can be saved by caller-managed persistence.
    #[must_use]
    pub fn persisted_session(&self) -> PersistedSession {
        PersistedSession {
            session_id: self.agent.session_id,
            messages: self.session.messages.clone(),
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

    fn request_messages(&self, transcript: &[ModelMessage]) -> Result<Vec<ModelMessage>> {
        let mut messages = Vec::new();
        for provider in &self.context_providers {
            messages.extend(provider.context_messages(&self.session)?);
        }
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
        let prior_messages = self.request_messages(&self.session.messages[..user_index])?;
        let response = self
            .agent
            .generate_text_with_provider_and_history(provider, prompt, prior_messages)
            .await?;
        self.session.messages.truncate(user_index);
        self.session.messages.push(user_message);
        self.session
            .messages
            .push(assistant_message(response.text.clone()));
        if self.persistence.is_some() {
            self.save()?;
        }
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
        let messages = self.request_messages(&self.session.messages)?;
        let response = self
            .agent
            .generate_text_with_provider_and_history(provider, prompt.clone(), messages)
            .await?;
        self.session.messages.push(user_message(prompt));
        self.session
            .messages
            .push(assistant_message(response.text.clone()));
        if self.persistence.is_some() {
            self.save()?;
        }
        Ok(response)
    }
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
    provider_context: ProviderRequestContext,
    system_prompt: Option<String>,
    parameters: ModelParameters,
    metadata: BTreeMap<String, String>,
    timeout: Duration,
    max_tool_rounds: u32,
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
            .field("provider_context", &self.provider_context)
            .field("system_prompt", &self.system_prompt)
            .field("parameters", &self.parameters)
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("max_tool_rounds", &self.max_tool_rounds)
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
        self.hooks.run_before_model(&context)?;
        let request = self.middleware.before_request(
            self.turn_request_with_structured_output_messages_and_cancellation(
                prompt,
                structured_output,
                messages,
                cancellation,
            ),
        )?;
        let response = if let Some(response) = self
            .response_cache
            .as_ref()
            .map(|cache| cache.get(&request))
            .transpose()?
            .flatten()
        {
            response
        } else {
            let response = self
                .run_provider_tool_loop(
                    provider,
                    request.clone(),
                    Arc::clone(&self.invocation_event_sink),
                )
                .await?;
            let response = GenerateTextResponse::from(response);
            if let Some(cache) = &self.response_cache {
                cache.put(&request, &response)?;
            }
            response
        };
        let response = self.middleware.after_response(&request, response)?;
        self.hooks.run_after_model(
            &context,
            &ModelCallOutcome {
                response: response.clone(),
            },
        )?;
        Ok(response)
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
            let provider = plugins
                .invoke_service_json_scoped::<(), ProviderCapabilities>(
                    provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    bcode_model::OP_CAPABILITIES,
                    &(),
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .is_ok_and(|capabilities| {
                    capabilities
                        .capabilities
                        .contains(&bcode_model::ProviderCapability::ParallelToolCalls)
                });
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
                })
                .is_some_and(|model| {
                    model
                        .capabilities
                        .contains(&bcode_model::ModelCapability::ParallelToolCalls)
                });
            request.tool_call_policy = bcode_model::ParallelToolCallCapabilities {
                provider,
                model,
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
            self.session_id = persisted.session_id;
            InMemorySession::from_messages(persisted.messages)
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
    provider_context: ProviderRequestContext,
    system_prompt: Option<String>,
    parameters: ModelParameters,
    metadata: BTreeMap<String, String>,
    timeout: Duration,
    max_tool_rounds: u32,
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
            .field("provider_context", &self.provider_context)
            .field("system_prompt", &self.system_prompt)
            .field("parameters", &self.parameters)
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("max_tool_rounds", &self.max_tool_rounds)
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
            provider_context: ProviderRequestContext::default(),
            system_prompt: None,
            parameters: ModelParameters::default(),
            metadata: BTreeMap::new(),
            timeout: Duration::from_mins(2),
            max_tool_rounds: 8,
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
        self.parallel_tool_capabilities.model = false;
        self
    }

    /// Configure provider/model selection from a [`ModelSelector`].
    #[must_use]
    pub fn model_selector(mut self, selector: impl Into<ModelSelector>) -> Self {
        let selector = selector.into();
        self.provider_plugin_id = selector.provider_plugin_id;
        self.model_id = Some(selector.model_id);
        self.parallel_tool_capabilities.provider = false;
        self.parallel_tool_capabilities.model = false;
        self
    }

    /// Configure a specific provider plugin ID.
    #[must_use]
    pub fn provider_plugin(mut self, provider_plugin_id: impl Into<String>) -> Self {
        self.provider_plugin_id = Some(provider_plugin_id.into());
        self.parallel_tool_capabilities.provider = false;
        self.parallel_tool_capabilities.model = false;
        self
    }

    /// Configure provider request context.
    #[must_use]
    pub fn provider_context(mut self, provider_context: ProviderRequestContext) -> Self {
        self.provider_context = provider_context;
        self
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
    #[must_use]
    pub fn provider_round_planner(mut self, planner: Arc<dyn ProviderRoundPlanner>) -> Self {
        self.provider_round_planner = planner;
        self
    }

    /// Configure fixed-delay retry behavior for provider-originated failures.
    #[must_use]
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.provider_round_planner = Arc::new(policy);
        self
    }

    /// Configure ordered provider/model fallbacks for provider-originated failures.
    #[must_use]
    pub fn fallback_policy(mut self, policy: FallbackPolicy) -> Self {
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
        self.inline_tool(tool.definition, move |request| {
            let input = serde_json::from_value(request.arguments)
                .map_err(|error| format!("invalid typed tool arguments: {error}"))?;
            let output = handler(input)?;
            let value = serde_json::to_value(output)
                .map_err(|error| format!("failed to serialize typed tool result: {error}"))?;
            let encoded = serde_json::to_string(&value)
                .map_err(|error| format!("failed to encode typed tool result: {error}"))?;
            Ok(ToolInvocationResponse {
                output: encoded.clone(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: Some(ToolInvocationResult::Json { value: encoded }),
            })
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
            provider_context: self.provider_context,
            system_prompt: self.system_prompt,
            parameters: self.parameters,
            metadata: self.metadata,
            timeout: self.timeout,
            max_tool_rounds: self.max_tool_rounds,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        /// Total generation latency in milliseconds.
        latency_ms: u128,
    },
}

/// Response from text generation.
#[derive(Debug, Clone)]
pub struct GenerateTextResponse {
    /// Generated assistant text.
    pub text: String,
    /// Ordered normalized model/tool/final steps.
    pub steps: Vec<GenerationStep>,
    /// Runtime response containing metadata and events.
    pub runtime: AgentTurnResponse,
}

impl From<AgentTurnResponse> for GenerateTextResponse {
    fn from(runtime: AgentTurnResponse) -> Self {
        Self {
            text: runtime.text.clone(),
            steps: generation_steps(&runtime),
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
    }

    fn flush_model(steps: &mut Vec<GenerationStep>, model: &mut ModelRound) {
        if model.started {
            steps.push(GenerationStep::Model {
                round: model.round,
                text: std::mem::take(&mut model.text),
                reasoning: std::mem::take(&mut model.reasoning),
                usage: model.usage.take(),
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
            AgentEvent::ToolCallStarted { .. }
            | AgentEvent::ToolCallDelta { .. }
            | AgentEvent::ExactRequestInputTokens(_)
            | AgentEvent::RequestProjection(_)
            | AgentEvent::ContextCompacted
            | AgentEvent::ProviderMetadata { .. }
            | AgentEvent::RetryScheduled { .. }
            | AgentEvent::Warning(_)
            | AgentEvent::ProviderError { .. }
            | AgentEvent::Finished { .. }
            | AgentEvent::Cancelled => {}
        }
    }
    flush_model(&mut steps, &mut model);
    steps.push(GenerationStep::FinalResponse {
        text: runtime.text.clone(),
        stop_reason: runtime.stop_reason,
        latency_ms: runtime.latency_ms,
    });
    steps
}
