//! Offline comparator behavior only: synthetic captures are not measured live gains.
use bcode_eval::judgement_benchmark::effectiveness::{
    DatasetSplit, EffectivenessIdentity, EffectivenessObservation, EffectivenessPlan,
    EffectivenessTask, compare_effectiveness,
};
use bcode_eval::judgement_benchmark::{Arm, EvidenceSource, EvidenceStatus, Verdict};
use bcode_model::judgement::Feature;
use std::collections::{BTreeMap, BTreeSet};

fn fixture() -> (EffectivenessPlan, Vec<EffectivenessObservation>) {
    let mut plan = EffectivenessPlan {
        schema_version: 1,
        preregistration_id: "registration".into(),
        dataset_id: "frozen-corpus".into(),
        split: DatasetSplit::HeldOut,
        tasks: Vec::new(),
        minimum_families: 5,
        minimum_gain: 0.1,
        maximum_extra_cost_usd: 0.02,
        maximum_extra_latency_ms: 100.0,
    };
    let mut rows = Vec::new();
    for index in 0..5 {
        let task_id = format!("task-{index}");
        plan.tasks.push(EffectivenessTask {
            task_id: task_id.clone(),
            family_id: format!("family-{index}"),
            feature: Feature::ContextReranking,
            pairing_id: format!("pair-{index}"),
            oracle_id: "frozen-command-tests".into(),
            test_ids: BTreeSet::from(["correctness".into()]),
            repetitions: 1,
            requires_workflow_dispatch: false,
        });
        for arm in [Arm::Off, Arm::On] {
            let on = arm == Arm::On;
            rows.push(EffectivenessObservation {
                schema_version: 1,
                preregistration_id: plan.preregistration_id.clone(),
                dataset_id: plan.dataset_id.clone(),
                split: plan.split,
                task_id: task_id.clone(),
                repetition: 0,
                arm,
                capture_id: format!("capture-{index}-{arm:?}"),
                pairing_id: format!("pair-{index}"),
                identity: EffectivenessIdentity {
                    artifact_id: "subject".into(),
                    base_model_id: "provider/model".into(),
                    budget_id: "equal-budget".into(),
                    settings_id: "equal-input-settings".into(),
                },
                source: EvidenceSource::OfflineConsumer,
                oracle_id: "frozen-command-tests".into(),
                tests: BTreeMap::from([("correctness".into(), Some(on))]),
                independent_tests: EvidenceStatus::Confirmed,
                terminal_observed: EvidenceStatus::Confirmed,
                isolated_feature: EvidenceStatus::Confirmed,
                safety_preserved: EvidenceStatus::Confirmed,
                trigger_count: Some(1),
                application_count: Some(u64::from(on)),
                discovery_calls: Some(u64::from(on)),
                verification_calls: Some(0),
                inference_calls: Some(u64::from(on)),
                ordinary_admission: EvidenceStatus::Confirmed,
                base_context_consumed: EvidenceStatus::Confirmed,
                workflow_dispatched: EvidenceStatus::Unknown,
                billing_complete: EvidenceStatus::Confirmed,
                total_cost_usd: Some(if on { 0.11 } else { 0.10 }),
                latency_ms: Some(if on { 110.0 } else { 100.0 }),
            });
        }
    }
    (plan, rows)
}

#[test]
fn independent_task_wins_are_descriptive_not_live_evidence() {
    let (plan, rows) = fixture();
    let report = compare_effectiveness(&plan, &rows).unwrap();
    let feature = &report.features[1];
    assert_eq!(feature.pairs.wins, 5);
    assert_eq!(feature.families.wins, 5);
    assert_eq!(feature.family_mean_delta, Some(1.0));
    assert_eq!(feature.verdict, Verdict::Improved);
    assert!(!feature.promotion_eligible);
    assert!(feature.gaps.iter().any(|gap| gap.contains("offline")));
    assert_eq!(report.features[0].verdict, Verdict::Inconclusive);
    assert_eq!(report.plan.dataset_id, plan.dataset_id);
}

#[test]
fn broken_off_calls_missing_application_oracle_rewrite_and_pair_mismatch_are_inconclusive() {
    let (plan, rows) = fixture();
    let mutations: [fn(&mut Vec<EffectivenessObservation>); 10] = [
        |rows| rows[0].inference_calls = Some(1),
        |rows| rows[0].verification_calls = Some(1),
        |rows| rows[0].discovery_calls = Some(1),
        |rows| rows[1].application_count = None,
        |rows| rows[1].oracle_id = "rewritten-oracle".into(),
        |rows| rows[1].pairing_id = "other-task".into(),
        |rows| rows[1].identity.budget_id = "larger-budget".into(),
        |rows| rows[1].base_context_consumed = EvidenceStatus::Unknown,
        |rows| {
            rows[1].tests.insert("correctness".into(), None);
        },
        |rows| rows[1].trigger_count = None,
    ];
    for mutate in mutations {
        let mut rows = rows.clone();
        mutate(&mut rows);
        let report = compare_effectiveness(&plan, &rows).unwrap();
        let feature = &report.features[1];
        assert_eq!(feature.pairs.inconclusive, 1);
        assert_eq!(feature.verdict, Verdict::Inconclusive);
        assert!(!feature.promotion_eligible);
    }
}

#[test]
fn claimed_live_heldout_evidence_cannot_authenticate_or_promote() {
    let (mut plan, mut rows) = fixture();
    // Simulated statuses exercise the gate only; they do not establish real live proof.
    for row in &mut rows {
        row.source = EvidenceSource::LiveConsumer;
    }
    let claimed = compare_effectiveness(&plan, &rows).unwrap();
    assert_eq!(claimed.evidence_scope, bcode_eval::judgement_benchmark::effectiveness::EffectivenessEvidenceScope::CallerDeclarationsOnly);
    assert!(
        claimed.features[1]
            .gaps
            .iter()
            .any(|gap| gap.contains("unauthenticated"))
    );
    assert!(!claimed.features[1].promotion_eligible);
    rows[1].total_cost_usd = None;
    assert!(!compare_effectiveness(&plan, &rows).unwrap().features[1].promotion_eligible);
    rows[1].total_cost_usd = Some(0.11);
    rows[1].billing_complete = EvidenceStatus::Unknown;
    assert!(!compare_effectiveness(&plan, &rows).unwrap().features[1].promotion_eligible);
    rows[1].billing_complete = EvidenceStatus::Confirmed;
    rows[1].total_cost_usd = Some(1.0);
    assert!(!compare_effectiveness(&plan, &rows).unwrap().features[1].promotion_eligible);
    rows[1].total_cost_usd = Some(0.11);
    rows[1].latency_ms = None;
    assert!(!compare_effectiveness(&plan, &rows).unwrap().features[1].promotion_eligible);
    rows[1].latency_ms = Some(110.0);
    plan.tasks[0].requires_workflow_dispatch = true;
    assert!(!compare_effectiveness(&plan, &rows).unwrap().features[1].promotion_eligible);
    plan.tasks[0].requires_workflow_dispatch = false;
    plan.split = DatasetSplit::Development;
    for row in &mut rows {
        row.split = DatasetSplit::Development;
    }
    assert!(!compare_effectiveness(&plan, &rows).unwrap().features[1].promotion_eligible);
}

#[test]
fn seeds_and_permutations_are_one_family_and_regressions_are_not_hidden() {
    let (mut plan, mut rows) = fixture();
    for task in &mut plan.tasks {
        task.family_id = "original-task".into();
    }
    let feature = &compare_effectiveness(&plan, &rows).unwrap().features[1];
    assert_eq!(feature.families.wins, 1);
    assert_eq!(feature.verdict, Verdict::Inconclusive);
    for (index, task) in plan.tasks.iter_mut().enumerate() {
        task.family_id = format!("family-{index}");
    }
    rows[0].tests.insert("correctness".into(), Some(true));
    rows[1].tests.insert("correctness".into(), Some(false));
    let feature = &compare_effectiveness(&plan, &rows).unwrap().features[1];
    assert_eq!(feature.pairs.wins, 4);
    assert_eq!(feature.pairs.losses, 1);
    assert_eq!(feature.verdict, Verdict::Tradeoff);
    assert!(!feature.promotion_eligible);
}

#[test]
fn missing_and_duplicate_arms_do_not_shrink_schedule_and_future_versions_fail() {
    let (plan, mut rows) = fixture();
    rows.pop();
    let feature = &compare_effectiveness(&plan, &rows).unwrap().features[1];
    assert_eq!(feature.pairs.inconclusive, 1);
    assert_eq!(feature.pairs.wins + feature.pairs.inconclusive, 5);
    let mut duplicate = rows[0].clone();
    duplicate.capture_id = "second-off".into();
    rows.push(duplicate);
    assert_eq!(
        compare_effectiveness(&plan, &rows).unwrap().features[1]
            .pairs
            .inconclusive,
        2
    );
    rows[0].schema_version = 2;
    assert!(compare_effectiveness(&plan, &rows).is_err());
    let mut value = serde_json::to_value(&plan).unwrap();
    value["new_unrecognized_fact"] = serde_json::json!(true);
    assert!(serde_json::from_value::<EffectivenessPlan>(value).is_err());
}

#[test]
fn repeat_count_is_not_an_independent_family_count_and_order_is_irrelevant() {
    let (mut plan, rows) = fixture();
    plan.tasks.truncate(1);
    plan.tasks[0].repetitions = 10;
    let mut repeated = Vec::new();
    for repetition in 0..10 {
        for source in &rows[..2] {
            let mut row = source.clone();
            row.repetition = repetition;
            row.capture_id = format!("{}-{repetition}", row.capture_id);
            repeated.push(row);
        }
    }
    let report = compare_effectiveness(&plan, &repeated).unwrap();
    assert_eq!(report.features[1].pairs.wins, 10);
    assert_eq!(report.features[1].families.wins, 1);
    assert_eq!(report.features[1].verdict, Verdict::Inconclusive);
    repeated.reverse();
    assert_eq!(
        serde_json::to_value(report).unwrap(),
        serde_json::to_value(compare_effectiveness(&plan, &repeated).unwrap()).unwrap()
    );
}
