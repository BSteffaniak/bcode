//! Offline eval pipeline tests: real invocation encoding and downstream application, no services.
use bcode_eval::diagnosis::{EvalAttemptJudgementObservation, classify, invoke_attempt};
use bcode_eval::judgement_capture::CaptureTelemetry;
use bcode_model::judgement::{
    ApplicationDisposition, OptionalOperationObservation, OptionalOperationOutcome,
    OptionalRequest, OptionalResponse, TriggerDisposition,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn eval_pipeline_disabled_call_is_observed_not_inferred_and_oracle_is_unchanged() {
    let oracle = json!({"status":"failed","artifact_correct":false,"capture_complete":true});
    let oracle_bytes = serde_json::to_vec(&oracle).unwrap();
    let measurements = BTreeMap::from([("oracle_passed".to_owned(), 0.0)]);
    let measurements_before = measurements.clone();
    let mut diagnosis = classify(&oracle, &json!([]), &json!({}), &measurements);
    let baseline = diagnosis.clone();
    let operation = invoke_attempt(&oracle, &mut diagnosis, |args| {
        assert_eq!(
            &args[..3],
            ["judgement", "optional-judge", "--request-json"]
        );
        let query: OptionalRequest = serde_json::from_str(&args[3]).unwrap();
        assert_eq!(
            query.feature,
            bcode_model::judgement::Feature::EvalCategorization
        );
        Some(
            serde_json::to_value(OptionalResponse {
                response: None,
                observation: Some(OptionalOperationObservation {
                    outcome: OptionalOperationOutcome::Disabled,
                    ..Default::default()
                }),
            })
            .unwrap(),
        )
    });
    assert_eq!(operation.trigger, TriggerDisposition::Triggered);
    assert_eq!(
        operation.application,
        Some(ApplicationDisposition::BaselineRetained)
    );
    let observation =
        EvalAttemptJudgementObservation::new("actual-run".into(), "failed", operation);
    observation.validate().unwrap();
    assert!(observation.complete);
    let mut telemetry = CaptureTelemetry::default();
    telemetry.retain_eval_observation(&observation).unwrap();
    assert_eq!(telemetry.inference_calls, Some(0));
    assert_eq!(telemetry.total_cost_usd(), None);
    assert_eq!(diagnosis, baseline);
    assert_eq!(serde_json::to_vec(&oracle).unwrap(), oracle_bytes);
    assert_eq!(measurements, measurements_before);
}

#[test]
fn missing_telemetry_is_unknown_even_after_terminal_fallback() {
    let oracle = json!({"status":"failed"});
    let mut diagnosis = classify(&oracle, &json!([]), &json!({}), &BTreeMap::new());
    let operation = invoke_attempt(&oracle, &mut diagnosis, |_| Some(json!({"response":null})));
    let observation =
        EvalAttemptJudgementObservation::new("actual-run".into(), "failed", operation);
    observation.validate().unwrap();
    assert!(!observation.complete);
    let mut telemetry = CaptureTelemetry::default();
    telemetry.retain_eval_observation(&observation).unwrap();
    assert_eq!(telemetry.inference_calls, None);
    assert_eq!(telemetry.total_cost_usd(), None);
}

#[test]
fn absent_failure_prerequisite_does_not_invoke_and_unresolved_status_is_not_terminal() {
    let oracle = json!({"status":"completed","artifact_correct":true});
    let mut diagnosis = classify(&oracle, &json!([]), &json!({}), &BTreeMap::new());
    let operation = invoke_attempt(&oracle, &mut diagnosis, |_| {
        panic!("ineligible attempt invoked")
    });
    assert_eq!(operation.trigger, TriggerDisposition::NotApplicable);
    let mut observation =
        EvalAttemptJudgementObservation::new("actual-run".into(), "running", operation);
    observation.validate().unwrap();
    assert!(!observation.complete);
    observation.schema_version = 2;
    assert!(observation.validate().is_err());
}

#[test]
fn valid_advice_is_acknowledged_only_after_eval_application() {
    let oracle = json!({"status":"failed"});
    let before = serde_json::to_vec(&oracle).unwrap();
    let mut diagnosis = classify(&oracle, &json!([]), &json!({}), &BTreeMap::new());
    let baseline = diagnosis.clone();
    let operation = invoke_attempt(&oracle, &mut diagnosis, |_| {
        Some(json!({
            "response":{"answers":{"failure_category":{"kind":"choice","selected":"execution_failure","probabilities":null,"confidence":null}},"usage":null},
            "observation":OptionalOperationObservation {
                outcome: OptionalOperationOutcome::Advice,
                inference_invocations: 1,
                ..Default::default()
            }
        }))
    });
    assert_eq!(operation.application, Some(ApplicationDisposition::Applied));
    assert_eq!(
        diagnosis["advisory_model_labels"]["failure_category"],
        "execution_failure"
    );
    assert_eq!(
        diagnosis["advisory_model_labels"]["root_cause_verified"],
        false
    );
    diagnosis
        .as_object_mut()
        .unwrap()
        .remove("advisory_model_labels");
    assert_eq!(diagnosis, baseline);
    assert_eq!(serde_json::to_vec(&oracle).unwrap(), before);
}

#[test]
fn central_advice_is_not_application_when_downstream_answer_is_invalid() {
    let oracle = json!({"status":"failed"});
    let mut diagnosis = classify(&oracle, &json!([]), &json!({}), &BTreeMap::new());
    let baseline = diagnosis.clone();
    let operation = invoke_attempt(&oracle, &mut diagnosis, |_| {
        Some(json!({
            "response":{"answers":{},"usage":null},
            "observation":OptionalOperationObservation {
                outcome: OptionalOperationOutcome::Advice,
                inference_invocations: 1,
                ..Default::default()
            }
        }))
    });
    assert_eq!(
        operation.application,
        Some(ApplicationDisposition::BaselineRetained)
    );
    assert_eq!(diagnosis, baseline);
    let observation =
        EvalAttemptJudgementObservation::new("actual-run".into(), "failed", operation);
    observation.validate().unwrap();
    assert!(observation.complete); // Complete observation of fallback, not applied advice.
    assert_eq!(
        observation.operation.application,
        Some(ApplicationDisposition::BaselineRetained)
    );
}
