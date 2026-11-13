//! Native protocol normalization.
//!
//! Raw native frames never leave this module. Assistant text, reasoning, tool inputs and
//! tool outputs leave only through the bounded projections in `normalize_display.rs`, as
//! untrusted display content that never dispatches or authorizes a Bcode tool.
#[path = "normalize_display.rs"]
pub mod display;
#[path = "normalizer_retention.rs"]
mod retention;

use bcode_claude_code::{CallbackRequest, CallbackResponse, Event, NativeMessage, Settlement};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessActivity, HarnessAssistanceGrant, HarnessAttention,
    HarnessChoice, HarnessCompleteness, HarnessConnectivity, HarnessContinuation,
    HarnessContractError, HarnessExecutionStatus, HarnessInteraction, HarnessInteractionKind,
    HarnessObservation, HarnessObservationKind, HarnessResponse, HarnessResponseValue,
    HarnessSessionBinding, HarnessSessionTarget, HarnessSubmission, MAX_HARNESS_ID_BYTES,
    MAX_HARNESS_ITEMS, MAX_HARNESS_TEXT_BYTES,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Per-process transient state; never a recovery journal or execution authority.
pub struct Normalizer {
    pub target: HarnessSessionTarget,
    pub open_id: String,
    sequence: u64,
    pub pending: BTreeMap<String, (HarnessInteraction, CallbackRequest)>,
    /// Reserved for verified submission acceptance; raw message IDs cannot set it.
    pub turn_id: Option<String>,
    pub submitted: bool,
    /// Populated exclusively from a host-admitted open scope once its shared contract is wired.
    /// Native protocol input never writes this field.
    pub assistance_grant: Option<HarnessAssistanceGrant>,
    pending_grants: BTreeMap<String, HarnessAssistanceGrant>,
    attribution: crate::attribution::Producer,
    staging: retention::Staging,
    execution: crate::execution::Tracker,
    /// Transient partial-message text; never retained or digested.
    live: crate::live::LiveBuffer,
    blocks: display::BlockNumbering,
    /// Latest native model identity (system init or assistant message) for usage.
    model: Option<String>,
    /// Last cumulative `total_cost_usd` (in micros) this process reported. Claude reports a
    /// running process total on every result; usage observations carry only the increment.
    reported_cost_micros: Option<u64>,
}
impl Normalizer {
    #[cfg(test)]
    pub fn new(binding: HarnessSessionBinding) -> Self {
        Self::with_target(
            HarnessSessionTarget::Confirmed(binding),
            "confirmed-open".into(),
        )
    }
    pub const fn with_target(target: HarnessSessionTarget, open_id: String) -> Self {
        Self {
            target,
            open_id,
            sequence: 0,
            pending: BTreeMap::new(),
            turn_id: None,
            submitted: false,
            assistance_grant: None,
            pending_grants: BTreeMap::new(),
            attribution: crate::attribution::Producer::new(),
            staging: retention::Staging::new(),
            execution: crate::execution::Tracker::new(),
            live: crate::live::LiveBuffer::new(),
            blocks: display::BlockNumbering::new(),
            model: None,
            reported_cost_micros: None,
        }
    }
    /// Whether transient live updates wait for the next observe page.
    pub fn live_pending(&self) -> bool {
        self.live.is_pending()
    }
    /// Whether pending live text is close enough to its bounds that coalescing longer would
    /// risk dropping deltas.
    pub fn live_near_capacity(&self) -> bool {
        self.live.near_capacity()
    }
    /// Take pending live updates for one page; they are never offered twice.
    pub fn take_live(&mut self) -> Vec<bcode_harness_models::HarnessLiveUpdate> {
        self.live.take()
    }
    /// Record transient live text from a partial-message frame before staging.
    fn observe_live(&mut self, event: &Event) {
        if let Event::Message(NativeMessage::Stream(value)) = event {
            self.live.stream_event(value);
        }
    }
    pub fn confirmation(
        &mut self,
        native_id: &str,
    ) -> Result<Vec<HarnessObservation>, HarnessContractError> {
        let HarnessSessionTarget::Prepared(prepared) = &self.target else {
            if let HarnessSessionTarget::Confirmed(binding) = &self.target
                && binding.native_session_id != native_id
            {
                return Err(HarnessContractError::WrongOwner);
            }
            return Ok(Vec::new());
        };
        let binding = HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: prepared.session_id.clone(),
            instance_id: prepared.instance_id.clone(),
            native_session_id: native_id.into(),
            native_version: prepared.native_version.clone(),
            generation: prepared.generation,
        };
        self.target.validate_confirmation(&self.open_id, &binding)?;
        self.target = HarnessSessionTarget::Confirmed(binding.clone());
        Ok(self
            .observation(HarnessObservationKind::BindingConfirmed {
                open_id: self.open_id.clone(),
                binding,
            })
            .into_iter()
            .collect())
    }
    pub(super) fn observation(
        &mut self,
        kind: HarnessObservationKind,
    ) -> Option<HarnessObservation> {
        self.sequence = self.sequence.checked_add(1)?;
        let native_correlations = match &kind {
            HarnessObservationKind::Execution { event } => {
                self.execution.native_correlations(event)
            }
            _ => Vec::new(),
        };
        let observation = HarnessObservation {
            observation_id: format!("claude:{}:{}", self.target.generation(), self.sequence),
            session_id: self.target.session_id().to_owned(),
            generation: self.target.generation(),
            native_correlations,
            completeness: HarnessCompleteness::Complete,
            kind,
        };
        observation.validate().ok()?;
        if let HarnessObservationKind::Execution { event } = &observation.kind {
            self.execution.published(event, &observation.observation_id);
        }
        Some(observation)
    }
    pub fn acknowledge_execution(&mut self, observation: &str) {
        self.execution.acknowledge(observation);
    }
    pub fn retire_acknowledged(&mut self) -> Vec<bcode_harness_models::HarnessAttemptIdentity> {
        let mut retired = self.execution.retire_canonical_attempts();
        retired.extend(self.execution.retire_completed());
        self.attribution.retire(&retired);
        retired
    }
    #[must_use]
    pub fn retained_attribution(&self) -> &[crate::RetainedAttribution] {
        self.attribution.evidence()
    }
    pub const fn configure_execution(&mut self, supported: bool, fresh: bool) {
        self.execution.configure(supported, fresh);
    }
    pub fn retain_submission(
        &mut self,
        submission: &HarnessSubmission,
    ) -> Result<bcode_claude_code::SubmissionUuid, HarnessContractError> {
        if submission.session_id != self.target.session_id()
            || submission.generation != self.target.generation()
        {
            return Err(HarnessContractError::WrongOwner);
        }
        self.execution.check_retain()?;
        self.attribution.retain(submission).and_then(|uuid| {
            self.execution
                .retain(bcode_harness_models::HarnessAttemptIdentity {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                })?;
            Ok(uuid)
        })
    }
    fn event_ready(&mut self, event: Event) -> Vec<HarnessObservation> {
        let mut kinds = match self.execution.event(&event) {
            Ok(events) => events
                .into_iter()
                .map(|event| HarnessObservationKind::Execution { event })
                .collect::<Vec<_>>(),
            Err(_) => return self.gap(),
        };
        match event {
            Event::Attribution(evidence) => {
                // Only typed native correlation plus an exact retained pair can
                // publish these events. Neither establishes a native turn identity.
                match self.attribution.record(&evidence) {
                    Ok(observations) => {
                        if self
                            .execution
                            .attribute(&evidence, self.attribution.evidence())
                            .is_err()
                        {
                            return self.gap();
                        }
                        kinds.extend(observations);
                    }
                    Err(_) => return self.gap(),
                }
            }
            Event::Message(NativeMessage::Assistant(value)) => {
                let Some(normalized) = self.assistant(&value) else {
                    return self.gap();
                };
                kinds.extend(normalized);
            }
            Event::Message(NativeMessage::User(value)) => {
                kinds.extend(display::tool_results(&value));
            }
            Event::Message(NativeMessage::Result(value)) => {
                let Some(normalized) = self.result(&value) else {
                    return self.gap();
                };
                kinds.extend(normalized);
            }
            Event::Message(NativeMessage::System(value)) => {
                if value.get("subtype").and_then(Value::as_str) == Some("init")
                    && let Some(model) = display::model_identity(value.get("model"))
                {
                    self.model = Some(model);
                }
                // Earlier turns were summarized: context Bcode added in them is gone.
                if value.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                    kinds.push(HarnessObservationKind::Vendor {
                        namespace: bcode_harness_models::NATIVE_CONTEXT_NAMESPACE.into(),
                        detail: serde_json::json!({"code": "compacted"}),
                    });
                }
                if let Some("task_started" | "task_notification") =
                    value.get("subtype").and_then(Value::as_str)
                    && let Some(id) = bounded_id(value.get("task_id"))
                {
                    kinds.push(HarnessObservationKind::Subagent {
                        native_agent_id: id,
                        status: "native activity observed".into(),
                    });
                }
            }
            Event::Callback {
                request_id,
                request,
            } => {
                let Some(normalized) = self.callback(request_id, request) else {
                    return self.gap();
                };
                kinds.extend(normalized);
            }
            Event::CallbackCancelled { request_id } => {
                self.pending_grants.remove(&request_id);
                // The host already showed this prompt; tell it the prompt is gone so it is
                // never left waiting for an answer the native harness will refuse.
                if self.pending.remove(&request_id).is_some() {
                    kinds.push(HarnessObservationKind::InteractionWithdrawn { request_id });
                }
            }
            Event::Settlement(Settlement::Settled) => {
                kinds.extend(self.settled_kinds());
            }
            Event::Settlement(state) => {
                kinds.push(HarnessObservationKind::Status {
                    status: status(&state),
                });
            }
            Event::Message(NativeMessage::Stream(_) | NativeMessage::Notice(_)) => {}
        }
        self.observations_from_kinds(kinds)
    }
    /// Record one native callback as a pending interaction, or report it as unsupported;
    /// `None` when the callback exceeds bounds or lacks host-admitted scope.
    fn callback(
        &mut self,
        request_id: String,
        request: CallbackRequest,
    ) -> Option<Vec<HarnessObservationKind>> {
        if self.pending.len() >= MAX_HARNESS_ITEMS || self.pending.contains_key(&request_id) {
            return None;
        }
        let Some(interaction) = self.interaction(&request_id, &request) else {
            self.execution.lose_coverage();
            let mut kinds: Vec<_> = self
                .execution
                .coverage()
                .into_iter()
                .map(|event| HarnessObservationKind::Execution { event })
                .collect();
            kinds.push(HarnessObservationKind::Diagnostic {
                code: "unsupported_callback".into(),
                message: "Native callback requires unsupported or unscoped behavior; no action authorized".into(),
            });
            return Some(kinds);
        };
        if interaction.kind == HarnessInteractionKind::Assistance {
            let grant = self.assistance_grant.clone()?;
            self.pending_grants.insert(request_id.clone(), grant);
        }
        self.pending
            .insert(request_id, (interaction.clone(), request));
        Some(vec![HarnessObservationKind::Interaction { interaction }])
    }
    /// Result text and usage of one native result frame; `None` when the result is malformed.
    /// A result can precede background continuation, so it never synthesizes settlement.
    fn result(&mut self, value: &Value) -> Option<Vec<HarnessObservationKind>> {
        let mut kinds = Vec::new();
        if let Some(text) = bounded_text(value.get("result")) {
            kinds.push(HarnessObservationKind::Result { text });
        } else if value.get("result").is_some() {
            return None;
        }
        kinds.extend(
            display::usage(value, self.model.clone()).map(|kind| self.cost_increment(kind)),
        );
        Some(kinds)
    }
    /// Replace a result's cumulative native cost with the increment since the previous
    /// result of this process. A total below the previous one means the native total was
    /// reset, so the whole new total is the increment.
    fn cost_increment(&mut self, mut kind: HarnessObservationKind) -> HarnessObservationKind {
        if let HarnessObservationKind::Usage {
            total_cost_micros: Some(cost),
            ..
        } = &mut kind
        {
            let total = *cost;
            let previous = self.reported_cost_micros.replace(total).unwrap_or(0);
            *cost = total.checked_sub(previous).unwrap_or(total);
        }
        kind
    }
    fn observations_from_kinds(
        &mut self,
        kinds: Vec<HarnessObservationKind>,
    ) -> Vec<HarnessObservation> {
        let expected = kinds.len();
        let observations: Vec<_> = kinds
            .into_iter()
            .filter_map(|kind| self.observation(kind))
            .collect();
        if observations.len() != expected {
            return self.gap();
        }
        observations
    }
    fn settled_kinds(&self) -> Vec<HarnessObservationKind> {
        self.turn_id.as_ref().map_or_else(
            || vec![
                HarnessObservationKind::Status { status: status(&Settlement::Settled) },
                // Retained evidence for workflow settlement, not a user-facing notice: an
                // ordinary turn always settles this way until turn correlation is verified.
                HarnessObservationKind::Vendor {
                    namespace: "claude_code.settlement".into(),
                    detail: serde_json::json!({
                        "code": "settlement_turn_unavailable",
                        "message": "Native settled state observed without a native turn correlation; no turn settlement synthesized",
                    }),
                },
            ],
            |native_turn_id| vec![HarnessObservationKind::Settlement {
                native_turn_id: native_turn_id.clone(),
            }],
        )
    }
    fn assistant(&mut self, value: &Value) -> Option<Vec<HarnessObservationKind>> {
        let mut kinds = Vec::new();
        // Raw assistant IDs (including an unverified turn_id field) do not prove
        // acceptance of the exact host submission/attempt. Only a verified SDK
        // acceptance contract may populate turn correlation in the future. The native
        // message ID is used solely as a display segment identity.
        if let Some(model) = display::model_identity(value.pointer("/message/model")) {
            self.model = Some(model);
        }
        let message_id = value
            .pointer("/message/id")
            .and_then(Value::as_str)
            .filter(|id| crate::live::valid_message_id(id));
        if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
            if content.len() > MAX_HARNESS_ITEMS {
                return None;
            }
            for block in content {
                let mut segment = |kind| message_id.map(|id| self.blocks.next(id, kind));
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        // Oversized text is truncated rather than dropped, so the segment is
                        // still finalized and supersedes its live stream.
                        let text = block.get("text").and_then(Value::as_str)?;
                        kinds.push(HarnessObservationKind::Text {
                            segment_id: segment(display::SegmentKind::Text),
                            text: display::truncate_display(text, MAX_HARNESS_TEXT_BYTES).0,
                        });
                    }
                    Some("thinking") => {
                        // The native signature is opaque verification material, never shown.
                        let thinking = block.get("thinking").and_then(Value::as_str)?;
                        let segment_id = segment(display::SegmentKind::Reasoning);
                        // Summaries may be omitted natively; an empty block has nothing to show.
                        if !thinking.is_empty() {
                            kinds.push(HarnessObservationKind::Reasoning {
                                segment_id,
                                text: display::truncate_display(thinking, MAX_HARNESS_TEXT_BYTES).0,
                                opaque: false,
                            });
                        }
                    }
                    Some("redacted_thinking") => {
                        kinds.push(HarnessObservationKind::Reasoning {
                            segment_id: segment(display::SegmentKind::Reasoning),
                            text: String::new(),
                            opaque: true,
                        });
                    }
                    Some("tool_use") => {
                        if let (Some(id), Some(name)) =
                            (bounded_id(block.get("id")), bounded_text(block.get("name")))
                        {
                            kinds.push(HarnessObservationKind::Tool {
                                input_json: block
                                    .get("input")
                                    .and_then(display::bounded_input_json),
                                native_tool_id: id,
                                name,
                                status: "observed".into(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        Some(kinds)
    }
    fn interaction(
        &self,
        request_id: &str,
        request: &CallbackRequest,
    ) -> Option<HarnessInteraction> {
        let (kind, prompt, scope, choices) = match request {
            CallbackRequest::CanUseTool {
                tool_name, input, ..
            } if tool_name == "AskUserQuestion" => {
                let (prompt, choices) = question_form(input)?;
                (
                    HarnessInteractionKind::Question,
                    prompt,
                    "Answer one native Claude clarification".into(),
                    choices,
                )
            }
            CallbackRequest::CanUseTool {
                tool_name, input, ..
            } => {
                if tool_name.len() > MAX_HARNESS_ID_BYTES {
                    return None;
                }
                let interaction = HarnessInteraction {
                    tool_name: (!tool_name.is_empty()
                        && tool_name.trim() == tool_name
                        && !tool_name.chars().any(char::is_control))
                    .then(|| tool_name.clone()),
                    input_preview: display::input_preview(input),
                    request_id: request_id.into(),
                    session_id: self.target.session_id().to_owned(),
                    generation: self.target.generation(),
                    kind: HarnessInteractionKind::Approval,
                    prompt: format!("Allow Claude Code to use {tool_name}?"),
                    // The preview is untrusted display content; Claude Code runs the tool itself.
                    scope: format!("Claude Code runs {tool_name} itself once you approve"),
                    choices: Vec::new(),
                };
                return interaction.validate().ok().map(|()| interaction);
            }
            CallbackRequest::McpMessage { .. } => {
                let grant = self.assistance_grant.as_ref().filter(|grant| {
                    grant.validate().is_ok()
                        && grant.child_session_id == self.target.session_id()
                        && grant.child_generation == self.target.generation()
                });
                match crate::parent_bridge::handle(
                    request_id,
                    request,
                    &self.target,
                    grant.map(|grant| grant.scope.as_str()),
                ) {
                    Some(crate::parent_bridge::BridgeCall::Pending(interaction)) => {
                        return Some(interaction);
                    }
                    _ => return None,
                }
            }
            CallbackRequest::HookCallback { .. } => return None,
        };
        let interaction = HarnessInteraction {
            input_preview: None,
            tool_name: None,
            request_id: request_id.into(),
            session_id: self.target.session_id().to_owned(),
            generation: self.target.generation(),
            kind,
            prompt,
            scope,
            choices,
        };
        interaction.validate().ok().map(|()| interaction)
    }
    pub fn response(
        &self,
        response: &HarnessResponse,
    ) -> Result<CallbackResponse, HarnessContractError> {
        let (interaction, request) = self
            .pending
            .get(&response.request_id)
            .ok_or(HarnessContractError::InvalidIdentity)?;
        response.validate_for(interaction)?;
        if interaction.kind == HarnessInteractionKind::Assistance {
            let retained = self
                .pending_grants
                .get(&response.request_id)
                .ok_or(HarnessContractError::WrongOwner)?;
            if self.assistance_grant.as_ref() != Some(retained)
                || retained.validate().is_err()
                || retained.child_session_id != response.session_id
                || retained.child_generation != response.generation
                || retained.scope != interaction.scope
            {
                return Err(HarnessContractError::WrongOwner);
            }
        }
        match (&response.value, request) {
            (
                HarnessResponseValue::Approval { approved: true },
                CallbackRequest::CanUseTool { input, .. },
            ) => Ok(CallbackResponse::Allow {
                updated_input: input.clone(),
            }),
            (
                HarnessResponseValue::Approval { approved: false } | HarnessResponseValue::Cancel,
                CallbackRequest::CanUseTool { .. },
            ) => Ok(CallbackResponse::Deny {
                message: "Request denied".into(),
                interrupt: false,
            }),
            (
                HarnessResponseValue::Answer { choice_id, text },
                CallbackRequest::CanUseTool { input, .. },
            ) => {
                let mut updated_input = input.clone();
                let questions = input
                    .get("questions")
                    .and_then(Value::as_array)
                    .ok_or(HarnessContractError::InvalidShape)?;
                let answer = choice_id
                    .as_ref()
                    .and_then(|id| {
                        interaction
                            .choices
                            .iter()
                            .find(|choice| &choice.choice_id == id)
                    })
                    .map_or(text.as_str(), |choice| choice.label.as_str());
                // One reply answers every question asked; with several, it covers them all.
                let answers = questions
                    .iter()
                    .filter_map(|question| question.get("question")?.as_str())
                    .map(|question| (question.to_owned(), Value::from(answer)))
                    .collect::<serde_json::Map<_, _>>();
                if answers.is_empty() {
                    return Err(HarnessContractError::InvalidShape);
                }
                updated_input["answers"] = Value::Object(answers);
                Ok(CallbackResponse::Allow { updated_input })
            }
            (HarnessResponseValue::Cancel, CallbackRequest::McpMessage { message, .. }) => {
                Ok(CallbackResponse::Mcp {
                    response: json!({"jsonrpc":"2.0", "id":message.get("id"), "error":{"code":-32000,"message":"Parent assistance cancelled"}}),
                })
            }
            (
                HarnessResponseValue::Assistance { text },
                CallbackRequest::McpMessage { message, .. },
            ) => Ok(CallbackResponse::Mcp {
                response: json!({"jsonrpc":"2.0", "id":message.get("id"), "result":{"content":[{"type":"text", "text":text}]}}),
            }),
            _ => Err(HarnessContractError::InvalidShape),
        }
    }
    pub fn complete_callback(&mut self, request_id: &str) {
        self.pending.remove(request_id);
        self.pending_grants.remove(request_id);
    }
    pub fn connection_lost(&mut self) -> Vec<HarnessObservation> {
        let mut observations = self.invalidate_callbacks();
        self.live.clear();
        let events = self.execution.disconnected();
        observations.extend(
            events
                .into_iter()
                .filter_map(|event| self.observation(HarnessObservationKind::Execution { event })),
        );
        observations
    }
    /// Drop every outstanding native callback and return one withdrawal observation for each
    /// prompt the host was shown, so no dialog waits on a callback that can never be answered.
    pub fn invalidate_callbacks(&mut self) -> Vec<HarnessObservation> {
        self.execution.lose_coverage();
        self.pending_grants.clear();
        self.assistance_grant = None;
        std::mem::take(&mut self.pending)
            .into_keys()
            .filter_map(|request_id| {
                self.observation(HarnessObservationKind::InteractionWithdrawn { request_id })
            })
            .collect()
    }
    pub fn gap(&mut self) -> Vec<HarnessObservation> {
        self.execution.lose_coverage();
        let events = self.execution.coverage();
        let mut observations: Vec<_> = events
            .into_iter()
            .filter_map(|event| self.observation(HarnessObservationKind::Execution { event }))
            .collect();
        observations.extend(
            self.observation(HarnessObservationKind::Diagnostic {
                code: "native_observation_gap".into(),
                message: "Native observations exceeded a bound; reconcile before continuing".into(),
            })
            .map(|mut observation| {
                observation.completeness = HarnessCompleteness::Gap;
                vec![observation]
            })
            .unwrap_or_default(),
        );
        observations
    }
}
/// The prompt and choices of a native `AskUserQuestion` form. One question offers its options
/// (with what each means); several questions, or several selections, are answered in one reply,
/// so every form the native agent can ask is answerable.
fn question_form(input: &Value) -> Option<(String, Vec<HarnessChoice>)> {
    use std::fmt::Write as _;
    let questions = input.get("questions")?.as_array()?;
    if questions.is_empty() || questions.len() > MAX_HARNESS_ITEMS {
        return None;
    }
    let options = |question: &Value| -> Option<Vec<Value>> {
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        (options.len() <= MAX_HARNESS_ITEMS).then_some(options)
    };
    let several =
        |question: &Value| question.get("multiSelect").and_then(Value::as_bool) == Some(true);
    if let [question] = questions.as_slice() {
        let mut prompt = bounded_text(question.get("question"))?;
        if several(question) {
            prompt.push_str("\n(Choose one, or type several separated by commas.)");
        }
        let choices = options(question)?
            .iter()
            .enumerate()
            .map(|(index, value)| {
                Some(HarnessChoice {
                    choice_id: index.to_string(),
                    label: bounded_text(value.get("label"))?,
                    description: bounded_text(value.get("description"))
                        .filter(|text| !text.trim().is_empty()),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        return (prompt.len() <= MAX_HARNESS_TEXT_BYTES).then_some((prompt, choices));
    }
    let mut prompt = String::new();
    for (index, question) in questions.iter().enumerate() {
        let text = bounded_text(question.get("question"))?;
        if index > 0 {
            prompt.push('\n');
        }
        let _ = write!(prompt, "{}. {text}", index + 1);
        let labels = options(question)?
            .iter()
            .filter_map(|option| option.get("label")?.as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        if !labels.is_empty() {
            let _ = write!(
                prompt,
                " ({}{})",
                if several(question) { "any of: " } else { "" },
                labels.join(" / ")
            );
        }
    }
    prompt.push_str("\nAnswer each question in one reply.");
    (prompt.len() <= MAX_HARNESS_TEXT_BYTES).then_some((prompt, Vec::new()))
}
fn bounded_text(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?;
    (value.len() <= MAX_HARNESS_TEXT_BYTES).then(|| value.to_owned())
}
fn bounded_id(value: Option<&Value>) -> Option<String> {
    let value = bounded_text(value)?;
    (!value.is_empty()
        && value.len() <= MAX_HARNESS_ID_BYTES
        && !value.chars().any(char::is_control))
    .then_some(value)
}
pub fn status(state: &Settlement) -> HarnessExecutionStatus {
    HarnessExecutionStatus {
        connectivity: if *state == Settlement::Uncertain {
            HarnessConnectivity::Disconnected
        } else {
            HarnessConnectivity::Connected
        },
        activity: match state {
            Settlement::Running => HarnessActivity::Running,
            Settlement::Settled => HarnessActivity::Settled,
            _ => HarnessActivity::Unknown,
        },
        attention: if *state == Settlement::RequiresAction {
            HarnessAttention::Unknown
        } else {
            HarnessAttention::None
        },
        continuation: HarnessContinuation::Unknown,
        completeness: if *state == Settlement::Uncertain {
            HarnessCompleteness::Gap
        } else {
            HarnessCompleteness::Complete
        },
    }
}
