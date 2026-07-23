#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Fake model provider plugin for deterministic tests and smoke flows.

use bcode_model::{
    AckResponse, CancelTurnRequest, CompactContextRequest, CompactContextResponse, ContentBlock,
    ContextManagementCapabilities, ContextManagementCapabilitiesRequest, FinishTurnRequest,
    MODEL_PROVIDER_INTERFACE_ID, MessageRole, ModelCapability, ModelInfo, ModelList,
    ModelListRequest, ModelMessage, ModelTurnRequest, NativeWebSearchRequest,
    NativeWebSearchResponse, NativeWebSearchResult, OP_CANCEL_TURN, OP_CAPABILITIES,
    OP_COMPACT_CONTEXT, OP_CONTEXT_MANAGEMENT_CAPABILITIES, OP_FINISH_TURN, OP_MODELS,
    OP_NATIVE_WEB_SEARCH, OP_POLL_TURN_EVENTS, OP_START_TURN, OP_VALIDATE_CONFIG,
    PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities, ProviderCapability,
    ProviderContextFormat, ProviderError, ProviderErrorCategory, ProviderTurnEvent,
    StartTurnResponse, StopReason, TokenUsage, ToolCall, ToolChoice, ValidateConfigRequest,
    ValidateConfigResponse,
};
use bcode_plugin_sdk::prelude::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static FAKE_COMPACTION_STARTED: AtomicBool = AtomicBool::new(false);
static FAKE_COMPACTION_SUMMARY_STARTED: AtomicBool = AtomicBool::new(false);
static FAKE_MANAGED_COMPACTION_EMITTED: AtomicBool = AtomicBool::new(false);
static FAKE_LAST_PARALLEL_TOOL_POLICY: AtomicBool = AtomicBool::new(false);

/// Reset the provider-compaction start signal used by static runtime tests.
#[cfg(feature = "static-bundled")]
pub fn reset_fake_compaction_started() {
    FAKE_COMPACTION_STARTED.store(false, Ordering::Release);
    FAKE_COMPACTION_SUMMARY_STARTED.store(false, Ordering::Release);
    FAKE_MANAGED_COMPACTION_EMITTED.store(false, Ordering::Release);
    FAKE_LAST_PARALLEL_TOOL_POLICY.store(false, Ordering::Release);
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
}

#[derive(Debug, Clone, Default)]
struct FakeTurn {
    events: Arc<Mutex<VecDeque<ProviderTurnEvent>>>,
    cancelled: Arc<AtomicBool>,
}

impl FakeTurn {
    fn push(&self, event: ProviderTurnEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
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
        if context.request.interface_id != MODEL_PROVIDER_INTERFACE_ID {
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
            OP_START_TURN => self.start_turn(&context.request),
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

    fn start_turn(&self, request: &ServiceRequest) -> ServiceResponse {
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
        let is_compaction_request = request
            .metadata
            .get("bcode_request_kind")
            .is_some_and(|kind| kind == "compaction");
        if is_compaction_request {
            FAKE_COMPACTION_SUMMARY_STARTED.store(true, Ordering::Release);
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
        let configured_tool_call_count = fake_tool_call_count(&request);
        let emit_malformed_tool_call = request
            .provider_context
            .settings
            .get("fake_malformed_tool_call")
            .is_some_and(|value| value == "true");
        state.turns.insert(provider_turn_id.clone(), turn.clone());
        drop(state);
        if emit_overflow {
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
        } else if finish_configured_fake_tool_conformance(
            &turn,
            &request,
            configured_tool_call_count,
            emit_malformed_tool_call,
            has_tool_result,
        ) {
        } else if let Some(tool_call) = tool_call {
            finish_fake_tool_turn(&turn, tool_call);
        } else {
            finish_fake_text_response(
                turn,
                text,
                fake_request_delay(&request),
                request_input_tokens,
            );
        }
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

fn validate_fake_request(request: &ModelTurnRequest) -> Option<ProviderError> {
    let unsupported = if request.parameters != bcode_model::ModelParameters::default() {
        Some((
            "fake_model_parameters_unsupported",
            "fake provider does not implement model sampling or reasoning parameters",
        ))
    } else if matches!(
        request.prompt_cache.mode,
        bcode_model::PromptCacheMode::Aggressive
    ) || request.prompt_cache.cache_system_prompt
        || request.prompt_cache.cache_tools
        || request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::CachePoint { .. }))
        })
    {
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

fn finish_fake_tool_turn(turn: &FakeTurn, call: ToolCall) {
    finish_fake_tool_call(turn, call);
    turn.push(ProviderTurnEvent::Usage {
        usage: TokenUsage::default(),
    });
    turn.push(ProviderTurnEvent::TurnFinished {
        stop_reason: StopReason::ToolCall,
    });
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
            ]
            .into_iter()
            .collect(),
            feature_support: fake_feature_support(),
            reasoning: None,
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
    messages.iter().rev().find_map(|message| {
        if message.role != MessageRole::Tool {
            return None;
        }
        message.content.iter().find_map(|block| match block {
            ContentBlock::ToolResult { result } => Some(result.output.clone()),
            _ => None,
        })
    })
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
        CapabilitySupport, ModelParameterKey, RequestedModelFeature, StructuredOutputMode,
    };

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
