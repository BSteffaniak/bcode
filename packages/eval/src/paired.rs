//! Pair-level reports retain failed and missing attempts; gains never cancel regressions.
use super::{EvalRepetitionResult, EvalVariantRunResult};
use std::collections::{BTreeMap, BTreeSet};

fn execution_failed(rep: &EvalRepetitionResult) -> bool {
    rep.measurements
        .get("eval.execution_failed")
        .is_some_and(|value| *value > 0.0)
}

fn complete(rep: &EvalRepetitionResult) -> bool {
    let evidence: Vec<_> = ["eval.capture_complete", "workflow.capture_complete"]
        .iter()
        .filter_map(|key| rep.measurements.get(*key))
        .collect();
    !execution_failed(rep)
        && !evidence.is_empty()
        && evidence
            .iter()
            .all(|value| value.to_bits() == 1.0_f64.to_bits())
}

type Identity = (String, u32);

fn inventory(variant: &EvalVariantRunResult) -> BTreeMap<Identity, Vec<&EvalRepetitionResult>> {
    let mut rows = BTreeMap::<Identity, Vec<&EvalRepetitionResult>>::new();
    for case in &variant.cases {
        for rep in &case.repetitions {
            rows.entry((case.case_id.clone(), rep.repetition))
                .or_default()
                .push(rep);
        }
    }
    rows
}

fn unique<'a>(
    rows: &'a BTreeMap<Identity, Vec<&'a EvalRepetitionResult>>,
    id: &Identity,
) -> Option<&'a EvalRepetitionResult> {
    rows.get(id)
        .filter(|values| values.len() == 1)
        .map(|values| values[0])
}

fn outcome(rep: Option<&EvalRepetitionResult>, observed: bool) -> &'static str {
    match rep {
        Some(value) if execution_failed(value) => "infrastructure_failure",
        Some(value) if value.passed => "passed",
        Some(_) => "failed",
        None if observed => "ambiguous_duplicate",
        None => "missing",
    }
}

fn latency(rep: &EvalRepetitionResult) -> Option<f64> {
    rep.measurements
        .get("workflow.wall_time_ms")
        .or_else(|| rep.measurements.get("agent_wall_time_ms"))
        .or_else(|| rep.measurements.get("command_wall_time_ms"))
        .copied()
        .filter(|value| value.is_finite() && *value > 0.0)
}

/// Retained-result report. Scheduled coverage is unknown and cannot support acceptance.
#[cfg(test)]
fn report(variants: &[EvalVariantRunResult]) -> serde_json::Value {
    report_inner(variants, None)
}

/// Report against planned case/repetition blocks, shared by every variant.
/// Duplicate planned blocks or duplicate observed identities fail coverage closed.
/// This is a derived diagnostic schema, not a canonical session contract.
pub fn report_with_schedule(
    variants: &[EvalVariantRunResult],
    scheduled: &[(String, u32)],
) -> serde_json::Value {
    report_inner(variants, Some(scheduled))
}

fn report_inner(
    variants: &[EvalVariantRunResult],
    scheduled: Option<&[Identity]>,
) -> serde_json::Value {
    let baselines: Vec<_> = variants
        .iter()
        .filter(|value| value.variant_id == "single")
        .collect();
    let baseline = (baselines.len() == 1).then(|| inventory(baselines[0]));
    let planned: BTreeSet<_> = scheduled.into_iter().flatten().cloned().collect();
    let mut identities = planned.clone();
    for variant in variants {
        identities.extend(inventory(variant).into_keys());
    }
    let variant_ids: BTreeSet<_> = variants.iter().map(|value| &value.variant_id).collect();
    let mut coverage_complete = baseline.is_some()
        && scheduled.is_some_and(|values| !values.is_empty() && values.len() == planned.len())
        && variant_ids.len() == variants.len()
        && variants.len() > 1;
    let mut pairs = Vec::new();
    let empty = BTreeMap::new();
    let references = baseline.as_ref().unwrap_or(&empty);
    for candidate in variants.iter().filter(|value| value.variant_id != "single") {
        let observations = inventory(candidate);
        for id in &identities {
            let reference = unique(references, id);
            let observed = unique(&observations, id);
            let expected = scheduled.is_none() || planned.contains(id);
            coverage_complete &= expected && reference.is_some() && observed.is_some();
            let quality_evaluable = reference.is_some_and(|value| !execution_failed(value))
                && observed.is_some_and(|value| !execution_failed(value));
            let evidence_complete =
                reference.is_some_and(complete) && observed.is_some_and(complete);
            let regression = quality_evaluable
                && reference.is_some_and(|value| value.passed)
                && observed.is_some_and(|value| !value.passed);
            let ratio = observed
                .and_then(latency)
                .zip(reference.and_then(latency))
                .map(|(value, base)| value / base)
                .filter(|value| value.is_finite() && *value > 0.0);
            let speed_eligible = scheduled.is_some()
                && expected
                && quality_evaluable
                && evidence_complete
                && ratio.is_some()
                && reference.is_some_and(|value| value.passed)
                && observed.is_some_and(|value| value.passed);
            pairs.push(serde_json::json!({
                "case": id.0, "repetition": id.1, "baseline": "single", "candidate": candidate.variant_id,
                "schedule_known": scheduled.is_some(), "scheduled": expected,
                "baseline_observed": references.contains_key(id), "candidate_observed": observations.contains_key(id),
                "baseline_outcome": outcome(reference, references.contains_key(id)),
                "candidate_outcome": outcome(observed, observations.contains_key(id)),
                "baseline_passed": reference.map(|value| value.passed), "candidate_passed": observed.map(|value| value.passed),
                "quality_evaluable": quality_evaluable, "evidence_complete": evidence_complete,
                "quality_regression": regression, "latency_ratio": ratio, "speed_eligible": speed_eligible,
            }));
        }
    }
    let count = |field: &str| pairs.iter().filter(|pair| pair[field] == true).count();
    let regressions = count("quality_regression");
    let passed_pairs = pairs
        .iter()
        .filter(|pair| pair["quality_evaluable"] == true && pair["baseline_passed"] == true)
        .count();
    let unevaluable_pairs = pairs.len() - count("quality_evaluable");
    let incomplete_pairs = pairs.len() - count("evidence_complete");
    serde_json::json!({
        "schema_version": 3, "uncertainty": super::statistics::describe(&pairs),
        "schedule_known": scheduled.is_some(), "scheduled_pairs": scheduled.map(|_| planned.len() * variants.iter().filter(|value| value.variant_id != "single").count()),
        "observed_pairs": pairs.iter().filter(|pair| pair["baseline_observed"] == true && pair["candidate_observed"] == true).count(),
        "evaluable_pairs": count("quality_evaluable"), "baseline_present": baseline.is_some(),
        "quality_regressions": regressions, "coverage_complete": coverage_complete,
        "unevaluable_pairs": unevaluable_pairs, "incomplete_evidence_pairs": incomplete_pairs,
        "baseline_pass_pairs": passed_pairs,
        "passed": coverage_complete && passed_pairs > 0 && regressions == 0 && unevaluable_pairs == 0 && incomplete_pairs == 0,
        "pairs": pairs, "statistical_conclusion": "descriptive only; no noninferiority or benchmark claim",
    })
}

#[cfg(test)]
mod tests {
    use super::{report, report_with_schedule};
    use bcode_eval_models::{
        EvalCaseRunResult, EvalRepetitionResult, EvalScore, EvalVariantRunResult,
    };

    fn variant(id: &str, outcomes: &[bool]) -> EvalVariantRunResult {
        EvalVariantRunResult {
            variant_id: id.into(),
            cases: vec![EvalCaseRunResult {
                case_id: "case".into(),
                variant_id: id.into(),
                pass_rate: 0.0,
                repetitions: outcomes
                    .iter()
                    .enumerate()
                    .map(|(index, passed)| EvalRepetitionResult {
                        timeout: None,
                        case_id: "case".into(),
                        variant_id: id.into(),
                        repetition: u32::try_from(index + 1).unwrap(),
                        passed: *passed,
                        exit_code: Some(0),
                        wall_time_ms: 1,
                        measurements: std::collections::BTreeMap::new(),
                        judges: vec![],
                        diagnostics: vec![],
                        artifacts: vec![],
                    })
                    .collect(),
            }],
            pass_rate: 0.0,
            measurements: std::collections::BTreeMap::new(),
            score: EvalScore::default(),
        }
    }

    #[test]
    fn measured_terminal_failures_remain_quality_trials_and_gains_cannot_hide_regressions() {
        let result = report(&[
            variant("single", &[true, false]),
            variant("swarm", &[false, true]),
        ]);
        assert_eq!(result["quality_regressions"], 1);
        assert_eq!(result["evaluable_pairs"], 2);
        assert_eq!(
            result["uncertainty"]["candidates"][0]["candidate_failed_trials"],
            1
        );
        assert_eq!(result["passed"], false);
    }

    #[test]
    fn missing_and_infrastructure_attempts_stay_in_scheduled_outcome_denominator() {
        let mut candidate = variant("swarm", &[true, false]);
        candidate.cases[0].repetitions[1]
            .measurements
            .insert("eval.execution_failed".into(), 1.0);
        let planned = vec![("case".into(), 1), ("case".into(), 2), ("case".into(), 3)];
        let result = report_with_schedule(&[variant("single", &[true, true]), candidate], &planned);
        let summary = &result["uncertainty"]["candidates"][0];
        assert_eq!(summary["scheduled_trials"], 3);
        assert_eq!(summary["observed_trials"], 2);
        assert_eq!(summary["evaluable_trials"], 1);
        assert_eq!(summary["candidate_infrastructure_failures"], 1);
        assert_eq!(summary["candidate_missing_trials"], 1);
        assert!(
            (summary["all_attempt_success_rate"].as_f64().unwrap() - 1.0 / 3.0).abs()
                < f64::EPSILON
        );
        assert_eq!(result["coverage_complete"], false);
        assert_eq!(result["pairs"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn candidate_without_baseline_is_retained_and_unknown_schedule_is_not_complete() {
        let result = report(&[variant("single", &[]), variant("swarm", &[false])]);
        assert_eq!(result["pairs"].as_array().unwrap().len(), 1);
        assert_eq!(result["scheduled_pairs"], serde_json::Value::Null);
        assert_eq!(
            result["uncertainty"]["candidates"][0]["candidate_failed_trials"],
            1
        );
        assert_eq!(result["coverage_complete"], false);
    }

    #[test]
    fn duplicate_and_unexpected_results_cannot_certify_coverage() {
        let mut candidate = variant("swarm", &[true, true]);
        candidate.cases[0].repetitions[1].repetition = 1;
        let planned = vec![("case".into(), 1)];
        let result = report_with_schedule(&[variant("single", &[true]), candidate], &planned);
        assert_eq!(result["coverage_complete"], false);
        assert_eq!(
            result["pairs"][0]["candidate_outcome"],
            "ambiguous_duplicate"
        );
        assert_eq!(
            result["uncertainty"]["candidates"][0]["candidate_ambiguous_trials"],
            1
        );
        let result = report_with_schedule(
            &[variant("single", &[true]), variant("swarm", &[true, false])],
            &planned,
        );
        assert_eq!(result["coverage_complete"], false);
        assert_eq!(
            result["uncertainty"]["candidates"][0]["scheduled_trials"],
            1
        );
        assert_eq!(
            result["uncertainty"]["candidates"][0]["unexpected_identity_trials"],
            1
        );
        assert_eq!(
            result["uncertainty"]["candidates"][0]["candidate_failed_trials"],
            1
        );
    }

    #[test]
    fn speed_requires_explicit_evidence_and_valid_positive_durations() {
        let mut variants = vec![variant("single", &[true]), variant("direct", &[true])];
        let planned = vec![("case".into(), 1)];
        for value in &mut variants {
            value.cases[0].repetitions[0]
                .measurements
                .insert("agent_wall_time_ms".into(), 10.0);
        }
        assert_eq!(
            report_with_schedule(&variants, &planned)["pairs"][0]["speed_eligible"],
            false
        );
        for value in &mut variants {
            value.cases[0].repetitions[0]
                .measurements
                .insert("eval.capture_complete".into(), 1.0);
        }
        assert_eq!(
            report_with_schedule(&variants, &planned)["pairs"][0]["speed_eligible"],
            true
        );
        variants[1].cases[0].repetitions[0]
            .measurements
            .insert("workflow.capture_complete".into(), 0.0);
        assert_eq!(
            report_with_schedule(&variants, &planned)["pairs"][0]["speed_eligible"],
            false
        );
        variants[1].cases[0].repetitions[0]
            .measurements
            .remove("workflow.capture_complete");
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            variants[1].cases[0].repetitions[0]
                .measurements
                .insert("agent_wall_time_ms".into(), invalid);
            assert_eq!(
                report_with_schedule(&variants, &planned)["pairs"][0]["speed_eligible"],
                false
            );
        }
    }
}
