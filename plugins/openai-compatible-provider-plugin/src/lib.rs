#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! OpenAI-compatible model provider plugin for Bcode.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bcode_config::AuthMode;
use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    MessageRole, ModelCapability, ModelInfo, ModelList, ModelMessage, ModelTurnRequest,
    OP_CANCEL_TURN, OP_CAPABILITIES, OP_FINISH_TURN, OP_MODELS, OP_POLL_TURN_EVENTS, OP_START_TURN,
    OP_VALIDATE_CONFIG, PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities,
    ProviderCapability, ProviderError, ProviderErrorCategory, ProviderRequestContext,
    ProviderTurnEvent, StartTurnResponse, StopReason, TokenUsage, ToolCall, ValidateConfigResponse,
};
use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::prelude::*;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use zeroize::Zeroizing;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_ID: &str = "gpt-4.1-mini";
const DEFAULT_CODEX_MODEL_ID: &str = "gpt-5.5";
const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1";
const DEFAULT_XAI_MODEL_ID: &str = "grok-4.3"; // from https://docs.x.ai/developers/models/grok-4.3
const PROVIDER_ID: &str = "bcode.openai-compatible";
const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const OPENAI_DIALECT_SETTING: &str = "dialect";
const OPENAI_NAMESPACED_DIALECT_SETTING: &str = "openai.dialect";

/// OpenAI-compatible model provider plugin.
pub struct OpenAiCompatibleProviderPlugin {
    next_turn: u64,
    turns: BTreeMap<String, TurnState>,
    runtime: Result<ProviderRuntime, String>,
}

impl Default for OpenAiCompatibleProviderPlugin {
    fn default() -> Self {
        Self {
            next_turn: 0,
            turns: BTreeMap::new(),
            runtime: ProviderRuntime::new().map_err(|error| error.to_string()),
        }
    }
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
            OP_MODELS => json_response(&self.models()),
            OP_VALIDATE_CONFIG => json_response(&self.validate_config()),
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
        match &self.runtime {
            Ok(runtime) => {
                runtime.spawn(async move {
                    stream_chat_completion(&request, &turn).await;
                });
            }
            Err(error) => push_runtime_error(&turn, error),
        }
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
    auth: AuthSettings,
    auth_diagnostics: AuthDiagnostics,
    dialect: OpenAiCompatibleDialect,
    base_url: String,
    default_model: Option<String>,
    fallback_model: String,
    model_ids: Vec<String>,
    model_ids_are_explicit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiCompatibleDialect {
    ChatCompletions,
    ResponsesApi,
    ChatGptCodex,
}

impl OpenAiCompatibleDialect {
    const fn supports_native_conversation_reuse(self) -> bool {
        matches!(self, Self::ResponsesApi)
    }

    const fn projects_reused_history(self) -> bool {
        self.supports_native_conversation_reuse()
    }

    const fn uses_codex_request_shape(self) -> bool {
        matches!(self, Self::ChatGptCodex)
    }

    const fn metadata_value(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::ResponsesApi => "responses_api",
            Self::ChatGptCodex => "chatgpt_codex",
        }
    }
}

#[derive(Debug, Clone)]
struct AuthDiagnostics {
    source: String,
    mode: String,
    detail: String,
}

#[derive(Debug, Clone)]
enum AuthSettings {
    Missing,
    ApiKey(String),
    ChatGpt {
        access_token: String,
        refresh_token: Option<String>,
        expires_at: Option<u64>,
        account_id: Option<String>,
        profile: Option<String>,
        vault: Option<std::path::PathBuf>,
    },
}

impl AuthSettings {
    const fn is_configured(&self) -> bool {
        !matches!(self, Self::Missing)
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<ChatStreamOptions>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<ResponsesInputItem>,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponsesTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<ResponsesTextOptions>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
}

#[derive(Debug, Serialize)]
struct ResponsesTextOptions {
    verbosity: &'static str,
}

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesInputItem {
    Message {
        role: &'static str,
        content: Vec<ResponsesContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesContent {
    InputText { text: String },
    OutputText { text: String },
}

#[derive(Debug, Serialize)]
struct ResponsesTool {
    r#type: &'static str,
    name: String,
    description: String,
    parameters: serde_json::Value,
    strict: Option<bool>,
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
    #[serde(default)]
    usage: Option<OpenAiUsage>,
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
    #[serde(default)]
    created: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokenDetails>,
    #[serde(default)]
    completion_tokens_details: Option<OpenAiCompletionTokenDetails>,
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    input_tokens_details: Option<OpenAiInputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<OpenAiOutputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct OpenAiPromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
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

fn push_runtime_error(turn: &TurnState, error: &str) {
    turn.push(ProviderTurnEvent::Error {
        error: provider_error(
            "runtime_unavailable",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        ),
    });
    turn.push(ProviderTurnEvent::TurnFinished {
        stop_reason: StopReason::Error,
    });
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
    let mut settings = settings_for_context(&request.provider_context);
    refresh_chatgpt_auth_if_needed(&mut settings).await?;
    if matches!(settings.auth, AuthSettings::Missing) {
        return Err(provider_error(
            "missing_openai_auth",
            ProviderErrorCategory::Auth,
            "run `bcode login openai` (or `bcode login xai`) for ChatGPT subscription auth or set BCODE_OPENAI_API_KEY/OPENAI_API_KEY (or BCODE_XAI_API_KEY/XAI_API_KEY) for API-key auth",
        ));
    }
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
    let model_id = resolve_model_id_for_turn(&settings, request, turn).await;
    match (&settings.auth, settings.dialect) {
        (AuthSettings::ApiKey(api_key), OpenAiCompatibleDialect::ChatCompletions) => {
            let response =
                send_chat_completion_request(&client, &settings, api_key, request, &model_id)
                    .await?;
            read_stream_events(response, turn, request).await
        }
        (AuthSettings::ApiKey(api_key), OpenAiCompatibleDialect::ResponsesApi) => {
            let response =
                send_responses_request(&client, &settings, api_key, request, &model_id).await?;
            read_responses_stream_events(response, turn, request, settings.dialect).await
        }
        (AuthSettings::ChatGpt { access_token, .. }, OpenAiCompatibleDialect::ChatGptCodex) => {
            let response =
                send_responses_request(&client, &settings, access_token, request, &model_id)
                    .await?;
            read_responses_stream_events(response, turn, request, settings.dialect).await
        }
        (AuthSettings::ChatGpt { .. }, _) => Err(provider_error(
            "invalid_dialect_for_auth",
            ProviderErrorCategory::InvalidRequest,
            "ChatGPT subscription auth requires the chatgpt_codex dialect",
        )),
        (AuthSettings::ApiKey(_), OpenAiCompatibleDialect::ChatGptCodex) => Err(provider_error(
            "invalid_dialect_for_auth",
            ProviderErrorCategory::InvalidRequest,
            "chatgpt_codex dialect requires ChatGPT subscription auth; use responses_api or chat_completions with API-key auth",
        )),
        (AuthSettings::Missing, _) => unreachable!("missing auth handled above"),
    }
}

async fn resolve_model_id_for_turn(
    settings: &Settings,
    request: &ModelTurnRequest,
    turn: &TurnState,
) -> String {
    if !request.model_id.trim().is_empty() {
        return request.model_id.clone();
    }
    if let Some(model_id) = &settings.default_model
        && !model_id.trim().is_empty()
    {
        return model_id.clone();
    }
    if let AuthSettings::ApiKey(api_key) = &settings.auth {
        match discover_models_async(settings, api_key).await {
            Ok(models) => {
                if let Some(model) = select_default_model_info(&models) {
                    return model.model_id.clone();
                }
            }
            Err(error) => turn.push(ProviderTurnEvent::Warning {
                message: format!(
                    "OpenAI-compatible model discovery failed ({}); falling back to {}",
                    error.message, settings.fallback_model
                ),
            }),
        }
    }
    settings.fallback_model.clone()
}

async fn send_chat_completion_request(
    client: &Client,
    settings: &Settings,
    api_key: &str,
    request: &ModelTurnRequest,
    model_id: &str,
) -> Result<reqwest::Response, ProviderError> {
    let url = format!(
        "{}/chat/completions",
        settings.base_url.trim_end_matches('/')
    );
    let request_body = ChatCompletionRequest {
        model: model_id.to_string(),
        messages: model_messages_to_chat_messages(request),
        stream: true,
        stream_options: Some(ChatStreamOptions {
            include_usage: true,
        }),
        tools: model_tools_to_chat_tools(request, settings.dialect)?,
        temperature: request.parameters.temperature,
        max_tokens: request.parameters.max_output_tokens,
        top_p: request.parameters.top_p,
        stop: request.parameters.stop_sequences.clone(),
        reasoning_effort: request.parameters.reasoning_effort.map(|e| match e {
            bcode_model::ReasoningEffort::Low => "low".to_string(),
            bcode_model::ReasoningEffort::Medium => "medium".to_string(),
            bcode_model::ReasoningEffort::High => "high".to_string(),
        }),
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

async fn send_responses_request(
    client: &Client,
    settings: &Settings,
    access_token: &str,
    request: &ModelTurnRequest,
    model_id: &str,
) -> Result<reqwest::Response, ProviderError> {
    let url = responses_endpoint(settings);
    let request_body = build_responses_request(settings, request, model_id)?;
    let mut builder = client
        .post(url)
        .bearer_auth(access_token)
        .header("originator", "bcode")
        .header("User-Agent", "bcode/0.0.1")
        .header("accept", "text/event-stream")
        .header("session_id", request.session_id.to_string());
    if settings.dialect.uses_codex_request_shape() {
        builder = builder.header("OpenAI-Beta", "responses=experimental");
    }
    let mut builder = builder.json(&request_body);
    if let AuthSettings::ChatGpt {
        account_id: Some(account_id),
        ..
    } = &settings.auth
    {
        builder = builder.header("ChatGPT-Account-Id", account_id);
    }
    let response = builder.send().await.map_err(|error| {
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
    let name_map = projected_tool_name_map(request, OpenAiCompatibleDialect::ChatCompletions)?;
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
                let outcome = process_stream_buffer(&mut buffer, turn, &mut tool_calls, &name_map)?;
                if matches!(outcome, StreamOutcome::Finished | StreamOutcome::ToolCall) {
                    return Ok(outcome);
                }
            }
            () = turn.cancel_notify.notified() => return Ok(StreamOutcome::Cancelled),
        }
    }
}

async fn read_responses_stream_events(
    mut response: reqwest::Response,
    turn: &TurnState,
    request: &ModelTurnRequest,
    dialect: OpenAiCompatibleDialect,
) -> Result<StreamOutcome, ProviderError> {
    let mut buffer = String::new();
    let name_map = projected_tool_name_map(request, dialect)?;
    let mut tool_calls = BTreeMap::new();
    let mut saw_tool_call = false;
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
                    return Ok(if saw_tool_call { StreamOutcome::ToolCall } else { StreamOutcome::Finished });
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                let outcome = process_responses_stream_buffer(
                    &mut buffer,
                    turn,
                    dialect,
                    &mut tool_calls,
                    &mut saw_tool_call,
                    &name_map,
                )?;
                if matches!(outcome, StreamOutcome::Finished | StreamOutcome::ToolCall) {
                    return Ok(outcome);
                }
            }
            () = turn.cancel_notify.notified() => return Ok(StreamOutcome::Cancelled),
        }
    }
}

fn process_responses_stream_buffer(
    buffer: &mut String,
    turn: &TurnState,
    dialect: OpenAiCompatibleDialect,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
    saw_tool_call: &mut bool,
    name_map: &BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    while let Some(position) = buffer.find('\n') {
        let mut line = buffer[..position].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=position);
        let outcome = process_responses_stream_line(
            line.trim(),
            turn,
            dialect,
            tool_calls,
            saw_tool_call,
            name_map,
        )?;
        if matches!(outcome, StreamOutcome::Finished | StreamOutcome::ToolCall) {
            return Ok(outcome);
        }
    }
    Ok(StreamOutcome::Cancelled)
}

fn process_responses_stream_line(
    line: &str,
    turn: &TurnState,
    dialect: OpenAiCompatibleDialect,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
    saw_tool_call: &mut bool,
    name_map: &BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let Some(data) = line.strip_prefix("data: ") else {
        return Ok(StreamOutcome::Cancelled);
    };
    if data == "[DONE]" {
        return Ok(if *saw_tool_call {
            StreamOutcome::ToolCall
        } else {
            StreamOutcome::Finished
        });
    }
    let event = serde_json::from_str::<serde_json::Value>(data).map_err(|error| {
        provider_error(
            "stream_decode_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    let event_type = event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    match event_type {
        "response.output_text.delta" | "response.refusal.delta" => {
            if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str)
                && !delta.is_empty()
            {
                turn.push(ProviderTurnEvent::TextDelta {
                    text: delta.to_string(),
                });
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str)
                && !delta.is_empty()
            {
                turn.push(ProviderTurnEvent::ReasoningDelta {
                    text: delta.to_string(),
                });
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            process_responses_output_item(&event, turn, tool_calls, saw_tool_call, name_map);
        }
        "response.function_call_arguments.delta" => {
            process_responses_function_arguments_delta(&event, turn, tool_calls);
        }
        "response.function_call_arguments.done" => {
            process_responses_function_arguments_done(&event, tool_calls);
        }
        "response.completed" | "response.done" | "response.incomplete" => {
            if let Some(usage) = token_usage_from_responses_event(&event) {
                turn.push(ProviderTurnEvent::Usage { usage });
            }
            let outcome = if *saw_tool_call {
                finish_tool_calls(turn, tool_calls, name_map, dialect)?;
                StreamOutcome::ToolCall
            } else {
                StreamOutcome::Finished
            };
            if dialect.supports_native_conversation_reuse()
                && let Some(response_id) = event
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(serde_json::Value::as_str)
            {
                turn.push(ProviderTurnEvent::ProviderMetadata {
                    key: "provider_response_id".to_string(),
                    value: response_id.to_string(),
                });
            }
            return Ok(outcome);
        }
        "response.failed" | "error" => {
            let message = event
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| event.get("message").and_then(serde_json::Value::as_str))
                .unwrap_or("OpenAI Responses stream failed");
            let code = event
                .get("error")
                .and_then(|error| error.get("code").or_else(|| error.get("type")))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("responses_stream_failed");
            return Err(provider_error(
                code,
                category_from_openai_error(400, code, message),
                message,
            ));
        }
        _ => {}
    }
    Ok(StreamOutcome::Cancelled)
}

fn process_responses_output_item(
    event: &serde_json::Value,
    turn: &TurnState,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
    saw_tool_call: &mut bool,
    name_map: &BTreeMap<String, String>,
) {
    let Some(item) = event.get("item") else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) != Some("function_call") {
        return;
    }
    *saw_tool_call = true;
    let output_index = responses_output_index(event, item, tool_calls);
    let entry = tool_calls.entry(output_index).or_default();
    if let Some(call_id) = item.get("call_id").and_then(serde_json::Value::as_str) {
        entry.id = Some(call_id.to_string());
    }
    if let Some(name) = item.get("name").and_then(serde_json::Value::as_str) {
        entry.name = Some(name.to_string());
    }
    if let Some(arguments) = item.get("arguments").and_then(serde_json::Value::as_str)
        && !arguments.is_empty()
    {
        entry.arguments = arguments.to_string();
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

fn responses_output_index(
    event: &serde_json::Value,
    item: &serde_json::Value,
    tool_calls: &BTreeMap<u32, ToolCallAccumulator>,
) -> u32 {
    if let Some(output_index) = event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
    {
        return output_index;
    }
    let call_id = item.get("call_id").and_then(serde_json::Value::as_str);
    if let Some((index, _)) = tool_calls
        .iter()
        .find(|(_, call)| call.id.as_deref() == call_id)
    {
        return *index;
    }
    u32::try_from(tool_calls.len()).unwrap_or(u32::MAX)
}

fn process_responses_function_arguments_delta(
    event: &serde_json::Value,
    turn: &TurnState,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
) {
    let output_index = event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0);
    if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str) {
        let entry = tool_calls.entry(output_index).or_default();
        entry.arguments.push_str(delta);
        if let Some(call_id) = &entry.id {
            turn.push(ProviderTurnEvent::ToolCallDelta {
                call_id: call_id.clone(),
                delta: delta.to_string(),
            });
        }
    }
}

fn process_responses_function_arguments_done(
    event: &serde_json::Value,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
) {
    let output_index = event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0);
    if let Some(arguments) = event.get("arguments").and_then(serde_json::Value::as_str) {
        tool_calls.entry(output_index).or_default().arguments = arguments.to_string();
    }
}

fn process_stream_buffer(
    buffer: &mut String,
    turn: &TurnState,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
    name_map: &BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    while let Some(position) = buffer.find('\n') {
        let mut line = buffer[..position].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=position);
        let outcome = process_stream_line(&line, turn, tool_calls, name_map)?;
        if matches!(outcome, StreamOutcome::Finished | StreamOutcome::ToolCall) {
            return Ok(outcome);
        }
    }
    Ok(StreamOutcome::Cancelled)
}

fn process_stream_line(
    line: &str,
    turn: &TurnState,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
    name_map: &BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let Some(data) = line.strip_prefix("data: ") else {
        return Ok(StreamOutcome::Cancelled);
    };
    if data == "[DONE]" {
        return Ok(StreamOutcome::Finished);
    }

    // Some providers (including certain error cases on xAI/OpenAI-compatible)
    // return error payloads as the first data chunk even on 2xx.
    if let Ok(err_body) = serde_json::from_str::<ErrorResponseBody>(data)
        && let Some(err) = err_body.error
    {
        let code = err
            .code
            .or(err.r#type)
            .unwrap_or_else(|| "api_error".to_string());
        let category = category_from_openai_error(400, &code, &err.message);
        return Err(provider_error(code, category, err.message));
    }

    let chunk = serde_json::from_str::<ChatCompletionChunk>(data).map_err(|error| {
        provider_error(
            "stream_decode_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    if let Some(usage) = chunk.usage {
        turn.push(ProviderTurnEvent::Usage {
            usage: token_usage_from_openai_usage(usage),
        });
    }
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            turn.push(ProviderTurnEvent::TextDelta { text: content });
        }
        process_tool_call_deltas(turn, &choice.delta.tool_calls, tool_calls, name_map);
        if let Some(finish_reason) = choice.finish_reason {
            if finish_reason == "tool_calls" {
                finish_tool_calls(
                    turn,
                    tool_calls,
                    name_map,
                    OpenAiCompatibleDialect::ChatCompletions,
                )?;
                return Ok(StreamOutcome::ToolCall);
            }
            return Ok(StreamOutcome::Finished);
        }
    }
    Ok(StreamOutcome::Cancelled)
}

fn token_usage_from_openai_usage(usage: OpenAiUsage) -> TokenUsage {
    let cached_input_tokens = usage
        .prompt_tokens_details
        .and_then(|details| details.cached_tokens)
        .or_else(|| {
            usage
                .input_tokens_details
                .and_then(|details| details.cached_tokens)
        });
    let reasoning_tokens = usage
        .completion_tokens_details
        .and_then(|details| details.reasoning_tokens)
        .or_else(|| {
            usage
                .output_tokens_details
                .and_then(|details| details.reasoning_tokens)
        });
    TokenUsage {
        input_tokens: usage.prompt_tokens.or(usage.input_tokens),
        output_tokens: usage.completion_tokens.or(usage.output_tokens),
        total_tokens: usage.total_tokens,
        cached_input_tokens,
        cache_write_input_tokens: None,
        reasoning_tokens,
    }
}

fn token_usage_from_responses_event(event: &serde_json::Value) -> Option<TokenUsage> {
    let usage = event
        .get("response")
        .and_then(|response| response.get("usage"))
        .or_else(|| event.get("usage"))?;
    serde_json::from_value::<OpenAiUsage>(usage.clone())
        .ok()
        .map(token_usage_from_openai_usage)
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
                if let Some(call_id) = &entry.id {
                    turn.push(ProviderTurnEvent::ToolCallDelta {
                        call_id: call_id.clone(),
                        delta: arguments.clone(),
                    });
                }
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
    dialect: OpenAiCompatibleDialect,
) -> Result<(), ProviderError> {
    for accumulator in tool_calls.values() {
        let id = accumulator.id.clone().ok_or_else(|| {
            provider_error(
                "missing_tool_call_id",
                ProviderErrorCategory::ProviderInternal,
                "provider emitted a tool call without an id",
            )
        })?;
        let provider_name = accumulator.name.clone().ok_or_else(|| {
            provider_error(
                "missing_tool_call_name",
                ProviderErrorCategory::ProviderInternal,
                "provider emitted a tool call without a function name",
            )
        })?;
        let name = original_tool_name(&provider_name, name_map);
        let arguments = parse_tool_arguments(&accumulator.arguments, &id, &name)?;
        turn.push(ProviderTurnEvent::ToolCallFinished {
            call: ToolCall {
                id,
                name: name.clone(),
                arguments: provider_arguments_to_bcode(&name, arguments, dialect),
            },
        });
    }
    Ok(())
}

fn parse_tool_arguments(
    arguments: &str,
    call_id: &str,
    tool_name: &str,
) -> Result<serde_json::Value, ProviderError> {
    if arguments.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(arguments).map_err(|error| {
        provider_error(
            "tool_arguments_decode_failed",
            ProviderErrorCategory::ProviderInternal,
            format!("failed to decode arguments for tool call {call_id} ({tool_name}): {error}"),
        )
    })
}

fn provider_arguments_to_bcode(
    tool_name: &str,
    arguments: serde_json::Value,
    dialect: OpenAiCompatibleDialect,
) -> serde_json::Value {
    if dialect != OpenAiCompatibleDialect::ChatGptCodex || tool_name != "shell.run" {
        return arguments;
    }
    normalize_shell_run_arguments(arguments)
}

fn normalize_shell_run_arguments(arguments: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut object) = arguments else {
        return arguments;
    };
    if !object.contains_key("command")
        && let Some(command) = object.remove("cmd")
    {
        object.insert("command".to_string(), command);
    }
    if !object.contains_key("cwd")
        && let Some(cwd) = object.remove("workdir")
    {
        object.insert("cwd".to_string(), cwd);
    }
    if !object.contains_key("timeout_ms")
        && let Some(timeout) = object.remove("timeout")
    {
        object.insert("timeout_ms".to_string(), timeout_seconds_to_millis(timeout));
    }
    serde_json::Value::Object(object)
}

fn timeout_seconds_to_millis(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .and_then(|seconds| seconds.checked_mul(1_000))
            .map_or(serde_json::Value::Number(number), |millis| {
                serde_json::Value::Number(serde_json::Number::from(millis))
            }),
        other => other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsesInstructionStrategy {
    TopLevelInstructions,
}

struct ResponsesProjection {
    instructions: Option<String>,
    input: Vec<ResponsesInputItem>,
}

fn build_responses_request(
    settings: &Settings,
    request: &ModelTurnRequest,
    model_id: &str,
) -> Result<ResponsesRequest, ProviderError> {
    let previous_response_id = responses_previous_response_id(settings, request);
    let projection = responses_projection(
        request,
        responses_instruction_strategy(settings),
        settings.dialect.projects_reused_history() && previous_response_id.is_some(),
        settings.dialect,
    );
    Ok(ResponsesRequest {
        model: model_id.to_string(),
        instructions: projection.instructions,
        input: projection.input,
        stream: true,
        store: responses_store_enabled(settings, request),
        previous_response_id,
        tools: model_tools_to_responses_tools(request, settings.dialect)?,
        tool_choice: settings
            .dialect
            .uses_codex_request_shape()
            .then_some("auto"),
        parallel_tool_calls: settings.dialect.uses_codex_request_shape().then_some(true),
        text: settings
            .dialect
            .uses_codex_request_shape()
            .then_some(ResponsesTextOptions { verbosity: "low" }),
        include: if settings.dialect.uses_codex_request_shape() {
            vec!["reasoning.encrypted_content"]
        } else {
            Vec::new()
        },
        prompt_cache_key: settings
            .dialect
            .uses_codex_request_shape()
            .then(|| request.session_id.to_string()),
        temperature: request.parameters.temperature,
        max_output_tokens: request.parameters.max_output_tokens,
        top_p: request.parameters.top_p,
    })
}

const fn responses_instruction_strategy(_settings: &Settings) -> ResponsesInstructionStrategy {
    ResponsesInstructionStrategy::TopLevelInstructions
}

const fn responses_store_enabled(settings: &Settings, request: &ModelTurnRequest) -> bool {
    settings.dialect.supports_native_conversation_reuse()
        && request.conversation_reuse.mode.is_enabled()
}

fn responses_previous_response_id(
    settings: &Settings,
    request: &ModelTurnRequest,
) -> Option<String> {
    settings
        .dialect
        .supports_native_conversation_reuse()
        .then(|| {
            request
                .conversation_reuse
                .previous_provider_response_id
                .clone()
        })
        .flatten()
}

fn responses_projection(
    request: &ModelTurnRequest,
    strategy: ResponsesInstructionStrategy,
    project_reused_history: bool,
    dialect: OpenAiCompatibleDialect,
) -> ResponsesProjection {
    let instruction_bundle = response_instruction_bundle(request);
    let input = model_messages_to_responses_input(request, project_reused_history, dialect);
    let instructions = match strategy {
        ResponsesInstructionStrategy::TopLevelInstructions => instruction_bundle,
    };
    ResponsesProjection {
        instructions,
        input,
    }
}

fn response_instruction_bundle(request: &ModelTurnRequest) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(system_prompt) = &request.system_prompt
        && !system_prompt.trim().is_empty()
    {
        parts.push(system_prompt.clone());
    }
    parts.extend(
        request
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::System)
            .map(joined_text_content)
            .filter(|text| !text.trim().is_empty()),
    );
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn model_messages_to_responses_input(
    request: &ModelTurnRequest,
    project_reused_history: bool,
    dialect: OpenAiCompatibleDialect,
) -> Vec<ResponsesInputItem> {
    let start = project_reused_history
        .then(|| {
            request
                .conversation_reuse
                .previous_provider_response_id
                .as_ref()
                .and(request.conversation_reuse.new_messages_start_index)
        })
        .flatten()
        .unwrap_or_default();
    request
        .messages
        .iter()
        .skip(start.min(request.messages.len()))
        .flat_map(|message| model_message_to_responses_input(message, dialect))
        .collect()
}

fn model_message_to_responses_input(
    message: &ModelMessage,
    dialect: OpenAiCompatibleDialect,
) -> Vec<ResponsesInputItem> {
    match message.role {
        MessageRole::System => Vec::new(),
        MessageRole::User => responses_text_message("user", message, true),
        MessageRole::Assistant => responses_assistant_items(message, dialect),
        MessageRole::Tool => responses_tool_items(message),
    }
}

fn responses_text_message(
    role: &'static str,
    message: &ModelMessage,
    input_text: bool,
) -> Vec<ResponsesInputItem> {
    let text = joined_text_content(message);
    if text.is_empty() {
        return Vec::new();
    }
    let content = if input_text {
        ResponsesContent::InputText { text }
    } else {
        ResponsesContent::OutputText { text }
    };
    vec![ResponsesInputItem::Message {
        role,
        content: vec![content],
    }]
}

fn responses_assistant_items(
    message: &ModelMessage,
    dialect: OpenAiCompatibleDialect,
) -> Vec<ResponsesInputItem> {
    let mut items = responses_text_message("assistant", message, false);
    items.extend(message.content.iter().filter_map(|block| match block {
        ContentBlock::ToolCall { call } => Some(ResponsesInputItem::FunctionCall {
            call_id: call.id.clone(),
            name: provider_tool_name(&call.name, dialect),
            arguments: serde_json::to_string(&call.arguments).unwrap_or_default(),
        }),
        _ => None,
    }));
    items
}

fn responses_tool_items(message: &ModelMessage) -> Vec<ResponsesInputItem> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { result } => Some(ResponsesInputItem::FunctionCallOutput {
                call_id: result.call_id.clone(),
                output: result.output.clone(),
            }),
            _ => None,
        })
        .collect()
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

fn model_tools_to_responses_tools(
    request: &ModelTurnRequest,
    dialect: OpenAiCompatibleDialect,
) -> Result<Vec<ResponsesTool>, ProviderError> {
    let tools = project_model_tools(request, dialect)?;
    Ok(tools
        .into_iter()
        .map(|tool| ResponsesTool {
            r#type: "function",
            name: tool.provider_name,
            description: tool.description,
            parameters: tool.parameters,
            strict: if dialect.uses_codex_request_shape() {
                None
            } else {
                Some(false)
            },
        })
        .collect())
}

fn model_tools_to_chat_tools(
    request: &ModelTurnRequest,
    dialect: OpenAiCompatibleDialect,
) -> Result<Vec<ChatTool>, ProviderError> {
    let tools = project_model_tools(request, dialect)?;
    Ok(tools
        .into_iter()
        .map(|tool| ChatTool {
            r#type: "function",
            function: ChatToolFunction {
                name: tool.provider_name,
                description: tool.description,
                parameters: tool.parameters,
            },
        })
        .collect())
}

#[derive(Debug)]
struct ProjectedTool {
    provider_name: String,
    description: String,
    parameters: serde_json::Value,
}

fn project_model_tools(
    request: &ModelTurnRequest,
    dialect: OpenAiCompatibleDialect,
) -> Result<Vec<ProjectedTool>, ProviderError> {
    let mut projected = Vec::with_capacity(request.tools.len());
    let mut names = BTreeMap::new();
    for tool in &request.tools {
        let provider_name = provider_tool_name(&tool.name, dialect);
        if let Some(existing) = names.insert(provider_name.clone(), tool.name.clone()) {
            return Err(provider_error(
                "tool_name_collision",
                ProviderErrorCategory::InvalidRequest,
                format!(
                    "tools '{existing}' and '{}' both project to provider tool name '{provider_name}'",
                    tool.name
                ),
            ));
        }
        projected.push(ProjectedTool {
            provider_name,
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        });
    }
    Ok(projected)
}

fn projected_tool_name_map(
    request: &ModelTurnRequest,
    dialect: OpenAiCompatibleDialect,
) -> Result<BTreeMap<String, String>, ProviderError> {
    let mut names = BTreeMap::new();
    for tool in &request.tools {
        let provider_name = provider_tool_name(&tool.name, dialect);
        if let Some(existing) = names.insert(provider_name.clone(), tool.name.clone()) {
            return Err(provider_error(
                "tool_name_collision",
                ProviderErrorCategory::InvalidRequest,
                format!(
                    "tools '{existing}' and '{}' both project to provider tool name '{provider_name}'",
                    tool.name
                ),
            ));
        }
    }
    Ok(names)
}

fn provider_tool_name(name: &str, _dialect: OpenAiCompatibleDialect) -> String {
    openai_tool_name(name)
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
    let settings = settings();
    ProviderCapabilities {
        provider_id: PROVIDER_ID.to_string(),
        display_name: "OpenAI-Compatible (xAI, Grok, OpenAI, Groq, ...)".to_string(),
        capabilities: [
            ProviderCapability::Streaming,
            ProviderCapability::Cancellation,
            ProviderCapability::Tools,
            ProviderCapability::PromptCaching,
        ]
        .into_iter()
        .collect(),
        metadata: diagnostics_metadata(&settings, None),
    }
}

impl OpenAiCompatibleProviderPlugin {
    fn models(&self) -> ModelList {
        let settings = settings();
        if !settings.model_ids_are_explicit
            && settings.default_model.is_none()
            && let Some(discovered_models) = self.discover_models(&settings)
        {
            return ModelList {
                models: discovered_models,
            };
        }
        ModelList {
            models: model_infos_from_ids(&settings.model_ids, settings.default_model.as_deref()),
        }
    }
}

fn model_infos_from_ids(model_ids: &[String], default_model: Option<&str>) -> Vec<ModelInfo> {
    model_infos_from_items(
        model_ids
            .iter()
            .map(|model_id| ModelResponseItem {
                id: model_id.clone(),
                created: None,
            })
            .collect(),
        default_model,
    )
}

fn model_infos_from_items(
    models: Vec<ModelResponseItem>,
    default_model: Option<&str>,
) -> Vec<ModelInfo> {
    let mut deduped = models
        .into_iter()
        .filter(|model| !model.id.trim().is_empty())
        .map(|model| (model.id.clone(), model))
        .collect::<BTreeMap<_, _>>();
    if let Some(default_model) = default_model
        && !deduped.contains_key(default_model)
    {
        deduped.insert(
            default_model.to_string(),
            ModelResponseItem {
                id: default_model.to_string(),
                created: None,
            },
        );
    }
    let mut models = deduped.into_values().collect::<Vec<_>>();
    models.sort_by(compare_model_candidates);
    let selected_default = default_model
        .map(ToString::to_string)
        .or_else(|| models.first().map(|model| model.id.clone()));
    models
        .into_iter()
        .map(|model| ModelInfo {
            is_default: selected_default.as_deref() == Some(model.id.as_str()),
            model_id: model.id.clone(),
            display_name: model.id,
            context_window: None,
            max_output_tokens: None,
            capabilities: [
                ModelCapability::StreamingText,
                ModelCapability::ToolCalls,
                ModelCapability::PromptCaching,
            ]
            .into_iter()
            .collect(),
        })
        .collect()
}

fn select_default_model_info(models: &[ModelInfo]) -> Option<&ModelInfo> {
    models
        .iter()
        .find(|model| model.is_default)
        .or_else(|| models.first())
}

fn compare_model_candidates(left: &ModelResponseItem, right: &ModelResponseItem) -> CmpOrdering {
    model_preference_key(right)
        .cmp(&model_preference_key(left))
        .then_with(|| left.id.cmp(&right.id))
}

fn model_preference_key(model: &ModelResponseItem) -> (i32, Vec<u32>, i64) {
    let id = model.id.to_ascii_lowercase();
    (
        model_variant_score(&id),
        numeric_version_key(&id),
        model.created.unwrap_or_default(),
    )
}

fn model_variant_score(model_id: &str) -> i32 {
    if contains_any(
        model_id,
        &[
            "embedding",
            "embed",
            "audio",
            "whisper",
            "tts",
            "transcribe",
            "image",
            "dall-e",
            "moderation",
            "realtime",
            "search",
            "rerank",
            "vision",
            "ft:",
        ],
    ) {
        return 0;
    }
    if contains_any(
        model_id,
        &[
            "mini", "nano", "micro", "small", "lite", "flash", "fast", "cheap",
        ],
    ) {
        return 10;
    }
    20
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn numeric_version_key(model_id: &str) -> Vec<u32> {
    let mut key = Vec::new();
    let mut current = String::new();
    for character in model_id.chars() {
        if character.is_ascii_digit() {
            current.push(character);
        } else if !current.is_empty() {
            if let Ok(number) = current.parse() {
                key.push(number);
            }
            current.clear();
        }
    }
    if !current.is_empty()
        && let Ok(number) = current.parse()
    {
        key.push(number);
    }
    key
}

impl OpenAiCompatibleProviderPlugin {
    fn discover_models(&self, settings: &Settings) -> Option<Vec<ModelInfo>> {
        let AuthSettings::ApiKey(api_key) = settings.auth.clone() else {
            return None;
        };
        let runtime = self.runtime.as_ref().ok()?;
        let settings = settings.clone();
        runtime
            .block_on(async move { discover_models_async(&settings, &api_key).await })
            .ok()
            .and_then(Result::ok)
    }
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
    Ok(model_infos_from_items(
        body.data,
        settings.default_model.as_deref(),
    ))
}

impl OpenAiCompatibleProviderPlugin {
    fn validate_config(&self) -> ValidateConfigResponse {
        let mut settings = settings();
        let refresh_status = self.validate_chatgpt_refresh(&mut settings);
        let valid = settings.auth.is_configured() && refresh_status.is_ok();
        let refresh_metadata = match &refresh_status {
            Ok(status) => status.clone(),
            Err(error) => format!("failed:{}:{:?}", error.code, error.category),
        };
        if valid {
            ValidateConfigResponse {
                valid: true,
                message: Some(format!(
                    "OpenAI-compatible provider authentication is configured ({}) (supports xAI/Grok, OpenAI, etc.)",
                    settings.auth_diagnostics.detail
                )),
                metadata: diagnostics_metadata(&settings, Some(&refresh_metadata)),
            }
        } else {
            ValidateConfigResponse {
                valid: false,
                message: Some(validation_failure_message(
                    &settings,
                    refresh_status.as_ref().err(),
                )),
                metadata: diagnostics_metadata(&settings, Some(&refresh_metadata)),
            }
        }
    }
}

fn validation_failure_message(
    settings: &Settings,
    refresh_error: Option<&ProviderError>,
) -> String {
    if let Some(error) = refresh_error {
        return format!(
            "OpenAI-compatible provider authentication refresh failed ({}: {:?}); {}",
            error.code, error.category, error.message
        );
    }
    format!(
        "OpenAI-compatible provider authentication is not configured ({}); run `bcode login openai` (or `bcode login xai`) or set BCODE_OPENAI_API_KEY/OPENAI_API_KEY (or BCODE_XAI_API_KEY/XAI_API_KEY)",
        settings.auth_diagnostics.detail
    )
}

fn diagnostics_metadata(
    settings: &Settings,
    token_refresh_status: Option<&str>,
) -> BTreeMap<String, String> {
    let mut metadata = [
        (
            "auth_configured".to_string(),
            settings.auth.is_configured().to_string(),
        ),
        (
            "auth_source".to_string(),
            settings.auth_diagnostics.source.clone(),
        ),
        (
            "auth_mode".to_string(),
            settings.auth_diagnostics.mode.clone(),
        ),
        (
            "auth_detail".to_string(),
            settings.auth_diagnostics.detail.clone(),
        ),
        (
            "dialect".to_string(),
            settings.dialect.metadata_value().to_string(),
        ),
        ("endpoint".to_string(), responses_or_chat_endpoint(settings)),
        ("api_base_url".to_string(), settings.base_url.clone()),
        (
            "default_model".to_string(),
            settings
                .default_model
                .clone()
                .unwrap_or_else(|| "<auto-discovered>".to_string()),
        ),
        (
            "fallback_model".to_string(),
            settings.fallback_model.clone(),
        ),
        (
            "model_list_source".to_string(),
            if settings.model_ids_are_explicit {
                "environment"
            } else if matches!(settings.auth, AuthSettings::ChatGpt { .. }) {
                "bundled_chatgpt_codex_defaults"
            } else {
                "provider_or_defaults"
            }
            .to_string(),
        ),
    ]
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    if let Some(token_refresh_status) = token_refresh_status {
        metadata.insert(
            "token_refresh_status".to_string(),
            token_refresh_status.to_string(),
        );
    }
    metadata
}

fn settings() -> Settings {
    settings_for_context(&ProviderRequestContext::default())
}

fn settings_for_context(context: &ProviderRequestContext) -> Settings {
    let saved = saved_openai_auth();
    let xai_mode = saved_has_xai_keys(&saved) || env_has_xai_keys();
    let chatgpt_mode = saved_openai_auth_is_chatgpt(&saved) && !xai_mode;
    let fallback_model = if xai_mode {
        DEFAULT_XAI_MODEL_ID.to_string()
    } else if chatgpt_mode {
        DEFAULT_CODEX_MODEL_ID.to_string()
    } else {
        DEFAULT_MODEL_ID.to_string()
    };
    let default_model = first_env([
        "BCODE_XAI_MODEL",
        "XAI_MODEL",
        "BCODE_OPENAI_MODEL",
        "OPENAI_MODEL",
    ])
    .or_else(|| chatgpt_mode.then(|| DEFAULT_CODEX_MODEL_ID.to_string()));
    let model_ids_env = first_env([
        "BCODE_XAI_MODELS",
        "XAI_MODELS",
        "BCODE_OPENAI_MODELS",
        "OPENAI_MODELS",
    ]);
    let mut model_ids = model_ids_env
        .as_deref()
        .map_or_else(|| default_model_ids(chatgpt_mode), parse_model_list);
    if let Some(default_model) = &default_model
        && !model_ids.contains(default_model)
    {
        model_ids.insert(0, default_model.clone());
    }
    let (auth, auth_diagnostics) = openai_auth_settings(&saved);
    let base_url = first_context_or_env(
        context,
        "base_url",
        "openai.base_url",
        [
            "BCODE_XAI_BASE_URL",
            "XAI_BASE_URL",
            "BCODE_OPENAI_BASE_URL",
            "OPENAI_BASE_URL",
        ],
    )
    .or_else(|| saved.values.get("BCODE_XAI_BASE_URL").cloned())
    .or_else(|| saved.values.get("XAI_BASE_URL").cloned())
    .or_else(|| saved.values.get("BCODE_OPENAI_BASE_URL").cloned())
    .or_else(|| saved.values.get("OPENAI_BASE_URL").cloned())
    .unwrap_or_else(|| {
        if xai_mode {
            DEFAULT_XAI_BASE_URL.to_string()
        } else {
            DEFAULT_BASE_URL.to_string()
        }
    });
    let dialect = resolve_dialect(&auth, context);
    Settings {
        auth,
        auth_diagnostics,
        dialect,
        base_url,
        default_model,
        fallback_model,
        model_ids,
        model_ids_are_explicit: model_ids_env.is_some(),
    }
}

#[derive(Debug, Default)]
struct SavedOpenAiAuth {
    values: BTreeMap<String, String>,
    mode: Option<AuthMode>,
    profile: Option<String>,
    vault: Option<std::path::PathBuf>,
}

fn saved_openai_auth() -> SavedOpenAiAuth {
    let Ok(config) = bcode_config::load_config() else {
        return SavedOpenAiAuth::default();
    };
    let Some(auth) = config.auth.openai else {
        return SavedOpenAiAuth::default();
    };
    if auth.backend != "sshenv" {
        return SavedOpenAiAuth::default();
    }
    let vault = auth
        .vault
        .clone()
        .unwrap_or_else(bcode_config::default_auth_vault_path);
    let store = sshenv_vault::SshenvStore::new(sshenv_vault::SshenvStoreConfig::new(vault.clone()));
    let Ok(Some(profile)) = store.get_profile(&auth.profile) else {
        return SavedOpenAiAuth {
            values: BTreeMap::new(),
            mode: Some(auth.mode),
            profile: Some(auth.profile),
            vault: Some(vault),
        };
    };
    SavedOpenAiAuth {
        values: profile
            .into_iter()
            .map(|(key, value)| (key, value.to_string()))
            .collect(),
        mode: Some(auth.mode),
        profile: Some(auth.profile),
        vault: Some(vault),
    }
}

fn saved_openai_auth_is_chatgpt(saved: &SavedOpenAiAuth) -> bool {
    matches!(saved.mode, Some(AuthMode::ChatGpt))
        || saved
            .values
            .get("BCODE_OPENAI_AUTH_MODE")
            .is_some_and(|mode| mode == "chatgpt")
        || saved.values.contains_key("BCODE_OPENAI_CODEX_ACCESS_TOKEN")
}

fn saved_has_xai_keys(saved: &SavedOpenAiAuth) -> bool {
    saved.values.contains_key("BCODE_XAI_API_KEY")
        || saved.values.contains_key("XAI_API_KEY")
        || saved.values.contains_key("BCODE_XAI_CODEX_ACCESS_TOKEN") // unlikely but for symmetry
}

fn env_has_xai_keys() -> bool {
    env_value("BCODE_XAI_API_KEY").is_some() || env_value("XAI_API_KEY").is_some()
}

fn openai_auth_settings(saved: &SavedOpenAiAuth) -> (AuthSettings, AuthDiagnostics) {
    // XAI takes precedence for generic OpenAI-compatible usage (xAI, Grok, etc.)
    if let Some(api_key) = env_value("BCODE_XAI_API_KEY") {
        return (
            AuthSettings::ApiKey(api_key),
            AuthDiagnostics {
                source: "environment".to_string(),
                mode: "api_key (xai)".to_string(),
                detail: "environment variable BCODE_XAI_API_KEY".to_string(),
            },
        );
    }
    if let Some(api_key) = env_value("XAI_API_KEY") {
        return (
            AuthSettings::ApiKey(api_key),
            AuthDiagnostics {
                source: "environment".to_string(),
                mode: "api_key (xai)".to_string(),
                detail: "environment variable XAI_API_KEY".to_string(),
            },
        );
    }
    if let Some(api_key) = saved.values.get("BCODE_XAI_API_KEY").cloned() {
        return (
            AuthSettings::ApiKey(api_key),
            saved_auth_diagnostics(
                saved,
                "api_key (xai)",
                "saved sshenv API key BCODE_XAI_API_KEY",
            ),
        );
    }
    if let Some(api_key) = saved.values.get("XAI_API_KEY").cloned() {
        return (
            AuthSettings::ApiKey(api_key),
            saved_auth_diagnostics(saved, "api_key (xai)", "saved sshenv API key XAI_API_KEY"),
        );
    }
    if let Some(api_key) = env_value("BCODE_OPENAI_API_KEY") {
        return (
            AuthSettings::ApiKey(api_key),
            AuthDiagnostics {
                source: "environment".to_string(),
                mode: "api_key".to_string(),
                detail: "environment variable BCODE_OPENAI_API_KEY".to_string(),
            },
        );
    }
    if let Some(api_key) = env_value("OPENAI_API_KEY") {
        return (
            AuthSettings::ApiKey(api_key),
            AuthDiagnostics {
                source: "environment".to_string(),
                mode: "api_key".to_string(),
                detail: "environment variable OPENAI_API_KEY".to_string(),
            },
        );
    }
    if let Some(api_key) = saved.values.get("BCODE_OPENAI_API_KEY").cloned() {
        return (
            AuthSettings::ApiKey(api_key),
            saved_auth_diagnostics(
                saved,
                "api_key",
                "saved sshenv API key BCODE_OPENAI_API_KEY",
            ),
        );
    }
    if let Some(api_key) = saved.values.get("OPENAI_API_KEY").cloned() {
        return (
            AuthSettings::ApiKey(api_key),
            saved_auth_diagnostics(saved, "api_key", "saved sshenv API key OPENAI_API_KEY"),
        );
    }
    let saved_mode = saved
        .values
        .get("BCODE_OPENAI_AUTH_MODE")
        .map(String::as_str);
    if saved_openai_auth_is_chatgpt(saved) || saved_mode == Some("chatgpt") {
        return saved_chatgpt_auth_settings(saved);
    }
    (
        AuthSettings::Missing,
        AuthDiagnostics {
            source: "missing".to_string(),
            mode: saved
                .mode
                .as_ref()
                .map_or("unknown", |mode| match mode {
                    AuthMode::ApiKey => "api_key",
                    AuthMode::ChatGpt => "chatgpt",
                })
                .to_string(),
            detail: saved.profile.as_ref().map_or_else(
                || {
                    "no saved OpenAI-compatible auth profile and no API key environment variable"
                        .to_string()
                },
                |profile| format!("saved profile '{profile}' did not contain usable credentials"),
            ),
        },
    )
}

fn env_value(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn saved_auth_diagnostics(
    saved: &SavedOpenAiAuth,
    mode: &str,
    credential_description: &str,
) -> AuthDiagnostics {
    let location = match (&saved.profile, &saved.vault) {
        (Some(profile), Some(vault)) => format!(
            "{credential_description} from profile '{profile}' in vault {}",
            vault.display()
        ),
        (Some(profile), None) => {
            format!("{credential_description} from profile '{profile}'")
        }
        (None, Some(vault)) => format!("{credential_description} from vault {}", vault.display()),
        (None, None) => credential_description.to_string(),
    };
    AuthDiagnostics {
        source: "sshenv".to_string(),
        mode: mode.to_string(),
        detail: location,
    }
}

fn saved_chatgpt_auth_settings(saved: &SavedOpenAiAuth) -> (AuthSettings, AuthDiagnostics) {
    let Some(access_token) = saved.values.get("BCODE_OPENAI_CODEX_ACCESS_TOKEN").cloned() else {
        return (
            AuthSettings::Missing,
            saved_auth_diagnostics(
                saved,
                "chatgpt",
                "saved sshenv ChatGPT/Codex auth without an access token",
            ),
        );
    };
    let account_id = saved
        .values
        .get("BCODE_OPENAI_CODEX_ACCOUNT_ID")
        .cloned()
        .or_else(|| {
            saved
                .values
                .get("BCODE_OPENAI_CODEX_ID_TOKEN")
                .and_then(|token| chatgpt_account_id_from_access_token(token))
        })
        .or_else(|| chatgpt_account_id_from_access_token(&access_token));
    (
        AuthSettings::ChatGpt {
            access_token,
            refresh_token: saved
                .values
                .get("BCODE_OPENAI_CODEX_REFRESH_TOKEN")
                .cloned(),
            expires_at: saved
                .values
                .get("BCODE_OPENAI_CODEX_EXPIRES_AT")
                .and_then(|value| value.parse().ok()),
            account_id,
            profile: saved.profile.clone(),
            vault: saved.vault.clone(),
        },
        saved_auth_diagnostics(saved, "chatgpt", "saved sshenv ChatGPT/Codex auth"),
    )
}

fn default_model_ids(chatgpt_mode: bool) -> Vec<String> {
    if chatgpt_mode {
        return [
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.3-codex",
            "gpt-5.2-codex",
            "gpt-5.1-codex-mini",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect();
    }
    Vec::new()
}

#[derive(Debug, Deserialize)]
struct OpenAiOauthTokenResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl OpenAiCompatibleProviderPlugin {
    fn validate_chatgpt_refresh(&self, settings: &mut Settings) -> Result<String, ProviderError> {
        match &settings.auth {
            AuthSettings::Missing | AuthSettings::ApiKey(_) => Ok("not_applicable".to_string()),
            AuthSettings::ChatGpt {
                expires_at: None, ..
            } => Ok("not_checked_no_expiry".to_string()),
            AuthSettings::ChatGpt {
                expires_at: Some(expires_at),
                refresh_token,
                ..
            } => {
                let now = unix_timestamp();
                if *expires_at > now + 60 {
                    return Ok(format!("not_needed_expires_in_{}s", expires_at - now));
                }
                if refresh_token.is_none() {
                    return Err(provider_error(
                        "missing_refresh_token",
                        ProviderErrorCategory::Auth,
                        "saved ChatGPT/Codex access token is expired or expiring soon and no refresh token is saved; run `bcode login openai` again",
                    ));
                }
                let runtime = self.runtime.as_ref().map_err(|error| {
                    provider_error(
                        "runtime_unavailable",
                        ProviderErrorCategory::ProviderInternal,
                        error.clone(),
                    )
                })?;
                let mut refreshed_settings = settings.clone();
                refreshed_settings = runtime
                    .block_on(async move {
                        refresh_chatgpt_auth_if_needed(&mut refreshed_settings)
                            .await
                            .map(|()| refreshed_settings)
                    })
                    .map_err(|error| {
                        provider_error(
                            "runtime_unavailable",
                            ProviderErrorCategory::ProviderInternal,
                            error.to_string(),
                        )
                    })??;
                *settings = refreshed_settings;
                Ok("refreshed".to_string())
            }
        }
    }
}

async fn refresh_chatgpt_auth_if_needed(settings: &mut Settings) -> Result<(), ProviderError> {
    let AuthSettings::ChatGpt {
        refresh_token: Some(refresh_token),
        expires_at,
        profile,
        vault,
        ..
    } = &settings.auth
    else {
        return Ok(());
    };
    let Some(expires_at) = expires_at else {
        return Ok(());
    };
    if *expires_at > unix_timestamp() + 60 {
        return Ok(());
    }
    let refreshed = refresh_openai_codex_token(refresh_token).await?;
    let next_refresh_token = refreshed
        .refresh_token
        .clone()
        .unwrap_or_else(|| refresh_token.clone());
    let next_expires_at =
        unix_timestamp() + refreshed.expires_in.unwrap_or(3600).saturating_sub(60);
    let account_id = refreshed
        .id_token
        .as_deref()
        .and_then(chatgpt_account_id_from_access_token)
        .or_else(|| chatgpt_account_id_from_access_token(&refreshed.access_token));
    if let (Some(profile), Some(vault)) = (profile, vault) {
        store_refreshed_chatgpt_auth(
            profile,
            vault,
            &refreshed,
            &next_refresh_token,
            next_expires_at,
            account_id.as_deref(),
        )?;
    }
    settings.auth = AuthSettings::ChatGpt {
        access_token: refreshed.access_token,
        refresh_token: Some(next_refresh_token),
        expires_at: Some(next_expires_at),
        account_id,
        profile: profile.clone(),
        vault: vault.clone(),
    };
    Ok(())
}

fn store_refreshed_chatgpt_auth(
    profile: &str,
    vault: &std::path::Path,
    refreshed: &OpenAiOauthTokenResponse,
    next_refresh_token: &str,
    next_expires_at: u64,
    account_id: Option<&str>,
) -> Result<(), ProviderError> {
    let store = sshenv_vault::SshenvStore::new(sshenv_vault::SshenvStoreConfig::new(vault));
    set_codex_secret(
        &store,
        profile,
        "BCODE_OPENAI_CODEX_ACCESS_TOKEN",
        refreshed.access_token.clone(),
    )?;
    if let Some(id_token) = &refreshed.id_token {
        set_codex_secret(
            &store,
            profile,
            "BCODE_OPENAI_CODEX_ID_TOKEN",
            id_token.clone(),
        )?;
    }
    set_codex_secret(
        &store,
        profile,
        "BCODE_OPENAI_CODEX_REFRESH_TOKEN",
        next_refresh_token.to_string(),
    )?;
    set_codex_secret(
        &store,
        profile,
        "BCODE_OPENAI_CODEX_EXPIRES_AT",
        next_expires_at.to_string(),
    )?;
    if let Some(account_id) = account_id {
        set_codex_secret(
            &store,
            profile,
            "BCODE_OPENAI_CODEX_ACCOUNT_ID",
            account_id.to_string(),
        )?;
    }
    Ok(())
}

fn set_codex_secret(
    store: &sshenv_vault::SshenvStore,
    profile: &str,
    key: &str,
    value: String,
) -> Result<(), ProviderError> {
    store
        .set_secret(profile, key, Zeroizing::new(value))
        .map_err(|error| {
            provider_error(
                "token_store_failed",
                ProviderErrorCategory::Auth,
                error.to_string(),
            )
        })
}

async fn refresh_openai_codex_token(
    refresh_token: &str,
) -> Result<OpenAiOauthTokenResponse, ProviderError> {
    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", OPENAI_CODEX_CLIENT_ID),
        ("refresh_token", refresh_token),
    ];
    let response = Client::new()
        .post(OPENAI_CODEX_TOKEN_URL)
        .form(&params)
        .send()
        .await
        .map_err(|error| {
            provider_error(
                "token_refresh_failed",
                ProviderErrorCategory::Network,
                error.to_string(),
            )
        })?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        provider_error(
            "token_refresh_response_failed",
            ProviderErrorCategory::Network,
            error.to_string(),
        )
    })?;
    if !status.is_success() {
        return Err(error_from_status(status.as_u16(), &body));
    }
    serde_json::from_str(&body).map_err(|error| {
        provider_error(
            "token_refresh_decode_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })
}

fn chatgpt_account_id_from_access_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    claims
        .get("chatgpt_account_id")
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(serde_json::Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
        })
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
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

fn first_context_or_env<const N: usize>(
    context: &ProviderRequestContext,
    key: &str,
    namespaced_key: &str,
    env_names: [&str; N],
) -> Option<String> {
    context
        .settings
        .get(namespaced_key)
        .or_else(|| context.settings.get(key))
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| first_env(env_names))
}

fn resolve_dialect(
    auth: &AuthSettings,
    context: &ProviderRequestContext,
) -> OpenAiCompatibleDialect {
    if matches!(auth, AuthSettings::ChatGpt { .. }) {
        return OpenAiCompatibleDialect::ChatGptCodex;
    }
    first_context_or_env(
        context,
        OPENAI_DIALECT_SETTING,
        OPENAI_NAMESPACED_DIALECT_SETTING,
        ["BCODE_OPENAI_DIALECT", "OPENAI_DIALECT"],
    )
    .as_deref()
    .and_then(parse_dialect)
    .unwrap_or(OpenAiCompatibleDialect::ChatCompletions)
}

fn parse_dialect(value: &str) -> Option<OpenAiCompatibleDialect> {
    match value.trim().to_ascii_lowercase().as_str() {
        "chat_completions" | "chat-completions" | "chat" | "completions" => {
            Some(OpenAiCompatibleDialect::ChatCompletions)
        }
        "responses_api" | "responses-api" | "responses" | "openai_responses" => {
            Some(OpenAiCompatibleDialect::ResponsesApi)
        }
        "chatgpt_codex" | "chatgpt-codex" | "codex" => Some(OpenAiCompatibleDialect::ChatGptCodex),
        _ => None,
    }
}

fn responses_endpoint(settings: &Settings) -> String {
    match settings.dialect {
        OpenAiCompatibleDialect::ChatGptCodex => OPENAI_CODEX_API_ENDPOINT.to_string(),
        OpenAiCompatibleDialect::ResponsesApi => {
            format!("{}/responses", settings.base_url.trim_end_matches('/'))
        }
        OpenAiCompatibleDialect::ChatCompletions => responses_or_chat_endpoint(settings),
    }
}

fn responses_or_chat_endpoint(settings: &Settings) -> String {
    match settings.dialect {
        OpenAiCompatibleDialect::ChatCompletions => {
            format!(
                "{}/chat/completions",
                settings.base_url.trim_end_matches('/')
            )
        }
        OpenAiCompatibleDialect::ResponsesApi => {
            format!("{}/responses", settings.base_url.trim_end_matches('/'))
        }
        OpenAiCompatibleDialect::ChatGptCodex => OPENAI_CODEX_API_ENDPOINT.to_string(),
    }
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
    let category = category_from_openai_error(status, &code, &message);
    provider_error(code, category, message)
}

fn category_from_openai_error(status: u16, code: &str, message: &str) -> ProviderErrorCategory {
    if is_context_length_error(code, message) {
        return ProviderErrorCategory::ContextLength;
    }
    category_from_status(status)
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

fn is_context_length_error(code: &str, message: &str) -> bool {
    let code = code.to_ascii_lowercase();
    if code.contains("context_length") || code.contains("context_window") {
        return true;
    }

    let message = message.to_ascii_lowercase();
    message.contains("context_length_exceeded")
        || message.contains("maximum context length")
        || message.contains("prompt is too long")
        || message.contains("input is too long")
        || message.contains("too many tokens")
        || (message.contains("context length")
            && (message.contains("exceed") || message.contains("too long")))
        || (message.contains("context window")
            && (message.contains("exceed")
                || message.contains("too long")
                || message.contains("overflow")))
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

    fn model_item(id: &str, created: i64) -> ModelResponseItem {
        ModelResponseItem {
            id: id.to_string(),
            created: Some(created),
        }
    }

    fn test_settings(auth: AuthSettings, dialect: OpenAiCompatibleDialect) -> Settings {
        Settings {
            auth,
            auth_diagnostics: AuthDiagnostics {
                source: "test".to_string(),
                mode: "test".to_string(),
                detail: "test".to_string(),
            },
            dialect,
            base_url: DEFAULT_BASE_URL.to_string(),
            default_model: Some("model".to_string()),
            fallback_model: DEFAULT_MODEL_ID.to_string(),
            model_ids: vec!["model".to_string()],
            model_ids_are_explicit: true,
        }
    }

    fn test_chatgpt_auth() -> AuthSettings {
        AuthSettings::ChatGpt {
            access_token: "token".to_string(),
            refresh_token: None,
            expires_at: None,
            account_id: None,
            profile: None,
            vault: None,
        }
    }

    fn test_api_key_auth() -> AuthSettings {
        AuthSettings::ApiKey("token".to_string())
    }

    fn test_request(messages: Vec<ModelMessage>) -> ModelTurnRequest {
        ModelTurnRequest {
            session_id: "00000000-0000-0000-0000-000000000000"
                .parse()
                .expect("static nil UUID should parse"),
            turn_id: "turn".to_string(),
            model_id: "model".to_string(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: None,
            messages,
            tools: Vec::new(),
            parameters: bcode_model::ModelParameters::default(),
            prompt_cache: bcode_model::PromptCacheHints::default(),
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: BTreeMap::new(),
        }
    }

    fn text_message(role: MessageRole, text: &str) -> ModelMessage {
        ModelMessage {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn discovered_model_infos_prefers_newest_flagship() {
        let model_infos = model_infos_from_items(
            vec![
                model_item("gpt-4.1", 100),
                model_item("gpt-5", 90),
                model_item("gpt-4.1-mini", 300),
                model_item("text-embedding-3-large", 400),
            ],
            None,
        );

        assert_eq!(
            select_default_model_info(&model_infos).map(|model| model.model_id.as_str()),
            Some("gpt-5")
        );
    }

    #[test]
    fn discovered_model_infos_prefers_flagship_over_newer_mini() {
        let model_infos = model_infos_from_items(
            vec![model_item("gpt-5-mini", 300), model_item("gpt-4.1", 100)],
            None,
        );

        assert_eq!(
            select_default_model_info(&model_infos).map(|model| model.model_id.as_str()),
            Some("gpt-4.1")
        );
    }

    #[test]
    fn model_infos_mark_default_and_tool_capability() {
        let model_infos = model_infos_from_ids(
            &["default-model".to_string(), "other-model".to_string()],
            Some("default-model"),
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

    #[test]
    fn chat_completion_usage_maps_to_provider_neutral_usage() {
        let usage = serde_json::from_str::<OpenAiUsage>(
            r#"{
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": { "cached_tokens": 3 },
                "completion_tokens_details": { "reasoning_tokens": 2 }
            }"#,
        )
        .expect("usage should decode");

        let usage = token_usage_from_openai_usage(usage);

        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(15));
        assert_eq!(usage.cached_input_tokens, Some(3));
        assert_eq!(usage.reasoning_tokens, Some(2));
    }

    #[test]
    fn responses_completed_usage_maps_to_provider_neutral_usage() {
        let event = serde_json::json!({
            "type": "response.completed",
            "response": {
                "usage": {
                    "input_tokens": 20,
                    "output_tokens": 7,
                    "total_tokens": 27,
                    "input_tokens_details": { "cached_tokens": 4 },
                    "output_tokens_details": { "reasoning_tokens": 6 }
                }
            }
        });

        let usage = token_usage_from_responses_event(&event).expect("usage should parse");

        assert_eq!(usage.input_tokens, Some(20));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(usage.total_tokens, Some(27));
        assert_eq!(usage.cached_input_tokens, Some(4));
        assert_eq!(usage.reasoning_tokens, Some(6));
    }

    #[test]
    fn responses_top_level_strategy_omits_system_messages_and_uses_instructions() {
        let request = ModelTurnRequest {
            session_id: "00000000-0000-0000-0000-000000000000"
                .parse()
                .expect("static nil UUID should parse"),
            turn_id: "turn".to_string(),
            model_id: "model".to_string(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: Some("top-level".to_string()),
            messages: vec![
                ModelMessage {
                    role: MessageRole::System,
                    content: vec![ContentBlock::Text {
                        text: "dynamic system".to_string(),
                    }],
                },
                ModelMessage {
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text {
                        text: "hello".to_string(),
                    }],
                },
            ],
            tools: Vec::new(),
            parameters: bcode_model::ModelParameters::default(),
            prompt_cache: bcode_model::PromptCacheHints::default(),
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: BTreeMap::new(),
        };

        let projection = responses_projection(
            &request,
            ResponsesInstructionStrategy::TopLevelInstructions,
            false,
            OpenAiCompatibleDialect::ChatGptCodex,
        );
        let instructions = projection.instructions.expect("instructions should exist");
        let encoded_items =
            serde_json::to_value(&projection.input).expect("input should serialize");

        assert!(instructions.contains("top-level"));
        assert!(instructions.contains("dynamic system"));
        assert_eq!(projection.input.len(), 1);
        assert!(!encoded_items.to_string().contains(r#""role":"system""#));
    }

    #[test]
    fn final_responses_request_has_required_instructions_without_system_input() {
        let request = ModelTurnRequest {
            session_id: "00000000-0000-0000-0000-000000000000"
                .parse()
                .expect("static nil UUID should parse"),
            turn_id: "turn".to_string(),
            model_id: "model".to_string(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: Some("top-level".to_string()),
            messages: vec![
                ModelMessage {
                    role: MessageRole::System,
                    content: vec![ContentBlock::Text {
                        text: "dynamic system".to_string(),
                    }],
                },
                ModelMessage {
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text {
                        text: "hello".to_string(),
                    }],
                },
            ],
            tools: Vec::new(),
            parameters: bcode_model::ModelParameters::default(),
            prompt_cache: bcode_model::PromptCacheHints::default(),
            conversation_reuse: bcode_model::ConversationReuseHints {
                mode: bcode_model::ConversationReuseMode::Auto,
                key: Some("key".to_string()),
                previous_provider_response_id: Some("resp_previous".to_string()),
                new_messages_start_index: Some(0),
            },
            metadata: BTreeMap::new(),
        };
        let settings = test_settings(test_chatgpt_auth(), OpenAiCompatibleDialect::ChatGptCodex);

        let body =
            build_responses_request(&settings, &request, "model").expect("request should build");
        let encoded = serde_json::to_value(&body).expect("request should serialize");
        let encoded_text = encoded.to_string();

        assert!(
            body.instructions.as_deref().is_some_and(|text| {
                text.contains("top-level") && text.contains("dynamic system")
            })
        );
        assert!(!encoded_text.contains(r#""role":"system""#));
        assert!(encoded.get("instructions").is_some());
        assert_eq!(
            encoded.get("store").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(encoded.get("previous_response_id").is_none());
    }

    #[test]
    fn responses_reuse_sends_only_new_messages() {
        let request = ModelTurnRequest {
            session_id: "00000000-0000-0000-0000-000000000000"
                .parse()
                .expect("static nil UUID should parse"),
            turn_id: "turn".to_string(),
            model_id: "model".to_string(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: None,
            messages: vec![
                ModelMessage {
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text {
                        text: "old".to_string(),
                    }],
                },
                ModelMessage {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "old answer".to_string(),
                    }],
                },
                ModelMessage {
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text {
                        text: "new".to_string(),
                    }],
                },
            ],
            tools: Vec::new(),
            parameters: bcode_model::ModelParameters::default(),
            prompt_cache: bcode_model::PromptCacheHints::default(),
            conversation_reuse: bcode_model::ConversationReuseHints {
                mode: bcode_model::ConversationReuseMode::Auto,
                key: Some("key".to_string()),
                previous_provider_response_id: Some("resp_1".to_string()),
                new_messages_start_index: Some(2),
            },
            metadata: BTreeMap::new(),
        };

        let items = model_messages_to_responses_input(
            &request,
            true,
            OpenAiCompatibleDialect::ResponsesApi,
        );

        assert_eq!(items.len(), 1);
    }

    #[test]
    fn chatgpt_codex_request_uses_full_history_when_reuse_hint_exists() {
        let mut request = test_request(vec![
            text_message(MessageRole::User, "old"),
            text_message(MessageRole::Assistant, "old answer"),
            text_message(MessageRole::User, "new"),
        ]);
        request.conversation_reuse = bcode_model::ConversationReuseHints {
            mode: bcode_model::ConversationReuseMode::Auto,
            key: Some("key".to_string()),
            previous_provider_response_id: Some("resp_1".to_string()),
            new_messages_start_index: Some(2),
        };
        let settings = test_settings(test_chatgpt_auth(), OpenAiCompatibleDialect::ChatGptCodex);

        let body =
            build_responses_request(&settings, &request, "model").expect("request should build");
        let encoded = serde_json::to_value(&body).expect("request should serialize");

        assert_eq!(body.input.len(), 3);
        assert_eq!(
            encoded.get("store").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(encoded.get("previous_response_id").is_none());
        assert_eq!(
            encoded
                .get("tool_choice")
                .and_then(serde_json::Value::as_str),
            Some("auto")
        );
        assert_eq!(
            encoded
                .get("parallel_tool_calls")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn responses_api_request_projects_reused_history_only_when_previous_id_is_sent() {
        let mut request = test_request(vec![
            text_message(MessageRole::User, "old"),
            text_message(MessageRole::Assistant, "old answer"),
            text_message(MessageRole::User, "new"),
        ]);
        request.conversation_reuse = bcode_model::ConversationReuseHints {
            mode: bcode_model::ConversationReuseMode::Auto,
            key: Some("key".to_string()),
            previous_provider_response_id: Some("resp_1".to_string()),
            new_messages_start_index: Some(2),
        };
        let settings = test_settings(test_api_key_auth(), OpenAiCompatibleDialect::ResponsesApi);

        let body =
            build_responses_request(&settings, &request, "model").expect("request should build");

        assert_eq!(body.previous_response_id.as_deref(), Some("resp_1"));
        assert!(body.store);
        assert_eq!(body.input.len(), 1);
    }

    #[test]
    fn projected_tool_name_collision_is_rejected() {
        let mut request = test_request(Vec::new());
        request.tools = vec![
            bcode_model::ToolDefinition {
                name: "fs.read".to_string(),
                description: "read".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
                side_effect: bcode_model::ToolSideEffect::ReadOnly,
                requires_permission: false,
            },
            bcode_model::ToolDefinition {
                name: "fs_read".to_string(),
                description: "read".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
                side_effect: bcode_model::ToolSideEffect::ReadOnly,
                requires_permission: false,
            },
        ];

        let error = project_model_tools(&request, OpenAiCompatibleDialect::ChatCompletions)
            .expect_err("collision should fail");

        assert_eq!(error.code, "tool_name_collision");
    }

    #[test]
    fn responses_tool_call_done_parses_original_tool_and_codex_argument_aliases() {
        let mut request = test_request(Vec::new());
        request.tools = vec![bcode_model::ToolDefinition {
            name: "shell.run".to_string(),
            description: "run shell".to_string(),
            input_schema: serde_json::json!({ "type": "object" }),
            side_effect: bcode_model::ToolSideEffect::ExecuteProcess,
            requires_permission: true,
        }];
        let name_map = projected_tool_name_map(&request, OpenAiCompatibleDialect::ChatGptCodex)
            .expect("tool names should project");
        let turn = TurnState::default();
        let mut tool_calls = BTreeMap::new();
        let mut saw_tool_call = false;

        let added = process_responses_stream_line(
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"shell_run","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/tmp\",\"timeout\":2}"}}"#,
            &turn,
            OpenAiCompatibleDialect::ChatGptCodex,
            &mut tool_calls,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("tool event should process");
        let completed = process_responses_stream_line(
            r#"data: {"type":"response.completed","response":{"id":"resp_123"}}"#,
            &turn,
            OpenAiCompatibleDialect::ChatGptCodex,
            &mut tool_calls,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("completed event should process");

        assert!(matches!(added, StreamOutcome::Cancelled));
        assert!(matches!(completed, StreamOutcome::ToolCall));
        assert!(turn.drain().iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ToolCallFinished { call }
                if call.name == "shell.run"
                    && call.arguments.get("command").and_then(serde_json::Value::as_str) == Some("ls")
                    && call.arguments.get("cwd").and_then(serde_json::Value::as_str) == Some("/tmp")
                    && call.arguments.get("timeout_ms").and_then(serde_json::Value::as_u64) == Some(2_000)
        )));
    }

    #[test]
    fn responses_completed_emits_provider_response_id_metadata() {
        let turn = TurnState::default();
        let mut tool_calls = BTreeMap::new();
        let mut saw_tool_call = false;
        let name_map = BTreeMap::new();

        let outcome = process_responses_stream_line(
            r#"data: {"type":"response.completed","response":{"id":"resp_123"}}"#,
            &turn,
            OpenAiCompatibleDialect::ResponsesApi,
            &mut tool_calls,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("stream event should process");

        assert!(matches!(outcome, StreamOutcome::Finished));
        assert!(turn.drain().iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ProviderMetadata { key, value }
                if key == "provider_response_id" && value == "resp_123"
        )));
    }

    #[test]
    fn chatgpt_codex_completed_does_not_emit_reuse_metadata() {
        let turn = TurnState::default();
        let mut tool_calls = BTreeMap::new();
        let mut saw_tool_call = false;
        let name_map = BTreeMap::new();

        let outcome = process_responses_stream_line(
            r#"data: {"type":"response.completed","response":{"id":"resp_123"}}"#,
            &turn,
            OpenAiCompatibleDialect::ChatGptCodex,
            &mut tool_calls,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("stream event should process");

        assert!(matches!(outcome, StreamOutcome::Finished));
        assert!(!turn.drain().iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ProviderMetadata { key, .. } if key == "provider_response_id"
        )));
    }

    #[test]
    fn http_context_length_error_is_classified_for_overflow_recovery() {
        let error = error_from_status(
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, your messages resulted in 9000 tokens.","code":"context_length_exceeded","type":"invalid_request_error"}}"#,
        );

        assert_eq!(error.category, ProviderErrorCategory::ContextLength);
        assert_eq!(error.code, "context_length_exceeded");
    }

    #[test]
    fn unsupported_temperature_error_stays_invalid_request() {
        let error = error_from_status(
            400,
            r#"{"error":{"message":"Unsupported parameter: temperature","code":"unsupported_parameter","type":"invalid_request_error"}}"#,
        );

        assert_eq!(error.category, ProviderErrorCategory::InvalidRequest);
    }

    #[test]
    fn responses_stream_context_length_error_is_classified_for_overflow_recovery() {
        let turn = TurnState::default();
        let mut tool_calls = BTreeMap::new();
        let mut saw_tool_call = false;
        let name_map = BTreeMap::new();

        let error = process_responses_stream_line(
            r#"data: {"type":"response.failed","error":{"code":"context_length_exceeded","message":"input is too long for the model context window"}}"#,
            &turn,
            OpenAiCompatibleDialect::ResponsesApi,
            &mut tool_calls,
            &mut saw_tool_call,
            &name_map,
        )
        .expect_err("context error should fail");

        assert_eq!(error.category, ProviderErrorCategory::ContextLength);
    }

    #[test]
    fn saved_chatgpt_auth_reports_sshenv_diagnostics() {
        let saved = SavedOpenAiAuth {
            values: std::iter::once((
                "BCODE_OPENAI_CODEX_ACCESS_TOKEN".to_string(),
                "token".to_string(),
            ))
            .collect(),
            mode: Some(AuthMode::ChatGpt),
            profile: Some("bcode-openai".to_string()),
            vault: Some(std::path::PathBuf::from("/tmp/bcode-auth-vault")),
        };

        let (auth, diagnostics) = saved_chatgpt_auth_settings(&saved);

        assert!(auth.is_configured());
        assert_eq!(diagnostics.source, "sshenv");
        assert_eq!(diagnostics.mode, "chatgpt");
        assert!(diagnostics.detail.contains("bcode-openai"));
    }
}

bcode_plugin_sdk::export_plugin!(
    OpenAiCompatibleProviderPlugin,
    include_str!("../bcode-plugin.toml")
);
