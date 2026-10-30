//! Plugin-owned serialized question transitions for dynamic and bundled loading.
//!
//! State is returned to the host, not retained in a process-global controller map.

use bcode_plugin_models::interaction_controller::{
    INTERACTION_CONTROLLER_VERSION, InteractionControllerRequest, InteractionControllerRoute,
    InteractionControllerTransition, InteractionOutput, MAX_CONTROLLER_BYTES,
};
#[cfg(feature = "native")]
use bcode_plugin_models::interaction_controller::{
    InteractionControllerDiscovery, InteractionControllerDiscoveryRequest,
};
#[cfg(feature = "native")]
use bcode_plugin_sdk::{NativeServiceContext, ServiceResponse};

use super::{
    QUESTION_EXCHANGE_SCHEMA, QUESTION_INTERACTION_KIND,
    question_interaction::QuestionInteractionController,
};

use bcode_plugin_models::interaction_controller::{
    DurableInteractionBinding, DurableInteractionCapability, DurableInteractionRequest,
    DurableInteractionResponse, DurableInvocationCompletion, ProducerInteractionCheckpoint,
    validate_durable_size,
};
use serde::{Deserialize, Serialize};

/// Exact independently versioned durable question capability.
#[must_use]
pub fn durable_capability() -> DurableInteractionCapability {
    DurableInteractionCapability {
        route: route(),
        checkpoint_schema: "bcode.question.checkpoint".into(),
        checkpoint_version: 1,
        continuation_schema: "bcode.question.complete".into(),
        continuation_version: 1,
    }
}

/// One semantic answer. Indices are stable within the immutable normalized request;
/// labels/values alone cannot distinguish options with duplicate values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionCheckpointAnswerV1 {
    /// Exact selected option identities, sorted and unique.
    pub selected_option_indices: Vec<usize>,
    /// Producer answer value order, checked against the selected immutable options.
    pub selected: Vec<String>,
    /// Acknowledged producer custom text, not a renderer text buffer.
    pub custom: Option<String>,
}

/// Explicit producer durable representation. No focus, layout, controller cache or
/// validation prose is persisted. Restore always focuses the first semantic control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionCheckpointV1 {
    /// Immutable normalized question definitions.
    pub request: super::NormalizedQuestionRequest,
    /// Answers in question order; vector length must match the immutable request.
    pub answers: Vec<QuestionCheckpointAnswerV1>,
    /// None while pending; only a validated terminal producer output is retained.
    pub terminal: Option<InteractionOutput>,
}

fn validate_binding(binding: &DurableInteractionBinding) -> Result<(), ()> {
    binding.validate().map_err(|_| ())?;
    if binding.capability != durable_capability()
        || binding.tool_name != "question"
        || binding.exchange_schema_version != 1
    {
        return Err(());
    }
    Ok(())
}

fn restore_question(
    checkpoint: &QuestionCheckpointV1,
) -> Result<QuestionInteractionController, ()> {
    let request =
        super::parse_question_request(serde_json::to_value(&checkpoint.request).map_err(|_| ())?)
            .map_err(|_| ())?;
    if request != checkpoint.request {
        return Err(());
    }
    if checkpoint.answers.iter().any(|answer| {
        answer
            .selected_option_indices
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    }) {
        return Err(());
    }
    let mut controller =
        QuestionInteractionController::restore_durable_answers(request, &checkpoint.answers)?;
    if let Some(terminal) = &checkpoint.terminal {
        let input = match terminal {
            InteractionOutput::Submitted { payload } => {
                match serde_json::from_value::<super::QuestionResolutionPayload>(payload.clone())
                    .map_err(|_| ())?
                {
                    super::QuestionResolutionPayload::Answered { .. } => {
                        bcode_tool::InteractionInput::Submit
                    }
                    super::QuestionResolutionPayload::Dismissed => {
                        bcode_tool::InteractionInput::Cancel
                    }
                    super::QuestionResolutionPayload::Unanswered => return Err(()),
                }
            }
            _ => return Err(()),
        };
        if &controller.handle_input(input) != terminal {
            return Err(());
        }
    }
    Ok(controller)
}

fn question_transition(
    controller: QuestionInteractionController,
    output: InteractionOutput,
) -> Result<InteractionControllerTransition, ()> {
    Ok(InteractionControllerTransition {
        version: INTERACTION_CONTROLLER_VERSION,
        interaction_kind: QUESTION_INTERACTION_KIND.into(),
        snapshot: serde_json::to_value(controller.snapshot()).map_err(|_| ())?,
        state: serde_json::to_value(controller).map_err(|_| ())?,
        output,
    })
}

/// Execute the separate opt-in durable protocol. This is pure producer logic:
/// export is not a commit, restore grants no authority, complete never invokes a tool.
///
/// # Errors
/// Rejects unsupported versions/routes, invalid identities/checkpoints, nonterminal
/// completion, inconsistent outcomes and oversized serialized messages.
pub fn durable_transition(
    request: DurableInteractionRequest,
) -> Result<DurableInteractionResponse, QuestionControllerError> {
    durable_transition_inner(request).map_err(|()| QuestionControllerError)
}

fn durable_transition_inner(
    request: DurableInteractionRequest,
) -> Result<DurableInteractionResponse, ()> {
    request.validate().map_err(|_| ())?;
    let response = match request {
        DurableInteractionRequest::Discover { .. } => {
            DurableInteractionResponse::Capabilities(vec![durable_capability()])
        }
        DurableInteractionRequest::Export {
            binding,
            revision,
            transition,
            ..
        } => {
            validate_binding(&binding)?;
            if transition.version != INTERACTION_CONTROLLER_VERSION
                || transition.interaction_kind != QUESTION_INTERACTION_KIND
            {
                return Err(());
            }
            let controller: QuestionInteractionController =
                serde_json::from_value(transition.state).map_err(|_| ())?;
            controller.validate_ephemeral_state()?;
            if serde_json::to_value(controller.snapshot()).map_err(|_| ())? != transition.snapshot {
                return Err(());
            }
            let (request, answers) = controller.durable_answers();
            let terminal = match transition.output {
                InteractionOutput::None | InteractionOutput::Redraw => None,
                terminal => Some(terminal),
            };
            let payload = QuestionCheckpointV1 {
                request,
                answers,
                terminal,
            };
            let restored = restore_question(&payload)?;
            if restored.snapshot().closed != controller.snapshot().closed {
                return Err(());
            }
            DurableInteractionResponse::Exported(ProducerInteractionCheckpoint {
                binding,
                revision,
                payload: serde_json::to_value(payload).map_err(|_| ())?,
            })
        }
        DurableInteractionRequest::Restore {
            binding,
            checkpoint,
            ..
        } => {
            validate_binding(&binding)?;
            let payload: QuestionCheckpointV1 =
                serde_json::from_value(checkpoint.payload).map_err(|_| ())?;
            let controller = restore_question(&payload)?;
            DurableInteractionResponse::Restored(question_transition(
                controller,
                payload.terminal.unwrap_or(InteractionOutput::None),
            )?)
        }
        DurableInteractionRequest::Complete {
            binding,
            checkpoint,
            completion_id,
            ..
        } => {
            validate_binding(&binding)?;
            let payload: QuestionCheckpointV1 =
                serde_json::from_value(checkpoint.payload).map_err(|_| ())?;
            restore_question(&payload)?;
            let outcome = match payload.terminal.ok_or(())? {
                InteractionOutput::Submitted { payload: result } => {
                    let resolution: super::QuestionResolutionPayload =
                        serde_json::from_value(result).map_err(|_| ())?;
                    match resolution {
                        super::QuestionResolutionPayload::Answered { questions } => {
                            super::answered_outcome(&payload.request, questions).map_err(|_| ())?
                        }
                        super::QuestionResolutionPayload::Dismissed => {
                            super::dismissed_outcome(&payload.request)
                        }
                        super::QuestionResolutionPayload::Unanswered => return Err(()),
                    }
                }
                _ => return Err(()),
            };
            DurableInteractionResponse::Completed(DurableInvocationCompletion {
                response: serde_json::to_value(durable_question_response(
                    &binding.invocation_id,
                    &outcome,
                )?)
                .map_err(|_| ())?,
                binding,
                completion_id,
            })
        }
    };
    validate_durable_size(&response).map_err(|_| ())?;
    Ok(response)
}

/// Canonical question outcome formatting for pure durable completion. This uses the
/// same domain formatter and artifact contract as normal native tool completion.
pub(super) fn durable_question_response(
    invocation_id: &str,
    outcome: &super::QuestionToolOutcome,
) -> Result<bcode_tool::ToolInvocationResponse, ()> {
    let metadata = serde_json::to_value(outcome).map_err(|_| ())?;
    let full_output = serde_json::to_string(outcome).map_err(|_| ())?;
    Ok(bcode_tool::ToolInvocationResponse {
        output: super::format_question_outcome_output(outcome),
        is_error: false,
        content: Vec::new(),
        full_output: Some(full_output),
        result: Some(bcode_tool::ToolInvocationResult::Artifact {
            artifact: Box::new(bcode_tool::ToolArtifact {
                artifact_id: format!("question-outcome-{invocation_id}"),
                producer_plugin_id: "bcode.question".into(),
                schema: "bcode.question.outcome".into(),
                schema_version: 1,
                tool_call_id: Some(invocation_id.into()),
                title: Some("Question outcome".into()),
                metadata,
                refs: Vec::new(),
            }),
        }),
    })
}

/// Route only the separate durable producer service. Service registration belongs
/// to the plugin manifest/native adapter; ephemeral v1 operations are unchanged.
#[must_use]
#[cfg(feature = "native")]
pub fn invoke_durable(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.payload.len() > MAX_CONTROLLER_BYTES {
        return invalid();
    }
    let Ok(request) = context.request.payload_json::<DurableInteractionRequest>() else {
        return invalid();
    };
    let operation = match &request {
        DurableInteractionRequest::Discover { .. } => "discover",
        DurableInteractionRequest::Export { .. } => "export",
        DurableInteractionRequest::Restore { .. } => "restore",
        DurableInteractionRequest::Complete { .. } => "complete",
    };
    if context.request.operation != operation {
        return invalid();
    }
    durable_transition(request).map_or_else(
        |_| invalid(),
        |response| super::question_native::json_response(&response),
    )
}

/// Return the exact producer route supported by this controller.
#[must_use]
pub fn route() -> InteractionControllerRoute {
    InteractionControllerRoute {
        producer_id: "bcode.question".to_owned(),
        exchange_schema: QUESTION_EXCHANGE_SCHEMA.to_owned(),
        min_schema_version: 1,
        max_schema_version: 1,
        interaction_kind: QUESTION_INTERACTION_KIND.to_owned(),
    }
}

/// Route the versioned controller service. All diagnostics deliberately omit payloads.
#[must_use]
#[cfg(feature = "native")]
pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.payload.len() > MAX_CONTROLLER_BYTES {
        return ServiceResponse::error(
            "controller_limit_exceeded",
            "question controller request exceeds size limit",
        );
    }
    match context.request.operation.as_str() {
        "discover" => {
            let Ok(request) = context
                .request
                .payload_json::<InteractionControllerDiscoveryRequest>()
            else {
                return invalid();
            };
            if request.version != INTERACTION_CONTROLLER_VERSION {
                return invalid();
            }
            super::question_native::json_response(&InteractionControllerDiscovery {
                version: INTERACTION_CONTROLLER_VERSION,
                routes: vec![route()],
            })
        }
        "transition" => {
            let Ok(request) = context
                .request
                .payload_json::<InteractionControllerRequest>()
            else {
                return invalid();
            };
            transition(request).map_or_else(
                |_| invalid(),
                |response| super::question_native::json_response(&response),
            )
        }
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported question controller operation",
        ),
    }
}

#[cfg(feature = "native")]
fn invalid() -> ServiceResponse {
    ServiceResponse::error(
        "invalid_controller_contract",
        "invalid or unsupported question controller contract",
    )
}

/// Open or advance a bounded serialized producer controller.
///
/// # Errors
/// Rejects unsupported routes/versions, malformed state, and oversized transitions.
pub fn transition(
    request: InteractionControllerRequest,
) -> Result<InteractionControllerTransition, QuestionControllerError> {
    transition_inner(request).map_err(|()| QuestionControllerError)
}

/// Invalid, unsupported, or oversized question controller contract. Payloads are never included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuestionControllerError;
impl std::fmt::Display for QuestionControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid or unsupported question controller contract")
    }
}
impl std::error::Error for QuestionControllerError {}

fn transition_inner(
    request: InteractionControllerRequest,
) -> Result<InteractionControllerTransition, ()> {
    let (controller, output) = match request {
        InteractionControllerRequest::Open {
            version,
            route: requested_route,
            schema_version,
            payload,
        } => {
            if version != INTERACTION_CONTROLLER_VERSION
                || requested_route != route()
                || schema_version != 1
            {
                return Err(());
            }
            let request = super::parse_question_request(payload).map_err(|_| ())?;
            (
                QuestionInteractionController::new(request),
                InteractionOutput::None,
            )
        }
        InteractionControllerRequest::Input {
            version,
            route: requested_route,
            state,
            input,
        } => {
            if version != INTERACTION_CONTROLLER_VERSION || requested_route != route() {
                return Err(());
            }
            let mut controller: QuestionInteractionController =
                serde_json::from_value(state).map_err(|_| ())?;
            controller.validate_ephemeral_state()?;
            let output = controller.handle_input(input);
            (controller, output)
        }
    };
    let snapshot = serde_json::to_value(controller.snapshot()).map_err(|_| ())?;
    let state = serde_json::to_value(controller).map_err(|_| ())?;
    let response = InteractionControllerTransition {
        version: INTERACTION_CONTROLLER_VERSION,
        interaction_kind: QUESTION_INTERACTION_KIND.to_owned(),
        state,
        snapshot,
        output,
    };
    // The host enforces the same bound before admission. A plugin never returns an oversized state.
    if serde_json::to_vec(&response).map_err(|_| ())?.len() > MAX_CONTROLLER_BYTES {
        return Err(());
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_models::interaction_controller::{InteractionControlId, InteractionInput};
    use serde_json::{Value, json};

    fn open() -> InteractionControllerTransition {
        transition(InteractionControllerRequest::Open {
            version: 1,
            route: route(),
            schema_version: 1,
            payload: json!({"questions": [{"question":"Choose", "required":true, "custom":false,
                "options":[{"label":"Yes","value":"yes"}]}]}),
        })
        .unwrap()
    }
    fn apply(state: Value, input: InteractionInput) -> InteractionControllerTransition {
        transition(InteractionControllerRequest::Input {
            version: 1,
            route: route(),
            state,
            input,
        })
        .unwrap()
    }
    fn durable_binding() -> DurableInteractionBinding {
        DurableInteractionBinding {
            session_id: "session".into(),
            turn_id: "turn".into(),
            invocation_id: "call-question".into(),
            request_sequence: 1,
            exchange_id: "call-question-question".into(),
            tool_name: "question".into(),
            request_digest: "a".repeat(64),
            exchange_schema_version: 1,
            capability: durable_capability(),
        }
    }
    fn export(transition: InteractionControllerTransition) -> ProducerInteractionCheckpoint {
        let DurableInteractionResponse::Exported(checkpoint) =
            durable_transition(DurableInteractionRequest::Export {
                version: 1,
                binding: durable_binding(),
                revision: 4,
                transition,
            })
            .unwrap()
        else {
            panic!("export");
        };
        checkpoint
    }
    fn restore(checkpoint: ProducerInteractionCheckpoint) -> InteractionControllerTransition {
        let DurableInteractionResponse::Restored(transition) =
            durable_transition(DurableInteractionRequest::Restore {
                version: 1,
                binding: durable_binding(),
                checkpoint,
            })
            .unwrap()
        else {
            panic!("restore");
        };
        transition
    }

    #[test]
    fn durable_selection_roundtrip_uses_semantics_and_deterministic_focus() {
        let selected = apply(
            open().state,
            InteractionInput::Activate {
                control_id: InteractionControlId::new("question-0.option-0"),
            },
        );
        let before: super::super::question_interaction::QuestionSnapshot =
            serde_json::from_value(selected.snapshot.clone()).unwrap();
        let checkpoint = export(selected);
        let payload: QuestionCheckpointV1 =
            serde_json::from_value(checkpoint.payload.clone()).unwrap();
        assert_eq!(payload.answers[0].selected_option_indices, vec![0]);
        assert_eq!(payload.answers[0].selected, vec!["yes"]);
        assert!(payload.terminal.is_none());
        let restored = restore(checkpoint.clone());
        let snapshot: super::super::question_interaction::QuestionSnapshot =
            serde_json::from_value(restored.snapshot.clone()).unwrap();
        assert_eq!(snapshot.answers, before.answers);
        assert_eq!(
            snapshot.selected_option_indices,
            before.selected_option_indices
        );
        assert_eq!(
            snapshot.focus,
            super::super::question_interaction::QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 0
            }
        );
        assert_eq!(export(restored).payload, checkpoint.payload);
        let submitted = apply(restore(checkpoint).state, InteractionInput::Submit);
        let checkpoint = export(submitted);
        let request = DurableInteractionRequest::Complete {
            version: 1,
            binding: durable_binding(),
            checkpoint,
            completion_id: "resolved-operation".into(),
        };
        let completion = durable_transition(request.clone()).unwrap();
        assert_eq!(completion, durable_transition(request).unwrap());
        let DurableInteractionResponse::Completed(completion) = completion else {
            panic!("complete");
        };
        let response: bcode_tool::ToolInvocationResponse =
            serde_json::from_value(completion.response).unwrap();
        assert!(!response.is_error);
        let outcome: super::super::QuestionToolOutcome =
            serde_json::from_str(response.full_output.as_ref().unwrap()).unwrap();
        assert_eq!(
            response.output,
            super::super::format_question_outcome_output(&outcome)
        );
        let Some(bcode_tool::ToolInvocationResult::Artifact { artifact }) = response.result else {
            panic!("artifact");
        };
        assert_eq!(artifact.tool_call_id.as_deref(), Some("call-question"));
        assert_eq!(artifact.artifact_id, "question-outcome-call-question");
        assert_eq!(artifact.metadata, serde_json::to_value(outcome).unwrap());
    }

    #[test]
    fn durable_custom_text_and_cancel_roundtrip() {
        let opened = transition(InteractionControllerRequest::Open {
            version: 1,
            route: route(),
            schema_version: 1,
            payload: json!({"questions":[{"question":"Custom", "custom":true, "options":[]}]}),
        })
        .unwrap();
        let edited = apply(
            opened.state,
            InteractionInput::Change {
                control_id: InteractionControlId::new("question-0.custom"),
                value: bcode_tool::InteractionValue::String("acknowledged answer".into()),
            },
        );
        let checkpoint = export(edited);
        let restored = restore(checkpoint);
        let snapshot: super::super::question_interaction::QuestionSnapshot =
            serde_json::from_value(restored.snapshot).unwrap();
        assert_eq!(
            snapshot.answers[0].custom.as_deref(),
            Some("acknowledged answer")
        );
        let cancelled = apply(restored.state, InteractionInput::Cancel);
        let checkpoint = export(cancelled);
        assert_eq!(
            restore(checkpoint.clone()).output,
            InteractionOutput::Submitted {
                payload: json!({"status":"dismissed"}),
            }
        );
        let DurableInteractionResponse::Completed(completion) =
            durable_transition(DurableInteractionRequest::Complete {
                version: 1,
                binding: durable_binding(),
                checkpoint,
                completion_id: "cancel-operation".into(),
            })
            .unwrap()
        else {
            panic!("cancel completion");
        };
        let response: bcode_tool::ToolInvocationResponse =
            serde_json::from_value(completion.response).unwrap();
        assert!(!response.is_error);
        assert!(response.full_output.unwrap().contains("dismissed"));
    }

    #[test]
    fn durable_rejects_unknown_versions_malformed_answers_and_nonterminal_completion() {
        assert!(durable_transition(DurableInteractionRequest::Discover { version: 2 }).is_err());
        let checkpoint = export(open());
        let completion = DurableInteractionRequest::Complete {
            version: 1,
            binding: durable_binding(),
            checkpoint: checkpoint.clone(),
            completion_id: "done".into(),
        };
        assert!(durable_transition(completion).is_err());
        for mutation in [
            json!([]),
            json!([{"selected_option_indices":[99],"selected":["yes"],"custom":null}]),
            json!([{"selected_option_indices":[0,0],"selected":["yes"],"custom":null}]),
            json!([{"selected_option_indices":[0],"selected":["forged"],"custom":null}]),
        ] {
            let mut invalid = checkpoint.clone();
            invalid.payload["answers"] = mutation;
            assert!(
                durable_transition(DurableInteractionRequest::Restore {
                    version: 1,
                    binding: durable_binding(),
                    checkpoint: invalid
                })
                .is_err()
            );
        }
        let mut invalid = checkpoint.clone();
        invalid.payload["terminal"] = serde_json::to_value(InteractionOutput::Submitted {
            payload: json!({"status":"answered","questions":[]}),
        })
        .unwrap();
        assert!(
            durable_transition(DurableInteractionRequest::Restore {
                version: 1,
                binding: durable_binding(),
                checkpoint: invalid
            })
            .is_err()
        );
        let mut binding = durable_binding();
        binding.capability.checkpoint_version = 2;
        let mut future = checkpoint.clone();
        future.binding = binding.clone();
        assert!(
            durable_transition(DurableInteractionRequest::Restore {
                version: 1,
                binding,
                checkpoint: future
            })
            .is_err()
        );
        let mut other = durable_binding();
        other.invocation_id = "different".into();
        assert!(
            durable_transition(DurableInteractionRequest::Restore {
                version: 1,
                binding: other,
                checkpoint
            })
            .is_err()
        );
    }

    #[test]
    fn durable_discovery_and_response_serialization_are_explicit() {
        let response =
            durable_transition(DurableInteractionRequest::Discover { version: 1 }).unwrap();
        assert_eq!(
            response,
            DurableInteractionResponse::Capabilities(vec![durable_capability()])
        );
        assert_eq!(
            serde_json::from_value::<DurableInteractionResponse>(
                serde_json::to_value(&response).unwrap()
            )
            .unwrap(),
            response
        );
        let checkpoint = export(open());
        let mut json = serde_json::to_value(&checkpoint.payload).unwrap();
        json["unknown"] = true.into();
        assert!(serde_json::from_value::<QuestionCheckpointV1>(json).is_err());
    }

    #[test]
    fn canonical_request_and_ephemeral_state_roundtrip_before_input() {
        let opened = open();
        let controller: QuestionInteractionController =
            serde_json::from_value(opened.state.clone()).unwrap();
        assert!(controller.validate_ephemeral_state().is_ok());
        assert_eq!(serde_json::to_value(&controller).unwrap(), opened.state);
        assert_eq!(
            serde_json::to_value(controller.snapshot()).unwrap(),
            opened.snapshot
        );
    }
    #[test]
    fn serialized_controller_retains_plugin_validation_and_submit_semantics() {
        let opened = open();
        let invalid = apply(opened.state, InteractionInput::Submit);
        assert!(!matches!(
            invalid.output,
            InteractionOutput::Submitted { .. }
        ));
        let selected = apply(
            invalid.state,
            InteractionInput::Activate {
                control_id: InteractionControlId::new("question-0.option-0"),
            },
        );
        let submitted = apply(selected.state, InteractionInput::Submit);
        assert!(matches!(
            submitted.output,
            InteractionOutput::Submitted { .. }
        ));
    }
    #[test]
    fn future_and_spoofed_routes_fail_without_interpreting_payload() {
        assert!(
            transition(InteractionControllerRequest::Open {
                version: 2,
                route: route(),
                schema_version: 1,
                payload: Value::Null
            })
            .is_err()
        );
        let mut spoofed = route();
        spoofed.producer_id = "other".into();
        assert!(
            transition(InteractionControllerRequest::Open {
                version: 1,
                route: spoofed,
                schema_version: 1,
                payload: Value::Null
            })
            .is_err()
        );
    }
    #[test]
    fn semantic_cancel_is_plugin_owned_and_corrupt_state_is_rejected() {
        let cancelled = apply(open().state, InteractionInput::Cancel);
        assert!(matches!(
            cancelled.output,
            InteractionOutput::Submitted { .. } | InteractionOutput::Cancelled
        ));
        assert!(
            transition(InteractionControllerRequest::Input {
                version: 1,
                route: route(),
                state: json!({}),
                input: InteractionInput::Submit
            })
            .is_err()
        );
    }
}
