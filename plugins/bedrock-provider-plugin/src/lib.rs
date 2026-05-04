#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Amazon Bedrock model provider plugin for Bcode.

use aws_config::{BehaviorVersion, Region};
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::DisplayErrorContext;
use aws_sdk_bedrockruntime::types::{
    ContentBlock as BedrockContentBlock, ContentBlockDelta, ContentBlockStart, ConversationRole,
    ConverseStreamOutput, InferenceConfiguration, Message as BedrockMessage,
    StopReason as BedrockStopReason, SystemContentBlock, Tool, ToolConfiguration, ToolInputSchema,
    ToolResultBlock, ToolResultContentBlock, ToolResultStatus, ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::{Document, Number};
use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    MessageRole, ModelCapability, ModelInfo, ModelList, ModelMessage, ModelTurnRequest,
    OP_CANCEL_TURN, OP_CAPABILITIES, OP_FINISH_TURN, OP_MODELS, OP_POLL_TURN_EVENTS, OP_START_TURN,
    OP_VALIDATE_CONFIG, PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities,
    ProviderCapability, ProviderError, ProviderErrorCategory, ProviderTurnEvent, StartTurnResponse,
    StopReason, TokenUsage, ToolCall, ToolDefinition, ValidateConfigResponse,
};
use bcode_model_provider_runtime::{
    StreamOutcome, TurnState, TurnStore, current_thread_runtime, provider_error,
};
use bcode_plugin_sdk::prelude::*;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

const PROVIDER_ID: &str = "bcode.bedrock";
const DEFAULT_MODEL_ID: &str = "anthropic.claude-3-5-sonnet-20241022-v2:0";
const DEFAULT_REGION: &str = "us-east-1";

/// Amazon Bedrock model provider plugin.
#[derive(Default)]
pub struct BedrockProviderPlugin {
    turns: TurnStore,
}

impl RustPlugin for BedrockProviderPlugin {
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

impl BedrockProviderPlugin {
    fn start_turn(&mut self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<ModelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let (provider_turn_id, turn) = self.turns.insert_started("bedrock-turn");
        std::thread::spawn(move || TurnWorker { request, turn }.run());
        json_response(&StartTurnResponse { provider_turn_id })
    }

    fn poll_turn_events(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<PollTurnEventsRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        json_response(&PollTurnEventsResponse {
            events: self.turns.drain(&request.provider_turn_id),
        })
    }

    fn cancel_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<CancelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        self.turns.cancel(&request.provider_turn_id);
        json_response(&AckResponse::default())
    }

    fn finish_turn(&mut self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<FinishTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        self.turns.finish(&request.provider_turn_id);
        json_response(&AckResponse::default())
    }
}

struct TurnWorker {
    request: ModelTurnRequest,
    turn: TurnState,
}

impl TurnWorker {
    fn run(self) {
        let runtime = match current_thread_runtime() {
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
        runtime.block_on(stream_bedrock_turn(&self.request, &self.turn));
    }
}

async fn stream_bedrock_turn(request: &ModelTurnRequest, turn: &TurnState) {
    match stream_bedrock_turn_inner(request, turn).await {
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
) -> Result<StreamOutcome, ProviderError> {
    let settings = Settings::resolve(Some(request));
    settings.validate()?;
    let client = bedrock_client(&settings).await;
    let bedrock_request = build_converse_request(request, &settings)?;
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
    let response = builder
        .send()
        .await
        .map_err(|error| bedrock_sdk_error(&error))?;
    read_bedrock_stream(response.stream, turn, bedrock_tool_name_map(&request.tools)).await
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
    loader.load().await
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
                    self.tool_calls
                        .entry(event.content_block_index())
                        .or_default()
                        .arguments
                        .push_str(delta.input());
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
                        error.to_string(),
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

fn build_converse_request(
    request: &ModelTurnRequest,
    settings: &Settings,
) -> Result<BedrockConverseRequest, ProviderError> {
    let model_id = if request.model_id.is_empty() {
        settings.default_model.clone()
    } else {
        request.model_id.clone()
    };
    Ok(BedrockConverseRequest {
        model_id,
        messages: model_messages_to_bedrock_messages(request)?,
        system: system_blocks(request),
        tool_config: model_tools_to_bedrock_tool_config(&request.tools)?,
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
    for block in &message.content {
        match block {
            ContentBlock::ToolCall { call } => {
                blocks.push(BedrockContentBlock::ToolUse(
                    ToolUseBlock::builder()
                        .tool_use_id(call.id.clone())
                        .name(call.name.clone())
                        .input(json_value_to_document(&call.arguments))
                        .build()
                        .map_err(|error| build_error(&error))?,
                ));
            }
            ContentBlock::ToolResult { result } => {
                let mut builder = ToolResultBlock::builder()
                    .tool_use_id(result.call_id.clone())
                    .content(ToolResultContentBlock::Text(result.output.clone()));
                if result.is_error {
                    builder = builder.status(ToolResultStatus::Error);
                }
                blocks.push(BedrockContentBlock::ToolResult(
                    builder.build().map_err(|error| build_error(&error))?,
                ));
            }
            ContentBlock::Text { .. } | ContentBlock::ProviderExtension { .. } => {}
        }
    }
    Ok(blocks)
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
    tools: &[ToolDefinition],
) -> Result<Option<ToolConfiguration>, ProviderError> {
    if tools.is_empty() {
        return Ok(None);
    }
    let tools = tools
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
    ToolConfiguration::builder()
        .set_tools(Some(tools))
        .build()
        .map(Some)
        .map_err(|error| build_error(&error))
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
    default_model: String,
    model_ids: Vec<String>,
    model_ids_are_explicit: bool,
    region: Option<String>,
    region_source: RegionSource,
    aws_profile: Option<String>,
    endpoint_url: Option<String>,
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
    fn resolve(request: Option<&ModelTurnRequest>) -> Self {
        let config = bcode_config::load_config().ok();
        let resolved = config
            .as_ref()
            .map(bcode_config::BcodeConfig::resolved_model_selection);
        let request_settings = request
            .map(|request| request.provider_context.settings.clone())
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
        let default_model = first_env(["BCODE_BEDROCK_MODEL", "BEDROCK_MODEL"])
            .or_else(|| value(&["model", "model_id"]))
            .or_else(|| resolved.and_then(|selection| selection.model_id))
            .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string());
        let model_ids_value = first_env(["BCODE_BEDROCK_MODELS", "BEDROCK_MODELS"])
            .or_else(|| value(&["models", "model_ids"]));
        let mut model_ids = model_ids_value
            .as_deref()
            .map_or_else(|| vec![default_model.clone()], parse_model_list);
        if !model_ids.contains(&default_model) {
            model_ids.insert(0, default_model.clone());
        }
        let (region, region_source) = resolve_configured_region(&value);
        Self {
            default_model,
            model_ids,
            model_ids_are_explicit: model_ids_value.is_some(),
            region,
            region_source,
            aws_profile: first_env(["BCODE_BEDROCK_AWS_PROFILE", "AWS_PROFILE"])
                .or_else(|| value(&["profile", "aws_profile"])),
            endpoint_url: first_env(["BCODE_BEDROCK_ENDPOINT_URL", "BEDROCK_ENDPOINT_URL"])
                .or_else(|| value(&["endpoint_url"])),
            config_source: if request.is_some() {
                "request/config/environment".to_string()
            } else {
                "config/environment".to_string()
            },
        }
    }

    fn validate(&self) -> Result<(), ProviderError> {
        if self.default_model.trim().is_empty() {
            return Err(provider_error(
                "missing_bedrock_model",
                ProviderErrorCategory::Config,
                "set BCODE_BEDROCK_MODEL or configure a Bedrock model profile",
            ));
        }
        Ok(())
    }
}

fn resolve_configured_region(
    value: &impl Fn(&[&str]) -> Option<String>,
) -> (Option<String>, RegionSource) {
    if let Some(region) = first_env(["BCODE_BEDROCK_REGION"]) {
        return (Some(region), RegionSource::BcodeEnv);
    }
    if let Some(region) = first_env(["AWS_REGION", "AWS_DEFAULT_REGION"]) {
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
        ]
        .into_iter()
        .collect(),
        metadata: diagnostics_metadata(&settings),
    }
}

fn models() -> ModelList {
    let settings = Settings::resolve(None);
    ModelList {
        models: settings
            .model_ids
            .iter()
            .map(|model_id| ModelInfo {
                model_id: model_id.clone(),
                display_name: model_id.clone(),
                is_default: model_id == &settings.default_model,
                context_window: None,
                max_output_tokens: None,
                capabilities: [ModelCapability::StreamingText, ModelCapability::ToolCalls]
                    .into_iter()
                    .collect(),
            })
            .collect(),
    }
}

fn validate_config() -> ValidateConfigResponse {
    let settings = Settings::resolve(None);
    let validation = settings.validate();
    let mut metadata = diagnostics_metadata(&settings);
    let effective_region = validation
        .as_ref()
        .ok()
        .and_then(|()| resolved_sdk_region(&settings));
    if let Some((region, source)) = &effective_region {
        metadata.insert("effective_region".to_string(), region.clone());
        metadata.insert(
            "effective_region_source".to_string(),
            source.as_str().to_string(),
        );
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

fn resolved_sdk_region(settings: &Settings) -> Option<(String, RegionSource)> {
    let runtime = current_thread_runtime().ok()?;
    let config = runtime.block_on(bedrock_sdk_config(settings));
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
    metadata.insert("default_model".to_string(), settings.default_model.clone());
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

fn first_env<const N: usize>(names: [&str; N]) -> Option<String> {
    names
        .into_iter()
        .find_map(|name| match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => Some(value),
            _ => None,
        })
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
    } else if message.contains("ValidationException") {
        ProviderErrorCategory::InvalidRequest
    } else if message.contains("ResourceNotFound") || message.contains("not found") {
        ProviderErrorCategory::ModelNotFound
    } else {
        ProviderErrorCategory::ProviderInternal
    };
    provider_error("bedrock_request_failed", category, message)
}

fn bedrock_stream_error(error: &(impl ToString + ?Sized)) -> ProviderError {
    provider_error(
        "bedrock_stream_failed",
        ProviderErrorCategory::ProviderInternal,
        error.to_string(),
    )
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

bcode_plugin_sdk::export_plugin!(BedrockProviderPlugin, include_str!("../bcode-plugin.toml"));

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
    fn bedrock_tool_names_are_sanitized() {
        assert_eq!(bedrock_tool_name("filesystem.read"), "filesystem_read");
    }

    #[test]
    fn model_list_includes_default_first() {
        let mut settings = Settings::resolve(None);
        settings.default_model = "model-b".to_string();
        settings.model_ids = vec!["model-b".to_string(), "model-a".to_string()];
        let metadata = diagnostics_metadata(&settings);
        assert_eq!(metadata.get("default_model"), Some(&"model-b".to_string()));
    }
}
