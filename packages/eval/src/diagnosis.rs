//! Deterministic failure labels from observations; not an LLM verdict.
use serde_json::{Value, json};
pub fn classify(
    oracle: &Value,
    outputs: &Value,
    inspection: &Value,
    metrics: &std::collections::BTreeMap<String, f64>,
) -> Value {
    let mut labels = Vec::new();
    if oracle["capture_complete"] == false {
        labels.push("incomplete_execution_evidence");
    }
    if oracle["status"] == "failed" {
        labels.push("workflow_terminal_failure");
    }
    if oracle["timed_out"] == true {
        labels.push("deadline_exhaustion");
    }
    if oracle["false_completion"] == true {
        labels.push("false_completion");
    }
    if oracle["preservation_passed"] == false {
        labels.push("preservation_violation");
    }
    if oracle["artifact_observed"] == false {
        labels.push("delivered_artifact_unobservable");
    } else if oracle["artifact_correct"] == false {
        labels.push("delivered_artifact_incorrect_or_missing");
    }
    if oracle["completion_failure"] == true {
        labels.push("correct_artifact_not_terminalized");
    }
    let states: Vec<_> = outputs
        .as_array()
        .into_iter()
        .flatten()
        .filter(|value| value["node_id"] == "loop.judgement.evaluate")
        .filter_map(|value| value.get("value"))
        .collect();
    if states.iter().any(|state| state["condition_met"] == false) {
        labels.push("judgement_gate_withheld_completion");
    }
    if metrics
        .get("workflow.repeated_worker_activations")
        .is_some_and(|value| *value > 0.0)
    {
        labels.push("repeated_worker_activation_observed");
    }
    let attempts: Vec<_> = inspection["attempts"].as_array().into_iter().flatten()
        .map(|attempt| json!({"dispatch_identity":attempt["dispatch_identity"],"node_id":attempt["node_id"],"activation_id":attempt["activation_id"],"status":attempt["status"],"prepared_at_ms":attempt["prepared_at_ms"],"admitted_at_ms":attempt["admitted_at_ms"],"terminal_at_ms":attempt["terminal_at_ms"]})).collect();
    json!({"schema_version":2,"labels":labels,"root_cause_verified":false,"evidence":{"inspection":"inspection.json","outputs":"workflow-output.json","oracle":"oracle.json"},"attempts":attempts,"judgements":states.iter().map(|state|json!({"condition_met":state["condition_met"],"summary":state["summary"]})).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    #[test]
    fn absent_terminal_report_is_not_proof_of_incorrect_work() {
        let result = super::classify(
            &serde_json::json!({"artifact_correct":false,"artifact_observed":false,"status":"failed","capture_complete":false}),
            &serde_json::json!([]),
            &serde_json::json!({}),
            &std::collections::BTreeMap::new(),
        );
        let labels = result["labels"].as_array().unwrap();
        assert!(labels.contains(&serde_json::json!("delivered_artifact_unobservable")));
        assert!(!labels.contains(&serde_json::json!(
            "delivered_artifact_incorrect_or_missing"
        )));
        assert_eq!(result["root_cause_verified"], false);
    }
}
