//! Explicit quality-only envelope; full readiness reports retain their existing semantics.
use super::{BenchmarkObservation, BenchmarkPlan, BenchmarkReport, compare_benchmark};
use serde::{Deserialize, Serialize};

/// Scope of a quality-only comparison. No efficiency or release-readiness claim is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityComparisonScope {
    /// Semantic quality and reliability only; cost and latency remain descriptive.
    QualityAndReliabilityOnly,
}

/// Separately versioned envelope prevents quality-only verdicts masquerading as readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityBenchmarkReport {
    /// Envelope version 1; nested report retains its own version and raw measurements.
    pub schema_version: u32,
    /// Explicitly excludes efficiency and full readiness from verdict interpretation.
    pub scope: QualityComparisonScope,
    /// Same audit trail and resource coverage as the full comparison, scoped verdicts.
    pub report: BenchmarkReport,
}

/// Compare semantic quality and reliability without gating on billing or resource overhead.
///
/// Missing or invalid consumer evidence, unknown reliability, duplicates, and insufficient
/// task coverage still block improvement. Unknown costs remain unknown, never zero-filled.
///
/// # Errors
/// Returns the same validation errors as [`compare_benchmark`].
pub fn compare_benchmark_quality_only(
    plan: &BenchmarkPlan,
    observations: &[BenchmarkObservation],
) -> Result<QualityBenchmarkReport, String> {
    let mut report = compare_benchmark(plan, observations)?;
    for feature in &mut report.features {
        feature.verdict = super::comparison::quality_verdict(feature, &plan.gates);
    }
    Ok(QualityBenchmarkReport {
        schema_version: 1,
        scope: QualityComparisonScope::QualityAndReliabilityOnly,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgement_benchmark::{Arm, TerminalOutcome, Verdict};

    #[test]
    fn unknown_billing_is_preserved_and_full_readiness_stays_inconclusive() {
        let mut plan = super::super::tests::plan();
        let mut rows = Vec::new();
        let template = plan.tasks[0].clone();
        plan.tasks.clear();
        for index in 0..5 {
            let mut task = template.clone();
            task.task_id = format!("independent-task-{index}");
            for (arm, label) in [(Arm::Off, "wrong"), (Arm::On, "correct")] {
                let mut row = super::super::tests::observation(arm, label);
                row.task_id.clone_from(&task.task_id);
                row.evidence.capture_id = format!("capture-{index}-{arm:?}");
                row.evidence.pairing_id = format!("pair-{index}");
                row.cost_usd = None;
                row.latency_ms = None;
                rows.push(row);
            }
            plan.tasks.push(task);
        }
        let full = compare_benchmark(&plan, &rows).unwrap();
        let quality = compare_benchmark_quality_only(&plan, &rows).unwrap();
        assert_eq!(full.features[0].verdict, Verdict::Inconclusive);
        assert_eq!(quality.report.features[0].verdict, Verdict::Improved);
        assert_eq!(quality.report.features[0].cost_usd.pairs, 0);
        assert_eq!(quality.report.features[0].cost_usd.mean_delta, None);
        assert_eq!(quality.report.observations[0].cost_usd, None);
        assert_eq!(quality.report.features[0].latency_ms.mean_delta, None);
        assert!(
            quality.report.features[0]
                .gaps
                .iter()
                .any(|gap| gap.contains("unknown total pricing"))
        );
        let encoded = serde_json::to_value(&quality).unwrap();
        assert_eq!(encoded["scope"], "quality_and_reliability_only");
    }

    #[test]
    fn quality_mode_does_not_hide_missing_or_failed_attempts() {
        let plan = super::super::tests::plan();
        let off = super::super::tests::observation(Arm::Off, "wrong");
        let mut on = super::super::tests::observation(Arm::On, "correct");
        let missing = compare_benchmark_quality_only(&plan, std::slice::from_ref(&off)).unwrap();
        assert_eq!(missing.report.features[0].verdict, Verdict::Inconclusive);
        on.outcome = TerminalOutcome::Failed;
        let failed = compare_benchmark_quality_only(&plan, &[off, on]).unwrap();
        assert_eq!(failed.report.features[0].verdict, Verdict::Regressed);
    }
}
