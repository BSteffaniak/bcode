#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Reusable agent turn runtime for Bcode SDK, daemon, and product surfaces.
//!
//! This crate owns the provider/tool/policy boundary for a single agent turn without depending on
//! daemon IPC or TUI code. Higher-level crates supply concrete provider, tool, and permission
//! implementations. Contract ownership and dependency direction are documented in
//! `docs/tool-runtime-contract-ownership.md`.
//!
//! # Scheduler invariants
//!
//! * Provider event occurrence defines batch order; provider call IDs remain opaque and unchanged.
//! * Every call in the current provider batch is prepared before one complete-batch authorization
//!   request, and no approved invocation starts until that authorization request resolves.
//! * Parallel mode overlaps approved calls mechanically, optionally bounded by positive
//!   `max_concurrency`; sequential mode executes singleton groups. The scheduler never infers
//!   conflicts from tool names, arguments, commands, paths, URLs, or authorization facts.
//! * Completion order may differ from provider order, but ordered results are supplied to the next
//!   provider round only after the complete current batch reaches terminal outcomes. One provider
//!   batch consumes one tool round.
//! * Turn cancellation closes queued starts, signals registered active handles, and gates normal
//!   output before later provider rounds can begin.

pub mod turn;

pub use turn::{
    ArtifactCommitGuard, HostTurnEventSink, InvocationArtifactSink, InvocationCancellation,
    InvocationCapabilities, InvocationCapabilityFuture, InvocationExchangeBroker,
    InvocationInputRouter, InvocationScope, InvocationServiceRouter, PreparationScope,
    ScopedTurnEvent, TurnControl, TurnEventObservability, TurnEventPersistence, TurnEventSink,
    TurnGeneration, TurnLifecycle, TurnScope, TurnScopeOwner,
};

use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MessageRole, ModelMessage,
    ModelParameters, ModelTurnRequest, PollTurnEventsRequest, PollTurnEventsResponse,
    ProviderError, ProviderRequestContext, ProviderRequestProjection, ProviderTurnEvent,
    StartTurnResponse, StopReason, TokenUsage, ToolCall, ToolResult,
};
use bcode_session_models::SessionId;
use bcode_tool::{
    PreparedToolInvocation, ToolAuthorizationFact, ToolDefinition, ToolExecutionOptions,
    ToolInvocationDescriptor, ToolInvocationResponse, ToolPreparationRequest,
    ToolPreparationResponse, ToolResultContent as InvocationToolResultContent,
};
use futures::{StreamExt, stream};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Notify, mpsc};

/// Boxed future returned by runtime extension traits.
pub type RuntimeFuture<'a, T> =
    Pin<Box<dyn Future<Output = std::result::Result<T, RuntimeError>> + Send + 'a>>;

/// Agent runtime result alias.
pub type Result<T> = std::result::Result<T, RuntimeError>;

/// Errors produced by the reusable agent runtime.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Provider operation failed before it could be represented as a model event.
    #[error("provider invocation failed: {0}")]
    ProviderInvocation(String),
    /// Provider reported a structured model error.
    #[error("provider error {code}: {message}")]
    Provider {
        /// Provider-specific error code.
        code: String,
        /// Human-readable provider error message.
        message: String,
        /// Full provider-reported error metadata.
        error: Box<ProviderError>,
    },
    /// The turn was cancelled before completion.
    #[error("agent turn cancelled")]
    Cancelled,
    /// The turn did not complete before its timeout.
    #[error("agent turn timed out after {timeout:?}")]
    Timeout {
        /// Configured timeout for the turn.
        timeout: Duration,
    },
    /// Provider completed a tool-call round without supplying any completed calls.
    #[error("provider finished with tool_call but emitted no completed tool calls")]
    EmptyProviderToolRound,
    /// A host extension failed while observing canonical orchestration.
    #[error("host extension failed: {0}")]
    HostExtension(String),
    /// A tool was requested but no executor could handle it.
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    /// Tool execution failed.
    #[error("tool execution failed for {tool_name}: {message}")]
    ToolExecution {
        /// Tool name.
        tool_name: String,
        /// Human-readable error message.
        message: String,
    },
    /// Tool execution requires user permission.
    #[error("tool execution requires permission: {0}")]
    PermissionRequired(String),
    /// Tool execution was denied by policy.
    #[error("tool execution denied: {0}")]
    PermissionDenied(String),
    /// The runtime reached its configured maximum tool rounds.
    #[error("maximum tool rounds reached: {0}")]
    MaxToolRounds(u32),
    /// A host adapter returned an invalid batch response.
    #[error("invalid {component} batch response: expected {expected} decisions, received {actual}")]
    InvalidBatchResponse {
        /// Adapter component that returned the invalid response.
        component: &'static str,
        /// Required response count.
        expected: usize,
        /// Actual response count.
        actual: usize,
    },
    /// A host supplied invalid or oversized opaque tool context.
    #[error("invalid tool host context: {0}")]
    InvalidToolHostContext(String),
    /// Tool preparation failed.
    #[error("tool preparation failed for {tool_name}: {message}")]
    ToolPreparation {
        /// Tool whose preparation failed.
        tool_name: String,
        /// Human-readable preparation failure.
        message: String,
    },
    /// Tool preparation exceeded its configured bound.
    #[error("tool preparation timed out for {tool_name} after {timeout:?}")]
    ToolPreparationTimeout {
        /// Tool whose preparation timed out.
        tool_name: String,
        /// Configured per-invocation preparation timeout.
        timeout: Duration,
    },
}

/// Cancellation state shared between callers and a running turn.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    /// Create a new uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark this token as cancelled.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Return whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Request for one stateless agent turn.
#[derive(Debug, Clone)]
pub struct AgentTurnRequest {
    /// Optional provider plugin ID. `None` lets the provider implementation choose a default.
    pub provider_plugin_id: Option<String>,
    /// Selected model ID. Empty means provider default.
    pub model_id: String,
    /// Provider-specific request context.
    pub provider_context: ProviderRequestContext,
    /// Optional system prompt.
    pub system_prompt: Option<String>,
    /// Prior conversation messages included before this turn's user prompt.
    pub messages: Vec<ModelMessage>,
    /// User prompt for this turn.
    pub prompt: String,
    /// Whether `prompt` should be appended as a new user message.
    pub append_prompt: bool,
    /// Model-callable tool definitions available to the provider.
    pub tools: Vec<ToolDefinition>,
    /// Host-resolved provider tool-call policy.
    pub tool_call_policy: bcode_model::ToolCallRequestPolicy,
    /// Provider-native structured output request.
    pub structured_output: Option<bcode_model::StructuredOutputRequest>,
    /// Model parameters.
    pub parameters: ModelParameters,
    /// Host-defined metadata forwarded to the provider.
    pub metadata: BTreeMap<String, String>,
    /// Turn timeout.
    pub timeout: Duration,
    /// Maximum number of tool rounds allowed by the caller.
    pub max_tool_rounds: u32,
    /// Cancellation token for this turn.
    pub cancellation: CancellationToken,
}

impl AgentTurnRequest {
    /// Create a basic stateless turn request.
    #[must_use]
    pub fn new(model_id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            provider_plugin_id: None,
            model_id: model_id.into(),
            provider_context: ProviderRequestContext::default(),
            system_prompt: None,
            messages: Vec::new(),
            prompt: prompt.into(),
            append_prompt: true,
            tools: Vec::new(),
            tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
            structured_output: None,
            parameters: ModelParameters::default(),
            metadata: BTreeMap::new(),
            timeout: Duration::from_mins(2),
            max_tool_rounds: 8,
            cancellation: CancellationToken::new(),
        }
    }
}

/// Completed text-generation turn response.
#[derive(Debug, Clone)]
pub struct AgentTurnResponse {
    /// Accumulated assistant text.
    pub text: String,
    /// Provider-reported stop reason, when the provider finished normally.
    pub stop_reason: Option<StopReason>,
    /// Last provider-reported token usage snapshot, when available.
    pub usage: Option<TokenUsage>,
    /// Total turn latency in milliseconds.
    pub latency_ms: u128,
    /// Runtime events observed during the turn.
    pub events: Vec<AgentRuntimeEvent>,
}

/// Normalized runtime event exposed independently from provider-specific details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRuntimeEvent {
    /// The provider accepted the turn.
    TurnStarted,
    /// Assistant text delta.
    TextDelta(String),
    /// Reasoning text delta.
    ReasoningDelta(String),
    /// A tool call started.
    ToolCallStarted {
        /// Provider tool-call ID.
        call_id: String,
        /// Tool name.
        name: String,
    },
    /// Incremental tool-call arguments.
    ToolCallDelta {
        /// Provider tool-call ID.
        call_id: String,
        /// Argument delta.
        delta: String,
    },
    /// Provider completed a tool call request.
    ToolCallFinished(ToolCall),
    /// Runtime completed a tool call and produced a model-visible result.
    ToolResult(ToolResult),
    /// Token usage snapshot.
    Usage(TokenUsage),
    /// Provider confirmed complete request-input context usage.
    ExactRequestInputTokens(bcode_model::ExactRequestInputTokens),
    /// Provider reported actual request projection metadata.
    RequestProjection(ProviderRequestProjection),
    /// Provider compacted the active context while serving the turn.
    ContextCompacted,
    /// Provider-specific metadata used for invisible optimization state.
    ProviderMetadata {
        /// Metadata key.
        key: String,
        /// Metadata value.
        value: String,
    },
    /// Provider scheduled a retry.
    RetryScheduled {
        /// Human-readable retry message.
        message: String,
        /// Unix timestamp when retry is scheduled.
        retry_at_unix: u64,
    },
    /// Provider warning.
    Warning(String),
    /// Provider error.
    ProviderError {
        /// Provider-specific error code.
        code: String,
        /// Human-readable provider error message.
        message: String,
    },
    /// Provider finished the turn.
    Finished {
        /// Provider stop reason.
        stop_reason: StopReason,
        /// Last provider-reported token usage snapshot, when available.
        usage: Option<TokenUsage>,
        /// Total turn latency in milliseconds when the finish event was emitted.
        latency_ms: u128,
    },
    /// Turn was cancelled.
    Cancelled,
}

/// Item produced by a streaming text-generation turn.
#[derive(Debug)]
pub enum AgentRuntimeStreamItem {
    /// Normalized runtime event.
    Event(AgentRuntimeEvent),
    /// Completed turn response.
    Finished(AgentTurnResponse),
    /// Runtime error that terminated the stream.
    Error(RuntimeError),
}

/// Typed asynchronous stream of agent runtime events.
#[derive(Debug)]
pub struct AgentRuntimeStream {
    receiver: mpsc::UnboundedReceiver<AgentRuntimeStreamItem>,
}

impl AgentRuntimeStream {
    /// Receive the next stream item.
    pub async fn next(&mut self) -> Option<AgentRuntimeStreamItem> {
        self.receiver.recv().await
    }
}

/// Item produced by the canonical provider/tool loop stream.
#[derive(Debug)]
pub enum AgentLoopStreamItem {
    /// Provider, runtime, lifecycle, or contribution event from the active scope.
    Event(ScopedTurnEvent),
    /// Completed provider/tool conversation.
    Finished(AgentTurnResponse),
    /// Runtime error that terminated the conversation.
    Error(RuntimeError),
}

/// Unified stream for one complete canonical provider/tool conversation.
#[derive(Debug)]
pub struct AgentLoopStream {
    receiver: mpsc::UnboundedReceiver<AgentLoopStreamItem>,
}

impl AgentLoopStream {
    /// Receive the next scoped stream item.
    pub async fn next(&mut self) -> Option<AgentLoopStreamItem> {
        self.receiver.recv().await
    }
}

struct LoopStreamEventSink {
    configured: Arc<dyn TurnEventSink>,
    sender: mpsc::UnboundedSender<AgentLoopStreamItem>,
}

impl TurnEventSink for LoopStreamEventSink {
    fn emit(&self, event: ScopedTurnEvent) -> bool {
        if !self.configured.emit(event.clone()) {
            return false;
        }
        self.sender.send(AgentLoopStreamItem::Event(event)).is_ok()
    }
}

struct SharedToolCatalog(Arc<dyn ToolCatalog>);

impl ToolCatalog for SharedToolCatalog {
    fn tools(&self) -> Vec<RegisteredTool> {
        self.0.tools()
    }

    fn find_tool(&self, name: &str) -> Option<RegisteredTool> {
        self.0.find_tool(name)
    }
}

#[derive(Debug, Clone)]
struct RuntimeStreamEventSink {
    sender: Option<mpsc::UnboundedSender<AgentRuntimeStreamItem>>,
}

impl TurnEventSink for RuntimeStreamEventSink {
    fn emit(&self, event: ScopedTurnEvent) -> bool {
        let ScopedTurnEvent::Runtime(event) = event else {
            return false;
        };
        self.sender
            .as_ref()
            .is_none_or(|sender| sender.send(AgentRuntimeStreamItem::Event(event)).is_ok())
    }
}

/// Abstract provider invocation boundary used by the runtime.
pub trait ModelProviderInvoker: Send {
    /// Start a model turn.
    fn start_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse>;

    /// Poll model turn events.
    fn poll_turn_events<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse>;

    /// Cancel an active model turn.
    fn cancel_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse>;

    /// Finish and clean up an active model turn.
    fn finish_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse>;
}

impl<T> ModelProviderInvoker for Box<T>
where
    T: ModelProviderInvoker + ?Sized,
{
    fn start_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        (**self).start_turn(provider_plugin_id, request)
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        (**self).poll_turn_events(provider_plugin_id, request)
    }

    fn cancel_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        (**self).cancel_turn(provider_plugin_id, request)
    }

    fn finish_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        (**self).finish_turn(provider_plugin_id, request)
    }
}

/// Source used to route a registered model-callable tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// Tool is implemented by an SDK caller in-process.
    Inline,
    /// Tool is implemented by a plugin service.
    Plugin {
        /// Plugin ID that owns the tool implementation.
        plugin_id: String,
    },
}

/// Model-callable tool with routing metadata attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredTool {
    /// Model-visible tool definition.
    pub definition: ToolDefinition,
    /// Runtime routing source.
    pub source: ToolSource,
}

impl RegisteredTool {
    /// Create an inline tool registration.
    #[must_use]
    pub const fn inline(definition: ToolDefinition) -> Self {
        Self {
            definition,
            source: ToolSource::Inline,
        }
    }

    /// Create a plugin-owned tool registration.
    #[must_use]
    pub fn plugin(definition: ToolDefinition, plugin_id: impl Into<String>) -> Self {
        Self {
            definition,
            source: ToolSource::Plugin {
                plugin_id: plugin_id.into(),
            },
        }
    }
}

/// Tool execution output normalized for model feedback and host/UI consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionOutput {
    /// Model-visible tool result.
    pub model_result: ToolResult,
    /// Full typed invocation response returned by the executor.
    pub invocation: ToolInvocationResponse,
    /// Normalized runtime events emitted for this execution.
    pub events: Vec<AgentRuntimeEvent>,
}

/// Ordered outputs from one provider tool-call round.
#[derive(Debug)]
pub struct ToolBatchExecutionOutput {
    /// Per-call execution results in the same order as the requested calls.
    pub results: Vec<Result<ToolExecutionOutput>>,
}

/// Host observer for product behavior around canonical tool rounds.
pub trait ToolRoundObserver: Send + Sync {
    /// Observe one complete provider tool-call batch before preparation begins.
    ///
    /// # Errors
    ///
    /// Returns an error when host-owned pre-invocation behavior rejects the batch.
    fn before_tool_batch(&self, _calls: &[ToolCall]) -> Result<()> {
        Ok(())
    }

    /// Observe one successful tool result before it is added to provider continuation context.
    ///
    /// # Errors
    ///
    /// Returns an error when host-owned post-invocation behavior fails.
    fn after_tool_call(&self, _call: &ToolCall, _output: &ToolExecutionOutput) -> Result<()> {
        Ok(())
    }
}

/// Tool-round observer that performs no host-specific work.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopToolRoundObserver;

impl ToolRoundObserver for NoopToolRoundObserver {}

/// Input supplied to a host-owned provider round planner before provider work starts.
pub struct ProviderRoundPlanContext<'a> {
    /// Zero-based provider round within the canonical turn.
    pub round: u32,
    /// Zero-based attempt within `round`.
    pub attempt: u32,
    /// Complete request proposed for this attempt.
    pub proposed_request: &'a AgentTurnRequest,
    /// Failure from the prior attempt, when recovery is being planned.
    pub previous_failure: Option<&'a RuntimeError>,
    /// Canonical turn scope shared by planning, provider work, and tool continuation.
    pub scope: &'a TurnScope,
}

/// Directive returned by a host-owned provider round planner.
#[derive(Debug)]
pub enum ProviderRoundPlan {
    /// Start provider work immediately with a complete request.
    Proceed {
        /// Complete request to execute.
        request: AgentTurnRequest,
    },
    /// Wait through the canonical cancellation boundary, then retry with a complete request.
    RetryAfter {
        /// Complete replacement request to execute after `delay`.
        request: AgentTurnRequest,
        /// Host-policy-resolved retry delay.
        delay: Duration,
    },
    /// Stop planning with an optional host-normalized terminal error.
    ///
    /// When `error` is `None` after a provider attempt, the runtime preserves the prior provider
    /// failure. Before any provider attempt, `error` must be present.
    Fail {
        /// Host-selected terminal error, when it should replace the prior provider failure.
        error: Option<RuntimeError>,
    },
}

/// Host extension for retry, recovery, compaction, and complete provider-request rebuilding.
pub trait ProviderRoundPlanner: Send + Sync {
    /// Plan one provider attempt without moving product policy into the runtime.
    fn plan_round<'a>(
        &'a self,
        context: ProviderRoundPlanContext<'a>,
    ) -> RuntimeFuture<'a, ProviderRoundPlan>;
}

/// Provider planner that runs each round once and preserves the first provider failure.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopProviderRoundPlanner;

impl ProviderRoundPlanner for NoopProviderRoundPlanner {
    fn plan_round<'a>(
        &'a self,
        context: ProviderRoundPlanContext<'a>,
    ) -> RuntimeFuture<'a, ProviderRoundPlan> {
        Box::pin(async move {
            Ok(if context.previous_failure.is_some() {
                ProviderRoundPlan::Fail { error: None }
            } else {
                ProviderRoundPlan::Proceed {
                    request: context.proposed_request.clone(),
                }
            })
        })
    }
}

#[derive(Debug, Clone)]
struct PreparedRuntimeToolCall {
    index: usize,
    call: ToolCall,
    tool: RegisteredTool,
    invocation: PreparedToolInvocation,
}

/// Mutable state that enforces a maximum number of tool rounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRoundState {
    max_tool_rounds: u32,
    completed_rounds: u32,
}

impl ToolRoundState {
    /// Create tool-round state with a maximum number of permitted rounds.
    #[must_use]
    pub const fn new(max_tool_rounds: u32) -> Self {
        Self {
            max_tool_rounds,
            completed_rounds: 0,
        }
    }

    /// Return configured maximum tool rounds.
    #[must_use]
    pub const fn max_tool_rounds(&self) -> u32 {
        self.max_tool_rounds
    }

    /// Return completed tool rounds.
    #[must_use]
    pub const fn completed_rounds(&self) -> u32 {
        self.completed_rounds
    }

    const fn begin_round(&mut self) -> Result<()> {
        if self.completed_rounds >= self.max_tool_rounds {
            return Err(RuntimeError::MaxToolRounds(self.max_tool_rounds));
        }
        self.completed_rounds = self.completed_rounds.saturating_add(1);
        Ok(())
    }
}

/// Neutral adapter that prepares and invokes tools regardless of their transport.
pub trait ToolInvoker: Send + Sync {
    /// Prepare one invocation without performing its side effects.
    ///
    /// Implementations must only inspect the request, opaque host context, and tool-owned state;
    /// they must not mutate external state, start externally visible work, or require cleanup.
    /// The runtime bounds this future by the configured preparation timeout and turn cancellation.
    fn prepare_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        request: &'a ToolPreparationRequest,
        scope: &'a PreparationScope,
    ) -> RuntimeFuture<'a, ToolPreparationResponse>;

    /// Return an opaque cancellation handle before the invocation becomes externally active.
    ///
    /// Invokers without external work may use the default `None` implementation.
    fn cancellation_handle(
        &self,
        _tool: &RegisteredTool,
        _invocation: &PreparedToolInvocation,
    ) -> Option<Arc<dyn InvocationCancellation>> {
        None
    }

    /// Execute a previously prepared invocation.
    fn invoke_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        invocation: &'a PreparedToolInvocation,
        scope: &'a InvocationScope,
    ) -> RuntimeFuture<'a, ToolInvocationResponse>;
}

/// One prepared call supplied to a batch authorization coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAuthorizationRequest {
    /// Input order in the provider batch.
    pub index: usize,
    /// Requested provider tool call.
    pub call: ToolCall,
    /// Resolved tool registration.
    pub tool: RegisteredTool,
    /// Tool-owner-produced authorization facts.
    pub facts: Vec<ToolAuthorizationFact>,
    /// Stable host permission context.
    pub context: RuntimePermissionContext,
}

/// Decision returned by a batch authorization coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAuthorizationDecision {
    /// Permit invocation.
    Allow,
    /// Require host/user approval that the coordinator did not resolve.
    Ask(String),
    /// Reject invocation with a model-visible reason.
    Deny(String),
}

/// Host-injected authorization coordinator that evaluates a complete prepared batch.
pub trait ToolAuthorizationCoordinator: Send + Sync {
    /// Authorize every request and return decisions in matching order.
    fn authorize_batch<'a>(
        &'a self,
        requests: &'a [ToolAuthorizationRequest],
        scope: &'a TurnScope,
    ) -> RuntimeFuture<'a, Vec<ToolAuthorizationDecision>>;
}

/// Permission-policy compatibility adapter for the neutral batch coordinator.
pub struct PermissionPolicyAuthorization<'a, P: ?Sized> {
    policy: &'a P,
}

impl<'a, P: ?Sized> PermissionPolicyAuthorization<'a, P> {
    /// Wrap an existing permission policy as a batch authorization coordinator.
    #[must_use]
    pub const fn new(policy: &'a P) -> Self {
        Self { policy }
    }
}

impl<P> ToolAuthorizationCoordinator for PermissionPolicyAuthorization<'_, P>
where
    P: PermissionPolicy + ?Sized,
{
    fn authorize_batch<'a>(
        &'a self,
        requests: &'a [ToolAuthorizationRequest],
        _scope: &'a TurnScope,
    ) -> RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
        Box::pin(authorize_with_permission_policy(self.policy, requests))
    }
}

/// Owned permission-policy adapter for spawned canonical provider/tool loops.
#[derive(Clone)]
pub struct SharedPermissionPolicyAuthorization {
    policy: Arc<dyn PermissionPolicy>,
}

impl std::fmt::Debug for SharedPermissionPolicyAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedPermissionPolicyAuthorization")
            .finish_non_exhaustive()
    }
}

impl SharedPermissionPolicyAuthorization {
    /// Create an owned complete-batch authorization adapter.
    #[must_use]
    pub fn new(policy: Arc<dyn PermissionPolicy>) -> Self {
        Self { policy }
    }
}

impl ToolAuthorizationCoordinator for SharedPermissionPolicyAuthorization {
    fn authorize_batch<'a>(
        &'a self,
        requests: &'a [ToolAuthorizationRequest],
        _scope: &'a TurnScope,
    ) -> RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
        Box::pin(authorize_with_permission_policy(
            self.policy.as_ref(),
            requests,
        ))
    }
}

async fn authorize_with_permission_policy<P>(
    policy: &P,
    requests: &[ToolAuthorizationRequest],
) -> Result<Vec<ToolAuthorizationDecision>>
where
    P: PermissionPolicy + ?Sized,
{
    let mut decisions = Vec::with_capacity(requests.len());
    for request in requests {
        let permission_request = RuntimePermissionRequest {
            context: request.context.clone(),
            call: request.call.clone(),
            tool: request.tool.clone(),
            facts: request.facts.clone(),
        };
        decisions.push(
            match policy.evaluate_tool_call(&permission_request).await? {
                PermissionDecision::Allow => ToolAuthorizationDecision::Allow,
                PermissionDecision::Ask(reason) => ToolAuthorizationDecision::Ask(reason),
                PermissionDecision::Deny(reason) => ToolAuthorizationDecision::Deny(reason),
            },
        );
    }
    Ok(decisions)
}

/// Tool catalog visible to the runtime.
pub trait ToolCatalog: Send + Sync {
    /// Return registered model-callable tools with routing metadata.
    fn tools(&self) -> Vec<RegisteredTool>;

    /// Return model-callable tool definitions.
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools()
            .into_iter()
            .map(|tool| tool.definition)
            .collect()
    }

    /// Return the registered tool with the requested name, when present.
    fn find_tool(&self, name: &str) -> Option<RegisteredTool> {
        self.tools()
            .into_iter()
            .find(|tool| tool.definition.name == name)
    }
}

/// Empty tool catalog for stateless turns without tools.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyToolCatalog;

impl ToolCatalog for EmptyToolCatalog {
    fn tools(&self) -> Vec<RegisteredTool> {
        Vec::new()
    }
}

/// Tool permission decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Permit tool execution.
    Allow,
    /// Ask the host/user whether tool execution should proceed.
    Ask(String),
    /// Deny tool execution with a reason.
    Deny(String),
}

/// Stable context used while evaluating tool permissions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePermissionContext {
    /// Session that owns the tool call.
    pub session_id: SessionId,
    /// Active agent/profile ID.
    pub agent_id: String,
}

impl Default for RuntimePermissionContext {
    fn default() -> Self {
        Self {
            session_id: SessionId::default(),
            agent_id: "build".to_string(),
        }
    }
}

/// Complete permission evaluation request for one resolved tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePermissionRequest {
    /// Stable permission context for this execution path.
    pub context: RuntimePermissionContext,
    /// Requested provider tool call, retained for host correlation only.
    pub call: ToolCall,
    /// Resolved tool registration, retained for host correlation only.
    pub tool: RegisteredTool,
    /// Tool-owner-produced authorization facts consumed by domain policy adapters.
    pub facts: Vec<ToolAuthorizationFact>,
}

/// Tool permission hook used before sensitive execution.
pub trait PermissionPolicy: Send + Sync {
    /// Evaluate one requested tool call.
    fn evaluate_tool_call<'a>(
        &'a self,
        request: &'a RuntimePermissionRequest,
    ) -> RuntimeFuture<'a, PermissionDecision>;
}

/// In-memory source-aware tool catalog.
#[derive(Debug, Clone, Default)]
pub struct UnifiedToolCatalog {
    tools: BTreeMap<String, RegisteredTool>,
}

impl UnifiedToolCatalog {
    /// Create an empty unified tool catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an inline SDK tool definition.
    #[must_use]
    pub fn with_inline_tool(mut self, definition: ToolDefinition) -> Self {
        self.insert(RegisteredTool::inline(definition));
        self
    }

    /// Register a plugin-backed tool definition.
    #[must_use]
    pub fn with_plugin_tool(
        mut self,
        definition: ToolDefinition,
        plugin_id: impl Into<String>,
    ) -> Self {
        self.insert(RegisteredTool::plugin(definition, plugin_id));
        self
    }

    /// Insert a fully specified registered tool.
    pub fn insert(&mut self, tool: RegisteredTool) {
        self.tools.insert(tool.definition.name.clone(), tool);
    }
}

impl ToolCatalog for UnifiedToolCatalog {
    fn tools(&self) -> Vec<RegisteredTool> {
        self.tools.values().cloned().collect()
    }

    fn find_tool(&self, name: &str) -> Option<RegisteredTool> {
        self.tools.get(name).cloned()
    }
}

/// Permission policy that allows every tool call.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllPolicy;

impl PermissionPolicy for AllowAllPolicy {
    fn evaluate_tool_call<'a>(
        &'a self,
        _request: &'a RuntimePermissionRequest,
    ) -> RuntimeFuture<'a, PermissionDecision> {
        Box::pin(async { Ok(PermissionDecision::Allow) })
    }
}

/// Reusable runtime for one or more agent turns.
#[derive(Debug, Clone)]
pub struct AgentRuntime {
    poll_interval: Duration,
    turns: TurnScopeOwner,
}

struct ActiveRuntimeTurn {
    owner: TurnScopeOwner,
    scope: TurnScope,
    terminal: bool,
}

impl ActiveRuntimeTurn {
    fn new(
        owner: TurnScopeOwner,
        turn_id: impl Into<Arc<str>>,
        events: Arc<dyn TurnEventSink>,
        capabilities: InvocationCapabilities,
    ) -> Self {
        let scope = owner.begin_turn(turn_id, events, capabilities);
        Self {
            owner,
            scope,
            terminal: false,
        }
    }

    const fn scope(&self) -> &TurnScope {
        &self.scope
    }

    fn complete(&mut self) -> bool {
        let completed = self.owner.complete_turn(&self.scope);
        self.terminal = completed;
        completed
    }
}

impl Drop for ActiveRuntimeTurn {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        let _ = self.owner.cancel_turn(&self.scope);
        let _ = self.scope.control().mark_cancelled();
        let _ = self.owner.release_terminal_turn(&self.scope);
    }
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(50),
            turns: TurnScopeOwner::new(),
        }
    }
}

impl AgentRuntime {
    /// Create a runtime with default polling behavior.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure provider event poll interval.
    #[must_use]
    pub const fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Allocate and activate the runtime's next monotonic turn scope.
    ///
    /// Any prior runtime-owned scope is synchronously closed before this method returns.
    #[must_use]
    pub fn begin_turn_scope(
        &self,
        turn_id: impl Into<Arc<str>>,
        events: Arc<dyn TurnEventSink>,
        capabilities: InvocationCapabilities,
    ) -> TurnScope {
        self.turns.begin_turn(turn_id, events, capabilities)
    }

    /// Return the runtime's active turn generation, when one has been allocated.
    #[must_use]
    pub fn active_turn_generation(&self) -> Option<TurnGeneration> {
        self.turns.active_generation()
    }

    /// Cancel a runtime-owned turn only if it is still active.
    #[must_use]
    pub fn cancel_turn_scope(&self, scope: &TurnScope) -> bool {
        self.turns.cancel_turn(scope)
    }

    /// Complete and release a runtime-owned turn only if it is still active.
    #[must_use]
    pub fn complete_turn_scope(&self, scope: &TurnScope) -> bool {
        self.turns.complete_turn(scope)
    }

    /// Stream a complete provider/tool conversation through one canonical scoped event surface.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn run_streaming_provider_tool_loop<P>(
        &self,
        mut provider: P,
        request: AgentTurnRequest,
        catalog: Arc<dyn ToolCatalog>,
        authorization: Arc<dyn ToolAuthorizationCoordinator>,
        invoker: Arc<dyn ToolInvoker>,
        context: RuntimePermissionContext,
        host_context: Vec<bcode_tool::ToolHostContextEntry>,
        options: ToolExecutionOptions,
        events: Arc<dyn TurnEventSink>,
        capabilities: InvocationCapabilities,
        observer: Arc<dyn ToolRoundObserver>,
        planner: Arc<dyn ProviderRoundPlanner>,
    ) -> AgentLoopStream
    where
        P: ModelProviderInvoker + 'static,
    {
        let (sender, receiver) = mpsc::unbounded_channel();
        let runtime = self.clone();
        let stream_events: Arc<dyn TurnEventSink> = Arc::new(LoopStreamEventSink {
            configured: events,
            sender: sender.clone(),
        });
        tokio::spawn(async move {
            let catalog = SharedToolCatalog(catalog);
            let result = runtime
                .run_provider_tool_loop(
                    &mut provider,
                    request,
                    &catalog,
                    authorization.as_ref(),
                    invoker.as_ref(),
                    &context,
                    &host_context,
                    options,
                    stream_events,
                    capabilities,
                    observer.as_ref(),
                    planner.as_ref(),
                )
                .await;
            let item = match result {
                Ok(response) => AgentLoopStreamItem::Finished(response),
                Err(error) => AgentLoopStreamItem::Error(error),
            };
            let _ = sender.send(item);
        });
        AgentLoopStream { receiver }
    }

    /// Run a complete provider/tool conversation through one canonical turn scope.
    ///
    /// Provider rounds, complete-batch preparation and authorization, ordered tool results, and
    /// continuation all share the same lifecycle and whole-turn timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when provider execution, tool orchestration, host observation, scope
    /// completion, cancellation, or the whole-turn timeout fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_provider_tool_loop<P, C, A, I, O, R>(
        &self,
        provider: &mut P,
        request: AgentTurnRequest,
        catalog: &C,
        authorization: &A,
        invoker: &I,
        context: &RuntimePermissionContext,
        host_context: &[bcode_tool::ToolHostContextEntry],
        options: ToolExecutionOptions,
        events: Arc<dyn TurnEventSink>,
        capabilities: InvocationCapabilities,
        observer: &O,
        planner: &R,
    ) -> Result<AgentTurnResponse>
    where
        P: ModelProviderInvoker + ?Sized,
        C: ToolCatalog + Sync,
        A: ToolAuthorizationCoordinator + ?Sized,
        I: ToolInvoker + Sync + ?Sized,
        O: ToolRoundObserver + ?Sized,
        R: ProviderRoundPlanner + ?Sized,
    {
        let scope = self.begin_turn_scope(
            format!("agent-turn:{}", context.session_id),
            events,
            capabilities,
        );
        let result = self
            .run_provider_tool_loop_in_scope(
                provider,
                request,
                catalog,
                authorization,
                invoker,
                context,
                host_context,
                options,
                &scope,
                observer,
                planner,
            )
            .await;
        match result {
            Ok(response) if self.complete_turn_scope(&scope) => Ok(response),
            Ok(_) => Err(RuntimeError::Cancelled),
            Err(error) => {
                let _ = self.cancel_turn_scope(&scope);
                Err(error)
            }
        }
    }

    /// Run a complete provider/tool conversation inside an existing canonical scope.
    ///
    /// This lower-level entry point is intended for hosts that own scope allocation. It contains
    /// the same provider-round and tool-continuation loop as [`Self::run_provider_tool_loop`].
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as [`Self::run_provider_tool_loop`].
    #[allow(clippy::too_many_arguments)]
    pub async fn run_provider_tool_loop_in_scope<P, C, A, I, O, R>(
        &self,
        provider: &mut P,
        mut request: AgentTurnRequest,
        catalog: &C,
        authorization: &A,
        invoker: &I,
        context: &RuntimePermissionContext,
        host_context: &[bcode_tool::ToolHostContextEntry],
        options: ToolExecutionOptions,
        scope: &TurnScope,
        observer: &O,
        planner: &R,
    ) -> Result<AgentTurnResponse>
    where
        P: ModelProviderInvoker + ?Sized,
        C: ToolCatalog + Sync,
        A: ToolAuthorizationCoordinator + ?Sized,
        I: ToolInvoker + Sync + ?Sized,
        O: ToolRoundObserver + ?Sized,
        R: ProviderRoundPlanner + ?Sized,
    {
        validate_tool_host_context(host_context)?;
        request.tool_call_policy.parallel &= options.parallel;
        let mut rounds = ToolRoundState::new(request.max_tool_rounds);
        let turn_cancellation = request.cancellation.clone();
        let started = Instant::now();
        let timeout = request.timeout;
        let mut provider_round = 0_u32;
        let mut all_events = Vec::new();
        let mut messages = request.messages.clone();
        if request.append_prompt {
            messages.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: request.prompt.clone(),
                }],
            });
        }

        loop {
            let planned_round = run_planned_provider_round(
                self,
                provider,
                request,
                planner,
                provider_round,
                &turn_cancellation,
                started,
                timeout,
                options.parallel,
                scope,
            )
            .await?;
            let response = planned_round.response;
            request = planned_round.request;
            all_events.extend(response.events.iter().cloned());
            let calls = response
                .events
                .iter()
                .filter_map(|event| match event {
                    AgentRuntimeEvent::ToolCallFinished(call) => Some(call.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();

            if response.stop_reason != Some(StopReason::ToolCall) {
                return Ok(AgentTurnResponse {
                    text: response.text,
                    stop_reason: response.stop_reason,
                    usage: response.usage,
                    latency_ms: started.elapsed().as_millis(),
                    events: all_events,
                });
            }
            if calls.is_empty() {
                return Err(RuntimeError::EmptyProviderToolRound);
            }

            append_provider_tool_calls(&mut messages, &response, &calls);
            observer.before_tool_batch(&calls)?;
            let cancellation = request.cancellation.clone();
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or(RuntimeError::Timeout { timeout })?;
            let batch = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    let _ = self.cancel_turn_scope(scope);
                    return Err(RuntimeError::Cancelled);
                }
                () = tokio::time::sleep(remaining) => {
                    let _ = self.cancel_turn_scope(scope);
                    return Err(RuntimeError::Timeout { timeout });
                }
                batch = self.execute_prepared_tool_batch_with_host_context(
                    catalog,
                    authorization,
                    invoker,
                    &calls,
                    &mut rounds,
                    context,
                    host_context,
                    options,
                    scope,
                ) => batch?,
            };

            append_tool_batch_results(
                &mut messages,
                &mut all_events,
                &calls,
                batch,
                scope,
                observer,
            )?;
            request.messages.clone_from(&messages);
            request.prompt.clear();
            request.append_prompt = false;
            provider_round = provider_round.saturating_add(1);
        }
    }

    /// Execute an ordered tool-call batch with no additional host preparation context.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute_prepared_tool_batch_with_host_context`].
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_prepared_tool_batch<C, A, I>(
        &self,
        catalog: &C,
        authorization: &A,
        invoker: &I,
        calls: &[ToolCall],
        rounds: &mut ToolRoundState,
        context: &RuntimePermissionContext,
        options: ToolExecutionOptions,
        scope: &TurnScope,
    ) -> Result<ToolBatchExecutionOutput>
    where
        C: ToolCatalog + Sync,
        A: ToolAuthorizationCoordinator + ?Sized,
        I: ToolInvoker + Sync + ?Sized,
    {
        self.execute_prepared_tool_batch_with_host_context(
            catalog,
            authorization,
            invoker,
            calls,
            rounds,
            context,
            &[],
            options,
            scope,
        )
        .await
    }

    /// Execute an ordered tool-call batch through neutral preparation and authorization contracts.
    ///
    /// The complete batch is prepared and authorized before invocation begins. When parallel mode
    /// is enabled, approved calls from this provider batch overlap without a default limit, or up
    /// to an explicitly configured `max_concurrency`; dependencies belong in later provider rounds. Results retain provider order regardless of
    /// completion order. `host_context` remains opaque to the runtime and is forwarded unchanged
    /// to every preparation request.
    ///
    /// # Errors
    ///
    /// Returns an error when the tool-round budget is exhausted, host context is invalid or
    /// oversized, an authorization adapter returns an invalid response, or authorization cannot
    /// complete. Per-call resolution, preparation, denial, cancellation, and invocation failures
    /// are returned in the ordered batch output.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_prepared_tool_batch_with_host_context<C, A, I>(
        &self,
        catalog: &C,
        authorization: &A,
        invoker: &I,
        calls: &[ToolCall],
        rounds: &mut ToolRoundState,
        context: &RuntimePermissionContext,
        host_context: &[bcode_tool::ToolHostContextEntry],
        options: ToolExecutionOptions,
        scope: &TurnScope,
    ) -> Result<ToolBatchExecutionOutput>
    where
        C: ToolCatalog + Sync,
        A: ToolAuthorizationCoordinator + ?Sized,
        I: ToolInvoker + Sync + ?Sized,
    {
        validate_tool_host_context(host_context)?;
        if calls.is_empty() {
            return Ok(empty_batch_output());
        }
        rounds.begin_round()?;
        if !scope.control().accepts_normal_output() {
            return Ok(cancelled_batch_output(calls.len()));
        }

        let mut terminal = BTreeMap::<usize, Result<ToolExecutionOutput>>::new();
        let prepared = prepare_runtime_tool_batch(
            catalog,
            invoker,
            calls,
            host_context,
            Duration::from_millis(options.preparation_timeout_ms.get()),
            scope,
            &mut terminal,
        )
        .await;

        if !scope.control().accepts_normal_output() {
            insert_cancelled_calls(&mut terminal, &prepared);
            return Ok(ordered_batch_output(calls.len(), terminal));
        }

        let approved =
            authorize_runtime_tool_batch(authorization, prepared, context, scope, &mut terminal)
                .await?;

        if !scope.control().accepts_normal_output() {
            insert_cancelled_calls(&mut terminal, &approved);
            return Ok(ordered_batch_output(calls.len(), terminal));
        }

        for group in provider_batch_execution_groups(approved, options.parallel) {
            if !scope.control().accepts_normal_output() {
                for call in group {
                    terminal.insert(call.index, Err(RuntimeError::Cancelled));
                }
                continue;
            }
            let concurrency = batch_concurrency(options, group.len());
            let group_indices = group.iter().map(|call| call.index);
            let executions = stream::iter(group.iter().cloned().map(|prepared| async move {
                let index = prepared.index;
                let result = invoke_prepared_tool(invoker, prepared, scope).await;
                (index, result)
            }))
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>();
            let cancellation = scope.control().cancellation();
            let completions = tokio::select! {
                biased;
                () = cancellation.cancelled() => group_indices
                    .map(|index| (index, Err(RuntimeError::Cancelled)))
                    .collect(),
                completions = executions => completions,
            };
            terminal.extend(completions);
        }

        Ok(ordered_batch_output(calls.len(), terminal))
    }

    /// Run a stateless text-generation turn through a provider invoker.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the provider reports an error, the turn is
    /// cancelled, or the timeout expires.
    pub async fn run_text_turn<P>(
        &self,
        provider: &mut P,
        request: AgentTurnRequest,
    ) -> Result<AgentTurnResponse>
    where
        P: ModelProviderInvoker,
    {
        self.run_text_turn_internal(provider, request, None).await
    }

    /// Run a stateless text-generation turn and stream normalized events as they arrive.
    #[must_use]
    pub fn run_streaming_text_turn<P>(
        &self,
        provider: P,
        request: AgentTurnRequest,
    ) -> AgentRuntimeStream
    where
        P: ModelProviderInvoker + 'static,
    {
        let (sender, receiver) = mpsc::unbounded_channel();
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut provider = provider;
            let result = runtime
                .run_text_turn_internal(&mut provider, request, Some(&sender))
                .await;
            let item = match result {
                Ok(response) => AgentRuntimeStreamItem::Finished(response),
                Err(error) => AgentRuntimeStreamItem::Error(error),
            };
            let _ = sender.send(item);
        });
        AgentRuntimeStream { receiver }
    }

    /// Run one provider turn inside an existing canonical turn scope.
    ///
    /// The caller owns scope completion and may continue the same scope through tool execution and
    /// subsequent provider rounds.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the provider reports an error, the turn is
    /// cancelled, the scope closes, or the timeout expires.
    pub async fn run_text_turn_in_scope<P>(
        &self,
        provider: &mut P,
        request: &AgentTurnRequest,
        scope: &TurnScope,
    ) -> Result<AgentTurnResponse>
    where
        P: ModelProviderInvoker + ?Sized,
    {
        let start = Instant::now();
        let model_request = model_turn_request(request);
        let provider_plugin_id = request.provider_plugin_id.as_deref();
        let start_response =
            start_provider_turn(provider, provider_plugin_id, &model_request, request, scope)
                .await?;
        let poll_request = PollTurnEventsRequest {
            provider_turn_id: start_response.provider_turn_id.clone(),
        };
        let finish_request = FinishTurnRequest {
            provider_turn_id: start_response.provider_turn_id.clone(),
        };
        let cancel_request = CancelTurnRequest {
            provider_turn_id: start_response.provider_turn_id,
        };
        let mut events = Vec::new();
        let mut text = String::new();
        let mut usage = None;

        emit_turn_started(
            provider,
            provider_plugin_id,
            &cancel_request,
            &finish_request,
            scope,
            &mut events,
        )
        .await?;

        loop {
            ensure_scope_active(
                provider,
                provider_plugin_id,
                &cancel_request,
                &finish_request,
                scope,
            )
            .await?;
            let poll = poll_provider_events(
                provider,
                &ProviderPollContext {
                    provider_plugin_id,
                    poll_request: &poll_request,
                    cancel_request: &cancel_request,
                    finish_request: &finish_request,
                    request,
                    scope,
                    start,
                },
            )
            .await?;
            let should_sleep = poll.events.is_empty();
            for event in poll.events {
                let disposition = normalize_provider_event_or_cleanup(
                    provider,
                    provider_plugin_id,
                    &cancel_request,
                    &finish_request,
                    event,
                    &mut text,
                    &mut usage,
                )
                .await?;
                if let Some(response) = apply_provider_event_disposition(
                    provider,
                    &ProviderEventContext {
                        provider_plugin_id,
                        cancel_request: &cancel_request,
                        finish_request: &finish_request,
                        scope,
                        start,
                    },
                    disposition,
                    &mut text,
                    &mut usage,
                    &mut events,
                )
                .await?
                {
                    return Ok(response);
                }
            }
            sleep_after_empty_poll(should_sleep, self.poll_interval).await;
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn run_text_turn_internal<P>(
        &self,
        provider: &mut P,
        request: AgentTurnRequest,
        stream: Option<&mpsc::UnboundedSender<AgentRuntimeStreamItem>>,
    ) -> Result<AgentTurnResponse>
    where
        P: ModelProviderInvoker,
    {
        let mut active_turn = ActiveRuntimeTurn::new(
            self.turns.clone(),
            "text-turn",
            Arc::new(RuntimeStreamEventSink {
                sender: stream.cloned(),
            }),
            InvocationCapabilities::default(),
        );
        let response = self
            .run_text_turn_in_scope(provider, &request, active_turn.scope())
            .await?;
        if !active_turn.complete() {
            return Err(RuntimeError::Cancelled);
        }
        Ok(response)
    }
}

enum EventDisposition {
    Continue(AgentRuntimeEvent),
    Finished { stop_reason: StopReason },
    Cancelled(AgentRuntimeEvent),
}

async fn sleep_after_empty_poll(should_sleep: bool, poll_interval: Duration) {
    if should_sleep {
        tokio::time::sleep(poll_interval).await;
    }
}

async fn ensure_scope_active<P>(
    provider: &mut P,
    provider_plugin_id: Option<&str>,
    cancel_request: &CancelTurnRequest,
    finish_request: &FinishTurnRequest,
    scope: &TurnScope,
) -> Result<()>
where
    P: ModelProviderInvoker + ?Sized,
{
    if scope.accepts_work() {
        return Ok(());
    }
    cancel_and_finish(provider, provider_plugin_id, cancel_request, finish_request).await;
    Err(RuntimeError::Cancelled)
}

async fn emit_turn_started<P>(
    provider: &mut P,
    provider_plugin_id: Option<&str>,
    cancel_request: &CancelTurnRequest,
    finish_request: &FinishTurnRequest,
    scope: &TurnScope,
    events: &mut Vec<AgentRuntimeEvent>,
) -> Result<()>
where
    P: ModelProviderInvoker + ?Sized,
{
    let event = AgentRuntimeEvent::TurnStarted;
    if scope.emit(ScopedTurnEvent::Runtime(event.clone())) {
        events.push(event);
        return Ok(());
    }
    cancel_and_finish(provider, provider_plugin_id, cancel_request, finish_request).await;
    Err(RuntimeError::Cancelled)
}

struct ProviderEventContext<'a> {
    provider_plugin_id: Option<&'a str>,
    cancel_request: &'a CancelTurnRequest,
    finish_request: &'a FinishTurnRequest,
    scope: &'a TurnScope,
    start: Instant,
}

async fn apply_provider_event_disposition<P>(
    provider: &mut P,
    context: &ProviderEventContext<'_>,
    disposition: EventDisposition,
    text: &mut String,
    usage: &mut Option<TokenUsage>,
    events: &mut Vec<AgentRuntimeEvent>,
) -> Result<Option<AgentTurnResponse>>
where
    P: ModelProviderInvoker + ?Sized,
{
    match disposition {
        EventDisposition::Continue(event) => {
            if !context.scope.emit(ScopedTurnEvent::Runtime(event.clone())) {
                cancel_and_finish(
                    provider,
                    context.provider_plugin_id,
                    context.cancel_request,
                    context.finish_request,
                )
                .await;
                return Err(RuntimeError::Cancelled);
            }
            events.push(event);
            Ok(None)
        }
        EventDisposition::Finished { stop_reason } => {
            provider
                .finish_turn(context.provider_plugin_id, context.finish_request)
                .await?;
            let finished_event =
                finished_event(usage.as_ref(), context.start.elapsed(), stop_reason);
            if !context
                .scope
                .emit(ScopedTurnEvent::Runtime(finished_event.clone()))
            {
                return Err(RuntimeError::Cancelled);
            }
            events.push(finished_event);
            Ok(Some(AgentTurnResponse {
                text: std::mem::take(text),
                stop_reason: Some(stop_reason),
                usage: usage.take(),
                latency_ms: context.start.elapsed().as_millis(),
                events: std::mem::take(events),
            }))
        }
        EventDisposition::Cancelled(event) => {
            if !context.scope.emit(ScopedTurnEvent::Runtime(event.clone())) {
                return Err(RuntimeError::Cancelled);
            }
            events.push(event);
            provider
                .finish_turn(context.provider_plugin_id, context.finish_request)
                .await?;
            Err(RuntimeError::Cancelled)
        }
    }
}

async fn normalize_provider_event_or_cleanup<P>(
    provider: &mut P,
    provider_plugin_id: Option<&str>,
    cancel_request: &CancelTurnRequest,
    finish_request: &FinishTurnRequest,
    event: ProviderTurnEvent,
    text: &mut String,
    usage: &mut Option<TokenUsage>,
) -> Result<EventDisposition>
where
    P: ModelProviderInvoker + ?Sized,
{
    match normalize_provider_event(event, text, usage) {
        Ok(disposition) => Ok(disposition),
        Err(error) => {
            cancel_and_finish(provider, provider_plugin_id, cancel_request, finish_request).await;
            Err(error)
        }
    }
}

async fn start_provider_turn<P>(
    provider: &mut P,
    provider_plugin_id: Option<&str>,
    model_request: &ModelTurnRequest,
    request: &AgentTurnRequest,
    scope: &TurnScope,
) -> Result<StartTurnResponse>
where
    P: ModelProviderInvoker + ?Sized,
{
    let scope_cancellation = scope.control().cancellation();
    tokio::select! {
        biased;
        () = request.cancellation.cancelled() => Err(RuntimeError::Cancelled),
        () = scope_cancellation.cancelled() => Err(RuntimeError::Cancelled),
        () = tokio::time::sleep(request.timeout) => {
            Err(RuntimeError::Timeout { timeout: request.timeout })
        }
        response = provider.start_turn(provider_plugin_id, model_request) => response,
    }
}

struct ProviderPollContext<'a> {
    provider_plugin_id: Option<&'a str>,
    poll_request: &'a PollTurnEventsRequest,
    cancel_request: &'a CancelTurnRequest,
    finish_request: &'a FinishTurnRequest,
    request: &'a AgentTurnRequest,
    scope: &'a TurnScope,
    start: Instant,
}

async fn poll_provider_events<P>(
    provider: &mut P,
    context: &ProviderPollContext<'_>,
) -> Result<PollTurnEventsResponse>
where
    P: ModelProviderInvoker + ?Sized,
{
    let remaining = context
        .request
        .timeout
        .checked_sub(context.start.elapsed())
        .ok_or(RuntimeError::Timeout {
            timeout: context.request.timeout,
        })?;
    let request_cancellation = context.request.cancellation.clone();
    let scope_cancellation = context.scope.control().cancellation();
    tokio::select! {
        biased;
        () = request_cancellation.cancelled() => {
            cancel_and_finish(
                provider,
                context.provider_plugin_id,
                context.cancel_request,
                context.finish_request,
            ).await;
            Err(RuntimeError::Cancelled)
        }
        () = scope_cancellation.cancelled() => {
            cancel_and_finish(
                provider,
                context.provider_plugin_id,
                context.cancel_request,
                context.finish_request,
            ).await;
            Err(RuntimeError::Cancelled)
        }
        () = tokio::time::sleep(remaining) => {
            cancel_and_finish(
                provider,
                context.provider_plugin_id,
                context.cancel_request,
                context.finish_request,
            ).await;
            Err(RuntimeError::Timeout { timeout: context.request.timeout })
        }
        poll = provider.poll_turn_events(context.provider_plugin_id, context.poll_request) => {
            match poll {
                Ok(response) => Ok(response),
                Err(error) => {
                    cancel_and_finish(
                        provider,
                        context.provider_plugin_id,
                        context.cancel_request,
                        context.finish_request,
                    ).await;
                    Err(error)
                }
            }
        },
    }
}

async fn cancel_and_finish<P>(
    provider: &mut P,
    provider_plugin_id: Option<&str>,
    cancel_request: &CancelTurnRequest,
    finish_request: &FinishTurnRequest,
) where
    P: ModelProviderInvoker + ?Sized,
{
    let _ = provider
        .cancel_turn(provider_plugin_id, cancel_request)
        .await;
    let _ = provider
        .finish_turn(provider_plugin_id, finish_request)
        .await;
}

fn finished_event(
    usage: Option<&TokenUsage>,
    latency: Duration,
    stop_reason: StopReason,
) -> AgentRuntimeEvent {
    AgentRuntimeEvent::Finished {
        stop_reason,
        usage: usage.cloned(),
        latency_ms: latency.as_millis(),
    }
}

fn insert_cancelled_calls(
    terminal: &mut BTreeMap<usize, Result<ToolExecutionOutput>>,
    calls: &[PreparedRuntimeToolCall],
) {
    terminal.extend(
        calls
            .iter()
            .map(|call| (call.index, Err(RuntimeError::Cancelled))),
    );
}

async fn authorize_runtime_tool_batch<A>(
    authorization: &A,
    prepared: Vec<PreparedRuntimeToolCall>,
    context: &RuntimePermissionContext,
    scope: &TurnScope,
    terminal: &mut BTreeMap<usize, Result<ToolExecutionOutput>>,
) -> Result<Vec<PreparedRuntimeToolCall>>
where
    A: ToolAuthorizationCoordinator + ?Sized,
{
    let requests = prepared
        .iter()
        .map(|prepared| ToolAuthorizationRequest {
            index: prepared.index,
            call: prepared.call.clone(),
            tool: prepared.tool.clone(),
            facts: prepared.invocation.preparation.authorization.clone(),
            context: context.clone(),
        })
        .collect::<Vec<_>>();
    let authorization_future = authorization.authorize_batch(&requests, scope);
    let cancellation = scope.control().cancellation();
    let decisions = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            insert_cancelled_calls(terminal, &prepared);
            return Ok(Vec::new());
        }
        decisions = authorization_future => decisions?,
    };
    if decisions.len() != prepared.len() {
        return Err(RuntimeError::InvalidBatchResponse {
            component: "authorization",
            expected: prepared.len(),
            actual: decisions.len(),
        });
    }

    let mut approved = Vec::with_capacity(prepared.len());
    for (prepared, decision) in prepared.into_iter().zip(decisions) {
        match decision {
            ToolAuthorizationDecision::Allow => approved.push(prepared),
            ToolAuthorizationDecision::Ask(reason) => {
                terminal.insert(
                    prepared.index,
                    Err(RuntimeError::PermissionRequired(reason)),
                );
            }
            ToolAuthorizationDecision::Deny(reason) => {
                terminal.insert(prepared.index, Err(RuntimeError::PermissionDenied(reason)));
            }
        }
    }
    Ok(approved)
}

async fn prepare_runtime_tool_batch<C, I>(
    catalog: &C,
    invoker: &I,
    calls: &[ToolCall],
    host_context: &[bcode_tool::ToolHostContextEntry],
    preparation_timeout: Duration,
    scope: &TurnScope,
    terminal: &mut BTreeMap<usize, Result<ToolExecutionOutput>>,
) -> Vec<PreparedRuntimeToolCall>
where
    C: ToolCatalog + ?Sized,
    I: ToolInvoker + ?Sized,
{
    let mut prepared = Vec::with_capacity(calls.len());
    for (index, call) in calls.iter().enumerate() {
        if !scope.control().accepts_normal_output() {
            terminal.insert(index, Err(RuntimeError::Cancelled));
            continue;
        }
        let Some(tool) = catalog.find_tool(&call.name) else {
            terminal.insert(index, Err(RuntimeError::ToolNotFound(call.name.clone())));
            continue;
        };
        let invocation = tool_invocation_descriptor(call);
        let preparation_request = ToolPreparationRequest {
            invocation: invocation.clone(),
            host_context: host_context.to_vec(),
        };
        let preparation_scope = PreparationScope::new(scope.clone(), host_context.to_vec());
        let preparation = invoker.prepare_tool(&tool, &preparation_request, &preparation_scope);
        let cancellation = scope.control().cancellation();
        let prepared_result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(RuntimeError::Cancelled),
            result = tokio::time::timeout(preparation_timeout, preparation) => result.unwrap_or_else(
                |_| Err(RuntimeError::ToolPreparationTimeout {
                    tool_name: call.name.clone(),
                    timeout: preparation_timeout,
                }),
            ),
        };
        match prepared_result {
            Ok(preparation) => prepared.push(PreparedRuntimeToolCall {
                index,
                call: call.clone(),
                tool,
                invocation: PreparedToolInvocation {
                    invocation,
                    preparation,
                },
            }),
            Err(error) => {
                terminal.insert(index, Err(error));
            }
        }
    }
    prepared
}

struct PlannedProviderRound {
    request: AgentTurnRequest,
    response: AgentTurnResponse,
}

#[allow(clippy::too_many_arguments)]
async fn run_planned_provider_round<P, R>(
    runtime: &AgentRuntime,
    provider: &mut P,
    mut proposed_request: AgentTurnRequest,
    planner: &R,
    round: u32,
    turn_cancellation: &CancellationToken,
    started: Instant,
    timeout: Duration,
    parallel_tool_calls: bool,
    scope: &TurnScope,
) -> Result<PlannedProviderRound>
where
    P: ModelProviderInvoker + ?Sized,
    R: ProviderRoundPlanner + ?Sized,
{
    let mut attempt = 0_u32;
    let mut previous_failure = None;
    loop {
        proposed_request.timeout = remaining_turn_duration(started, timeout)?;
        proposed_request.cancellation = turn_cancellation.clone();
        let plan = plan_provider_round(
            planner,
            ProviderRoundPlanContext {
                round,
                attempt,
                proposed_request: &proposed_request,
                previous_failure: previous_failure.as_ref(),
                scope,
            },
            turn_cancellation,
            started,
            timeout,
            scope,
        )
        .await?;
        let (request, delay) = match plan {
            ProviderRoundPlan::Proceed { request } => (request, None),
            ProviderRoundPlan::RetryAfter { request, delay } => (request, Some(delay)),
            ProviderRoundPlan::Fail { error } => {
                return Err(error.or(previous_failure).unwrap_or_else(|| {
                    RuntimeError::HostExtension(
                        "provider planner returned fail before a provider attempt without an error"
                            .to_string(),
                    )
                }));
            }
        };
        if let Some(delay) = delay {
            wait_for_provider_retry_delay(delay, turn_cancellation, started, timeout, scope)
                .await?;
        }
        proposed_request = request;
        proposed_request.tool_call_policy.parallel &= parallel_tool_calls;
        proposed_request.timeout = remaining_turn_duration(started, timeout)?;
        proposed_request.cancellation = turn_cancellation.clone();
        match runtime
            .run_text_turn_in_scope(provider, &proposed_request, scope)
            .await
        {
            Ok(response) => {
                return Ok(PlannedProviderRound {
                    request: proposed_request,
                    response,
                });
            }
            Err(RuntimeError::Cancelled) => return Err(RuntimeError::Cancelled),
            Err(RuntimeError::Timeout { .. }) => {
                return Err(RuntimeError::Timeout { timeout });
            }
            Err(error) => {
                previous_failure = Some(error);
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

async fn plan_provider_round<R>(
    planner: &R,
    context: ProviderRoundPlanContext<'_>,
    turn_cancellation: &CancellationToken,
    started: Instant,
    timeout: Duration,
    scope: &TurnScope,
) -> Result<ProviderRoundPlan>
where
    R: ProviderRoundPlanner + ?Sized,
{
    let remaining = remaining_turn_duration(started, timeout)?;
    let scope_cancellation = scope.control().cancellation();
    tokio::select! {
        biased;
        () = turn_cancellation.cancelled() => Err(RuntimeError::Cancelled),
        () = scope_cancellation.cancelled() => Err(RuntimeError::Cancelled),
        () = tokio::time::sleep(remaining) => Err(RuntimeError::Timeout { timeout }),
        plan = planner.plan_round(context) => plan,
    }
}

async fn wait_for_provider_retry_delay(
    delay: Duration,
    turn_cancellation: &CancellationToken,
    started: Instant,
    timeout: Duration,
    scope: &TurnScope,
) -> Result<()> {
    let remaining = remaining_turn_duration(started, timeout)?;
    let scope_cancellation = scope.control().cancellation();
    tokio::select! {
        biased;
        () = turn_cancellation.cancelled() => Err(RuntimeError::Cancelled),
        () = scope_cancellation.cancelled() => Err(RuntimeError::Cancelled),
        () = tokio::time::sleep(remaining) => Err(RuntimeError::Timeout { timeout }),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

fn remaining_turn_duration(started: Instant, timeout: Duration) -> Result<Duration> {
    timeout
        .checked_sub(started.elapsed())
        .ok_or(RuntimeError::Timeout { timeout })
}

fn append_provider_tool_calls(
    messages: &mut Vec<ModelMessage>,
    response: &AgentTurnResponse,
    calls: &[ToolCall],
) {
    let mut content = Vec::with_capacity(calls.len() + usize::from(!response.text.is_empty()));
    if !response.text.is_empty() {
        content.push(ContentBlock::Text {
            text: response.text.clone(),
        });
    }
    content.extend(
        calls
            .iter()
            .cloned()
            .map(|call| ContentBlock::ToolCall { call }),
    );
    messages.push(ModelMessage {
        role: MessageRole::Assistant,
        content,
    });
}

fn append_tool_batch_results<O>(
    messages: &mut Vec<ModelMessage>,
    all_events: &mut Vec<AgentRuntimeEvent>,
    calls: &[ToolCall],
    batch: ToolBatchExecutionOutput,
    scope: &TurnScope,
    observer: &O,
) -> Result<()>
where
    O: ToolRoundObserver + ?Sized,
{
    for (call, result) in calls.iter().zip(batch.results) {
        let model_result = match result {
            Ok(output) => {
                observer.after_tool_call(call, &output)?;
                if let Some(event) = output
                    .events
                    .iter()
                    .find(|event| matches!(event, AgentRuntimeEvent::ToolResult(_)))
                    .cloned()
                {
                    all_events.push(event);
                }
                output.model_result
            }
            Err(error) => {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: error.to_string(),
                    is_error: true,
                    content: Vec::new(),
                };
                let event = AgentRuntimeEvent::ToolResult(result.clone());
                if !scope.emit(ScopedTurnEvent::Runtime(event.clone())) {
                    return Err(RuntimeError::Cancelled);
                }
                all_events.push(event);
                result
            }
        };
        messages.push(ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: model_result,
            }],
        });
    }
    Ok(())
}

const TOOL_HOST_CONTEXT_MAX_ENTRIES: usize = 32;
const TOOL_HOST_CONTEXT_SCHEMA_MAX_BYTES: usize = 128;
const TOOL_HOST_CONTEXT_PAYLOAD_MAX_BYTES: usize = 64 * 1024;
const TOOL_HOST_CONTEXT_TOTAL_MAX_BYTES: usize = 256 * 1024;

fn validate_tool_host_context(host_context: &[bcode_tool::ToolHostContextEntry]) -> Result<()> {
    if host_context.len() > TOOL_HOST_CONTEXT_MAX_ENTRIES {
        return Err(RuntimeError::InvalidToolHostContext(format!(
            "received {} entries; maximum is {TOOL_HOST_CONTEXT_MAX_ENTRIES}",
            host_context.len()
        )));
    }

    let mut identities = BTreeSet::new();
    for entry in host_context {
        if entry.schema.is_empty() {
            return Err(RuntimeError::InvalidToolHostContext(
                "schema identifier must not be empty".to_string(),
            ));
        }
        if entry.schema.len() > TOOL_HOST_CONTEXT_SCHEMA_MAX_BYTES {
            return Err(RuntimeError::InvalidToolHostContext(format!(
                "schema identifier is {} bytes; maximum is {TOOL_HOST_CONTEXT_SCHEMA_MAX_BYTES}",
                entry.schema.len()
            )));
        }
        if entry.schema_version == 0 {
            return Err(RuntimeError::InvalidToolHostContext(format!(
                "schema {} has version zero",
                entry.schema
            )));
        }
        if !identities.insert((entry.schema.as_str(), entry.schema_version)) {
            return Err(RuntimeError::InvalidToolHostContext(format!(
                "duplicate schema {} version {}",
                entry.schema, entry.schema_version
            )));
        }
        let payload_bytes = serde_json::to_vec(&entry.payload)
            .expect("serializing a serde_json::Value cannot fail")
            .len();
        if payload_bytes > TOOL_HOST_CONTEXT_PAYLOAD_MAX_BYTES {
            return Err(RuntimeError::InvalidToolHostContext(format!(
                "schema {} payload is {payload_bytes} bytes; maximum is {TOOL_HOST_CONTEXT_PAYLOAD_MAX_BYTES}",
                entry.schema
            )));
        }
    }

    let total_bytes = serde_json::to_vec(host_context)
        .expect("serializing tool host context cannot fail")
        .len();
    if total_bytes > TOOL_HOST_CONTEXT_TOTAL_MAX_BYTES {
        return Err(RuntimeError::InvalidToolHostContext(format!(
            "serialized context is {total_bytes} bytes; maximum is {TOOL_HOST_CONTEXT_TOTAL_MAX_BYTES}"
        )));
    }
    Ok(())
}

fn tool_invocation_descriptor(call: &ToolCall) -> ToolInvocationDescriptor {
    ToolInvocationDescriptor {
        invocation_id: call.id.clone(),
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
    }
}

fn batch_concurrency(options: ToolExecutionOptions, batch_len: usize) -> usize {
    options
        .max_concurrency
        .map_or_else(|| batch_len.max(1), NonZeroUsize::get)
}

fn provider_batch_execution_groups(
    prepared: Vec<PreparedRuntimeToolCall>,
    parallel: bool,
) -> Vec<Vec<PreparedRuntimeToolCall>> {
    if parallel && !prepared.is_empty() {
        vec![prepared]
    } else {
        prepared.into_iter().map(|call| vec![call]).collect()
    }
}

async fn invoke_prepared_tool<I>(
    invoker: &I,
    prepared: PreparedRuntimeToolCall,
    scope: &TurnScope,
) -> Result<ToolExecutionOutput>
where
    I: ToolInvoker + ?Sized,
{
    if !scope.control().accepts_normal_output() {
        return Err(RuntimeError::Cancelled);
    }
    let invocation_scope = InvocationScope::new(scope.clone(), prepared.call.id.clone());
    if let Some(handle) = invoker.cancellation_handle(&prepared.tool, &prepared.invocation)
        && !invocation_scope.register_cancellation(handle)
    {
        return Err(RuntimeError::Cancelled);
    }
    if !invocation_scope.accepts_work() {
        let _ = invocation_scope.unregister_cancellation();
        return Err(RuntimeError::Cancelled);
    }
    let invocation = invoker
        .invoke_tool(&prepared.tool, &prepared.invocation, &invocation_scope)
        .await
        .map_err(|error| RuntimeError::ToolExecution {
            tool_name: prepared.call.name.clone(),
            message: error.to_string(),
        });
    let _ = invocation_scope.unregister_cancellation();
    let invocation = invocation?;
    if !scope.control().accepts_normal_output() {
        return Err(RuntimeError::Cancelled);
    }
    let output = tool_execution_output(&prepared.call, invocation);
    for event in &output.events {
        if matches!(event, AgentRuntimeEvent::ToolCallFinished(_)) {
            continue;
        }
        if !scope.emit(ScopedTurnEvent::Runtime(event.clone())) {
            return Err(RuntimeError::Cancelled);
        }
    }
    Ok(output)
}

fn tool_execution_output(
    call: &ToolCall,
    invocation: ToolInvocationResponse,
) -> ToolExecutionOutput {
    let model_result = ToolResult {
        call_id: call.id.clone(),
        output: invocation.output.clone(),
        is_error: invocation.is_error,
        content: invocation
            .content
            .iter()
            .cloned()
            .map(model_tool_result_content)
            .collect(),
    };
    ToolExecutionOutput {
        model_result: model_result.clone(),
        invocation,
        events: vec![
            AgentRuntimeEvent::ToolCallFinished(call.clone()),
            AgentRuntimeEvent::ToolResult(model_result),
        ],
    }
}

fn ordered_batch_output(
    len: usize,
    mut results: BTreeMap<usize, Result<ToolExecutionOutput>>,
) -> ToolBatchExecutionOutput {
    ToolBatchExecutionOutput {
        results: (0..len)
            .map(|index| {
                results
                    .remove(&index)
                    .unwrap_or(Err(RuntimeError::Cancelled))
            })
            .collect(),
    }
}

const fn empty_batch_output() -> ToolBatchExecutionOutput {
    ToolBatchExecutionOutput {
        results: Vec::new(),
    }
}

fn cancelled_batch_output(len: usize) -> ToolBatchExecutionOutput {
    ToolBatchExecutionOutput {
        results: (0..len).map(|_| Err(RuntimeError::Cancelled)).collect(),
    }
}

fn model_tool_result_content(
    content: InvocationToolResultContent,
) -> bcode_model::ToolResultContent {
    match content {
        InvocationToolResultContent::Text { text } => bcode_model::ToolResultContent::Text { text },
        InvocationToolResultContent::Image { image } => bcode_model::ToolResultContent::Image {
            image: bcode_model::ImageContent {
                mime_type: image.mime_type,
                data_base64: image.data_base64,
                metadata: model_image_metadata(image.metadata),
            },
        },
        InvocationToolResultContent::ImageRef { image } => {
            bcode_model::ToolResultContent::ImageRef {
                image: bcode_model::ImageRefContent {
                    path: image.path,
                    mime_type: image.mime_type,
                    metadata: model_image_metadata(image.metadata),
                },
            }
        }
    }
}

fn model_image_metadata(metadata: bcode_tool::ImageMetadata) -> bcode_model::ImageMetadata {
    bcode_model::ImageMetadata {
        width: metadata.width,
        height: metadata.height,
        byte_len: metadata.byte_len,
        source_path: metadata.source_path,
    }
}

fn model_turn_request(request: &AgentTurnRequest) -> ModelTurnRequest {
    let session_id = SessionId::new();
    let mut messages = request.messages.clone();
    if request.append_prompt {
        messages.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: request.prompt.clone(),
            }],
        });
    }
    ModelTurnRequest {
        session_id,
        turn_id: format!("sdk-turn-{session_id}"),
        model_id: request.model_id.clone(),
        provider_context: request.provider_context.clone(),
        system_prompt: request.system_prompt.clone(),
        messages,
        tools: request
            .tools
            .iter()
            .cloned()
            .map(model_tool_definition)
            .collect(),
        tool_call_policy: request.tool_call_policy,
        parameters: request.parameters.clone(),
        structured_output: request.structured_output.clone(),
        context_management: bcode_model::ContextManagementRequest::default(),
        prompt_cache: bcode_model::PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: request.metadata.clone(),
    }
}

fn model_tool_definition(definition: ToolDefinition) -> bcode_model::ToolDefinition {
    bcode_model::ToolDefinition {
        name: definition.name,
        description: definition.description,
        input_schema: definition.input_schema,
    }
}

fn normalize_provider_event(
    event: ProviderTurnEvent,
    text_buffer: &mut String,
    usage_buffer: &mut Option<TokenUsage>,
) -> Result<EventDisposition> {
    match event {
        ProviderTurnEvent::TurnStarted => {
            Ok(EventDisposition::Continue(AgentRuntimeEvent::TurnStarted))
        }
        ProviderTurnEvent::TextDelta { text } => {
            text_buffer.push_str(&text);
            Ok(EventDisposition::Continue(AgentRuntimeEvent::TextDelta(
                text,
            )))
        }
        ProviderTurnEvent::ReasoningDelta { text } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::ReasoningDelta(text),
        )),
        ProviderTurnEvent::ToolCallStarted { call_id, name } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::ToolCallStarted { call_id, name },
        )),
        ProviderTurnEvent::ToolCallDelta { call_id, delta } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::ToolCallDelta { call_id, delta },
        )),
        ProviderTurnEvent::ToolCallFinished { call } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::ToolCallFinished(call),
        )),
        ProviderTurnEvent::Usage { usage } => {
            *usage_buffer = Some(usage.clone());
            Ok(EventDisposition::Continue(AgentRuntimeEvent::Usage(usage)))
        }
        ProviderTurnEvent::ExactRequestInputTokens { tokens } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::ExactRequestInputTokens(tokens),
        )),
        ProviderTurnEvent::RequestProjection { projection } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::RequestProjection(projection),
        )),
        ProviderTurnEvent::ContextCompacted { .. } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::ContextCompacted,
        )),
        ProviderTurnEvent::ProviderMetadata { key, value } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::ProviderMetadata { key, value },
        )),
        ProviderTurnEvent::RetryScheduled {
            message,
            retry_at_unix,
        } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::RetryScheduled {
                message,
                retry_at_unix,
            },
        )),
        ProviderTurnEvent::Warning { message } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::Warning(message),
        )),
        ProviderTurnEvent::Error { error } => Err(RuntimeError::Provider {
            code: error.code.clone(),
            message: error.message.clone(),
            error: Box::new(error),
        }),
        ProviderTurnEvent::TurnFinished { stop_reason } => {
            Ok(EventDisposition::Finished { stop_reason })
        }
        ProviderTurnEvent::Cancelled => {
            Ok(EventDisposition::Cancelled(AgentRuntimeEvent::Cancelled))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{ProviderTurnEvent, StopReason};
    use bcode_tool::{
        ToolArtifactWriteRequest, ToolArtifactWriteResolution, ToolContributionEvent,
        ToolContributionOperation, ToolContributionPersistence, ToolExchangeRequest,
        ToolExchangeResolution, ToolExchangeResponsePolicy, ToolInvocationInput,
        ToolInvocationInputResolution, ToolInvocationLifecycleEvent, ToolInvocationLifecycleStage,
        ToolInvocationServiceRequest, ToolInvocationServiceResolution, ToolPolicyMetadata,
        ToolSideEffect, ToolUiMetadata,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;

    struct FakeProvider {
        events: VecDeque<ProviderTurnEvent>,
        finished: bool,
        cancelled: bool,
    }

    impl FakeProvider {
        fn new(events: impl IntoIterator<Item = ProviderTurnEvent>) -> Self {
            Self {
                events: events.into_iter().collect(),
                finished: false,
                cancelled: false,
            }
        }
    }

    impl ModelProviderInvoker for FakeProvider {
        fn start_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a ModelTurnRequest,
        ) -> RuntimeFuture<'a, StartTurnResponse> {
            Box::pin(async {
                Ok(StartTurnResponse {
                    provider_turn_id: "turn-1".to_string(),
                })
            })
        }

        fn poll_turn_events<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a PollTurnEventsRequest,
        ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
            Box::pin(async move {
                Ok(PollTurnEventsResponse {
                    events: self.events.pop_front().into_iter().collect(),
                })
            })
        }

        fn cancel_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a CancelTurnRequest,
        ) -> RuntimeFuture<'a, AckResponse> {
            self.cancelled = true;
            Box::pin(async { Ok(AckResponse::default()) })
        }

        fn finish_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a FinishTurnRequest,
        ) -> RuntimeFuture<'a, AckResponse> {
            self.finished = true;
            Box::pin(async { Ok(AckResponse::default()) })
        }
    }

    struct FakeToolInvoker;

    impl ToolInvoker for FakeToolInvoker {
        fn prepare_tool<'a>(
            &'a self,
            tool: &'a RegisteredTool,
            request: &'a ToolPreparationRequest,
            _scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            let result = bcode_agent_profile::prepare_tool_policy(request, &tool.definition)
                .map_err(|message| RuntimeError::ToolPreparation {
                    tool_name: request.invocation.tool_name.clone(),
                    message,
                });
            Box::pin(async move { result })
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            invocation: &'a PreparedToolInvocation,
            _scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            Box::pin(async move {
                Ok(ToolInvocationResponse {
                    output: format!("called {}", invocation.invocation.tool_name),
                    is_error: false,
                    content: vec![InvocationToolResultContent::Text {
                        text: "structured".to_string(),
                    }],
                    full_output: None,
                    host_action: None,
                    result: None,
                })
            })
        }
    }

    async fn execute_fake_batch<P: PermissionPolicy + ?Sized>(
        runtime: &AgentRuntime,
        catalog: &UnifiedToolCatalog,
        policy: &P,
        calls: &[ToolCall],
        rounds: &mut ToolRoundState,
        options: ToolExecutionOptions,
    ) -> Result<ToolBatchExecutionOutput> {
        let authorization = PermissionPolicyAuthorization::new(policy);
        let scope = TurnScope::without_events("test-tool-batch", TurnGeneration::new(0));
        runtime
            .execute_prepared_tool_batch(
                catalog,
                &authorization,
                &FakeToolInvoker,
                calls,
                rounds,
                &RuntimePermissionContext::default(),
                options,
                &scope,
            )
            .await
    }

    fn tool_definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: "test tool".to_string(),
            input_schema: serde_json::json!({ "type": "object" }),
            side_effect: ToolSideEffect::ReadOnly,
            requires_permission: false,
            policy: ToolPolicyMetadata::default(),
            ui: ToolUiMetadata::default(),
        }
    }

    #[derive(Debug)]
    struct TestCancelHandle(Arc<AtomicUsize>);

    impl InvocationCancellation for TestCancelHandle {
        fn request_cancel(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug)]
    struct CancellationHandleInvoker {
        started: AtomicUsize,
        cancellations: BTreeMap<String, Arc<AtomicUsize>>,
    }

    impl ToolInvoker for CancellationHandleInvoker {
        fn prepare_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            _request: &'a ToolPreparationRequest,
            _scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            Box::pin(async move {
                Ok(ToolPreparationResponse {
                    authorization: Vec::new(),
                    descriptor: serde_json::Value::Null,
                })
            })
        }

        fn cancellation_handle(
            &self,
            _tool: &RegisteredTool,
            invocation: &PreparedToolInvocation,
        ) -> Option<Arc<dyn InvocationCancellation>> {
            self.cancellations
                .get(&invocation.invocation.tool_name)
                .map(|count| {
                    Arc::new(TestCancelHandle(Arc::clone(count))) as Arc<dyn InvocationCancellation>
                })
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            _invocation: &'a PreparedToolInvocation,
            _scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            self.started.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }

    #[derive(Debug)]
    struct BatchOverlapInvoker {
        prepared: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl ToolInvoker for BatchOverlapInvoker {
        fn prepare_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            _request: &'a ToolPreparationRequest,
            _scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            self.prepared.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(ToolPreparationResponse::default()) })
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            invocation: &'a PreparedToolInvocation,
            _scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            Box::pin(async move {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(ToolInvocationResponse {
                    output: invocation.invocation.tool_name.clone(),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                    host_action: None,
                    result: None,
                })
            })
        }
    }

    #[derive(Debug)]
    struct SelectivePreparationInvoker {
        fail_name: String,
        started: AtomicUsize,
    }

    impl ToolInvoker for SelectivePreparationInvoker {
        fn prepare_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            request: &'a ToolPreparationRequest,
            _scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            Box::pin(async move {
                if request.invocation.tool_name == self.fail_name {
                    Err(RuntimeError::ToolPreparation {
                        tool_name: request.invocation.tool_name.clone(),
                        message: "synthetic preparation failure".to_string(),
                    })
                } else {
                    Ok(ToolPreparationResponse::default())
                }
            })
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            invocation: &'a PreparedToolInvocation,
            _scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            self.started.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(ToolInvocationResponse {
                    output: format!("called {}", invocation.invocation.tool_name),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                    host_action: None,
                    result: None,
                })
            })
        }
    }

    #[derive(Debug, Default)]
    struct BlockingAuthorization {
        release: Notify,
        observed: AtomicUsize,
    }

    impl ToolAuthorizationCoordinator for BlockingAuthorization {
        fn authorize_batch<'a>(
            &'a self,
            requests: &'a [ToolAuthorizationRequest],
            _scope: &'a TurnScope,
        ) -> RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
            self.observed.store(requests.len(), Ordering::SeqCst);
            Box::pin(async move {
                self.release.notified().await;
                Ok(requests
                    .iter()
                    .map(|_| ToolAuthorizationDecision::Allow)
                    .collect())
            })
        }
    }

    #[derive(Debug, Default)]
    struct AllowBatchAuthorization {
        observed: AtomicUsize,
    }

    impl ToolAuthorizationCoordinator for AllowBatchAuthorization {
        fn authorize_batch<'a>(
            &'a self,
            requests: &'a [ToolAuthorizationRequest],
            _scope: &'a TurnScope,
        ) -> RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
            self.observed.store(requests.len(), Ordering::SeqCst);
            Box::pin(async move {
                Ok(requests
                    .iter()
                    .map(|_| ToolAuthorizationDecision::Allow)
                    .collect())
            })
        }
    }

    #[derive(Debug)]
    struct ContractTestInvoker {
        prepared: AtomicUsize,
        started: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        expected_prepared_before_start: usize,
    }

    impl ContractTestInvoker {
        fn new(expected_prepared_before_start: usize) -> Self {
            Self {
                prepared: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                expected_prepared_before_start,
            }
        }
    }

    impl ToolInvoker for ContractTestInvoker {
        fn prepare_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            _request: &'a ToolPreparationRequest,
            _scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            self.prepared.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(ToolPreparationResponse {
                    authorization: Vec::new(),
                    descriptor: serde_json::Value::Null,
                })
            })
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            invocation: &'a PreparedToolInvocation,
            scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            Box::pin(async move {
                assert_eq!(
                    self.prepared.load(Ordering::SeqCst),
                    self.expected_prepared_before_start,
                    "every call must be prepared before invocation"
                );
                if !scope.accepts_work() {
                    return Err(RuntimeError::Cancelled);
                }
                self.started.fetch_add(1, Ordering::SeqCst);
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(ToolInvocationResponse {
                    output: format!("called {}", invocation.invocation.tool_name),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                    host_action: None,
                    result: None,
                })
            })
        }
    }

    #[derive(Debug, Default)]
    struct BlockingPreparationInvoker;

    impl ToolInvoker for BlockingPreparationInvoker {
        fn prepare_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            _request: &'a ToolPreparationRequest,
            _scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            Box::pin(std::future::pending())
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            _invocation: &'a PreparedToolInvocation,
            _scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            unreachable!("timed-out preparation must not invoke the tool")
        }
    }

    #[tokio::test]
    async fn provider_error_event_cancels_and_finishes_active_provider_turn() {
        let mut provider = FakeProvider::new([ProviderTurnEvent::Error {
            error: ProviderError {
                code: "failed".to_string(),
                category: bcode_model::ProviderErrorCategory::ProviderInternal,
                message: "synthetic failure".to_string(),
                retryable: false,
                provider_message: None,
                retry: None,
            },
        }]);

        let error = AgentRuntime::new()
            .run_text_turn(&mut provider, AgentTurnRequest::new("model", "prompt"))
            .await
            .expect_err("provider error should fail the turn");

        assert!(matches!(
            error,
            RuntimeError::Provider { code, .. } if code == "failed"
        ));
        assert!(provider.cancelled);
        assert!(provider.finished);
    }

    #[derive(Debug, Default)]
    struct CountingRoundObserver {
        batches: AtomicUsize,
        results: AtomicUsize,
    }

    impl ToolRoundObserver for CountingRoundObserver {
        fn before_tool_batch(&self, calls: &[ToolCall]) -> Result<()> {
            self.batches.fetch_add(1, Ordering::SeqCst);
            assert_eq!(calls.len(), 2);
            Ok(())
        }

        fn after_tool_call(&self, _call: &ToolCall, _output: &ToolExecutionOutput) -> Result<()> {
            self.results.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct MultiRoundProvider {
        rounds: VecDeque<VecDeque<ProviderTurnEvent>>,
        active: VecDeque<ProviderTurnEvent>,
        requests: Arc<StdMutex<Vec<ModelTurnRequest>>>,
        next_turn: usize,
    }

    impl MultiRoundProvider {
        fn new(
            rounds: impl IntoIterator<Item = Vec<ProviderTurnEvent>>,
            requests: Arc<StdMutex<Vec<ModelTurnRequest>>>,
        ) -> Self {
            Self {
                rounds: rounds
                    .into_iter()
                    .map(VecDeque::from)
                    .collect::<VecDeque<_>>(),
                active: VecDeque::new(),
                requests,
                next_turn: 0,
            }
        }
    }

    impl ModelProviderInvoker for MultiRoundProvider {
        fn start_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            request: &'a ModelTurnRequest,
        ) -> RuntimeFuture<'a, StartTurnResponse> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            self.active = self.rounds.pop_front().expect("configured provider round");
            self.next_turn += 1;
            let provider_turn_id = format!("turn-{}", self.next_turn);
            Box::pin(async move { Ok(StartTurnResponse { provider_turn_id }) })
        }

        fn poll_turn_events<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a PollTurnEventsRequest,
        ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
            Box::pin(async move {
                Ok(PollTurnEventsResponse {
                    events: self.active.pop_front().into_iter().collect(),
                })
            })
        }

        fn cancel_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a CancelTurnRequest,
        ) -> RuntimeFuture<'a, AckResponse> {
            Box::pin(async { Ok(AckResponse::default()) })
        }

        fn finish_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a FinishTurnRequest,
        ) -> RuntimeFuture<'a, AckResponse> {
            Box::pin(async { Ok(AckResponse::default()) })
        }
    }

    #[tokio::test]
    async fn canonical_loop_preserves_provider_parallel_capability_fallback() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut provider = MultiRoundProvider::new(
            [vec![ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            }]],
            Arc::clone(&requests),
        );
        let request = AgentTurnRequest::new("model", "no tools");

        AgentRuntime::new()
            .run_provider_tool_loop(
                &mut provider,
                request,
                &EmptyToolCatalog,
                &AllowBatchAuthorization::default(),
                &ContractTestInvoker::new(0),
                &RuntimePermissionContext::default(),
                &[],
                ToolExecutionOptions::default(),
                Arc::new(RuntimeStreamEventSink { sender: None }),
                InvocationCapabilities::default(),
                &NoopToolRoundObserver,
                &NoopProviderRoundPlanner,
            )
            .await
            .expect("unsupported parallel policy fallback should complete");

        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].tool_call_policy.parallel,
            "scheduler support must not upgrade unsupported provider/model capability"
        );
        drop(requests);
    }

    #[tokio::test]
    async fn canonical_loop_preserves_negotiated_parallel_policy() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut provider = MultiRoundProvider::new(
            [vec![ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            }]],
            Arc::clone(&requests),
        );
        let mut request = AgentTurnRequest::new("model", "no tools");
        request.tool_call_policy.parallel = true;

        AgentRuntime::new()
            .run_provider_tool_loop(
                &mut provider,
                request,
                &EmptyToolCatalog,
                &AllowBatchAuthorization::default(),
                &ContractTestInvoker::new(0),
                &RuntimePermissionContext::default(),
                &[],
                ToolExecutionOptions::default(),
                Arc::new(RuntimeStreamEventSink { sender: None }),
                InvocationCapabilities::default(),
                &NoopToolRoundObserver,
                &NoopProviderRoundPlanner,
            )
            .await
            .expect("negotiated parallel policy should complete");

        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tool_call_policy.parallel);
        drop(requests);
    }

    #[tokio::test]
    async fn canonical_loop_derives_sequential_provider_policy_from_scheduler_options() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut provider = MultiRoundProvider::new(
            [vec![ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            }]],
            Arc::clone(&requests),
        );
        let mut request = AgentTurnRequest::new("model", "no tools");
        request.tool_call_policy.parallel = true;

        AgentRuntime::new()
            .run_provider_tool_loop(
                &mut provider,
                request,
                &EmptyToolCatalog,
                &AllowBatchAuthorization::default(),
                &ContractTestInvoker::new(0),
                &RuntimePermissionContext::default(),
                &[],
                ToolExecutionOptions {
                    parallel: false,
                    ..ToolExecutionOptions::default()
                },
                Arc::new(RuntimeStreamEventSink { sender: None }),
                InvocationCapabilities::default(),
                &NoopToolRoundObserver,
                &NoopProviderRoundPlanner,
            )
            .await
            .expect("sequential canonical loop should complete");

        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].tool_call_policy.parallel);
        drop(requests);
    }

    #[tokio::test]
    async fn canonical_loop_runs_provider_batch_and_ordered_continuation() {
        let first = ToolCall {
            id: "call-1".to_string(),
            name: "first".to_string(),
            arguments: serde_json::Value::Null,
        };
        let second = ToolCall {
            id: "call-2".to_string(),
            name: "second".to_string(),
            arguments: serde_json::Value::Null,
        };
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut provider = MultiRoundProvider::new(
            [
                vec![
                    ProviderTurnEvent::ToolCallFinished {
                        call: first.clone(),
                    },
                    ProviderTurnEvent::ToolCallFinished {
                        call: second.clone(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::ToolCall,
                    },
                ],
                vec![
                    ProviderTurnEvent::TextDelta {
                        text: "done".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ],
            Arc::clone(&requests),
        );
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"));
        let invoker = ContractTestInvoker::new(2);
        let observer = CountingRoundObserver::default();
        let mut request = AgentTurnRequest::new("model", "run tools");
        request.max_tool_rounds = 1;

        let response = AgentRuntime::new()
            .run_provider_tool_loop(
                &mut provider,
                request,
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &RuntimePermissionContext::default(),
                &[],
                ToolExecutionOptions::default(),
                Arc::new(RuntimeStreamEventSink { sender: None }),
                InvocationCapabilities::default(),
                &observer,
                &NoopProviderRoundPlanner,
            )
            .await
            .expect("canonical provider/tool loop should complete");

        assert_eq!(response.text, "done");
        assert_eq!(invoker.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(observer.batches.load(Ordering::SeqCst), 1);
        assert_eq!(observer.results.load(Ordering::SeqCst), 2);
        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 2);
        assert!(matches!(requests[1].messages[0].role, MessageRole::User));
        assert!(matches!(
            requests[1].messages[1].role,
            MessageRole::Assistant
        ));
        let result_ids = requests[1].messages[2..]
            .iter()
            .map(|message| match &message.content[0] {
                ContentBlock::ToolResult { result } => result.call_id.as_str(),
                other => panic!("expected tool result, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(result_ids, ["call-1", "call-2"]);
        drop(requests);
    }

    #[derive(Debug, Default)]
    struct RetryOncePlanner {
        plans: AtomicUsize,
    }

    impl ProviderRoundPlanner for RetryOncePlanner {
        fn plan_round<'a>(
            &'a self,
            context: ProviderRoundPlanContext<'a>,
        ) -> RuntimeFuture<'a, ProviderRoundPlan> {
            self.plans.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if context.previous_failure.is_some() {
                    assert_eq!(context.round, 0);
                    assert_eq!(context.attempt, 1);
                    let mut request = context.proposed_request.clone();
                    request
                        .metadata
                        .insert("recovered".to_string(), "true".to_string());
                    Ok(ProviderRoundPlan::RetryAfter {
                        request,
                        delay: Duration::from_millis(1),
                    })
                } else {
                    assert_eq!(context.attempt, 0);
                    Ok(ProviderRoundPlan::Proceed {
                        request: context.proposed_request.clone(),
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn canonical_planner_recovers_provider_failure_in_same_scope() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut provider = MultiRoundProvider::new(
            [
                vec![ProviderTurnEvent::Error {
                    error: ProviderError {
                        code: "retry-me".to_string(),
                        category: bcode_model::ProviderErrorCategory::Network,
                        message: "temporary".to_string(),
                        retryable: true,
                        provider_message: None,
                        retry: None,
                    },
                }],
                vec![
                    ProviderTurnEvent::TextDelta {
                        text: "recovered".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ],
            Arc::clone(&requests),
        );
        let planner = RetryOncePlanner::default();

        let response = AgentRuntime::new()
            .run_provider_tool_loop(
                &mut provider,
                AgentTurnRequest::new("model", "recover"),
                &UnifiedToolCatalog::new(),
                &AllowBatchAuthorization::default(),
                &ContractTestInvoker::new(0),
                &RuntimePermissionContext::default(),
                &[],
                ToolExecutionOptions::default(),
                Arc::new(RuntimeStreamEventSink { sender: None }),
                InvocationCapabilities::default(),
                &NoopToolRoundObserver,
                &planner,
            )
            .await
            .expect("planner should recover provider failure");

        assert_eq!(response.text, "recovered");
        assert_eq!(planner.plans.load(Ordering::SeqCst), 2);
        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].metadata.get("recovered").map(String::as_str),
            Some("true")
        );
        drop(requests);
    }

    #[derive(Debug, Default)]
    struct BlockingProviderPlanner;

    impl ProviderRoundPlanner for BlockingProviderPlanner {
        fn plan_round<'a>(
            &'a self,
            _context: ProviderRoundPlanContext<'a>,
        ) -> RuntimeFuture<'a, ProviderRoundPlan> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_provider_planning_before_start() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut provider = MultiRoundProvider::new(Vec::<Vec<ProviderTurnEvent>>::new(), requests);
        let cancellation = CancellationToken::new();
        let mut request = AgentTurnRequest::new("model", "blocked planning");
        request.cancellation = cancellation.clone();
        let runtime = AgentRuntime::new();
        let catalog = UnifiedToolCatalog::new();
        let authorization = AllowBatchAuthorization::default();
        let invoker = ContractTestInvoker::new(0);
        let context = RuntimePermissionContext::default();
        let execution = runtime.run_provider_tool_loop(
            &mut provider,
            request,
            &catalog,
            &authorization,
            &invoker,
            &context,
            &[],
            ToolExecutionOptions::default(),
            Arc::new(RuntimeStreamEventSink { sender: None }),
            InvocationCapabilities::default(),
            &NoopToolRoundObserver,
            &BlockingProviderPlanner,
        );
        let cancel = async {
            tokio::task::yield_now().await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(execution, cancel);

        assert!(matches!(result, Err(RuntimeError::Cancelled)));
        assert_eq!(provider.next_turn, 0);
    }

    #[derive(Debug, Default)]
    struct DelayedProviderPlanner;

    impl ProviderRoundPlanner for DelayedProviderPlanner {
        fn plan_round<'a>(
            &'a self,
            context: ProviderRoundPlanContext<'a>,
        ) -> RuntimeFuture<'a, ProviderRoundPlan> {
            Box::pin(async move {
                Ok(ProviderRoundPlan::RetryAfter {
                    request: context.proposed_request.clone(),
                    delay: Duration::from_mins(1),
                })
            })
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_provider_retry_delay_before_start() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut provider = MultiRoundProvider::new(Vec::<Vec<ProviderTurnEvent>>::new(), requests);
        let cancellation = CancellationToken::new();
        let mut request = AgentTurnRequest::new("model", "delayed planning");
        request.cancellation = cancellation.clone();
        let runtime = AgentRuntime::new();
        let catalog = UnifiedToolCatalog::new();
        let authorization = AllowBatchAuthorization::default();
        let invoker = ContractTestInvoker::new(0);
        let context = RuntimePermissionContext::default();
        let execution = runtime.run_provider_tool_loop(
            &mut provider,
            request,
            &catalog,
            &authorization,
            &invoker,
            &context,
            &[],
            ToolExecutionOptions::default(),
            Arc::new(RuntimeStreamEventSink { sender: None }),
            InvocationCapabilities::default(),
            &NoopToolRoundObserver,
            &DelayedProviderPlanner,
        );
        let cancel = async {
            tokio::task::yield_now().await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(execution, cancel);

        assert!(matches!(result, Err(RuntimeError::Cancelled)));
        assert_eq!(provider.next_turn, 0);
    }

    #[tokio::test]
    async fn canonical_loop_stops_at_tool_round_limit() {
        let first = ToolCall {
            id: "call-1".to_string(),
            name: "first".to_string(),
            arguments: serde_json::Value::Null,
        };
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut provider = MultiRoundProvider::new(
            [
                vec![
                    ProviderTurnEvent::ToolCallFinished {
                        call: first.clone(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::ToolCall,
                    },
                ],
                vec![
                    ProviderTurnEvent::ToolCallFinished {
                        call: first.clone(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::ToolCall,
                    },
                ],
            ],
            Arc::clone(&requests),
        );
        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("first"));
        let invoker = ContractTestInvoker::new(1);
        let mut request = AgentTurnRequest::new("model", "run tools");
        request.max_tool_rounds = 1;

        let error = AgentRuntime::new()
            .run_provider_tool_loop(
                &mut provider,
                request,
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &RuntimePermissionContext::default(),
                &[],
                ToolExecutionOptions::default(),
                Arc::new(RuntimeStreamEventSink { sender: None }),
                InvocationCapabilities::default(),
                &NoopToolRoundObserver,
                &NoopProviderRoundPlanner,
            )
            .await
            .expect_err("second tool round must exceed the configured limit");

        assert!(matches!(error, RuntimeError::MaxToolRounds(1)));
        assert_eq!(invoker.started.load(Ordering::SeqCst), 1);
        assert_eq!(
            requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn preparation_timeout_is_a_per_call_terminal_outcome() {
        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("blocked"));
        let calls = [ToolCall {
            id: "call".to_string(),
            name: "blocked".to_string(),
            arguments: serde_json::json!({}),
        }];
        let mut rounds = ToolRoundState::new(1);
        let timeout = std::num::NonZeroU64::new(1).expect("one is non-zero");

        let output = AgentRuntime::new()
            .execute_prepared_tool_batch(
                &catalog,
                &AllowBatchAuthorization::default(),
                &BlockingPreparationInvoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions {
                    preparation_timeout_ms: timeout,
                    ..ToolExecutionOptions::default()
                },
                &TurnScope::without_events("turn", TurnGeneration::new(1)),
            )
            .await
            .expect("batch orchestration should complete");

        assert!(matches!(
            &output.results[0],
            Err(RuntimeError::ToolPreparationTimeout { tool_name, timeout: actual })
                if tool_name == "blocked" && *actual == Duration::from_millis(timeout.get())
        ));
    }

    #[derive(Debug, Default)]
    struct DirectInvocationHost {
        events: StdMutex<Vec<ScopedTurnEvent>>,
        exchanges: AtomicUsize,
        inputs: AtomicUsize,
        services: AtomicUsize,
        artifacts: AtomicUsize,
    }

    impl TurnEventSink for DirectInvocationHost {
        fn emit(&self, event: ScopedTurnEvent) -> bool {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
            true
        }
    }

    impl InvocationExchangeBroker for DirectInvocationHost {
        fn request(
            &self,
            request: ToolExchangeRequest,
        ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
            self.exchanges.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"exchange_id": request.exchange_id}),
                }
            })
        }
    }

    impl InvocationInputRouter for DirectInvocationHost {
        fn receive(
            &self,
            invocation_id: &str,
        ) -> InvocationCapabilityFuture<'_, ToolInvocationInputResolution> {
            self.inputs.fetch_add(1, Ordering::SeqCst);
            let invocation_id = invocation_id.to_string();
            Box::pin(async move {
                ToolInvocationInputResolution::Received {
                    input: ToolInvocationInput {
                        invocation_id,
                        input_id: "input".to_string(),
                        producer_id: "test-host".to_string(),
                        schema: "test.input".to_string(),
                        schema_version: 1,
                        payload: serde_json::json!({"action": "continue"}),
                    },
                }
            })
        }
    }

    impl InvocationServiceRouter for DirectInvocationHost {
        fn invoke(
            &self,
            request: ToolInvocationServiceRequest,
        ) -> InvocationCapabilityFuture<'_, ToolInvocationServiceResolution> {
            self.services.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                ToolInvocationServiceResolution::Responded {
                    payload: serde_json::json!({"operation": request.operation}),
                }
            })
        }
    }

    impl InvocationArtifactSink for DirectInvocationHost {
        fn write(
            &self,
            request: ToolArtifactWriteRequest,
            commit: ArtifactCommitGuard,
        ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution> {
            self.artifacts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                commit
                    .commit(|| ToolArtifactWriteResolution::Written {
                        artifact_id: request.artifact_id,
                        byte_len: u64::try_from(request.bytes.len()).unwrap_or(u64::MAX),
                        reference: serde_json::json!({"stored": true}),
                    })
                    .unwrap_or(ToolArtifactWriteResolution::Cancelled)
            })
        }
    }

    #[derive(Debug, Default)]
    struct DirectCapabilityInvoker;

    impl ToolInvoker for DirectCapabilityInvoker {
        fn prepare_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            _request: &'a ToolPreparationRequest,
            scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            Box::pin(async move {
                assert!(scope.accepts_work());
                Ok(ToolPreparationResponse {
                    authorization: Vec::new(),
                    descriptor: serde_json::json!({"prepared": true}),
                })
            })
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            invocation: &'a PreparedToolInvocation,
            scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            Box::pin(async move {
                let invocation_id = invocation.invocation.invocation_id.clone();
                assert!(scope.emit_lifecycle(ToolInvocationLifecycleEvent {
                    invocation_id: invocation_id.clone(),
                    sequence: 1,
                    stage: ToolInvocationLifecycleStage::Started,
                    message: None,
                    metadata: serde_json::Value::Null,
                }));
                assert!(scope.emit_contribution(ToolContributionEvent {
                    invocation_id: invocation_id.clone(),
                    contribution_id: "contribution".to_string(),
                    sequence: 1,
                    producer_id: "direct-tool".to_string(),
                    schema: "test.contribution".to_string(),
                    schema_version: 1,
                    operation: ToolContributionOperation::Upsert,
                    persistence: ToolContributionPersistence::Transient,
                    payload: serde_json::json!({"text": "working"}),
                }));
                assert!(matches!(
                    scope
                        .request_exchange(ToolExchangeRequest {
                            invocation_id: invocation_id.clone(),
                            exchange_id: "exchange".to_string(),
                            producer_id: "direct-tool".to_string(),
                            schema: "test.exchange".to_string(),
                            schema_version: 1,
                            payload: serde_json::json!({"prompt": "continue?"}),
                            response_policy: ToolExchangeResponsePolicy::Required,
                        })
                        .await,
                    ToolExchangeResolution::Responded { .. }
                ));
                assert!(matches!(
                    scope.receive_input().await,
                    ToolInvocationInputResolution::Received { .. }
                ));
                assert!(matches!(
                    scope
                        .invoke_service(ToolInvocationServiceRequest {
                            invocation_id: invocation_id.clone(),
                            request_id: "service".to_string(),
                            interface_id: "test.service/v1".to_string(),
                            operation: "execute".to_string(),
                            payload: serde_json::Value::Null,
                        })
                        .await,
                    ToolInvocationServiceResolution::Responded { .. }
                ));
                assert!(matches!(
                    scope
                        .write_artifact(ToolArtifactWriteRequest {
                            invocation_id: invocation_id.clone(),
                            artifact_id: "artifact".to_string(),
                            content_type: "application/octet-stream".to_string(),
                            bytes: vec![1, 2, 3],
                            metadata: serde_json::Value::Null,
                        })
                        .await,
                    ToolArtifactWriteResolution::Written { .. }
                ));
                assert!(scope.emit_lifecycle(ToolInvocationLifecycleEvent {
                    invocation_id,
                    sequence: 2,
                    stage: ToolInvocationLifecycleStage::Completed,
                    message: None,
                    metadata: serde_json::Value::Null,
                }));
                Ok(ToolInvocationResponse {
                    output: "direct capability invocation completed".to_string(),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                    host_action: None,
                    result: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn direct_rust_tool_uses_every_neutral_invocation_capability() {
        let host = Arc::new(DirectInvocationHost::default());
        let runtime = AgentRuntime::new();
        let scope = runtime.begin_turn_scope(
            "turn",
            host.clone(),
            InvocationCapabilities::new(host.clone(), host.clone(), host.clone(), host.clone()),
        );
        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("direct"));
        let calls = [ToolCall {
            id: "call".to_string(),
            name: "direct".to_string(),
            arguments: serde_json::json!({}),
        }];
        let mut rounds = ToolRoundState::new(1);

        let output = runtime
            .execute_prepared_tool_batch(
                &catalog,
                &AllowBatchAuthorization::default(),
                &DirectCapabilityInvoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions::default(),
                &scope,
            )
            .await
            .expect("direct invocation should execute");

        assert!(output.results[0].is_ok());
        assert_eq!(host.exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(host.inputs.load(Ordering::SeqCst), 1);
        assert_eq!(host.services.load(Ordering::SeqCst), 1);
        assert_eq!(host.artifacts.load(Ordering::SeqCst), 1);
        let events = host
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events.as_slice(),
            [
                ScopedTurnEvent::InvocationLifecycle(_),
                ScopedTurnEvent::Contribution(_),
                ScopedTurnEvent::InvocationLifecycle(_),
                ScopedTurnEvent::Runtime(AgentRuntimeEvent::ToolResult(_))
            ]
        ));
        drop(events);
    }

    #[derive(Debug)]
    struct GatedExchangeBroker {
        requests: AtomicUsize,
        permits: tokio::sync::Semaphore,
    }

    impl Default for GatedExchangeBroker {
        fn default() -> Self {
            Self {
                requests: AtomicUsize::new(0),
                permits: tokio::sync::Semaphore::new(0),
            }
        }
    }

    impl InvocationExchangeBroker for GatedExchangeBroker {
        fn request(
            &self,
            request: ToolExchangeRequest,
        ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                self.permits
                    .acquire()
                    .await
                    .expect("test semaphore should remain open")
                    .forget();
                ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"exchange_id": request.exchange_id}),
                }
            })
        }
    }

    #[derive(Debug, Default)]
    struct ExchangeWaitingInvoker {
        started: AtomicUsize,
    }

    impl ToolInvoker for ExchangeWaitingInvoker {
        fn prepare_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            _request: &'a ToolPreparationRequest,
            _scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            Box::pin(async {
                Ok(ToolPreparationResponse {
                    authorization: Vec::new(),
                    descriptor: serde_json::Value::Null,
                })
            })
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            invocation: &'a PreparedToolInvocation,
            scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            self.started.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let resolution = scope
                    .request_exchange(ToolExchangeRequest {
                        invocation_id: invocation.invocation.invocation_id.clone(),
                        exchange_id: "exchange".to_string(),
                        producer_id: "direct-tool".to_string(),
                        schema: "test.exchange".to_string(),
                        schema_version: 1,
                        payload: serde_json::Value::Null,
                        response_policy: ToolExchangeResponsePolicy::Required,
                    })
                    .await;
                assert!(matches!(
                    resolution,
                    ToolExchangeResolution::Responded { .. }
                ));
                Ok(ToolInvocationResponse {
                    output: invocation.invocation.tool_name.clone(),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                    host_action: None,
                    result: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn exchange_wait_retains_scheduler_concurrency_slot() {
        let broker = Arc::new(GatedExchangeBroker::default());
        let host = Arc::new(DirectInvocationHost::default());
        let invoker = Arc::new(ExchangeWaitingInvoker::default());
        let runtime = Arc::new(AgentRuntime::new());
        let catalog = Arc::new(
            UnifiedToolCatalog::new()
                .with_inline_tool(tool_definition("first"))
                .with_inline_tool(tool_definition("second")),
        );
        let calls = vec![
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let scope = runtime.begin_turn_scope(
            "turn",
            Arc::new(RuntimeStreamEventSink { sender: None }),
            InvocationCapabilities::new(broker.clone(), host.clone(), host.clone(), host),
        );
        let task_runtime = runtime.clone();
        let task_catalog = catalog.clone();
        let task_invoker = invoker.clone();
        let task_scope = scope.clone();
        let execution = tokio::spawn(async move {
            let mut rounds = ToolRoundState::new(1);
            task_runtime
                .execute_prepared_tool_batch(
                    task_catalog.as_ref(),
                    &AllowBatchAuthorization::default(),
                    task_invoker.as_ref(),
                    &calls,
                    &mut rounds,
                    &RuntimePermissionContext::default(),
                    ToolExecutionOptions {
                        max_concurrency: Some(std::num::NonZeroUsize::MIN),
                        ..ToolExecutionOptions::default()
                    },
                    &task_scope,
                )
                .await
        });

        wait_for_atomic_value(&invoker.started, 1).await;
        assert_eq!(broker.requests.load(Ordering::SeqCst), 1);
        assert_eq!(invoker.started.load(Ordering::SeqCst), 1);
        broker.permits.add_permits(1);
        wait_for_atomic_value(&invoker.started, 2).await;
        assert_eq!(broker.requests.load(Ordering::SeqCst), 2);
        broker.permits.add_permits(1);

        let output = execution
            .await
            .expect("execution task should not panic")
            .expect("batch should complete");
        assert!(output.results.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn cancelling_exchange_wait_terminates_scheduler_without_starting_queued_sibling() {
        let broker = Arc::new(GatedExchangeBroker::default());
        let host = Arc::new(DirectInvocationHost::default());
        let invoker = Arc::new(ExchangeWaitingInvoker::default());
        let runtime = Arc::new(AgentRuntime::new());
        let catalog = Arc::new(
            UnifiedToolCatalog::new()
                .with_inline_tool(tool_definition("first"))
                .with_inline_tool(tool_definition("second")),
        );
        let calls = vec![
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let scope = runtime.begin_turn_scope(
            "turn",
            Arc::new(RuntimeStreamEventSink { sender: None }),
            InvocationCapabilities::new(broker.clone(), host.clone(), host.clone(), host),
        );
        let task_runtime = runtime.clone();
        let task_catalog = catalog.clone();
        let task_invoker = invoker.clone();
        let task_scope = scope.clone();
        let execution = tokio::spawn(async move {
            let mut rounds = ToolRoundState::new(1);
            task_runtime
                .execute_prepared_tool_batch(
                    task_catalog.as_ref(),
                    &AllowBatchAuthorization::default(),
                    task_invoker.as_ref(),
                    &calls,
                    &mut rounds,
                    &RuntimePermissionContext::default(),
                    ToolExecutionOptions {
                        max_concurrency: Some(std::num::NonZeroUsize::MIN),
                        ..ToolExecutionOptions::default()
                    },
                    &task_scope,
                )
                .await
        });

        wait_for_atomic_value(&invoker.started, 1).await;
        assert!(runtime.cancel_turn_scope(&scope));
        let output = tokio::time::timeout(Duration::from_secs(1), execution)
            .await
            .expect("cancelled scheduler should terminate")
            .expect("execution task should not panic")
            .expect("batch orchestration should return ordered outcomes");

        assert_eq!(invoker.started.load(Ordering::SeqCst), 1);
        assert_eq!(broker.requests.load(Ordering::SeqCst), 1);
        assert!(
            output
                .results
                .iter()
                .all(|result| matches!(result, Err(RuntimeError::Cancelled)))
        );
    }

    async fn wait_for_atomic_value(value: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while value.load(Ordering::SeqCst) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("atomic value should reach expected count");
    }

    #[derive(Debug, Default)]
    struct HostContextInvoker {
        observed: std::sync::Mutex<Vec<bcode_tool::ToolHostContextEntry>>,
    }

    impl ToolInvoker for HostContextInvoker {
        fn prepare_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            request: &'a ToolPreparationRequest,
            scope: &'a PreparationScope,
        ) -> RuntimeFuture<'a, ToolPreparationResponse> {
            assert_eq!(scope.host_context(), request.host_context);
            *self
                .observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = request.host_context.clone();
            Box::pin(async { Ok(ToolPreparationResponse::default()) })
        }

        fn invoke_tool<'a>(
            &'a self,
            _tool: &'a RegisteredTool,
            invocation: &'a PreparedToolInvocation,
            _scope: &'a InvocationScope,
        ) -> RuntimeFuture<'a, ToolInvocationResponse> {
            Box::pin(async move {
                Ok(ToolInvocationResponse {
                    output: invocation.invocation.tool_name.clone(),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                    host_action: None,
                    result: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn preparation_receives_opaque_host_context_unchanged() {
        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("first"));
        let calls = [ToolCall {
            id: "call-1".to_string(),
            name: "first".to_string(),
            arguments: serde_json::json!({}),
        }];
        let host_context = [bcode_tool::ToolHostContextEntry {
            schema: "example.host-context".to_string(),
            schema_version: 7,
            payload: serde_json::json!({"opaque": [1, 2, 3]}),
        }];
        let invoker = HostContextInvoker::default();
        let mut rounds = ToolRoundState::new(1);

        let output = AgentRuntime::new()
            .execute_prepared_tool_batch_with_host_context(
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                &host_context,
                ToolExecutionOptions::default(),
                &TurnScope::without_events("turn", TurnGeneration::new(1)),
            )
            .await
            .expect("batch should execute");

        assert_eq!(output.results.len(), 1);
        assert_eq!(
            *invoker
                .observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            host_context
        );
    }

    #[tokio::test]
    async fn invalid_host_context_is_rejected_before_preparation() {
        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("first"));
        let calls = [ToolCall {
            id: "call-1".to_string(),
            name: "first".to_string(),
            arguments: serde_json::json!({}),
        }];
        let host_context = [
            bcode_tool::ToolHostContextEntry {
                schema: "example.duplicate".to_string(),
                schema_version: 1,
                payload: serde_json::Value::Null,
            },
            bcode_tool::ToolHostContextEntry {
                schema: "example.duplicate".to_string(),
                schema_version: 1,
                payload: serde_json::Value::Null,
            },
        ];
        let invoker = HostContextInvoker::default();
        let mut rounds = ToolRoundState::new(1);

        let error = AgentRuntime::new()
            .execute_prepared_tool_batch_with_host_context(
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                &host_context,
                ToolExecutionOptions::default(),
                &TurnScope::without_events("turn", TurnGeneration::new(1)),
            )
            .await
            .expect_err("duplicate host context should fail");

        assert!(matches!(
            error,
            RuntimeError::InvalidToolHostContext(message)
                if message.contains("duplicate schema example.duplicate version 1")
        ));
        assert_eq!(rounds.completed_rounds(), 0);
        assert!(
            invoker
                .observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn host_context_validation_enforces_transport_bounds() {
        let entry = |schema: String, schema_version, payload| bcode_tool::ToolHostContextEntry {
            schema,
            schema_version,
            payload,
        };
        let maximum_count = (0..TOOL_HOST_CONTEXT_MAX_ENTRIES)
            .map(|index| entry(format!("example.{index}"), 1, serde_json::Value::Null))
            .collect::<Vec<_>>();
        assert!(validate_tool_host_context(&maximum_count).is_ok());

        let too_many = (0..=TOOL_HOST_CONTEXT_MAX_ENTRIES)
            .map(|index| entry(format!("example.{index}"), 1, serde_json::Value::Null))
            .collect::<Vec<_>>();
        assert!(matches!(
            validate_tool_host_context(&too_many),
            Err(RuntimeError::InvalidToolHostContext(message))
                if message.contains("maximum is 32")
        ));
        assert!(matches!(
            validate_tool_host_context(&[entry(
                "x".repeat(TOOL_HOST_CONTEXT_SCHEMA_MAX_BYTES + 1),
                1,
                serde_json::Value::Null,
            )]),
            Err(RuntimeError::InvalidToolHostContext(message))
                if message.contains("schema identifier")
        ));
        assert!(matches!(
            validate_tool_host_context(&[entry(
                "example.payload".to_string(),
                1,
                serde_json::Value::String("x".repeat(TOOL_HOST_CONTEXT_PAYLOAD_MAX_BYTES)),
            )]),
            Err(RuntimeError::InvalidToolHostContext(message))
                if message.contains("payload")
        ));
        let excessive_total = (0..5)
            .map(|index| {
                entry(
                    format!("example.total.{index}"),
                    1,
                    serde_json::Value::String("x".repeat(60_000)),
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            validate_tool_host_context(&excessive_total),
            Err(RuntimeError::InvalidToolHostContext(message))
                if message.contains("serialized context")
        ));
    }

    #[tokio::test]
    async fn default_concurrency_starts_the_complete_provider_batch() {
        let mut catalog = UnifiedToolCatalog::new();
        let calls = (0..8)
            .map(|index| {
                let name = format!("tool-{index}");
                catalog = std::mem::take(&mut catalog).with_inline_tool(tool_definition(&name));
                ToolCall {
                    id: format!("call-{index}"),
                    name,
                    arguments: serde_json::json!({}),
                }
            })
            .collect::<Vec<_>>();
        let invoker = ContractTestInvoker::new(calls.len());
        let mut rounds = ToolRoundState::new(1);

        AgentRuntime::new()
            .execute_prepared_tool_batch(
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions::default(),
                &TurnScope::without_events("turn", TurnGeneration::new(12)),
            )
            .await
            .expect("default-unlimited batch should execute");

        assert_eq!(invoker.max_active.load(Ordering::SeqCst), calls.len());
    }

    #[tokio::test]
    async fn neutral_batch_never_exceeds_configured_concurrency() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"))
            .with_inline_tool(tool_definition("third"));
        let calls = [
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-3".to_string(),
                name: "third".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let invoker = ContractTestInvoker::new(3);
        let mut rounds = ToolRoundState::new(1);

        AgentRuntime::new()
            .execute_prepared_tool_batch(
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions {
                    parallel: true,
                    max_concurrency: Some(std::num::NonZeroUsize::new(2).expect("two is non-zero")),
                    ..ToolExecutionOptions::default()
                },
                &TurnScope::without_events("turn", TurnGeneration::new(14)),
            )
            .await
            .expect("batch should execute");

        assert_eq!(invoker.started.load(Ordering::SeqCst), 3);
        assert_eq!(invoker.max_active.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancellation_signals_every_active_invoker_handle_and_returns_immediately() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"));
        let calls = [
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let invoker = CancellationHandleInvoker {
            started: AtomicUsize::new(0),
            cancellations: BTreeMap::from([
                ("first".to_string(), Arc::clone(&first)),
                ("second".to_string(), Arc::clone(&second)),
            ]),
        };
        let scope = TurnScope::without_events("turn", TurnGeneration::new(13));
        let control = scope.control();
        let mut rounds = ToolRoundState::new(1);
        let runtime = AgentRuntime::new();
        let authorization = AllowBatchAuthorization::default();
        let context = RuntimePermissionContext::default();
        let execution = runtime.execute_prepared_tool_batch(
            &catalog,
            &authorization,
            &invoker,
            &calls,
            &mut rounds,
            &context,
            ToolExecutionOptions::default(),
            &scope,
        );
        let cancellation = async {
            while invoker.started.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
            assert!(control.begin_cancellation());
        };
        let output = tokio::time::timeout(Duration::from_secs(1), async {
            let (output, ()) = tokio::join!(execution, cancellation);
            output
        })
        .await
        .expect("local cancellation must not wait for invocations")
        .expect("batch orchestration should finish");

        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 1);
        assert!(
            output
                .results
                .iter()
                .all(|result| matches!(result, Err(RuntimeError::Cancelled)))
        );
    }

    #[tokio::test]
    async fn parallel_group_cancellation_returns_exactly_one_outcome_per_invocation() {
        let mut catalog = UnifiedToolCatalog::new();
        let mut calls = Vec::new();
        let mut cancellation_counts = BTreeMap::new();
        for index in 0..5 {
            let name = format!("tool-{index}");
            catalog = catalog.with_inline_tool(tool_definition(&name));
            calls.push(ToolCall {
                id: format!("call-{index}"),
                name: name.clone(),
                arguments: serde_json::Value::Null,
            });
            cancellation_counts.insert(name, Arc::new(AtomicUsize::new(0)));
        }
        let invoker = CancellationHandleInvoker {
            started: AtomicUsize::new(0),
            cancellations: cancellation_counts.clone(),
        };
        let scope = TurnScope::without_events("turn", TurnGeneration::new(15));
        let control = scope.control();
        let mut rounds = ToolRoundState::new(1);
        let runtime = AgentRuntime::new();
        let authorization = AllowBatchAuthorization::default();
        let context = RuntimePermissionContext::default();
        let execution = runtime.execute_prepared_tool_batch(
            &catalog,
            &authorization,
            &invoker,
            &calls,
            &mut rounds,
            &context,
            ToolExecutionOptions {
                parallel: true,
                max_concurrency: NonZeroUsize::new(2),
                ..ToolExecutionOptions::default()
            },
            &scope,
        );
        let cancellation = async {
            while invoker.started.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
            assert!(control.begin_cancellation());
        };
        let (output, ()) = tokio::join!(execution, cancellation);
        let output = output.expect("cancelled group should return ordered outcomes");

        assert_eq!(invoker.started.load(Ordering::SeqCst), 2);
        assert_eq!(output.results.len(), calls.len());
        assert!(
            output
                .results
                .iter()
                .all(|result| matches!(result, Err(RuntimeError::Cancelled)))
        );
        assert_eq!(cancellation_counts["tool-0"].load(Ordering::SeqCst), 1);
        assert_eq!(cancellation_counts["tool-1"].load(Ordering::SeqCst), 1);
        for index in 2..5 {
            assert_eq!(
                cancellation_counts[&format!("tool-{index}")].load(Ordering::SeqCst),
                0,
                "queued invocation must not activate or receive an active-handle signal"
            );
        }
    }

    #[tokio::test]
    async fn one_provider_batch_declares_parallel_intent_without_domain_conflict_inference() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("alpha"))
            .with_inline_tool(tool_definition("beta"))
            .with_inline_tool(tool_definition("gamma"));
        let calls = [
            ToolCall {
                id: "call-1".to_string(),
                name: "alpha".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "beta".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-3".to_string(),
                name: "gamma".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let invoker = BatchOverlapInvoker {
            prepared: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        };
        let mut rounds = ToolRoundState::new(1);
        let output = AgentRuntime::new()
            .execute_prepared_tool_batch(
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions::default(),
                &TurnScope::without_events("turn", TurnGeneration::new(11)),
            )
            .await
            .expect("batch should execute");

        assert_eq!(invoker.max_active.load(Ordering::SeqCst), 3);
        assert_eq!(
            output
                .results
                .iter()
                .map(|result| result
                    .as_ref()
                    .expect("call should succeed")
                    .model_result
                    .call_id
                    .as_str())
                .collect::<Vec<_>>(),
            vec!["call-1", "call-2", "call-3"]
        );
    }

    #[tokio::test]
    async fn sequential_option_prevents_compatible_overlap() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"));
        let calls = [
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let invoker = ContractTestInvoker::new(2);
        let mut rounds = ToolRoundState::new(1);

        AgentRuntime::new()
            .execute_prepared_tool_batch(
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions {
                    parallel: false,
                    max_concurrency: Some(
                        std::num::NonZeroUsize::new(8).expect("eight is non-zero"),
                    ),
                    ..ToolExecutionOptions::default()
                },
                &TurnScope::without_events("turn", TurnGeneration::new(12)),
            )
            .await
            .expect("batch should execute");

        assert_eq!(invoker.max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn neutral_batch_keeps_preparation_failure_local_to_one_call() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("broken"))
            .with_inline_tool(tool_definition("working"));
        let calls = [
            ToolCall {
                id: "call-1".to_string(),
                name: "broken".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "working".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let invoker = SelectivePreparationInvoker {
            fail_name: "broken".to_string(),
            started: AtomicUsize::new(0),
        };
        let mut rounds = ToolRoundState::new(1);
        let output = AgentRuntime::new()
            .execute_prepared_tool_batch(
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions::default(),
                &TurnScope::without_events("turn", TurnGeneration::new(9)),
            )
            .await
            .expect("batch orchestration should succeed");

        assert!(matches!(
            &output.results[0],
            Err(RuntimeError::ToolPreparation { tool_name, .. }) if tool_name == "broken"
        ));
        assert_eq!(
            output.results[1]
                .as_ref()
                .expect("working sibling should execute")
                .model_result
                .call_id,
            "call-2"
        );
        assert_eq!(invoker.started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_during_authorization_starts_no_tools() {
        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("first"));
        let calls = [ToolCall {
            id: "call-1".to_string(),
            name: "first".to_string(),
            arguments: serde_json::json!({}),
        }];
        let invoker = ContractTestInvoker::new(1);
        let authorization = BlockingAuthorization::default();
        let scope = TurnScope::without_events("turn", TurnGeneration::new(10));
        let control = scope.control();
        let mut rounds = ToolRoundState::new(1);
        let runtime = AgentRuntime::new();
        let context = RuntimePermissionContext::default();
        let execution = runtime.execute_prepared_tool_batch(
            &catalog,
            &authorization,
            &invoker,
            &calls,
            &mut rounds,
            &context,
            ToolExecutionOptions::default(),
            &scope,
        );
        let cancellation = async {
            while authorization.observed.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
            assert!(control.begin_cancellation());
        };
        let (output, ()) = tokio::join!(execution, cancellation);
        let output = output.expect("batch orchestration should finish");

        assert_eq!(invoker.started.load(Ordering::SeqCst), 0);
        assert!(matches!(&output.results[0], Err(RuntimeError::Cancelled)));
    }

    #[tokio::test]
    async fn neutral_batch_waits_for_complete_authorization_before_starting() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"));
        let calls = [
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let invoker = ContractTestInvoker::new(2);
        let authorization = BlockingAuthorization::default();
        let scope = TurnScope::without_events("turn", TurnGeneration::new(8));
        let mut rounds = ToolRoundState::new(1);
        let runtime = AgentRuntime::new();
        let context = RuntimePermissionContext::default();
        let execution = runtime.execute_prepared_tool_batch(
            &catalog,
            &authorization,
            &invoker,
            &calls,
            &mut rounds,
            &context,
            ToolExecutionOptions::default(),
            &scope,
        );
        let release = async {
            while authorization.observed.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
            assert_eq!(invoker.started.load(Ordering::SeqCst), 0);
            authorization.release.notify_one();
        };
        let (output, ()) = tokio::join!(execution, release);

        assert!(output.is_ok());
        assert_eq!(invoker.started.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn neutral_batch_prepares_and_authorizes_before_overlapping_invocations() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"));
        let calls = vec![
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let invoker = ContractTestInvoker::new(2);
        let authorization = AllowBatchAuthorization::default();
        let scope = TurnScope::without_events("turn", TurnGeneration::new(1));
        let mut rounds = ToolRoundState::new(1);
        let output = AgentRuntime::new()
            .execute_prepared_tool_batch(
                &catalog,
                &authorization,
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions::default(),
                &scope,
            )
            .await
            .expect("batch should execute");

        assert_eq!(authorization.observed.load(Ordering::SeqCst), 2);
        assert_eq!(invoker.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(rounds.completed_rounds(), 1);
        assert_eq!(
            output
                .results
                .iter()
                .map(|result| result
                    .as_ref()
                    .expect("call should succeed")
                    .model_result
                    .call_id
                    .as_str())
                .collect::<Vec<_>>(),
            vec!["call-1", "call-2"]
        );
    }

    #[tokio::test]
    async fn parallel_batch_overlaps_without_per_tool_scheduling_metadata() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"));
        let calls = [
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let invoker = ContractTestInvoker::new(2);
        let mut rounds = ToolRoundState::new(1);

        AgentRuntime::new()
            .execute_prepared_tool_batch(
                &catalog,
                &AllowBatchAuthorization::default(),
                &invoker,
                &calls,
                &mut rounds,
                &RuntimePermissionContext::default(),
                ToolExecutionOptions::default(),
                &TurnScope::without_events("turn", TurnGeneration::new(2)),
            )
            .await
            .expect("batch should execute");

        assert_eq!(invoker.max_active.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn neutral_batch_cancellation_prevents_queued_start() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"));
        let calls = [
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let invoker = ContractTestInvoker::new(2);
        let scope = TurnScope::without_events("turn", TurnGeneration::new(3));
        let control = scope.control();
        let mut rounds = ToolRoundState::new(1);
        let cancellation = async {
            while invoker.started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            assert!(control.begin_cancellation());
        };
        let runtime = AgentRuntime::new();
        let authorization = AllowBatchAuthorization::default();
        let context = RuntimePermissionContext::default();
        let execution = runtime.execute_prepared_tool_batch(
            &catalog,
            &authorization,
            &invoker,
            &calls,
            &mut rounds,
            &context,
            ToolExecutionOptions {
                parallel: true,
                max_concurrency: Some(std::num::NonZeroUsize::new(1).expect("one is non-zero")),
                ..ToolExecutionOptions::default()
            },
            &scope,
        );
        let (output, ()) = tokio::join!(execution, cancellation);
        let output = output.expect("batch orchestration should finish");

        assert_eq!(invoker.started.load(Ordering::SeqCst), 1);
        assert!(
            output
                .results
                .iter()
                .all(|result| matches!(result, Err(RuntimeError::Cancelled)))
        );
    }

    #[tokio::test]
    async fn tool_batch_preserves_order_and_consumes_one_round() {
        let catalog = UnifiedToolCatalog::new()
            .with_inline_tool(tool_definition("first"))
            .with_inline_tool(tool_definition("second"));
        let calls = vec![
            ToolCall {
                id: "call-1".to_string(),
                name: "first".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "second".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        let mut rounds = ToolRoundState::new(1);
        let output = execute_fake_batch(
            &AgentRuntime::new(),
            &catalog,
            &AllowAllPolicy,
            &calls,
            &mut rounds,
            ToolExecutionOptions {
                parallel: true,
                max_concurrency: Some(NonZeroUsize::new(2).expect("two is non-zero")),
                ..ToolExecutionOptions::default()
            },
        )
        .await
        .expect("batch should execute");

        assert_eq!(rounds.completed_rounds(), 1);
        assert_eq!(output.results.len(), 2);
        assert_eq!(
            output.results[0]
                .as_ref()
                .expect("first call should succeed")
                .model_result
                .call_id,
            "call-1"
        );
        assert_eq!(
            output.results[1]
                .as_ref()
                .expect("second call should succeed")
                .model_result
                .call_id,
            "call-2"
        );
    }

    #[tokio::test]
    async fn unified_tool_catalog_routes_inline_tool_execution() {
        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("echo"));
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({ "text": "hi" }),
        };
        let runtime = AgentRuntime::new();
        let mut rounds = ToolRoundState::new(u32::MAX);
        let output = execute_fake_batch(
            &runtime,
            &catalog,
            &AllowAllPolicy,
            std::slice::from_ref(&call),
            &mut rounds,
            ToolExecutionOptions {
                parallel: false,
                ..ToolExecutionOptions::default()
            },
        )
        .await
        .expect("tool should execute")
        .results
        .into_iter()
        .next()
        .expect("single call result")
        .expect("single call should succeed");

        assert_eq!(output.model_result.call_id, "call-1");
        assert_eq!(output.model_result.output, "called echo");
        assert_eq!(output.model_result.content.len(), 1);
        assert!(matches!(
            output.events.as_slice(),
            [
                AgentRuntimeEvent::ToolCallFinished(call),
                AgentRuntimeEvent::ToolResult(result)
            ] if call.id == "call-1" && result.call_id == "call-1"
        ));
    }

    #[tokio::test]
    async fn tool_round_state_enforces_max_tool_rounds() {
        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("echo"));
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({ "text": "hi" }),
        };
        let runtime = AgentRuntime::new();
        let mut rounds = ToolRoundState::new(1);

        execute_fake_batch(
            &runtime,
            &catalog,
            &AllowAllPolicy,
            std::slice::from_ref(&call),
            &mut rounds,
            ToolExecutionOptions {
                parallel: false,
                ..ToolExecutionOptions::default()
            },
        )
        .await
        .expect("first tool round should execute");
        let error = execute_fake_batch(
            &runtime,
            &catalog,
            &AllowAllPolicy,
            std::slice::from_ref(&call),
            &mut rounds,
            ToolExecutionOptions {
                parallel: false,
                ..ToolExecutionOptions::default()
            },
        )
        .await
        .expect_err("second tool round should exceed max");

        assert!(matches!(error, RuntimeError::MaxToolRounds(1)));
    }

    #[tokio::test]
    async fn unified_tool_catalog_preserves_plugin_routing_source() {
        let catalog = UnifiedToolCatalog::new()
            .with_plugin_tool(tool_definition("search"), "synthetic-plugin");
        let tool = catalog
            .find_tool("search")
            .expect("plugin tool should be registered");

        assert!(matches!(
            tool.source,
            ToolSource::Plugin { ref plugin_id } if plugin_id == "synthetic-plugin"
        ));
    }

    #[tokio::test]
    async fn tool_permission_denial_is_actionable() {
        struct DenyPolicy;
        impl PermissionPolicy for DenyPolicy {
            fn evaluate_tool_call<'a>(
                &'a self,
                _request: &'a RuntimePermissionRequest,
            ) -> RuntimeFuture<'a, PermissionDecision> {
                Box::pin(async { Ok(PermissionDecision::Deny("blocked".to_string())) })
            }
        }

        let catalog = UnifiedToolCatalog::new().with_inline_tool(tool_definition("echo"));
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({}),
        };
        let runtime = AgentRuntime::new();
        let mut rounds = ToolRoundState::new(u32::MAX);
        let error = execute_fake_batch(
            &runtime,
            &catalog,
            &DenyPolicy,
            std::slice::from_ref(&call),
            &mut rounds,
            ToolExecutionOptions {
                parallel: false,
                ..ToolExecutionOptions::default()
            },
        )
        .await
        .expect("batch authorization should complete")
        .results
        .into_iter()
        .next()
        .expect("single call result")
        .expect_err("policy should deny tool");

        assert!(matches!(error, RuntimeError::PermissionDenied(reason) if reason == "blocked"));
    }

    #[tokio::test]
    async fn text_turn_accumulates_provider_deltas() {
        let mut provider = FakeProvider::new([
            ProviderTurnEvent::TextDelta {
                text: "hello".to_string(),
            },
            ProviderTurnEvent::TextDelta {
                text: " world".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ]);
        let runtime = AgentRuntime::new();

        let response = runtime
            .run_text_turn(&mut provider, AgentTurnRequest::new("test-model", "hello"))
            .await
            .expect("turn should finish");

        assert_eq!(response.text, "hello world");
        assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
        assert!(provider.finished);
    }

    #[tokio::test]
    async fn streaming_text_turn_emits_deltas_and_final_response() {
        let provider = FakeProvider::new([
            ProviderTurnEvent::TextDelta {
                text: "hello".to_string(),
            },
            ProviderTurnEvent::ReasoningDelta {
                text: "thinking".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ]);
        let runtime = AgentRuntime::new();
        let mut stream =
            runtime.run_streaming_text_turn(provider, AgentTurnRequest::new("test-model", "hello"));
        let mut text_delta = None;
        let mut reasoning_delta = None;
        let mut final_text = None;

        while let Some(item) = stream.next().await {
            match item {
                AgentRuntimeStreamItem::Event(AgentRuntimeEvent::TextDelta(text)) => {
                    text_delta = Some(text);
                }
                AgentRuntimeStreamItem::Event(AgentRuntimeEvent::ReasoningDelta(text)) => {
                    reasoning_delta = Some(text);
                }
                AgentRuntimeStreamItem::Finished(response) => {
                    final_text = Some(response.text);
                    break;
                }
                AgentRuntimeStreamItem::Error(error) => panic!("unexpected stream error: {error}"),
                AgentRuntimeStreamItem::Event(_) => {}
            }
        }

        assert_eq!(text_delta.as_deref(), Some("hello"));
        assert_eq!(reasoning_delta.as_deref(), Some("thinking"));
        assert_eq!(final_text.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn streaming_text_turn_preserves_provider_metadata_events() {
        let provider = FakeProvider::new([
            ProviderTurnEvent::RequestProjection {
                projection: ProviderRequestProjection {
                    provider: Some("example-provider".to_string()),
                    ..ProviderRequestProjection::default()
                },
            },
            ProviderTurnEvent::ProviderMetadata {
                key: "conversation".to_string(),
                value: "reused".to_string(),
            },
            ProviderTurnEvent::RetryScheduled {
                message: "retrying".to_string(),
                retry_at_unix: 42,
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ]);
        let runtime = AgentRuntime::new();
        let mut stream =
            runtime.run_streaming_text_turn(provider, AgentTurnRequest::new("test-model", "hello"));
        let mut saw_projection = false;
        let mut saw_metadata = false;
        let mut saw_retry = false;

        while let Some(item) = stream.next().await {
            match item {
                AgentRuntimeStreamItem::Event(AgentRuntimeEvent::RequestProjection(projection)) => {
                    saw_projection = projection.provider.as_deref() == Some("example-provider");
                }
                AgentRuntimeStreamItem::Event(AgentRuntimeEvent::ProviderMetadata {
                    key,
                    value,
                }) => {
                    saw_metadata = key == "conversation" && value == "reused";
                }
                AgentRuntimeStreamItem::Event(AgentRuntimeEvent::RetryScheduled {
                    message,
                    retry_at_unix,
                }) => {
                    saw_retry = message == "retrying" && retry_at_unix == 42;
                }
                AgentRuntimeStreamItem::Finished(_) => break,
                AgentRuntimeStreamItem::Error(error) => panic!("unexpected stream error: {error}"),
                AgentRuntimeStreamItem::Event(_) => {}
            }
        }

        assert!(saw_projection);
        assert!(saw_metadata);
        assert!(saw_retry);
    }

    #[tokio::test]
    async fn newer_runtime_text_turn_supersedes_blocked_older_turn() {
        let runtime = AgentRuntime::new().with_poll_interval(Duration::from_millis(1));
        let mut first = runtime.run_streaming_text_turn(
            FakeProvider::new([]),
            AgentTurnRequest::new("test-model", "first"),
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
        let mut second = runtime.run_streaming_text_turn(
            FakeProvider::new([ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            }]),
            AgentTurnRequest::new("test-model", "second"),
        );

        let first_terminal = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(item) = first.next().await {
                if matches!(item, AgentRuntimeStreamItem::Error(RuntimeError::Cancelled)) {
                    return true;
                }
            }
            false
        })
        .await
        .expect("superseded turn should terminate");
        let second_terminal = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(item) = second.next().await {
                if matches!(item, AgentRuntimeStreamItem::Finished(_)) {
                    return true;
                }
            }
            false
        })
        .await
        .expect("newer turn should complete");

        assert!(first_terminal);
        assert!(second_terminal);
        assert_eq!(runtime.active_turn_generation(), None);
    }

    #[tokio::test]
    async fn streaming_turn_uses_explicit_cancellation_token() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut request = AgentTurnRequest::new("test-model", "hello");
        request.cancellation = cancellation;
        let runtime = AgentRuntime::new();
        let mut stream = runtime.run_streaming_text_turn(FakeProvider::new([]), request);

        let mut cancelled = false;
        while let Some(item) = stream.next().await {
            if matches!(item, AgentRuntimeStreamItem::Error(RuntimeError::Cancelled)) {
                cancelled = true;
                break;
            }
        }

        assert!(cancelled);
    }
    struct BlockingPollProvider;

    impl ModelProviderInvoker for BlockingPollProvider {
        fn start_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a ModelTurnRequest,
        ) -> RuntimeFuture<'a, StartTurnResponse> {
            Box::pin(async {
                Ok(StartTurnResponse {
                    provider_turn_id: "blocked".to_string(),
                })
            })
        }

        fn poll_turn_events<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a PollTurnEventsRequest,
        ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
            Box::pin(std::future::pending())
        }

        fn cancel_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a CancelTurnRequest,
        ) -> RuntimeFuture<'a, AckResponse> {
            Box::pin(async { Ok(AckResponse::default()) })
        }

        fn finish_turn<'a>(
            &'a mut self,
            _provider_plugin_id: Option<&'a str>,
            _request: &'a FinishTurnRequest,
        ) -> RuntimeFuture<'a, AckResponse> {
            Box::pin(async { Ok(AckResponse::default()) })
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_provider_poll() {
        let cancellation = CancellationToken::new();
        let mut request = AgentTurnRequest::new("test-model", "hello");
        request.cancellation = cancellation.clone();
        let runtime = AgentRuntime::new();
        let mut stream = runtime.run_streaming_text_turn(BlockingPollProvider, request);

        cancellation.cancel();
        let cancelled = tokio::time::timeout(Duration::from_millis(100), async {
            while let Some(item) = stream.next().await {
                if matches!(item, AgentRuntimeStreamItem::Error(RuntimeError::Cancelled)) {
                    return true;
                }
            }
            false
        })
        .await
        .expect("cancellation should interrupt a blocked provider poll");

        assert!(cancelled);
    }
}
