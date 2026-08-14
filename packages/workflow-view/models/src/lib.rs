#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable, renderer-neutral workflow projection contracts.

use serde::{Deserialize, Serialize};

/// Current workflow projection schema version.
pub const WORKFLOW_VIEW_VERSION: u32 = 1;

/// Current workflow live-event contract version.
pub const WORKFLOW_LIVE_EVENT_VERSION: u32 = 1;

/// Notification that canonical state for one workflow run changed.
///
/// This event deliberately carries no projected state. Consumers refetch a bounded
/// [`WorkflowRunView`] so notification delivery cannot become a second source of truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLiveEvent {
    /// Live-event schema version. Unknown versions must not be interpreted as this version.
    pub version: u32,
    /// Run whose canonical state changed.
    pub run_id: String,
    /// Monotonic canonical event sequence within the run.
    pub event_sequence: u64,
    /// Time the canonical event was persisted.
    pub changed_at_ms: u64,
}

/// Bounded page of ordered workflow live notifications used only for gap catch-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLiveEventPage {
    pub events: Vec<WorkflowLiveEvent>,
    /// More canonical events exist than fit in this bounded page; replace from a fresh snapshot.
    pub resync_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowLiveEventDisposition {
    /// This is the next unseen sequence; refetch the bounded run projection.
    Refetch,
    /// This sequence was already observed and is safe to ignore.
    Duplicate,
    /// One or more sequences are missing; perform bounded catch-up or resync.
    Gap,
    /// The event uses an unsupported future contract version.
    UnsupportedVersion,
}

/// Ephemeral sequence tracker for a single subscribed workflow run.
///
/// This does not imply durable resume. It exists only for duplicate and gap detection within a
/// live subscription; reconnecting clients obtain a fresh bounded snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowLiveSequence {
    last_observed: Option<u64>,
}

impl WorkflowLiveSequence {
    /// Start ephemeral observation from a snapshot/subscription watermark.
    #[must_use]
    pub const fn from_last_observed(last_observed: u64) -> Self {
        Self {
            last_observed: Some(last_observed),
        }
    }

    /// Observe one notification without mutating state for duplicates, gaps, or future versions.
    #[must_use]
    pub const fn observe(&mut self, event: &WorkflowLiveEvent) -> WorkflowLiveEventDisposition {
        if event.version != WORKFLOW_LIVE_EVENT_VERSION {
            return WorkflowLiveEventDisposition::UnsupportedVersion;
        }
        match self.last_observed {
            Some(last) if event.event_sequence <= last => WorkflowLiveEventDisposition::Duplicate,
            Some(last) if event.event_sequence != last.saturating_add(1) => {
                WorkflowLiveEventDisposition::Gap
            }
            _ => {
                self.last_observed = Some(event.event_sequence);
                WorkflowLiveEventDisposition::Refetch
            }
        }
    }

    /// Return the latest contiguous sequence observed during this live subscription.
    #[must_use]
    pub const fn last_observed(&self) -> Option<u64> {
        self.last_observed
    }
}

/// Bounded catalog snapshot of durable workflow runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCatalogView {
    /// Projection schema version.
    pub version: u32,
    /// Runs ordered by the application catalog query.
    pub runs: Vec<WorkflowRunListItem>,
}

/// Portable workflow run catalog item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunListItem {
    pub run_id: String,
    pub definition_id: String,
    pub definition_version: u32,
    pub status: WorkflowRunStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Portable workflow run state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    RepairRequired,
}

/// Bounded semantic projection of one workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunView {
    pub version: u32,
    pub run: WorkflowRunListItem,
    pub nodes: Vec<WorkflowNodeView>,
    pub edges: Vec<WorkflowEdgeView>,
    pub waits: Vec<WorkflowWaitView>,
    pub mutation_approvals: Vec<WorkflowMutationApprovalView>,
    pub attempts: Vec<WorkflowAttemptView>,
    pub outputs: Vec<WorkflowOutputView>,
    pub descendant_runs: Vec<WorkflowDescendantRunView>,
    pub child_sessions: Vec<WorkflowChildSessionView>,
    pub terminal: Option<WorkflowTerminalView>,
    pub health: WorkflowProjectionHealth,
}

/// Semantic node kind, independent of any renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Task,
    Agent,
    Branch,
    Repeat,
    Retry,
    Parallel,
    FanOut,
    PluginBlock,
    Input,
    Approval,
    WorkflowCall,
}

/// Projected node and latest bounded activation state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNodeView {
    pub node_id: String,
    pub name: String,
    pub kind: WorkflowNodeKind,
    pub activation_id: Option<String>,
    pub status: WorkflowNodeStatus,
}

/// Semantic node state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeStatus {
    NotStarted,
    Pending,
    Running,
    WaitingInput,
    WaitingApproval,
    WaitingMutationApproval,
    Completed,
    Failed,
    Cancelled,
    Skipped,
    RepairRequired,
    Unknown(String),
}

/// Portable graph edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEdgeView {
    pub from: String,
    pub to: String,
    pub kind: String,
}

/// Durable wait kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowWaitKind {
    Input,
    Approval,
}

/// Pending durable wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowWaitView {
    pub node_id: String,
    pub activation_id: String,
    pub kind: WorkflowWaitKind,
    pub input: Option<serde_json::Value>,
    pub requested_at_ms: u64,
}

/// Pending plugin mutation approval. Presentation fields cannot authorize execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowMutationApprovalView {
    pub approval_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub requested_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

/// Bounded dispatch attempt state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAttemptView {
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub status: String,
    pub has_receipt: bool,
    pub prepared_at_ms: u64,
    pub terminal_at_ms: Option<u64>,
}

/// Availability of a canonical validated output value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum WorkflowOutputValue {
    Resolved { value: serde_json::Value },
    Unresolved,
}

/// Portable typed node output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowOutputView {
    pub output_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub checksum_sha256: String,
    pub value: WorkflowOutputValue,
    pub artifact_reference: Option<String>,
    pub created_at_ms: u64,
}

/// Bounded descendant workflow run and its parent linkage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDescendantRunView {
    pub run: WorkflowRunListItem,
    pub parent_run_id: String,
    pub parent_node_id: String,
    pub depth: u32,
}

/// Link from workflow execution to a child session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowChildSessionView {
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub session_id: String,
}

/// Stable terminal outcome of a workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkflowTerminalView {
    Completed { output_id: String },
    Failed,
    Cancelled,
    RepairRequired,
}

/// Projection health when canonical or derived data cannot be represented completely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkflowProjectionHealth {
    Current,
    Degraded { reason: String },
    RepairRequired { reason: String },
    UnsupportedVersion { version: u32 },
}

#[cfg(test)]
mod live_event_tests {
    use super::*;

    fn event(version: u32, event_sequence: u64) -> WorkflowLiveEvent {
        WorkflowLiveEvent {
            version,
            run_id: "run-1".to_string(),
            event_sequence,
            changed_at_ms: event_sequence,
        }
    }

    #[test]
    fn live_sequence_detects_duplicates_gaps_and_future_versions() {
        let mut sequence = WorkflowLiveSequence::default();
        assert_eq!(
            sequence.observe(&event(WORKFLOW_LIVE_EVENT_VERSION, 4)),
            WorkflowLiveEventDisposition::Refetch
        );
        assert_eq!(sequence.last_observed(), Some(4));
        assert_eq!(
            sequence.observe(&event(WORKFLOW_LIVE_EVENT_VERSION, 4)),
            WorkflowLiveEventDisposition::Duplicate
        );
        assert_eq!(
            sequence.observe(&event(WORKFLOW_LIVE_EVENT_VERSION, 6)),
            WorkflowLiveEventDisposition::Gap
        );
        assert_eq!(sequence.last_observed(), Some(4));
        assert_eq!(
            sequence.observe(&event(WORKFLOW_LIVE_EVENT_VERSION + 1, 5)),
            WorkflowLiveEventDisposition::UnsupportedVersion
        );
        assert_eq!(sequence.last_observed(), Some(4));
        assert_eq!(
            sequence.observe(&event(WORKFLOW_LIVE_EVENT_VERSION, 5)),
            WorkflowLiveEventDisposition::Refetch
        );
        assert_eq!(sequence.last_observed(), Some(5));
    }
}
