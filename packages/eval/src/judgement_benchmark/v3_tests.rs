//! Diagnostics and exact v3 fixture conversion behavior.
use super::*;
use std::collections::BTreeMap;

#[test]
fn development_v3_maps_all_six_without_supplements_and_future_rejects() {
    let json = include_str!("../../../../fixtures/evals/judgement/development.json");
    let plan = parse_corpus_plan(
        json,
        "authored-synthetic-v3".into(),
        "diagnostic-v3".into(),
        1,
        super::tests::plan().gates,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(plan.tasks.len(), 36);
    for feature in FEATURES {
        assert_eq!(
            plan.tasks
                .iter()
                .filter(|task| task.feature == feature)
                .count(),
            6
        );
    }
    let source: serde_json::Value = serde_json::from_str(json).unwrap();
    for task in &plan.tasks {
        let expected = &source["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == task.task_id)
            .unwrap()["expected"];
        match &task.oracle {
            Oracle::Ranking {
                relevance,
                costs,
                budget,
                k,
                required,
                protected_positions,
                ideal_admission_gain,
            } => {
                let expected_relevance: BTreeMap<String, f64> =
                    serde_json::from_value(expected["relevance"].clone()).unwrap();
                assert_eq!(*relevance, expected_relevance);
                assert_eq!(serde_json::to_value(costs).unwrap(), expected["costs"]);
                assert_eq!(serde_json::json!(budget), expected["budget"]);
                assert_eq!(serde_json::json!(k), expected["k"]);
                assert_eq!(
                    serde_json::to_value(required).unwrap(),
                    expected["required"]
                );
                assert_eq!(
                    serde_json::to_value(protected_positions).unwrap(),
                    expected["protected_positions"]
                );
                assert!(
                    (*ideal_admission_gain - expected["ideal_admission_gain"].as_f64().unwrap())
                        .abs()
                        <= f64::EPSILON
                );
            }
            Oracle::Progress {
                label,
                onset_ms,
                deadline_ms,
            } => {
                assert_eq!(serde_json::to_value(label).unwrap(), expected["label"]);
                assert_eq!(serde_json::json!(onset_ms), expected["onset_ms"]);
                assert_eq!(serde_json::json!(deadline_ms), expected["deadline_ms"]);
            }
            Oracle::Classification { .. } => {}
        }
    }
    let mut future = source;
    future["schema_version"] = serde_json::json!(4);
    assert!(
        parse_corpus_plan(
            &future.to_string(),
            "future".into(),
            "future".into(),
            1,
            plan.gates,
            &BTreeMap::new()
        )
        .is_err()
    );
}
#[test]
fn confusion_preserves_abstentions_errors_and_unknown_denominators() {
    let plan = super::tests::plan();
    let task = &plan.tasks[0];
    let mut summary = ArmDiagnostics::new(Arm::On, 5);
    for label in [Some("correct"), Some("wrong"), None] {
        let mut row = super::tests::observation(Arm::On, "correct");
        row.output = Some(ConsumerOutput::Classification {
            label: label.map(str::to_owned),
        });
        summary.observe(task, &row);
    }
    summary.finish();
    assert_eq!(summary.evaluated, 3);
    assert_eq!(summary.abstentions, 1);
    assert_eq!(summary.confusion["correct"]["<abstain>"], 1);
    assert_eq!(summary.classes["correct"].support, 3);
    assert_eq!(summary.classes["correct"].true_positive, 1);
    assert_eq!(summary.classes["correct"].precision, Some(1.0));
    assert_eq!(summary.classes["correct"].recall, Some(1.0 / 3.0));
    assert_eq!(summary.macro_f1, Some(0.5));
    assert_eq!(summary.evaluation_coverage, Some(0.6));
    let mut empty = ArmDiagnostics::new(Arm::Off, 2);
    empty.finish();
    assert_eq!(empty.macro_f1, None);
    assert_eq!(empty.prediction_coverage, None);
    assert_eq!(empty.evaluation_coverage, Some(0.0));
}
#[test]
fn progress_false_alerts_and_missed_detection_keep_raw_counts() {
    let mut task = super::tests::plan().tasks.remove(0);
    task.oracle = Oracle::Progress {
        label: Some("stall".into()),
        onset_ms: 100,
        deadline_ms: 200,
    };
    let mut summary = ArmDiagnostics::new(Arm::On, 4);
    for (label, detected_ms) in [
        (Some("stall"), Some(150)),
        (Some("stall"), Some(99)),
        (None, None),
    ] {
        let mut row = super::tests::observation(Arm::On, "correct");
        row.output = Some(ConsumerOutput::Progress {
            label: label.map(str::to_owned),
            detected_ms,
        });
        summary.observe(&task, &row);
    }
    task.oracle = Oracle::Progress {
        label: None,
        onset_ms: 0,
        deadline_ms: 200,
    };
    let mut row = super::tests::observation(Arm::On, "correct");
    row.output = Some(ConsumerOutput::Progress {
        label: Some("stall".into()),
        detected_ms: Some(50),
    });
    summary.observe(&task, &row);
    summary.finish();
    assert_eq!(summary.positive_events, 3);
    assert_eq!(summary.negative_events, 1);
    assert_eq!(summary.detections, 1);
    assert_eq!(summary.false_alerts, 2);
    assert_eq!(summary.missed_events, 2);
    assert_eq!(summary.detection_delays_ms, vec![50]);
}
#[test]
fn ranking_components_are_exposed_independently() {
    let mut task = super::tests::plan().tasks.remove(0);
    task.oracle = Oracle::Ranking {
        relevance: BTreeMap::from([("a".into(), 3.0), ("b".into(), 1.0)]),
        k: 1,
        costs: BTreeMap::from([("a".into(), 1), ("b".into(), 1)]),
        budget: 1,
        required: vec![],
        protected_positions: BTreeMap::new(),
        ideal_admission_gain: 7.0,
    };
    let row = super::tests::observation(Arm::On, "correct");
    let output = ConsumerOutput::Ranking {
        order: vec!["a".into(), "b".into()],
        admitted: vec!["b".into()],
    };
    let metrics = super::diagnostics::ranking_metrics(&task, &row, &output).unwrap();
    assert!((metrics.ndcg - 1.0).abs() <= f64::EPSILON);
    assert_eq!(metrics.recall_at_k, Some(0.5));
    assert!((metrics.admission_score - 1.0 / 7.0).abs() <= f64::EPSILON);
}
