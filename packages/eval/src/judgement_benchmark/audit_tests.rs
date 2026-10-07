//! Independent counterexample families reserved before feature implementation inspection.
use super::*;
use std::collections::BTreeMap;

#[test]
fn independent_public_corpus_maps_reserved_cases_without_supplements() {
    let plan = parse_corpus_plan(
        include_str!("../../../../fixtures/evals/judgement/independent-public.json"),
        "public-independent-synthetic-audit-20261007-r1".into(),
        "public-audit-calibration-not-blinded".into(),
        1,
        super::tests::plan().gates,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(plan.tasks.len(), 8);
    assert!(validate_service_oracle_vocabulary(&plan).is_ok());
    assert_eq!(
        plan.tasks
            .iter()
            .filter(|task| matches!(task.oracle, Oracle::Progress { label: None, .. }))
            .count(),
        2
    );
}

#[test]
fn protected_low_relevance_cannot_be_dropped_for_optional_gain() {
    let oracle = Oracle::Ranking {
        relevance: BTreeMap::from([("policy".into(), 0.0), ("answer".into(), 4.0)]),
        k: 2,
        costs: BTreeMap::from([("policy".into(), 1), ("answer".into(), 1)]),
        budget: 2,
        ideal_admission_gain: 15.0,
        required: vec!["policy".into()],
        protected_positions: BTreeMap::from([("policy".into(), 0)]),
    };
    for (order, admitted) in [
        (vec!["policy", "answer"], vec!["answer"]),
        (vec!["answer", "policy"], vec!["policy", "answer"]),
    ] {
        let output = ConsumerOutput::Ranking {
            order: order.into_iter().map(str::to_owned).collect(),
            admitted: admitted.into_iter().map(str::to_owned).collect(),
        };
        let score = score_output(&oracle, &output);
        assert!(score.quality.abs() < f64::EPSILON);
        assert!(!score.constraints_passed);
    }
    let valid = ConsumerOutput::Ranking {
        order: vec!["policy".into(), "answer".into()],
        admitted: vec!["policy".into(), "answer".into()],
    };
    assert!(score_output(&oracle, &valid).constraints_passed);
    // Residual audit gap: ranking's ideal DCG ignores protected positions, so
    // the best permissible order receives less than 1. No policy change here.
    assert!(score_output(&oracle, &valid).quality < 1.0);
}

#[test]
fn equal_relevance_does_not_authorize_over_budget_admission() {
    let oracle = Oracle::Ranking {
        relevance: BTreeMap::from([("large".into(), 3.0), ("small".into(), 3.0)]),
        k: 2,
        costs: BTreeMap::from([("large".into(), 2), ("small".into(), 1)]),
        budget: 1,
        ideal_admission_gain: 7.0,
        required: vec![],
        protected_positions: BTreeMap::new(),
    };
    let output = |admitted: &str| ConsumerOutput::Ranking {
        order: vec!["large".into(), "small".into()],
        admitted: vec![admitted.into()],
    };
    let unsafe_score = score_output(&oracle, &output("large"));
    assert!(unsafe_score.quality.abs() < f64::EPSILON);
    assert!(!unsafe_score.constraints_passed);
    assert!((score_output(&oracle, &output("small")).quality - 1.0).abs() < f64::EPSILON);
}

#[test]
fn completed_progress_trace_must_not_accept_stale_advice() {
    // Independent consumer expectation after verified completion, not model output.
    let oracle = Oracle::Progress {
        label: None,
        onset_ms: 0,
        deadline_ms: 200,
    };
    let stale = ConsumerOutput::Progress {
        label: Some("loop".into()),
        detected_ms: Some(150),
    };
    let scored = score_output(&oracle, &stale);
    assert!(scored.quality.abs() < f64::EPSILON);
    assert!(!scored.constraints_passed);
    assert_eq!(scored.detection_delay_ms, None);
}
