//! Portable workflow eval settings. These describe experiments, never runtime policy.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Settings stored in a variant's `metadata.workflow` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEvalConfig {
    /// Exact executable artifact to exercise.
    pub binary: PathBuf,
    /// Goal strategy: auto, collaborate, or no-delegation.
    pub strategy: String,
    /// Shared, authored acceptance condition (no per-variant generation).
    pub stop_condition: String,
    /// Maximum implementation/evaluation iterations.
    pub max_iterations: u32,
    /// Concurrent workflow node allowance.
    pub concurrency: u32,
    /// Additional workflow attempts.
    pub worker_attempts: u32,
    /// Case-independent report substrings (case overrides use `metadata.workflow_oracle`).
    #[serde(default)]
    pub expected_contains: Vec<String>,
    /// Negative control: the task must not be claimed complete.
    #[serde(default)]
    pub expect_rejection: bool,
    /// Fixture files that must remain byte-identical.
    #[serde(default)]
    pub preserve_files: Vec<PathBuf>,
    /// Strategy instructions, without changing task acceptance.
    #[serde(default)]
    pub guidance: String,
    /// Optional explicit judge plugin/model/account selection.
    #[serde(default)]
    pub judge: Option<WorkflowEvalJudge>,
}

/// Judge is part of the subject, not the independent oracle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEvalJudge {
    /// Provider plugin identity.
    pub provider: String,
    /// Model identity (pin a concrete version for calibrated runs).
    pub model: String,
    /// Owned account; empty explicitly selects daemon environment credentials.
    pub account: String,
    /// Completion probability percentage.
    pub threshold: u8,
}
