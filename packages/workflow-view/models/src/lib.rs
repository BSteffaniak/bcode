#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable, renderer-neutral workflow projection contracts.

use serde::{Deserialize, Serialize};

/// Current workflow projection schema version.
pub const WORKFLOW_VIEW_VERSION: u32 = 2;

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

/// Error returned when a workflow projection uses an unsupported contract version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedWorkflowViewVersion {
    pub version: u32,
}

impl std::fmt::Display for UnsupportedWorkflowViewVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "unsupported workflow view version {}; expected {WORKFLOW_VIEW_VERSION}",
            self.version
        )
    }
}

impl std::error::Error for UnsupportedWorkflowViewVersion {}

/// Portable catalog filter applied by the workflow application boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCatalogFilter {
    Active,
    NeedsAttention,
    Failed,
    Completed,
    #[default]
    All,
}

/// Portable deterministic catalog ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCatalogSort {
    #[default]
    UpdatedAt,
    CreatedAt,
    Status,
}

/// Portable catalog grouping preference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCatalogGroup {
    #[default]
    None,
    AuthoredWorkflow,
    Definition,
}

/// Stable keyset cursor for a bounded workflow catalog page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCatalogCursor {
    pub sort: WorkflowCatalogSort,
    pub timestamp_ms: u64,
    pub status_rank: u8,
    pub run_id: String,
}

/// Renderer-neutral bounded workflow catalog request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCatalogRequest {
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<WorkflowCatalogCursor>,
    #[serde(default)]
    pub filter: WorkflowCatalogFilter,
    #[serde(default)]
    pub sort: WorkflowCatalogSort,
    #[serde(default)]
    pub group: WorkflowCatalogGroup,
    /// Bounded case-insensitive search over approved run display fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

/// Bounded catalog page of durable workflow runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCatalogView {
    /// Projection schema version.
    pub version: u32,
    /// Runs ordered by the application catalog query.
    pub runs: Vec<WorkflowRunListItem>,
    /// Cursor for the next deterministic page.
    pub next_cursor: Option<WorkflowCatalogCursor>,
    /// True when another bounded page follows this page.
    pub has_more: bool,
    pub filter: WorkflowCatalogFilter,
    pub sort: WorkflowCatalogSort,
    pub group: WorkflowCatalogGroup,
    pub search: Option<String>,
}

impl WorkflowCatalogView {
    /// Reject projections that do not use the exact supported contract version.
    ///
    /// # Errors
    ///
    /// Returns the unsupported version without guessing future semantics.
    pub const fn validate_version(&self) -> Result<(), UnsupportedWorkflowViewVersion> {
        if self.version == WORKFLOW_VIEW_VERSION {
            Ok(())
        } else {
            Err(UnsupportedWorkflowViewVersion {
                version: self.version,
            })
        }
    }
}

/// Authored source identity for a workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoredSourceView {
    /// Stable authored workflow identity.
    pub workflow_id: String,
    /// Exact immutable published revision used by this run.
    pub revision: u64,
}

/// Renderer-neutral definition navigation disposition for a workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowDefinitionDisposition {
    /// This run uses an exact published authored revision.
    Published {
        workflow_id: String,
        revision: u64,
        editable_draft_id: Option<String>,
    },
    /// This run uses a compiled definition with no authored editing boundary.
    CompiledOnly,
}

/// Portable bounded run-progress counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunProgress {
    pub total_nodes: u32,
    pub not_started: u32,
    pub active: u32,
    pub blocked: u32,
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub skipped: u32,
    pub repair_required: u32,
}

/// Portable bounded summary of workflow states requiring attention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAttentionSummary {
    pub pending_inputs: u32,
    pub pending_approvals: u32,
    pub pending_mutation_approvals: u32,
    pub retryable_failures: u32,
    pub repair_required: bool,
}

impl WorkflowAttentionSummary {
    /// Return whether the run currently needs operator attention.
    #[must_use]
    pub const fn needs_attention(&self) -> bool {
        self.pending_inputs > 0
            || self.pending_approvals > 0
            || self.pending_mutation_approvals > 0
            || self.retryable_failures > 0
            || self.repair_required
    }
}

/// Portable workflow run catalog item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunListItem {
    pub run_id: String,
    /// Primary user-facing identity.
    pub display_title: String,
    /// Optional product binding label.
    pub binding_label: Option<String>,
    pub definition_id: String,
    pub definition_version: u32,
    pub authored_source: Option<WorkflowAuthoredSourceView>,
    pub definition_disposition: WorkflowDefinitionDisposition,
    pub parent_run_id: Option<String>,
    pub descendant_count: u32,
    pub progress: WorkflowRunProgress,
    pub attention: WorkflowAttentionSummary,
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

/// Renderer-neutral action kind exposed only as a presentation affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowActionKind {
    Pause,
    Resume,
    Cancel,
    ProvideInput,
    Approve,
    Deny,
    ApproveMutation,
    DenyMutation,
    RetryNode,
    OpenSession,
    ViewDefinition,
    EditDraft,
    ForkDefinition,
}

/// Exact stable identity for one presentation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowActionTarget {
    Run {
        run_id: String,
    },
    Activation {
        run_id: String,
        node_id: String,
        activation_id: String,
    },
    MutationApproval {
        approval_id: String,
    },
    Attempt {
        run_id: String,
        node_id: String,
        activation_id: String,
        attempt: u32,
    },
    Session {
        session_id: String,
    },
    PublishedDefinition {
        workflow_id: String,
        revision: u64,
    },
    Draft {
        workflow_id: String,
        draft_id: String,
    },
}

/// Presentation-only action availability. The application boundary revalidates every request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowActionAffordance {
    pub kind: WorkflowActionKind,
    pub target: WorkflowActionTarget,
    pub enabled: bool,
    pub unavailable_reason: Option<String>,
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
    pub actions: Vec<WorkflowActionAffordance>,
    pub terminal: Option<WorkflowTerminalView>,
    pub health: WorkflowProjectionHealth,
}

impl WorkflowRunView {
    /// Reject projections that do not use the exact supported contract version.
    ///
    /// # Errors
    ///
    /// Returns the unsupported version without guessing future semantics.
    pub const fn validate_version(&self) -> Result<(), UnsupportedWorkflowViewVersion> {
        if self.version == WORKFLOW_VIEW_VERSION {
            Ok(())
        } else {
            Err(UnsupportedWorkflowViewVersion {
                version: self.version,
            })
        }
    }
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
    /// Bounded user-facing prompt derived from the exact node definition.
    pub prompt: String,
    /// Expected value schema for input waits.
    pub expected_schema: Option<serde_json::Value>,
    /// Current bounded activation input summary.
    pub input: Option<serde_json::Value>,
    pub requested_at_ms: u64,
}

/// Side-effect class of a pending mutation operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOperationEffect {
    Mutating,
}

/// Portable resource claim shown for informed approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowResourceClaimView {
    pub resource: String,
    pub access: String,
}

/// Pending plugin mutation approval. Presentation fields cannot authorize execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowMutationApprovalView {
    pub approval_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub plugin_id: String,
    pub block_id: String,
    pub block_version: u32,
    pub operation: String,
    pub effect: WorkflowOperationEffect,
    pub input_summary: serde_json::Value,
    pub resource_claims: Vec<WorkflowResourceClaimView>,
    pub workspace_snapshot: String,
    pub reconciliation_warning: Option<String>,
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

    #[test]
    fn projection_versions_fail_closed() {
        let catalog = WorkflowCatalogView {
            version: WORKFLOW_VIEW_VERSION + 1,
            runs: Vec::new(),
            next_cursor: None,
            has_more: false,
            filter: WorkflowCatalogFilter::All,
            sort: WorkflowCatalogSort::UpdatedAt,
            group: WorkflowCatalogGroup::None,
            search: None,
        };
        assert_eq!(
            catalog.validate_version(),
            Err(UnsupportedWorkflowViewVersion {
                version: WORKFLOW_VIEW_VERSION + 1,
            })
        );
    }

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
