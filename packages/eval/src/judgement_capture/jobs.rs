//! CLI-hosted bounded jobs. The host uses the selected subject's verified public client.
use super::{
    CaptureRequest, ConsumerCapture, ConsumerRequest, MAX_CAPTURE_BYTES, capture_bounded,
    compare_captures,
};
use crate::judgement_benchmark::{BenchmarkPlan, BenchmarkReport};
use bcode_client::BcodeClient;
use serde::{Deserialize, Serialize};

/// One public execution job; no auth secrets, expected provider answers or oracle scoring here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureJob {
    /// Version 1 only; unknown versions fail before any execution.
    pub schema_version: u32,
    /// Independently scheduled treatment and exact subject identity.
    pub request: CaptureRequest,
    /// Actual consumer input; never included in the returned capture artifact.
    pub consumer: ConsumerRequest,
    /// Explicit live opt-in. Host policy/user approval must also permit execution.
    pub allow_live: bool,
}

/// Deserialize a bounded execution job without side effects or secret-bearing diagnostics.
///
/// # Errors
/// Rejects unknown fields, malformed input, future versions and oversized jobs.
pub fn decode_capture_job(bytes: &[u8]) -> Result<CaptureJob, &'static str> {
    if bytes.len() > MAX_CAPTURE_BYTES {
        return Err("capture job exceeds bound");
    }
    let job: CaptureJob = serde_json::from_slice(bytes).map_err(|_| "invalid capture job")?;
    if job.schema_version != 1 {
        return Err("unsupported capture job version");
    }
    Ok(job)
}

/// Run one bounded actual-consumer job inside the explicitly selected subject executable.
///
/// Never constructs its own client, discovers credentials, starts services, or changes treatment.
/// The host verifies effective configuration/artifact/state and supplies a require-running client.
///
/// # Errors
/// Returns bounded static validation diagnostics; provider/transport failure bodies are discarded.
pub async fn execute_capture_job(
    client: &BcodeClient,
    job: CaptureJob,
) -> Result<ConsumerCapture, &'static str> {
    if job.schema_version != 1 {
        return Err("unsupported capture job version");
    }
    if serde_json::to_vec(&job)
        .map_err(|_| "invalid capture job")?
        .len()
        > MAX_CAPTURE_BYTES
    {
        return Err("capture job exceeds bound");
    }
    capture_bounded(client, job.request, job.consumer, job.allow_live).await
}

/// Read-only offline comparison input uses the shared benchmark plan directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonJob {
    /// Version 1 only.
    pub schema_version: u32,
    /// Preregistered independent corpus converted to the shared plan by its owner.
    pub plan: BenchmarkPlan,
    /// Raw captures, including unavailable/incomplete arms.
    pub captures: Vec<ConsumerCapture>,
}

/// Shared benchmark report plus explicit rejected-capture gaps; no parallel scoring schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureComparison {
    /// Version 1 only.
    pub schema_version: u32,
    /// Direct shared metrics contract.
    pub report: BenchmarkReport,
    /// Captures which could not be promoted; missing arms stay in the scheduled denominator.
    pub capture_gaps: Vec<String>,
    /// Raw output scores independent of unverified safety/efficiency; never improvement closure.
    pub raw_scores: Vec<super::RawCaptureScore>,
}

/// Execute a bounded offline comparison with no daemon, database or model access.
///
/// # Errors
/// Rejects future versions, unknown fields, oversized inputs and invalid preregistered plans.
pub fn compare_capture_job(bytes: &[u8]) -> Result<CaptureComparison, String> {
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("comparison job exceeds bound".into());
    }
    let job: ComparisonJob =
        serde_json::from_slice(bytes).map_err(|_| "invalid comparison job".to_owned())?;
    if job.schema_version != 1 {
        return Err("unsupported comparison job version".into());
    }
    let (report, capture_gaps) = compare_captures(&job.plan, &job.captures)?;
    let raw_scores = super::raw_capture_scores(&job.plan, &job.captures)?;
    Ok(CaptureComparison {
        schema_version: 1,
        report,
        capture_gaps,
        raw_scores,
    })
}
