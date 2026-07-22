//! Deterministic provider utilities for SDK tests.
//!
//! [`ScriptedProvider`] implements the same [`ModelProviderInvoker`]
//! boundary as production providers. Scripts are finite, ordered, and network-free. Raw event
//! batches deliberately permit malformed provider output so applications can exercise validation
//! and failure paths as well as successful responses.

use crate::{ModelProviderInvoker, RuntimeError, RuntimeFuture};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelMessage, ModelParameters,
    ModelTurnRequest, PollTurnEventsRequest, PollTurnEventsResponse, ProviderError,
    ProviderErrorCategory, ProviderTurnEvent, StartTurnResponse, StopReason,
    StructuredOutputRequest, ToolDefinition,
};
use std::collections::{BTreeMap, VecDeque};
use std::future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// One poll operation in a scripted provider turn.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedProviderAction {
    /// Return this exact ordered event batch from one poll.
    Events(Vec<ProviderTurnEvent>),
    /// Wait for the duration and return an empty event batch.
    ///
    /// The delay uses Tokio's clock, so applications can use Tokio's optional paused-time test
    /// support when it is enabled in their own test dependency configuration.
    Delay(Duration),
    /// Fail the poll operation before returning provider events.
    PollError(ProviderError),
    /// Keep the poll operation pending until its caller cancels or times it out.
    Pending,
}

/// One provider turn consumed by [`ScriptedProvider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedProviderTurn {
    start_error: Option<ProviderError>,
    actions: VecDeque<ScriptedProviderAction>,
    cancel_error: Option<ProviderError>,
    finish_error: Option<ProviderError>,
}

impl Default for ScriptedProviderTurn {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedProviderTurn {
    /// Create an empty turn script.
    ///
    /// An exhausted nonterminal script remains pending rather than busy-spinning. Successful
    /// scripts should therefore include a terminal event.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            start_error: None,
            actions: VecDeque::new(),
            cancel_error: None,
            finish_error: None,
        }
    }

    /// Create a complete text response with canonical lifecycle events.
    #[must_use]
    pub fn complete_text(text: impl Into<String>) -> Self {
        Self::new().events([
            ProviderTurnEvent::TurnStarted,
            ProviderTurnEvent::TextDelta { text: text.into() },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ])
    }

    /// Create a turn that emits a normalized provider error.
    #[must_use]
    pub fn provider_error(error: ProviderError) -> Self {
        Self::new().events([
            ProviderTurnEvent::TurnStarted,
            ProviderTurnEvent::Error { error },
        ])
    }

    /// Create a turn whose start operation fails.
    #[must_use]
    pub fn start_error(error: ProviderError) -> Self {
        Self {
            start_error: Some(error),
            ..Self::new()
        }
    }

    /// Append one exact event batch.
    ///
    /// No lifecycle normalization is performed. This is intentional: callers can script usage,
    /// warnings, metadata, tool calls, malformed output, duplicate terminals, or any other event
    /// sequence accepted by the public provider contract.
    #[must_use]
    pub fn events(mut self, events: impl IntoIterator<Item = ProviderTurnEvent>) -> Self {
        self.actions
            .push_back(ScriptedProviderAction::Events(events.into_iter().collect()));
        self
    }

    /// Append a deterministic Tokio-clock delay.
    #[must_use]
    pub fn delay(mut self, delay: Duration) -> Self {
        self.actions.push_back(ScriptedProviderAction::Delay(delay));
        self
    }

    /// Append a poll-operation failure.
    #[must_use]
    pub fn poll_error(mut self, error: ProviderError) -> Self {
        self.actions
            .push_back(ScriptedProviderAction::PollError(error));
        self
    }

    /// Append a poll that remains pending until cancellation or timeout.
    #[must_use]
    pub fn pending(mut self) -> Self {
        self.actions.push_back(ScriptedProviderAction::Pending);
        self
    }

    /// Configure the provider cleanup cancellation operation to fail.
    #[must_use]
    pub fn cancel_error(mut self, error: ProviderError) -> Self {
        self.cancel_error = Some(error);
        self
    }

    /// Configure the provider finish operation to fail.
    #[must_use]
    pub fn finish_error(mut self, error: ProviderError) -> Self {
        self.finish_error = Some(error);
        self
    }
}

/// One captured provider request and its selected provider plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedProviderRequest {
    /// Zero-based start order across this scripted provider.
    pub sequence: usize,
    /// Provider plugin selected by routing, if any.
    pub provider_plugin_id: Option<String>,
    /// Complete post-middleware request received by the provider.
    pub request: ModelTurnRequest,
}

/// Expected fields for one captured provider request.
///
/// Unconfigured fields are ignored. Configured fields use exact equality, making continuation and
/// middleware assertions concise without hiding the complete captured request.
#[derive(Debug, Clone, Default, PartialEq)]
enum Expected<T> {
    #[default]
    Ignored,
    Value(T),
}

/// Exact, opt-in field expectations for one captured provider request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScriptedRequestExpectation {
    provider_plugin_id: Expected<Option<String>>,
    model_id: Expected<String>,
    messages: Expected<Vec<ModelMessage>>,
    tools: Expected<Vec<ToolDefinition>>,
    structured_output: Expected<Option<StructuredOutputRequest>>,
    parameters: Expected<ModelParameters>,
    metadata: Expected<BTreeMap<String, String>>,
}

impl ScriptedRequestExpectation {
    /// Create an expectation that initially accepts every request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Require the selected provider plugin ID.
    #[must_use]
    pub fn provider_plugin_id(mut self, provider_plugin_id: impl Into<String>) -> Self {
        self.provider_plugin_id = Expected::Value(Some(provider_plugin_id.into()));
        self
    }

    /// Require that routing selected no provider plugin ID.
    #[must_use]
    pub fn without_provider_plugin_id(mut self) -> Self {
        self.provider_plugin_id = Expected::Value(None);
        self
    }

    /// Require the selected model ID.
    #[must_use]
    pub fn model_id(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = Expected::Value(model_id.into());
        self
    }

    /// Require the complete ordered provider message list.
    #[must_use]
    pub fn messages(mut self, messages: impl IntoIterator<Item = ModelMessage>) -> Self {
        self.messages = Expected::Value(messages.into_iter().collect());
        self
    }

    /// Require the complete ordered tool definition list.
    #[must_use]
    pub fn tools(mut self, tools: impl IntoIterator<Item = ToolDefinition>) -> Self {
        self.tools = Expected::Value(tools.into_iter().collect());
        self
    }

    /// Require this structured-output request.
    #[must_use]
    pub fn structured_output(mut self, structured_output: StructuredOutputRequest) -> Self {
        self.structured_output = Expected::Value(Some(structured_output));
        self
    }

    /// Require that no structured-output request is present.
    #[must_use]
    pub fn without_structured_output(mut self) -> Self {
        self.structured_output = Expected::Value(None);
        self
    }

    /// Require the complete model parameter set.
    #[must_use]
    pub fn parameters(mut self, parameters: ModelParameters) -> Self {
        self.parameters = Expected::Value(parameters);
        self
    }

    /// Require the complete application metadata map.
    #[must_use]
    pub fn metadata(mut self, metadata: BTreeMap<String, String>) -> Self {
        self.metadata = Expected::Value(metadata);
        self
    }

    fn assert_matches(
        &self,
        index: usize,
        captured: &CapturedProviderRequest,
    ) -> Result<(), ScriptedProviderAssertionError> {
        assert_optional_field(
            index,
            "provider_plugin_id",
            &self.provider_plugin_id,
            &captured.provider_plugin_id,
        )?;
        assert_optional_field(
            index,
            "model_id",
            &self.model_id,
            &captured.request.model_id,
        )?;
        assert_optional_field(
            index,
            "messages",
            &self.messages,
            &captured.request.messages,
        )?;
        assert_optional_field(index, "tools", &self.tools, &captured.request.tools)?;
        assert_optional_field(
            index,
            "structured_output",
            &self.structured_output,
            &captured.request.structured_output,
        )?;
        assert_optional_field(
            index,
            "parameters",
            &self.parameters,
            &captured.request.parameters,
        )?;
        assert_optional_field(
            index,
            "metadata",
            &self.metadata,
            &captured.request.metadata,
        )
    }
}

fn assert_optional_field<T: std::fmt::Debug + PartialEq>(
    index: usize,
    field: &'static str,
    expected: &Expected<T>,
    actual: &T,
) -> Result<(), ScriptedProviderAssertionError> {
    if let Expected::Value(expected) = expected
        && expected != actual
    {
        return Err(ScriptedProviderAssertionError::FieldMismatch {
            request_index: index,
            field,
            expected: format!("{expected:?}"),
            actual: format!("{actual:?}"),
        });
    }
    Ok(())
}

/// Failure from a scripted-provider request or lifecycle assertion.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptedProviderAssertionError {
    /// The number of observed calls did not match.
    #[error("expected {expected} {operation} call(s), observed {actual}")]
    CallCount {
        /// Lifecycle operation being checked.
        operation: &'static str,
        /// Expected call count.
        expected: usize,
        /// Observed call count.
        actual: usize,
    },
    /// One exact request field did not match.
    #[error(
        "request {request_index} field {field} mismatch: expected {expected}, observed {actual}"
    )]
    FieldMismatch {
        /// Zero-based captured request index.
        request_index: usize,
        /// Field being compared.
        field: &'static str,
        /// Debug representation of the expected value.
        expected: String,
        /// Debug representation of the observed value.
        actual: String,
    },
}

/// Read-only probe for captured scripted-provider interactions.
#[derive(Debug, Clone)]
pub struct ScriptedProviderProbe {
    state: Arc<Mutex<ScriptedProviderState>>,
}

impl ScriptedProviderProbe {
    /// Snapshot all captured requests in start order.
    #[must_use]
    pub fn requests(&self) -> Vec<CapturedProviderRequest> {
        lock_state(&self.state).requests.clone()
    }

    /// Snapshot provider turn IDs passed to cancellation, in call order.
    #[must_use]
    pub fn cancellations(&self) -> Vec<String> {
        lock_state(&self.state).cancellations.clone()
    }

    /// Snapshot provider turn IDs passed to finish, in call order.
    #[must_use]
    pub fn finishes(&self) -> Vec<String> {
        lock_state(&self.state).finishes.clone()
    }

    /// Assert the exact request count and selected fields for every request.
    ///
    /// # Errors
    ///
    /// Returns a field-aware error when the count or any configured expectation differs.
    pub fn assert_requests(
        &self,
        expected: &[ScriptedRequestExpectation],
    ) -> Result<(), ScriptedProviderAssertionError> {
        let actual = self.requests();
        if actual.len() != expected.len() {
            return Err(ScriptedProviderAssertionError::CallCount {
                operation: "request",
                expected: expected.len(),
                actual: actual.len(),
            });
        }
        for (index, (expectation, captured)) in expected.iter().zip(&actual).enumerate() {
            expectation.assert_matches(index, captured)?;
        }
        Ok(())
    }

    /// Assert how many provider cancellation calls occurred.
    ///
    /// # Errors
    ///
    /// Returns an error when the count differs.
    pub fn assert_cancellation_count(
        &self,
        expected: usize,
    ) -> Result<(), ScriptedProviderAssertionError> {
        assert_call_count("cancellation", expected, self.cancellations().len())
    }

    /// Assert how many provider finish calls occurred.
    ///
    /// # Errors
    ///
    /// Returns an error when the count differs.
    pub fn assert_finish_count(
        &self,
        expected: usize,
    ) -> Result<(), ScriptedProviderAssertionError> {
        assert_call_count("finish", expected, self.finishes().len())
    }
}

const fn assert_call_count(
    operation: &'static str,
    expected: usize,
    actual: usize,
) -> Result<(), ScriptedProviderAssertionError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ScriptedProviderAssertionError::CallCount {
            operation,
            expected,
            actual,
        })
    }
}

/// Deterministic in-process implementation of the production provider boundary.
///
/// Clones share scripts and captures, which allows a probe to outlive a provider moved into an
/// SDK stream or provider factory. Each `start_turn` consumes exactly one configured turn.
#[derive(Debug, Clone)]
pub struct ScriptedProvider {
    state: Arc<Mutex<ScriptedProviderState>>,
}

#[derive(Debug)]
struct ScriptedProviderState {
    turns: VecDeque<ScriptedProviderTurn>,
    active: BTreeMap<String, ActiveScriptedTurn>,
    requests: Vec<CapturedProviderRequest>,
    cancellations: Vec<String>,
    finishes: Vec<String>,
    next_turn_id: u64,
}

#[derive(Debug)]
struct ActiveScriptedTurn {
    actions: VecDeque<ScriptedProviderAction>,
    cancel_error: Option<ProviderError>,
    finish_error: Option<ProviderError>,
}

impl ScriptedProvider {
    /// Create a provider from turns consumed in start order.
    #[must_use]
    pub fn new(turns: impl IntoIterator<Item = ScriptedProviderTurn>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ScriptedProviderState {
                turns: turns.into_iter().collect(),
                active: BTreeMap::new(),
                requests: Vec::new(),
                cancellations: Vec::new(),
                finishes: Vec::new(),
                next_turn_id: 0,
            })),
        }
    }

    /// Return a read-only probe sharing this provider's captures.
    #[must_use]
    pub fn probe(&self) -> ScriptedProviderProbe {
        ScriptedProviderProbe {
            state: Arc::clone(&self.state),
        }
    }
}

impl ModelProviderInvoker for ScriptedProvider {
    fn start_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        let result = {
            let mut state = lock_state(&self.state);
            let sequence = state.requests.len();
            state.requests.push(CapturedProviderRequest {
                sequence,
                provider_plugin_id: provider_plugin_id.map(str::to_string),
                request: request.clone(),
            });
            let turn = state.turns.pop_front();
            let result = if let Some(mut turn) = turn {
                if let Some(error) = turn.start_error.take() {
                    Err(runtime_provider_error(error))
                } else {
                    state.next_turn_id = state.next_turn_id.saturating_add(1);
                    let provider_turn_id = format!("scripted-turn-{}", state.next_turn_id);
                    state.active.insert(
                        provider_turn_id.clone(),
                        ActiveScriptedTurn {
                            actions: turn.actions,
                            cancel_error: turn.cancel_error,
                            finish_error: turn.finish_error,
                        },
                    );
                    Ok(StartTurnResponse { provider_turn_id })
                }
            } else {
                Err(script_exhausted_error())
            };
            drop(state);
            result
        };
        Box::pin(async move { result })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        let action = {
            let mut state = lock_state(&self.state);
            state.active.get_mut(&request.provider_turn_id).map_or_else(
                || Err(unknown_turn_error(&request.provider_turn_id)),
                |turn| {
                    Ok(turn
                        .actions
                        .pop_front()
                        .unwrap_or(ScriptedProviderAction::Pending))
                },
            )
        };
        Box::pin(async move {
            match action? {
                ScriptedProviderAction::Events(events) => Ok(PollTurnEventsResponse { events }),
                ScriptedProviderAction::Delay(delay) => {
                    tokio::time::sleep(delay).await;
                    Ok(PollTurnEventsResponse { events: Vec::new() })
                }
                ScriptedProviderAction::PollError(error) => Err(runtime_provider_error(error)),
                ScriptedProviderAction::Pending => future::pending().await,
            }
        })
    }

    fn cancel_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        let result = {
            let mut state = lock_state(&self.state);
            state.cancellations.push(request.provider_turn_id.clone());
            state
                .active
                .get(&request.provider_turn_id)
                .and_then(|turn| turn.cancel_error.clone())
                .map_or_else(
                    || Ok(AckResponse::default()),
                    |error| Err(runtime_provider_error(error)),
                )
        };
        Box::pin(async move { result })
    }

    fn finish_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        let result = {
            let mut state = lock_state(&self.state);
            state.finishes.push(request.provider_turn_id.clone());
            state
                .active
                .remove(&request.provider_turn_id)
                .and_then(|turn| turn.finish_error)
                .map_or_else(
                    || Ok(AckResponse::default()),
                    |error| Err(runtime_provider_error(error)),
                )
        };
        Box::pin(async move { result })
    }
}

fn lock_state(state: &Arc<Mutex<ScriptedProviderState>>) -> MutexGuard<'_, ScriptedProviderState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn runtime_provider_error(error: ProviderError) -> RuntimeError {
    RuntimeError::Provider {
        code: error.code.clone(),
        message: error.message.clone(),
        error: Box::new(error),
    }
}

fn script_exhausted_error() -> RuntimeError {
    runtime_provider_error(ProviderError {
        code: "script_exhausted".to_string(),
        category: ProviderErrorCategory::Config,
        message: "scripted provider has no remaining turn".to_string(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    })
}

fn unknown_turn_error(provider_turn_id: &str) -> RuntimeError {
    runtime_provider_error(ProviderError {
        code: "unknown_scripted_turn".to_string(),
        category: ProviderErrorCategory::InvalidRequest,
        message: format!("scripted provider turn {provider_turn_id} is not active"),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    })
}
