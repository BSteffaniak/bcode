//! Portable, observational scheduling diagnostics. These facts never authorize execution.

use serde::{Deserialize, Serialize};

#[cfg(test)]
#[path = "scheduling_tests.rs"]
mod tests;

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
    /// Shared admission predicate reports a resource conflict outside the displayed claim prefix.
    ResourceConflict,
    /// Shared admission predicate reports local concurrency exhaustion.
    Concurrency,
    /// Canonical fresh-admission clock or lifetime allowance restriction.
    AdmissionAllowance,
    /// Candidate activation is no longer current unresolved work.
    StaleActivation,
    /// Canonical run is terminal.
    Terminal,
    /// Canonical recovery reconciliation barrier.
    Recovery,
    /// Durable cancellation requested.
    Cancellation,
    /// Canonical control failure prevents admission.
    ControlFailure,
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
    /// Exact application-issued continuation token. Never derived from display identities.
    #[serde(default)]
    pub next_cursor: Option<String>,
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
        lines.push(format!(
            "Scheduling page: {} activation observations (not whole-run totals)",
            self.activations.len().min(100)
        ));
        if self.has_more || self.activations.len() > 100 {
            lines.push(if self.next_cursor.is_some() {
                "Additional activations available through workflow scheduling-page using the exact returned cursor.".into()
            } else {
                "Additional activations omitted; this view exposes no scheduling continuation cursor. Refresh is not pagination.".into()
            });
        }
        lines.push("Unknown means eligibility was not established; absence of a blocker does not mean Ready.".into());
        lines.push("Provider backpressure: unknown unless an explicit observed Backpressure reason is shown; blocker lists do not establish complete eligibility.".into());
        for activation in self.activations.iter().take(100) {
            lines.extend(activation.detail_lines());
        }
        if self.details_truncated {
            lines.push(
                "Scheduling details truncated; displayed identifiers are not action targets".into(),
            );
        }
        lines
    }

    /// Exact selected activation detail from this bounded page, not a historical attempt lookup.
    #[must_use]
    pub fn selected_activation_lines(
        &self,
        node_id: &str,
        activation_id: Option<&str>,
    ) -> Vec<String> {
        // Projection may have shortened identities: never match a shortened prefix to a target.
        if self.details_truncated {
            return vec!["Exact scheduling detail unavailable: this observation contains shortened details; inspect the scheduling overview.".into()];
        }
        let mut lines = vec![format!(
            "Current scheduling observation: revision {} at {} ms (not selected-attempt history)",
            self.graph_revision, self.observed_at_ms
        )];
        for activation in self.activations.iter().take(100).filter(|activation| {
            activation.node_id == node_id
                && activation_id.is_none_or(|id| activation.activation_id == id)
        }) {
            lines.extend(activation.detail_lines());
        }
        if lines.len() == 1 {
            lines.push("No scheduling observation for this exact selection in the bounded page; state is unknown.".into());
        }
        if self.has_more {
            lines.push(if self.next_cursor.is_some() {
                "Additional activations available through scheduling-page using the exact returned cursor.".into()
            } else {
                "Additional activations omitted; no scheduling continuation cursor is exposed by this view.".into()
            });
        }
        lines
    }
}

impl WorkflowSchedulingActivation {
    /// Bounded, single-line-per-fact fallback; content budgets are not terminal geometry.
    #[must_use]
    pub fn detail_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "{} / {}: {:?}",
            display_text(&self.node_id),
            display_text(&self.activation_id),
            self.state
        )];
        for reason in self.reasons.iter().take(8) {
            lines.push(match reason {
                WorkflowSchedulingBlocker::Dependency { remaining } => {
                    format!("  Dependency: {remaining} unresolved")
                }
                WorkflowSchedulingBlocker::Policy { reason } => format!("  Policy: {reason:?}"),
                WorkflowSchedulingBlocker::Resource { resource } => {
                    format!("  Resource conflict: {}", display_text(resource))
                }
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
        if self.reasons.len() > 8 {
            lines.push("  Additional blocker reasons omitted".into());
        }
        if self.state == WorkflowSchedulingState::Blocked && self.reasons.is_empty() {
            lines.push("  Blocker reason was not observed".into());
        }
        lines
    }
}

fn display_text(value: &str) -> String {
    value
        .chars()
        .take(256)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}
