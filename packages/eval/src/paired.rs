//! Pair-level regression reports: improvements never cancel baseline-pass failures.
use super::{EvalRepetitionResult, EvalVariantRunResult};

pub fn report(variants: &[EvalVariantRunResult]) -> serde_json::Value {
    let baseline = variants
        .iter()
        .find(|variant| variant.variant_id == "single");
    let mut pairs = Vec::new();
    if let Some(baseline) = baseline {
        for candidate in variants
            .iter()
            .filter(|variant| variant.variant_id != "single")
        {
            for case in &baseline.cases {
                for reference in &case.repetitions {
                    let observed = candidate
                        .cases
                        .iter()
                        .find(|value| value.case_id == case.case_id)
                        .and_then(|value| {
                            value
                                .repetitions
                                .iter()
                                .find(|value| value.repetition == reference.repetition)
                        });
                    let regression = reference.passed && observed.is_none_or(|value| !value.passed);
                    let latency = |rep: &EvalRepetitionResult| {
                        rep.measurements
                            .get("workflow.wall_time_ms")
                            .or_else(|| rep.measurements.get("agent_wall_time_ms"))
                            .or_else(|| rep.measurements.get("command_wall_time_ms"))
                            .copied()
                    };
                    let ratio = observed
                        .and_then(&latency)
                        .zip(latency(reference))
                        .and_then(|(value, base)| (base > 0.0).then_some(value / base));
                    pairs.push(serde_json::json!({"case":case.case_id,"repetition":reference.repetition,"baseline":"single","candidate":candidate.variant_id,"baseline_passed":reference.passed,"candidate_passed":observed.map(|value|value.passed),"quality_regression":regression,"latency_ratio":ratio,"speed_eligible":reference.passed && observed.is_some_and(|value|value.passed)}));
                }
            }
        }
    }
    let regressions = pairs
        .iter()
        .filter(|pair| pair["quality_regression"] == true)
        .count();
    let passed_pairs = pairs
        .iter()
        .filter(|pair| pair["baseline_passed"] == true)
        .count();
    let coverage_complete = variants.iter().all(|variant| {
        variant
            .cases
            .iter()
            .all(|case| !case.repetitions.is_empty())
    });
    let uncertainty = super::statistics::describe(&pairs);
    serde_json::json!({"schema_version":1,"uncertainty":uncertainty,"baseline_present":baseline.is_some(),"quality_regressions":regressions,"coverage_complete":coverage_complete,"baseline_pass_pairs":passed_pairs,"passed":baseline.is_some() && coverage_complete && passed_pairs>0 && regressions==0,"pairs":pairs,"statistical_conclusion":"descriptive only; no noninferiority claim"})
}

#[cfg(test)]
mod tests {
    use super::report;
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
    fn gains_cannot_hide_a_paired_regression() {
        let result = report(&[
            variant("single", &[true, false]),
            variant("swarm", &[false, true]),
        ]);
        assert_eq!(result["quality_regressions"], 1);
        assert_eq!(result["passed"], false);
        assert_eq!(result["pairs"][0]["speed_eligible"], false);
    }
    #[test]
    fn all_failed_baselines_are_not_positive_quality_evidence() {
        assert_eq!(
            report(&[variant("single", &[false]), variant("candidate", &[false])])["passed"],
            false
        );
    }
    #[test]
    fn missing_candidate_is_a_regression_and_missing_baseline_is_not_success() {
        assert_eq!(
            report(&[variant("single", &[true]), variant("swarm", &[])])["passed"],
            false
        );
        assert_eq!(report(&[variant("swarm", &[true])])["passed"], false);
    }
}
