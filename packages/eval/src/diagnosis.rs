//! Deterministic failure labels from observations; not an LLM verdict.
use serde_json::{Value, json};
pub fn classify(
    oracle: &Value,
    outputs: &Value,
    metrics: &std::collections::BTreeMap<String, f64>,
) -> Value {
    let mut labels = Vec::new();
    if oracle["timed_out"] == true {
        labels.push("deadline_exhaustion");
    }
    if oracle["false_completion"] == true {
        labels.push("false_completion");
    }
    if oracle["preservation_passed"] == false {
        labels.push("preservation_violation");
    }
    if oracle["artifact_correct"] == false {
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
    json!({"schema_version":1,"labels":labels,"root_cause_verified":false,"judgements":states.iter().map(|state|json!({"condition_met":state["condition_met"],"summary":state["summary"]})).collect::<Vec<_>>()})
}
