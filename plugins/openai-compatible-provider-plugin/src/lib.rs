#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! OpenAI-compatible model provider plugin for Bcode.

use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    MessageRole, ModelCapability, ModelInfo, ModelList, ModelMessage, ModelTurnRequest,
    OP_CANCEL_TURN, OP_CAPABILITIES, OP_FINISH_TURN, OP_MODELS, OP_POLL_TURN_EVENTS, OP_START_TURN,
    OP_VALIDATE_CONFIG, PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities,
    ProviderCapability, ProviderError, ProviderErrorCategory, ProviderTurnEvent, StartTurnResponse,
    StopReason, ToolCall, ValidateConfigResponse,
};
use bcode_plugin_sdk::prelude::*;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_ID: &str = "gpt-4.1-mini";
const PROVIDER_ID: &str = "bcode.openai-compatible";

/// OpenAI-compatible model provider plugin.
#[derive(Default)]
pub struct OpenAiCompatibleProviderPlugin {
    next_turn: u64,
    turns: BTreeMap<String, TurnState>,
}

#[derive(Debug, Clone, Default)]
struct TurnState {
    events: Arc<Mutex<VecDeque<ProviderTurnEvent>>>,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
}

impl TurnState {
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
        self.cancel_notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl RustPlugin for OpenAiCompatibleProviderPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != MODEL_PROVIDER_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported model provider service interface",
            );
        }

        match context.request.operation.as_str() {
            OP_CAPABILITIES => json_response(&capabilities()),
            OP_MODELS => json_response(&models()),
            OP_VALIDATE_CONFIG => json_response(&validate_config()),
            OP_START_TURN => self.start_turn(&context.request),
            OP_POLL_TURN_EVENTS => self.poll_turn_events(&context.request),
            OP_CANCEL_TURN => self.cancel_turn(&context.request),
            OP_FINISH_TURN => self.finish_turn(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported model provider operation",
            ),
        }
    }
}

impl OpenAiCompatibleProviderPlugin {
    fn start_turn(&mut self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<ModelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        self.next_turn += 1;
        let provider_turn_id = format!("openai-compatible-turn-{}", self.next_turn);
        let turn = TurnState::default();
        turn.push(ProviderTurnEvent::TurnStarted);
        self.turns.insert(provider_turn_id.clone(), turn.clone());
        std::thread::spawn(move || TurnWorker { request, turn }.run());
        json_response(&StartTurnResponse { provider_turn_id })
    }

    fn poll_turn_events(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<PollTurnEventsRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let events = self
            .turns
            .get(&request.provider_turn_id)
            .map_or_else(Vec::new, TurnState::drain);
        json_response(&PollTurnEventsResponse { events })
    }

    fn cancel_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<CancelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        if let Some(turn) = self.turns.get(&request.provider_turn_id) {
            turn.cancel();
        }
        json_response(&AckResponse::default())
    }

    fn finish_turn(&mut self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<FinishTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        self.turns.remove(&request.provider_turn_id);
        json_response(&AckResponse::default())
    }
}

#[derive(Debug, Clone)]
struct Settings {
    api_key: Option<String>,
    base_url: String,
    default_model: String,
    model_ids: Vec<String>,
    model_ids_are_explicit: bool,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ChatTool {
    r#type: &'static str,
    function: ChatToolFunction,
}

#[derive(Debug, Serialize)]
struct ChatToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tool_calls: Vec<ChatMessageToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatMessageToolCall {
    id: String,
    r#type: &'static str,
    function: ChatMessageToolCallFunction,
}

#[derive(Debug, Serialize)]
struct ChatMessageToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    choices: Vec<ChatChunkChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChunkChoice {
    delta: ChatChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatChunkDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatDeltaToolCall>,
}

#[derive(Debug, Deserialize)]
struct ChatDeltaToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatDeltaToolCallFunction>,
}

#[derive(Debug, Deserialize)]
struct ChatDeltaToolCallFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponseBody {
    data: Vec<ModelResponseItem>,
}

#[derive(Debug, Deserialize)]
struct ModelResponseItem {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponseBody {
    error: Option<OpenAiErrorBody>,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorBody {
    message: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamOutcome {
    Finished,
    ToolCall,
    Cancelled,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    started: bool,
}

struct TurnWorker {
    request: ModelTurnRequest,
    turn: TurnState,
}

impl TurnWorker {
    fn run(self) {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                self.turn.push(ProviderTurnEvent::Error {
                    error: provider_error(
                        "runtime_build_failed",
                        ProviderErrorCategory::ProviderInternal,
                        error.to_string(),
                    ),
                });
                self.turn.push(ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::Error,
                });
                return;
            }
        };
        runtime.block_on(stream_chat_completion(&self.request, &self.turn));
    }
}

async fn stream_chat_completion(request: &ModelTurnRequest, turn: &TurnState) {
    match stream_chat_completion_inner(request, turn).await {
        Ok(StreamOutcome::Finished) => turn.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        }),
        Ok(StreamOutcome::ToolCall) => turn.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::ToolCall,
        }),
        Ok(StreamOutcome::Cancelled) => {
            turn.push(ProviderTurnEvent::Cancelled);
            turn.push(ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Cancelled,
            });
        }
        Err(error) => {
            turn.push(ProviderTurnEvent::Error { error });
            turn.push(ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::Error,
            });
        }
    }
}

async fn stream_chat_completion_inner(
    request: &ModelTurnRequest,
    turn: &TurnState,
) -> Result<StreamOutcome, ProviderError> {
    let settings = settings();
    let Some(api_key) = settings.api_key.clone() else {
        return Err(provider_error(
            "missing_api_key",
            ProviderErrorCategory::Auth,
            "set BCODE_OPENAI_API_KEY or OPENAI_API_KEY",
        ));
    };
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| {
            provider_error(
                "client_build_failed",
                ProviderErrorCategory::ProviderInternal,
                error.to_string(),
            )
        })?;
    let response = send_chat_completion_request(&client, &settings, api_key, request).await?;
    read_stream_events(response, turn, request).await
}

async fn send_chat_completion_request(
    client: &Client,
    settings: &Settings,
    api_key: String,
    request: &ModelTurnRequest,
) -> Result<reqwest::Response, ProviderError> {
    let url = format!(
        "{}/chat/completions",
        settings.base_url.trim_end_matches('/')
    );
    let request_body = ChatCompletionRequest {
        model: if request.model_id.is_empty() {
            settings.default_model.clone()
        } else {
            request.model_id.clone()
        },
        messages: model_messages_to_chat_messages(request),
        stream: true,
        tools: model_tools_to_chat_tools(request),
        temperature: request.parameters.temperature,
        max_tokens: request.parameters.max_output_tokens,
        top_p: request.parameters.top_p,
        stop: request.parameters.stop_sequences.clone(),
    };
    let response = client
        .post(url)
        .bearer_auth(api_key)
        .json(&request_body)
        .send()
        .await
        .map_err(|error| {
            provider_error(
                "request_failed",
                if error.is_timeout() {
                    ProviderErrorCategory::Timeout
                } else {
                    ProviderErrorCategory::Network
                },
                error.to_string(),
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(error_from_status(status.as_u16(), &body));
    }
    Ok(response)
}

async fn read_stream_events(
    mut response: reqwest::Response,
    turn: &TurnState,
    request: &ModelTurnRequest,
) -> Result<StreamOutcome, ProviderError> {
    let mut buffer = String::new();
    let mut tool_calls = BTreeMap::new();
    loop {
        if turn.is_cancelled() {
            return Ok(StreamOutcome::Cancelled);
        }
        tokio::select! {
            chunk = response.chunk() => {
                let Some(chunk) = chunk.map_err(|error| {
                    provider_error(
                        "stream_read_failed",
                        ProviderErrorCategory::Network,
                        error.to_string(),
                    )
                })? else {
                    return Ok(StreamOutcome::Finished);
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                let outcome = process_stream_buffer(&mut buffer, turn, request, &mut tool_calls)?;
                if matches!(outcome, StreamOutcome::Finished | StreamOutcome::ToolCall) {
                    return Ok(outcome);
                }
            }
            () = turn.cancel_notify.notified() => return Ok(StreamOutcome::Cancelled),
        }
    }
}

fn process_stream_buffer(
    buffer: &mut String,
    turn: &TurnState,
    request: &ModelTurnRequest,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
) -> Result<StreamOutcome, ProviderError> {
    while let Some(position) = buffer.find('\n') {
        let mut line = buffer[..position].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=position);
        let outcome = process_stream_line(&line, turn, request, tool_calls)?;
        if matches!(outcome, StreamOutcome::Finished | StreamOutcome::ToolCall) {
            return Ok(outcome);
        }
    }
    Ok(StreamOutcome::Cancelled)
}

fn process_stream_line(
    line: &str,
    turn: &TurnState,
    request: &ModelTurnRequest,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
) -> Result<StreamOutcome, ProviderError> {
    let Some(data) = line.strip_prefix("data: ") else {
        return Ok(StreamOutcome::Cancelled);
    };
    if data == "[DONE]" {
        return Ok(StreamOutcome::Finished);
    }
    let chunk = serde_json::from_str::<ChatCompletionChunk>(data).map_err(|error| {
        provider_error(
            "stream_decode_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    let name_map = openai_tool_name_map(request);
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            turn.push(ProviderTurnEvent::TextDelta { text: content });
        }
        process_tool_call_deltas(turn, &choice.delta.tool_calls, tool_calls, &name_map);
        if let Some(finish_reason) = choice.finish_reason {
            if finish_reason == "tool_calls" {
                finish_tool_calls(turn, tool_calls, &name_map)?;
                return Ok(StreamOutcome::ToolCall);
            }
            return Ok(StreamOutcome::Finished);
        }
    }
    Ok(StreamOutcome::Cancelled)
}

fn process_tool_call_deltas(
    turn: &TurnState,
    deltas: &[ChatDeltaToolCall],
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
    name_map: &BTreeMap<String, String>,
) {
    for delta in deltas {
        let entry = tool_calls.entry(delta.index).or_default();
        if let Some(id) = &delta.id {
            entry.id = Some(id.clone());
        }
        if let Some(function) = &delta.function {
            if let Some(name) = &function.name {
                entry.name = Some(name.clone());
            }
            if let Some(arguments) = &function.arguments {
                entry.arguments.push_str(arguments);
            }
        }
        if !entry.started
            && let (Some(id), Some(name)) = (&entry.id, &entry.name)
        {
            turn.push(ProviderTurnEvent::ToolCallStarted {
                call_id: id.clone(),
                name: original_tool_name(name, name_map),
            });
            entry.started = true;
        }
    }
}

fn finish_tool_calls(
    turn: &TurnState,
    tool_calls: &BTreeMap<u32, ToolCallAccumulator>,
    name_map: &BTreeMap<String, String>,
) -> Result<(), ProviderError> {
    for accumulator in tool_calls.values() {
        let id = accumulator.id.clone().ok_or_else(|| {
            provider_error(
                "missing_tool_call_id",
                ProviderErrorCategory::ProviderInternal,
                "provider emitted a tool call without an id",
            )
        })?;
        let name = accumulator.name.clone().ok_or_else(|| {
            provider_error(
                "missing_tool_call_name",
                ProviderErrorCategory::ProviderInternal,
                "provider emitted a tool call without a function name",
            )
        })?;
        let arguments = if accumulator.arguments.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&accumulator.arguments).map_err(|error| {
                provider_error(
                    "tool_arguments_decode_failed",
                    ProviderErrorCategory::ProviderInternal,
                    error.to_string(),
                )
            })?
        };
        turn.push(ProviderTurnEvent::ToolCallFinished {
            call: ToolCall {
                id,
                name: original_tool_name(&name, name_map),
                arguments,
            },
        });
    }
    Ok(())
}

fn model_messages_to_chat_messages(request: &ModelTurnRequest) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        messages.push(ChatMessage {
            role: "system",
            content: Some(system_prompt.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    messages.extend(
        request
            .messages
            .iter()
            .filter_map(model_message_to_chat_message),
    );
    messages
}

fn model_message_to_chat_message(message: &ModelMessage) -> Option<ChatMessage> {
    match message.role {
        MessageRole::System | MessageRole::User => text_chat_message(message),
        MessageRole::Assistant => assistant_chat_message(message),
        MessageRole::Tool => tool_chat_message(message),
    }
}

fn text_chat_message(message: &ModelMessage) -> Option<ChatMessage> {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        _ => return None,
    };
    let content = joined_text_content(message);
    (!content.is_empty()).then_some(ChatMessage {
        role,
        content: Some(content),
        tool_calls: Vec::new(),
        tool_call_id: None,
    })
}

fn assistant_chat_message(message: &ModelMessage) -> Option<ChatMessage> {
    let content = joined_text_content(message);
    let tool_calls = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call } => Some(ChatMessageToolCall {
                id: call.id.clone(),
                r#type: "function",
                function: ChatMessageToolCallFunction {
                    name: openai_tool_name(&call.name),
                    arguments: serde_json::to_string(&call.arguments).unwrap_or_default(),
                },
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if content.is_empty() && tool_calls.is_empty() {
        None
    } else {
        Some(ChatMessage {
            role: "assistant",
            content: (!content.is_empty()).then_some(content),
            tool_calls,
            tool_call_id: None,
        })
    }
}

fn tool_chat_message(message: &ModelMessage) -> Option<ChatMessage> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::ToolResult { result } => Some(ChatMessage {
            role: "tool",
            content: Some(result.output.clone()),
            tool_calls: Vec::new(),
            tool_call_id: Some(result.call_id.clone()),
        }),
        _ => None,
    })
}

fn joined_text_content(message: &ModelMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn model_tools_to_chat_tools(request: &ModelTurnRequest) -> Vec<ChatTool> {
    request
        .tools
        .iter()
        .map(|tool| ChatTool {
            r#type: "function",
            function: ChatToolFunction {
                name: openai_tool_name(&tool.name),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            },
        })
        .collect()
}

fn openai_tool_name_map(request: &ModelTurnRequest) -> BTreeMap<String, String> {
    request
        .tools
        .iter()
        .map(|tool| (openai_tool_name(&tool.name), tool.name.clone()))
        .collect()
}

fn original_tool_name(name: &str, name_map: &BTreeMap<String, String>) -> String {
    name_map
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

fn openai_tool_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        provider_id: PROVIDER_ID.to_string(),
        display_name: "OpenAI-Compatible".to_string(),
        capabilities: [
            ProviderCapability::Streaming,
            ProviderCapability::Cancellation,
            ProviderCapability::Tools,
        ]
        .into_iter()
        .collect(),
        metadata: BTreeMap::new(),
    }
}

fn models() -> ModelList {
    let settings = settings();
    if !settings.model_ids_are_explicit
        && let Some(discovered_models) = discover_models(&settings)
    {
        return ModelList {
            models: discovered_models,
        };
    }
    ModelList {
        models: model_infos_from_ids(&settings.model_ids, &settings.default_model),
    }
}

fn model_infos_from_ids(model_ids: &[String], default_model: &str) -> Vec<ModelInfo> {
    model_ids
        .iter()
        .map(|model_id| ModelInfo {
            model_id: model_id.clone(),
            display_name: model_id.clone(),
            is_default: model_id == default_model,
            context_window: None,
            max_output_tokens: None,
            capabilities: [ModelCapability::StreamingText, ModelCapability::ToolCalls]
                .into_iter()
                .collect(),
        })
        .collect()
}

fn discover_models(settings: &Settings) -> Option<Vec<ModelInfo>> {
    let api_key = settings.api_key.clone()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .ok()?;
    runtime
        .block_on(discover_models_async(settings, &api_key))
        .ok()
}

async fn discover_models_async(
    settings: &Settings,
    api_key: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| {
            provider_error(
                "client_build_failed",
                ProviderErrorCategory::ProviderInternal,
                error.to_string(),
            )
        })?;
    let url = format!("{}/models", settings.base_url.trim_end_matches('/'));
    let response = client
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|error| {
            provider_error(
                "models_request_failed",
                if error.is_timeout() {
                    ProviderErrorCategory::Timeout
                } else {
                    ProviderErrorCategory::Network
                },
                error.to_string(),
            )
        })?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        provider_error(
            "models_response_read_failed",
            ProviderErrorCategory::Network,
            error.to_string(),
        )
    })?;
    if !status.is_success() {
        return Err(error_from_status(status.as_u16(), &body));
    }
    let body = serde_json::from_str::<ModelsResponseBody>(&body).map_err(|error| {
        provider_error(
            "models_response_decode_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    Ok(model_infos_from_ids(
        &model_ids_with_default(body.data, &settings.default_model),
        &settings.default_model,
    ))
}

fn model_ids_with_default(models: Vec<ModelResponseItem>, default_model: &str) -> Vec<String> {
    let mut model_ids = models
        .into_iter()
        .map(|model| model.id)
        .filter(|model_id| !model_id.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !model_ids.iter().any(|model_id| model_id == default_model) {
        model_ids.insert(0, default_model.to_string());
    }
    model_ids
}

fn validate_config() -> ValidateConfigResponse {
    let settings = settings();
    if settings.api_key.is_some() {
        ValidateConfigResponse {
            valid: true,
            message: Some("OpenAI-compatible provider API key is configured".to_string()),
        }
    } else {
        ValidateConfigResponse {
            valid: false,
            message: Some("set BCODE_OPENAI_API_KEY or OPENAI_API_KEY".to_string()),
        }
    }
}

fn settings() -> Settings {
    let default_model = first_env(["BCODE_OPENAI_MODEL", "OPENAI_MODEL"])
        .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string());
    let model_ids_env = first_env(["BCODE_OPENAI_MODELS", "OPENAI_MODELS"]);
    let mut model_ids = model_ids_env
        .as_deref()
        .map_or_else(Vec::new, parse_model_list);
    if !model_ids.contains(&default_model) {
        model_ids.insert(0, default_model.clone());
    }
    Settings {
        api_key: first_env(["BCODE_OPENAI_API_KEY", "OPENAI_API_KEY"]),
        base_url: first_env(["BCODE_OPENAI_BASE_URL", "OPENAI_BASE_URL"])
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        default_model,
        model_ids,
        model_ids_are_explicit: model_ids_env.is_some(),
    }
}

fn parse_model_list(models: &str) -> Vec<String> {
    models
        .split(',')
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn first_env<const N: usize>(names: [&str; N]) -> Option<String> {
    names
        .into_iter()
        .find_map(|name| match std::env::var(name) {
            Ok(value) if !value.is_empty() => Some(value),
            _ => None,
        })
}

fn error_from_status(status: u16, body: &str) -> ProviderError {
    let parsed = serde_json::from_str::<ErrorResponseBody>(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|body| body.error.as_ref())
        .map_or_else(|| body.to_string(), |error| error.message.clone());
    let code = parsed
        .as_ref()
        .and_then(|body| body.error.as_ref())
        .and_then(|error| error.code.clone().or_else(|| error.r#type.clone()))
        .unwrap_or_else(|| format!("http_{status}"));
    provider_error(code, category_from_status(status), message)
}

const fn category_from_status(status: u16) -> ProviderErrorCategory {
    match status {
        401 | 403 => ProviderErrorCategory::Auth,
        404 => ProviderErrorCategory::ModelNotFound,
        408 => ProviderErrorCategory::Timeout,
        429 => ProviderErrorCategory::RateLimit,
        400..=499 => ProviderErrorCategory::InvalidRequest,
        _ => ProviderErrorCategory::ProviderInternal,
    }
}

fn provider_error(
    code: impl Into<String>,
    category: ProviderErrorCategory,
    message: impl Into<String>,
) -> ProviderError {
    ProviderError {
        code: code.into(),
        category,
        message: message.into(),
        retryable: matches!(
            category,
            ProviderErrorCategory::Network
                | ProviderErrorCategory::Timeout
                | ProviderErrorCategory::RateLimit
                | ProviderErrorCategory::ProviderInternal
        ),
        provider_message: None,
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_with_default_sorts_deduplicates_and_inserts_default() {
        let model_ids = model_ids_with_default(
            vec![
                ModelResponseItem {
                    id: "z-model".to_string(),
                },
                ModelResponseItem {
                    id: "a-model".to_string(),
                },
                ModelResponseItem {
                    id: "z-model".to_string(),
                },
            ],
            "default-model",
        );

        assert_eq!(
            model_ids,
            vec![
                "default-model".to_string(),
                "a-model".to_string(),
                "z-model".to_string(),
            ]
        );
    }

    #[test]
    fn model_ids_with_default_does_not_duplicate_existing_default() {
        let model_ids = model_ids_with_default(
            vec![ModelResponseItem {
                id: "default-model".to_string(),
            }],
            "default-model",
        );

        assert_eq!(model_ids, vec!["default-model".to_string()]);
    }

    #[test]
    fn model_infos_mark_default_and_tool_capability() {
        let model_infos = model_infos_from_ids(
            &["default-model".to_string(), "other-model".to_string()],
            "default-model",
        );

        assert!(model_infos[0].is_default);
        assert!(!model_infos[1].is_default);
        assert!(
            model_infos[0]
                .capabilities
                .contains(&ModelCapability::ToolCalls)
        );
    }

    #[test]
    fn models_response_body_decodes_openai_models_payload() {
        let body = serde_json::from_str::<ModelsResponseBody>(
            r#"{"data":[{"id":"model-a"},{"id":"model-b"}]}"#,
        )
        .expect("models response body");

        assert_eq!(body.data.len(), 2);
        assert_eq!(body.data[0].id, "model-a");
    }
}

bcode_plugin_sdk::export_plugin!(
    OpenAiCompatibleProviderPlugin,
    include_str!("../bcode-plugin.toml")
);
