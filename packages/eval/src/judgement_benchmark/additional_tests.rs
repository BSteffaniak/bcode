#[test]
fn six_features_never_pool_quality_or_resource_evidence() {
    let mut plan = plan();
    plan.tasks.clear();
    for (index, feature) in FEATURES.into_iter().enumerate() {
        let oracle = match feature {
            Feature::ContextReranking | Feature::Recommendations => ranking(),
            Feature::ProgressMonitoring => Oracle::Progress {
                label: None,
                onset_ms: 0,
                deadline_ms: 100,
            },
            _ => classification(),
        };
        plan.tasks.push(BenchmarkTask {
            task_id: format!("feature-{index}"),
            feature,
            repetitions: 1,
            oracle,
        });
    }
    let mut off = observation(Arm::Off, "wrong");
    off.task_id = "feature-0".into();
    let mut on = observation(Arm::On, "correct");
    on.task_id = "feature-0".into();
    let result = compare_benchmark(&plan, &[off, on]).unwrap();
    assert_eq!(result.features[0].verdict, Verdict::Inconclusive);
    for feature in &result.features[1..] {
        assert_eq!(feature.verdict, Verdict::Inconclusive);
        assert_eq!(feature.quality.mean_delta, None);
        assert_eq!(feature.scheduled_arms, 2);
    }
}

#[test]
fn invalid_numbers_and_infeasible_oracles_never_become_zero_cost() {
    let plan = plan();
    let mut on = observation(Arm::On, "correct");
    on.cost_usd = Some(f64::NAN);
    on.latency_ms = Some(-1.0);
    let result = report(&plan, &[observation(Arm::Off, "wrong"), on]);
    assert_eq!(result.cost_usd.pairs, 0);
    assert_eq!(result.latency_ms.pairs, 0);
    assert_eq!(result.verdict, Verdict::Inconclusive);
    let mut oracle = ranking();
    if let Oracle::Ranking { budget, .. } = &mut oracle {
        *budget = 1;
    }
    assert!(!score_output(&oracle, &ranked(&["a", "b", "c"], &["a"])).constraints_passed);
}
