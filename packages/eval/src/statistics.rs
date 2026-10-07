//! Descriptive uncertainty; scheduled outcomes and evaluable quality use distinct denominators.
use serde_json::{Value, json};

pub fn wilson(successes: usize, total: usize) -> Option<[f64; 2]> {
    if total == 0 || successes > total {
        return None;
    }
    let n = total as f64;
    let p = successes as f64 / n;
    let z = 1.959_963_984_540_054;
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let radius = z * ((p.mul_add(1.0 - p, z * z / (4.0 * n))) / n).sqrt() / denominator;
    Some([center - radius, center + radius])
}

fn rate(successes: usize, total: usize) -> Option<f64> {
    (total > 0).then(|| successes as f64 / total as f64)
}

fn outcome_rates(pairs: &[&Value], field: &str) -> Value {
    let count = |outcome: &str| pairs.iter().filter(|pair| pair[field] == outcome).count();
    json!({
        "passed": rate(count("passed"), pairs.len()),
        "failed": rate(count("failed"), pairs.len()),
        "infrastructure_failure": rate(count("infrastructure_failure"), pairs.len()),
        "missing": rate(count("missing"), pairs.len()),
        "ambiguous_duplicate": rate(count("ambiguous_duplicate"), pairs.len()),
    })
}

fn candidate_summary(id: &str, pairs: &[&Value]) -> Value {
    let count =
        |field: &str, value: &Value| pairs.iter().filter(|pair| pair[field] == *value).count();
    let known_schedule = pairs.iter().all(|pair| pair["schedule_known"] == true);
    let scheduled: Vec<_> = pairs
        .iter()
        .copied()
        .filter(|pair| pair["scheduled"] == true)
        .collect();
    let scheduled_successes = scheduled
        .iter()
        .filter(|pair| pair["candidate_outcome"] == "passed")
        .count();
    let baseline_scheduled_successes = scheduled
        .iter()
        .filter(|pair| pair["baseline_outcome"] == "passed")
        .count();
    let evaluable: Vec<_> = pairs
        .iter()
        .copied()
        .filter(|pair| pair["quality_evaluable"] == true)
        .collect();
    let successes = evaluable
        .iter()
        .filter(|pair| pair["candidate_passed"] == true)
        .count();
    let baseline_passes = evaluable
        .iter()
        .filter(|pair| pair["baseline_passed"] == true)
        .count();
    let regressions = count("quality_regression", &json!(true));
    let mut ratios: Vec<_> = pairs
        .iter()
        .filter(|pair| pair["speed_eligible"] == true)
        .filter_map(|pair| pair["latency_ratio"].as_f64())
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect();
    ratios.sort_by(f64::total_cmp);
    let quantile = |percent: usize| {
        ratios
            .get(
                ratios
                    .len()
                    .saturating_mul(percent)
                    .div_ceil(100)
                    .saturating_sub(1),
            )
            .copied()
    };
    json!({
        "candidate": id,
        "scheduled_trials": known_schedule.then_some(scheduled.len()),
        "retained_identity_trials": pairs.len(),
        "unexpected_identity_trials": if known_schedule { pairs.len() - scheduled.len() } else { 0 },
        "observed_trials": count("candidate_observed", &json!(true)),
        "baseline_observed_trials": count("baseline_observed", &json!(true)),
        "evaluable_trials": evaluable.len(),
        "trials": evaluable.len(),
        "unevaluable_pairs": pairs.len() - evaluable.len(),
        "candidate_passed_trials": count("candidate_outcome", &json!("passed")),
        "candidate_failed_trials": count("candidate_outcome", &json!("failed")),
        "candidate_infrastructure_failures": count("candidate_outcome", &json!("infrastructure_failure")),
        "candidate_missing_trials": count("candidate_outcome", &json!("missing")),
        "candidate_ambiguous_trials": count("candidate_outcome", &json!("ambiguous_duplicate")),
        "baseline_failed_trials": count("baseline_outcome", &json!("failed")),
        "baseline_infrastructure_failures": count("baseline_outcome", &json!("infrastructure_failure")),
        "baseline_missing_trials": count("baseline_outcome", &json!("missing")),
        "baseline_ambiguous_trials": count("baseline_outcome", &json!("ambiguous_duplicate")),
        "baseline_all_attempt_success_rate": known_schedule.then(|| rate(baseline_scheduled_successes, scheduled.len())).flatten(),
        "all_attempt_outcome_rates": known_schedule.then(|| outcome_rates(&scheduled, "candidate_outcome")),
        "baseline_all_attempt_outcome_rates": known_schedule.then(|| outcome_rates(&scheduled, "baseline_outcome")),
        "all_attempt_success_rate": known_schedule.then(|| rate(scheduled_successes, scheduled.len())).flatten(),
        "all_attempt_success_rate_wilson_95": known_schedule.then(|| wilson(scheduled_successes, scheduled.len())).flatten(),
        "successes": successes,
        "evaluable_success_rate": rate(successes, evaluable.len()),
        "success_rate_wilson_95": wilson(successes, evaluable.len()),
        "regressions": regressions, "baseline_pass_trials": baseline_passes,
        "regression_rate_wilson_95": wilson(regressions, baseline_passes),
        "successful_pair_latency_ratio_p50": quantile(50),
        "successful_pair_latency_ratio_p95": quantile(95), "speed_sample_count": ratios.len(),
        "noninferiority_established": false,
    })
}

pub fn describe(pairs: &[Value]) -> Value {
    let mut candidates = std::collections::BTreeMap::<String, Vec<&Value>>::new();
    for pair in pairs {
        if let Some(id) = pair["candidate"].as_str() {
            candidates.entry(id.into()).or_default().push(pair);
        }
    }
    let summaries: Vec<_> = candidates
        .into_iter()
        .map(|(id, pairs)| candidate_summary(&id, &pairs))
        .collect();
    json!({
        "method": "Wilson 95% descriptive intervals; scheduled all-attempt outcomes retain missing/infrastructure trials; evaluable quality is conditional; nearest-rank eligible successful-pair timing; no independent-task generalization",
        "candidates": summaries,
    })
}

#[cfg(test)]
mod tests {
    use super::wilson;

    #[test]
    fn intervals_do_not_claim_certainty_from_tiny_samples() {
        assert!(wilson(0, 0).is_none());
        assert!(wilson(3, 2).is_none());
        let interval = wilson(2, 2).unwrap();
        assert!(interval[0] < 0.5);
        assert!(interval[1] <= 1.0 + f64::EPSILON);
        assert!(wilson(0, 2).unwrap()[1] > 0.5);
    }
}
