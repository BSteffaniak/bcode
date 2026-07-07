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
    PromptCacheHints, ProviderRequestContext, ProviderTurnEvent, StartTurnResponse, StopReason,
    TokenUsage, ToolCall,
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
    },
    /// Turn was cancelled.
    Cancelled,
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

        loop {
            if request.cancellation.is_cancelled() {
                let _ = provider
                    .cancel_turn(provider_plugin_id, &cancel_request)
                    .await;
                let _ = provider
                    .finish_turn(provider_plugin_id, &finish_request)
                    .await;
                return Err(RuntimeError::Cancelled);
            }
            if start.elapsed() >= request.timeout {
                let _ = provider
                    .cancel_turn(provider_plugin_id, &cancel_request)
                    .await;
                let _ = provider
                    .finish_turn(provider_plugin_id, &finish_request)
                    .await;
                return Err(RuntimeError::Timeout {
                    timeout: request.timeout,
                });
            }

            let poll = provider
                .poll_turn_events(provider_plugin_id, &poll_request)
                .await?;
            let mut should_sleep = poll.events.is_empty();
            for event in poll.events {
                should_sleep = false;
                match normalize_provider_event(event)? {
                    EventDisposition::Continue(event) => {
                        if let AgentRuntimeEvent::TextDelta(delta) = &event {
                            text.push_str(delta);
                        }
                        if let AgentRuntimeEvent::Usage(event_usage) = &event {
                            usage = Some(event_usage.clone());
                        }
                        events.push(event);
                    }
                    EventDisposition::Finished { event, stop_reason } => {
                        events.push(event);
                        provider
                            .finish_turn(provider_plugin_id, &finish_request)
                            .await?;
                        return Ok(AgentTurnResponse {
                            text,
                            stop_reason: Some(stop_reason),
                            usage,
                            latency_ms: start.elapsed().as_millis(),
                            events,
                        });
                    }
                    EventDisposition::Cancelled(event) => {
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
    Finished {
        event: AgentRuntimeEvent,
        stop_reason: StopReason,
    },
    Cancelled(AgentRuntimeEvent),
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
        prompt_cache: PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: request.metadata.clone(),
    }
}

fn normalize_provider_event(event: ProviderTurnEvent) -> Result<EventDisposition> {
    match event {
        ProviderTurnEvent::TurnStarted => {
            Ok(EventDisposition::Continue(AgentRuntimeEvent::TurnStarted))
        }
        ProviderTurnEvent::TextDelta { text } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::TextDelta(text),
        )),
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
            Ok(EventDisposition::Continue(AgentRuntimeEvent::Usage(usage)))
        }
        ProviderTurnEvent::RequestProjection { .. }
        | ProviderTurnEvent::ProviderMetadata { .. }
        | ProviderTurnEvent::RetryScheduled { .. } => {
            Ok(EventDisposition::Continue(AgentRuntimeEvent::Warning(
                "provider metadata event ignored by text runtime".to_string(),
            )))
        }
        ProviderTurnEvent::Warning { message } => Ok(EventDisposition::Continue(
            AgentRuntimeEvent::Warning(message),
        )),
        ProviderTurnEvent::Error { error } => Err(RuntimeError::Provider {
            code: error.code,
            message: error.message,
        }),
        ProviderTurnEvent::TurnFinished { stop_reason } => Ok(EventDisposition::Finished {
            event: AgentRuntimeEvent::Finished { stop_reason },
            stop_reason,
        }),
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
    async fn cancelled_provider_event_returns_cancelled_error() {
        let mut provider = FakeProvider::new([ProviderTurnEvent::Cancelled]);
        let runtime = AgentRuntime::new();

        let error = runtime
            .run_text_turn(&mut provider, AgentTurnRequest::new("test-model", "hello"))
            .await
            .expect_err("turn should cancel");

        assert!(matches!(error, RuntimeError::Cancelled));
        assert!(provider.finished);
    }
}
