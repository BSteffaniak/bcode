//! Native-like projection of managed external harness events.
//!
//! Each canonical external event is projected into ordinary renderer-neutral transcript effects:
//! user and assistant messages, reasoning activities, observed native tool cards, usage, compact
//! notices, and pending native interactions. Native text, reasoning, tool input and tool output
//! are untrusted display content. Nothing here dispatches Bcode tools, grants permission, enters
//! model context, or authorizes a native response; the daemon rechecks every response.
//!
//! The generic literal fallback (`external::canonical_external_text` and
//! `external::external_fallback_blocks`) remains available for renderers that cannot adapt these
//! items.

use bcode_harness_models::{
    HarnessCapabilities, HarnessCapability, HarnessFailure, HarnessInstance, HarnessInteraction,
    HarnessInteractionKind, HarnessObservation, HarnessObservationKind, HarnessPlacement,
    HarnessReconciliation, HarnessSupportLevel,
};
use bcode_session_models::external_backend::{ExternalBackendEnvelope, ExternalBackendRecord};
use bcode_session_models::external_harness::{
    ExternalHarnessOperation, ExternalHarnessOperationRecord as Record, ExternalHarnessRequest,
    ExternalHarnessRequestKind, ExternalHarnessResponseKind as Response,
};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionTokenUsage};
use bcode_session_view_models::external::{ExternalInteractionView, ExternalProvenanceView};
use std::fmt::Write as _;

use crate::external::{ExternalInteractionAvailability, external_interaction};

/// Label used when no harness display name is known.
pub const EXTERNAL_HARNESS_FALLBACK_LABEL: &str = "External harness";

/// Most pending native interactions retained by one view.
pub const MAX_PENDING_EXTERNAL_INTERACTIONS: usize = 16;

/// Longest prompt excerpt included in a compact interaction notice, in characters.
const NOTICE_PROMPT_CHARS: usize = 160;

/// Derive a generic display name from a harness plugin identifier.
///
/// The last dotted/slashed segment is used, generic `plugin`/`harness` suffix words are removed,
/// and the remaining words are title-cased (`bcode.claude-code` becomes `Claude Code`). No vendor
/// is special-cased; an identifier without usable words yields
/// [`EXTERNAL_HARNESS_FALLBACK_LABEL`].
#[must_use]
pub fn harness_display_label(plugin_id: &str) -> String {
    let segment = plugin_id
        .rsplit(['.', '/', ':'])
        .find(|segment| !segment.trim().is_empty())
        .unwrap_or_default();
    let mut words = segment
        .split(['-', '_', ' '])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    while words.len() > 1
        && words.last().is_some_and(|word| {
            word.eq_ignore_ascii_case("plugin") || word.eq_ignore_ascii_case("harness")
        })
    {
        words.pop();
    }
    let label = words
        .iter()
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect::<String>()
            })
        })
        .collect::<Vec<_>>()
        .join(" ");
    if label.is_empty() || label.chars().any(char::is_control) {
        EXTERNAL_HARNESS_FALLBACK_LABEL.to_owned()
    } else {
        label
    }
}

/// One renderer-neutral transcript or state effect of a canonical external event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalEffect {
    /// The user's own submitted input.
    User {
        /// Stable transcript item identity.
        id: String,
        /// Submitted text.
        text: String,
        /// Whether the input steers an in-flight native turn.
        steering: bool,
        /// Bcode agent announced to the native agent with this input, if any.
        agent: Option<String>,
    },
    /// The native agent summarized earlier turns; context the host added there is gone.
    ContextCompacted,
    /// Untrusted native assistant text rendered as markdown.
    Assistant {
        /// Stable transcript item identity (shared with the live stream when segmented).
        id: String,
        /// Complete segment text.
        text: String,
        /// Whether a live stream with the same identity must be terminalized.
        segmented: bool,
    },
    /// Native result text, shown only when it does not repeat the latest assistant text.
    Result {
        /// Stable transcript item identity.
        id: String,
        /// Result text.
        text: String,
    },
    /// Completed native reasoning.
    Reasoning {
        /// Synthetic turn identity.
        turn_id: String,
        /// Activity and part identity.
        activity_id: String,
        /// Readable text; empty for opaque reasoning.
        text: String,
        /// Whether the producer withheld readable reasoning.
        opaque: bool,
    },
    /// An observed native tool started or changed; never dispatched by Bcode.
    ToolStarted {
        /// Generation-scoped observed tool identity.
        tool_call_id: String,
        /// Native tool name.
        name: String,
        /// Bounded compact JSON input, for display only.
        input_json: Option<String>,
        /// Harness plugin that observed the tool.
        plugin_id: Option<String>,
    },
    /// Output reported for an observed native tool.
    ToolFinished {
        /// Generation-scoped observed tool identity.
        tool_call_id: String,
        /// Bounded untrusted output.
        output: String,
        /// Whether the native tool reported an error.
        is_error: bool,
        /// Whether the producer omitted part of the output.
        truncated: bool,
    },
    /// Display-only native usage.
    Usage(Box<SessionTokenUsage>),
    /// Compact system notice.
    Notice {
        /// Stable transcript item identity.
        id: String,
        /// Friendly literal text.
        text: String,
    },
    /// A native question was answered; its card shows the answer in place of the prompt to
    /// answer.
    QuestionAnswered {
        /// Transcript identity of the question card.
        id: String,
        /// The answer as sent.
        answer: String,
    },
    /// A native interaction now awaits a response.
    InteractionRequested(Box<ExternalInteractionView>),
    /// A native interaction was answered, or withdrawn by the native harness.
    InteractionResolved {
        /// Ownership generation of the interaction.
        generation: u64,
        /// Native request identity.
        request_id: String,
    },
}

/// The complete projection of one canonical external event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalProjection {
    /// Highest ownership generation carried by the event.
    pub generation: Option<u64>,
    /// Harness display label derived from the event's instance, when present.
    pub label: Option<String>,
    /// Native capabilities retained by a configuration record.
    pub capabilities: Option<HarnessCapabilities>,
    /// Ordered effects.
    pub effects: Vec<ExternalEffect>,
}

/// View state that informs, but never authorizes, a projection.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExternalProjectionContext<'a> {
    /// Last known harness display label.
    pub label: Option<&'a str>,
    /// Last retained native capabilities.
    pub capabilities: Option<&'a HarnessCapabilities>,
    /// Highest ownership generation already observed.
    pub generation: Option<u64>,
}

struct Builder<'a> {
    context: ExternalProjectionContext<'a>,
    sequence: u64,
    projection: ExternalProjection,
}

impl Builder<'_> {
    fn label(&self) -> &str {
        self.projection
            .label
            .as_deref()
            .or(self.context.label)
            .unwrap_or(EXTERNAL_HARNESS_FALLBACK_LABEL)
    }

    fn observe_instance(&mut self, instance: &HarnessInstance) {
        self.projection.label = Some(harness_display_label(&instance.plugin_id));
    }

    fn observe_generation(&mut self, generation: u64) {
        self.projection.generation = Some(
            self.projection
                .generation
                .map_or(generation, |current| current.max(generation)),
        );
    }

    fn current_generation(&self) -> Option<u64> {
        match (self.context.generation, self.projection.generation) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        }
    }

    fn notice(&mut self, text: String) {
        let index = self.projection.effects.len();
        self.projection.effects.push(ExternalEffect::Notice {
            id: format!("external-event:{}:{index}", self.sequence),
            text,
        });
    }

    fn submission(&mut self, submission: &bcode_harness_models::HarnessSubmission) {
        self.observe_generation(submission.generation);
        if submission.text.trim().is_empty() {
            return;
        }
        let (agent, text) = split_agent_block(&submission.text);
        self.projection.effects.push(ExternalEffect::User {
            id: format!(
                "external-submission:{}:{}",
                submission.generation, submission.submission_id
            ),
            text: text.to_owned(),
            steering: matches!(submission.placement, HarnessPlacement::Steer { .. }),
            agent: agent.map(ToOwned::to_owned),
        });
    }

    fn resolved(&mut self, generation: u64, request_id: &str) {
        self.projection
            .effects
            .push(ExternalEffect::InteractionResolved {
                generation,
                request_id: request_id.to_owned(),
            });
    }
}

/// Project one canonical external event into native-like effects.
///
/// # Errors
/// Rejects malformed reserved envelopes or invalid canonical observations without exposing raw
/// protocol content.
pub fn project_external_event(
    event: &SessionEvent,
    context: ExternalProjectionContext<'_>,
) -> Result<ExternalProjection, String> {
    let mut builder = Builder {
        context,
        sequence: event.sequence,
        projection: ExternalProjection::default(),
    };
    if let SessionEventKind::ExternalHarnessOperation { operation } = &event.kind {
        project_operation(&mut builder, operation)?;
        return Ok(builder.projection);
    }
    let Some(envelope) = ExternalBackendEnvelope::from_event(event)? else {
        return Ok(builder.projection);
    };
    builder.observe_generation(envelope.record.generation());
    match &envelope.record {
        ExternalBackendRecord::Binding { instance, binding } => {
            instance.validate().map_err(|error| error.to_string())?;
            binding.validate().map_err(|error| error.to_string())?;
            builder.observe_instance(instance);
            let text = format!("{} session connected", builder.label());
            builder.notice(text);
        }
        ExternalBackendRecord::Observation { observation } => {
            project_observation(&mut builder, observation, None)?;
        }
        ExternalBackendRecord::Submission { submission } => builder.submission(submission),
        ExternalBackendRecord::Receipt { .. } => {}
        ExternalBackendRecord::Response { response, .. } => {
            builder.resolved(response.generation, &response.request_id);
        }
    }
    Ok(builder.projection)
}

fn project_operation(
    builder: &mut Builder<'_>,
    operation: &ExternalHarnessOperation,
) -> Result<(), String> {
    operation.validate()?;
    builder.observe_generation(operation.owner.generation);
    match &operation.record {
        Record::Configured {
            instance,
            capabilities,
            ..
        } => {
            builder.observe_instance(instance);
            builder.projection.capabilities = Some((**capabilities).clone());
        }
        // Process lifecycle is plumbing: a released process resumes transparently when the
        // conversation is reopened, so neither event belongs in the transcript.
        Record::TransportReleased { .. } | Record::RecoveryPrepared { .. } => {}
        Record::DeliveryClaimed { request } => {
            builder.observe_instance(&request.instance);
            match &request.kind {
                ExternalHarnessRequestKind::Submit { submission, .. } => {
                    builder.submission(submission);
                }
                ExternalHarnessRequestKind::Respond { response, .. } => match &response.value {
                    bcode_harness_models::HarnessResponseValue::Answer { text, .. } => {
                        builder
                            .projection
                            .effects
                            .push(ExternalEffect::QuestionAnswered {
                                id: question_card_id(response.generation, &response.request_id),
                                answer: text.clone(),
                            });
                    }
                    // Answered by the conversation's agent policy rather than the user.
                    bcode_harness_models::HarnessResponseValue::Approval { approved }
                        if response
                            .response_id
                            .starts_with(AGENT_POLICY_RESPONSE_PREFIX) =>
                    {
                        builder.notice(
                            if *approved {
                                "Allowed by the agent's permission rules"
                            } else {
                                "Denied by the agent's permission rules"
                            }
                            .to_owned(),
                        );
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Record::DeliveryCompleted { request, response } => {
            builder.observe_instance(&request.instance);
            project_completion(builder, request, &response.kind);
        }
        Record::ObservationPage { instance, page, .. } => {
            builder.observe_instance(instance);
            for observation in &page.observations {
                project_observation(builder, observation, Some(instance))?;
            }
        }
    }
    Ok(())
}

fn project_completion(
    builder: &mut Builder<'_>,
    request: &ExternalHarnessRequest,
    response: &Response,
) {
    let label = builder.label().to_owned();
    match response {
        Response::Failure(failure) => {
            builder.notice(failure_text(&label, &request.kind, *failure));
            return;
        }
        Response::Prepared(prepared)
        | Response::Reconciled(HarnessReconciliation::KnownPrepared { prepared, .. }) => {
            builder.observe_generation(prepared.generation);
        }
        Response::Reconciled(HarnessReconciliation::Uncertain) => builder.notice(format!(
            "{label} could not confirm whether the last request arrived. It was not resent."
        )),
        Response::Reconciled(HarnessReconciliation::NotApplied) => builder.notice(format!(
            "{label} confirmed the last request was not applied. It was not resent."
        )),
        Response::Reconciled(HarnessReconciliation::Known { binding, .. }) => {
            builder.observe_generation(binding.generation);
        }
        Response::Opened(binding) => builder.observe_generation(binding.generation),
        Response::Selected { .. } | Response::Receipt(_) | Response::Acknowledged { .. } => {}
    }
    if let ExternalHarnessRequestKind::Respond { response, .. } = &request.kind {
        builder.resolved(response.generation, &response.request_id);
    }
}

fn failure_text(
    label: &str,
    request: &ExternalHarnessRequestKind,
    failure: HarnessFailure,
) -> String {
    let action = match request {
        ExternalHarnessRequestKind::Open(_) | ExternalHarnessRequestKind::OpenSelected(_) => {
            "start the session"
        }
        ExternalHarnessRequestKind::Submit { .. } => "deliver your message",
        ExternalHarnessRequestKind::Respond { .. } => "deliver your response",
        ExternalHarnessRequestKind::Interrupt(_) => "interrupt the current turn",
        ExternalHarnessRequestKind::Configure(_) => "apply the setting",
        ExternalHarnessRequestKind::Close(_) => "close the session",
        ExternalHarnessRequestKind::Reconcile(_) => "check the last request",
    };
    let reason = match failure {
        HarnessFailure::Unsupported => "this is not supported",
        HarnessFailure::InvalidRequest => "the request was rejected as invalid",
        HarnessFailure::Unavailable => "it is unavailable right now",
        HarnessFailure::StaleGeneration => "the session was restarted; refresh and try again",
        HarnessFailure::Conflict => "another request is already in progress",
        HarnessFailure::Authentication => "sign-in is required",
        HarnessFailure::Policy => "policy does not allow it",
        HarnessFailure::Uncertain => "the outcome is unknown, so nothing was resent",
    };
    format!("{label} could not {action}: {reason}.")
}

fn project_observation(
    builder: &mut Builder<'_>,
    observation: &HarnessObservation,
    instance: Option<&HarnessInstance>,
) -> Result<(), String> {
    observation.validate().map_err(|error| error.to_string())?;
    let generation = observation.generation;
    builder.observe_generation(generation);
    let observation_id = &observation.observation_id;
    if push_content_effect(&mut builder.projection.effects, observation, instance) {
        return Ok(());
    }
    let effect = match &observation.kind {
        HarnessObservationKind::Result { text } => {
            if text.trim().is_empty() {
                return Ok(());
            }
            ExternalEffect::Result {
                id: format!("external-observation:{generation}:{observation_id}"),
                text: text.clone(),
            }
        }
        HarnessObservationKind::Interaction { interaction } => {
            let text = interaction_notice(builder.label(), interaction);
            if interaction.kind == HarnessInteractionKind::Question {
                // One card per question, updated in place once it is answered.
                builder.projection.effects.push(ExternalEffect::Notice {
                    id: question_card_id(generation, &interaction.request_id),
                    text,
                });
            } else {
                builder.notice(text);
            }
            ExternalEffect::InteractionRequested(Box::new(pending_interaction(
                builder,
                interaction,
                instance,
            )?))
        }
        // A withdrawn prompt can no longer be answered; it ends without a notice, like a
        // native permission prompt cancelled with its turn.
        HarnessObservationKind::InteractionWithdrawn { request_id } => {
            ExternalEffect::InteractionResolved {
                generation,
                request_id: request_id.clone(),
            }
        }
        HarnessObservationKind::Diagnostic { code, message } => {
            let text = format!("{}: {message} ({code})", builder.label());
            builder.notice(text);
            return Ok(());
        }
        HarnessObservationKind::Subagent {
            native_agent_id,
            status,
        } => {
            let text = format!("{} subagent {native_agent_id}: {status}", builder.label());
            builder.notice(text);
            return Ok(());
        }
        HarnessObservationKind::Text { .. }
        | HarnessObservationKind::Reasoning { .. }
        | HarnessObservationKind::Tool { .. }
        | HarnessObservationKind::ToolResult { .. }
        | HarnessObservationKind::Usage { .. }
        | HarnessObservationKind::Execution { .. }
        | HarnessObservationKind::SubmissionAccepted { .. }
        | HarnessObservationKind::SubmissionConsumed { .. }
        | HarnessObservationKind::SubmissionResultObserved { .. }
        | HarnessObservationKind::BindingConfirmed { .. }
        | HarnessObservationKind::Status { .. }
        | HarnessObservationKind::Settlement { .. }
        | HarnessObservationKind::NativeTurnTerminal { .. }
        | HarnessObservationKind::Vendor { .. } => {
            if let HarnessObservationKind::Vendor { namespace, detail } = &observation.kind
                && namespace == bcode_harness_models::NATIVE_CONTEXT_NAMESPACE
                && detail.get("code").and_then(serde_json::Value::as_str) == Some("compacted")
            {
                builder.notice(format!(
                    "{} summarized earlier turns to free up context",
                    builder.label()
                ));
                builder
                    .projection
                    .effects
                    .push(ExternalEffect::ContextCompacted);
            }
            return Ok(());
        }
    };
    builder.projection.effects.push(effect);
    Ok(())
}

/// Project message, reasoning, tool and usage observations, which need no builder state.
///
/// Returns whether the observation was one of those kinds (even when it projects to nothing).
fn push_content_effect(
    effects: &mut Vec<ExternalEffect>,
    observation: &HarnessObservation,
    instance: Option<&HarnessInstance>,
) -> bool {
    let generation = observation.generation;
    let observation_id = &observation.observation_id;
    let effect = match &observation.kind {
        HarnessObservationKind::Text { text, segment_id } => {
            if text.trim().is_empty() {
                return true;
            }
            ExternalEffect::Assistant {
                id: segment_id.as_deref().map_or_else(
                    || format!("external-observation:{generation}:{observation_id}"),
                    |segment| {
                        bcode_harness_models::harness_stream_identity(generation, segment)
                            .assistant_item_id()
                    },
                ),
                text: text.clone(),
                segmented: segment_id.is_some(),
            }
        }
        HarnessObservationKind::Reasoning {
            segment_id,
            text,
            opaque,
        } => {
            let segment = segment_id.as_deref().unwrap_or(observation_id);
            let identity = bcode_harness_models::harness_stream_identity(generation, segment);
            ExternalEffect::Reasoning {
                activity_id: identity.reasoning_activity_id().to_owned(),
                turn_id: identity.turn_id,
                text: if *opaque { String::new() } else { text.clone() },
                opaque: *opaque,
            }
        }
        HarnessObservationKind::Tool {
            native_tool_id,
            name,
            input_json,
            ..
        } => ExternalEffect::ToolStarted {
            tool_call_id: external_tool_call_id(generation, native_tool_id),
            name: name.clone(),
            input_json: input_json.clone(),
            plugin_id: instance.map(|instance| instance.plugin_id.clone()),
        },
        HarnessObservationKind::ToolResult {
            native_tool_id,
            output,
            is_error,
            truncated,
        } => ExternalEffect::ToolFinished {
            tool_call_id: external_tool_call_id(generation, native_tool_id),
            output: output.clone(),
            is_error: *is_error,
            truncated: *truncated,
        },
        HarnessObservationKind::Usage { .. } => {
            let Some(usage) = bcode_session_models::native_harness_usage(observation) else {
                return true;
            };
            ExternalEffect::Usage(Box::new(usage))
        }
        _ => return false,
    };
    effects.push(effect);
    true
}

/// Generation-scoped identity of an observed native tool; never a Bcode tool invocation id.
#[must_use]
pub fn external_tool_call_id(generation: u64, native_tool_id: &str) -> String {
    format!("external:{generation}:{native_tool_id}")
}

fn excerpt(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut chars = line.trim().chars();
    let mut excerpt = chars.by_ref().take(NOTICE_PROMPT_CHARS).collect::<String>();
    if chars.next().is_some() {
        excerpt.push('…');
    }
    excerpt
}

/// Opening of the block announcing the conversation's Bcode agent to a native agent at the
/// start of a message: `<bcode-agent name="NAME">` … `</bcode-agent>`, then the message.
pub const AGENT_BLOCK_OPEN: &str = "<bcode-agent name=\"";
/// Closing of the agent block.
pub const AGENT_BLOCK_CLOSE: &str = "</bcode-agent>";

/// A message announcing `agent` and its `instructions` before `text`.
#[must_use]
pub fn with_agent_block(agent: &str, instructions: Option<&str>, text: &str) -> String {
    let name: String = agent
        .chars()
        .filter(|character| *character != '"')
        .collect();
    let mut block = format!(
        "{AGENT_BLOCK_OPEN}{name}\">\nThe Bcode agent for this conversation is now {name}. \
These instructions replace any earlier Bcode agent instructions."
    );
    if let Some(instructions) = instructions.filter(|value| !value.trim().is_empty()) {
        block.push_str("\n\n");
        block.push_str(instructions.trim());
    }
    format!("{block}\n{AGENT_BLOCK_CLOSE}\n\n{text}")
}

/// The announced agent name and the message itself, for display.
#[must_use]
pub fn split_agent_block(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix(AGENT_BLOCK_OPEN) else {
        return (None, text);
    };
    let Some((name, _)) = rest.split_once("\">") else {
        return (None, text);
    };
    let Some((_, message)) = rest.split_once(AGENT_BLOCK_CLOSE) else {
        return (None, text);
    };
    (Some(name), message.trim_start_matches('\n'))
}

/// Response identities of native approvals the conversation's agent policy answered.
pub const AGENT_POLICY_RESPONSE_PREFIX: &str = "agent-policy-";

/// Transcript identity of a native question's card.
fn question_card_id(generation: u64, request_id: &str) -> String {
    format!("external-question:{generation}:{request_id}")
}

/// The answered form of a question card: the question and its options, then the answer in
/// place of the prompt to answer.
#[must_use]
pub fn answered_question_text(card: &str, answer: &str) -> String {
    let question = card
        .rsplit_once('\n')
        .filter(|(_, last)| last.contains("answer"))
        .map_or(card, |(rest, _)| rest);
    format!("{question}\nYou answered: {}", excerpt(answer))
}

fn interaction_notice(label: &str, interaction: &HarnessInteraction) -> String {
    let prompt = excerpt(&interaction.prompt);
    match interaction.kind {
        HarnessInteractionKind::Approval => interaction.tool_name.as_deref().map_or_else(
            || {
                if prompt.is_empty() {
                    format!("{label} is asking for approval")
                } else {
                    format!("{label} is asking for approval: {prompt}")
                }
            },
            |tool| format!("{label} is asking for approval to run {tool}"),
        ),
        HarnessInteractionKind::Question => {
            let mut notice = format!("{label} asks: {prompt}");
            for (index, choice) in interaction.choices.iter().take(9).enumerate() {
                let _ = write!(notice, "\n  {}. {}", index + 1, excerpt(&choice.label));
            }
            notice.push_str(if interaction.choices.is_empty() {
                "\nType your answer and send it."
            } else {
                "\nPick an option, or type your own answer and send it."
            });
            notice
        }
        HarnessInteractionKind::Assistance => format!("{label} needs assistance: {prompt}"),
    }
}

fn unknown_capabilities() -> HarnessCapabilities {
    let capability = HarnessCapability {
        support: HarnessSupportLevel::Constrained,
        constraints: vec![
            "native capability not retained in this view; the daemon rechecks every response"
                .to_owned(),
        ],
    };
    HarnessCapabilities {
        resume: capability.clone(),
        steering: capability.clone(),
        approval: capability.clone(),
        questions: capability.clone(),
        assistance: capability.clone(),
        interrupt: capability.clone(),
        authoritative_settlement: capability.clone(),
        background_continuation: capability.clone(),
        authentication: capability,
        model_switch: HarnessCapability::unsupported(),
        permission_mode: HarnessCapability::unsupported(),
    }
}

fn pending_interaction(
    builder: &Builder<'_>,
    interaction: &HarnessInteraction,
    instance: Option<&HarnessInstance>,
) -> Result<ExternalInteractionView, String> {
    let provenance = ExternalProvenanceView {
        session_id: interaction.session_id.clone(),
        instance_id: instance.map_or_else(
            || "unreported".to_owned(),
            |value| value.instance_id.clone(),
        ),
        plugin_id: instance
            .map_or_else(|| "unreported".to_owned(), |value| value.plugin_id.clone()),
        native_session_id: "unreported".to_owned(),
        generation: interaction.generation,
    };
    let fallback = unknown_capabilities();
    let capabilities = builder
        .projection
        .capabilities
        .as_ref()
        .or(builder.context.capabilities)
        .unwrap_or(&fallback);
    let availability = if builder
        .current_generation()
        .is_some_and(|current| interaction.generation < current)
    {
        ExternalInteractionAvailability::Stale
    } else {
        ExternalInteractionAvailability::Admitted
    };
    external_interaction(interaction, &provenance, capabilities, availability)
        .map_err(|error| error.to_string())
}
