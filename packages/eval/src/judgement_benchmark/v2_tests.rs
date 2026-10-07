//! Metrics-v2 independent-cluster gate and corpus mapping calibration.
use super::tests::{observation, plan};
use super::*;
use std::collections::BTreeMap;

fn independent(count: usize, repetitions: u32) -> (BenchmarkPlan, Vec<BenchmarkObservation>) {
    let mut plan = plan();
    let original = plan.tasks[0].clone();
    plan.tasks.clear();
    let mut rows = Vec::new();
    for index in 0..count {
        let mut task = original.clone();
        task.task_id = format!("independent-{index}");
        task.repetitions = repetitions;
        for repetition in 0..repetitions {
            for arm in [Arm::Off, Arm::On] {
                let mut row = observation(arm, if arm == Arm::Off { "wrong" } else { "correct" });
                row.task_id = task.task_id.clone();
                row.repetition = repetition;
                row.evidence.capture_id = format!("{index}-{repetition}-{arm:?}");
                rows.push(row);
            }
        }
        plan.tasks.push(task);
    }
    (plan, rows)
}
#[test]
fn repeats_do_not_create_confidence_but_independent_tasks_can() {
    let (plan, rows) = independent(1, 100);
    let repeated = compare_benchmark(&plan, &rows).unwrap();
    assert_eq!(repeated.features[0].quality.tasks, 1);
    assert_eq!(repeated.features[0].quality.confidence_interval, None);
    assert_eq!(repeated.features[0].verdict, Verdict::Inconclusive);
    let (plan, rows) = independent(5, 1);
    let independent = compare_benchmark(&plan, &rows).unwrap();
    assert_eq!(
        independent.features[0].quality.confidence_interval,
        Some([1.0, 1.0])
    );
    assert_eq!(independent.features[0].verdict, Verdict::Improved);
    assert_eq!(independent.schema_version, 3);
    assert_eq!(
        serde_json::to_value(&independent).unwrap(),
        serde_json::to_value(compare_benchmark(&plan, &rows).unwrap()).unwrap()
    );
}
#[test]
fn lower_quality_bound_and_feature_limits_are_required() {
    let (mut plan, mut rows) = independent(5, 1);
    plan.gates.feature_limits.clear();
    assert_eq!(
        compare_benchmark(&plan, &rows).unwrap().features[0].verdict,
        Verdict::Inconclusive
    );
    plan.gates.feature_limits = super::tests::plan().gates.feature_limits;
    for row in &mut rows {
        if row.task_id == "independent-0" {
            row.output = Some(ConsumerOutput::Classification {
                label: Some(
                    if row.arm == Arm::Off {
                        "correct"
                    } else {
                        "wrong"
                    }
                    .into(),
                ),
            });
        }
    }
    let mixed = compare_benchmark(&plan, &rows).unwrap();
    assert!(mixed.features[0].quality.mean_delta.unwrap() > 0.1);
    assert!(mixed.features[0].quality.confidence_interval.unwrap()[0] < 0.1);
    assert_eq!(mixed.features[0].verdict, Verdict::Inconclusive);
}
#[test]
fn independent_quality_can_reveal_preregistered_resource_tradeoff() {
    let (plan, mut rows) = independent(5, 1);
    for row in &mut rows {
        if row.arm == Arm::On {
            row.cost_usd = Some(0.5);
        }
    }
    assert_eq!(
        compare_benchmark(&plan, &rows).unwrap().features[0].verdict,
        Verdict::Tradeoff
    );
    rows[1].cost_usd = None;
    assert_eq!(
        compare_benchmark(&plan, &rows).unwrap().features[0].verdict,
        Verdict::Inconclusive
    );
}
#[test]
fn actual_corpus_reports_mapping_gaps_instead_of_inventing_truth() {
    let json = include_str!("../../../../fixtures/evals/judgement/held-out.json");
    let error = parse_corpus_plan(
        json,
        "authored-freeze-pending-review".into(),
        "frozen-v2".into(),
        1,
        plan().gates,
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(error.contains("ranking lacks graded relevance"), "{error}");
    let mut corpus: serde_json::Value = serde_json::from_str(json).unwrap();
    corpus["cases"].as_array_mut().unwrap().retain(|case| {
        case["feature"] == "tool_result_triage"
            || case["feature"] == "session_categorization"
            || case["feature"] == "eval_categorization"
    });
    let exact = parse_corpus_plan(
        &corpus.to_string(),
        "authored-freeze-pending-review".into(),
        "frozen-v2".into(),
        1,
        plan().gates,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(exact.tasks.len(), 6);
    let session = exact
        .tasks
        .iter()
        .find(|task| task.task_id == "held-session-documentation")
        .unwrap();
    let Oracle::Classification { labels, .. } = &session.oracle else {
        panic!("classification expected")
    };
    assert_eq!(labels.get("documentation/frontend"), Some(&1.0));
    assert_eq!(
        compare_benchmark(&exact, &[]).unwrap().features[0].verdict,
        Verdict::Inconclusive
    );
}
