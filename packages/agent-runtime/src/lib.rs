#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Reusable agent turn runtime for Bcode SDK, daemon, and product surfaces.
//!
//! This crate owns the provider/tool/policy boundary for a single agent turn without depending on
//! daemon IPC or TUI code. Higher-level crates supply concrete provider, tool, and permission
//! implementations.

use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MessageRole, ModelMessage,
    ModelParameters, ModelTurnRequest, PollTurnEventsRequest, PollTurnEventsResponse,
    ProviderError, ProviderRequestContext, ProviderRequestProjection, ProviderTurnEvent,
    StartTurnResponse, StopReason, TokenUsage, ToolCall,
};
use bcode_session_models::SessionId;
use bcode_tool::ToolDefinition;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;

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
    /// A tool was requested but no executor could handle it.
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    /// Tool execution was denied by policy.
    #[error("tool execution denied: {0}")]
    PermissionDenied(String),
    /// The runtime reached its configured maximum tool rounds.
    #[error("maximum tool rounds reached: {0}")]
    MaxToolRounds(u32),
}

/// Cancellation state shared between callers and a running turn.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Create a new uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark this token as cancelled.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Return whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
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
    /// User prompt for this turn.
    pub prompt: String,
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
            prompt: prompt.into(),
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
#[derive(Debug, Clone)]
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
    /// Token usage snapshot.
    Usage(TokenUsage),
    /// Provider reported actual request projection metadata.
    RequestProjection(ProviderRequestProjection),
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
    receiver: mpsc::Receiver<AgentRuntimeStreamItem>,
}

impl AgentRuntimeStream {
    /// Receive the next stream item.
    pub async fn next(&mut self) -> Option<AgentRuntimeStreamItem> {
        self.receiver.recv().await
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

/// Tool catalog visible to the runtime.
pub trait ToolCatalog: Send + Sync {
    /// Return model-callable tool definitions.
    fn definitions(&self) -> Vec<ToolDefinition>;
}

/// Empty tool catalog for stateless turns without tools.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyToolCatalog;

impl ToolCatalog for EmptyToolCatalog {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
}

/// Tool permission decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Permit tool execution.
    Allow,
    /// Deny tool execution with a reason.
    Deny(String),
}

/// Tool permission hook used before sensitive execution.
pub trait PermissionPolicy: Send + Sync {
    /// Evaluate one requested tool call.
    fn evaluate_tool_call<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> RuntimeFuture<'a, PermissionDecision>;
}

/// Permission policy that allows every tool call.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllPolicy;

impl PermissionPolicy for AllowAllPolicy {
    fn evaluate_tool_call<'a>(
        &'a self,
        _call: &'a ToolCall,
    ) -> RuntimeFuture<'a, PermissionDecision> {
        Box::pin(async { Ok(PermissionDecision::Allow) })
    }
}

/// Reusable runtime for one or more agent turns.
#[derive(Debug, Clone)]
pub struct AgentRuntime {
    poll_interval: Duration,
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(50),
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
        let (sender, receiver) = mpsc::channel(64);
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
            let _ = sender.send(item).await;
        });
        AgentRuntimeStream { receiver }
    }

    async fn run_text_turn_internal<P>(
        &self,
        provider: &mut P,
        request: AgentTurnRequest,
        stream: Option<&mpsc::Sender<AgentRuntimeStreamItem>>,
    ) -> Result<AgentTurnResponse>
    where
        P: ModelProviderInvoker,
    {
        let start = Instant::now();
        let model_request = model_turn_request(&request);
        let provider_plugin_id = request.provider_plugin_id.as_deref();
        let start_response = provider
            .start_turn(provider_plugin_id, &model_request)
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

        let turn_started = AgentRuntimeEvent::TurnStarted;
        if !emit_stream_event(stream, &turn_started).await {
            cancel_and_finish(
                provider,
                provider_plugin_id,
                &cancel_request,
                &finish_request,
            )
            .await;
            return Err(RuntimeError::Cancelled);
        }
        events.push(turn_started);

        loop {
            if let Some(error) = terminal_control_error(
                provider,
                provider_plugin_id,
                &cancel_request,
                &finish_request,
                &request,
                start,
            )
            .await
            {
                return Err(error);
            }

            let poll = provider
                .poll_turn_events(provider_plugin_id, &poll_request)
                .await?;
            let should_sleep = poll.events.is_empty();
            for event in poll.events {
                match normalize_provider_event(event, &mut text, &mut usage)? {
                    EventDisposition::Continue(event) => {
                        if !emit_stream_event(stream, &event).await {
                            cancel_and_finish(
                                provider,
                                provider_plugin_id,
                                &cancel_request,
                                &finish_request,
                            )
                            .await;
                            return Err(RuntimeError::Cancelled);
                        }
                        events.push(event);
                    }
                    EventDisposition::Finished { stop_reason } => {
                        provider
                            .finish_turn(provider_plugin_id, &finish_request)
                            .await?;
                        let finished_event =
                            finished_event(usage.as_ref(), start.elapsed(), stop_reason);
                        if !emit_stream_event(stream, &finished_event).await {
                            return Err(RuntimeError::Cancelled);
                        }
                        events.push(finished_event);
                        return Ok(AgentTurnResponse {
                            text,
                            stop_reason: Some(stop_reason),
                            usage,
                            latency_ms: start.elapsed().as_millis(),
                            events,
                        });
                    }
                    EventDisposition::Cancelled(event) => {
                        if !emit_stream_event(stream, &event).await {
                            return Err(RuntimeError::Cancelled);
                        }
                        events.push(event);
                        provider
                            .finish_turn(provider_plugin_id, &finish_request)
                            .await?;
                        return Err(RuntimeError::Cancelled);
                    }
                }
            }
            if should_sleep {
                tokio::time::sleep(self.poll_interval).await;
            }
        }
    }
}

enum EventDisposition {
    Continue(AgentRuntimeEvent),
    Finished { stop_reason: StopReason },
    Cancelled(AgentRuntimeEvent),
}

async fn emit_stream_event(
    stream: Option<&mpsc::Sender<AgentRuntimeStreamItem>>,
    event: &AgentRuntimeEvent,
) -> bool {
    let Some(stream) = stream else {
        return true;
    };
    stream
        .send(AgentRuntimeStreamItem::Event(event.clone()))
        .await
        .is_ok()
}

async fn terminal_control_error<P>(
    provider: &mut P,
    provider_plugin_id: Option<&str>,
    cancel_request: &CancelTurnRequest,
    finish_request: &FinishTurnRequest,
    request: &AgentTurnRequest,
    start: Instant,
) -> Option<RuntimeError>
where
    P: ModelProviderInvoker,
{
    if request.cancellation.is_cancelled() {
        cancel_and_finish(provider, provider_plugin_id, cancel_request, finish_request).await;
        return Some(RuntimeError::Cancelled);
    }
    if start.elapsed() >= request.timeout {
        cancel_and_finish(provider, provider_plugin_id, cancel_request, finish_request).await;
        return Some(RuntimeError::Timeout {
            timeout: request.timeout,
        });
    }
    None
}

async fn cancel_and_finish<P>(
    provider: &mut P,
    provider_plugin_id: Option<&str>,
    cancel_request: &CancelTurnRequest,
    finish_request: &FinishTurnRequest,
) where
    P: ModelProviderInvoker,
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

fn model_turn_request(request: &AgentTurnRequest) -> ModelTurnRequest {
    let session_id = SessionId::new();
    ModelTurnRequest {
        session_id,
        turn_id: format!("sdk-turn-{session_id}"),
        model_id: request.model_id.clone(),
        provider_context: request.provider_context.clone(),
        system_prompt: request.system_prompt.clone(),
        messages: vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: request.prompt.clone(),
            }],
        }],
        tools: Vec::new(),
        parameters: request.parameters.clone(),
        prompt_cache: bcode_model::PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: request.metadata.clone(),
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
        ProviderTurnEvent::RequestProjection { projection } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::RequestProjection(projection),
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
    use std::collections::VecDeque;

    struct FakeProvider {
        events: VecDeque<ProviderTurnEvent>,
        finished: bool,
    }

    impl FakeProvider {
        fn new(events: impl IntoIterator<Item = ProviderTurnEvent>) -> Self {
            Self {
                events: events.into_iter().collect(),
                finished: false,
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
}
