//! Deterministic helpers for recording and asserting high-level SDK text streams.

pub use bcode_agent_runtime::testing::*;

use crate::{
    AgentBuilder, AgentEvent, AgentTurnRequest, BcodeError, CancellationToken,
    GenerateTextResponse, ModelResponseCache, ModelResponseCachePrivacy, PermissionDecision,
    PermissionPolicy, PersistedSession, RuntimeError, RuntimeFuture, RuntimePermissionRequest,
    ScopedTurnEvent, SessionPersistenceAdapter, TextStream, TextStreamItem, ToolDefinition,
    ToolInvocationDescriptor, ToolInvocationResponse,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::Notify;

/// Stable event discriminants for concise ordering assertions that do not couple tests to payloads.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextStreamEventKind {
    /// Provider turn started.
    TurnStarted,
    /// Assistant text delta.
    TextDelta,
    /// Reasoning delta.
    ReasoningDelta,
    /// Tool call started.
    ToolCallStarted,
    /// Tool-call argument delta.
    ToolCallDelta,
    /// Tool call completed.
    ToolCallFinished,
    /// Tool result completed.
    ToolResult,
    /// Usage update.
    Usage,
    /// Exact request-input token count.
    ExactRequestInputTokens,
    /// Provider request projection.
    RequestProjection,
    /// Context compaction.
    ContextCompacted,
    /// Provider metadata.
    ProviderMetadata,
    /// Retry scheduling.
    RetryScheduled,
    /// Warning.
    Warning,
    /// Provider error.
    ProviderError,
    /// Successful provider finish.
    Finished,
    /// Cancellation.
    Cancelled,
}

impl From<&AgentEvent> for TextStreamEventKind {
    fn from(event: &AgentEvent) -> Self {
        match event {
            AgentEvent::TurnStarted => Self::TurnStarted,
            AgentEvent::TextDelta(_) => Self::TextDelta,
            AgentEvent::ReasoningDelta(_) => Self::ReasoningDelta,
            AgentEvent::ToolCallStarted { .. } => Self::ToolCallStarted,
            AgentEvent::ToolCallDelta { .. } => Self::ToolCallDelta,
            AgentEvent::ToolCallFinished(_) => Self::ToolCallFinished,
            AgentEvent::ToolResult(_) => Self::ToolResult,
            AgentEvent::Usage(_) => Self::Usage,
            AgentEvent::ExactRequestInputTokens(_) => Self::ExactRequestInputTokens,
            AgentEvent::RequestProjection(_) => Self::RequestProjection,
            AgentEvent::ContextCompacted => Self::ContextCompacted,
            AgentEvent::ProviderMetadata { .. } => Self::ProviderMetadata,
            AgentEvent::RetryScheduled { .. } => Self::RetryScheduled,
            AgentEvent::Warning(_) => Self::Warning,
            AgentEvent::ProviderError { .. } => Self::ProviderError,
            AgentEvent::Finished { .. } => Self::Finished,
            AgentEvent::Cancelled => Self::Cancelled,
        }
    }
}

/// Failure from a text-stream transcript assertion.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextStreamAssertionError {
    /// Normalized runtime events differed from the expected order.
    #[error("text stream event order mismatch: expected {expected:?}, observed {actual:?}")]
    EventOrder {
        /// Expected normalized events.
        expected: Vec<AgentEvent>,
        /// Observed normalized events.
        actual: Vec<AgentEvent>,
    },
    /// Normalized runtime event kinds differed from the expected order.
    #[error("text stream event-kind order mismatch: expected {expected:?}, observed {actual:?}")]
    EventKindOrder {
        /// Expected event kinds.
        expected: Vec<TextStreamEventKind>,
        /// Observed event kinds.
        actual: Vec<TextStreamEventKind>,
    },
    /// The stream had not been consumed to exhaustion.
    #[error("text stream was not consumed to exhaustion")]
    NotExhausted,
    /// The stream had no terminal item.
    #[error("text stream had no terminal item")]
    MissingTerminal,
    /// The stream had more than one terminal item.
    #[error("text stream had {count} terminal items; expected exactly one")]
    MultipleTerminals {
        /// Number of observed terminal items.
        count: usize,
    },
    /// A terminal item was followed by another item.
    #[error(
        "text stream terminal item was at index {terminal_index}, but item count was {item_count}"
    )]
    TerminalNotLast {
        /// Zero-based terminal item index.
        terminal_index: usize,
        /// Total recorded item count.
        item_count: usize,
    },
    /// The stream ended in an error instead of a successful response.
    #[error("text stream ended in error: {message}")]
    ExpectedFinished {
        /// Display representation of the terminal error.
        message: String,
    },
    /// The stream completed successfully instead of returning an error.
    #[error("text stream completed successfully; expected a runtime error")]
    ExpectedRuntimeError,
    /// The stream returned a non-runtime SDK error.
    #[error("text stream returned a non-runtime SDK error: {message}")]
    ExpectedRuntimeErrorKind {
        /// Display representation of the terminal SDK error.
        message: String,
    },
    /// The terminal runtime error was not cancellation.
    #[error("text stream runtime error was not cancellation: {actual}")]
    ExpectedCancellation {
        /// Debug representation of the actual runtime error.
        actual: String,
    },
    /// The terminal runtime error was not the expected bounded-buffer overflow.
    #[error("text stream did not overflow at capacity {expected_capacity}: {actual}")]
    ExpectedBackpressure {
        /// Expected configured stream capacity.
        expected_capacity: usize,
        /// Debug representation of the actual runtime error.
        actual: String,
    },
}

/// Incremental recorder for a high-level [`TextStream`].
///
/// The recorder supports intentional partial consumption: call [`Self::consume_up_to`], inspect
/// [`Self::items`] and [`Self::is_exhausted`], then continue or consume the remainder with
/// [`Self::finish`]. Dropping a partially consumed recorder drops the underlying stream and
/// therefore exercises the stream's normal drop-cancellation behavior.
#[derive(Debug)]
pub struct TextStreamRecorder {
    stream: TextStream,
    items: Vec<TextStreamItem>,
    exhausted: bool,
}

impl TextStreamRecorder {
    /// Wrap one high-level text stream.
    #[must_use]
    pub const fn new(stream: TextStream) -> Self {
        Self {
            stream,
            items: Vec::new(),
            exhausted: false,
        }
    }

    /// Return all items consumed so far in exact delivery order.
    #[must_use]
    pub fn items(&self) -> &[TextStreamItem] {
        &self.items
    }

    /// Return whether the underlying stream has returned `None`.
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Consume and record at most one item.
    ///
    /// Returns `true` when an item was recorded and `false` after stream exhaustion.
    pub async fn consume_next(&mut self) -> bool {
        if self.exhausted {
            return false;
        }
        if let Some(item) = self.stream.next().await {
            self.items.push(item);
            true
        } else {
            self.exhausted = true;
            false
        }
    }

    /// Consume and record at most `limit` additional items.
    ///
    /// Returns the number of newly recorded items. A zero limit does not poll the stream.
    pub async fn consume_up_to(&mut self, limit: usize) -> usize {
        let initial = self.items.len();
        for _ in 0..limit {
            if !self.consume_next().await {
                break;
            }
        }
        self.items.len() - initial
    }

    /// Request cancellation and consume through the resulting terminal state.
    pub async fn cancel_and_finish(self, cancellation: &CancellationToken) -> TextStreamTranscript {
        cancellation.cancel();
        self.finish().await
    }

    /// Consume the stream to exhaustion and return its complete transcript.
    pub async fn finish(mut self) -> TextStreamTranscript {
        while self.consume_next().await {}
        TextStreamTranscript {
            items: self.items,
            exhausted: self.exhausted,
        }
    }
}

/// Complete or partial ordered recording of a high-level text stream.
#[derive(Debug)]
pub struct TextStreamTranscript {
    items: Vec<TextStreamItem>,
    exhausted: bool,
}

impl TextStreamTranscript {
    /// Construct a transcript from explicitly recorded items.
    ///
    /// This is useful for negative tests of transcript assertions. Normal stream tests should use
    /// [`TextStreamRecorder`].
    #[must_use]
    pub const fn from_items(items: Vec<TextStreamItem>, exhausted: bool) -> Self {
        Self { items, exhausted }
    }

    /// Return all recorded items in exact delivery order.
    #[must_use]
    pub fn items(&self) -> &[TextStreamItem] {
        &self.items
    }

    /// Return whether recording observed stream exhaustion.
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Return cloned normalized runtime events in delivery order.
    #[must_use]
    pub fn events(&self) -> Vec<AgentEvent> {
        self.items
            .iter()
            .filter_map(|item| match item {
                TextStreamItem::Event(event) => Some(event.clone()),
                TextStreamItem::ScopedEvent(_)
                | TextStreamItem::Finished(_)
                | TextStreamItem::Error(_) => None,
            })
            .collect()
    }

    /// Return normalized runtime event discriminants in delivery order.
    #[must_use]
    pub fn event_kinds(&self) -> Vec<TextStreamEventKind> {
        self.items
            .iter()
            .filter_map(|item| match item {
                TextStreamItem::Event(event) => Some(TextStreamEventKind::from(event)),
                TextStreamItem::ScopedEvent(_)
                | TextStreamItem::Finished(_)
                | TextStreamItem::Error(_) => None,
            })
            .collect()
    }

    /// Return scoped non-runtime events in delivery order.
    #[must_use]
    pub fn scoped_events(&self) -> Vec<&ScopedTurnEvent> {
        self.items
            .iter()
            .filter_map(|item| match item {
                TextStreamItem::ScopedEvent(event) => Some(event),
                TextStreamItem::Event(_)
                | TextStreamItem::Finished(_)
                | TextStreamItem::Error(_) => None,
            })
            .collect()
    }

    /// Assert exact normalized runtime event ordering.
    ///
    /// # Errors
    ///
    /// Returns both expected and observed event sequences when they differ.
    pub fn assert_event_order(
        &self,
        expected: &[AgentEvent],
    ) -> Result<(), TextStreamAssertionError> {
        let actual = self.events();
        if actual == expected {
            Ok(())
        } else {
            Err(TextStreamAssertionError::EventOrder {
                expected: expected.to_vec(),
                actual,
            })
        }
    }

    /// Assert normalized runtime event discriminants in exact order.
    ///
    /// # Errors
    ///
    /// Returns both expected and observed kind sequences when they differ.
    pub fn assert_event_kind_order(
        &self,
        expected: &[TextStreamEventKind],
    ) -> Result<(), TextStreamAssertionError> {
        let actual = self.event_kinds();
        if actual == expected {
            Ok(())
        } else {
            Err(TextStreamAssertionError::EventKindOrder {
                expected: expected.to_vec(),
                actual,
            })
        }
    }

    /// Assert exhaustion, exactly one terminal item, and terminal-last delivery.
    ///
    /// # Errors
    ///
    /// Returns a precise lifecycle-coherence error when the recording is partial or malformed.
    pub fn assert_terminal_coherence(&self) -> Result<(), TextStreamAssertionError> {
        if !self.exhausted {
            return Err(TextStreamAssertionError::NotExhausted);
        }
        let terminal_indices = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                matches!(item, TextStreamItem::Finished(_) | TextStreamItem::Error(_))
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        match terminal_indices.as_slice() {
            [] => Err(TextStreamAssertionError::MissingTerminal),
            [index] if *index + 1 == self.items.len() => Ok(()),
            [index] => Err(TextStreamAssertionError::TerminalNotLast {
                terminal_index: *index,
                item_count: self.items.len(),
            }),
            indices => Err(TextStreamAssertionError::MultipleTerminals {
                count: indices.len(),
            }),
        }
    }

    /// Assert coherent successful completion and return the final response.
    ///
    /// # Errors
    ///
    /// Returns an error for partial, incoherent, or error-terminal recordings.
    pub fn assert_finished(&self) -> Result<&GenerateTextResponse, TextStreamAssertionError> {
        self.assert_terminal_coherence()?;
        match self.items.last() {
            Some(TextStreamItem::Finished(response)) => Ok(response),
            Some(TextStreamItem::Error(error)) => Err(TextStreamAssertionError::ExpectedFinished {
                message: error.to_string(),
            }),
            Some(TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_)) | None => {
                unreachable!("coherence requires terminal last")
            }
        }
    }

    /// Assert coherent error completion and return the typed runtime error.
    ///
    /// # Errors
    ///
    /// Returns an error for partial, incoherent, successful, or non-runtime error recordings.
    pub fn assert_runtime_error(&self) -> Result<&RuntimeError, TextStreamAssertionError> {
        self.assert_terminal_coherence()?;
        match self.items.last() {
            Some(TextStreamItem::Error(BcodeError::Runtime(error))) => Ok(error),
            Some(TextStreamItem::Error(error)) => {
                Err(TextStreamAssertionError::ExpectedRuntimeErrorKind {
                    message: error.to_string(),
                })
            }
            Some(TextStreamItem::Finished(_)) => {
                Err(TextStreamAssertionError::ExpectedRuntimeError)
            }
            Some(TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_)) | None => {
                unreachable!("coherence requires terminal last")
            }
        }
    }

    /// Assert coherent cancellation completion.
    ///
    /// # Errors
    ///
    /// Returns an error unless the terminal runtime error is [`RuntimeError::Cancelled`].
    pub fn assert_cancelled(&self) -> Result<(), TextStreamAssertionError> {
        let error = self.assert_runtime_error()?;
        if matches!(error, RuntimeError::Cancelled) {
            Ok(())
        } else {
            Err(TextStreamAssertionError::ExpectedCancellation {
                actual: format!("{error:?}"),
            })
        }
    }

    /// Assert deterministic bounded-buffer overflow at `capacity`.
    ///
    /// # Errors
    ///
    /// Returns an error unless the terminal runtime error is
    /// [`RuntimeError::StreamBufferFull`] with the exact configured capacity.
    pub fn assert_backpressure_overflow(
        &self,
        capacity: usize,
    ) -> Result<(), TextStreamAssertionError> {
        let error = self.assert_runtime_error()?;
        if matches!(error, RuntimeError::StreamBufferFull { capacity: actual } if *actual == capacity)
        {
            Ok(())
        } else {
            Err(TextStreamAssertionError::ExpectedBackpressure {
                expected_capacity: capacity,
                actual: format!("{error:?}"),
            })
        }
    }
}

/// Consume one high-level stream to exhaustion and record its exact item order.
pub async fn record_text_stream(stream: TextStream) -> TextStreamTranscript {
    TextStreamRecorder::new(stream).finish().await
}

/// One deterministic inline-tool invocation outcome.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedToolOutcome {
    /// Return this complete application-visible invocation response.
    Response(ToolInvocationResponse),
    /// Fail the inline tool handler with this application message.
    Error(String),
    /// Wait on Tokio's clock before returning another outcome.
    Delay {
        /// Delay duration.
        duration: Duration,
        /// Outcome returned after the delay.
        outcome: Box<Self>,
    },
    /// Remain pending until the canonical invocation scope is cancelled.
    PendingUntilCancelled,
}

impl ScriptedToolOutcome {
    /// Create a successful text response.
    #[must_use]
    pub fn text(output: impl Into<String>) -> Self {
        Self::Response(ToolInvocationResponse {
            output: output.into(),
            is_error: false,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        })
    }

    /// Delay this outcome using Tokio's clock.
    #[must_use]
    pub fn after(self, duration: Duration) -> Self {
        Self::Delay {
            duration,
            outcome: Box::new(self),
        }
    }
}

/// Captured request passed to a scripted inline tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedToolInvocation {
    /// Zero-based invocation order.
    pub sequence: usize,
    /// Complete canonical invocation descriptor.
    pub request: ToolInvocationDescriptor,
}

/// Read-only probe for scripted inline tool invocations.
#[derive(Debug, Clone)]
pub struct ScriptedToolProbe {
    state: Arc<Mutex<ScriptedToolState>>,
}

impl ScriptedToolProbe {
    /// Snapshot invocations in start order.
    #[must_use]
    pub fn invocations(&self) -> Vec<CapturedToolInvocation> {
        lock_unpoisoned(&self.state).invocations.clone()
    }

    /// Return how many scripted invocations started.
    #[must_use]
    pub fn invocation_count(&self) -> usize {
        lock_unpoisoned(&self.state).invocations.len()
    }
}

/// Finite deterministic inline-tool script.
#[derive(Debug, Clone)]
pub struct ScriptedTool {
    state: Arc<Mutex<ScriptedToolState>>,
}

#[derive(Debug)]
struct ScriptedToolState {
    outcomes: VecDeque<ScriptedToolOutcome>,
    invocations: Vec<CapturedToolInvocation>,
}

impl ScriptedTool {
    /// Create a scripted tool from outcomes consumed in invocation order.
    #[must_use]
    pub fn new(outcomes: impl IntoIterator<Item = ScriptedToolOutcome>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ScriptedToolState {
                outcomes: outcomes.into_iter().collect(),
                invocations: Vec::new(),
            })),
        }
    }

    /// Return a read-only invocation probe.
    #[must_use]
    pub fn probe(&self) -> ScriptedToolProbe {
        ScriptedToolProbe {
            state: Arc::clone(&self.state),
        }
    }

    /// Register this script as one scoped inline tool.
    #[must_use]
    pub fn register(self, builder: AgentBuilder, definition: ToolDefinition) -> AgentBuilder {
        let state = Arc::clone(&self.state);
        builder.scoped_inline_tool(definition, move |request, scope| {
            let state = Arc::clone(&state);
            async move {
                let outcome = {
                    let mut state = lock_unpoisoned(&state);
                    let sequence = state.invocations.len();
                    state
                        .invocations
                        .push(CapturedToolInvocation { sequence, request });
                    state.outcomes.pop_front()
                };
                run_scripted_tool_outcome(outcome, scope.cancellation()).await
            }
        })
    }
}

fn run_scripted_tool_outcome(
    outcome: Option<ScriptedToolOutcome>,
    cancellation: CancellationToken,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = std::result::Result<ToolInvocationResponse, String>>
            + Send,
    >,
> {
    Box::pin(async move {
        match outcome {
            Some(ScriptedToolOutcome::Response(response)) => Ok(response),
            Some(ScriptedToolOutcome::Error(message)) => Err(message),
            Some(ScriptedToolOutcome::Delay { duration, outcome }) => {
                tokio::select! {
                    () = cancellation.cancelled() => Err("scripted tool cancelled".to_string()),
                    () = tokio::time::sleep(duration) => {
                        run_scripted_tool_outcome(Some(*outcome), cancellation).await
                    }
                }
            }
            Some(ScriptedToolOutcome::PendingUntilCancelled) => {
                cancellation.cancelled().await;
                Err("scripted tool cancelled".to_string())
            }
            None => Err("scripted tool exhausted".to_string()),
        }
    })
}

/// Finite deterministic permission-decision script.
#[derive(Debug, Clone)]
pub struct ScriptedPermissionPolicy {
    state: Arc<Mutex<ScriptedPermissionState>>,
}

#[derive(Debug)]
struct ScriptedPermissionState {
    decisions: VecDeque<PermissionDecision>,
    requests: Vec<RuntimePermissionRequest>,
}

impl ScriptedPermissionPolicy {
    /// Create a policy from decisions consumed in evaluation order.
    #[must_use]
    pub fn new(decisions: impl IntoIterator<Item = PermissionDecision>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ScriptedPermissionState {
                decisions: decisions.into_iter().collect(),
                requests: Vec::new(),
            })),
        }
    }

    /// Snapshot complete permission requests in evaluation order.
    #[must_use]
    pub fn requests(&self) -> Vec<RuntimePermissionRequest> {
        lock_unpoisoned(&self.state).requests.clone()
    }
}

impl PermissionPolicy for ScriptedPermissionPolicy {
    fn evaluate_tool_call<'a>(
        &'a self,
        request: &'a RuntimePermissionRequest,
    ) -> RuntimeFuture<'a, PermissionDecision> {
        let decision = {
            let mut state = lock_unpoisoned(&self.state);
            state.requests.push(request.clone());
            state.decisions.pop_front().unwrap_or_else(|| {
                PermissionDecision::Deny("scripted permission decisions exhausted".to_string())
            })
        };
        Box::pin(async move { Ok(decision) })
    }
}

/// One deterministic cache operation kind.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptedCacheOperation {
    /// Cache lookup.
    Get,
    /// Cache storage.
    Put,
    /// Miss-reservation abort.
    Abort,
    /// Exact-key invalidation.
    Invalidate,
}

/// Application-owned deterministic response-cache fixture with failure injection.
#[derive(Debug, Clone)]
pub struct ScriptedModelResponseCache {
    state: Arc<Mutex<ScriptedCacheState>>,
    privacy: ModelResponseCachePrivacy,
    allow_tool_responses: bool,
}

#[derive(Debug, Default)]
struct ScriptedCacheState {
    response: Option<GenerateTextResponse>,
    operations: Vec<ScriptedCacheOperation>,
    fail_next: Option<(ScriptedCacheOperation, String)>,
}

impl ScriptedModelResponseCache {
    /// Create an empty private cache fixture.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ScriptedCacheState::default())),
            privacy: ModelResponseCachePrivacy::Private,
            allow_tool_responses: false,
        }
    }

    /// Preload one complete response returned by subsequent lookups.
    #[must_use]
    pub fn with_response(self, response: GenerateTextResponse) -> Self {
        lock_unpoisoned(&self.state).response = Some(response);
        self
    }

    /// Configure cache privacy behavior.
    #[must_use]
    pub const fn with_privacy(mut self, privacy: ModelResponseCachePrivacy) -> Self {
        self.privacy = privacy;
        self
    }

    /// Configure whether tool-advertising requests may be cached.
    #[must_use]
    pub const fn with_tool_responses(mut self, allow: bool) -> Self {
        self.allow_tool_responses = allow;
        self
    }

    /// Fail the next matching operation with one safe message.
    pub fn fail_next(&self, operation: ScriptedCacheOperation, message: impl Into<String>) {
        lock_unpoisoned(&self.state).fail_next = Some((operation, message.into()));
    }

    /// Snapshot cache operations in call order.
    #[must_use]
    pub fn operations(&self) -> Vec<ScriptedCacheOperation> {
        lock_unpoisoned(&self.state).operations.clone()
    }

    fn operation(&self, operation: ScriptedCacheOperation) -> crate::Result<()> {
        let mut state = lock_unpoisoned(&self.state);
        state.operations.push(operation);
        if state
            .fail_next
            .as_ref()
            .is_some_and(|(expected, _)| *expected == operation)
        {
            let (_, message) = state.fail_next.take().expect("matched operation failure");
            drop(state);
            return Err(BcodeError::Cache(message));
        }
        drop(state);
        Ok(())
    }
}

impl Default for ScriptedModelResponseCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelResponseCache for ScriptedModelResponseCache {
    fn get(&self, _request: &AgentTurnRequest) -> crate::Result<Option<GenerateTextResponse>> {
        self.operation(ScriptedCacheOperation::Get)?;
        Ok(lock_unpoisoned(&self.state).response.clone())
    }

    fn privacy(&self, _request: &AgentTurnRequest) -> ModelResponseCachePrivacy {
        self.privacy
    }

    fn allow_tool_responses(&self) -> bool {
        self.allow_tool_responses
    }

    fn put(
        &self,
        _request: &AgentTurnRequest,
        response: &GenerateTextResponse,
    ) -> crate::Result<()> {
        self.operation(ScriptedCacheOperation::Put)?;
        lock_unpoisoned(&self.state).response = Some(response.clone());
        Ok(())
    }

    fn abort(&self, _request: &AgentTurnRequest) {
        let _ = self.operation(ScriptedCacheOperation::Abort);
    }

    fn invalidate(&self, _request: &AgentTurnRequest) -> crate::Result<()> {
        self.operation(ScriptedCacheOperation::Invalidate)?;
        lock_unpoisoned(&self.state).response = None;
        Ok(())
    }
}

/// Application-owned deterministic session store with failure injection.
#[derive(Debug, Clone, Default)]
pub struct ScriptedSessionStore {
    state: Arc<Mutex<ScriptedSessionStoreState>>,
}

#[derive(Debug, Default)]
struct ScriptedSessionStoreState {
    session: Option<PersistedSession>,
    loads: usize,
    saves: Vec<PersistedSession>,
    load_failure: Option<String>,
    save_failure: Option<String>,
}

impl ScriptedSessionStore {
    /// Create an empty session store fixture.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Preload one persisted session.
    #[must_use]
    pub fn with_session(self, session: PersistedSession) -> Self {
        lock_unpoisoned(&self.state).session = Some(session);
        self
    }

    /// Fail subsequent loads until replaced or cleared.
    pub fn set_load_failure(&self, message: Option<String>) {
        lock_unpoisoned(&self.state).load_failure = message;
    }

    /// Fail subsequent saves until replaced or cleared.
    pub fn set_save_failure(&self, message: Option<String>) {
        lock_unpoisoned(&self.state).save_failure = message;
    }

    /// Return the load-call count.
    #[must_use]
    pub fn load_count(&self) -> usize {
        lock_unpoisoned(&self.state).loads
    }

    /// Snapshot successful save payloads in call order.
    #[must_use]
    pub fn saves(&self) -> Vec<PersistedSession> {
        lock_unpoisoned(&self.state).saves.clone()
    }
}

impl SessionPersistenceAdapter for ScriptedSessionStore {
    fn load(&self) -> crate::Result<Option<PersistedSession>> {
        let mut state = lock_unpoisoned(&self.state);
        state.loads = state.loads.saturating_add(1);
        if let Some(message) = &state.load_failure {
            return Err(BcodeError::SessionPersistence(message.clone()));
        }
        Ok(state.session.clone())
    }

    fn save(&self, session: &PersistedSession) -> crate::Result<()> {
        let mut state = lock_unpoisoned(&self.state);
        if let Some(message) = &state.save_failure {
            return Err(BcodeError::SessionPersistence(message.clone()));
        }
        state.session = Some(session.clone());
        state.saves.push(session.clone());
        drop(state);
        Ok(())
    }
}

/// Manually advanced deterministic clock for application test doubles.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    state: Arc<Mutex<ManualClockState>>,
    changed: Arc<Notify>,
}

#[derive(Debug, Default)]
struct ManualClockState {
    now: Duration,
}

impl ManualClock {
    /// Create a clock at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return current monotonic test time.
    #[must_use]
    pub fn now(&self) -> Duration {
        lock_unpoisoned(&self.state).now
    }

    /// Advance monotonic test time and wake sleepers.
    pub fn advance(&self, duration: Duration) {
        let mut state = lock_unpoisoned(&self.state);
        state.now = state.now.saturating_add(duration);
        drop(state);
        self.changed.notify_waiters();
    }

    /// Wait until current test time reaches `deadline`.
    pub async fn sleep_until(&self, deadline: Duration) {
        loop {
            let notified = self.changed.notified();
            if self.now() >= deadline {
                return;
            }
            notified.await;
        }
    }

    /// Wait for one relative test duration.
    pub async fn sleep(&self, duration: Duration) {
        self.sleep_until(self.now().saturating_add(duration)).await;
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
