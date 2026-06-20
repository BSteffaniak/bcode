#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Amazon Bedrock model provider plugin for Bcode.

use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_bedrock as bedrock;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::DisplayErrorContext;
use aws_sdk_bedrockruntime::types::{
    CachePointBlock, CachePointType, ContentBlock as BedrockContentBlock, ContentBlockDelta,
    ContentBlockStart, ConversationRole, ConverseStreamOutput, ImageBlock, ImageFormat,
    ImageSource, InferenceConfiguration, Message as BedrockMessage,
    StopReason as BedrockStopReason, SystemContentBlock, Tool, ToolConfiguration, ToolInputSchema,
    ToolResultBlock, ToolResultContentBlock, ToolResultStatus, ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::Blob;
use aws_smithy_types::{Document, Number};
use base64::Engine as _;
use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    MessageRole, ModelCapability, ModelInfo, ModelList, ModelListRequest, ModelMessage,
    ModelTurnRequest, OP_CANCEL_TURN, OP_CAPABILITIES, OP_FINISH_TURN, OP_MODELS,
    OP_POLL_TURN_EVENTS, OP_START_TURN, OP_VALIDATE_CONFIG, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderCapabilities, ProviderCapability, ProviderError,
    ProviderErrorCategory, ProviderRequestContext, ProviderRequestProjection, ProviderTurnEvent,
    StartTurnResponse, StopReason, TokenUsage, ToolCall, ToolDefinition, ValidateConfigResponse,
};
use bcode_model_provider_runtime::{
    ProviderRuntime, StreamOutcome, TurnState, TurnStore, provider_error,
};
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROVIDER_ID: &str = "bcode.bedrock";
const DEFAULT_REGION: &str = "us-east-1";
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
}

impl Default for BedrockProviderPlugin {
    fn default() -> Self {
        Self {
            turns: Mutex::default(),
            discovery: Arc::default(),
            runtime: ProviderRuntime::new().map_err(|error| error.to_string()),
        }
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

    fn start_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<ModelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let (provider_turn_id, turn) = self
            .turns
            .lock()
            .expect("bedrock turn store lock should not be poisoned")
            .insert_started("bedrock-turn");
        turn.push(ProviderTurnEvent::RequestProjection {
            projection: bedrock_request_projection(&request),
        });
        match &self.runtime {
            Ok(runtime) => {
                let discovery = self.discovery.clone();
                runtime.spawn(async move {
                    stream_bedrock_turn(&request, &turn, discovery).await;
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

async fn stream_bedrock_turn_inner(
    request: &ModelTurnRequest,
    turn: &TurnState,
    discovery: Arc<Mutex<DiscoveryCache>>,
) -> Result<StreamOutcome, ProviderError> {
    let settings = Settings::resolve(Some(request));
    let client = bedrock_client(&settings).await;
    let selection = resolve_turn_model_selection(request, &settings, turn, &discovery).await?;
    let name_map = bedrock_tool_name_map(&request.tools);
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
        match builder.send().await {
            Ok(response) => {
                return read_bedrock_stream(response.stream, turn, name_map.clone()).await;
            }
            Err(error) => {
                let error = bedrock_sdk_error(&error);
                if prompt_cache_rejected(&error) && request_for_model.prompt_cache.mode.is_enabled()
                {
                    turn.push(ProviderTurnEvent::Warning {
                        message: format!(
                            "Bedrock model {model_id} rejected prompt cache points; retrying without explicit cache points"
                        ),
                    });
                    mark_prompt_cache_unsupported(
                        &discovery,
                        selection.cache_key.as_ref(),
                        model_id,
                        &error.message,
                    );
                    return retry_bedrock_without_prompt_cache(
                        &client,
                        request,
                        model_id,
                        turn,
                        name_map.clone(),
                    )
                    .await;
                }
                if !selection.explicit
                    && streaming_tool_use_unsupported(&error)
                    && selection.model_ids.last() != Some(model_id)
                {
                    mark_streaming_tool_unsupported(
                        &discovery,
                        selection.cache_key.as_ref(),
                        model_id,
                        &error.message,
                    );
                    turn.push(ProviderTurnEvent::Warning {
                        message: format!(
                            "Bedrock model {model_id} does not support streaming tool use; retrying another discovered model"
                        ),
                    });
                    last_error = Some(error);
                    continue;
                }
                return Err(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        provider_error(
            "bedrock_model_discovery_empty",
            ProviderErrorCategory::Config,
            "Bedrock model discovery returned no usable streaming tool-use models; set BCODE_BEDROCK_MODEL or configure a Bedrock model profile",
        )
    }))
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
    match retry_builder.send().await {
        Ok(response) => read_bedrock_stream(response.stream, turn, name_map).await,
        Err(retry_error) => Err(bedrock_sdk_error(&retry_error)),
    }
}

async fn bedrock_client(settings: &Settings) -> Client {
    let config = bedrock_sdk_config(settings).await;
    Client::new(&config)
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
    name_map: BTreeMap<String, String>,
}

impl StreamAccumulator {
    const fn new(name_map: BTreeMap<String, String>) -> Self {
        Self {
            tool_calls: BTreeMap::new(),
            saw_tool_call: false,
            stop_reason: None,
            name_map,
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
                    turn.push(ProviderTurnEvent::ReasoningDelta {
                        text: format!("{delta:?}"),
                    });
                }
                _ => {}
            },
            ConverseStreamOutput::Metadata(event) => {
                if let Some(usage) = event.usage() {
                    turn.push(ProviderTurnEvent::Usage {
                        usage: TokenUsage {
                            input_tokens: nonnegative_u32(usage.input_tokens()),
                            output_tokens: nonnegative_u32(usage.output_tokens()),
                            cached_input_tokens: usage
                                .cache_read_input_tokens()
                                .and_then(nonnegative_i32_to_u32),
                            cache_write_input_tokens: usage
                                .cache_write_input_tokens()
                                .and_then(nonnegative_i32_to_u32),
                            ..TokenUsage::default()
                        },
                    });
                }
            }
            ConverseStreamOutput::MessageStop(event) => {
                self.stop_reason = Some(map_stop_reason(event.stop_reason()));
                if self.saw_tool_call {
                    self.finish_tool_calls(turn)?;
                    return Ok(Some(StreamOutcome::ToolCall));
                }
                return Ok(Some(StreamOutcome::Finished));
            }
            _ => {}
        }
        Ok(None)
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
                    provider_error(
                        "tool_arguments_decode_failed",
                        ProviderErrorCategory::ProviderInternal,
                        format!(
                            "failed to decode arguments for tool call {id} ({name}): {error}; received {} bytes",
                            accumulator.arguments.len()
                        ),
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
        Ok(())
    }

    const fn finish_outcome(&self) -> StreamOutcome {
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
}

fn bedrock_request_projection(request: &ModelTurnRequest) -> ProviderRequestProjection {
    let emitted_cache_points = bedrock_emitted_cache_point_count(request);
    let sent_messages = request
        .messages
        .iter()
        .filter(|message| message.role != MessageRole::System)
        .count();
    ProviderRequestProjection {
        provider: Some("bcode.bedrock".to_string()),
        api_shape: Some("bedrock_converse".to_string()),
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
    if request.tools.is_empty() {
        return Ok(None);
    }
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
        .build()
        .map(Some)
        .map_err(|error| build_error(&error))
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

#[derive(Debug, Clone)]
struct Settings {
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
    fn resolve_from_context(context: &ProviderRequestContext) -> Self {
        Self::resolve_context(Some(context))
    }

    fn resolve(request: Option<&ModelTurnRequest>) -> Self {
        Self::resolve_context(request.map(|request| &request.provider_context))
    }

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
        Self {
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

fn capabilities() -> ProviderCapabilities {
    let settings = Settings::resolve(None);
    ProviderCapabilities {
        provider_id: PROVIDER_ID.to_string(),
        display_name: "Amazon Bedrock".to_string(),
        capabilities: [
            ProviderCapability::Streaming,
            ProviderCapability::Cancellation,
            ProviderCapability::Tools,
            ProviderCapability::PromptCaching,
        ]
        .into_iter()
        .collect(),
        auth_schemes: [
            "aws_default_chain".to_string(),
            "aws_credentials".to_string(),
        ]
        .into_iter()
        .collect(),
        metadata: diagnostics_metadata(&settings),
    }
}

impl BedrockProviderPlugin {
    fn models(&self, request: &ModelListRequest) -> ModelList {
        let settings = Settings::resolve_from_context(&request.provider_context);
        if settings.model_ids_are_explicit || settings.default_model.is_some() {
            return ModelList {
                models: ensure_selected_model_info(
                    model_infos_from_ids(&settings.model_ids, settings.default_model.as_deref()),
                    request.selected_model_id.as_deref(),
                ),
            };
        }
        let discovered = self
            .runtime
            .as_ref()
            .map_err(|error| {
                provider_error(
                    "runtime_unavailable",
                    ProviderErrorCategory::ProviderInternal,
                    error.clone(),
                )
            })
            .and_then(|runtime| get_or_refresh_discovery_sync(runtime, &self.discovery, &settings))
            .unwrap_or_else(|error| {
                tracing::warn!(
                    target: "bcode_bedrock::discovery",
                    error = %error.message,
                    "Bedrock model discovery failed"
                );
                ModelDiscovery::default()
            });
        ModelList {
            models: ensure_selected_model_info(
                discovered.models,
                request.selected_model_id.as_deref(),
            ),
        }
    }

    fn validate_config(&self) -> ValidateConfigResponse {
        let settings = Settings::resolve(None);
        let validation: Result<(), ProviderError> = Ok(());
        let mut metadata = diagnostics_metadata(&settings);
        let effective_region = validation.as_ref().ok().and_then(|()| {
            self.runtime
                .as_ref()
                .ok()
                .and_then(|runtime| resolved_sdk_region(runtime, &settings))
        });
        if let Some((region, source)) = &effective_region {
            metadata.insert("effective_region".to_string(), region.clone());
            metadata.insert(
                "effective_region_source".to_string(),
                source.as_str().to_string(),
            );
        }
        if validation.is_ok()
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
                    metadata.insert("model_discovery_error".to_string(), error.message);
                }
            }
        }
        ValidateConfigResponse {
            valid: validation.is_ok(),
            message: Some(match validation {
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
            metadata,
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
    let models = model_ids
        .iter()
        .map(|model_id| ModelInfo {
            model_id: model_id.clone(),
            display_name: model_id.clone(),
            is_default: default_model == Some(model_id.as_str()),
            context_window: None,
            max_output_tokens: None,
            capabilities: [
                ModelCapability::StreamingText,
                ModelCapability::ToolCalls,
                ModelCapability::PromptCaching,
            ]
            .into_iter()
            .collect(),
            reasoning: None,
            cache: bedrock_model_cache_info(),
            metadata_source: None,
            pricing: None,
            visibility: bcode_model::ModelVisibility::Visible,
        })
        .collect::<Vec<_>>();

    if let Ok(catalog) = bcode_model_catalog::ModelCatalog::load_bundled() {
        catalog.merge_provider_models("bedrock", models, false)
    } else {
        models
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
        ));
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
    if settings.model_ids_are_explicit || settings.default_model.is_some() {
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
    (cached.discovered_at.elapsed() < MODEL_DISCOVERY_TTL)
        .then(|| filtered_discovery(&cached.discovery, &cached.unsupported_streaming_tool_models))
}

fn filtered_discovery(
    discovery: &ModelDiscovery,
    unsupported_streaming_tool_models: &BTreeSet<String>,
) -> ModelDiscovery {
    let models = discovery
        .models
        .iter()
        .filter(|model| !unsupported_streaming_tool_models.contains(&model.model_id))
        .cloned()
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
            capabilities: [
                ModelCapability::StreamingText,
                ModelCapability::ToolCalls,
                ModelCapability::PromptCaching,
            ]
            .into_iter()
            .collect(),
            reasoning: None,
            cache: bedrock_model_cache_info(),
            metadata_source: None,
            pricing: None,
            visibility: bcode_model::ModelVisibility::Visible,
        })
        .collect();
    let models = if let Ok(catalog) = bcode_model_catalog::ModelCatalog::load_bundled() {
        catalog.merge_provider_models("bedrock", models, false)
    } else {
        models
    };
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

fn diagnostics_metadata(settings: &Settings) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("provider".to_string(), PROVIDER_ID.to_string());
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
    if std::env::var("AWS_BEARER_TOKEN_BEDROCK").is_ok_and(|value| !value.trim().is_empty()) {
        metadata.insert(
            "bearer_token_source".to_string(),
            "AWS_BEARER_TOKEN_BEDROCK".to_string(),
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
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        BedrockProviderPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

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

const fn map_stop_reason(reason: &BedrockStopReason) -> StopReason {
    match reason {
        BedrockStopReason::ToolUse => StopReason::ToolCall,
        BedrockStopReason::MaxTokens => StopReason::MaxTokens,
        BedrockStopReason::StopSequence => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

fn nonnegative_u32(value: i32) -> Option<u32> {
    nonnegative_i32_to_u32(value)
}

fn nonnegative_i32_to_u32(value: i32) -> Option<u32> {
    u32::try_from(value).ok()
}

fn build_error(error: &(impl ToString + ?Sized)) -> ProviderError {
    provider_error(
        "bedrock_request_build_failed",
        ProviderErrorCategory::InvalidRequest,
        error.to_string(),
    )
}

fn bedrock_sdk_error(
    error: &aws_sdk_bedrockruntime::error::SdkError<
        aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError,
    >,
) -> ProviderError {
    let message = DisplayErrorContext(error).to_string();
    let category = if message.contains("UnrecognizedClient")
        || message.contains("AccessDenied")
        || message.contains("ExpiredToken")
        || message.contains("credentials")
    {
        ProviderErrorCategory::Auth
    } else if message.contains("Throttl") || message.contains("TooManyRequests") {
        ProviderErrorCategory::RateLimit
    } else if is_context_length_error(&message) {
        ProviderErrorCategory::ContextLength
    } else if message.contains("ValidationException") {
        ProviderErrorCategory::InvalidRequest
    } else if message.contains("ResourceNotFound") || message.contains("not found") {
        ProviderErrorCategory::ModelNotFound
    } else {
        ProviderErrorCategory::ProviderInternal
    };
    provider_error("bedrock_request_failed", category, message)
}

fn bedrock_discovery_error(error: &(impl std::fmt::Debug + ToString + ?Sized)) -> ProviderError {
    let message = format!("{} ({error:?})", error.to_string());
    let category = if message.contains("AccessDenied") || message.contains("credentials") {
        ProviderErrorCategory::Auth
    } else if message.contains("Throttl") || message.contains("TooManyRequests") {
        ProviderErrorCategory::RateLimit
    } else if is_context_length_error(&message) {
        ProviderErrorCategory::ContextLength
    } else if message.contains("ValidationException") {
        ProviderErrorCategory::InvalidRequest
    } else {
        ProviderErrorCategory::ProviderInternal
    };
    provider_error("bedrock_model_discovery_failed", category, message)
}

fn bedrock_stream_error(error: &(impl ToString + ?Sized)) -> ProviderError {
    let message = error.to_string();
    let category = if is_context_length_error(&message) {
        ProviderErrorCategory::ContextLength
    } else {
        ProviderErrorCategory::ProviderInternal
    };
    provider_error("bedrock_stream_failed", category, message)
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

    #[test]
    fn json_document_round_trip_preserves_objects() {
        let value = serde_json::json!({"path":"/tmp/file", "count": 2, "ok": true});
        let document = json_value_to_document(&value);
        assert_eq!(document_to_json_value(&document), value);
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
    fn bedrock_tool_names_are_sanitized() {
        assert_eq!(bedrock_tool_name("filesystem.read"), "filesystem_read");
    }

    #[test]
    fn explicit_bedrock_model_infos_include_context_windows() {
        let models = model_infos_from_ids(
            &["anthropic.claude-3-5-sonnet-20241022-v2:0".to_string()],
            None,
        );

        assert_eq!(models[0].context_window, Some(200_000));
        assert_eq!(models[0].max_output_tokens, Some(64_000));
    }

    #[test]
    fn unknown_bedrock_model_infos_include_provider_defaults() {
        let models = model_infos_from_ids(&["provider.future-model-v1:0".to_string()], None);

        assert_eq!(models[0].context_window, Some(128_000));
        assert_eq!(models[0].max_output_tokens, Some(16_384));
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
                side_effect: bcode_model::ToolSideEffect::default(),
                requires_permission: false,
            }],
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
        let filtered =
            filtered_discovery(&discovery, &compatibility.unsupported_streaming_for(&key));
        assert_eq!(filtered.default_model_id, Some("good-model".to_string()));
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
