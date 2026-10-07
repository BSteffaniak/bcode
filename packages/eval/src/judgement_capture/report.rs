//! Direct shared-contract reporting; rejected captures remain explicit gaps, never fake zeros.
use super::ConsumerCapture;
use crate::judgement_benchmark::{BenchmarkPlan, BenchmarkReport, compare_benchmark};

/// Compare semantic quality without billing or latency readiness gates.
///
/// Capture provenance and reliability requirements remain unchanged; rejected captures
/// stay explicit gaps rather than qualifying observations.
///
/// # Errors
/// Rejects an oversized capture set or an invalid benchmark plan.
pub fn compare_captures_quality_only(
    plan: &BenchmarkPlan,
    captures: &[ConsumerCapture],
) -> Result<
    (
        crate::judgement_benchmark::QualityBenchmarkReport,
        Vec<String>,
    ),
    String,
> {
    let (report, gaps) = compare_captures(plan, captures)?;
    let quality =
        crate::judgement_benchmark::compare_benchmark_quality_only(plan, &report.observations)?;
    Ok((quality, gaps))
}

/// Compare bounded captured arms directly with the v1 preregistered plan.
/// Returns the shared report and promotion gaps; missing arms remain scheduled denominators.
///
/// # Errors
/// Rejects an oversized capture set or an invalid shared benchmark plan.
pub fn compare_captures(
    plan: &BenchmarkPlan,
    captures: &[ConsumerCapture],
) -> Result<(BenchmarkReport, Vec<String>), String> {
    if captures.len() > 4096 {
        return Err("capture set exceeds bounded report input".into());
    }
    let mut observations = Vec::new();
    let mut gaps = Vec::new();
    for capture in captures {
        match capture.benchmark_observation() {
            Ok(observation) => observations.push(observation),
            Err(gap) => gaps.push(format!(
                "{} repetition {} {:?}: {gap}",
                capture.request.task_id, capture.request.repetition, capture.request.arm
            )),
        }
    }
    Ok((compare_benchmark(plan, &observations)?, gaps))
}
