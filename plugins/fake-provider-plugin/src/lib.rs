#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Fake model provider plugin for deterministic tests and smoke flows.

use bcode_model::{
    AckResponse, CancelTurnRequest, CompactContextRequest, CompactContextResponse, ContentBlock,
    ContextManagementCapabilities, ContextManagementCapabilitiesRequest, FinishTurnRequest,
    MODEL_PROVIDER_INTERFACE_ID, MODEL_PROVIDER_INTERFACE_ID_V2, MessageRole, ModelCapability,
    ModelInfo, ModelList, ModelListRequest, ModelMessage, ModelTurnRequest, NativeWebSearchRequest,
    NativeWebSearchResponse, NativeWebSearchResult, OP_CANCEL_TURN, OP_CAPABILITIES,
    OP_COMPACT_CONTEXT, OP_CONTEXT_MANAGEMENT_CAPABILITIES, OP_FINISH_TURN, OP_MODELS,
    OP_NATIVE_WEB_SEARCH, OP_POLL_TURN_EVENTS, OP_START_TURN, OP_VALIDATE_CONFIG,
    PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities, ProviderCapability,
    ProviderContextFormat, ProviderError, ProviderErrorCategory, ProviderTurnEvent,
    StartTurnResponse, StopReason, TokenUsage, ToolCall, ToolChoice, ValidateConfigRequest,
    ValidateConfigResponse,
};
use bcode_model_provider_runtime::ProviderOutputPositionAllocator;
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::Duration;

static FAKE_COMPACTION_STARTED: AtomicBool = AtomicBool::new(false);
static FAKE_COMPACTION_SUMMARY_STARTED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "static-bundled")]
static FAKE_COMPACTION_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));
static FAKE_COMPACTION_SUMMARY_SIGNALS: LazyLock<Mutex<BTreeSet<String>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));
static FAKE_SCRIPT_GATES: LazyLock<(Mutex<BTreeSet<String>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(BTreeSet::new()), Condvar::new()));

const FAKE_EVENT_SCRIPT_SETTING: &str = "fake_event_script";

/// One deterministic fake-provider event script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeProviderEventScript {
    /// Ordered provider events and release controls.
    pub steps: Vec<FakeProviderEventScriptStep>,
}

/// One event in a deterministic fake-provider script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeProviderEventScriptStep {
    /// Optional process-local gate that must be released before this event is emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// Optional deterministic delay after gate release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// Provider-neutral event emitted by this step.
    pub event: ProviderTurnEvent,
}

/// Release one process-local fake-provider script gate.
#[cfg(feature = "static-bundled")]
pub fn release_fake_event_script_gate(gate: &str) {
    let (gates, ready) = &*FAKE_SCRIPT_GATES;
    if let Ok(mut gates) = gates.lock() {
        gates.insert(gate.to_owned());
        ready.notify_all();
    }
}

/// Reset every process-local fake-provider script gate.
#[cfg(feature = "static-bundled")]
pub fn reset_fake_event_script_gates() {
    let (gates, _) = &*FAKE_SCRIPT_GATES;
    if let Ok(mut gates) = gates.lock() {
        gates.clear();
    }
}
static FAKE_MANAGED_COMPACTION_EMITTED: AtomicBool = AtomicBool::new(false);
static FAKE_LAST_PARALLEL_TOOL_POLICY: AtomicBool = AtomicBool::new(false);

/// Acquire exclusive access to process-wide fake compaction test signals.
#[cfg(feature = "static-bundled")]
pub async fn fake_compaction_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    FAKE_COMPACTION_TEST_LOCK.lock().await
}

/// Reset the provider-compaction start signal used by static runtime tests.
#[cfg(feature = "static-bundled")]
pub fn reset_fake_compaction_started() {
    FAKE_COMPACTION_STARTED.store(false, Ordering::Release);
    FAKE_COMPACTION_SUMMARY_STARTED.store(false, Ordering::Release);
    FAKE_MANAGED_COMPACTION_EMITTED.store(false, Ordering::Release);
    FAKE_LAST_PARALLEL_TOOL_POLICY.store(false, Ordering::Release);
}

/// Reset the compaction-summary signal for one test-owned key.
#[cfg(feature = "static-bundled")]
pub fn reset_fake_compaction_summary_signal(key: &str) {
    FAKE_COMPACTION_SUMMARY_SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(key);
}

/// Return whether a keyed fake compaction-summary model turn has started.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn fake_compaction_summary_signal_started(key: &str) -> bool {
    FAKE_COMPACTION_SUMMARY_SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(key)
}

/// Return the last provider-visible parallel tool-call policy observed by the fake adapter.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn fake_last_parallel_tool_policy() -> bool {
    FAKE_LAST_PARALLEL_TOOL_POLICY.load(Ordering::Acquire)
}

/// Return whether fake provider-managed compaction was emitted.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn fake_managed_compaction_emitted() -> bool {
    FAKE_MANAGED_COMPACTION_EMITTED.load(Ordering::Acquire)
}

/// Return whether a fake compaction-summary model turn has started.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn fake_compaction_summary_started() -> bool {
    FAKE_COMPACTION_SUMMARY_STARTED.load(Ordering::Acquire)
}

/// Return whether a fake provider-native compaction call has started.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn fake_compaction_started() -> bool {
    FAKE_COMPACTION_STARTED.load(Ordering::Acquire)
}

/// Deterministic fake model provider.
#[derive(Default)]
pub struct FakeProviderPlugin {
    state: Mutex<FakeProviderState>,
}

#[derive(Debug, Default)]
struct FakeProviderState {
    next_turn: u64,
    tool_rounds_emitted: u64,
    turns: BTreeMap<String, FakeTurn>,
    overflow_emitted: bool,
    max_tokens_emitted: bool,
}

#[derive(Debug, Clone, Default)]
struct FakeTurn {
    events: Arc<Mutex<VecDeque<ProviderTurnEvent>>>,
    output_positions: Arc<Mutex<ProviderOutputPositionAllocator>>,
    positioned_output: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl FakeTurn {
    fn push(&self, event: ProviderTurnEvent) {
        let event = if self.positioned_output.load(Ordering::Acquire) {
            match self.output_positions.lock() {
                Ok(mut positions) => positions.position(event),
                Err(_) => event,
            }
        } else {
            event
        };
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
    }

    fn enable_positioned_output(&self) {
        self.positioned_output.store(true, Ordering::Release);
    }

    fn drain(&self) -> Vec<ProviderTurnEvent> {
        self.events
            .lock()
            .map_or_else(|_| Vec::new(), |mut events| events.drain(..).collect())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut events) = self.events.lock() {
            if events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::TurnFinished { .. }))
            {
                return;
            }
            events.push_back(ProviderTurnEvent::Cancelled);
            events.push_back(ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Cancelled,
            });
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl ConcurrentRustPlugin for FakeProviderPlugin {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_provider_service(&context)
    }
}

impl RustPlugin for FakeProviderPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_provider_service(&context)
    }
}

impl FakeProviderPlugin {
    fn invoke_provider_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        if !matches!(
            context.request.interface_id.as_str(),
            MODEL_PROVIDER_INTERFACE_ID | MODEL_PROVIDER_INTERFACE_ID_V2
        ) {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported model provider service interface",
            );
        }

        match context.request.operation.as_str() {
            OP_CAPABILITIES => json_response(&capabilities()),
            OP_MODELS => Self::models(&context.request),
            OP_VALIDATE_CONFIG => Self::validate_config(&context.request),
            OP_CONTEXT_MANAGEMENT_CAPABILITIES => {
                let request = match context
                    .request
                    .payload_json::<ContextManagementCapabilitiesRequest>()
                {
                    Ok(request) => request,
                    Err(error) => return invalid_request(&error),
                };
                if request
                    .provider_context
                    .settings
                    .get("fake_context_capabilities_failure")
                    .is_some_and(|value| value == "true")
                {
                    return ServiceResponse::error(
                        "capability_discovery_failed",
                        "fake context capability discovery failed",
                    );
                }
                json_response(&fake_context_capabilities(&request))
            }
            OP_COMPACT_CONTEXT => Self::compact_context(&context.request),
            OP_START_TURN => self.start_turn(
                &context.request,
                context.request.interface_id == MODEL_PROVIDER_INTERFACE_ID_V2,
            ),
            OP_POLL_TURN_EVENTS => self.poll_turn_events(&context.request),
            OP_CANCEL_TURN => self.cancel_turn(&context.request),
            OP_FINISH_TURN => self.finish_turn(&context.request),
            OP_NATIVE_WEB_SEARCH => native_web_search(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported model provider operation",
            ),
        }
    }

    fn validate_config(request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<ValidateConfigRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        json_response(&ValidateConfigResponse {
            valid: true,
            message: Some("fake provider is always valid".to_string()),
            failures: Vec::new(),
            metadata: request.provider_context.settings,
        })
    }

    fn models(request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<ModelListRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        json_response(&models(
            request
                .provider_context
                .settings
                .get("fake_unknown_context_window")
                .is_none_or(|value| value != "true"),
        ))
    }

    fn compact_context(request: &ServiceRequest) -> ServiceResponse {
        FAKE_COMPACTION_STARTED.store(true, Ordering::Release);
        let request = match request.payload_json::<CompactContextRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        if let Some(delay_ms) = request
            .provider_context
            .settings
            .get("fake_compaction_delay_ms")
            .and_then(|value| value.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        if request
            .provider_context
            .settings
            .get("fake_compaction_failure")
            .is_some_and(|value| value == "true")
        {
            return ServiceResponse::error("fake_compaction_failed", "requested fake failure");
        }
        let opaque = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ProviderExtension {
                value: serde_json::json!({
                    "type": "fake_compaction",
                    "message_count": request.messages.len(),
                }),
            }],
        };
        json_response(&CompactContextResponse {
            messages: vec![opaque],
            context_format: fake_context_format(),
        })
    }

    fn start_turn(&self, request: &ServiceRequest, positioned_output: bool) -> ServiceResponse {
        let request = match request.payload_json::<ModelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        if let Some(error) = validate_fake_request(&request) {
            return json_response(&StartTurnResponse {
                provider_turn_id: insert_fake_error_turn(&self.state, error),
            });
        }
        if let Some(error) = validate_fake_parallel_tool_policy(&request) {
            return error;
        }
        let event_script = match fake_event_script(&request) {
            Ok(script) => script,
            Err(message) => return ServiceResponse::error("invalid_fake_event_script", message),
        };
        let is_compaction_request = request
            .metadata
            .get("bcode_request_kind")
            .is_some_and(|kind| kind == "compaction");
        if is_compaction_request {
            mark_fake_compaction_summary_started(&request);
        }
        let mut state = self
            .state
            .lock()
            .expect("fake provider state lock should not be poisoned");
        state.next_turn += 1;
        let provider_turn_id = format!("fake-turn-{}", state.next_turn);
        let request_input_tokens = fake_request_input_tokens(&request);
        let user_text = last_user_text(&request.messages);
        let tool_result = last_tool_result(&request.messages);
        let tool_call = repeated_fake_tool_call(&mut state, &request, is_compaction_request)
            .or_else(|| {
                tool_result
                    .is_none()
                    .then(|| required_fake_tool_call(&request, state.next_turn))
                    .flatten()
            })
            .or_else(|| {
                (tool_result.is_none()
                    && !matches!(request.tool_call_policy.choice, ToolChoice::None))
                .then(|| fake_tool_call(&user_text, state.next_turn))
                .flatten()
            });
        let has_tool_result = tool_result.is_some();
        let text = fake_response_text(&request, tool_result.as_deref(), &user_text);
        let turn = FakeTurn::default();
        if positioned_output {
            turn.enable_positioned_output();
        }
        turn.push(ProviderTurnEvent::TurnStarted);
        emit_fake_managed_compaction(&request, &turn);
        let emit_overflow = request
            .provider_context
            .settings
            .get("fake_context_overflow_once")
            .is_some_and(|value| value == "true")
            && !state.overflow_emitted;
        if emit_overflow {
            state.overflow_emitted = true;
        }
        let emit_max_tokens = request
            .provider_context
            .settings
            .get("fake_max_tokens_once")
            .is_some_and(|value| value == "true")
            && !is_compaction_request
            && !state.max_tokens_emitted;
        if emit_max_tokens {
            state.max_tokens_emitted = true;
        }
        let forced_stop = if emit_overflow {
            Some(StopReason::Error)
        } else if emit_max_tokens {
            Some(StopReason::MaxTokens)
        } else {
            None
        };
        let configured_tool_call_count = fake_tool_call_count(&request);
        let emit_malformed_tool_call = request
            .provider_context
            .settings
            .get("fake_malformed_tool_call")
            .is_some_and(|value| value == "true");
        state.turns.insert(provider_turn_id.clone(), turn.clone());
        drop(state);
        dispatch_fake_turn(FakeTurnDispatch {
            turn,
            request: &request,
            event_script,
            forced_stop,
            configured_tool_call_count,
            emit_malformed_tool_call,
            has_tool_result,
            tool_call,
            text,
            request_input_tokens,
        });
        json_response(&StartTurnResponse { provider_turn_id })
    }

    fn poll_turn_events(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<PollTurnEventsRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let events = self
            .state
            .lock()
            .expect("fake provider state lock should not be poisoned")
            .turns
            .get(&request.provider_turn_id)
            .map_or_else(Vec::new, FakeTurn::drain);
        json_response(&PollTurnEventsResponse { events })
    }

    fn cancel_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<CancelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let turn = self
            .state
            .lock()
            .expect("fake provider state lock should not be poisoned")
            .turns
            .get(&request.provider_turn_id)
            .cloned();
        if let Some(turn) = turn {
            turn.cancel();
        }
        json_response(&AckResponse::default())
    }

    fn finish_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<FinishTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let turn = self
            .state
            .lock()
            .expect("fake provider state lock should not be poisoned")
            .turns
            .remove(&request.provider_turn_id);
        if let Some(turn) = turn {
            turn.cancel();
        }
        json_response(&AckResponse::default())
    }
}

struct FakeTurnDispatch<'a> {
    turn: FakeTurn,
    request: &'a ModelTurnRequest,
    event_script: Option<FakeProviderEventScript>,
    forced_stop: Option<StopReason>,
    configured_tool_call_count: usize,
    emit_malformed_tool_call: bool,
    has_tool_result: bool,
    tool_call: Option<ToolCall>,
    text: Result<String, ProviderError>,
    request_input_tokens: u64,
}

fn dispatch_fake_turn(dispatch: FakeTurnDispatch<'_>) {
    let FakeTurnDispatch {
        turn,
        request,
        event_script,
        forced_stop,
        configured_tool_call_count,
        emit_malformed_tool_call,
        has_tool_result,
        tool_call,
        text,
        request_input_tokens,
    } = dispatch;
    if let Some(script) = event_script {
        run_fake_event_script(turn, script);
    } else if forced_stop == Some(StopReason::Error) {
        turn.push(ProviderTurnEvent::Error {
            error: ProviderError {
                code: "context_length_exceeded".to_string(),
                category: ProviderErrorCategory::ContextLength,
                message: "fake context overflow".to_string(),
                retryable: false,
                provider_message: None,
                failure: None,
                request_id: None,
                diagnostic_context: Box::default(),
                sources: Box::default(),
                retry: None,
            },
        });
        turn.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::Error,
        });
    } else if forced_stop == Some(StopReason::MaxTokens) {
        turn.push(ProviderTurnEvent::TextDelta {
            text: "fake partial output".to_owned(),
        });
        turn.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::MaxTokens,
        });
    } else if finish_configured_fake_tool_conformance(
        &turn,
        request,
        configured_tool_call_count,
        emit_malformed_tool_call,
        has_tool_result,
    ) {
    } else if let Some(text) = last_user_text(&request.messages).strip_prefix("stream-reasoning ") {
        dispatch_fake_reasoning_turn(
            turn,
            text.to_owned(),
            fake_tool_delta_delay(request),
            request_input_tokens,
        );
    } else if let Some(text) = last_user_text(&request.messages).strip_prefix("stream-text ") {
        dispatch_fake_streaming_text(
            turn,
            text.to_owned(),
            fake_tool_delta_delay(request),
            request_input_tokens,
        );
    } else if let Some(tool_call) = tool_call {
        dispatch_fake_tool_turn(turn, tool_call, fake_tool_delta_delay(request));
    } else {
        finish_fake_text_response(
            turn,
            text,
            fake_request_delay(request),
            request_input_tokens,
        );
    }
}

fn fake_event_script(
    request: &ModelTurnRequest,
) -> Result<Option<FakeProviderEventScript>, String> {
    let Some(encoded) = request
        .provider_context
        .settings
        .get(FAKE_EVENT_SCRIPT_SETTING)
    else {
        return Ok(None);
    };
    let script = serde_json::from_str::<FakeProviderEventScript>(encoded)
        .map_err(|error| error.to_string())?;
    if script.steps.is_empty() {
        return Err("fake event script must contain at least one step".to_owned());
    }
    if script.steps.len() > 1_024 {
        return Err("fake event script exceeds 1024 steps".to_owned());
    }
    if script
        .steps
        .iter()
        .any(|step| step.gate.as_ref().is_some_and(String::is_empty))
    {
        return Err("fake event script gates must not be empty".to_owned());
    }
    Ok(Some(script))
}

fn run_fake_event_script(turn: FakeTurn, script: FakeProviderEventScript) {
    let controlled = script
        .steps
        .iter()
        .any(|step| step.gate.is_some() || step.delay_ms.is_some_and(|delay| delay > 0));
    if controlled {
        std::thread::spawn(move || emit_fake_event_script(&turn, script));
    } else {
        emit_fake_event_script(&turn, script);
    }
}

fn emit_fake_event_script(turn: &FakeTurn, script: FakeProviderEventScript) {
    for step in script.steps {
        if let Some(gate) = step.gate.as_deref()
            && !wait_for_fake_event_script_gate(turn, gate)
        {
            return;
        }
        if let Some(delay_ms) = step.delay_ms.filter(|delay| *delay > 0) {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        if turn.is_cancelled() {
            return;
        }
        turn.push(step.event);
    }
}

fn wait_for_fake_event_script_gate(turn: &FakeTurn, gate: &str) -> bool {
    let (gates, ready) = &*FAKE_SCRIPT_GATES;
    let Ok(mut gates) = gates.lock() else {
        return false;
    };
    while !gates.contains(gate) {
        if turn.is_cancelled() {
            return false;
        }
        let Ok((next, _)) = ready.wait_timeout(gates, Duration::from_millis(10)) else {
            return false;
        };
        gates = next;
    }
    true
}

fn finish_fake_text_response(
    turn: FakeTurn,
    text: Result<String, ProviderError>,
    delay: Option<Duration>,
    request_input_tokens: u64,
) {
    let text = match text {
        Ok(text) => text,
        Err(error) => {
            turn.push(ProviderTurnEvent::Error { error });
            turn.push(ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Error,
            });
            return;
        }
    };
    if let Some(delay) = delay {
        std::thread::spawn(move || {
            FakeTurnWorker {
                turn,
                text,
                delay,
                request_input_tokens,
            }
            .run();
        });
    } else {
        finish_fake_turn(&turn, text, request_input_tokens);
    }
}

struct FakeTurnWorker {
    turn: FakeTurn,
    text: String,
    delay: Duration,
    request_input_tokens: u64,
}

impl FakeTurnWorker {
    fn run(self) {
        std::thread::sleep(self.delay);
        if !self.turn.is_cancelled() {
            finish_fake_turn(&self.turn, self.text, self.request_input_tokens);
        }
    }
}

fn fake_response_text(
    request: &ModelTurnRequest,
    tool_result: Option<&str>,
    user_text: &str,
) -> Result<String, ProviderError> {
    if let Some(result) = tool_result {
        return Ok(format!("fake tool result: {result}"));
    }
    let Some(structured) = request.structured_output.as_ref() else {
        return Ok(format!("fake: {user_text}"));
    };
    if let Some(value) = configured_fake_structured_output(request, structured, user_text)? {
        return Ok(value);
    }
    let validator =
        jsonschema::validator_for(&structured.schema).map_err(|error| ProviderError {
            code: "invalid_structured_output_schema".to_string(),
            category: ProviderErrorCategory::InvalidRequest,
            message: error.to_string(),
            retryable: false,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: None,
        })?;
    let value = fake_value_for_schema(&structured.schema, 0).ok_or_else(|| ProviderError {
        code: "unsupported_structured_output_schema".to_string(),
        category: ProviderErrorCategory::UnsupportedFeature,
        message: "fake provider cannot construct a value for the requested JSON schema".to_string(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    })?;
    if !validator.is_valid(&value) {
        return Err(ProviderError {
            code: "unsupported_structured_output_schema".to_string(),
            category: ProviderErrorCategory::UnsupportedFeature,
            message: "fake provider cannot satisfy the requested JSON schema".to_string(),
            retryable: false,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: None,
        });
    }
    serde_json::to_string(&value).map_err(|error| ProviderError {
        code: "structured_output_encode_failed".to_string(),
        category: ProviderErrorCategory::ProviderInternal,
        message: error.to_string(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    })
}

fn fake_structured_output_error(error: &dyn std::fmt::Display) -> ProviderError {
    ProviderError {
        code: "invalid_fake_structured_output".to_string(),
        category: ProviderErrorCategory::InvalidRequest,
        message: error.to_string(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

fn configured_fake_structured_output(
    request: &ModelTurnRequest,
    structured: &bcode_model::StructuredOutputRequest,
    user_text: &str,
) -> Result<Option<String>, ProviderError> {
    let Some(configured) = request
        .provider_context
        .settings
        .get("fake_structured_output_json")
    else {
        return Ok(None);
    };
    let value = if let Some(threshold) = configured.strip_prefix("loop_until:") {
        let threshold = threshold
            .parse::<u64>()
            .map_err(|error| fake_structured_output_error(&error))?;
        let mut value: serde_json::Value =
            serde_json::from_str(user_text.rsplit("\n\n").next().unwrap_or(user_text))
                .map_err(|error| fake_structured_output_error(&error))?;
        let evaluation = user_text.starts_with("Read-only loop completion evaluation.");
        let iteration = value
            .get("iteration")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        if evaluation {
            value["condition_met"] = serde_json::json!(iteration >= threshold);
            value["evidence"] = serde_json::json!([format!("evaluated iteration {iteration}")]);
            value["summary"] = serde_json::json!(format!("iteration {iteration} evaluated"));
        } else {
            value["iteration"] = serde_json::json!(iteration);
            value["condition_met"] = serde_json::json!(false);
            value["evidence"] = serde_json::json!([]);
            value["summary"] = serde_json::json!("");
        }
        value
    } else {
        configured
            .strip_prefix("echo_input:")
            .map_or_else(
                || serde_json::from_str(configured),
                |_| serde_json::from_str(user_text.rsplit("\n\n").next().unwrap_or(user_text)),
            )
            .map_err(|error| fake_structured_output_error(&error))?
    };
    let validator =
        jsonschema::validator_for(&structured.schema).map_err(|error| ProviderError {
            code: "invalid_structured_output_schema".to_string(),
            category: ProviderErrorCategory::InvalidRequest,
            message: error.to_string(),
            retryable: false,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: None,
        })?;
    if !validator.is_valid(&value) {
        return Ok(None);
    }
    serde_json::to_string(&value)
        .map(Some)
        .map_err(|error| ProviderError {
            code: "structured_output_encode_failed".to_string(),
            category: ProviderErrorCategory::ProviderInternal,
            message: error.to_string(),
            retryable: false,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: None,
        })
}

fn fake_value_for_schema(schema: &serde_json::Value, depth: usize) -> Option<serde_json::Value> {
    if depth > 32 {
        return None;
    }
    if let Some(value) = schema.get("const") {
        return Some(value.clone());
    }
    if let Some(value) = schema
        .get("enum")
        .and_then(serde_json::Value::as_array)
        .and_then(|values| values.first())
    {
        return Some(value.clone());
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(value) = schema
            .get(keyword)
            .and_then(serde_json::Value::as_array)
            .and_then(|variants| {
                variants
                    .iter()
                    .find_map(|variant| fake_value_for_schema(variant, depth + 1))
            })
        {
            return Some(value);
        }
    }
    if let Some(value) = schema.get("default") {
        return Some(value.clone());
    }
    let schema_type = schema
        .get("type")
        .and_then(|value| {
            value.as_str().or_else(|| {
                value
                    .as_array()
                    .and_then(|types| types.iter().find_map(serde_json::Value::as_str))
            })
        })
        .unwrap_or("null");
    match schema_type {
        "object" => fake_object_for_schema(schema, depth + 1),
        "array" => fake_array_for_schema(schema, depth + 1),
        "string" if schema.get("pattern").is_some() => None,
        "string" => {
            let length = schema
                .get("minLength")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or_default();
            Some(serde_json::Value::String("x".repeat(length)))
        }
        "integer" => schema
            .get("minimum")
            .and_then(serde_json::Value::as_i64)
            .map_or_else(
                || Some(serde_json::json!(0)),
                |minimum| Some(serde_json::json!(minimum)),
            ),
        "number" => schema
            .get("minimum")
            .and_then(serde_json::Value::as_f64)
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| Some(serde_json::json!(0)), |number| Some(number.into())),
        "boolean" => Some(serde_json::Value::Bool(true)),
        "null" => Some(serde_json::Value::Null),
        _ => None,
    }
}

fn fake_object_for_schema(schema: &serde_json::Value, depth: usize) -> Option<serde_json::Value> {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object);
    let required = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(serde_json::Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    let mut object = serde_json::Map::new();
    for name in required {
        let property = properties?.get(name)?;
        object.insert(name.to_string(), fake_value_for_schema(property, depth)?);
    }
    Some(serde_json::Value::Object(object))
}

fn fake_array_for_schema(schema: &serde_json::Value, depth: usize) -> Option<serde_json::Value> {
    let length = schema
        .get("minItems")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    let item_schema = schema.get("items").unwrap_or(&serde_json::Value::Null);
    let mut values = Vec::with_capacity(length);
    for _ in 0..length {
        values.push(fake_value_for_schema(item_schema, depth)?);
    }
    Some(serde_json::Value::Array(values))
}

fn insert_fake_error_turn(state: &Mutex<FakeProviderState>, error: ProviderError) -> String {
    let mut state = state
        .lock()
        .expect("fake provider state lock should not be poisoned");
    state.next_turn += 1;
    let provider_turn_id = format!("fake-turn-{}", state.next_turn);
    let turn = FakeTurn::default();
    turn.push(ProviderTurnEvent::TurnStarted);
    turn.push(ProviderTurnEvent::Error { error });
    turn.push(ProviderTurnEvent::TurnFinished {
        stop_reason: StopReason::Error,
    });
    state.turns.insert(provider_turn_id.clone(), turn);
    provider_turn_id
}

fn unsupported_fake_sampling_parameters(
    parameters: &bcode_model::ModelParameters,
) -> Option<(&'static str, &'static str)> {
    let mut supported_parameters = parameters.clone();
    supported_parameters.reasoning_effort = None;
    supported_parameters.reasoning_effort_value = None;
    supported_parameters.reasoning_summary = None;
    (supported_parameters != bcode_model::ModelParameters::default()).then_some((
        "fake_model_parameters_unsupported",
        "fake provider does not implement model sampling parameters",
    ))
}

fn unsupported_fake_error(code: &str, message: &str) -> ProviderError {
    ProviderError {
        code: code.to_owned(),
        category: ProviderErrorCategory::UnsupportedFeature,
        message: message.to_owned(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

fn validate_fake_request(request: &ModelTurnRequest) -> Option<ProviderError> {
    if let Some((code, message)) = unsupported_fake_sampling_parameters(&request.parameters) {
        return Some(unsupported_fake_error(code, message));
    }
    let unsupported = if matches!(
        request.prompt_cache.mode,
        bcode_model::PromptCacheMode::Aggressive
    ) || request.prompt_cache.cache_system_prompt
        || request.prompt_cache.cache_tools
        || request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::CachePoint { .. }))
        }) {
        Some((
            "fake_prompt_cache_unsupported",
            "fake provider does not implement prompt caching",
        ))
    } else if request.conversation_reuse.mode.is_enabled()
        || request
            .conversation_reuse
            .previous_provider_response_id
            .is_some()
        || request
            .conversation_reuse
            .new_messages_start_index
            .is_some()
        || request.conversation_reuse.provider_state.is_some()
    {
        Some((
            "fake_conversation_reuse_unsupported",
            "fake provider does not implement provider-native conversation reuse",
        ))
    } else if !request.provider_context.request.is_empty() {
        Some((
            "fake_provider_options_unsupported",
            "fake provider does not implement provider-native request options",
        ))
    } else if matches!(request.tool_call_policy.choice, ToolChoice::Required)
        && request.tools.is_empty()
    {
        Some((
            "fake_required_tool_without_tools",
            "required tool choice needs at least one registered tool",
        ))
    } else if let ToolChoice::Tool { name } = &request.tool_call_policy.choice
        && !request.tools.iter().any(|tool| tool.name == *name)
    {
        Some((
            "fake_unknown_required_tool",
            "named tool choice must reference a registered tool",
        ))
    } else if request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    }) {
        Some((
            "fake_image_input_unsupported",
            "fake provider does not implement image input",
        ))
    } else {
        None
    };
    if let Some((code, message)) = unsupported {
        return Some(ProviderError {
            code: code.to_string(),
            category: ProviderErrorCategory::UnsupportedFeature,
            message: message.to_string(),
            retryable: false,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: None,
        });
    }
    request
        .explicitly_unsupported_features(&fake_feature_support())
        .first()
        .map(|feature| ProviderError {
            code: "fake_feature_unsupported".to_string(),
            category: ProviderErrorCategory::UnsupportedFeature,
            message: format!("fake provider does not support {feature:?}"),
            retryable: false,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: None,
        })
}

fn fake_tool_call_count(request: &ModelTurnRequest) -> usize {
    request
        .provider_context
        .settings
        .get("fake_parallel_tool_calls")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| {
            usize::from(
                request.tool_call_policy.parallel == Some(true)
                    && request.tools.len() > 1
                    && matches!(request.tool_call_policy.choice, ToolChoice::Required),
            ) * request.tools.len()
        })
}

fn validate_fake_parallel_tool_policy(request: &ModelTurnRequest) -> Option<ServiceResponse> {
    FAKE_LAST_PARALLEL_TOOL_POLICY.store(
        request.tool_call_policy.parallel == Some(true),
        Ordering::Release,
    );
    let expected = request
        .provider_context
        .settings
        .get("fake_expected_parallel_tool_policy")
        .and_then(|value| value.parse::<bool>().ok())?;
    (request.tool_call_policy.parallel != Some(expected)).then(|| {
        ServiceResponse::error(
            "unexpected_parallel_tool_policy",
            format!(
                "expected parallel tool policy {expected}, received {:?}",
                request.tool_call_policy.parallel
            ),
        )
    })
}

#[cfg(test)]
fn fake_reasoning_events(
    activity_id: &str,
    status: bcode_session_models::ReasoningActivityStatus,
) -> Vec<ProviderTurnEvent> {
    use bcode_session_models::{
        ReasoningActivityEvent, ReasoningContentKind, ReasoningContentRole,
    };
    vec![
        ProviderTurnEvent::ReasoningActivity {
            event: ReasoningActivityEvent::Started {
                activity_id: activity_id.to_owned(),
                order: 0,
            },
        },
        ProviderTurnEvent::ReasoningActivity {
            event: ReasoningActivityEvent::PartCompleted {
                activity_id: activity_id.to_owned(),
                activity_order: 0,
                part_id: "summary-0".to_owned(),
                kind: ReasoningContentKind::Summary,
                role: ReasoningContentRole::Milestone,
                part_order: 0,
                text: "fake summary".to_owned(),
            },
        },
        ProviderTurnEvent::ReasoningActivity {
            event: ReasoningActivityEvent::PartCompleted {
                activity_id: activity_id.to_owned(),
                activity_order: 0,
                part_id: "raw-0".to_owned(),
                kind: ReasoningContentKind::Raw,
                role: ReasoningContentRole::Detail,
                part_order: 1,
                text: "fake raw detail".to_owned(),
            },
        },
        ProviderTurnEvent::ReasoningActivity {
            event: ReasoningActivityEvent::OpaqueObserved {
                activity_id: activity_id.to_owned(),
                activity_order: 0,
            },
        },
        ProviderTurnEvent::ReasoningActivity {
            event: ReasoningActivityEvent::Finished {
                activity_id: activity_id.to_owned(),
                activity_order: 0,
                status,
            },
        },
    ]
}

fn finish_fake_turn(turn: &FakeTurn, text: String, request_input_tokens: u64) {
    let output_tokens = u32::try_from(text.split_whitespace().count()).unwrap_or(u32::MAX);
    turn.push(ProviderTurnEvent::TextDelta { text });
    turn.push(ProviderTurnEvent::Usage {
        usage: TokenUsage {
            input_tokens: Some(1),
            output_tokens: Some(output_tokens),
            total_tokens: Some(output_tokens.saturating_add(1)),
            ..TokenUsage::default()
        },
    });
    turn.push(ProviderTurnEvent::ExactRequestInputTokens {
        tokens: bcode_model::ExactRequestInputTokens::new(request_input_tokens),
    });
    turn.push(ProviderTurnEvent::TurnFinished {
        stop_reason: StopReason::EndTurn,
    });
}

fn fake_request_input_tokens(request: &ModelTurnRequest) -> u64 {
    let visible = serde_json::to_string(&(
        request.system_prompt.as_ref(),
        &request.messages,
        &request.tools,
        &request.parameters,
        request.structured_output.as_ref(),
        &request.provider_context.request,
    ))
    .unwrap_or_default();
    u64::try_from(visible.split_whitespace().count()).unwrap_or(u64::MAX)
}

fn emit_fake_managed_compaction(request: &ModelTurnRequest, turn: &FakeTurn) {
    if request.context_management.compact_threshold.is_none() {
        return;
    }
    FAKE_MANAGED_COMPACTION_EMITTED.store(true, Ordering::Release);
    turn.push(ProviderTurnEvent::ContextCompacted {
        messages: vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ProviderExtension {
                value: serde_json::json!({
                    "type": "fake_managed_compaction",
                    "message_count": request.messages.len(),
                }),
            }],
        }],
        context_format: fake_context_format(),
    });
}

fn finish_configured_fake_tool_conformance(
    turn: &FakeTurn,
    request: &ModelTurnRequest,
    tool_call_count: usize,
    malformed: bool,
    has_tool_result: bool,
) -> bool {
    if malformed {
        turn.push(ProviderTurnEvent::Error {
            error: ProviderError {
                code: "malformed_tool_call".to_owned(),
                category: ProviderErrorCategory::InvalidRequest,
                message: "fake provider emitted a malformed tool call".to_owned(),
                retryable: false,
                provider_message: None,
                failure: None,
                request_id: None,
                diagnostic_context: Box::default(),
                sources: Box::default(),
                retry: None,
            },
        });
        turn.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::Error,
        });
        return true;
    }
    if tool_call_count == 0 || has_tool_result {
        return false;
    }
    for index in 0..tool_call_count {
        finish_fake_tool_call(
            turn,
            ToolCall {
                id: format!("fake-call-{index}"),
                name: request
                    .tools
                    .get(index % request.tools.len().max(1))
                    .map_or_else(|| "fake.tool".to_owned(), |tool| tool.name.clone()),
                arguments: serde_json::json!({"index": index}),
            },
        );
    }
    turn.push(ProviderTurnEvent::Usage {
        usage: TokenUsage::default(),
    });
    turn.push(ProviderTurnEvent::TurnFinished {
        stop_reason: StopReason::ToolCall,
    });
    true
}

fn finish_fake_tool_call(turn: &FakeTurn, call: ToolCall) {
    turn.push(ProviderTurnEvent::ToolCallStarted {
        call_id: call.id.clone(),
        name: call.name.clone(),
    });
    turn.push(ProviderTurnEvent::ToolCallFinished { call });
}

fn fake_tool_arguments_json(call: &ToolCall) -> String {
    let Some(arguments) = call.arguments.as_object() else {
        return serde_json::to_string(&call.arguments).unwrap_or_default();
    };
    let ordered_keys: &[&str] = match call.name.as_str() {
        "filesystem.write" => &["path", "contents"],
        "filesystem.edit" => &["path", "old_text", "new_text"],
        _ => return serde_json::to_string(&call.arguments).unwrap_or_default(),
    };
    let mut ordered = serde_json::Map::new();
    for key in ordered_keys {
        if let Some(value) = arguments.get(*key) {
            ordered.insert((*key).to_owned(), value.clone());
        }
    }
    for (key, value) in arguments {
        if !ordered.contains_key(key) {
            ordered.insert(key.clone(), value.clone());
        }
    }
    serde_json::to_string(&ordered).unwrap_or_default()
}

fn fake_tool_argument_split_index(call: &ToolCall, arguments: &str) -> usize {
    let marker = match call.name.as_str() {
        "filesystem.write" => "PTYFILESYSTEMFIRST",
        "filesystem.edit" => "PTYFILESYSTEMSECOND",
        _ => "",
    };
    if !marker.is_empty()
        && let Some(index) = arguments.find(marker)
    {
        return index + marker.len();
    }
    let split_target = arguments.len().saturating_mul(3) / 4;
    arguments
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index >= split_target)
        .unwrap_or(arguments.len())
}

fn dispatch_fake_reasoning_turn(
    turn: FakeTurn,
    text: String,
    delta_delay: Option<Duration>,
    request_input_tokens: u64,
) {
    let delay = delta_delay.unwrap_or_default();
    std::thread::spawn(move || {
        let activity_id = "fake-streaming-reasoning".to_owned();
        turn.push(ProviderTurnEvent::ReasoningActivity {
            event: bcode_session_models::ReasoningActivityEvent::Started {
                activity_id: activity_id.clone(),
                order: 0,
            },
        });
        let midpoint = text
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| *index >= text.len() / 2)
            .unwrap_or(text.len());
        for delta in [&text[..midpoint], &text[midpoint..]] {
            turn.push(ProviderTurnEvent::ReasoningActivity {
                event: bcode_session_models::ReasoningActivityEvent::PartDelta {
                    activity_id: activity_id.clone(),
                    activity_order: 0,
                    part_id: "summary".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    part_order: 0,
                    text: delta.to_owned(),
                },
            });
            std::thread::sleep(delay);
            if turn.is_cancelled() {
                return;
            }
        }
        turn.push(ProviderTurnEvent::ReasoningActivity {
            event: bcode_session_models::ReasoningActivityEvent::Finished {
                activity_id,
                activity_order: 0,
                status: bcode_session_models::ReasoningActivityStatus::Completed,
            },
        });
        finish_fake_turn(&turn, "REASONINGFINAL".to_owned(), request_input_tokens);
    });
}

fn dispatch_fake_streaming_text(
    turn: FakeTurn,
    text: String,
    delta_delay: Option<Duration>,
    request_input_tokens: u64,
) {
    if let Some(delay) = delta_delay {
        std::thread::spawn(move || {
            let midpoint = text
                .char_indices()
                .map(|(index, _)| index)
                .find(|index| *index >= text.len() / 2)
                .unwrap_or(text.len());
            std::thread::sleep(delay);
            if turn.is_cancelled() {
                return;
            }
            turn.push(ProviderTurnEvent::TextDelta {
                text: text[..midpoint].to_owned(),
            });
            std::thread::sleep(delay);
            if turn.is_cancelled() {
                return;
            }
            finish_fake_turn(&turn, text[midpoint..].to_owned(), request_input_tokens);
        });
    } else {
        finish_fake_turn(&turn, text, request_input_tokens);
    }
}

fn dispatch_fake_tool_turn(turn: FakeTurn, call: ToolCall, delta_delay: Option<Duration>) {
    if let Some(delay) = delta_delay {
        std::thread::spawn(move || finish_fake_tool_turn(&turn, call, Some(delay)));
    } else {
        finish_fake_tool_turn(&turn, call, None);
    }
}

fn finish_fake_tool_turn(turn: &FakeTurn, call: ToolCall, delta_delay: Option<Duration>) {
    turn.push(ProviderTurnEvent::ToolCallStarted {
        call_id: call.id.clone(),
        name: call.name.clone(),
    });
    let arguments = fake_tool_arguments_json(&call);
    let midpoint = fake_tool_argument_split_index(&call, &arguments);
    let deltas = [&arguments[..midpoint], &arguments[midpoint..]];
    for delta in deltas {
        if !delta.is_empty() {
            turn.push(ProviderTurnEvent::ToolCallDelta {
                call_id: call.id.clone(),
                delta: delta.to_owned(),
            });
            if let Some(delay) = delta_delay {
                std::thread::sleep(delay);
                if turn.is_cancelled() {
                    return;
                }
            }
        }
    }
    turn.push(ProviderTurnEvent::ToolCallFinished { call });
    turn.push(ProviderTurnEvent::Usage {
        usage: TokenUsage::default(),
    });
    turn.push(ProviderTurnEvent::TurnFinished {
        stop_reason: StopReason::ToolCall,
    });
}

fn fake_tool_delta_delay(request: &ModelTurnRequest) -> Option<Duration> {
    request
        .provider_context
        .settings
        .get("fake_tool_delta_delay_ms")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
}

fn fake_request_delay(request: &ModelTurnRequest) -> Option<Duration> {
    request
        .provider_context
        .settings
        .get("fake_turn_delay_ms")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .or_else(fake_delay)
}

fn fake_delay() -> Option<Duration> {
    let millis = std::env::var("BCODE_FAKE_PROVIDER_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())?;
    if millis == 0 {
        None
    } else {
        Some(Duration::from_millis(millis))
    }
}

fn fake_feature_support() -> bcode_model::ModelFeatureSupport {
    use bcode_model::{
        CapabilitySource, CapabilitySupport, MediaInputFeature, ModelFeatureSupport,
        ModelParameterKey, PromptCacheFeature, StructuredOutputMode, ToolChoiceMode,
    };
    let supported = || CapabilitySupport::Supported {
        source: CapabilitySource::TestContract,
    };
    let unsupported = |reason: &str| CapabilitySupport::Unsupported {
        source: CapabilitySource::TestContract,
        reason: reason.to_string(),
    };
    ModelFeatureSupport {
        parameters: [
            ModelParameterKey::Temperature,
            ModelParameterKey::MaxOutputTokens,
            ModelParameterKey::TopP,
            ModelParameterKey::StopSequences,
            ModelParameterKey::ReasoningBudgetTokens,
            ModelParameterKey::ReasoningEffort,
            ModelParameterKey::ReasoningEffortValue,
            ModelParameterKey::ReasoningSummary,
        ]
        .into_iter()
        .map(|key| {
            (
                key,
                unsupported("fake provider accepts no model parameters"),
            )
        })
        .collect(),
        structured_output: [
            (StructuredOutputMode::JsonSchema, supported()),
            (StructuredOutputMode::StrictJsonSchema, supported()),
        ]
        .into_iter()
        .collect(),
        tool_choice: [
            ToolChoiceMode::Auto,
            ToolChoiceMode::None,
            ToolChoiceMode::Required,
            ToolChoiceMode::Named,
            ToolChoiceMode::Parallel,
        ]
        .into_iter()
        .map(|mode| (mode, supported()))
        .collect(),
        prompt_cache: [
            PromptCacheFeature::ConversationPrefix,
            PromptCacheFeature::ExplicitSystem,
            PromptCacheFeature::ExplicitTools,
            PromptCacheFeature::ExplicitMessage,
            PromptCacheFeature::Ttl,
        ]
        .into_iter()
        .map(|feature| {
            (
                feature,
                unsupported("fake provider does not implement prompt caching"),
            )
        })
        .collect(),
        media_input: [
            MediaInputFeature::UserImage,
            MediaInputFeature::SystemImage,
            MediaInputFeature::AssistantImage,
            MediaInputFeature::ToolMessageImage,
            MediaInputFeature::ImageReference,
            MediaInputFeature::ToolResultImage,
        ]
        .into_iter()
        .map(|feature| {
            (
                feature,
                unsupported("fake provider accepts text input only"),
            )
        })
        .collect(),
    }
}

fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        provider_id: "bcode.fake-provider".to_string(),
        display_name: "Bcode Fake Provider".to_string(),
        capabilities: [
            ProviderCapability::Streaming,
            ProviderCapability::Tools,
            ProviderCapability::ParallelToolCalls,
            ProviderCapability::Cancellation,
            ProviderCapability::JsonMode,
        ]
        .into_iter()
        .collect(),
        feature_support: fake_feature_support(),
        auth_schemes: BTreeSet::new(),
        retry_rules: Vec::new(),
        metadata: BTreeMap::new(),
    }
}

fn models(has_context_window: bool) -> ModelList {
    ModelList {
        models: vec![ModelInfo {
            model_id: "fake-echo".to_string(),
            display_name: "Fake Echo".to_string(),
            is_default: true,
            context_window: has_context_window.then_some(8_000),
            max_output_tokens: Some(1_000),
            capabilities: [
                ModelCapability::StreamingText,
                ModelCapability::ToolCalls,
                ModelCapability::ParallelToolCalls,
                ModelCapability::JsonMode,
                ModelCapability::Reasoning,
            ]
            .into_iter()
            .collect(),
            feature_support: fake_feature_support(),
            reasoning: Some(bcode_model::ModelReasoningInfo {
                effort_values: ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect(),
                default_effort: Some("medium".to_owned()),
                source: bcode_model::ModelReasoningCapabilitySource::ProviderMetadata,
                ..bcode_model::ModelReasoningInfo::default()
            }),
            cache: bcode_model::ModelCacheInfo::default(),
            metadata_source: Some(bcode_model::ModelMetadataSource::BundledCatalog),
            pricing: None,
            visibility: bcode_model::ModelVisibility::Visible,
        }],
        catalog: bcode_model::ModelCatalogHints::default(),
    }
}

fn configured_fake_tool_rounds(request: &ModelTurnRequest) -> u64 {
    request
        .provider_context
        .settings
        .get("fake_tool_rounds")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default()
}

fn repeated_fake_tool_call(
    state: &mut FakeProviderState,
    request: &ModelTurnRequest,
    is_compaction_request: bool,
) -> Option<ToolCall> {
    if is_compaction_request || state.tool_rounds_emitted >= configured_fake_tool_rounds(request) {
        return None;
    }
    state.tool_rounds_emitted += 1;
    Some(ToolCall {
        id: format!("fake-tool-{}", state.next_turn),
        name: "fake.missing-tool".to_string(),
        arguments: serde_json::json!({ "round": state.next_turn }),
    })
}

fn required_fake_tool_call(request: &ModelTurnRequest, next_turn: u64) -> Option<ToolCall> {
    let name = match &request.tool_call_policy.choice {
        ToolChoice::Required => request.tools.first()?.name.clone(),
        ToolChoice::Tool { name } => request
            .tools
            .iter()
            .find(|tool| tool.name == *name)?
            .name
            .clone(),
        ToolChoice::Auto | ToolChoice::None => return None,
    };
    let arguments = if name == "filesystem.read" {
        serde_json::json!({"path": "/tmp/bcode-provider-conformance"})
    } else {
        serde_json::json!({})
    };
    Some(ToolCall {
        id: format!("fake-required-tool-{next_turn}"),
        name,
        arguments,
    })
}

fn fake_tool_call(user_text: &str, next_turn: u64) -> Option<ToolCall> {
    if let Some(path) = user_text.strip_prefix("tool-read ") {
        return Some(ToolCall {
            id: format!("fake-tool-{next_turn}"),
            name: "filesystem.read".to_string(),
            arguments: serde_json::json!({ "path": path }),
        });
    }
    if let Some(rest) = user_text.strip_prefix("tool-write ") {
        let (path, contents) = rest.split_once(' ').unwrap_or((rest, "fake write"));
        return Some(ToolCall {
            id: format!("fake-tool-{next_turn}"),
            name: "filesystem.write".to_string(),
            arguments: serde_json::json!({ "path": path, "contents": contents }),
        });
    }
    if let Some(rest) = user_text.strip_prefix("tool-edit ") {
        let mut parts = rest.splitn(3, ' ');
        let path = parts.next().unwrap_or_default();
        let old_text = parts.next().unwrap_or_default();
        let new_text = parts.next().unwrap_or_default();
        return Some(ToolCall {
            id: format!("fake-tool-{next_turn}"),
            name: "filesystem.edit".to_string(),
            arguments: serde_json::json!({
                "path": path,
                "old_text": old_text,
                "new_text": new_text,
            }),
        });
    }
    if let Some(command) = user_text.strip_prefix("tool-shell ") {
        return Some(ToolCall {
            id: format!("fake-tool-{next_turn}"),
            name: "shell.run".to_string(),
            arguments: serde_json::json!({ "command": command }),
        });
    }
    None
}

fn last_user_text(messages: &[ModelMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::Image { image } => Some(format!("[image: {}]", image.mime_type)),
                _ => None,
            })
        })
        .unwrap_or_default()
}

fn last_tool_result(messages: &[ModelMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .take_while(|message| message.role != MessageRole::User)
        .find_map(|message| {
            if message.role != MessageRole::Tool {
                return None;
            }
            message.content.iter().find_map(|block| match block {
                ContentBlock::ToolResult { result } => Some(result.output.clone()),
                _ => None,
            })
        })
}

fn mark_fake_compaction_summary_started(request: &ModelTurnRequest) {
    FAKE_COMPACTION_SUMMARY_STARTED.store(true, Ordering::Release);
    if let Some(key) = request
        .provider_context
        .settings
        .get("fake_compaction_summary_signal")
    {
        FAKE_COMPACTION_SUMMARY_SIGNALS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone());
    }
}

fn fake_context_format() -> ProviderContextFormat {
    ProviderContextFormat {
        version: 1,
        compatibility_key: "bcode.fake-provider/context-v1".to_string(),
    }
}

fn fake_context_capabilities(
    request: &ContextManagementCapabilitiesRequest,
) -> ContextManagementCapabilities {
    let enabled = request
        .provider_context
        .settings
        .get("fake_native_compaction")
        .is_some_and(|value| value == "true");
    let provider_managed = request
        .provider_context
        .settings
        .get("fake_managed_compaction")
        .is_some_and(|value| value == "true");
    ContextManagementCapabilities {
        provider_managed,
        native_compaction: enabled,
        context_format: (enabled || provider_managed).then(fake_context_format),
    }
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn native_web_search(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<NativeWebSearchRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    json_response(&NativeWebSearchResponse {
        provider: "fake-native".to_string(),
        results: vec![NativeWebSearchResult {
            title: format!("Result for {}", request.query),
            url: "https://example.com/native".to_string(),
            snippet: "fake provider-native search result".to_string(),
            published: None,
            source: Some("fake".to_string()),
        }],
        partial: false,
        message: None,
    })
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        FakeProviderPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(
    FakeProviderPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{
        CapabilitySupport, ModelParameterKey, ProviderOutputEvent, RequestedModelFeature,
        StructuredOutputMode, TurnOutputPosition,
    };

    fn drain_script_until(
        turn: &FakeTurn,
        predicate: impl Fn(&[ProviderTurnEvent]) -> bool,
    ) -> Vec<ProviderTurnEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while std::time::Instant::now() < deadline {
            events.extend(turn.drain());
            if predicate(&events) {
                return events;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("fake event script did not reach expected state: {events:?}");
    }

    #[test]
    fn fake_tool_turn_streams_arguments_before_finishing() {
        let turn = FakeTurn::default();
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "filesystem.write".to_owned(),
            arguments: serde_json::json!({"path": "src/lib.rs", "contents": "hello"}),
        };
        finish_fake_tool_turn(&turn, call.clone(), None);
        let events = turn.drain();
        assert!(matches!(
            events.first(),
            Some(ProviderTurnEvent::ToolCallStarted { call_id, .. }) if call_id == &call.id
        ));
        let deltas = events
            .iter()
            .filter_map(|event| match event {
                ProviderTurnEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(deltas, fake_tool_arguments_json(&call));
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ToolCallFinished { call: finished } if finished == &call
        )));
    }

    #[test]
    fn fake_tool_turn_can_pause_between_argument_deltas() {
        let turn = FakeTurn::default();
        let call = ToolCall {
            id: "call-delayed".to_owned(),
            name: "filesystem.write".to_owned(),
            arguments: serde_json::json!({"path": "src/lib.rs", "contents": "hello"}),
        };
        dispatch_fake_tool_turn(turn.clone(), call, Some(Duration::from_millis(100)));
        let first = drain_script_until(&turn, |events| {
            events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::ToolCallDelta { .. }))
        });
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(event, ProviderTurnEvent::ToolCallDelta { .. }))
                .count(),
            1
        );
        assert!(
            !first
                .iter()
                .any(|event| { matches!(event, ProviderTurnEvent::ToolCallFinished { .. }) })
        );
        let finished = drain_script_until(&turn, |events| {
            events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::ToolCallFinished { .. }))
        });
        assert!(
            finished
                .iter()
                .any(|event| { matches!(event, ProviderTurnEvent::ToolCallFinished { .. }) })
        );
    }

    #[test]
    fn fake_reasoning_parameters_are_removed_from_sampling_validation() {
        let parameters = bcode_model::ModelParameters {
            reasoning_effort_value: Some("high".to_owned()),
            reasoning_summary: Some("detailed".to_owned()),
            ..bcode_model::ModelParameters::default()
        };

        assert_eq!(unsupported_fake_sampling_parameters(&parameters), None);
    }

    #[test]
    fn fake_text_turn_can_pause_between_deltas() {
        let turn = FakeTurn::default();
        dispatch_fake_streaming_text(
            turn.clone(),
            "ASSISTANTPREFIXASSISTANTSUFFIX".to_owned(),
            Some(Duration::from_millis(100)),
            4,
        );
        let first = drain_script_until(&turn, |events| {
            events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::TextDelta { .. }))
        });
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(event, ProviderTurnEvent::TextDelta { .. }))
                .count(),
            1
        );
        assert!(
            !first
                .iter()
                .any(|event| { matches!(event, ProviderTurnEvent::TurnFinished { .. }) })
        );
        let finished = drain_script_until(&turn, |events| {
            events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::TurnFinished { .. }))
        });
        assert!(
            finished
                .iter()
                .any(|event| { matches!(event, ProviderTurnEvent::TextDelta { .. }) })
        );
    }

    #[test]
    fn fake_reasoning_turn_can_pause_between_ordered_updates() {
        let turn = FakeTurn::default();
        dispatch_fake_reasoning_turn(
            turn.clone(),
            "REASONINGFIRSTREASONINGSECOND".to_owned(),
            Some(Duration::from_millis(100)),
            4,
        );
        let first = drain_script_until(&turn, |events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    ProviderTurnEvent::ReasoningActivity {
                        event: bcode_session_models::ReasoningActivityEvent::PartDelta { .. }
                    }
                )
            })
        });
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(
                    event,
                    ProviderTurnEvent::ReasoningActivity {
                        event: bcode_session_models::ReasoningActivityEvent::PartDelta { .. }
                    }
                ))
                .count(),
            1
        );
        assert!(
            !first
                .iter()
                .any(|event| { matches!(event, ProviderTurnEvent::TurnFinished { .. }) })
        );
        let finished = drain_script_until(&turn, |events| {
            events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::TurnFinished { .. }))
        });
        assert!(finished.iter().any(|event| {
            matches!(
                event,
                ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::Finished { .. }
                }
            )
        }));
    }

    #[test]
    fn tool_result_only_applies_after_the_current_user_message() {
        let messages = vec![
            ModelMessage {
                role: MessageRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    result: bcode_model::ToolResult {
                        call_id: "old-call".to_owned(),
                        output: "old result".to_owned(),
                        is_error: false,
                        content: Vec::new(),
                    },
                }],
            },
            ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "new request".to_owned(),
                }],
            },
        ];
        assert_eq!(last_tool_result(&messages), None);
    }

    #[test]
    fn fake_event_script_gates_arbitrary_provider_events_in_declared_order() {
        let gate = "fake-event-script-test-tool";
        let (gates, ready) = &*FAKE_SCRIPT_GATES;
        gates.lock().expect("script gates").remove(gate);
        let reasoning = ProviderTurnEvent::output(
            TurnOutputPosition::new(1),
            ProviderOutputEvent::ReasoningActivity {
                event: bcode_session_models::ReasoningActivityEvent::PartDelta {
                    activity_id: "reasoning-1".to_owned(),
                    activity_order: 0,
                    part_id: "summary-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    part_order: 0,
                    text: "reasoning".to_owned(),
                },
            },
        );
        let tool = ToolCall {
            id: "call-1".to_owned(),
            name: "filesystem.write".to_owned(),
            arguments: serde_json::json!({"path": "src/lib.rs", "contents": "hello"}),
        };
        let expected_after_gate = vec![
            reasoning,
            ProviderTurnEvent::output(
                TurnOutputPosition::new(2),
                ProviderOutputEvent::ToolCallStarted {
                    call_id: tool.id.clone(),
                    name: tool.name.clone(),
                },
            ),
            ProviderTurnEvent::output(
                TurnOutputPosition::new(2),
                ProviderOutputEvent::ToolCallDelta {
                    call_id: tool.id.clone(),
                    delta: r#"{"path":"src/lib.rs""#.to_owned(),
                },
            ),
            ProviderTurnEvent::output(
                TurnOutputPosition::new(2),
                ProviderOutputEvent::ToolCallFinished { call: tool },
            ),
            ProviderTurnEvent::Usage {
                usage: TokenUsage::default(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::ToolCall,
            },
        ];
        let mut steps = vec![FakeProviderEventScriptStep {
            gate: None,
            delay_ms: None,
            event: ProviderTurnEvent::output(
                TurnOutputPosition::new(0),
                ProviderOutputEvent::TextDelta {
                    text: "before gate".to_owned(),
                },
            ),
        }];
        steps.extend(
            expected_after_gate
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, event)| FakeProviderEventScriptStep {
                    gate: (index == 0).then(|| gate.to_owned()),
                    delay_ms: None,
                    event,
                }),
        );
        let turn = FakeTurn::default();
        run_fake_event_script(turn.clone(), FakeProviderEventScript { steps });

        let before_gate = drain_script_until(&turn, |events| !events.is_empty());
        assert_eq!(
            before_gate,
            [ProviderTurnEvent::output(
                TurnOutputPosition::new(0),
                ProviderOutputEvent::TextDelta {
                    text: "before gate".to_owned(),
                },
            )]
        );
        std::thread::sleep(Duration::from_millis(20));
        assert!(turn.drain().is_empty(), "gated events escaped early");

        gates.lock().expect("script gates").insert(gate.to_owned());
        ready.notify_all();
        let after_gate = drain_script_until(&turn, |events| {
            events
                .last()
                .is_some_and(|event| matches!(event, ProviderTurnEvent::TurnFinished { .. }))
        });
        assert_eq!(after_gate, expected_after_gate);
        gates.lock().expect("script gates").remove(gate);
    }

    #[test]
    fn fake_event_script_json_round_trips_gate_delay_and_provider_event() {
        let script = FakeProviderEventScript {
            steps: vec![FakeProviderEventScriptStep {
                gate: Some("release".to_owned()),
                delay_ms: Some(25),
                event: ProviderTurnEvent::output(
                    TurnOutputPosition::new(2),
                    ProviderOutputEvent::ToolCallDelta {
                        call_id: "call-1".to_owned(),
                        delta: "partial arguments".to_owned(),
                    },
                ),
            }],
        };
        let encoded = serde_json::to_string(&script).expect("encode script");
        let decoded: FakeProviderEventScript =
            serde_json::from_str(&encoded).expect("decode script");
        assert_eq!(decoded, script);
    }

    #[test]
    fn fake_reasoning_fixture_drives_every_neutral_signal_and_terminal_state() {
        for status in [
            bcode_session_models::ReasoningActivityStatus::Completed,
            bcode_session_models::ReasoningActivityStatus::Interrupted,
            bcode_session_models::ReasoningActivityStatus::Failed,
        ] {
            let events = fake_reasoning_events("reasoning-1", status);
            assert!(events.iter().any(|event| matches!(
                event,
                ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::PartCompleted {
                        kind: bcode_session_models::ReasoningContentKind::Summary,
                        ..
                    }
                }
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::PartCompleted {
                        kind: bcode_session_models::ReasoningContentKind::Raw,
                        ..
                    }
                }
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::OpaqueObserved { .. }
                }
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::Finished {
                        status: actual,
                        ..
                    }
                } if *actual == status
            )));
        }
    }

    #[test]
    fn fake_capability_contract_matches_request_validation() {
        let provider = capabilities();
        let model = models(true).models.remove(0);
        assert!(provider.feature_support.has_complete_inventory());
        assert!(model.feature_support.has_complete_inventory());
        assert!(
            provider
                .feature_support
                .negotiate(
                    &model.feature_support,
                    RequestedModelFeature::StructuredOutput(StructuredOutputMode::StrictJsonSchema)
                )
                .is_guaranteed()
        );
        assert!(matches!(
            provider
                .feature_support
                .parameter(ModelParameterKey::Temperature),
            CapabilitySupport::Unsupported { .. }
        ));
    }
}
