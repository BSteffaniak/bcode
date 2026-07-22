//! Ergonomic in-process provider extension boundary.

use crate::{CancellationToken, ModelProviderInvoker, RuntimeFuture};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderError, ProviderErrorCategory, ProviderTurnEvent,
    StartTurnResponse, StopReason,
};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Boxed future returned by an [`InProcessModelProvider`].
pub type InProcessProviderFuture<'a> = Pin<
    Box<
        dyn Future<Output = std::result::Result<InProcessProviderOutcome, ProviderError>>
            + Send
            + 'a,
    >,
>;

/// Result of one in-process provider round.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProcessProviderOutcome {
    /// The model produced a complete assistant response.
    EndTurn,
    /// The model produced one or more complete tool calls.
    ToolCall,
    /// Generation stopped at the provider's output-token limit.
    MaxTokens,
    /// Generation stopped at a configured/provider stop sequence.
    StopSequence,
}

impl InProcessProviderOutcome {
    const fn stop_reason(self) -> StopReason {
        match self {
            Self::EndTurn => StopReason::EndTurn,
            Self::ToolCall => StopReason::ToolCall,
            Self::MaxTokens => StopReason::MaxTokens,
            Self::StopSequence => StopReason::StopSequence,
        }
    }
}

/// Context supplied to one in-process provider round.
#[derive(Debug, Clone)]
pub struct InProcessProviderContext {
    events: InProcessProviderEventSink,
    cancellation: CancellationToken,
}

impl InProcessProviderContext {
    /// Return the ordered event sink for this round.
    #[must_use]
    pub const fn events(&self) -> &InProcessProviderEventSink {
        &self.events
    }

    /// Return cancellation state for the complete provider round.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

/// Cloneable ordered event sink for an in-process provider round.
#[derive(Debug, Clone)]
pub struct InProcessProviderEventSink {
    state: Arc<InProcessTurnState>,
}

impl InProcessProviderEventSink {
    /// Emit one normalized nonterminal provider event.
    ///
    /// Lifecycle events are adapter-owned. Providers must return an
    /// [`InProcessProviderOutcome`] or `ProviderError` instead of emitting `TurnStarted`,
    /// `TurnFinished`, `Cancelled`, or `Error` directly.
    ///
    /// # Errors
    ///
    /// Returns an error when the event is adapter-owned or the turn is already terminal.
    pub fn emit(&self, event: ProviderTurnEvent) -> Result<(), InProcessProviderEmitError> {
        if is_adapter_owned_event(&event) {
            return Err(InProcessProviderEmitError::AdapterOwnedEvent);
        }
        self.state.push_nonterminal(event)
    }
}

/// Error returned while emitting an in-process provider event.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InProcessProviderEmitError {
    /// The event is owned by [`InProcessModelProviderAdapter`].
    #[error("provider lifecycle events are owned by the in-process adapter")]
    AdapterOwnedEvent,
    /// The provider round has already reached a terminal state.
    #[error("in-process provider turn is already finished")]
    TurnFinished,
}

/// Minimal extension trait for application-defined in-process model providers.
///
/// Implementations receive the complete provider-neutral request, emit normalized nonterminal
/// events, and return one normal stop outcome or a structured provider error. The adapter owns the
/// polling lifecycle, cancellation races, and cleanup required by [`ModelProviderInvoker`].
pub trait InProcessModelProvider: Send + Sync + 'static {
    /// Run one provider round.
    fn run_turn(
        &self,
        request: ModelTurnRequest,
        context: InProcessProviderContext,
    ) -> InProcessProviderFuture<'_>;
}

/// Adapt an [`InProcessModelProvider`] to the canonical [`ModelProviderInvoker`] boundary.
#[derive(Debug)]
pub struct InProcessModelProviderAdapter<P> {
    provider: Arc<P>,
    next_turn: AtomicU64,
    turns: Arc<Mutex<BTreeMap<String, Arc<InProcessTurnState>>>>,
}

impl<P> InProcessModelProviderAdapter<P>
where
    P: InProcessModelProvider,
{
    /// Create an adapter around an application-defined provider.
    #[must_use]
    pub fn new(provider: P) -> Self {
        Self {
            provider: Arc::new(provider),
            next_turn: AtomicU64::new(0),
            turns: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn turn_id(&self) -> String {
        let sequence = self.next_turn.fetch_add(1, Ordering::Relaxed) + 1;
        format!("in-process-turn-{sequence}")
    }
}

impl<P> ModelProviderInvoker for InProcessModelProviderAdapter<P>
where
    P: InProcessModelProvider,
{
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        let provider_turn_id = self.turn_id();
        let state = Arc::new(InProcessTurnState::new());
        state.push(ProviderTurnEvent::TurnStarted);
        self.turns
            .lock()
            .expect("in-process provider turn lock should not be poisoned")
            .insert(provider_turn_id.clone(), Arc::clone(&state));
        let provider = Arc::clone(&self.provider);
        let request = request.clone();
        tokio::spawn(async move {
            let context = InProcessProviderContext {
                events: InProcessProviderEventSink {
                    state: Arc::clone(&state),
                },
                cancellation: state.cancellation.clone(),
            };
            tokio::select! {
                biased;
                () = state.cancellation.cancelled() => state.finish_cancelled(),
                result = provider.run_turn(request, context) => match result {
                    Ok(outcome) => state.finish(outcome.stop_reason()),
                    Err(error) => state.finish_error(error),
                },
            }
        });
        Box::pin(async move { Ok(StartTurnResponse { provider_turn_id }) })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        let events = self
            .turns
            .lock()
            .expect("in-process provider turn lock should not be poisoned")
            .get(&request.provider_turn_id)
            .map_or_else(Vec::new, |state| state.drain());
        Box::pin(async move { Ok(PollTurnEventsResponse { events }) })
    }

    fn cancel_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        if let Some(state) = self
            .turns
            .lock()
            .expect("in-process provider turn lock should not be poisoned")
            .get(&request.provider_turn_id)
        {
            state.cancellation.cancel();
        }
        Box::pin(async { Ok(AckResponse::default()) })
    }

    fn finish_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        let state = self
            .turns
            .lock()
            .expect("in-process provider turn lock should not be poisoned")
            .remove(&request.provider_turn_id);
        if let Some(state) = state {
            state.cancellation.cancel();
        }
        Box::pin(async { Ok(AckResponse::default()) })
    }
}

impl<P> Drop for InProcessModelProviderAdapter<P> {
    fn drop(&mut self) {
        let turns = self
            .turns
            .lock()
            .expect("in-process provider turn lock should not be poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for turn in turns {
            turn.cancellation.cancel();
        }
    }
}

impl<P> From<P> for InProcessModelProviderAdapter<P>
where
    P: InProcessModelProvider,
{
    fn from(provider: P) -> Self {
        Self::new(provider)
    }
}

#[derive(Debug)]
struct InProcessTurnState {
    events: Mutex<VecDeque<ProviderTurnEvent>>,
    cancellation: CancellationToken,
    terminal: AtomicBool,
}

impl InProcessTurnState {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            cancellation: CancellationToken::new(),
            terminal: AtomicBool::new(false),
        }
    }

    fn push(&self, event: ProviderTurnEvent) {
        self.events
            .lock()
            .expect("in-process provider event lock should not be poisoned")
            .push_back(event);
    }

    fn push_nonterminal(&self, event: ProviderTurnEvent) -> Result<(), InProcessProviderEmitError> {
        let mut events = self
            .events
            .lock()
            .expect("in-process provider event lock should not be poisoned");
        if self.terminal.load(Ordering::Acquire) {
            drop(events);
            return Err(InProcessProviderEmitError::TurnFinished);
        }
        events.push_back(event);
        drop(events);
        Ok(())
    }

    fn drain(&self) -> Vec<ProviderTurnEvent> {
        self.events
            .lock()
            .expect("in-process provider event lock should not be poisoned")
            .drain(..)
            .collect()
    }

    fn begin_finish(&self) -> bool {
        self.terminal
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn finish(&self, stop_reason: StopReason) {
        if self.begin_finish() {
            self.push(ProviderTurnEvent::TurnFinished { stop_reason });
        }
    }

    fn finish_error(&self, error: ProviderError) {
        if self.begin_finish() {
            self.push(ProviderTurnEvent::Error { error });
            self.push(ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Error,
            });
        }
    }

    fn finish_cancelled(&self) {
        if self.begin_finish() {
            self.push(ProviderTurnEvent::Cancelled);
            self.push(ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Cancelled,
            });
        }
    }
}

const fn is_adapter_owned_event(event: &ProviderTurnEvent) -> bool {
    matches!(
        event,
        ProviderTurnEvent::TurnStarted
            | ProviderTurnEvent::TurnFinished { .. }
            | ProviderTurnEvent::Cancelled
            | ProviderTurnEvent::Error { .. }
    )
}

/// Construct a non-retryable in-process provider error.
#[must_use]
pub fn in_process_provider_error(
    code: impl Into<String>,
    category: ProviderErrorCategory,
    message: impl Into<String>,
) -> ProviderError {
    ProviderError {
        code: code.into(),
        category,
        message: message.into(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentRuntime, AgentTurnRequest, RuntimeError};
    use bcode_model::{TokenUsage, ToolCall};
    use std::time::Duration;

    #[derive(Debug)]
    struct EchoProvider;

    impl InProcessModelProvider for EchoProvider {
        fn run_turn(
            &self,
            _request: ModelTurnRequest,
            context: InProcessProviderContext,
        ) -> InProcessProviderFuture<'_> {
            Box::pin(async move {
                context
                    .events()
                    .emit(ProviderTurnEvent::TextDelta {
                        text: "hello from custom provider".to_string(),
                    })
                    .expect("emit text");
                context
                    .events()
                    .emit(ProviderTurnEvent::Usage {
                        usage: TokenUsage {
                            input_tokens: Some(2),
                            output_tokens: Some(4),
                            total_tokens: Some(6),
                            ..TokenUsage::default()
                        },
                    })
                    .expect("emit usage");
                Ok(InProcessProviderOutcome::EndTurn)
            })
        }
    }

    #[tokio::test]
    async fn adapter_runs_custom_provider_through_canonical_runtime() {
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider);
        let response = AgentRuntime::new()
            .run_text_turn(&mut provider, AgentTurnRequest::new("model", "hello"))
            .await
            .expect("custom provider turn");

        assert_eq!(response.text, "hello from custom provider");
        assert_eq!(
            response.usage.expect("provider usage").total_tokens,
            Some(6)
        );
    }

    #[derive(Debug)]
    struct ToolProvider;

    impl InProcessModelProvider for ToolProvider {
        fn run_turn(
            &self,
            _request: ModelTurnRequest,
            context: InProcessProviderContext,
        ) -> InProcessProviderFuture<'_> {
            Box::pin(async move {
                let call = ToolCall {
                    id: "call-1".to_string(),
                    name: "custom.tool".to_string(),
                    arguments: serde_json::json!({}),
                };
                context
                    .events()
                    .emit(ProviderTurnEvent::ToolCallStarted {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                    })
                    .expect("emit tool start");
                context
                    .events()
                    .emit(ProviderTurnEvent::ToolCallFinished { call })
                    .expect("emit tool completion");
                context
                    .events()
                    .emit(ProviderTurnEvent::Usage {
                        usage: TokenUsage::default(),
                    })
                    .expect("emit usage");
                Ok(InProcessProviderOutcome::ToolCall)
            })
        }
    }

    #[tokio::test]
    async fn adapter_preserves_tool_calls_for_canonical_orchestration() {
        let mut provider = InProcessModelProviderAdapter::new(ToolProvider);
        let response = AgentRuntime::new()
            .run_text_turn(&mut provider, AgentTurnRequest::new("model", "tool"))
            .await
            .expect("custom provider tool turn");

        assert_eq!(response.stop_reason, Some(StopReason::ToolCall));
        let calls = response
            .events
            .iter()
            .filter_map(|event| match event {
                crate::AgentRuntimeEvent::ToolCallFinished(call) => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "custom.tool");
    }

    #[derive(Debug)]
    struct ErrorProvider;

    impl InProcessModelProvider for ErrorProvider {
        fn run_turn(
            &self,
            _request: ModelTurnRequest,
            _context: InProcessProviderContext,
        ) -> InProcessProviderFuture<'_> {
            Box::pin(async {
                Err(in_process_provider_error(
                    "custom_failure",
                    ProviderErrorCategory::ProviderInternal,
                    "custom provider failed",
                ))
            })
        }
    }

    #[tokio::test]
    async fn adapter_preserves_structured_provider_errors() {
        let mut provider = InProcessModelProviderAdapter::new(ErrorProvider);
        let error = AgentRuntime::new()
            .run_text_turn(&mut provider, AgentTurnRequest::new("model", "fail"))
            .await
            .expect_err("custom provider error must remain terminal");

        assert!(matches!(
            error,
            RuntimeError::Provider { code, message, .. }
                if code == "custom_failure" && message == "custom provider failed"
        ));
    }

    #[derive(Debug)]
    struct BlockingProvider;

    impl InProcessModelProvider for BlockingProvider {
        fn run_turn(
            &self,
            _request: ModelTurnRequest,
            context: InProcessProviderContext,
        ) -> InProcessProviderFuture<'_> {
            Box::pin(async move {
                context.cancellation().cancelled().await;
                Ok(InProcessProviderOutcome::EndTurn)
            })
        }
    }

    #[tokio::test]
    async fn runtime_timeout_cancels_and_finishes_custom_provider() {
        let mut provider = InProcessModelProviderAdapter::new(BlockingProvider);
        let mut request = AgentTurnRequest::new("model", "wait");
        request.timeout = Duration::from_millis(10);

        let error = AgentRuntime::new()
            .run_text_turn(&mut provider, request)
            .await
            .expect_err("blocking provider must time out");

        assert!(matches!(error, RuntimeError::Timeout { .. }));
        assert!(
            provider.turns.lock().expect("turns").is_empty(),
            "finish_turn must release custom provider state"
        );
    }

    #[tokio::test]
    async fn dropping_adapter_cancels_active_custom_provider_work() {
        let mut adapter = InProcessModelProviderAdapter::new(BlockingProvider);
        let started = adapter
            .start_turn(None, &model_request("drop"))
            .await
            .expect("start custom provider");
        let cancellation = adapter
            .turns
            .lock()
            .expect("turns")
            .get(&started.provider_turn_id)
            .expect("active turn")
            .cancellation
            .clone();

        drop(adapter);

        assert!(cancellation.is_cancelled());
    }

    fn model_request(turn_id: &str) -> ModelTurnRequest {
        ModelTurnRequest {
            session_id: bcode_session_models::SessionId::new(),
            turn_id: turn_id.to_string(),
            model_id: "model".to_string(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
            parameters: bcode_model::ModelParameters::default(),
            structured_output: None,
            context_management: bcode_model::ContextManagementRequest::default(),
            prompt_cache: bcode_model::PromptCacheHints::default(),
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn sink_rejects_events_after_adapter_finishes_turn() {
        let state = Arc::new(InProcessTurnState::new());
        let sink = InProcessProviderEventSink {
            state: Arc::clone(&state),
        };
        state.finish(StopReason::EndTurn);

        assert_eq!(
            sink.emit(ProviderTurnEvent::TextDelta {
                text: "late".to_string(),
            }),
            Err(InProcessProviderEmitError::TurnFinished)
        );
        assert!(matches!(
            state.drain().as_slice(),
            [ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn
            }]
        ));
    }

    #[test]
    fn sink_rejects_adapter_owned_lifecycle_events() {
        let state = Arc::new(InProcessTurnState::new());
        let sink = InProcessProviderEventSink { state };

        assert_eq!(
            sink.emit(ProviderTurnEvent::TurnStarted),
            Err(InProcessProviderEmitError::AdapterOwnedEvent)
        );
    }
}
