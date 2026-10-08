//! Portable bounded workflow-lineage activity observations.
use crate::SessionProgressSnapshot;
use serde::{Deserialize, Serialize};

/// Independent public activity contract version.
pub const WORKFLOW_ACTIVITY_VERSION: u32 = 2;

/// Content-free canonical stage transitions, never a polling or heartbeat counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStageProgress {
    /// Opaque canonical dispatch identity.
    pub source_id: String,
    /// Attempt-scoped comparison epoch.
    pub epoch: String,
    /// Preparation, admission and terminal settlement are transitions 1, 2 and 3.
    pub sequence: u64,
    /// Verified canonical transition time in Unix milliseconds.
    pub last_progress_at_ms: u64,
}

/// Canonical dispatch owner semantics, not inferred from missing session links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAttemptSource {
    /// Agent turn requires verified session provenance and live coverage.
    LinkedSession,
    /// Sessionless plugin execution uses canonical stage transitions.
    RuntimeStage,
    /// Workflow call requires recursive child inventory coverage.
    ChildWorkflow,
}

/// Typed readiness and progress; absence/unknown never proves silence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptObservation {
    /// Retry with fresh scans within an explicit readiness allowance, not an idle reset.
    PendingReadiness {
        /// Expected canonical execution source.
        expected_source: WorkflowAttemptSource,
        /// Preparation/admission progress, independent of readiness.
        stage: WorkflowStageProgress,
    },
    /// Verified session source; unavailable live coverage still fails closed.
    LinkedSession {
        /// Normalized session progress.
        progress: SessionProgressSnapshot,
        /// Canonical attempt progress.
        stage: WorkflowStageProgress,
    },
    /// Sessionless execution; unchanged admitted rows are not progress.
    RuntimeStage {
        /// Canonical transition marker.
        progress: WorkflowStageProgress,
    },
    /// Parent marker alone cannot certify descendant silence.
    ChildWorkflow {
        /// Verified canonical direct child.
        child_run_id: String,
        /// Parent attempt transitions.
        stage: WorkflowStageProgress,
    },
    /// Stable terminal ledger outcome.
    Terminal {
        /// Canonical terminal transition.
        stage: WorkflowStageProgress,
    },
    /// Unsupported source semantics cannot certify idleness.
    Unavailable {
        /// Typed coverage failure.
        reason: WorkflowAttemptUnavailableReason,
    },
}

/// Content-free source classification failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAttemptUnavailableReason {
    /// Canonical owner is unknown or unsupported.
    UnknownDispatchSource,
}

/// Inventory dimension; recursively expand every child-run page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowActivitySection {
    /// Includes unresolved attempts with missing execution-session provenance.
    Attempts,
    /// Direct workflow descendants, not ordinary session children.
    Children,
}

/// Read-only bounded request for one run in a recursively discovered workflow subtree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowActivityPageRequest {
    /// Independent contract version; unknown versions are rejected.
    pub version: u32,
    /// Ephemeral retained snapshot identity; None starts a new scan. Never durably resumable.
    pub scan_id: Option<String>,
    /// Root workflow scope shared by every recursive page in this scan.
    pub root_run_id: String,
    /// Exact run identity. No field conveys execution authority.
    pub run_id: String,
    /// Expected graph revision from the initial page, if continuing a scan.
    pub expected_revision: Option<u64>,
    /// Inventory dimension to page.
    pub section: WorkflowActivitySection,
    /// Opaque exclusive continuation bound to run/revision/section.
    pub cursor: Option<String>,
    /// Positive per-request work bound, at most 100; not a graph-size cap.
    pub limit: u16,
}

/// Typed active-attempt observation with content-free session progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowActivityAttempt {
    /// Canonical dispatch identity.
    pub dispatch_identity: String,
    /// Exact node identity.
    pub node_id: String,
    /// Exact activation identity, including historical unresolved work.
    pub activation_id: String,
    /// Attempt number.
    pub attempt: u32,
    /// Whether a canonical terminal outcome is present.
    pub terminal: bool,
    /// Linked execution session identity, absent when no verified link exists.
    pub session_id: Option<String>,
    /// Source/readiness semantics; required by activity contract v2.
    pub observation: AttemptObservation,
    /// Live own-session marker; absent is NOT idle. Terminal attempts need no live marker.
    pub progress: Option<SessionProgressSnapshot>,
}

/// Verified direct workflow lineage coordinates without private run payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowActivityChild {
    /// Child identity for recursive expansion.
    pub child_run_id: String,
    /// Verified direct parent identity.
    pub parent_run_id: String,
}

/// Bounded observation, not a durable stream or subtree snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowActivityPage {
    /// Independent contract version.
    pub version: u32,
    /// Ephemeral retained snapshot identity; expired or evicted IDs fail closed.
    pub scan_id: String,
    /// Root scope of this snapshot.
    pub root_run_id: String,
    /// Lower bound on snapshot establishment, in Unix milliseconds.
    pub snapshot_started_at_ms: u64,
    /// Server-monotonic age at response construction, not a progress timestamp.
    pub snapshot_age_ms: u64,
    /// Configured absolute scan lifetime; requests do not refresh it.
    pub scan_lifetime_ms: u64,
    /// False: snapshot inventory plus later live reads do not certify current idleness.
    pub current_idle_eligible: bool,
    /// Exact queried run.
    pub run_id: String,
    /// Graph revision observed in the inventory transaction.
    pub revision: u64,
    /// Queried inventory dimension.
    pub section: WorkflowActivitySection,
    /// Scanned attempt entries, even when none have session links.
    pub attempts: Vec<WorkflowActivityAttempt>,
    /// Direct workflow child links.
    pub children: Vec<WorkflowActivityChild>,
    /// Exclusive continuation; exhaustion is not proof of subtree stability.
    pub next_cursor: Option<String>,
    /// True only when this transaction exhausted the requested inventory dimension.
    pub inventory_exhausted: bool,
    /// Server-certified recursive inventory exhaustion in the retained snapshot.
    /// True only after both sections of every discovered workflow descendant have
    /// been paged. This is as-of coverage, never current idleness or durable resume.
    pub subtree_coverage_complete: bool,
    /// Normalized limitations, never raw database errors or private provider data.
    pub issues: Vec<String>,
}
