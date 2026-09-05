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
        Box::pin(async move {
            let provider_turn_id = self.turn_id();
            let state = Arc::new(InProcessTurnState::new());
            state.push(ProviderTurnEvent::TurnStarted);
            self.turns
                .lock()
                .expect("in-process provider turn lock should not be poisoned")
                .insert(provider_turn_id.clone(), Arc::clone(&state));
            let provider = Arc::clone(&self.provider);
            let request = request.clone();
            let worker = InProcessWorkerGuard(Arc::clone(&state));
            switchy::unsync::task::spawn(async move {
                let _worker = worker;
                let context = InProcessProviderContext {
                    events: InProcessProviderEventSink {
                        state: Arc::clone(&state),
                    },
                    cancellation: state.cancellation.clone(),
                };
                switchy::unsync::select! {
                    biased;
                    () = state.cancellation.cancelled() => state.finish_cancelled(),
                    result = async { provider.run_turn(request, context).await } => match result {
                        Ok(outcome) => state.finish(outcome.stop_reason()),
                        Err(error) => state.finish_error(error),
                    },
                }
            });
            Ok(StartTurnResponse { provider_turn_id })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            let events = self
                .turns
                .lock()
                .expect("in-process provider turn lock should not be poisoned")
                .get(&request.provider_turn_id)
                .ok_or_else(|| {
                    crate::RuntimeError::ProviderInvocation(
                        "in-process provider turn is unknown or released".into(),
                    )
                })?
                .drain();
            Ok(PollTurnEventsResponse { events })
        })
    }

    fn cancel_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        let state = self
            .turns
            .lock()
            .expect("in-process provider turn lock should not be poisoned")
            .get(&request.provider_turn_id)
            .cloned();
        if let Some(state) = state {
            state.finish_cancelled();
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
            state.finish_cancelled();
            state.cancellation.cancel();
        }
        Box::pin(async { Ok(AckResponse::default()) })
    }
}

impl<P> Drop for InProcessModelProviderAdapter<P> {
    fn drop(&mut self) {
        let turns = std::mem::take(
            &mut *self
                .turns
                .lock()
                .expect("in-process provider turn lock should not be poisoned"),
        );
        for turn in turns.into_values() {
            turn.finish_cancelled();
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

struct InProcessWorkerGuard(Arc<InProcessTurnState>);

impl Drop for InProcessWorkerGuard {
    fn drop(&mut self) {
        self.0.finish_error(in_process_provider_error(
            "in_process_worker_stopped",
            ProviderErrorCategory::ProviderInternal,
            "in-process provider worker stopped before completing its turn",
        ));
        self.0.cancellation.cancel();
    }
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

    fn finish_events<const N: usize>(&self, terminal_events: [ProviderTurnEvent; N]) {
        let mut events = self
            .events
            .lock()
            .expect("in-process provider event lock should not be poisoned");
        if self.begin_finish() {
            events.extend(terminal_events);
        }
    }

    fn finish(&self, stop_reason: StopReason) {
        self.finish_events([ProviderTurnEvent::TurnFinished { stop_reason }]);
    }

    fn finish_error(&self, error: ProviderError) {
        self.finish_events([
            ProviderTurnEvent::Error { error },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Error,
            },
        ]);
    }

    fn finish_cancelled(&self) {
        self.finish_events([
            ProviderTurnEvent::Cancelled,
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Cancelled,
            },
        ]);
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

    #[test]
    fn abandoned_worker_cancels_retained_context_and_terminalizes_once() {
        let state = Arc::new(InProcessTurnState::new());
        let cancellation = state.cancellation.clone();
        drop(InProcessWorkerGuard(state.clone()));
        assert!(cancellation.is_cancelled());
        let events = state.drain();
        assert!(
            matches!(events.as_slice(), [ProviderTurnEvent::Error { error },
            ProviderTurnEvent::TurnFinished { stop_reason: StopReason::Error }]
            if error.code == "in_process_worker_stopped" && !error.retryable)
        );
        drop(InProcessWorkerGuard(state.clone()));
        assert!(state.drain().is_empty());
    }

    #[derive(Debug)]
    struct ResourceProvider {
        started: Arc<AtomicBool>,
        released: Arc<AtomicBool>,
    }

    impl InProcessModelProvider for ResourceProvider {
        fn run_turn(
            &self,
            _request: ModelTurnRequest,
            _context: InProcessProviderContext,
        ) -> InProcessProviderFuture<'_> {
            struct Resource(Arc<AtomicBool>);
            impl Drop for Resource {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            Box::pin(async move {
                let _resource = Resource(self.released.clone());
                self.started.store(true, Ordering::Release);
                std::future::pending().await
            })
        }
    }

    #[tokio::test]
    async fn cancelling_polled_worker_drops_provider_future_resource() {
        let started = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let mut adapter = InProcessModelProviderAdapter::new(ResourceProvider {
            started: started.clone(),
            released: released.clone(),
        });
        let turn = adapter
            .start_turn(None, &model_request("resource"))
            .await
            .expect("start");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider polled");
        assert!(!released.load(Ordering::Acquire));
        adapter
            .cancel_turn(
                None,
                &CancelTurnRequest {
                    provider_turn_id: turn.provider_turn_id.clone(),
                },
            )
            .await
            .expect("cancel");
        adapter
            .finish_turn(
                None,
                &FinishTurnRequest {
                    provider_turn_id: turn.provider_turn_id,
                },
            )
            .await
            .expect("finish");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !released.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider future resource released");
        assert!(adapter.turns.lock().expect("turns").is_empty());
    }

    #[tokio::test]
    async fn dropping_adapter_releases_polled_provider_future_resource() {
        let started = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let mut adapter = InProcessModelProviderAdapter::new(ResourceProvider {
            started: started.clone(),
            released: released.clone(),
        });
        adapter
            .start_turn(None, &model_request("drop resource"))
            .await
            .expect("start");
        let turns = adapter.turns.clone();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider polled");
        assert!(!released.load(Ordering::Acquire));
        drop(adapter);
        assert!(turns.lock().expect("turns").is_empty());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !released.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("adapter drop releases provider future resource");
    }

    #[derive(Debug)]
    struct PanickingProvider;

    impl InProcessModelProvider for PanickingProvider {
        fn run_turn(
            &self,
            _request: ModelTurnRequest,
            _context: InProcessProviderContext,
        ) -> InProcessProviderFuture<'_> {
            Box::pin(async { panic!("private panic payload") })
        }
    }

    #[tokio::test]
    async fn worker_panic_returns_normalized_error_and_releases_turn() {
        let mut adapter = InProcessModelProviderAdapter::new(PanickingProvider);
        let error = AgentRuntime::new()
            .run_text_turn(&mut adapter, AgentTurnRequest::new("model", "panic"))
            .await
            .expect_err("worker panic must terminalize");
        assert!(matches!(error, RuntimeError::Provider { code, message, .. }
            if code == "in_process_worker_stopped"
            && message == "in-process provider worker stopped before completing its turn"));
        assert!(adapter.turns.lock().expect("turns").is_empty());
    }

    #[derive(Debug)]
    struct CountInvocations(Arc<AtomicU64>);

    impl InProcessModelProvider for CountInvocations {
        fn run_turn(
            &self,
            _request: ModelTurnRequest,
            _context: InProcessProviderContext,
        ) -> InProcessProviderFuture<'_> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(InProcessProviderOutcome::EndTurn) })
        }
    }

    #[tokio::test]
    async fn cancellation_before_worker_poll_skips_provider_invocation() {
        let calls = Arc::new(AtomicU64::new(0));
        let mut adapter = InProcessModelProviderAdapter::new(CountInvocations(calls.clone()));
        let started = adapter
            .start_turn(None, &model_request("cancel-before-poll"))
            .await
            .expect("start");
        adapter
            .cancel_turn(
                None,
                &CancelTurnRequest {
                    provider_turn_id: started.provider_turn_id,
                },
            )
            .await
            .expect("cancel");
        // Current-thread executor cannot poll the spawned worker before this yield.
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn cancellation_wakes_outside_turn_map_lock() {
        struct CheckWake(Box<dyn Fn() + Send + Sync>);
        impl std::task::Wake for CheckWake {
            fn wake(self: Arc<Self>) {
                (self.0)();
            }
        }
        let mut adapter = InProcessModelProviderAdapter::new(BlockingProvider);
        let started = adapter
            .start_turn(None, &model_request("wake"))
            .await
            .expect("start");
        let turns = adapter.turns.clone();
        let token = turns
            .lock()
            .expect("turns")
            .get(&started.provider_turn_id)
            .expect("active")
            .cancellation
            .clone();
        let woke = Arc::new(AtomicBool::new(false));
        let observed = woke.clone();
        let waker = std::task::Waker::from(Arc::new(CheckWake(Box::new(move || {
            assert!(turns.try_lock().is_ok(), "wake must not hold turn map lock");
            observed.store(true, Ordering::Release);
        }))));
        let mut cancelled = Box::pin(token.cancelled());
        assert!(
            cancelled
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );
        adapter
            .cancel_turn(
                None,
                &CancelTurnRequest {
                    provider_turn_id: started.provider_turn_id,
                },
            )
            .await
            .expect("cancel");
        assert!(woke.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn polling_unknown_or_released_turn_fails_explicitly() {
        let mut adapter = InProcessModelProviderAdapter::new(BlockingProvider);
        let unknown = PollTurnEventsRequest {
            provider_turn_id: "unknown".into(),
        };
        assert!(matches!(
            adapter.poll_turn_events(None, &unknown).await,
            Err(RuntimeError::ProviderInvocation(_))
        ));
        let started = adapter
            .start_turn(None, &model_request("released"))
            .await
            .expect("start");
        let finish = FinishTurnRequest {
            provider_turn_id: started.provider_turn_id.clone(),
        };
        adapter.finish_turn(None, &finish).await.expect("finish");
        adapter
            .finish_turn(None, &finish)
            .await
            .expect("duplicate finish");
        let released = PollTurnEventsRequest {
            provider_turn_id: started.provider_turn_id,
        };
        assert!(matches!(
            adapter.poll_turn_events(None, &released).await,
            Err(RuntimeError::ProviderInvocation(_))
        ));
    }

    #[tokio::test]
    async fn unpolled_poll_preserves_queued_events() {
        let mut adapter = InProcessModelProviderAdapter::new(BlockingProvider);
        let started = adapter
            .start_turn(None, &model_request("poll"))
            .await
            .expect("start");
        let request = PollTurnEventsRequest {
            provider_turn_id: started.provider_turn_id,
        };
        drop(adapter.poll_turn_events(None, &request));
        let response = adapter
            .poll_turn_events(None, &request)
            .await
            .expect("poll");
        assert_eq!(response.events, vec![ProviderTurnEvent::TurnStarted]);
        assert!(
            adapter
                .poll_turn_events(None, &request)
                .await
                .expect("second poll")
                .events
                .is_empty()
        );
    }

    #[test]
    fn unpolled_start_does_not_allocate_or_spawn() {
        // No async executor is installed: constructing a future must not spawn work.
        let mut adapter = InProcessModelProviderAdapter::new(BlockingProvider);
        let request = model_request("unpolled");
        let future = adapter.start_turn(None, &request);
        drop(future);
        assert!(adapter.turns.lock().expect("turns").is_empty());
        assert_eq!(adapter.next_turn.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn runtime_timeout_cancels_and_finishes_custom_provider() {
        let mut provider = InProcessModelProviderAdapter::new(BlockingProvider);
        let mut request = AgentTurnRequest::new("model", "wait");
        request.timeout = Duration::from_millis(10);

        let runtime = AgentRuntime::new().with_poll_interval(Duration::from_mins(1));
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            runtime.run_text_turn(&mut provider, request),
        )
        .await
        .expect("turn deadline must interrupt the one-minute poll wait")
        .expect_err("blocking provider must time out");

        assert!(matches!(error, RuntimeError::Timeout { .. }));
        assert!(
            provider.turns.lock().expect("turns").is_empty(),
            "finish_turn must release custom provider state"
        );
    }

    #[tokio::test]
    async fn request_cancellation_interrupts_long_poll_wait() {
        let mut provider = InProcessModelProviderAdapter::new(BlockingProvider);
        let mut request = AgentTurnRequest::new("model", "cancel while waiting");
        request.timeout = Duration::from_mins(2);
        let cancellation = request.cancellation.clone();
        let turns = provider.turns.clone();
        let runtime = AgentRuntime::new().with_poll_interval(Duration::from_mins(1));
        let mut generation = Box::pin(runtime.run_text_turn(&mut provider, request));
        std::future::poll_fn(|context| {
            assert!(generation.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        {
            let turns = turns.lock().expect("turns");
            assert_eq!(turns.len(), 1, "provider started before cancellation");
            let state = turns.values().next().expect("active provider turn").clone();
            drop(turns);
            assert!(state.events.lock().expect("events").is_empty());
            assert!(!state.cancellation.is_cancelled());
        }
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), generation)
            .await
            .expect("request cancellation must interrupt the one-minute poll wait")
            .expect_err("cancelled request");
        assert!(matches!(error, RuntimeError::Cancelled));
        assert!(provider.turns.lock().expect("turns").is_empty());
        assert!(runtime.active_turn_generation().is_none());
    }

    #[tokio::test]
    async fn cancellation_closes_sink_and_preserves_existing_terminal_outcome() {
        for completed in [false, true] {
            let mut adapter = InProcessModelProviderAdapter::new(BlockingProvider);
            let started = adapter
                .start_turn(None, &model_request("cancel"))
                .await
                .expect("start");
            let state = adapter
                .turns
                .lock()
                .expect("turns")
                .get(&started.provider_turn_id)
                .expect("active")
                .clone();
            let _ = state.drain();
            if completed {
                state.finish(StopReason::EndTurn);
            }
            let request = CancelTurnRequest {
                provider_turn_id: started.provider_turn_id,
            };
            adapter.cancel_turn(None, &request).await.expect("cancel");
            adapter
                .cancel_turn(None, &request)
                .await
                .expect("duplicate cancel");
            assert!(state.cancellation.is_cancelled());
            let sink = InProcessProviderEventSink {
                state: state.clone(),
            };
            assert_eq!(
                sink.emit(ProviderTurnEvent::TextDelta {
                    text: "late".into()
                }),
                Err(InProcessProviderEmitError::TurnFinished)
            );
            let events = state.drain();
            if completed {
                assert!(matches!(
                    events.as_slice(),
                    [ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn
                    }]
                ));
            } else {
                assert!(matches!(
                    events.as_slice(),
                    [
                        ProviderTurnEvent::Cancelled,
                        ProviderTurnEvent::TurnFinished {
                            stop_reason: StopReason::Cancelled
                        }
                    ]
                ));
            }
        }
    }

    #[tokio::test]
    async fn finishing_adapter_turn_rejects_retained_event_sink() {
        let mut adapter = InProcessModelProviderAdapter::new(BlockingProvider);
        let started = adapter
            .start_turn(None, &model_request("finish"))
            .await
            .expect("start");
        let state = adapter
            .turns
            .lock()
            .expect("turns")
            .get(&started.provider_turn_id)
            .expect("active turn")
            .clone();
        let sink = InProcessProviderEventSink {
            state: state.clone(),
        };
        adapter
            .finish_turn(
                None,
                &FinishTurnRequest {
                    provider_turn_id: started.provider_turn_id,
                },
            )
            .await
            .expect("finish");
        assert!(adapter.turns.lock().expect("turns").is_empty());
        assert!(state.cancellation.is_cancelled());
        assert_eq!(
            sink.emit(ProviderTurnEvent::TextDelta {
                text: "late".into()
            }),
            Err(InProcessProviderEmitError::TurnFinished)
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
            tool_schema_mode: None,
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
