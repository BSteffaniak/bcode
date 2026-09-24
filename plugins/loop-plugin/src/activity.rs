//! Read-only loop activity projection; execution state is never modified here.

use std::fmt::Write as _;

use bcode_session_models::{
    ACTIVITY_PRESENTATION_VERSION, ActivityPresentation, ActivityProjectionRequest,
};

use crate::{LoopWorkflowInput, LoopWorkflowIteration, PLUGIN_ID};

fn preview(value: &str) -> String {
    // Semantic preview budget, not terminal-cell measurement. Preserve UTF-8 boundaries.
    if value.len() <= 512 {
        return value.to_owned();
    }
    let mut end = 512;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn activity_heading(
    initialization: bool,
    state: &LoopWorkflowIteration,
    phase: &str,
    prompt: &str,
    stop: &str,
) -> String {
    if phase == "workflow_completed" {
        return completion_text(state, &preview(&state.summary));
    }
    if initialization {
        format!(
            "Goal initialization · Researching and preparing progress document\nPrompt: {prompt}\nStop when: {stop}"
        )
    } else {
        format!(
            "Loop · Iteration {} of {} · {phase}\nPrompt: {prompt}\nStop when: {stop}",
            state.iteration, state.max_iterations
        )
    }
}

fn validate_stage(stage: &str) -> Result<(), String> {
    if matches!(
        stage,
        "initialization"
            | "initialization_complete"
            | "implementation"
            | "implementation_complete"
            | "evaluation"
            | "evaluation_complete"
            | "workflow_completed"
    ) {
        Ok(())
    } else {
        Err("unsupported loop activity stage".into())
    }
}

fn completion_text(state: &LoopWorkflowIteration, summary: &str) -> String {
    let outcome = if state.condition_met {
        "Goal completed — evaluator marked the stop condition satisfied."
    } else if state.iteration >= state.max_iterations {
        "Goal stopped — iteration allowance exhausted; stop condition not satisfied."
    } else {
        "Goal stopped — evaluator did not mark the stop condition satisfied."
    };
    format!(
        "{outcome}\nIteration {} of {}.\nEvaluator summary: {summary}\nThis is the evaluator's judgment, not independent proof of correctness.\nUse /goal.status to inspect the run; evaluation evidence is in the preceding transcript.",
        state.iteration, state.max_iterations
    )
}

pub fn project(request: ActivityProjectionRequest) -> Result<ActivityPresentation, String> {
    validate_stage(&request.stage)?;
    // Bound encoded details independently of previews, including JSON escaping. Retain the
    // original value rather than reserializing the typed state, which could drop extra fields.
    let encoded_input = serde_json::to_vec(&request.input).map_err(|error| error.to_string())?;
    let exact_input = (encoded_input.len() <= 16_384).then(|| request.input.clone());
    let previews_only = exact_input.is_none();
    let state: LoopWorkflowIteration =
        serde_json::from_value(request.input).map_err(|error| error.to_string())?;
    LoopWorkflowInput::new(
        state.implementation_prompt.clone(),
        state.stop_condition.clone(),
        u64::from(state.max_iterations),
    )?;
    if state.iteration == 0 || state.iteration > state.max_iterations {
        return Err("invalid loop iteration ordinal".into());
    }
    let prompt = preview(&state.implementation_prompt);
    let stop_condition = preview(&state.stop_condition);
    let summary = preview(&state.summary);
    let evidence: Vec<_> = state
        .evidence
        .iter()
        .take(8)
        .map(|item| preview(item))
        .collect();
    let initialization = request.stage.starts_with("initialization");
    let mut fallback = activity_heading(
        initialization,
        &state,
        &request.stage,
        &prompt,
        &stop_condition,
    );
    let evaluation_complete = request.stage == "evaluation_complete";
    if !summary.trim().is_empty() && request.stage != "workflow_completed" {
        let label = if evaluation_complete {
            "Evaluation summary"
        } else {
            "Previous evaluation summary"
        };
        let _ = write!(fallback, "\n{label}: {summary}");
    }
    if evaluation_complete {
        let result = if state.condition_met {
            "condition met"
        } else {
            "condition not met"
        };
        let _ = write!(
            fallback,
            "\nEvaluator reported: {result} (not workflow control state)"
        );
    }
    let mut presentation = ActivityPresentation {
        version: ACTIVITY_PRESENTATION_VERSION,
        producer: PLUGIN_ID.into(),
        activity_id: if initialization {
            "initialization".into()
        } else {
            format!("iteration:{}", state.iteration)
        },
        revision: request.revision,
        schema: "bcode.loop.iteration".into(),
        schema_version: 1,
        fallback,
        payload: serde_json::json!({
            "iteration": state.iteration,
            "limit": state.max_iterations,
            "stage": request.stage,
            "prompt_preview": prompt,
            "stop_condition_preview": stop_condition,
            "previous_summary_preview": summary,
            "previous_evidence_previews": evidence,
            "previous_evidence_count": state.evidence.len(),
            "previous_evidence_omitted": state.evidence.len().saturating_sub(8),
            "previews_only": previews_only,
            "exact_structured_input": exact_input,
            "structured_input_bytes": encoded_input.len(),
            "details_unavailable_reason": previews_only.then_some("structured input exceeds inline detail budget")
        }),
    };
    if evaluation_complete {
        let payload = presentation
            .payload
            .as_object_mut()
            .expect("object payload");
        for (previous, current) in [
            ("previous_summary_preview", "evaluation_summary_preview"),
            ("previous_evidence_previews", "evaluation_evidence_previews"),
            ("previous_evidence_count", "evaluation_evidence_count"),
            ("previous_evidence_omitted", "evaluation_evidence_omitted"),
        ] {
            if let Some(value) = payload.remove(previous) {
                payload.insert(current.into(), value);
            }
        }
        payload.insert("evaluator_condition_met".into(), state.condition_met.into());
    }
    presentation.validate(PLUGIN_ID).map_err(str::to_owned)?;
    Ok(presentation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_presentation_distinguishes_approval_from_exhaustion() {
        let mut input = request("workflow_completed");
        input.input["condition_met"] = true.into();
        let approved = project(input.clone()).expect("approved presentation");
        assert!(
            approved
                .fallback
                .contains("evaluator marked the stop condition satisfied")
        );
        assert!(approved.fallback.contains("Iteration 2 of 10"));
        assert!(approved.fallback.contains("not independent proof"));
        input.input["condition_met"] = false.into();
        input.input["iteration"] = 10.into();
        let exhausted = project(input).expect("exhausted presentation");
        assert!(exhausted.fallback.contains("iteration allowance exhausted"));
        assert!(exhausted.fallback.contains("stop condition not satisfied"));
    }

    fn request(stage: &str) -> ActivityProjectionRequest {
        ActivityProjectionRequest {
            stage: stage.into(),
            revision: 1,
            input: serde_json::json!({
                "implementation_prompt": "implement", "stop_condition": "tests pass",
                "max_iterations": 10, "iteration": 2, "condition_met": false,
                "summary": "remaining work", "evidence": []
            }),
        }
    }

    fn invoke(payload: Vec<u8>, operation: &str) -> bcode_plugin_sdk::ServiceResponse {
        use bcode_plugin_sdk::RustPlugin as _;
        crate::LoopPlugin.invoke_service(bcode_plugin_sdk::NativeServiceContext {
            plugin_id: PLUGIN_ID.into(),
            request: bcode_plugin_sdk::ServiceRequest {
                interface_id: bcode_session_models::ACTIVITY_PRESENTATION_INTERFACE_ID.into(),
                operation: operation.into(),
                payload,
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: bcode_plugin_sdk::ServiceBridge::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        })
    }

    #[test]
    fn initialization_has_separate_identity_and_does_not_claim_iteration_completion() {
        let view = project(request("initialization")).unwrap();
        let iteration = project(request("implementation")).unwrap();
        assert_ne!(view.activity_id, iteration.activity_id);
        assert!(view.fallback.contains("Researching"));
        assert!(view.payload.get("condition_met").is_none());
    }

    #[test]
    fn service_routes_projection_and_rejects_invalid_requests() {
        let payload = serde_json::to_vec(&request("implementation")).unwrap();
        let response = invoke(payload.clone(), bcode_session_models::OP_PROJECT_ACTIVITY);
        assert!(response.error.is_none());
        let view: ActivityPresentation = serde_json::from_slice(&response.payload).unwrap();
        assert_eq!(view.producer, PLUGIN_ID);
        assert_eq!(view.payload["iteration"], 2);
        assert!(invoke(payload, "unknown").error.is_some());
        assert!(
            invoke(
                b"not json".to_vec(),
                bcode_session_models::OP_PROJECT_ACTIVITY
            )
            .error
            .is_some()
        );
        let oversized = vec![b' '; bcode_session_models::MAX_ACTIVITY_PROJECTION_REQUEST_BYTES + 1];
        let response = invoke(oversized, bcode_session_models::OP_PROJECT_ACTIVITY);
        assert_eq!(response.error.unwrap().code, "invalid_request");
    }

    #[test]
    fn stages_share_iteration_identity_without_mutating_input() {
        let implementation = request("implementation");
        let original = implementation.clone();
        let view = project(implementation.clone()).unwrap();
        let evaluation = project(request("evaluation")).unwrap();
        assert_eq!(implementation, original);
        assert_eq!(view.activity_id, evaluation.activity_id);
        assert_ne!(view.payload["stage"], evaluation.payload["stage"]);
        assert!(view.fallback.contains("Iteration 2 of 10"));
    }

    #[test]
    fn exact_details_preserve_input_and_disclose_oversized_omission() {
        let mut input = request("implementation");
        input.input["extension"] = serde_json::json!({"literal": "<user>🦀</user>"});
        let original = input.input.clone();
        let view = project(input).unwrap();
        assert_eq!(view.payload["exact_structured_input"], original);
        assert_eq!(view.payload["previews_only"], false);
        assert!(view.payload["details_unavailable_reason"].is_null());
        assert!(view.validate(PLUGIN_ID).is_ok());

        let mut input = request("evaluation");
        input.input["implementation_prompt"] = serde_json::json!("work\n".repeat(4096));
        let view = project(input).unwrap();
        assert!(view.payload["exact_structured_input"].is_null());
        assert_eq!(view.payload["previews_only"], true);
        assert!(view.payload["structured_input_bytes"].as_u64().unwrap() > 16_384);
        assert!(view.payload["details_unavailable_reason"].is_string());
        assert!(view.validate(PLUGIN_ID).is_ok());
    }

    #[test]
    fn completion_boundaries_preserve_identity_and_label_current_evaluation() {
        let implementing = project(request("implementation")).unwrap();
        let implemented = project(request("implementation_complete")).unwrap();
        assert_eq!(implementing.activity_id, implemented.activity_id);
        assert!(implemented.payload.get("evaluator_condition_met").is_none());
        for met in [false, true] {
            let mut input = request("evaluation_complete");
            input.revision = 4;
            input.input["condition_met"] = met.into();
            let evaluated = project(input).unwrap();
            assert_eq!(evaluated.activity_id, implementing.activity_id);
            assert_eq!(evaluated.revision, 4);
            assert_eq!(evaluated.payload["evaluator_condition_met"], met);
            assert_eq!(
                evaluated.payload["evaluation_summary_preview"],
                "remaining work"
            );
            assert!(evaluated.payload.get("previous_summary_preview").is_none());
            assert!(evaluated.payload.get("outcome").is_none());
            assert!(evaluated.fallback.contains("Evaluation summary:"));
            assert!(!evaluated.fallback.contains("Previous evaluation summary:"));
        }
    }

    #[test]
    fn previous_evaluation_is_bounded_and_not_a_current_outcome() {
        let mut input = request("evaluation");
        input.input["evidence"] = serde_json::json!(vec!["evidence".repeat(200); 12]);
        input.input["condition_met"] = serde_json::json!(true);
        let view = project(input).unwrap();
        assert!(
            view.fallback
                .contains("Previous evaluation summary: remaining work")
        );
        assert_eq!(view.payload["previous_evidence_count"], 12);
        assert_eq!(view.payload["previous_evidence_omitted"], 4);
        let evidence = view.payload["previous_evidence_previews"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), 8);
        assert!(
            evidence
                .iter()
                .all(|item| item.as_str().unwrap().len() <= 515)
        );
        assert!(view.payload.get("condition_met").is_none());
        assert!(view.payload.get("outcome").is_none());
        assert!(view.validate(PLUGIN_ID).is_ok());
    }

    #[test]
    fn rejects_invalid_stage_and_ordinal_and_bounds_unicode_preview() {
        assert!(project(request("success")).is_err());
        let mut invalid = request("implementation");
        invalid.input["iteration"] = serde_json::json!(0);
        assert!(project(invalid).is_err());
        let mut large = request("implementation");
        large.input["implementation_prompt"] = serde_json::json!("🦀".repeat(1000));
        let view = project(large).unwrap();
        assert!(view.payload["prompt_preview"].as_str().unwrap().len() <= 515);
        assert!(view.payload.get("condition_met").is_none());
    }
}
