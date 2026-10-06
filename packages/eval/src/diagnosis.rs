//! Deterministic failure labels from observations; not an LLM verdict.
use bcode_model::judgement::{
    Answer, Feature, OptionalRequest, OptionalResponse, Question, Request, State,
};
use serde_json::{Value, json};

const FAILURE_CATEGORIES: &[&str] = &[
    "execution_failure",
    "evidence_gap",
    "deadline",
    "artifact_failure",
    "preservation_failure",
    "completion_disagreement",
    "coordination",
    "uncertain",
];

/// Build one advisory request from measured failure facts, never raw output or prompts.
pub fn optional_request(oracle: &Value, diagnosis: &Value) -> Option<OptionalRequest> {
    if diagnosis["labels"].as_array()?.is_empty() {
        return None;
    }
    let mut facts = serde_json::Map::new();
    for key in [
        "capture_complete",
        "timed_out",
        "artifact_correct",
        "artifact_observed",
        "false_completion",
        "preservation_passed",
        "completion_claimed",
        "completion_failure",
    ] {
        if let Some(value) = oracle[key].as_bool() {
            facts.insert(key.to_owned(), json!(value));
        }
    }
    if let Some(status) = oracle["status"]
        .as_str()
        .filter(|status| ["completed", "failed", "cancelled", "unknown"].contains(status))
    {
        facts.insert("status".to_owned(), json!(status));
    }
    Some(OptionalRequest {
        feature: Feature::EvalCategorization,
        selection: None,
        request: Request {
            model_id: "auto".to_owned(),
            state: State::Structured(Value::Object(facts)),
            questions: std::collections::BTreeMap::from([("failure_category".to_owned(), Question::Choice {
                instructions: "Label these measured evaluation failure facts for advisory grouping only. Do not infer root cause or change measured outcomes; choose uncertain when evidence is insufficient.".to_owned(),
                options: FAILURE_CATEGORIES.iter().map(|label| ((*label).to_owned(), (*label).to_owned())).collect(),
            })]),
        },
    })
}

/// Keep model labels distinct from deterministic observations and oracle verdicts.
pub fn attach_advisory(diagnosis: &mut Value, query: &Request, result: OptionalResponse) {
    let Some(response) = result.response else {
        return;
    };
    // Provider capability and numeric validation belongs to central access; this consumer
    // checks exact caller-owned question identity, answer kind, and option cardinality.
    if query.questions.len() != 1 || response.answers.len() != 1 {
        return;
    }
    let Some(Question::Choice { options, .. }) = query.questions.get("failure_category") else {
        return;
    };
    let Some(Answer::Choice { selected, .. }) = response.answers.get("failure_category") else {
        return;
    };
    if !options.contains_key(selected) {
        return;
    }
    diagnosis["advisory_model_labels"] =
        json!({"failure_category":selected,"root_cause_verified":false});
}

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
    if oracle["artifact_correct"] == true
        && oracle["completion_claimed"] == false
        && !states.is_empty()
    {
        labels.push("independent_oracle_completion_disagreement");
    }
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
    fn representative_eval_cases_use_real_diagnosis_facts_without_overwriting_oracle() {
        use bcode_model::judgement::{Answer, OptionalResponse, Response};
        let suite = crate::quality::representative_quality_suite().unwrap();
        for case in suite
            .cases
            .iter()
            .filter(|case| case.feature == bcode_model::judgement::Feature::EvalCategorization)
        {
            let oracle = &case.input["oracle"];
            let mut diagnosis = super::classify(
                oracle,
                &serde_json::json!([]),
                &serde_json::json!({}),
                &std::collections::BTreeMap::new(),
            );
            let baseline = diagnosis.clone();
            let request = super::optional_request(oracle, &diagnosis)
                .expect("representative failure triggers actual categorization");
            assert_eq!(
                request.request.state,
                bcode_model::judgement::State::Structured(oracle.clone())
            );
            super::attach_advisory(
                &mut diagnosis,
                &request.request,
                OptionalResponse { response: None },
            );
            assert_eq!(diagnosis, baseline);
            let crate::quality::Expected::Labels { values } = &case.expected else {
                panic!("expected category")
            };
            let category = &values["failure_category"];
            // This checks consumer application of an injected answer, not model quality.
            super::attach_advisory(
                &mut diagnosis,
                &request.request,
                OptionalResponse {
                    response: Some(Response {
                        answers: std::collections::BTreeMap::from([(
                            "failure_category".into(),
                            Answer::Choice {
                                selected: category.clone(),
                                probabilities: None,
                                confidence: None,
                            },
                        )]),
                        usage: None,
                    }),
                },
            );
            assert_eq!(
                diagnosis["advisory_model_labels"]["failure_category"],
                category.as_str()
            );
            diagnosis
                .as_object_mut()
                .unwrap()
                .remove("advisory_model_labels");
            assert_eq!(diagnosis, baseline);
        }
    }

    #[test]
    fn advisory_labels_cannot_replace_measured_outcomes() {
        use bcode_model::judgement::{Answer, OptionalResponse, Response};
        let oracle = serde_json::json!({"status":"failed","artifact_correct":false,"capture_complete":true,"secret":"DO_NOT_SEND"});
        let mut diagnosis = super::classify(
            &oracle,
            &serde_json::json!([]),
            &serde_json::json!({}),
            &std::collections::BTreeMap::new(),
        );
        let baseline = diagnosis.clone();
        let optional = super::optional_request(&oracle, &diagnosis).unwrap();
        assert!(
            !serde_json::to_string(&optional)
                .unwrap()
                .contains("DO_NOT_SEND")
        );
        super::attach_advisory(
            &mut diagnosis,
            &optional.request,
            OptionalResponse { response: None },
        );
        assert_eq!(diagnosis, baseline);
        super::attach_advisory(
            &mut diagnosis,
            &optional.request,
            OptionalResponse {
                response: Some(Response {
                    answers: std::collections::BTreeMap::from([(
                        "failure_category".into(),
                        Answer::Choice {
                            selected: "evidence_gap".into(),
                            probabilities: None,
                            confidence: None,
                        },
                    )]),
                    usage: None,
                }),
            },
        );
        assert_eq!(diagnosis["labels"], baseline["labels"]);
        assert_eq!(diagnosis["root_cause_verified"], false);
        assert_eq!(
            diagnosis["advisory_model_labels"]["failure_category"],
            "evidence_gap"
        );
    }

    #[test]
    fn successful_baseline_has_no_categorization_trigger() {
        let oracle = serde_json::json!({"status":"completed","artifact_correct":true,"completion_claimed":true});
        let diagnosis = super::classify(
            &oracle,
            &serde_json::json!([]),
            &serde_json::json!({}),
            &std::collections::BTreeMap::new(),
        );
        assert!(super::optional_request(&oracle, &diagnosis).is_none());
    }

    #[test]
    fn advisory_question_kind_option_and_count_must_correlate() {
        use bcode_model::judgement::{Answer, OptionalResponse, Response};
        let oracle = serde_json::json!({"status":"failed"});
        let baseline = super::classify(
            &oracle,
            &serde_json::json!([]),
            &serde_json::json!({}),
            &std::collections::BTreeMap::new(),
        );
        let query = super::optional_request(&oracle, &baseline).unwrap().request;
        let choice = |selected: &str| Answer::Choice {
            selected: selected.into(),
            probabilities: None,
            confidence: None,
        };
        for answers in [
            std::collections::BTreeMap::from([("failure_category".into(), choice("unknown"))]),
            std::collections::BTreeMap::from([(
                "failure_category".into(),
                Answer::YesNo { probability: 0.5 },
            )]),
            std::collections::BTreeMap::from([("wrong_id".into(), choice("evidence_gap"))]),
            std::collections::BTreeMap::from([
                ("failure_category".into(), choice("evidence_gap")),
                ("extra".into(), choice("deadline")),
            ]),
        ] {
            let mut diagnosis = baseline.clone();
            super::attach_advisory(
                &mut diagnosis,
                &query,
                OptionalResponse {
                    response: Some(Response {
                        answers,
                        usage: None,
                    }),
                },
            );
            assert_eq!(diagnosis, baseline);
        }
    }

    #[test]
    fn malformed_advisory_does_not_change_baseline() {
        let oracle = serde_json::json!({"status":"failed"});
        let mut diagnosis = super::classify(
            &oracle,
            &serde_json::json!([]),
            &serde_json::json!({}),
            &std::collections::BTreeMap::new(),
        );
        let baseline = diagnosis.clone();
        let optional = super::optional_request(&oracle, &diagnosis).unwrap();
        super::attach_advisory(
            &mut diagnosis,
            &optional.request,
            bcode_model::judgement::OptionalResponse {
                response: Some(bcode_model::judgement::Response {
                    answers: std::collections::BTreeMap::new(),
                    usage: None,
                }),
            },
        );
        assert_eq!(diagnosis, baseline);
    }

    #[test]
    fn correct_artifact_with_withheld_completion_is_an_explicit_disagreement() {
        let result = super::classify(
            &serde_json::json!({"artifact_correct":true,"completion_claimed":false}),
            &serde_json::json!([{"node_id":"loop.judgement.evaluate","value":{"condition_met":false}}]),
            &serde_json::json!({}),
            &std::collections::BTreeMap::new(),
        );
        assert!(
            result["labels"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(
                    "independent_oracle_completion_disagreement"
                ))
        );
        assert_eq!(result["root_cause_verified"], false);
    }
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
