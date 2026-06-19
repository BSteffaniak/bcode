#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! OpenAI-compatible model provider plugin for Bcode.

mod auth_pool_state;
mod model_catalog;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bcode_config::AuthMode;
use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    MessageRole, ModelCapability, ModelInfo, ModelList, ModelListRequest, ModelMessage,
    ModelReasoningCapabilitySource, ModelTurnRequest, NativeWebSearchRequest,
    NativeWebSearchResponse, NativeWebSearchResult, OP_CANCEL_TURN, OP_CAPABILITIES,
    OP_FINISH_TURN, OP_MODELS, OP_NATIVE_WEB_SEARCH, OP_POLL_TURN_EVENTS, OP_START_TURN,
    OP_VALIDATE_CONFIG, PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities,
    ProviderCapability, ProviderError, ProviderErrorCategory, ProviderRequestContext,
    ProviderRequestProjection, ProviderTurnEvent, StartTurnResponse, StopReason, TokenUsage,
    ToolCall, ValidateConfigResponse,
};
use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::prelude::*;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
    state: Mutex<OpenAiCompatibleProviderState>,
    runtime: Result<ProviderRuntime, String>,
}

#[derive(Debug, Default)]
struct OpenAiCompatibleProviderState {
    next_turn: u64,
    turns: BTreeMap<String, TurnState>,
}

impl Default for OpenAiCompatibleProviderPlugin {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
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

impl ConcurrentRustPlugin for OpenAiCompatibleProviderPlugin {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_provider_service(&context)
    }
}

impl RustPlugin for OpenAiCompatibleProviderPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_provider_service(&context)
    }
}

fn openai_request_projection(request: &ModelTurnRequest) -> ProviderRequestProjection {
    let settings = settings_for_context(&request.provider_context);
    match settings.dialect {
        OpenAiCompatibleDialect::ChatCompletions => {
            let messages = model_messages_to_chat_messages(request);
            ProviderRequestProjection {
                provider: Some("bcode.openai-compatible".to_string()),
                api_shape: Some("chat_completions".to_string()),
                message_count: Some(messages.len()),
                original_message_count: Some(request.messages.len()),
                sent_message_count: Some(request.messages.len()),
                omitted_message_count: Some(0),
                cache_point_count: Some(prompt_cache_point_count(request)),
                emitted_cache_point_count: Some(0),
                dropped_cache_point_count: Some(prompt_cache_point_count(request)),
                detail: Some(
                    "explicit cache points are not supported by this API shape".to_string(),
                ),
                ..ProviderRequestProjection::default()
            }
        }
        dialect => {
            let previous_response_id = responses_previous_response_id(&settings, request);
            let project_reused_history =
                dialect.projects_reused_history() && previous_response_id.is_some();
            let mut projection = responses_projection(
                request,
                responses_instruction_strategy(&settings),
                project_reused_history,
                dialect,
            );
            let had_provider_reasoning_state = has_provider_reasoning_state(dialect, request);
            prepend_provider_reasoning_state(&mut projection.input, dialect, request);
            let sent = responses_projected_message_count(request, project_reused_history);
            ProviderRequestProjection {
                provider: Some("bcode.openai-compatible".to_string()),
                api_shape: Some("responses".to_string()),
                input_item_count: Some(projection.input.len()),
                original_message_count: Some(request.messages.len()),
                sent_message_count: Some(sent),
                omitted_message_count: Some(request.messages.len().saturating_sub(sent)),
                cache_point_count: Some(prompt_cache_point_count(request)),
                emitted_cache_point_count: Some(0),
                dropped_cache_point_count: Some(prompt_cache_point_count(request)),
                used_previous_response_id: previous_response_id.is_some(),
                detail: Some(format!(
                    "explicit cache points are not supported by this API shape; prompt_cache_key={}; reasoning_context={}; provider_reasoning_state={}",
                    dialect.uses_codex_request_shape(),
                    dialect.uses_codex_request_shape(),
                    had_provider_reasoning_state
                )),
                ..ProviderRequestProjection::default()
            }
        }
    }
}

fn responses_projected_message_count(
    request: &ModelTurnRequest,
    project_reused_history: bool,
) -> usize {
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
        .len()
        .saturating_sub(start.min(request.messages.len()))
}

fn prompt_cache_point_count(request: &ModelTurnRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::CachePoint { .. }))
        .count()
}

impl OpenAiCompatibleProviderPlugin {
    fn invoke_provider_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != MODEL_PROVIDER_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported model provider service interface",
            );
        }

        match context.request.operation.as_str() {
            OP_CAPABILITIES => json_response(&capabilities()),
            OP_MODELS => self.models_response(&context.request),
            OP_VALIDATE_CONFIG => json_response(&self.validate_config()),
            OP_NATIVE_WEB_SEARCH => self.native_web_search(&context.request),
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

    fn models_response(&self, request: &ServiceRequest) -> ServiceResponse {
        json_response(&self.models(&model_list_request(request)))
    }

    fn native_web_search(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<NativeWebSearchRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        match &self.runtime {
            Ok(runtime) => match runtime.block_on(native_web_search_inner(request)) {
                Ok(Ok(response)) => json_response(&response),
                Ok(Err(error)) => ServiceResponse::error(error.code, error.message),
                Err(error) => ServiceResponse::error("runtime_error", error.to_string()),
            },
            Err(error) => ServiceResponse::error("runtime_error", error),
        }
    }

    fn start_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<ModelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let mut state = self
            .state
            .lock()
            .expect("openai-compatible provider state lock should not be poisoned");
        state.next_turn += 1;
        let provider_turn_id = format!("openai-compatible-turn-{}", state.next_turn);
        let turn = TurnState::default();
        turn.push(ProviderTurnEvent::TurnStarted);
        turn.push(ProviderTurnEvent::RequestProjection {
            projection: openai_request_projection(&request),
        });
        state.turns.insert(provider_turn_id.clone(), turn.clone());
        drop(state);
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
            .state
            .lock()
            .expect("openai-compatible provider state lock should not be poisoned")
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
        let turn = self
            .state
            .lock()
            .expect("openai-compatible provider state lock should not be poisoned")
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
            .expect("openai-compatible provider state lock should not be poisoned")
            .turns
            .remove(&request.provider_turn_id);
        if let Some(turn) = turn {
            turn.cancel();
        }
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
    request_timeout: Option<Duration>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReasoningRequestShape {
    supports_reasoning_object: bool,
    include_state: &'static [&'static str],
    include_summary: &'static [&'static str],
    fallback_effort_values: &'static [&'static str],
    fallback_summary_values: &'static [&'static str],
    source: ModelReasoningCapabilitySource,
}

impl OpenAiCompatibleDialect {
    const fn reasoning_request_shape(self) -> ReasoningRequestShape {
        match self {
            Self::ChatCompletions => ReasoningRequestShape {
                supports_reasoning_object: false,
                include_state: &[],
                include_summary: &[],
                fallback_effort_values: &["low", "medium", "high"],
                fallback_summary_values: &[],
                source: ModelReasoningCapabilitySource::GenericFallback,
            },
            Self::ResponsesApi => ReasoningRequestShape {
                supports_reasoning_object: true,
                include_state: &[],
                include_summary: &["reasoning.summary"],
                fallback_effort_values: &["none", "minimal", "low", "medium", "high", "xhigh"],
                fallback_summary_values: &["auto", "concise", "detailed"],
                source: ModelReasoningCapabilitySource::KnownModelTable,
            },
            Self::ChatGptCodex => ReasoningRequestShape {
                supports_reasoning_object: true,
                include_state: &["reasoning.encrypted_content"],
                include_summary: &["reasoning.summary"],
                fallback_effort_values: &["none", "minimal", "low", "medium", "high", "xhigh"],
                fallback_summary_values: &["auto", "concise", "detailed"],
                source: ModelReasoningCapabilitySource::KnownModelTable,
            },
        }
    }
}

const fn default_reasoning_request_shape() -> ReasoningRequestShape {
    OpenAiCompatibleDialect::ChatGptCodex.reasoning_request_shape()
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
        storage: BTreeMap<String, bcode_model::ProviderAuthStorageRef>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoningOptions>,
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
struct ResponsesReasoningOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'static str>,
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
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<ResponsesReasoningSummary>,
        encrypted_content: String,
    },
}

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesReasoningSummary {
    SummaryText { text: String },
}

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesContent {
    InputText { text: String },
    OutputText { text: String },
    InputImage { image_url: String },
}

#[derive(Debug, Serialize)]
struct ResponsesTool {
    r#type: &'static str,
    name: String,
    description: String,
    parameters: serde_json::Value,
    strict: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ResponsesNativeSearchBody {
    #[serde(default)]
    output: Vec<ResponsesNativeSearchOutputItem>,
}

#[derive(Debug, Deserialize)]
struct ResponsesNativeSearchOutputItem {
    #[serde(default)]
    content: Vec<ResponsesNativeSearchContentItem>,
}

#[derive(Debug, Deserialize)]
struct ResponsesNativeSearchContentItem {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    annotations: Vec<ResponsesNativeSearchAnnotation>,
}

#[derive(Debug, Deserialize)]
struct ResponsesNativeSearchAnnotation {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
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
    content: Option<ChatMessageContent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tool_calls: Vec<ChatMessageToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ChatMessageContent {
    Text(String),
    Parts(Vec<ChatMessageContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatMessageContentPart {
    Text { text: String },
    ImageUrl { image_url: ChatImageUrl },
}

#[derive(Debug, Serialize)]
struct ChatImageUrl {
    url: String,
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
    #[serde(flatten)]
    metadata: BTreeMap<String, serde_json::Value>,
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

#[derive(Debug, Default)]
struct ReasoningItemAccumulator {
    id: Option<String>,
    encrypted_content: Option<String>,
    summary: Vec<String>,
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

async fn native_web_search_inner(
    request: NativeWebSearchRequest,
) -> Result<NativeWebSearchResponse, ProviderError> {
    let mut settings = settings_for_context(&request.provider_context);
    refresh_chatgpt_auth_if_needed(&mut settings).await?;
    let AuthSettings::ApiKey(api_key) = &settings.auth else {
        return Ok(native_search_unavailable(
            "OpenAI Responses API native web search currently requires API-key auth",
        ));
    };
    if !matches!(settings.dialect, OpenAiCompatibleDialect::ResponsesApi) {
        return Ok(native_search_unavailable(
            "OpenAI native web search requires the responses_api dialect",
        ));
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|error| {
            provider_error(
                "client_build_failed",
                ProviderErrorCategory::ProviderInternal,
                error.to_string(),
            )
        })?;
    let model_id = native_search_model_id(&settings);
    let body = build_native_web_search_body(&request, &model_id);
    let response = client
        .post(responses_endpoint(&settings))
        .bearer_auth(api_key)
        .header("originator", "bcode")
        .header("User-Agent", "bcode/0.0.1")
        .json(&body)
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
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(error_from_status(status.as_u16(), &text));
    }
    Ok(native_web_search_response(&request.query, &text))
}

fn native_search_unavailable(message: &str) -> NativeWebSearchResponse {
    NativeWebSearchResponse {
        provider: PROVIDER_ID.to_string(),
        results: Vec::new(),
        partial: true,
        message: Some(message.to_string()),
    }
}

fn native_search_model_id(settings: &Settings) -> String {
    settings
        .default_model
        .clone()
        .unwrap_or_else(|| settings.fallback_model.clone())
}

fn build_native_web_search_body(
    request: &NativeWebSearchRequest,
    model_id: &str,
) -> serde_json::Value {
    let mut query = request.query.clone();
    if let Some(site) = request
        .site
        .as_deref()
        .filter(|site| !site.trim().is_empty())
    {
        query = format!("site:{} {}", site.trim(), query);
    }
    let mut prompt = format!(
        "Search the web for this query and return concise results with titles, URLs, and snippets.\n\nQuery: {query}"
    );
    if let Some(freshness) = request.freshness.as_deref() {
        prompt.push_str("\nFreshness: ");
        prompt.push_str(freshness);
    }
    if let Some(region) = request.region.as_deref() {
        prompt.push_str("\nRegion: ");
        prompt.push_str(region);
    }
    if let Some(safe_search) = request.safe_search.as_deref() {
        prompt.push_str("\nSafe search: ");
        prompt.push_str(safe_search);
    }
    if let Some(max_results) = request.max_results {
        prompt.push_str("\nMaximum results: ");
        prompt.push_str(&max_results.to_string());
    }
    serde_json::json!({
        "model": model_id,
        "input": prompt,
        "stream": false,
        "store": false,
        "tools": [{ "type": "web_search_preview" }]
    })
}

fn native_web_search_response(query: &str, body: &str) -> NativeWebSearchResponse {
    let Ok(decoded) = serde_json::from_str::<ResponsesNativeSearchBody>(body) else {
        return NativeWebSearchResponse {
            provider: PROVIDER_ID.to_string(),
            results: vec![NativeWebSearchResult {
                title: format!("Search response for {query}"),
                url: String::new(),
                snippet: body.to_string(),
                published: None,
                source: Some("openai_responses_api".to_string()),
            }],
            partial: true,
            message: Some(
                "provider response did not match expected Responses API shape".to_string(),
            ),
        };
    };
    let mut results = Vec::new();
    let mut fallback_text = String::new();
    for output in decoded.output {
        for content in output.content {
            if let Some(text) = content.text {
                append_text_with_space(&mut fallback_text, &text);
            }
            for annotation in content.annotations {
                if annotation.r#type == "url_citation"
                    && let Some(url) = annotation.url
                    && !url.is_empty()
                {
                    results.push(NativeWebSearchResult {
                        title: annotation.title.unwrap_or_else(|| url.clone()),
                        url,
                        snippet: String::new(),
                        published: None,
                        source: Some("openai_responses_api".to_string()),
                    });
                }
            }
        }
    }
    if results.is_empty() && !fallback_text.trim().is_empty() {
        results.push(NativeWebSearchResult {
            title: format!("Search response for {query}"),
            url: String::new(),
            snippet: fallback_text.trim().to_string(),
            published: None,
            source: Some("openai_responses_api".to_string()),
        });
    } else if !fallback_text.trim().is_empty() {
        for result in &mut results {
            result.snippet = fallback_text.trim().to_string();
        }
    }
    NativeWebSearchResponse {
        provider: PROVIDER_ID.to_string(),
        partial: results.is_empty(),
        message: results
            .is_empty()
            .then(|| "provider returned no web search results".to_string()),
        results,
    }
}

fn append_text_with_space(buffer: &mut String, text: &str) {
    if !buffer.is_empty() {
        buffer.push(' ');
    }
    buffer.push_str(text.trim());
}

async fn stream_chat_completion(request: &ModelTurnRequest, turn: &TurnState) {
    match stream_chat_completion_with_failover(request, turn).await {
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

async fn stream_chat_completion_with_failover(
    request: &ModelTurnRequest,
    turn: &TurnState,
) -> Result<StreamOutcome, ProviderError> {
    if request.provider_context.auth_candidates.is_empty() {
        return stream_chat_completion_inner(request, turn).await;
    }
    let mut skipped_profiles = Vec::new();
    let mut last_error = None;
    for candidate in &request.provider_context.auth_candidates {
        if !auth_pool_state::is_profile_available(
            request.provider_context.auth_pool.as_deref(),
            candidate.profile.as_deref(),
        ) {
            if let Some(profile) = &candidate.profile {
                skipped_profiles.push(profile.clone());
            }
            continue;
        }
        let mut candidate_request = request.clone();
        candidate_request.provider_context.auth_profile = candidate.profile.clone();
        candidate_request.provider_context.auth = Some(candidate.auth.clone());
        candidate_request.provider_context.env = candidate.env.clone();
        match stream_chat_completion_inner(&candidate_request, turn).await {
            Ok(outcome) => return Ok(outcome),
            Err(error) if is_subscription_quota_error(&error) => {
                if let Some(profile) = &candidate.profile {
                    auth_pool_state::mark_profile_quota_limited(
                        request.provider_context.auth_pool.as_deref(),
                        Some(profile),
                        quota_error_reason(&error),
                        &error.message,
                        quota_error_cooldown(&error),
                    );
                    turn.push(ProviderTurnEvent::Warning {
                        message: format!(
                            "OpenAI subscription auth profile '{profile}' appears quota-limited; trying the next configured subscription."
                        ),
                    });
                }
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        provider_error(
            "openai_auth_pool_exhausted",
            ProviderErrorCategory::RateLimit,
            all_subscriptions_exhausted_message(&skipped_profiles),
        )
    }))
}

fn is_subscription_quota_error(error: &ProviderError) -> bool {
    if error.category != ProviderErrorCategory::RateLimit {
        return false;
    }
    let code = error.code.to_ascii_lowercase();
    let message = error.message.to_ascii_lowercase();
    code.contains("quota")
        || code.contains("rate_limit")
        || message.contains("quota")
        || message.contains("usage limit")
        || message.contains("rate limit")
        || message.contains("too many requests")
}

fn quota_error_reason(error: &ProviderError) -> &'static str {
    let message = error.message.to_ascii_lowercase();
    if message.contains("week") || message.contains("weekly") {
        "weekly_quota"
    } else if message.contains("rate limit") || message.contains("too many requests") {
        "rate_limit"
    } else {
        "quota"
    }
}

fn quota_error_cooldown(error: &ProviderError) -> Duration {
    if quota_error_reason(error) == "weekly_quota" {
        Duration::from_secs(7 * 24 * 60 * 60)
    } else {
        Duration::from_secs(5 * 60 * 60)
    }
}

fn all_subscriptions_exhausted_message(skipped_profiles: &[String]) -> String {
    if skipped_profiles.is_empty() {
        "all configured OpenAI subscriptions are currently quota-limited; add another subscription with `bcode login openai --add-subscription` or try again after reset".to_string()
    } else {
        format!(
            "all configured OpenAI subscriptions are currently quota-limited; skipped cooldown profiles: {}; add another subscription with `bcode login openai --add-subscription` or try again after reset",
            skipped_profiles.join(", ")
        )
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
    let client = model_stream_client(settings.request_timeout).map_err(|error| {
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

fn model_stream_client(request_timeout: Option<Duration>) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder();
    if let Some(timeout) = request_timeout {
        builder = builder.timeout(timeout);
    }
    builder.build()
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
                let Some(chunk) = chunk.map_err(|error| stream_read_error(&error))? else {
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
    let mut reasoning_items = BTreeMap::new();
    let mut saw_tool_call = false;
    loop {
        if turn.is_cancelled() {
            return Ok(StreamOutcome::Cancelled);
        }
        tokio::select! {
            chunk = response.chunk() => {
                let Some(chunk) = chunk.map_err(|error| stream_read_error(&error))? else {
                    return Ok(if saw_tool_call { StreamOutcome::ToolCall } else { StreamOutcome::Finished });
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                let outcome = process_responses_stream_buffer(
                    &mut buffer,
                    turn,
                    dialect,
                    &mut tool_calls,
                    &mut reasoning_items,
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
    reasoning_items: &mut BTreeMap<u32, ReasoningItemAccumulator>,
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
            reasoning_items,
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
    reasoning_items: &mut BTreeMap<u32, ReasoningItemAccumulator>,
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
            process_responses_reasoning_output_item(&event, reasoning_items);
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
            push_responses_provider_state(turn, reasoning_items);
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

fn process_responses_reasoning_output_item(
    event: &serde_json::Value,
    reasoning_items: &mut BTreeMap<u32, ReasoningItemAccumulator>,
) {
    let Some(item) = event.get("item") else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) != Some("reasoning") {
        return;
    }
    let output_index = event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or_else(|| u32::try_from(reasoning_items.len()).unwrap_or(u32::MAX));
    let entry = reasoning_items.entry(output_index).or_default();
    if let Some(id) = item.get("id").and_then(serde_json::Value::as_str) {
        entry.id = Some(id.to_string());
    }
    if let Some(encrypted_content) = item
        .get("encrypted_content")
        .and_then(serde_json::Value::as_str)
        .filter(|encrypted_content| !encrypted_content.is_empty())
    {
        entry.encrypted_content = Some(encrypted_content.to_string());
    }
    if let Some(summary) = item.get("summary").and_then(serde_json::Value::as_array) {
        entry.summary = summary
            .iter()
            .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
            .filter(|text| !text.is_empty())
            .map(ToString::to_string)
            .collect();
    }
}

fn push_responses_provider_state(
    turn: &TurnState,
    reasoning_items: &BTreeMap<u32, ReasoningItemAccumulator>,
) {
    let state = OpenAiProviderState {
        reasoning_items: reasoning_items
            .values()
            .filter_map(|item| {
                Some(OpenAiReasoningStateItem {
                    id: item.id.clone()?,
                    summary: item.summary.clone(),
                    encrypted_content: item.encrypted_content.clone()?,
                })
            })
            .filter(|item| !item.id.is_empty() && !item.encrypted_content.is_empty())
            .collect(),
    };
    if state.reasoning_items.is_empty() {
        return;
    }
    if let Ok(value) = serde_json::to_string(&state) {
        turn.push(ProviderTurnEvent::ProviderMetadata {
            key: "provider_state".to_string(),
            value,
        });
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
        if !delta.is_empty()
            && let Some(call_id) = &entry.id
        {
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
            if !entry.started
                && let (Some(id), Some(name)) = (&entry.id, &entry.name)
            {
                turn.push(ProviderTurnEvent::ToolCallStarted {
                    call_id: id.clone(),
                    name: original_tool_name(name, name_map),
                });
                entry.started = true;
            }
            if let Some(arguments) = &function.arguments {
                entry.arguments.push_str(arguments);
                if !arguments.is_empty()
                    && let Some(call_id) = &entry.id
                {
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
            format!(
                "failed to decode arguments for tool call {call_id} ({tool_name}): {error}; received {} bytes",
                arguments.len()
            ),
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
) -> Result<serde_json::Value, ProviderError> {
    let previous_response_id = responses_previous_response_id(settings, request);
    let mut projection = responses_projection(
        request,
        responses_instruction_strategy(settings),
        settings.dialect.projects_reused_history() && previous_response_id.is_some(),
        settings.dialect,
    );
    prepend_provider_reasoning_state(&mut projection.input, settings.dialect, request);
    let typed_request = ResponsesRequest {
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
        reasoning: responses_reasoning_options(settings, request),
        include: responses_include(settings.dialect.reasoning_request_shape(), request),
        prompt_cache_key: settings
            .dialect
            .uses_codex_request_shape()
            .then(|| request.session_id.to_string()),
        temperature: request.parameters.temperature,
        max_output_tokens: request.parameters.max_output_tokens,
        top_p: request.parameters.top_p,
    };
    let mut body = serde_json::to_value(typed_request).map_err(|error| {
        provider_error(
            "request_encode_failed",
            ProviderErrorCategory::InvalidRequest,
            error.to_string(),
        )
    })?;
    merge_provider_request_options(&mut body, &request.provider_context.request)?;
    Ok(body)
}

fn responses_reasoning_options(
    settings: &Settings,
    request: &ModelTurnRequest,
) -> Option<ResponsesReasoningOptions> {
    let effort = request
        .parameters
        .reasoning_effort_value
        .clone()
        .or_else(|| {
            request
                .parameters
                .reasoning_effort
                .map(reasoning_effort_name)
        });
    let summary = request.parameters.reasoning_summary.clone();
    let context = settings
        .dialect
        .uses_codex_request_shape()
        .then_some("current_turn");
    (effort.is_some() || summary.is_some() || context.is_some()).then_some(
        ResponsesReasoningOptions {
            effort,
            summary,
            context,
        },
    )
}

fn responses_include(
    shape: ReasoningRequestShape,
    request: &ModelTurnRequest,
) -> Vec<&'static str> {
    let mut include = Vec::new();
    include.extend_from_slice(shape.include_state);
    if request.parameters.reasoning_summary.is_some() {
        include.extend_from_slice(shape.include_summary);
    }
    include
}

fn reasoning_effort_name(effort: bcode_model::ReasoningEffort) -> String {
    match effort {
        bcode_model::ReasoningEffort::Low => "low",
        bcode_model::ReasoningEffort::Medium => "medium",
        bcode_model::ReasoningEffort::High => "high",
    }
    .to_string()
}

fn merge_provider_request_options(
    body: &mut serde_json::Value,
    request_options: &BTreeMap<String, bcode_model::ProviderRequestValue>,
) -> Result<(), ProviderError> {
    let Some(body_object) = body.as_object_mut() else {
        return Err(provider_error(
            "invalid_provider_request",
            ProviderErrorCategory::InvalidRequest,
            "provider request body is not a JSON object",
        ));
    };
    for (key, value) in request_options {
        if is_reserved_responses_request_key(key) {
            return Err(provider_error(
                "reserved_provider_request_option",
                ProviderErrorCategory::InvalidRequest,
                format!("provider request option '{key}' is reserved and cannot be overridden"),
            ));
        }
        body_object.insert(key.clone(), serde_json::Value::from(value.clone()));
    }
    Ok(())
}

fn is_reserved_responses_request_key(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "input"
            | "messages"
            | "stream"
            | "tools"
            | "tool_choice"
            | "instructions"
            | "previous_response_id"
    )
}

const fn responses_instruction_strategy(_settings: &Settings) -> ResponsesInstructionStrategy {
    ResponsesInstructionStrategy::TopLevelInstructions
}

const fn responses_store_enabled(settings: &Settings, request: &ModelTurnRequest) -> bool {
    matches!(settings.dialect, OpenAiCompatibleDialect::ResponsesApi)
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OpenAiProviderState {
    #[serde(default)]
    reasoning_items: Vec<OpenAiReasoningStateItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAiReasoningStateItem {
    id: String,
    #[serde(default)]
    summary: Vec<String>,
    encrypted_content: String,
}

fn has_provider_reasoning_state(
    dialect: OpenAiCompatibleDialect,
    request: &ModelTurnRequest,
) -> bool {
    if !dialect.uses_codex_request_shape() {
        return false;
    }
    request
        .conversation_reuse
        .provider_state
        .as_ref()
        .and_then(|value| serde_json::from_value::<OpenAiProviderState>(value.clone()).ok())
        .is_some_and(|state| {
            state
                .reasoning_items
                .iter()
                .any(|item| !item.id.is_empty() && !item.encrypted_content.is_empty())
        })
}

fn prepend_provider_reasoning_state(
    input: &mut Vec<ResponsesInputItem>,
    dialect: OpenAiCompatibleDialect,
    request: &ModelTurnRequest,
) {
    if !dialect.uses_codex_request_shape() {
        return;
    }
    let Some(provider_state) = request.conversation_reuse.provider_state.as_ref() else {
        return;
    };
    let Ok(state) = serde_json::from_value::<OpenAiProviderState>(provider_state.clone()) else {
        return;
    };
    let reasoning_items = state
        .reasoning_items
        .into_iter()
        .filter(|item| !item.id.is_empty() && !item.encrypted_content.is_empty())
        .map(|item| ResponsesInputItem::Reasoning {
            id: item.id,
            summary: item
                .summary
                .into_iter()
                .filter(|text| !text.is_empty())
                .map(|text| ResponsesReasoningSummary::SummaryText { text })
                .collect(),
            encrypted_content: item.encrypted_content,
        })
        .collect::<Vec<_>>();
    if reasoning_items.is_empty() {
        return;
    }
    let insert_index = provider_reasoning_insert_index(input);
    input.splice(insert_index..insert_index, reasoning_items);
}

fn provider_reasoning_insert_index(input: &[ResponsesInputItem]) -> usize {
    if input.is_empty() {
        return 0;
    }
    let trailing_tool_protocol_start = input
        .iter()
        .rposition(|item| {
            !matches!(
                item,
                ResponsesInputItem::FunctionCall { .. }
                    | ResponsesInputItem::FunctionCallOutput { .. }
            )
        })
        .map_or(0, |index| index.saturating_add(1));
    if trailing_tool_protocol_start < input.len() {
        return trailing_tool_protocol_start;
    }
    input.len().saturating_sub(1)
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
    let mut input = Vec::new();
    let mut seen_tool_call_ids = BTreeSet::new();
    let mut pending_tool_call_ids = BTreeSet::new();
    for message in request
        .messages
        .iter()
        .skip(start.min(request.messages.len()))
    {
        for item in model_message_to_responses_input(message, dialect) {
            push_sanitized_responses_input_item(
                &mut input,
                &mut seen_tool_call_ids,
                &mut pending_tool_call_ids,
                item,
            );
        }
    }
    append_missing_responses_tool_outputs(&mut input, &mut pending_tool_call_ids);
    input
}

fn push_sanitized_responses_input_item(
    input: &mut Vec<ResponsesInputItem>,
    seen_tool_call_ids: &mut BTreeSet<String>,
    pending_tool_call_ids: &mut BTreeSet<String>,
    item: ResponsesInputItem,
) {
    match item {
        ResponsesInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => {
            if !seen_tool_call_ids.insert(call_id.clone()) {
                append_missing_responses_tool_outputs(input, pending_tool_call_ids);
                input.push(ResponsesInputItem::Message {
                    role: "user",
                    content: vec![ResponsesContent::InputText {
                        text: format!(
                            "Historical assistant tool call omitted from structured tool protocol because its call id was duplicated. Call id: {call_id}; tool: {name}; arguments: {arguments}"
                        ),
                    }],
                });
                return;
            }
            pending_tool_call_ids.insert(call_id.clone());
            input.push(ResponsesInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            });
        }
        ResponsesInputItem::FunctionCallOutput { call_id, output } => {
            if pending_tool_call_ids.remove(&call_id) {
                input.push(ResponsesInputItem::FunctionCallOutput { call_id, output });
            } else {
                append_missing_responses_tool_outputs(input, pending_tool_call_ids);
                input.push(ResponsesInputItem::Message {
                    role: "user",
                    content: vec![ResponsesContent::InputText {
                        text: format!(
                            "Historical tool result omitted from structured tool protocol because its matching assistant tool call is unavailable. Call id: {call_id}; result: {output}"
                        ),
                    }],
                });
            }
        }
        ResponsesInputItem::Message { role, content } => {
            append_missing_responses_tool_outputs(input, pending_tool_call_ids);
            input.push(ResponsesInputItem::Message { role, content });
        }
        ResponsesInputItem::Reasoning {
            id,
            summary,
            encrypted_content,
        } => {
            append_missing_responses_tool_outputs(input, pending_tool_call_ids);
            input.push(ResponsesInputItem::Reasoning {
                id,
                summary,
                encrypted_content,
            });
        }
    }
}

fn append_missing_responses_tool_outputs(
    input: &mut Vec<ResponsesInputItem>,
    pending_tool_call_ids: &mut BTreeSet<String>,
) {
    input.extend(
        std::mem::take(pending_tool_call_ids)
            .into_iter()
            .map(|call_id| ResponsesInputItem::FunctionCallOutput {
                call_id,
                output: "tool invocation was interrupted before Bcode could persist a result"
                    .to_string(),
            }),
    );
}

fn model_message_to_responses_input(
    message: &ModelMessage,
    dialect: OpenAiCompatibleDialect,
) -> Vec<ResponsesInputItem> {
    match message.role {
        MessageRole::System => Vec::new(),
        MessageRole::User => responses_message("user", message, true),
        MessageRole::Assistant => responses_assistant_items(message, dialect),
        MessageRole::Tool => responses_tool_items(message),
    }
}

fn responses_message(
    role: &'static str,
    message: &ModelMessage,
    input_text: bool,
) -> Vec<ResponsesInputItem> {
    let mut content = Vec::new();
    let text = joined_text_content(message);
    if !text.is_empty() {
        content.push(if input_text {
            ResponsesContent::InputText { text }
        } else {
            ResponsesContent::OutputText { text }
        });
    }
    for block in &message.content {
        if let ContentBlock::Image { image } = block {
            content.push(ResponsesContent::InputImage {
                image_url: image_data_url(image),
            });
        }
    }
    if content.is_empty() {
        return Vec::new();
    }
    vec![ResponsesInputItem::Message { role, content }]
}

fn responses_assistant_items(
    message: &ModelMessage,
    dialect: OpenAiCompatibleDialect,
) -> Vec<ResponsesInputItem> {
    let mut items = responses_message("assistant", message, false);
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
    let mut items = Vec::new();
    for block in &message.content {
        let ContentBlock::ToolResult { result } = block else {
            continue;
        };
        items.push(ResponsesInputItem::FunctionCallOutput {
            call_id: result.call_id.clone(),
            output: result.output.clone(),
        });
        for content in &result.content {
            match content {
                bcode_model::ToolResultContent::Image { image } => {
                    items.push(ResponsesInputItem::Message {
                        role: "user",
                        content: vec![
                            ResponsesContent::InputText {
                                text: format!(
                                    "Image content returned by tool call {}:",
                                    result.call_id
                                ),
                            },
                            ResponsesContent::InputImage {
                                image_url: image_data_url(image),
                            },
                        ],
                    });
                }
                bcode_model::ToolResultContent::ImageRef { image } => {
                    items.push(ResponsesInputItem::Message {
                        role: "user",
                        content: vec![ResponsesContent::InputText {
                            text: image_ref_text(&result.call_id, image),
                        }],
                    });
                }
                bcode_model::ToolResultContent::Text { text } => {
                    items.push(ResponsesInputItem::Message {
                        role: "user",
                        content: vec![ResponsesContent::InputText { text: text.clone() }],
                    });
                }
            }
        }
    }
    items
}

fn image_ref_text(call_id: &str, image: &bcode_model::ImageRefContent) -> String {
    let dimensions = image
        .metadata
        .width
        .zip(image.metadata.height)
        .map_or_else(String::new, |(width, height)| format!(" {width}x{height}"));
    let byte_len = image
        .metadata
        .byte_len
        .map_or_else(String::new, |byte_len| format!(" {byte_len} bytes"));
    format!(
        "Image reference returned by tool call {call_id}: {} {}{}{}",
        image.path, image.mime_type, dimensions, byte_len
    )
}

fn image_data_url(image: &bcode_model::ImageContent) -> String {
    format!("data:{};base64,{}", image.mime_type, image.data_base64)
}

fn model_messages_to_chat_messages(request: &ModelTurnRequest) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        messages.push(ChatMessage {
            role: "system",
            content: Some(ChatMessageContent::Text(system_prompt.clone())),
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
    sanitize_chat_tool_protocol(messages)
}

fn sanitize_chat_tool_protocol(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut sanitized = Vec::with_capacity(messages.len());
    let mut seen_tool_call_ids = BTreeSet::new();
    let mut pending_tool_call_ids = BTreeSet::new();

    for mut message in messages {
        match message.role {
            "assistant" if !message.tool_calls.is_empty() => {
                let mut valid_tool_calls = Vec::new();
                let mut omitted_context = Vec::new();
                for tool_call in message.tool_calls {
                    if seen_tool_call_ids.insert(tool_call.id.clone()) {
                        pending_tool_call_ids.insert(tool_call.id.clone());
                        valid_tool_calls.push(tool_call);
                    } else {
                        omitted_context.push(format!(
                            "Historical assistant tool call omitted from structured tool protocol because its call id was duplicated. Call id: {}; tool: {}; arguments: {}",
                            tool_call.id, tool_call.function.name, tool_call.function.arguments
                        ));
                    }
                }
                message.tool_calls = valid_tool_calls;
                if message.content.is_some() || !message.tool_calls.is_empty() {
                    sanitized.push(message);
                }
                for context in omitted_context {
                    append_missing_chat_tool_outputs(&mut sanitized, &mut pending_tool_call_ids);
                    sanitized.push(chat_plain_user_message(context));
                }
            }
            "tool" => {
                if let Some(call_id) = message.tool_call_id.as_ref()
                    && pending_tool_call_ids.remove(call_id)
                {
                    sanitized.push(message);
                } else {
                    append_missing_chat_tool_outputs(&mut sanitized, &mut pending_tool_call_ids);
                    sanitized.push(chat_plain_user_message(format!(
                        "Historical tool result omitted from structured tool protocol because its matching assistant tool call is unavailable. Call id: {}; result: {}",
                        message.tool_call_id.as_deref().unwrap_or("<missing>"),
                        chat_message_text_content(&message)
                    )));
                }
            }
            _ => {
                append_missing_chat_tool_outputs(&mut sanitized, &mut pending_tool_call_ids);
                sanitized.push(message);
            }
        }
    }

    append_missing_chat_tool_outputs(&mut sanitized, &mut pending_tool_call_ids);
    sanitized
}

fn append_missing_chat_tool_outputs(
    messages: &mut Vec<ChatMessage>,
    pending_tool_call_ids: &mut BTreeSet<String>,
) {
    messages.extend(
        std::mem::take(pending_tool_call_ids)
            .into_iter()
            .map(|call_id| ChatMessage {
                role: "tool",
                content: Some(ChatMessageContent::Text(
                    "tool invocation was interrupted before Bcode could persist a result"
                        .to_string(),
                )),
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id),
            }),
    );
}

const fn chat_plain_user_message(text: String) -> ChatMessage {
    ChatMessage {
        role: "user",
        content: Some(ChatMessageContent::Text(text)),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

fn chat_message_text_content(message: &ChatMessage) -> String {
    match &message.content {
        Some(ChatMessageContent::Text(text)) => text.clone(),
        Some(ChatMessageContent::Parts(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                ChatMessageContentPart::Text { text } => Some(text.as_str()),
                ChatMessageContentPart::ImageUrl { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
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
    chat_message_with_content(role, message, Vec::new())
}

fn chat_message_with_content(
    role: &'static str,
    message: &ModelMessage,
    tool_calls: Vec<ChatMessageToolCall>,
) -> Option<ChatMessage> {
    let text = joined_text_content(message);
    let images = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { image } => Some(image),
            _ => None,
        })
        .collect::<Vec<_>>();
    if text.is_empty() && images.is_empty() && tool_calls.is_empty() {
        return None;
    }
    let content = if images.is_empty() {
        (!text.is_empty()).then_some(ChatMessageContent::Text(text))
    } else {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(ChatMessageContentPart::Text { text });
        }
        parts.extend(
            images
                .into_iter()
                .map(|image| ChatMessageContentPart::ImageUrl {
                    image_url: ChatImageUrl {
                        url: image_data_url(image),
                    },
                }),
        );
        Some(ChatMessageContent::Parts(parts))
    };
    Some(ChatMessage {
        role,
        content,
        tool_calls,
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
            content: (!content.is_empty()).then_some(ChatMessageContent::Text(content)),
            tool_calls,
            tool_call_id: None,
        })
    }
}

fn tool_chat_message(message: &ModelMessage) -> Option<ChatMessage> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::ToolResult { result } => Some(ChatMessage {
            role: "tool",
            content: Some(ChatMessageContent::Text(result.output.clone())),
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
            ProviderCapability::NativeWebSearch,
        ]
        .into_iter()
        .collect(),
        auth_schemes: ["api_key".to_string(), "chatgpt".to_string()]
            .into_iter()
            .collect(),
        metadata: diagnostics_metadata(&settings, None),
    }
}

impl OpenAiCompatibleProviderPlugin {
    fn models(&self, request: &ModelListRequest) -> ModelList {
        let settings = settings_for_context(&request.provider_context);
        if !settings.model_ids_are_explicit
            && settings.default_model.is_none()
            && let Some(discovered_models) = self.discover_models(&settings)
        {
            return ModelList {
                models: ensure_selected_model_info(
                    discovered_models,
                    request.selected_model_id.as_deref(),
                ),
            };
        }
        ModelList {
            models: ensure_selected_model_info(
                model_infos_from_ids(&settings.model_ids, settings.default_model.as_deref()),
                request.selected_model_id.as_deref(),
            ),
        }
    }
}

fn model_list_request(request: &ServiceRequest) -> ModelListRequest {
    request
        .payload_json::<ModelListRequest>()
        .unwrap_or_default()
}

fn ensure_selected_model_info(
    mut models: Vec<ModelInfo>,
    selected_model_id: Option<&str>,
) -> Vec<ModelInfo> {
    let Some(selected_model_id) = selected_model_id.filter(|model_id| !model_id.trim().is_empty())
    else {
        return models;
    };
    if models
        .iter()
        .any(|model| model.model_id == selected_model_id)
    {
        return models;
    }
    let mut selected =
        model_infos_from_ids(&[selected_model_id.to_string()], Some(selected_model_id));
    models.append(&mut selected);
    models
}

fn model_infos_from_ids(model_ids: &[String], default_model: Option<&str>) -> Vec<ModelInfo> {
    model_infos_from_items(
        model_ids
            .iter()
            .map(|model_id| ModelResponseItem {
                id: model_id.clone(),
                created: None,
                metadata: BTreeMap::new(),
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
                metadata: BTreeMap::new(),
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
        .map(|model| {
            let metadata = model_catalog::resolve(&model.id, &model.metadata);
            ModelInfo {
                is_default: selected_default.as_deref() == Some(model.id.as_str()),
                model_id: model.id.clone(),
                display_name: model.id.clone(),
                context_window: Some(metadata.metadata.context_window),
                max_output_tokens: Some(metadata.metadata.max_output_tokens),
                capabilities: model_capabilities_for(&model),
                reasoning: reasoning_info_for_model(&model, default_reasoning_request_shape()),
                cache: openai_model_cache_info(),
                metadata_source: Some(metadata.source),
            }
        })
        .collect()
}

fn openai_model_cache_info() -> bcode_model::ModelCacheInfo {
    bcode_model::ModelCacheInfo {
        capabilities: BTreeSet::from([
            bcode_model::ModelCacheCapability::PromptCacheKey,
            bcode_model::ModelCacheCapability::AutomaticPrefixCache,
            bcode_model::ModelCacheCapability::CacheUsageReporting,
            bcode_model::ModelCacheCapability::PreviousResponseId,
        ]),
    }
}

fn model_capabilities_for(model: &ModelResponseItem) -> BTreeSet<ModelCapability> {
    let mut capabilities = BTreeSet::from([
        ModelCapability::StreamingText,
        ModelCapability::ToolCalls,
        ModelCapability::PromptCaching,
        ModelCapability::Reasoning,
    ]);
    if model_supports_native_web_search(model) {
        capabilities.insert(ModelCapability::NativeWebSearch);
    }
    capabilities
}

fn model_supports_native_web_search(model: &ModelResponseItem) -> bool {
    let lower = model.id.to_ascii_lowercase();
    lower.contains("gpt-4.1")
        || lower.contains("gpt-5")
        || lower.contains("o3")
        || lower.contains("o4")
        || lower.contains("grok")
}

fn reasoning_info_for_model(
    model: &ModelResponseItem,
    shape: ReasoningRequestShape,
) -> Option<bcode_model::ModelReasoningInfo> {
    let metadata_reasoning = reasoning_info_from_metadata(&model.metadata);
    if metadata_reasoning.is_some() {
        return metadata_reasoning;
    }
    let lower = model.id.to_ascii_lowercase();
    let known = known_reasoning_info(&lower, shape);
    if known.is_some() {
        return known;
    }
    if lower.contains("gpt-5") || lower.contains("o3") || lower.contains("o4") {
        Some(reasoning_info(
            shape.fallback_effort_values,
            default_value(shape.fallback_effort_values, "medium"),
            !shape.include_summary.is_empty(),
            shape.fallback_summary_values,
            default_value(shape.fallback_summary_values, "auto"),
            false,
            shape.source,
        ))
    } else {
        None
    }
}

fn known_reasoning_info(
    model_id: &str,
    shape: ReasoningRequestShape,
) -> Option<bcode_model::ModelReasoningInfo> {
    if model_id.contains("grok-4") {
        return None;
    }
    if model_id.contains("grok") || model_id.contains("x-ai/") || model_id.contains("xai/") {
        return Some(reasoning_info(
            &["low", "high"],
            None,
            false,
            &[],
            None,
            false,
            ModelReasoningCapabilitySource::KnownModelTable,
        ));
    }
    if model_id.contains("qwen3") || model_id.contains("qwen-3") {
        return Some(reasoning_info(
            &["none", "default"],
            Some("default"),
            false,
            &[],
            None,
            true,
            ModelReasoningCapabilitySource::KnownModelTable,
        ));
    }
    if model_id.contains("gpt-oss") {
        return Some(reasoning_info(
            &["low", "medium", "high"],
            Some("medium"),
            false,
            &[],
            None,
            true,
            ModelReasoningCapabilitySource::KnownModelTable,
        ));
    }
    if model_id.contains("gpt-5-pro") || model_id.contains("gpt-5.5-pro") {
        return Some(reasoning_info(
            &["high"],
            Some("high"),
            !shape.include_summary.is_empty(),
            shape.fallback_summary_values,
            default_value(shape.fallback_summary_values, "auto"),
            false,
            ModelReasoningCapabilitySource::KnownModelTable,
        ));
    }
    if model_id.contains("gpt-5.5") {
        return Some(reasoning_info(
            &["none", "minimal", "low", "medium", "high", "xhigh"],
            Some("medium"),
            !shape.include_summary.is_empty(),
            shape.fallback_summary_values,
            default_value(shape.fallback_summary_values, "auto"),
            false,
            ModelReasoningCapabilitySource::KnownModelTable,
        ));
    }
    if model_id.contains("gpt-5.1") {
        return Some(reasoning_info(
            &["none", "low", "medium", "high"],
            Some("none"),
            !shape.include_summary.is_empty(),
            shape.fallback_summary_values,
            default_value(shape.fallback_summary_values, "auto"),
            false,
            ModelReasoningCapabilitySource::KnownModelTable,
        ));
    }
    None
}

fn reasoning_info(
    effort_values: &[&str],
    default_effort: Option<&str>,
    visible_summary_supported: bool,
    summary_values: &[&str],
    default_summary: Option<&str>,
    raw_reasoning_supported: bool,
    source: ModelReasoningCapabilitySource,
) -> bcode_model::ModelReasoningInfo {
    bcode_model::ModelReasoningInfo {
        effort_values: effort_values
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        default_effort: default_effort.map(str::to_string),
        visible_summary_supported,
        summary_values: summary_values
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        default_summary: default_summary.map(str::to_string),
        raw_reasoning_supported,
        source,
    }
}

fn default_value<'a>(values: &'a [&str], value: &'a str) -> Option<&'a str> {
    values.contains(&value).then_some(value)
}

fn reasoning_info_from_metadata(
    metadata: &BTreeMap<String, serde_json::Value>,
) -> Option<bcode_model::ModelReasoningInfo> {
    let reasoning = metadata.get("reasoning")?.as_object()?;
    Some(bcode_model::ModelReasoningInfo {
        effort_values: string_array_field(reasoning, "effort_values"),
        default_effort: string_field(reasoning, "default_effort"),
        visible_summary_supported: bool_field(reasoning, "visible_summary_supported"),
        summary_values: string_array_field(reasoning, "summary_values"),
        default_summary: string_field(reasoning, "default_summary"),
        raw_reasoning_supported: bool_field(reasoning, "raw_reasoning_supported"),
        source: ModelReasoningCapabilitySource::ProviderMetadata,
    })
}

fn string_array_field(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map_or_else(Vec::new, |values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
}

fn string_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn bool_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    object
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_default()
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
    let allow_saved_auth = context.auth_profile.is_none();
    let xai_mode = env_has_xai_keys(context) || (allow_saved_auth && saved_has_xai_keys(&saved));
    let chatgpt_mode = (context_has_chatgpt_auth(context)
        || (allow_saved_auth && saved_openai_auth_is_chatgpt(&saved)))
        && !xai_mode;
    let fallback_model = if xai_mode {
        DEFAULT_XAI_MODEL_ID.to_string()
    } else if chatgpt_mode {
        DEFAULT_CODEX_MODEL_ID.to_string()
    } else {
        DEFAULT_MODEL_ID.to_string()
    };
    let default_model = first_context_env(
        context,
        [
            "BCODE_XAI_MODEL",
            "XAI_MODEL",
            "BCODE_OPENAI_MODEL",
            "OPENAI_MODEL",
        ],
    )
    .or_else(|| chatgpt_mode.then(|| DEFAULT_CODEX_MODEL_ID.to_string()));
    let model_ids_env = first_context_env(
        context,
        [
            "BCODE_XAI_MODELS",
            "XAI_MODELS",
            "BCODE_OPENAI_MODELS",
            "OPENAI_MODELS",
        ],
    );
    let mut model_ids = model_ids_env
        .as_deref()
        .map_or_else(|| default_model_ids(chatgpt_mode), parse_model_list);
    if let Some(default_model) = &default_model
        && !model_ids.contains(default_model)
    {
        model_ids.insert(0, default_model.clone());
    }
    let (auth, auth_diagnostics) = openai_auth_settings(&saved, context);
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
    let request_timeout = optional_duration_from_context_or_env(
        context,
        "request_timeout_secs",
        "openai.request_timeout_secs",
        [
            "BCODE_XAI_REQUEST_TIMEOUT_SECS",
            "XAI_REQUEST_TIMEOUT_SECS",
            "BCODE_OPENAI_REQUEST_TIMEOUT_SECS",
            "OPENAI_REQUEST_TIMEOUT_SECS",
        ],
    );
    Settings {
        auth,
        auth_diagnostics,
        dialect,
        base_url,
        default_model,
        fallback_model,
        model_ids,
        model_ids_are_explicit: model_ids_env.is_some(),
        request_timeout,
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
    let store = sshenv_vault::SshenvStore::new(
        sshenv_vault::SshenvStoreConfig::new(vault.clone()).with_private_key_paths(
            bcode_provider_auth::security::vault_private_key_paths(&vault),
        ),
    );
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

fn env_has_xai_keys(context: &ProviderRequestContext) -> bool {
    context_auth_env_value(context, "BCODE_XAI_API_KEY").is_some()
        || context_auth_env_value(context, "XAI_API_KEY").is_some()
}

fn context_has_chatgpt_auth(context: &ProviderRequestContext) -> bool {
    context_auth_env_value(context, "BCODE_OPENAI_CODEX_ACCESS_TOKEN").is_some()
        || context_auth_env_value(context, "BCODE_OPENAI_AUTH_MODE")
            .is_some_and(|mode| mode == "chatgpt")
}

#[allow(clippy::too_many_lines)]
fn openai_auth_settings(
    saved: &SavedOpenAiAuth,
    context: &ProviderRequestContext,
) -> (AuthSettings, AuthDiagnostics) {
    let allow_saved_auth = context.auth_profile.is_none();
    if let Some(auth) = &context.auth {
        if let Some(api_key) = auth.credentials.get("api_key") {
            return (
                AuthSettings::ApiKey(api_key.value.clone()),
                AuthDiagnostics {
                    source: "runtime_auth".to_string(),
                    mode: auth.scheme.clone().unwrap_or_else(|| "api_key".to_string()),
                    detail: auth.profile.as_ref().map_or_else(
                        || "runtime auth credential 'api_key'".to_string(),
                        |profile| format!("runtime auth profile '{profile}' credential 'api_key'"),
                    ),
                },
            );
        }
        if auth.credentials.contains_key("access_token") {
            return semantic_chatgpt_auth_settings(auth);
        }
    }
    if let Some(api_key_env) = configured_api_key_env(context) {
        if let Some(api_key) = context_auth_env_value(context, &api_key_env) {
            return (
                AuthSettings::ApiKey(api_key),
                AuthDiagnostics {
                    source: "runtime_context".to_string(),
                    mode: "api_key".to_string(),
                    detail: format!("configured API key environment variable {api_key_env}"),
                },
            );
        }
        if allow_saved_auth && let Some(api_key) = saved.values.get(&api_key_env).cloned() {
            return (
                AuthSettings::ApiKey(api_key),
                saved_auth_diagnostics(
                    saved,
                    "api_key",
                    &format!("saved sshenv API key {api_key_env}"),
                ),
            );
        }
    }
    // XAI takes precedence for generic OpenAI-compatible usage (xAI, Grok, etc.)
    if let Some(api_key) = context_auth_env_value(context, "BCODE_XAI_API_KEY") {
        return (
            AuthSettings::ApiKey(api_key),
            AuthDiagnostics {
                source: "environment".to_string(),
                mode: "api_key (xai)".to_string(),
                detail: "environment variable BCODE_XAI_API_KEY".to_string(),
            },
        );
    }
    if let Some(api_key) = context_auth_env_value(context, "XAI_API_KEY") {
        return (
            AuthSettings::ApiKey(api_key),
            AuthDiagnostics {
                source: "environment".to_string(),
                mode: "api_key (xai)".to_string(),
                detail: "environment variable XAI_API_KEY".to_string(),
            },
        );
    }
    if allow_saved_auth && let Some(api_key) = saved.values.get("BCODE_XAI_API_KEY").cloned() {
        return (
            AuthSettings::ApiKey(api_key),
            saved_auth_diagnostics(
                saved,
                "api_key (xai)",
                "saved sshenv API key BCODE_XAI_API_KEY",
            ),
        );
    }
    if allow_saved_auth && let Some(api_key) = saved.values.get("XAI_API_KEY").cloned() {
        return (
            AuthSettings::ApiKey(api_key),
            saved_auth_diagnostics(saved, "api_key (xai)", "saved sshenv API key XAI_API_KEY"),
        );
    }
    if let Some(api_key) = context_auth_env_value(context, "BCODE_OPENAI_API_KEY") {
        return (
            AuthSettings::ApiKey(api_key),
            AuthDiagnostics {
                source: "environment".to_string(),
                mode: "api_key".to_string(),
                detail: "environment variable BCODE_OPENAI_API_KEY".to_string(),
            },
        );
    }
    if let Some(api_key) = context_auth_env_value(context, "OPENAI_API_KEY") {
        return (
            AuthSettings::ApiKey(api_key),
            AuthDiagnostics {
                source: "environment".to_string(),
                mode: "api_key".to_string(),
                detail: "environment variable OPENAI_API_KEY".to_string(),
            },
        );
    }
    if allow_saved_auth && let Some(api_key) = saved.values.get("BCODE_OPENAI_API_KEY").cloned() {
        return (
            AuthSettings::ApiKey(api_key),
            saved_auth_diagnostics(
                saved,
                "api_key",
                "saved sshenv API key BCODE_OPENAI_API_KEY",
            ),
        );
    }
    if allow_saved_auth && let Some(api_key) = saved.values.get("OPENAI_API_KEY").cloned() {
        return (
            AuthSettings::ApiKey(api_key),
            saved_auth_diagnostics(saved, "api_key", "saved sshenv API key OPENAI_API_KEY"),
        );
    }
    if context_has_chatgpt_auth(context) {
        return context_chatgpt_auth_settings(context);
    }
    let saved_mode = saved
        .values
        .get("BCODE_OPENAI_AUTH_MODE")
        .map(String::as_str);
    if allow_saved_auth && (saved_openai_auth_is_chatgpt(saved) || saved_mode == Some("chatgpt")) {
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

fn configured_api_key_env(context: &ProviderRequestContext) -> Option<String> {
    context
        .settings
        .get("openai.api_key_env")
        .or_else(|| context.settings.get("api_key_env"))
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

fn semantic_chatgpt_auth_settings(
    auth: &bcode_model::ProviderAuthContext,
) -> (AuthSettings, AuthDiagnostics) {
    let profile = auth.profile.clone();
    let vault = auth
        .storage
        .values()
        .find_map(|storage| storage.vault.as_ref())
        .map(std::path::PathBuf::from);
    let Some(access_token) = auth
        .credentials
        .get("access_token")
        .map(|credential| credential.value.clone())
    else {
        return (
            AuthSettings::Missing,
            AuthDiagnostics {
                source: "runtime_auth".to_string(),
                mode: "chatgpt".to_string(),
                detail: profile.as_ref().map_or_else(
                    || "runtime auth did not contain access_token".to_string(),
                    |profile| {
                        format!("runtime auth profile '{profile}' did not contain access_token")
                    },
                ),
            },
        );
    };
    let id_token = auth
        .credentials
        .get("id_token")
        .map(|credential| credential.value.as_str());
    let account_id = auth
        .credentials
        .get("account_id")
        .map(|credential| credential.value.clone())
        .or_else(|| id_token.and_then(chatgpt_account_id_from_access_token))
        .or_else(|| chatgpt_account_id_from_access_token(&access_token));
    (
        AuthSettings::ChatGpt {
            access_token,
            refresh_token: auth
                .credentials
                .get("refresh_token")
                .map(|credential| credential.value.clone()),
            expires_at: auth
                .credentials
                .get("expires_at")
                .and_then(|credential| credential.value.parse().ok()),
            account_id,
            profile: profile.clone(),
            vault: vault.clone(),
            storage: auth.storage.clone(),
        },
        AuthDiagnostics {
            source: "runtime_auth".to_string(),
            mode: "chatgpt".to_string(),
            detail: match (&profile, &vault) {
                (Some(profile), Some(vault)) => format!(
                    "runtime semantic ChatGPT/Codex auth from profile '{profile}' in vault {}",
                    vault.display()
                ),
                (Some(profile), None) => {
                    format!("runtime semantic ChatGPT/Codex auth from profile '{profile}'")
                }
                (None, Some(vault)) => format!(
                    "runtime semantic ChatGPT/Codex auth from vault {}",
                    vault.display()
                ),
                (None, None) => "runtime semantic ChatGPT/Codex auth".to_string(),
            },
        },
    )
}

fn context_chatgpt_auth_settings(
    context: &ProviderRequestContext,
) -> (AuthSettings, AuthDiagnostics) {
    let profile = context_auth_env_value(context, "BCODE_OPENAI_AUTH_PROFILE")
        .or_else(|| context.auth_profile.clone());
    let vault =
        context_auth_env_value(context, "BCODE_OPENAI_AUTH_VAULT").map(std::path::PathBuf::from);
    let Some(access_token) = context_auth_env_value(context, "BCODE_OPENAI_CODEX_ACCESS_TOKEN")
    else {
        return (
            AuthSettings::Missing,
            AuthDiagnostics {
                source: "runtime_context".to_string(),
                mode: "chatgpt".to_string(),
                detail: profile.as_ref().map_or_else(
                    || "runtime ChatGPT auth did not contain an access token".to_string(),
                    |profile| {
                        format!(
                            "runtime auth profile '{profile}' did not contain BCODE_OPENAI_CODEX_ACCESS_TOKEN"
                        )
                    },
                ),
            },
        );
    };
    let account_id = context_auth_env_value(context, "BCODE_OPENAI_CODEX_ACCOUNT_ID")
        .or_else(|| {
            context_auth_env_value(context, "BCODE_OPENAI_CODEX_ID_TOKEN")
                .and_then(|token| chatgpt_account_id_from_access_token(&token))
        })
        .or_else(|| chatgpt_account_id_from_access_token(&access_token));
    (
        AuthSettings::ChatGpt {
            access_token,
            refresh_token: context_auth_env_value(context, "BCODE_OPENAI_CODEX_REFRESH_TOKEN"),
            expires_at: context_auth_env_value(context, "BCODE_OPENAI_CODEX_EXPIRES_AT")
                .and_then(|value| value.parse().ok()),
            account_id,
            profile: profile.clone(),
            vault: vault.clone(),
            storage: BTreeMap::new(),
        },
        AuthDiagnostics {
            source: "runtime_context".to_string(),
            mode: "chatgpt".to_string(),
            detail: match (&profile, &vault) {
                (Some(profile), Some(vault)) => format!(
                    "runtime sshenv ChatGPT/Codex auth from profile '{profile}' in vault {}",
                    vault.display()
                ),
                (Some(profile), None) => {
                    format!("runtime sshenv ChatGPT/Codex auth from profile '{profile}'")
                }
                (None, Some(vault)) => format!(
                    "runtime sshenv ChatGPT/Codex auth from vault {}",
                    vault.display()
                ),
                (None, None) => "runtime ChatGPT/Codex auth".to_string(),
            },
        },
    )
}

fn context_auth_env_value(context: &ProviderRequestContext, name: &str) -> Option<String> {
    context
        .env
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| {
            context
                .auth_profile
                .is_none()
                .then(|| env_value(name))
                .flatten()
        })
}

fn context_env_value(context: &ProviderRequestContext, name: &str) -> Option<String> {
    context
        .env
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| env_value(name))
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
            storage: BTreeMap::new(),
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
        storage,
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
            storage,
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
        storage: storage.clone(),
    };
    Ok(())
}

fn store_refreshed_chatgpt_auth(
    profile: &str,
    vault: &std::path::Path,
    storage: &BTreeMap<String, bcode_model::ProviderAuthStorageRef>,
    refreshed: &OpenAiOauthTokenResponse,
    next_refresh_token: &str,
    next_expires_at: u64,
    account_id: Option<&str>,
) -> Result<(), ProviderError> {
    let store = sshenv_vault::SshenvStore::new(
        sshenv_vault::SshenvStoreConfig::new(vault).with_private_key_paths(
            bcode_provider_auth::security::vault_private_key_paths(vault),
        ),
    );
    set_codex_secret(
        &store,
        profile,
        chatgpt_storage_key(storage, "access_token", "BCODE_OPENAI_CODEX_ACCESS_TOKEN"),
        refreshed.access_token.clone(),
    )?;
    if let Some(id_token) = &refreshed.id_token {
        set_codex_secret(
            &store,
            profile,
            chatgpt_storage_key(storage, "id_token", "BCODE_OPENAI_CODEX_ID_TOKEN"),
            id_token.clone(),
        )?;
    }
    set_codex_secret(
        &store,
        profile,
        chatgpt_storage_key(storage, "refresh_token", "BCODE_OPENAI_CODEX_REFRESH_TOKEN"),
        next_refresh_token.to_string(),
    )?;
    set_codex_secret(
        &store,
        profile,
        chatgpt_storage_key(storage, "expires_at", "BCODE_OPENAI_CODEX_EXPIRES_AT"),
        next_expires_at.to_string(),
    )?;
    if let Some(account_id) = account_id {
        set_codex_secret(
            &store,
            profile,
            chatgpt_storage_key(storage, "account_id", "BCODE_OPENAI_CODEX_ACCOUNT_ID"),
            account_id.to_string(),
        )?;
    }
    Ok(())
}

fn chatgpt_storage_key<'a>(
    storage: &'a BTreeMap<String, bcode_model::ProviderAuthStorageRef>,
    credential: &str,
    fallback: &'a str,
) -> &'a str {
    storage
        .get(credential)
        .map_or(fallback, |storage| storage.key.as_str())
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

fn first_context_env<const N: usize>(
    context: &ProviderRequestContext,
    env_names: [&str; N],
) -> Option<String> {
    env_names
        .into_iter()
        .find_map(|name| context_env_value(context, name))
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
        .or_else(|| first_context_env(context, env_names))
}

fn optional_duration_from_context_or_env<const N: usize>(
    context: &ProviderRequestContext,
    key: &str,
    namespaced_key: &str,
    env_names: [&str; N],
) -> Option<Duration> {
    first_context_or_env(context, key, namespaced_key, env_names)
        .as_deref()
        .and_then(parse_positive_duration_secs)
}

fn parse_positive_duration_secs(value: &str) -> Option<Duration> {
    let secs = value.trim().parse::<u64>().ok()?;
    (secs > 0).then(|| Duration::from_secs(secs))
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

fn stream_read_error(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        provider_error(
            "stream_read_timeout",
            ProviderErrorCategory::Timeout,
            format!(
                "provider stream timed out while reading response body. This usually means an explicit OpenAI-compatible request timeout was configured; by default Bcode does not set a total timeout for model streams. underlying error: {error}"
            ),
        )
    } else {
        provider_error(
            "stream_read_failed",
            ProviderErrorCategory::Network,
            format!("provider stream failed while reading response body: {error}"),
        )
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

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        OpenAiCompatibleProviderPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_item(id: &str, created: i64) -> ModelResponseItem {
        ModelResponseItem {
            id: id.to_string(),
            created: Some(created),
            metadata: BTreeMap::new(),
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
            request_timeout: None,
        }
    }

    #[test]
    fn subscription_quota_detection_matches_quota_rate_limit_only() {
        let quota = provider_error(
            "rate_limit_exceeded",
            ProviderErrorCategory::RateLimit,
            "usage limit reached for this subscription",
        );
        assert!(is_subscription_quota_error(&quota));

        let context = provider_error(
            "context_length_exceeded",
            ProviderErrorCategory::ContextLength,
            "maximum context length exceeded",
        );
        assert!(!is_subscription_quota_error(&context));
    }

    #[test]
    fn model_stream_client_defaults_to_no_total_timeout() {
        assert!(model_stream_client(None).is_ok());
    }

    #[test]
    fn request_timeout_setting_is_disabled_by_default() {
        let context = ProviderRequestContext::default();
        let settings = settings_for_context(&context);

        assert_eq!(settings.request_timeout, None);
    }

    #[test]
    fn request_timeout_setting_can_be_configured() {
        let mut context = ProviderRequestContext::default();
        context
            .settings
            .insert("openai.request_timeout_secs".to_string(), "17".to_string());
        let settings = settings_for_context(&context);

        assert_eq!(settings.request_timeout, Some(Duration::from_secs(17)));
    }

    fn test_chatgpt_auth() -> AuthSettings {
        AuthSettings::ChatGpt {
            access_token: "token".to_string(),
            refresh_token: None,
            expires_at: None,
            account_id: None,
            profile: None,
            vault: None,
            storage: BTreeMap::new(),
        }
    }

    fn test_api_key_auth() -> AuthSettings {
        AuthSettings::ApiKey("token".to_string())
    }

    #[test]
    fn runtime_context_chatgpt_auth_uses_context_env() {
        let context = ProviderRequestContext {
            auth_profile: Some("openai".to_string()),
            env: BTreeMap::from([
                (
                    "BCODE_OPENAI_CODEX_ACCESS_TOKEN".to_string(),
                    "access-token".to_string(),
                ),
                (
                    "BCODE_OPENAI_CODEX_REFRESH_TOKEN".to_string(),
                    "refresh-token".to_string(),
                ),
                (
                    "BCODE_OPENAI_CODEX_EXPIRES_AT".to_string(),
                    "12345".to_string(),
                ),
                (
                    "BCODE_OPENAI_AUTH_PROFILE".to_string(),
                    "openai".to_string(),
                ),
                (
                    "BCODE_OPENAI_AUTH_VAULT".to_string(),
                    "/tmp/bcode-auth-vault".to_string(),
                ),
            ]),
            ..ProviderRequestContext::default()
        };

        let (auth, diagnostics) = openai_auth_settings(&SavedOpenAiAuth::default(), &context);

        match auth {
            AuthSettings::ChatGpt {
                access_token,
                refresh_token,
                expires_at,
                profile,
                vault,
                ..
            } => {
                assert_eq!(access_token, "access-token");
                assert_eq!(refresh_token.as_deref(), Some("refresh-token"));
                assert_eq!(expires_at, Some(12_345));
                assert_eq!(profile.as_deref(), Some("openai"));
                assert_eq!(
                    vault.as_deref(),
                    Some(std::path::Path::new("/tmp/bcode-auth-vault"))
                );
            }
            AuthSettings::Missing | AuthSettings::ApiKey(_) => panic!("expected ChatGPT auth"),
        }
        assert_eq!(diagnostics.source, "runtime_context");
        assert_eq!(diagnostics.mode, "chatgpt");
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

    #[test]
    fn responses_request_merges_generic_provider_options() {
        let mut request = test_request(vec![text_message(MessageRole::User, "hello")]);
        request.provider_context.request = BTreeMap::from([
            (
                "service_tier".to_string(),
                bcode_model::ProviderRequestValue::from(serde_json::json!("priority")),
            ),
            (
                "custom_boolean".to_string(),
                bcode_model::ProviderRequestValue::from(serde_json::json!(true)),
            ),
        ]);
        let settings = test_settings(test_chatgpt_auth(), OpenAiCompatibleDialect::ChatGptCodex);

        let body =
            build_responses_request(&settings, &request, "gpt-5.5").expect("request should build");

        assert_eq!(
            body.get("model").and_then(serde_json::Value::as_str),
            Some("gpt-5.5")
        );
        assert_eq!(
            body.get("service_tier").and_then(serde_json::Value::as_str),
            Some("priority")
        );
        assert_eq!(
            body.get("custom_boolean")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn responses_request_rejects_reserved_provider_options() {
        let mut request = test_request(vec![text_message(MessageRole::User, "hello")]);
        request.provider_context.request = BTreeMap::from([(
            "model".to_string(),
            bcode_model::ProviderRequestValue::from(serde_json::json!("other-model")),
        )]);
        let settings = test_settings(test_chatgpt_auth(), OpenAiCompatibleDialect::ChatGptCodex);

        let error = build_responses_request(&settings, &request, "gpt-5.5")
            .expect_err("reserved field should be rejected");

        assert_eq!(error.code, "reserved_provider_request_option");
        assert_eq!(error.category, ProviderErrorCategory::InvalidRequest);
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
    fn model_infos_include_catalog_context_windows() {
        let model_infos = model_infos_from_ids(&["gpt-4.1-mini".to_string()], None);

        assert_eq!(model_infos[0].context_window, Some(1_047_576));
        assert_eq!(model_infos[0].max_output_tokens, Some(32_768));
    }

    #[test]
    fn unknown_model_infos_include_provider_defaults() {
        let model_infos = model_infos_from_ids(&["custom-proxy-model".to_string()], None);

        assert_eq!(model_infos[0].context_window, Some(128_000));
        assert_eq!(model_infos[0].max_output_tokens, Some(16_384));
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
                provider_state: None,
            },
            metadata: BTreeMap::new(),
        };
        let settings = test_settings(test_chatgpt_auth(), OpenAiCompatibleDialect::ChatGptCodex);

        let encoded =
            build_responses_request(&settings, &request, "model").expect("request should build");
        let encoded_text = encoded.to_string();

        assert!(
            encoded
                .get("instructions")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.contains("top-level") && text.contains("dynamic system"))
        );
        assert!(!encoded_text.contains(r#""role":"system""#));
        assert!(encoded.get("instructions").is_some());
        assert_eq!(
            encoded.get("store").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(encoded.get("previous_response_id").is_none());
        assert!(encoded.get("prompt_cache_retention").is_none());
        assert_eq!(
            encoded
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("context"))
                .and_then(serde_json::Value::as_str),
            Some("current_turn")
        );
    }

    #[test]
    fn chatgpt_codex_replays_encrypted_reasoning_provider_state() {
        let mut request = test_request(vec![text_message(MessageRole::User, "new")]);
        request.conversation_reuse.provider_state = Some(serde_json::json!({
            "reasoning_items": [{
                "id": "rs_1",
                "summary": [],
                "encrypted_content": "encrypted"
            }]
        }));
        let settings = test_settings(test_chatgpt_auth(), OpenAiCompatibleDialect::ChatGptCodex);

        let encoded =
            build_responses_request(&settings, &request, "model").expect("request should build");
        let input = encoded
            .get("input")
            .and_then(serde_json::Value::as_array)
            .expect("input should be an array");

        assert_eq!(
            input
                .first()
                .and_then(|item| item.get("type"))
                .and_then(serde_json::Value::as_str),
            Some("reasoning")
        );
        assert_eq!(
            input
                .first()
                .and_then(|item| item.get("id"))
                .and_then(serde_json::Value::as_str),
            Some("rs_1")
        );
        assert_eq!(
            input
                .first()
                .and_then(|item| item.get("summary"))
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(
            input
                .first()
                .and_then(|item| item.get("encrypted_content"))
                .and_then(serde_json::Value::as_str),
            Some("encrypted")
        );
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
                provider_state: None,
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
    fn responses_input_converts_orphan_tool_result_to_plain_message() {
        let mut request = test_request(vec![ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: "call-1".to_string(),
                    output: "orphaned output".to_string(),
                    is_error: false,
                    content: Vec::new(),
                },
            }],
        }]);
        request.system_prompt = Some("system".to_string());

        let items = model_messages_to_responses_input(
            &request,
            false,
            OpenAiCompatibleDialect::ChatGptCodex,
        );

        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            ResponsesInputItem::Message { role: "user", content }
                if matches!(
                    &content[0],
                    ResponsesContent::InputText { text }
                        if text.contains("matching assistant tool call is unavailable")
                            && text.contains("orphaned output")
                )
        ));
    }

    #[test]
    fn responses_input_synthesizes_missing_tool_output_before_user_message() {
        let request = test_request(vec![
            ModelMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    call: ToolCall {
                        id: "call-1".to_string(),
                        name: "filesystem.read".to_string(),
                        arguments: serde_json::json!({ "path": "Cargo.toml" }),
                    },
                }],
            },
            text_message(MessageRole::User, "continue"),
        ]);

        let items = model_messages_to_responses_input(
            &request,
            false,
            OpenAiCompatibleDialect::ChatGptCodex,
        );

        assert_eq!(items.len(), 3);
        assert!(matches!(
            &items[0],
            ResponsesInputItem::FunctionCall { call_id, .. } if call_id == "call-1"
        ));
        assert!(matches!(
            &items[1],
            ResponsesInputItem::FunctionCallOutput { call_id, output }
                if call_id == "call-1" && output.contains("interrupted")
        ));
        assert!(matches!(
            &items[2],
            ResponsesInputItem::Message { role: "user", .. }
        ));
    }

    #[test]
    fn chatgpt_codex_request_does_not_send_previous_response_id() {
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
            provider_state: None,
        };
        let settings = test_settings(test_chatgpt_auth(), OpenAiCompatibleDialect::ChatGptCodex);

        let encoded =
            build_responses_request(&settings, &request, "model").expect("request should build");

        assert_eq!(
            encoded
                .get("input")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(3)
        );
        assert_eq!(
            encoded.get("store").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(encoded.get("previous_response_id").is_none());
        assert!(encoded.get("prompt_cache_retention").is_none());
        assert_eq!(
            encoded
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("context"))
                .and_then(serde_json::Value::as_str),
            Some("current_turn")
        );
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
            provider_state: None,
        };
        let settings = test_settings(test_api_key_auth(), OpenAiCompatibleDialect::ResponsesApi);

        let body =
            build_responses_request(&settings, &request, "model").expect("request should build");

        assert_eq!(
            body.get("previous_response_id")
                .and_then(serde_json::Value::as_str),
            Some("resp_1")
        );
        assert_eq!(
            body.get("store").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            body.get("input")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn chat_completion_tool_argument_delta_emits_progress_event() {
        let turn = TurnState::default();
        let mut tool_calls = BTreeMap::new();
        let name_map = BTreeMap::new();

        let outcome = process_stream_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"filesystem_write","arguments":"{\"path\""}}]},"finish_reason":null}]}"#,
            &turn,
            &mut tool_calls,
            &name_map,
        )
        .expect("stream event should process");

        assert!(matches!(outcome, StreamOutcome::Cancelled));
        assert!(turn.drain().iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ToolCallDelta { call_id, delta }
                if call_id == "call_1" && delta == "{\"path\""
        )));
    }

    #[test]
    fn responses_tool_argument_delta_emits_progress_event() {
        let turn = TurnState::default();
        let mut tool_calls = BTreeMap::new();
        let mut reasoning_items = BTreeMap::new();
        let mut saw_tool_call = false;
        let name_map = BTreeMap::new();

        process_responses_stream_line(
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"filesystem_write"}}"#,
            &turn,
            OpenAiCompatibleDialect::ResponsesApi,
            &mut tool_calls,
            &mut reasoning_items,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("tool event should process");
        let outcome = process_responses_stream_line(
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\""}"#,
            &turn,
            OpenAiCompatibleDialect::ResponsesApi,
            &mut tool_calls,
            &mut reasoning_items,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("argument delta should process");

        assert!(matches!(outcome, StreamOutcome::Cancelled));
        assert!(turn.drain().iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ToolCallDelta { call_id, delta }
                if call_id == "call_1" && delta == "{\"path\""
        )));
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
        let mut reasoning_items = BTreeMap::new();
        let mut saw_tool_call = false;

        let added = process_responses_stream_line(
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"shell_run","arguments":"{\"cmd\":\"ls\",\"workdir\":\"/tmp\",\"timeout\":2}"}}"#,
            &turn,
            OpenAiCompatibleDialect::ChatGptCodex,
            &mut tool_calls,
            &mut reasoning_items,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("tool event should process");
        let completed = process_responses_stream_line(
            r#"data: {"type":"response.completed","response":{"id":"resp_123"}}"#,
            &turn,
            OpenAiCompatibleDialect::ChatGptCodex,
            &mut tool_calls,
            &mut reasoning_items,
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
        let mut reasoning_items = BTreeMap::new();
        let mut saw_tool_call = false;
        let name_map = BTreeMap::new();

        let outcome = process_responses_stream_line(
            r#"data: {"type":"response.completed","response":{"id":"resp_123"}}"#,
            &turn,
            OpenAiCompatibleDialect::ResponsesApi,
            &mut tool_calls,
            &mut reasoning_items,
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
    fn chatgpt_codex_completed_does_not_emit_provider_response_id_metadata() {
        let turn = TurnState::default();
        let mut tool_calls = BTreeMap::new();
        let mut reasoning_items = BTreeMap::new();
        let mut saw_tool_call = false;
        let name_map = BTreeMap::new();

        let outcome = process_responses_stream_line(
            r#"data: {"type":"response.completed","response":{"id":"resp_123"}}"#,
            &turn,
            OpenAiCompatibleDialect::ChatGptCodex,
            &mut tool_calls,
            &mut reasoning_items,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("stream event should process");

        assert!(matches!(outcome, StreamOutcome::Finished));
        assert!(!turn.drain().iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ProviderMetadata { key, .. }
                if key == "provider_response_id"
        )));
    }

    #[test]
    fn responses_completed_emits_encrypted_reasoning_provider_state() {
        let turn = TurnState::default();
        let mut tool_calls = BTreeMap::new();
        let mut reasoning_items = BTreeMap::new();
        let mut saw_tool_call = false;
        let name_map = BTreeMap::new();

        process_responses_stream_line(
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"encrypted","summary":[{"type":"summary_text","text":"kept summary"}]}}"#,
            &turn,
            OpenAiCompatibleDialect::ChatGptCodex,
            &mut tool_calls,
            &mut reasoning_items,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("reasoning item should process");
        let outcome = process_responses_stream_line(
            r#"data: {"type":"response.completed","response":{"id":"resp_123"}}"#,
            &turn,
            OpenAiCompatibleDialect::ChatGptCodex,
            &mut tool_calls,
            &mut reasoning_items,
            &mut saw_tool_call,
            &name_map,
        )
        .expect("completed event should process");

        let events = turn.drain();
        assert!(matches!(outcome, StreamOutcome::Finished));
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ProviderMetadata { key, value }
                if key == "provider_state"
                    && value.contains("rs_1")
                    && value.contains("encrypted")
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
        let mut reasoning_items = BTreeMap::new();
        let mut saw_tool_call = false;
        let name_map = BTreeMap::new();

        let error = process_responses_stream_line(
            r#"data: {"type":"response.failed","error":{"code":"context_length_exceeded","message":"input is too long for the model context window"}}"#,
            &turn,
            OpenAiCompatibleDialect::ResponsesApi,
            &mut tool_calls,
            &mut reasoning_items,
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

bcode_plugin_sdk::export_concurrent_plugin!(
    OpenAiCompatibleProviderPlugin,
    include_str!("../bcode-plugin.toml")
);
