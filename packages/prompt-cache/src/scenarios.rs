//! Prompt-cache verification scenarios driven through public provider operations.
//!
//! The suite works against any [`BlockingModelProviderInvoker`]: an in-process adapter, a loaded
//! native plugin, or a CLI plugin host. It sends only normalized [`bcode_model`] requests and
//! judges only normalized usage and request-projection events, so it verifies the provider's
//! caching behavior as Bcode observes it rather than any wire format.

use crate::analysis::{analyze_rounds, analyze_warm_repeat};
use crate::expectations::{PromptCacheExpectationOverrides, expectations_from_claims};
use crate::planning::{PromptCachePlanInput, estimated_tool_definition_tokens, plan_prompt_cache};
use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MessageRole, ModelInfo,
    ModelList, ModelListRequest, ModelMessage, ModelParameters, ModelTurnRequest,
    PollTurnEventsRequest, PollTurnEventsResponse, PromptCacheHints, PromptCacheMode,
    PromptCachePoint, ProviderCapabilities, ProviderCapabilitiesRequest, ProviderErrorCategory,
    ProviderRequestContext, ProviderTurnEvent, StartTurnResponse, StopReason, ToolCall,
    ToolCallRequestPolicy, ToolChoice, ToolDefinition, ToolResult,
};
use bcode_model_provider_runtime::{BlockingModelProviderInvoker, ProviderEventValidator};
use bcode_prompt_cache_models::{
    CacheRoundObservation, PROMPT_CACHE_VERIFICATION_REPORT_SCHEMA_VERSION,
    PromptCacheExpectations, PromptCacheMechanism, PromptCacheScenarioOutcome,
    PromptCacheScenarioResult, PromptCacheVerificationReport, measurement,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::time::{Duration, Instant};

/// Stable scenario identifiers.
pub mod scenario {
    /// Cold request: points accounted, no drops, coherent usage.
    pub const COLD_REQUEST: &str = "cold_request";
    /// Identical repeat of the cold request reads from cache.
    pub const WARM_SAME_PREFIX: &str = "warm_same_prefix";
    /// Appending turns keeps cached tokens non-decreasing.
    pub const GROWING_CONVERSATION: &str = "growing_conversation";
    /// Tool call / result continuation rounds keep hitting the cache.
    pub const TOOL_LOOP: &str = "tool_loop";
    /// Every advertised TTL is accepted and echoed; unadvertised TTLs fail closed.
    pub const TTL_MATRIX: &str = "ttl_matrix";
    /// `PromptCacheMode::Off` emits no points and claims no cached tokens.
    pub const MODE_OFF: &str = "mode_off";
    /// Over-budget explicit points are dropped with accounting, not an error.
    pub const BUDGET_OVERFLOW: &str = "budget_overflow";
}

/// Inputs for [`run_prompt_cache_scenarios`].
#[derive(Debug, Clone)]
pub struct PromptCacheScenarioOptions {
    /// Plugin id used to route each operation. `None` lets the invoker select its default.
    pub provider_plugin_id: Option<String>,
    /// Provider context containing the credentials and endpoint under test.
    pub provider_context: ProviderRequestContext,
    /// Model to test. `None` selects the provider's default or first listed model.
    pub model_id: Option<String>,
    /// Catalog-resolved model metadata to use instead of the provider's raw listing.
    ///
    /// Hosts resolve provider listings through the model catalog, which can expand models the
    /// provider cannot discover on its own and enrich cache claims. Callers that have already
    /// performed that resolution pass the result here so the suite judges against the same
    /// claims the daemon would use; `model_id` is then ignored for lookup.
    pub resolved_model: Option<ModelInfo>,
    /// Expectation overrides for facts the claim source cannot express.
    pub overrides: PromptCacheExpectationOverrides,
    /// Number of tool rounds in the tool-loop scenario.
    pub tool_rounds: usize,
    /// Number of appended exchanges in the growing-conversation scenario.
    pub conversation_turns: usize,
    /// Maximum time to wait for each provider turn to terminate.
    pub turn_timeout: Duration,
    /// Delay between empty event polls.
    pub poll_interval: Duration,
}

impl Default for PromptCacheScenarioOptions {
    fn default() -> Self {
        Self {
            provider_plugin_id: None,
            provider_context: ProviderRequestContext::default(),
            model_id: None,
            resolved_model: None,
            overrides: PromptCacheExpectationOverrides::default(),
            tool_rounds: 6,
            conversation_turns: 4,
            turn_timeout: Duration::from_mins(1),
            poll_interval: Duration::from_millis(10),
        }
    }
}

/// Failure that prevents the scenario suite from producing a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptCacheScenarioError {
    /// A typed provider operation could not be invoked or decoded.
    Invocation {
        /// Scenario that was running.
        scenario: &'static str,
        /// Invoker error message.
        message: String,
    },
    /// The provider emitted events that violate the provider contract.
    Contract {
        /// Scenario that was running.
        scenario: &'static str,
        /// Contract violation.
        message: String,
    },
    /// A turn did not terminate within the configured bound.
    Timeout {
        /// Scenario that was running.
        scenario: &'static str,
        /// Provider turn that timed out.
        provider_turn_id: String,
    },
}

impl fmt::Display for PromptCacheScenarioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invocation { scenario, message } => {
                write!(
                    formatter,
                    "prompt cache scenario '{scenario}' could not run: {message}"
                )
            }
            Self::Contract { scenario, message } => write!(
                formatter,
                "prompt cache scenario '{scenario}' hit a provider contract violation: {message}"
            ),
            Self::Timeout {
                scenario,
                provider_turn_id,
            } => write!(
                formatter,
                "prompt cache scenario '{scenario}' timed out waiting for turn '{provider_turn_id}'"
            ),
        }
    }
}

impl std::error::Error for PromptCacheScenarioError {}

/// Run every prompt-cache scenario against the selected provider/model.
///
/// Scenarios that do not apply to the advertised mechanism are reported as skipped. The suite
/// stops early only on invocation, contract, or timeout failures; behavioral mismatches are
/// recorded as failed scenarios in the report.
///
/// # Errors
///
/// Returns an error when discovery fails, a provider operation cannot be invoked, the provider
/// violates the event contract, or a turn does not terminate within `turn_timeout`.
pub fn run_prompt_cache_scenarios<I>(
    invoker: &mut I,
    options: &PromptCacheScenarioOptions,
) -> Result<PromptCacheVerificationReport, PromptCacheScenarioError>
where
    I: BlockingModelProviderInvoker,
{
    let (provider, model) = discover(invoker, options)?;
    let expectations = expectations_from_claims(&provider, &model, &options.overrides);
    let mut report = PromptCacheVerificationReport {
        schema_version: PROMPT_CACHE_VERIFICATION_REPORT_SCHEMA_VERSION,
        provider_id: provider.provider_id,
        model_id: model.model_id.clone(),
        expectations: expectations.clone(),
        scenarios: Vec::new(),
    };
    let Some(expectations) = expectations else {
        for name in [
            scenario::COLD_REQUEST,
            scenario::WARM_SAME_PREFIX,
            scenario::GROWING_CONVERSATION,
            scenario::TOOL_LOOP,
            scenario::TTL_MATRIX,
            scenario::MODE_OFF,
            scenario::BUDGET_OVERFLOW,
        ] {
            report.scenarios.push(skipped(
                name,
                "provider/model do not advertise prompt caching",
            ));
        }
        return Ok(report);
    };

    let mut runner = ScenarioRunner {
        invoker,
        options,
        model: &model,
        expectations: &expectations,
        workload: Workload::new(&expectations),
    };
    let (cold, warm) = runner.cold_and_warm()?;
    report.scenarios.push(cold);
    report.scenarios.push(warm);
    report.scenarios.push(runner.growing_conversation()?);
    report.scenarios.push(runner.tool_loop()?);
    report.scenarios.push(runner.ttl_matrix()?);
    report.scenarios.push(runner.mode_off()?);
    report.scenarios.push(runner.budget_overflow()?);
    Ok(report)
}

struct ScenarioRunner<'a, I> {
    invoker: &'a mut I,
    options: &'a PromptCacheScenarioOptions,
    model: &'a ModelInfo,
    expectations: &'a PromptCacheExpectations,
    workload: Workload,
}

/// Deterministic request content sized so its stable prefix clears the minimum cacheable length.
struct Workload {
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    tool_definition_tokens: u64,
}

/// Minimum stable system-prompt size, in words, for the verification workload.
///
/// Bcode's real coding system prompt plus tool definitions is several thousand tokens; sizing the
/// workload similarly keeps the late-tail and write-amplification checks representative of
/// production requests rather than of a degenerate tiny prefix.
const STABLE_PREFIX_MIN_WORDS: usize = 2_048;

impl Workload {
    fn new(expectations: &PromptCacheExpectations) -> Self {
        // Both the character-based host estimate and whitespace-based test providers must see a
        // prefix above the minimum, so size by words and pad generously.
        let target_words = usize::try_from(expectations.min_prefix_tokens)
            .unwrap_or(usize::MAX)
            .saturating_mul(2)
            .max(STABLE_PREFIX_MIN_WORDS);
        // Real provider caches are keyed by content, not by the caller's cache key, so a prefix
        // reused from an earlier run would already be warm. Salt the stable prefix per suite run
        // so the cold request is genuinely cold while every scenario within the run shares it.
        let run_salt = bcode_session_models::SessionId::new();
        let mut system_prompt = format!(
            "You are a deterministic prompt-cache verification assistant (run {run_salt}). Reply briefly.\n"
        );
        let mut words = system_prompt.split_whitespace().count();
        let mut word = 0_usize;
        while words < target_words {
            let _ = write!(system_prompt, "stable-context-token-{word:05} ");
            word += 1;
            words += 1;
        }
        let tools = vec![ToolDefinition {
            name: "cache_probe.read".to_string(),
            description: "Return deterministic verification content for a numbered probe."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"index": {"type": "integer"}},
                "required": ["index"],
                "additionalProperties": false
            }),
        }];
        let tool_definition_tokens = estimated_tool_definition_tokens(&tools);
        Self {
            system_prompt,
            tools,
            tool_definition_tokens,
        }
    }
}

impl<I> ScenarioRunner<'_, I>
where
    I: BlockingModelProviderInvoker,
{
    fn base_request(&self, mode: PromptCacheMode, with_tools: bool) -> ModelTurnRequest {
        let session_id = bcode_session_models::SessionId::new();
        ModelTurnRequest {
            session_id,
            turn_id: format!("prompt-cache-{session_id}"),
            model_id: self.model.model_id.clone(),
            provider_context: self.options.provider_context.clone(),
            system_prompt: Some(self.workload.system_prompt.clone()),
            messages: Vec::new(),
            tools: if with_tools {
                self.workload.tools.clone()
            } else {
                Vec::new()
            },
            tool_call_policy: ToolCallRequestPolicy {
                choice: if with_tools {
                    ToolChoice::Auto
                } else {
                    ToolChoice::None
                },
                ..ToolCallRequestPolicy::default()
            },
            tool_schema_mode: None,
            parameters: ModelParameters {
                // Hosts resolve the model's advertised output limit into every request; some
                // adapters refuse to substitute a local default.
                max_output_tokens: self.model.max_output_tokens.map(|limit| limit.min(1_024)),
                ..ModelParameters::default()
            },
            structured_output: None,
            context_management: bcode_model::ContextManagementRequest::default(),
            prompt_cache: PromptCacheHints {
                mode,
                ..PromptCacheHints::default()
            },
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: std::iter::once((
                "bcode_request_kind".to_string(),
                "prompt_cache_verification".to_string(),
            ))
            .collect(),
        }
    }

    /// Apply host planning exactly as the daemon would for this model.
    fn plan(&self, request: &mut ModelTurnRequest, mode: PromptCacheMode) {
        let hints = plan_prompt_cache(
            &mut request.messages,
            &PromptCachePlanInput {
                mode,
                cache_key: format!("prompt-cache-verification:{}", request.session_id),
                cache: &self.model.cache,
                system_prompt: request.system_prompt.as_deref(),
                tool_definition_tokens: self.workload.tool_definition_tokens,
            },
        );
        request.prompt_cache = hints;
        if mode.is_enabled() {
            self.annotate_cache_capabilities(request);
        }
    }

    /// Mirror the host's `model_cache_capabilities` metadata so adapters that key explicit
    /// behavior on it (for example Responses-surface adapters) behave as in production.
    fn annotate_cache_capabilities(&self, request: &mut ModelTurnRequest) {
        let capabilities = self
            .model
            .cache
            .capabilities
            .iter()
            .map(|capability| {
                serde_json::to_value(capability)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default()
            })
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(",");
        request
            .metadata
            .insert("model_cache_capabilities".to_string(), capabilities);
    }

    fn cold_and_warm(
        &mut self,
    ) -> Result<(PromptCacheScenarioResult, PromptCacheScenarioResult), PromptCacheScenarioError>
    {
        let mut request = self.base_request(PromptCacheMode::Auto, true);
        request.messages = vec![
            user("Reply with exactly: cache verification round one."),
            assistant("cache verification round one."),
            user("Reply with exactly: cache verification round two."),
        ];
        self.plan(&mut request, PromptCacheMode::Auto);
        let cold = self.run(scenario::COLD_REQUEST, &request)?;
        let warm = self.run(scenario::WARM_SAME_PREFIX, &request)?;

        let mut cold_result = PromptCacheScenarioResult {
            scenario: scenario::COLD_REQUEST.to_string(),
            outcome: PromptCacheScenarioOutcome::Passed,
            measurements: BTreeMap::new(),
            rounds: vec![cold.clone()],
        };
        let mut failures = Vec::new();
        if !cold.valid_input_breakdown {
            failures.push("cold request reported cache subsets exceeding its input".to_string());
        }
        if cold.dropped_cache_points.is_some_and(|dropped| dropped > 0) {
            failures.push(format!(
                "cold request dropped {} host cache points under normal planning",
                cold.dropped_cache_points.unwrap_or_default()
            ));
        }
        if self.expectations.mechanism == PromptCacheMechanism::ExplicitPoints
            && cold.requested_cache_points.is_none()
        {
            failures.push(
                "explicit-cache provider omitted cache point accounting from its request projection"
                    .to_string(),
            );
        }
        if cold.has_cache_read() {
            failures.push(
                "cold request with a fresh cache key reported cached input tokens".to_string(),
            );
        }
        cold_result.measurements.insert(
            measurement::CACHE_WRITE_INPUT_TOKENS.to_string(),
            f64::from(cold.cache_write_input_tokens.unwrap_or_default()),
        );
        cold_result.measurements.insert(
            measurement::INPUT_TOKENS.to_string(),
            f64::from(cold.input_tokens.unwrap_or_default()),
        );
        if !failures.is_empty() {
            cold_result.outcome = PromptCacheScenarioOutcome::Failed {
                reason: failures.join("; "),
            };
        }

        let analysis = analyze_warm_repeat(&cold, &warm, self.expectations);
        let warm_result = PromptCacheScenarioResult {
            scenario: scenario::WARM_SAME_PREFIX.to_string(),
            outcome: analysis.outcome(),
            measurements: analysis.measurements,
            rounds: vec![cold, warm],
        };
        Ok((cold_result, warm_result))
    }

    fn growing_conversation(
        &mut self,
    ) -> Result<PromptCacheScenarioResult, PromptCacheScenarioError> {
        let mut request = self.base_request(PromptCacheMode::Auto, false);
        let mut rounds = Vec::new();
        for turn in 0..=self.options.conversation_turns {
            if turn > 0 {
                request
                    .messages
                    .push(assistant(&format!("acknowledged turn {turn}.")));
            }
            request.messages.push(user(&format!(
                "Reply with exactly: growing conversation turn {turn}."
            )));
            self.plan(&mut request, PromptCacheMode::Auto);
            rounds.push(self.run(scenario::GROWING_CONVERSATION, &request)?);
        }
        let analysis = analyze_rounds(&rounds, self.expectations);
        Ok(PromptCacheScenarioResult {
            scenario: scenario::GROWING_CONVERSATION.to_string(),
            outcome: analysis.outcome(),
            measurements: analysis.measurements,
            rounds,
        })
    }

    fn tool_loop(&mut self) -> Result<PromptCacheScenarioResult, PromptCacheScenarioError> {
        let mut request = self.base_request(PromptCacheMode::Auto, true);
        request.messages.push(user(
            "Read probes 0 through N using cache_probe.read, one per turn, then reply done.",
        ));
        let mut rounds = Vec::new();
        for round in 0..self.options.tool_rounds {
            request.messages.push(ModelMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    call: ToolCall {
                        id: format!("probe-{round}"),
                        name: "cache_probe.read".to_string(),
                        arguments: serde_json::json!({"index": round}),
                    },
                }],
            });
            request.messages.push(ModelMessage {
                role: MessageRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    result: ToolResult {
                        call_id: format!("probe-{round}"),
                        output: format!("probe {round} deterministic content line ").repeat(48),
                        is_error: false,
                        content: Vec::new(),
                    },
                }],
            });
            self.plan(&mut request, PromptCacheMode::Auto);
            rounds.push(
                self.run(scenario::TOOL_LOOP, &request)?
                    .with_tool_round(true),
            );
        }
        let analysis = analyze_rounds(&rounds, self.expectations);
        Ok(PromptCacheScenarioResult {
            scenario: scenario::TOOL_LOOP.to_string(),
            outcome: analysis.outcome(),
            measurements: analysis.measurements,
            rounds,
        })
    }

    fn ttl_matrix(&mut self) -> Result<PromptCacheScenarioResult, PromptCacheScenarioError> {
        if self.expectations.mechanism != PromptCacheMechanism::ExplicitPoints {
            return Ok(skipped(
                scenario::TTL_MATRIX,
                "automatic-prefix caches take no TTL hints",
            ));
        }
        if self.expectations.ttl_seconds.is_empty() {
            return Ok(skipped(
                scenario::TTL_MATRIX,
                "model advertises no explicit cache TTLs",
            ));
        }
        let mut rounds = Vec::new();
        let mut failures = Vec::new();
        for ttl in self.expectations.ttl_seconds.clone() {
            let mut request = self.base_request(PromptCacheMode::Auto, false);
            request.messages = vec![
                user(&format!("Reply with exactly: ttl {ttl} round one.")),
                assistant(&format!("ttl {ttl} round one.")),
                user(&format!("Reply with exactly: ttl {ttl} round two.")),
            ];
            self.plan(&mut request, PromptCacheMode::Auto);
            request.prompt_cache.ttl_seconds = Some(ttl);
            for message in &mut request.messages {
                for block in &mut message.content {
                    if let ContentBlock::CachePoint { hint } = block {
                        hint.ttl_seconds = Some(ttl);
                    }
                }
            }
            let cold = self.run(scenario::TTL_MATRIX, &request)?;
            if cold.has_cache_write()
                && cold
                    .cache_ttl_seconds
                    .is_some_and(|reported| reported != ttl)
            {
                failures.push(format!(
                    "requested TTL {ttl}s but the provider confirmed {}s",
                    cold.cache_ttl_seconds.unwrap_or_default()
                ));
            }
            if cold.has_cache_write()
                && cold
                    .cache_write_tokens_by_ttl
                    .iter()
                    .any(|entry| entry.ttl_seconds.is_some_and(|reported| reported != ttl))
            {
                failures.push(format!(
                    "requested TTL {ttl}s but detailed cache-write buckets carry another TTL"
                ));
            }
            rounds.push(cold);
        }

        // An unadvertised TTL must fail closed before any provider call.
        let unadvertised = self
            .expectations
            .ttl_seconds
            .iter()
            .max()
            .copied()
            .unwrap_or(3_600)
            .saturating_mul(7)
            .saturating_add(13);
        let mut request = self.base_request(PromptCacheMode::Auto, false);
        request.messages = vec![user("Reply with exactly: unsupported ttl probe.")];
        self.plan(&mut request, PromptCacheMode::Auto);
        request.prompt_cache.ttl_seconds = Some(unadvertised);
        match self.run_expecting_error(scenario::TTL_MATRIX, &request)? {
            Some(ProviderErrorCategory::UnsupportedFeature) => {}
            Some(other) => failures.push(format!(
                "unadvertised TTL {unadvertised}s was rejected as {other:?} instead of UnsupportedFeature"
            )),
            None => failures.push(format!(
                "unadvertised TTL {unadvertised}s was accepted instead of failing closed"
            )),
        }

        Ok(PromptCacheScenarioResult {
            scenario: scenario::TTL_MATRIX.to_string(),
            outcome: if failures.is_empty() {
                PromptCacheScenarioOutcome::Passed
            } else {
                PromptCacheScenarioOutcome::Failed {
                    reason: failures.join("; "),
                }
            },
            measurements: BTreeMap::new(),
            rounds,
        })
    }

    fn mode_off(&mut self) -> Result<PromptCacheScenarioResult, PromptCacheScenarioError> {
        let mut request = self.base_request(PromptCacheMode::Off, true);
        request.messages = vec![
            user("Reply with exactly: mode off round one."),
            assistant("mode off round one."),
            user("Reply with exactly: mode off round two."),
        ];
        self.plan(&mut request, PromptCacheMode::Off);
        let first = self.run(scenario::MODE_OFF, &request)?;
        let second = self.run(scenario::MODE_OFF, &request)?;
        let mut failures = Vec::new();
        if request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::CachePoint { .. }))
        {
            failures.push("planner emitted cache points with caching disabled".to_string());
        }
        if first
            .emitted_cache_points
            .is_some_and(|emitted| emitted > 0)
            || second
                .emitted_cache_points
                .is_some_and(|emitted| emitted > 0)
        {
            failures.push("provider emitted cache points with caching disabled".to_string());
        }
        if self.expectations.mechanism == PromptCacheMechanism::ExplicitPoints
            && (first.has_cache_write() || second.has_cache_write())
        {
            failures.push("provider wrote cache entries with caching disabled".to_string());
        }
        Ok(PromptCacheScenarioResult {
            scenario: scenario::MODE_OFF.to_string(),
            outcome: if failures.is_empty() {
                PromptCacheScenarioOutcome::Passed
            } else {
                PromptCacheScenarioOutcome::Failed {
                    reason: failures.join("; "),
                }
            },
            measurements: BTreeMap::new(),
            rounds: vec![first, second],
        })
    }

    fn budget_overflow(&mut self) -> Result<PromptCacheScenarioResult, PromptCacheScenarioError> {
        if self.expectations.mechanism != PromptCacheMechanism::ExplicitPoints {
            return Ok(skipped(
                scenario::BUDGET_OVERFLOW,
                "automatic-prefix caches have no explicit point budget",
            ));
        }
        let mut request = self.base_request(PromptCacheMode::Auto, true);
        let mut messages = Vec::new();
        for index in 0..8 {
            messages.push(user(&format!("Overflow probe message {index}.")));
            messages.push(assistant(&format!("overflow probe reply {index}.")));
        }
        // Conversations must end on a user turn: some models reject assistant prefill.
        messages.push(user("Reply with exactly: overflow probe complete."));
        request.messages = messages;
        self.plan(&mut request, PromptCacheMode::Auto);
        let ttl = request.prompt_cache.ttl_seconds;
        for message in &mut request.messages {
            if message.role == MessageRole::User
                && !matches!(
                    message.content.last(),
                    Some(ContentBlock::CachePoint { .. })
                )
            {
                message.content.push(ContentBlock::CachePoint {
                    hint: PromptCachePoint {
                        label: Some("overflow".to_string()),
                        ttl_seconds: ttl,
                    },
                });
            }
        }
        let requested = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|block| matches!(block, ContentBlock::CachePoint { .. }))
            .count();
        let round = self.run(scenario::BUDGET_OVERFLOW, &request)?;
        let mut failures = Vec::new();
        match (round.emitted_cache_points, round.dropped_cache_points) {
            (Some(emitted), Some(dropped)) => {
                if emitted.saturating_add(dropped) < requested {
                    failures.push(format!(
                        "projection accounted for {} of {requested} requested cache points",
                        emitted.saturating_add(dropped)
                    ));
                }
                if let Some(budget) = self.expectations.max_cache_points
                    && emitted > budget
                {
                    failures.push(format!(
                        "provider emitted {emitted} cache points above its declared budget {budget}"
                    ));
                }
            }
            _ => failures.push(
                "provider omitted emitted/dropped cache point accounting for an over-budget request"
                    .to_string(),
            ),
        }
        if !round.valid_input_breakdown {
            failures.push("over-budget request reported incoherent cache usage".to_string());
        }
        let mut measurements = BTreeMap::new();
        measurements.insert(
            measurement::DROPPED_CACHE_POINTS.to_string(),
            f64::from(
                u32::try_from(round.dropped_cache_points.unwrap_or_default()).unwrap_or(u32::MAX),
            ),
        );
        Ok(PromptCacheScenarioResult {
            scenario: scenario::BUDGET_OVERFLOW.to_string(),
            outcome: if failures.is_empty() {
                PromptCacheScenarioOutcome::Passed
            } else {
                PromptCacheScenarioOutcome::Failed {
                    reason: failures.join("; "),
                }
            },
            measurements,
            rounds: vec![round],
        })
    }

    /// Run one turn that must complete and return its cache observation.
    fn run(
        &mut self,
        scenario: &'static str,
        request: &ModelTurnRequest,
    ) -> Result<CacheRoundObservation, PromptCacheScenarioError> {
        let outcome = execute_turn(self.invoker, self.options, scenario, request)?;
        let (summary, usage) = (outcome.summary, outcome.usage);
        if matches!(
            summary.stop_reason,
            StopReason::Error | StopReason::Cancelled
        ) {
            let detail = outcome.error.map_or_else(
                || format!("{:?}", summary.error_category),
                |error| format!("{:?} {}: {}", error.category, error.code, error.message),
            );
            return Err(PromptCacheScenarioError::Contract {
                scenario,
                message: format!("turn ended with {:?} ({detail})", summary.stop_reason),
            });
        }
        let Some(usage) = usage else {
            return Err(PromptCacheScenarioError::Contract {
                scenario,
                message: "turn completed without a usage snapshot".to_string(),
            });
        };
        Ok(CacheRoundObservation::from_provider_usage(
            0,
            &usage,
            summary.request_projections.last(),
        ))
    }

    /// Run one turn that is expected to fail before generation; return its error category.
    fn run_expecting_error(
        &mut self,
        scenario: &'static str,
        request: &ModelTurnRequest,
    ) -> Result<Option<ProviderErrorCategory>, PromptCacheScenarioError> {
        Ok(execute_turn(self.invoker, self.options, scenario, request)?
            .summary
            .error_category)
    }
}

fn user(text: &str) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn assistant(text: &str) -> ModelMessage {
    ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn skipped(name: &str, reason: &str) -> PromptCacheScenarioResult {
    PromptCacheScenarioResult {
        scenario: name.to_string(),
        outcome: PromptCacheScenarioOutcome::Skipped {
            reason: reason.to_string(),
        },
        measurements: BTreeMap::new(),
        rounds: Vec::new(),
    }
}

fn discover<I>(
    invoker: &mut I,
    options: &PromptCacheScenarioOptions,
) -> Result<(ProviderCapabilities, ModelInfo), PromptCacheScenarioError>
where
    I: BlockingModelProviderInvoker,
{
    const SCENARIO: &str = "discovery";
    let provider: ProviderCapabilities = invoke(
        invoker,
        options,
        SCENARIO,
        bcode_model::OP_CAPABILITIES,
        &ProviderCapabilitiesRequest {
            provider_context: options.provider_context.clone(),
            selected_model_id: options.model_id.clone(),
        },
    )?;
    if let Some(model) = &options.resolved_model {
        return Ok((provider, model.clone()));
    }
    let models: ModelList = invoke(
        invoker,
        options,
        SCENARIO,
        bcode_model::OP_MODELS,
        &ModelListRequest {
            provider_context: options.provider_context.clone(),
            selected_model_id: options.model_id.clone(),
        },
    )?;
    let model = options
        .model_id
        .as_deref()
        .map_or_else(
            || {
                models
                    .models
                    .iter()
                    .find(|model| model.is_default)
                    .or_else(|| models.models.first())
            },
            |model_id| {
                models
                    .models
                    .iter()
                    .find(|model| model.model_id == model_id)
            },
        )
        .cloned()
        .ok_or_else(|| PromptCacheScenarioError::Invocation {
            scenario: SCENARIO,
            message: format!(
                "model {:?} was not listed by provider {}",
                options.model_id, provider.provider_id
            ),
        })?;
    Ok((provider, model))
}

struct TurnOutcome {
    summary: bcode_model_provider_runtime::ProviderEventSummary,
    usage: Option<bcode_model::TokenUsage>,
    /// Normalized provider error when the turn failed; carried for diagnostics only.
    error: Option<bcode_model::ProviderError>,
}

fn execute_turn<I>(
    invoker: &mut I,
    options: &PromptCacheScenarioOptions,
    scenario: &'static str,
    request: &ModelTurnRequest,
) -> Result<TurnOutcome, PromptCacheScenarioError>
where
    I: BlockingModelProviderInvoker,
{
    let start: StartTurnResponse = invoke(
        invoker,
        options,
        scenario,
        bcode_model::OP_START_TURN,
        request,
    )?;
    let deadline = Instant::now() + options.turn_timeout;
    let mut validator = ProviderEventValidator::default();
    let mut usage = None;
    let mut error = None;
    while !validator.is_terminal() {
        if Instant::now() >= deadline {
            cleanup(invoker, options, scenario, &start.provider_turn_id);
            return Err(PromptCacheScenarioError::Timeout {
                scenario,
                provider_turn_id: start.provider_turn_id,
            });
        }
        let response: PollTurnEventsResponse = match invoke(
            invoker,
            options,
            scenario,
            bcode_model::OP_POLL_TURN_EVENTS,
            &PollTurnEventsRequest {
                provider_turn_id: start.provider_turn_id.clone(),
            },
        ) {
            Ok(response) => response,
            Err(error) => {
                cleanup(invoker, options, scenario, &start.provider_turn_id);
                return Err(error);
            }
        };
        for event in &response.events {
            match event {
                ProviderTurnEvent::Usage { usage: snapshot } => usage = Some(snapshot.clone()),
                ProviderTurnEvent::Error { error: failure } => error = Some(failure.clone()),
                _ => {}
            }
        }
        if let Err(error) = validator.observe(&response.events) {
            cleanup(invoker, options, scenario, &start.provider_turn_id);
            return Err(PromptCacheScenarioError::Contract {
                scenario,
                message: error.to_string(),
            });
        }
        if response.events.is_empty() {
            std::thread::sleep(options.poll_interval);
        }
    }
    let summary = validator.finish().map_err(|error| {
        cleanup(invoker, options, scenario, &start.provider_turn_id);
        PromptCacheScenarioError::Contract {
            scenario,
            message: error.to_string(),
        }
    })?;
    let _: AckResponse = invoke(
        invoker,
        options,
        scenario,
        bcode_model::OP_FINISH_TURN,
        &FinishTurnRequest {
            provider_turn_id: start.provider_turn_id.clone(),
        },
    )?;
    Ok(TurnOutcome {
        summary,
        usage,
        error,
    })
}

fn cleanup<I>(
    invoker: &mut I,
    options: &PromptCacheScenarioOptions,
    scenario: &'static str,
    provider_turn_id: &str,
) where
    I: BlockingModelProviderInvoker,
{
    let _ = invoke::<I, _, AckResponse>(
        invoker,
        options,
        scenario,
        bcode_model::OP_CANCEL_TURN,
        &CancelTurnRequest {
            provider_turn_id: provider_turn_id.to_string(),
        },
    );
    let _ = invoke::<I, _, AckResponse>(
        invoker,
        options,
        scenario,
        bcode_model::OP_FINISH_TURN,
        &FinishTurnRequest {
            provider_turn_id: provider_turn_id.to_string(),
        },
    );
}

fn invoke<I, Q, R>(
    invoker: &mut I,
    options: &PromptCacheScenarioOptions,
    scenario: &'static str,
    operation: &'static str,
    request: &Q,
) -> Result<R, PromptCacheScenarioError>
where
    I: BlockingModelProviderInvoker,
    Q: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    invoker
        .invoke_json(options.provider_plugin_id.as_deref(), operation, request)
        .map_err(|message| PromptCacheScenarioError::Invocation { scenario, message })
}

/// Set of TTLs the suite will exercise for a model, for callers that display a plan first.
#[must_use]
pub fn planned_ttl_matrix(expectations: &PromptCacheExpectations) -> BTreeSet<u64> {
    if expectations.mechanism == PromptCacheMechanism::ExplicitPoints {
        expectations.ttl_seconds.clone()
    } else {
        BTreeSet::new()
    }
}
