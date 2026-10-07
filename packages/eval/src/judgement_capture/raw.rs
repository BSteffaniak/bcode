//! Raw semantic scoring independent from safety and efficiency promotion.
use super::ConsumerCapture;
use crate::judgement_benchmark::{Arm, BenchmarkPlan, OutputScore, score_output};
use serde::{Deserialize, Serialize};

/// One raw output score; never a safety attestation or whole-feature improvement verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCaptureScore {
    /// Scheduled independent task.
    pub task_id: String,
    /// Scheduled repetition.
    pub repetition: u32,
    /// Captured treatment.
    pub arm: Arm,
    /// Actual execution boundary, retained for truthful interpretation.
    pub scope: super::CaptureScope,
    /// Output utility even when safety/cost evidence is missing; None means unavailable.
    pub score: Option<OutputScore>,
    /// Canonical safety verification remains separately nullable.
    pub authority_preserved: Option<bool>,
    /// Request-only persistence verification remains separately nullable.
    pub request_only: Option<bool>,
}

/// Score matching bounded raw outputs without inventing call counts, cost or safety facts.
/// Duplicate captures are preserved here, not silently selected as authoritative.
///
/// # Errors
/// Rejects oversized capture sets. Shared plan validation remains with `compare_benchmark`.
pub fn raw_capture_scores(
    plan: &BenchmarkPlan,
    captures: &[ConsumerCapture],
) -> Result<Vec<RawCaptureScore>, String> {
    if captures.len() > 4096 {
        return Err("capture set exceeds bound".into());
    }
    let mut scores = Vec::new();
    for capture in captures {
        let Some(task) = plan.tasks.iter().find(|task| {
            task.task_id == capture.request.task_id
                && task.feature == capture.request.feature
                && capture.request.repetition < task.repetitions
                && capture.request.preregistration_id == plan.preregistration_id
        }) else {
            continue;
        };
        scores.push(RawCaptureScore {
            task_id: task.task_id.clone(),
            repetition: capture.request.repetition,
            arm: capture.request.arm,
            scope: capture.scope,
            score: capture
                .output
                .as_ref()
                .map(|output| score_output(&task.oracle, output)),
            authority_preserved: capture.authority_preserved,
            request_only: capture.request_only,
        });
    }
    Ok(scores)
}
