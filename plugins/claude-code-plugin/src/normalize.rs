//! Native protocol normalization. Raw messages and tool inputs never leave this module.
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
    fn observation(&mut self, kind: HarnessObservationKind) -> Option<HarnessObservation> {
        self.sequence = self.sequence.checked_add(1)?;
        let observation = HarnessObservation {
            observation_id: format!("claude:{}:{}", self.target.generation(), self.sequence),
            session_id: self.target.session_id().to_owned(),
            generation: self.target.generation(),
            native_correlations: Vec::new(),
            completeness: HarnessCompleteness::Complete,
            kind,
        };
        observation.validate().ok().map(|()| observation)
    }
    #[must_use]
    pub fn retained_attribution(&self) -> &[crate::RetainedAttribution] {
        self.attribution.evidence()
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
        self.attribution.retain(submission)
    }
    pub fn event(&mut self, event: Event) -> Vec<HarnessObservation> {
        let mut kinds = Vec::new();
        match event {
            Event::Attribution(evidence) => {
                // Only typed native correlation plus an exact retained pair can
                // publish these events. Neither establishes a native turn identity.
                match self.attribution.record(&evidence) {
                    Ok(observations) => kinds.extend(observations),
                    Err(_) => return self.gap(),
                }
            }
            Event::Message(NativeMessage::Assistant(value)) => {
                let Some(normalized) = Self::assistant(&value) else {
                    return self.gap();
                };
                kinds.extend(normalized);
            }
            Event::Message(NativeMessage::Result(value)) => {
                if let Some(text) = bounded_text(value.get("result")) {
                    kinds.push(HarnessObservationKind::Result { text });
                } else if value.get("result").is_some() {
                    return self.gap();
                }
                // A result can precede background continuation; never synthesize settlement.
            }
            Event::Message(NativeMessage::System(value)) => {
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
                if self.pending.len() >= MAX_HARNESS_ITEMS || self.pending.contains_key(&request_id)
                {
                    return self.gap();
                }
                if let Some(interaction) = self.interaction(&request_id, &request) {
                    if interaction.kind == HarnessInteractionKind::Assistance {
                        let Some(grant) = self.assistance_grant.clone() else {
                            return self.gap();
                        };
                        self.pending_grants.insert(request_id.clone(), grant);
                    }
                    self.pending
                        .insert(request_id, (interaction.clone(), request));
                    kinds.push(HarnessObservationKind::Interaction { interaction });
                } else {
                    kinds.push(HarnessObservationKind::Diagnostic { code: "unsupported_callback".into(), message: "Native callback requires unsupported or unscoped behavior; no action authorized".into() });
                }
            }
            Event::CallbackCancelled { request_id } => {
                self.pending.remove(&request_id);
                self.pending_grants.remove(&request_id);
            }
            Event::Settlement(Settlement::Settled) => {
                if let Some(native_turn_id) = &self.turn_id {
                    kinds.push(HarnessObservationKind::Settlement {
                        native_turn_id: native_turn_id.clone(),
                    });
                } else {
                    kinds.push(HarnessObservationKind::Status {
                        status: status(&Settlement::Settled),
                    });
                    kinds.push(HarnessObservationKind::Diagnostic { code: "settlement_turn_unavailable".into(), message: "Native settled state observed without a native turn correlation; no turn settlement synthesized".into() });
                }
            }
            Event::Settlement(state) => {
                kinds.push(HarnessObservationKind::Status {
                    status: status(&state),
                });
            }
            Event::Message(
                NativeMessage::User(_) | NativeMessage::Stream(_) | NativeMessage::Notice(_),
            ) => {}
        }
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
    fn assistant(value: &Value) -> Option<Vec<HarnessObservationKind>> {
        let mut kinds = Vec::new();
        // Raw assistant IDs (including an unverified turn_id field) do not prove
        // acceptance of the exact host submission/attempt. Only a verified SDK
        // acceptance contract may populate turn correlation in the future.
        if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
            if content.len() > MAX_HARNESS_ITEMS {
                return None;
            }
            for block in content.iter().take(MAX_HARNESS_ITEMS) {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = bounded_text(block.get("text"))?;
                        kinds.push(HarnessObservationKind::Text { text });
                    }
                    Some("tool_use") => {
                        if let (Some(id), Some(name)) =
                            (bounded_id(block.get("id")), bounded_text(block.get("name")))
                        {
                            kinds.push(HarnessObservationKind::Tool {
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
                // One native question per response. Multi-question forms are not silently flattened.
                let questions = input.get("questions")?.as_array()?;
                if questions.len() != 1 {
                    return None;
                }
                let question = &questions[0];
                if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
                    return None;
                }
                let prompt = bounded_text(question.get("question"))?;
                let options = question.get("options")?.as_array()?;
                if options.len() > MAX_HARNESS_ITEMS {
                    return None;
                }
                let choices = options
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        Some(HarnessChoice {
                            choice_id: index.to_string(),
                            label: bounded_text(value.get("label"))?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                (
                    HarnessInteractionKind::Question,
                    prompt,
                    "Answer one native Claude clarification".into(),
                    choices,
                )
            }
            CallbackRequest::CanUseTool { tool_name, .. } => {
                if tool_name.len() > MAX_HARNESS_ID_BYTES {
                    return None;
                }
                (
                    HarnessInteractionKind::Approval,
                    format!("Allow native Claude tool {tool_name}?"),
                    format!(
                        "Native tool {tool_name}; input is withheld, not dispatched as a Bcode tool"
                    ),
                    Vec::new(),
                )
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
                let question = input
                    .pointer("/questions/0/question")
                    .and_then(Value::as_str)
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
                updated_input["answers"] = json!({(question): answer});
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
    pub fn invalidate_callbacks(&mut self) {
        self.pending.clear();
        self.pending_grants.clear();
        self.assistance_grant = None;
    }
    pub fn gap(&mut self) -> Vec<HarnessObservation> {
        self.observation(HarnessObservationKind::Diagnostic {
            code: "native_observation_gap".into(),
            message: "Native observations exceeded a bound; reconcile before continuing".into(),
        })
        .map(|mut observation| {
            observation.completeness = HarnessCompleteness::Gap;
            vec![observation]
        })
        .unwrap_or_default()
    }
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
