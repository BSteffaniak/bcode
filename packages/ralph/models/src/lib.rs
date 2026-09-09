#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable Ralph operation contracts.
//!
//! These retain the existing JSON field names and serde defaults. Compatibility
//! is negotiated by the enclosing application transport; moving ownership does
//! not introduce a new wire version. String statuses remain opaque observations,
//! not authority to resume or mutate a run. Cancellation receipts acknowledge a
//! request, not terminal completion.

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_request_preserves_wire_defaults() {
        let request: RalphRunRequest =
            serde_json::from_value(serde_json::json!({"repo_root": "/repo"})).unwrap();
        assert_eq!(request.repo_root, PathBuf::from("/repo"));
        assert_eq!(request.loop_state_dir, None);
        assert_eq!(request.max_iterations, None);
        assert_eq!(request.no_progress_limit, None);
        assert!(!request.require_approval);
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "repo_root": "/repo", "loop_state_dir": null, "max_iterations": null,
                "no_progress_limit": null, "require_approval": false
            })
        );
    }

    #[test]
    fn status_response_preserves_optional_fields() {
        let response: RalphRunStatusResponse =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(response.loop_summary, None);
        assert_eq!(response.active_run, None);
        assert!(response.interrupted_runs.is_empty());
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::json!({
                "loop_summary": null, "active_run": null, "interrupted_runs": []
            })
        );
    }
}

/// Ralph lifecycle session-history append request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphLifecycleRequest {
    /// Session that should receive the durable lifecycle marker.
    pub session_id: SessionId,
    /// User-facing loop name.
    pub loop_name: String,
    /// Ralph loop state directory.
    pub state_dir: PathBuf,
    /// Lifecycle kind.
    pub kind: String,
    /// Human-readable lifecycle message.
    pub message: String,
    /// Lifecycle time in Unix epoch milliseconds.
    pub occurred_at_ms: u64,
}

/// Ralph loop status request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphStatusRequest {
    /// Repository root used to discover the active/latest Ralph loop.
    pub repo_root: PathBuf,
}

/// Ralph loop status summary for application clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphStatusSummary {
    /// User-facing loop name.
    pub loop_name: String,
    /// Current lifecycle status.
    pub status: String,
    /// Loop state directory.
    pub state_dir: PathBuf,
    /// Canonical progress document path.
    pub progress_doc_path: PathBuf,
    /// Isolated work area path, when created.
    #[serde(default)]
    pub work_area_path: Option<PathBuf>,
    /// Session ID rooted at the isolated work area, when created.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Completed iteration count.
    pub iteration_count: u64,
    /// Suggested next action.
    pub next_action: String,
    /// Checked progress-doc checklist items.
    pub checked_count: usize,
    /// Unchecked progress-doc checklist items.
    pub unchecked_count: usize,
    /// Validation commands configured for the loop.
    #[serde(default)]
    pub validation_commands: Vec<String>,
}

/// Ralph loop status response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphStatusResponse {
    /// Latest Ralph loop summary for the repository, when one exists.
    #[serde(default)]
    pub loop_summary: Option<RalphStatusSummary>,
}

/// Request to start a bounded Ralph autonomous run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to run, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
    /// Requested max iteration override.
    #[serde(default)]
    pub max_iterations: Option<u64>,
    /// Requested no-progress limit override.
    #[serde(default)]
    pub no_progress_limit: Option<u64>,
    /// Whether this run should begin in an approval-gated state.
    #[serde(default)]
    pub require_approval: bool,
}

/// Request to cancel an active Ralph run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphCancelRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific run ID to cancel. Defaults to the active run for the loop.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Specific Ralph loop state directory to cancel, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
}

/// Request to list recent Ralph runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphListRunsRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
}

/// Request to list recent Ralph iterations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphListIterationsRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
    /// Specific run ID to inspect, when not using the latest run.
    #[serde(default)]
    pub run_id: Option<String>,
}

/// Request to prepare resuming an interrupted Ralph run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphResumeRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
    /// Specific interrupted run ID to resume, when not using the latest interrupted run.
    #[serde(default)]
    pub interrupted_run_id: Option<String>,
}

/// Request to approve and start an approval-gated Ralph run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphApproveRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
    /// Specific run ID to approve, when not using the active approval-gated run.
    #[serde(default)]
    pub run_id: Option<String>,
}

/// Request to inspect Ralph autonomous run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunStatusRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
}

/// Ralph autonomous run summary for application clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunSummary {
    /// Run ID.
    pub run_id: String,
    /// Loop state directory this run belongs to.
    pub state_dir: PathBuf,
    /// Work-area session used by the runner, when known.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Parent runtime-work ID emitted for this run.
    #[serde(default)]
    pub runtime_work_id: Option<String>,
    /// Current run status.
    pub status: String,
    /// Requested max iteration override.
    #[serde(default)]
    pub requested_max_iterations: Option<u64>,
    /// Requested no-progress limit override.
    #[serde(default)]
    pub requested_no_progress_limit: Option<u64>,
    /// Whether cancellation was requested.
    pub cancel_requested: bool,
    /// Run start time in Unix epoch milliseconds.
    pub started_at_ms: u64,
    /// Last update time in Unix epoch milliseconds.
    pub updated_at_ms: u64,
    /// Run finish time in Unix epoch milliseconds.
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
    /// Terminal stop reason, when known.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Terminal error message, when known.
    #[serde(default)]
    pub error_message: Option<String>,
}

/// Ralph iteration summary for application clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphIterationSummary {
    /// Iteration ID.
    pub iteration_id: String,
    /// Run ID this iteration belongs to.
    pub run_id: String,
    /// Iteration number.
    pub iteration_number: u64,
    /// Iteration status.
    pub status: String,
    /// Stop reason, when known.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Error message, when known.
    #[serde(default)]
    pub error_message: Option<String>,
    /// Finish time in Unix epoch milliseconds.
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
}

/// Ralph validation summary for application clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphValidationSummary {
    /// Validation ID.
    pub validation_id: String,
    /// Parent iteration ID.
    pub iteration_id: String,
    /// Validation command.
    pub command: String,
    /// Validation status.
    pub status: String,
    /// Process exit code, when available.
    #[serde(default)]
    pub exit_code: Option<i64>,
    /// Bounded output reference, when retained.
    #[serde(default)]
    pub output_ref: Option<String>,
    /// Validation finish time in Unix epoch milliseconds.
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
    /// Error message, when validation failed to run.
    #[serde(default)]
    pub error_message: Option<String>,
}

/// Response after starting a Ralph run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunResponse {
    /// Persisted run summary.
    pub run: RalphRunSummary,
}

/// Response after requesting Ralph run cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphCancelResponse {
    /// Run summary after cancellation was requested.
    pub run: RalphRunSummary,
    /// Whether the cancel flag was requested by this call.
    pub cancel_requested: bool,
}

/// Response listing recent Ralph runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphListRunsResponse {
    /// Latest or selected Ralph loop summary for the repository, when one exists.
    #[serde(default)]
    pub loop_summary: Option<RalphStatusSummary>,
    /// Recent runs for the loop.
    #[serde(default)]
    pub runs: Vec<RalphRunSummary>,
}

/// Response listing recent Ralph iterations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphListIterationsResponse {
    /// Latest or selected Ralph loop summary for the repository, when one exists.
    #[serde(default)]
    pub loop_summary: Option<RalphStatusSummary>,
    /// Run whose iterations were listed, when one exists.
    #[serde(default)]
    pub run: Option<RalphRunSummary>,
    /// Iterations for the run.
    #[serde(default)]
    pub iterations: Vec<RalphIterationSummary>,
    /// Validation records grouped with the listed iterations.
    #[serde(default)]
    pub validations: Vec<RalphValidationSummary>,
}

/// Response after preparing a Ralph resume run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphResumeResponse {
    /// Interrupted run selected for resume.
    pub interrupted_run: RalphRunSummary,
    /// Newly created approval-gated run.
    pub resumed_run: RalphRunSummary,
}

/// Response describing Ralph autonomous run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunStatusResponse {
    /// Latest or selected Ralph loop summary for the repository, when one exists.
    #[serde(default)]
    pub loop_summary: Option<RalphStatusSummary>,
    /// Active run for the loop, when one exists.
    #[serde(default)]
    pub active_run: Option<RalphRunSummary>,
    /// Interrupted runs for the loop.
    #[serde(default)]
    pub interrupted_runs: Vec<RalphRunSummary>,
}
