#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Amazon Bedrock Runtime and Mantle model provider plugin for Bcode.

#[cfg(feature = "static-bundled")]
mod cli;

use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_bedrock as bedrock;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::operation::RequestId;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError;
use aws_sdk_bedrockruntime::operation::invoke_model_with_response_stream::InvokeModelWithResponseStreamError;
use aws_sdk_bedrockruntime::types::{
    AnyToolChoice, AutoToolChoice, CachePointBlock, CachePointType,
    ContentBlock as BedrockContentBlock, ContentBlockDelta, ContentBlockStart, ConversationRole,
    ConverseStreamOutput, ImageBlock, ImageFormat, ImageSource, InferenceConfiguration,
    Message as BedrockMessage, ReasoningContentBlockDelta, ResponseStream, SpecificToolChoice,
    StopReason as BedrockStopReason, SystemContentBlock, Tool, ToolChoice as BedrockToolChoice,
    ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolResultStatus,
    ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::Blob;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use aws_smithy_types::{Document, Number};
use base64::Engine as _;
use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    MODEL_PROVIDER_INTERFACE_ID_V2, MessageRole, ModelCapability, ModelCatalogHints, ModelInfo,
    ModelList, ModelListRequest, ModelMessage, ModelTurnRequest, OP_CANCEL_TURN, OP_CAPABILITIES,
    OP_FINISH_TURN, OP_MODELS, OP_POLL_TURN_EVENTS, OP_START_TURN, OP_VALIDATE_CONFIG,
    PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities, ProviderCapability,
    ProviderError, ProviderErrorCategory, ProviderErrorSource, ProviderRequestContext,
    ProviderRequestProjection, ProviderTurnEvent, StartTurnResponse, StopReason, TokenUsage,
    ToolCall, ToolChoice, ToolDefinition, ValidateConfigResponse,
};
use bcode_model_provider_runtime::{
    ProviderRuntime, StreamOutcome, TurnState, TurnStore, provider_error, retry_hint_from_body,
    sanitize_provider_diagnostic,
};
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROVIDER_ID: &str = "bcode.bedrock";
const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_MANTLE_BASE_URL_PREFIX: &str = "https://bedrock-mantle.";
const MODEL_DISCOVERY_TTL: Duration = Duration::from_mins(10);
const COMPATIBILITY_CACHE_VERSION: u8 = 1;
const COMPATIBILITY_CACHE_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
const STREAMING_TOOL_UNSUPPORTED_REASON: &str = "streaming_tool_use_unsupported";
const PROMPT_CACHE_UNSUPPORTED_REASON: &str = "prompt_cache_unsupported";

/// Amazon Bedrock model provider plugin.
pub struct BedrockProviderPlugin {
    turns: Mutex<TurnStore>,
    discovery: Arc<Mutex<DiscoveryCache>>,
    runtime: Result<ProviderRuntime, String>,
    turn_executor: Arc<dyn BedrockTurnExecutor>,
}

impl Default for BedrockProviderPlugin {
    fn default() -> Self {
        Self {
            turns: Mutex::default(),
            discovery: Arc::default(),
            runtime: ProviderRuntime::new().map_err(|error| error.to_string()),
            turn_executor: Arc::new(AwsBedrockTurnExecutor),
        }
    }
}

#[derive(Debug)]
struct AwsBedrockTurnExecutor;

trait BedrockTurnExecutor: Send + Sync {
    fn start(
        &self,
        runtime: &ProviderRuntime,
        request: ModelTurnRequest,
        turn: TurnState,
        discovery: Arc<Mutex<DiscoveryCache>>,
    );
}

impl BedrockTurnExecutor for AwsBedrockTurnExecutor {
    fn start(
        &self,
        runtime: &ProviderRuntime,
        request: ModelTurnRequest,
        turn: TurnState,
        discovery: Arc<Mutex<DiscoveryCache>>,
    ) {
        runtime.spawn(async move {
            stream_bedrock_turn(&request, &turn, discovery).await;
        });
    }
}

impl ConcurrentRustPlugin for BedrockProviderPlugin {
    fn activate_concurrent(&self) -> Result<(), PluginError> {
        self.activate_provider();
        Ok(())
    }

    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_provider_service(&context)
    }
}

impl RustPlugin for BedrockProviderPlugin {
    fn activate(&mut self) -> Result<(), PluginError> {
        self.activate_provider();
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_provider_service(&context)
    }
}

impl BedrockProviderPlugin {
    fn activate_provider(&self) {
        match load_compatibility_cache() {
            Ok(compatibility) => {
                if let Ok(mut discovery) = self.discovery.lock() {
                    discovery.compatibility = compatibility;
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "bcode_bedrock::compatibility",
                    error = %error.message,
                    "failed to load Bedrock compatibility cache"
                );
            }
        }
        if let Ok(runtime) = &self.runtime {
            warm_discovery_cache(runtime, self.discovery.clone(), Settings::resolve(None));
        }
    }

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
            OP_MODELS => self.models_response(&context.request),
            OP_VALIDATE_CONFIG => self.validate_config_response(&context.request),
            OP_START_TURN => self.start_turn(
                &context.request,
                context.request.interface_id == MODEL_PROVIDER_INTERFACE_ID_V2,
            ),
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

    fn start_turn(&self, request: &ServiceRequest, positioned_output: bool) -> ServiceResponse {
        let request = match request.payload_json::<ModelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let (provider_turn_id, turn) = self
            .turns
            .lock()
            .expect("bedrock turn store lock should not be poisoned")
            .insert_started("bedrock-turn");
        if positioned_output {
            turn.enable_positioned_output();
        }
        turn.push(ProviderTurnEvent::RequestProjection {
            projection: bedrock_request_projection(&request),
        });
        match &self.runtime {
            Ok(runtime) => {
                self.turn_executor
                    .start(runtime, request, turn, Arc::clone(&self.discovery));
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
            .lock()
            .expect("bedrock turn store lock should not be poisoned")
            .drain(&request.provider_turn_id);
        json_response(&PollTurnEventsResponse { events })
    }

    fn cancel_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<CancelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        self.turns
            .lock()
            .expect("bedrock turn store lock should not be poisoned")
            .cancel(&request.provider_turn_id);
        json_response(&AckResponse::default())
    }

    fn finish_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<FinishTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        self.turns
            .lock()
            .expect("bedrock turn store lock should not be poisoned")
            .finish(&request.provider_turn_id);
        json_response(&AckResponse::default())
    }
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

async fn stream_bedrock_turn(
    request: &ModelTurnRequest,
    turn: &TurnState,
    discovery: Arc<Mutex<DiscoveryCache>>,
) {
    match stream_bedrock_turn_inner(request, turn, discovery).await {
        Ok(StreamOutcome::Finished) => turn.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        }),
        Ok(StreamOutcome::ToolCall) => turn.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::ToolCall,
        }),
        Ok(StreamOutcome::MaxTokens) => turn.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::MaxTokens,
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

fn validate_bedrock_request(request: &ModelTurnRequest) -> Result<(), ProviderError> {
    if request.structured_output.is_some() {
        return Err(provider_error(
            "bedrock_structured_output_unsupported",
            ProviderErrorCategory::UnsupportedFeature,
            "Bedrock Converse does not provide provider-native JSON Schema enforcement",
        ));
    }
    if request.parameters.reasoning_summary.is_some() {
        return Err(provider_error(
            "bedrock_reasoning_summary_unsupported",
            ProviderErrorCategory::UnsupportedFeature,
            "Bedrock Anthropic extended thinking does not support provider-visible reasoning summaries",
        ));
    }
    if request.tool_call_policy.parallel == Some(true) {
        return Err(provider_error(
            "bedrock_parallel_tool_policy_unsupported",
            ProviderErrorCategory::UnsupportedFeature,
            "Bedrock model metadata does not guarantee parallel tool-call generation",
        ));
    }
    if request.conversation_reuse.mode.is_enabled()
        || request
            .conversation_reuse
            .previous_provider_response_id
            .is_some()
        || request.conversation_reuse.provider_state.is_some()
    {
        return Err(provider_error(
            "bedrock_conversation_reuse_unsupported",
            ProviderErrorCategory::UnsupportedFeature,
            "Bedrock provider-native conversation reuse is not supported by this adapter",
        ));
    }
    if !request.provider_context.request.is_empty() {
        return Err(provider_error(
            "bedrock_provider_options_unsupported",
            ProviderErrorCategory::UnsupportedFeature,
            "Bedrock provider-native request options are not supported by this adapter",
        ));
    }
    if request
        .parameters
        .max_output_tokens
        .is_some_and(|tokens| i32::try_from(tokens).is_err())
    {
        return Err(provider_error(
            "bedrock_max_output_tokens_out_of_range",
            ProviderErrorCategory::InvalidRequest,
            "Bedrock max_output_tokens must fit in a signed 32-bit integer",
        ));
    }
    validate_bedrock_cache_ttl(request)?;
    validate_declared_bedrock_features(request)?;
    validate_tool_choice_registration(request)
}

fn validate_declared_bedrock_features(request: &ModelTurnRequest) -> Result<(), ProviderError> {
    let unsupported = request.explicitly_unsupported_features(&bedrock_feature_support());
    unsupported.first().map_or(Ok(()), |feature| {
        Err(provider_error(
            "bedrock_feature_unsupported",
            ProviderErrorCategory::UnsupportedFeature,
            format!("Bedrock does not support requested feature {feature:?}"),
        ))
    })
}

fn validate_bedrock_cache_ttl(request: &ModelTurnRequest) -> Result<(), ProviderError> {
    let has_ttl = request.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::CachePoint {
                    hint: bcode_model::PromptCachePoint {
                        ttl_seconds: Some(_),
                        ..
                    }
                }
            )
        })
    });
    if has_ttl {
        Err(provider_error(
            "bedrock_cache_ttl_unsupported",
            ProviderErrorCategory::UnsupportedFeature,
            "Bedrock cache points do not accept a portable TTL",
        ))
    } else {
        Ok(())
    }
}

fn validate_tool_choice_registration(request: &ModelTurnRequest) -> Result<(), ProviderError> {
    match &request.tool_call_policy.choice {
        ToolChoice::Required if request.tools.is_empty() => Err(provider_error(
            "tool_choice_without_tools",
            ProviderErrorCategory::InvalidRequest,
            "required tool choice needs at least one registered tool",
        )),
        ToolChoice::Tool { name } if !request.tools.iter().any(|tool| tool.name == *name) => {
            Err(provider_error(
                "unknown_required_tool",
                ProviderErrorCategory::InvalidRequest,
                format!("required tool '{name}' is not registered"),
            ))
        }
        ToolChoice::Auto | ToolChoice::None | ToolChoice::Required | ToolChoice::Tool { .. } => {
            Ok(())
        }
    }
}

async fn stream_bedrock_turn_inner(
    request: &ModelTurnRequest,
    turn: &TurnState,
    discovery: Arc<Mutex<DiscoveryCache>>,
) -> Result<StreamOutcome, ProviderError> {
    validate_bedrock_request(request)?;
    let settings = Settings::resolve(Some(request));
    let transport = settings.transport.clone()?;
    let selection = resolve_turn_model_selection(request, &settings, turn, &discovery).await?;
    let name_map = bedrock_tool_name_map(&request.tools);
    if transport == BedrockTransport::MantleAnthropic {
        return stream_mantle_anthropic_turn(request, &settings, &selection, turn, name_map).await;
    }
    if transport == BedrockTransport::MantleOpenAi {
        // The Responses streaming adapter is not implemented yet. Fail closed rather than falling
        // through to Converse, which cannot serve these models at all.
        return Err(provider_error(
            "bedrock_mantle_openai_unsupported",
            ProviderErrorCategory::Config,
            "Bedrock Mantle OpenAI (Responses) streaming is not implemented yet; \
             use 'mantle_anthropic' or 'bedrock_runtime'",
        ));
    }
    let client = bedrock_client(&settings).await;
    if request.provider_context.api_surface == Some(bcode_model::ModelApiSurface::Messages) {
        return stream_bedrock_messages_turn(request, &client, &selection, turn, name_map).await;
    }
    let mut last_error = None;
    for model_id in &selection.model_ids {
        let mut effective_request;
        let request_for_model =
            if prompt_cache_known_unsupported(&discovery, selection.cache_key.as_ref(), model_id) {
                effective_request = request.clone();
                effective_request.prompt_cache = bcode_model::PromptCacheHints::default();
                &effective_request
            } else {
                request
            };
        let bedrock_request = build_converse_request(request_for_model, model_id.clone())?;
        let mut builder = client
            .converse_stream()
            .model_id(bedrock_request.model_id)
            .set_messages(Some(bedrock_request.messages));
        if !bedrock_request.system.is_empty() {
            builder = builder.set_system(Some(bedrock_request.system));
        }
        if let Some(tool_config) = bedrock_request.tool_config {
            builder = builder.tool_config(tool_config);
        }
        if let Some(inference_config) = bedrock_request.inference_config {
            builder = builder.inference_config(inference_config);
        }
        if let Some(additional_fields) = bedrock_request.additional_model_request_fields {
            builder = builder.additional_model_request_fields(additional_fields);
        }
        match builder.send().await {
            Ok(response) => {
                return read_bedrock_stream(response.stream, turn, name_map.clone()).await;
            }
            Err(error) => {
                let error = bedrock_sdk_error(&error);
                match handle_bedrock_turn_error(
                    error,
                    request,
                    request_for_model,
                    model_id,
                    &selection,
                    turn,
                    &client,
                    &discovery,
                    name_map.clone(),
                )
                .await
                {
                    TurnAttempt::Completed(outcome) => return outcome,
                    TurnAttempt::TryNextModel(error) => {
                        last_error = Some(error);
                    }
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        provider_error(
            "bedrock_model_discovery_empty",
            ProviderErrorCategory::Config,
            "Bedrock model discovery returned no usable streaming tool-use models; set BCODE_BEDROCK_MODEL or configure a Bedrock model profile",
        )
        .with_failure(bedrock_failure_context(
            bcode_model::ProviderFailureSourceKind::ModelProfile,
            "BCODE_BEDROCK_MODEL or model profile",
            bcode_model::ProviderFailureCapability::ModelDiscovery,
            "set BCODE_BEDROCK_MODEL or configure an accessible streaming Bedrock model",
        ))
    }))
}

async fn stream_mantle_anthropic_turn(
    request: &ModelTurnRequest,
    settings: &Settings,
    selection: &ModelSelection,
    turn: &TurnState,
    name_map: BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let token = client_context_bearer_token(settings).ok_or_else(|| {
        provider_error(
            "bedrock_mantle_missing_bearer_token",
            ProviderErrorCategory::Auth,
            "Bedrock Mantle requires AWS_BEARER_TOKEN_BEDROCK or a mapped bearer_token credential",
        )
    })?;
    let endpoint = mantle_anthropic_messages_endpoint(settings)?;
    let client = if settings.force_http1 {
        reqwest::Client::builder().http1_only().build()
    } else {
        reqwest::Client::builder().build()
    }
    .map_err(|error| mantle_network_error("client_build_failed", &error))?;
    let mut request_builder = client
        .post(endpoint.clone())
        .header("x-api-key", &token)
        .header("anthropic-version", "2023-06-01")
        .header("accept", "text/event-stream")
        .header("user-agent", "bcode/0.0.1");
    if settings.mantle_auth_header {
        request_builder = request_builder.bearer_auth(&token);
    }
    let mut last_error = None;
    for model_id in &selection.model_ids {
        let response = request_builder
            .try_clone()
            .ok_or_else(|| {
                provider_error(
                    "bedrock_mantle_request_clone_failed",
                    ProviderErrorCategory::ProviderInternal,
                    "failed to prepare Bedrock Mantle request",
                )
            })?
            .json(&build_mantle_anthropic_request(request, model_id)?)
            .send()
            .await
            .map_err(|error| mantle_network_error("request_failed", &error))?;
        if !response.status().is_success() {
            let error = mantle_status_error(response).await;
            let is_last = selection.model_ids.last().map(String::as_str) == Some(model_id);
            if selection.explicit || is_last {
                return Err(error);
            }
            last_error = Some(error);
            continue;
        }
        return read_mantle_anthropic_stream(response, turn, name_map.clone()).await;
    }
    Err(last_error.unwrap_or_else(|| {
        provider_error(
            "bedrock_mantle_model_unavailable",
            ProviderErrorCategory::Config,
            "no usable Bedrock Mantle Anthropic model was available for the turn",
        )
    }))
}

/// One Bedrock Mantle API flavor.
///
/// Mantle exposes provider-native surfaces under distinct path prefixes on the same regional
/// host. Each flavor pairs the default base-URL suffix with the request path appended to it, so a
/// new surface is added as data rather than as another hardcoded endpoint builder.
///
/// Note the `OpenAI` surface lives on `/openai/v1/responses`, which AWS documents as intentionally
/// different from the `/v1/responses` path used by other models on the responses endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MantleFlavor {
    /// Anthropic Messages surface (`/anthropic` + `/v1/messages`).
    Anthropic,
    /// `OpenAI` Responses surface (`/openai/v1` + `/responses`).
    OpenAi,
}

impl MantleFlavor {
    /// Default base-URL path suffix for this flavor.
    const fn base_url_suffix(self) -> &'static str {
        match self {
            Self::Anthropic => ".api.aws/anthropic",
            Self::OpenAi => ".api.aws/openai/v1",
        }
    }

    /// Request path appended to the resolved base URL.
    const fn request_path(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::OpenAi => "/responses",
        }
    }
}

/// Build the Mantle endpoint for one API flavor.
///
/// # Errors
///
/// Returns an error when the configured base URL cannot be parsed, or when it uses a
/// non-HTTPS scheme for a non-loopback host.
fn mantle_endpoint(settings: &Settings, flavor: MantleFlavor) -> Result<String, ProviderError> {
    let region = settings.region.as_deref().unwrap_or(DEFAULT_REGION);
    let base_url = settings.mantle_base_url.clone().unwrap_or_else(|| {
        format!(
            "{DEFAULT_MANTLE_BASE_URL_PREFIX}{region}{suffix}",
            suffix = flavor.base_url_suffix()
        )
    });
    let mut url = reqwest::Url::parse(base_url.trim()).map_err(|error| {
        provider_error(
            "bedrock_mantle_base_url_invalid",
            ProviderErrorCategory::Config,
            format!("invalid Bedrock Mantle base URL: {error}"),
        )
    })?;
    if url.scheme() != "https"
        && !url
            .host_str()
            .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost"))
    {
        return Err(provider_error(
            "bedrock_mantle_base_url_insecure",
            ProviderErrorCategory::Config,
            "Bedrock Mantle base URL must use HTTPS",
        ));
    }
    let path = format!(
        "{}{}",
        url.path().trim_end_matches('/'),
        flavor.request_path()
    );
    url.set_path(&path);
    Ok(url.to_string())
}

fn mantle_anthropic_messages_endpoint(settings: &Settings) -> Result<String, ProviderError> {
    mantle_endpoint(settings, MantleFlavor::Anthropic)
}

async fn mantle_status_error(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-amzn-requestid")
        .or_else(|| response.headers().get("request-id"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(sanitize_provider_diagnostic)
        })
        .unwrap_or_else(|| format!("Bedrock Mantle request failed with HTTP {status}"));
    let category = match status.as_u16() {
        401 | 403 => ProviderErrorCategory::Auth,
        400 | 404 | 409 | 422 => ProviderErrorCategory::InvalidRequest,
        408 | 425 | 429 | 500..=599 => ProviderErrorCategory::Network,
        _ => ProviderErrorCategory::ProviderInternal,
    };
    let mut error = provider_error("bedrock_mantle_http_error", category, message);
    error.request_id = request_id.map(String::into_boxed_str);
    error.retry = retry_hint_from_body(&body).map(Box::new);
    error
}

fn mantle_network_error(code: &str, error: &reqwest::Error) -> ProviderError {
    provider_error(
        format!("bedrock_mantle_{code}"),
        ProviderErrorCategory::Network,
        format!("Bedrock Mantle transport failed: {error}"),
    )
}

#[derive(Default)]
struct MantleSseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl MantleSseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<serde_json::Value>, ProviderError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=position).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8(line).map_err(|error| {
                provider_error(
                    "bedrock_mantle_stream_decode_failed",
                    ProviderErrorCategory::ProviderInternal,
                    format!("Bedrock Mantle stream was not valid UTF-8: {error}"),
                )
            })?;
            self.process_line(&line, &mut events)?;
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<serde_json::Value>, ProviderError> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = String::from_utf8(std::mem::take(&mut self.buffer)).map_err(|error| {
                provider_error(
                    "bedrock_mantle_stream_decode_failed",
                    ProviderErrorCategory::ProviderInternal,
                    format!("Bedrock Mantle stream was not valid UTF-8: {error}"),
                )
            })?;
            self.process_line(line.trim_end_matches('\r'), &mut events)?;
        }
        self.flush(&mut events)?;
        Ok(events)
    }

    fn process_line(
        &mut self,
        line: &str,
        events: &mut Vec<serde_json::Value>,
    ) -> Result<(), ProviderError> {
        if line.is_empty() {
            return self.flush(events);
        }
        if line.starts_with(':') {
            return Ok(());
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.data
                .push(data.strip_prefix(' ').unwrap_or(data).to_string());
        }
        Ok(())
    }

    fn flush(&mut self, events: &mut Vec<serde_json::Value>) -> Result<(), ProviderError> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = self.data.join("\n");
        self.data.clear();
        if data == "[DONE]" {
            return Ok(());
        }
        events.push(serde_json::from_str(&data).map_err(|error| {
            provider_error(
                "bedrock_mantle_stream_decode_failed",
                ProviderErrorCategory::ProviderInternal,
                format!("failed to decode Bedrock Mantle SSE event: {error}"),
            )
        })?);
        Ok(())
    }
}

async fn read_mantle_anthropic_stream(
    mut response: reqwest::Response,
    turn: &TurnState,
    name_map: BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let mut decoder = MantleSseDecoder::default();
    let mut accumulator = AnthropicMessagesAccumulator::new(name_map);
    loop {
        if turn.is_cancelled() {
            return Ok(StreamOutcome::Cancelled);
        }
        let cancel_notify = turn.cancel_notify();
        tokio::select! {
            chunk = response.chunk() => {
                if let Some(chunk) = chunk.map_err(|error| mantle_network_error("stream_failed", &error))? {
                    for event in decoder.push(&chunk)? {
                        if let Some(outcome) = accumulator.process(&event, turn)? {
                            return Ok(outcome);
                        }
                    }
                } else {
                    for event in decoder.finish()? {
                        if let Some(outcome) = accumulator.process(&event, turn)? {
                            return Ok(outcome);
                        }
                    }
                    return Ok(accumulator.finish());
                }
            }
            () = cancel_notify.notified() => return Ok(StreamOutcome::Cancelled),
        }
    }
}

async fn stream_bedrock_messages_turn(
    request: &ModelTurnRequest,
    client: &Client,
    selection: &ModelSelection,
    turn: &TurnState,
    name_map: BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let mut last_error = None;
    for model_id in &selection.model_ids {
        match stream_bedrock_messages_model(request, client, model_id, turn, name_map.clone()).await
        {
            Ok(outcome) => return Ok(outcome),
            Err(error) => {
                let is_last = selection.model_ids.last().map(String::as_str) == Some(model_id);
                if selection.explicit || is_last {
                    return Err(error);
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        provider_error(
            "bedrock_messages_model_unavailable",
            ProviderErrorCategory::Config,
            "no usable Bedrock Messages model was available for the turn",
        )
    }))
}

async fn stream_bedrock_messages_model(
    request: &ModelTurnRequest,
    client: &Client,
    model_id: &str,
    turn: &TurnState,
    name_map: BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let body = build_anthropic_messages_request(request)?;
    let response = client
        .invoke_model_with_response_stream()
        .model_id(model_id)
        .content_type("application/json")
        .accept("application/json")
        .body(Blob::new(body))
        .send()
        .await
        .map_err(|error| bedrock_messages_sdk_error(&error))?;
    read_anthropic_messages_stream(response.body, turn, name_map).await
}

/// Outcome of a single per-model Converse attempt.
enum TurnAttempt {
    /// The attempt reached a terminal result (success or a fatal error) for the whole turn.
    Completed(Result<StreamOutcome, ProviderError>),
    /// The model was structurally incompatible; try the next discovered model.
    TryNextModel(ProviderError),
}

/// Classify and react to a Converse send error for one model.
///
/// Handles prompt-cache rejection, streaming-tool incompatibility, and rerouting models rejected
/// by Converse through the Anthropic Messages adapter.
#[allow(clippy::too_many_arguments)]
async fn handle_bedrock_turn_error(
    error: ProviderError,
    request: &ModelTurnRequest,
    request_for_model: &ModelTurnRequest,
    model_id: &str,
    selection: &ModelSelection,
    turn: &TurnState,
    client: &Client,
    discovery: &Arc<Mutex<DiscoveryCache>>,
    name_map: BTreeMap<String, String>,
) -> TurnAttempt {
    if prompt_cache_rejected(&error) && request_for_model.prompt_cache.mode.is_enabled() {
        turn.push(ProviderTurnEvent::Warning {
            message: format!(
                "Bedrock model {model_id} rejected prompt cache points; retrying without explicit cache points"
            ),
        });
        mark_prompt_cache_unsupported(
            discovery,
            selection.cache_key.as_ref(),
            model_id,
            &error.message,
        );
        return TurnAttempt::Completed(
            retry_bedrock_without_prompt_cache(client, request, model_id, turn, name_map).await,
        );
    }
    let is_last = selection.model_ids.last().map(String::as_str) == Some(model_id);
    if !selection.explicit && streaming_tool_use_unsupported(&error) && !is_last {
        mark_streaming_tool_unsupported(
            discovery,
            selection.cache_key.as_ref(),
            model_id,
            &error.message,
        );
        turn.push(ProviderTurnEvent::Warning {
            message: format!(
                "Bedrock model {model_id} does not support streaming tool use; retrying another discovered model"
            ),
        });
        return TurnAttempt::TryNextModel(error);
    }
    if model_unusable_via_converse(&error) {
        turn.push(ProviderTurnEvent::Warning {
            message: format!(
                "Bedrock model {model_id} requires the Anthropic Messages API; retrying through InvokeModelWithResponseStream"
            ),
        });
        return TurnAttempt::Completed(
            stream_bedrock_messages_model(request, client, model_id, turn, name_map).await,
        );
    }
    TurnAttempt::Completed(Err(error))
}

async fn retry_bedrock_without_prompt_cache(
    client: &Client,
    request: &ModelTurnRequest,
    model_id: &str,
    turn: &TurnState,
    name_map: BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let mut retry_request = request.clone();
    retry_request.prompt_cache = bcode_model::PromptCacheHints::default();
    let bedrock_request = build_converse_request(&retry_request, model_id.to_string())?;
    let mut retry_builder = client
        .converse_stream()
        .model_id(bedrock_request.model_id)
        .set_messages(Some(bedrock_request.messages));
    if !bedrock_request.system.is_empty() {
        retry_builder = retry_builder.set_system(Some(bedrock_request.system));
    }
    if let Some(tool_config) = bedrock_request.tool_config {
        retry_builder = retry_builder.tool_config(tool_config);
    }
    if let Some(inference_config) = bedrock_request.inference_config {
        retry_builder = retry_builder.inference_config(inference_config);
    }
    if let Some(additional_fields) = bedrock_request.additional_model_request_fields {
        retry_builder = retry_builder.additional_model_request_fields(additional_fields);
    }
    match retry_builder.send().await {
        Ok(response) => read_bedrock_stream(response.stream, turn, name_map).await,
        Err(retry_error) => Err(bedrock_sdk_error(&retry_error)),
    }
}

async fn bedrock_client(settings: &Settings) -> Client {
    let config = bedrock_sdk_config(settings).await;
    client_context_bearer_token(settings).map_or_else(
        || Client::new(&config),
        |token| {
            let config = aws_sdk_bedrockruntime::config::Builder::from(&config)
                .bearer_token(aws_sdk_bedrockruntime::config::Token::new(token, None))
                .auth_scheme_preference([
                    aws_smithy_runtime_api::client::auth::http::HTTP_BEARER_AUTH_SCHEME_ID,
                ])
                .build();
            Client::from_conf(config)
        },
    )
}

async fn bedrock_sdk_config(settings: &Settings) -> aws_config::SdkConfig {
    let mut config = bedrock_sdk_config_with_region(settings, settings.region.clone()).await;
    if config.region().is_none() {
        tracing::debug!(
            target: "bcode_bedrock::config",
            fallback_region = DEFAULT_REGION,
            "AWS SDK region chain did not resolve a region; using Bedrock fallback region"
        );
        config = bedrock_sdk_config_with_region(settings, Some(DEFAULT_REGION.to_string())).await;
    }
    config
}

async fn bedrock_sdk_config_with_region(
    settings: &Settings,
    region: Option<String>,
) -> aws_config::SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(region) = region {
        loader = loader.region(Region::new(region));
    }
    if let Some(profile) = &settings.aws_profile {
        loader = loader.profile_name(profile.clone());
    }
    if let Some(endpoint_url) = &settings.endpoint_url {
        loader = loader.endpoint_url(endpoint_url.clone());
    }
    if let Some(credentials) = client_context_credentials(settings) {
        loader = loader.credentials_provider(credentials);
    }
    loader.load().await
}

fn client_context_bearer_token(settings: &Settings) -> Option<String> {
    settings
        .auth_credentials
        .get("bearer_token")
        .or_else(|| settings.env.get("AWS_BEARER_TOKEN_BEDROCK"))
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

fn client_context_credentials(settings: &Settings) -> Option<Credentials> {
    let access_key = settings
        .auth_credentials
        .get("access_key_id")
        .or_else(|| settings.env.get("AWS_ACCESS_KEY_ID"))
        .filter(|value| !value.trim().is_empty())?;
    let secret_key = settings
        .auth_credentials
        .get("secret_access_key")
        .or_else(|| settings.env.get("AWS_SECRET_ACCESS_KEY"))
        .filter(|value| !value.trim().is_empty())?;
    let session_token = settings
        .auth_credentials
        .get("session_token")
        .or_else(|| settings.env.get("AWS_SESSION_TOKEN"))
        .filter(|value| !value.trim().is_empty())
        .cloned();
    Some(Credentials::new(
        access_key.clone(),
        secret_key.clone(),
        session_token,
        None,
        "bcode-client-context",
    ))
}

async fn read_anthropic_messages_stream(
    mut stream: aws_sdk_bedrockruntime::primitives::event_stream::EventReceiver<
        ResponseStream,
        aws_sdk_bedrockruntime::types::error::ResponseStreamError,
    >,
    turn: &TurnState,
    name_map: BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let mut accumulator = AnthropicMessagesAccumulator::new(name_map);
    loop {
        if turn.is_cancelled() {
            return Ok(StreamOutcome::Cancelled);
        }
        let cancel_notify = turn.cancel_notify();
        tokio::select! {
            event = stream.recv() => {
                let Some(event) = event.map_err(|error| bedrock_messages_stream_error(&error))? else {
                    return Ok(accumulator.finish());
                };
                if let ResponseStream::Chunk(chunk) = event
                    && let Some(bytes) = chunk.bytes()
                {
                    let event = serde_json::from_slice::<serde_json::Value>(bytes.as_ref()).map_err(|error| {
                        provider_error(
                            "bedrock_messages_stream_decode_failed",
                            ProviderErrorCategory::ProviderInternal,
                            format!("failed to decode Bedrock Messages stream event: {error}"),
                        )
                    })?;
                    if let Some(outcome) = accumulator.process(&event, turn)? {
                        return Ok(outcome);
                    }
                }
            }
            () = cancel_notify.notified() => return Ok(StreamOutcome::Cancelled),
        }
    }
}

fn anthropic_messages_event_error(event: &serde_json::Value) -> ProviderError {
    let error_type = event
        .pointer("/error/type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("stream_error");
    let message = event
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || "Bedrock Anthropic Messages stream returned an error".to_string(),
            sanitize_provider_diagnostic,
        );
    let category = match error_type {
        "authentication_error" | "permission_error" => ProviderErrorCategory::Auth,
        "invalid_request_error" | "not_found_error" => ProviderErrorCategory::InvalidRequest,
        "rate_limit_error" | "overloaded_error" | "api_error" => ProviderErrorCategory::Network,
        _ => ProviderErrorCategory::ProviderInternal,
    };
    provider_error(format!("bedrock_anthropic_{error_type}"), category, message)
}

struct AnthropicMessagesAccumulator {
    tool_calls: BTreeMap<u32, ToolCallAccumulator>,
    reasoning_blocks: BTreeMap<u32, String>,
    saw_tool_call: bool,
    /// Normalized stop reason reported by the provider on `message_delta`.
    ///
    /// Anthropic reports `max_tokens` here when the output budget is exhausted. Without it a
    /// truncated turn is indistinguishable from a completed one.
    stop_reason: Option<StopReason>,
    name_map: BTreeMap<String, String>,
}

impl AnthropicMessagesAccumulator {
    const fn new(name_map: BTreeMap<String, String>) -> Self {
        Self {
            tool_calls: BTreeMap::new(),
            reasoning_blocks: BTreeMap::new(),
            saw_tool_call: false,
            stop_reason: None,
            name_map,
        }
    }

    fn process(
        &mut self,
        event: &serde_json::Value,
        turn: &TurnState,
    ) -> Result<Option<StreamOutcome>, ProviderError> {
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => Self::emit_usage(
                event
                    .get("message")
                    .and_then(|message| message.get("usage")),
                turn,
                true,
            ),
            Some("error") => return Err(anthropic_messages_event_error(event)),
            Some("content_block_start") => self.start_content_block(event, turn)?,
            Some("content_block_delta") => self.content_block_delta(event, turn),
            Some("content_block_stop") => self.finish_content_block(event, turn)?,
            Some("message_delta") => {
                self.record_message_delta_stop_reason(event);
                Self::emit_usage(event.get("usage"), turn, false);
            }
            Some("message_stop") => return Ok(Some(self.finish())),
            _ => {}
        }
        Ok(None)
    }

    fn start_content_block(
        &mut self,
        event: &serde_json::Value,
        turn: &TurnState,
    ) -> Result<(), ProviderError> {
        let index = event_index(event)?;
        let block = event
            .get("content_block")
            .unwrap_or(&serde_json::Value::Null);
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("tool_use") => {
                let id = required_event_string(block, "id")?;
                let name = required_event_string(block, "name")?;
                self.saw_tool_call = true;
                self.tool_calls.insert(
                    index,
                    ToolCallAccumulator {
                        id: Some(id.clone()),
                        name: Some(name.clone()),
                        arguments: String::new(),
                    },
                );
                turn.push(ProviderTurnEvent::ToolCallStarted {
                    call_id: id,
                    name: original_tool_name(&name, &self.name_map),
                });
            }
            Some("thinking" | "redacted_thinking") => {
                self.reasoning_blocks.insert(index, String::new());
                turn.push(ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::Started {
                        activity_id: format!("bedrock-messages-reasoning-{index}"),
                        order: index,
                    },
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn content_block_delta(&mut self, event: &serde_json::Value, turn: &TurnState) {
        let Some(index) = event
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        else {
            return;
        };
        let delta = event.get("delta").unwrap_or(&serde_json::Value::Null);
        match delta.get("type").and_then(serde_json::Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = delta
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    turn.push(ProviderTurnEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
            }
            Some("input_json_delta") => {
                if let Some(json) = delta
                    .get("partial_json")
                    .and_then(serde_json::Value::as_str)
                    && let Some(tool) = self.tool_calls.get_mut(&index)
                {
                    tool.arguments.push_str(json);
                    if let Some(call_id) = &tool.id {
                        turn.push(ProviderTurnEvent::ToolCallDelta {
                            call_id: call_id.clone(),
                            delta: json.to_string(),
                        });
                    }
                }
            }
            Some("thinking_delta") => {
                if let Some(text) = delta
                    .get("thinking")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    self.reasoning_blocks
                        .entry(index)
                        .or_default()
                        .push_str(text);
                    turn.push(ProviderTurnEvent::ReasoningActivity {
                        event: bcode_session_models::ReasoningActivityEvent::PartDelta {
                            activity_id: format!("bedrock-messages-reasoning-{index}"),
                            activity_order: index,
                            part_id: format!("raw-{index}"),
                            kind: bcode_session_models::ReasoningContentKind::Raw,
                            role: bcode_session_models::ReasoningContentRole::Detail,
                            part_order: index,
                            text: text.to_string(),
                        },
                    });
                }
            }
            _ => {}
        }
    }

    fn finish_content_block(
        &mut self,
        event: &serde_json::Value,
        turn: &TurnState,
    ) -> Result<(), ProviderError> {
        let index = event_index(event)?;
        if let Some(tool) = self.tool_calls.get(&index) {
            let id = tool.id.clone().ok_or_else(|| {
                provider_error(
                    "missing_tool_call_id",
                    ProviderErrorCategory::ProviderInternal,
                    "Bedrock Messages emitted a tool call without an id",
                )
            })?;
            let name = tool.name.clone().ok_or_else(|| {
                provider_error(
                    "missing_tool_call_name",
                    ProviderErrorCategory::ProviderInternal,
                    "Bedrock Messages emitted a tool call without a name",
                )
            })?;
            let arguments = if tool.arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&tool.arguments).map_err(|error| {
                    provider_error(
                        "tool_arguments_decode_failed",
                        ProviderErrorCategory::ProviderInternal,
                        format!("failed to decode arguments for tool call {id} ({name}): {error}"),
                    )
                })?
            };
            turn.push(ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id,
                    name: original_tool_name(&name, &self.name_map),
                    arguments,
                },
            });
        }
        if self.reasoning_blocks.remove(&index).is_some() {
            turn.push(ProviderTurnEvent::ReasoningActivity {
                event: bcode_session_models::ReasoningActivityEvent::Finished {
                    activity_id: format!("bedrock-messages-reasoning-{index}"),
                    activity_order: index,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                },
            });
        }
        Ok(())
    }

    /// Record the normalized stop reason carried by an Anthropic `message_delta` event.
    fn record_message_delta_stop_reason(&mut self, event: &serde_json::Value) {
        if let Some(stop_reason) = event
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(serde_json::Value::as_str)
            .and_then(map_anthropic_stop_reason)
        {
            self.stop_reason = Some(stop_reason);
        }
    }

    fn emit_usage(usage: Option<&serde_json::Value>, turn: &TurnState, exact_input: bool) {
        let Some(usage) = usage else {
            return;
        };
        let input_tokens = usage.get("input_tokens").and_then(json_u32);
        let output_tokens = usage.get("output_tokens").and_then(json_u32);
        let cached_input_tokens = usage.get("cache_read_input_tokens").and_then(json_u32);
        let cache_write_input_tokens = usage.get("cache_creation_input_tokens").and_then(json_u32);
        turn.push(ProviderTurnEvent::Usage {
            usage: TokenUsage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cache_write_input_tokens,
                ..TokenUsage::default()
            },
        });
        if exact_input && let Some(input_tokens) = input_tokens {
            turn.push(ProviderTurnEvent::ExactRequestInputTokens {
                tokens: bcode_model::ExactRequestInputTokens::new(complete_request_input_tokens(
                    input_tokens,
                    cached_input_tokens,
                    cache_write_input_tokens,
                )),
            });
        }
    }

    /// Choose the stream outcome for a finished Anthropic Messages turn.
    ///
    /// A truncated turn must not be reported as a completed tool call: the model may have started
    /// a `tool_use` block that never closed, so no complete tool call was emitted. Reporting
    /// truncation lets the host continue the turn instead of failing it.
    const fn finish(&self) -> StreamOutcome {
        if matches!(self.stop_reason, Some(StopReason::MaxTokens)) {
            return StreamOutcome::MaxTokens;
        }
        if self.saw_tool_call {
            StreamOutcome::ToolCall
        } else {
            StreamOutcome::Finished
        }
    }
}

fn event_index(event: &serde_json::Value) -> Result<u32, ProviderError> {
    event.get("index").and_then(json_u32).ok_or_else(|| {
        provider_error(
            "bedrock_messages_stream_invalid_event",
            ProviderErrorCategory::ProviderInternal,
            "Bedrock Messages stream event omitted a valid content block index",
        )
    })
}

fn required_event_string(value: &serde_json::Value, key: &str) -> Result<String, ProviderError> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            provider_error(
                "bedrock_messages_stream_invalid_event",
                ProviderErrorCategory::ProviderInternal,
                format!("Bedrock Messages stream content block omitted {key}"),
            )
        })
}

fn json_u32(value: &serde_json::Value) -> Option<u32> {
    value.as_u64().and_then(|value| u32::try_from(value).ok())
}

async fn read_bedrock_stream(
    mut stream: aws_sdk_bedrockruntime::primitives::event_stream::EventReceiver<
        ConverseStreamOutput,
        aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError,
    >,
    turn: &TurnState,
    name_map: BTreeMap<String, String>,
) -> Result<StreamOutcome, ProviderError> {
    let mut accumulator = StreamAccumulator::new(name_map);
    loop {
        if turn.is_cancelled() {
            return Ok(StreamOutcome::Cancelled);
        }
        let cancel_notify = turn.cancel_notify();
        tokio::select! {
            event = stream.recv() => {
                let Some(event) = event.map_err(|error| bedrock_stream_error(&error))? else {
                    return Ok(accumulator.finish_outcome());
                };
                if let Some(outcome) = accumulator.process_event(event, turn)? {
                    return Ok(outcome);
                }
            }
            () = cancel_notify.notified() => return Ok(StreamOutcome::Cancelled),
        }
    }
}

#[derive(Debug)]
struct StreamAccumulator {
    tool_calls: BTreeMap<i32, ToolCallAccumulator>,
    saw_tool_call: bool,
    stop_reason: Option<StopReason>,
    reasoning_blocks: BTreeMap<i32, String>,
    name_map: BTreeMap<String, String>,
    saw_message_stop: bool,
}

impl StreamAccumulator {
    const fn new(name_map: BTreeMap<String, String>) -> Self {
        Self {
            tool_calls: BTreeMap::new(),
            saw_tool_call: false,
            stop_reason: None,
            reasoning_blocks: BTreeMap::new(),
            name_map,
            saw_message_stop: false,
        }
    }

    fn process_event(
        &mut self,
        event: ConverseStreamOutput,
        turn: &TurnState,
    ) -> Result<Option<StreamOutcome>, ProviderError> {
        match event {
            ConverseStreamOutput::ContentBlockStart(event) => {
                if let Some(ContentBlockStart::ToolUse(tool_use)) = event.start() {
                    let entry = self
                        .tool_calls
                        .entry(event.content_block_index())
                        .or_default();
                    entry.id = Some(tool_use.tool_use_id().to_string());
                    entry.name = Some(tool_use.name().to_string());
                    self.saw_tool_call = true;
                    turn.push(ProviderTurnEvent::ToolCallStarted {
                        call_id: tool_use.tool_use_id().to_string(),
                        name: original_tool_name(tool_use.name(), &self.name_map),
                    });
                }
            }
            ConverseStreamOutput::ContentBlockDelta(event) => match event.delta() {
                Some(ContentBlockDelta::Text(text)) if !text.is_empty() => {
                    turn.push(ProviderTurnEvent::TextDelta { text: text.clone() });
                }
                Some(ContentBlockDelta::ToolUse(delta)) => {
                    self.process_tool_use_delta(event.content_block_index(), delta.input(), turn);
                }
                Some(ContentBlockDelta::ReasoningContent(delta)) => {
                    self.process_reasoning_delta(event.content_block_index(), delta, turn);
                }
                _ => {}
            },
            ConverseStreamOutput::Metadata(event) => {
                if let Some(usage) = event.usage() {
                    let input_tokens = nonnegative_u32(usage.input_tokens());
                    let cache_read_input_tokens =
                        usage.cache_read_input_tokens().and_then(nonnegative_u32);
                    let cache_write_input_tokens =
                        usage.cache_write_input_tokens().and_then(nonnegative_u32);
                    turn.push(ProviderTurnEvent::Usage {
                        usage: TokenUsage {
                            input_tokens,
                            output_tokens: nonnegative_u32(usage.output_tokens()),
                            cached_input_tokens: cache_read_input_tokens,
                            cache_write_input_tokens,
                            ..TokenUsage::default()
                        },
                    });
                    if let Some(input_tokens) = input_tokens {
                        turn.push(ProviderTurnEvent::ExactRequestInputTokens {
                            tokens: bcode_model::ExactRequestInputTokens::new(
                                complete_request_input_tokens(
                                    input_tokens,
                                    cache_read_input_tokens,
                                    cache_write_input_tokens,
                                ),
                            ),
                        });
                    }
                }
                // `metadata` is the documented final Converse event. Once the message already
                // stopped, terminate instead of waiting on end-of-stream.
                if self.saw_message_stop {
                    return Ok(Some(self.finish_outcome()));
                }
            }
            ConverseStreamOutput::MessageStop(event) => {
                self.stop_reason = Some(map_stop_reason(event.stop_reason()));
                self.finish_reasoning(turn);
                if self.saw_tool_call {
                    self.finish_tool_calls(turn)?;
                }
                // `messageStop` is not the last Converse event: the trailing `metadata` event
                // carries `usage`, which is the only source of provider-exact request input
                // tokens. Keep reading so the stream terminates on end-of-stream instead of
                // discarding usage. `finish_outcome` reproduces the outcome chosen here.
                self.saw_message_stop = true;
            }
            _ => {}
        }
        Ok(None)
    }

    fn process_reasoning_delta(
        &mut self,
        content_block_index: i32,
        delta: &ReasoningContentBlockDelta,
        turn: &TurnState,
    ) {
        let part_order = u32::try_from(content_block_index).unwrap_or_default();
        let activity_id = format!("bedrock-reasoning-{content_block_index}");
        let part_id = format!("raw-{part_order}");
        let started = self.reasoning_blocks.contains_key(&content_block_index);
        if !started {
            self.reasoning_blocks
                .insert(content_block_index, String::new());
            turn.push(ProviderTurnEvent::ReasoningActivity {
                event: bcode_session_models::ReasoningActivityEvent::Started {
                    activity_id: activity_id.clone(),
                    order: part_order,
                },
            });
        }
        match delta {
            ReasoningContentBlockDelta::Text(text) if !text.is_empty() => {
                self.reasoning_blocks
                    .entry(content_block_index)
                    .or_default()
                    .push_str(text);
                turn.push(ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::PartDelta {
                        activity_id,
                        activity_order: part_order,
                        part_id,
                        kind: bcode_session_models::ReasoningContentKind::Raw,
                        role: bcode_session_models::ReasoningContentRole::Detail,
                        part_order,
                        text: text.clone(),
                    },
                });
            }
            ReasoningContentBlockDelta::RedactedContent(_) => {
                turn.push(ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::OpaqueObserved {
                        activity_id,
                        activity_order: part_order,
                    },
                });
            }
            _ => {}
        }
    }

    fn finish_reasoning(&mut self, turn: &TurnState) {
        for (content_block_index, text) in std::mem::take(&mut self.reasoning_blocks) {
            let part_order = u32::try_from(content_block_index).unwrap_or_default();
            let activity_id = format!("bedrock-reasoning-{content_block_index}");
            if !text.is_empty() {
                turn.push(ProviderTurnEvent::ReasoningActivity {
                    event: bcode_session_models::ReasoningActivityEvent::PartCompleted {
                        activity_id: activity_id.clone(),
                        activity_order: part_order,
                        part_id: format!("raw-{part_order}"),
                        kind: bcode_session_models::ReasoningContentKind::Raw,
                        role: bcode_session_models::ReasoningContentRole::Detail,
                        part_order,
                        text,
                    },
                });
            }
            turn.push(ProviderTurnEvent::ReasoningActivity {
                event: bcode_session_models::ReasoningActivityEvent::Finished {
                    activity_id,
                    activity_order: part_order,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                },
            });
        }
    }

    fn process_tool_use_delta(&mut self, content_block_index: i32, input: &str, turn: &TurnState) {
        let entry = self.tool_calls.entry(content_block_index).or_default();
        entry.arguments.push_str(input);
        if !input.is_empty()
            && let Some(call_id) = &entry.id
        {
            turn.push(ProviderTurnEvent::ToolCallDelta {
                call_id: call_id.clone(),
                delta: input.to_string(),
            });
        }
    }

    fn finish_tool_calls(&self, turn: &TurnState) -> Result<(), ProviderError> {
        for accumulator in self.tool_calls.values() {
            let id = accumulator.id.clone().ok_or_else(|| {
                provider_error(
                    "missing_tool_call_id",
                    ProviderErrorCategory::ProviderInternal,
                    "Bedrock emitted a tool call without an id",
                )
            })?;
            let name = accumulator.name.clone().ok_or_else(|| {
                provider_error(
                    "missing_tool_call_name",
                    ProviderErrorCategory::ProviderInternal,
                    "Bedrock emitted a tool call without a name",
                )
            })?;
            let arguments = if accumulator.arguments.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&accumulator.arguments).map_err(|error| {
                    let mut error = provider_error(
                        "tool_arguments_decode_failed",
                        ProviderErrorCategory::ProviderInternal,
                        format!(
                            "failed to decode arguments for tool call {id} ({name}): {error}; received {} bytes",
                            accumulator.arguments.len()
                        ),
                    );
                    error.retryable = false;
                    error
                })?
            };
            turn.push(ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id,
                    name: original_tool_name(&name, &self.name_map),
                    arguments,
                },
            });
        }
        Ok(())
    }

    /// Choose the stream outcome for a finished Converse turn.
    ///
    /// Mirrors [`AnthropicMessagesAccumulator::finish`]: a turn truncated by the output budget is
    /// reported as truncation rather than a completed tool call.
    const fn finish_outcome(&self) -> StreamOutcome {
        if matches!(self.stop_reason, Some(StopReason::MaxTokens)) {
            return StreamOutcome::MaxTokens;
        }
        if self.saw_tool_call {
            StreamOutcome::ToolCall
        } else {
            StreamOutcome::Finished
        }
    }
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

struct BedrockConverseRequest {
    model_id: String,
    messages: Vec<BedrockMessage>,
    system: Vec<SystemContentBlock>,
    tool_config: Option<ToolConfiguration>,
    inference_config: Option<InferenceConfiguration>,
    additional_model_request_fields: Option<Document>,
}

fn build_anthropic_messages_request_value(
    request: &ModelTurnRequest,
) -> Result<serde_json::Map<String, serde_json::Value>, ProviderError> {
    let mut body = serde_json::Map::new();
    body.insert(
        "anthropic_version".to_string(),
        serde_json::json!("bedrock-2023-05-31"),
    );
    body.insert(
        "max_tokens".to_string(),
        serde_json::json!(request.parameters.max_output_tokens.unwrap_or(4_096)),
    );
    if let Some(system) = anthropic_system_content(request) {
        body.insert("system".to_string(), system);
    }
    body.insert(
        "messages".to_string(),
        serde_json::Value::Array(
            request
                .messages
                .iter()
                .filter_map(|message| anthropic_message(message).transpose())
                .collect::<Result<Vec<_>, _>>()?,
        ),
    );
    if !request.tools.is_empty() && !matches!(request.tool_call_policy.choice, ToolChoice::None) {
        body.insert(
            "tools".to_string(),
            serde_json::Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "name": bedrock_tool_name(&tool.name),
                            "description": tool.description,
                            "input_schema": tool.input_schema,
                        })
                    })
                    .collect(),
            ),
        );
        body.insert("tool_choice".to_string(), anthropic_tool_choice(request)?);
    }
    if request.prompt_cache.cache_tools
        && let Some(last_tool) = body
            .get_mut("tools")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|tools| tools.last_mut())
            .and_then(serde_json::Value::as_object_mut)
    {
        last_tool.insert("cache_control".to_string(), anthropic_cache_control());
    }
    if let Some(temperature) = request.parameters.temperature {
        body.insert("temperature".to_string(), serde_json::json!(temperature));
    }
    if let Some(top_p) = request.parameters.top_p {
        body.insert("top_p".to_string(), serde_json::json!(top_p));
    }
    if !request.parameters.stop_sequences.is_empty() {
        body.insert(
            "stop_sequences".to_string(),
            serde_json::json!(request.parameters.stop_sequences),
        );
    }
    apply_anthropic_thinking_fields(&mut body, &request.parameters);
    Ok(body)
}

fn build_anthropic_messages_request(request: &ModelTurnRequest) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(&build_anthropic_messages_request_value(request)?)
        .map_err(|error| build_error(&error))
}

fn build_mantle_anthropic_request(
    request: &ModelTurnRequest,
    model_id: &str,
) -> Result<serde_json::Value, ProviderError> {
    let mut body = build_anthropic_messages_request_value(request)?;
    body.remove("anthropic_version");
    body.insert("model".to_string(), serde_json::json!(model_id));
    body.insert("stream".to_string(), serde_json::Value::Bool(true));
    Ok(serde_json::Value::Object(body))
}

fn anthropic_system_content(request: &ModelTurnRequest) -> Option<serde_json::Value> {
    let mut blocks = Vec::new();
    if let Some(prompt) = request
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        let mut block = serde_json::json!({"type": "text", "text": prompt});
        if request.prompt_cache.cache_system_prompt {
            block
                .as_object_mut()
                .expect("text block is an object")
                .insert("cache_control".to_string(), anthropic_cache_control());
        }
        blocks.push(block);
    }
    for message in &request.messages {
        if message.role == MessageRole::System {
            let text = joined_text_content(message);
            if !text.is_empty() {
                blocks.push(serde_json::json!({"type": "text", "text": text}));
            }
        }
    }
    (!blocks.is_empty()).then_some(serde_json::Value::Array(blocks))
}

fn anthropic_message(message: &ModelMessage) -> Result<Option<serde_json::Value>, ProviderError> {
    let role = match message.role {
        MessageRole::System => return Ok(None),
        MessageRole::User | MessageRole::Tool => "user",
        MessageRole::Assistant => "assistant",
    };
    let content = anthropic_content_blocks(message)?;
    Ok((!content.is_empty()).then(|| serde_json::json!({"role": role, "content": content})))
}

fn anthropic_cache_control() -> serde_json::Value {
    serde_json::json!({"type": "ephemeral"})
}

/// Placeholder text used when a failed tool result carries no model-visible content.
///
/// The Anthropic Messages schema rejects a `tool_result` block whose `content` is
/// empty while `is_error` is `true`, so failed results must always carry text.
const EMPTY_ERROR_TOOL_RESULT_PLACEHOLDER: &str = "Tool call failed without producing any output.";

fn anthropic_content_blocks(
    message: &ModelMessage,
) -> Result<Vec<serde_json::Value>, ProviderError> {
    let mut blocks = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(serde_json::json!({"type": "text", "text": text}));
            }
            ContentBlock::Image { image } => {
                base64::engine::general_purpose::STANDARD.decode(&image.data_base64).map_err(|error| {
                    provider_error(
                        "bedrock_invalid_image_data",
                        ProviderErrorCategory::InvalidRequest,
                        format!("invalid image data: {error}"),
                    )
                })?;
                blocks.push(serde_json::json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": image.mime_type, "data": image.data_base64}
                }));
            }
            ContentBlock::ToolCall { call } => blocks.push(serde_json::json!({
                "type": "tool_use", "id": call.id, "name": bedrock_tool_name(&call.name), "input": call.arguments,
            })),
            ContentBlock::ToolResult { result } => {
                let mut content = Vec::with_capacity(result.content.len() + 1);
                if !result.output.is_empty() {
                    content.push(serde_json::json!({"type": "text", "text": result.output}));
                }
                content.extend(result.content.iter().map(|item| match item {
                    bcode_model::ToolResultContent::Text { text } => serde_json::json!({"type": "text", "text": text}),
                    bcode_model::ToolResultContent::Image { image } => serde_json::json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": image.mime_type, "data": image.data_base64}
                    }),
                    bcode_model::ToolResultContent::ImageRef { image } => serde_json::json!({
                        "type": "text",
                        "text": format!("[image reference: {} {}]", image.path, image.mime_type),
                    }),
                }));
                if content.is_empty() && result.is_error {
                    content.push(serde_json::json!({
                        "type": "text", "text": EMPTY_ERROR_TOOL_RESULT_PLACEHOLDER,
                    }));
                }
                blocks.push(serde_json::json!({
                    "type": "tool_result", "tool_use_id": result.call_id,
                    "content": content, "is_error": result.is_error,
                }));
            }
            ContentBlock::CachePoint { .. } => {
                if let Some(previous) = blocks
                    .last_mut()
                    .and_then(serde_json::Value::as_object_mut)
                {
                    previous.insert("cache_control".to_string(), anthropic_cache_control());
                }
            }
            ContentBlock::ProviderExtension { .. } | ContentBlock::Text { .. } => {}
        }
    }
    Ok(blocks)
}

fn anthropic_tool_choice(request: &ModelTurnRequest) -> Result<serde_json::Value, ProviderError> {
    match &request.tool_call_policy.choice {
        ToolChoice::Auto => Ok(serde_json::json!({"type": "auto"})),
        ToolChoice::Required => Ok(serde_json::json!({"type": "any"})),
        ToolChoice::Tool { name } => {
            let tool = request
                .tools
                .iter()
                .find(|tool| tool.name == *name)
                .ok_or_else(|| {
                    provider_error(
                        "unknown_required_tool",
                        ProviderErrorCategory::InvalidRequest,
                        format!("required Bedrock tool '{name}' is not registered"),
                    )
                })?;
            Ok(serde_json::json!({"type": "tool", "name": bedrock_tool_name(&tool.name)}))
        }
        ToolChoice::None => Ok(serde_json::Value::Null),
    }
}

fn apply_anthropic_thinking_fields(
    body: &mut serde_json::Map<String, serde_json::Value>,
    params: &bcode_model::ModelParameters,
) {
    match params.reasoning_control {
        Some(bcode_model::ReasoningControl::Adaptive) => {
            body.insert(
                "thinking".to_string(),
                serde_json::json!({"type": "adaptive"}),
            );
            if let Some(effort) = adaptive_reasoning_effort(params) {
                body.insert(
                    "output_config".to_string(),
                    serde_json::json!({"effort": effort}),
                );
            }
        }
        Some(bcode_model::ReasoningControl::Budget) | None => {
            if let Some(budget) = resolve_reasoning_budget_tokens(params) {
                body.insert(
                    "thinking".to_string(),
                    serde_json::json!({"type": "enabled", "budget_tokens": budget}),
                );
            }
        }
    }
}

fn bedrock_request_projection(request: &ModelTurnRequest) -> ProviderRequestProjection {
    let messages_surface =
        request.provider_context.api_surface == Some(bcode_model::ModelApiSurface::Messages);
    let emitted_cache_points = if messages_surface {
        0
    } else {
        bedrock_emitted_cache_point_count(request)
    };
    let sent_messages = request
        .messages
        .iter()
        .filter(|message| message.role != MessageRole::System)
        .count();
    ProviderRequestProjection {
        provider: Some("bcode.bedrock".to_string()),
        api_shape: Some(
            if messages_surface {
                "bedrock_anthropic_messages"
            } else {
                "bedrock_converse"
            }
            .to_string(),
        ),
        message_count: Some(sent_messages),
        original_message_count: Some(request.messages.len()),
        sent_message_count: Some(sent_messages),
        omitted_message_count: Some(request.messages.len().saturating_sub(sent_messages)),
        cache_point_count: Some(prompt_cache_point_count(request)),
        emitted_cache_point_count: Some(emitted_cache_points),
        dropped_cache_point_count: Some(0),
        used_previous_response_id: false,
        ..ProviderRequestProjection::default()
    }
}

fn bedrock_emitted_cache_point_count(request: &ModelTurnRequest) -> usize {
    let system_prompt_cache_point = usize::from(
        request.prompt_cache.cache_system_prompt
            && request
                .system_prompt
                .as_ref()
                .is_some_and(|prompt| !prompt.trim().is_empty()),
    );
    let tool_cache_point =
        usize::from(request.prompt_cache.cache_tools && !request.tools.is_empty());
    system_prompt_cache_point + tool_cache_point + prompt_cache_point_count(request)
}

fn prompt_cache_point_count(request: &ModelTurnRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::CachePoint { .. }))
        .count()
}

fn build_converse_request(
    request: &ModelTurnRequest,
    model_id: String,
) -> Result<BedrockConverseRequest, ProviderError> {
    Ok(BedrockConverseRequest {
        model_id,
        messages: model_messages_to_bedrock_messages(request)?,
        system: system_blocks(request),
        tool_config: model_tools_to_bedrock_tool_config(request)?,
        inference_config: model_parameters_to_inference_config(request),
        additional_model_request_fields: bedrock_thinking_fields(&request.parameters),
    })
}

/// Default Anthropic extended-thinking token budgets per Bcode reasoning-effort level.
const REASONING_EFFORT_LOW_BUDGET: u32 = 1_024;
const REASONING_EFFORT_MEDIUM_BUDGET: u32 = 4_096;
const REASONING_EFFORT_HIGH_BUDGET: u32 = 16_384;

/// Resolve the Anthropic extended-thinking token budget requested for this turn.
///
/// Prefers an explicit `reasoning_budget_tokens`, otherwise maps a named
/// `reasoning_effort`/`reasoning_effort_value` onto a portable default budget. Returns `None`
/// when the request does not ask for reasoning.
fn resolve_reasoning_budget_tokens(params: &bcode_model::ModelParameters) -> Option<u32> {
    if let Some(budget) = params.reasoning_budget_tokens
        && budget > 0
    {
        return Some(budget);
    }
    let effort = params.reasoning_effort.map(|effort| match effort {
        bcode_model::ReasoningEffort::Low => REASONING_EFFORT_LOW_BUDGET,
        bcode_model::ReasoningEffort::Medium => REASONING_EFFORT_MEDIUM_BUDGET,
        bcode_model::ReasoningEffort::High => REASONING_EFFORT_HIGH_BUDGET,
    });
    if let Some(budget) = effort {
        return Some(budget);
    }
    match params.reasoning_effort_value.as_deref() {
        Some("low" | "minimal") => Some(REASONING_EFFORT_LOW_BUDGET),
        Some("medium") => Some(REASONING_EFFORT_MEDIUM_BUDGET),
        Some("high" | "xhigh" | "max") => Some(REASONING_EFFORT_HIGH_BUDGET),
        _ => None,
    }
}

/// Build the Bedrock `additionalModelRequestFields` document that enables Anthropic extended
/// thinking, or `None` when reasoning was not requested.
///
/// The Converse API forwards these fields to the model verbatim. The shape depends on the
/// normalized reasoning control resolved by the host:
///
/// * [`ReasoningControl::Budget`] (and unspecified, the historical default) sends
///   `{"thinking": {"type": "enabled", "budget_tokens": N}}`.
/// * [`ReasoningControl::Adaptive`] sends `{"thinking": {"type": "adaptive"}}` plus a sibling
///   `{"output_config": {"effort": "..."}}` object. Newer Claude models reject the budget shape,
///   and `effort` must not be nested inside `thinking`.
fn bedrock_thinking_fields(params: &bcode_model::ModelParameters) -> Option<Document> {
    if params.reasoning_control == Some(bcode_model::ReasoningControl::Adaptive) {
        return Some(bedrock_adaptive_thinking_fields(params));
    }
    let budget = resolve_reasoning_budget_tokens(params)?;
    let mut thinking = HashMap::new();
    thinking.insert("type".to_string(), Document::String("enabled".to_string()));
    thinking.insert(
        "budget_tokens".to_string(),
        Document::Number(Number::PosInt(u64::from(budget))),
    );
    let mut fields = HashMap::new();
    fields.insert("thinking".to_string(), Document::Object(thinking));
    Some(Document::Object(fields))
}

/// Build adaptive-thinking request fields for models that reject explicit thinking budgets.
///
/// Adaptive thinking is requested whenever the model uses it, even without a named effort, so the
/// model still allocates its own reasoning depth. A budget is never emitted; if only a budget was
/// requested it is dropped rather than translated, because the model chooses its own depth.
fn bedrock_adaptive_thinking_fields(params: &bcode_model::ModelParameters) -> Document {
    let mut thinking = HashMap::new();
    thinking.insert("type".to_string(), Document::String("adaptive".to_string()));
    let mut fields = HashMap::new();
    fields.insert("thinking".to_string(), Document::Object(thinking));
    if let Some(effort) = adaptive_reasoning_effort(params) {
        let mut output_config = HashMap::new();
        output_config.insert("effort".to_string(), Document::String(effort));
        fields.insert("output_config".to_string(), Document::Object(output_config));
    }
    Document::Object(fields)
}

/// Resolve the adaptive `output_config.effort` value requested for this turn.
///
/// Prefers the provider-native effort value advertised by the model catalog, falling back to the
/// portable [`bcode_model::ReasoningEffort`] level. Budget-token requests carry no effort name.
fn adaptive_reasoning_effort(params: &bcode_model::ModelParameters) -> Option<String> {
    if let Some(effort) = params
        .reasoning_effort_value
        .as_deref()
        .map(str::trim)
        .filter(|effort| !effort.is_empty())
    {
        return Some(effort.to_owned());
    }
    params.reasoning_effort.map(|effort| {
        match effort {
            bcode_model::ReasoningEffort::Low => "low",
            bcode_model::ReasoningEffort::Medium => "medium",
            bcode_model::ReasoningEffort::High => "high",
        }
        .to_owned()
    })
}

fn system_blocks(request: &ModelTurnRequest) -> Vec<SystemContentBlock> {
    let mut system = request
        .system_prompt
        .as_ref()
        .filter(|prompt| !prompt.trim().is_empty())
        .map(|prompt| vec![SystemContentBlock::Text(prompt.clone())])
        .unwrap_or_default();
    if request.prompt_cache.cache_system_prompt && !system.is_empty() {
        system.push(SystemContentBlock::CachePoint(default_cache_point()));
    }
    for message in &request.messages {
        if message.role == MessageRole::System {
            let text = joined_text_content(message);
            if !text.is_empty() {
                system.push(SystemContentBlock::Text(text));
            }
        }
    }
    system
}

fn model_messages_to_bedrock_messages(
    request: &ModelTurnRequest,
) -> Result<Vec<BedrockMessage>, ProviderError> {
    request
        .messages
        .iter()
        .filter(|message| message.role != MessageRole::System)
        .filter_map(model_message_to_bedrock_message)
        .collect()
}

fn model_message_to_bedrock_message(
    message: &ModelMessage,
) -> Option<Result<BedrockMessage, ProviderError>> {
    let role = match message.role {
        MessageRole::System => return None,
        MessageRole::User | MessageRole::Tool => ConversationRole::User,
        MessageRole::Assistant => ConversationRole::Assistant,
    };
    let content = match bedrock_content_blocks(message) {
        Ok(content) if content.is_empty() => return None,
        Ok(content) => content,
        Err(error) => return Some(Err(error)),
    };
    Some(
        BedrockMessage::builder()
            .role(role)
            .set_content(Some(content))
            .build()
            .map_err(|error| build_error(&error)),
    )
}

fn bedrock_content_blocks(
    message: &ModelMessage,
) -> Result<Vec<BedrockContentBlock>, ProviderError> {
    let mut blocks = Vec::new();
    let text = joined_text_content(message);
    if !text.is_empty() {
        blocks.push(BedrockContentBlock::Text(text));
    }
    for image in message_image_blocks(message) {
        blocks.push(BedrockContentBlock::Image(bedrock_image_block(image)?));
    }
    for block in &message.content {
        match block {
            ContentBlock::ToolCall { call } => {
                blocks.push(BedrockContentBlock::ToolUse(
                    ToolUseBlock::builder()
                        .tool_use_id(call.id.clone())
                        .name(bedrock_tool_name(&call.name))
                        .input(json_value_to_document(&call.arguments))
                        .build()
                        .map_err(|error| build_error(&error))?,
                ));
            }
            ContentBlock::ToolResult { result } => {
                let mut builder = ToolResultBlock::builder()
                    .tool_use_id(result.call_id.clone())
                    .content(ToolResultContentBlock::Text(result.output.clone()));
                for content in &result.content {
                    match content {
                        bcode_model::ToolResultContent::Image { image } => {
                            builder = builder.content(ToolResultContentBlock::Image(
                                bedrock_image_block(image)?,
                            ));
                        }
                        bcode_model::ToolResultContent::ImageRef { image } => {
                            builder = builder.content(ToolResultContentBlock::Text(format!(
                                "[image reference: {} {}{}{}]",
                                image.path,
                                image.mime_type,
                                image
                                    .metadata
                                    .width
                                    .zip(image.metadata.height)
                                    .map_or_else(String::new, |(width, height)| format!(
                                        " {width}x{height}"
                                    )),
                                image
                                    .metadata
                                    .byte_len
                                    .map_or_else(String::new, |byte_len| format!(
                                        " {byte_len} bytes"
                                    ))
                            )));
                        }
                        bcode_model::ToolResultContent::Text { text } => {
                            builder = builder.content(ToolResultContentBlock::Text(text.clone()));
                        }
                    }
                }
                if result.is_error {
                    builder = builder.status(ToolResultStatus::Error);
                }
                blocks.push(BedrockContentBlock::ToolResult(
                    builder.build().map_err(|error| build_error(&error))?,
                ));
            }
            ContentBlock::CachePoint { .. } => {
                blocks.push(BedrockContentBlock::CachePoint(default_cache_point()));
            }
            ContentBlock::Text { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::ProviderExtension { .. } => {}
        }
    }
    Ok(blocks)
}

fn message_image_blocks(
    message: &ModelMessage,
) -> impl Iterator<Item = &bcode_model::ImageContent> {
    message.content.iter().filter_map(|block| match block {
        ContentBlock::Image { image } => Some(image),
        _ => None,
    })
}

fn bedrock_image_block(image: &bcode_model::ImageContent) -> Result<ImageBlock, ProviderError> {
    let format = bedrock_image_format(&image.mime_type).ok_or_else(|| {
        provider_error(
            "bedrock_unsupported_image_mime_type",
            ProviderErrorCategory::UnsupportedFeature,
            format!("unsupported Bedrock image MIME type: {}", image.mime_type),
        )
    })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data_base64)
        .map_err(|error| {
            provider_error(
                "bedrock_invalid_image_data",
                ProviderErrorCategory::InvalidRequest,
                format!("invalid image data: {error}"),
            )
        })?;
    ImageBlock::builder()
        .format(format)
        .source(ImageSource::Bytes(Blob::new(bytes)))
        .build()
        .map_err(|error| build_error(&error))
}

fn bedrock_image_format(mime_type: &str) -> Option<ImageFormat> {
    match mime_type {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::Webp),
        _ => None,
    }
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

fn model_tools_to_bedrock_tool_config(
    request: &ModelTurnRequest,
) -> Result<Option<ToolConfiguration>, ProviderError> {
    if matches!(request.tool_call_policy.choice, ToolChoice::None) {
        return Ok(None);
    }
    if request.tools.is_empty() {
        return match &request.tool_call_policy.choice {
            ToolChoice::Auto | ToolChoice::None => Ok(None),
            ToolChoice::Required | ToolChoice::Tool { .. } => Err(provider_error(
                "tool_choice_without_tools",
                ProviderErrorCategory::InvalidRequest,
                "Bedrock tool choice requires at least one registered tool",
            )),
        };
    }
    let tool_choice = bedrock_tool_choice(request)?;
    let mut tools = request
        .tools
        .iter()
        .map(|tool| {
            ToolSpecification::builder()
                .name(bedrock_tool_name(&tool.name))
                .description(tool.description.clone())
                .input_schema(ToolInputSchema::Json(json_value_to_document(
                    &tool.input_schema,
                )))
                .build()
                .map(Tool::ToolSpec)
                .map_err(|error| build_error(&error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if request.prompt_cache.cache_tools {
        tools.push(Tool::CachePoint(default_cache_point()));
    }
    ToolConfiguration::builder()
        .set_tools(Some(tools))
        .set_tool_choice(tool_choice)
        .build()
        .map(Some)
        .map_err(|error| build_error(&error))
}

fn bedrock_tool_choice(
    request: &ModelTurnRequest,
) -> Result<Option<BedrockToolChoice>, ProviderError> {
    match &request.tool_call_policy.choice {
        ToolChoice::Auto => Ok(Some(BedrockToolChoice::Auto(
            AutoToolChoice::builder().build(),
        ))),
        ToolChoice::None => Ok(None),
        ToolChoice::Required => Ok(Some(BedrockToolChoice::Any(
            AnyToolChoice::builder().build(),
        ))),
        ToolChoice::Tool { name } => {
            let tool = request
                .tools
                .iter()
                .find(|tool| tool.name == *name)
                .ok_or_else(|| {
                    provider_error(
                        "unknown_required_tool",
                        ProviderErrorCategory::InvalidRequest,
                        format!("required Bedrock tool '{name}' is not registered"),
                    )
                })?;
            SpecificToolChoice::builder()
                .name(bedrock_tool_name(&tool.name))
                .build()
                .map(BedrockToolChoice::Tool)
                .map(Some)
                .map_err(|error| build_error(&error))
        }
    }
}

fn default_cache_point() -> CachePointBlock {
    CachePointBlock::builder()
        .r#type(CachePointType::Default)
        .build()
        .expect("default cache point should build")
}

fn bedrock_tool_name_map(tools: &[ToolDefinition]) -> BTreeMap<String, String> {
    tools
        .iter()
        .map(|tool| (bedrock_tool_name(&tool.name), tool.name.clone()))
        .collect()
}

fn original_tool_name(name: &str, name_map: &BTreeMap<String, String>) -> String {
    name_map
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

fn bedrock_tool_name(name: &str) -> String {
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

fn model_parameters_to_inference_config(
    request: &ModelTurnRequest,
) -> Option<InferenceConfiguration> {
    let params = &request.parameters;
    if params.temperature.is_none()
        && params.top_p.is_none()
        && params.max_output_tokens.is_none()
        && params.stop_sequences.is_empty()
    {
        return None;
    }
    let mut builder = InferenceConfiguration::builder()
        .set_temperature(params.temperature)
        .set_top_p(params.top_p)
        .set_stop_sequences(
            (!params.stop_sequences.is_empty()).then(|| params.stop_sequences.clone()),
        );
    if let Some(max_tokens) = params
        .max_output_tokens
        .and_then(|tokens| i32::try_from(tokens).ok())
    {
        builder = builder.max_tokens(max_tokens);
    }
    Some(builder.build())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BedrockTransport {
    Runtime,
    MantleAnthropic,
    MantleOpenAi,
}

impl BedrockTransport {
    fn parse(value: Option<&str>) -> Result<Self, ProviderError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("bedrock_runtime" | "runtime") => Ok(Self::Runtime),
            Some("mantle_anthropic" | "mantle") => Ok(Self::MantleAnthropic),
            Some("mantle_openai") => Ok(Self::MantleOpenAi),
            Some(value) => Err(provider_error(
                "bedrock_transport_invalid",
                ProviderErrorCategory::Config,
                format!(
                    "unsupported Bedrock transport '{value}'; expected 'bedrock_runtime', 'mantle_anthropic', or 'mantle_openai'"
                ),
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "bedrock_runtime",
            Self::MantleAnthropic => "mantle_anthropic",
            Self::MantleOpenAi => "mantle_openai",
        }
    }

    /// Whether this transport talks to the Mantle endpoint rather than Bedrock Runtime.
    ///
    /// Mantle has no Bedrock control-plane discovery API, so configured model membership is
    /// authoritative for every Mantle flavor.
    const fn is_mantle(self) -> bool {
        matches!(self, Self::MantleAnthropic | Self::MantleOpenAi)
    }
}

#[derive(Debug, Clone)]
struct Settings {
    transport: Result<BedrockTransport, ProviderError>,
    mantle_base_url: Option<String>,
    mantle_auth_header: bool,
    force_http1: bool,
    default_model: Option<String>,
    model_ids: Vec<String>,
    model_ids_are_explicit: bool,
    region: Option<String>,
    region_source: RegionSource,
    aws_profile: Option<String>,
    endpoint_url: Option<String>,
    auth_credentials: BTreeMap<String, String>,
    env: BTreeMap<String, String>,
    config_source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionSource {
    BcodeEnv,
    AwsEnv,
    Profile,
    AwsSdkDefaultChain,
    Fallback,
}

impl RegionSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::BcodeEnv => "bcode_env",
            Self::AwsEnv => "aws_env",
            Self::Profile => "profile",
            Self::AwsSdkDefaultChain => "aws_sdk_default_chain",
            Self::Fallback => "fallback",
        }
    }
}

impl Settings {
    /// Resolve the Mantle API flavor implied by the configured transport.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured transport is invalid, or when it does not target
    /// Mantle at all.
    fn mantle_flavor(&self) -> Result<MantleFlavor, ProviderError> {
        match self.transport.clone()? {
            BedrockTransport::MantleAnthropic => Ok(MantleFlavor::Anthropic),
            BedrockTransport::MantleOpenAi => Ok(MantleFlavor::OpenAi),
            BedrockTransport::Runtime => Err(provider_error(
                "bedrock_mantle_transport_required",
                ProviderErrorCategory::Config,
                "Bedrock Runtime transport does not use a Mantle endpoint",
            )),
        }
    }

    fn resolve_from_context(context: &ProviderRequestContext) -> Self {
        Self::resolve_context(Some(context))
    }

    fn resolve(request: Option<&ModelTurnRequest>) -> Self {
        Self::resolve_context(request.map(|request| &request.provider_context))
    }

    #[allow(clippy::too_many_lines)]
    fn resolve_context(request_context: Option<&ProviderRequestContext>) -> Self {
        let config = bcode_config::load_config().ok();
        let resolved = config
            .as_ref()
            .map(bcode_config::BcodeConfig::resolved_model_selection);
        let request_settings = request_context
            .map(|context| context.settings.clone())
            .unwrap_or_default();
        let request_env = request_context
            .map(|context| context.env.clone())
            .unwrap_or_default();
        let request_auth = request_context.and_then(|context| context.auth.as_ref());
        let request_auth_attributes = request_auth
            .map(|auth| auth.attributes.clone())
            .unwrap_or_default();
        let request_auth_credentials = request_auth
            .map(|auth| {
                auth.credentials
                    .iter()
                    .map(|(key, credential)| (key.clone(), credential.value.clone()))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let profile_settings = resolved
            .as_ref()
            .map(|selection| selection.settings.clone())
            .unwrap_or_default();
        let auth_settings = config
            .as_ref()
            .and_then(|config| {
                resolved
                    .as_ref()
                    .and_then(|selection| selection.auth_profile.as_ref())
                    .and_then(|auth_profile| config.auth.profiles.get(auth_profile))
            })
            .map(|auth| auth.settings.clone())
            .unwrap_or_default();
        let value = |keys: &[&str]| {
            first_nonempty(
                keys.iter()
                    .filter_map(|key| request_settings.get(*key).cloned()),
            )
            .or_else(|| {
                first_nonempty(
                    keys.iter()
                        .filter_map(|key| request_auth_attributes.get(*key).cloned()),
                )
            })
            .or_else(|| {
                first_nonempty(
                    keys.iter()
                        .filter_map(|key| profile_settings.get(*key).cloned()),
                )
            })
            .or_else(|| {
                first_nonempty(
                    keys.iter()
                        .filter_map(|key| auth_settings.get(*key).cloned()),
                )
            })
        };
        let first_context_env = |keys: &[&str]| {
            first_nonempty(keys.iter().filter_map(|key| request_env.get(*key).cloned()))
                .or_else(|| first_nonempty(keys.iter().filter_map(|key| std::env::var(key).ok())))
        };
        let default_model = first_context_env(&["BCODE_BEDROCK_MODEL", "BEDROCK_MODEL"])
            .or_else(|| value(&["model", "model_id"]))
            .or_else(|| resolved.and_then(|selection| selection.model_id));
        let model_ids_value = first_context_env(&["BCODE_BEDROCK_MODELS", "BEDROCK_MODELS"])
            .or_else(|| value(&["models", "model_ids"]));
        let mut model_ids = model_ids_value
            .as_deref()
            .map_or_else(Vec::new, parse_model_list);
        if let Some(default_model) = &default_model
            && !model_ids.contains(default_model)
        {
            model_ids.insert(0, default_model.clone());
        }
        let (region, region_source) = resolve_configured_region(&value, &first_context_env);
        let transport_value =
            first_context_env(&["BCODE_BEDROCK_TRANSPORT"]).or_else(|| value(&["transport"]));
        let transport = BedrockTransport::parse(transport_value.as_deref());
        let mantle_base_url = first_context_env(&["BCODE_BEDROCK_MANTLE_BASE_URL"])
            .or_else(|| value(&["mantle_base_url"]));
        let mantle_auth_header = first_context_env(&["BCODE_BEDROCK_MANTLE_AUTH_HEADER"])
            .or_else(|| value(&["mantle_auth_header"]))
            .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"));
        let force_http1 =
            first_context_env(&["BCODE_BEDROCK_FORCE_HTTP1", "AWS_BEDROCK_FORCE_HTTP1"])
                .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"));
        Self {
            transport,
            mantle_base_url,
            mantle_auth_header,
            force_http1,
            default_model,
            model_ids,
            model_ids_are_explicit: model_ids_value.is_some(),
            region,
            region_source,
            aws_profile: first_context_env(&["BCODE_BEDROCK_AWS_PROFILE", "AWS_PROFILE"])
                .or_else(|| value(&["profile", "aws_profile"])),
            endpoint_url: first_context_env(&[
                "BCODE_BEDROCK_ENDPOINT_URL",
                "BEDROCK_ENDPOINT_URL",
            ])
            .or_else(|| value(&["endpoint_url"])),
            auth_credentials: request_auth_credentials,
            env: request_env,
            config_source: if request_context.is_some() {
                "request/config/environment".to_string()
            } else {
                "config/environment".to_string()
            },
        }
    }
}

fn resolve_configured_region(
    value: &impl Fn(&[&str]) -> Option<String>,
    first_context_env: &impl Fn(&[&str]) -> Option<String>,
) -> (Option<String>, RegionSource) {
    if let Some(region) = first_context_env(&["BCODE_BEDROCK_REGION"]) {
        return (Some(region), RegionSource::BcodeEnv);
    }
    if let Some(region) = first_context_env(&["AWS_REGION", "AWS_DEFAULT_REGION"]) {
        return (Some(region), RegionSource::AwsEnv);
    }
    if let Some(region) = value(&["region"]) {
        return (Some(region), RegionSource::Profile);
    }
    (None, RegionSource::AwsSdkDefaultChain)
}

fn bedrock_feature_support() -> bcode_model::ModelFeatureSupport {
    use bcode_model::{
        CapabilitySource, CapabilitySupport, MediaInputFeature, ModelFeatureSupport,
        ModelParameterKey, PromptCacheFeature, StructuredOutputMode, ToolChoiceMode,
    };
    let supported = || CapabilitySupport::Supported {
        source: CapabilitySource::BundledCatalog,
    };
    let unsupported = |reason: &str| CapabilitySupport::Unsupported {
        source: CapabilitySource::BundledCatalog,
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
        ]
        .into_iter()
        .map(|key| (key, supported()))
        .chain(std::iter::once((
            ModelParameterKey::ReasoningSummary,
            unsupported(
                "Bedrock Anthropic extended thinking has no provider-visible reasoning summary",
            ),
        )))
        .collect(),
        structured_output: [
            StructuredOutputMode::JsonSchema,
            StructuredOutputMode::StrictJsonSchema,
        ]
        .into_iter()
        .map(|mode| {
            (
                mode,
                unsupported("Bedrock Converse structured output is not implemented"),
            )
        })
        .collect(),
        tool_choice: [
            (ToolChoiceMode::Auto, supported()),
            (ToolChoiceMode::None, supported()),
            (ToolChoiceMode::Required, supported()),
            (ToolChoiceMode::Named, supported()),
            (
                ToolChoiceMode::Parallel,
                unsupported("Bedrock model-specific parallel support is not guaranteed"),
            ),
        ]
        .into_iter()
        .collect(),
        prompt_cache: [
            PromptCacheFeature::ConversationPrefix,
            PromptCacheFeature::ExplicitSystem,
            PromptCacheFeature::ExplicitTools,
            PromptCacheFeature::ExplicitMessage,
        ]
        .into_iter()
        .map(|feature| (feature, supported()))
        .chain(std::iter::once((
            PromptCacheFeature::Ttl,
            unsupported("Bedrock cache points do not accept a portable TTL"),
        )))
        .collect(),
        media_input: [
            (MediaInputFeature::UserImage, supported()),
            (
                MediaInputFeature::SystemImage,
                unsupported("Bedrock Converse system content does not accept images"),
            ),
            (
                MediaInputFeature::AssistantImage,
                unsupported("Bedrock Converse assistant messages do not accept images"),
            ),
            (
                MediaInputFeature::ToolMessageImage,
                unsupported("use structured tool-result image content instead"),
            ),
            (MediaInputFeature::ToolResultImage, supported()),
            (
                MediaInputFeature::ImageReference,
                unsupported("Bedrock requires inline image bytes"),
            ),
        ]
        .into_iter()
        .collect(),
    }
}

fn capabilities() -> ProviderCapabilities {
    let settings = Settings::resolve(None);
    ProviderCapabilities {
        provider_id: PROVIDER_ID.to_string(),
        display_name: "Amazon Bedrock".to_string(),
        capabilities: [
            ProviderCapability::Streaming,
            ProviderCapability::Cancellation,
            ProviderCapability::Tools,
            ProviderCapability::ParallelToolCalls,
            ProviderCapability::PromptCaching,
        ]
        .into_iter()
        .collect(),
        feature_support: bedrock_feature_support(),
        auth_schemes: [
            "aws_default_chain".to_string(),
            "aws_credentials".to_string(),
        ]
        .into_iter()
        .collect(),
        retry_rules: Vec::new(),
        metadata: diagnostics_metadata(&settings),
    }
}

impl BedrockProviderPlugin {
    fn models(&self, request: &ModelListRequest) -> ModelList {
        let settings = Settings::resolve_from_context(&request.provider_context);
        // Mantle has no Bedrock control-plane discovery API. Its configured model membership is
        // authoritative and the central catalog enriches those IDs with capabilities and limits.
        if settings
            .transport
            .as_ref()
            .is_ok_and(|transport| transport.is_mantle())
        {
            let model_ids = if settings.model_ids.is_empty() {
                settings.default_model.iter().cloned().collect::<Vec<_>>()
            } else {
                settings.model_ids.clone()
            };
            return ModelList {
                models: model_infos_from_ids(&model_ids, settings.default_model.as_deref()),
                catalog: ModelCatalogHints {
                    policy: bcode_model::ModelCatalogPolicy::EnrichOnly {
                        provider_id: "bedrock".to_string(),
                        target: None,
                        authority: bcode_model::ModelListAuthority::Explicit,
                    },
                },
            };
        }
        if settings.model_ids_are_explicit {
            return ModelList {
                models: model_infos_from_ids(
                    &settings.model_ids,
                    settings.default_model.as_deref(),
                ),
                catalog: ModelCatalogHints {
                    policy: bcode_model::ModelCatalogPolicy::EnrichOnly {
                        provider_id: "bedrock".to_string(),
                        target: None,
                        authority: bcode_model::ModelListAuthority::Explicit,
                    },
                },
            };
        }
        let discovered = match self.runtime.as_ref() {
            Ok(runtime) => discovery_for_picker_nonblocking(runtime, &self.discovery, &settings),
            Err(error) => {
                tracing::warn!(
                    target: "bcode_bedrock::discovery",
                    error = %error,
                    "Bedrock model discovery runtime unavailable"
                );
                ModelDiscovery::default()
            }
        };
        let mut models = discovered.models;
        apply_default_model_to_list(&mut models, settings.default_model.as_deref());
        ModelList {
            models,
            catalog: ModelCatalogHints {
                policy: bcode_model::ModelCatalogPolicy::EnrichOnly {
                    provider_id: "bedrock".to_string(),
                    target: None,
                    authority: bcode_model::ModelListAuthority::Authoritative,
                },
            },
        }
    }

    fn validate_config_response(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<bcode_model::ValidateConfigRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        json_response(&self.validate_config(&request.provider_context))
    }

    fn validate_config(&self, provider_context: &ProviderRequestContext) -> ValidateConfigResponse {
        let settings = Settings::resolve_from_context(provider_context);
        let mut validation = settings.transport.clone().map(|_| ());
        if validation.is_ok()
            && settings
                .transport
                .as_ref()
                .is_ok_and(|transport| transport.is_mantle())
        {
            validation = validate_mantle_settings(&settings);
        }
        let mut metadata = diagnostics_metadata(&settings);
        let transport = settings.transport.as_ref().copied();
        let effective_region = validation.as_ref().ok().and_then(|()| {
            (transport == Ok(BedrockTransport::Runtime))
                .then(|| {
                    self.runtime
                        .as_ref()
                        .ok()
                        .and_then(|runtime| resolved_sdk_region(runtime, &settings))
                })
                .flatten()
        });
        if let Some((region, source)) = &effective_region {
            metadata.insert("effective_region".to_string(), region.clone());
            metadata.insert(
                "effective_region_source".to_string(),
                source.as_str().to_string(),
            );
        }
        if validation.is_ok()
            && settings
                .transport
                .as_ref()
                .is_ok_and(|transport| *transport == BedrockTransport::Runtime)
            && !settings.model_ids_are_explicit
            && settings.default_model.is_none()
        {
            match self
                .runtime
                .as_ref()
                .map_err(|error| {
                    provider_error(
                        "runtime_unavailable",
                        ProviderErrorCategory::ProviderInternal,
                        error.clone(),
                    )
                })
                .and_then(|runtime| {
                    get_or_refresh_discovery_sync(runtime, &self.discovery, &settings)
                }) {
                Ok(discovery) => {
                    metadata.insert(
                        "discovered_model_count".to_string(),
                        discovery.models.len().to_string(),
                    );
                    if let Some(model_id) = discovery.default_model_id {
                        metadata.insert("discovered_default_model".to_string(), model_id);
                    }
                }
                Err(error) => {
                    metadata.insert("model_discovery_error".to_string(), error.message.clone());
                    if matches!(
                        error.category,
                        ProviderErrorCategory::Auth | ProviderErrorCategory::Config
                    ) {
                        validation = Err(error);
                    }
                }
            }
        }
        let failures = validation
            .as_ref()
            .err()
            .and_then(|error| error.failure.as_deref())
            .cloned()
            .into_iter()
            .collect();
        ValidateConfigResponse {
            valid: validation.is_ok(),
            message: Some(match &validation {
                Ok(()) => effective_region.map_or_else(
                    || format!(
                        "Bedrock configuration is usable; region will fall back to '{DEFAULT_REGION}' if the AWS SDK chain is empty and credentials will be resolved at request time"
                    ),
                    |(region, source)| format!(
                        "Bedrock configuration is usable; region '{region}' resolved from {} and credentials will be resolved at request time",
                        source.as_str()
                    ),
                ),
                Err(error) => format!("Bedrock configuration is not usable: {}", error.message),
            }),
            failures,
            metadata,
        }
    }
}

fn model_list_request(request: &ServiceRequest) -> ModelListRequest {
    request
        .payload_json::<ModelListRequest>()
        .unwrap_or_default()
}

fn bedrock_model_capabilities() -> BTreeSet<ModelCapability> {
    [
        ModelCapability::StreamingText,
        ModelCapability::ToolCalls,
        ModelCapability::PromptCaching,
    ]
    .into_iter()
    .collect()
}

fn model_infos_from_ids(model_ids: &[String], default_model: Option<&str>) -> Vec<ModelInfo> {
    model_ids
        .iter()
        .map(|model_id| ModelInfo {
            model_id: model_id.clone(),
            display_name: model_id.clone(),
            is_default: default_model == Some(model_id.as_str()),
            context_window: None,
            max_output_tokens: None,
            capabilities: bedrock_model_capabilities(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: bedrock_model_cache_info(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        })
        .collect::<Vec<_>>()
}

/// Mark the selected/configured model as the default within a discovered model list.
///
/// The full discovered list is preserved. When `default_model` is set but discovery did not
/// surface it (for example a pinned version ID that is not an ACTIVE inference profile, or a
/// failed discovery), it is prepended so it stays selectable rather than replacing the list. When
/// `default_model` is `None`, the list's existing default (chosen by discovery) is left untouched.
fn apply_default_model_to_list(models: &mut Vec<ModelInfo>, default_model: Option<&str>) {
    let Some(default_model) = default_model.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    if !models.iter().any(|model| model.model_id == default_model) {
        models.insert(
            0,
            model_infos_from_ids(&[default_model.to_string()], Some(default_model)).remove(0),
        );
    }
    for model in models.iter_mut() {
        model.is_default = model.model_id == default_model;
    }
}

fn bedrock_model_cache_info() -> bcode_model::ModelCacheInfo {
    bcode_model::ModelCacheInfo {
        capabilities: std::collections::BTreeSet::from([
            bcode_model::ModelCacheCapability::ExplicitCachePoints,
            bcode_model::ModelCacheCapability::CacheUsageReporting,
        ]),
    }
}

#[derive(Debug, Clone)]
struct ModelSelection {
    model_ids: Vec<String>,
    explicit: bool,
    cache_key: Option<DiscoveryCacheKey>,
}

async fn resolve_turn_model_selection(
    request: &ModelTurnRequest,
    settings: &Settings,
    turn: &TurnState,
    cache: &Arc<Mutex<DiscoveryCache>>,
) -> Result<ModelSelection, ProviderError> {
    if !request.model_id.trim().is_empty() {
        return Ok(ModelSelection {
            model_ids: vec![request.model_id.clone()],
            explicit: true,
            cache_key: None,
        });
    }
    if let Some(model_id) = &settings.default_model
        && !model_id.trim().is_empty()
    {
        return Ok(ModelSelection {
            model_ids: vec![model_id.clone()],
            explicit: true,
            cache_key: None,
        });
    }
    if settings
        .transport
        .as_ref()
        .is_ok_and(|transport| transport.is_mantle())
    {
        return Err(provider_error(
            "bedrock_mantle_model_required",
            ProviderErrorCategory::Config,
            "Bedrock Mantle requires a configured model",
        ));
    }
    let key = discovery_cache_key(settings).await;
    let discovery = if let Some(discovery) = cached_discovery(cache, &key) {
        discovery
    } else {
        turn.push(ProviderTurnEvent::Warning {
            message: "discovering available Bedrock models".to_string(),
        });
        let discovery = discover_models(settings).await?;
        store_discovery(cache, key.clone(), discovery.clone());
        discovery
    };
    let model_ids = discovery
        .models
        .iter()
        .map(|model| model.model_id.clone())
        .collect::<Vec<_>>();
    if model_ids.is_empty() {
        return Err(provider_error(
            "bedrock_model_discovery_empty",
            ProviderErrorCategory::Config,
            "Bedrock model discovery returned no usable text/streaming models; set BCODE_BEDROCK_MODEL or configure a Bedrock model profile",
        )
        .with_failure(bedrock_failure_context(
            bcode_model::ProviderFailureSourceKind::ModelProfile,
            "BCODE_BEDROCK_MODEL or model profile",
            bcode_model::ProviderFailureCapability::ModelDiscovery,
            "set BCODE_BEDROCK_MODEL or configure an accessible streaming Bedrock model",
        )));
    }
    Ok(ModelSelection {
        model_ids,
        explicit: false,
        cache_key: Some(key),
    })
}

#[derive(Debug, Clone, Default)]
struct ModelDiscovery {
    models: Vec<ModelInfo>,
    default_model_id: Option<String>,
}

#[derive(Debug, Default)]
struct DiscoveryCache {
    entries: BTreeMap<DiscoveryCacheKey, CachedDiscovery>,
    compatibility: PersistedCompatibilityCache,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct DiscoveryCacheKey {
    region: String,
    aws_profile: Option<String>,
    endpoint_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedCompatibilityCache {
    version: u8,
    entries: Vec<PersistedCompatibilityEntry>,
}

impl Default for PersistedCompatibilityCache {
    fn default() -> Self {
        Self {
            version: COMPATIBILITY_CACHE_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedCompatibilityEntry {
    key: DiscoveryCacheKey,
    #[serde(default)]
    unsupported_streaming_tool_models: BTreeMap<String, PersistedModelIncompatibility>,
    #[serde(default)]
    unsupported_prompt_cache_models: BTreeMap<String, PersistedModelIncompatibility>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedModelIncompatibility {
    reason: String,
    message: String,
    first_seen_unix_seconds: u64,
    last_seen_unix_seconds: u64,
}

#[derive(Debug, Clone)]
struct CachedDiscovery {
    discovered_at: Instant,
    discovery: ModelDiscovery,
    unsupported_streaming_tool_models: BTreeSet<String>,
    unsupported_prompt_cache_models: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct CandidateModel {
    model_id: String,
    display_name: String,
    /// Higher values are preferred. This is based on Bedrock resource shape, not model family.
    priority: i32,
    /// Service-provided recency timestamp when available.
    date_key: i64,
}

fn warm_discovery_cache(
    runtime: &ProviderRuntime,
    cache: Arc<Mutex<DiscoveryCache>>,
    settings: Settings,
) {
    // Mantle has no Bedrock control-plane discovery API; configured model membership is used.
    if settings.model_ids_are_explicit
        || settings
            .transport
            .as_ref()
            .is_ok_and(|transport| transport.is_mantle())
    {
        return;
    }
    runtime.spawn(async move {
        if let Err(error) = get_or_refresh_discovery(&cache, &settings).await {
            tracing::debug!(
                target: "bcode_bedrock::discovery",
                error = %error.message,
                "background Bedrock model discovery failed"
            );
        }
    });
}

fn get_or_refresh_discovery_sync(
    runtime: &ProviderRuntime,
    cache: &Arc<Mutex<DiscoveryCache>>,
    settings: &Settings,
) -> Result<ModelDiscovery, ProviderError> {
    let cache = Arc::clone(cache);
    let settings = settings.clone();
    runtime
        .block_on(async move { get_or_refresh_discovery(&cache, &settings).await })
        .map_err(|error| {
            provider_error(
                "runtime_unavailable",
                ProviderErrorCategory::ProviderInternal,
                error.to_string(),
            )
        })?
}

/// Return the discovered model list for the interactive picker without blocking on AWS.
///
/// The model picker is an interactive, bounded path, so it must not block on paginated Bedrock
/// API calls. This returns whatever is cached immediately (even if stale) and spawns a background
/// refresh when the cache is missing or expired. The first open before any cache is warmed
/// returns an empty live list (the host still enriches from the bundled catalog); subsequent opens
/// return the full discovered list once the background refresh completes.
fn discovery_for_picker_nonblocking(
    runtime: &ProviderRuntime,
    cache: &Arc<Mutex<DiscoveryCache>>,
    settings: &Settings,
) -> ModelDiscovery {
    let cache = Arc::clone(cache);
    let settings = settings.clone();
    let key = runtime.block_on({
        let settings = settings.clone();
        async move { discovery_cache_key(&settings).await }
    });
    if let Ok(key) = key {
        if let Some(discovery) = cached_discovery(&cache, &key) {
            return discovery;
        }
        if let Some(stale) = stale_cached_discovery(&cache, &key) {
            spawn_discovery_refresh(runtime, cache, settings, key);
            return stale;
        }
        spawn_discovery_refresh(runtime, cache, settings, key);
    }
    ModelDiscovery::default()
}

fn spawn_discovery_refresh(
    runtime: &ProviderRuntime,
    cache: Arc<Mutex<DiscoveryCache>>,
    settings: Settings,
    key: DiscoveryCacheKey,
) {
    runtime.spawn(async move {
        match discover_models(&settings).await {
            Ok(discovery) => store_discovery(&cache, key, discovery),
            Err(error) => tracing::debug!(
                target: "bcode_bedrock::discovery",
                error = %error.message,
                "background Bedrock model discovery refresh failed"
            ),
        }
    });
}

/// Return a cached discovery ignoring the freshness TTL, applying compatibility filtering.
fn stale_cached_discovery(
    cache: &Arc<Mutex<DiscoveryCache>>,
    key: &DiscoveryCacheKey,
) -> Option<ModelDiscovery> {
    let cached = cache.lock().ok()?.entries.get(key).cloned()?;
    Some(filtered_discovery(
        &cached.discovery,
        &cached.unsupported_streaming_tool_models,
        &cached.unsupported_prompt_cache_models,
    ))
}

async fn get_or_refresh_discovery(
    cache: &Arc<Mutex<DiscoveryCache>>,
    settings: &Settings,
) -> Result<ModelDiscovery, ProviderError> {
    let key = discovery_cache_key(settings).await;
    if let Some(discovery) = cached_discovery(cache, &key) {
        return Ok(discovery);
    }
    let discovery = discover_models(settings).await?;
    store_discovery(cache, key.clone(), discovery.clone());
    Ok(cached_discovery(cache, &key).unwrap_or(discovery))
}

fn cached_discovery(
    cache: &Arc<Mutex<DiscoveryCache>>,
    key: &DiscoveryCacheKey,
) -> Option<ModelDiscovery> {
    let cached = cache.lock().ok()?.entries.get(key).cloned()?;
    (cached.discovered_at.elapsed() < MODEL_DISCOVERY_TTL).then(|| {
        filtered_discovery(
            &cached.discovery,
            &cached.unsupported_streaming_tool_models,
            &cached.unsupported_prompt_cache_models,
        )
    })
}

fn filtered_discovery(
    discovery: &ModelDiscovery,
    unsupported_streaming_tool_models: &BTreeSet<String>,
    unsupported_prompt_cache_models: &BTreeSet<String>,
) -> ModelDiscovery {
    let models = discovery
        .models
        .iter()
        .filter(|model| !unsupported_streaming_tool_models.contains(&model.model_id))
        .cloned()
        .map(|mut model| {
            if unsupported_prompt_cache_models.contains(&model.model_id) {
                model.capabilities.remove(&ModelCapability::PromptCaching);
                model.cache.capabilities.clear();
                for feature in [
                    bcode_model::PromptCacheFeature::ConversationPrefix,
                    bcode_model::PromptCacheFeature::ExplicitSystem,
                    bcode_model::PromptCacheFeature::ExplicitTools,
                    bcode_model::PromptCacheFeature::ExplicitMessage,
                    bcode_model::PromptCacheFeature::Ttl,
                ] {
                    model.feature_support.prompt_cache.insert(
                        feature,
                        bcode_model::CapabilitySupport::Unsupported {
                            source: bcode_model::CapabilitySource::Probe,
                            reason: "Bedrock previously rejected prompt caching for this model"
                                .to_string(),
                        },
                    );
                }
            }
            model
        })
        .collect::<Vec<_>>();
    let default_model_id = models.first().map(|model| model.model_id.clone());
    ModelDiscovery {
        models,
        default_model_id,
    }
}

fn store_discovery(
    cache: &Arc<Mutex<DiscoveryCache>>,
    key: DiscoveryCacheKey,
    discovery: ModelDiscovery,
) {
    if let Ok(mut cache) = cache.lock() {
        let mut unsupported_streaming_tool_models = cache
            .entries
            .get(&key)
            .map(|cached| cached.unsupported_streaming_tool_models.clone())
            .unwrap_or_default();
        let mut unsupported_prompt_cache_models = cache
            .entries
            .get(&key)
            .map(|cached| cached.unsupported_prompt_cache_models.clone())
            .unwrap_or_default();
        unsupported_streaming_tool_models
            .extend(cache.compatibility.unsupported_streaming_for(&key));
        unsupported_prompt_cache_models
            .extend(cache.compatibility.unsupported_prompt_cache_for(&key));
        cache.entries.insert(
            key,
            CachedDiscovery {
                discovered_at: Instant::now(),
                discovery,
                unsupported_streaming_tool_models,
                unsupported_prompt_cache_models,
            },
        );
    }
}

fn mark_streaming_tool_unsupported(
    cache: &Arc<Mutex<DiscoveryCache>>,
    key: Option<&DiscoveryCacheKey>,
    model_id: &str,
    message: &str,
) {
    let Some(key) = key else {
        return;
    };
    let compatibility = cache.lock().ok().map(|mut cache| {
        if let Some(cached) = cache.entries.get_mut(key) {
            cached
                .unsupported_streaming_tool_models
                .insert(model_id.to_string());
        }
        cache.compatibility.mark_streaming_tool_unsupported(
            key,
            model_id,
            message,
            now_unix_seconds(),
        );
        cache.compatibility.clone()
    });
    if let Some(compatibility) = compatibility
        && let Err(error) = save_compatibility_cache(&compatibility)
    {
        tracing::warn!(
            target: "bcode_bedrock::compatibility",
            error = %error.message,
            "failed to save Bedrock compatibility cache"
        );
    }
}

fn streaming_tool_use_unsupported(error: &ProviderError) -> bool {
    error.category == ProviderErrorCategory::InvalidRequest
        && error
            .message
            .contains("doesn't support tool use in streaming mode")
}

/// Detect models that cannot be driven through the Converse API by this adapter.
///
/// The newest Anthropic models (for example the `global.` inference tier) are served through the
/// Bedrock Anthropic Messages API and reject Converse requests with a data-retention
/// `ValidationException`. Treat these as structurally unusable so they are pruned from discovery
/// like other adapter-incompatible models.
fn model_unusable_via_converse(error: &ProviderError) -> bool {
    error.category == ProviderErrorCategory::InvalidRequest
        && error
            .message
            .to_ascii_lowercase()
            .contains("data retention mode")
}

fn prompt_cache_rejected(error: &ProviderError) -> bool {
    error.category == ProviderErrorCategory::InvalidRequest
        && error.message.to_ascii_lowercase().contains("cache")
}

fn prompt_cache_known_unsupported(
    cache: &Arc<Mutex<DiscoveryCache>>,
    key: Option<&DiscoveryCacheKey>,
    model_id: &str,
) -> bool {
    let Some(key) = key else {
        return false;
    };
    cache
        .lock()
        .ok()
        .and_then(|cache| cache.entries.get(key).cloned())
        .is_some_and(|entry| entry.unsupported_prompt_cache_models.contains(model_id))
}

fn mark_prompt_cache_unsupported(
    cache: &Arc<Mutex<DiscoveryCache>>,
    key: Option<&DiscoveryCacheKey>,
    model_id: &str,
    message: &str,
) {
    let Some(key) = key else {
        return;
    };
    let compatibility = cache.lock().ok().map(|mut cache| {
        if let Some(cached) = cache.entries.get_mut(key) {
            cached
                .unsupported_prompt_cache_models
                .insert(model_id.to_string());
        }
        cache.compatibility.mark_prompt_cache_unsupported(
            key,
            model_id,
            message,
            now_unix_seconds(),
        );
        cache.compatibility.clone()
    });
    if let Some(compatibility) = compatibility
        && let Err(error) = save_compatibility_cache(&compatibility)
    {
        tracing::warn!(
            target: "bcode_bedrock::compatibility",
            error = %error.message,
            "failed to save Bedrock compatibility cache"
        );
    }
}

impl PersistedCompatibilityCache {
    fn unsupported_streaming_for(&self, key: &DiscoveryCacheKey) -> BTreeSet<String> {
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| {
                entry
                    .unsupported_streaming_tool_models
                    .keys()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn unsupported_prompt_cache_for(&self, key: &DiscoveryCacheKey) -> BTreeSet<String> {
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| {
                entry
                    .unsupported_prompt_cache_models
                    .keys()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn mark_streaming_tool_unsupported(
        &mut self,
        key: &DiscoveryCacheKey,
        model_id: &str,
        message: &str,
        now: u64,
    ) {
        self.mark_unsupported(
            key,
            model_id,
            STREAMING_TOOL_UNSUPPORTED_REASON,
            message,
            now,
            true,
        );
    }

    fn mark_prompt_cache_unsupported(
        &mut self,
        key: &DiscoveryCacheKey,
        model_id: &str,
        message: &str,
        now: u64,
    ) {
        self.mark_unsupported(
            key,
            model_id,
            PROMPT_CACHE_UNSUPPORTED_REASON,
            message,
            now,
            false,
        );
    }

    fn mark_unsupported(
        &mut self,
        key: &DiscoveryCacheKey,
        model_id: &str,
        reason: &str,
        message: &str,
        now: u64,
        streaming_tool: bool,
    ) {
        let entry = self.entry_mut(key.clone());
        let models = if streaming_tool {
            &mut entry.unsupported_streaming_tool_models
        } else {
            &mut entry.unsupported_prompt_cache_models
        };
        models
            .entry(model_id.to_string())
            .and_modify(|model| {
                model.message = message.to_string();
                model.last_seen_unix_seconds = now;
            })
            .or_insert_with(|| PersistedModelIncompatibility {
                reason: reason.to_string(),
                message: message.to_string(),
                first_seen_unix_seconds: now,
                last_seen_unix_seconds: now,
            });
    }

    fn prune_expired(&mut self, now: u64) {
        for entry in &mut self.entries {
            entry.unsupported_streaming_tool_models.retain(|_, model| {
                now.saturating_sub(model.last_seen_unix_seconds) <= COMPATIBILITY_CACHE_TTL_SECONDS
            });
            entry.unsupported_prompt_cache_models.retain(|_, model| {
                now.saturating_sub(model.last_seen_unix_seconds) <= COMPATIBILITY_CACHE_TTL_SECONDS
            });
        }
        self.entries.retain(|entry| {
            !entry.unsupported_streaming_tool_models.is_empty()
                || !entry.unsupported_prompt_cache_models.is_empty()
        });
    }

    fn entry_mut(&mut self, key: DiscoveryCacheKey) -> &mut PersistedCompatibilityEntry {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            return &mut self.entries[index];
        }
        self.entries.push(PersistedCompatibilityEntry {
            key,
            unsupported_streaming_tool_models: BTreeMap::new(),
            unsupported_prompt_cache_models: BTreeMap::new(),
        });
        self.entries.last_mut().expect("entry was just inserted")
    }
}

fn load_compatibility_cache() -> Result<PersistedCompatibilityCache, ProviderError> {
    load_compatibility_cache_from_path(&compatibility_cache_path())
}

fn load_compatibility_cache_from_path(
    path: &Path,
) -> Result<PersistedCompatibilityCache, ProviderError> {
    if !path.exists() {
        return Ok(PersistedCompatibilityCache::default());
    }
    let contents = std::fs::read_to_string(path).map_err(|error| {
        provider_error(
            "bedrock_compatibility_cache_read_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    let mut cache =
        serde_json::from_str::<PersistedCompatibilityCache>(&contents).map_err(|error| {
            provider_error(
                "bedrock_compatibility_cache_decode_failed",
                ProviderErrorCategory::ProviderInternal,
                error.to_string(),
            )
        })?;
    if cache.version != COMPATIBILITY_CACHE_VERSION {
        return Ok(PersistedCompatibilityCache::default());
    }
    cache.prune_expired(now_unix_seconds());
    Ok(cache)
}

fn save_compatibility_cache(cache: &PersistedCompatibilityCache) -> Result<(), ProviderError> {
    save_compatibility_cache_to_path(&compatibility_cache_path(), cache)
}

fn save_compatibility_cache_to_path(
    path: &Path,
    cache: &PersistedCompatibilityCache,
) -> Result<(), ProviderError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            provider_error(
                "bedrock_compatibility_cache_dir_failed",
                ProviderErrorCategory::ProviderInternal,
                error.to_string(),
            )
        })?;
    }
    let temp_path = path.with_extension("json.tmp");
    let contents = serde_json::to_vec_pretty(cache).map_err(|error| {
        provider_error(
            "bedrock_compatibility_cache_encode_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    std::fs::write(&temp_path, contents).map_err(|error| {
        provider_error(
            "bedrock_compatibility_cache_write_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        provider_error(
            "bedrock_compatibility_cache_rename_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    Ok(())
}

fn compatibility_cache_path() -> PathBuf {
    bcode_config::default_state_dir()
        .join("providers")
        .join("bedrock")
        .join("compatibility-cache-v1.json")
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

async fn discovery_cache_key(settings: &Settings) -> DiscoveryCacheKey {
    let config = bedrock_sdk_config(settings).await;
    DiscoveryCacheKey {
        region: config
            .region()
            .map_or_else(|| DEFAULT_REGION.to_string(), ToString::to_string),
        aws_profile: settings.aws_profile.clone(),
        endpoint_url: settings.endpoint_url.clone(),
    }
}

async fn discover_models(settings: &Settings) -> Result<ModelDiscovery, ProviderError> {
    let client = bedrock_control_client(settings).await;
    let mut candidates = BTreeMap::<String, CandidateModel>::new();
    for profile in discover_inference_profiles(&client).await? {
        candidates
            .entry(profile.model_id.clone())
            .or_insert(profile);
    }
    for model in discover_foundation_models(&client).await? {
        candidates.entry(model.model_id.clone()).or_insert(model);
    }
    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| right.date_key.cmp(&left.date_key))
            .then_with(|| left.model_id.cmp(&right.model_id))
    });
    let default_model_id = candidates
        .first()
        .map(|candidate| candidate.model_id.clone());
    let models: Vec<ModelInfo> = candidates
        .into_iter()
        .map(|candidate| ModelInfo {
            is_default: default_model_id.as_deref() == Some(candidate.model_id.as_str()),
            model_id: candidate.model_id,
            display_name: candidate.display_name,
            context_window: None,
            max_output_tokens: None,
            capabilities: bedrock_model_capabilities(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: bedrock_model_cache_info(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        })
        .collect();
    Ok(ModelDiscovery {
        models,
        default_model_id,
    })
}

async fn bedrock_control_client(settings: &Settings) -> bedrock::Client {
    let config = bedrock_sdk_config(settings).await;
    bedrock::Client::new(&config)
}

async fn discover_inference_profiles(
    client: &bedrock::Client,
) -> Result<Vec<CandidateModel>, ProviderError> {
    let mut next_token = None;
    let mut candidates = Vec::new();
    loop {
        let response = client
            .list_inference_profiles()
            .set_next_token(next_token)
            .send()
            .await
            .map_err(|error| bedrock_discovery_error(&error))?;
        for profile in response.inference_profile_summaries() {
            if profile.status().as_str() != "ACTIVE" {
                continue;
            }
            let model_id = profile.inference_profile_id().to_string();
            let display_name = profile.inference_profile_name().to_string();
            let date_key = profile
                .updated_at()
                .or_else(|| profile.created_at())
                .map_or(0, aws_smithy_types::DateTime::secs);
            candidates.push(CandidateModel {
                model_id,
                display_name,
                priority: 2,
                date_key,
            });
        }
        next_token = response.next_token().map(ToString::to_string);
        if next_token.is_none() {
            break;
        }
    }
    Ok(candidates)
}

async fn discover_foundation_models(
    client: &bedrock::Client,
) -> Result<Vec<CandidateModel>, ProviderError> {
    let response = client
        .list_foundation_models()
        .send()
        .await
        .map_err(|error| bedrock_discovery_error(&error))?;
    let mut candidates = Vec::new();
    for model in response.model_summaries() {
        let supports_text_output = model
            .output_modalities()
            .iter()
            .any(|modality| modality.as_str() == "TEXT");
        if !supports_text_output || model.response_streaming_supported() != Some(true) {
            continue;
        }
        let legacy = model
            .model_lifecycle()
            .is_some_and(|lifecycle| lifecycle.status().as_str() == "LEGACY");
        if legacy {
            continue;
        }
        let model_id = model.model_id().to_string();
        let display_name = model
            .model_name()
            .map_or_else(|| model_id.clone(), ToString::to_string);
        let date_key = model
            .model_lifecycle()
            .and_then(|lifecycle| lifecycle.start_of_life_time())
            .map_or(0, aws_smithy_types::DateTime::secs);
        candidates.push(CandidateModel {
            model_id,
            display_name,
            priority: 1,
            date_key,
        });
    }
    Ok(candidates)
}

fn resolved_sdk_region(
    runtime: &ProviderRuntime,
    settings: &Settings,
) -> Option<(String, RegionSource)> {
    let settings_for_config = settings.clone();
    let config = runtime
        .block_on(async move { bedrock_sdk_config(&settings_for_config).await })
        .ok()?;
    let region = config.region().map(ToString::to_string)?;
    let source = if settings.region.is_some() {
        settings.region_source
    } else if region == DEFAULT_REGION {
        RegionSource::Fallback
    } else {
        RegionSource::AwsSdkDefaultChain
    };
    Some((region, source))
}

fn validate_mantle_settings(settings: &Settings) -> Result<(), ProviderError> {
    let model_configured = settings
        .default_model
        .as_ref()
        .is_some_and(|model| !model.trim().is_empty())
        || settings
            .model_ids
            .iter()
            .any(|model| !model.trim().is_empty());
    if !model_configured {
        return Err(provider_error(
            "bedrock_mantle_model_required",
            ProviderErrorCategory::Config,
            "Bedrock Mantle requires a configured model because control-plane discovery is unavailable",
        ));
    }
    if client_context_bearer_token(settings).is_none() {
        return Err(provider_error(
            "bedrock_mantle_missing_bearer_token",
            ProviderErrorCategory::Auth,
            "Bedrock Mantle requires AWS_BEARER_TOKEN_BEDROCK or a mapped bearer_token credential",
        ));
    }
    // Validate the endpoint for the flavor actually configured, so a bad base URL is reported
    // against the surface the turn will use.
    mantle_endpoint(settings, settings.mantle_flavor()?).map(|_| ())
}

fn diagnostics_metadata(settings: &Settings) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("provider".to_string(), PROVIDER_ID.to_string());
    metadata.insert(
        "transport".to_string(),
        settings
            .transport
            .as_ref()
            .map_or("invalid", |transport| transport.as_str())
            .to_string(),
    );
    metadata.insert("force_http1".to_string(), settings.force_http1.to_string());
    metadata.insert(
        "mantle_auth_header".to_string(),
        settings.mantle_auth_header.to_string(),
    );
    if let Some(base_url) = &settings.mantle_base_url {
        metadata.insert("mantle_base_url".to_string(), base_url.clone());
    }
    metadata.insert(
        "default_model".to_string(),
        settings
            .default_model
            .clone()
            .unwrap_or_else(|| "<bedrock-discovery>".to_string()),
    );
    metadata.insert(
        "model_list_source".to_string(),
        if settings.model_ids_are_explicit {
            "explicit".to_string()
        } else {
            "default".to_string()
        },
    );
    metadata.insert(
        "configured_region".to_string(),
        settings
            .region
            .clone()
            .unwrap_or_else(|| "<aws-sdk-default-chain-or-fallback>".to_string()),
    );
    metadata.insert(
        "configured_region_source".to_string(),
        settings.region_source.as_str().to_string(),
    );
    metadata.insert("fallback_region".to_string(), DEFAULT_REGION.to_string());
    if let Some(profile) = &settings.aws_profile {
        metadata.insert("aws_profile".to_string(), profile.clone());
    }
    if let Some(endpoint_url) = &settings.endpoint_url {
        metadata.insert("endpoint_url".to_string(), endpoint_url.clone());
    }
    if client_context_bearer_token(settings).is_some()
        || std::env::var("AWS_BEARER_TOKEN_BEDROCK").is_ok_and(|value| !value.trim().is_empty())
    {
        metadata.insert(
            "bearer_token_source".to_string(),
            if settings.auth_credentials.contains_key("bearer_token") {
                "provider_auth_context"
            } else if settings.env.contains_key("AWS_BEARER_TOKEN_BEDROCK") {
                "request_environment"
            } else {
                "AWS_BEARER_TOKEN_BEDROCK"
            }
            .to_string(),
        );
    }
    metadata.insert("config_source".to_string(), settings.config_source.clone());
    metadata
}

fn parse_model_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn first_nonempty(values: impl IntoIterator<Item = String>) -> Option<String> {
    values.into_iter().find(|value| !value.trim().is_empty())
}

fn json_value_to_document(value: &serde_json::Value) -> Document {
    match value {
        serde_json::Value::Null => Document::Null,
        serde_json::Value::Bool(value) => Document::Bool(*value),
        serde_json::Value::Number(value) => value
            .as_u64()
            .map(Number::PosInt)
            .or_else(|| value.as_i64().map(Number::NegInt))
            .or_else(|| value.as_f64().map(Number::Float))
            .map_or(Document::Null, Document::Number),
        serde_json::Value::String(value) => Document::String(value.clone()),
        serde_json::Value::Array(values) => {
            Document::Array(values.iter().map(json_value_to_document).collect())
        }
        serde_json::Value::Object(values) => Document::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), json_value_to_document(value)))
                .collect::<HashMap<_, _>>(),
        ),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable = bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        BedrockProviderPlugin,
        include_str!("../bcode-plugin.toml")
    );
    vtable.cli_registration = Some(cli::registration);
    vtable
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(
    BedrockProviderPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
fn document_to_json_value(document: &Document) -> serde_json::Value {
    match document {
        Document::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), document_to_json_value(value)))
                .collect(),
        ),
        Document::Array(values) => {
            serde_json::Value::Array(values.iter().map(document_to_json_value).collect())
        }
        Document::Number(number) => match number {
            Number::PosInt(value) => serde_json::Value::Number((*value).into()),
            Number::NegInt(value) => serde_json::Value::Number((*value).into()),
            Number::Float(value) => serde_json::Number::from_f64(*value)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
        },
        Document::String(value) => serde_json::Value::String(value.clone()),
        Document::Bool(value) => serde_json::Value::Bool(*value),
        Document::Null => serde_json::Value::Null,
    }
}

/// Map an Anthropic Messages wire stop reason onto normalized model semantics.
///
/// Returns `None` for unrecognized values so an unknown future stop reason is not silently
/// interpreted as a known one.
fn map_anthropic_stop_reason(reason: &str) -> Option<StopReason> {
    match reason {
        "end_turn" => Some(StopReason::EndTurn),
        "tool_use" => Some(StopReason::ToolCall),
        "max_tokens" => Some(StopReason::MaxTokens),
        "stop_sequence" => Some(StopReason::StopSequence),
        _ => None,
    }
}

const fn map_stop_reason(reason: &BedrockStopReason) -> StopReason {
    match reason {
        BedrockStopReason::ToolUse => StopReason::ToolCall,
        BedrockStopReason::MaxTokens => StopReason::MaxTokens,
        BedrockStopReason::StopSequence => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

fn nonnegative_u32(value: i32) -> Option<u32> {
    u32::try_from(value).ok()
}

/// Combine Bedrock input-token fields into the complete request input context.
///
/// Bedrock reports `inputTokens` as *non-cached* input whenever prompt caching participates in a
/// request, so the complete model-visible request is
/// `inputTokens + cacheReadInputTokens + cacheWriteInputTokens`. Callers must only supply an
/// `input_tokens` value the provider actually reported, because a cache-only sum would understate
/// context occupancy.
fn complete_request_input_tokens(
    input_tokens: u32,
    cache_read_input_tokens: Option<u32>,
    cache_write_input_tokens: Option<u32>,
) -> u64 {
    u64::from(input_tokens)
        .saturating_add(u64::from(cache_read_input_tokens.unwrap_or_default()))
        .saturating_add(u64::from(cache_write_input_tokens.unwrap_or_default()))
}

fn build_error(error: &(impl ToString + ?Sized)) -> ProviderError {
    provider_error(
        "bedrock_request_build_failed",
        ProviderErrorCategory::InvalidRequest,
        error.to_string(),
    )
}

fn bedrock_messages_sdk_error(
    error_source: &aws_sdk_bedrockruntime::error::SdkError<InvokeModelWithResponseStreamError>,
) -> ProviderError {
    let Some(service_error) = error_source.as_service_error() else {
        let (kind, category) = match error_source {
            aws_sdk_bedrockruntime::error::SdkError::TimeoutError(_) => {
                ("timeout", ProviderErrorCategory::Timeout)
            }
            aws_sdk_bedrockruntime::error::SdkError::DispatchFailure(dispatch) => {
                return bedrock_dispatch_error(
                    "bedrock_messages_request_failed",
                    "Bedrock Messages runtime",
                    dispatch,
                    bcode_model::ProviderFailureCapability::ModelInvocation,
                );
            }
            aws_sdk_bedrockruntime::error::SdkError::ResponseError(_) => {
                ("response", ProviderErrorCategory::Network)
            }
            aws_sdk_bedrockruntime::error::SdkError::ConstructionFailure(_) => {
                ("construction", ProviderErrorCategory::Config)
            }
            _ => ("unknown", ProviderErrorCategory::ProviderInternal),
        };
        return bedrock_runtime_transport_error(kind, category);
    };
    let metadata = service_error.meta();
    let provider_message = metadata.message().map(sanitize_provider_diagnostic);
    let category = if service_error.is_access_denied_exception() {
        ProviderErrorCategory::Auth
    } else if service_error.is_throttling_exception() {
        ProviderErrorCategory::RateLimit
    } else if service_error.is_service_unavailable_exception() {
        ProviderErrorCategory::Overloaded
    } else if service_error.is_model_timeout_exception() {
        ProviderErrorCategory::Timeout
    } else if service_error.is_resource_not_found_exception() {
        ProviderErrorCategory::ModelNotFound
    } else if service_error.is_validation_exception() {
        ProviderErrorCategory::InvalidRequest
    } else if provider_message
        .as_deref()
        .is_some_and(is_context_length_error)
    {
        ProviderErrorCategory::ContextLength
    } else {
        ProviderErrorCategory::ProviderInternal
    };
    let mut normalized = provider_error(
        "bedrock_messages_request_failed",
        category,
        provider_message
            .as_deref()
            .unwrap_or("Bedrock Messages request failed"),
    );
    normalized.request_id = metadata
        .request_id()
        .map(str::to_string)
        .map(String::into_boxed_str);
    normalized.provider_message = provider_message.clone().map(String::into_boxed_str);
    normalized.sources.push(ProviderErrorSource {
        source: "aws_bedrock_messages".to_string(),
        code: metadata.code().map(str::to_string),
        message: provider_message,
    });
    normalized
}

fn bedrock_sdk_error(
    error_source: &aws_sdk_bedrockruntime::error::SdkError<ConverseStreamError>,
) -> ProviderError {
    let Some(service_error) = error_source.as_service_error() else {
        let (kind, category) = match error_source {
            aws_sdk_bedrockruntime::error::SdkError::TimeoutError(_) => {
                ("timeout", ProviderErrorCategory::Timeout)
            }
            aws_sdk_bedrockruntime::error::SdkError::DispatchFailure(dispatch) => {
                return bedrock_dispatch_error(
                    "bedrock_request_failed",
                    "Bedrock runtime",
                    dispatch,
                    bcode_model::ProviderFailureCapability::ModelInvocation,
                );
            }
            aws_sdk_bedrockruntime::error::SdkError::ResponseError(_) => {
                ("response", ProviderErrorCategory::Network)
            }
            aws_sdk_bedrockruntime::error::SdkError::ConstructionFailure(_) => {
                ("construction", ProviderErrorCategory::Config)
            }
            _ => ("unknown", ProviderErrorCategory::ProviderInternal),
        };
        return bedrock_runtime_transport_error(kind, category);
    };
    let category = bedrock_converse_stream_error_category(service_error);
    let provider_message = service_error
        .meta()
        .message()
        .map(sanitize_provider_diagnostic);
    let mut normalized = provider_error(
        "bedrock_request_failed",
        category,
        provider_message
            .as_deref()
            .unwrap_or("Bedrock runtime service request failed"),
    );
    if matches!(category, ProviderErrorCategory::Auth) {
        normalized.failure = Some(Box::new(bedrock_failure_context(
            bcode_model::ProviderFailureSourceKind::ProviderResponse,
            service_error
                .meta()
                .code()
                .unwrap_or("aws_bedrock_runtime_auth"),
            bcode_model::ProviderFailureCapability::ModelInvocation,
            "verify the AWS credential/profile source, region, and Bedrock model access",
        )));
    }
    normalized.request_id = service_error
        .meta()
        .request_id()
        .map(str::to_string)
        .map(String::into_boxed_str);
    normalized.provider_message = provider_message.clone().map(String::into_boxed_str);
    normalized.sources.push(ProviderErrorSource {
        source: "aws_bedrock_runtime".to_string(),
        code: service_error.meta().code().map(str::to_string),
        message: provider_message.clone(),
    });
    if category == ProviderErrorCategory::RateLimit || category == ProviderErrorCategory::Overloaded
    {
        normalized.retry = provider_message
            .as_deref()
            .and_then(retry_hint_from_body)
            .map(Box::new);
    }
    normalized
}

fn bedrock_failure_context(
    source_kind: bcode_model::ProviderFailureSourceKind,
    source: impl Into<String>,
    capability: bcode_model::ProviderFailureCapability,
    remediation: impl Into<String>,
) -> bcode_model::ProviderFailureContext {
    bcode_model::ProviderFailureContext {
        provider_id: PROVIDER_ID.to_string(),
        source_kind,
        source: source.into(),
        capability,
        remediation: remediation.into(),
    }
}

const MAX_BEDROCK_ERROR_CHAIN_DEPTH: usize = 16;
const MAX_BEDROCK_SOURCE_MESSAGE_CHARS: usize = 512;

#[allow(clippy::too_many_lines)]
fn bedrock_dispatch_error(
    code: &str,
    operation: &str,
    dispatch: &aws_smithy_runtime_api::client::result::DispatchFailure,
    capability: bcode_model::ProviderFailureCapability,
) -> ProviderError {
    let connector = dispatch
        .as_connector_error()
        .expect("AWS dispatch failures contain a connector error");
    let connector_kind = if connector.is_timeout() {
        "timeout"
    } else if connector.is_io() {
        "io"
    } else if connector.is_user() {
        "user"
    } else if connector.is_other() {
        "other"
    } else {
        "unknown"
    };
    let chain = bedrock_error_chain(connector);
    let credential_chain_failure = bedrock_credential_chain_failure(&chain.sources);
    let category = if credential_chain_failure {
        ProviderErrorCategory::Auth
    } else if connector.is_timeout() {
        ProviderErrorCategory::Timeout
    } else if connector.is_user() {
        ProviderErrorCategory::Config
    } else {
        ProviderErrorCategory::Network
    };
    let mut normalized = provider_error(
        code,
        category,
        if credential_chain_failure {
            "AWS credentials could not be resolved for Bedrock".to_string()
        } else {
            format!("{operation} dispatch failure")
        },
    );
    normalized.retryable = !credential_chain_failure
        && matches!(
            category,
            ProviderErrorCategory::Network | ProviderErrorCategory::Timeout
        );
    normalized
        .diagnostic_context
        .insert("transport_kind".to_string(), "dispatch".to_string());
    normalized.diagnostic_context.insert(
        "connector_error_kind".to_string(),
        connector_kind.to_string(),
    );
    if let Some(retry_kind) = connector.as_other() {
        normalized.diagnostic_context.insert(
            "connector_retry_kind".to_string(),
            retry_kind.to_string().replace(' ', "_"),
        );
    }
    normalized.diagnostic_context.insert(
        "connection_established".to_string(),
        connector.connection_metadata().is_some().to_string(),
    );
    if credential_chain_failure {
        normalized.diagnostic_context.insert(
            "auth_failure_kind".to_string(),
            "credential_chain_exhausted".to_string(),
        );
    }

    normalized.diagnostic_context.insert(
        "error_chain_depth".to_string(),
        chain.sources.len().to_string(),
    );
    if chain.truncated {
        normalized
            .diagnostic_context
            .insert("error_chain_truncated".to_string(), "true".to_string());
    }
    if let Some(io_error) = preferred_bedrock_io_error(&chain.sources) {
        normalized.diagnostic_context.insert(
            "io_error_kind".to_string(),
            bedrock_io_error_kind(io_error.kind()).to_string(),
        );
        if let Some(os_error) = io_error.raw_os_error() {
            normalized
                .diagnostic_context
                .insert("os_error_code".to_string(), os_error.to_string());
        }
    }
    if let Some(tls_error) = chain
        .sources
        .iter()
        .find_map(|source| source.downcast_ref::<rustls::Error>())
    {
        normalized.diagnostic_context.insert(
            "tls_error_kind".to_string(),
            bedrock_rustls_error_kind(tls_error).to_string(),
        );
    }
    if let Some(root) = chain.sources.last() {
        let (root_source, root_code) = bedrock_error_source_identity(
            *root,
            chain.sources.len().saturating_sub(1),
            chain.sources.len(),
        );
        normalized
            .diagnostic_context
            .insert("root_error_source".to_string(), root_source);
        normalized
            .diagnostic_context
            .insert("root_error_code".to_string(), root_code);
        normalized.diagnostic_context.insert(
            "root_error_message".to_string(),
            safe_bedrock_source_message(&root.to_string()),
        );
    }
    normalized.sources.push(ProviderErrorSource {
        source: "aws_sdk".to_string(),
        code: Some("dispatch".to_string()),
        message: None,
    });
    normalized
        .sources
        .extend(chain.sources.iter().enumerate().map(|(index, source)| {
            let (source_name, source_code) =
                bedrock_error_source_identity(*source, index, chain.sources.len());
            ProviderErrorSource {
                source: source_name,
                code: Some(source_code),
                message: Some(safe_bedrock_source_message(&source.to_string())),
            }
        }));
    if matches!(
        category,
        ProviderErrorCategory::Auth | ProviderErrorCategory::Config
    ) {
        normalized.failure = Some(Box::new(bedrock_failure_context(
            bcode_model::ProviderFailureSourceKind::Runtime,
            if credential_chain_failure {
                "aws_sdk_credential_chain"
            } else {
                "aws_sdk_connector"
            },
            capability,
            if credential_chain_failure {
                "configure AWS credentials through the selected Bedrock auth profile, AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, AWS_PROFILE, web identity, ECS task credentials, or EC2 instance credentials"
            } else {
                "verify the AWS region, endpoint, credentials, proxy, TLS trust roots, and network policy"
            },
        )));
    }
    normalized
}

fn bedrock_credential_chain_failure(chain: &[&(dyn std::error::Error + 'static)]) -> bool {
    chain.iter().any(|source| {
        let message = source.to_string().to_ascii_lowercase();
        message.contains("no credentials found in chain")
            || message.contains("credential provider was not enabled")
            || message.contains("credentials could not be loaded")
    })
}

struct BedrockErrorChain<'a> {
    sources: Vec<&'a (dyn std::error::Error + 'static)>,
    truncated: bool,
}

fn bedrock_error_chain<'a>(error: &'a (dyn std::error::Error + 'static)) -> BedrockErrorChain<'a> {
    let mut sources = Vec::new();
    let mut source = bedrock_nested_error_source(error);
    while let Some(current) = source
        && sources.len() < MAX_BEDROCK_ERROR_CHAIN_DEPTH
    {
        sources.push(current);
        source = bedrock_nested_error_source(current);
    }
    BedrockErrorChain {
        sources,
        truncated: source.is_some(),
    }
}

fn bedrock_nested_error_source<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a (dyn std::error::Error + 'static)> {
    error.source().or_else(|| {
        error.downcast_ref::<std::io::Error>().and_then(|error| {
            error
                .get_ref()
                .map(|source| source as &(dyn std::error::Error + 'static))
        })
    })
}

fn preferred_bedrock_io_error<'a>(
    chain: &[&'a (dyn std::error::Error + 'static)],
) -> Option<&'a std::io::Error> {
    let errors = chain
        .iter()
        .filter_map(|source| source.downcast_ref::<std::io::Error>())
        .collect::<Vec<_>>();
    errors
        .iter()
        .rev()
        .copied()
        .find(|error| error.kind() != std::io::ErrorKind::Other)
        .or_else(|| errors.last().copied())
}

fn bedrock_error_source_identity(
    source: &(dyn std::error::Error + 'static),
    index: usize,
    chain_len: usize,
) -> (String, String) {
    if let Some(io_error) = source.downcast_ref::<std::io::Error>() {
        return (
            if index + 1 == chain_len {
                "io_root"
            } else {
                "io"
            }
            .to_string(),
            bedrock_io_error_kind(io_error.kind()).to_string(),
        );
    }
    if let Some(tls_error) = source.downcast_ref::<rustls::Error>() {
        return (
            if index + 1 == chain_len {
                "rustls_root"
            } else {
                "rustls"
            }
            .to_string(),
            bedrock_rustls_error_kind(tls_error).to_string(),
        );
    }
    let message = source.to_string().to_ascii_lowercase();
    let code = if message.contains("dns") {
        "dns"
    } else if message.contains("proxy") || message.contains("tunnel") {
        "proxy"
    } else if message.contains("certificate") || message.contains("tls") {
        "tls"
    } else if message.contains("connect") || message.contains("tcp") {
        "connect"
    } else {
        "unknown"
    };
    (
        if index + 1 == chain_len {
            "transport_root"
        } else {
            "transport"
        }
        .to_string(),
        code.to_string(),
    )
}

fn safe_bedrock_source_message(message: &str) -> String {
    let sanitized = sanitize_provider_diagnostic(message);
    let normalized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let prefix = chars
        .by_ref()
        .take(MAX_BEDROCK_SOURCE_MESSAGE_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…[TRUNCATED]")
    } else {
        prefix
    }
}

const fn bedrock_io_error_kind(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        std::io::ErrorKind::ConnectionRefused => "connection_refused",
        std::io::ErrorKind::ConnectionReset => "connection_reset",
        std::io::ErrorKind::HostUnreachable => "host_unreachable",
        std::io::ErrorKind::NetworkUnreachable => "network_unreachable",
        std::io::ErrorKind::ConnectionAborted => "connection_aborted",
        std::io::ErrorKind::NotConnected => "not_connected",
        std::io::ErrorKind::AddrNotAvailable => "address_not_available",
        std::io::ErrorKind::NetworkDown => "network_down",
        std::io::ErrorKind::BrokenPipe => "broken_pipe",
        std::io::ErrorKind::TimedOut => "timed_out",
        std::io::ErrorKind::Interrupted => "interrupted",
        std::io::ErrorKind::UnexpectedEof => "unexpected_eof",
        std::io::ErrorKind::InvalidData => "invalid_data",
        _ => "other",
    }
}

const fn bedrock_rustls_error_kind(error: &rustls::Error) -> &'static str {
    match error {
        rustls::Error::InvalidCertificate(certificate_error) => {
            bedrock_certificate_error_kind(certificate_error)
        }
        rustls::Error::NoCertificatesPresented => "no_certificates_presented",
        rustls::Error::PeerIncompatible(_) => "peer_incompatible",
        rustls::Error::PeerMisbehaved(_) => "peer_misbehaved",
        rustls::Error::NoApplicationProtocol => "no_application_protocol",
        rustls::Error::AlertReceived(_) => "alert_received",
        _ => "tls_protocol_error",
    }
}

const fn bedrock_certificate_error_kind(error: &rustls::CertificateError) -> &'static str {
    match error {
        rustls::CertificateError::BadEncoding => "certificate_bad_encoding",
        rustls::CertificateError::Expired | rustls::CertificateError::ExpiredContext { .. } => {
            "certificate_expired"
        }
        rustls::CertificateError::NotValidYet
        | rustls::CertificateError::NotValidYetContext { .. } => "certificate_not_valid_yet",
        rustls::CertificateError::Revoked => "certificate_revoked",
        rustls::CertificateError::UnknownIssuer => "certificate_unknown_issuer",
        rustls::CertificateError::BadSignature => "certificate_bad_signature",
        rustls::CertificateError::NotValidForName
        | rustls::CertificateError::NotValidForNameContext { .. } => {
            "certificate_not_valid_for_name"
        }
        _ => "certificate_error",
    }
}

fn bedrock_runtime_transport_error(kind: &str, category: ProviderErrorCategory) -> ProviderError {
    let mut normalized = provider_error(
        "bedrock_request_failed",
        category,
        format!("Bedrock runtime {kind} failure"),
    );
    normalized
        .diagnostic_context
        .insert("transport_kind".to_string(), kind.to_string());
    normalized.sources.push(ProviderErrorSource {
        source: "aws_sdk".to_string(),
        code: Some(kind.to_string()),
        message: None,
    });
    if matches!(
        category,
        ProviderErrorCategory::Auth | ProviderErrorCategory::Config
    ) {
        normalized.failure = Some(Box::new(bedrock_failure_context(
            bcode_model::ProviderFailureSourceKind::Runtime,
            "aws_sdk_credential_and_region_chain",
            bcode_model::ProviderFailureCapability::ModelInvocation,
            "verify the AWS credential/profile source, region, and Bedrock model access",
        )));
    }
    normalized
}

fn bedrock_converse_stream_error_category(error: &ConverseStreamError) -> ProviderErrorCategory {
    if error.is_service_unavailable_exception() {
        ProviderErrorCategory::Overloaded
    } else if error.is_throttling_exception() {
        ProviderErrorCategory::RateLimit
    } else if error.is_access_denied_exception() {
        ProviderErrorCategory::Auth
    } else if is_context_length_error(error.to_string().as_str()) {
        ProviderErrorCategory::ContextLength
    } else if error.is_validation_exception() {
        ProviderErrorCategory::InvalidRequest
    } else if error.is_resource_not_found_exception() {
        ProviderErrorCategory::ModelNotFound
    } else if error.is_model_timeout_exception() {
        ProviderErrorCategory::Timeout
    } else {
        ProviderErrorCategory::ProviderInternal
    }
}

fn bedrock_error_category_from_message(message: &str) -> ProviderErrorCategory {
    if message.contains("ServiceUnavailableException")
        || message.contains("ServiceUnavailable")
        || message.contains("status code: 503")
        || message.contains("status: 503")
    {
        ProviderErrorCategory::Overloaded
    } else if message.contains("UnrecognizedClient")
        || message.contains("AccessDenied")
        || message.contains("ExpiredToken")
        || message.contains("credentials")
    {
        ProviderErrorCategory::Auth
    } else if message.contains("Throttl") || message.contains("TooManyRequests") {
        ProviderErrorCategory::RateLimit
    } else if is_context_length_error(message) {
        ProviderErrorCategory::ContextLength
    } else if message.contains("ValidationException") {
        ProviderErrorCategory::InvalidRequest
    } else if message.contains("ResourceNotFound") || message.contains("not found") {
        ProviderErrorCategory::ModelNotFound
    } else {
        ProviderErrorCategory::ProviderInternal
    }
}

fn bedrock_discovery_error<E, R>(error: &bedrock::error::SdkError<E, R>) -> ProviderError
where
    E: ProvideErrorMetadata,
{
    match error {
        bedrock::error::SdkError::ServiceError(service) => {
            let metadata = service.err().meta();
            let provider_message = metadata.message().map(sanitize_provider_diagnostic);
            let category = metadata
                .code()
                .or_else(|| metadata.message())
                .map_or(ProviderErrorCategory::ProviderInternal, |detail| {
                    bedrock_error_category_from_message(detail)
                });
            let mut normalized = provider_error(
                "bedrock_model_discovery_failed",
                category,
                provider_message
                    .as_deref()
                    .unwrap_or("Bedrock model discovery service request failed"),
            );
            normalized.request_id = metadata
                .request_id()
                .map(str::to_string)
                .map(String::into_boxed_str);
            normalized.provider_message = provider_message.clone().map(String::into_boxed_str);
            normalized.sources.push(ProviderErrorSource {
                source: "aws_bedrock_control".to_string(),
                code: metadata.code().map(str::to_string),
                message: provider_message,
            });
            if matches!(category, ProviderErrorCategory::Auth) {
                normalized.failure = Some(Box::new(bedrock_failure_context(
                    bcode_model::ProviderFailureSourceKind::ProviderResponse,
                    metadata.code().unwrap_or("aws_bedrock_control_auth"),
                    bcode_model::ProviderFailureCapability::ModelDiscovery,
                    "verify the AWS credential/profile source, region, and Bedrock model access",
                )));
            }
            normalized
        }
        bedrock::error::SdkError::TimeoutError(_) => {
            bedrock_transport_error("timeout", ProviderErrorCategory::Timeout)
        }
        bedrock::error::SdkError::DispatchFailure(dispatch) => bedrock_dispatch_error(
            "bedrock_model_discovery_failed",
            "Bedrock model discovery",
            dispatch,
            bcode_model::ProviderFailureCapability::ModelDiscovery,
        ),
        bedrock::error::SdkError::ResponseError(_) => {
            bedrock_transport_error("response", ProviderErrorCategory::Network)
        }
        bedrock::error::SdkError::ConstructionFailure(_) => {
            bedrock_transport_error("construction", ProviderErrorCategory::Config)
        }
        _ => bedrock_transport_error("unknown", ProviderErrorCategory::ProviderInternal),
    }
}

fn bedrock_transport_error(kind: &str, category: ProviderErrorCategory) -> ProviderError {
    let mut normalized = provider_error(
        "bedrock_model_discovery_failed",
        category,
        format!("Bedrock model discovery {kind} failure"),
    );
    normalized
        .diagnostic_context
        .insert("transport_kind".to_string(), kind.to_string());
    normalized.sources.push(ProviderErrorSource {
        source: "aws_sdk".to_string(),
        code: Some(kind.to_string()),
        message: None,
    });
    if matches!(
        category,
        ProviderErrorCategory::Auth | ProviderErrorCategory::Config
    ) {
        normalized.failure = Some(Box::new(bedrock_failure_context(
            bcode_model::ProviderFailureSourceKind::Runtime,
            "aws_sdk_credential_and_region_chain",
            bcode_model::ProviderFailureCapability::ModelDiscovery,
            "verify the AWS credential/profile source, region, and Bedrock model access",
        )));
    }
    normalized
}

fn bedrock_messages_stream_error<R>(
    error: &aws_sdk_bedrockruntime::error::SdkError<
        aws_sdk_bedrockruntime::types::error::ResponseStreamError,
        R,
    >,
) -> ProviderError {
    let Some(service_error) = error.as_service_error() else {
        return provider_error(
            "bedrock_messages_stream_failed",
            ProviderErrorCategory::Network,
            "Bedrock Messages response stream failed",
        );
    };
    let metadata = service_error.meta();
    let provider_message = metadata.message().map(sanitize_provider_diagnostic);
    let category = if service_error.is_service_unavailable_exception() {
        ProviderErrorCategory::Overloaded
    } else if service_error.is_throttling_exception() {
        ProviderErrorCategory::RateLimit
    } else if service_error.is_validation_exception() {
        ProviderErrorCategory::InvalidRequest
    } else if service_error.is_model_timeout_exception() {
        ProviderErrorCategory::Timeout
    } else {
        ProviderErrorCategory::ProviderInternal
    };
    let mut normalized = provider_error(
        "bedrock_messages_stream_failed",
        category,
        provider_message
            .as_deref()
            .unwrap_or("Bedrock Messages response stream failed"),
    );
    normalized.request_id = metadata
        .request_id()
        .map(str::to_string)
        .map(String::into_boxed_str);
    normalized.provider_message = provider_message.clone().map(String::into_boxed_str);
    normalized.sources.push(ProviderErrorSource {
        source: "aws_bedrock_messages_stream".to_string(),
        code: metadata.code().map(str::to_string),
        message: provider_message,
    });
    normalized
}

fn bedrock_stream_error<R>(
    error: &aws_sdk_bedrockruntime::error::SdkError<
        aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError,
        R,
    >,
) -> ProviderError {
    let Some(service_error) = error.as_service_error() else {
        let (kind, category) = match error {
            aws_sdk_bedrockruntime::error::SdkError::TimeoutError(_) => {
                ("timeout", ProviderErrorCategory::Timeout)
            }
            aws_sdk_bedrockruntime::error::SdkError::DispatchFailure(dispatch) => {
                return bedrock_dispatch_error(
                    "bedrock_stream_failed",
                    "Bedrock stream",
                    dispatch,
                    bcode_model::ProviderFailureCapability::ModelInvocation,
                );
            }
            aws_sdk_bedrockruntime::error::SdkError::ResponseError(_) => {
                ("response", ProviderErrorCategory::Network)
            }
            aws_sdk_bedrockruntime::error::SdkError::ConstructionFailure(_) => {
                ("construction", ProviderErrorCategory::ProviderInternal)
            }
            _ => ("unknown", ProviderErrorCategory::ProviderInternal),
        };
        let mut normalized = provider_error(
            "bedrock_stream_failed",
            category,
            format!("Bedrock stream {kind} failure"),
        );
        normalized
            .diagnostic_context
            .insert("transport_kind".to_string(), kind.to_string());
        normalized.sources.push(ProviderErrorSource {
            source: "aws_sdk".to_string(),
            code: Some(kind.to_string()),
            message: None,
        });
        return normalized;
    };
    let metadata = service_error.meta();
    let provider_message = metadata.message().map(sanitize_provider_diagnostic);
    let category = if service_error.is_service_unavailable_exception() {
        ProviderErrorCategory::Overloaded
    } else if service_error.is_throttling_exception() {
        ProviderErrorCategory::RateLimit
    } else if service_error.is_validation_exception() {
        ProviderErrorCategory::InvalidRequest
    } else if provider_message
        .as_deref()
        .is_some_and(is_context_length_error)
    {
        ProviderErrorCategory::ContextLength
    } else {
        ProviderErrorCategory::ProviderInternal
    };
    let mut normalized = provider_error(
        "bedrock_stream_failed",
        category,
        provider_message
            .as_deref()
            .unwrap_or("Bedrock response stream failed"),
    );
    normalized.request_id = metadata
        .request_id()
        .map(str::to_string)
        .map(String::into_boxed_str);
    normalized.provider_message = provider_message.clone().map(String::into_boxed_str);
    normalized.sources.push(ProviderErrorSource {
        source: "aws_bedrock_stream".to_string(),
        code: metadata.code().map(str::to_string),
        message: provider_message,
    });
    normalized
}

fn is_context_length_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("maximum context length")
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

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match serde_json::to_vec(value) {
        Ok(payload) => ServiceResponse::ok(payload),
        Err(error) => ServiceResponse::error("serialization_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model_provider_runtime::{
        BlockingModelProviderInvoker, ProviderConformanceOptions, ProviderConformanceOutcome,
        run_provider_conformance_suite,
    };
    use bcode_plugin_sdk::{PluginConfigContext, ServiceCancellation};

    #[test]
    fn anthropic_messages_error_event_is_normalized() {
        let mut accumulator = AnthropicMessagesAccumulator::new(BTreeMap::new());
        let turn = TurnState::default();
        let event = serde_json::json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": "invalid API key"
            }
        });
        let error = accumulator
            .process(&event, &turn)
            .expect_err("error event must fail the stream");
        assert_eq!(error.category, ProviderErrorCategory::Auth);
        assert_eq!(error.code, "bedrock_anthropic_authentication_error");
    }

    #[test]
    fn mantle_anthropic_request_uses_direct_messages_shape() {
        let mut request = test_model_turn_request();
        request.parameters.reasoning_control = Some(bcode_model::ReasoningControl::Adaptive);
        request.parameters.reasoning_effort_value = Some("high".to_string());

        let value = build_mantle_anthropic_request(&request, "anthropic.claude-opus-5")
            .expect("Mantle request should serialize");

        assert_eq!(value["model"], "anthropic.claude-opus-5");
        assert_eq!(value["stream"], true);
        assert!(value.get("anthropic_version").is_none());
        assert_eq!(value["thinking"]["type"], "adaptive");
        assert_eq!(value["output_config"]["effort"], "high");
    }

    #[test]
    fn mantle_config_validation_checks_the_endpoint_for_the_configured_flavor() {
        // A base URL that is valid for one flavor must still be validated against the surface the
        // configured transport will actually use.
        for (transport, flavor) in [
            (BedrockTransport::MantleAnthropic, MantleFlavor::Anthropic),
            (BedrockTransport::MantleOpenAi, MantleFlavor::OpenAi),
        ] {
            let mut settings = test_settings();
            settings.transport = Ok(transport);
            assert_eq!(
                settings.mantle_flavor().expect("flavor should resolve"),
                flavor
            );

            settings.mantle_base_url = Some("http://mantle.example.com/v1".to_string());
            // The bearer-token check runs before endpoint validation, so supply one to reach the
            // endpoint check.
            settings
                .auth_credentials
                .insert("bearer_token".to_string(), "secret".to_string());
            assert_eq!(
                validate_mantle_settings(&settings).unwrap_err().code,
                "bedrock_mantle_base_url_insecure"
            );
        }

        let mut runtime = test_settings();
        runtime.transport = Ok(BedrockTransport::Runtime);
        assert_eq!(
            runtime.mantle_flavor().unwrap_err().code,
            "bedrock_mantle_transport_required"
        );
    }

    #[test]
    fn mantle_endpoint_defaults_from_region_and_accepts_local_tests() {
        let settings = test_settings();
        assert_eq!(
            mantle_anthropic_messages_endpoint(&settings).expect("default endpoint"),
            "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages"
        );

        let mut local = settings;
        local.mantle_base_url = Some("http://127.0.0.1:8080/anthropic/".to_string());
        assert_eq!(
            mantle_anthropic_messages_endpoint(&local).expect("local endpoint"),
            "http://127.0.0.1:8080/anthropic/v1/messages"
        );
    }

    #[test]
    fn mantle_openai_endpoint_uses_the_documented_responses_path() {
        let mut settings = test_settings();
        // AWS documents this as `openai/v1/responses`, deliberately different from the
        // `v1/responses` path used by other models on the responses endpoint.
        assert_eq!(
            mantle_endpoint(&settings, MantleFlavor::OpenAi).expect("default endpoint"),
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1/responses"
        );

        settings.region = Some("eu-west-1".to_string());
        assert_eq!(
            mantle_endpoint(&settings, MantleFlavor::OpenAi).expect("regional endpoint"),
            "https://bedrock-mantle.eu-west-1.api.aws/openai/v1/responses"
        );

        let mut local = settings;
        local.mantle_base_url = Some("http://localhost:8080/openai/v1/".to_string());
        assert_eq!(
            mantle_endpoint(&local, MantleFlavor::OpenAi).expect("local endpoint"),
            "http://localhost:8080/openai/v1/responses"
        );
    }

    #[test]
    fn mantle_endpoints_reject_insecure_non_loopback_base_urls() {
        for flavor in [MantleFlavor::Anthropic, MantleFlavor::OpenAi] {
            let mut settings = test_settings();
            settings.mantle_base_url = Some("http://mantle.example.com/openai/v1".to_string());
            assert_eq!(
                mantle_endpoint(&settings, flavor).unwrap_err().code,
                "bedrock_mantle_base_url_insecure"
            );
        }
    }

    #[test]
    fn transport_parsing_covers_every_supported_value() {
        for (value, expected) in [
            (None, BedrockTransport::Runtime),
            (Some("runtime"), BedrockTransport::Runtime),
            (Some("bedrock_runtime"), BedrockTransport::Runtime),
            (Some("mantle"), BedrockTransport::MantleAnthropic),
            (Some("mantle_anthropic"), BedrockTransport::MantleAnthropic),
            (Some("mantle_openai"), BedrockTransport::MantleOpenAi),
        ] {
            let parsed = BedrockTransport::parse(value).expect("transport should parse");
            assert_eq!(parsed, expected, "value: {value:?}");
            // `as_str` must round-trip so persisted/diagnostic values stay stable.
            assert_eq!(
                BedrockTransport::parse(Some(parsed.as_str())).expect("round-trip"),
                expected
            );
        }

        assert_eq!(
            BedrockTransport::parse(Some("mantle_bedrock"))
                .unwrap_err()
                .code,
            "bedrock_transport_invalid"
        );
    }

    #[test]
    fn every_mantle_transport_requires_an_explicitly_configured_model() {
        // Mantle has no control-plane discovery, so both flavors must demand a configured model
        // instead of silently falling back to a discovered default.
        for transport in [
            BedrockTransport::MantleAnthropic,
            BedrockTransport::MantleOpenAi,
        ] {
            assert!(
                transport.is_mantle(),
                "{transport:?} must be a Mantle transport"
            );
            let mut settings = test_settings();
            settings.transport = Ok(transport);
            settings.default_model = None;
            settings.model_ids.clear();
            assert_eq!(
                validate_mantle_settings(&settings).unwrap_err().code,
                "bedrock_mantle_model_required"
            );
        }

        assert!(!BedrockTransport::Runtime.is_mantle());
    }

    #[test]
    fn mantle_sse_decoder_handles_fragmented_multiline_events() {
        let mut decoder = MantleSseDecoder::default();
        assert!(
            decoder
                .push(b"event: message_start\r\ndata: {\"type\":")
                .unwrap()
                .is_empty()
        );
        let events = decoder
            .push(b"\"message_start\",\r\ndata: \"message\":{}}\r\n\r\n")
            .expect("fragmented event should decode");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "message_start");
    }

    #[test]
    fn mantle_settings_require_model_and_bearer_token() {
        let mut settings = test_settings();
        settings.transport = Ok(BedrockTransport::MantleAnthropic);
        settings.default_model = None;
        settings.model_ids.clear();
        assert_eq!(
            validate_mantle_settings(&settings).unwrap_err().code,
            "bedrock_mantle_model_required"
        );

        settings.default_model = Some("anthropic.claude-opus-5".to_string());
        assert_eq!(
            validate_mantle_settings(&settings).unwrap_err().code,
            "bedrock_mantle_missing_bearer_token"
        );
        settings
            .auth_credentials
            .insert("bearer_token".to_string(), "secret".to_string());
        validate_mantle_settings(&settings).expect("valid Mantle settings");
        let diagnostics = diagnostics_metadata(&settings);
        assert_eq!(
            diagnostics.get("transport").map(String::as_str),
            Some("mantle_anthropic")
        );
        assert!(!diagnostics.values().any(|value| value.contains("secret")));
    }

    fn test_settings() -> Settings {
        Settings {
            transport: Ok(BedrockTransport::Runtime),
            mantle_base_url: None,
            mantle_auth_header: false,
            force_http1: false,
            default_model: Some("anthropic.claude-opus-5".to_string()),
            model_ids: vec!["anthropic.claude-opus-5".to_string()],
            model_ids_are_explicit: false,
            region: Some(DEFAULT_REGION.to_string()),
            region_source: RegionSource::Profile,
            aws_profile: None,
            endpoint_url: None,
            auth_credentials: BTreeMap::new(),
            env: BTreeMap::new(),
            config_source: "test".to_string(),
        }
    }

    #[test]
    fn anthropic_messages_request_serializes_tools_and_adaptive_thinking() {
        let mut request = test_model_turn_request();
        request.messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "inspect the repository".to_string(),
            }],
        }];
        request.parameters.max_output_tokens = Some(8_192);
        request.parameters.reasoning_control = Some(bcode_model::ReasoningControl::Adaptive);
        request.parameters.reasoning_effort_value = Some("high".to_string());
        request.system_prompt = Some("system instructions".to_string());
        request.prompt_cache.cache_system_prompt = true;
        request.prompt_cache.cache_tools = true;
        request.tools = vec![ToolDefinition {
            name: "shell.run".to_string(),
            description: "Run a shell command".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];

        let value: serde_json::Value = serde_json::from_slice(
            &build_anthropic_messages_request(&request).expect("request should serialize"),
        )
        .expect("request should be JSON");

        assert_eq!(value["anthropic_version"], "bedrock-2023-05-31");
        assert_eq!(value["max_tokens"], 8_192);
        assert_eq!(
            value["messages"][0]["content"][0]["text"],
            "inspect the repository"
        );
        assert_eq!(value["tools"][0]["name"], "shell_run");
        assert_eq!(value["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(value["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(value["thinking"]["type"], "adaptive");
        assert_eq!(value["output_config"]["effort"], "high");
    }

    #[test]
    fn anthropic_messages_request_serializes_tool_result_output() {
        let mut request = test_model_turn_request();
        request.messages = vec![ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: "toolu_1".to_string(),
                    output: "useful tool output".to_string(),
                    is_error: false,
                    content: Vec::new(),
                },
            }],
        }];

        let value: serde_json::Value = serde_json::from_slice(
            &build_anthropic_messages_request(&request).expect("request should serialize"),
        )
        .expect("request should be JSON");

        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(
            value["messages"][0]["content"][0],
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [{"type": "text", "text": "useful tool output"}],
                "is_error": false,
            })
        );
    }

    #[test]
    fn anthropic_messages_request_substitutes_placeholder_for_empty_error_tool_result() {
        let mut request = test_model_turn_request();
        request.messages = vec![ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: "toolu_err".to_string(),
                    output: String::new(),
                    is_error: true,
                    content: Vec::new(),
                },
            }],
        }];

        let value: serde_json::Value = serde_json::from_slice(
            &build_anthropic_messages_request(&request).expect("request should serialize"),
        )
        .expect("request should be JSON");

        assert_eq!(
            value["messages"][0]["content"][0],
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": "toolu_err",
                "content": [{"type": "text", "text": EMPTY_ERROR_TOOL_RESULT_PLACEHOLDER}],
                "is_error": true,
            })
        );
    }

    #[test]
    fn anthropic_messages_request_keeps_empty_content_for_successful_tool_result() {
        let mut request = test_model_turn_request();
        request.messages = vec![ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: "toolu_ok".to_string(),
                    output: String::new(),
                    is_error: false,
                    content: Vec::new(),
                },
            }],
        }];

        let value: serde_json::Value = serde_json::from_slice(
            &build_anthropic_messages_request(&request).expect("request should serialize"),
        )
        .expect("request should be JSON");

        assert_eq!(
            value["messages"][0]["content"][0],
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": "toolu_ok",
                "content": [],
                "is_error": false,
            })
        );
    }

    #[test]
    fn anthropic_messages_accumulator_emits_text_tools_and_usage() {
        let turn = TurnState::default();
        let mut accumulator = AnthropicMessagesAccumulator::new(BTreeMap::from([(
            "shell_run".to_string(),
            "shell.run".to_string(),
        )]));
        accumulator
            .process(
                &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
                &turn,
            )
            .expect("text delta should process");
        accumulator
            .process(
                &serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell_run","input":{}}}),
                &turn,
            )
            .expect("tool start should process");
        accumulator
            .process(
                &serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pwd\"}"}}),
                &turn,
            )
            .expect("tool delta should process");
        accumulator
            .process(
                &serde_json::json!({"type":"content_block_stop","index":1}),
                &turn,
            )
            .expect("tool stop should process");
        accumulator
            .process(
                &serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}),
                &turn,
            )
            .expect("usage should process");

        let events = turn.drain();
        assert!(events.iter().any(
            |event| matches!(event, ProviderTurnEvent::TextDelta { text } if text == "hello")
        ));
        assert!(events.iter().any(|event| matches!(event, ProviderTurnEvent::ToolCallFinished { call } if call.name == "shell.run" && call.arguments["command"] == "pwd")));
        assert!(events.iter().any(|event| matches!(event, ProviderTurnEvent::ExactRequestInputTokens { tokens } if tokens.get() == 12)));
    }

    #[test]
    fn anthropic_messages_exact_input_tokens_include_cache_reads_and_writes() {
        let turn = TurnState::default();
        let mut accumulator = AnthropicMessagesAccumulator::new(BTreeMap::new());

        accumulator
            .process(
                &serde_json::json!({"type":"message_start","message":{"usage":{
                    "input_tokens": 12,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 400,
                    "cache_creation_input_tokens": 80
                }}}),
                &turn,
            )
            .expect("usage should process");

        let events = turn.drain();
        // Anthropic reports `input_tokens` as non-cached input, so the complete request context is
        // 12 + 400 + 80.
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ExactRequestInputTokens { tokens } if tokens.get() == 492
        )));
        // Billing-shaped usage keeps the provider's own field split.
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::Usage { usage }
                if usage.input_tokens == Some(12)
                    && usage.cached_input_tokens == Some(400)
                    && usage.cache_write_input_tokens == Some(80)
        )));
    }

    #[test]
    fn anthropic_messages_omits_exact_input_tokens_without_input_count() {
        let turn = TurnState::default();
        let mut accumulator = AnthropicMessagesAccumulator::new(BTreeMap::new());

        accumulator
            .process(
                &serde_json::json!({"type":"message_start","message":{"usage":{
                    "cache_read_input_tokens": 400
                }}}),
                &turn,
            )
            .expect("usage should process");

        // A cache-only sum would understate context, so fail closed to the local estimate.
        assert!(
            !turn
                .drain()
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::ExactRequestInputTokens { .. }))
        );
    }

    fn converse_metadata_event(
        input_tokens: i32,
        cache_read_input_tokens: Option<i32>,
        cache_write_input_tokens: Option<i32>,
    ) -> ConverseStreamOutput {
        let mut usage = aws_sdk_bedrockruntime::types::TokenUsage::builder()
            .input_tokens(input_tokens)
            .output_tokens(7)
            .total_tokens(input_tokens.saturating_add(7));
        if let Some(tokens) = cache_read_input_tokens {
            usage = usage.cache_read_input_tokens(tokens);
        }
        if let Some(tokens) = cache_write_input_tokens {
            usage = usage.cache_write_input_tokens(tokens);
        }
        ConverseStreamOutput::Metadata(
            aws_sdk_bedrockruntime::types::ConverseStreamMetadataEvent::builder()
                .usage(usage.build().expect("token usage should build"))
                .build(),
        )
    }

    fn converse_message_stop_event(stop_reason: BedrockStopReason) -> ConverseStreamOutput {
        ConverseStreamOutput::MessageStop(
            aws_sdk_bedrockruntime::types::MessageStopEvent::builder()
                .stop_reason(stop_reason)
                .build()
                .expect("message stop event should build"),
        )
    }

    #[test]
    fn converse_metadata_after_message_stop_emits_usage_and_exact_input_tokens() {
        let turn = TurnState::default();
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());

        // AWS documents the order messageStart -> content -> messageStop -> metadata, so
        // `messageStop` must not end the stream before usage arrives.
        assert!(
            accumulator
                .process_event(
                    converse_message_stop_event(BedrockStopReason::EndTurn),
                    &turn
                )
                .expect("message stop should process")
                .is_none(),
            "messageStop must not terminate the stream before the trailing metadata event"
        );

        let outcome = accumulator
            .process_event(converse_metadata_event(47, None, None), &turn)
            .expect("metadata should process")
            .expect("metadata after messageStop should terminate the stream");
        assert_eq!(outcome, StreamOutcome::Finished);

        let events = turn.drain();
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::Usage { usage } if usage.input_tokens == Some(47)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ExactRequestInputTokens { tokens } if tokens.get() == 47
        )));
    }

    #[test]
    fn anthropic_messages_truncated_tool_call_reports_max_tokens() {
        // Reproduces the observed failure: the model starts a `tool_use` block, runs out of output
        // tokens mid-JSON, and Anthropic reports `max_tokens`. Reporting `ToolCall` here would
        // claim a tool call that was never completed, which the runtime cannot execute.
        let turn = TurnState::default();
        let mut accumulator = AnthropicMessagesAccumulator::new(BTreeMap::from([(
            "shell_run".to_string(),
            "shell.run".to_string(),
        )]));
        accumulator
            .process(
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "tool_use", "id": "toolu_trunc", "name": "shell_run"},
                }),
                &turn,
            )
            .expect("tool use start should process");
        accumulator
            .process(
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"echo "},
                }),
                &turn,
            )
            .expect("partial arguments should process");
        accumulator
            .process(
                &serde_json::json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "max_tokens"},
                    "usage": {"output_tokens": 4096},
                }),
                &turn,
            )
            .expect("message delta should process");
        let outcome = accumulator
            .process(&serde_json::json!({"type": "message_stop"}), &turn)
            .expect("message stop should process")
            .expect("message stop should terminate the stream");

        assert_eq!(outcome, StreamOutcome::MaxTokens);
        let events = turn.drain();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::ToolCallFinished { .. })),
            "a truncated tool call must not be reported as finished"
        );
    }

    #[test]
    fn anthropic_messages_completed_tool_call_still_reports_tool_call() {
        let turn = TurnState::default();
        let mut accumulator = AnthropicMessagesAccumulator::new(BTreeMap::from([(
            "shell_run".to_string(),
            "shell.run".to_string(),
        )]));
        accumulator
            .process(
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "tool_use", "id": "toolu_ok", "name": "shell_run"},
                }),
                &turn,
            )
            .expect("tool use start should process");
        accumulator
            .process(
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"command\":\"ls\"}"},
                }),
                &turn,
            )
            .expect("arguments should process");
        accumulator
            .process(
                &serde_json::json!({"type": "content_block_stop", "index": 0}),
                &turn,
            )
            .expect("content block stop should process");
        accumulator
            .process(
                &serde_json::json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                }),
                &turn,
            )
            .expect("message delta should process");
        let outcome = accumulator
            .process(&serde_json::json!({"type": "message_stop"}), &turn)
            .expect("message stop should process")
            .expect("message stop should terminate the stream");

        assert_eq!(outcome, StreamOutcome::ToolCall);
        let events = turn.drain();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderTurnEvent::ToolCallFinished { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn anthropic_messages_unknown_stop_reason_is_not_interpreted() {
        let mut accumulator = AnthropicMessagesAccumulator::new(BTreeMap::new());
        accumulator.record_message_delta_stop_reason(&serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "some_future_reason"},
        }));
        assert_eq!(accumulator.stop_reason, None);
        assert_eq!(accumulator.finish(), StreamOutcome::Finished);
    }

    #[test]
    fn converse_truncated_tool_call_reports_max_tokens() {
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());
        accumulator.saw_tool_call = true;
        accumulator.stop_reason = Some(StopReason::MaxTokens);
        assert_eq!(accumulator.finish_outcome(), StreamOutcome::MaxTokens);
    }

    #[test]
    fn converse_tool_call_turn_still_reports_usage_after_message_stop() {
        let turn = TurnState::default();
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());
        accumulator.tool_calls.insert(
            0,
            ToolCallAccumulator {
                id: Some("call-1".to_string()),
                name: Some("shell_run".to_string()),
                arguments: "{}".to_string(),
            },
        );
        accumulator.saw_tool_call = true;

        assert!(
            accumulator
                .process_event(
                    converse_message_stop_event(BedrockStopReason::ToolUse),
                    &turn
                )
                .expect("message stop should process")
                .is_none()
        );
        let outcome = accumulator
            .process_event(converse_metadata_event(31, None, None), &turn)
            .expect("metadata should process")
            .expect("metadata after messageStop should terminate the stream");
        assert_eq!(outcome, StreamOutcome::ToolCall);

        let events = turn.drain();
        // Tool calls must still be finalized exactly once at messageStop.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderTurnEvent::ToolCallFinished { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ExactRequestInputTokens { tokens } if tokens.get() == 31
        )));
    }

    #[test]
    fn converse_exact_input_tokens_include_cache_reads_and_writes() {
        let turn = TurnState::default();
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());

        accumulator
            .process_event(converse_metadata_event(12, Some(400), Some(80)), &turn)
            .expect("metadata should process");

        let events = turn.drain();
        // Bedrock reports non-cached input in `inputTokens`, so complete context is 12 + 400 + 80.
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ExactRequestInputTokens { tokens } if tokens.get() == 492
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::Usage { usage }
                if usage.input_tokens == Some(12)
                    && usage.cached_input_tokens == Some(400)
                    && usage.cache_write_input_tokens == Some(80)
        )));
    }

    #[test]
    fn converse_stream_truncated_after_message_stop_still_terminates() {
        let turn = TurnState::default();
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());

        accumulator
            .process_event(
                converse_message_stop_event(BedrockStopReason::EndTurn),
                &turn,
            )
            .expect("message stop should process");

        // If the provider never sends `metadata`, end-of-stream must still yield the outcome
        // chosen at `messageStop` rather than hanging or erroring.
        assert_eq!(accumulator.finish_outcome(), StreamOutcome::Finished);
    }

    #[derive(Debug)]
    struct DeterministicBedrockTurnExecutor;

    impl BedrockTurnExecutor for DeterministicBedrockTurnExecutor {
        fn start(
            &self,
            _runtime: &ProviderRuntime,
            request: ModelTurnRequest,
            turn: TurnState,
            _discovery: Arc<Mutex<DiscoveryCache>>,
        ) {
            if turn.is_cancelled() {
                turn.push(ProviderTurnEvent::Cancelled);
                turn.push(ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::Cancelled,
                });
                return;
            }
            if matches!(request.tool_call_policy.choice, ToolChoice::Required) {
                let tool_count = if request.tool_call_policy.parallel == Some(true) {
                    request.tools.len()
                } else {
                    1
                };
                for (index, tool) in request.tools.iter().take(tool_count).enumerate() {
                    let call = ToolCall {
                        id: format!("bedrock-conformance-call-{index}"),
                        name: tool.name.clone(),
                        arguments: serde_json::json!({}),
                    };
                    turn.push(ProviderTurnEvent::ToolCallStarted {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                    });
                    turn.push(ProviderTurnEvent::ToolCallFinished { call });
                }
                turn.push(ProviderTurnEvent::Usage {
                    usage: TokenUsage::default(),
                });
                turn.push(ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::ToolCall,
                });
                return;
            }
            turn.push(ProviderTurnEvent::TextDelta {
                text: "bedrock conformance".to_string(),
            });
            turn.push(ProviderTurnEvent::Usage {
                usage: TokenUsage {
                    input_tokens: Some(3),
                    output_tokens: Some(2),
                    total_tokens: Some(5),
                    ..TokenUsage::default()
                },
            });
            turn.push(ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            });
        }
    }

    struct BedrockPluginInvoker {
        plugin: BedrockProviderPlugin,
    }

    impl Default for BedrockPluginInvoker {
        fn default() -> Self {
            Self {
                plugin: BedrockProviderPlugin {
                    turns: Mutex::default(),
                    discovery: Arc::default(),
                    runtime: ProviderRuntime::new().map_err(|error| error.to_string()),
                    turn_executor: Arc::new(DeterministicBedrockTurnExecutor),
                },
            }
        }
    }

    impl BlockingModelProviderInvoker for BedrockPluginInvoker {
        fn invoke_json<Q, R>(
            &mut self,
            _provider_plugin_id: Option<&str>,
            operation: &'static str,
            request: &Q,
        ) -> Result<R, String>
        where
            Q: serde::Serialize,
            R: serde::de::DeserializeOwned,
        {
            let response = self.plugin.invoke_service_concurrent(NativeServiceContext {
                plugin_id: PROVIDER_ID.to_string(),
                request: ServiceRequest {
                    interface_id: MODEL_PROVIDER_INTERFACE_ID_V2.to_string(),
                    operation: operation.to_string(),
                    payload: serde_json::to_vec(request).map_err(|error| error.to_string())?,
                },
                config: PluginConfigContext::default(),
                events: ServiceEventEmitter::default(),
                cancellation: ServiceCancellation::default(),
                bridge: ServiceBridge::default(),
                transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            });
            if let Some(error) = response.error {
                return Err(format!("{}: {}", error.code, error.message));
            }
            serde_json::from_slice(&response.payload).map_err(|error| error.to_string())
        }
    }

    #[test]
    fn bedrock_adapter_passes_public_deterministic_conformance_suite() {
        let options = ProviderConformanceOptions {
            provider_context: ProviderRequestContext {
                settings: BTreeMap::from([
                    ("model".to_string(), "test-model".to_string()),
                    ("models".to_string(), "test-model".to_string()),
                ]),
                ..ProviderRequestContext::default()
            },
            model_id: Some("test-model".to_string()),
            turn_timeout: Duration::from_secs(2),
            ..ProviderConformanceOptions::default()
        };

        let report = run_provider_conformance_suite(&mut BedrockPluginInvoker::default(), &options)
            .expect("Bedrock adapter should satisfy deterministic conformance");

        assert_eq!(report.provider.provider_id, PROVIDER_ID);
        assert!(report.cases.iter().all(|case| {
            case.outcome == ProviderConformanceOutcome::Passed
                || matches!(case.outcome, ProviderConformanceOutcome::Skipped { .. })
        }));
        for required_case in [
            "baseline turn",
            "tool calling",
            "prompt caching",
            "cancellation",
        ] {
            assert!(report.cases.iter().any(|case| {
                case.name == required_case && case.outcome == ProviderConformanceOutcome::Passed
            }));
        }
    }

    #[test]
    fn bedrock_service_unavailable_is_overload_from_sdk_error() {
        let error = ConverseStreamError::ServiceUnavailableException(
            aws_sdk_bedrockruntime::types::error::ServiceUnavailableException::builder()
                .message("service unavailable")
                .build(),
        );

        assert_eq!(
            bedrock_converse_stream_error_category(&error),
            ProviderErrorCategory::Overloaded
        );
    }

    #[test]
    fn bedrock_throttling_remains_rate_limit_from_sdk_error() {
        let error = ConverseStreamError::ThrottlingException(
            aws_sdk_bedrockruntime::types::error::ThrottlingException::builder()
                .message("too many requests")
                .build(),
        );

        assert_eq!(
            bedrock_converse_stream_error_category(&error),
            ProviderErrorCategory::RateLimit
        );
    }

    #[test]
    fn bedrock_overload_message_fallback_is_conservative() {
        assert_eq!(
            bedrock_error_category_from_message("ServiceUnavailableException: service unavailable"),
            ProviderErrorCategory::Overloaded
        );
        assert_eq!(
            bedrock_error_category_from_message("ThrottlingException: too many requests"),
            ProviderErrorCategory::RateLimit
        );
    }

    #[test]
    fn json_document_round_trip_preserves_objects() {
        let value = serde_json::json!({"path":"/tmp/file", "count": 2, "ok": true});
        let document = json_value_to_document(&value);
        assert_eq!(document_to_json_value(&document), value);
    }

    #[test]
    fn bedrock_provider_cancel_turn_signals_active_adapter_state() {
        let plugin = BedrockProviderPlugin::default();
        let (provider_turn_id, turn) = plugin
            .turns
            .lock()
            .expect("turn store")
            .insert_started("cancel-me");
        let response = plugin.cancel_turn(&ServiceRequest {
            interface_id: MODEL_PROVIDER_INTERFACE_ID.to_owned(),
            operation: OP_CANCEL_TURN.to_owned(),
            payload: serde_json::to_vec(&CancelTurnRequest { provider_turn_id })
                .expect("cancel request"),
        });
        assert!(response.error.is_none());
        assert!(turn.is_cancelled());
    }

    #[test]
    fn bedrock_reasoning_text_and_redaction_use_neutral_activity_events() {
        let turn = TurnState::default();
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());

        accumulator.process_reasoning_delta(
            2,
            &ReasoningContentBlockDelta::Text("raw detail".to_owned()),
            &turn,
        );
        accumulator.process_reasoning_delta(
            2,
            &ReasoningContentBlockDelta::RedactedContent(Blob::new("secret")),
            &turn,
        );
        accumulator.finish_reasoning(&turn);

        let events = turn.drain();
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ReasoningActivity {
                event: bcode_session_models::ReasoningActivityEvent::PartDelta {
                    kind: bcode_session_models::ReasoningContentKind::Raw,
                    text,
                    ..
                }
            } if text == "raw detail"
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
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    ..
                }
            }
        )));
        assert!(
            events
                .iter()
                .all(|event| !format!("{event:?}").contains("secret"))
        );
    }

    #[test]
    fn tool_use_delta_emits_progress_event_when_call_id_is_known() {
        let turn = TurnState::default();
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());
        accumulator.tool_calls.insert(
            0,
            ToolCallAccumulator {
                id: Some("call-1".to_string()),
                name: Some("filesystem_write".to_string()),
                arguments: String::new(),
            },
        );

        accumulator.process_tool_use_delta(0, "{\"path\"", &turn);

        assert!(turn.drain().iter().any(|event| matches!(
            event,
            ProviderTurnEvent::ToolCallDelta { call_id, delta }
                if call_id == "call-1" && delta == "{\"path\""
        )));
    }

    #[test]
    fn completed_tool_calls_preserve_bedrock_order_and_exact_ids() {
        let turn = TurnState::default();
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());
        accumulator.tool_calls.extend([
            (
                1,
                ToolCallAccumulator {
                    id: Some("bedrock-call-second".to_owned()),
                    name: Some("second_tool".to_owned()),
                    arguments: r#"{"position":2}"#.to_owned(),
                },
            ),
            (
                0,
                ToolCallAccumulator {
                    id: Some("bedrock-call-first".to_owned()),
                    name: Some("first_tool".to_owned()),
                    arguments: r#"{"position":1}"#.to_owned(),
                },
            ),
        ]);

        accumulator
            .finish_tool_calls(&turn)
            .expect("Bedrock tool calls should finish");
        let completed = turn
            .drain()
            .into_iter()
            .filter_map(|event| match event {
                ProviderTurnEvent::ToolCallFinished { call } => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            completed
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            ["bedrock-call-first", "bedrock-call-second"]
        );
        assert_eq!(completed[0].arguments["position"], 1);
        assert_eq!(completed[1].arguments["position"], 2);
    }

    #[test]
    fn malformed_bedrock_tool_call_is_rejected_without_partial_completion() {
        let turn = TurnState::default();
        let mut accumulator = StreamAccumulator::new(BTreeMap::new());
        accumulator.tool_calls.insert(
            0,
            ToolCallAccumulator {
                id: Some("bedrock-call-malformed".to_owned()),
                name: Some("broken_tool".to_owned()),
                arguments: r#"{"unterminated""#.to_owned(),
            },
        );

        let error = accumulator
            .finish_tool_calls(&turn)
            .expect_err("malformed Bedrock arguments must fail");
        assert_eq!(error.code, "tool_arguments_decode_failed");
        assert!(
            !turn
                .drain()
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::ToolCallFinished { .. }))
        );
    }

    #[test]
    fn bedrock_tool_names_are_sanitized() {
        assert_eq!(bedrock_tool_name("filesystem.read"), "filesystem_read");
    }

    #[test]
    fn explicit_bedrock_model_infos_are_raw_provider_candidates() {
        let models = model_infos_from_ids(
            &["anthropic.claude-3-5-sonnet-20241022-v2:0".to_string()],
            None,
        );

        assert_eq!(models[0].context_window, None);
        assert_eq!(models[0].max_output_tokens, None);
    }

    #[test]
    fn unknown_bedrock_model_infos_are_raw_provider_candidates() {
        let models = model_infos_from_ids(&["provider.future-model-v1:0".to_string()], None);

        assert_eq!(models[0].context_window, None);
        assert_eq!(models[0].max_output_tokens, None);
    }

    #[test]
    fn historical_tool_use_names_are_sanitized_for_bedrock() {
        let message = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                call: ToolCall {
                    id: "tooluse_1".to_string(),
                    name: "shell.run".to_string(),
                    arguments: serde_json::json!({"command":"git status"}),
                },
            }],
        };
        let blocks = bedrock_content_blocks(&message).expect("tool call should convert");

        let BedrockContentBlock::ToolUse(tool_use) = &blocks[0] else {
            panic!("expected tool use block");
        };
        assert_eq!(tool_use.name(), "shell_run");
    }

    #[test]
    fn cache_hints_emit_bedrock_cache_points() {
        let request = ModelTurnRequest {
            session_id: "00000000-0000-0000-0000-000000000000"
                .parse()
                .expect("static nil UUID should parse"),
            turn_id: "turn".to_string(),
            model_id: "model".to_string(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: Some("stable".to_string()),
            messages: vec![ModelMessage {
                role: MessageRole::User,
                content: vec![
                    ContentBlock::Text {
                        text: "hello".to_string(),
                    },
                    ContentBlock::CachePoint {
                        hint: bcode_model::PromptCachePoint::default(),
                    },
                ],
            }],
            tools: vec![ToolDefinition {
                name: "filesystem.read".to_string(),
                description: "read".to_string(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
            structured_output: None,
            context_management: bcode_model::ContextManagementRequest::default(),
            parameters: bcode_model::ModelParameters::default(),
            prompt_cache: bcode_model::PromptCacheHints {
                mode: bcode_model::PromptCacheMode::Auto,
                cache_system_prompt: true,
                cache_tools: true,
            },
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: BTreeMap::default(),
        };

        let system = system_blocks(&request);
        assert!(matches!(system[1], SystemContentBlock::CachePoint(_)));
        let messages = model_messages_to_bedrock_messages(&request).expect("messages convert");
        assert!(matches!(
            messages[0].content().last(),
            Some(BedrockContentBlock::CachePoint(_))
        ));
        let tool_config = model_tools_to_bedrock_tool_config(&request)
            .expect("tools convert")
            .expect("tool config should exist");
        assert!(matches!(
            tool_config.tools().last(),
            Some(Tool::CachePoint(_))
        ));
        assert!(
            tool_config
                .tool_choice()
                .is_some_and(BedrockToolChoice::is_auto)
        );
    }

    #[test]
    fn bedrock_rejects_unmapped_cache_ttl() {
        let mut request = test_model_turn_request();
        request.messages.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::CachePoint {
                hint: bcode_model::PromptCachePoint {
                    label: None,
                    ttl_seconds: Some(300),
                },
            }],
        });

        let error = validate_bedrock_request(&request).expect_err("TTL must be rejected");
        assert_eq!(error.code, "bedrock_cache_ttl_unsupported");
    }

    #[test]
    fn bedrock_granular_capabilities_distinguish_transport_and_unknown_model() {
        use bcode_model::{
            CapabilitySupport, ModelFeatureSupport, ModelParameterKey, RequestedModelFeature,
            StructuredOutputMode,
        };
        let provider = bedrock_feature_support();
        assert!(provider.has_complete_inventory());
        assert!(
            provider
                .parameter(ModelParameterKey::Temperature)
                .is_guaranteed()
        );
        assert!(matches!(
            provider.structured_output(StructuredOutputMode::StrictJsonSchema),
            CapabilitySupport::Unsupported { .. }
        ));
        assert!(
            provider
                .parameter(ModelParameterKey::ReasoningBudgetTokens)
                .is_guaranteed(),
            "Bedrock now maps extended thinking to a reasoning budget"
        );
        assert!(
            provider
                .parameter(ModelParameterKey::ReasoningEffort)
                .is_guaranteed()
        );
        assert!(matches!(
            provider.parameter(ModelParameterKey::ReasoningSummary),
            CapabilitySupport::Unsupported { .. }
        ));
        assert!(matches!(
            provider.negotiate(
                &ModelFeatureSupport::default(),
                RequestedModelFeature::Parameter(ModelParameterKey::Temperature)
            ),
            bcode_model::NegotiatedFeatureSupport::Unknown {
                scope: bcode_model::CapabilityScope::Model
            }
        ));
    }

    #[test]
    fn bedrock_rejects_unsupported_correctness_sensitive_controls() {
        let mut request = test_model_turn_request();
        request.structured_output = Some(bcode_model::StructuredOutputRequest {
            name: "Output".to_string(),
            schema: serde_json::json!({"type": "object"}),
            strict: true,
        });
        let error = validate_bedrock_request(&request).expect_err("schema mode must fail");
        assert_eq!(error.category, ProviderErrorCategory::UnsupportedFeature);

        request.structured_output = None;
        request.parameters.reasoning_effort = Some(bcode_model::ReasoningEffort::High);
        validate_bedrock_request(&request)
            .expect("reasoning effort is now mapped to extended thinking");

        request.parameters = bcode_model::ModelParameters::default();
        request.parameters.reasoning_summary = Some("detailed".to_string());
        let error = validate_bedrock_request(&request).expect_err("reasoning summary must fail");
        assert_eq!(error.code, "bedrock_reasoning_summary_unsupported");

        request.parameters = bcode_model::ModelParameters::default();
        request.tool_call_policy.parallel = Some(true);
        let error = validate_bedrock_request(&request).expect_err("parallel policy must fail");
        assert_eq!(error.code, "bedrock_parallel_tool_policy_unsupported");
    }

    #[test]
    fn bedrock_thinking_fields_from_reasoning_effort() {
        let params = bcode_model::ModelParameters::default();
        assert!(bedrock_thinking_fields(&params).is_none());

        let params = bcode_model::ModelParameters {
            reasoning_effort: Some(bcode_model::ReasoningEffort::High),
            ..Default::default()
        };
        let fields = bedrock_thinking_fields(&params).expect("thinking fields");
        let Document::Object(root) = fields else {
            panic!("thinking fields must be an object");
        };
        let Some(Document::Object(thinking)) = root.get("thinking") else {
            panic!("thinking key must be an object");
        };
        assert_eq!(
            thinking.get("type"),
            Some(&Document::String("enabled".to_string()))
        );
        assert_eq!(
            thinking.get("budget_tokens"),
            Some(&Document::Number(Number::PosInt(u64::from(
                REASONING_EFFORT_HIGH_BUDGET
            ))))
        );
    }

    #[test]
    fn bedrock_adaptive_thinking_sends_effort_in_output_config() {
        let params = bcode_model::ModelParameters {
            reasoning_control: Some(bcode_model::ReasoningControl::Adaptive),
            reasoning_effort_value: Some("xhigh".to_string()),
            ..Default::default()
        };
        let Some(Document::Object(root)) = bedrock_thinking_fields(&params) else {
            panic!("adaptive thinking fields must be an object");
        };
        let Some(Document::Object(thinking)) = root.get("thinking") else {
            panic!("thinking key must be an object");
        };
        assert_eq!(
            thinking.get("type"),
            Some(&Document::String("adaptive".to_string()))
        );
        assert!(
            !thinking.contains_key("budget_tokens"),
            "adaptive thinking must never send a token budget"
        );
        assert!(
            !thinking.contains_key("effort"),
            "effort nested inside thinking is rejected by Bedrock"
        );
        let Some(Document::Object(output_config)) = root.get("output_config") else {
            panic!("output_config must be a sibling object");
        };
        assert_eq!(
            output_config.get("effort"),
            Some(&Document::String("xhigh".to_string()))
        );
    }

    #[test]
    fn bedrock_adaptive_thinking_ignores_budget_only_requests() {
        let params = bcode_model::ModelParameters {
            reasoning_control: Some(bcode_model::ReasoningControl::Adaptive),
            reasoning_budget_tokens: Some(8_192),
            ..Default::default()
        };
        let Some(Document::Object(root)) = bedrock_thinking_fields(&params) else {
            panic!("adaptive models still request adaptive thinking");
        };
        let Some(Document::Object(thinking)) = root.get("thinking") else {
            panic!("thinking key must be an object");
        };
        assert_eq!(
            thinking.get("type"),
            Some(&Document::String("adaptive".to_string()))
        );
        assert!(
            !root.contains_key("output_config"),
            "a token budget carries no adaptive effort name"
        );
    }

    #[test]
    fn bedrock_budget_control_keeps_enabled_thinking_shape() {
        let params = bcode_model::ModelParameters {
            reasoning_control: Some(bcode_model::ReasoningControl::Budget),
            reasoning_effort_value: Some("high".to_string()),
            ..Default::default()
        };
        let Some(Document::Object(root)) = bedrock_thinking_fields(&params) else {
            panic!("budget thinking fields must be an object");
        };
        let Some(Document::Object(thinking)) = root.get("thinking") else {
            panic!("thinking key must be an object");
        };
        assert_eq!(
            thinking.get("type"),
            Some(&Document::String("enabled".to_string()))
        );
        assert!(!root.contains_key("output_config"));
    }

    #[test]
    fn bedrock_thinking_budget_prefers_explicit_tokens() {
        let mut params = bcode_model::ModelParameters {
            reasoning_effort: Some(bcode_model::ReasoningEffort::Low),
            reasoning_budget_tokens: Some(8_192),
            ..Default::default()
        };
        assert_eq!(resolve_reasoning_budget_tokens(&params), Some(8_192));

        params.reasoning_budget_tokens = Some(0);
        assert_eq!(
            resolve_reasoning_budget_tokens(&params),
            Some(REASONING_EFFORT_LOW_BUDGET),
            "a zero explicit budget falls back to the effort mapping"
        );
    }

    #[test]
    fn bedrock_rejects_unmapped_provider_options_and_reuse() {
        let mut request = test_model_turn_request();
        request.provider_context.request.insert(
            "unsupported".to_string(),
            bcode_model::ProviderRequestValue::Bool(true),
        );
        let error = validate_bedrock_request(&request).expect_err("provider option must fail");
        assert_eq!(error.code, "bedrock_provider_options_unsupported");

        request.provider_context.request.clear();
        request.conversation_reuse.mode = bcode_model::ConversationReuseMode::Auto;
        let error = validate_bedrock_request(&request).expect_err("reuse must fail");
        assert_eq!(error.code, "bedrock_conversation_reuse_unsupported");
    }

    #[test]
    fn bedrock_tool_choice_none_omits_tools() {
        let mut request = test_model_turn_request();
        request.tools.push(ToolDefinition {
            name: "filesystem.read".to_string(),
            description: "read".to_string(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        request.tool_call_policy.choice = ToolChoice::None;

        assert!(
            model_tools_to_bedrock_tool_config(&request)
                .expect("none choice should project")
                .is_none()
        );
    }

    #[test]
    fn bedrock_required_and_specific_tool_choices_are_typed() {
        let mut request = test_model_turn_request();
        request.tools.push(ToolDefinition {
            name: "filesystem.read".to_string(),
            description: "read".to_string(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        request.tool_call_policy.choice = ToolChoice::Required;
        let required = model_tools_to_bedrock_tool_config(&request)
            .expect("required choice should project")
            .expect("required choice needs tool config");
        assert!(
            required
                .tool_choice()
                .is_some_and(BedrockToolChoice::is_any)
        );

        request.tool_call_policy.choice = ToolChoice::Tool {
            name: "filesystem.read".to_string(),
        };
        let specific = model_tools_to_bedrock_tool_config(&request)
            .expect("specific choice should project")
            .expect("specific choice needs tool config");
        let selected = specific
            .tool_choice()
            .and_then(|choice| choice.as_tool().ok())
            .expect("specific Bedrock choice");
        assert_eq!(selected.name(), "filesystem_read");
    }

    #[test]
    fn bedrock_rejects_unknown_specific_tool_choice() {
        let mut request = test_model_turn_request();
        request.tools.push(ToolDefinition {
            name: "filesystem.read".to_string(),
            description: "read".to_string(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        request.tool_call_policy.choice = ToolChoice::Tool {
            name: "missing".to_string(),
        };

        let error = model_tools_to_bedrock_tool_config(&request)
            .expect_err("unknown required tool must fail");
        assert_eq!(error.code, "unknown_required_tool");
        assert_eq!(error.category, ProviderErrorCategory::InvalidRequest);
    }

    fn test_model_turn_request() -> ModelTurnRequest {
        ModelTurnRequest {
            session_id: "00000000-0000-0000-0000-000000000000"
                .parse()
                .expect("static nil UUID should parse"),
            turn_id: "turn".to_string(),
            model_id: "model".to_string(),
            provider_context: ProviderRequestContext::default(),
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
            parameters: bcode_model::ModelParameters::default(),
            structured_output: None,
            context_management: bcode_model::ContextManagementRequest::default(),
            prompt_cache: bcode_model::PromptCacheHints::default(),
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn persisted_compatibility_tracks_prompt_cache_unsupported() {
        let key = DiscoveryCacheKey {
            region: "us-east-1".to_string(),
            aws_profile: None,
            endpoint_url: None,
        };
        let mut compatibility = PersistedCompatibilityCache::default();
        compatibility.mark_prompt_cache_unsupported(&key, "model", "no cache", 10);

        assert!(
            compatibility
                .unsupported_prompt_cache_for(&key)
                .contains("model")
        );
        assert!(compatibility.unsupported_streaming_for(&key).is_empty());
    }

    #[test]
    fn model_list_includes_default_first() {
        let mut settings = Settings::resolve(None);
        settings.default_model = Some("model-b".to_string());
        settings.model_ids = vec!["model-b".to_string(), "model-a".to_string()];
        let metadata = diagnostics_metadata(&settings);
        assert_eq!(metadata.get("default_model"), Some(&"model-b".to_string()));
    }

    #[test]
    fn apply_default_model_preserves_full_discovered_list() {
        // A selected/configured model must mark the default without collapsing the picker to a
        // single entry (regression: only the configured model was shown).
        let mut models = model_infos_from_ids(
            &[
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
                "us.anthropic.claude-opus-4-8-20250101-v1:0".to_string(),
                "us.anthropic.claude-haiku-4-5-20250101-v1:0".to_string(),
            ],
            None,
        );

        apply_default_model_to_list(
            &mut models,
            Some("us.anthropic.claude-opus-4-8-20250101-v1:0"),
        );

        assert_eq!(models.len(), 3, "the full list must be preserved");
        let default = models
            .iter()
            .filter(|model| model.is_default)
            .collect::<Vec<_>>();
        assert_eq!(default.len(), 1);
        assert_eq!(
            default[0].model_id,
            "us.anthropic.claude-opus-4-8-20250101-v1:0"
        );
    }

    #[test]
    fn apply_default_model_adds_missing_pinned_model() {
        let mut models = model_infos_from_ids(
            &["us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string()],
            None,
        );

        apply_default_model_to_list(
            &mut models,
            Some("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        );

        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0].model_id,
            "anthropic.claude-3-5-sonnet-20241022-v2:0"
        );
        assert!(models[0].is_default);
    }

    #[test]
    fn apply_default_model_none_leaves_list_untouched() {
        let mut models = model_infos_from_ids(&["a".to_string(), "b".to_string()], Some("b"));
        apply_default_model_to_list(&mut models, None);
        assert_eq!(models.len(), 2);
        assert!(
            models
                .iter()
                .find(|m| m.model_id == "b")
                .unwrap()
                .is_default
        );
    }

    #[test]
    fn model_unusable_via_converse_detects_data_retention_error() {
        let error = provider_error(
            "bedrock_request_failed",
            ProviderErrorCategory::InvalidRequest,
            "The model returned the following errors: data retention mode 'default' is not available for this model",
        );
        assert!(model_unusable_via_converse(&error));

        let unrelated = provider_error(
            "bedrock_request_failed",
            ProviderErrorCategory::InvalidRequest,
            "some other validation problem",
        );
        assert!(!model_unusable_via_converse(&unrelated));

        let wrong_category = provider_error(
            "bedrock_request_failed",
            ProviderErrorCategory::RateLimit,
            "data retention mode 'default' is not available for this model",
        );
        assert!(!model_unusable_via_converse(&wrong_category));
    }

    #[test]
    fn persisted_compatibility_filters_discovery() {
        let key = DiscoveryCacheKey {
            region: "us-east-1".to_string(),
            aws_profile: Some("work".to_string()),
            endpoint_url: None,
        };
        let mut compatibility = PersistedCompatibilityCache::default();
        compatibility.mark_streaming_tool_unsupported(&key, "bad-model", "unsupported", 10);
        let discovery = ModelDiscovery {
            models: model_infos_from_ids(
                &["bad-model".to_string(), "good-model".to_string()],
                None,
            ),
            default_model_id: Some("bad-model".to_string()),
        };
        let filtered = filtered_discovery(
            &discovery,
            &compatibility.unsupported_streaming_for(&key),
            &BTreeSet::new(),
        );
        assert_eq!(filtered.default_model_id, Some("good-model".to_string()));
    }

    #[test]
    fn persisted_prompt_cache_incompatibility_removes_cache_capabilities() {
        let key = DiscoveryCacheKey {
            region: "us-east-1".to_string(),
            aws_profile: None,
            endpoint_url: None,
        };
        let mut compatibility = PersistedCompatibilityCache::default();
        compatibility.mark_prompt_cache_unsupported(&key, "no-cache", "unsupported", 10);
        let discovery = ModelDiscovery {
            models: model_infos_from_ids(&["no-cache".to_string(), "cache-ok".to_string()], None),
            default_model_id: Some("no-cache".to_string()),
        };

        let filtered = filtered_discovery(
            &discovery,
            &BTreeSet::new(),
            &compatibility.unsupported_prompt_cache_for(&key),
        );

        assert!(
            !filtered.models[0]
                .capabilities
                .contains(&ModelCapability::PromptCaching)
        );
        assert!(filtered.models[0].cache.capabilities.is_empty());
        assert!(
            filtered.models[1]
                .capabilities
                .contains(&ModelCapability::PromptCaching)
        );
    }

    #[test]
    fn context_bearer_token_is_resolved_from_auth_credentials() {
        let settings = Settings {
            transport: Ok(BedrockTransport::Runtime),
            mantle_base_url: None,
            mantle_auth_header: false,
            force_http1: false,
            default_model: None,
            model_ids: Vec::new(),
            model_ids_are_explicit: false,
            region: Some("us-east-1".to_string()),
            region_source: RegionSource::Profile,
            aws_profile: None,
            endpoint_url: None,
            auth_credentials: BTreeMap::from([(
                "bearer_token".to_string(),
                "bedrock-token".to_string(),
            )]),
            env: BTreeMap::new(),
            config_source: "test".to_string(),
        };

        assert_eq!(
            client_context_bearer_token(&settings).as_deref(),
            Some("bedrock-token")
        );
        assert_eq!(
            diagnostics_metadata(&settings)
                .get("bearer_token_source")
                .map(String::as_str),
            Some("provider_auth_context")
        );
    }

    #[test]
    fn dispatch_error_preserves_connector_root_cause() {
        let connector = aws_smithy_runtime_api::client::result::ConnectorError::io(Box::new(
            std::io::Error::from(std::io::ErrorKind::NetworkUnreachable),
        ));
        let dispatch = aws_smithy_runtime_api::client::result::DispatchFailure::builder()
            .source(connector)
            .build();

        let error = bedrock_dispatch_error(
            "bedrock_request_failed",
            "Bedrock runtime",
            &dispatch,
            bcode_model::ProviderFailureCapability::ModelInvocation,
        );

        assert_eq!(error.category, ProviderErrorCategory::Network);
        assert_eq!(
            error
                .diagnostic_context
                .get("connector_error_kind")
                .map(String::as_str),
            Some("io")
        );
        assert_eq!(
            error
                .diagnostic_context
                .get("io_error_kind")
                .map(String::as_str),
            Some("network_unreachable")
        );
        assert_eq!(
            error
                .diagnostic_context
                .get("root_error_source")
                .map(String::as_str),
            Some("io_root")
        );
        assert_eq!(
            error
                .diagnostic_context
                .get("root_error_message")
                .map(String::as_str),
            Some("network unreachable")
        );
        assert!(matches!(
            error.sources.as_slice(),
            [
                ProviderErrorSource { source: aws, code: Some(dispatch), .. },
                ProviderErrorSource { source: root, code: Some(root_code), .. }
            ] if aws == "aws_sdk"
                && dispatch == "dispatch"
                && root == "io_root"
                && root_code == "network_unreachable"
        ));
    }

    #[test]
    fn dispatch_error_classifies_tls_unknown_issuer() {
        let tls = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let connector = aws_smithy_runtime_api::client::result::ConnectorError::io(Box::new(
            std::io::Error::new(std::io::ErrorKind::InvalidData, tls),
        ));
        let dispatch = aws_smithy_runtime_api::client::result::DispatchFailure::builder()
            .source(connector)
            .build();

        let error = bedrock_dispatch_error(
            "bedrock_request_failed",
            "Bedrock runtime",
            &dispatch,
            bcode_model::ProviderFailureCapability::ModelInvocation,
        );

        assert_eq!(
            error
                .diagnostic_context
                .get("tls_error_kind")
                .map(String::as_str),
            Some("certificate_unknown_issuer")
        );
        assert_eq!(
            error
                .diagnostic_context
                .get("root_error_source")
                .map(String::as_str),
            Some("rustls_root")
        );
    }

    #[test]
    fn dispatch_error_classifies_missing_credentials_as_non_retryable_auth() {
        let connector = aws_smithy_runtime_api::client::result::ConnectorError::other(
            Box::new(std::io::Error::other(
                "no credentials found in chain. Attempted: Environment: the credential provider was not enabled",
            )),
            None,
        );
        let dispatch = aws_smithy_runtime_api::client::result::DispatchFailure::builder()
            .source(connector)
            .build();

        let error = bedrock_dispatch_error(
            "bedrock_request_failed",
            "Bedrock runtime",
            &dispatch,
            bcode_model::ProviderFailureCapability::ModelInvocation,
        );

        assert_eq!(error.category, ProviderErrorCategory::Auth);
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "AWS credentials could not be resolved for Bedrock"
        );
        assert_eq!(
            error
                .diagnostic_context
                .get("auth_failure_kind")
                .map(String::as_str),
            Some("credential_chain_exhausted")
        );
        let failure = error.failure.expect("actionable auth failure");
        assert_eq!(failure.source, "aws_sdk_credential_chain");
        assert!(failure.remediation.contains("AWS_PROFILE"));
    }

    #[test]
    fn config_transport_failure_is_actionable_and_secret_safe() {
        let error = bedrock_transport_error("construction", ProviderErrorCategory::Config);
        let failure = error.failure.expect("config failure context");

        assert_eq!(failure.provider_id, PROVIDER_ID);
        assert_eq!(
            failure.capability,
            bcode_model::ProviderFailureCapability::ModelDiscovery
        );
        assert!(failure.is_actionable());
        assert!(
            !serde_json::to_string(&failure)
                .expect("failure should encode")
                .contains("secret_access_key")
        );
    }

    #[test]
    fn persisted_compatibility_updates_timestamps() {
        let key = DiscoveryCacheKey {
            region: "us-east-1".to_string(),
            aws_profile: None,
            endpoint_url: None,
        };
        let mut compatibility = PersistedCompatibilityCache::default();
        compatibility.mark_streaming_tool_unsupported(&key, "model", "first", 10);
        compatibility.mark_streaming_tool_unsupported(&key, "model", "second", 20);
        let record = compatibility.entries[0]
            .unsupported_streaming_tool_models
            .get("model")
            .expect("model should be recorded");
        assert_eq!(record.first_seen_unix_seconds, 10);
        assert_eq!(record.last_seen_unix_seconds, 20);
        assert_eq!(record.message, "second");
    }

    #[test]
    fn persisted_compatibility_prunes_expired_records() {
        let key = DiscoveryCacheKey {
            region: "us-east-1".to_string(),
            aws_profile: None,
            endpoint_url: None,
        };
        let mut compatibility = PersistedCompatibilityCache::default();
        compatibility.mark_streaming_tool_unsupported(&key, "stale", "old", 1);
        compatibility.mark_streaming_tool_unsupported(
            &key,
            "fresh",
            "new",
            COMPATIBILITY_CACHE_TTL_SECONDS + 1,
        );
        compatibility.prune_expired(COMPATIBILITY_CACHE_TTL_SECONDS + 2);
        let unsupported = compatibility.unsupported_streaming_for(&key);
        assert!(!unsupported.contains("stale"));
        assert!(unsupported.contains("fresh"));
    }

    #[test]
    fn persisted_compatibility_save_load_round_trip() {
        let root = unique_temp_dir();
        let path = root.join("compatibility-cache-v1.json");
        let key = DiscoveryCacheKey {
            region: "us-east-1".to_string(),
            aws_profile: None,
            endpoint_url: Some("https://example.com".to_string()),
        };
        let mut compatibility = PersistedCompatibilityCache::default();
        compatibility.mark_streaming_tool_unsupported(&key, "model", "message", now_unix_seconds());
        save_compatibility_cache_to_path(&path, &compatibility).expect("cache should save");
        let loaded = load_compatibility_cache_from_path(&path).expect("cache should load");
        assert!(loaded.unsupported_streaming_for(&key).contains("model"));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    fn unique_temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("bcode-bedrock-test-{nanos}"))
    }
}
