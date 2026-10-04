//! Portable, observational scheduling diagnostics. These facts never authorize execution.

use serde::{Deserialize, Serialize};

/// Scope of an observed capacity reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCapacityScope {
    /// Per-run policy.
    Run,
    /// Daemon execution capacity.
    Daemon,
}

/// Effective policy and occupancy from the same observation scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSchedulingPolicy {
    /// None explicitly means uncapped, not unknown.
    pub max_running: Option<u64>,
    /// Includes reserved admission slots, not only executing providers.
    pub running: u64,
    pub scope: WorkflowCapacityScope,
}

/// Verified policy gate preventing admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPolicyBlocker {
    Paused,
    Approval,
    /// Waiting for externally supplied workflow input.
    Input,
    Retry,
    ExecutionAuthority,
}

/// Observed temporary execution backpressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowBackpressureKind {
    Provider,
    Dispatch,
    Storage,
}

/// Canonical reason for non-admission. Absence is not evidence of readiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowSchedulingBlocker {
    Dependency {
        remaining: u64,
    },
    Policy {
        reason: WorkflowPolicyBlocker,
    },
    Resource {
        resource: String,
    },
    Capacity {
        scope: WorkflowCapacityScope,
        running: u64,
        limit: u64,
    },
    Backpressure {
        reason: WorkflowBackpressureKind,
    },
}

/// Canonical observed scheduler state, never inferred from a truncated graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSchedulingState {
    Ready,
    Running,
    Blocked,
    Unknown,
}

/// One bounded activation observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSchedulingActivation {
    pub node_id: String,
    pub activation_id: String,
    pub state: WorkflowSchedulingState,
    pub reasons: Vec<WorkflowSchedulingBlocker>,
}

/// Bounded scheduling snapshot. Counts in this page are not whole-run totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSchedulingView {
    pub graph_revision: u64,
    pub observed_at_ms: u64,
    /// None means effective policy was not observed.
    pub policy: Option<WorkflowSchedulingPolicy>,
    pub activations: Vec<WorkflowSchedulingActivation>,
    /// Additional activations exist outside this page.
    pub has_more: bool,
    /// At least one reason list or display string was shortened.
    pub details_truncated: bool,
}

impl WorkflowSchedulingView {
    /// Bounded textual fallback shared by CLI and rich presentation adapters.
    /// All counts are explicitly page-local; this does not claim a complete scheduler census.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "Scheduling observation: revision {} at {} ms{}",
            self.graph_revision,
            self.observed_at_ms,
            if self.has_more {
                " · partial page"
            } else {
                ""
            }
        )];
        lines.push(self.policy.as_ref().map_or_else(
            || "Effective concurrency policy: unknown".into(),
            |policy| {
                format!(
                    "Effective concurrency ({:?}): {} · {} reserved/running",
                    policy.scope,
                    policy
                        .max_running
                        .map_or_else(|| "uncapped".into(), |cap| cap.to_string()),
                    policy.running
                )
            },
        ));
        for activation in self.activations.iter().take(100) {
            lines.push(format!(
                "{} / {}: {:?}",
                activation.node_id.chars().take(256).collect::<String>(),
                activation
                    .activation_id
                    .chars()
                    .take(256)
                    .collect::<String>(),
                activation.state
            ));
            for reason in activation.reasons.iter().take(8) {
                lines.push(match reason {
                    WorkflowSchedulingBlocker::Dependency { remaining } => {
                        format!("  Dependency: {remaining} unresolved")
                    }
                    WorkflowSchedulingBlocker::Policy { reason } => format!("  Policy: {reason:?}"),
                    WorkflowSchedulingBlocker::Resource { resource } => format!(
                        "  Resource conflict: {}",
                        resource.chars().take(256).collect::<String>()
                    ),
                    WorkflowSchedulingBlocker::Capacity {
                        scope,
                        running,
                        limit,
                    } => format!("  Capacity ({scope:?}): {running}/{limit}"),
                    WorkflowSchedulingBlocker::Backpressure { reason } => {
                        format!("  Backpressure: {reason:?}")
                    }
                });
            }
        }
        if self.details_truncated {
            lines.push("Scheduling details truncated".into());
        }
        lines
    }
}
