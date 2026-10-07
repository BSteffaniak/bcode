#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Durable workflow persistence owned independently from session transcript storage.

use bcode_workflow::{
    WORKFLOW_COMPILATION_PREVIEW_VERSION, WorkflowAuthoringDocument, WorkflowAuthoringListCursor,
    WorkflowBlockDefinition, WorkflowCompilationPreview, WorkflowDefinition,
    WorkflowDefinitionIdentity, WorkflowNodeDataflowPolicy, WorkflowPackageApplyRequest,
    WorkflowPackageLock, WorkflowPackageMutationMemberResult, WorkflowPackageMutationOutcome,
    WorkflowPackageMutationResult, WorkflowPackagePublishRequest, WorkflowProducerProvenance,
    WorkflowRevisionListCursor, WorkflowRunLimitPolicy, prepare_workflow_node_dataflow,
    validate_persistable_authoring_value,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use thiserror::Error;

use switchy_database::query::{
    FilterableQuery as _, JsonPath, JsonType, Or, Projection, SortDirection, bounded_text,
    count_all, delete, identifier, join_columns, json_type_is, qualified_column as col, select,
    update, where_and, where_eq, where_eq_expression, where_gt, where_in, where_not_eq_expression,
    where_or,
};
use switchy_database::schema::{
    Column, DataType, SchemaComparison as C, SchemaExpression as E, alter_table, create_table,
};
use switchy_database::{DatabaseValue as V, Row};
use switchy_database_connection::SqliteAccess;
mod admission;
mod authoring;
#[cfg(test)]
mod backup_tests;
mod composition;
use composition::{
    fan_out_member_controller, settle_fan_out_member_failure, settle_fan_out_member_success,
    settle_parallel_failure, validate_parallel_run_topology,
};
mod controls;
use controls::{pending_replacement_in, sibling_cancellation_requested_for_attempt};
mod continuation;
mod database;
mod dispatch;
mod history;
mod offline_recovery;
mod output;
mod permissions;
use output::{
    activation_input, activation_output_value, activation_output_value_by_identity,
    configured_node_ids, dependency_input_edge, evaluate_predicate,
    materialize_selected_successors, persist_validated_output_transaction,
    settle_output_successors, stored_activation_output, successor_already_admitted,
    successor_dependencies_ready, transform_run_json,
};
mod receipts;
use receipts::fan_out_retirement_pending;
mod recovery;
/// Typed additive run-representation admission; executable lifecycle remains unavailable.
pub mod representation;
mod run_admission;
mod run_graph;
mod scheduler_pages;
mod schema;
mod storage_compatibility;
pub use run_graph::{RunGraphCandidateValidation, RunGraphEdge, RunGraphNode, RunGraphPage};
pub use scheduler_pages::WorkflowConcurrencySnapshot;

const DATABASE_FILE: &str = "workflow.db";
const LOCK_FILE: &str = "workflow.lock";
const ARTIFACT_DIRECTORY: &str = "artifacts";
const RESET_RECEIPT_FILE: &str = "workflow-reset-receipt.json";
const MIGRATION_RECEIPT_FILE: &str = "workflow-migration-receipt.json";
const MIGRATION_BACKUP_DIRECTORY: &str = "migration-backups";
const RESET_BACKUP_DIRECTORY: &str = "reset-backups";
/// Stable destructive confirmation required by public workflow-store reset surfaces.
pub const WORKFLOW_STORE_RESET_CONFIRMATION: &str = "DELETE-INCOMPATIBLE-WORKFLOW-STATE";
/// Current clean-break workflow store schema version.
pub const WORKFLOW_STORE_SCHEMA_VERSION: u32 = 43;
// Changes only when existing readers/writers would become unsafe, not for additive DDL.
const WORKFLOW_STORE_COMPATIBILITY: u32 = 3;
/// Current bounded workflow-store reset receipt version.
pub const WORKFLOW_STORE_RESET_RECEIPT_VERSION: u32 = 1;
/// Current explicit workflow-store migration receipt contract.
pub const WORKFLOW_STORE_MIGRATION_RECEIPT_VERSION: u32 = 1;
/// Current durable workflow execution-session link contract version.
pub const WORKFLOW_EXECUTION_SESSION_LINK_VERSION: u32 = 1;
/// Current durable authored-run provenance contract version.
pub use bcode_workflow::AUTHORED_WORKFLOW_RUN_PROVENANCE_VERSION;
const MAX_ID_BYTES: usize = 512;
const MAX_DISPLAY_LABEL_BYTES: usize = 512;
const MAX_INLINE_JSON_BYTES: usize = 1_048_576;
/// Maximum canonical activation input copied into one normal workflow-view summary.
pub const WORKFLOW_ACTIVATION_INPUT_SUMMARY_MAX_BYTES: usize = 4_096;

/// One bounded internal authored-list page with stable continuation knowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowAuthoringStorePage<T> {
    /// Items in stable query order, never exceeding the requested page size.
    pub items: Vec<T>,
    /// Whether at least one item follows this page.
    pub has_more: bool,
}

fn finish_authoring_store_page<T>(
    mut items: Vec<T>,
    limit: usize,
) -> WorkflowAuthoringStorePage<T> {
    let has_more = items.len() > limit;
    items.truncate(limit);
    WorkflowAuthoringStorePage { items, has_more }
}

/// Compatibility export of the workflow-owned run contract.
pub use bcode_workflow::RunStatus;

/// Compatibility export of the workflow-owned inspection contract.
pub use bcode_workflow::DispatchSideEffect;

/// Deterministic fault boundaries for atomic package mutation tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowPackageMutationBoundary {
    /// One package member has been staged inside the transaction.
    MemberStaged,
    /// Every member has been staged but the transaction has not committed.
    AllMembersStaged,
}

/// Deterministic package mutation fault injection.
pub trait WorkflowPackageMutationFault {
    /// Fail after one exact transactional boundary.
    ///
    /// # Errors
    ///
    /// Returns an error to force complete transaction rollback.
    fn after_boundary(
        &self,
        boundary: WorkflowPackageMutationBoundary,
        member_id: Option<&str>,
    ) -> Result<(), WorkflowStoreError>;
}

struct NoopWorkflowPackageMutationFault;

impl WorkflowPackageMutationFault for NoopWorkflowPackageMutationFault {
    fn after_boundary(
        &self,
        _boundary: WorkflowPackageMutationBoundary,
        _member_id: Option<&str>,
    ) -> Result<(), WorkflowStoreError> {
        Ok(())
    }
}

/// Compatibility export of the workflow-owned run contract.
pub use bcode_workflow::AuthoredWorkflowRunProvenance;

/// Compatibility export of the workflow-owned run contract.
pub use bcode_workflow::WorkflowRunSummary;

/// Bounded derived counts used by workflow catalog presentation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowRunCatalogSummary {
    pub total_nodes: u32,
    pub not_started: u32,
    pub active: u32,
    pub blocked: u32,
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub skipped: u32,
    pub repair_required: u32,
    pub pending_inputs: u32,
    pub pending_approvals: u32,
    pub pending_mutation_approvals: u32,
    pub retryable_failures: u32,
    pub descendant_count: u32,
}

/// Store-owned catalog filter for bounded run discovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkflowRunCatalogFilter {
    Active,
    NeedsAttention,
    Failed,
    Completed,
    #[default]
    All,
}

/// Store-owned deterministic catalog ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkflowRunCatalogSort {
    #[default]
    UpdatedAt,
    CreatedAt,
    Status,
}

/// Store-owned keyset cursor for one run catalog query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunCatalogCursor {
    pub sort: WorkflowRunCatalogSort,
    pub timestamp_ms: u64,
    pub status_rank: u8,
    pub run_id: String,
}

/// Bounded store query for workflow catalog rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunCatalogQuery {
    pub limit: usize,
    pub cursor: Option<WorkflowRunCatalogCursor>,
    pub filter: WorkflowRunCatalogFilter,
    pub sort: WorkflowRunCatalogSort,
    pub search: Option<String>,
}

/// Stable cursor and presentation data for one bounded workflow catalog row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunCatalogEntry {
    pub run: WorkflowRunSummary,
    pub summary: WorkflowRunCatalogSummary,
}

/// One bounded stable workflow run catalog page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunCatalogPage {
    pub entries: Vec<WorkflowRunCatalogEntry>,
    pub has_more: bool,
}

/// Bounded pending activation ready for host admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingActivation {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub dependency_generation: u64,
    pub input: Option<serde_json::Value>,
    pub node: bcode_workflow::NodeDefinition,
    pub created_at_ms: u64,
}

/// Compatibility export of the workflow-owned inspection contract.
pub use bcode_workflow::WorkflowWaitKind;

/// Compatibility export of the workflow-owned inspection contract.
pub use bcode_workflow::WaitingActivation;

/// Compatibility export of the workflow-owned inspection contract.
pub use bcode_workflow::WorkflowActivationInputSummary;

/// Compatibility export of the workflow-owned inspection contract.
pub use bcode_workflow::WorkflowActivationSummary;

pub use bcode_workflow::WaitingResolutionResult;

pub use bcode_workflow::AttemptCursor;

/// Compatibility export of the workflow-owned inspection contract.
pub use bcode_workflow::AttemptSummary;

/// Persisted automatic-retry backoff schedule for one exact next attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomaticRetrySchedule {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub failed_attempt: u32,
    pub next_attempt: u32,
    pub failure_kind: bcode_workflow::AutomaticRetryFailureKind,
    pub backoff_ms: u64,
    pub next_attempt_at_ms: u64,
    pub scheduled_at_ms: u64,
}

/// One bounded workflow event row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowEventRow {
    pub event_seq: u64,
    pub run_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at_ms: u64,
}

/// Compatibility export of workflow-owned run allowance data.
pub use bcode_workflow::WorkflowRunLimits;

/// Compatibility export of the workflow-owned run contract.
pub use bcode_workflow::WorkflowRunBinding;

/// Generic associated-run lookup key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunBindingKey {
    pub owner_plugin_id: String,
    pub workflow_kind: String,
    pub scope_key: String,
}

/// Derive the deterministic child run identity for one exact parent attempt and target.
/// # Panics
///
/// Panics only if the typed target identity tuple cannot be serialized, which is impossible for
/// its serde-derived fields.
#[must_use]
pub fn workflow_child_run_id(
    root_run_id: &str,
    parent_run_id: &str,
    parent_activation_id: &str,
    parent_attempt: u32,
    target: &bcode_workflow::WorkflowCallTarget,
) -> String {
    let encoded = serde_json::to_vec(&(
        root_run_id,
        parent_run_id,
        parent_activation_id,
        parent_attempt,
        target,
    ))
    .expect("workflow child identity tuple is serializable");
    format!("workflow-child:{}", sha256_hex(&encoded))
}

/// Current durable parent/child workflow link contract version.
pub const WORKFLOW_RUN_LINK_VERSION: u32 = 1;
/// Current maximum child-workflow nesting depth, including the root run.
pub const MAX_WORKFLOW_RUN_DEPTH: u32 = bcode_workflow::MAX_WORKFLOW_CALL_DEPTH;
/// Current maximum descendants beneath one root run.
pub const MAX_WORKFLOW_RUN_DESCENDANTS: u32 = bcode_workflow::MAX_WORKFLOW_CALL_DESCENDANTS;

/// Compatibility export of the workflow-owned composition observation contract.
pub use bcode_workflow::WorkflowRunLink;

/// Atomic exact child-run creation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewChildWorkflowRun {
    pub link: WorkflowRunLink,
    pub run: NewWorkflowRun,
}

/// Compatibility export of the workflow-owned composition observation contract.
pub use bcode_workflow::WorkflowDescendantRunSummary;

/// Durable authority required to mutate one active workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowExecutionAuthority {
    /// Exact daemon artifact selected when the run was admitted.
    pub target_artifact_id: String,
    /// Current coordinator daemon instance.
    pub daemon_instance_id: String,
    /// Monotonic ownership generation.
    pub generation: u64,
    /// Opaque fencing token changed on every ownership transfer.
    pub fencing_token: String,
}

/// Durable evidence that a run's recorded coordinator daemon ended.
///
/// Persisted with every cross-artifact authority reassignment so operators can audit why a run
/// changed hands. Every field is derived from canonical session-owner observations or daemon
/// registry state; none is inferred from the run's own contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndedOwnerEvidence {
    /// Coordinator daemon instance that no longer owns the run.
    pub ended_daemon_instance_id: String,
    /// Artifact the ended coordinator belonged to.
    pub ended_target_artifact_id: String,
    /// How the prior owner's end was established.
    pub liveness: EndedOwnerLiveness,
    /// Whether the ended artifact's executable image was still available when reassigned.
    pub artifact_image_available: bool,
}

/// How a prior coordinator's end was proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedOwnerLiveness {
    /// Session-owner observations positively reported the recorded daemon process as ended.
    ObservedEnded,
    /// No session-owner observation named the recorded daemon and no daemon registry record
    /// claims that instance; the owner left no live trace.
    NoLiveTrace,
}

/// Durable workflow run creation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewWorkflowRun {
    /// Stable run identity.
    pub run_id: String,
    /// Definition identity and version.
    pub definition_id: String,
    pub definition_version: u32,
    /// Immutable workspace snapshot identity.
    pub workspace_snapshot: String,
    /// Optional parent session identity serialized without coupling this store to session logic.
    pub parent_session_id: Option<String>,
    /// Exact accepted parent-session generation pinned at root admission when workflow prompts use
    /// fixed-generation context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_generation: Option<u64>,
    /// Optional bounded product ownership and discovery association.
    #[serde(default)]
    pub binding: Option<WorkflowRunBinding>,
    /// Optional exact authored source and resolved configuration. This is persisted atomically
    /// with run creation and must be supplied only for published authored revisions.
    #[serde(default)]
    pub authored_provenance: Option<AuthoredWorkflowRunProvenance>,
    /// Optional bounded initial input validated against the definition input schema.
    pub input: Option<serde_json::Value>,
    /// Current durable execution authority. Production active runs must provide this; `None` is
    /// retained for isolated library use and historical terminal data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_authority: Option<WorkflowExecutionAuthority>,
    /// Creation timestamp supplied by the host clock.
    pub created_at_ms: u64,
    /// Exact normalized non-secret policy/profile identity pinned before any run side effect.
    pub authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity,
    /// Maximum capability inherited from the initiating context for this run and descendants.
    /// This immutable value is the run's pinned authorization profile: every attempt still requires
    /// normalized operation authorization, while no retry, restart, or child may exceed this ceiling.
    #[serde(default)]
    pub authorization_ceiling: bcode_workflow::WorkflowToolCapability,
    /// Persisted execution limits enforced by durable admission.
    pub limits: WorkflowRunLimits,
}

pub use bcode_workflow::ReplacementReadiness;

pub use bcode_workflow::NewActivation;

/// Prepared external-operation intent written before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedAttempt {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub side_effect: DispatchSideEffect,
    /// Owner-specific bounded dispatch intent.
    pub intent: serde_json::Value,
    pub prepared_at_ms: u64,
}

impl PreparedAttempt {
    /// Return the stable dispatch identity derived solely from durable attempt identity.
    #[must_use]
    pub fn dispatch_identity(&self) -> String {
        dispatch_identity(
            &self.run_id,
            &self.node_id,
            &self.activation_id,
            self.attempt,
        )
    }
}

/// One exact workflow activation/attempt to session relationship.
///
/// This row is workflow-owned recovery state only. Session content, visibility, retention, and
/// history remain canonical in the session domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowExecutionSessionLink {
    /// Link contract version.
    pub version: u32,
    /// Owning workflow run.
    pub run_id: String,
    /// Owning workflow node.
    pub node_id: String,
    /// Exact logical activation.
    pub activation_id: String,
    /// Exact dispatch attempt.
    pub attempt: u32,
    /// Session-domain identity, treated as an opaque typed string by workflow persistence.
    pub session_id: String,
    /// Exact immutable workspace snapshot associated with the execution session.
    pub workspace_snapshot: String,
    /// Time the relationship was accepted durably.
    pub created_at_ms: u64,
}

impl WorkflowExecutionSessionLink {
    /// Construct the current version of an execution-session link.
    #[must_use]
    pub const fn new(
        run_id: String,
        node_id: String,
        activation_id: String,
        attempt: u32,
        session_id: String,
        workspace_snapshot: String,
        created_at_ms: u64,
    ) -> Self {
        Self {
            version: WORKFLOW_EXECUTION_SESSION_LINK_VERSION,
            run_id,
            node_id,
            activation_id,
            attempt,
            session_id,
            workspace_snapshot,
            created_at_ms,
        }
    }
}

/// Durable dispatch receipt returned by an external operation owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchReceipt {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub receipt: serde_json::Value,
    pub admitted_at_ms: u64,
}

/// One active durable workflow attempt to signal after cancellation intent commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveAttemptCancellation {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub receipt: Option<serde_json::Value>,
}

/// Result of signaling active attempt owners after durable cancellation intent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CancellationPropagationSummary {
    pub signalled: Vec<String>,
    pub already_terminal: Vec<String>,
}

/// Owner boundary for active workflow attempt cancellation.
pub trait AttemptCancellationOwner: Sync {
    /// Signal one active owner using its stable dispatch identity and optional receipt.
    ///
    /// Implementations must be idempotent for repeated calls with the same request.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner cannot durably accept or safely repeat cancellation.
    fn cancel_attempt<'a>(
        &'a self,
        request: &'a ActiveAttemptCancellation,
    ) -> Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>>;
}

/// Compatibility export of the workflow-owned observation contract.
pub use bcode_workflow::WorkflowDecision;

/// Compatibility export of the workflow-owned observation contract.
pub use bcode_workflow::WorkflowGrant;

/// Compatibility export of the workflow-owned observation contract.
pub use bcode_workflow::WorkflowMutationApproval;

/// Compatibility exports of workflow-owned mutation resolution contracts.
pub use bcode_workflow::{WorkflowMutationApprovalDecision, WorkflowMutationApprovalResolution};

/// Compatibility export of the workflow-owned observation contract.
pub use bcode_workflow::ResourceLeaseMode;

/// Compatibility export of the workflow-owned observation contract.
pub use bcode_workflow::WorkflowResourceLease;

/// One durable workflow projection checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowProjectionCheckpoint {
    pub projection_name: String,
    pub projection_version: u32,
    pub last_event_seq: u64,
}

/// Result of bounded restart reconciliation for prepared attempts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconciliationSummary {
    /// Mutating attempts moved to repair-required because no receipt proves their outcome.
    pub repair_required: Vec<String>,
    /// Read-only prepared attempts left eligible for owner-specific redispatch/reconciliation.
    pub safe_prepared: Vec<String>,
}

/// Receipt-backed attempt requiring bounded owner observation after restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptReconciliationRequest {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub side_effect: DispatchSideEffect,
    pub receipt: serde_json::Value,
}

impl AttemptReconciliationRequest {
    /// Return whether this attempt may have committed mutations.
    #[must_use]
    pub const fn may_mutate(&self) -> bool {
        matches!(self.side_effect, DispatchSideEffect::Mutating)
    }
}

/// Recoverable owner outcome that pauses a workflow attempt without treating it as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptPauseReason {
    /// The selected provider is unavailable and may become available later.
    ProviderUnavailable,
    /// The owner reached its idle timeout and requires explicit operator resume.
    IdleTimeout,
    /// The owner exhausted its bounded tool-call rounds.
    ToolRoundLimitReached,
    /// The work was steered or cancelled independently of whole-run cancellation.
    Steering,
}

/// Owner-reported durable external attempt state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AttemptObservation {
    /// Owner still recognizes admission but work has not started.
    Admitted,
    /// External work is active.
    Running,
    /// Observation is intentionally deferred because another live or artifact-specific owner is
    /// authoritative. Deferral never mutates durable attempt state.
    Deferred { reason: String },
    /// External work completed with schema-validated output.
    Succeeded { output: ValidatedOutput },
    /// External work failed terminally and must not be retried automatically.
    Failed { message: String },
    /// A trustworthy owner receipt reports a terminal failure that is explicitly retryable.
    RetryableFailure { message: String },
    /// External work stopped in a recoverable state and requires explicit run resume.
    Paused {
        /// Stable owner-neutral reason for the pause.
        reason: AttemptPauseReason,
        /// Human-readable owner diagnostic.
        message: String,
    },
    /// External work was cancelled terminally.
    Cancelled,
    /// Owner cannot prove the current or terminal state.
    Unknown,
}

/// Owner boundary used for bounded restart reconciliation.
pub trait AttemptStatusObserver {
    /// Observe one receipt-backed attempt without mutating workflow storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner status API cannot be queried safely.
    fn observe(
        &self,
        request: &AttemptReconciliationRequest,
    ) -> Result<AttemptObservation, WorkflowStoreError>;
}

/// Asynchronous owner boundary used when status lives behind an async host projection.
pub trait AsyncAttemptStatusObserver: Sync {
    /// Observe one receipt-backed attempt without mutating workflow storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded owner status API cannot be queried safely.
    fn observe_async<'a>(
        &'a self,
        request: &'a AttemptReconciliationRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>;
}

/// Summary of receipt-backed restart reconciliation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiptReconciliationSummary {
    pub admitted: Vec<String>,
    pub running: Vec<String>,
    /// Attempts intentionally left unchanged because another owner remains authoritative.
    pub deferred: Vec<String>,
    pub succeeded: Vec<String>,
    pub failed: Vec<String>,
    /// Durable retries scheduled from exact owner observations and node policy.
    pub retries_scheduled: Vec<AutomaticRetrySchedule>,
    /// Active sibling attempts whose cancellation intent committed during fail-fast settlement.
    pub sibling_cancellations: Vec<ActiveAttemptCancellation>,
    pub paused: Vec<String>,
    pub cancelled: Vec<String>,
    pub repair_required: Vec<String>,
    pub unresolved_read_only: Vec<String>,
}

pub use bcode_workflow::{WorkflowDoctorIssue, WorkflowDoctorReport};

pub use bcode_workflow::{RepairResolution, RepairResult};

/// Optional fault hook used by deterministic output/downstream crash-boundary acceptance tests.
pub trait WorkflowOutputFault: Sync {
    /// Observe one boundary inside the atomic output transaction.
    ///
    /// # Errors
    ///
    /// Returns an injected error, causing the entire output transaction to roll back.
    fn after_boundary(
        &self,
        boundary: WorkflowOutputBoundary,
        output: &ValidatedOutput,
    ) -> Result<(), WorkflowStoreError>;
}

/// Output transaction boundaries covered by durable crash acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowOutputBoundary {
    OutputInserted,
    ActivationCompleted,
    BranchDecisionPersisted,
    SuccessorsMaterialized,
}

/// Production no-op output fault hook.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWorkflowOutputFault;

impl WorkflowOutputFault for NoopWorkflowOutputFault {
    fn after_boundary(
        &self,
        _boundary: WorkflowOutputBoundary,
        _output: &ValidatedOutput,
    ) -> Result<(), WorkflowStoreError> {
        Ok(())
    }
}

/// Optional fault hook used by deterministic crash-boundary acceptance tests.
///
/// Production callers use [`NoopWorkflowDispatchFault`]. Hooks run only after the named durable or
/// external boundary has completed and before the next boundary begins.
pub trait WorkflowDispatchFault: Sync {
    /// Observe one dispatch boundary.
    ///
    /// # Errors
    ///
    /// Returns an injected error to simulate process loss at this exact boundary.
    fn after_boundary(
        &self,
        boundary: WorkflowDispatchBoundary,
        request: &PreparedActivationDispatch,
    ) -> Result<(), WorkflowStoreError>;
}

/// External-dispatch crash boundaries covered by the durable protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowDispatchBoundary {
    IntentCommitted,
    OwnerAccepted,
    ReceiptCommitted,
}

/// Production no-op dispatch fault hook.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWorkflowDispatchFault;

impl WorkflowDispatchFault for NoopWorkflowDispatchFault {
    fn after_boundary(
        &self,
        _boundary: WorkflowDispatchBoundary,
        _request: &PreparedActivationDispatch,
    ) -> Result<(), WorkflowStoreError> {
        Ok(())
    }
}

/// Owner-specific plan persisted before one pending activation is dispatched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationDispatchPlan {
    pub side_effect: DispatchSideEffect,
    pub intent: serde_json::Value,
}

/// Result of bounded owner dispatch from pending activation snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivationDispatchSummary {
    pub admitted: Vec<String>,
    pub unsupported: Vec<String>,
    pub raced: Vec<String>,
    /// Pending activations blocked by the run or root's durable execution allowance.
    pub exhausted: Vec<String>,
}

/// Host owner boundary for executable durable workflow activations.
pub trait ActivationDispatchOwner: Sync {
    /// Build the exact bounded intent that must be committed before external dispatch.
    /// Blocks with `preparation_required` must complete the versioned owner preparation contract;
    /// `operation_facts`, descriptor, owner identity, exact input checksum, and preparation version
    /// are persisted opaquely in this intent before authorization or dispatch.
    ///
    /// Returning `None` leaves an unsupported activation pending for another owner.
    ///
    /// # Errors
    ///
    /// Returns an error when the node contract is malformed or policy admission fails.
    fn plan<'a>(
        &'a self,
        activation: &'a PendingActivation,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>>
                + Send
                + 'a,
        >,
    >;

    /// Dispatch one activation only after its prepared intent has committed.
    ///
    /// Implementations must use `dispatch_identity` as their idempotency/correlation identity.
    /// The returned receipt must be sufficient for bounded status observation and cancellation.
    /// Dispatch must return after durable owner admission, not after operation completion.
    ///
    /// # Errors
    ///
    /// Returns an error when the external owner cannot accept the operation. The durable attempt
    /// remains prepared for explicit reconciliation and must not be blindly redispatched.
    fn dispatch<'a>(
        &'a self,
        request: &'a PreparedActivationDispatch,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>;
}

pub use bcode_workflow::WorkflowNodeRetryResult;

/// Summary of bounded deterministic control-node settlement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlSettlementSummary {
    pub settled: Vec<String>,
    pub activated: Vec<NewActivation>,
}

/// Result of atomically reserving one pending activation for owner dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedActivationDispatch {
    pub activation: PendingActivation,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub intent: serde_json::Value,
}

/// Result of atomically persisting output and materializing direct successors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputPersistenceResult {
    pub completed_activation_id: String,
    pub activated: Vec<NewActivation>,
    pub run_status: RunStatus,
}

/// Compatibility export of the workflow-owned composition observation contract.
pub use bcode_workflow::WorkflowRepeatOutcomeSummary;

/// Compatibility export of the workflow-owned output contract.
pub use bcode_workflow::WorkflowOutputSummary;

pub use bcode_workflow::ValidatedOutput;

/// Errors returned by durable workflow persistence.
/// Actionable identity and schema detail for one target-input validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error(
    "workflow target input validation failed for {run_id}/{target_node_id} from {source_node_id}/{source_activation_id}: {message}"
)]
pub struct TargetInputValidationFailure {
    pub run_id: String,
    pub source_node_id: String,
    pub source_activation_id: String,
    pub target_node_id: String,
    pub target_schema_id: String,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum WorkflowStoreError {
    /// Structured database operation failed.
    #[error("workflow structured database error: {0}")]
    StructuredDatabase(#[from] switchy_database::DatabaseError),
    /// Structured database initialization failed.
    #[error("workflow database initialization error: {0}")]
    DatabaseInitialization(#[from] switchy_database_connection::InitSqliteRusqliteError),
    /// Filesystem operation failed.
    #[error("workflow store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Definition serialization failed.
    #[error("workflow definition serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    /// The requested workflow run does not exist.
    #[error("workflow run not found: {run_id}")]
    RunNotFound { run_id: String },
    /// A lifecycle control action is invalid for the run's current state.
    #[error("workflow run cannot transition from {current} to {target}")]
    InvalidRunTransition {
        current: RunStatus,
        target: RunStatus,
    },
    /// A single-active binding is occupied by a nonterminal run.
    #[error(
        "workflow binding is occupied by run {run_id} ({status}); inspect and explicitly resolve that run before starting another"
    )]
    ActiveBindingConflict { run_id: String, status: RunStatus },
    /// A durable cancellation request prevents further lifecycle changes.
    #[error("workflow cancellation prevents run state changes")]
    CancellationPreventsControl,
    /// A computed successor value did not match the exact target input contract.
    #[error("{0}")]
    TargetInputValidation(Box<TargetInputValidationFailure>),
    /// An optimistic authoring mutation was based on stale durable state.
    #[error(
        "workflow authoring conflict for {entity_id}: expected generation {expected}, current generation {current}"
    )]
    AuthoringConflict {
        entity_id: String,
        expected: u64,
        current: u64,
    },
    /// An existing workflow database belongs to an unsupported schema generation.
    #[error(
        "unsupported workflow store schema version {actual:?}; expected {expected}; run the explicit workflow-store reset maintenance operation after reviewing the retained state"
    )]
    UnsupportedStore {
        /// Detected schema version, or `None` when the contract table is absent or malformed.
        actual: Option<u32>,
        /// Sole supported clean-break schema version.
        expected: u32,
    },
    /// Upgrade coordination could not verify exclusive ownership within its bounded wait.
    #[error(
        "workflow storage upgrade is blocked by another owner; retry after that owner releases the store"
    )]
    UpgradeOwnershipUnavailable,
    /// Owner access is temporarily blocked by an existing execution owner.
    #[error("workflow execution owner access is deferred")]
    OwnerAccessDeferred,
    /// External owner dispatch failed; durable preparation remains available for recovery.
    #[error("workflow owner dispatch failed: {0}")]
    OwnerDispatch(Box<Self>),
    /// The run is not a supported settled repeat-limit continuation checkpoint.
    #[error(
        "continuation requires a settled repeat-iteration limit failure; active allowance exhaustion is not supported"
    )]
    ContinuationIneligible,
    /// The run's durable execution allowance has been consumed.
    #[error("workflow node-execution cap exceeded")]
    NodeExecutionAllowanceExhausted,
    /// The composition root's durable execution allowance has been consumed.
    #[error("workflow root node-execution cap exceeded")]
    RootExecutionAllowanceExhausted,
    /// All concurrent execution slots are occupied; pending work must wait for settlement.
    #[error("workflow concurrency cap reached")]
    ConcurrencyCapacityReached,
    /// Persisted data violated the storage contract.
    #[error("invalid workflow store data: {0}")]
    InvalidData(String),
}

/// Bounded durable evidence from one explicit non-destructive workflow-store migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStoreMigrationReceipt {
    pub version: u32,
    pub previous_schema_version: u32,
    pub new_schema_version: u32,
    pub backup_path: PathBuf,
    pub backup_sha256: String,
    pub migrated_at_ms: u64,
}

/// Bounded durable evidence from one explicit destructive workflow-store reset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStoreResetReceipt {
    /// Must equal [`WORKFLOW_STORE_RESET_RECEIPT_VERSION`].
    pub version: u32,
    /// Store schema detected before reset, when readable.
    pub previous_schema_version: Option<u32>,
    /// Newly initialized clean-break store schema.
    pub new_schema_version: u32,
    /// Canonical retained backup database path.
    pub backup_path: PathBuf,
    /// SHA-256 of the verified backup database bytes.
    pub backup_sha256: String,
    /// Number of workflow-owned artifact entries removed.
    pub removed_artifact_entries: u64,
    /// Caller-supplied maintenance timestamp.
    pub reset_at_ms: u64,
}

impl WorkflowStoreError {
    /// Whether a failed storage initialization may be retried without repair.
    ///
    /// This classifies initialization only; it never authorizes replay of an admission
    /// or another operation whose effects may already have committed.
    #[must_use]
    pub const fn is_transient_initialization_failure(&self) -> bool {
        matches!(self, Self::UpgradeOwnershipUnavailable)
            || matches!(self, Self::StructuredDatabase(error)
                if matches!(error.classification(), Some(switchy_database::DatabaseErrorKind::Busy | switchy_database::DatabaseErrorKind::Locked)))
    }
}

/// Compatibility export of the workflow-owned registered definition value.
pub use bcode_workflow::StoredWorkflowDefinition;

/// Durable logical runtime-authored workflow metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthoredWorkflow {
    pub workflow_id: String,
    pub title: String,
    pub description: Option<String>,
    pub archived: bool,
    pub active_revision: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Durable mutable authoring draft guarded by optimistic generation and checksum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDraft {
    pub workflow_id: String,
    pub draft_id: String,
    pub base_revision: Option<u64>,
    pub generation: u64,
    pub checksum_sha256: String,
    pub document: WorkflowAuthoringDocument,
    pub producer: WorkflowProducerProvenance,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// One bounded immutable authored-workflow lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowAuthoringEventRow {
    pub event_seq: u64,
    pub workflow_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at_ms: u64,
}

/// One bounded consistency issue found without repairing authored state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "issue", rename_all = "snake_case")]
pub enum WorkflowAuthoringIssue {
    InvalidActiveRevision {
        revision: u64,
    },
    MissingCompiledDefinition {
        revision: u64,
        definition_id: String,
        definition_version: u32,
    },
    OrphanedPreset {
        preset_id: String,
        revision: u64,
    },
    StaleDraftBase {
        draft_id: String,
        base_revision: u64,
    },
}

/// Immutable published authored-workflow revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedWorkflowRevision {
    pub workflow_id: String,
    pub revision: u64,
    pub source_checksum_sha256: String,
    pub executable_source_checksum_sha256: String,
    pub definition_identity: WorkflowDefinitionIdentity,
    pub document: WorkflowAuthoringDocument,
    pub producer: WorkflowProducerProvenance,
    pub published_at_ms: u64,
}

/// Reusable mutable configuration preset bound to one exact immutable revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPreset {
    pub workflow_id: String,
    pub preset_id: String,
    pub revision: u64,
    pub name: String,
    pub generation: u64,
    pub configuration: serde_json::Value,
    pub run_limits: Option<WorkflowRunLimitPolicy>,
    pub producer: WorkflowProducerProvenance,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Successful atomic draft publication result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPublication {
    pub revision: PublishedWorkflowRevision,
    pub active_revision: Option<u64>,
}

/// Authoring mutation boundary used by deterministic optimistic-update fault tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowAuthoringMutationBoundary {
    DraftUpdated,
    PresetUpdated,
}

/// Optional fault hook for atomic draft and preset update tests.
pub trait WorkflowAuthoringMutationFault: Sync {
    /// Observe one uncommitted authoring update.
    ///
    /// # Errors
    ///
    /// Returning an error rolls back the complete update transaction.
    fn after_boundary(
        &self,
        boundary: WorkflowAuthoringMutationBoundary,
    ) -> Result<(), WorkflowStoreError>;
}

/// Production no-op authoring mutation fault hook.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopWorkflowAuthoringMutationFault;

impl WorkflowAuthoringMutationFault for NoopWorkflowAuthoringMutationFault {
    fn after_boundary(
        &self,
        _boundary: WorkflowAuthoringMutationBoundary,
    ) -> Result<(), WorkflowStoreError> {
        Ok(())
    }
}

/// Publication transaction boundary used by deterministic fault tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowPublicationBoundary {
    RevisionPersisted,
    DefinitionPersisted,
    ActiveRevisionUpdated,
    EventPersisted,
}

/// Optional fault hook for atomic authoring publication tests.
pub trait WorkflowPublicationFault: Sync {
    /// Observe one uncommitted publication boundary.
    ///
    /// # Errors
    ///
    /// Returning an error rolls back the complete publication transaction.
    fn after_boundary(
        &self,
        boundary: WorkflowPublicationBoundary,
    ) -> Result<(), WorkflowStoreError>;
}

/// Production no-op publication fault hook.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopWorkflowPublicationFault;

impl WorkflowPublicationFault for NoopWorkflowPublicationFault {
    fn after_boundary(
        &self,
        _boundary: WorkflowPublicationBoundary,
    ) -> Result<(), WorkflowStoreError> {
        Ok(())
    }
}

/// Explicit authored-store maintenance result with retained backup evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowAuthoringRepairReport {
    /// Canonical confined backup created before mutation.
    pub backup_path: PathBuf,
    /// Whether an invalid active pointer was cleared.
    pub active_pointer_cleared: bool,
    /// Number of safe derived indexes recreated.
    pub indexes_recreated: u32,
    /// Damage deliberately left unrepaired because canonical intent is ambiguous.
    pub remaining_issues: Vec<WorkflowAuthoringIssue>,
}

/// Process-local read fence. Any database change invalidates it; it is not a durable snapshot.
#[derive(Debug)]
pub struct WorkflowPublicationReadFence {
    identity: std::sync::Arc<()>,
    data_version: i64,
    total_changes: u64,
}

#[derive(Debug)]
struct StoreOwnership {
    file: File,
    database_metadata: std::fs::Metadata,
    owner_pid: u32,
}

impl Drop for StoreOwnership {
    fn drop(&mut self) {
        // Release the fence even if an unrelated fork temporarily inherited
        // this open file description. A child must not unlock its parent's fence.
        if self.owner_pid == std::process::id() {
            let _ = self.file.unlock();
        }
    }
}

fn verify_retained_database_identity(
    expected: &std::fs::Metadata,
    actual: &std::fs::Metadata,
) -> Result<(), WorkflowStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if expected.is_file()
            && actual.is_file()
            && expected.dev() == actual.dev()
            && expected.ino() == actual.ino()
        {
            return Ok(());
        }
    }
    #[cfg(not(unix))]
    let _ = (expected, actual);
    Err(WorkflowStoreError::InvalidData(
        "workflow database identity cannot be retained safely".into(),
    ))
}

/// Durable workflow database.
#[derive(Debug)]
pub struct WorkflowStore {
    path: PathBuf,
    connection: switchy_database::rusqlite::RusqliteDatabase,
    // Declared after the connection so the database closes before fence release.
    ownership: Option<std::sync::Arc<StoreOwnership>>,
    read_identity: std::sync::Arc<()>,
}

impl WorkflowStore {
    /// Create an isolated, noncanonical store for an unavailable workflow service.
    ///
    /// Hosts must gate all workflow operations until canonical initialization succeeds.
    /// This store has no durable path or execution authority.
    ///
    /// # Errors
    /// Returns an error if the in-memory schema cannot be initialized.
    pub fn unavailable_placeholder() -> Result<Self, WorkflowStoreError> {
        let connection = database::open(
            Path::new(":memory:"),
            switchy_database_connection::SqliteAccess::ReadWriteCreate,
            std::time::Duration::from_secs(5),
            true,
        )?;
        schema::initialize(&connection, WORKFLOW_STORE_SCHEMA_VERSION)?;
        Ok(Self {
            path: PathBuf::new(),
            connection,
            ownership: None,
            read_identity: std::sync::Arc::new(()),
        })
    }

    /// Open or create the canonical workflow database below an explicit Bcode state directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or an existing schema is unsupported.
    pub fn open_in_state_dir(state_dir: &Path) -> Result<Self, WorkflowStoreError> {
        Self::open_at(&workflow_database_path(state_dir))
    }

    /// Initialize workflow storage, upgrading known schemas under exclusive ownership.
    ///
    /// Ordinary reads must use [`Self::open_in_state_dir`] instead. Ownership contention
    /// returns immediately; initialization never revokes another owner's authority or resets state.
    ///
    /// # Errors
    /// Returns an error for unavailable ownership, unsupported or damaged storage, or a failed
    /// backup, migration, or database open. Existing canonical state is preserved on failure.
    pub fn initialize_in_state_dir(
        state_dir: &Path,
        migrated_at_ms: u64,
    ) -> Result<Self, WorkflowStoreError> {
        let path = workflow_database_path(state_dir);
        let root = path.parent().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow database has no parent directory".to_string())
        })?;
        bcode_metrics::startup::measure("workflow_store.prepare_directory", || {
            std::fs::create_dir_all(root)
        })?;
        let ownership =
            bcode_metrics::startup::measure("workflow_store.ownership_file_open", || {
                open_ownership_file(root)
            })?;
        let shared_wait = bcode_metrics::startup::phase("workflow_store.shared_ownership_wait");
        match ownership.try_lock_shared() {
            Ok(()) => {
                shared_wait.finish();
                if path.is_file() {
                    match Self::open_with_ownership(&path, ownership) {
                        Ok(store) => return Ok(store),
                        Err(WorkflowStoreError::UnsupportedStore {
                            actual: Some(14..=42),
                            ..
                        }) => {}
                        Err(error) => return Err(error),
                    }
                } else {
                    drop(ownership);
                }
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(WorkflowStoreError::UpgradeOwnershipUnavailable);
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let ownership = open_ownership_file(root)?;
        let exclusive_wait =
            bcode_metrics::startup::phase("workflow_store.exclusive_ownership_wait");
        match ownership.try_lock() {
            Ok(()) => exclusive_wait.finish(),
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(WorkflowStoreError::UpgradeOwnershipUnavailable);
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        if path.is_file() {
            let connection = database::open(
                &path,
                switchy_database_connection::SqliteAccess::ReadOnly,
                std::time::Duration::from_secs(5),
                false,
            )?;
            let schema = detected_typed_store_schema(&connection);
            drop(connection);
            if schema != Some(WORKFLOW_STORE_SCHEMA_VERSION) {
                bcode_metrics::startup::measure("workflow_store.migrate", || {
                    Self::migrate_owned(&path, migrated_at_ms)
                })?;
            }
        }
        let lock = ownership.try_clone()?;
        // Fresh production stores retain the coexistence baseline. Authorized admission
        // installs the typed representation only after executable capability validation.
        // Existing stores are verified above and are never downgraded here.
        let store = Self::open_with_schema(&path, ownership, 42)?;
        bcode_metrics::startup::measure("workflow_store.downgrade_ownership", || {
            lock.lock_shared()
        })?;
        drop(lock);
        Ok(store)
    }

    /// Open an explicit workflow database path.
    ///
    /// This is used by host cancellation handles that must reopen the same canonical database
    /// without borrowing a live connection across an async boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or an existing schema is unsupported.
    pub fn open_at_path(path: &Path) -> Result<Self, WorkflowStoreError> {
        Self::open_at(path)
    }

    /// Open the production-default workflow database.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or an existing schema is unsupported.
    pub fn open_default() -> Result<Self, WorkflowStoreError> {
        Self::open_in_state_dir(&bcode_config::default_state_dir())
    }

    fn open_at(path: &Path) -> Result<Self, WorkflowStoreError> {
        let root = path.parent().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow database has no parent directory".to_string())
        })?;
        std::fs::create_dir_all(root)?;
        let ownership = open_ownership_file(root)?;
        match ownership.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(WorkflowStoreError::UpgradeOwnershipUnavailable);
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        Self::open_with_ownership(path, ownership)
    }

    /// Create a fresh canonical compatibility2 baseline without converting existing storage.
    ///
    /// # Errors
    /// Rejects any existing database, ownership contention or schema initialization failure.
    pub fn create_coexistence_baseline(state_dir: &Path) -> Result<Self, WorkflowStoreError> {
        let path = workflow_database_path(state_dir);
        let root = path.parent().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow database root missing".into())
        })?;
        std::fs::create_dir_all(root)?;
        let ownership = open_ownership_file(root)?;
        ownership
            .try_lock()
            .map_err(|_| WorkflowStoreError::UpgradeOwnershipUnavailable)?;
        if path.exists() {
            return Err(WorkflowStoreError::InvalidData(
                "coexistence baseline requires absent database".into(),
            ));
        }
        let lock = ownership.try_clone()?;
        let store = Self::open_with_schema(&path, ownership, 42)?;
        lock.lock_shared()?;
        Ok(store)
    }

    fn open_with_ownership(path: &Path, ownership: File) -> Result<Self, WorkflowStoreError> {
        Self::open_with_schema(path, ownership, WORKFLOW_STORE_SCHEMA_VERSION)
    }

    fn open_with_schema(
        path: &Path,
        ownership: File,
        fresh_version: u32,
    ) -> Result<Self, WorkflowStoreError> {
        use bcode_metrics::startup::measure;
        let existed = path.exists();
        let connection = measure("workflow_store.connection_open", || {
            database::open(
                path,
                if existed {
                    switchy_database_connection::SqliteAccess::ReadWrite
                } else {
                    switchy_database_connection::SqliteAccess::ReadWriteCreate
                },
                std::time::Duration::from_secs(5),
                true,
            )
        })?;
        if existed {
            measure("workflow_store.verify_schema", || {
                storage_compatibility::verify(&connection)
            })?;
        } else {
            measure("workflow_store.create_schema", || {
                schema::initialize(&connection, fresh_version)
            })?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            connection,
            ownership: Some(std::sync::Arc::new(StoreOwnership {
                file: ownership,
                database_metadata: std::fs::metadata(path)?,
                owner_pid: std::process::id(),
            })),
            read_identity: std::sync::Arc::new(()),
        })
    }

    /// Retain a verified store's maintenance fence with an independent connection.
    ///
    /// Does not initialize, migrate or traverse schema. The originating fence remains
    /// held until all retained handles close. Publication read fences remain connection-local.
    /// # Errors
    /// Rejects placeholders, inherited process ownership, replaced/missing database files,
    /// unsupported file-identity platforms and connection/configuration failures.
    pub fn retained_handle(&self) -> Result<Self, WorkflowStoreError> {
        let ownership = self.ownership.as_ref().ok_or_else(|| {
            WorkflowStoreError::InvalidData("cannot retain an unverified workflow store".into())
        })?;
        if ownership.owner_pid != std::process::id() {
            return Err(WorkflowStoreError::InvalidData(
                "cannot retain inherited workflow ownership".into(),
            ));
        }
        verify_retained_database_identity(
            &ownership.database_metadata,
            &std::fs::metadata(&self.path)?,
        )?;
        let connection = database::open(
            &self.path,
            switchy_database_connection::SqliteAccess::ReadWrite,
            std::time::Duration::from_secs(5),
            true,
        )?;
        verify_retained_database_identity(
            &ownership.database_metadata,
            &std::fs::metadata(&self.path)?,
        )?;
        Ok(Self {
            path: self.path.clone(),
            connection,
            ownership: Some(std::sync::Arc::clone(ownership)),
            read_identity: std::sync::Arc::new(()),
        })
    }

    /// Explicitly migrate supported historical workflow schemas, preserving canonical state.
    ///
    /// Acquires exclusive store ownership and verifies a backup before adding authority columns
    /// where needed, materializing initial run graphs for schemas 14/15, and adding the
    /// source-edge index for schema 16. Integrity is checked before commit.
    /// Normal reads never invoke this operation.
    ///
    /// # Errors
    /// Returns an error for unavailable ownership, unsupported source schemas, inconsistent
    /// canonical state, backup failure, or database failure.
    pub fn migrate_to_current_in_state_dir(
        state_dir: &Path,
        migrated_at_ms: u64,
    ) -> Result<WorkflowStoreMigrationReceipt, WorkflowStoreError> {
        Self::migrate_schema_14_to_current_in_state_dir(state_dir, migrated_at_ms)
    }

    /// Compatibility entry point for explicit migration from schemas 14–16 to the current schema.
    ///
    /// Prefer [`Self::migrate_to_current_in_state_dir`]. Existing state is preserved; initial
    /// run-owned graphs are materialized for schemas 14/15 and schema-14 authority columns
    /// are added when needed. Schema 16 receives only the source-edge index, without graph rebuild.
    ///
    /// # Errors
    /// Returns an error when ownership is unavailable, the source schema is unsupported, backup or
    /// integrity verification fails, or migration cannot commit atomically.
    pub fn migrate_schema_14_to_current_in_state_dir(
        state_dir: &Path,
        migrated_at_ms: u64,
    ) -> Result<WorkflowStoreMigrationReceipt, WorkflowStoreError> {
        let path = workflow_database_path(state_dir);
        let root = path.parent().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow database has no parent directory".to_string())
        })?;
        std::fs::create_dir_all(root)?;
        let ownership = open_ownership_file(root)?;
        match ownership.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow store migration requires exclusive ownership; stop active workflow writers"
                        .to_string(),
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        Self::migrate_owned(&path, migrated_at_ms)
    }

    // The caller retains exclusive file ownership through migration and any subsequent open.
    fn migrate_owned(
        path: &Path,
        migrated_at_ms: u64,
    ) -> Result<WorkflowStoreMigrationReceipt, WorkflowStoreError> {
        let root = path.parent().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow database has no parent directory".to_string())
        })?;
        if !path.is_file() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow store migration requires an existing database".to_string(),
            ));
        }
        let source = database::open(
            path,
            switchy_database_connection::SqliteAccess::ReadWrite,
            std::time::Duration::ZERO,
            true,
        )?;
        let transaction = database::begin(&source, switchy_database::TransactionMode::Immediate)?;
        let db: &dyn switchy_database::Database = &*transaction;
        let previous_schema_version = verify_migration_source(db)?;
        let (backup_path, backup_sha256) = create_migration_backup(path, root, migrated_at_ms)?;
        if previous_schema_version == 14 {
            crate::database::block_on(
                alter_table("workflow_runs")
                    .add_column("target_artifact_id".into(), DataType::Text, true, None)
                    .add_column(
                        "coordinator_daemon_instance_id".into(),
                        DataType::Text,
                        true,
                        None,
                    )
                    .add_column("coordinator_generation".into(), DataType::Int, true, None)
                    .add_column(
                        "coordinator_fencing_token".into(),
                        DataType::Text,
                        true,
                        None,
                    )
                    .execute(db),
            )?;
        }
        if previous_schema_version < 16 {
            run_graph::migrate(db)?;
        } else if previous_schema_version == 16 {
            run_graph::initialize_source_index(db)?;
        }
        if previous_schema_version < 18 {
            crate::database::block_on(
                alter_table("workflow_activations")
                    .add_column_check(
                        "node_revision".into(),
                        DataType::Int,
                        false,
                        Some(switchy_database::DatabaseValue::Int64(1)),
                        E::column("node_revision").compare(C::Greater, E::Integer(0)),
                    )
                    .execute(db),
            )?;
        }
        if (16..=20).contains(&previous_schema_version) {
            run_graph::initialize_retirement(db)?;
        }
        run_graph::initialize_edit_candidates(db)?;
        migrate_run_package_bindings(db)?;
        continuation::initialize(db)?;
        recovery::initialize(db)?;
        initialize_compatibility_contract(db)?;
        migrate_recursion_policy(db)?;

        crate::database::block_on(
            db.exec_update_count(
                &(update("workflow_store_contract")
                    .value("schema_version", i64::from(WORKFLOW_STORE_SCHEMA_VERSION))
                    .where_eq("contract_id", 1_i64)),
            ),
        )?;
        verify_store_schema(db)?;
        verify_migration_integrity(db)?;
        database::commit(transaction)?;
        drop(source);
        let receipt = WorkflowStoreMigrationReceipt {
            version: WORKFLOW_STORE_MIGRATION_RECEIPT_VERSION,
            previous_schema_version,
            new_schema_version: WORKFLOW_STORE_SCHEMA_VERSION,
            backup_path,
            backup_sha256,
            migrated_at_ms,
        };
        write_migration_receipt(root, &receipt)?;
        Ok(receipt)
    }

    /// Explicitly replace incompatible workflow state after retaining a verified backup.
    ///
    /// This destructive maintenance operation takes exclusive workflow-store ownership, verifies
    /// that no store handles or `SQLite` writers are active, creates a confined backup, removes only
    /// workflow-owned database sidecars and artifacts, initializes the sole current schema, and
    /// atomically records a bounded receipt. Normal open paths never invoke this operation.
    ///
    /// # Errors
    ///
    /// Returns an error when ownership is unavailable, the database is already current or absent,
    /// backup creation or verification fails, a workflow-owned path is unsafe, or reset fails.
    pub fn reset_incompatible_store_in_state_dir(
        state_dir: &Path,
        reset_at_ms: u64,
    ) -> Result<WorkflowStoreResetReceipt, WorkflowStoreError> {
        let path = workflow_database_path(state_dir);
        let root = path.parent().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow database has no parent directory".to_string())
        })?;
        std::fs::create_dir_all(root)?;
        let ownership = open_ownership_file(root)?;
        match ownership.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow store reset requires exclusive ownership; stop active workflow writers"
                        .to_string(),
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        if !path.is_file() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow store reset requires an existing incompatible database".to_string(),
            ));
        }

        let source = database::open(
            &path,
            switchy_database_connection::SqliteAccess::ReadWrite,
            std::time::Duration::ZERO,
            false,
        )?;
        let previous_schema_version = detected_typed_store_schema(&source);
        if previous_schema_version == Some(WORKFLOW_STORE_SCHEMA_VERSION)
            || storage_compatibility::verify(&source).is_ok()
        {
            return Err(WorkflowStoreError::InvalidData(
                "refusing to reset a current workflow store".to_string(),
            ));
        }
        database::commit(database::begin(
            &source,
            switchy_database::TransactionMode::Exclusive,
        )?)?;
        let backup_directory = root.join(RESET_BACKUP_DIRECTORY);
        std::fs::create_dir_all(&backup_directory)?;
        let backup_path = backup_directory.join(format!("workflow-{reset_at_ms}"));
        std::fs::create_dir(&backup_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                WorkflowStoreError::InvalidData(
                    "workflow reset backup already exists for this timestamp".to_string(),
                )
            } else {
                error.into()
            }
        })?;
        let backup_database = backup_path.join(DATABASE_FILE);
        crate::database::block_on(switchy_database::Database::sqlite_backup_to(
            &source,
            &backup_database,
        ))?;
        drop(source);
        let backup_sha256 = verify_reset_backup(&backup_database)?;
        let artifacts = root.join(ARTIFACT_DIRECTORY);
        let backup_artifacts = backup_path.join(ARTIFACT_DIRECTORY);
        let removed_artifact_entries = backup_workflow_artifacts(&artifacts, &backup_artifacts)?;

        let reset = (|| -> Result<WorkflowStoreResetReceipt, WorkflowStoreError> {
            if artifacts.exists() {
                std::fs::remove_dir_all(&artifacts)?;
            }
            std::fs::remove_file(&path)?;
            for sidecar in [path.with_extension("db-wal"), path.with_extension("db-shm")] {
                if sidecar.exists() {
                    std::fs::remove_file(sidecar)?;
                }
            }
            let connection = database::open(
                &path,
                switchy_database_connection::SqliteAccess::ReadWriteCreate,
                std::time::Duration::from_secs(5),
                true,
            )?;
            schema::initialize(&connection, WORKFLOW_STORE_SCHEMA_VERSION)?;
            verify_store_schema(&connection)?;
            drop(connection);

            let receipt = WorkflowStoreResetReceipt {
                version: WORKFLOW_STORE_RESET_RECEIPT_VERSION,
                previous_schema_version,
                new_schema_version: WORKFLOW_STORE_SCHEMA_VERSION,
                backup_path: backup_path.clone(),
                backup_sha256: backup_sha256.clone(),
                removed_artifact_entries,
                reset_at_ms,
            };
            write_reset_receipt(root, &receipt)?;
            Ok(receipt)
        })();
        if reset.is_err() {
            restore_reset_backup(&path, &artifacts, &backup_database, &backup_artifacts)?;
        }
        reset
    }

    /// Return the canonical database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the current durable execution authority for one run.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed persisted authority or a database failure.
    pub(crate) fn run_summary_in(
        db: &dyn switchy_database::Database,
        run_id: &str,
    ) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
        run_summary_in(db, run_id)
    }

    pub(crate) fn associated_run_in(
        db: &dyn switchy_database::Database,
        key: &WorkflowRunBindingKey,
    ) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
        associated_run_in(db, key)
    }

    pub(crate) fn pending_replacement_in(
        db: &dyn switchy_database::Database,
        run_id: &str,
    ) -> Result<Option<NewWorkflowRun>, WorkflowStoreError> {
        pending_replacement_in(db, run_id)
    }

    pub(crate) fn verify_execution_authority_in(
        db: &dyn switchy_database::Database,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<(), WorkflowStoreError> {
        verify_execution_authority_in(db, run_id, authority)
    }

    pub(crate) fn validate_quiescent_reassignment_in(
        db: &dyn switchy_database::Database,
        run_id: &str,
    ) -> Result<(), WorkflowStoreError> {
        validate_quiescent_reassignment_in(db, run_id)
    }

    pub(crate) fn definition_in(
        db: &dyn switchy_database::Database,
        id: &str,
        version: u32,
    ) -> Result<Option<StoredWorkflowDefinition>, WorkflowStoreError> {
        definition_in(db, id, version)
    }

    pub(crate) fn execution_authority_in(
        db: &dyn switchy_database::Database,
        run_id: &str,
    ) -> Result<Option<WorkflowExecutionAuthority>, WorkflowStoreError> {
        execution_authority_in(db, run_id)
    }

    /// # Errors
    /// Returns an error for malformed identities, inconsistent persisted state, or storage failure.
    pub fn execution_authority(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowExecutionAuthority>, WorkflowStoreError> {
        execution_authority_in(&self.connection, run_id)
    }

    /// Verify that the supplied authority is current without mutating workflow state.
    ///
    /// # Errors
    ///
    /// Returns an error when the run is missing, ownerless, stale, or malformed.
    pub fn verify_execution_authority(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<(), WorkflowStoreError> {
        Self::verify_execution_authority_in(&self.connection, run_id, authority)
    }

    /// Read whether a run is fenced to recovery-only work without mutating storage.
    ///
    /// # Errors
    /// Rejects malformed run identities or database failures.
    pub fn is_recovery_only(&self, run_id: &str) -> Result<bool, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        Ok(!database::rows(
            &self.connection,
            &select("workflow_recovery_barriers")
                .projections(database::projections(&["run_id"]))
                .where_eq("run_id", run_id)
                .limit(1),
        )?
        .is_empty())
    }

    /// Read the original execution artifact retained by a recovery barrier.
    ///
    /// # Errors
    /// Rejects malformed run identities, invalid persisted artifact identity, or database failure.
    pub fn recovery_source_artifact(
        &self,
        run_id: &str,
    ) -> Result<Option<String>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        database::rows(
            &self.connection,
            &select("workflow_recovery_barriers")
                .project_as(
                    bounded_text(
                        identifier("source_artifact_id"),
                        MAX_ID_BYTES as u64,
                        V::String(String::new()),
                    ),
                    "source_artifact_id",
                )
                .where_eq("run_id", run_id)
                .limit(1),
        )?
        .first()
        .map(|row| {
            let source = database::text(row, "source_artifact_id")?;
            validate_id("recovery source artifact", &source)?;
            Ok(source)
        })
        .transpose()
    }

    /// Enter durable recovery-only mode under current ownership, preserving receipt provenance.
    /// No attempt may be dispatched or run resumed until explicit recovery completion.
    ///
    /// # Errors
    /// Rejects stale authority, terminal runs, or database failures.
    pub fn enter_recovery_only(
        &mut self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        now_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        let tx = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*tx;
        Self::verify_execution_authority_in(db, run_id, authority)?;
        let run =
            Self::run_summary_in(db, run_id)?.ok_or_else(|| WorkflowStoreError::RunNotFound {
                run_id: run_id.into(),
            })?;
        if !matches!(
            run.status,
            RunStatus::Running | RunStatus::Paused | RunStatus::RepairRequired
        ) {
            return Err(WorkflowStoreError::InvalidData(
                "terminal run cannot enter recovery".into(),
            ));
        }
        let changed = crate::database::block_on(
            db.insert("workflow_recovery_barriers")
                .value("run_id", run_id)
                .value("source_artifact_id", authority.target_artifact_id.as_str())
                .value("created_at_ms", database::integer_value(now_ms)?)
                .or_ignore()
                .execute_count(db),
        )?;
        if changed == 1 {
            append_event_in(db, run_id, "recovery_started", "{}", now_ms)?;
        }
        crate::database::block_on(
            db.exec_update_count(
                &(update("workflow_runs")
                    .value("status", "paused")
                    .where_eq("run_id", run_id)
                    .where_eq("status", "running")),
            ),
        )?;
        database::commit(tx)
    }

    /// Finish recovery without resuming execution or changing terminal outcomes.
    ///
    /// Only the original execution artifact may clear the barrier; cross-artifact
    /// execution compatibility must be established separately, never inferred from ownership.
    ///
    /// # Errors
    /// Rejects stale authority, unsettled attempts, incompatible artifacts, or storage failures.
    pub fn finish_recovery_only(
        &mut self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        now_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.complete_recovery(run_id, authority, now_ms, false)
    }

    /// Atomically complete compatible recovery and explicitly resume a paused run.
    ///
    /// # Errors
    /// Rejects cancellation, non-paused state, stale ownership, unresolved work, or incompatible artifacts.
    pub fn resume_recovered_run(
        &mut self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        now_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.complete_recovery(run_id, authority, now_ms, true)
    }

    fn complete_recovery(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        now_ms: u64,
        resume: bool,
    ) -> Result<bool, WorkflowStoreError> {
        let tx = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*tx;
        Self::verify_execution_authority_in(db, run_id, authority)?;
        if resume {
            let run = Self::run_summary_in(db, run_id)?.ok_or_else(|| {
                WorkflowStoreError::RunNotFound {
                    run_id: run_id.into(),
                }
            })?;
            if run.cancellation_requested_at_ms.is_some() {
                return Err(WorkflowStoreError::CancellationPreventsControl);
            }
            if run.status != RunStatus::Paused {
                return Err(WorkflowStoreError::InvalidRunTransition {
                    current: run.status,
                    target: RunStatus::Running,
                });
            }
        }
        let barriers = database::rows(
            db,
            &select("workflow_recovery_barriers")
                .project_as(
                    bounded_text(
                        identifier("source_artifact_id"),
                        MAX_ID_BYTES as u64,
                        V::String(String::new()),
                    ),
                    "source_artifact_id",
                )
                .where_eq("run_id", run_id)
                .limit(1),
        )?;
        let Some(barrier) = barriers.first() else {
            return Ok(false);
        };
        let source = database::text(barrier, "source_artifact_id")?;
        validate_id("recovery source artifact", &source)?;
        if source != authority.target_artifact_id {
            return Err(WorkflowStoreError::InvalidData(
                "recovery completion requires compatible execution artifact".into(),
            ));
        }
        Self::validate_quiescent_reassignment_in(db, run_id)?;
        let terminal = select("workflow_runs")
            .projections(database::projections(&["run_id"]))
            .where_in("status", vec!["completed", "failed", "cancelled"]);
        let unresolved = database::rows(
            db,
            &select("workflow_run_links")
                .projections(database::projections(&["child_run_id"]))
                .where_eq("parent_run_id", run_id)
                .where_not_in("child_run_id", terminal)
                .limit(1),
        )?;
        if !unresolved.is_empty() {
            return Err(WorkflowStoreError::InvalidData(
                "recovery completion requires terminal child workflows".into(),
            ));
        }
        crate::database::block_on(
            delete("workflow_recovery_barriers")
                .where_eq("run_id", run_id)
                .execute(db),
        )?;
        append_event_in(db, run_id, "recovery_completed", "{}", now_ms)?;
        if resume {
            crate::database::block_on(
                db.exec_update_count(
                    &(update("workflow_runs")
                        .value("status", "running")
                        .value("updated_at_ms", database::integer_value(now_ms)?)
                        .where_eq("run_id", run_id)),
                ),
            )?;
            append_event_in(
                db,
                run_id,
                "run_resumed",
                "{\"status\":\"running\"}",
                now_ms,
            )?;
        }
        database::commit(tx)?;
        Ok(true)
    }

    /// Atomically acquire recovery-only authority after verified coordinator termination.
    ///
    /// Unlike executable reassignment, unresolved attempts are preserved and permitted.
    /// The caller must verify termination and retain canonical session ownership. Receipt
    /// compatibility is not implied by this transfer; execution remains durably blocked.
    ///
    /// # Errors
    /// Rejects stale authority, mismatched evidence, invalid generation/fence, terminal runs,
    /// or persistence failure. All changes roll back together.
    pub fn take_recovery_authority(
        &mut self,
        run_id: &str,
        expected: &WorkflowExecutionAuthority,
        replacement: &WorkflowExecutionAuthority,
        evidence: &EndedOwnerEvidence,
        now_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        self.take_recovery_authority_with_terminal_proof(
            run_id,
            expected,
            replacement,
            evidence,
            now_ms,
            false,
        )
    }

    /// Acquire proof-only recovery authority for a terminal descendant after verified owner loss.
    /// This preserves its terminal outcome and installs the ordinary recovery dispatch barrier.
    ///
    /// # Errors
    /// Rejects missing parent links, nonterminal runs, stale ownership, or invalid ended-owner evidence.
    pub fn take_descendant_proof_authority(
        &mut self,
        run_id: &str,
        expected: &WorkflowExecutionAuthority,
        replacement: &WorkflowExecutionAuthority,
        evidence: &EndedOwnerEvidence,
        now_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        self.take_recovery_authority_with_terminal_proof(
            run_id,
            expected,
            replacement,
            evidence,
            now_ms,
            true,
        )
    }

    fn take_recovery_authority_with_terminal_proof(
        &self,
        run_id: &str,
        expected: &WorkflowExecutionAuthority,
        replacement: &WorkflowExecutionAuthority,
        evidence: &EndedOwnerEvidence,
        now_ms: u64,
        terminal_proof: bool,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        if evidence.liveness != EndedOwnerLiveness::ObservedEnded
            || evidence.ended_daemon_instance_id != expected.daemon_instance_id
            || evidence.ended_target_artifact_id != expected.target_artifact_id
            || expected.generation.checked_add(1) != Some(replacement.generation)
            || expected.fencing_token == replacement.fencing_token
            || expected.daemon_instance_id == replacement.daemon_instance_id
        {
            return Err(WorkflowStoreError::InvalidData(
                "invalid recovery authority transfer".into(),
            ));
        }
        validate_id("replacement artifact", &replacement.target_artifact_id)?;
        validate_id("replacement daemon", &replacement.daemon_instance_id)?;
        validate_id("replacement fence", &replacement.fencing_token)?;

        let tx = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let db: &dyn switchy_database::Database = &*tx;
        verify_store_schema(db)?;
        Self::verify_execution_authority_in(db, run_id, expected)?;
        let run =
            Self::run_summary_in(db, run_id)?.ok_or_else(|| WorkflowStoreError::RunNotFound {
                run_id: run_id.into(),
            })?;
        if terminal_proof {
            let descendant = !database::rows(
                db,
                &select("workflow_run_links")
                    .projections(database::projections(&["child_run_id"]))
                    .where_eq("child_run_id", run_id)
                    .limit(1),
            )?
            .is_empty();
            if !matches!(
                run.status,
                RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
            ) || !descendant
            {
                return Err(WorkflowStoreError::InvalidData(
                    "proof takeover requires a terminal descendant".into(),
                ));
            }
        } else if !matches!(
            run.status,
            RunStatus::Running | RunStatus::Paused | RunStatus::RepairRequired
        ) {
            return Err(WorkflowStoreError::InvalidData(
                "terminal workflow cannot transfer recovery authority".into(),
            ));
        }
        crate::database::block_on(
            db.insert("workflow_recovery_barriers")
                .value("run_id", run_id)
                .value("source_artifact_id", expected.target_artifact_id.as_str())
                .value("created_at_ms", database::integer_value(now_ms)?)
                .or_ignore()
                .execute_count(db),
        )?;
        let status = if run.status == RunStatus::Running {
            RunStatus::Paused
        } else {
            run.status
        };
        let changed = crate::database::block_on(
            db.exec_update_count(
                &(update("workflow_runs")
                    .value(
                        "target_artifact_id",
                        replacement.target_artifact_id.as_str(),
                    )
                    .value(
                        "coordinator_daemon_instance_id",
                        replacement.daemon_instance_id.as_str(),
                    )
                    .value(
                        "coordinator_generation",
                        database::integer_value(replacement.generation)?,
                    )
                    .value(
                        "coordinator_fencing_token",
                        replacement.fencing_token.as_str(),
                    )
                    .value("status", status.as_str())
                    .value("updated_at_ms", database::integer_value(now_ms)?)
                    .where_eq("run_id", run_id)),
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "terminal workflow cannot transfer recovery authority".into(),
            ));
        }
        append_event_in(db,run_id,"recovery_authority_transferred",&serde_json::json!({"from":expected,"to":replacement,"evidence":evidence,"reassigned_at_ms":now_ms}).to_string(),now_ms)?;
        database::commit(tx)
    }

    /// Atomically transfer one active run to a replacement coordinator.
    ///
    /// The compare-and-swap predicate fences stale owners. Artifact identity cannot change.
    /// Repair-required runs may transfer without changing their status: ownership is needed
    /// to resolve ambiguity, but the transfer does not authorize dispatch or retry.
    ///
    /// # Errors
    ///
    /// Returns an error when the current authority is stale, the artifact changes, or persistence
    /// fails.
    pub fn transfer_execution_authority(
        &mut self,
        run_id: &str,
        expected: &WorkflowExecutionAuthority,
        replacement: &WorkflowExecutionAuthority,
        transferred_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        if expected.target_artifact_id != replacement.target_artifact_id
            || expected.daemon_instance_id == replacement.daemon_instance_id
            || expected.fencing_token == replacement.fencing_token
            || expected.generation.checked_add(1) != Some(replacement.generation)
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow execution authority transfer is invalid".to_string(),
            ));
        }
        validate_id("replacement daemon", &replacement.daemon_instance_id)?;
        validate_id("replacement fence", &replacement.fencing_token)?;

        let tx = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let db: &dyn switchy_database::Database = &*tx;
        verify_store_schema(db)?;
        let pending = Self::pending_replacement_in(db, run_id)?.is_some();
        let mut query = update("workflow_runs")
            .value(
                "coordinator_daemon_instance_id",
                replacement.daemon_instance_id.as_str(),
            )
            .value(
                "coordinator_generation",
                database::integer_value(replacement.generation)?,
            )
            .value(
                "coordinator_fencing_token",
                replacement.fencing_token.as_str(),
            )
            .value("updated_at_ms", database::integer_value(transferred_at_ms)?)
            .where_eq("run_id", run_id)
            .where_eq("target_artifact_id", expected.target_artifact_id.as_str())
            .where_eq(
                "coordinator_daemon_instance_id",
                expected.daemon_instance_id.as_str(),
            )
            .where_eq(
                "coordinator_generation",
                database::integer_value(expected.generation)?,
            )
            .where_eq("coordinator_fencing_token", expected.fencing_token.as_str());
        query = query.where_in(
            "status",
            if pending {
                vec![
                    "running",
                    "paused",
                    "repair_required",
                    "completed",
                    "failed",
                    "cancelled",
                ]
            } else {
                vec!["running", "paused", "repair_required"]
            },
        );
        let changed = crate::database::block_on(db.exec_update_count(&(query)))?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow execution authority transfer lost its ownership race: {run_id}"
            )));
        }
        database::commit(tx)
    }

    /// Check whether cross-artifact reassignment has unsettled external work.
    ///
    /// This read-only check uses the same eligibility rule as reassignment. A cancellation
    /// request or signal is not evidence that an operation ended. Ambiguous outcomes also
    /// remain pinned until an authorized recovery path resolves them.
    ///
    /// # Errors
    /// Returns an error for an invalid identity, unsettled work, or database failure.
    pub fn validate_quiescent_reassignment(&self, run_id: &str) -> Result<(), WorkflowStoreError> {
        Self::validate_quiescent_reassignment_in(&self.connection, run_id)
    }

    /// Atomically reassign one quiescent run from a coordinator that verifiably ended to a
    /// coordinator on a different daemon artifact.
    ///
    /// This is the cross-artifact counterpart of [`Self::transfer_execution_authority`]. It is
    /// permitted only when the run holds no unsettled attempt (including cancellation in
    /// progress and repair-required outcomes), because such a run is durable data rather than
    /// live or ambiguous work and no artifact-specific receipt
    /// interpretation is pending. The caller must have already proven that the recorded
    /// coordinator daemon ended; that evidence is persisted as a `authority_reassigned` event so
    /// the reassignment is auditable.
    ///
    /// # Errors
    ///
    /// Returns an error when the replacement generation is not `expected.generation + 1`, when the
    /// run still has a live attempt, when the current authority is stale, or when persistence
    /// fails.
    pub fn reassign_execution_authority_from_ended_owner(
        &mut self,
        run_id: &str,
        expected: &WorkflowExecutionAuthority,
        replacement: &WorkflowExecutionAuthority,
        evidence: &EndedOwnerEvidence,
        reassigned_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        if expected.generation.checked_add(1) != Some(replacement.generation)
            || expected.daemon_instance_id == replacement.daemon_instance_id
            || expected.fencing_token == replacement.fencing_token
            || evidence.ended_daemon_instance_id != expected.daemon_instance_id
            || evidence.ended_target_artifact_id != expected.target_artifact_id
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow execution authority reassignment is invalid".to_string(),
            ));
        }
        validate_id("replacement artifact", &replacement.target_artifact_id)?;
        validate_id("replacement daemon", &replacement.daemon_instance_id)?;
        validate_id("replacement fence", &replacement.fencing_token)?;

        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        verify_store_schema(db)?;
        Self::validate_quiescent_reassignment_in(db, run_id)?;
        let changed = crate::database::block_on(
            db.exec_update_count(
                &(update("workflow_runs")
                    .value(
                        "target_artifact_id",
                        replacement.target_artifact_id.as_str(),
                    )
                    .value(
                        "coordinator_daemon_instance_id",
                        replacement.daemon_instance_id.as_str(),
                    )
                    .value(
                        "coordinator_generation",
                        database::integer_value(replacement.generation)?,
                    )
                    .value(
                        "coordinator_fencing_token",
                        replacement.fencing_token.as_str(),
                    )
                    .value("updated_at_ms", database::integer_value(reassigned_at_ms)?)
                    .where_eq("run_id", run_id)
                    .where_eq("target_artifact_id", expected.target_artifact_id.as_str())
                    .where_eq(
                        "coordinator_daemon_instance_id",
                        expected.daemon_instance_id.as_str(),
                    )
                    .where_eq(
                        "coordinator_generation",
                        database::integer_value(expected.generation)?,
                    )
                    .where_eq("coordinator_fencing_token", expected.fencing_token.as_str())
                    .where_in("status", vec!["running", "paused", "repair_required"])),
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow execution authority reassignment lost its ownership race: {run_id}"
            )));
        }
        append_event(
            db,
            run_id,
            "authority_reassigned",
            &serde_json::json!({
                "from": expected,
                "to": replacement,
                "evidence": evidence,
                "reassigned_at_ms": reassigned_at_ms,
            })
            .to_string(),
            reassigned_at_ms,
        )?;
        database::commit(transaction)?;
        Ok(())
    }

    /// Return bounded direct child links without replaying workflow history.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/limit, malformed persisted targets, or query failure.
    pub fn child_run_links(
        &self,
        parent_run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowRunLink>, WorkflowStoreError> {
        validate_id("parent_run_id", parent_run_id)?;
        bounded_limit(limit)?;
        database::rows(
            &self.connection,
            &select("workflow_run_links")
                .projections(
                    [
                        "root_run_id",
                        "parent_run_id",
                        "parent_node_id",
                        "parent_activation_id",
                        "parent_attempt",
                        "child_run_id",
                        "version",
                        "target_json",
                        "depth",
                        "created_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("parent_run_id", parent_run_id)
                .sort("created_at_ms", SortDirection::Asc)
                .sort("child_run_id", SortDirection::Asc)
                .limit(limit),
        )?
        .iter()
        .map(decode_run_link)
        .collect()
    }

    /// Return bounded descendant runs for one root without replaying workflow history.
    ///
    /// Results are ordered by durable link creation and include each exact parent/child link so
    /// clients can reconstruct the bounded hierarchy without private store access.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/limit, malformed persisted targets or run rows,
    /// missing linked runs, or query failure.
    pub fn descendant_run_summaries(
        &self,
        root_run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowDescendantRunSummary>, WorkflowStoreError> {
        validate_id("root_run_id", root_run_id)?;
        bounded_limit(limit)?;
        let tx = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*tx;
        let rows = database::rows(
            db,
            &select("workflow_run_links")
                .projections(
                    [
                        "root_run_id",
                        "parent_run_id",
                        "parent_node_id",
                        "parent_activation_id",
                        "parent_attempt",
                        "child_run_id",
                        "version",
                        "target_json",
                        "depth",
                        "created_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("root_run_id", root_run_id)
                .sort("created_at_ms", SortDirection::Asc)
                .sort("child_run_id", SortDirection::Asc)
                .limit(limit),
        )?;
        let result = rows
            .iter()
            .map(|row| {
                let link = decode_run_link(row)?;
                let run = Self::run_summary_in(db, &link.child_run_id)?.ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "workflow run link references a missing child run".into(),
                    )
                })?;
                Ok(WorkflowDescendantRunSummary { link, run })
            })
            .collect::<Result<Vec<_>, WorkflowStoreError>>()?;
        database::commit(tx)?;
        Ok(result)
    }

    /// Return the exact parent link for one child without replaying history.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed child identity, malformed persisted target, or query failure.
    pub fn parent_run_link(
        &self,
        child_run_id: &str,
    ) -> Result<Option<WorkflowRunLink>, WorkflowStoreError> {
        validate_id("child_run_id", child_run_id)?;
        database::rows(
            &self.connection,
            &select("workflow_run_links")
                .projections(
                    [
                        "root_run_id",
                        "parent_run_id",
                        "parent_node_id",
                        "parent_activation_id",
                        "parent_attempt",
                        "child_run_id",
                        "version",
                        "target_json",
                        "depth",
                        "created_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("child_run_id", child_run_id)
                .limit(1),
        )?
        .first()
        .map(decode_run_link)
        .transpose()
    }

    /// Return exact persisted execution limits for one run.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid persisted limits, or query failure.
    pub fn run_limits(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowRunLimits>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let rows = database::rows(
            &self.connection,
            &select("workflow_runs")
                .projections(
                    [
                        "deadline_at_ms",
                        "node_execution_cap",
                        "concurrency_cap",
                        "cycle_cap",
                        "retry_cap",
                        "recursion_depth_cap",
                        "descendant_cap",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("run_id", run_id)
                .limit(1),
        )?;
        rows.first()
            .map(|row| {
                fn cap(
                    row: &switchy_database::Row,
                    column: &str,
                ) -> Result<u32, WorkflowStoreError> {
                    u32::try_from(database::unsigned(row, column)?).map_err(|_| {
                        WorkflowStoreError::InvalidData(format!("workflow {column} exceeds u32"))
                    })
                }
                let limits = WorkflowRunLimits {
                    deadline_at_ms: database::optional_unsigned(row, "deadline_at_ms")?,
                    node_execution_cap: database::unsigned(row, "node_execution_cap")?,
                    concurrency_cap: database::optional_unsigned(row, "concurrency_cap")?
                        .map(|value| {
                            u32::try_from(value).map_err(|_| {
                                WorkflowStoreError::InvalidData(
                                    "workflow concurrency cap exceeds u32".into(),
                                )
                            })
                        })
                        .transpose()?,
                    cycle_cap: cap(row, "cycle_cap")?,
                    retry_cap: cap(row, "retry_cap")?,
                    recursion_depth_cap: cap(row, "recursion_depth_cap")?,
                    descendant_cap: cap(row, "descendant_cap")?,
                };
                validate_run_limits(&limits)?;
                Ok(limits)
            })
            .transpose()
    }

    /// Return bounded persisted output metadata without loading inline output values.
    ///
    /// # Errors
    ///
    /// Returns an error when identity/limit validation or the bounded query fails.
    pub fn output_summaries(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowOutputSummary>, WorkflowStoreError> {
        Self::output_summaries_after_in(&self.connection, run_id, None, limit)
    }

    /// Read canonical output metadata using an exclusive identity cursor.
    ///
    /// # Errors
    /// Rejects invalid identities or limits and database failures. Does not load values.
    pub fn output_summaries_after(
        &self,
        run_id: &str,
        after_output_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WorkflowOutputSummary>, WorkflowStoreError> {
        Self::output_summaries_after_in(&self.connection, run_id, after_output_id, limit)
    }

    /// Read one exact checksum-verified output without scanning prior outputs.
    ///
    /// # Errors
    /// Rejects invalid identities, missing outputs, oversized payloads, corrupt JSON,
    /// checksum disagreement, or database failures. No artifact is opened implicitly.
    pub fn inspect_exact_output(
        &self,
        run_id: &str,
        output_id: &str,
    ) -> Result<bcode_workflow::WorkflowOutputInspection, WorkflowStoreError> {
        Self::inspect_exact_output_in(&self.connection, run_id, output_id)
    }

    /// Inspect an output with its immutable activation-bound producer.
    ///
    /// No current-graph substitution or evidence inference is permitted. This is a
    /// bounded non-mutating read and does not certify delivery or command success.
    ///
    /// # Errors
    /// Returns an error for invalid, missing, corrupt, oversized, or unverifiable
    /// outputs and executable bindings, or database failures.
    pub fn inspect_output_provenance(
        &self,
        run_id: &str,
        output_id: &str,
    ) -> Result<bcode_workflow::WorkflowOutputProvenance, WorkflowStoreError> {
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let output = Self::inspect_exact_output_in(db, run_id, output_id)?;
        let producer =
            Self::activation_graph_node_in(db, run_id, &output.node_id, &output.activation_id)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData("output producer binding is missing".into())
                })?;
        let evidence = bcode_workflow::WorkflowOutputProvenance {
            version: bcode_workflow::WorkflowOutputProvenance::VERSION,
            output,
            producer_revision: producer.revision,
            producer: producer.node,
        };
        database::commit(transaction)?;
        Ok(evidence)
    }

    /// Inspect canonical output, producer, and retained activation input atomically.
    ///
    /// This bounded read does not interpret plugin plans or certify execution effects.
    ///
    /// # Errors
    /// Rejects unverifiable provenance, absent, oversized or malformed input, and
    /// database failures. No current input or graph is substituted for retained state.
    pub fn inspect_output_execution(
        &self,
        run_id: &str,
        output_id: &str,
    ) -> Result<bcode_workflow::WorkflowOutputExecutionEvidence, WorkflowStoreError> {
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let output = Self::inspect_exact_output_in(db, run_id, output_id)?;
        let producer =
            Self::activation_graph_node_in(db, run_id, &output.node_id, &output.activation_id)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData("output producer binding is missing".into())
                })?;
        let provenance = bcode_workflow::WorkflowOutputProvenance {
            version: bcode_workflow::WorkflowOutputProvenance::VERSION,
            output,
            producer_revision: producer.revision,
            producer: producer.node,
        };
        let rows = database::rows(
            db,
            &select("workflow_activations")
                .project_as(
                    bounded_text(
                        identifier("input_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        switchy_database::DatabaseValue::Null,
                    ),
                    "input_json",
                )
                .where_eq("run_id", run_id)
                .where_eq("node_id", provenance.output.node_id.as_str())
                .where_eq("activation_id", provenance.output.activation_id.as_str())
                .limit(1),
        )?;
        let input = database::text(
            rows.first().ok_or(switchy_database::DatabaseError::NoRow)?,
            "input_json",
        )?;
        let evidence = bcode_workflow::WorkflowOutputExecutionEvidence {
            version: bcode_workflow::WorkflowOutputExecutionEvidence::VERSION,
            provenance,
            admitted_input: serde_json::from_str(&input)?,
        };
        database::commit(transaction)?;
        Ok(evidence)
    }

    /// Return bounded validated output values without replaying workflow history.
    ///
    /// Values are read from the canonical output rows and checksum-verified before being returned.
    ///
    /// # Errors
    ///
    /// Returns an error when identity/limit validation, JSON decoding, checksum verification, or
    /// the bounded query fails.
    pub fn validated_outputs(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<ValidatedOutput>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;

        bounded_limit(limit)?;
        let rows = database::rows(
            &self.connection,
            &select("workflow_outputs")
                .projections(
                    [
                        "output_id",
                        "run_id",
                        "node_id",
                        "activation_id",
                        "schema_id",
                        "schema_version",
                        "value_json",
                        "artifact_reference",
                        "checksum_sha256",
                        "created_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("run_id", run_id)
                .sort("created_at_ms", SortDirection::Asc)
                .sort("output_id", SortDirection::Asc)
                .limit(limit),
        )?;
        rows.iter()
            .map(|row| {
                let row = (
                    database::text(row, "output_id")?,
                    database::text(row, "run_id")?,
                    database::text(row, "node_id")?,
                    database::text(row, "activation_id")?,
                    database::text(row, "schema_id")?,
                    u32::try_from(database::unsigned(row, "schema_version")?).map_err(|_| {
                        WorkflowStoreError::InvalidData("output schema version exceeds u32".into())
                    })?,
                    database::text(row, "value_json")?,
                    database::optional_text(row, "artifact_reference")?,
                    database::text(row, "checksum_sha256")?,
                    database::unsigned(row, "created_at_ms")?,
                );
                if sha256_hex(row.6.as_bytes()) != row.8 {
                    return Err(WorkflowStoreError::InvalidData(format!(
                        "workflow output checksum is inconsistent: {}",
                        row.0
                    )));
                }
                Ok(ValidatedOutput {
                    output_id: row.0,
                    run_id: row.1,
                    node_id: row.2,
                    activation_id: row.3,
                    schema_id: row.4,
                    schema_version: row.5,
                    value: serde_json::from_str(&row.6)?,
                    artifact_reference: row.7,
                    created_at_ms: row.9,
                })
            })
            .collect()
    }

    /// Return bounded typed repeat outcomes without replaying workflow history.
    ///
    /// # Errors
    ///
    /// Returns an error when identity/limit validation, admitted graph/output decoding, or querying
    /// fails. Malformed persisted typed outcomes fail closed.
    pub fn repeat_outcomes(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowRepeatOutcomeSummary>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;

        bounded_limit(limit)?;
        let path = JsonPath::member("iterations_completed")
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let rows = database::rows(
            &self.connection,
            &select("workflow_outputs")
                .projections(
                    [
                        "run_id",
                        "node_id",
                        "activation_id",
                        "output_id",
                        "value_json",
                        "checksum_sha256",
                        "created_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("run_id", run_id)
                .filter(Box::new(json_type_is(
                    identifier("value_json"),
                    path,
                    JsonType::Integer,
                )))
                .sort("created_at_ms", SortDirection::Asc)
                .sort("output_id", SortDirection::Asc)
                .limit(limit),
        )?;
        rows.iter()
            .map(|row| {
                let run_id = database::text(row, "run_id")?;
                let node_id = database::text(row, "node_id")?;
                let activation_id = database::text(row, "activation_id")?;
                let output_id = database::text(row, "output_id")?;
                let value_json = database::text(row, "value_json")?;
                let checksum = database::text(row, "checksum_sha256")?;
                let created_at = database::unsigned(row, "created_at_ms")?;

                let node = self.run_graph_node(&run_id, &node_id)?.ok_or_else(|| {
                    WorkflowStoreError::InvalidData(format!(
                        "repeat output references missing run-graph node: {node_id}"
                    ))
                })?;
                if node.kind != bcode_workflow::NodeKind::Repeat
                    || node
                        .configuration
                        .get("exhaustion_policy")
                        .and_then(serde_json::Value::as_str)
                        != Some("emit_outcome")
                {
                    return Ok(None);
                }
                let value: serde_json::Value = serde_json::from_str(&value_json)?;
                if value
                    .get("iterations_completed")
                    .and_then(serde_json::Value::as_u64)
                    .is_none()
                {
                    return Ok(None);
                }
                let outcome: bcode_workflow::WorkflowRepeatOutcome<serde_json::Value> =
                    serde_json::from_value(value)?;
                Ok(Some(WorkflowRepeatOutcomeSummary {
                    run_id,
                    node_id,
                    activation_id,
                    output_id,
                    outcome: outcome.outcome,
                    iterations_completed: outcome.iterations_completed,
                    max_iterations: outcome.max_iterations,
                    cycle_cap: outcome.cycle_cap,
                    effective_iteration_bound: outcome.effective_iteration_bound,
                    checksum_sha256: checksum,
                    created_at_ms: created_at,
                }))
            })
            .filter_map(Result::transpose)
            .collect()
    }

    /// Return bounded grants for one run ordered by grant time and identity.
    ///
    /// Grants are deliberately queried by exact run identity. Child runs never inherit parent
    /// grant rows implicitly.
    ///
    /// # Errors
    ///
    /// Returns an error when identity/limit validation, JSON decoding, or the query fails.
    pub fn grants_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowGrant>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;

        bounded_limit(limit)?;
        database::rows(
            &self.connection,
            &select("workflow_grants")
                .projections(
                    [
                        "grant_id",
                        "run_id",
                        "node_id",
                        "scope_json",
                        "granted_at_ms",
                        "expires_at_ms",
                        "max_uses",
                        "uses_consumed",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("run_id", run_id)
                .sort("granted_at_ms", SortDirection::Asc)
                .sort("grant_id", SortDirection::Asc)
                .limit(limit),
        )?
        .iter()
        .map(|row| {
            let grant_id = database::text(row, "grant_id")?;
            let run_id = database::text(row, "run_id")?;
            let node_id = database::text(row, "node_id")?;
            let scope_json = database::text(row, "scope_json")?;
            let granted_at_ms = database::unsigned(row, "granted_at_ms")?;
            let expires_at_ms = database::optional_unsigned(row, "expires_at_ms")?;
            let max_uses = database::optional_unsigned(row, "max_uses")?
                .map(u32::try_from)
                .transpose()
                .map_err(|_| WorkflowStoreError::InvalidData("max_uses exceeds u32".into()))?;
            let uses_consumed = u32::try_from(database::unsigned(row, "uses_consumed")?)
                .map_err(|_| WorkflowStoreError::InvalidData("uses_consumed exceeds u32".into()))?;
            Ok(WorkflowGrant {
                grant_id,
                run_id,
                node_id,
                scope: serde_json::from_str(&scope_json)?,
                granted_at_ms,
                expires_at_ms,
                max_uses,
                uses_consumed,
            })
        })
        .collect()
    }

    /// Return bounded active resource leases for one run.
    ///
    /// # Errors
    ///
    /// Returns an error when identity/limit validation or the bounded query fails.
    pub fn resource_leases_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowResourceLease>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;

        bounded_limit(limit)?;
        database::rows(
            &self.connection,
            &select("workflow_resource_leases")
                .projections(
                    [
                        "lease_id",
                        "run_id",
                        "node_id",
                        "activation_id",
                        "resource_key",
                        "mode",
                        "acquired_at_ms",
                        "expires_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("run_id", run_id)
                .where_eq("released_at_ms", switchy_database::DatabaseValue::Null)
                .sort("acquired_at_ms", SortDirection::Asc)
                .sort("lease_id", SortDirection::Asc)
                .limit(limit),
        )?
        .iter()
        .map(|row| {
            let lease_id = database::text(row, "lease_id")?;
            let run_id = database::text(row, "run_id")?;
            let node_id = database::text(row, "node_id")?;
            let activation_id = database::text(row, "activation_id")?;
            let resource_key = database::text(row, "resource_key")?;
            let mode = database::text(row, "mode")?;
            let acquired_at_ms = database::unsigned(row, "acquired_at_ms")?;
            let expires_at_ms = database::optional_unsigned(row, "expires_at_ms")?;
            Ok(WorkflowResourceLease {
                lease_id,
                run_id,
                node_id,
                activation_id,
                resource_key,
                mode: parse_resource_lease_mode(&mode)?,
                acquired_at_ms,
                expires_at_ms,
            })
        })
        .collect()
    }

    /// Persist one pending activation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing run or admitted graph node,
    /// unsupported or damaged graph state, invalid input, duplicate activation,
    /// exceeded execution limits, or database failure.
    pub fn create_activation(
        &mut self,
        activation: &NewActivation,
    ) -> Result<(), WorkflowStoreError> {
        validate_activation(activation)?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        enforce_activation_limits(db, activation)?;
        insert_activation(db, activation)?;
        database::commit(transaction)?;
        Ok(())
    }

    /// Admit an activation against an exact current graph under durable execution authority.
    ///
    /// Input validation, revision selection, ownership verification, and insertion share one
    /// transaction. Existing activation bindings are never changed. Legacy scheduling continues
    /// to use [`Self::create_activation`] until it supports revision-aware reconciliation.
    ///
    /// # Errors
    ///
    /// Returns an error for stale ownership or graph revision, missing or malformed executable
    /// data, invalid input, activation limits, duplicate identity, or persistence failure.
    pub fn create_activation_at_graph_revision(
        &mut self,
        activation: &NewActivation,
        expected_revision: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<(), WorkflowStoreError> {
        validate_activation(activation)?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        Self::verify_execution_authority_in(db, &activation.run_id, authority)?;
        if run_graph::graph_revision(db, &activation.run_id)? != Some(expected_revision) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph revision conflict".to_string(),
            ));
        }
        enforce_activation_limits(db, activation)?;
        let record = Self::current_run_graph_node_in(db, &activation.run_id, &activation.node_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow activation references missing run-graph node".to_string(),
                )
            })?;
        insert_activation_bound_to_node(
            db,
            activation,
            activation_status_for_node(&record.node),
            &record.node,
            record.revision,
        )?;
        record_activation_graph_binding(
            db,
            &activation.run_id,
            &activation.node_id,
            &activation.activation_id,
            expected_revision,
        )?;
        database::commit(transaction)?;
        Ok(())
    }

    /// Persist an immutable workflow decision.
    ///
    /// Re-persisting byte-equivalent content is idempotent. Conflicting content at one decision
    /// identity fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, oversized JSON, missing run/node, identity
    /// conflict, or database failure.
    pub fn persist_decision(
        &mut self,
        decision: &WorkflowDecision,
    ) -> Result<(), WorkflowStoreError> {
        validate_decision(decision)?;
        let value_json = bounded_json("workflow decision", &decision.value)?;

        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let changed = crate::database::block_on(
            db.insert("workflow_decisions")
                .value("decision_id", decision.decision_id.as_str())
                .value("run_id", decision.run_id.as_str())
                .value("node_id", decision.node_id.clone())
                .value("decision_type", decision.decision_type.as_str())
                .value("value_json", value_json.as_str())
                .value(
                    "created_at_ms",
                    database::integer_value(decision.created_at_ms)?,
                )
                .or_ignore()
                .execute_count(db),
        )?;
        if changed == 0 {
            let rows = database::rows(
                db,
                &select("workflow_decisions")
                    .projections(
                        [
                            "run_id",
                            "node_id",
                            "decision_type",
                            "value_json",
                            "created_at_ms",
                        ]
                        .into_iter()
                        .map(|column| {
                            switchy_database::query::Projection::new(
                                switchy_database::query::identifier(column),
                            )
                            .alias(column)
                        })
                        .collect(),
                    )
                    .where_eq("decision_id", decision.decision_id.as_str())
                    .limit(1),
            )?;
            let row = rows.first().ok_or(switchy_database::DatabaseError::NoRow)?;
            let existing = (
                database::text(row, "run_id")?,
                database::optional_text(row, "node_id")?,
                database::text(row, "decision_type")?,
                database::text(row, "value_json")?,
                database::unsigned(row, "created_at_ms")?,
            );
            if existing
                != (
                    decision.run_id.clone(),
                    decision.node_id.clone(),
                    decision.decision_type.clone(),
                    value_json,
                    decision.created_at_ms,
                )
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow decision identity conflict: {}",
                    decision.decision_id
                )));
            }
        } else {
            append_event(
                db,
                &decision.run_id,
                "decision_recorded",
                &serde_json::to_string(decision)?,
                decision.created_at_ms,
            )?;
        }
        database::commit(transaction)?;
        Ok(())
    }

    /// Load bounded newest-first durable decisions for one run.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/limit, malformed persisted JSON, or query failure.
    pub fn decisions_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowDecision>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;

        bounded_limit(limit)?;
        database::rows(
            &self.connection,
            &select("workflow_decisions")
                .projections(
                    [
                        "decision_id",
                        "node_id",
                        "decision_type",
                        "value_json",
                        "created_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("run_id", run_id)
                .sort("created_at_ms", SortDirection::Desc)
                .sort("decision_id", SortDirection::Desc)
                .limit(limit),
        )?
        .iter()
        .map(|row| {
            let decision_id = database::text(row, "decision_id")?;
            let node_id = database::optional_text(row, "node_id")?;
            let decision_type = database::text(row, "decision_type")?;
            let value_json = database::text(row, "value_json")?;
            let created_at_ms = database::unsigned(row, "created_at_ms")?;
            Ok(WorkflowDecision {
                decision_id,
                run_id: run_id.to_string(),
                node_id,
                decision_type,
                value: serde_json::from_str(&value_json)?,
                created_at_ms,
            })
        })
        .collect()
    }

    /// Load one exact durable workflow decision.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid persisted JSON, or database failure.
    pub fn decision(
        &self,
        decision_id: &str,
    ) -> Result<Option<WorkflowDecision>, WorkflowStoreError> {
        validate_id("decision_id", decision_id)?;

        database::rows(
            &self.connection,
            &select("workflow_decisions")
                .projections(
                    [
                        "run_id",
                        "node_id",
                        "decision_type",
                        "value_json",
                        "created_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("decision_id", decision_id)
                .limit(1),
        )?
        .first()
        .map(|row| {
            let run_id = database::text(row, "run_id")?;
            let node_id = database::optional_text(row, "node_id")?;
            let decision_type = database::text(row, "decision_type")?;
            let value_json = database::text(row, "value_json")?;
            let created_at_ms = database::unsigned(row, "created_at_ms")?;
            Ok(WorkflowDecision {
                decision_id: decision_id.to_string(),
                run_id,
                node_id,
                decision_type,
                value: serde_json::from_str(&value_json)?,
                created_at_ms,
            })
        })
        .transpose()
    }

    /// Advance one workflow projection checkpoint monotonically.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/version, checkpoint regression, an event sequence
    /// beyond the durable tail, or database failure.
    pub fn advance_projection_checkpoint(
        &mut self,
        checkpoint: &WorkflowProjectionCheckpoint,
    ) -> Result<(), WorkflowStoreError> {
        validate_projection_checkpoint(checkpoint)?;

        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let rows = database::rows(
            db,
            &select("workflow_events")
                .projections(database::projections(&["event_seq"]))
                .sort("event_seq", SortDirection::Desc)
                .limit(1),
        )?;
        let event_tail = rows
            .first()
            .map(|row| database::unsigned(row, "event_seq"))
            .transpose()?
            .unwrap_or(0);
        if checkpoint.last_event_seq > event_tail {
            return Err(WorkflowStoreError::InvalidData(format!(
                "projection checkpoint {} is beyond event tail {event_tail}",
                checkpoint.last_event_seq
            )));
        }
        let rows = database::rows(
            db,
            &select("workflow_projection_checkpoints")
                .projections(database::projections(&[
                    "projection_version",
                    "last_event_seq",
                ]))
                .where_eq("projection_name", checkpoint.projection_name.as_str())
                .limit(1),
        )?;
        let existing = rows
            .first()
            .map(|row| {
                Ok::<_, WorkflowStoreError>((
                    u32::try_from(database::unsigned(row, "projection_version")?).map_err(
                        |_| {
                            WorkflowStoreError::InvalidData("projection version exceeds u32".into())
                        },
                    )?,
                    database::unsigned(row, "last_event_seq")?,
                ))
            })
            .transpose()?;
        if let Some((version, sequence)) = existing
            && (version != checkpoint.projection_version || sequence > checkpoint.last_event_seq)
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow projection checkpoint cannot regress or change version".to_string(),
            ));
        }
        if existing.is_some() {
            crate::database::block_on(
                db.exec_update_count(
                    &update("workflow_projection_checkpoints")
                        .value(
                            "last_event_seq",
                            database::integer_value(checkpoint.last_event_seq)?,
                        )
                        .where_eq("projection_name", checkpoint.projection_name.as_str()),
                ),
            )?;
        } else {
            crate::database::block_on(
                db.insert("workflow_projection_checkpoints")
                    .value("projection_name", checkpoint.projection_name.as_str())
                    .value(
                        "projection_version",
                        i64::from(checkpoint.projection_version),
                    )
                    .value(
                        "last_event_seq",
                        database::integer_value(checkpoint.last_event_seq)?,
                    )
                    .execute(db),
            )?;
        }
        database::commit(transaction)
    }

    /// Load one exact durable workflow projection checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity or database failure.
    pub fn projection_checkpoint(
        &self,
        projection_name: &str,
    ) -> Result<Option<WorkflowProjectionCheckpoint>, WorkflowStoreError> {
        validate_id("projection_name", projection_name)?;

        database::rows(
            &self.connection,
            &select("workflow_projection_checkpoints")
                .projections(database::projections(&[
                    "projection_version",
                    "last_event_seq",
                ]))
                .where_eq("projection_name", projection_name)
                .limit(1),
        )?
        .first()
        .map(|row| {
            Ok(WorkflowProjectionCheckpoint {
                projection_name: projection_name.to_string(),
                projection_version: u32::try_from(database::unsigned(row, "projection_version")?)
                    .map_err(|_| {
                    WorkflowStoreError::InvalidData("projection version exceeds u32".into())
                })?,
                last_event_seq: database::unsigned(row, "last_event_seq")?,
            })
        })
        .transpose()
    }

    /// Acquire all declared resources for one activation before dispatch admission.
    ///
    /// Acquisitions use deterministic claim order and stable lease identities. Any conflict rolls
    /// back the complete set, so parallel branches never partially hold resources.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed claims, incompatible active leases, or database failure.
    #[cfg(test)]
    fn acquire_activation_resources(
        &self,
        activation: &PendingActivation,
        acquired_at_ms: u64,
        expected_authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<(), WorkflowStoreError> {
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        if execution_authority_in(db, &activation.run_id)?.as_ref() != expected_authority {
            return Err(WorkflowStoreError::InvalidData(
                "resource acquisition execution authority changed".into(),
            ));
        }
        Self::acquire_activation_resources_in_transaction(db, activation, acquired_at_ms)?;
        database::commit(transaction)?;
        Ok(())
    }

    pub(crate) fn acquire_activation_resources_in_transaction(
        transaction: &dyn switchy_database::Database,
        activation: &PendingActivation,
        acquired_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        let mut claims = activation.node.resources.clone();
        claims.sort_by(|left, right| left.resource.cmp(&right.resource));
        for claim in claims {
            let mode = match claim.access {
                bcode_workflow::ResourceAccess::Read => ResourceLeaseMode::Read,
                bcode_workflow::ResourceAccess::Write => ResourceLeaseMode::Write,
            };
            let lease = WorkflowResourceLease {
                lease_id: format!(
                    "{}:{}:{}",
                    activation.activation_id,
                    claim.resource,
                    mode.as_str()
                ),
                run_id: activation.run_id.clone(),
                node_id: activation.node_id.clone(),
                activation_id: activation.activation_id.clone(),
                resource_key: claim.resource,
                mode,
                acquired_at_ms,
                expires_at_ms: None,
            };
            let existing = resource_lease(transaction, &lease.lease_id)?;
            if let Some((existing, released_at_ms)) = existing {
                if released_at_ms.is_none()
                    && existing.run_id == lease.run_id
                    && existing.node_id == lease.node_id
                    && existing.activation_id == lease.activation_id
                    && existing.resource_key == lease.resource_key
                    && existing.mode == lease.mode
                    && existing.expires_at_ms == lease.expires_at_ms
                {
                    continue;
                }
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow resource lease identity conflict: {}",
                    lease.lease_id
                )));
            }

            let mut query = select("workflow_resource_leases")
                .projections(database::projections(&["lease_id"]))
                .where_eq("run_id", lease.run_id.as_str())
                .where_eq("resource_key", lease.resource_key.as_str())
                .where_eq("released_at_ms", V::Null)
                .filter(Box::new(Or {
                    conditions: vec![
                        Box::new(where_eq("expires_at_ms", V::Null)),
                        Box::new(where_gt(
                            "expires_at_ms",
                            database::integer_value(lease.acquired_at_ms)?,
                        )),
                    ],
                }))
                .limit(1);
            if lease.mode != ResourceLeaseMode::Write {
                query = query.where_eq("mode", "write");
            }
            let conflict = !database::rows(transaction, &query)?.is_empty();
            if conflict {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow resource is already leased incompatibly: {}",
                    lease.resource_key
                )));
            }
            crate::database::block_on(
                transaction
                    .insert("workflow_resource_leases")
                    .value("lease_id", lease.lease_id.as_str())
                    .value("run_id", lease.run_id.as_str())
                    .value("node_id", lease.node_id.as_str())
                    .value("activation_id", lease.activation_id.as_str())
                    .value("resource_key", lease.resource_key.as_str())
                    .value("mode", lease.mode.as_str())
                    .value(
                        "acquired_at_ms",
                        database::integer_value(lease.acquired_at_ms)?,
                    )
                    .value(
                        "expires_at_ms",
                        database::optional_integer_value(lease.expires_at_ms)?,
                    )
                    .execute(transaction),
            )?;
            append_event_in(
                transaction,
                &lease.run_id,
                "resource_lease_acquired",
                &serde_json::to_string(&lease)?,
                acquired_at_ms,
            )?;
        }
        Ok(())
    }

    /// Persist or recover one exact workflow activation/attempt execution-session link.
    ///
    /// Repeating the identical link is idempotent. Reusing the activation/attempt for another
    /// session, or reusing one session for another activation/attempt, fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/version-incompatible identities, missing canonical
    /// activation/attempt state, conflicting links, or database failure.
    pub fn link_execution_session(
        &mut self,
        link: &WorkflowExecutionSessionLink,
    ) -> Result<bool, WorkflowStoreError> {
        validate_execution_session_link(link)?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let existing = execution_session_link_by_activation(db, link)?;
        if let Some(existing) = existing {
            if existing == *link {
                database::commit(transaction)?;
                return Ok(false);
            }
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow execution-session link conflicts for {}/{}/{}/{}",
                link.run_id, link.node_id, link.activation_id, link.attempt
            )));
        }

        let conflicting_session = database::rows(
            db,
            &select("workflow_execution_sessions")
                .projections(database::projections(&["session_id"]))
                .where_eq("session_id", link.session_id.as_str())
                .limit(1),
        )?
        .first()
        .map(|_| ());
        if conflicting_session.is_some() {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow execution session '{}' is already linked",
                link.session_id
            )));
        }
        let exists = !database::rows(
            db,
            &select("workflow_attempts")
                .projections(database::projections(&["attempt"]))
                .where_eq("run_id", link.run_id.as_str())
                .where_eq("node_id", link.node_id.as_str())
                .where_eq("activation_id", link.activation_id.as_str())
                .where_eq("attempt", i64::from(link.attempt))
                .limit(1),
        )?
        .is_empty();
        let inserted = if exists {
            crate::database::block_on(
                db.insert("workflow_execution_sessions")
                    .value("version", i64::from(link.version))
                    .value("run_id", link.run_id.as_str())
                    .value("node_id", link.node_id.as_str())
                    .value("activation_id", link.activation_id.as_str())
                    .value("attempt", i64::from(link.attempt))
                    .value("session_id", link.session_id.as_str())
                    .value("workspace_snapshot", link.workspace_snapshot.as_str())
                    .value(
                        "created_at_ms",
                        database::integer_value(link.created_at_ms)?,
                    )
                    .execute(db),
            )?;
            1
        } else {
            0
        };
        if inserted != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "workflow execution-session link requires an existing exact attempt".to_string(),
            ));
        }
        database::commit(transaction)?;
        Ok(true)
    }

    /// Load one exact workflow activation/attempt execution-session link.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identities, damaged stored state, or query failure.
    pub(crate) fn execution_session_link_in(
        db: &dyn switchy_database::Database,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        attempt: u32,
    ) -> Result<Option<WorkflowExecutionSessionLink>, WorkflowStoreError> {
        validate_execution_session_link_identity(run_id, node_id, activation_id, attempt)?;
        let mut query = select("workflow_execution_sessions")
            .projections(database::projections(&[
                "version",
                "attempt",
                "created_at_ms",
            ]))
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .where_eq("attempt", i64::from(attempt))
            .limit(1);
        for column in [
            "run_id",
            "node_id",
            "activation_id",
            "session_id",
            "workspace_snapshot",
        ] {
            query = query.project_as(
                bounded_text(
                    identifier(column),
                    MAX_ID_BYTES as u64,
                    V::String(String::new()),
                ),
                column,
            );
        }
        database::rows(db, &query)?
            .first()
            .map(|row| {
                let version = u32::try_from(database::unsigned(row, "version")?).map_err(|_| {
                    WorkflowStoreError::InvalidData("execution-session version exceeds u32".into())
                })?;
                let attempt = u32::try_from(database::unsigned(row, "attempt")?).map_err(|_| {
                    WorkflowStoreError::InvalidData("execution-session attempt exceeds u32".into())
                })?;
                let link = WorkflowExecutionSessionLink {
                    version,
                    run_id: database::text(row, "run_id")?,
                    node_id: database::text(row, "node_id")?,
                    activation_id: database::text(row, "activation_id")?,
                    attempt,
                    session_id: database::text(row, "session_id")?,
                    workspace_snapshot: database::text(row, "workspace_snapshot")?,
                    created_at_ms: database::unsigned(row, "created_at_ms")?,
                };
                validate_execution_session_link(&link)?;
                Ok(link)
            })
            .transpose()
    }

    /// # Errors
    /// Returns an error for malformed identities, inconsistent persisted state, or storage failure.
    pub fn execution_session_link(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        attempt: u32,
    ) -> Result<Option<WorkflowExecutionSessionLink>, WorkflowStoreError> {
        Self::execution_session_link_in(&self.connection, run_id, node_id, activation_id, attempt)
    }

    /// Return bounded execution-session links for one run without opening session history.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid bound, damaged stored state, or query
    /// failure.
    pub fn execution_session_links_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowExecutionSessionLink>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;

        bounded_limit(limit)?;
        database::rows(
            &self.connection,
            &select("workflow_execution_sessions")
                .projections(
                    [
                        "version",
                        "run_id",
                        "node_id",
                        "activation_id",
                        "attempt",
                        "session_id",
                        "workspace_snapshot",
                        "created_at_ms",
                    ]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
                )
                .where_eq("run_id", run_id)
                .sort("node_id", SortDirection::Asc)
                .sort("activation_id", SortDirection::Asc)
                .sort("attempt", SortDirection::Asc)
                .limit(limit),
        )?
        .iter()
        .map(decode_execution_session_link)
        .collect()
    }

    /// Reconcile receipt-less prepared attempts after restart without external dispatch.
    ///
    /// Mutating attempts are atomically marked repair-required. Read-only attempts remain prepared
    /// for an owner-specific reconciler or safe redispatch policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded query or transition fails.
    pub fn reconcile_prepared_attempts(
        &mut self,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReconciliationSummary, WorkflowStoreError> {
        self.reconcile_prepared_attempts_scoped(None, limit, reconciled_at_ms, None)
    }

    /// Reconcile receipt-less attempts for one authority-qualified run.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity, bound, query, or transition is invalid.
    pub fn reconcile_prepared_attempts_for_run(
        &mut self,
        run_id: &str,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReconciliationSummary, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        self.reconcile_prepared_attempts_scoped(Some(run_id), limit, reconciled_at_ms, None)
    }

    /// Reconcile receipt-less preparations under the caller's held execution authority.
    ///
    /// # Errors
    /// Rejects stale authority within the recovery transaction, invalid bounds,
    /// malformed durable attempts, or persistence failures.
    pub fn reconcile_owned_prepared_attempts_for_run(
        &mut self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReconciliationSummary, WorkflowStoreError> {
        self.reconcile_prepared_attempts_scoped(
            Some(run_id),
            limit,
            reconciled_at_ms,
            Some(authority),
        )
    }

    /// Classify only preparations whose owner evidence was examined by the caller.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, oversized batches, or persistence failures.
    pub fn reconcile_examined_preparations(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        identities: &[String],
        reconciled_at_ms: u64,
    ) -> Result<ReconciliationSummary, WorkflowStoreError> {
        bounded_limit(identities.len().max(1))?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;

        Self::verify_execution_authority_in(db, run_id, authority)?;
        let mut summary = ReconciliationSummary::default();
        for identity in identities {
            let rows = database::rows(
                db,
                &select("workflow_attempts")
                    .projections(database::projections(&["side_effect"]))
                    .where_eq("run_id", run_id)
                    .where_eq("dispatch_identity", identity.as_str())
                    .where_eq("status", "prepared")
                    .where_eq("receipt_json", switchy_database::DatabaseValue::Null)
                    .limit(1),
            )?;
            let effect = rows
                .first()
                .map(|row| database::text(row, "side_effect"))
                .transpose()?;
            match effect.as_deref() {
                Some("mutating") => {
                    crate::database::block_on(
                        db.exec_update_count(
                            &update("workflow_attempts")
                                .value("status", "repair_required")
                                .value("terminal_at_ms", database::integer_value(reconciled_at_ms)?)
                                .where_eq("dispatch_identity", identity.as_str()),
                        ),
                    )?;
                    crate::database::block_on(
                        db.exec_update_count(
                            &update("workflow_runs")
                                .value("status", "repair_required")
                                .value("updated_at_ms", database::integer_value(reconciled_at_ms)?)
                                .where_eq("run_id", run_id),
                        ),
                    )?;
                    append_event(
                        db,
                        run_id,
                        "attempt_repair_required",
                        &serde_json::json!({"dispatch_identity": identity}).to_string(),
                        reconciled_at_ms,
                    )?;
                    summary.repair_required.push(identity.clone());
                }
                Some("read_only") => summary.safe_prepared.push(identity.clone()),
                None => {}
                Some(_) => {
                    return Err(WorkflowStoreError::InvalidData(
                        "unknown preparation side effect".into(),
                    ));
                }
            }
        }
        database::commit(transaction)?;
        Ok(summary)
    }

    fn reconcile_prepared_attempts_scoped(
        &self,
        run_id: Option<&str>,
        limit: usize,
        reconciled_at_ms: u64,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<ReconciliationSummary, WorkflowStoreError> {
        if limit == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "reconciliation limit must be positive".to_string(),
            ));
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;

        if let Some(authority) = authority {
            let run_id = run_id.ok_or_else(|| {
                WorkflowStoreError::InvalidData("owned recovery requires a run".into())
            })?;
            Self::verify_execution_authority_in(db, run_id, authority)?;
        }
        let mut query = select("workflow_attempts")
            .projections(database::projections(&[
                "run_id",
                "dispatch_identity",
                "side_effect",
            ]))
            .where_eq("status", "prepared")
            .sort("prepared_at_ms", SortDirection::Asc)
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(usize::try_from(limit).unwrap_or(usize::MAX));
        if let Some(run_id) = run_id {
            query = query.where_eq("run_id", run_id);
        }
        let rows = database::rows(db, &query)?
            .iter()
            .map(|row| {
                Ok::<_, WorkflowStoreError>((
                    database::text(row, "run_id")?,
                    database::text(row, "dispatch_identity")?,
                    database::text(row, "side_effect")?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut summary = ReconciliationSummary::default();
        for (run_id, identity, side_effect) in rows {
            if side_effect == DispatchSideEffect::Mutating.as_str() {
                crate::database::block_on(
                    db.exec_update_count(
                        &update("workflow_attempts")
                            .value("status", "repair_required")
                            .value("terminal_at_ms", database::integer_value(reconciled_at_ms)?)
                            .where_eq("dispatch_identity", identity.as_str())
                            .where_eq("status", "prepared"),
                    ),
                )?;
                crate::database::block_on(
                    db.exec_update_count(
                        &update("workflow_runs")
                            .value("status", "repair_required")
                            .value("updated_at_ms", database::integer_value(reconciled_at_ms)?)
                            .where_eq("run_id", run_id.as_str()),
                    ),
                )?;
                append_event(
                    db,
                    &run_id,
                    "attempt_repair_required",
                    &serde_json::json!({"dispatch_identity": identity}).to_string(),
                    reconciled_at_ms,
                )?;
                summary.repair_required.push(identity);
            } else {
                summary.safe_prepared.push(identity);
            }
        }
        database::commit(transaction)?;
        Ok(summary)
    }

    /// Return bounded active run IDs requiring startup ownership qualification.
    ///
    /// # Errors
    ///
    /// Returns an error when the bound or query is invalid.
    pub fn recovery_candidate_run_ids(
        &self,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        self.recovery_candidate_run_ids_after("", limit)
    }

    /// Read a keyset page of recoverable runs, ordered by stable run identity.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds or database failures.
    pub fn recovery_candidate_run_ids_after(
        &self,
        after_run_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        bounded_limit(limit)?;
        database::rows(
            &self.connection,
            &select("workflow_runs")
                .project_as(col("workflow_runs", "run_id"), "run_id")
                .joins(vec![join_columns(
                    "workflow_recovery_barriers",
                    col("workflow_recovery_barriers", "run_id"),
                    col("workflow_runs", "run_id"),
                    true,
                )])
                .filter(Box::new(where_or(vec![
                    qualified_text_membership(
                        "workflow_runs",
                        "status",
                        ["running".into(), "paused".into()],
                    ),
                    Box::new(where_and(vec![
                        Box::new(where_eq_expression(
                            col("workflow_runs", "status"),
                            "repair_required",
                        )),
                        Box::new(where_not_eq_expression(
                            col("workflow_recovery_barriers", "run_id"),
                            switchy_database::DatabaseValue::Null,
                        )),
                    ])),
                ])))
                .where_gt_expression(col("workflow_runs", "run_id"), after_run_id)
                .sorts(vec![switchy_database::query::sort(
                    col("workflow_runs", "run_id"),
                    SortDirection::Asc,
                )])
                .limit(limit),
        )?
        .iter()
        .map(|row| database::text(row, "run_id"))
        .collect()
    }

    /// Whether periodic driving is needed, including paused inherited cancellation.
    ///
    /// # Errors
    /// Rejects malformed or missing parent state and database failures.
    pub fn needs_continuation(&self, run_id: &str) -> Result<bool, WorkflowStoreError> {
        let Some(run) = self.run_summary(run_id)? else {
            return Ok(false);
        };
        if run.status == RunStatus::Running {
            return Ok(true);
        }
        if !matches!(run.status, RunStatus::Paused | RunStatus::RepairRequired) {
            return Ok(false);
        }
        if run.cancellation_requested_at_ms.is_some() {
            return Ok(true);
        }
        let Some(link) = self.parent_run_link(run_id)? else {
            return Ok(false);
        };
        let parent = self.run_summary(&link.parent_run_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("continuation parent missing".to_owned())
        })?;
        Ok(parent.cancellation_requested_at_ms.is_some())
    }

    /// Return a keyset page of run identities for notification-independent discovery.
    ///
    /// All statuses are included so each query examines at most one page rather than scanning
    /// an unbounded number of inactive runs. Callers must check current run status and qualify
    /// execution ownership before driving, and restart after reaching an empty page.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit is invalid or the query fails.
    pub fn continuation_run_ids_after(
        &self,
        after_run_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        bounded_limit(limit)?;
        database::rows(
            &self.connection,
            &select("workflow_runs")
                .projections(database::projections(&["run_id"]))
                .where_gt("run_id", after_run_id)
                .sort("run_id", SortDirection::Asc)
                .limit(limit),
        )?
        .iter()
        .map(|row| database::text(row, "run_id"))
        .collect()
    }

    /// Discover the next bounded page, durably rotating this artifact's sweep cursor.
    ///
    /// Discovery grants no execution authority. A crash may skip a returned page for
    /// the current sweep, but wraparound revisits it; no canonical work is acknowledged.
    /// Separate artifacts cannot consume each other's discovery progress.
    ///
    /// # Errors
    /// Rejects invalid bounds, malformed identities, damaged cursors, or storage errors.
    pub fn advance_continuation_discovery(
        &mut self,
        artifact_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        validate_id("discovery artifact", artifact_id)?;
        bounded_limit(limit)?;

        let tx = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*tx;
        let rows = database::rows(
            db,
            &select("workflow_discovery_cursors")
                .project_as(
                    bounded_text(
                        identifier("after_run_id"),
                        1024,
                        switchy_database::DatabaseValue::Null,
                    ),
                    "after_run_id",
                )
                .where_eq("artifact_id", artifact_id)
                .limit(1),
        )?;
        let cursor = rows
            .first()
            .map(|row| database::text(row, "after_run_id"))
            .transpose()?
            .unwrap_or_default();
        let page = database::rows(
            db,
            &select("workflow_runs")
                .projections(database::projections(&["run_id"]))
                .where_gt("run_id", cursor.as_str())
                .sort("run_id", SortDirection::Asc)
                .limit(limit),
        )?
        .iter()
        .map(|row| database::text(row, "run_id"))
        .collect::<Result<Vec<_>, _>>()?;
        let next = page.last().map_or("", String::as_str);
        if rows.is_empty() {
            crate::database::block_on(
                db.insert("workflow_discovery_cursors")
                    .value("artifact_id", artifact_id)
                    .value("after_run_id", next)
                    .execute(db),
            )?;
        } else {
            crate::database::block_on(
                db.exec_update_count(
                    &update("workflow_discovery_cursors")
                        .value("after_run_id", next)
                        .where_eq("artifact_id", artifact_id),
                ),
            )?;
        }
        database::commit(tx)?;
        Ok(page)
    }

    /// Return bounded artifact identities that still own resumable workflow runs.
    ///
    /// Deleting a daemon image whose artifact still owns `running`, `paused`, or
    /// `repair_required` runs can strand their recovery: execution authority is fenced to
    /// the artifact, and unresolved receipts may require its operation-owner contracts.
    /// Pending replacements also retain their old coordinator and recovery-source artifacts
    /// after the old run settles; terminal status alone does not release that reservation.
    /// Callers use this as retention evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when the bound or query is invalid.
    pub fn active_target_artifact_ids(
        &self,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        bounded_limit(limit)?;
        let tx = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*tx;
        let statuses = vec!["running", "paused", "repair_required"];
        let queries = vec![
            select("workflow_runs")
                .projections(database::projections(&["target_artifact_id"]))
                .where_in("status", statuses.clone())
                .where_not_eq("target_artifact_id", switchy_database::DatabaseValue::Null)
                .distinct()
                .sort("target_artifact_id", SortDirection::Asc)
                .limit(limit),
            select("workflow_recovery_barriers")
                .project_as(
                    col("workflow_recovery_barriers", "source_artifact_id"),
                    "target_artifact_id",
                )
                .joins(vec![join_columns(
                    "workflow_runs",
                    col("workflow_runs", "run_id"),
                    col("workflow_recovery_barriers", "run_id"),
                    false,
                )])
                .filter(Box::new(where_or(
                    statuses
                        .into_iter()
                        .map(|status| {
                            Box::new(where_eq_expression(col("workflow_runs", "status"), status))
                                as Box<dyn switchy_database::query::BooleanExpression>
                        })
                        .collect(),
                )))
                .distinct()
                .sort("target_artifact_id", SortDirection::Asc)
                .limit(limit),
            select("workflow_replacement_intents")
                .project_as(
                    col("workflow_runs", "target_artifact_id"),
                    "target_artifact_id",
                )
                .joins(vec![join_columns(
                    "workflow_runs",
                    col("workflow_runs", "run_id"),
                    col("workflow_replacement_intents", "old_run_id"),
                    false,
                )])
                .where_not_eq_expression(
                    col("workflow_runs", "target_artifact_id"),
                    switchy_database::DatabaseValue::Null,
                )
                .distinct()
                .sort("target_artifact_id", SortDirection::Asc)
                .limit(limit),
            select("workflow_replacement_intents")
                .project_as(
                    col("workflow_recovery_barriers", "source_artifact_id"),
                    "target_artifact_id",
                )
                .joins(vec![join_columns(
                    "workflow_recovery_barriers",
                    col("workflow_recovery_barriers", "run_id"),
                    col("workflow_replacement_intents", "old_run_id"),
                    false,
                )])
                .distinct()
                .sort("target_artifact_id", SortDirection::Asc)
                .limit(limit),
        ];
        let mut artifacts = BTreeSet::new();
        for query in queries {
            for row in database::rows(db, &query)? {
                artifacts.insert(database::text(&row, "target_artifact_id")?);
            }
        }
        database::commit(tx)?;
        Ok(artifacts.into_iter().take(limit).collect())
    }

    /// Return bounded receipt-backed attempts that need owner reattachment/observation.
    ///
    /// # Errors
    ///
    /// Returns an error when the bound is invalid, receipt data is malformed, or the query fails.
    pub fn active_attempts(
        &self,
        limit: usize,
    ) -> Result<Vec<AttemptReconciliationRequest>, WorkflowStoreError> {
        receipt_backed_attempts(&self.connection, None, bounded_limit(limit)?)
    }

    /// Return bounded receipt-backed attempts for one exact run.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity or bound is invalid, receipt data is malformed, or
    /// the query fails.
    pub fn active_attempts_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<AttemptReconciliationRequest>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        receipt_backed_attempts(&self.connection, Some(run_id), bounded_limit(limit)?)
    }

    /// Report whether the exact prepared attempt has a committed owner receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when the dispatch identity is malformed or the query fails.
    pub fn dispatch_receipt_committed(
        &self,
        dispatch_identity: &str,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;

        Ok(!database::rows(
            &self.connection,
            &select("workflow_attempts")
                .projections(database::projections(&["dispatch_identity"]))
                .where_eq("dispatch_identity", dispatch_identity)
                .where_not_eq("receipt_json", switchy_database::DatabaseValue::Null)
                .limit(1),
        )?
        .is_empty())
    }

    /// Observe a durable child run by its exact persisted link and project its canonical output to
    /// the parent attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed receipt/link state, missing child runs, invalid canonical
    /// output, schema mismatch, or database failure.
    pub fn observe_child_attempt(
        &self,
        request: &AttemptReconciliationRequest,
        observed_at_ms: u64,
    ) -> Result<AttemptObservation, WorkflowStoreError> {
        let child_run_id = request
            .receipt
            .get("child_run_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow child receipt has no child_run_id".to_string(),
                )
            })?;
        let link = child_link_for_parent_attempt_connection(&self.connection, request)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow child receipt has no canonical parent link".to_string(),
                )
            })?;
        if link.child_run_id != child_run_id {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child receipt conflicts with the canonical link".to_string(),
            ));
        }
        self.run_graph_node(&request.run_id, &request.node_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow child parent call is missing from run graph".to_string(),
                )
            })?;
        let node = self
            .activation_graph_node(&request.run_id, &request.node_id, &request.activation_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow child parent executable binding is missing".to_string(),
                )
            })?
            .node;
        if node.kind != bcode_workflow::NodeKind::WorkflowCall {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child parent node is not an admitted workflow call".to_string(),
            ));
        }
        let configuration: bcode_workflow::WorkflowCallConfiguration =
            serde_json::from_value(node.configuration)?;
        configuration
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        if configuration.target != link.target {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child link conflicts with admitted call target".to_string(),
            ));
        }
        let child = self.run_summary(child_run_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow child run is missing".to_string())
        })?;
        match child.status {
            RunStatus::Running | RunStatus::Paused => Ok(AttemptObservation::Running),
            RunStatus::Completed => {
                let output = self
                    .canonical_terminal_output(child_run_id)?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "completed workflow child has no terminal output".to_string(),
                        )
                    })?;
                let projected_value = if let Some(mapping) = &configuration.output {
                    mapping
                        .evaluate(&[bcode_workflow::WorkflowTransformInput {
                            name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                            value: &output.value,
                        }])
                        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
                } else {
                    output.value
                };
                Ok(AttemptObservation::Succeeded {
                    output: ValidatedOutput {
                        output_id: format!("{}:output", request.dispatch_identity),
                        run_id: request.run_id.clone(),
                        node_id: request.node_id.clone(),
                        activation_id: request.activation_id.clone(),
                        schema_id: configuration
                            .output
                            .as_ref()
                            .map_or(output.schema_id, |mapping| mapping.output.type_name.clone()),
                        schema_version: output.schema_version,
                        value: projected_value,
                        artifact_reference: output.artifact_reference,
                        created_at_ms: observed_at_ms,
                    },
                })
            }
            RunStatus::Failed => Ok(AttemptObservation::Failed {
                message: "child workflow failed".to_string(),
            }),
            RunStatus::Cancelled => Ok(AttemptObservation::Cancelled),
            RunStatus::RepairRequired => Ok(AttemptObservation::Unknown),
        }
    }

    /// Inspect one run for bounded structural inconsistencies without mutating durable state.
    ///
    /// This explicit maintenance API validates stable attempt identities, run/attempt repair
    /// status agreement, and activation/output relationships. It never replays events, dispatches
    /// external work, or changes projections.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity/limit is invalid, the run does not exist, persisted
    /// status is malformed, or the bounded queries fail.
    #[allow(clippy::too_many_lines)]
    pub fn doctor_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<WorkflowDoctorReport, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        bounded_limit(limit)?;

        let run_status = Self::run_summary_in(&self.connection, run_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!("workflow run not found: {run_id}"))
            })?
            .status;
        let rows = database::rows(
            &self.connection,
            &select("workflow_attempts")
                .project_as(count_all(), "count")
                .where_eq("run_id", run_id)
                .where_eq("status", "repair_required"),
        )?;
        let repair_required_attempts = database::unsigned(
            rows.first().ok_or(switchy_database::DatabaseError::NoRow)?,
            "count",
        )?;
        let mut issues = Vec::new();
        if (run_status == RunStatus::RepairRequired) != (repair_required_attempts > 0) {
            issues.push(WorkflowDoctorIssue::RepairStatusMismatch {
                run_status,
                repair_required_attempts,
            });
        }

        let report_limit = limit;
        let fetch_limit = report_limit.saturating_add(1);
        let mut scan_truncated = false;
        if issues.len() < fetch_limit {
            let remaining = fetch_limit - issues.len();
            let rows = database::rows(
                &self.connection,
                &select("workflow_grants")
                    .projections(database::projections(&[
                        "grant_id",
                        "node_id",
                        "scope_json",
                        "expires_at_ms",
                    ]))
                    .where_eq("run_id", run_id)
                    .sort("granted_at_ms", SortDirection::Asc)
                    .sort("grant_id", SortDirection::Asc)
                    .limit(remaining.saturating_add(1)),
            )?;
            let mut invalid_count = 0;
            for row in &rows {
                let grant_id = database::text(row, "grant_id")?;
                let node_id = database::text(row, "node_id")?;
                let scope_json = database::text(row, "scope_json")?;
                let expires_at_ms = database::optional_unsigned(row, "expires_at_ms")?;
                let reason = match serde_json::from_str::<bcode_workflow::WorkflowMutationGrantScope>(
                    &scope_json,
                ) {
                    Ok(scope)
                        if scope.validate().is_ok()
                            && scope.run_id == run_id
                            && scope.node_id == node_id
                            && expires_at_ms.is_none() =>
                    {
                        None
                    }
                    Ok(_) => Some("grant scope does not match its durable row".to_string()),
                    Err(error) => Some(format!("grant scope is malformed: {error}")),
                };
                if let Some(reason) = reason {
                    issues.push(WorkflowDoctorIssue::InvalidGrant { grant_id, reason });
                    invalid_count += 1;
                    if issues.len() >= fetch_limit || invalid_count >= remaining {
                        break;
                    }
                }
            }
        }
        if issues.len() < fetch_limit {
            let remaining = fetch_limit - issues.len();
            let rows = database::rows(
                &self.connection,
                &select("workflow_attempts")
                    .projections(database::projections(&[
                        "dispatch_identity",
                        "status",
                        "side_effect",
                    ]))
                    .where_eq("run_id", run_id)
                    .where_in("status", vec!["prepared", "admitted", "running"])
                    .where_eq("receipt_json", switchy_database::DatabaseValue::Null)
                    .sort("prepared_at_ms", SortDirection::Asc)
                    .sort("dispatch_identity", SortDirection::Asc)
                    .limit(remaining.saturating_add(1)),
            )?;
            for row in &rows {
                let dispatch_identity = database::text(row, "dispatch_identity")?;
                let status = database::text(row, "status")?;
                let side_effect = database::text(row, "side_effect")?;
                let side_effect = parse_side_effect(&side_effect)?;
                let guidance = match side_effect {
                    DispatchSideEffect::ReadOnly => {
                        "query the owner by dispatch identity; replay only when its contract proves idempotency"
                    }
                    DispatchSideEffect::Mutating => {
                        "do not replay; inspect persisted intent and owner evidence, then use explicit repair"
                    }
                };
                issues.push(WorkflowDoctorIssue::OrphanedAttempt {
                    dispatch_identity,
                    status,
                    side_effect,
                    guidance: guidance.to_string(),
                });
                if issues.len() >= fetch_limit {
                    break;
                }
            }
        }
        if issues.len() < fetch_limit {
            let remaining = fetch_limit - issues.len();

            let query = select("workflow_activations")
                .project_as(col("workflow_activations", "node_id"), "node_id")
                .project_as(
                    col("workflow_activations", "activation_id"),
                    "activation_id",
                )
                .project_as(col("workflow_activations", "status"), "status")
                .project_as(col("workflow_activations", "output_id"), "output_id")
                .joins(vec![join_columns(
                    "workflow_outputs",
                    col("workflow_outputs", "output_id"),
                    col("workflow_activations", "output_id"),
                    true,
                )])
                .where_eq_expression(col("workflow_activations", "run_id"), run_id)
                .filter(Box::new(where_or(vec![
                    Box::new(where_and(vec![
                        Box::new(where_eq_expression(
                            col("workflow_activations", "status"),
                            "completed",
                        )),
                        Box::new(where_or(vec![
                            Box::new(where_eq_expression(
                                col("workflow_outputs", "output_id"),
                                V::Null,
                            )),
                            Box::new(where_not_eq_expression(
                                col("workflow_outputs", "run_id"),
                                col("workflow_activations", "run_id"),
                            )),
                            Box::new(where_not_eq_expression(
                                col("workflow_outputs", "node_id"),
                                col("workflow_activations", "node_id"),
                            )),
                            Box::new(where_not_eq_expression(
                                col("workflow_outputs", "activation_id"),
                                col("workflow_activations", "activation_id"),
                            )),
                        ])),
                    ])),
                    Box::new(where_and(vec![
                        Box::new(where_not_eq_expression(
                            col("workflow_activations", "status"),
                            "completed",
                        )),
                        Box::new(where_not_eq_expression(
                            col("workflow_activations", "output_id"),
                            V::Null,
                        )),
                    ])),
                ])))
                .sorts(vec![
                    switchy_database::query::sort(
                        col("workflow_activations", "node_id"),
                        SortDirection::Asc,
                    ),
                    switchy_database::query::sort(
                        col("workflow_activations", "activation_id"),
                        SortDirection::Asc,
                    ),
                ])
                .limit(remaining);
            let mismatches = database::rows(&self.connection, &query)?
                .iter()
                .map(|row| {
                    Ok::<_, WorkflowStoreError>(WorkflowDoctorIssue::ActivationOutputMismatch {
                        node_id: database::text(row, "node_id")?,
                        activation_id: database::text(row, "activation_id")?,
                        activation_status: database::text(row, "status")?,
                        output_id: database::optional_text(row, "output_id")?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            issues.extend(mismatches);
        }

        if issues.len() < fetch_limit {
            let remaining = fetch_limit - issues.len();
            let scan_limit = report_limit.saturating_mul(16).max(report_limit);
            let rows = database::rows(
                &self.connection,
                &select("workflow_attempts")
                    .projections(database::projections(&[
                        "node_id",
                        "activation_id",
                        "attempt",
                        "dispatch_identity",
                    ]))
                    .where_eq("run_id", run_id)
                    .sort("prepared_at_ms", SortDirection::Asc)
                    .sort("dispatch_identity", SortDirection::Asc)
                    .limit(scan_limit.saturating_add(1)),
            )?;
            let mut mismatch_count = 0;
            for (inspected, row) in rows.iter().enumerate() {
                if inspected >= scan_limit {
                    scan_truncated = true;
                    break;
                }
                let node_id = database::text(row, "node_id")?;
                let activation_id = database::text(row, "activation_id")?;
                let attempt = u32::try_from(database::unsigned(row, "attempt")?)
                    .map_err(|_| WorkflowStoreError::InvalidData("attempt exceeds u32".into()))?;
                let identity = database::text(row, "dispatch_identity")?;
                let expected = dispatch_identity(run_id, &node_id, &activation_id, attempt);
                if identity != expected {
                    issues.push(WorkflowDoctorIssue::AttemptIdentityMismatch {
                        dispatch_identity: identity,
                        expected_dispatch_identity: expected,
                    });
                    mismatch_count += 1;
                    if issues.len() >= fetch_limit || mismatch_count >= remaining {
                        break;
                    }
                }
            }
        }

        let truncated = scan_truncated || issues.len() > report_limit;
        issues.truncate(report_limit);
        Ok(WorkflowDoctorReport {
            run_id: run_id.to_string(),
            issues,
            truncated,
        })
    }

    /// Explicitly resolve one repair-required ambiguous attempt.
    ///
    /// Repair never dispatches external work. Success requires a validated output; failure and
    /// cancellation are terminal; abandonment records an explicit operator decision before a
    /// later higher-numbered attempt may be prepared. The run leaves repair-required only after
    /// all ambiguous attempts have been resolved.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/non-repairable attempts, mismatched output,
    /// unbounded messages, conflicting output, or database failure.
    pub fn repair_attempt(
        &mut self,
        dispatch_identity: &str,
        resolution: &RepairResolution,
        repaired_at_ms: u64,
    ) -> Result<RepairResult, WorkflowStoreError> {
        self.repair_attempt_with_authority(dispatch_identity, resolution, repaired_at_ms, None)
    }

    /// Resolve one ambiguous attempt under the caller's current durable authority.
    ///
    /// Ownership is rechecked in the same transaction as the repair, so a transfer
    /// between application admission and persistence cannot authorize a stale repair.
    ///
    /// # Errors
    /// Rejects stale authority, invalid resolution, non-repairable attempts, or persistence failure.
    pub fn repair_attempt_owned(
        &mut self,
        dispatch_identity: &str,
        resolution: &RepairResolution,
        repaired_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<RepairResult, WorkflowStoreError> {
        self.repair_attempt_with_authority(
            dispatch_identity,
            resolution,
            repaired_at_ms,
            Some(authority),
        )
    }

    // Keep validation, disposition, and run state in one atomic repair transaction.
    #[allow(clippy::too_many_lines)]
    fn repair_attempt_with_authority(
        &self,
        dispatch_identity: &str,
        resolution: &RepairResolution,
        repaired_at_ms: u64,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<RepairResult, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        validate_repair_resolution(resolution)?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;

        let publication_query = |run_id: &str| {
            select("workflow_publication_cancellations")
                .projections(database::projections(&["dispatch_identity"]))
                .where_eq("run_id", run_id)
                .where_eq("dispatch_identity", dispatch_identity)
                .limit(1)
        };
        let attempt = repair_required_attempt(db, dispatch_identity)?;
        let (run_id, node_id, activation_id, attempt_number, side_effect) = attempt;
        if let Some(authority) = authority {
            Self::verify_execution_authority_in(db, &run_id, authority)?;
        }
        parse_side_effect(&side_effect)?;
        let expected = crate::dispatch_identity(&run_id, &node_id, &activation_id, attempt_number);
        if expected != dispatch_identity {
            return Err(WorkflowStoreError::InvalidData(
                "repair attempt identity does not match durable attempt identity".to_string(),
            ));
        }

        if matches!(resolution, RepairResolution::AbandonForExplicitRetry { .. }) {
            let retry_blocked = cancellation_requested_for_run(db, &run_id)?
                || !database::rows(db, &publication_query(&run_id))?.is_empty();
            if retry_blocked {
                return Err(WorkflowStoreError::InvalidData(
                    "cancelled work cannot be abandoned for retry; resolve its cancellation explicitly".into(),
                ));
            }
        }
        let (attempt_status, event_type) = match resolution {
            RepairResolution::ConfirmSucceeded { output } => {
                if output.run_id != run_id
                    || output.node_id != node_id
                    || output.activation_id != activation_id
                {
                    return Err(WorkflowStoreError::InvalidData(
                        "repair output identity does not match durable attempt".to_string(),
                    ));
                }
                persist_validated_output_transaction(db, output)?;
                ("succeeded", "attempt_repair_succeeded")
            }
            RepairResolution::ConfirmFailed { .. } => ("failed", "attempt_repair_failed"),
            RepairResolution::ConfirmCancelled { .. } => ("cancelled", "attempt_repair_cancelled"),
            RepairResolution::AbandonForExplicitRetry { .. } => {
                ("abandoned", "attempt_repair_abandoned")
            }
        };
        let changed = crate::database::block_on(
            db.exec_update_count(
                &update("workflow_attempts")
                    .value("status", attempt_status)
                    .value("terminal_at_ms", database::integer_value(repaired_at_ms)?)
                    .where_eq("dispatch_identity", dispatch_identity)
                    .where_eq("status", "repair_required"),
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "attempt cannot be repaired: {dispatch_identity}"
            )));
        }
        append_event(
            db,
            &run_id,
            event_type,
            &serde_json::json!({
                "dispatch_identity": dispatch_identity,
                "resolution": resolution,
            })
            .to_string(),
            repaired_at_ms,
        )?;
        if matches!(resolution, RepairResolution::AbandonForExplicitRetry { .. }) {
            crate::database::block_on(
                db.exec_update_count(
                    &update("workflow_activations")
                        .value("status", "pending")
                        .where_eq("run_id", run_id.as_str())
                        .where_eq("node_id", node_id.as_str())
                        .where_eq("activation_id", activation_id.as_str())
                        .where_eq("output_id", switchy_database::DatabaseValue::Null)
                        .where_in("status", vec!["running", "repair_required"]),
                ),
            )?;
        }
        let publication_cancelled = matches!(resolution, RepairResolution::ConfirmCancelled { .. })
            && !database::rows(db, &publication_query(&run_id))?.is_empty();
        if publication_cancelled {
            let changed = crate::database::block_on(
                db.exec_update_count(
                    &update("workflow_activations")
                        .value("status", "cancelled")
                        .where_eq("run_id", run_id.as_str())
                        .where_eq("node_id", node_id.as_str())
                        .where_eq("activation_id", activation_id.as_str())
                        .where_eq("output_id", switchy_database::DatabaseValue::Null)
                        .where_in("status", vec!["running", "repair_required"]),
                ),
            )?;
            if changed != 1 {
                return Err(WorkflowStoreError::InvalidData(
                    "publication repair activation is inconsistent".into(),
                ));
            }
        }
        let remaining = !database::rows(
            db,
            &select("workflow_attempts")
                .projections(database::projections(&["dispatch_identity"]))
                .where_eq("run_id", run_id.as_str())
                .where_eq("status", "repair_required")
                .limit(1),
        )?
        .is_empty();
        let recovery_only = !database::rows(
            db,
            &select("workflow_recovery_barriers")
                .projections(database::projections(&["run_id"]))
                .where_eq("run_id", run_id.as_str())
                .limit(1),
        )?
        .is_empty();
        let mut run_status = if remaining {
            RunStatus::RepairRequired
        } else if (publication_cancelled
            || matches!(resolution, RepairResolution::AbandonForExplicitRetry { .. }))
            && !recovery_only
        {
            RunStatus::Running
        } else {
            RunStatus::Paused
        };
        crate::database::block_on(
            db.exec_update_count(
                &update("workflow_runs")
                    .value("status", run_status.as_str())
                    .value("updated_at_ms", database::integer_value(repaired_at_ms)?)
                    .where_eq("run_id", run_id.as_str()),
            ),
        )?;
        if !remaining
            && cancellation_requested_for_run(db, &run_id)?
            && finalize_run_cancellation_if_settled(db, &run_id, repaired_at_ms)?
        {
            run_status = RunStatus::Cancelled;
        }
        database::commit(transaction)?;
        Ok(RepairResult {
            dispatch_identity: dispatch_identity.to_string(),
            attempt_status: attempt_status.to_string(),
            run_status,
        })
    }

    /// Detach a repair-required run from its generic session binding.
    ///
    /// Execution state, parent provenance, and unresolved attempts remain unchanged. The prior
    /// binding is retained in an audit event; repeated detachment is a no-op. This association-only
    /// operation deliberately does not verify, acquire, or modify execution authority.
    ///
    /// # Errors
    /// Rejects runs not requiring repair, pending replacements, or storage errors.
    pub fn detach_run(&mut self, run_id: &str, now_ms: u64) -> Result<bool, WorkflowStoreError> {
        let tx = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*tx;

        let run =
            Self::run_summary_in(db, run_id)?.ok_or_else(|| WorkflowStoreError::RunNotFound {
                run_id: run_id.to_owned(),
            })?;
        if run.status != RunStatus::RepairRequired {
            return Err(WorkflowStoreError::InvalidData(
                "only repair-required workflows can be detached".into(),
            ));
        }
        if Self::pending_replacement_in(db, run_id)?.is_some() {
            return Err(WorkflowStoreError::InvalidData(
                "withdraw the pending replacement before detaching".into(),
            ));
        }
        let Some(binding) = run.binding else {
            return Ok(false);
        };
        crate::database::block_on(
            db.exec_update_count(
                &update("workflow_runs")
                    .value("owner_plugin_id", switchy_database::DatabaseValue::Null)
                    .value("workflow_kind", switchy_database::DatabaseValue::Null)
                    .value("scope_key", switchy_database::DatabaseValue::Null)
                    .value("display_label", switchy_database::DatabaseValue::Null)
                    .value("single_active", 0_i64)
                    .value("updated_at_ms", database::integer_value(now_ms)?)
                    .where_eq("run_id", run_id),
            ),
        )?;
        append_event(
            db,
            run_id,
            "run_detached",
            &serde_json::to_string(&binding)?,
            now_ms,
        )?;
        database::commit(tx)?;
        Ok(true)
    }

    /// Return the newest run associated with one exact generic binding key.
    ///
    /// This uses a bounded indexed lookup and never replays workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is malformed, persisted binding data is inconsistent, or the
    /// row query fails.
    pub fn associated_run(
        &self,
        key: &WorkflowRunBindingKey,
    ) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::associated_run_in(&*transaction, key)?;
        database::commit(transaction)?;
        Ok(result)
    }

    /// Fail one running workflow whose durable wall-clock deadline has elapsed.
    ///
    /// Active external attempts are not rewritten: the caller must cancel them through the normal
    /// end-to-end owner cancellation path. This transition applies only when no active attempt can
    /// still report an authoritative outcome.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/corrupt run state, or database failure.
    pub fn expire_run_deadline(
        &mut self,
        run_id: &str,
        now_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("run_id", run_id)?;

        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let rows = database::rows(
            db,
            &select("workflow_runs")
                .projections(database::projections(&["status", "deadline_at_ms"]))
                .where_eq("run_id", run_id)
                .limit(1),
        )?;
        let row = rows
            .first()
            .ok_or_else(|| WorkflowStoreError::RunNotFound {
                run_id: run_id.to_string(),
            })?;
        let status = database::text(row, "status")?;
        let deadline = database::optional_unsigned(row, "deadline_at_ms")?;
        if parse_run_status(&status)? != RunStatus::Running
            || deadline.is_none_or(|deadline| now_ms < deadline)
        {
            return Ok(false);
        }
        let active_attempt = !database::rows(
            db,
            &select("workflow_attempts")
                .projections(database::projections(&["dispatch_identity"]))
                .where_eq("run_id", run_id)
                .where_in(
                    "status",
                    vec![
                        "prepared",
                        "admitted",
                        "running",
                        "cancelling",
                        "sibling_cancelling",
                    ],
                )
                .limit(1),
        )?
        .is_empty();
        if active_attempt {
            return Ok(false);
        }
        let changed = crate::database::block_on(
            db.exec_update_count(
                &update("workflow_runs")
                    .value("status", "failed")
                    .value("updated_at_ms", database::integer_value(now_ms)?)
                    .where_eq("run_id", run_id)
                    .where_eq("status", "running"),
            ),
        )?;
        if changed == 1 {
            crate::database::block_on(
                db.exec_update_count(
                    &update("workflow_activations")
                        .value("status", "failed")
                        .where_eq("run_id", run_id)
                        .where_in(
                            "status",
                            vec![
                                "pending",
                                "running",
                                "waiting_input",
                                "waiting_approval",
                                "waiting_mutation_approval",
                            ],
                        )
                        .where_eq("output_id", switchy_database::DatabaseValue::Null),
                ),
            )?;
            append_event(
                db,
                run_id,
                "run_deadline_elapsed",
                &serde_json::json!({"deadline_at_ms": deadline, "observed_at_ms": now_ms})
                    .to_string(),
                now_ms,
            )?;
        }
        database::commit(transaction)?;
        Ok(changed == 1)
    }

    /// Resolve one exact waiting input gate with schema-validated typed data.
    ///
    /// Resolution, output persistence, activation completion, and downstream materialization are
    /// atomic and idempotency-safe through the exact activation identity.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, wrong gate kind/state, invalid input schema,
    /// cancellation, conflicting resolution, or database failure.
    pub fn provide_input(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        value: serde_json::Value,
        resolved_at_ms: u64,
    ) -> Result<WaitingResolutionResult, WorkflowStoreError> {
        self.resolve_waiting_activation(
            run_id,
            node_id,
            activation_id,
            WorkflowWaitKind::Input,
            Some(value),
            true,
            resolved_at_ms,
            None,
        )
    }

    /// Resolve input only while holding the exact durable execution authority.
    ///
    /// # Errors
    /// Rejects stale/foreign authority, invalid input or gate identity, and storage failures.
    pub fn provide_input_owned(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        value: serde_json::Value,
        resolved_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<WaitingResolutionResult, WorkflowStoreError> {
        self.resolve_waiting_activation(
            run_id,
            node_id,
            activation_id,
            WorkflowWaitKind::Input,
            Some(value),
            true,
            resolved_at_ms,
            Some(authority),
        )
    }

    /// Resolve one exact waiting approval gate.
    ///
    /// Approval forwards the waiting activation's typed input. Denial terminates the run as failed
    /// and never enables downstream work.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, wrong gate kind/state, cancellation, conflicting
    /// resolution, invalid forwarded input, or database failure.
    pub fn resolve_approval(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        approved: bool,
        resolved_at_ms: u64,
    ) -> Result<WaitingResolutionResult, WorkflowStoreError> {
        self.resolve_waiting_activation(
            run_id,
            node_id,
            activation_id,
            WorkflowWaitKind::Approval,
            None,
            approved,
            resolved_at_ms,
            None,
        )
    }

    /// Resolve approval only while holding the exact durable execution authority.
    ///
    /// # Errors
    /// Rejects stale/foreign authority, invalid gate state, conflicting decisions and storage failures.
    pub fn resolve_approval_owned(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        approved: bool,
        resolved_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<WaitingResolutionResult, WorkflowStoreError> {
        self.resolve_waiting_activation(
            run_id,
            node_id,
            activation_id,
            WorkflowWaitKind::Approval,
            None,
            approved,
            resolved_at_ms,
            Some(authority),
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn resolve_waiting_activation(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        kind: WorkflowWaitKind,
        supplied_value: Option<serde_json::Value>,
        accepted: bool,
        resolved_at_ms: u64,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<WaitingResolutionResult, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;

        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        if let Some(authority) = authority {
            Self::verify_execution_authority_in(db, run_id, authority)?;
        }
        let rows = database::rows(
            db,
            &select("workflow_activations")
                .projections(database::projections(&["status", "input_json"]))
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id)
                .where_eq("activation_id", activation_id)
                .limit(1),
        )?;
        let row = rows.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "waiting activation not found: {run_id}/{node_id}/{activation_id}"
            ))
        })?;
        let status = database::text(row, "status")?;
        let input_json = database::optional_text(row, "input_json")?;
        let run = Self::run_summary_in(db, run_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "waiting activation not found: {run_id}/{node_id}/{activation_id}"
            ))
        })?;
        let cancellation_requested = run.cancellation_requested_at_ms.is_some();
        let run_status = run.status.as_str();
        if cancellation_requested || parse_run_status(run_status)? != RunStatus::Running {
            return Err(WorkflowStoreError::InvalidData(
                "workflow run is not accepting waiting resolutions".to_string(),
            ));
        }
        let expected_status = match kind {
            WorkflowWaitKind::Input => "waiting_input",
            WorkflowWaitKind::Approval => "waiting_approval",
        };
        if status != expected_status {
            return Err(WorkflowStoreError::InvalidData(format!(
                "activation is not waiting for {}: {status}",
                kind.as_str()
            )));
        }
        // Resolve the admitted executable, not the initial graph: an explicitly retained
        // gate and a gate added by publication may both settle at a later graph revision.
        run_graph::reconciled_activation_exit(db, run_id, node_id, activation_id)?;
        let node = run_graph::bound_activation_node(db, run_id, node_id, activation_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow waiting run-graph node not found: {node_id}"
                ))
            })?;
        let expected_kind = match kind {
            WorkflowWaitKind::Input => bcode_workflow::NodeKind::Input,
            WorkflowWaitKind::Approval => bcode_workflow::NodeKind::Approval,
        };
        if node.kind != expected_kind {
            return Err(WorkflowStoreError::InvalidData(
                "workflow wait status conflicts with admitted node kind".to_string(),
            ));
        }
        let value = match kind {
            WorkflowWaitKind::Input => {
                let supplied = supplied_value.ok_or_else(|| {
                    WorkflowStoreError::InvalidData("input gate requires a value".into())
                })?;
                if let Some(resolution) = node.configuration.get("input_resolution") {
                    let resolution: bcode_workflow::WorkflowInputResolution =
                        serde_json::from_value(resolution.clone())?;
                    node.validate_structure()
                        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
                    validate_json_schema(
                        "supplied gate input",
                        &resolution.supplied.schema,
                        &supplied,
                    )?;
                    let retained = input_json
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .unwrap_or(serde_json::Value::Null);
                    resolution
                        .transform
                        .evaluate(&[
                            bcode_workflow::WorkflowTransformInput {
                                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                                value: &retained,
                            },
                            bcode_workflow::WorkflowTransformInput {
                                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                                value: &supplied,
                            },
                        ])
                        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
                } else {
                    supplied
                }
            }
            WorkflowWaitKind::Approval => input_json
                .map(|value| serde_json::from_str(&value))
                .transpose()?
                .unwrap_or(serde_json::Value::Null),
        };
        if accepted {
            validate_json_schema("waiting activation output", &node.output.schema, &value)?;
            crate::database::block_on(
                db.exec_update_count(
                    &update("workflow_activations")
                        .value("status", "running")
                        .where_eq("run_id", run_id)
                        .where_eq("node_id", node_id)
                        .where_eq("activation_id", activation_id)
                        .where_eq("status", expected_status),
                ),
            )?;
            let output = ValidatedOutput {
                output_id: format!("{activation_id}:wait-output"),
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                activation_id: activation_id.to_string(),
                schema_id: node.output.type_name,
                schema_version: 1,
                value,
                artifact_reference: None,
                created_at_ms: resolved_at_ms,
            };
            let result = persist_validated_output_transaction(db, &output)?;
            append_event(
                db,
                run_id,
                "waiting_activation_resolved",
                &serde_json::json!({"node_id": node_id, "activation_id": activation_id, "kind": kind, "accepted": true}).to_string(),
                resolved_at_ms,
            )?;
            database::commit(transaction)?;
            return Ok(WaitingResolutionResult {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                activation_id: activation_id.to_string(),
                outcome: "accepted".to_string(),
                activated: result.activated,
                run_status: result.run_status,
            });
        }
        crate::database::block_on(
            db.exec_update_count(
                &update("workflow_activations")
                    .value("status", "failed")
                    .where_eq("run_id", run_id)
                    .where_eq("node_id", node_id)
                    .where_eq("activation_id", activation_id)
                    .where_eq("status", expected_status),
            ),
        )?;
        crate::database::block_on(
            db.exec_update_count(
                &update("workflow_runs")
                    .value("status", "failed")
                    .value("updated_at_ms", database::integer_value(resolved_at_ms)?)
                    .where_eq("run_id", run_id)
                    .where_eq("status", "running"),
            ),
        )?;
        append_event(
            db,
            run_id,
            "waiting_activation_resolved",
            &serde_json::json!({"node_id": node_id, "activation_id": activation_id, "kind": kind, "accepted": false}).to_string(),
            resolved_at_ms,
        )?;
        append_event(db, run_id, "run_failed", "{}", resolved_at_ms)?;
        database::commit(transaction)?;
        Ok(WaitingResolutionResult {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            activation_id: activation_id.to_string(),
            outcome: "denied".to_string(),
            activated: Vec::new(),
            run_status: RunStatus::Failed,
        })
    }
}

fn prepared_read_only_dispatches(
    connection: &dyn switchy_database::Database,
    limit: i64,
) -> Result<Vec<PreparedActivationDispatch>, WorkflowStoreError> {
    prepared_read_only_dispatches_scoped(connection, None, limit)
}

fn prepared_read_only_dispatches_for_run(
    connection: &dyn switchy_database::Database,
    run_id: &str,
    limit: i64,
) -> Result<Vec<PreparedActivationDispatch>, WorkflowStoreError> {
    prepared_read_only_dispatches_scoped(connection, Some(run_id), limit)
}

fn prepared_read_only_dispatches_scoped(
    connection: &dyn switchy_database::Database,
    run_id: Option<&str>,
    limit: i64,
) -> Result<Vec<PreparedActivationDispatch>, WorkflowStoreError> {
    prepared_dispatches_scoped(connection, run_id, limit, true, None)
}

fn qualified_text_membership(
    table: &str,
    column: &str,
    values: impl IntoIterator<Item = String>,
) -> Box<dyn switchy_database::query::BooleanExpression> {
    Box::new(where_or(
        values
            .into_iter()
            .map(|value| {
                Box::new(where_eq_expression(col(table, column), V::String(value)))
                    as Box<dyn switchy_database::query::BooleanExpression>
            })
            .collect(),
    ))
}

fn constrain_prepared_read_only(
    mut query: switchy_database::query::SelectQuery<'_>,
    read_only: bool,
) -> switchy_database::query::SelectQuery<'_> {
    if read_only {
        query = query
            .where_eq_expression(col("workflow_attempts", "side_effect"), "read_only")
            .where_eq_expression(col("workflow_activations", "status"), "running")
            .where_eq_expression(col("workflow_runs", "status"), "running")
            .where_eq_expression(
                col("workflow_runs", "cancellation_requested_at_ms"),
                V::Null,
            );
    }
    query
}

fn prepared_dispatch_order() -> Vec<switchy_database::query::Sort> {
    vec![
        switchy_database::query::sort(
            col("workflow_attempts", "prepared_at_ms"),
            SortDirection::Asc,
        ),
        switchy_database::query::sort(
            col("workflow_attempts", "dispatch_identity"),
            SortDirection::Asc,
        ),
    ]
}

fn prepared_dispatch_query<'a>(
    run_id: Option<&'a str>,
    limit: i64,
    read_only: bool,
    identities: Option<&'a [String]>,
) -> Result<switchy_database::query::SelectQuery<'a>, WorkflowStoreError> {
    let mut query = select("workflow_attempts")
        .projections(
            [
                "run_id",
                "node_id",
                "activation_id",
                "attempt",
                "dispatch_identity",
            ]
            .into_iter()
            .map(|name| Projection::new(col("workflow_attempts", name)).alias(name))
            .collect(),
        )
        .project_as(
            col("workflow_activations", "dependency_generation"),
            "dependency_generation",
        )
        .project_as(
            col("workflow_activations", "created_at_ms"),
            "created_at_ms",
        )
        .project_as(
            bounded_text(
                col("workflow_activations", "input_json"),
                MAX_INLINE_JSON_BYTES as u64,
                V::Null,
            ),
            "input_json",
        )
        .project_as(
            bounded_text(
                col("workflow_attempts", "intent_json"),
                MAX_INLINE_JSON_BYTES as u64,
                V::Null,
            ),
            "intent_json",
        )
        .project_as(
            Box::new(where_eq_expression(
                col("workflow_activations", "input_json"),
                V::Null,
            )) as Box<dyn switchy_database::query::Expression>,
            "input_absent",
        )
        .joins(vec![
            join_columns(
                "workflow_activations",
                col("workflow_activations", "activation_id"),
                col("workflow_attempts", "activation_id"),
                false,
            ),
            join_columns(
                "workflow_runs",
                col("workflow_runs", "run_id"),
                col("workflow_attempts", "run_id"),
                false,
            ),
        ])
        .filter(Box::new(where_eq_expression(
            col("workflow_activations", "run_id"),
            col("workflow_attempts", "run_id"),
        )))
        .filter(Box::new(where_eq_expression(
            col("workflow_activations", "node_id"),
            col("workflow_attempts", "node_id"),
        )))
        .filter(qualified_text_membership(
            "workflow_attempts",
            "status",
            (if read_only {
                vec!["prepared"]
            } else {
                vec!["prepared", "cancelling", "sibling_cancelling"]
            })
            .into_iter()
            .map(str::to_owned),
        ))
        .where_eq_expression(col("workflow_attempts", "receipt_json"), V::Null)
        .sorts(prepared_dispatch_order())
        .limit(usize::try_from(limit).map_err(|_| {
            WorkflowStoreError::InvalidData("invalid prepared dispatch limit".into())
        })?);
    query = constrain_prepared_read_only(query, read_only);
    if let Some(run_id) = run_id {
        query = query.where_eq_expression(col("workflow_attempts", "run_id"), run_id);
    }
    if let Some(identities) = identities {
        query = query.filter(qualified_text_membership(
            "workflow_attempts",
            "dispatch_identity",
            identities.iter().cloned(),
        ));
    }
    Ok(query)
}

fn prepared_dispatches_scoped(
    connection: &dyn switchy_database::Database,
    run_id: Option<&str>,
    limit: i64,
    read_only: bool,
    identities: Option<&[String]>,
) -> Result<Vec<PreparedActivationDispatch>, WorkflowStoreError> {
    if identities.is_some_and(<[String]>::is_empty) {
        return Ok(Vec::new());
    }
    let query = prepared_dispatch_query(run_id, limit, read_only, identities)?;
    database::rows(connection, &query)?
        .iter()
        .map(|row| {
            let run_id = database::text(row, "run_id")?;
            let node_id = database::text(row, "node_id")?;
            let activation_id = database::text(row, "activation_id")?;
            let attempt = u32::try_from(database::unsigned(row, "attempt")?).map_err(|_| {
                WorkflowStoreError::InvalidData("prepared attempt exceeds u32".into())
            })?;
            let dispatch_identity = database::text(row, "dispatch_identity")?;
            let dependency_generation = database::unsigned(row, "dependency_generation")?;
            let input_json = database::optional_text(row, "input_json")?;
            let created_at_ms = database::unsigned(row, "created_at_ms")?;
            let intent_json = database::optional_text(row, "intent_json")?;
            let input_absent = database::unsigned(row, "input_absent")? != 0;
            let intent_json = intent_json.ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "prepared workflow attempt intent is oversized or not text".to_string(),
                )
            })?;
            if input_json.is_none() && !input_absent {
                return Err(WorkflowStoreError::InvalidData(
                    "prepared workflow activation input is oversized or not text".to_string(),
                ));
            }
            run_graph::reconciled_activation_exit(connection, &run_id, &node_id, &activation_id)?;
            let node =
                run_graph::bound_activation_node(connection, &run_id, &node_id, &activation_id)?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(format!(
                            "prepared workflow attempt references missing run-graph node: {node_id}"
                        ))
                    })?;
            Ok(PreparedActivationDispatch {
                activation: PendingActivation {
                    run_id,
                    node_id,
                    activation_id,
                    dependency_generation,
                    input: input_json
                        .map(|json| serde_json::from_str(&json))
                        .transpose()?,
                    node,
                    created_at_ms,
                },
                attempt,
                dispatch_identity,
                intent: serde_json::from_str(&intent_json)?,
            })
        })
        .collect()
}

type AttemptReconciliationRow = (String, String, String, u32, String, String, String);

fn attempt_reconciliation_request(
    row: AttemptReconciliationRow,
) -> Result<AttemptReconciliationRequest, WorkflowStoreError> {
    let (run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, receipt_json) =
        row;
    Ok(AttemptReconciliationRequest {
        run_id,
        node_id,
        activation_id,
        attempt,
        dispatch_identity,
        side_effect: parse_side_effect(&side_effect)?,
        receipt: serde_json::from_str(&receipt_json)?,
    })
}

fn attempt_reconciliation_row(
    row: &switchy_database::Row,
) -> Result<AttemptReconciliationRow, WorkflowStoreError> {
    let attempt = u32::try_from(database::unsigned(row, "attempt")?)
        .map_err(|_| WorkflowStoreError::InvalidData("receipt attempt exceeds u32".into()))?;
    Ok((
        database::text(row, "run_id")?,
        database::text(row, "node_id")?,
        database::text(row, "activation_id")?,
        attempt,
        database::text(row, "dispatch_identity")?,
        database::text(row, "side_effect")?,
        database::text(row, "receipt_json")?,
    ))
}
fn receipt_attempt_query() -> switchy_database::query::SelectQuery<'static> {
    select("workflow_attempts")
        .projections(
            [
                "run_id",
                "node_id",
                "activation_id",
                "attempt",
                "dispatch_identity",
                "side_effect",
            ]
            .into_iter()
            .map(|column| {
                switchy_database::query::Projection::new(switchy_database::query::identifier(
                    column,
                ))
                .alias(column)
            })
            .collect(),
        )
        .project_as(
            bounded_text(
                identifier("receipt_json"),
                MAX_INLINE_JSON_BYTES as u64,
                switchy_database::DatabaseValue::Null,
            ),
            "receipt_json",
        )
        .where_in(
            "status",
            vec!["admitted", "running", "cancelling", "sibling_cancelling"],
        )
        .where_not_eq("receipt_json", switchy_database::DatabaseValue::Null)
}
fn receipt_backed_attempt(
    db: &dyn switchy_database::Database,
    dispatch_identity: &str,
) -> Result<Option<AttemptReconciliationRequest>, WorkflowStoreError> {
    database::rows(
        db,
        &receipt_attempt_query()
            .where_eq("dispatch_identity", dispatch_identity)
            .limit(1),
    )?
    .first()
    .map(|row| attempt_reconciliation_request(attempt_reconciliation_row(row)?))
    .transpose()
}
fn receipt_backed_attempts_after(
    db: &dyn switchy_database::Database,
    run_id: &str,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<AttemptReconciliationRequest>, WorkflowStoreError> {
    let mut query =
        receipt_attempt_query()
            .where_eq("run_id", run_id)
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(usize::try_from(limit).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid receipt page limit".into())
            })?);
    if let Some(after) = after {
        query = query.where_gt("dispatch_identity", after);
    }
    database::rows(db, &query)?
        .iter()
        .map(|row| attempt_reconciliation_request(attempt_reconciliation_row(row)?))
        .collect()
}
fn receipt_backed_attempts(
    db: &dyn switchy_database::Database,
    run_id: Option<&str>,
    limit: i64,
) -> Result<Vec<AttemptReconciliationRequest>, WorkflowStoreError> {
    let mut query =
        receipt_attempt_query()
            .sort("prepared_at_ms", SortDirection::Asc)
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(usize::try_from(limit).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid receipt page limit".into())
            })?);
    if let Some(run_id) = run_id.filter(|id| !id.is_empty()) {
        query = query.where_eq("run_id", run_id);
    }
    database::rows(db, &query)?
        .iter()
        .map(|row| attempt_reconciliation_request(attempt_reconciliation_row(row)?))
        .collect()
}

type ActiveAttemptCancellationRow = (String, String, String, u32, String, Option<String>);

fn active_attempt_cancellation_row(
    row: &switchy_database::Row,
) -> Result<ActiveAttemptCancellationRow, WorkflowStoreError> {
    let attempt = u32::try_from(database::unsigned(row, "attempt")?).map_err(|_| {
        WorkflowStoreError::InvalidData("workflow cancellation attempt exceeds u32".into())
    })?;
    Ok((
        database::text(row, "run_id")?,
        database::text(row, "node_id")?,
        database::text(row, "activation_id")?,
        attempt,
        database::text(row, "dispatch_identity")?,
        database::optional_text(row, "receipt_json")?,
    ))
}

fn active_attempt_cancellation(
    row: (String, String, String, u32, String, Option<String>),
) -> Result<ActiveAttemptCancellation, WorkflowStoreError> {
    let (run_id, node_id, activation_id, attempt, dispatch_identity, receipt_json) = row;
    Ok(ActiveAttemptCancellation {
        run_id,
        node_id,
        activation_id,
        attempt,
        dispatch_identity,
        receipt: receipt_json
            .map(|receipt| serde_json::from_str(&receipt))
            .transpose()?,
    })
}

fn cancellation_requested_for_run(
    db: &dyn switchy_database::Database,
    run_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let rows = database::rows(
        db,
        &select("workflow_runs")
            .projections(database::projections(&["cancellation_requested_at_ms"]))
            .where_eq("run_id", run_id)
            .limit(1),
    )?;
    let row = rows.first().ok_or(switchy_database::DatabaseError::NoRow)?;
    Ok(database::optional_unsigned(row, "cancellation_requested_at_ms")?.is_some())
}

fn activation_status_at_generation(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    generation: u64,
) -> Result<Option<String>, WorkflowStoreError> {
    Ok(activation_at_generation(db, run_id, node_id, generation)?.map(|(_, status)| status))
}

fn activation_at_generation(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    generation: u64,
) -> Result<Option<(String, String)>, WorkflowStoreError> {
    let rows = database::rows(
        db,
        &select("workflow_activations")
            .project_as(
                bounded_text(identifier("activation_id"), MAX_ID_BYTES as u64, V::Null),
                "activation_id",
            )
            .project_as(bounded_text(identifier("status"), 32, V::Null), "status")
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq(
                "dependency_generation",
                database::integer_value(generation)?,
            )
            .limit(2),
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let activation_id = database::text(row, "activation_id")?;
    validate_id("activation_id", &activation_id)?;
    if rows.len() > 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow node has ambiguous activations: {node_id}"
        )));
    }
    let status = database::text(row, "status").map_err(|_| {
        WorkflowStoreError::InvalidData(format!(
            "workflow node has unsupported activation status: {node_id}"
        ))
    })?;
    if !matches!(
        status.as_str(),
        "pending"
            | "running"
            | "waiting_input"
            | "waiting_approval"
            | "waiting_mutation_approval"
            | "completed"
            | "skipped"
            | "failed"
            | "cancelled"
    ) {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow node has unsupported activation status: {node_id}"
        )));
    }
    Ok(Some((activation_id, status)))
}

fn validate_observed_output(
    request: &AttemptReconciliationRequest,
    output: &ValidatedOutput,
) -> Result<(), WorkflowStoreError> {
    if output.run_id != request.run_id
        || output.node_id != request.node_id
        || output.activation_id != request.activation_id
    {
        return Err(WorkflowStoreError::InvalidData(
            "observed output identity does not match reconciled attempt".to_string(),
        ));
    }
    Ok(())
}

fn mark_pending_activation_running(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<(), WorkflowStoreError> {
    let members = database::rows(
        db,
        &select("workflow_fan_out_members")
            .projections(database::projections(&["status"]))
            .where_eq("run_id", run_id)
            .where_eq("member_node_id", node_id)
            .where_eq("member_activation_id", activation_id)
            .limit(1),
    )?;
    if let Some(member) = members.first()
        && database::text(member, "status")? != "pending"
    {
        return Err(WorkflowStoreError::InvalidData(
            "fan-out member state disagrees with pending activation".into(),
        ));
    }
    let changed = crate::database::block_on(
        db.exec_update_count(
            &(update("workflow_activations")
                .value("status", "running")
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id)
                .where_eq("activation_id", activation_id)
                .where_eq("status", "pending")),
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "pending workflow activation is missing or inconsistent".into(),
        ));
    }
    if !members.is_empty() {
        let changed = crate::database::block_on(
            db.exec_update_count(
                &(update("workflow_fan_out_members")
                    .value("status", "running")
                    .where_eq("run_id", run_id)
                    .where_eq("member_node_id", node_id)
                    .where_eq("member_activation_id", activation_id)
                    .where_eq("status", "pending")),
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "pending fan-out member is missing or inconsistent".into(),
            ));
        }
    }
    Ok(())
}

fn runnable_run_in(
    db: &dyn switchy_database::Database,
    run_id: &str,
) -> Result<bool, WorkflowStoreError> {
    Ok(!database::rows(
        db,
        &select("workflow_runs")
            .projections(database::projections(&["run_id"]))
            .where_eq("run_id", run_id)
            .where_eq("status", "running")
            .where_eq(
                "cancellation_requested_at_ms",
                switchy_database::DatabaseValue::Null,
            )
            .limit(1),
    )?
    .is_empty())
}

fn pending_activation_by_identity(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<PendingActivation>, WorkflowStoreError> {
    if !runnable_run_in(db, run_id)? {
        return Ok(None);
    }
    if !database::rows(
        db,
        &select("workflow_fan_out_members")
            .projections(database::projections(&["run_id"]))
            .where_eq("run_id", run_id)
            .where_eq("member_activation_id", activation_id)
            .limit(1),
    )?
    .is_empty()
    {
        return Ok(None);
    }
    let rows = database::rows(
        db,
        &select("workflow_activations")
            .projections(database::projections(&[
                "dependency_generation",
                "input_json",
                "created_at_ms",
            ]))
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .where_eq("status", "pending")
            .limit(1),
    )?;
    rows.first()
        .map(|row| {
            let node = if run_graph::graph_revision(db, run_id)? == Some(1) {
                run_graph::initial_activation_node(db, run_id, node_id, activation_id)?
            } else {
                run_graph::reconciled_activation_exit(db, run_id, node_id, activation_id)?;
                run_graph::bound_activation_node(db, run_id, node_id, activation_id)?
            }
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow activation references missing run-graph node: {node_id}"
                ))
            })?;
            Ok(PendingActivation {
                run_id: run_id.into(),
                node_id: node_id.into(),
                activation_id: activation_id.into(),
                dependency_generation: database::unsigned(row, "dependency_generation")?,
                input: database::optional_text(row, "input_json")?
                    .map(|json| serde_json::from_str(&json))
                    .transpose()?,
                node,
                created_at_ms: database::unsigned(row, "created_at_ms")?,
            })
        })
        .transpose()
}

fn pending_fan_out_member_by_identity(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<PendingActivation>, WorkflowStoreError> {
    if !runnable_run_in(db, run_id)? {
        return Ok(None);
    }
    database::rows(
        db,
        &select("workflow_fan_out_members")
            .projections(
                [
                    "controller_node_id",
                    "member_index",
                    "input_json",
                    "created_at_ms",
                    "controller_activation_id",
                ]
                .into_iter()
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
            )
            .where_eq("run_id", run_id)
            .where_eq("member_node_id", node_id)
            .where_eq("member_activation_id", activation_id)
            .where_eq("status", "pending")
            .limit(1),
    )?
    .first()
    .map(|row| {
        let controller_node_id = database::text(row, "controller_node_id")?;
        let controller_activation_id = database::text(row, "controller_activation_id")?;
        let controller = run_graph::initial_activation_node(
            db,
            run_id,
            &controller_node_id,
            &controller_activation_id,
        )?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("fan-out run-graph controller node is missing".into())
        })?;
        let configuration: bcode_workflow::WorkflowFanOutConfiguration =
            serde_json::from_value(controller.configuration)?;
        let mut node = *configuration.member_node;
        node.id = node_id.into();
        let index = u32::try_from(database::unsigned(row, "member_index")?).map_err(|_| {
            WorkflowStoreError::InvalidData("fan-out member index exceeds u32".into())
        })?;
        Ok(PendingActivation {
            run_id: run_id.into(),
            node_id: node_id.into(),
            activation_id: activation_id.into(),
            dependency_generation: u64::from(index),
            input: Some(serde_json::from_str(&database::text(row, "input_json")?)?),
            node,
            created_at_ms: database::unsigned(row, "created_at_ms")?,
        })
    })
    .transpose()
}

fn enforce_activation_limits(
    connection: &dyn switchy_database::Database,
    activation: &NewActivation,
) -> Result<(), WorkflowStoreError> {
    enforce_activation_materialization_limits(connection, activation, false)
}

fn enforce_activation_materialization_limits(
    connection: &dyn switchy_database::Database,
    activation: &NewActivation,
    settling_admitted_work: bool,
) -> Result<(), WorkflowStoreError> {
    let rows = database::rows(
        connection,
        &select("workflow_runs")
            .projections(
                [
                    "cycle_cap",
                    "status",
                    "cancellation_requested_at_ms",
                    "deadline_at_ms",
                ]
                .into_iter()
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
            )
            .where_eq("run_id", activation.run_id.as_str())
            .limit(1),
    )?;
    let row = rows
        .first()
        .ok_or_else(|| WorkflowStoreError::RunNotFound {
            run_id: activation.run_id.clone(),
        })?;
    let cycle_cap = u32::try_from(database::unsigned(row, "cycle_cap")?)
        .map_err(|_| WorkflowStoreError::InvalidData("cycle cap exceeds u32".into()))?;
    let status = parse_run_status(&database::text(row, "status")?)?;
    let cancellation_requested =
        database::optional_unsigned(row, "cancellation_requested_at_ms")?.is_some();
    let deadline_at_ms = database::optional_unsigned(row, "deadline_at_ms")?;
    let status = status.as_str();
    if status != RunStatus::Running.as_str()
        && !(settling_admitted_work && status == RunStatus::Paused.as_str())
    {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow run does not accept activations while {status}"
        )));
    }
    if cancellation_requested {
        return Err(WorkflowStoreError::InvalidData(
            "workflow cancellation has been requested".to_string(),
        ));
    }
    if deadline_at_ms.is_some_and(|deadline| activation.created_at_ms >= deadline) {
        return Err(WorkflowStoreError::InvalidData(
            "workflow wall-clock deadline has elapsed".to_string(),
        ));
    }
    if activation.dependency_generation >= u64::from(cycle_cap) {
        return Err(WorkflowStoreError::InvalidData(
            "workflow cycle cap exceeded".to_string(),
        ));
    }
    Ok(())
}

fn enforce_attempt_limits(
    connection: &dyn switchy_database::Database,
    attempt: &PreparedAttempt,
) -> Result<(), WorkflowStoreError> {
    let rows = database::rows(
        connection,
        &select("workflow_runs")
            .projections(
                [
                    "deadline_at_ms",
                    "node_execution_cap",
                    "concurrency_cap",
                    "retry_cap",
                    "cancellation_requested_at_ms",
                    "status",
                ]
                .into_iter()
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
            )
            .where_eq("run_id", attempt.run_id.as_str())
            .limit(1),
    )?;
    let row = rows
        .first()
        .ok_or_else(|| WorkflowStoreError::RunNotFound {
            run_id: attempt.run_id.clone(),
        })?;
    let deadline_at_ms = database::optional_unsigned(row, "deadline_at_ms")?;
    let node_execution_cap = database::unsigned(row, "node_execution_cap")?;
    let concurrency_cap = database::optional_unsigned(row, "concurrency_cap")?
        .map(u32::try_from)
        .transpose()
        .map_err(|_| WorkflowStoreError::InvalidData("concurrency cap exceeds u32".into()))?;
    let retry_cap = u32::try_from(database::unsigned(row, "retry_cap")?)
        .map_err(|_| WorkflowStoreError::InvalidData("retry cap exceeds u32".into()))?;
    let cancellation_requested =
        database::optional_unsigned(row, "cancellation_requested_at_ms")?.is_some();
    let run_status = parse_run_status(&database::text(row, "status")?)?;
    validate_attempt_admission_state(
        run_status,
        cancellation_requested,
        deadline_at_ms,
        retry_cap,
        attempt,
    )?;
    let execution_count =
        typed_attempt_count(connection, &attempt.run_id, node_execution_cap, false)?;
    if execution_count >= node_execution_cap {
        return Err(WorkflowStoreError::NodeExecutionAllowanceExhausted);
    }
    enforce_root_attempt_limit(connection, &attempt.run_id)?;
    if let Some(cap) = concurrency_cap
        && typed_attempt_count(connection, &attempt.run_id, u64::from(cap), true)? >= u64::from(cap)
    {
        return Err(WorkflowStoreError::ConcurrencyCapacityReached);
    }
    Ok(())
}

fn typed_root_attempt_count(
    db: &dyn switchy_database::Database,
    root_run_id: &str,
    cap: u64,
) -> Result<u64, WorkflowStoreError> {
    let children = select("workflow_run_links")
        .columns(&["child_run_id"])
        .where_eq("root_run_id", root_run_id);
    let members = select("workflow_attempts")
        .columns(&["dispatch_identity"])
        .where_or(vec![
            Box::new(where_eq("run_id", root_run_id)),
            Box::new(where_in("run_id", children)),
        ])
        .limit(usize::try_from(cap).unwrap_or(usize::MAX));
    let rows = database::rows(
        db,
        &select("workflow_attempts")
            .project_as(count_all(), "attempt_count")
            .where_in("dispatch_identity", members),
    )?;
    database::unsigned(
        rows.first()
            .ok_or_else(|| WorkflowStoreError::InvalidData("root attempt count missing".into()))?,
        "attempt_count",
    )
}

fn typed_attempt_count(
    connection: &dyn switchy_database::Database,
    run_id: &str,
    limit: u64,
    active_only: bool,
) -> Result<u64, WorkflowStoreError> {
    let mut members = select("workflow_attempts")
        .projections(
            std::iter::once("dispatch_identity")
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
        )
        .where_eq("run_id", run_id)
        .limit(usize::try_from(limit).unwrap_or(usize::MAX));
    if active_only {
        members = members.where_in(
            "status",
            vec![
                "prepared",
                "admitted",
                "running",
                "cancelling",
                "sibling_cancelling",
            ],
        );
    }
    let query = select("workflow_attempts")
        .project_as(count_all(), "attempt_count")
        .where_in("dispatch_identity", members);
    let rows = database::rows(connection, &query)?;
    match rows.first().and_then(|row| row.get("attempt_count")) {
        Some(switchy_database::DatabaseValue::Int64(value)) if value >= 0 => {
            Ok(value.cast_unsigned())
        }
        _ => Err(WorkflowStoreError::InvalidData(
            "typed attempt count is invalid".to_string(),
        )),
    }
}

// Keep admission policy independent of the query adapter. Callers must obtain these
// facts inside the same immediate transaction that reserves the attempt.
fn validate_attempt_admission_state(
    status: RunStatus,
    cancellation_requested: bool,
    deadline_at_ms: Option<u64>,
    retry_cap: u32,
    attempt: &PreparedAttempt,
) -> Result<(), WorkflowStoreError> {
    if status != RunStatus::Running {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow run does not accept attempts while {}",
            status.as_str()
        )));
    }
    if cancellation_requested {
        return Err(WorkflowStoreError::InvalidData(
            "workflow cancellation has been requested".to_string(),
        ));
    }
    if deadline_at_ms.is_some_and(|deadline| attempt.prepared_at_ms >= deadline) {
        return Err(WorkflowStoreError::InvalidData(
            "workflow wall-clock deadline has elapsed".to_string(),
        ));
    }
    if attempt.attempt > retry_cap.saturating_add(1) {
        return Err(WorkflowStoreError::InvalidData(
            "workflow retry cap exceeded".to_string(),
        ));
    }
    Ok(())
}

fn enforce_root_attempt_limit(
    db: &dyn switchy_database::Database,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    let links = database::rows(
        db,
        &select("workflow_run_links")
            .projections(database::projections(&["root_run_id"]))
            .where_eq("child_run_id", run_id)
            .limit(1),
    )?;
    let root = links
        .first()
        .map(|row| database::text(row, "root_run_id"))
        .transpose()?
        .unwrap_or_else(|| run_id.into());
    let runs = database::rows(
        db,
        &select("workflow_runs")
            .projections(database::projections(&["node_execution_cap"]))
            .where_eq("run_id", root.as_str())
            .limit(1),
    )?;
    let cap = database::unsigned(
        runs.first()
            .ok_or_else(|| WorkflowStoreError::RunNotFound {
                run_id: root.clone(),
            })?,
        "node_execution_cap",
    )?;
    let children = select("workflow_run_links")
        .projections(
            std::iter::once("child_run_id")
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
        )
        .where_eq("root_run_id", root.as_str());
    let members = select("workflow_attempts")
        .projections(
            std::iter::once("dispatch_identity")
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
        )
        .filter(Box::new(Or {
            conditions: vec![
                Box::new(where_eq("run_id", root.clone())),
                Box::new(where_in("run_id", children)),
            ],
        }))
        .limit(usize::try_from(cap).unwrap_or(usize::MAX));
    let rows = database::rows(
        db,
        &select("workflow_attempts")
            .project_as(count_all(), "used")
            .where_in("dispatch_identity", members),
    )?;
    if database::unsigned(
        rows.first()
            .ok_or_else(|| WorkflowStoreError::InvalidData("root attempt count missing".into()))?,
        "used",
    )? >= cap
    {
        return Err(WorkflowStoreError::RootExecutionAllowanceExhausted);
    }
    Ok(())
}

const fn run_status_rank(status: RunStatus) -> u8 {
    match status {
        RunStatus::Running => 0,
        RunStatus::Paused => 1,
        RunStatus::RepairRequired => 2,
        RunStatus::Failed => 3,
        RunStatus::Completed => 4,
        RunStatus::Cancelled => 5,
    }
}

#[allow(clippy::suspicious_operation_groupings)]
fn run_follows_catalog_cursor(run: &WorkflowRunSummary, cursor: &WorkflowRunCatalogCursor) -> bool {
    match cursor.sort {
        WorkflowRunCatalogSort::UpdatedAt => {
            run.updated_at_ms < cursor.timestamp_ms
                || (run.updated_at_ms == cursor.timestamp_ms && run.run_id > cursor.run_id)
        }
        WorkflowRunCatalogSort::CreatedAt => {
            run.created_at_ms < cursor.timestamp_ms
                || (run.created_at_ms == cursor.timestamp_ms && run.run_id > cursor.run_id)
        }
        WorkflowRunCatalogSort::Status => {
            let rank = run_status_rank(run.status);
            rank > cursor.status_rank
                || (rank == cursor.status_rank
                    && (run.updated_at_ms < cursor.timestamp_ms
                        || (run.updated_at_ms == cursor.timestamp_ms
                            && run.run_id > cursor.run_id)))
        }
    }
}

fn authoring_page_query_limit(limit: usize) -> Result<i64, WorkflowStoreError> {
    let limit = bounded_limit(limit)?;
    Ok(limit.saturating_add(1))
}

fn bounded_limit(limit: usize) -> Result<i64, WorkflowStoreError> {
    if limit == 0 || limit > 1_000 {
        return Err(WorkflowStoreError::InvalidData(
            "history limit must be in 1..=1000".to_string(),
        ));
    }
    Ok(i64::try_from(limit).unwrap_or(1_000))
}

type RawRunSummary = (
    String,
    String,
    u32,
    String,
    Option<String>,
    Option<u64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    Option<u64>,
    u64,
    u64,
);

// Empty text is rejected by the field's domain parser; SQL NULL stays absent.
fn run_summary_from_row(row: &switchy_database::Row) -> Result<RawRunSummary, WorkflowStoreError> {
    fn bounded(
        row: &switchy_database::Row,
        column: &str,
        limit: usize,
    ) -> Result<Option<String>, WorkflowStoreError> {
        Ok(database::optional_text(row, column)?.map(|text| {
            if text.len() > limit {
                String::new()
            } else {
                text
            }
        }))
    }
    let version = u32::try_from(database::unsigned(row, "definition_version")?).map_err(|_| {
        WorkflowStoreError::InvalidData("workflow definition version exceeds u32".into())
    })?;
    let Some(switchy_database::DatabaseValue::Int64(single_active)) = row.get("single_active")
    else {
        return Err(WorkflowStoreError::InvalidData(
            "invalid workflow single-active flag".into(),
        ));
    };
    Ok((
        bounded(row, "run_id", MAX_ID_BYTES)?.unwrap_or_default(),
        bounded(row, "definition_id", MAX_ID_BYTES)?.unwrap_or_default(),
        version,
        bounded(row, "workspace_snapshot", MAX_ID_BYTES)?.unwrap_or_default(),
        bounded(row, "parent_session_id", MAX_ID_BYTES)?,
        database::optional_unsigned(row, "parent_session_generation")?,
        bounded(row, "owner_plugin_id", MAX_ID_BYTES)?,
        bounded(row, "workflow_kind", MAX_ID_BYTES)?,
        bounded(row, "scope_key", MAX_ID_BYTES)?,
        bounded(row, "display_label", MAX_DISPLAY_LABEL_BYTES)?,
        single_active,
        bounded(row, "authored_provenance_json", MAX_INLINE_JSON_BYTES)?,
        database::optional_text(row, "terminal_output_id")?,
        database::optional_text(row, "terminal_output_checksum_sha256")?,
        bounded(row, "authorization_profile_json", MAX_INLINE_JSON_BYTES)?.unwrap_or_default(),
        bounded(row, "authorization_ceiling", MAX_ID_BYTES)?.unwrap_or_default(),
        bounded(row, "status", MAX_ID_BYTES)?.unwrap_or_default(),
        database::optional_unsigned(row, "cancellation_requested_at_ms")?,
        database::unsigned(row, "created_at_ms")?,
        database::unsigned(row, "updated_at_ms")?,
    ))
}

fn parse_run_binding(
    owner_plugin_id: Option<String>,
    workflow_kind: Option<String>,
    scope_key: Option<String>,
    display_label: Option<String>,
    single_active: i64,
) -> Result<Option<WorkflowRunBinding>, WorkflowStoreError> {
    let single_active = match single_active {
        0 => false,
        1 => true,
        _ => {
            return Err(WorkflowStoreError::InvalidData(
                "stored workflow run single-active flag is invalid".to_string(),
            ));
        }
    };
    match (owner_plugin_id, workflow_kind, scope_key) {
        (Some(owner_plugin_id), Some(workflow_kind), Some(scope_key)) => {
            let binding = WorkflowRunBinding {
                owner_plugin_id,
                workflow_kind,
                scope_key,
                display_label,
                single_active,
            };
            validate_binding(&binding)?;
            Ok(Some(binding))
        }
        (None, None, None) if display_label.is_none() && !single_active => Ok(None),
        _ => Err(WorkflowStoreError::InvalidData(
            "stored workflow run has an incomplete binding".to_string(),
        )),
    }
}

fn parse_run_summary(raw: RawRunSummary) -> Result<WorkflowRunSummary, WorkflowStoreError> {
    let (
        run_id,
        definition_id,
        definition_version,
        workspace_snapshot,
        parent_session_id,
        parent_session_generation,
        owner_plugin_id,
        workflow_kind,
        scope_key,
        display_label,
        single_active,
        authored_provenance_json,
        terminal_output_id,
        terminal_output_checksum_sha256,
        authorization_profile_json,
        authorization_ceiling,
        status,
        cancellation_requested_at_ms,
        created_at_ms,
        updated_at_ms,
    ) = raw;
    validate_id("run_id", &run_id)?;
    validate_id("definition_id", &definition_id)?;
    validate_id("workspace_snapshot", &workspace_snapshot)?;
    if definition_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "stored workflow run definition version must be positive".to_string(),
        ));
    }
    if let Some(parent_session_id) = &parent_session_id {
        validate_id("parent_session_id", parent_session_id)?;
    }
    if parent_session_generation.is_some() && parent_session_id.is_none() {
        return Err(WorkflowStoreError::InvalidData(
            "stored workflow run parent session generation requires parent_session_id".to_string(),
        ));
    }
    let authored_provenance: Option<AuthoredWorkflowRunProvenance> = authored_provenance_json
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|_| {
            WorkflowStoreError::InvalidData(
                "stored workflow run authored provenance JSON is malformed".to_string(),
            )
        })?;
    let binding = parse_run_binding(
        owner_plugin_id,
        workflow_kind,
        scope_key,
        display_label,
        single_active,
    )?;
    if let Some(provenance) = &authored_provenance
        && (provenance.version != AUTHORED_WORKFLOW_RUN_PROVENANCE_VERSION
            || provenance.revision == 0
            || provenance.workflow_id.trim().is_empty()
            || provenance.definition_identity.kind != provenance.workflow_id
            || provenance.definition_identity.definition_id != definition_id
            || provenance.definition_identity.definition_version != definition_version
            || matches!(
                (&provenance.preset_id, provenance.preset_generation),
                (Some(_), None) | (None, Some(_))
            ))
    {
        return Err(WorkflowStoreError::InvalidData(
            "stored authored workflow run provenance is inconsistent".to_string(),
        ));
    }
    let authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity =
        serde_json::from_str(&authorization_profile_json).map_err(|_| {
            WorkflowStoreError::InvalidData(
                "stored workflow run authorization profile JSON is malformed".to_string(),
            )
        })?;
    authorization_profile.validate().map_err(|_| {
        WorkflowStoreError::InvalidData(
            "stored workflow run authorization profile is invalid".to_string(),
        )
    })?;
    Ok(WorkflowRunSummary {
        run_id,
        definition_id,
        definition_version,
        workspace_snapshot,
        parent_session_id,
        parent_session_generation,
        binding,
        authored_provenance,
        terminal_output_id,
        terminal_output_checksum_sha256,
        authorization_profile,
        authorization_ceiling: parse_workflow_capability(&authorization_ceiling)?,
        status: parse_run_status(&status)?,
        cancellation_requested_at_ms,
        created_at_ms,
        updated_at_ms,
    })
}

const fn workflow_capability_name(
    capability: bcode_workflow::WorkflowToolCapability,
) -> &'static str {
    match capability {
        bcode_workflow::WorkflowToolCapability::Disabled => "disabled",
        bcode_workflow::WorkflowToolCapability::ReadOnly => "read_only",
        bcode_workflow::WorkflowToolCapability::Mutating => "mutating",
    }
}

fn parse_workflow_capability(
    capability: &str,
) -> Result<bcode_workflow::WorkflowToolCapability, WorkflowStoreError> {
    match capability {
        "disabled" => Ok(bcode_workflow::WorkflowToolCapability::Disabled),
        "read_only" => Ok(bcode_workflow::WorkflowToolCapability::ReadOnly),
        "mutating" => Ok(bcode_workflow::WorkflowToolCapability::Mutating),
        _ => Err(WorkflowStoreError::InvalidData(
            "unknown workflow authorization ceiling".to_string(),
        )),
    }
}

fn parse_run_status(status: &str) -> Result<RunStatus, WorkflowStoreError> {
    match status {
        "running" => Ok(RunStatus::Running),
        "paused" => Ok(RunStatus::Paused),
        "completed" => Ok(RunStatus::Completed),
        "failed" => Ok(RunStatus::Failed),
        "cancelled" => Ok(RunStatus::Cancelled),
        "repair_required" => Ok(RunStatus::RepairRequired),
        _ => Err(WorkflowStoreError::InvalidData(
            "unknown workflow run status".to_string(),
        )),
    }
}

type RawAttemptSummary = (
    String,
    String,
    String,
    u32,
    String,
    String,
    String,
    bool,
    u64,
    Option<u64>,
    Option<u64>,
);

fn attempt_summary_from_row(
    row: &switchy_database::Row,
) -> Result<RawAttemptSummary, WorkflowStoreError> {
    fn bounded(row: &switchy_database::Row, column: &str) -> Result<String, WorkflowStoreError> {
        Ok(database::optional_text(row, column)?
            .filter(|text| text.len() <= MAX_ID_BYTES)
            .unwrap_or_default())
    }
    let run_id = bounded(row, "run_id")?;
    let node_id = bounded(row, "node_id")?;
    let activation_id = bounded(row, "activation_id")?;
    let attempt = u32::try_from(database::unsigned(row, "attempt")?)
        .map_err(|_| WorkflowStoreError::InvalidData("workflow attempt exceeds u32".into()))?;
    let dispatch_identity = bounded(row, "dispatch_identity")?;
    let side_effect = bounded(row, "side_effect")?;
    let status = bounded(row, "status")?;
    let has_receipt = database::unsigned(row, "has_receipt")?;
    if has_receipt > 1 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow attempt receipt marker is invalid".into(),
        ));
    }
    Ok((
        run_id,
        node_id,
        activation_id,
        attempt,
        dispatch_identity,
        side_effect,
        status,
        has_receipt != 0,
        database::unsigned(row, "prepared_at_ms")?,
        database::optional_unsigned(row, "admitted_at_ms")?,
        database::optional_unsigned(row, "terminal_at_ms")?,
    ))
}

fn parse_side_effect(side_effect: &str) -> Result<DispatchSideEffect, WorkflowStoreError> {
    match side_effect {
        "read_only" => Ok(DispatchSideEffect::ReadOnly),
        "mutating" => Ok(DispatchSideEffect::Mutating),
        _ => Err(WorkflowStoreError::InvalidData(
            "unknown workflow side effect".to_string(),
        )),
    }
}

fn parse_attempt_summary(raw: RawAttemptSummary) -> Result<AttemptSummary, WorkflowStoreError> {
    let (
        run_id,
        node_id,
        activation_id,
        attempt,
        dispatch_identity,
        side_effect,
        status,
        has_receipt,
        prepared_at_ms,
        admitted_at_ms,
        terminal_at_ms,
    ) = raw;
    validate_id("run_id", &run_id)?;
    validate_id("node_id", &node_id)?;
    validate_id("activation_id", &activation_id)?;
    validate_id("dispatch_identity", &dispatch_identity)?;
    if dispatch_identity != self::dispatch_identity(&run_id, &node_id, &activation_id, attempt) {
        return Err(WorkflowStoreError::InvalidData(
            "stored workflow attempt identity conflicts with its coordinates".to_string(),
        ));
    }
    if attempt == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "stored workflow attempt number must be positive".to_string(),
        ));
    }
    let side_effect = parse_side_effect(&side_effect)?;
    if !matches!(
        status.as_str(),
        "prepared"
            | "admitted"
            | "running"
            | "cancelling"
            | "sibling_cancelling"
            | "succeeded"
            | "failed"
            | "cancelled"
            | "repair_required"
            | "paused"
            | "abandoned"
    ) {
        return Err(WorkflowStoreError::InvalidData(
            "unknown workflow attempt status".to_string(),
        ));
    }
    Ok(AttemptSummary {
        run_id,
        node_id,
        activation_id,
        attempt,
        dispatch_identity,
        side_effect,
        status,
        has_receipt,
        prepared_at_ms,
        admitted_at_ms,
        terminal_at_ms,
    })
}

fn insert_activation(
    transaction: &dyn switchy_database::Database,
    activation: &NewActivation,
) -> Result<(), WorkflowStoreError> {
    insert_activation_with_status(transaction, activation, "pending")
}

const fn activation_status_for_node(node: &bcode_workflow::NodeDefinition) -> &'static str {
    match node.kind {
        bcode_workflow::NodeKind::Input => "waiting_input",
        bcode_workflow::NodeKind::Approval => "waiting_approval",
        _ => "pending",
    }
}

fn insert_activation_with_status_in(
    db: &dyn switchy_database::Database,
    activation: &NewActivation,
    status: &str,
) -> Result<(), WorkflowStoreError> {
    if run_graph::graph_revision(db, &activation.run_id)? != Some(1) {
        return Err(WorkflowStoreError::InvalidData(
            "revised workflow graph requires revision-aware activation admission".into(),
        ));
    }
    activation.input.as_ref().ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "workflow activation input is required: {}/{}/{}",
            activation.run_id, activation.node_id, activation.activation_id
        ))
    })?;
    let node =
        run_graph::initial_node(db, &activation.run_id, &activation.node_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow activation references missing run-graph node: {}",
                activation.node_id
            ))
        })?;
    validate_json_schema(
        &format!("workflow activation input for node {}", activation.node_id),
        &node.input.schema,
        activation.input.as_ref().unwrap(),
    )?;
    crate::database::block_on(
        db.insert("workflow_activations")
            .value("run_id", activation.run_id.as_str())
            .value("node_id", activation.node_id.as_str())
            .value("activation_id", activation.activation_id.as_str())
            .value(
                "dependency_generation",
                database::integer_value(activation.dependency_generation)?,
            )
            .value(
                "input_json",
                serde_json::to_string(&activation.input.as_ref().unwrap())?,
            )
            .value("status", status)
            .value(
                "created_at_ms",
                database::integer_value(activation.created_at_ms)?,
            )
            .value("node_revision", 1_i64)
            .execute(db),
    )?;
    append_event_in(
        db,
        &activation.run_id,
        if status == "pending" {
            "activation_created"
        } else {
            "activation_waiting"
        },
        &serde_json::json!({"activation": activation, "status": status}).to_string(),
        activation.created_at_ms,
    )?;
    crate::database::block_on(
        db.insert("workflow_activation_graph_bindings")
            .value("run_id", activation.run_id.as_str())
            .value("node_id", activation.node_id.as_str())
            .value("activation_id", activation.activation_id.as_str())
            .value("graph_revision", 1_i64)
            .execute(db),
    )?;
    Ok(())
}

fn insert_activation_with_status(
    transaction: &dyn switchy_database::Database,
    activation: &NewActivation,
    status: &str,
) -> Result<(), WorkflowStoreError> {
    if run_graph::graph_revision(transaction, &activation.run_id)? != Some(1) {
        return Err(WorkflowStoreError::InvalidData(
            "revised workflow graph requires revision-aware activation admission".to_string(),
        ));
    }
    activation.input.as_ref().ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "workflow activation input is required: {}/{}/{}",
            activation.run_id, activation.node_id, activation.activation_id
        ))
    })?;
    let node = run_graph::initial_node(transaction, &activation.run_id, &activation.node_id)?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow activation references missing run-graph node: {}",
                activation.node_id
            ))
        })?;
    insert_activation_bound_to_node(transaction, activation, status, &node, 1)?;
    record_activation_graph_binding(
        transaction,
        &activation.run_id,
        &activation.node_id,
        &activation.activation_id,
        1,
    )
}

fn record_activation_graph_binding(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    graph_revision: u64,
) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(
        db.insert("workflow_activation_graph_bindings")
            .value("run_id", run_id)
            .value("node_id", node_id)
            .value("activation_id", activation_id)
            .value("graph_revision", database::integer_value(graph_revision)?)
            .execute(db),
    )?;
    Ok(())
}

fn insert_activation_bound_to_node(
    transaction: &dyn switchy_database::Database,
    activation: &NewActivation,
    status: &str,
    node: &bcode_workflow::NodeDefinition,
    node_revision: u64,
) -> Result<(), WorkflowStoreError> {
    let input = activation.input.as_ref().ok_or_else(|| {
        WorkflowStoreError::InvalidData("workflow activation input is required".to_string())
    })?;
    validate_json_schema(
        &format!("workflow activation input for node {}", activation.node_id),
        &node.input.schema,
        input,
    )?;
    crate::database::block_on(
        transaction
            .insert("workflow_activations")
            .value("run_id", activation.run_id.as_str())
            .value("node_id", activation.node_id.as_str())
            .value("activation_id", activation.activation_id.as_str())
            .value(
                "dependency_generation",
                database::integer_value(activation.dependency_generation)?,
            )
            .value(
                "input_json",
                activation
                    .input
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
            )
            .value("status", status)
            .value(
                "created_at_ms",
                database::integer_value(activation.created_at_ms)?,
            )
            .value("node_revision", database::integer_value(node_revision)?)
            .execute(transaction),
    )?;
    let event_type = if status == "pending" {
        "activation_created"
    } else {
        "activation_waiting"
    };
    append_event_in(
        transaction,
        &activation.run_id,
        event_type,
        &serde_json::json!({"activation": activation, "status": status}).to_string(),
        activation.created_at_ms,
    )
}

/// Return the stable identity for one node activation generation.
#[must_use]
pub fn activation_identity(run_id: &str, node_id: &str, generation: u64) -> String {
    sha256_hex(format!("{run_id}\0{node_id}\0{generation}").as_bytes())
}

/// Return the stable identity for one fan-out virtual member node.
#[must_use]
pub fn fan_out_member_node_id(controller_node_id: &str, member_index: u32) -> String {
    format!("{controller_node_id}::member::{member_index}")
}

/// Return the stable identity for one fan-out member activation.
#[must_use]
pub fn fan_out_member_activation_identity(
    run_id: &str,
    controller_activation_id: &str,
    member_index: u32,
) -> String {
    sha256_hex(format!("{run_id}\0{controller_activation_id}\0fan-out\0{member_index}").as_bytes())
}

/// Return the stable idempotency identity for one durable attempt.
#[must_use]
pub fn dispatch_identity(run_id: &str, node_id: &str, activation_id: &str, attempt: u32) -> String {
    sha256_hex(format!("{run_id}\0{node_id}\0{activation_id}\0{attempt}").as_bytes())
}

/// Return the canonical workflow database path under one Bcode state directory.
#[must_use]
pub fn workflow_database_path(state_dir: &Path) -> PathBuf {
    state_dir.join("workflows").join(DATABASE_FILE)
}

fn open_ownership_file(root: &Path) -> Result<File, WorkflowStoreError> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(LOCK_FILE))?)
}

fn validate_quiescent_reassignment_in(
    db: &dyn switchy_database::Database,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", run_id)?;
    let mut query = select("workflow_attempts")
        .projections(
            std::iter::once("run_id")
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
        )
        .where_eq("run_id", run_id)
        .limit(1);
    for status in ["succeeded", "failed", "cancelled", "paused", "abandoned"] {
        query = query.where_not_eq("status", status);
    }
    if !database::rows(db, &query)?.is_empty() {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow run has unsettled attempts and requires operation-owner recovery before cross-artifact reassignment: {run_id}"
        )));
    }
    Ok(())
}

fn detected_typed_store_schema(database: &dyn switchy_database::Database) -> Option<u32> {
    let rows = database::rows(
        database,
        &select("workflow_store_contract")
            .projections(database::projections(&["schema_version"]))
            .where_eq("contract_id", 1_i64)
            .limit(1),
    )
    .ok()?;
    let version = database::unsigned(rows.first()?, "schema_version").ok()?;
    u32::try_from(version).ok()
}

#[cfg(test)]
fn detected_store_schema(db: &dyn switchy_database::Database) -> Option<u32> {
    let rows = database::rows(
        db,
        &select("workflow_store_contract")
            .projections(database::projections(&["schema_version"]))
            .where_eq("contract_id", 1_i64)
            .limit(1),
    )
    .ok()?;
    u32::try_from(database::unsigned(rows.first()?, "schema_version").ok()?).ok()
}

fn verify_migration_source(db: &dyn switchy_database::Database) -> Result<u32, WorkflowStoreError> {
    let previous_schema_version = detected_typed_store_schema(db).ok_or_else(|| {
        WorkflowStoreError::InvalidData(
            "workflow store migration cannot read the source schema".to_string(),
        )
    })?;
    if !matches!(previous_schema_version, 14..=42) {
        return Err(WorkflowStoreError::UnsupportedStore {
            actual: Some(previous_schema_version),
            expected: WORKFLOW_STORE_SCHEMA_VERSION,
        });
    }
    verify_upgrade_recovery_source(db, previous_schema_version)?;
    if previous_schema_version >= 41
        && read_typed_contract_value(db, "workflow_storage_compatibility", "compatibility")?
            != Some(previous_schema_version - 40)
    {
        return Err(WorkflowStoreError::UnsupportedStore {
            actual: Some(previous_schema_version),
            expected: WORKFLOW_STORE_SCHEMA_VERSION,
        });
    }
    Ok(previous_schema_version)
}

fn create_migration_backup(
    path: &Path,
    root: &Path,
    migrated_at_ms: u64,
) -> Result<(PathBuf, String), WorkflowStoreError> {
    let backup_directory = root.join(MIGRATION_BACKUP_DIRECTORY);
    std::fs::create_dir_all(&backup_directory)?;
    let mut backup_path = backup_directory.join(format!("workflow-{migrated_at_ms}.db"));
    // Retained partial backups are never overwritten; retry at the same timestamp is safe.
    for suffix in 1..=1024 {
        if !backup_path.exists() {
            break;
        }
        backup_path = backup_directory.join(format!("workflow-{migrated_at_ms}-{suffix}.db"));
    }
    if backup_path.exists() {
        return Err(WorkflowStoreError::InvalidData(
            "workflow migration backup names exhausted; retry with a new timestamp".to_string(),
        ));
    }
    // Retain the write reservation while a separate reader backs up the pre-migration state.
    let backup_source = database::open(
        path,
        switchy_database_connection::SqliteAccess::ReadOnly,
        std::time::Duration::from_secs(5),
        false,
    )?;
    crate::database::block_on(switchy_database::Database::sqlite_backup_to(
        &backup_source,
        &backup_path,
    ))?;
    drop(backup_source);
    let backup_sha256 = verify_reset_backup(&backup_path)?;
    Ok((backup_path, backup_sha256))
}

fn open_verified_backup(
    path: &Path,
) -> Result<switchy_database::rusqlite::RusqliteDatabase, WorkflowStoreError> {
    let backup = database::open(
        path,
        SqliteAccess::ReadOnly,
        std::time::Duration::from_secs(5),
        false,
    )?;
    let transaction = database::begin(&backup, switchy_database::TransactionMode::Deferred)?;
    let rows = crate::database::block_on(transaction.sqlite_integrity_check())?;
    if rows.len() != 1 || database::text(&rows[0], "integrity_check")? != "ok" {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow reset backup failed integrity verification: {rows:?}"
        )));
    }
    database::commit(transaction)?;
    Ok(backup)
}

fn verify_reset_backup(path: &Path) -> Result<String, WorkflowStoreError> {
    drop(open_verified_backup(path)?);
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut encoded, byte| {
            write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
            encoded
        }))
}

fn backup_workflow_artifacts(source: &Path, backup: &Path) -> Result<u64, WorkflowStoreError> {
    if !source.exists() {
        return Ok(0);
    }
    if !source.is_dir() {
        return Err(WorkflowStoreError::InvalidData(
            "workflow artifact root is not a directory".to_string(),
        ));
    }
    copy_artifact_tree_verified(source, backup, 0)
}

fn copy_artifact_tree_verified(
    source: &Path,
    destination: &Path,
    depth: usize,
) -> Result<u64, WorkflowStoreError> {
    if depth > 64 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow artifact tree exceeds 64 directories of depth".to_string(),
        ));
    }
    std::fs::create_dir(destination)?;
    let mut copied = 0_u64;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_symlink() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow artifact backup refuses symbolic links".to_string(),
            ));
        }
        if kind.is_dir() {
            copied = copied
                .checked_add(copy_artifact_tree_verified(
                    &entry.path(),
                    &target,
                    depth + 1,
                )?)
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "workflow artifact entry count exceeds u64".to_string(),
                    )
                })?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
            if sha256_file(&entry.path())? != sha256_file(&target)? {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow artifact backup checksum verification failed".to_string(),
                ));
            }
            copied = copied.checked_add(1).ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow artifact entry count exceeds u64".to_string(),
                )
            })?;
        } else {
            return Err(WorkflowStoreError::InvalidData(
                "workflow artifact backup refuses non-file entries".to_string(),
            ));
        }
    }
    Ok(copied)
}

fn restore_reset_backup(
    database: &Path,
    artifacts: &Path,
    backup_database: &Path,
    backup_artifacts: &Path,
) -> Result<(), WorkflowStoreError> {
    if database.exists() {
        std::fs::remove_file(database)?;
    }
    std::fs::copy(backup_database, database)?;
    if artifacts.exists() {
        std::fs::remove_dir_all(artifacts)?;
    }
    if backup_artifacts.exists() {
        copy_artifact_tree_verified(backup_artifacts, artifacts, 0)?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, WorkflowStoreError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut encoded, byte| {
            write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
            encoded
        }))
}

fn write_migration_receipt(
    root: &Path,
    receipt: &WorkflowStoreMigrationReceipt,
) -> Result<(), WorkflowStoreError> {
    let path = root.join(MIGRATION_RECEIPT_FILE);
    let temporary = root.join(format!(
        "{MIGRATION_RECEIPT_FILE}.tmp-{}",
        receipt.migrated_at_ms
    ));
    let bytes = serde_json::to_vec_pretty(receipt)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn write_reset_receipt(
    root: &Path,
    receipt: &WorkflowStoreResetReceipt,
) -> Result<(), WorkflowStoreError> {
    let path = root.join(RESET_RECEIPT_FILE);
    if path.exists() {
        return Err(WorkflowStoreError::InvalidData(
            "workflow reset receipt already exists; retain it before another reset".to_string(),
        ));
    }
    let temporary = root.join(format!("{RESET_RECEIPT_FILE}.tmp-{}", receipt.reset_at_ms));
    let bytes = serde_json::to_vec_pretty(receipt)?;
    if bytes.len() > 16_384 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow reset receipt exceeds 16384 bytes".to_string(),
        ));
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn verify_stored_definition(
    stored: StoredWorkflowDefinition,
) -> Result<StoredWorkflowDefinition, WorkflowStoreError> {
    if sha256_hex(stored.definition_json.as_bytes()) != stored.checksum_sha256 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "definition checksum mismatch: {} v{}",
            stored.definition_id, stored.version
        )));
    }
    let definition: WorkflowDefinition = serde_json::from_str(&stored.definition_json)?;
    definition.validate().map_err(|error| {
        WorkflowStoreError::InvalidData(format!(
            "invalid stored workflow definition {} v{}: {error}",
            stored.definition_id, stored.version
        ))
    })?;
    Ok(stored)
}

fn associated_run_in(
    db: &dyn switchy_database::Database,
    key: &WorkflowRunBindingKey,
) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
    validate_id("owner_plugin_id", &key.owner_plugin_id)?;
    validate_id("workflow_kind", &key.workflow_kind)?;
    validate_id("scope_key", &key.scope_key)?;
    let rows = database::rows(
        db,
        &select("workflow_runs")
            .project_as(
                bounded_text(
                    identifier("run_id"),
                    MAX_ID_BYTES as u64,
                    V::String(String::new()),
                ),
                "run_id",
            )
            .where_eq("owner_plugin_id", key.owner_plugin_id.as_str())
            .where_eq("workflow_kind", key.workflow_kind.as_str())
            .where_eq("scope_key", key.scope_key.as_str())
            .sort("updated_at_ms", SortDirection::Desc)
            .sort("run_id", SortDirection::Asc)
            .limit(1),
    )?;
    match rows.first() {
        Some(row) => {
            let run_id = database::text(row, "run_id")?;
            validate_id("run_id", &run_id)?;
            run_summary_in(db, &run_id)
        }
        None => Ok(None),
    }
}
fn nullable_run_text(
    row: &Row,
    column: &str,
    null_alias: &str,
) -> Result<Option<String>, WorkflowStoreError> {
    if database::unsigned(row, null_alias)? == 1 {
        Ok(None)
    } else {
        Ok(Some(database::text(row, column)?))
    }
}

fn run_summary_in(
    db: &dyn switchy_database::Database,
    run_id: &str,
) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
    let mut query = select("workflow_runs")
        .projections(
            [
                "definition_version",
                "parent_session_generation",
                "single_active",
                "terminal_output_id",
                "terminal_output_checksum_sha256",
                "cancellation_requested_at_ms",
                "created_at_ms",
                "updated_at_ms",
            ]
            .into_iter()
            .map(|column| {
                switchy_database::query::Projection::new(switchy_database::query::identifier(
                    column,
                ))
                .alias(column)
            })
            .collect(),
        )
        .where_eq("run_id", run_id)
        .limit(1);
    for (column, limit) in [
        ("run_id", MAX_ID_BYTES),
        ("definition_id", MAX_ID_BYTES),
        ("workspace_snapshot", MAX_ID_BYTES),
        ("parent_session_id", MAX_ID_BYTES),
        ("owner_plugin_id", MAX_ID_BYTES),
        ("workflow_kind", MAX_ID_BYTES),
        ("scope_key", MAX_ID_BYTES),
        ("display_label", MAX_DISPLAY_LABEL_BYTES),
        ("authored_provenance_json", MAX_INLINE_JSON_BYTES),
        ("authorization_profile_json", MAX_INLINE_JSON_BYTES),
        ("authorization_ceiling", MAX_ID_BYTES),
        ("status", MAX_ID_BYTES),
    ] {
        query = query.project_as(
            bounded_text(identifier(column), limit as u64, V::String(String::new())),
            column,
        );
    }
    for (column, alias) in [
        ("parent_session_id", "parent_null"),
        ("owner_plugin_id", "owner_null"),
        ("workflow_kind", "kind_null"),
        ("scope_key", "scope_null"),
        ("display_label", "label_null"),
        ("authored_provenance_json", "provenance_null"),
    ] {
        query = query.project_as(
            Box::new(where_eq(column, V::Null)) as Box<dyn switchy_database::query::Expression>,
            alias,
        );
    }
    database::rows(db, &query)?
        .first()
        .map(|row| {
            let version =
                u32::try_from(database::unsigned(row, "definition_version")?).map_err(|_| {
                    WorkflowStoreError::InvalidData(
                        "workflow definition version exceeds u32".into(),
                    )
                })?;
            let Some(V::Int64(single_active)) = row.get("single_active") else {
                return Err(WorkflowStoreError::InvalidData(
                    "invalid workflow single-active flag".into(),
                ));
            };
            parse_run_summary((
                database::text(row, "run_id")?,
                database::text(row, "definition_id")?,
                version,
                database::text(row, "workspace_snapshot")?,
                nullable_run_text(row, "parent_session_id", "parent_null")?,
                database::optional_unsigned(row, "parent_session_generation")?,
                nullable_run_text(row, "owner_plugin_id", "owner_null")?,
                nullable_run_text(row, "workflow_kind", "kind_null")?,
                nullable_run_text(row, "scope_key", "scope_null")?,
                nullable_run_text(row, "display_label", "label_null")?,
                single_active,
                nullable_run_text(row, "authored_provenance_json", "provenance_null")?,
                database::optional_text(row, "terminal_output_id")?,
                database::optional_text(row, "terminal_output_checksum_sha256")?,
                database::text(row, "authorization_profile_json")?,
                database::text(row, "authorization_ceiling")?,
                database::text(row, "status")?,
                database::optional_unsigned(row, "cancellation_requested_at_ms")?,
                database::unsigned(row, "created_at_ms")?,
                database::unsigned(row, "updated_at_ms")?,
            ))
        })
        .transpose()
}

fn execution_authority_in(
    db: &dyn switchy_database::Database,
    run_id: &str,
) -> Result<Option<WorkflowExecutionAuthority>, WorkflowStoreError> {
    validate_id("run_id", run_id)?;
    let mut query = select("workflow_runs")
        .projections(
            std::iter::once("coordinator_generation")
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
        )
        .where_eq("run_id", run_id)
        .limit(1);
    for (column, alias) in [
        ("target_artifact_id", "artifact_null"),
        ("coordinator_daemon_instance_id", "daemon_null"),
        ("coordinator_fencing_token", "token_null"),
    ] {
        query = query
            .project_as(
                bounded_text(
                    identifier(column),
                    MAX_ID_BYTES as u64,
                    V::String(String::new()),
                ),
                column,
            )
            .project_as(
                Box::new(where_eq(column, V::Null)) as Box<dyn switchy_database::query::Expression>,
                alias,
            );
    }
    let rows = database::rows(db, &query)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let generation = database::optional_unsigned(row, "coordinator_generation")?;
    let absent = database::unsigned(row, "artifact_null")? == 1
        && database::unsigned(row, "daemon_null")? == 1
        && database::unsigned(row, "token_null")? == 1;
    if absent && generation.is_none() {
        return Ok(None);
    }
    if absent
        || generation.is_none()
        || database::unsigned(row, "artifact_null")? == 1
        || database::unsigned(row, "daemon_null")? == 1
        || database::unsigned(row, "token_null")? == 1
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow run has incomplete execution authority".into(),
        ));
    }
    let target_artifact_id = database::text(row, "target_artifact_id")?;
    let daemon_instance_id = database::text(row, "coordinator_daemon_instance_id")?;
    let fencing_token = database::text(row, "coordinator_fencing_token")?;
    validate_id("authority artifact", &target_artifact_id)?;
    validate_id("authority daemon", &daemon_instance_id)?;
    validate_id("authority fence", &fencing_token)?;
    Ok(Some(WorkflowExecutionAuthority {
        target_artifact_id,
        daemon_instance_id,
        generation: generation.unwrap(),
        fencing_token,
    }))
}

fn verify_execution_authority_in(
    db: &dyn switchy_database::Database,
    run_id: &str,
    authority: &WorkflowExecutionAuthority,
) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", run_id)?;
    validate_id("authority artifact", &authority.target_artifact_id)?;
    validate_id("authority daemon", &authority.daemon_instance_id)?;
    validate_id("authority fence", &authority.fencing_token)?;
    let generation = i64::try_from(authority.generation).map_err(|_| {
        WorkflowStoreError::InvalidData(
            "workflow authority generation exceeds SQLite integer range".into(),
        )
    })?;
    // Equality checks run inside SQLite; damaged persisted identities are never materialized.
    let rows = database::rows(
        db,
        &select("workflow_runs")
            .project_as(count_all(), "matches")
            .where_eq("run_id", run_id)
            .where_eq("target_artifact_id", authority.target_artifact_id.as_str())
            .where_eq(
                "coordinator_daemon_instance_id",
                authority.daemon_instance_id.as_str(),
            )
            .where_eq("coordinator_generation", generation)
            .where_eq(
                "coordinator_fencing_token",
                authority.fencing_token.as_str(),
            ),
    )?;
    if rows
        .first()
        .map(|row| database::unsigned(row, "matches"))
        .transpose()?
        == Some(1)
    {
        Ok(())
    } else {
        Err(WorkflowStoreError::InvalidData(format!(
            "workflow execution authority is stale or foreign: {run_id}"
        )))
    }
}

fn definition_in(
    db: &dyn switchy_database::Database,
    definition_id: &str,
    version: u32,
) -> Result<Option<StoredWorkflowDefinition>, WorkflowStoreError> {
    let rows = database::rows(
        db,
        &select("workflow_definitions")
            .projections(
                ["definition_id", "version", "checksum_sha256"]
                    .into_iter()
                    .map(|column| {
                        switchy_database::query::Projection::new(
                            switchy_database::query::identifier(column),
                        )
                        .alias(column)
                    })
                    .collect(),
            )
            .project_as(
                bounded_text(
                    identifier("definition_json"),
                    MAX_INLINE_JSON_BYTES as u64,
                    V::Null,
                ),
                "definition_json",
            )
            .where_eq("definition_id", definition_id)
            .where_eq("version", i64::from(version))
            .limit(1),
    )?;
    rows.first()
        .map(|row| {
            let version = u32::try_from(database::unsigned(row, "version")?).map_err(|_| {
                WorkflowStoreError::InvalidData("definition version exceeds u32".into())
            })?;
            verify_stored_definition(StoredWorkflowDefinition {
                definition_id: database::text(row, "definition_id")?,
                version,
                checksum_sha256: database::text(row, "checksum_sha256")?,
                definition_json: database::text(row, "definition_json").map_err(|_| {
                    WorkflowStoreError::InvalidData(
                        "workflow definition payload is invalid or oversized".into(),
                    )
                })?,
            })
        })
        .transpose()
}

fn append_event_in(
    db: &dyn switchy_database::Database,
    run_id: &str,
    event_type: &str,
    payload_json: &str,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    let created_at_ms = i64::try_from(created_at_ms).map_err(|_| {
        WorkflowStoreError::InvalidData(
            "workflow event timestamp exceeds SQLite integer range".into(),
        )
    })?;
    crate::database::block_on(
        db.insert("workflow_events")
            .value("run_id", run_id)
            .value("event_type", event_type)
            .value("payload_json", payload_json)
            .value("created_at_ms", created_at_ms)
            .execute(db),
    )?;
    Ok(())
}

fn append_event(
    db: &dyn switchy_database::Database,
    run_id: &str,
    event_type: &str,
    payload_json: &str,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    append_event_in(db, run_id, event_type, payload_json, created_at_ms)
}

type RepairRequiredAttempt = (String, String, String, u32, String);

fn repair_required_attempt(
    db: &dyn switchy_database::Database,
    dispatch_identity: &str,
) -> Result<RepairRequiredAttempt, WorkflowStoreError> {
    let rows = database::rows(
        db,
        &select("workflow_attempts")
            .projections(
                [
                    "run_id",
                    "node_id",
                    "activation_id",
                    "attempt",
                    "side_effect",
                ]
                .into_iter()
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
            )
            .where_eq("dispatch_identity", dispatch_identity)
            .where_eq("status", "repair_required")
            .limit(1),
    )?;
    let row = rows.first().ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "repair-required workflow attempt not found: {dispatch_identity}"
        ))
    })?;
    Ok((
        database::text(row, "run_id")?,
        database::text(row, "node_id")?,
        database::text(row, "activation_id")?,
        u32::try_from(database::unsigned(row, "attempt")?)
            .map_err(|_| WorkflowStoreError::InvalidData("repair attempt exceeds u32".into()))?,
        database::text(row, "side_effect")?,
    ))
}

fn finalize_run_cancellation_if_settled(
    db: &dyn switchy_database::Database,
    run_id: &str,
    cancelled_at_ms: u64,
) -> Result<bool, WorkflowStoreError> {
    if !database::rows(
        db,
        &select("workflow_attempts")
            .projections(database::projections(&["dispatch_identity"]))
            .where_eq("run_id", run_id)
            .where_not_in(
                "status",
                vec!["succeeded", "failed", "cancelled", "paused", "abandoned"],
            )
            .limit(1),
    )?
    .is_empty()
    {
        return Ok(false);
    }
    let changed = crate::database::block_on(
        db.exec_update_count(
            &(update("workflow_runs")
                .value("status", "cancelled")
                .value("updated_at_ms", database::integer_value(cancelled_at_ms)?)
                .where_eq("run_id", run_id)
                .where_in("status", vec!["running", "paused"])
                .where_not_eq(
                    "cancellation_requested_at_ms",
                    switchy_database::DatabaseValue::Null,
                )),
        ),
    )?;
    if changed == 1 {
        crate::database::block_on(
            db.exec_update_count(
                &(update("workflow_activations")
                    .value("status", "cancelled")
                    .where_eq("run_id", run_id)
                    .where_in(
                        "status",
                        vec![
                            "pending",
                            "running",
                            "waiting_input",
                            "waiting_approval",
                            "waiting_mutation_approval",
                        ],
                    )
                    .where_eq("output_id", switchy_database::DatabaseValue::Null)),
            ),
        )?;
        append_event(db, run_id, "run_cancelled", "{}", cancelled_at_ms)?;
    }
    Ok(changed == 1)
}

fn validate_repair_resolution(resolution: &RepairResolution) -> Result<(), WorkflowStoreError> {
    match resolution {
        RepairResolution::ConfirmSucceeded { output } => validate_output(output),
        RepairResolution::ConfirmFailed { message }
        | RepairResolution::ConfirmCancelled { message } => {
            validate_bounded_message("repair message", message)
        }
        RepairResolution::AbandonForExplicitRetry { reason } => {
            validate_bounded_message("repair reason", reason)
        }
    }
}

fn validate_mutation_approval_input_checksum(
    approval: &WorkflowMutationApproval,
    input_json: Option<&str>,
    connection: &dyn switchy_database::Database,
) -> Result<(), WorkflowStoreError> {
    let input: serde_json::Value = serde_json::from_str(input_json.unwrap_or("null"))?;
    let node = run_graph::bound_activation_node(
        connection,
        &approval.run_id,
        &approval.node_id,
        &approval.activation_id,
    )?
    .ok_or_else(|| {
        WorkflowStoreError::InvalidData(
            "mutation approval references a missing activation graph binding".to_string(),
        )
    })?;
    let owner_input = if node.dataflow == WorkflowNodeDataflowPolicy::StateEnvelopeV1 {
        let block: WorkflowBlockDefinition = serde_json::from_value(node.configuration.clone())?;
        prepare_workflow_node_dataflow(node.dataflow, &node.input, &block.input, &input)
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
            .owner_input()
            .clone()
    } else {
        input
    };
    let input = serde_json::to_vec(&owner_input)?;
    if sha256_hex(&input) != approval.scope.input_checksum_sha256 {
        return Err(WorkflowStoreError::InvalidData(
            "mutation approval input checksum is stale".to_string(),
        ));
    }
    if approval.scope.operation_facts.is_some()
        != approval.scope.preparation_descriptor_sha256.is_some()
    {
        return Err(WorkflowStoreError::InvalidData(
            "mutation approval preparation identity is incomplete".to_string(),
        ));
    }
    Ok(())
}

fn bounded_json<T: Serialize>(label: &str, value: &T) -> Result<String, WorkflowStoreError> {
    let json = serde_json::to_string(value)?;
    if json.len() > MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} exceeds {MAX_INLINE_JSON_BYTES} bytes"
        )));
    }
    Ok(json)
}

fn resource_lease(
    db: &dyn switchy_database::Database,
    lease_id: &str,
) -> Result<Option<(WorkflowResourceLease, Option<u64>)>, WorkflowStoreError> {
    let mut query = select("workflow_resource_leases").projections(database::projections(&[
        "acquired_at_ms",
        "expires_at_ms",
        "released_at_ms",
    ]));
    for column in ["run_id", "node_id", "activation_id", "resource_key"] {
        query = query.project_as(
            bounded_text(
                identifier(column),
                MAX_ID_BYTES as u64,
                switchy_database::DatabaseValue::Null,
            ),
            column,
        );
    }
    query = query
        .project_as(
            bounded_text(
                identifier("mode"),
                16,
                switchy_database::DatabaseValue::Null,
            ),
            "mode",
        )
        .where_eq("lease_id", lease_id)
        .limit(1);
    database::rows(db, &query)?
        .first()
        .map(|row| {
            Ok((
                WorkflowResourceLease {
                    lease_id: lease_id.into(),
                    run_id: database::text(row, "run_id")?,
                    node_id: database::text(row, "node_id")?,
                    activation_id: database::text(row, "activation_id")?,
                    resource_key: database::text(row, "resource_key")?,
                    mode: parse_resource_lease_mode(&database::text(row, "mode")?)?,
                    acquired_at_ms: database::unsigned(row, "acquired_at_ms")?,
                    expires_at_ms: database::optional_unsigned(row, "expires_at_ms")?,
                },
                database::optional_unsigned(row, "released_at_ms")?,
            ))
        })
        .transpose()
}

fn parse_resource_lease_mode(mode: &str) -> Result<ResourceLeaseMode, WorkflowStoreError> {
    match mode {
        "read" => Ok(ResourceLeaseMode::Read),
        "write" => Ok(ResourceLeaseMode::Write),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "unknown workflow resource lease mode: {mode}"
        ))),
    }
}

fn validate_decision(decision: &WorkflowDecision) -> Result<(), WorkflowStoreError> {
    validate_id("decision_id", &decision.decision_id)?;
    validate_id("run_id", &decision.run_id)?;
    validate_id("decision_type", &decision.decision_type)?;
    if let Some(node_id) = &decision.node_id {
        validate_id("node_id", node_id)?;
    }
    Ok(())
}

fn validate_mutation_approval(
    approval: &WorkflowMutationApproval,
) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("approval_id", approval.approval_id.as_str()),
        ("run_id", approval.run_id.as_str()),
        ("node_id", approval.node_id.as_str()),
        ("activation_id", approval.activation_id.as_str()),
    ] {
        validate_id(label, value)?;
    }
    approval
        .scope
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    if approval.scope.run_id != approval.run_id
        || approval.scope.node_id != approval.node_id
        || approval.scope.activation_id != approval.activation_id
        || approval
            .expires_at_ms
            .is_some_and(|expires| expires <= approval.requested_at_ms)
    {
        return Err(WorkflowStoreError::InvalidData(
            "mutation approval scope or expiry does not match its durable identity".to_string(),
        ));
    }
    Ok(())
}

fn validate_grant(grant: &WorkflowGrant) -> Result<(), WorkflowStoreError> {
    validate_id("grant_id", &grant.grant_id)?;
    validate_id("run_id", &grant.run_id)?;
    validate_id("node_id", &grant.node_id)?;
    if grant.max_uses == Some(0)
        || grant
            .max_uses
            .is_some_and(|max_uses| grant.uses_consumed > max_uses)
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow grant use accounting is invalid".to_string(),
        ));
    }
    if grant
        .expires_at_ms
        .is_some_and(|expires| expires <= grant.granted_at_ms)
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow grant expiry must follow grant time".to_string(),
        ));
    }
    Ok(())
}

fn validate_resource_lease(lease: &WorkflowResourceLease) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("lease_id", lease.lease_id.as_str()),
        ("run_id", lease.run_id.as_str()),
        ("node_id", lease.node_id.as_str()),
        ("activation_id", lease.activation_id.as_str()),
        ("resource_key", lease.resource_key.as_str()),
    ] {
        validate_id(label, value)?;
    }
    if lease
        .expires_at_ms
        .is_some_and(|expires| expires <= lease.acquired_at_ms)
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow resource lease expiry must follow acquisition".to_string(),
        ));
    }
    Ok(())
}

fn validate_projection_checkpoint(
    checkpoint: &WorkflowProjectionCheckpoint,
) -> Result<(), WorkflowStoreError> {
    validate_id("projection_name", &checkpoint.projection_name)?;
    if checkpoint.projection_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow projection version must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_bounded_message(label: &str, value: &str) -> Result<(), WorkflowStoreError> {
    if value.trim().is_empty() || value.len() > MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} must contain 1..={MAX_INLINE_JSON_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_run_input(
    definition: &WorkflowDefinition,
    input: &serde_json::Value,
) -> Result<String, WorkflowStoreError> {
    let input_json = bounded_json("workflow run input", input)?;
    let validator = jsonschema::validator_for(&definition.input.schema).map_err(|error| {
        WorkflowStoreError::InvalidData(format!("workflow input schema is invalid: {error}"))
    })?;
    if let Err(error) = validator.validate(input) {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow run input failed schema validation: {error}"
        )));
    }
    Ok(input_json)
}

fn diagnose_run_creation_database_error(operation: &'static str, error: &WorkflowStoreError) {
    if let WorkflowStoreError::StructuredDatabase(error) = error {
        let reason = run_creation_database_reason(error);
        tracing::warn!(target:"bcode_workflow_store::run_creation",operation,reason,"workflow run creation database failure");
    }
}

fn run_creation_database_reason(error: &switchy_database::DatabaseError) -> &'static str {
    match error.classification() {
        Some(switchy_database::DatabaseErrorKind::Busy) => return "database_busy",
        Some(switchy_database::DatabaseErrorKind::Locked) => return "database_locked",
        Some(switchy_database::DatabaseErrorKind::Constraint) => return "database_constraint",
        Some(switchy_database::DatabaseErrorKind::Corrupt) => return "database_corrupt",
        None => {}
    }
    let message = error.to_string();
    // Classify only fixed SQLite prefixes; never emit the suffix (names, SQL, or values).
    if message.starts_with("no such table:") {
        "missing_table"
    } else if message.starts_with("no such column:") {
        "missing_column"
    } else if message.starts_with("table ") && message.contains(" has no column named ") {
        "missing_insert_column"
    } else if message.starts_with("foreign key mismatch") {
        "foreign_key_mismatch"
    } else {
        "sqlite_error"
    }
}

fn validate_replacement_package(
    transaction: &dyn switchy_database::Database,
    old_run_id: &str,
    successor: &NewWorkflowRun,
) -> Result<Option<(String, String)>, WorkflowStoreError> {
    let rows = database::rows(
        transaction,
        &select("workflow_run_packages")
            .project_as(
                bounded_text(identifier("package_id"), MAX_ID_BYTES as u64, V::Null),
                "package_id",
            )
            .project_as(
                bounded_text(identifier("lock_digest"), 64, V::Null),
                "lock_digest",
            )
            .where_eq("run_id", old_run_id)
            .limit(1),
    )?;
    let package = rows
        .first()
        .map(|row| {
            Ok::<_, WorkflowStoreError>((
                database::text(row, "package_id")?,
                database::text(row, "lock_digest")?,
            ))
        })
        .transpose()?;
    if let Some((id, digest)) = &package {
        let lock = WorkflowStore::package_lock_on(transaction, id, digest)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("replacement package lock unavailable".into())
        })?;
        if !lock.members.iter().any(|member| {
            member.definition_identity.definition_id == successor.definition_id
                && member.definition_identity.definition_version == successor.definition_version
        }) {
            return Err(WorkflowStoreError::InvalidData(
                "replacement definition is not in the pinned package".into(),
            ));
        }
    }
    Ok(package)
}

fn validate_replacement_successor(
    transaction: &dyn switchy_database::Database,
    successor: &NewWorkflowRun,
    authority: &WorkflowExecutionAuthority,
) -> Result<(), WorkflowStoreError> {
    if successor.execution_authority.as_ref() != Some(authority) {
        return Err(WorkflowStoreError::InvalidData(
            "replacement successor must target the current fenced coordinator".into(),
        ));
    }

    let exists = !database::rows(
        transaction,
        &select("workflow_runs")
            .projections(database::projections(&["run_id"]))
            .where_eq("run_id", successor.run_id.as_str())
            .limit(1),
    )?
    .is_empty();
    if exists {
        return Err(WorkflowStoreError::InvalidData(
            "replacement successor identity already exists".into(),
        ));
    }
    Ok(())
}

fn validate_stored_run_input(
    transaction: &dyn switchy_database::Database,
    run: &NewWorkflowRun,
) -> Result<(WorkflowDefinition, Option<String>), WorkflowStoreError> {
    let rows = database::rows(
        transaction,
        &select("workflow_definitions")
            .projections(database::projections(&["checksum_sha256"]))
            .project_as(
                bounded_text(
                    identifier("definition_json"),
                    MAX_INLINE_JSON_BYTES as u64,
                    switchy_database::DatabaseValue::Null,
                ),
                "definition_json",
            )
            .where_eq("definition_id", run.definition_id.as_str())
            .where_eq("version", i64::from(run.definition_version))
            .limit(1),
    )?;
    let definition_json = rows
        .first()
        .map(|row| {
            Ok::<_, WorkflowStoreError>((
                database::optional_text(row, "definition_json")?,
                database::text(row, "checksum_sha256")?,
            ))
        })
        .transpose()?;
    let Some((definition_json, checksum)) = definition_json else {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow definition not found: {} v{}",
            run.definition_id, run.definition_version
        )));
    };
    let definition_json = definition_json.ok_or_else(|| {
        WorkflowStoreError::InvalidData(
            "workflow definition payload is invalid or oversized".to_string(),
        )
    })?;
    if sha256_hex(definition_json.as_bytes()) != checksum {
        return Err(WorkflowStoreError::InvalidData(
            "workflow definition checksum mismatch".to_string(),
        ));
    }
    let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
    definition.validate().map_err(|error| {
        WorkflowStoreError::InvalidData(format!("invalid stored workflow definition: {error}"))
    })?;
    let input_json = run
        .input
        .as_ref()
        .map(|input| validate_run_input(&definition, input))
        .transpose()?;
    Ok((definition, input_json))
}

fn create_run_in_transaction(
    transaction: &dyn switchy_database::Database,
    run: &NewWorkflowRun,
) -> Result<(), WorkflowStoreError> {
    run_admission::create(transaction, run)
}

fn validate_child_ancestry(
    transaction: &dyn switchy_database::Database,
    link: &WorkflowRunLink,
) -> Result<(), WorkflowStoreError> {
    let mut current = link.parent_run_id.clone();
    let mut expected_depth = link.depth - 1;
    loop {
        if current == link.child_run_id || expected_depth == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "workflow run ownership cycle or invalid depth".into(),
            ));
        }

        let rows = database::rows(
            transaction,
            &select("workflow_run_links")
                .projections(database::projections(&[
                    "parent_run_id",
                    "root_run_id",
                    "depth",
                ]))
                .where_eq("child_run_id", current.as_str())
                .limit(1),
        )?;
        let parent = rows
            .first()
            .map(|row| {
                Ok::<_, WorkflowStoreError>((
                    database::text(row, "parent_run_id")?,
                    database::text(row, "root_run_id")?,
                    u32::try_from(database::unsigned(row, "depth")?).map_err(|_| {
                        WorkflowStoreError::InvalidData(
                            "workflow ancestry depth exceeds u32".into(),
                        )
                    })?,
                ))
            })
            .transpose()?;
        let Some((parent, root, depth)) = parent else {
            if current != link.root_run_id || expected_depth != 1 {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow ancestry root or depth mismatch".into(),
                ));
            }
            return Ok(());
        };
        if root != link.root_run_id || depth != expected_depth {
            return Err(WorkflowStoreError::InvalidData(
                "workflow ancestry root or depth mismatch".into(),
            ));
        }
        current = parent;
        expected_depth -= 1;
    }
}

fn required_definition_capability(
    definition: &WorkflowDefinition,
    excluded_call_node: &str,
) -> Result<bcode_workflow::WorkflowToolCapability, WorkflowStoreError> {
    definition
        .nodes
        .values()
        .filter(|node| node.id != excluded_call_node)
        .try_fold(
            bcode_workflow::WorkflowToolCapability::Disabled,
            |maximum, node| {
                let capability = match node.kind {
                    bcode_workflow::NodeKind::Agent => {
                        let configuration: bcode_workflow::WorkflowPromptConfiguration =
                            serde_json::from_value(node.configuration.clone())?;
                        configuration.tool_capability
                    }
                    bcode_workflow::NodeKind::PluginBlock => {
                        let block: bcode_workflow::WorkflowBlockDefinition =
                            serde_json::from_value(node.configuration.clone())?;
                        block.authorization.capability
                    }
                    _ => bcode_workflow::WorkflowToolCapability::Disabled,
                };
                Ok(maximum.max(capability))
            },
        )
}

fn bounded_descendant_count(
    db: &dyn switchy_database::Database,
    root_run_id: &str,
    limit: u32,
) -> Result<u32, WorkflowStoreError> {
    if limit == 0 {
        return Ok(0);
    }
    u32::try_from(
        database::rows(
            db,
            &select("workflow_run_links")
                .projections(database::projections(&["root_run_id"]))
                .where_eq("root_run_id", root_run_id)
                .limit(usize::try_from(limit).map_err(|_| {
                    WorkflowStoreError::InvalidData("descendant count limit exceeds usize".into())
                })?),
        )?
        .len(),
    )
    .map_err(|_| WorkflowStoreError::InvalidData("workflow descendant count exceeds u32".into()))
}

fn validate_run_link(
    link: &WorkflowRunLink,
    child: &NewWorkflowRun,
) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("root_run_id", link.root_run_id.as_str()),
        ("parent_run_id", link.parent_run_id.as_str()),
        ("parent_node_id", link.parent_node_id.as_str()),
        ("parent_activation_id", link.parent_activation_id.as_str()),
        ("child_run_id", link.child_run_id.as_str()),
    ] {
        validate_id(label, value)?;
    }
    link.target
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    if link.version != WORKFLOW_RUN_LINK_VERSION
        || link.parent_attempt == 0
        || link.depth < 2
        || link.depth > child.limits.recursion_depth_cap
        || link.child_run_id != child.run_id
        || link.child_run_id
            != workflow_child_run_id(
                &link.root_run_id,
                &link.parent_run_id,
                &link.parent_activation_id,
                link.parent_attempt,
                &link.target,
            )
        || link.parent_run_id == link.child_run_id
        || link.root_run_id == link.child_run_id
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow child link identity, version, attempt, depth, or recursion is invalid"
                .to_string(),
        ));
    }
    Ok(())
}

fn child_link_for_parent_attempt(
    db: &dyn switchy_database::Database,
    link: &WorkflowRunLink,
) -> Result<Option<WorkflowRunLink>, WorkflowStoreError> {
    child_link_lookup(
        db,
        &link.parent_run_id,
        &link.parent_node_id,
        &link.parent_activation_id,
        link.parent_attempt,
    )
}
fn child_link_for_parent_attempt_connection(
    db: &dyn switchy_database::Database,
    request: &AttemptReconciliationRequest,
) -> Result<Option<WorkflowRunLink>, WorkflowStoreError> {
    child_link_lookup(
        db,
        &request.run_id,
        &request.node_id,
        &request.activation_id,
        request.attempt,
    )
}
fn child_link_lookup(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    attempt: u32,
) -> Result<Option<WorkflowRunLink>, WorkflowStoreError> {
    database::rows(
        db,
        &select("workflow_run_links")
            .projections(
                [
                    "root_run_id",
                    "parent_run_id",
                    "parent_node_id",
                    "parent_activation_id",
                    "parent_attempt",
                    "child_run_id",
                    "version",
                    "target_json",
                    "depth",
                    "created_at_ms",
                ]
                .into_iter()
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
            )
            .where_eq("parent_run_id", run_id)
            .where_eq("parent_node_id", node_id)
            .where_eq("parent_activation_id", activation_id)
            .where_eq("parent_attempt", i64::from(attempt))
            .limit(1),
    )?
    .first()
    .map(decode_run_link)
    .transpose()
}

fn decode_run_link(row: &switchy_database::Row) -> Result<WorkflowRunLink, WorkflowStoreError> {
    let version = u32::try_from(database::unsigned(row, "version")?).map_err(|_| {
        WorkflowStoreError::InvalidData("unsupported workflow run link version".into())
    })?;
    if version != WORKFLOW_RUN_LINK_VERSION {
        return Err(WorkflowStoreError::InvalidData(
            "unsupported workflow run link version".into(),
        ));
    }
    for column in [
        "root_run_id",
        "parent_run_id",
        "parent_node_id",
        "parent_activation_id",
        "child_run_id",
    ] {
        validate_id(column, &database::text(row, column)?)?;
    }
    let child_run_id = database::text(row, "child_run_id")?;
    let root_run_id = database::text(row, "root_run_id")?;
    let parent_run_id = database::text(row, "parent_run_id")?;
    if child_run_id == root_run_id || child_run_id == parent_run_id {
        return Err(WorkflowStoreError::InvalidData(
            "workflow child link aliases its root or parent run".into(),
        ));
    }
    let json = database::text(row, "target_json")?;
    if json.len() > MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(
            "workflow run link target exceeds the inline byte limit".into(),
        ));
    }
    let target: bcode_workflow::WorkflowCallTarget = serde_json::from_str(&json)?;
    target
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    let parent_attempt =
        u32::try_from(database::unsigned(row, "parent_attempt")?).map_err(|_| {
            WorkflowStoreError::InvalidData("workflow run link attempt is invalid".into())
        })?;
    let depth = u32::try_from(database::unsigned(row, "depth")?).map_err(|_| {
        WorkflowStoreError::InvalidData("workflow run link depth is invalid".into())
    })?;
    if parent_attempt == 0 || depth < 2 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow run link attempt or depth is invalid".into(),
        ));
    }
    Ok(WorkflowRunLink {
        root_run_id,
        parent_run_id,
        parent_node_id: database::text(row, "parent_node_id")?,
        parent_activation_id: database::text(row, "parent_activation_id")?,
        parent_attempt,
        child_run_id,
        version,
        target,
        depth,
        created_at_ms: database::unsigned(row, "created_at_ms")?,
    })
}

fn validate_run(run: &NewWorkflowRun) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", &run.run_id)?;
    validate_id("definition_id", &run.definition_id)?;
    validate_id("workspace_snapshot", &run.workspace_snapshot)?;
    run.authorization_profile
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    validate_run_limits(&run.limits)?;
    if run.definition_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "definition version must be positive".to_string(),
        ));
    }
    if let Some(parent_session_id) = &run.parent_session_id {
        validate_id("parent_session_id", parent_session_id)?;
    }
    if run.parent_session_generation.is_some() && run.parent_session_id.is_none() {
        return Err(WorkflowStoreError::InvalidData(
            "parent session generation requires parent_session_id".to_string(),
        ));
    }
    if let Some(authority) = &run.execution_authority {
        validate_id("target_artifact_id", &authority.target_artifact_id)?;
        validate_id(
            "coordinator_daemon_instance_id",
            &authority.daemon_instance_id,
        )?;
        validate_id("coordinator_fencing_token", &authority.fencing_token)?;
        if authority.generation == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "workflow coordinator generation must be positive".to_string(),
            ));
        }
        if run.parent_session_id.is_none() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow execution authority requires a parent session".to_string(),
            ));
        }
    }
    if let Some(binding) = &run.binding {
        validate_binding(binding)?;
    }
    if let Some(provenance) = &run.authored_provenance {
        validate_authored_run_provenance(run, provenance)?;
    }
    Ok(())
}

fn validate_authored_run_provenance(
    run: &NewWorkflowRun,
    provenance: &AuthoredWorkflowRunProvenance,
) -> Result<(), WorkflowStoreError> {
    if provenance.version != AUTHORED_WORKFLOW_RUN_PROVENANCE_VERSION {
        return Err(WorkflowStoreError::InvalidData(format!(
            "unsupported authored workflow run provenance version: {}",
            provenance.version
        )));
    }
    validate_id("authored workflow_id", &provenance.workflow_id)?;
    if provenance.revision == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "authored workflow revision must be positive".to_string(),
        ));
    }
    validate_id(
        "authored definition kind",
        &provenance.definition_identity.kind,
    )?;
    if provenance.definition_identity.kind != provenance.workflow_id {
        return Err(WorkflowStoreError::InvalidData(
            "authored definition kind must match the authored workflow identity".to_string(),
        ));
    }
    validate_id(
        "authored definition_id",
        &provenance.definition_identity.definition_id,
    )?;
    if provenance.definition_identity.definition_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "authored definition version must be positive".to_string(),
        ));
    }
    if provenance.definition_identity.definition_id != run.definition_id
        || provenance.definition_identity.definition_version != run.definition_version
    {
        return Err(WorkflowStoreError::InvalidData(
            "authored run provenance definition does not match the run definition".to_string(),
        ));
    }
    match (&provenance.preset_id, provenance.preset_generation) {
        (Some(preset_id), Some(generation)) => {
            validate_id("authored preset_id", preset_id)?;
            if generation == 0 {
                return Err(WorkflowStoreError::InvalidData(
                    "authored preset generation must be positive".to_string(),
                ));
            }
        }
        (None, None) => {}
        _ => {
            return Err(WorkflowStoreError::InvalidData(
                "authored preset identity and generation must be present together".to_string(),
            ));
        }
    }
    bounded_json("authored run configuration", &provenance.configuration)?;
    validate_persistable_authoring_value("authored_run.configuration", &provenance.configuration)
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    Ok(())
}

fn validate_persisted_authored_run_provenance(
    transaction: &dyn switchy_database::Database,
    run: &NewWorkflowRun,
    provenance: &AuthoredWorkflowRunProvenance,
) -> Result<(), WorkflowStoreError> {
    let rows = database::rows(
        transaction,
        &select("workflow_revisions")
            .projections(database::projections(&[
                "definition_id",
                "definition_version",
            ]))
            .where_eq("workflow_id", provenance.workflow_id.as_str())
            .where_eq("revision", database::integer_value(provenance.revision)?)
            .limit(1),
    )?;
    let revision = rows
        .first()
        .map(|row| {
            Ok::<_, WorkflowStoreError>((
                database::text(row, "definition_id")?,
                u32::try_from(database::unsigned(row, "definition_version")?).map_err(|_| {
                    WorkflowStoreError::InvalidData(
                        "authored definition version exceeds u32".into(),
                    )
                })?,
            ))
        })
        .transpose()?;
    let Some((definition_id, definition_version)) = revision else {
        return Err(WorkflowStoreError::InvalidData(format!(
            "authored workflow revision not found: {} v{}",
            provenance.workflow_id, provenance.revision
        )));
    };
    if definition_id != run.definition_id
        || definition_version != run.definition_version
        || provenance.definition_identity.definition_id != definition_id
        || provenance.definition_identity.definition_version != definition_version
    {
        return Err(WorkflowStoreError::InvalidData(
            "authored run provenance does not match the persisted revision definition".to_string(),
        ));
    }
    if let (Some(preset_id), Some(preset_generation)) =
        (&provenance.preset_id, provenance.preset_generation)
    {
        let rows = database::rows(
            transaction,
            &select("workflow_presets")
                .projections(database::projections(&["revision", "generation"]))
                .where_eq("workflow_id", provenance.workflow_id.as_str())
                .where_eq("preset_id", preset_id.as_str())
                .limit(1),
        )?;
        let preset = rows
            .first()
            .map(|row| {
                Ok::<_, WorkflowStoreError>((
                    database::unsigned(row, "revision")?,
                    database::unsigned(row, "generation")?,
                ))
            })
            .transpose()?;
        if preset != Some((provenance.revision, preset_generation)) {
            return Err(WorkflowStoreError::InvalidData(
                "authored run provenance does not match the persisted preset generation"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_binding(binding: &WorkflowRunBinding) -> Result<(), WorkflowStoreError> {
    validate_id("owner_plugin_id", &binding.owner_plugin_id)?;
    validate_id("workflow_kind", &binding.workflow_kind)?;
    validate_id("scope_key", &binding.scope_key)?;
    if binding
        .display_label
        .as_ref()
        .is_some_and(|label| label.trim().is_empty() || label.len() > MAX_DISPLAY_LABEL_BYTES)
    {
        return Err(WorkflowStoreError::InvalidData(format!(
            "display_label must contain 1..={MAX_DISPLAY_LABEL_BYTES} bytes when present"
        )));
    }
    Ok(())
}

fn validate_run_limits(limits: &WorkflowRunLimits) -> Result<(), WorkflowStoreError> {
    if limits.node_execution_cap > i64::MAX as u64 {
        return Err(WorkflowStoreError::InvalidData(
            "node_execution_cap exceeds the supported storage integer range".to_string(),
        ));
    }
    for (label, value) in [
        ("node_execution_cap", limits.node_execution_cap),
        (
            "concurrency_cap",
            u64::from(limits.concurrency_cap.unwrap_or(1)),
        ),
        ("cycle_cap", u64::from(limits.cycle_cap)),
        ("recursion_depth_cap", u64::from(limits.recursion_depth_cap)),
    ] {
        if value == 0 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "{label} must be positive"
            )));
        }
    }
    Ok(())
}

fn validate_activation(activation: &NewActivation) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", &activation.run_id)?;
    validate_id("node_id", &activation.node_id)?;
    validate_id("activation_id", &activation.activation_id)
}

fn validate_prepared_attempt(attempt: &PreparedAttempt) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", &attempt.run_id)?;
    validate_id("node_id", &attempt.node_id)?;
    validate_id("activation_id", &attempt.activation_id)?;
    if attempt.attempt == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "attempt must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_execution_session_link_identity(
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    attempt: u32,
) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", run_id)?;
    validate_id("node_id", node_id)?;
    validate_id("activation_id", activation_id)?;
    if attempt == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "execution-session link attempt must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_execution_session_link(
    link: &WorkflowExecutionSessionLink,
) -> Result<(), WorkflowStoreError> {
    if link.version != WORKFLOW_EXECUTION_SESSION_LINK_VERSION {
        return Err(WorkflowStoreError::InvalidData(format!(
            "unsupported workflow execution-session link version {}; expected {WORKFLOW_EXECUTION_SESSION_LINK_VERSION}",
            link.version
        )));
    }
    validate_execution_session_link_identity(
        &link.run_id,
        &link.node_id,
        &link.activation_id,
        link.attempt,
    )?;
    validate_id("session_id", &link.session_id)?;
    validate_id("workspace_snapshot", &link.workspace_snapshot)
}

fn decode_execution_session_link(
    row: &switchy_database::Row,
) -> Result<WorkflowExecutionSessionLink, WorkflowStoreError> {
    fn int(row: &switchy_database::Row, name: &str) -> Result<u32, WorkflowStoreError> {
        u32::try_from(database::unsigned(row, name)?).map_err(|_| {
            WorkflowStoreError::InvalidData(format!("execution session {name} exceeds u32"))
        })
    }
    Ok(WorkflowExecutionSessionLink {
        version: int(row, "version")?,
        run_id: database::text(row, "run_id")?,
        node_id: database::text(row, "node_id")?,
        activation_id: database::text(row, "activation_id")?,
        attempt: int(row, "attempt")?,
        session_id: database::text(row, "session_id")?,
        workspace_snapshot: database::text(row, "workspace_snapshot")?,
        created_at_ms: database::unsigned(row, "created_at_ms")?,
    })
}
fn execution_session_link_by_activation_connection(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    attempt: u32,
) -> Result<Option<WorkflowExecutionSessionLink>, WorkflowStoreError> {
    database::rows(
        db,
        &select("workflow_execution_sessions")
            .projections(
                [
                    "version",
                    "run_id",
                    "node_id",
                    "activation_id",
                    "attempt",
                    "session_id",
                    "workspace_snapshot",
                    "created_at_ms",
                ]
                .into_iter()
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
            )
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .where_eq("attempt", i64::from(attempt))
            .limit(1),
    )?
    .first()
    .map(|row| {
        let link = decode_execution_session_link(row)?;
        validate_execution_session_link(&link)?;
        Ok(link)
    })
    .transpose()
}
fn execution_session_link_by_activation(
    db: &dyn switchy_database::Database,
    link: &WorkflowExecutionSessionLink,
) -> Result<Option<WorkflowExecutionSessionLink>, WorkflowStoreError> {
    execution_session_link_by_activation_connection(
        db,
        &link.run_id,
        &link.node_id,
        &link.activation_id,
        link.attempt,
    )
}

fn validate_dispatch_receipt(receipt: &DispatchReceipt) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("run_id", receipt.run_id.as_str()),
        ("node_id", receipt.node_id.as_str()),
        ("activation_id", receipt.activation_id.as_str()),
        ("dispatch_identity", receipt.dispatch_identity.as_str()),
    ] {
        validate_id(label, value)?;
    }
    if receipt.attempt == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "attempt must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_output(output: &ValidatedOutput) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("output_id", output.output_id.as_str()),
        ("run_id", output.run_id.as_str()),
        ("node_id", output.node_id.as_str()),
        ("activation_id", output.activation_id.as_str()),
        ("schema_id", output.schema_id.as_str()),
    ] {
        validate_id(label, value)?;
    }
    if output.schema_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "output schema version must be positive".to_string(),
        ));
    }
    if let Some(reference) = &output.artifact_reference {
        validate_id("artifact_reference", reference)?;
    }
    Ok(())
}

fn validate_json_schema(
    label: &str,
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<(), WorkflowStoreError> {
    let validator = jsonschema::validator_for(schema).map_err(|error| {
        WorkflowStoreError::InvalidData(format!("invalid {label} schema: {error}"))
    })?;
    if let Err(error) = validator.validate(value) {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} does not match schema: {error}"
        )));
    }
    Ok(())
}

fn validate_output_against_node_schema(
    transaction: &dyn switchy_database::Database,
    output: &ValidatedOutput,
) -> Result<(), WorkflowStoreError> {
    let node = run_graph::bound_activation_node(
        transaction,
        &output.run_id,
        &output.node_id,
        &output.activation_id,
    )?
    .ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "validated output references missing run-graph node: {}",
            output.node_id
        ))
    })?;
    validate_output_schema(output, &node.output)
}

fn validate_output_schema(
    output: &ValidatedOutput,
    schema: &bcode_workflow::ValueSchema,
) -> Result<(), WorkflowStoreError> {
    if output.schema_id != schema.type_name {
        return Err(WorkflowStoreError::InvalidData(format!(
            "validated output schema identity mismatch for node {}: expected {}, received {}",
            output.node_id, schema.type_name, output.schema_id
        )));
    }
    let validator = jsonschema::validator_for(&schema.schema).map_err(|error| {
        WorkflowStoreError::InvalidData(format!(
            "invalid stored output schema for node {}: {error}",
            output.node_id
        ))
    })?;
    if let Err(error) = validator.validate(&output.value) {
        return Err(WorkflowStoreError::InvalidData(format!(
            "validated output does not match node {} schema: {error}",
            output.node_id
        )));
    }
    Ok(())
}

fn automatic_retry_schedule(
    db: &dyn switchy_database::Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<AutomaticRetrySchedule>, WorkflowStoreError> {
    database::rows(
        db,
        &select("workflow_retry_schedules")
            .projections(
                [
                    "failed_attempt",
                    "next_attempt",
                    "failure_kind",
                    "backoff_ms",
                    "next_attempt_at_ms",
                    "scheduled_at_ms",
                ]
                .into_iter()
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
            )
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .limit(1),
    )?
    .first()
    .map(|row| {
        let failed_attempt =
            u32::try_from(database::unsigned(row, "failed_attempt")?).map_err(|_| {
                WorkflowStoreError::InvalidData("retry failed attempt exceeds u32".into())
            })?;
        let next_attempt =
            u32::try_from(database::unsigned(row, "next_attempt")?).map_err(|_| {
                WorkflowStoreError::InvalidData("retry next attempt exceeds u32".into())
            })?;
        Ok(AutomaticRetrySchedule {
            run_id: run_id.into(),
            node_id: node_id.into(),
            activation_id: activation_id.into(),
            failed_attempt,
            next_attempt,
            failure_kind: serde_json::from_str(&database::text(row, "failure_kind")?)?,
            backoff_ms: database::unsigned(row, "backoff_ms")?,
            next_attempt_at_ms: database::unsigned(row, "next_attempt_at_ms")?,
            scheduled_at_ms: database::unsigned(row, "scheduled_at_ms")?,
        })
    })
    .transpose()
}

fn persist_definition_transaction(
    db: &dyn switchy_database::Database,
    stored: &StoredWorkflowDefinition,
) -> Result<(), WorkflowStoreError> {
    let rows = database::rows(
        db,
        &select("workflow_definitions")
            .projections(database::projections(&["checksum_sha256"]))
            .where_eq("definition_id", stored.definition_id.as_str())
            .where_eq("version", i64::from(stored.version))
            .limit(1),
    )?;
    if let Some(row) = rows.first() {
        if database::text(row, "checksum_sha256")? == stored.checksum_sha256 {
            return Ok(());
        }
        return Err(WorkflowStoreError::InvalidData(format!(
            "definition identity conflict: {} v{}",
            stored.definition_id, stored.version
        )));
    }
    crate::database::block_on(
        db.insert("workflow_definitions")
            .value("definition_id", stored.definition_id.as_str())
            .value("version", i64::from(stored.version))
            .value("checksum_sha256", stored.checksum_sha256.as_str())
            .value("definition_json", stored.definition_json.as_str())
            .execute(db),
    )?;
    Ok(())
}

fn validate_authored_workflow(workflow: &AuthoredWorkflow) -> Result<(), WorkflowStoreError> {
    validate_id("workflow_id", &workflow.workflow_id)?;
    if workflow.title.trim().is_empty() || workflow.title.len() > 256 {
        return Err(WorkflowStoreError::InvalidData(
            "authored workflow title must contain 1..=256 bytes".to_string(),
        ));
    }
    if workflow
        .description
        .as_ref()
        .is_some_and(|description| description.len() > 4_096)
        || workflow.updated_at_ms < workflow.created_at_ms
        || workflow.active_revision == Some(0)
    {
        return Err(WorkflowStoreError::InvalidData(
            "authored workflow metadata is invalid".to_string(),
        ));
    }
    Ok(())
}

fn validate_stored_authored_workflow(
    workflow: AuthoredWorkflow,
) -> Result<AuthoredWorkflow, WorkflowStoreError> {
    validate_authored_workflow(&workflow)?;
    Ok(workflow)
}

fn decode_authored_workflow(
    row: &switchy_database::Row,
) -> Result<AuthoredWorkflow, WorkflowStoreError> {
    let archived = database::unsigned(row, "archived")? != 0;
    Ok(AuthoredWorkflow {
        workflow_id: database::text(row, "workflow_id")?,
        title: database::text(row, "title")?,
        description: database::optional_text(row, "description")?,
        archived,
        active_revision: database::optional_unsigned(row, "active_revision")?,
        created_at_ms: database::unsigned(row, "created_at_ms")?,
        updated_at_ms: database::unsigned(row, "updated_at_ms")?,
    })
}

fn validate_authoring_document(
    workflow_id: &str,
    document: &WorkflowAuthoringDocument,
) -> Result<(), WorkflowStoreError> {
    document
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    if document.workflow_id != workflow_id {
        return Err(WorkflowStoreError::InvalidData(
            "authoring document workflow identity does not match durable owner".to_string(),
        ));
    }
    Ok(())
}

fn validate_workflow_draft(draft: &WorkflowDraft) -> Result<(), WorkflowStoreError> {
    validate_id("workflow_id", &draft.workflow_id)?;
    validate_id("draft_id", &draft.draft_id)?;
    validate_authoring_document(&draft.workflow_id, &draft.document)?;
    draft
        .producer
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    if draft.generation == 0
        || draft.base_revision == Some(0)
        || draft.updated_at_ms < draft.created_at_ms
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow draft generation, base revision, or timestamps are invalid".to_string(),
        ));
    }
    let checksum = draft
        .document
        .source_digest_sha256()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    if checksum != draft.checksum_sha256 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow draft checksum does not match its document".to_string(),
        ));
    }
    Ok(())
}

fn validate_stored_workflow_draft(
    draft: WorkflowDraft,
) -> Result<WorkflowDraft, WorkflowStoreError> {
    validate_workflow_draft(&draft)?;
    Ok(draft)
}

fn decode_workflow_draft(row: &switchy_database::Row) -> Result<WorkflowDraft, WorkflowStoreError> {
    Ok(WorkflowDraft {
        workflow_id: database::text(row, "workflow_id")?,
        draft_id: database::text(row, "draft_id")?,
        base_revision: database::optional_unsigned(row, "base_revision")?,
        generation: database::unsigned(row, "generation")?,
        checksum_sha256: database::text(row, "checksum_sha256")?,
        document: serde_json::from_str(&database::text(row, "document_json")?)?,
        producer: serde_json::from_str(&database::text(row, "producer_json")?)?,
        created_at_ms: database::unsigned(row, "created_at_ms")?,
        updated_at_ms: database::unsigned(row, "updated_at_ms")?,
    })
}

fn decode_workflow_revision(
    row: &switchy_database::Row,
) -> Result<PublishedWorkflowRevision, WorkflowStoreError> {
    let workflow_id = database::text(row, "workflow_id")?;
    let definition_version = u32::try_from(database::unsigned(row, "definition_version")?)
        .map_err(|_| {
            WorkflowStoreError::InvalidData("authored definition version exceeds u32".into())
        })?;
    Ok(PublishedWorkflowRevision {
        workflow_id: workflow_id.clone(),
        revision: database::unsigned(row, "revision")?,
        source_checksum_sha256: database::text(row, "source_checksum_sha256")?,
        executable_source_checksum_sha256: database::text(
            row,
            "executable_source_checksum_sha256",
        )?,
        definition_identity: WorkflowDefinitionIdentity {
            kind: workflow_id,
            definition_id: database::text(row, "definition_id")?,
            definition_version,
        },
        document: serde_json::from_str(&database::text(row, "document_json")?)?,
        producer: serde_json::from_str(&database::text(row, "producer_json")?)?,
        published_at_ms: database::unsigned(row, "published_at_ms")?,
    })
}

fn validate_stored_workflow_revision(
    revision: PublishedWorkflowRevision,
) -> Result<PublishedWorkflowRevision, WorkflowStoreError> {
    validate_id("workflow_id", &revision.workflow_id)?;
    validate_authoring_document(&revision.workflow_id, &revision.document)?;
    revision
        .producer
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    if revision.revision == 0
        || revision.definition_identity.kind != revision.workflow_id
        || revision.source_checksum_sha256
            != revision
                .document
                .source_digest_sha256()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
        || revision.executable_source_checksum_sha256
            != revision
                .document
                .executable_source_digest_sha256()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
    {
        return Err(WorkflowStoreError::InvalidData(
            "published workflow revision identity or checksum is inconsistent".to_string(),
        ));
    }
    Ok(revision)
}

fn validate_workflow_preset(preset: &WorkflowPreset) -> Result<(), WorkflowStoreError> {
    validate_id("workflow_id", &preset.workflow_id)?;
    validate_id("preset_id", &preset.preset_id)?;
    if preset.revision == 0
        || preset.generation == 0
        || preset.name.trim().is_empty()
        || preset.name.len() > 256
        || preset.updated_at_ms < preset.created_at_ms
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow preset identity, generation, name, or timestamps are invalid".to_string(),
        ));
    }
    preset
        .producer
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    if let Some(limits) = &preset.run_limits
        && (limits.maximum_duration_ms == Some(0)
            || limits.node_execution_cap == 0
            || limits.node_execution_cap > i64::MAX as u64
            || limits.concurrency_cap == Some(0)
            || limits.cycle_cap == 0
            || limits.retry_cap == 0
            || limits
                .concurrency_cap
                .is_some_and(|cap| u64::from(cap) > limits.node_execution_cap))
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow preset run limits are invalid".to_string(),
        ));
    }
    bounded_json("preset configuration", &preset.configuration)?;
    validate_persistable_authoring_value("preset.configuration", &preset.configuration)
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    Ok(())
}

fn validate_stored_workflow_preset(
    preset: WorkflowPreset,
) -> Result<WorkflowPreset, WorkflowStoreError> {
    validate_workflow_preset(&preset)?;
    Ok(preset)
}

fn decode_workflow_preset(
    row: &switchy_database::Row,
) -> Result<WorkflowPreset, WorkflowStoreError> {
    Ok(WorkflowPreset {
        workflow_id: database::text(row, "workflow_id")?,
        preset_id: database::text(row, "preset_id")?,
        revision: database::unsigned(row, "revision")?,
        name: database::text(row, "name")?,
        generation: database::unsigned(row, "generation")?,
        configuration: serde_json::from_str(&database::text(row, "configuration_json")?)?,
        run_limits: database::optional_text(row, "run_limits_json")?
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        producer: serde_json::from_str(&database::text(row, "producer_json")?)?,
        created_at_ms: database::unsigned(row, "created_at_ms")?,
        updated_at_ms: database::unsigned(row, "updated_at_ms")?,
    })
}

/// Verify migrated canonical state before committing schema changes.
fn verify_migration_integrity(
    db: &dyn switchy_database::Database,
) -> Result<(), WorkflowStoreError> {
    let integrity = crate::database::block_on(db.sqlite_integrity_check())?;
    if integrity.len() != 1 || database::text(&integrity[0], "integrity_check")? != "ok" {
        return Err(WorkflowStoreError::InvalidData(
            "workflow migration integrity verification failed".into(),
        ));
    }
    if !crate::database::block_on(db.sqlite_foreign_key_check())?.is_empty() {
        return Err(WorkflowStoreError::InvalidData("workflow migration foreign-key verification failed; canonical relationships are inconsistent".into()));
    }
    Ok(())
}

fn migrate_run_package_bindings(
    db: &dyn switchy_database::Database,
) -> Result<(), WorkflowStoreError> {
    if !crate::database::block_on(db.table_exists("workflow_run_packages"))? {
        if !database::rows(
            db,
            &select("workflow_package_publications")
                .projections(database::projections(&["package_id"]))
                .limit(1),
        )?
        .is_empty()
        {
            return Err(WorkflowStoreError::InvalidData("missing workflow run package bindings with existing publications require explicit maintenance".into()));
        }
        let mut table = create_table("workflow_run_packages");
        for name in ["run_id", "package_id", "lock_digest"] {
            table = table.column(Column {
                name: name.into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: None,
            });
        }
        crate::database::block_on(
            table
                .primary_key("run_id")
                .foreign_key_column("run_id", "workflow_runs", "run_id")
                .composite_foreign_key(
                    ["package_id", "lock_digest"],
                    "workflow_package_publications",
                    ["package_id", "package_lock_digest_sha256"],
                )
                .execute(db),
        )?;
    }
    crate::database::block_on(
        db.validate_select(
            &select("workflow_run_packages")
                .projections(database::projections(&[
                    "run_id",
                    "package_id",
                    "lock_digest",
                ]))
                .limit(0),
        ),
    )?;
    Ok(())
}

fn migrate_optional_concurrency_in(
    db: &dyn switchy_database::Database,
) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(
        db.sqlite_relax_integer_nullability("workflow_runs", "concurrency_cap"),
    )?;
    Ok(())
}

fn migrate_recursion_policy(db: &dyn switchy_database::Database) -> Result<(), WorkflowStoreError> {
    migrate_optional_concurrency_in(db)?;
    let columns = crate::database::block_on(db.get_table_columns("workflow_runs"))?;
    if !columns
        .iter()
        .any(|column| column.name == "recursion_depth_cap")
    {
        crate::database::block_on(
            alter_table("workflow_runs")
                .add_column(
                    "recursion_depth_cap".into(),
                    DataType::Int,
                    false,
                    Some(switchy_database::DatabaseValue::Int64(8)),
                )
                .add_column(
                    "descendant_cap".into(),
                    DataType::Int,
                    false,
                    Some(switchy_database::DatabaseValue::Int64(64)),
                )
                .execute(db),
        )?;
    }
    crate::database::block_on(
        db.exec_update_count(
            &(update("workflow_storage_compatibility")
                .value("compatibility", i64::from(WORKFLOW_STORE_COMPATIBILITY))
                .where_eq("contract_id", 1_i64)),
        ),
    )?;
    Ok(())
}

fn initialize_compatibility_contract(
    db: &dyn switchy_database::Database,
) -> Result<(), WorkflowStoreError> {
    initialize_compatibility_contract_version(db, WORKFLOW_STORE_COMPATIBILITY)
}

fn initialize_compatibility_contract_version(
    db: &dyn switchy_database::Database,
    compatibility: u32,
) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(
        create_table("workflow_storage_compatibility")
            .if_not_exists(true)
            .column(Column {
                name: "contract_id".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .primary_key("contract_id")
            .check(E::column("contract_id").compare(C::Equal, E::Integer(1)))
            .column(Column {
                name: "compatibility".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .check(E::column("compatibility").compare(C::Greater, E::Integer(0)))
            .execute(db),
    )?;
    crate::database::block_on(
        db.insert("workflow_storage_compatibility")
            .value("contract_id", 1_i64)
            .value("compatibility", i64::from(compatibility))
            .on_conflict_do_nothing(["contract_id"])
            .execute_count(db),
    )?;
    Ok(())
}

// Missing or malformed contracts remain unsupported, but lock contention must retain
// its transient classification so a later initialization request can retry safely.
fn read_typed_contract_value(
    db: &dyn switchy_database::Database,
    table: &str,
    column: &str,
) -> Result<Option<u32>, WorkflowStoreError> {
    let columns = [column];
    match database::rows(
        db,
        &select(table)
            .columns(&columns)
            .where_eq("contract_id", 1_i64)
            .limit(1),
    ) {
        Ok(rows) => Ok(rows
            .first()
            .and_then(|row| database::unsigned(row, column).ok())
            .and_then(|value| u32::try_from(value).ok())),
        Err(error) if error.is_transient_initialization_failure() => Err(error),
        Err(_) => Ok(None),
    }
}

fn verify_store_schema(db: &dyn switchy_database::Database) -> Result<(), WorkflowStoreError> {
    storage_compatibility::verify(db)
}

fn verify_upgrade_recovery_source(
    db: &dyn switchy_database::Database,
    version: u32,
) -> Result<(), WorkflowStoreError> {
    if version >= 34 {
        recovery::verify_barriers(db)?;
    }
    if version >= 35 {
        crate::database::block_on(
            db.validate_select(
                &select("workflow_receipt_cursors")
                    .projections(database::projections(&[
                        "run_id",
                        "after_dispatch_identity",
                    ]))
                    .limit(0),
            ),
        )?;
    }
    if version >= 36 {
        crate::database::block_on(
            db.validate_select(
                &select("workflow_attempts")
                    .indexed_by("workflow_receipt_recovery_page")
                    .projections(database::projections(&["dispatch_identity"]))
                    .where_eq("run_id", "")
                    .where_gt("dispatch_identity", "")
                    .filter(Box::new(switchy_database::query::schema_predicate(
                        &recovery::receipt_predicate(),
                    )?))
                    .sort("dispatch_identity", SortDirection::Asc)
                    .limit(1),
            ),
        )?;
    }
    if version >= 37 {
        crate::database::block_on(
            db.validate_select(
                &select("workflow_discovery_cursors")
                    .projections(database::projections(&["artifact_id", "after_run_id"]))
                    .limit(0),
            ),
        )?;
    }
    Ok(())
}

fn validate_receipt_recovery_cursor(cursor: &str) -> Result<(), WorkflowStoreError> {
    if !cursor.is_empty()
        && (cursor.len() != 64 || !cursor.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow receipt recovery cursor is malformed; maintenance required".into(),
        ));
    }
    Ok(())
}

fn validate_id(label: &str, value: &str) -> Result<(), WorkflowStoreError> {
    if value.len() > MAX_ID_BYTES || value.trim().is_empty() {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} must contain 1..={MAX_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(
        String::with_capacity(digest.len() * 2),
        |mut encoded, byte| {
            write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
            encoded
        },
    )
}

#[cfg(test)]
mod tests;
