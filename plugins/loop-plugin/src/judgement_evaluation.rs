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
        ("planning_ready", current("planning_ready")),
        ("judgement_evaluation", pinned("judgement_evaluation")),
        ("iteration", current("iteration")),
        ("condition_met", current("condition_met")),
        ("evidence", current("evidence")),
        ("summary", current("summary")),
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
    let Ok(mut input) = invocation.typed_input::<LoopWorkflowIteration>() else {
        return ServiceResponse::error("invalid_request", "invalid loop evaluation input");
    };
    let Some(config) = input.judgement_evaluation.as_ref() else {
        return ServiceResponse::error("invalid_request", "judgement configuration is required");
    };
    if let Err(message) = config.validate() {
        return ServiceResponse::error("invalid_request", message);
    }
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("cancelled", "loop evaluation cancelled");
    }
    let config = config.clone();
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
        *probability >= f64::from(config.threshold_percent) / 100.0,
        *probability,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for (percentage, expected) in [(89_u8, false), (90_u8, true)] {
            let bridge = ServiceBridge::new(
                Some(callback),
                std::ptr::from_ref(&percentage).cast_mut().cast(),
                bcode_plugin_sdk::ServiceCancellation::default(),
            );
            let (completed, probability) = evaluate(&bridge, "dispatch", &state).unwrap();
            assert_eq!(completed, expected);
            assert!((probability - f64::from(percentage) / 100.0).abs() < f64::EPSILON);
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
