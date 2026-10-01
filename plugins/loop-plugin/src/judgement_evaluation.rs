//! Optional loop-owned, read-only judgement of agent-collected evidence.
//! No vendor transport or credential lookup belongs here: the application owns both.

use super::*;
use bcode_model::judgement::{self, Answer, Question, State, WorkflowJudgementRequest};
use std::collections::BTreeMap;

const OPERATION: &str = "loop.judgement.evaluate";
const EVIDENCE_LIMIT: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvaluationConfig {
    pub provider_plugin_id: String,
    pub model_id: String,
    pub auth_profile: String,
    /// Completion requires at least this probability, plus concrete agent-collected evidence.
    pub threshold_percent: u8,
    /// The existing read-only agent evaluator can be retained on service failure.
    pub on_failure: FailurePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    Pause,
    AgentFallback,
}

impl EvaluationConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.provider_plugin_id.trim().is_empty()
            || self.provider_plugin_id.len() > 256
            || self.model_id.trim().is_empty()
            || self.model_id.len() > 256
            || self.auth_profile.len() > 256
            || !(1..=100).contains(&self.threshold_percent)
        {
            return Err("invalid loop judgement configuration");
        }
        Ok(())
    }
}

pub fn parse_config(raw: &str) -> Result<Option<EvaluationConfig>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > 1024 {
        return Err("judgement configuration is too large".into());
    }
    // Slash-delimited fields avoid ambiguity with provider IDs. Profile "-" selects the
    // provider's declared daemon environment credentials; named profiles never fall back.
    let fields: Vec<_> = raw.split('/').collect();
    let [
        provider_plugin_id,
        model_id,
        auth_profile,
        threshold,
        on_failure,
    ] = fields.as_slice()
    else {
        return Err(
            "judgement expects provider/model/profile/threshold/failure (pause or agent_fallback)"
                .into(),
        );
    };
    let threshold_percent: u8 = threshold
        .parse()
        .map_err(|_| "judgement threshold must be 1..=100".to_string())?;
    let on_failure = match *on_failure {
        "pause" => FailurePolicy::Pause,
        "agent_fallback" => FailurePolicy::AgentFallback,
        _ => return Err("judgement failure must be pause or agent_fallback".into()),
    };
    let config = EvaluationConfig {
        provider_plugin_id: (*provider_plugin_id).into(),
        model_id: (*model_id).into(),
        auth_profile: if *auth_profile == "-" {
            String::new()
        } else {
            (*auth_profile).into()
        },
        threshold_percent,
        on_failure,
    };
    config.validate().map_err(str::to_string)?;
    Ok(Some(config))
}

pub fn pinned_input_transform() -> bcode_workflow::WorkflowTransform {
    use bcode_workflow::WorkflowTransformExpression as Expr;
    let current = |path: &str| Expr::Input {
        source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.into(),
        path: path.into(),
    };
    let pinned = |path: &str| Expr::Input {
        source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE.into(),
        path: path.into(),
    };
    let fields = [
        ("implementation_prompt", pinned("implementation_prompt")),
        ("stop_condition", pinned("stop_condition")),
        ("max_iterations", pinned("max_iterations")),
        ("planning_ready", pinned("planning_ready")),
        (
            "delivery_required",
            Expr::Default {
                value: Box::new(pinned("delivery_required")),
                default: Box::new(Expr::Constant {
                    value: false.into(),
                }),
            },
        ),
        (
            "judgement_evaluation",
            Expr::Default {
                value: Box::new(pinned("judgement_evaluation")),
                default: Box::new(Expr::Constant {
                    value: serde_json::Value::Null,
                }),
            },
        ),
        ("iteration", current("iteration")),
        ("condition_met", current("condition_met")),
        ("external_blocker", current("external_blocker")),
        ("evidence", current("evidence")),
        ("summary", current("summary")),
        (
            "delivery",
            Expr::Default {
                value: Box::new(current("delivery")),
                default: Box::new(Expr::Constant {
                    value: serde_json::Value::Null,
                }),
            },
        ),
    ]
    .into_iter()
    .map(|(field, expression)| (field.into(), expression))
    .collect();
    bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: Expr::Object { fields },
        output: bcode_workflow::ValueSchema::of::<LoopWorkflowIteration>(),
    }
}

/// Resume consent forwards retained state, never caller-supplied goal data.
/// It clears the reported blocker but cannot attest completion or grant tool authority.
pub fn resume_input_transform() -> bcode_workflow::WorkflowTransform {
    use bcode_workflow::WorkflowTransformExpression as Expr;
    let mut transform = pinned_input_transform();
    if let Expr::Object { fields } = &mut transform.expression {
        fields.insert(
            "condition_met".into(),
            Expr::Constant {
                value: false.into(),
            },
        );
        fields.insert(
            "external_blocker".into(),
            Expr::Constant {
                value: "none".into(),
            },
        );
    }
    transform
}

pub fn manifest_block() -> bcode_workflow::WorkflowBlockDefinition {
    let manifest: bcode_plugin::PluginManifest =
        toml::from_str(include_str!("../bcode-plugin.toml")).expect("loop manifest");
    manifest
        .services
        .iter()
        .flat_map(|service| &service.workflow_blocks)
        .find(|block| block.operation == OPERATION)
        .expect("loop judgement block declaration")
        .clone()
}

pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation != OPERATION {
        return ServiceResponse::error("unsupported_operation", "unsupported loop block");
    }
    if context.request.payload.len() > judgement::MAX_REQUEST_BYTES + 16_384 {
        return ServiceResponse::error("invalid_request", "loop evaluation input exceeds limit");
    }
    let Ok(invocation) = context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    else {
        return ServiceResponse::error("invalid_request", "invalid loop block invocation");
    };
    if !valid_delivery_input(&invocation.input) {
        return ServiceResponse::error("invalid_request", "invalid loop delivery report");
    }
    let Ok(mut input) = invocation.typed_input::<LoopWorkflowIteration>() else {
        return ServiceResponse::error("invalid_request", "invalid loop evaluation input");
    };
    if input
        .judgement_evaluation
        .as_ref()
        .is_some_and(|config| config.validate().is_err())
    {
        return ServiceResponse::error("invalid_request", "invalid loop judgement configuration");
    }
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("cancelled", "loop evaluation cancelled");
    }
    if mismatched_delivery_criteria(&mut input)
        || missing_required_delivery(&mut input)
        || negative_delivery(&mut input)
    {
        return json_response(&input);
    }
    if input.condition_met
        && input.delivery.as_ref().is_some_and(|report| {
            !authenticate_contributions(&context.bridge, &invocation.dispatch_identity, report)
        })
    {
        input.condition_met = false;
        input.summary = "Completion withheld: canonical contribution references could not be authenticated in this run or report unresolved work".into();
        return json_response(&input);
    }
    if reject_snapshot_delivery(&mut input, &context.bridge, &invocation.dispatch_identity) {
        return json_response(&input);
    }
    if reject_legacy_delivery(&mut input, &context.bridge, &invocation.dispatch_identity) {
        return json_response(&input);
    }
    // Agent-only loops use the same deterministic safeguard without provider dispatch.
    let Some(config) = input.judgement_evaluation.clone() else {
        safeguard_agent_evidence(&mut input);
        return json_response(&input);
    };
    let result = evaluate(&context.bridge, &invocation.dispatch_identity, &input);
    match result {
        Ok((completed, probability)) => {
            input.condition_met = completed;
            input.summary = format!(
                "Judgement probability {probability:.3}; completion threshold {}%; {}",
                config.threshold_percent,
                if completed {
                    "condition met"
                } else {
                    "continue"
                }
            );
            json_response(&input)
        }
        Err(_) if context.cancellation.is_cancelled() => {
            ServiceResponse::error("cancelled", "loop evaluation cancelled")
        }
        Err("judgement service unavailable")
            if config.on_failure == FailurePolicy::AgentFallback =>
        {
            let evidence_bytes = input.evidence.join("\n").len();
            if input.evidence.is_empty()
                || input.evidence.iter().any(|item| item.trim().is_empty())
                || evidence_bytes > EVIDENCE_LIMIT
                || input.stop_condition.len() + evidence_bytes > judgement::MAX_REQUEST_BYTES / 2
            {
                return ServiceResponse::error(
                    "insufficient_evidence",
                    "loop evaluation has no bounded concrete evidence",
                );
            }
            // A failure is not completion. The agent's original decision remains visible on
            // explicitly selected fallback; no model answer is fabricated.
            input.summary = format!(
                "{} (judgement unavailable; agent evaluation retained)",
                input.summary
            );
            json_response(&input)
        }
        Err(_) => ServiceResponse::error(
            "judgement_unavailable",
            "loop judgement unavailable; workflow requires attention",
        ),
    }
}

// Negative evidence cannot be overridden by model confidence or provider fallback.
fn reject_legacy_delivery(
    input: &mut LoopWorkflowIteration,
    bridge: &ServiceBridge,
    dispatch_identity: &str,
) -> bool {
    if input
        .delivery
        .as_ref()
        .is_none_or(|report| report.version == delivery::ReportVersion::V1)
        && reject_unobserved_checks(input, bridge, dispatch_identity)
    {
        return true;
    }
    // V1 records evaluator assertions only. Even authenticated output identities
    // do not bind a successful check to the delivered content. Preserve the report
    // for inspection, but do not let confidence or fallback certify those claims.
    // Apply even to a negative agent verdict: judgement may otherwise promote it.
    if input
        .delivery
        .as_ref()
        .is_some_and(|report| report.version == delivery::ReportVersion::V1)
    {
        input.condition_met = false;
        input.summary = "Completion withheld: delivery V1 has no target-bound observed verification; retain the report and contributions until canonical verification support is available".into();
        return true;
    }
    false
}

fn reject_snapshot_delivery(
    input: &mut LoopWorkflowIteration,
    bridge: &ServiceBridge,
    dispatch_identity: &str,
) -> bool {
    if let Some(report) = &input.delivery
        && matches!(
            report.version,
            delivery::ReportVersion::V2 | delivery::ReportVersion::V3
        )
    {
        let verified = !report.precludes_completion()
            && report.snapshot_coverage(&input.stop_condition)
            && report
                .contribution_output_ids
                .iter()
                .all(|id| authenticate_output(bridge, dispatch_identity, id, report))
            && report.checks.iter().all(|check| {
                check.execution.as_ref().is_some_and(|reference| {
                    inspect_execution(bridge, dispatch_identity, &reference.output_id).is_some_and(
                        |evidence| {
                            report.delivered_snapshot.as_ref().is_some_and(|snapshot| {
                                super::delivery_execution::delivered_check(
                                    &evidence, reference, snapshot,
                                )
                            }) || report.repository_delivery.as_ref().is_some_and(|delivery| {
                                super::delivery_execution::repository_check(
                                    &evidence, reference, delivery,
                                )
                            })
                        },
                    )
                })
            });
        if !verified {
            input.condition_met = false;
            input.summary = "Completion withheld: incomplete original-criterion coverage or unauthenticated, failed, stale immutable delivery verification".into();
            return true;
        }
        input.summary.push_str("; delivery is the exact retained target, not live checkout freshness or hermetic environment verification; criterion reviews remain judgments");
    }
    false
}

fn negative_delivery(input: &mut LoopWorkflowIteration) -> bool {
    if input.external_blocker == LoopExternalBlocker::None
        && !input
            .delivery
            .as_ref()
            .is_some_and(delivery::DeliveryReport::precludes_completion)
    {
        return false;
    }
    input.condition_met = false;
    input.summary = "Completion withheld: resolve the reported blocker, failed or unverified delivery evidence, or unresolved work before reevaluation".into();
    true
}

fn mismatched_delivery_criteria(input: &mut LoopWorkflowIteration) -> bool {
    if input.delivery.as_ref().is_some_and(|report| {
        report.original_stop_condition.is_some()
            && !report.identifies_original_criteria(&input.stop_condition)
    }) {
        input.condition_met = false;
        input.summary =
            "Completion withheld: delivery coverage does not identify the original stop condition"
                .into();
        return true;
    }
    false
}

fn missing_required_delivery(input: &mut LoopWorkflowIteration) -> bool {
    if !input.delivery_required || input.delivery.is_some() {
        return false;
    }
    input.condition_met = false;
    input.summary =
        "Completion withheld: this goal requires a delivery report; omission is not verification"
            .into();
    true
}

fn valid_delivery_input(input: &serde_json::Value) -> bool {
    input
        .get("delivery")
        .filter(|value| !value.is_null())
        .is_none_or(|value| {
            bcode_workflow::ValueSchema::of::<delivery::DeliveryReport>()
                .validate_value("loop.delivery", value)
                .is_ok()
        })
}

fn safeguard_agent_evidence(input: &mut LoopWorkflowIteration) {
    // Absence of a judgement provider does not make empty or unbounded evidence sufficient.
    let evidence_bytes = input.evidence.join("\n").len();
    if input.condition_met
        && (input.evidence.is_empty()
            || input.evidence.iter().any(|item| item.trim().is_empty())
            || evidence_bytes > EVIDENCE_LIMIT
            || input.stop_condition.len() + evidence_bytes > judgement::MAX_REQUEST_BYTES / 2)
    {
        input.condition_met = false;
        input.summary = "Completion withheld: no bounded concrete evaluation evidence".into();
    }
}

fn reject_unobserved_checks(
    input: &mut LoopWorkflowIteration,
    bridge: &ServiceBridge,
    dispatch_identity: &str,
) -> bool {
    let rejected = input.condition_met
        && input.delivery.as_ref().is_some_and(|report| {
            report.checks.iter().any(|check| {
                check.execution.as_ref().is_some_and(|reference| {
                    !authenticate_check(bridge, dispatch_identity, check, reference)
                })
            })
        });
    if rejected {
        input.condition_met = false;
        input.summary = "Completion withheld: referenced checks lack matching shell-owned execution and unchanged selected-file observations".into();
    }
    rejected
}

fn authenticate_check(
    bridge: &ServiceBridge,
    dispatch_identity: &str,
    check: &delivery::Check,
    reference: &delivery::CheckExecution,
) -> bool {
    let Some(evidence) = inspect_execution(bridge, dispatch_identity, &reference.output_id) else {
        return false;
    };
    if !super::delivery_execution::observed_check(&evidence, check, reference) {
        return false;
    }
    reference
        .observation_output_id
        .as_ref()
        .is_none_or(|output_id| {
            inspect_execution(bridge, dispatch_identity, output_id).is_some_and(|observation| {
                super::delivery_execution::reobserved_check(
                    &evidence,
                    &observation,
                    check,
                    reference,
                    output_id,
                )
            })
        })
}

fn inspect_execution(
    bridge: &ServiceBridge,
    dispatch_identity: &str,
    output_id: &str,
) -> Option<bcode_workflow::WorkflowOutputExecutionEvidence> {
    let request = bcode_tool::ToolInvocationServiceRequest {
        invocation_id: dispatch_identity.into(),
        request_id: "loop-check".into(),
        route_id: Some(bcode_workflow::WORKFLOW_EVIDENCE_INTERFACE_ID.into()),
        interface_id: bcode_workflow::WORKFLOW_EVIDENCE_INTERFACE_ID.into(),
        operation: bcode_workflow::OP_INSPECT_OUTPUT_EXECUTION.into(),
        payload: serde_json::json!(bcode_workflow::WorkflowOutputEvidenceRequest {
            output_id: output_id.into(),
        }),
    };
    let Ok(ServiceBridgeResponse::Service(
        bcode_tool::ToolInvocationServiceResolution::Responded { payload },
    )) = bridge.request(&ServiceBridgeRequest::InvokeService(request))
    else {
        return None;
    };
    serde_json::from_value(payload).ok()
}

fn authenticate_contributions(
    bridge: &ServiceBridge,
    dispatch_identity: &str,
    report: &delivery::DeliveryReport,
) -> bool {
    report
        .contribution_output_ids
        .iter()
        .all(|id| authenticate_output(bridge, dispatch_identity, id, report))
}

fn authenticate_output(
    bridge: &ServiceBridge,
    dispatch_identity: &str,
    output_id: &str,
    report: &delivery::DeliveryReport,
) -> bool {
    let request = bcode_tool::ToolInvocationServiceRequest {
        invocation_id: dispatch_identity.into(),
        request_id: "loop-contribution".into(),
        route_id: Some(bcode_workflow::WORKFLOW_EVIDENCE_INTERFACE_ID.into()),
        interface_id: bcode_workflow::WORKFLOW_EVIDENCE_INTERFACE_ID.into(),
        operation: bcode_workflow::OP_INSPECT_OUTPUT_PROVENANCE.into(),
        payload: serde_json::json!(bcode_workflow::WorkflowOutputEvidenceRequest {
            output_id: output_id.into(),
        }),
    };
    let Ok(ServiceBridgeResponse::Service(
        bcode_tool::ToolInvocationServiceResolution::Responded { payload },
    )) = bridge.request(&ServiceBridgeRequest::InvokeService(request))
    else {
        return false;
    };
    serde_json::from_value::<bcode_workflow::WorkflowOutputProvenance>(payload).is_ok_and(
        |provenance| {
            valid_contribution_identity(&provenance, output_id)
                && if report
                    .resolutions
                    .iter()
                    .any(|resolution| resolution.output_id == output_id)
                {
                    // Explicit resolution claims must authenticate even when this
                    // output has no recognized negative items. Otherwise a stale,
                    // duplicate or custom-schema claim silently bypasses validation.
                    report.resolves(&provenance.output)
                } else {
                    !delivery::contribution_precludes_completion(&provenance.output)
                }
        },
    )
}

#[cfg(test)]
fn valid_contribution_provenance(
    provenance: &bcode_workflow::WorkflowOutputProvenance,
    output_id: &str,
) -> bool {
    valid_contribution_identity(provenance, output_id)
        && !delivery::contribution_precludes_completion(&provenance.output)
}

fn valid_contribution_identity(
    provenance: &bcode_workflow::WorkflowOutputProvenance,
    output_id: &str,
) -> bool {
    let evidence = &provenance.output;
    provenance.version == bcode_workflow::WorkflowOutputProvenance::VERSION
                && provenance.producer_revision > 0
                && provenance.producer.id == evidence.node_id
                // Until adapted contribution contracts are interpreted, do not let an
                // envelope hide the worker's blockers behind an unrelated schema.
                && provenance.producer.dataflow
                    == bcode_workflow::WorkflowNodeDataflowPolicy::Direct
                && evidence.version == bcode_workflow::WORKFLOW_OUTPUT_INSPECTION_VERSION
                && evidence.output_id == output_id
                && evidence.checksum_sha256.len() == 64
                && evidence
                    .checksum_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
}

fn evaluate(
    bridge: &ServiceBridge,
    dispatch_identity: &str,
    input: &LoopWorkflowIteration,
) -> Result<(bool, f64), &'static str> {
    let config = input
        .judgement_evaluation
        .as_ref()
        .ok_or("missing configuration")?;
    let evidence = input.evidence.join("\n");
    if input.evidence.is_empty()
        || input.evidence.iter().any(|item| item.trim().is_empty())
        || evidence.len() > EVIDENCE_LIMIT
        || input.stop_condition.len() + evidence.len() > judgement::MAX_REQUEST_BYTES / 2
    {
        return Err("insufficient or oversized evidence");
    }
    let question = Question::YesNo {
        instructions: "Given the stop condition and the observed evidence, is there enough verified evidence to conclude that the condition is met? Do not assume unverified work succeeded.".into(),
    };
    let request = WorkflowJudgementRequest {
        provider_plugin_id: config.provider_plugin_id.clone(),
        auth_profile: config.auth_profile.clone(),
        request: judgement::Request {
            model_id: config.model_id.clone(),
            state: State::Structured(serde_json::json!({
                "stop_condition": input.stop_condition,
                "iteration": input.iteration,
                "evidence": input.evidence,
                "delivery": input.delivery,
            })),
            questions: BTreeMap::from([("completion".into(), question)]),
        },
    };
    judgement::validate_request(&request.request)?;
    let payload = serde_json::to_value(request).map_err(|_| "invalid request")?;
    let resolution = bridge
        .request(&ServiceBridgeRequest::InvokeService(
            bcode_tool::ToolInvocationServiceRequest {
                invocation_id: dispatch_identity.into(),
                request_id: "loop-completion".into(),
                route_id: Some(judgement::WORKFLOW_APPLICATION_INTERFACE_ID.into()),
                interface_id: judgement::WORKFLOW_APPLICATION_INTERFACE_ID.into(),
                operation: judgement::OP_WORKFLOW_JUDGE.into(),
                payload,
            },
        ))
        .map_err(|_| "judgement service unavailable")?;
    let response = match resolution {
        ServiceBridgeResponse::Service(
            bcode_tool::ToolInvocationServiceResolution::Responded { payload },
        ) => serde_json::from_value::<judgement::Response>(payload)
            .map_err(|_| "invalid judgement answer")?,
        ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Failed {
            code,
            ..
        }) if code == "judgement_failed" => return Err("judgement service unavailable"),
        ServiceBridgeResponse::Service(
            bcode_tool::ToolInvocationServiceResolution::Unsupported,
        ) => {
            return Err("judgement service unavailable");
        }
        _ => return Err("invalid judgement service response"),
    };
    if response.answers.len() != 1 {
        return Err("invalid judgement answer");
    }
    let Some(Answer::YesNo { probability }) = response.answers.get("completion") else {
        return Err("invalid judgement answer");
    };
    if !probability.is_finite() || !(0.0..=1.0).contains(probability) {
        return Err("invalid judgement answer");
    }
    Ok((
        // Model confidence is an additional gate, not authority to promote an
        // evaluator's unresolved or unverified stop condition into completion.
        input.condition_met && *probability >= f64::from(config.threshold_percent) / 100.0,
        *probability,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collaboration_delivery_requirement_survives_evaluator_omission_and_resume() {
        let input = LoopWorkflowInput::new("implement".into(), "complete".into(), 2).unwrap();
        let base = loop_workflow_spec(&input).unwrap();
        let spec = collaborating_goal_spec(&base).unwrap();
        let state = serde_json::to_value(loop_workflow_initial_value(&input)).unwrap();
        let mut current = state.clone();
        current["condition_met"] = true.into();
        current["delivery_required"] = false.into();
        for definition in [base.definition(), spec.definition()] {
            let mut evaluated = 0;
            for edge in &definition.edges {
                if edge.to != "loop.judgement.evaluate" {
                    continue;
                }
                let output = edge
                    .transform
                    .as_ref()
                    .expect("pinned entry")
                    .evaluate(&[
                        bcode_workflow::WorkflowTransformInput {
                            name: "state",
                            value: &state,
                        },
                        bcode_workflow::WorkflowTransformInput {
                            name: "current",
                            value: &current,
                        },
                    ])
                    .unwrap();
                assert_eq!(output["delivery_required"], definition == spec.definition());
                assert_eq!(output["stop_condition"], state["stop_condition"]);
                evaluated += 1;
            }
            assert_eq!(evaluated, 2, "normal and approval entry both pinned");
        }
    }

    #[test]
    fn required_delivery_omission_blocks_all_judgement_policies() {
        for policy in [
            "",
            "bcode.fake-provider/fake-judgement/-/90/pause",
            "bcode.fake-provider/fake-judgement/-/90/agent_fallback",
        ] {
            let mut input =
                LoopWorkflowInput::new("implement".into(), "complete".into(), 2).unwrap();
            input.judgement_evaluation = parse_config(policy).unwrap();
            let mut state = loop_workflow_initial_value(&input);
            state.delivery_required = true;
            state.condition_met = true;
            state.evidence = vec!["claimed verification".into()];
            for (null_report, condition_met) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                state.condition_met = condition_met;
                let mut value = serde_json::to_value(&state).unwrap();
                if null_report {
                    value["delivery"] = serde_json::Value::Null;
                }
                let invocation = bcode_workflow::WorkflowBlockInvocation {
                    version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                    dispatch_identity: "dispatch".into(),
                    workspace_root: std::env::temp_dir(),
                    input: value,
                    preparation: None,
                };
                let context = NativeServiceContext {
                    plugin_id: PLUGIN_ID.into(),
                    request: ServiceRequest {
                        interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.into(),
                        operation: OPERATION.into(),
                        payload: serde_json::to_vec(&invocation).unwrap(),
                    },
                    config: bcode_plugin_sdk::PluginConfigContext::default(),
                    events: ServiceEventEmitter::default(),
                    cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                    bridge: ServiceBridge::default(),
                    transient_progress_limits: TransientProgressLimits::default(),
                };
                let response = invoke(&context);
                assert!(response.error.is_none());
                let output: LoopWorkflowIteration =
                    serde_json::from_slice(&response.payload).unwrap();
                assert!(!output.condition_met);
                assert!(output.summary.contains("requires a delivery report"));
                assert!(output.delivery_required);
            }
        }
    }

    #[test]
    fn unauthenticated_contributions_block_completion_without_judgement_fallback() {
        let mut input = LoopWorkflowInput::new("implement".into(), "complete".into(), 2).unwrap();
        for judgement in ["", "bcode.fake-provider/fake-judgement/-/90/agent_fallback"] {
            // Configure fallback explicitly below; parsing is not part of this test.
            input.judgement_evaluation = if judgement.is_empty() {
                None
            } else {
                let mut config = parse_config("bcode.fake-provider/fake-judgement/-/90/pause")
                    .unwrap()
                    .unwrap();
                config.on_failure = FailurePolicy::AgentFallback;
                Some(config)
            };
            let mut state = loop_workflow_initial_value(&input);
            state.condition_met = true;
            state.evidence = vec!["claimed observed output".into()];
            state.delivery = Some(
                serde_json::from_value(serde_json::json!({
                    "version":"1", "integrated_targets":["checkout"],
                    "contribution_output_ids":["invented:output"],
                    "criteria":[{"criterion":"works", "status":"passed", "evidence":"claimed"}],
                    "checks":[], "retained_workspaces":[], "unresolved_work":[]
                }))
                .unwrap(),
            );
            let invocation = bcode_workflow::WorkflowBlockInvocation {
                version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                dispatch_identity: "dispatch".into(),
                workspace_root: std::env::temp_dir(),
                input: serde_json::to_value(&state).unwrap(),
                preparation: None,
            };
            let context = NativeServiceContext {
                plugin_id: PLUGIN_ID.into(),
                request: ServiceRequest {
                    interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.into(),
                    operation: OPERATION.into(),
                    payload: serde_json::to_vec(&invocation).unwrap(),
                },
                config: bcode_plugin_sdk::PluginConfigContext::default(),
                events: ServiceEventEmitter::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                bridge: ServiceBridge::default(),
                transient_progress_limits: TransientProgressLimits::default(),
            };
            let response = invoke(&context);
            assert!(response.error.is_none());
            let output: LoopWorkflowIteration = serde_json::from_slice(&response.payload).unwrap();
            assert!(!output.condition_met);
            assert!(output.summary.contains("could not be authenticated"));
            assert_eq!(output.delivery, state.delivery);
            assert_eq!(output.stop_condition, state.stop_condition);
        }
    }

    #[test]
    fn contribution_provenance_rejects_unknown_mismatched_and_adapted_producers() {
        let mut provenance: bcode_workflow::WorkflowOutputProvenance =
            serde_json::from_value(serde_json::json!({
                "version": 1, "producer_revision": 1,
                "producer": {
                    "id":"worker", "name":"Worker", "kind":"agent",
                    "input":{"type_name":"input", "schema":{}},
                    "output":{"type_name":"custom", "schema":{}}
                },
                "output": {
                    "version":1, "output_id":"output", "run_id":"run",
                    "node_id":"worker", "activation_id":"activation",
                    "schema_id":"custom", "schema_version":1,
                    "checksum_sha256":"a".repeat(64), "created_at_ms":1,
                    "value":{"blockers":[]}
                }
            }))
            .unwrap();
        assert!(valid_contribution_provenance(&provenance, "output"));
        assert!(!valid_contribution_provenance(&provenance, "other"));
        provenance.version = 2;
        assert!(!valid_contribution_provenance(&provenance, "output"));
        provenance.version = 1;
        provenance.producer_revision = 0;
        assert!(!valid_contribution_provenance(&provenance, "output"));
        provenance.producer_revision = 1;
        provenance.producer.id = "different".into();
        assert!(!valid_contribution_provenance(&provenance, "output"));
        provenance.producer.id = "worker".into();
        provenance.producer.dataflow = bcode_workflow::WorkflowNodeDataflowPolicy::StateEnvelopeV1;
        assert!(!valid_contribution_provenance(&provenance, "output"));
    }

    extern "C" fn authenticate_assertion(
        request_ptr: *const u8,
        request_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        let bytes = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
        let request: ServiceBridgeRequest = serde_json::from_slice(bytes).unwrap();
        let ServiceBridgeRequest::InvokeService(request) = request else {
            panic!("expected evidence request")
        };
        assert_eq!(
            request.operation,
            bcode_workflow::OP_INSPECT_OUTPUT_PROVENANCE
        );
        let response = ServiceBridgeResponse::Service(
            bcode_tool::ToolInvocationServiceResolution::Responded {
                payload: serde_json::json!({
                    "version": 1,
                    "producer_revision": 1,
                    "producer": {
                        "id": "worker", "name": "Worker", "kind": "agent",
                        "input": {"type_name":"input", "schema":{}},
                        "output": {"type_name":"custom", "schema":{}}
                    },
                    "output": {
                    "version": 1,
                    "output_id": "same-run:unrelated-output",
                    "run_id": "same-run", "node_id": "worker", "activation_id": "activation",
                    "schema_id": "custom", "schema_version": 1,
                    "value": {"claim": "passed"}, "created_at_ms": 1,
                    "checksum_sha256": "a".repeat(64)
                    }
                }),
            },
        );
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(encoded.len() <= output_capacity);
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
            *output_len = encoded.len();
        }
        0
    }
    #[test]
    fn explicit_resolution_cannot_hide_behind_unrecognized_contribution_schema() {
        let bridge = ServiceBridge::new(
            Some(authenticate_assertion),
            std::ptr::null_mut(),
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        let mut report: delivery::DeliveryReport = serde_json::from_value(serde_json::json!({
            "version":"3", "integrated_targets":["retained-export"],
            "contribution_output_ids":["same-run:unrelated-output"],
            "criteria":[], "checks":[], "unresolved_work":[], "retained_workspaces":[]
        }))
        .unwrap();
        // Custom output contracts remain authorable; absence of recognized negative
        // evidence is not a certificate, but does not itself reject provenance.
        assert!(authenticate_contributions(&bridge, "dispatch", &report));
        for checksum in ["a".repeat(64), "b".repeat(64)] {
            report.resolutions = vec![delivery::ContributionResolution {
                output_id: "same-run:unrelated-output".into(),
                checksum_sha256: checksum,
                item_paths: vec!["/blockers/0".into()],
                evidence: "Claimed historical resolution".into(),
                check_indices: vec![0],
            }];
            assert!(!authenticate_contributions(&bridge, "dispatch", &report));
            report.resolutions.push(report.resolutions[0].clone());
            assert!(!authenticate_contributions(&bridge, "dispatch", &report));
        }
    }

    #[test]
    fn authenticated_assertions_do_not_certify_delivery_or_dispatch_judgement() {
        for judgement in ["", "bcode.jev/jev-1.13.0/-/90/agent_fallback"] {
            for (workspace, condition_met) in [
                (None, false),
                (None, true),
                (Some("other-checkout"), false),
                (Some("other-checkout"), true),
                (Some("delivered-checkout"), false),
                (Some("delivered-checkout"), true),
            ] {
                let mut input =
                    LoopWorkflowInput::new("implement".into(), "original criteria".into(), 2)
                        .unwrap();
                input.judgement_evaluation = parse_config(judgement).unwrap();
                let mut state = loop_workflow_initial_value(&input);
                state.condition_met = condition_met;
                state.evidence = vec!["claimed successful verification".into()];
                let checks: Vec<_> = workspace.into_iter().map(|workspace| serde_json::json!({
                    "command": "cargo test", "workspace": workspace,
                    "outcome": "passed", "evidence": "claimed prior success; content may have changed"
                })).collect();
                state.delivery = Some(serde_json::from_value(serde_json::json!({
                    "version": "1", "integrated_targets": ["delivered-checkout"],
                    "contribution_output_ids": ["same-run:unrelated-output"],
                    "criteria": [{"criterion": "works", "status": "passed", "evidence": "claimed"}],
                    "checks": checks, "retained_workspaces": ["worker-checkout"], "unresolved_work": []
                })).unwrap());
                let invocation = bcode_workflow::WorkflowBlockInvocation {
                    version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                    dispatch_identity: "dispatch".into(),
                    workspace_root: std::env::temp_dir(),
                    input: serde_json::to_value(&state).unwrap(),
                    preparation: None,
                };
                let context = NativeServiceContext {
                    plugin_id: PLUGIN_ID.into(),
                    request: ServiceRequest {
                        interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.into(),
                        operation: OPERATION.into(),
                        payload: serde_json::to_vec(&invocation).unwrap(),
                    },
                    config: bcode_plugin_sdk::PluginConfigContext::default(),
                    events: ServiceEventEmitter::default(),
                    cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                    bridge: ServiceBridge::new(
                        Some(authenticate_assertion),
                        std::ptr::null_mut(),
                        bcode_plugin_sdk::ServiceCancellation::default(),
                    ),
                    transient_progress_limits: TransientProgressLimits::default(),
                };
                let response = invoke(&context);
                assert!(response.error.is_none());
                let output: LoopWorkflowIteration =
                    serde_json::from_slice(&response.payload).unwrap();
                assert!(!output.condition_met);
                assert!(
                    output
                        .summary
                        .contains("no target-bound observed verification")
                );
                assert_eq!(output.delivery, state.delivery);
                assert_eq!(output.stop_condition, state.stop_condition);
                assert_eq!(output.implementation_prompt, state.implementation_prompt);
            }
        }
    }

    #[test]
    fn referenced_check_requires_execution_service_without_falling_back_to_claims() {
        let check: delivery::Check = serde_json::from_value(serde_json::json!({
            "command":"cargo test", "workspace":"/workspace", "outcome":"passed",
            "evidence":"asserted", "execution": {
                "output_id":"output", "command_index":0, "argv":["cargo", "test"]
            }
        }))
        .unwrap();
        assert!(!authenticate_check(
            &ServiceBridge::default(),
            "dispatch",
            &check,
            check.execution.as_ref().unwrap()
        ));
    }

    #[test]
    fn judgement_config_requires_explicit_failure_and_threshold() {
        assert!(parse_config("").unwrap().is_none());
        let config = parse_config("bcode.jev/jev-1.13.0/-/90/pause")
            .unwrap()
            .unwrap();
        assert_eq!(config.auth_profile, "");
        assert_eq!(config.threshold_percent, 90);
        for invalid in [
            "bcode.jev/jev-1.13.0/-/0/pause",
            "bcode.jev/jev-1.13.0/-/101/pause",
            "bcode.jev/jev-1.13.0/-/90/ignore",
            "bcode.jev//-/90/pause",
        ] {
            assert!(parse_config(invalid).is_err());
        }
    }

    #[test]
    fn judgement_answer_above_and_below_threshold_controls_completion() {
        use std::ffi::c_void;
        extern "C" fn callback(
            request_ptr: *const u8,
            request_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
            user_data: *mut c_void,
        ) -> i32 {
            let bytes = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
            let request: ServiceBridgeRequest = serde_json::from_slice(bytes).unwrap();
            let ServiceBridgeRequest::InvokeService(request) = request else {
                panic!("expected judgement bridge request")
            };
            assert_eq!(request.invocation_id, "dispatch");
            assert_eq!(
                request.interface_id,
                judgement::WORKFLOW_APPLICATION_INTERFACE_ID
            );
            let percentage = unsafe { *user_data.cast::<u8>() };
            let result = ServiceBridgeResponse::Service(
                bcode_tool::ToolInvocationServiceResolution::Responded {
                    payload: serde_json::json!({
                        "answers": { "completion": {"kind": "yes_no", "probability": f64::from(percentage) / 100.0}},
                        "usage": null
                    }),
                },
            );
            let encoded = serde_json::to_vec(&result).unwrap();
            assert!(encoded.len() <= output_capacity);
            unsafe {
                std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
                *output_len = encoded.len();
            }
            0
        }
        let mut input = LoopWorkflowInput::new("implement".into(), "complete".into(), 2).unwrap();
        input.judgement_evaluation =
            parse_config("bcode.fake-provider/fake-judgement/-/90/pause").unwrap();
        let mut state = loop_workflow_initial_value(&input);
        state.evidence = vec!["test output reviewed".into()];
        for (agent_completed, percentage, expected) in [
            (false, 89_u8, false),
            (false, 90_u8, false),
            (false, 100_u8, false),
            (true, 89_u8, false),
            (true, 90_u8, true),
        ] {
            state.condition_met = agent_completed;
            let bridge = ServiceBridge::new(
                Some(callback),
                std::ptr::from_ref(&percentage).cast_mut().cast(),
                bcode_plugin_sdk::ServiceCancellation::default(),
            );
            let (completed, probability) = evaluate(&bridge, "dispatch", &state).unwrap();
            assert_eq!(completed, expected);
            assert!((probability - f64::from(percentage) / 100.0).abs() < f64::EPSILON);

            let invocation = bcode_workflow::WorkflowBlockInvocation {
                version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                dispatch_identity: "dispatch".into(),
                workspace_root: std::env::temp_dir(),
                input: serde_json::to_value(&state).unwrap(),
                preparation: None,
            };
            let context = NativeServiceContext {
                plugin_id: PLUGIN_ID.into(),
                request: ServiceRequest {
                    interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.into(),
                    operation: OPERATION.into(),
                    payload: serde_json::to_vec(&invocation).unwrap(),
                },
                config: bcode_plugin_sdk::PluginConfigContext::default(),
                events: ServiceEventEmitter::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                bridge,
                transient_progress_limits: TransientProgressLimits::default(),
            };
            let response = invoke(&context);
            assert!(response.error.is_none());
            let output: LoopWorkflowIteration = serde_json::from_slice(&response.payload).unwrap();
            assert_eq!(output.condition_met, expected);
            assert_eq!(output.stop_condition, state.stop_condition);
            assert_eq!(output.evidence, state.evidence);
            assert_eq!(output.delivery, state.delivery);
        }
    }

    #[test]
    fn delivery_bounds_are_enforced_at_the_service_boundary() {
        let input = LoopWorkflowInput::new("implement".into(), "complete".into(), 2).unwrap();
        let mut state = loop_workflow_initial_value(&input);
        state.condition_met = true;
        state.evidence = vec!["observed".into()];
        let valid = serde_json::json!({
            "version": "1", "integrated_targets": ["result.rs"],
            "contribution_output_ids": [],
            "criteria": [{"criterion": "complete", "status": "passed", "evidence": "observed"}],
            "checks": [], "retained_workspaces": [], "unresolved_work": []
        });
        for (field, replacement) in [
            ("integrated_targets", serde_json::json!(vec!["target"; 65])),
            (
                "contribution_output_ids",
                serde_json::json!(vec!["output"; 65]),
            ),
            (
                "retained_workspaces",
                serde_json::json!(vec!["workspace"; 65]),
            ),
            ("integrated_targets", serde_json::json!(["x".repeat(4097)])),
        ] {
            let mut report = valid.clone();
            report[field] = replacement;
            let mut value = serde_json::to_value(&state).unwrap();
            value["delivery"] = report;
            let invocation = bcode_workflow::WorkflowBlockInvocation {
                version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                dispatch_identity: "dispatch".into(),
                workspace_root: std::env::temp_dir(),
                input: value,
                preparation: None,
            };
            let context = NativeServiceContext {
                plugin_id: PLUGIN_ID.into(),
                request: ServiceRequest {
                    interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.into(),
                    operation: OPERATION.into(),
                    payload: serde_json::to_vec(&invocation).unwrap(),
                },
                config: bcode_plugin_sdk::PluginConfigContext::default(),
                events: ServiceEventEmitter::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                bridge: ServiceBridge::default(),
                transient_progress_limits: TransientProgressLimits::default(),
            };
            assert_eq!(
                invoke(&context).error.unwrap().code,
                "invalid_request",
                "{field}"
            );
        }
    }

    #[test]
    fn agent_only_completion_rejects_negative_delivery_without_provider_dispatch() {
        let input = LoopWorkflowInput::new("implement".into(), "complete".into(), 2).unwrap();
        for (status, target, evidence, expected) in [
            ("passed", "result.rs", vec!["observed".into()], false),
            ("failed", "result.rs", vec!["observed".into()], false),
            ("unverified", "result.rs", vec!["observed".into()], false),
            ("passed", " \t\n", vec!["observed".into()], false),
            ("passed", "result.rs", vec![], false),
            ("passed", "result.rs", vec!["\u{2003}".into()], false),
            (
                "passed",
                "result.rs",
                vec!["x".repeat(EVIDENCE_LIMIT + 1)],
                false,
            ),
        ] {
            let mut state = loop_workflow_initial_value(&input);
            state.condition_met = true;
            state.evidence = evidence;
            state.delivery = Some(
                serde_json::from_value(serde_json::json!({
                    "version":"1", "integrated_targets":[target],
                    "contribution_output_ids":[],
                    "criteria":[{"criterion":"works", "status":status, "evidence":"observed"}],
                    "checks":[], "retained_workspaces":[], "unresolved_work":[]
                }))
                .unwrap(),
            );
            let invocation = bcode_workflow::WorkflowBlockInvocation {
                version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                dispatch_identity: "agent-only".into(),
                workspace_root: std::env::temp_dir(),
                input: serde_json::to_value(&state).unwrap(),
                preparation: None,
            };
            let context = NativeServiceContext {
                plugin_id: PLUGIN_ID.into(),
                request: ServiceRequest {
                    interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.into(),
                    operation: OPERATION.into(),
                    payload: serde_json::to_vec(&invocation).unwrap(),
                },
                config: bcode_plugin_sdk::PluginConfigContext::default(),
                events: ServiceEventEmitter::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                bridge: ServiceBridge::default(),
                transient_progress_limits: TransientProgressLimits::default(),
            };
            let response = invoke(&context);
            assert!(response.error.is_none());
            let output: LoopWorkflowIteration = serde_json::from_slice(&response.payload).unwrap();
            assert_eq!(output.condition_met, expected);
            assert_eq!(output.delivery, state.delivery);
            assert!(output.judgement_evaluation.is_none());
        }
    }

    #[test]
    fn judgement_failure_pauses_or_retains_existing_agent_result() {
        let mut input = LoopWorkflowInput::new("implement".into(), "complete".into(), 2).unwrap();
        let mut config = parse_config("bcode.jev/jev-1.13.0/-/90/pause")
            .unwrap()
            .unwrap();
        input.judgement_evaluation = Some(config.clone());
        let mut state = loop_workflow_initial_value(&input);
        state.evidence = vec!["tests passed".into()];
        state.condition_met = true;
        state.summary = "agent completed".into();
        let invocation = bcode_workflow::WorkflowBlockInvocation {
            version: bcode_workflow::WorkflowBlockInvocation::VERSION,
            dispatch_identity: "dispatch".into(),
            workspace_root: std::env::temp_dir(),
            input: serde_json::to_value(&state).unwrap(),
            preparation: None,
        };
        let context = |invocation: &bcode_workflow::WorkflowBlockInvocation| NativeServiceContext {
            plugin_id: PLUGIN_ID.into(),
            request: ServiceRequest {
                interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.into(),
                operation: "loop.judgement.evaluate".into(),
                payload: serde_json::to_vec(invocation).unwrap(),
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: ServiceBridge::default(),
            transient_progress_limits: TransientProgressLimits::default(),
        };
        assert_eq!(
            invoke(&context(&invocation)).error.unwrap().code,
            "judgement_unavailable"
        );
        config.on_failure = FailurePolicy::AgentFallback;
        state.judgement_evaluation = Some(config);
        let fallback = invoke(&context(&bcode_workflow::WorkflowBlockInvocation {
            input: serde_json::to_value(state).unwrap(),
            ..invocation
        }));
        assert!(fallback.error.is_none());
        let output: LoopWorkflowIteration = serde_json::from_slice(&fallback.payload).unwrap();
        assert!(output.condition_met);
        assert!(output.summary.contains("agent evaluation retained"));
        // Neither an unavailable service nor fallback consent may turn explicit
        // negative delivery evidence into completion. Retain the report verbatim.
        for status in ["failed", "unverified"] {
            let mut contradicted = output.clone();
            contradicted.delivery = Some(serde_json::from_value(serde_json::json!({
                "version":"1", "integrated_targets":["integrated.sh"],
                "contribution_output_ids":[],
                "criteria":[{"criterion":"combined behavior", "status":status, "evidence":"not verified"}],
                "checks":[], "retained_workspaces":[], "unresolved_work":[]
            })).unwrap());
            let response = invoke(&context(&bcode_workflow::WorkflowBlockInvocation {
                version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                dispatch_identity: "dispatch".into(),
                workspace_root: std::env::temp_dir(),
                input: serde_json::to_value(&contradicted).unwrap(),
                preparation: None,
            }));
            assert!(response.error.is_none());
            let withheld: LoopWorkflowIteration =
                serde_json::from_slice(&response.payload).unwrap();
            assert!(!withheld.condition_met);
            assert_eq!(withheld.delivery, contradicted.delivery);
        }
        let mut empty = output;
        empty.evidence.clear();
        let missing = invoke(&context(&bcode_workflow::WorkflowBlockInvocation {
            version: bcode_workflow::WorkflowBlockInvocation::VERSION,
            dispatch_identity: "dispatch".into(),
            workspace_root: std::env::temp_dir(),
            input: serde_json::to_value(empty).unwrap(),
            preparation: None,
        }));
        assert_eq!(missing.error.unwrap().code, "judgement_unavailable");
    }
}
