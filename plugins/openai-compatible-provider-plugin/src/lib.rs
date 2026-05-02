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
    StopReason, ValidateConfigResponse,
};
use bcode_plugin_sdk::prelude::*;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
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
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
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
struct ChatMessage {
    role: &'static str,
    content: String,
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
    Cancelled,
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
    read_stream_events(response, turn).await
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
) -> Result<StreamOutcome, ProviderError> {
    let mut buffer = String::new();
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
                if process_stream_buffer(&mut buffer, turn)? == StreamOutcome::Finished {
                    return Ok(StreamOutcome::Finished);
                }
            }
            () = turn.cancel_notify.notified() => return Ok(StreamOutcome::Cancelled),
        }
    }
}

fn process_stream_buffer(
    buffer: &mut String,
    turn: &TurnState,
) -> Result<StreamOutcome, ProviderError> {
    while let Some(position) = buffer.find('\n') {
        let mut line = buffer[..position].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=position);
        if process_stream_line(&line, turn)? == StreamOutcome::Finished {
            return Ok(StreamOutcome::Finished);
        }
    }
    Ok(StreamOutcome::Cancelled)
}

fn process_stream_line(line: &str, turn: &TurnState) -> Result<StreamOutcome, ProviderError> {
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
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            turn.push(ProviderTurnEvent::TextDelta { text: content });
        }
        if choice.finish_reason.is_some() {
            return Ok(StreamOutcome::Finished);
        }
    }
    Ok(StreamOutcome::Cancelled)
}

fn model_messages_to_chat_messages(request: &ModelTurnRequest) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        messages.push(ChatMessage {
            role: "system",
            content: system_prompt.clone(),
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
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };
    let content = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if content.is_empty() {
        None
    } else {
        Some(ChatMessage { role, content })
    }
}

fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        provider_id: PROVIDER_ID.to_string(),
        display_name: "OpenAI-Compatible".to_string(),
        capabilities: [
            ProviderCapability::Streaming,
            ProviderCapability::Cancellation,
        ]
        .into_iter()
        .collect(),
        metadata: BTreeMap::new(),
    }
}

fn models() -> ModelList {
    let settings = settings();
    ModelList {
        models: vec![ModelInfo {
            model_id: settings.default_model.clone(),
            display_name: settings.default_model,
            is_default: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: std::iter::once(ModelCapability::StreamingText).collect(),
        }],
    }
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
    Settings {
        api_key: first_env(["BCODE_OPENAI_API_KEY", "OPENAI_API_KEY"]),
        base_url: first_env(["BCODE_OPENAI_BASE_URL", "OPENAI_BASE_URL"])
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        default_model: first_env(["BCODE_OPENAI_MODEL", "OPENAI_MODEL"])
            .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string()),
    }
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

bcode_plugin_sdk::export_plugin!(
    OpenAiCompatibleProviderPlugin,
    include_str!("../bcode-plugin.toml")
);
