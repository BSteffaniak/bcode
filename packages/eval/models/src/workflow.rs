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
    /// Optional versioned swarm policy; omission preserves the executable's baseline default.
    #[serde(default)]
    pub swarm_policy: Option<WorkflowEvalSwarmPolicy>,
    /// Authored controlled-entry acceptance; not used when `generated_entry` is true.
    pub stop_condition: String,
    /// Preserve normal generation for product-entry evaluation rather than controlled inputs.
    #[serde(default)]
    pub generated_entry: bool,
    /// Maximum implementation/evaluation iterations.
    pub max_iterations: u32,
    /// Concurrent workflow node allowance.
    pub concurrency: u32,
    /// Additional workflow attempts.
    pub worker_attempts: u32,
    /// Case-independent report substrings (case overrides use `metadata.workflow_oracle`).
    #[serde(default)]
    pub expected_contains: Vec<String>,
    /// Independent delivered artifact, relative to the workspace.
    #[serde(default)]
    pub answer_file: Option<PathBuf>,
    /// Exact expected JSON answer, never included in agent prompts.
    #[serde(default)]
    pub expected_json: Option<serde_json::Value>,
    /// External hidden validation command. Runs after execution, with its own deadline.
    #[serde(default)]
    pub oracle_command: Option<String>,
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

/// Explicit policy used by a workflow comparison; unknown future policies are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowEvalSwarmPolicy {
    /// Preserve baseline orchestration.
    Baseline,
    /// Exercise opt-in adaptive orchestration version one.
    AdaptiveV1,
}

impl WorkflowEvalSwarmPolicy {
    /// Exact plugin command argument.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::AdaptiveV1 => "adaptive-v1",
        }
    }
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

#[cfg(test)]
mod tests {
    use super::{WorkflowEvalConfig, WorkflowEvalSwarmPolicy};

    #[test]
    fn policy_is_explicit_and_unknown_versions_are_rejected() {
        let mut value = serde_json::json!({
            "binary": "/test/bcode", "strategy": "auto", "stop_condition": "verify",
            "max_iterations": 3, "concurrency": 4, "worker_attempts": 16
        });
        let baseline: WorkflowEvalConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(baseline.swarm_policy, None);
        for policy in [
            WorkflowEvalSwarmPolicy::Baseline,
            WorkflowEvalSwarmPolicy::AdaptiveV1,
        ] {
            value["swarm_policy"] = serde_json::json!(policy.as_str());
            let config: WorkflowEvalConfig = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(config.swarm_policy, Some(policy));
        }
        value["swarm_policy"] = serde_json::json!("adaptive-v2");
        assert!(serde_json::from_value::<WorkflowEvalConfig>(value).is_err());
    }
}
