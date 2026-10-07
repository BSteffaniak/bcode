//! Calibration tests validate scorers and evidence gates, not provider improvement.
use super::*;
use bcode_model::judgement::Feature;
use std::collections::BTreeMap;

include!("additional_tests.rs");

fn classification() -> Oracle {
    Oracle::Classification {
        labels: BTreeMap::from([("correct".into(), 1.0), ("wrong".into(), 0.0)]),
        abstention_credit: 0.25,
    }
}
pub(super) fn plan() -> BenchmarkPlan {
    BenchmarkPlan {
        schema_version: 1,
        oracle_id: "independent-review-v1".into(),
        preregistration_id: "frozen-v1".into(),
        tasks: vec![BenchmarkTask {
            task_id: "task".into(),
            feature: Feature::ToolResultTriage,
            repetitions: 1,
            oracle: classification(),
        }],
        gates: Gates {
            bootstrap: BootstrapSettings::default(),
            feature_limits: FEATURES
                .into_iter()
                .map(|feature| FeatureLimits {
                    feature,
                    quality_gain: 0.1,
                    quality_loss: 0.05,
                    latency_increase_ms: 10.0,
                    cost_increase_usd: 0.01,
                    reliability_loss: 0.0,
                })
                .collect(),
            minimum_tasks: 1,
            minimum_completeness: 1.0,
            quality_gain: 0.1,
            quality_loss: 0.05,
            latency_increase_ms: 10.0,
            cost_increase_usd: 0.01,
            reliability_loss: 0.0,
        },
    }
}
pub(super) fn observation(arm: Arm, label: &str) -> BenchmarkObservation {
    BenchmarkObservation {
        schema_version: 1,
        preregistration_id: "frozen-v1".into(),
        task_id: "task".into(),
        repetition: 0,
        arm,
        evidence: ConsumerEvidence {
            source: EvidenceSource::LiveConsumer,
            operation: "bounded-public-consumer".into(),
            capture_id: format!("capture-{arm:?}"),
            consumer_executed: EvidenceStatus::Confirmed,
            isolated_feature: EvidenceStatus::Confirmed,
            pairing_id: "same-input-settings".into(),
            discovery_calls: 0,
            verification_calls: 0,
            inference_calls: u64::from(arm == Arm::On),
            authority_preserved: EvidenceStatus::Confirmed,
            request_only: EvidenceStatus::Confirmed,
        },
        outcome: TerminalOutcome::Succeeded,
        output: Some(ConsumerOutput::Classification {
            label: Some(label.into()),
        }),
        latency_ms: Some(100.0),
        cost_usd: Some(0.02),
    }
}
fn report(plan: &BenchmarkPlan, rows: &[BenchmarkObservation]) -> FeatureReport {
    compare_benchmark(plan, rows).unwrap().features.remove(0)
}
fn near(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn classification_and_abstention_are_independently_calibrated() {
    let oracle = classification();
    near(
        score_output(&oracle, &ConsumerOutput::Classification { label: None }).quality,
        0.25,
    );
    near(
        score_output(
            &oracle,
            &ConsumerOutput::Classification {
                label: Some("correct".into()),
            },
        )
        .quality,
        1.0,
    );
    let unknown = score_output(
        &oracle,
        &ConsumerOutput::Classification {
            label: Some("fabricated".into()),
        },
    );
    near(unknown.quality, 0.0);
    assert!(!unknown.constraints_passed);
    let mut invalid = classification();
    if let Oracle::Classification {
        abstention_credit, ..
    } = &mut invalid
    {
        *abstention_credit = f64::NAN;
    }
    assert!(
        !score_output(&invalid, &ConsumerOutput::Classification { label: None }).constraints_passed
    );
}

fn ranking() -> Oracle {
    Oracle::Ranking {
        relevance: BTreeMap::from([("a".into(), 3.0), ("b".into(), 1.0), ("c".into(), 1.0)]),
        k: 3,
        costs: BTreeMap::from([("a".into(), 2), ("b".into(), 1), ("c".into(), 1)]),
        budget: 3,
        ideal_admission_gain: 8.0,
        required: vec!["a".into()],
        protected_positions: BTreeMap::new(),
    }
}
fn ranked(order: &[&str], admitted: &[&str]) -> ConsumerOutput {
    ConsumerOutput::Ranking {
        order: order.iter().map(|id| (*id).into()).collect(),
        admitted: admitted.iter().map(|id| (*id).into()).collect(),
    }
}
#[test]
fn graded_ranking_ties_and_constrained_admission() {
    let oracle = ranking();
    near(
        score_output(&oracle, &ranked(&["a", "b", "c"], &["a", "b"])).quality,
        1.0,
    );
    near(
        score_output(&oracle, &ranked(&["a", "c", "b"], &["a", "c"])).quality,
        1.0,
    );
    let reversed = score_output(&oracle, &ranked(&["b", "c", "a"], &["a", "b"]));
    assert!(reversed.quality < 1.0 && reversed.quality > 0.5);
    let inferior_admission = score_output(&oracle, &ranked(&["a", "b", "c"], &["a"]));
    assert!(inferior_admission.quality < 1.0);
    for output in [
        ranked(&["a", "a", "c"], &["a"]),
        ranked(&["a", "b", "c"], &["a", "b", "c"]),
        ranked(&["a", "b", "c"], &["b"]),
        ranked(&["a", "b", "c"], &["a", "unknown"]),
    ] {
        let score = score_output(&oracle, &output);
        near(score.quality, 0.0);
        assert!(!score.constraints_passed);
    }
    let mut protected = oracle;
    if let Oracle::Ranking {
        protected_positions,
        ..
    } = &mut protected
    {
        protected_positions.insert("b".into(), 1);
    }
    assert!(!score_output(&protected, &ranked(&["a", "c", "b"], &["a", "c"])).constraints_passed);
}

#[test]
fn progress_penalizes_false_early_late_and_missing_advice() {
    let oracle = Oracle::Progress {
        label: Some("stall".into()),
        onset_ms: 100,
        deadline_ms: 200,
    };
    let output = |label: Option<&str>, time| ConsumerOutput::Progress {
        label: label.map(str::to_owned),
        detected_ms: time,
    };
    let instant = score_output(&oracle, &output(Some("stall"), Some(100)));
    near(instant.quality, 1.0);
    assert_eq!(instant.detection_delay_ms, Some(0));
    let delayed = score_output(&oracle, &output(Some("stall"), Some(150)));
    near(delayed.quality, 0.5);
    assert_eq!(delayed.detection_delay_ms, Some(50));
    for value in [
        output(Some("stall"), Some(99)),
        output(Some("stall"), Some(201)),
        output(Some("drift"), Some(100)),
        output(None, None),
    ] {
        near(score_output(&oracle, &value).quality, 0.0);
    }
    let negative = Oracle::Progress {
        label: None,
        onset_ms: 0,
        deadline_ms: 200,
    };
    near(score_output(&negative, &output(None, None)).quality, 1.0);
    near(
        score_output(&negative, &output(Some("stall"), Some(100))).quality,
        0.0,
    );
}

#[test]
fn no_capture_or_scorer_answer_can_claim_live_improvement() {
    let plan = plan();
    let missing = compare_benchmark(&plan, &[]).unwrap();
    assert_eq!(missing.features.len(), 6);
    assert_eq!(missing.features[0].scheduled_arms, 2);
    assert_eq!(missing.features[0].quality.mean_delta, None);
    assert_eq!(missing.features[0].verdict, Verdict::Inconclusive);
    assert_eq!(missing.features[1].completeness, None);
    for source in [
        EvidenceSource::ScorerCalibration,
        EvidenceSource::OfflineConsumer,
    ] {
        let mut rows = vec![
            observation(Arm::Off, "wrong"),
            observation(Arm::On, "correct"),
        ];
        for row in &mut rows {
            row.evidence.source = source;
        }
        assert_eq!(report(&plan, &rows).quality.pairs, 0);
    }
}

#[test]
fn preregistered_gates_retain_unknown_pricing_and_resource_tradeoffs() {
    let plan = plan();
    let mut rows = vec![
        observation(Arm::Off, "wrong"),
        observation(Arm::On, "correct"),
    ];
    assert_eq!(report(&plan, &rows).verdict, Verdict::Inconclusive);
    rows[1].cost_usd = None;
    let unknown = report(&plan, &rows);
    assert_eq!(unknown.verdict, Verdict::Inconclusive);
    assert_eq!(unknown.cost_usd.mean_delta, None);
    rows[1].cost_usd = Some(0.5);
    assert_eq!(report(&plan, &rows).verdict, Verdict::Inconclusive);
    rows[0].output = rows[1].output.clone();
    assert_eq!(report(&plan, &rows).verdict, Verdict::Inconclusive);
    rows[1].cost_usd = Some(0.02);
    assert_eq!(report(&plan, &rows).verdict, Verdict::Inconclusive);
}

#[test]
fn duplicates_missing_failed_and_unexecuted_treatments_keep_schedule() {
    let plan = plan();
    let off = observation(Arm::Off, "wrong");
    let on = observation(Arm::On, "correct");
    let duplicate = report(&plan, &[off.clone(), on.clone(), on.clone()]);
    assert_eq!(duplicate.scheduled_pairs, 1);
    assert_eq!(duplicate.observed_arms, 1);
    assert_eq!(duplicate.quality.pairs, 0);
    let mut failed = on.clone();
    failed.outcome = TerminalOutcome::TimedOut;
    failed.output = None;
    let failure = report(&plan, &[off.clone(), failed]);
    near(failure.reliability.mean_delta.unwrap(), -1.0);
    assert_eq!(failure.quality.pairs, 0);
    assert_eq!(failure.successful_arms, 1);
    assert_eq!(failure.verdict, Verdict::Regressed);
    let mut not_executed = on.clone();
    not_executed.evidence.consumer_executed = EvidenceStatus::Rejected;
    assert_eq!(report(&plan, &[off.clone(), not_executed]).quality.pairs, 0);
    let mut unknown = on.clone();
    unknown.evidence.consumer_executed = EvidenceStatus::Unknown;
    assert_eq!(report(&plan, &[off.clone(), unknown]).quality.pairs, 0);
    let mut not_inferred = on.clone();
    not_inferred.evidence.inference_calls = 0;
    assert_eq!(report(&plan, &[off.clone(), not_inferred]).quality.pairs, 0);
    let mut contaminated = off.clone();
    contaminated.evidence.discovery_calls = 1;
    assert_eq!(report(&plan, &[contaminated, on.clone()]).live_arms, 1);
    let mut mismatched = on;
    mismatched.evidence.pairing_id = "different-input".into();
    assert_eq!(report(&plan, &[off, mismatched]).reliability.pairs, 0);
}

#[test]
fn repeats_are_clustered_by_task_not_independent_samples() {
    let mut plan = plan();
    plan.tasks[0].repetitions = 10;
    let mut second = plan.tasks[0].clone();
    second.task_id = "other-task".into();
    second.repetitions = 1;
    plan.tasks.push(second);
    let mut rows = Vec::new();
    for repetition in 0..10 {
        for arm in [Arm::Off, Arm::On] {
            let mut row = observation(arm, if arm == Arm::On { "correct" } else { "wrong" });
            row.repetition = repetition;
            row.evidence.capture_id = format!("{arm:?}-{repetition}");
            rows.push(row);
        }
    }
    for arm in [Arm::Off, Arm::On] {
        let mut row = observation(arm, if arm == Arm::Off { "correct" } else { "wrong" });
        row.task_id = "other-task".into();
        rows.push(row);
    }
    let result = report(&plan, &rows);
    assert_eq!(result.quality.pairs, 11);
    assert_eq!(result.quality.tasks, 2);
    near(result.quality.mean_delta.unwrap(), 0.0);
    assert_eq!(result.quality.task_delta_range, Some([-1.0, 1.0]));
}

#[test]
fn unsupported_versions_unknown_fields_and_feature_mixing_fail_closed() {
    let mut plan = plan();
    plan.schema_version = 2;
    assert!(compare_benchmark(&plan, &[]).is_err());
    plan.schema_version = 1;
    plan.tasks[0].feature = Feature::Recommendations;
    assert!(compare_benchmark(&plan, &[]).is_err());
    let mut encoded = serde_json::to_value(observation(Arm::Off, "wrong")).unwrap();
    encoded["future_field"] = serde_json::json!(true);
    assert!(serde_json::from_value::<BenchmarkObservation>(encoded).is_err());
}
