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
    /// Returns an error when the event is adapter-owned, the turn is terminal, or
    /// the configured event-count capacity is exhausted.
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
    /// The event queue or encoded event size reached its configured limit; the turn is terminated.
    #[error("in-process provider event buffer is full")]
    BufferFull,
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
    closed: bool,
    event_capacity: std::num::NonZeroUsize,
    event_byte_limit: std::num::NonZeroUsize,
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
            closed: false,
            event_capacity: const { std::num::NonZeroUsize::new(1024).unwrap() },
            event_byte_limit: const { std::num::NonZeroUsize::new(1024 * 1024).unwrap() },
            turns: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Limit queued events for subsequently started turns.
    ///
    /// Defaults to 1024 events. Two additional slots are reserved for terminal
    /// error/cancellation and finish events. Overflow terminates the turn rather
    /// than silently dropping output. This limits event count, not payload bytes.
    #[must_use]
    pub const fn with_event_capacity(mut self, capacity: std::num::NonZeroUsize) -> Self {
        self.event_capacity = capacity;
        self
    }

    /// Limit the JSON-encoded size of each subsequently emitted provider event.
    ///
    /// Defaults to one MiB. Size checking does not allocate an encoded copy.
    /// Oversized or unencodable events fail the turn without retaining the payload.
    /// Returned provider errors are also checked; oversized errors are replaced
    /// with a fixed non-retryable diagnostic. Adapter-owned lifecycle events and
    /// fixed diagnostics are exempt so even tiny limits permit terminal delivery.
    /// This bounds accepted event representations, not provider-side allocations.
    #[must_use]
    pub const fn with_event_byte_limit(mut self, limit: std::num::NonZeroUsize) -> Self {
        self.event_byte_limit = limit;
        self
    }

    /// Close admission, cancel admitted turns, and await worker release.
    ///
    /// Admission closes and cancellation is requested when this method is called,
    /// even if the returned future is never polled. Success releases retained turn
    /// state. Timeout or abandonment retains ownership for a subsequent retry.
    /// The deadline uses the selected runtime clock and requires cooperative tasks.
    ///
    /// # Panics
    /// Panics if an internal turn-registry lock has been poisoned.
    ///
    /// # Errors
    /// Returns an invocation error if workers have not acknowledged release within
    /// `budget`. The adapter remains closed after either success or failure.
    pub fn shutdown(&mut self, budget: std::time::Duration) -> RuntimeFuture<'_, ()> {
        let drain = self.shutdown_wait();
        Box::pin(async move {
            switchy::unsync::time::timeout(budget, drain)
                .await
                .map_err(|_| {
                    crate::RuntimeError::ProviderInvocation(
                        "in-process provider shutdown incomplete; retry cleanup".into(),
                    )
                })?
        })
    }

    /// Close admission, cancel admitted turns, and await release without a timer.
    ///
    /// Hosts may wrap this wait in their own deadline. Admission closes and cancellation
    /// is requested on call, even if the future is never polled. An abandoned wait
    /// retains cleanup ownership for retry; success releases retained turn state.
    /// This does not change the execution backend used by provider workers.
    ///
    /// # Panics
    /// Panics if an internal turn-registry lock has been poisoned.
    pub fn shutdown_wait(&mut self) -> RuntimeFuture<'_, ()> {
        self.closed = true;
        for state in self.turns.lock().expect("turn registry").values() {
            state.finish_cancelled();
            state.cancellation.cancel();
        }
        Box::pin(async move {
            let drain = async {
                loop {
                    let next = self
                        .turns
                        .lock()
                        .expect("turn registry")
                        .iter()
                        .next()
                        .map(|(id, state)| (id.clone(), Arc::clone(state)));
                    let Some((id, state)) = next else {
                        break;
                    };
                    state.released.cancelled().await;
                    self.turns.lock().expect("turn registry").remove(&id);
                }
            };
            drain.await;
            Ok(())
        })
    }

    fn turn_id(&self) -> Result<String, crate::RuntimeError> {
        let sequence = self
            .next_turn
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                crate::RuntimeError::ProviderInvocation(
                    "in-process provider turn ID space exhausted".into(),
                )
            })?;
        Ok(format!("in-process-turn-{}", sequence + 1))
    }
}

struct InProcessTurnCleanup {
    turns: Arc<Mutex<BTreeMap<String, Arc<InProcessTurnState>>>>,
    turn_id: String,
    armed: bool,
}

impl crate::ProviderTurnCleanup for InProcessTurnCleanup {}

impl Drop for InProcessTurnCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = self
            .turns
            .lock()
            .expect("in-process provider turn lock should not be poisoned")
            .get(&self.turn_id)
            .cloned();
        if let Some(state) = state {
            state.abandoned.store(true, Ordering::Release);
            state.finish_cancelled();
            state.cancellation.cancel();
            if state.released.is_cancelled() {
                self.turns
                    .lock()
                    .expect("turn registry")
                    .remove(&self.turn_id);
            }
        }
    }
}

impl<P> ModelProviderInvoker for InProcessModelProviderAdapter<P>
where
    P: InProcessModelProvider,
{
    fn shutdown_wait(&mut self) -> RuntimeFuture<'_, ()> {
        Self::shutdown_wait(self)
    }

    fn shutdown(&mut self, budget: std::time::Duration) -> RuntimeFuture<'_, ()> {
        Self::shutdown(self, budget)
    }

    fn turn_cleanup_handle(
        &mut self,
        _provider_plugin_id: Option<&str>,
        provider_turn_id: &str,
    ) -> Option<Box<dyn crate::ProviderTurnCleanup>> {
        Some(Box::new(InProcessTurnCleanup {
            turns: self.turns.clone(),
            turn_id: provider_turn_id.to_owned(),
            armed: true,
        }))
    }

    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async move {
            if self.closed {
                let error = in_process_provider_error(
                    "in_process_admission_closed",
                    ProviderErrorCategory::ProviderInternal,
                    "in-process provider admission is closed",
                );
                return Err(crate::RuntimeError::Provider {
                    code: error.code.clone(),
                    message: error.message.clone(),
                    error: Box::new(error),
                });
            }
            let provider_turn_id = self.turn_id()?;
            let state = Arc::new(InProcessTurnState {
                capacity: self.event_capacity.get(),
                byte_limit: self.event_byte_limit.get(),
                ..InProcessTurnState::new()
            });
            state.push(ProviderTurnEvent::TurnStarted);
            self.turns
                .lock()
                .expect("in-process provider turn lock should not be poisoned")
                .insert(provider_turn_id.clone(), Arc::clone(&state));
            let mut acquisition = InProcessTurnCleanup {
                turns: Arc::clone(&self.turns),
                turn_id: provider_turn_id.clone(),
                armed: true,
            };
            let provider = Arc::clone(&self.provider);
            let request = request.clone();
            let worker = RegisteredWorker {
                worker: Some(InProcessWorkerGuard(Arc::clone(&state))),
                turns: Arc::clone(&self.turns),
                id: provider_turn_id.clone(),
                state: Arc::clone(&state),
            };
            switchy::unsync::task::spawn(async move {
                use futures::FutureExt as _;
                let _worker = worker;
                let context = InProcessProviderContext {
                    events: InProcessProviderEventSink {
                        state: Arc::clone(&state),
                    },
                    cancellation: state.cancellation.clone(),
                };
                let execution = std::panic::AssertUnwindSafe(async {
                    provider.run_turn(request, context).await
                })
                .catch_unwind();
                switchy::unsync::select! {
                    biased;
                    () = state.cancellation.cancelled() => state.finish_cancelled(),
                    result = execution => match result {
                        Ok(Ok(outcome)) => state.finish(outcome.stop_reason()),
                        Ok(Err(error)) => state.finish_error(error),
                        // The worker guard publishes the normalized failure and release
                        // acknowledgment; no panic payload crosses the provider boundary.
                        Err(_) => {},
                    },
                }
            });
            acquisition.armed = false;
            drop(acquisition);
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
            .get(&request.provider_turn_id)
            .cloned();
        if let Some(state) = &state {
            state.finish_cancelled();
            state.cancellation.cancel();
        }
        Box::pin(async move {
            if let Some(state) = state {
                state.released.cancelled().await;
                self.turns
                    .lock()
                    .expect("in-process provider turn lock should not be poisoned")
                    .remove(&request.provider_turn_id);
            }
            Ok(AckResponse::default())
        })
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
    capacity: usize,
    byte_limit: usize,
    events: Mutex<VecDeque<ProviderTurnEvent>>,
    cancellation: CancellationToken,
    released: CancellationToken,
    abandoned: AtomicBool,
    terminal: AtomicBool,
}

struct EventSizeBudget(usize);

impl std::io::Write for EventSizeBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("provider event exceeds size budget"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct RegisteredWorker {
    worker: Option<InProcessWorkerGuard>,
    turns: Arc<Mutex<BTreeMap<String, Arc<InProcessTurnState>>>>,
    id: String,
    state: Arc<InProcessTurnState>,
}

impl Drop for RegisteredWorker {
    fn drop(&mut self) {
        // Release must precede registry removal: shutdown never loses sight of
        // work that has not yet relinquished its provider future.
        drop(self.worker.take());
        if self.state.abandoned.load(Ordering::Acquire) {
            self.turns.lock().expect("turn registry").remove(&self.id);
        }
    }
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
        self.0.released.cancel();
    }
}

impl InProcessTurnState {
    fn new() -> Self {
        Self {
            capacity: 1024,
            byte_limit: 1024 * 1024,
            events: Mutex::new(VecDeque::new()),
            cancellation: CancellationToken::new(),
            released: CancellationToken::new(),
            abandoned: AtomicBool::new(false),
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
        let invalid_size = serde_json::to_writer(EventSizeBudget(self.byte_limit), &event).is_err();
        if events.len() >= self.capacity || invalid_size {
            if self.begin_finish() {
                events.extend([
                    ProviderTurnEvent::Error {
                        error: in_process_provider_error(
                            "in_process_buffer_full",
                            ProviderErrorCategory::ProviderInternal,
                            "in-process provider event buffer is full",
                        ),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::Error,
                    },
                ]);
            }
            drop(events);
            self.cancellation.cancel();
            return Err(InProcessProviderEmitError::BufferFull);
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
        let event = ProviderTurnEvent::Error { error };
        let event = if serde_json::to_writer(EventSizeBudget(self.byte_limit), &event).is_ok() {
            event
        } else {
            ProviderTurnEvent::Error {
                error: in_process_provider_error(
                    "in_process_error_too_large",
                    ProviderErrorCategory::ProviderInternal,
                    "in-process provider error exceeds the event size limit",
                ),
            }
        };
        self.finish_events([
            event,
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
    async fn event_overflow_is_terminal_and_worker_is_released() {
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider)
            .with_event_capacity(std::num::NonZeroUsize::new(1).unwrap());
        let error = AgentRuntime::new()
            .run_text_turn(&mut provider, AgentTurnRequest::new("model", "overflow"))
            .await
            .expect_err("producer exceeds queue capacity");
        let RuntimeError::ProviderAfterOutput(error) = error else {
            panic!("overflow after output must prevent retry");
        };
        assert!(matches!(*error, RuntimeError::Provider { code, .. }
            if code == "in_process_buffer_full"));
        assert!(provider.turns.lock().expect("turn registry").is_empty());
    }

    #[tokio::test]
    async fn boxed_provider_shutdown_delegates_and_rejects_new_turns() {
        let mut provider: Box<dyn ModelProviderInvoker> =
            Box::new(InProcessModelProviderAdapter::new(EchoProvider));
        provider.shutdown(Duration::from_secs(1)).await.unwrap();
        let error = AgentRuntime::new()
            .run_text_turn(&mut provider, AgentTurnRequest::new("model", "closed"))
            .await
            .expect_err("boxed shutdown closes admission");
        assert!(matches!(error, RuntimeError::Provider { code, error, .. }
            if code == "in_process_admission_closed" && !error.retryable));
    }

    #[tokio::test]
    async fn abandoned_turn_remains_owned_until_shutdown_acknowledgment() {
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider);
        let state = Arc::new(InProcessTurnState::new());
        provider
            .turns
            .lock()
            .unwrap()
            .insert("abandoned".into(), state.clone());
        drop(provider.turn_cleanup_handle(None, "abandoned"));
        assert!(state.cancellation.is_cancelled());
        assert_eq!(provider.turns.lock().unwrap().len(), 1);
        assert!(provider.shutdown(Duration::from_millis(1)).await.is_err());
        state.released.cancel();
        provider.shutdown(Duration::from_secs(1)).await.unwrap();
        assert!(provider.turns.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn shutdown_timeout_retains_ownership_and_retry_drains() {
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider);
        let state = Arc::new(InProcessTurnState::new());
        provider
            .turns
            .lock()
            .unwrap()
            .insert("held".into(), state.clone());
        drop(provider.shutdown(Duration::from_secs(1)));
        assert!(provider.closed);
        assert!(state.cancellation.is_cancelled());
        assert!(provider.shutdown(Duration::from_millis(1)).await.is_err());
        assert_eq!(provider.turns.lock().unwrap().len(), 1);
        state.released.cancel();
        provider.shutdown(Duration::from_secs(1)).await.unwrap();
        assert!(provider.turns.lock().unwrap().is_empty());
        provider.shutdown(Duration::from_secs(1)).await.unwrap();
        assert!(
            AgentRuntime::new()
                .run_text_turn(&mut provider, AgentTurnRequest::new("model", "rejected"),)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn boxed_provider_shutdown_wait_delegates_and_closes_before_polling() {
        let mut provider: Box<dyn ModelProviderInvoker> =
            Box::new(InProcessModelProviderAdapter::new(EchoProvider));
        drop(provider.shutdown_wait());
        let request = model_request("closed");
        assert!(provider.start_turn(None, &request).await.is_err());
        provider.shutdown_wait().await.unwrap();
        provider.shutdown_wait().await.unwrap();
        drop(provider);
    }

    #[test]
    fn shutdown_wait_needs_no_runtime_and_retains_abandoned_work() {
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider);
        let state = Arc::new(InProcessTurnState::new());
        provider
            .turns
            .lock()
            .unwrap()
            .insert("held".into(), state.clone());
        drop(provider.shutdown_wait());
        assert!(provider.closed);
        assert!(state.cancellation.is_cancelled());
        {
            let mut wait = provider.shutdown_wait();
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(wait.as_mut().poll(&mut context).is_pending());
        }
        assert_eq!(provider.turns.lock().unwrap().len(), 1);
        state.released.cancel();
        {
            let mut wait = provider.shutdown_wait();
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(matches!(
                wait.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(()))
            ));
        }
        assert!(provider.turns.lock().unwrap().is_empty());
        drop(provider);
    }

    #[test]
    fn terminal_error_limit_preserves_valid_errors_and_replaces_oversized_errors() {
        let mut error = in_process_provider_error(
            "provider_failure",
            ProviderErrorCategory::ProviderInternal,
            "failure",
        );
        error.retryable = true;
        error.provider_message = Some("private payload".repeat(100).into());
        let event = ProviderTurnEvent::Error {
            error: error.clone(),
        };
        let size = serde_json::to_vec(&event).unwrap().len();
        let state = InProcessTurnState {
            byte_limit: size,
            ..InProcessTurnState::new()
        };
        state.finish_error(error.clone());
        let events = state.drain();
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap(),
            serde_json::to_value(&event).unwrap()
        );
        for byte_limit in [1, size - 1] {
            let state = InProcessTurnState {
                byte_limit,
                ..InProcessTurnState::new()
            };
            state.finish_error(error.clone());
            state.finish(StopReason::EndTurn);
            let events = state.drain();
            assert_eq!(events.len(), 2);
            assert!(matches!(&events[0], ProviderTurnEvent::Error { error }
                if error.code == "in_process_error_too_large"
                    && !error.retryable && error.provider_message.is_none()));
            assert!(matches!(
                events[1],
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::Error
                }
            ));
            assert!(
                !serde_json::to_string(&events)
                    .unwrap()
                    .contains("private payload")
            );
        }
    }

    #[test]
    fn oversized_event_is_rejected_without_retaining_payload() {
        let event = ProviderTurnEvent::TextDelta {
            text: "sensitive large payload".repeat(100),
        };
        let size = serde_json::to_vec(&event).unwrap().len();
        let state = InProcessTurnState {
            byte_limit: size,
            ..InProcessTurnState::new()
        };
        state
            .push_nonterminal(event.clone())
            .expect("exact byte limit accepted");
        assert_eq!(state.drain().len(), 1);
        let state = InProcessTurnState {
            byte_limit: size - 1,
            ..InProcessTurnState::new()
        };
        assert_eq!(
            state.push_nonterminal(event),
            Err(InProcessProviderEmitError::BufferFull)
        );
        let events = state.drain();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderTurnEvent::Error { error }
            if error.code == "in_process_buffer_full"));
        assert!(state.cancellation.is_cancelled());
    }

    #[test]
    fn overflow_retains_bounded_history_and_rejects_late_events() {
        let state = InProcessTurnState {
            capacity: 1,
            ..InProcessTurnState::new()
        };
        let event = || ProviderTurnEvent::TextDelta {
            text: "delta".into(),
        };
        assert!(state.push_nonterminal(event()).is_ok());
        assert_eq!(
            state.push_nonterminal(event()),
            Err(InProcessProviderEmitError::BufferFull)
        );
        assert_eq!(
            state.push_nonterminal(event()),
            Err(InProcessProviderEmitError::TurnFinished)
        );
        state.finish(StopReason::EndTurn);
        let events = state.drain();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events.last(),
            Some(ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Error
            })
        ));
        assert!(state.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn abandoned_finish_retains_state_until_worker_release() {
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider);
        let started = provider
            .start_turn(None, &model_request("finish ownership"))
            .await
            .expect("start");
        let request = FinishTurnRequest {
            provider_turn_id: started.provider_turn_id,
        };
        let state =
            provider.turns.lock().expect("turn registry")[&request.provider_turn_id].clone();
        // The current-thread executor has not polled the spawned worker yet.
        let mut finish = provider.finish_turn(None, &request);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(finish.as_mut().poll(&mut context).is_pending());
        drop(finish);
        assert!(state.cancellation.is_cancelled());
        assert!(!state.released.is_cancelled());
        assert!(
            provider
                .turns
                .lock()
                .expect("turn registry")
                .contains_key(&request.provider_turn_id)
        );
        provider
            .finish_turn(None, &request)
            .await
            .expect("resume finish");
        assert!(state.released.is_cancelled());
        assert!(provider.turns.lock().expect("turn registry").is_empty());
    }

    #[test]
    fn failed_worker_spawn_releases_partial_turn_acquisition() {
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider);
        for _ in 0..2 {
            let request = model_request("no executor");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut start = provider.start_turn(None, &request);
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                let _ = start.as_mut().poll(&mut context);
            }));
            assert!(result.is_err(), "native spawning requires an executor");
            assert!(provider.turns.lock().expect("turn registry").is_empty());
        }
    }

    #[tokio::test]
    async fn exhausted_turn_ids_fail_before_registering_work() {
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider);
        provider.next_turn.store(u64::MAX - 1, Ordering::Relaxed);
        let started = provider
            .start_turn(None, &model_request("last-valid"))
            .await
            .expect("last available ID starts a turn");
        assert_eq!(
            started.provider_turn_id,
            format!("in-process-turn-{}", u64::MAX)
        );
        let state =
            provider.turns.lock().expect("turn registry")[&started.provider_turn_id].clone();
        for _ in 0..2 {
            let error = provider
                .start_turn(None, &model_request("exhausted"))
                .await
                .expect_err("ID exhaustion must reject start");
            assert!(matches!(error, RuntimeError::ProviderInvocation(message)
                if message == "in-process provider turn ID space exhausted"));
            let turns = provider.turns.lock().expect("turn registry");
            assert_eq!(turns.len(), 1);
            assert!(Arc::ptr_eq(&turns[&started.provider_turn_id], &state));
            drop(turns);
            assert_eq!(provider.next_turn.load(Ordering::Relaxed), u64::MAX);
        }
        provider
            .finish_turn(
                None,
                &FinishTurnRequest {
                    provider_turn_id: started.provider_turn_id,
                },
            )
            .await
            .expect("last valid turn remains releasable");
        assert!(provider.turns.lock().expect("turn registry").is_empty());
    }

    #[tokio::test]
    async fn exhausted_provider_start_releases_runtime_scope() {
        let runtime = AgentRuntime::new();
        let mut provider = InProcessModelProviderAdapter::new(EchoProvider);
        provider.next_turn.store(u64::MAX, Ordering::Relaxed);
        for _ in 0..2 {
            let error = runtime
                .run_text_turn(&mut provider, AgentTurnRequest::new("model", "exhausted"))
                .await
                .expect_err("exhausted provider rejects runtime turn");
            assert!(matches!(error, RuntimeError::ProviderInvocation(message)
                if message == "in-process provider turn ID space exhausted"));
            assert!(runtime.active_turn_generation().is_none());
            assert!(provider.turns.lock().expect("turn registry").is_empty());
        }
        let mut replacement = InProcessModelProviderAdapter::new(EchoProvider);
        let response = runtime
            .run_text_turn(&mut replacement, AgentTurnRequest::new("model", "recovery"))
            .await
            .expect("runtime remains usable after rejected start");
        assert_eq!(response.text, "hello from custom provider");
        assert!(runtime.active_turn_generation().is_none());
        assert!(replacement.turns.lock().expect("turn registry").is_empty());
    }

    #[tokio::test]
    async fn exhausted_provider_stream_emits_one_error_and_closes() {
        let runtime = AgentRuntime::new();
        let provider = InProcessModelProviderAdapter::new(EchoProvider);
        provider.next_turn.store(u64::MAX, Ordering::Relaxed);
        let turns = Arc::clone(&provider.turns);
        let mut stream = runtime
            .run_streaming_text_turn(provider, AgentTurnRequest::new("model", "exhausted stream"));
        let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("stream reports failure promptly")
            .expect("terminal error");
        assert!(matches!(item,
            crate::AgentRuntimeStreamItem::Error(RuntimeError::ProviderInvocation(message))
            if message == "in-process provider turn ID space exhausted"));
        assert!(
            tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("stream closes promptly")
                .is_none()
        );
        assert!(runtime.active_turn_generation().is_none());
        assert!(turns.lock().expect("turn registry").is_empty());
    }

    #[tokio::test]
    async fn closed_scope_cancellation_disposition_releases_acquired_adapter_turn() {
        let runtime = AgentRuntime::new();
        let scope = runtime.begin_turn_scope(
            "closed-adapter",
            Arc::new(crate::RuntimeStreamEventSink::default()),
            crate::InvocationCapabilities::default(),
        );
        let polled = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let mut adapter = InProcessModelProviderAdapter::new(ResourceProvider {
            started: polled.clone(),
            released: released.clone(),
        });
        let started = adapter
            .start_turn(None, &model_request("acquired"))
            .await
            .expect("acquire provider turn");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !polled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider resource acquired before scope cancellation");
        assert!(!released.load(Ordering::Acquire));
        assert_eq!(adapter.turns.lock().expect("registry").len(), 1);
        assert!(runtime.cancel_turn_scope(&scope));
        let cancel = CancelTurnRequest {
            provider_turn_id: started.provider_turn_id.clone(),
        };
        let finish = FinishTurnRequest {
            provider_turn_id: started.provider_turn_id,
        };
        let mut events = Vec::new();
        let result = crate::apply_provider_event_disposition(
            &mut adapter,
            &crate::ProviderEventContext {
                provider_plugin_id: None,
                cancel_request: &cancel,
                finish_request: &finish,
                scope: &scope,
                start: crate::instant_now(),
            },
            crate::EventDisposition::Cancelled(crate::AgentRuntimeEvent::Cancelled),
            &mut String::new(),
            &mut None,
            &mut events,
        )
        .await;
        assert!(matches!(result, Err(RuntimeError::Cancelled)));
        assert!(events.is_empty());
        assert!(adapter.turns.lock().expect("registry").is_empty());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !released.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed-scope disposition releases the running provider resource");
        assert!(scope.control().mark_cancelled());
        assert!(runtime.turns.release_terminal_turn(&scope));
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
    async fn abandoning_borrowed_generation_releases_running_provider_resource() {
        let started = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let mut adapter = InProcessModelProviderAdapter::new(ResourceProvider {
            started: started.clone(),
            released: released.clone(),
        });
        let runtime = AgentRuntime::new();
        let mut generation = Box::pin(
            runtime.run_text_turn(&mut adapter, AgentTurnRequest::new("model", "abandon")),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    result = &mut generation => panic!("pending provider completed: {result:?}"),
                    () = tokio::task::yield_now() => {
                        if started.load(Ordering::Acquire) { break; }
                    }
                }
            }
        })
        .await
        .expect("provider acquired resource");
        assert!(!released.load(Ordering::Acquire));
        drop(generation);
        assert_eq!(adapter.turns.lock().expect("registry").len(), 1);
        assert!(runtime.active_turn_generation().is_none());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !released.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("abandoned provider resource released while adapter remains alive");
        adapter.shutdown(Duration::from_secs(1)).await.unwrap();
        assert!(adapter.turns.lock().expect("registry").is_empty());
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
    async fn scope_cancellation_interrupts_long_poll_wait() {
        struct Sink;
        impl crate::TurnEventSink for Sink {
            fn emit(&self, _event: crate::ScopedTurnEvent) -> bool {
                true
            }
        }
        let mut provider = InProcessModelProviderAdapter::new(BlockingProvider);
        let turns = provider.turns.clone();
        let mut request = AgentTurnRequest::new("model", "scope cancellation");
        request.timeout = Duration::from_mins(2);
        let runtime = AgentRuntime::new().with_poll_interval(Duration::from_mins(1));
        let scope = runtime.begin_turn_scope(
            "scoped wait",
            Arc::new(Sink),
            crate::InvocationCapabilities::default(),
        );
        let mut generation =
            Box::pin(runtime.run_text_turn_in_scope(&mut provider, &request, &scope));
        std::future::poll_fn(|context| {
            assert!(generation.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(turns.lock().expect("turns").len(), 1);
        assert!(runtime.cancel_turn_scope(&scope));
        let error = tokio::time::timeout(Duration::from_secs(2), generation)
            .await
            .expect("scope cancellation interrupts poll wait")
            .expect_err("cancelled scope");
        assert!(matches!(error, RuntimeError::Cancelled));
        assert!(!request.cancellation.is_cancelled());
        assert!(turns.lock().expect("turns").is_empty());
    }

    #[tokio::test]
    async fn superseded_long_poll_releases_provider_without_releasing_new_scope() {
        struct Sink;
        impl crate::TurnEventSink for Sink {
            fn emit(&self, _event: crate::ScopedTurnEvent) -> bool {
                true
            }
        }
        let mut provider = InProcessModelProviderAdapter::new(BlockingProvider);
        let turns = provider.turns.clone();
        let mut request = AgentTurnRequest::new("model", "superseded");
        request.timeout = Duration::from_mins(2);
        let runtime = AgentRuntime::new().with_poll_interval(Duration::from_mins(1));
        let mut generation = Box::pin(runtime.run_text_turn(&mut provider, request));
        std::future::poll_fn(|context| {
            assert!(generation.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(turns.lock().expect("turns").len(), 1);
        let replacement = runtime.begin_turn_scope(
            "replacement",
            Arc::new(Sink),
            crate::InvocationCapabilities::default(),
        );
        let active = runtime.active_turn_generation();
        let error = tokio::time::timeout(Duration::from_secs(2), generation)
            .await
            .expect("superseding scope interrupts poll wait")
            .expect_err("superseded turn");
        assert!(matches!(error, RuntimeError::Cancelled));
        assert!(turns.lock().expect("turns").is_empty());
        assert!(replacement.accepts_work());
        assert_eq!(runtime.active_turn_generation(), active);
        assert!(runtime.cancel_turn_scope(&replacement));
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
