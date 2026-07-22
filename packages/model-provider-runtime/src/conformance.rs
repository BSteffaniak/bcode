//! Deterministic conformance checks for `bcode.model-provider/v1` adapters.
//!
//! The suite uses only the public typed provider operations. Provider authors can therefore run
//! it against an in-process adapter, a loaded native plugin, or a transport proxy by implementing
//! [`BlockingModelProviderInvoker`]. It intentionally does not depend on Bcode's TUI, daemon, or
//! bundled provider implementations.

use crate::BlockingModelProviderInvoker;
use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MessageRole, ModelCapability,
    ModelInfo, ModelList, ModelListRequest, ModelMessage, ModelParameters, ModelTurnRequest,
    NativeWebSearchRequest, NativeWebSearchResponse, PollTurnEventsRequest, PollTurnEventsResponse,
    PromptCacheHints, PromptCacheMode, PromptCachePoint, ProviderCapabilities, ProviderCapability,
    ProviderErrorCategory, ProviderRequestContext, ProviderRequestProjection, ProviderTurnEvent,
    StartTurnResponse, StopReason, StructuredOutputRequest, ToolCallRequestPolicy, ToolChoice,
    ToolDefinition, ToolResult, ValidateConfigRequest, ValidateConfigResponse,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, Instant};

const BASE_TURN: &str = "baseline turn";
const CANCELLATION: &str = "cancellation";
const NATIVE_WEB_SEARCH: &str = "native web search";
const PARALLEL_TOOL_CALLING: &str = "parallel tool calling";
const PROMPT_CACHING: &str = "prompt caching";
const STRUCTURED_OUTPUT: &str = "structured output";
const TOOL_CALLING: &str = "tool calling";

/// Inputs for [`run_provider_conformance_suite`].
#[derive(Debug, Clone)]
pub struct ProviderConformanceOptions {
    /// Plugin id used to route each operation. `None` lets the invoker select its default.
    pub provider_plugin_id: Option<String>,
    /// Provider context containing the credentials and endpoint under test.
    pub provider_context: ProviderRequestContext,
    /// Model to test. `None` selects the provider's declared default or first listed model.
    pub model_id: Option<String>,
    /// Configuration passed to `validate_config` before model turns begin.
    pub validation_config: BTreeMap<String, String>,
    /// Optional profile passed to `validate_config`.
    pub validation_profile: Option<String>,
    /// Maximum time to wait for each provider turn to terminate.
    pub turn_timeout: Duration,
    /// Delay between empty event polls.
    pub poll_interval: Duration,
}

impl Default for ProviderConformanceOptions {
    fn default() -> Self {
        Self {
            provider_plugin_id: None,
            provider_context: ProviderRequestContext::default(),
            model_id: None,
            validation_config: BTreeMap::new(),
            validation_profile: None,
            turn_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(10),
        }
    }
}

/// Outcome of one conformance case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConformanceCase {
    /// Stable human-readable case name.
    pub name: &'static str,
    /// Whether the case ran or was inapplicable to the advertised surface.
    pub outcome: ProviderConformanceOutcome,
}

/// Result state for one conformance case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConformanceOutcome {
    /// The behavior was exercised successfully.
    Passed,
    /// The behavior is not advertised by both the provider and selected model.
    Skipped { reason: String },
}

/// Successful provider conformance report.
#[derive(Debug, Clone)]
pub struct ProviderConformanceReport {
    /// Provider capability response validated by the suite.
    pub provider: ProviderCapabilities,
    /// Selected model validated by the suite.
    pub model: ModelInfo,
    /// Ordered case outcomes.
    pub cases: Vec<ProviderConformanceCase>,
}

/// Failure from a provider conformance run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConformanceError {
    /// A typed provider operation could not be invoked or decoded.
    Invocation { case: &'static str, message: String },
    /// The adapter returned typed data that violates the provider contract.
    Violation { case: &'static str, message: String },
    /// A turn did not terminate within the configured bound.
    Timeout {
        case: &'static str,
        provider_turn_id: String,
    },
}

impl fmt::Display for ProviderConformanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invocation { case, message } => {
                write!(
                    formatter,
                    "provider conformance case '{case}' could not run: {message}"
                )
            }
            Self::Violation { case, message } => {
                write!(
                    formatter,
                    "provider conformance case '{case}' failed: {message}"
                )
            }
            Self::Timeout {
                case,
                provider_turn_id,
            } => write!(
                formatter,
                "provider conformance case '{case}' timed out for turn '{provider_turn_id}'"
            ),
        }
    }
}

impl std::error::Error for ProviderConformanceError {}

/// Stateful validator for normalized events from one provider turn.
///
/// The validator accepts empty poll batches and all optional metadata events, while enforcing
/// ordering, tool-call correlation, usage coherence, normalized error shape, and one terminal
/// outcome. Call [`Self::finish`] after the provider reports a terminal event.
#[derive(Debug, Default)]
pub struct ProviderEventValidator {
    started: bool,
    terminal: Option<StopReason>,
    error_category: Option<ProviderErrorCategory>,
    request_projections: Vec<ProviderRequestProjection>,
    cancelled: bool,
    usage_events: usize,
    text_events: usize,
    reasoning_events: usize,
    open_tool_calls: BTreeMap<String, String>,
    finished_tool_calls: Vec<bcode_model::ToolCall>,
}

impl ProviderEventValidator {
    /// Validate and record one ordered event batch.
    ///
    /// # Errors
    ///
    /// Returns a contract violation when events are out of order, malformed, duplicated, or
    /// internally inconsistent.
    pub fn observe(
        &mut self,
        events: &[ProviderTurnEvent],
    ) -> Result<(), ProviderConformanceError> {
        for event in events {
            self.observe_one(event)?;
        }
        Ok(())
    }

    /// Return whether a terminal `TurnFinished` event has been observed.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Return the terminal stop reason, when present.
    #[must_use]
    pub const fn stop_reason(&self) -> Option<StopReason> {
        self.terminal
    }

    /// Validate final turn invariants and return event statistics.
    ///
    /// # Errors
    ///
    /// Returns a contract violation if the stream did not start and terminate cleanly, left a
    /// tool call incomplete, omitted usage, or used a terminal reason inconsistent with prior
    /// normalized error/cancellation events.
    pub fn finish(&self) -> Result<ProviderEventSummary, ProviderConformanceError> {
        if !self.started {
            return violation(BASE_TURN, "event stream omitted TurnStarted");
        }
        let Some(stop_reason) = self.terminal else {
            return violation(BASE_TURN, "event stream omitted TurnFinished");
        };
        if !self.open_tool_calls.is_empty() {
            return violation(BASE_TURN, "event stream ended with unfinished tool calls");
        }
        if self.error_category.is_some() != (stop_reason == StopReason::Error) {
            return violation(
                BASE_TURN,
                "Error event and TurnFinished(Error) must be emitted together",
            );
        }
        if self.cancelled && stop_reason != StopReason::Cancelled {
            return violation(
                BASE_TURN,
                "Cancelled event must terminate with TurnFinished(Cancelled)",
            );
        }
        if self.usage_events == 0
            && !matches!(stop_reason, StopReason::Cancelled | StopReason::Error)
        {
            return violation(BASE_TURN, "event stream omitted provider usage metadata");
        }
        if stop_reason == StopReason::ToolCall && self.finished_tool_calls.is_empty() {
            return violation(
                BASE_TURN,
                "TurnFinished(ToolCall) requires a completed tool call",
            );
        }
        Ok(ProviderEventSummary {
            stop_reason,
            text_events: self.text_events,
            reasoning_events: self.reasoning_events,
            usage_events: self.usage_events,
            tool_calls: self.finished_tool_calls.clone(),
            error_category: self.error_category,
            request_projections: self.request_projections.clone(),
        })
    }

    fn observe_one(&mut self, event: &ProviderTurnEvent) -> Result<(), ProviderConformanceError> {
        if self.terminal.is_some() {
            return violation(BASE_TURN, "provider emitted an event after TurnFinished");
        }
        if !self.started && !matches!(event, ProviderTurnEvent::TurnStarted) {
            return violation(BASE_TURN, "TurnStarted must be the first event");
        }
        match event {
            ProviderTurnEvent::TurnStarted => {
                if self.started {
                    return violation(BASE_TURN, "provider emitted TurnStarted more than once");
                }
                self.started = true;
            }
            ProviderTurnEvent::TextDelta { text } => {
                if text.is_empty() {
                    return violation(BASE_TURN, "provider emitted an empty text delta");
                }
                self.text_events += 1;
            }
            ProviderTurnEvent::ReasoningDelta { text } => {
                if text.is_empty() {
                    return violation(BASE_TURN, "provider emitted an empty reasoning delta");
                }
                self.reasoning_events += 1;
            }
            ProviderTurnEvent::ToolCallStarted { call_id, name } => {
                self.observe_tool_call_started(call_id, name)?;
            }
            ProviderTurnEvent::ToolCallDelta { call_id, delta } => {
                self.observe_tool_call_delta(call_id, delta)?;
            }
            ProviderTurnEvent::ToolCallFinished { call } => {
                self.observe_tool_call_finished(call)?;
            }
            ProviderTurnEvent::Usage { usage } => self.observe_usage(usage)?,
            ProviderTurnEvent::ProviderMetadata { key, .. } if key.is_empty() => {
                return violation(BASE_TURN, "provider metadata key must be non-empty");
            }
            ProviderTurnEvent::Warning { message } if message.is_empty() => {
                return violation(BASE_TURN, "provider warning message must be non-empty");
            }
            ProviderTurnEvent::RetryScheduled {
                message,
                retry_at_unix,
            } => {
                if message.is_empty() {
                    return violation(BASE_TURN, "provider retry message must be non-empty");
                }
                if *retry_at_unix == 0 {
                    return violation(BASE_TURN, "provider retry time must be a Unix timestamp");
                }
            }
            ProviderTurnEvent::Error { error } => self.observe_error(error)?,
            ProviderTurnEvent::Cancelled => {
                if self.cancelled {
                    return violation(BASE_TURN, "provider emitted Cancelled more than once");
                }
                self.cancelled = true;
            }
            ProviderTurnEvent::TurnFinished { stop_reason } => {
                self.terminal = Some(*stop_reason);
            }
            ProviderTurnEvent::RequestProjection { projection } => {
                self.request_projections.push(projection.clone());
            }
            ProviderTurnEvent::ExactRequestInputTokens { .. }
            | ProviderTurnEvent::ContextCompacted { .. }
            | ProviderTurnEvent::ProviderMetadata { .. }
            | ProviderTurnEvent::Warning { .. } => {}
        }
        Ok(())
    }

    fn observe_tool_call_started(
        &mut self,
        call_id: &str,
        name: &str,
    ) -> Result<(), ProviderConformanceError> {
        if call_id.is_empty() || name.is_empty() {
            return violation(BASE_TURN, "tool call id and name must be non-empty");
        }
        if self
            .open_tool_calls
            .insert(call_id.to_string(), name.to_string())
            .is_some()
        {
            return violation(BASE_TURN, "provider reused an active tool call id");
        }
        Ok(())
    }

    fn observe_tool_call_delta(
        &self,
        call_id: &str,
        delta: &str,
    ) -> Result<(), ProviderConformanceError> {
        if !self.open_tool_calls.contains_key(call_id) {
            return violation(BASE_TURN, "tool-call delta referenced an unknown call id");
        }
        if delta.is_empty() {
            return violation(BASE_TURN, "provider emitted an empty tool-call delta");
        }
        Ok(())
    }

    fn observe_tool_call_finished(
        &mut self,
        call: &bcode_model::ToolCall,
    ) -> Result<(), ProviderConformanceError> {
        let Some(name) = self.open_tool_calls.remove(&call.id) else {
            return violation(BASE_TURN, "finished tool call was not started");
        };
        if name != call.name {
            return violation(BASE_TURN, "tool call name changed before completion");
        }
        if !call.arguments.is_object() {
            return violation(BASE_TURN, "tool call arguments must be a JSON object");
        }
        self.finished_tool_calls.push(call.clone());
        Ok(())
    }

    fn observe_usage(
        &mut self,
        usage: &bcode_model::TokenUsage,
    ) -> Result<(), ProviderConformanceError> {
        if let (Some(total), Some(input), Some(output)) =
            (usage.total_tokens, usage.input_tokens, usage.output_tokens)
            && total < input.saturating_add(output)
        {
            return violation(
                BASE_TURN,
                "total token usage is smaller than input plus output usage",
            );
        }
        if let (Some(input), Some(cached)) = (usage.input_tokens, usage.cached_input_tokens)
            && cached > input
        {
            return violation(
                BASE_TURN,
                "cached input token usage exceeds total input token usage",
            );
        }
        self.usage_events += 1;
        Ok(())
    }

    fn observe_error(
        &mut self,
        error: &bcode_model::ProviderError,
    ) -> Result<(), ProviderConformanceError> {
        if error.code.is_empty() || error.message.is_empty() {
            return violation(
                BASE_TURN,
                "normalized provider errors require code and message",
            );
        }
        if let Some(retry) = error.retry.as_deref() {
            if !error.retryable {
                return violation(
                    BASE_TURN,
                    "a non-retryable provider error cannot include a retry hint",
                );
            }
            if retry.retry_after_ms.is_none() && retry.retry_at_unix.is_none() {
                return violation(
                    BASE_TURN,
                    "provider retry hint omitted both relative and absolute timing",
                );
            }
            if retry.source.as_deref().is_some_and(str::is_empty) {
                return violation(BASE_TURN, "provider retry-hint source is empty");
            }
        }
        if self.error_category.replace(error.category).is_some() {
            return violation(BASE_TURN, "provider emitted more than one terminal error");
        }
        Ok(())
    }
}

/// Validated summary of one normalized provider event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEventSummary {
    /// Terminal provider stop reason.
    pub stop_reason: StopReason,
    /// Number of non-empty text deltas.
    pub text_events: usize,
    /// Number of non-empty reasoning deltas.
    pub reasoning_events: usize,
    /// Number of coherent usage events.
    pub usage_events: usize,
    /// Completed, correlated tool calls in provider order.
    pub tool_calls: Vec<bcode_model::ToolCall>,
    /// Normalized error category, when the turn failed.
    pub error_category: Option<ProviderErrorCategory>,
    /// Provider-reported request projections in event order.
    pub request_projections: Vec<ProviderRequestProjection>,
}

/// Exercise the baseline provider contract and every capability advertised by the selected model.
///
/// The supplied context must point at a deterministic test endpoint or account: this function may
/// make model requests. The suite always validates discovery, configuration, one streaming turn,
/// lifecycle cleanup, and normalized usage. It additionally checks tool calling, structured JSON,
/// and cancellation when those capabilities are advertised.
///
/// # Errors
///
/// Returns an invocation failure when routing or typed serialization fails, a violation when the
/// adapter contradicts the contract/capabilities, or a timeout when a turn does not terminate.
pub fn run_provider_conformance_suite<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
) -> Result<ProviderConformanceReport, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let (provider, model) = discover_provider(invoker, options)?;
    validate_configuration(invoker, options)?;
    let mut cases = vec![
        passed("capabilities"),
        passed("models"),
        passed("validate config"),
        run_baseline_case(invoker, options, &model)?,
    ];

    cases.push(run_tool_case(invoker, options, &provider, &model)?);
    cases.push(run_parallel_tool_case(invoker, options, &provider, &model)?);
    cases.push(run_prompt_cache_case(invoker, options, &provider, &model)?);
    cases.push(run_native_web_search_case(invoker, options, &provider)?);
    cases.push(run_structured_output_case(
        invoker, options, &provider, &model,
    )?);
    cases.push(run_cancellation_case(invoker, options, &provider, &model)?);

    Ok(ProviderConformanceReport {
        provider,
        model,
        cases,
    })
}

fn discover_provider<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
) -> Result<(ProviderCapabilities, ModelInfo), ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let provider: ProviderCapabilities = invoke(
        invoker,
        options,
        "capabilities",
        bcode_model::OP_CAPABILITIES,
        &(),
    )?;
    validate_provider_identity(&provider)?;
    let models: ModelList = invoke(
        invoker,
        options,
        "models",
        bcode_model::OP_MODELS,
        &ModelListRequest {
            provider_context: options.provider_context.clone(),
            selected_model_id: options.model_id.clone(),
        },
    )?;
    let model = select_and_validate_model(&provider, &models.models, options.model_id.as_deref())?;
    Ok((provider, model))
}

fn validate_configuration<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
) -> Result<(), ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let validation: ValidateConfigResponse = invoke(
        invoker,
        options,
        "validate config",
        bcode_model::OP_VALIDATE_CONFIG,
        &ValidateConfigRequest {
            profile: options.validation_profile.clone(),
            config: options.validation_config.clone(),
            provider_context: options.provider_context.clone(),
        },
    )?;
    if validation.valid {
        Ok(())
    } else {
        violation(
            "validate config",
            validation
                .message
                .as_deref()
                .unwrap_or("provider rejected the conformance configuration"),
        )
    }
}

fn run_baseline_case<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    model: &ModelInfo,
) -> Result<ProviderConformanceCase, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let summary = run_turn(
        invoker,
        options,
        BASE_TURN,
        &turn_request(
            options,
            &model.model_id,
            "Reply with: bcode provider conformance.",
        ),
        false,
    )?;
    if summary.stop_reason != StopReason::EndTurn || summary.text_events == 0 {
        return violation(
            BASE_TURN,
            "baseline request must finish with non-empty model text",
        );
    }
    Ok(passed(BASE_TURN))
}

fn run_tool_case<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    provider: &ProviderCapabilities,
    model: &ModelInfo,
) -> Result<ProviderConformanceCase, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    if !supports(
        provider,
        model,
        ProviderCapability::Tools,
        ModelCapability::ToolCalls,
    ) {
        return Ok(skipped(
            TOOL_CALLING,
            "provider/model do not advertise tool calls",
        ));
    }
    let mut request = turn_request(
        options,
        &model.model_id,
        "Call the filesystem.read tool exactly once with path /tmp/bcode-provider-conformance.",
    );
    request.tools.push(ToolDefinition {
        name: "filesystem.read".to_string(),
        description: "Read the requested test path.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }),
    });
    request.tool_call_policy = ToolCallRequestPolicy {
        parallel: false,
        choice: ToolChoice::Required,
    };
    let summary = run_turn(invoker, options, TOOL_CALLING, &request, false)?;
    if summary.stop_reason != StopReason::ToolCall || summary.tool_calls.is_empty() {
        return violation(
            TOOL_CALLING,
            "advertised tool support did not produce a completed required tool call",
        );
    }
    if !request.tool_call_policy.parallel && summary.tool_calls.len() != 1 {
        return violation(
            TOOL_CALLING,
            "provider emitted multiple tool calls while parallel tool calls were disabled",
        );
    }
    continue_after_tools(invoker, options, TOOL_CALLING, request, &summary.tool_calls)?;
    Ok(passed(TOOL_CALLING))
}

fn run_parallel_tool_case<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    provider: &ProviderCapabilities,
    model: &ModelInfo,
) -> Result<ProviderConformanceCase, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    if !supports(
        provider,
        model,
        ProviderCapability::ParallelToolCalls,
        ModelCapability::ParallelToolCalls,
    ) {
        return Ok(skipped(
            PARALLEL_TOOL_CALLING,
            "provider/model do not advertise parallel tool calls",
        ));
    }
    let mut request = turn_request(
        options,
        &model.model_id,
        "Call both conformance tools in the same response.",
    );
    request.tools = ["conformance.first", "conformance.second"]
        .into_iter()
        .map(|name| ToolDefinition {
            name: name.to_string(),
            description: format!("Invoke {name}."),
            input_schema: serde_json::json!({"type": "object"}),
        })
        .collect();
    request.tool_call_policy = ToolCallRequestPolicy {
        parallel: true,
        choice: ToolChoice::Required,
    };
    let summary = run_turn(invoker, options, PARALLEL_TOOL_CALLING, &request, false)?;
    if summary.stop_reason != StopReason::ToolCall || summary.tool_calls.len() < 2 {
        return violation(
            PARALLEL_TOOL_CALLING,
            "advertised parallel tool support did not produce multiple completed calls",
        );
    }
    let names = summary
        .tool_calls
        .iter()
        .map(|call| call.name.as_str())
        .collect::<BTreeSet<_>>();
    if names != BTreeSet::from(["conformance.first", "conformance.second"]) {
        return violation(
            PARALLEL_TOOL_CALLING,
            "parallel tool calls did not preserve the registered tool names",
        );
    }
    continue_after_tools(
        invoker,
        options,
        PARALLEL_TOOL_CALLING,
        request,
        &summary.tool_calls,
    )?;
    Ok(passed(PARALLEL_TOOL_CALLING))
}

fn continue_after_tools<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    mut request: ModelTurnRequest,
    calls: &[bcode_model::ToolCall],
) -> Result<(), ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    request.turn_id.push_str("-continuation");
    request.tool_call_policy = ToolCallRequestPolicy {
        parallel: false,
        choice: ToolChoice::None,
    };
    request.messages.push(ModelMessage {
        role: MessageRole::Assistant,
        content: calls
            .iter()
            .cloned()
            .map(|call| ContentBlock::ToolCall { call })
            .collect(),
    });
    request
        .messages
        .extend(calls.iter().map(|call| ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: ToolResult {
                    call_id: call.id.clone(),
                    output: "conformance tool result".to_string(),
                    is_error: false,
                    content: Vec::new(),
                },
            }],
        }));
    let continuation = run_turn(invoker, options, case, &request, false)?;
    if continuation.stop_reason != StopReason::EndTurn || continuation.text_events == 0 {
        return violation(
            case,
            "provider did not complete normally after typed tool results were supplied",
        );
    }
    Ok(())
}

fn run_prompt_cache_case<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    provider: &ProviderCapabilities,
    model: &ModelInfo,
) -> Result<ProviderConformanceCase, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    if !supports(
        provider,
        model,
        ProviderCapability::PromptCaching,
        ModelCapability::PromptCaching,
    ) {
        return Ok(skipped(
            PROMPT_CACHING,
            "provider/model do not advertise prompt caching",
        ));
    }
    let mut request = turn_request(
        options,
        &model.model_id,
        "Reply with a short cache conformance response.",
    );
    request.prompt_cache = PromptCacheHints {
        mode: PromptCacheMode::Aggressive,
        cache_system_prompt: true,
        cache_tools: true,
    };
    request.messages[0].content.push(ContentBlock::CachePoint {
        hint: PromptCachePoint {
            label: Some("conformance".to_string()),
            ttl_seconds: Some(60),
        },
    });
    request.tools.push(ToolDefinition {
        name: "conformance.cache".to_string(),
        description: "Stable cache conformance tool.".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    });
    request.tool_call_policy.choice = ToolChoice::None;
    let summary = run_turn(invoker, options, PROMPT_CACHING, &request, false)?;
    if summary.stop_reason != StopReason::EndTurn || summary.text_events == 0 {
        return violation(
            PROMPT_CACHING,
            "cache-hinted request did not complete normally",
        );
    }
    let projection =
        summary
            .request_projections
            .last()
            .ok_or_else(|| ProviderConformanceError::Violation {
                case: PROMPT_CACHING,
                message: "advertised prompt caching omitted request projection metadata"
                    .to_string(),
            })?;
    let explicit = projection.cache_point_count.unwrap_or_default();
    let accounted = projection
        .emitted_cache_point_count
        .unwrap_or_default()
        .saturating_add(projection.dropped_cache_point_count.unwrap_or_default());
    if explicit == 0 || accounted < explicit {
        return violation(
            PROMPT_CACHING,
            "request projection did not account for explicit cache points",
        );
    }
    Ok(passed(PROMPT_CACHING))
}

fn run_native_web_search_case<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    provider: &ProviderCapabilities,
) -> Result<ProviderConformanceCase, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    if !provider
        .capabilities
        .contains(&ProviderCapability::NativeWebSearch)
    {
        return Ok(skipped(
            NATIVE_WEB_SEARCH,
            "provider/model do not advertise native web search",
        ));
    }
    let response: NativeWebSearchResponse = invoke(
        invoker,
        options,
        NATIVE_WEB_SEARCH,
        bcode_model::OP_NATIVE_WEB_SEARCH,
        &NativeWebSearchRequest {
            query: "Bcode provider conformance".to_string(),
            max_results: Some(2),
            site: None,
            freshness: None,
            region: None,
            safe_search: None,
            provider_context: options.provider_context.clone(),
            metadata: BTreeMap::new(),
        },
    )?;
    if response.provider.trim().is_empty() {
        return violation(NATIVE_WEB_SEARCH, "native search omitted provider identity");
    }
    if response.results.is_empty() {
        return violation(
            NATIVE_WEB_SEARCH,
            "advertised native search returned no deterministic conformance result",
        );
    }
    for result in &response.results {
        if result.title.trim().is_empty() || result.url.trim().is_empty() {
            return violation(
                NATIVE_WEB_SEARCH,
                "native search result requires non-empty title and URL",
            );
        }
    }
    Ok(passed(NATIVE_WEB_SEARCH))
}

fn run_structured_output_case<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    provider: &ProviderCapabilities,
    model: &ModelInfo,
) -> Result<ProviderConformanceCase, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    if !supports(
        provider,
        model,
        ProviderCapability::JsonMode,
        ModelCapability::JsonMode,
    ) {
        return Ok(skipped(
            STRUCTURED_OUTPUT,
            "provider/model do not advertise JSON mode",
        ));
    }
    let mut request = turn_request(
        options,
        &model.model_id,
        "Return only a JSON object whose ok property is true.",
    );
    request.structured_output = Some(StructuredOutputRequest {
        name: "ConformanceResult".to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {"ok": {"const": true}},
            "required": ["ok"],
            "additionalProperties": false
        }),
        strict: true,
    });
    let text = run_text_turn(invoker, options, STRUCTURED_OUTPUT, &request)?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| ProviderConformanceError::Violation {
            case: STRUCTURED_OUTPUT,
            message: format!("structured output was not JSON: {error}"),
        })?;
    if value.get("ok") != Some(&serde_json::Value::Bool(true))
        || value.as_object().is_none_or(|object| object.len() != 1)
    {
        return violation(
            STRUCTURED_OUTPUT,
            "response did not satisfy the requested schema",
        );
    }
    Ok(passed(STRUCTURED_OUTPUT))
}

fn run_cancellation_case<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    provider: &ProviderCapabilities,
    model: &ModelInfo,
) -> Result<ProviderConformanceCase, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    if !provider
        .capabilities
        .contains(&ProviderCapability::Cancellation)
    {
        return Ok(skipped(
            CANCELLATION,
            "provider does not advertise cancellation",
        ));
    }
    let request = turn_request(
        options,
        &model.model_id,
        "Write a detailed response for cancellation testing.",
    );
    let summary = run_turn(invoker, options, CANCELLATION, &request, true)?;
    if !matches!(
        summary.stop_reason,
        StopReason::Cancelled | StopReason::EndTurn
    ) {
        return violation(
            CANCELLATION,
            "a cancellation race must terminate as Cancelled or a completed EndTurn",
        );
    }
    Ok(passed(CANCELLATION))
}

fn run_text_turn<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    request: &ModelTurnRequest,
) -> Result<String, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let (summary, text) = execute_turn(invoker, options, case, request, false)?;
    if summary.stop_reason != StopReason::EndTurn || text.is_empty() {
        return violation(case, "request did not finish with non-empty text");
    }
    Ok(text)
}

fn run_turn<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    request: &ModelTurnRequest,
    cancel_immediately: bool,
) -> Result<ProviderEventSummary, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    execute_turn(invoker, options, case, request, cancel_immediately).map(|(summary, _)| summary)
}

fn execute_turn<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    request: &ModelTurnRequest,
    cancel_immediately: bool,
) -> Result<(ProviderEventSummary, String), ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let start: StartTurnResponse =
        invoke(invoker, options, case, bcode_model::OP_START_TURN, &request)?;
    if start.provider_turn_id.is_empty() {
        return violation(case, "start_turn returned an empty provider turn id");
    }
    if cancel_immediately
        && let Err(error) = cancel_turn(invoker, options, case, &start.provider_turn_id)
    {
        let _ = finish_turn(invoker, options, case, &start.provider_turn_id);
        return Err(error);
    }

    let deadline = Instant::now() + options.turn_timeout;
    let mut validator = ProviderEventValidator::default();
    let mut text = String::new();
    while !validator.is_terminal() {
        if Instant::now() >= deadline {
            cancel_turn(invoker, options, case, &start.provider_turn_id)?;
            finish_turn(invoker, options, case, &start.provider_turn_id)?;
            return Err(ProviderConformanceError::Timeout {
                case,
                provider_turn_id: start.provider_turn_id,
            });
        }
        let response: PollTurnEventsResponse = match invoke(
            invoker,
            options,
            case,
            bcode_model::OP_POLL_TURN_EVENTS,
            &PollTurnEventsRequest {
                provider_turn_id: start.provider_turn_id.clone(),
            },
        ) {
            Ok(response) => response,
            Err(error) => {
                best_effort_cleanup(invoker, options, case, &start.provider_turn_id);
                return Err(error);
            }
        };
        for event in &response.events {
            if let ProviderTurnEvent::TextDelta { text: delta } = event {
                text.push_str(delta);
            }
        }
        if let Err(error) = validator.observe(&response.events) {
            best_effort_cleanup(invoker, options, case, &start.provider_turn_id);
            return Err(recase(error, case));
        }
        if response.events.is_empty() {
            std::thread::sleep(options.poll_interval);
        }
    }
    let summary = match validator.finish() {
        Ok(summary) => summary,
        Err(error) => {
            best_effort_cleanup(invoker, options, case, &start.provider_turn_id);
            return Err(recase(error, case));
        }
    };
    if let Err(error) = assert_turn_drained(invoker, options, case, &start.provider_turn_id) {
        best_effort_cleanup(invoker, options, case, &start.provider_turn_id);
        return Err(error);
    }
    finish_turn(invoker, options, case, &start.provider_turn_id)?;
    // Cleanup is required to be idempotent and safe after completion races.
    finish_turn(invoker, options, case, &start.provider_turn_id)?;
    cancel_turn(invoker, options, case, &start.provider_turn_id)?;
    assert_turn_drained(invoker, options, case, &start.provider_turn_id)?;
    Ok((summary, text))
}

fn best_effort_cleanup<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    provider_turn_id: &str,
) where
    I: BlockingModelProviderInvoker,
{
    let _ = cancel_turn(invoker, options, case, provider_turn_id);
    let _ = finish_turn(invoker, options, case, provider_turn_id);
}

fn assert_turn_drained<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    provider_turn_id: &str,
) -> Result<(), ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let response: PollTurnEventsResponse = invoke(
        invoker,
        options,
        case,
        bcode_model::OP_POLL_TURN_EVENTS,
        &PollTurnEventsRequest {
            provider_turn_id: provider_turn_id.to_string(),
        },
    )?;
    if response.events.is_empty() {
        Ok(())
    } else {
        violation(
            case,
            "polling replayed events after terminal delivery or cleanup",
        )
    }
}

fn cancel_turn<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    provider_turn_id: &str,
) -> Result<(), ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let _: AckResponse = invoke(
        invoker,
        options,
        case,
        bcode_model::OP_CANCEL_TURN,
        &CancelTurnRequest {
            provider_turn_id: provider_turn_id.to_string(),
        },
    )?;
    Ok(())
}

fn finish_turn<I>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    provider_turn_id: &str,
) -> Result<(), ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
{
    let _: AckResponse = invoke(
        invoker,
        options,
        case,
        bcode_model::OP_FINISH_TURN,
        &FinishTurnRequest {
            provider_turn_id: provider_turn_id.to_string(),
        },
    )?;
    Ok(())
}

fn invoke<I, Q, R>(
    invoker: &mut I,
    options: &ProviderConformanceOptions,
    case: &'static str,
    operation: &'static str,
    request: &Q,
) -> Result<R, ProviderConformanceError>
where
    I: BlockingModelProviderInvoker,
    Q: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    invoker
        .invoke_json(options.provider_plugin_id.as_deref(), operation, request)
        .map_err(|message| ProviderConformanceError::Invocation { case, message })
}

fn validate_provider_identity(
    provider: &ProviderCapabilities,
) -> Result<(), ProviderConformanceError> {
    if provider.provider_id.trim().is_empty() || provider.display_name.trim().is_empty() {
        return violation(
            "capabilities",
            "provider id and display name must be non-empty",
        );
    }
    if !provider
        .capabilities
        .contains(&ProviderCapability::Streaming)
    {
        return violation(
            "capabilities",
            "the v1 baseline requires normalized streaming events",
        );
    }
    if provider
        .capabilities
        .contains(&ProviderCapability::ParallelToolCalls)
        && !provider.capabilities.contains(&ProviderCapability::Tools)
    {
        return violation(
            "capabilities",
            "parallel tool calls require the provider Tools capability",
        );
    }
    Ok(())
}

fn select_and_validate_model(
    provider: &ProviderCapabilities,
    models: &[ModelInfo],
    requested: Option<&str>,
) -> Result<ModelInfo, ProviderConformanceError> {
    if models.is_empty() {
        return violation("models", "provider returned no models");
    }
    let mut ids = BTreeSet::new();
    for model in models {
        if model.model_id.trim().is_empty() || model.display_name.trim().is_empty() {
            return violation("models", "model id and display name must be non-empty");
        }
        if !ids.insert(model.model_id.as_str()) {
            return violation("models", "provider returned duplicate model ids");
        }
        validate_model_capabilities(provider, model)?;
    }
    let selected = requested
        .and_then(|model_id| models.iter().find(|model| model.model_id == model_id))
        .or_else(|| models.iter().find(|model| model.is_default))
        .or_else(|| models.first())
        .cloned()
        .ok_or_else(|| ProviderConformanceError::Violation {
            case: "models",
            message: "requested model was not returned by the provider".to_string(),
        })?;
    if requested.is_some_and(|model_id| selected.model_id != model_id) {
        return violation("models", "requested model was not returned by the provider");
    }
    if !selected
        .capabilities
        .contains(&ModelCapability::StreamingText)
    {
        return violation("models", "selected model does not advertise streaming text");
    }
    Ok(selected)
}

fn validate_model_capabilities(
    provider: &ProviderCapabilities,
    model: &ModelInfo,
) -> Result<(), ProviderConformanceError> {
    let pairs = [
        (ProviderCapability::Tools, ModelCapability::ToolCalls),
        (
            ProviderCapability::ParallelToolCalls,
            ModelCapability::ParallelToolCalls,
        ),
        (ProviderCapability::JsonMode, ModelCapability::JsonMode),
        (
            ProviderCapability::PromptCaching,
            ModelCapability::PromptCaching,
        ),
        (
            ProviderCapability::NativeWebSearch,
            ModelCapability::NativeWebSearch,
        ),
        (ProviderCapability::CodeSearch, ModelCapability::CodeSearch),
    ];
    for (provider_capability, model_capability) in pairs {
        if model.capabilities.contains(&model_capability)
            && !provider.capabilities.contains(&provider_capability)
        {
            return violation(
                "models",
                format!(
                    "model '{}' advertises {model_capability:?} but provider omits {provider_capability:?}",
                    model.model_id
                ),
            );
        }
    }
    if model
        .capabilities
        .contains(&ModelCapability::ParallelToolCalls)
        && !model.capabilities.contains(&ModelCapability::ToolCalls)
    {
        return violation(
            "models",
            "parallel tool calls require model ToolCalls capability",
        );
    }
    validate_granular_capabilities(provider, model)?;
    Ok(())
}

fn validate_granular_capabilities(
    provider: &ProviderCapabilities,
    model: &ModelInfo,
) -> Result<(), ProviderConformanceError> {
    if !provider.feature_support.has_complete_inventory() {
        return violation(
            "models",
            "provider granular feature inventory is incomplete",
        );
    }
    for feature in granular_feature_inventory() {
        let negotiated = provider
            .feature_support
            .negotiate(&model.feature_support, feature);
        if negotiated.is_guaranteed()
            && !feature
                .support_in(&provider.feature_support)
                .is_guaranteed()
        {
            return violation(
                "models",
                format!("feature {feature:?} was guaranteed without provider support"),
            );
        }
    }
    Ok(())
}

fn granular_feature_inventory() -> Vec<bcode_model::RequestedModelFeature> {
    use bcode_model::{
        MediaInputFeature, ModelParameterKey, PromptCacheFeature, RequestedModelFeature,
        StructuredOutputMode, ToolChoiceMode,
    };
    let mut features = Vec::new();
    features.extend(
        [
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
        .map(RequestedModelFeature::Parameter),
    );
    features.extend(
        [
            StructuredOutputMode::JsonSchema,
            StructuredOutputMode::StrictJsonSchema,
        ]
        .into_iter()
        .map(RequestedModelFeature::StructuredOutput),
    );
    features.extend(
        [
            ToolChoiceMode::Auto,
            ToolChoiceMode::None,
            ToolChoiceMode::Required,
            ToolChoiceMode::Named,
            ToolChoiceMode::Parallel,
        ]
        .into_iter()
        .map(RequestedModelFeature::ToolChoice),
    );
    features.extend(
        [
            PromptCacheFeature::ConversationPrefix,
            PromptCacheFeature::ExplicitSystem,
            PromptCacheFeature::ExplicitTools,
            PromptCacheFeature::ExplicitMessage,
            PromptCacheFeature::Ttl,
        ]
        .into_iter()
        .map(RequestedModelFeature::PromptCache),
    );
    features.extend(
        [
            MediaInputFeature::UserImage,
            MediaInputFeature::SystemImage,
            MediaInputFeature::AssistantImage,
            MediaInputFeature::ToolMessageImage,
            MediaInputFeature::ImageReference,
            MediaInputFeature::ToolResultImage,
        ]
        .into_iter()
        .map(RequestedModelFeature::MediaInput),
    );
    features
}

fn supports(
    provider: &ProviderCapabilities,
    model: &ModelInfo,
    provider_capability: ProviderCapability,
    model_capability: ModelCapability,
) -> bool {
    provider.capabilities.contains(&provider_capability)
        && model.capabilities.contains(&model_capability)
}

fn turn_request(
    options: &ProviderConformanceOptions,
    model_id: &str,
    prompt: &str,
) -> ModelTurnRequest {
    let session_id = bcode_session_models::SessionId::new();
    ModelTurnRequest {
        session_id,
        turn_id: format!("provider-conformance-{session_id}"),
        model_id: model_id.to_string(),
        provider_context: options.provider_context.clone(),
        system_prompt: Some(
            "Follow the conformance request exactly. Do not add commentary.".to_string(),
        ),
        messages: vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
        }],
        tools: Vec::new(),
        tool_call_policy: ToolCallRequestPolicy::default(),
        parameters: ModelParameters::default(),
        structured_output: None,
        context_management: bcode_model::ContextManagementRequest::default(),
        prompt_cache: PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: std::iter::once(("bcode_request_kind".to_string(), "conformance".to_string()))
            .collect(),
    }
}

const fn passed(name: &'static str) -> ProviderConformanceCase {
    ProviderConformanceCase {
        name,
        outcome: ProviderConformanceOutcome::Passed,
    }
}

fn skipped(name: &'static str, reason: impl Into<String>) -> ProviderConformanceCase {
    ProviderConformanceCase {
        name,
        outcome: ProviderConformanceOutcome::Skipped {
            reason: reason.into(),
        },
    }
}

fn violation<T>(
    case: &'static str,
    message: impl Into<String>,
) -> Result<T, ProviderConformanceError> {
    Err(ProviderConformanceError::Violation {
        case,
        message: message.into(),
    })
}

fn recase(error: ProviderConformanceError, case: &'static str) -> ProviderConformanceError {
    match error {
        ProviderConformanceError::Invocation { message, .. } => {
            ProviderConformanceError::Invocation { case, message }
        }
        ProviderConformanceError::Violation { message, .. } => {
            ProviderConformanceError::Violation { case, message }
        }
        ProviderConformanceError::Timeout {
            provider_turn_id, ..
        } => ProviderConformanceError::Timeout {
            case,
            provider_turn_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderEventValidator;
    use bcode_model::{
        ProviderError, ProviderErrorCategory, ProviderTurnEvent, StopReason, TokenUsage, ToolCall,
    };

    #[test]
    fn validator_accepts_text_usage_warning_metadata_and_completion() {
        let mut validator = ProviderEventValidator::default();
        validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::ProviderMetadata {
                    key: "response_id".to_string(),
                    value: "response-1".to_string(),
                },
                ProviderTurnEvent::Warning {
                    message: "test warning".to_string(),
                },
                ProviderTurnEvent::TextDelta {
                    text: "done".to_string(),
                },
                ProviderTurnEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: Some(2),
                        output_tokens: Some(1),
                        total_tokens: Some(3),
                        ..TokenUsage::default()
                    },
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ])
            .expect("valid events");
        let summary = validator.finish().expect("valid terminal state");
        assert_eq!(summary.stop_reason, StopReason::EndTurn);
        assert_eq!(summary.usage_events, 1);
    }

    #[test]
    fn validator_accepts_correlated_tool_call() {
        let mut validator = ProviderEventValidator::default();
        validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::ToolCallStarted {
                    call_id: "call-1".to_string(),
                    name: "read".to_string(),
                },
                ProviderTurnEvent::ToolCallDelta {
                    call_id: "call-1".to_string(),
                    delta: "{}".to_string(),
                },
                ProviderTurnEvent::ToolCallFinished {
                    call: ToolCall {
                        id: "call-1".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
                ProviderTurnEvent::Usage {
                    usage: TokenUsage::default(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::ToolCall,
                },
            ])
            .expect("valid events");
        assert_eq!(validator.finish().expect("valid turn").tool_calls.len(), 1);
    }

    #[test]
    fn validator_accepts_normalized_error() {
        let mut validator = ProviderEventValidator::default();
        validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::Usage {
                    usage: TokenUsage::default(),
                },
                ProviderTurnEvent::Error {
                    error: ProviderError {
                        code: "rate_limit".to_string(),
                        category: ProviderErrorCategory::RateLimit,
                        message: "try later".to_string(),
                        retryable: true,
                        provider_message: None,
                        failure: None,
                        request_id: None,
                        diagnostic_context: Box::default(),
                        sources: Box::default(),
                        retry: None,
                    },
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::Error,
                },
            ])
            .expect("valid events");
        assert_eq!(
            validator.finish().expect("valid turn").error_category,
            Some(ProviderErrorCategory::RateLimit)
        );
    }

    #[test]
    fn validator_rejects_events_after_terminal_completion() {
        let mut validator = ProviderEventValidator::default();
        let error = validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::Usage {
                    usage: TokenUsage::default(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
                ProviderTurnEvent::TextDelta {
                    text: "late".to_string(),
                },
            ])
            .expect_err("late event must fail");
        assert!(error.to_string().contains("after TurnFinished"));
    }

    #[test]
    fn validator_rejects_missing_usage_for_successful_turn() {
        let mut validator = ProviderEventValidator::default();
        validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::TextDelta {
                    text: "done".to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ])
            .expect("ordered events should be observed");
        let error = validator.finish().expect_err("missing usage must fail");
        assert!(error.to_string().contains("usage metadata"));
    }

    #[test]
    fn validator_rejects_incoherent_usage() {
        let mut validator = ProviderEventValidator::default();
        let error = validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        total_tokens: Some(12),
                        ..TokenUsage::default()
                    },
                },
            ])
            .expect_err("incoherent usage must fail");
        assert!(error.to_string().contains("input plus output"));
    }

    #[test]
    fn validator_rejects_mismatched_error_terminal_reason() {
        let mut validator = ProviderEventValidator::default();
        validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::Error {
                    error: ProviderError {
                        code: "timeout".to_string(),
                        category: ProviderErrorCategory::Timeout,
                        message: "provider timed out".to_string(),
                        retryable: true,
                        provider_message: None,
                        failure: None,
                        request_id: None,
                        diagnostic_context: Box::default(),
                        sources: Box::default(),
                        retry: None,
                    },
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ])
            .expect("ordered events should be observed");
        let error = validator.finish().expect_err("error mismatch must fail");
        assert!(error.to_string().contains("TurnFinished(Error)"));
    }

    #[test]
    fn validator_rejects_unstarted_tool_delta() {
        let mut validator = ProviderEventValidator::default();
        let error = validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::ToolCallDelta {
                    call_id: "missing".to_string(),
                    delta: "{}".to_string(),
                },
            ])
            .expect_err("unknown tool call must fail");
        assert!(error.to_string().contains("unknown call id"));
    }

    #[test]
    fn validator_rejects_retry_hint_on_non_retryable_error() {
        let mut validator = ProviderEventValidator::default();
        let error = validator
            .observe(&[
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::Error {
                    error: ProviderError {
                        code: "invalid".to_string(),
                        category: ProviderErrorCategory::InvalidRequest,
                        message: "invalid request".to_string(),
                        retryable: false,
                        provider_message: None,
                        failure: None,
                        request_id: None,
                        diagnostic_context: Box::default(),
                        sources: Box::default(),
                        retry: Some(Box::new(bcode_model::ProviderRetryHint {
                            retry_after_ms: Some(10),
                            retry_at_unix: None,
                            source: Some("header".to_string()),
                        })),
                    },
                },
            ])
            .expect_err("non-retryable hint must fail");
        assert!(error.to_string().contains("non-retryable"));
    }
}
