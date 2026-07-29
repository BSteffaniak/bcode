#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Durable workflow persistence owned independently from session transcript storage.

use bcode_workflow::WorkflowDefinition;
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use thiserror::Error;

const DATABASE_FILE: &str = "workflow.db";
const SCHEMA_VERSION: u32 = 6;
const MAX_ID_BYTES: usize = 512;
const MAX_DISPLAY_LABEL_BYTES: usize = 512;
const MAX_INLINE_JSON_BYTES: usize = 1_048_576;

/// Durable workflow run status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Run may produce new activations.
    Running,
    /// Run is paused for external input or explicit operator action.
    Paused,
    /// Run is terminal and successful.
    Completed,
    /// Run is terminal and failed.
    Failed,
    /// Run is terminal and cancelled.
    Cancelled,
    /// Run cannot continue automatically without explicit repair.
    RepairRequired,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl RunStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RepairRequired => "repair_required",
        }
    }
}

/// Side-effect classification persisted before external dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchSideEffect {
    /// Operation cannot mutate external state.
    ReadOnly,
    /// Operation may mutate external state and must never be blindly duplicated.
    Mutating,
}

impl DispatchSideEffect {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Mutating => "mutating",
        }
    }
}

/// Bounded run summary used by normal list/status paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunSummary {
    pub run_id: String,
    pub definition_id: String,
    pub definition_version: u32,
    pub workspace_snapshot: String,
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub binding: Option<WorkflowRunBinding>,
    pub status: RunStatus,
    pub cancellation_requested_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
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

/// Durable waiting-gate kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowWaitKind {
    Input,
    Approval,
}

impl WorkflowWaitKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Approval => "approval",
        }
    }
}

/// Bounded durable waiting activation summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingActivation {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub kind: WorkflowWaitKind,
    pub input: Option<serde_json::Value>,
    pub requested_at_ms: u64,
}

/// Bounded activation summary for workflow status and next-action projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowActivationSummary {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub dependency_generation: u64,
    pub status: String,
    pub has_output: bool,
    pub created_at_ms: u64,
}

/// Result of resolving one exact durable waiting activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingResolutionResult {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub outcome: String,
    pub activated: Vec<NewActivation>,
    pub run_status: RunStatus,
}

/// Keyset cursor for bounded attempt history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptCursor {
    pub prepared_at_ms: u64,
    pub dispatch_identity: String,
}

/// Bounded durable attempt summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptSummary {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub side_effect: DispatchSideEffect,
    pub status: String,
    pub has_receipt: bool,
    pub prepared_at_ms: u64,
    pub admitted_at_ms: Option<u64>,
    pub terminal_at_ms: Option<u64>,
}

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

/// Persisted workflow execution limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunLimits {
    /// Absolute wall-clock deadline, when configured.
    pub deadline_at_ms: Option<u64>,
    /// Maximum total node attempts in the run.
    pub node_execution_cap: u32,
    /// Maximum concurrently running nodes.
    pub concurrency_cap: u32,
    /// Maximum cycle/repeat activations.
    pub cycle_cap: u32,
    /// Maximum attempts per activation.
    pub retry_cap: u32,
}

impl Default for WorkflowRunLimits {
    fn default() -> Self {
        Self {
            deadline_at_ms: None,
            node_execution_cap: 1_000,
            concurrency_cap: 8,
            cycle_cap: 100,
            retry_cap: 3,
        }
    }
}

/// Bounded product ownership and discovery association for one workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunBinding {
    pub owner_plugin_id: String,
    pub workflow_kind: String,
    pub scope_key: String,
    pub display_label: Option<String>,
    pub single_active: bool,
}

/// Generic associated-run lookup key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunBindingKey {
    pub owner_plugin_id: String,
    pub workflow_kind: String,
    pub scope_key: String,
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
    /// Optional bounded product ownership and discovery association.
    #[serde(default)]
    pub binding: Option<WorkflowRunBinding>,
    /// Optional bounded initial input validated against the definition input schema.
    pub input: Option<serde_json::Value>,
    /// Creation timestamp supplied by the host clock.
    pub created_at_ms: u64,
    /// Persisted execution limits enforced by durable admission.
    pub limits: WorkflowRunLimits,
}

/// One durable node activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewActivation {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub dependency_generation: u64,
    /// Optional schema-validated activation input. Entry activations inherit the run input;
    /// downstream activations inherit the predecessor output or a controller-derived envelope.
    pub input: Option<serde_json::Value>,
    pub created_at_ms: u64,
}

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

/// One explicit durable workflow decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDecision {
    pub decision_id: String,
    pub run_id: String,
    pub node_id: Option<String>,
    pub decision_type: String,
    pub value: serde_json::Value,
    pub created_at_ms: u64,
}

/// One bounded durable workflow grant record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGrant {
    pub grant_id: String,
    pub run_id: String,
    pub node_id: String,
    pub scope: serde_json::Value,
    pub granted_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

/// One exact pending workflow mutation approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMutationApproval {
    pub approval_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub scope: bcode_workflow::WorkflowMutationGrantScope,
    pub requested_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

/// Terminal decision applied to one exact pending mutation approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMutationApprovalDecision {
    Approve,
    Deny,
}

/// Result of atomically resolving one mutation approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMutationApprovalResolution {
    pub approval_id: String,
    pub status: String,
    pub grant_id: Option<String>,
    pub activation_status: String,
}

/// Access mode for one durable workflow resource lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLeaseMode {
    Read,
    Write,
}

impl ResourceLeaseMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// One durable workflow resource lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowResourceLease {
    pub lease_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub resource_key: String,
    pub mode: ResourceLeaseMode,
    pub acquired_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

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
    /// External work completed with schema-validated output.
    Succeeded { output: ValidatedOutput },
    /// External work failed terminally.
    Failed { message: String },
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
    pub succeeded: Vec<String>,
    pub failed: Vec<String>,
    pub paused: Vec<String>,
    pub cancelled: Vec<String>,
    pub repair_required: Vec<String>,
    pub unresolved_read_only: Vec<String>,
}

/// One bounded inconsistency found by an explicit workflow doctor operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "issue", rename_all = "snake_case")]
pub enum WorkflowDoctorIssue {
    /// A run and its repair-required attempts disagree about whether repair is needed.
    RepairStatusMismatch {
        run_status: RunStatus,
        repair_required_attempts: u64,
    },
    /// A persisted workflow grant is expired or its scope/row identity is inconsistent.
    InvalidGrant { grant_id: String, reason: String },
    /// An active external attempt has no receipt proving accepted owner identity.
    OrphanedAttempt {
        dispatch_identity: String,
        status: String,
        side_effect: DispatchSideEffect,
        guidance: String,
    },
    /// A completed activation has no matching validated output, or a non-completed activation
    /// references one.
    ActivationOutputMismatch {
        node_id: String,
        activation_id: String,
        activation_status: String,
        output_id: Option<String>,
    },
    /// Persisted attempt identity does not match its stable identity components.
    AttemptIdentityMismatch {
        dispatch_identity: String,
        expected_dispatch_identity: String,
    },
}

/// Bounded, non-mutating result of an explicit workflow doctor operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDoctorReport {
    pub run_id: String,
    pub issues: Vec<WorkflowDoctorIssue>,
    /// The requested bound prevented a complete inspection, so additional issues may exist.
    pub truncated: bool,
}

/// Explicit operator resolution for one ambiguous mutating attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resolution", rename_all = "snake_case")]
pub enum RepairResolution {
    /// The owner or operator proved the operation succeeded and supplies its validated output.
    ConfirmSucceeded { output: ValidatedOutput },
    /// The owner or operator proved the operation failed terminally.
    ConfirmFailed { message: String },
    /// The owner or operator proved the operation was cancelled.
    ConfirmCancelled { message: String },
    /// Explicitly abandon the ambiguous operation and permit a later, higher-numbered attempt.
    ///
    /// This is the only repair resolution that allows retry after an ambiguous mutation. It does
    /// not dispatch work itself.
    AbandonForExplicitRetry { reason: String },
}

/// Result of an explicit repair operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairResult {
    pub dispatch_identity: String,
    pub attempt_status: String,
    pub run_status: RunStatus,
}

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
}

/// Host owner boundary for executable durable workflow activations.
pub trait ActivationDispatchOwner: Sync {
    /// Build the exact bounded intent that must be committed before external dispatch.
    ///
    /// Returning `None` leaves an unsupported activation pending for another owner.
    ///
    /// # Errors
    ///
    /// Returns an error when the node contract is malformed or policy admission fails.
    fn plan(
        &self,
        activation: &PendingActivation,
    ) -> Result<Option<ActivationDispatchPlan>, WorkflowStoreError>;

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

/// Result of an explicit operator retry transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeRetryResult {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub previous_attempt: u32,
    pub next_attempt: u32,
}

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

/// Bounded persisted output summary for workflow inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowOutputSummary {
    pub output_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub artifact_reference: Option<String>,
    pub checksum_sha256: String,
    pub created_at_ms: u64,
}

/// Durable validated output persisted before downstream activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedOutput {
    pub output_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub value: serde_json::Value,
    pub artifact_reference: Option<String>,
    pub created_at_ms: u64,
}

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
    /// Database operation failed.
    #[error("workflow database error: {0}")]
    Database(#[from] rusqlite::Error),
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
    /// A durable cancellation request prevents further lifecycle changes.
    #[error("workflow cancellation prevents run state changes")]
    CancellationPreventsControl,
    /// A computed successor value did not match the exact target input contract.
    #[error("{0}")]
    TargetInputValidation(Box<TargetInputValidationFailure>),
    /// Persisted data violated the storage contract.
    #[error("invalid workflow store data: {0}")]
    InvalidData(String),
}

/// Canonical persisted definition identity and content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredWorkflowDefinition {
    /// Stable definition identity.
    pub definition_id: String,
    /// Positive definition version.
    pub version: u32,
    /// SHA-256 of canonical serialized definition JSON.
    pub checksum_sha256: String,
    /// Canonical serialized definition.
    pub definition_json: String,
}

/// Durable workflow database.
#[derive(Debug)]
pub struct WorkflowStore {
    path: PathBuf,
    connection: Connection,
}

impl WorkflowStore {
    /// Open or create the canonical workflow database below an explicit Bcode state directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or migrations fail.
    pub fn open_in_state_dir(state_dir: &Path) -> Result<Self, WorkflowStoreError> {
        Self::open_at(&workflow_database_path(state_dir))
    }

    /// Open an explicit workflow database path.
    ///
    /// This is used by host cancellation handles that must reopen the same canonical database
    /// without borrowing a live connection across an async boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or migrations fail.
    pub fn open_at_path(path: &Path) -> Result<Self, WorkflowStoreError> {
        Self::open_at(path)
    }

    /// Open the production-default workflow database.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or migrations fail.
    pub fn open_default() -> Result<Self, WorkflowStoreError> {
        Self::open_in_state_dir(&bcode_config::default_state_dir())
    }

    fn open_at(path: &Path) -> Result<Self, WorkflowStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut connection)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection,
        })
    }

    /// Return the canonical database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Persist an immutable normalized definition and checksum.
    ///
    /// The compiled definition is structurally validated before serialization. Re-persisting
    /// byte-identical content is idempotent. Reusing one definition/version for different content
    /// fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/version, serialization failure, checksum conflict, or
    /// database failure.
    pub fn persist_definition(
        &mut self,
        definition_id: &str,
        version: u32,
        definition: &WorkflowDefinition,
    ) -> Result<StoredWorkflowDefinition, WorkflowStoreError> {
        validate_id("definition_id", definition_id)?;
        definition.validate().map_err(|error| {
            WorkflowStoreError::InvalidData(format!("invalid workflow definition: {error}"))
        })?;
        if version == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "definition version must be positive".to_string(),
            ));
        }
        let definition_json = serde_json::to_string(definition)?;
        if definition_json.len() > MAX_INLINE_JSON_BYTES {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow definition exceeds {MAX_INLINE_JSON_BYTES} bytes"
            )));
        }
        let checksum_sha256 = sha256_hex(definition_json.as_bytes());
        let stored = StoredWorkflowDefinition {
            definition_id: definition_id.to_string(),
            version,
            checksum_sha256,
            definition_json,
        };
        let transaction = self.connection.transaction()?;
        persist_definition_transaction(&transaction, &stored)?;
        transaction.commit()?;
        Ok(stored)
    }

    /// Return a bounded definition list ordered by identity and newest version first.
    ///
    /// Each row is checksum-verified before it is returned. This normal discovery path performs no
    /// replay, repair, or external work.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, a checksum is inconsistent, or the bounded query
    /// fails.
    pub fn list_definitions(
        &self,
        limit: usize,
    ) -> Result<Vec<StoredWorkflowDefinition>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT definition_id, version, checksum_sha256, definition_json \
             FROM workflow_definitions ORDER BY definition_id, version DESC LIMIT ?1",
        )?;
        statement
            .query_map([limit], |row| {
                Ok(StoredWorkflowDefinition {
                    definition_id: row.get(0)?,
                    version: row.get(1)?,
                    checksum_sha256: row.get(2)?,
                    definition_json: row.get(3)?,
                })
            })?
            .map(|row| {
                let stored = row?;
                verify_stored_definition(stored)
            })
            .collect()
    }

    /// Load one exact definition version with checksum verification.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails or persisted content does not match its checksum.
    pub fn definition(
        &self,
        definition_id: &str,
        version: u32,
    ) -> Result<Option<StoredWorkflowDefinition>, WorkflowStoreError> {
        let stored = self
            .connection
            .query_row(
                "SELECT definition_id, version, checksum_sha256, definition_json \
                 FROM workflow_definitions WHERE definition_id = ?1 AND version = ?2",
                (definition_id, version),
                |row| {
                    Ok(StoredWorkflowDefinition {
                        definition_id: row.get(0)?,
                        version: row.get(1)?,
                        checksum_sha256: row.get(2)?,
                        definition_json: row.get(3)?,
                    })
                },
            )
            .optional()?;
        stored.map(verify_stored_definition).transpose()
    }

    /// Idempotently create one durable workflow run using a caller-stable identity.
    ///
    /// Returns `true` when the run was created and `false` when the exact immutable request was
    /// already present. Identity reuse with different definition, snapshot, parent, input, or
    /// limits fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed input, missing definition, conflicting identity, malformed
    /// stored input, or database failure.
    pub fn create_run_idempotent(
        &mut self,
        run: &NewWorkflowRun,
    ) -> Result<bool, WorkflowStoreError> {
        validate_run(run)?;
        let existing = self
            .connection
            .query_row(
                "SELECT definition_id, definition_version, workspace_snapshot, parent_session_id, \
                 input_json, deadline_at_ms, node_execution_cap, concurrency_cap, cycle_cap, retry_cap, \
                 owner_plugin_id, workflow_kind, scope_key, display_label, single_active \
                 FROM workflow_runs WHERE run_id = ?1",
                [&run.run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<u64>>(5)?,
                        row.get::<_, u32>(6)?,
                        row.get::<_, u32>(7)?,
                        row.get::<_, u32>(8)?,
                        row.get::<_, u32>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<String>>(13)?,
                        row.get::<_, bool>(14)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            definition_id,
            definition_version,
            workspace_snapshot,
            parent_session_id,
            input_json,
            deadline_at_ms,
            node_execution_cap,
            concurrency_cap,
            cycle_cap,
            retry_cap,
            owner_plugin_id,
            workflow_kind,
            scope_key,
            display_label,
            single_active,
        )) = existing
        else {
            self.create_run(run)?;
            return Ok(true);
        };
        let input = input_json
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
        let limits = WorkflowRunLimits {
            deadline_at_ms,
            node_execution_cap,
            concurrency_cap,
            cycle_cap,
            retry_cap,
        };
        let existing_binding = match (owner_plugin_id, workflow_kind, scope_key) {
            (Some(owner_plugin_id), Some(workflow_kind), Some(scope_key)) => {
                Some(WorkflowRunBinding {
                    owner_plugin_id,
                    workflow_kind,
                    scope_key,
                    display_label,
                    single_active,
                })
            }
            (None, None, None) => None,
            _ => {
                return Err(WorkflowStoreError::InvalidData(
                    "stored workflow run has an incomplete binding".to_string(),
                ));
            }
        };
        if definition_id == run.definition_id
            && definition_version == run.definition_version
            && workspace_snapshot == run.workspace_snapshot
            && parent_session_id == run.parent_session_id
            && existing_binding == run.binding
            && input == run.input
            && limits == run.limits
        {
            return Ok(false);
        }
        Err(WorkflowStoreError::InvalidData(format!(
            "workflow run identity conflicts with an existing run: {}",
            run.run_id
        )))
    }

    /// Create one durable workflow run bound to an existing exact definition version.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing definition, identity conflict, or database
    /// failure.
    #[allow(clippy::too_many_lines)]
    pub fn create_run(&mut self, run: &NewWorkflowRun) -> Result<(), WorkflowStoreError> {
        validate_run(run)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(binding) = &run.binding
            && binding.single_active
        {
            let active: Option<String> = transaction
                .query_row(
                    "SELECT run_id FROM workflow_runs WHERE owner_plugin_id = ?1 \
                     AND workflow_kind = ?2 AND scope_key = ?3 \
                     AND status IN ('running', 'paused', 'repair_required') \
                     ORDER BY updated_at_ms DESC, run_id LIMIT 1",
                    (
                        &binding.owner_plugin_id,
                        &binding.workflow_kind,
                        &binding.scope_key,
                    ),
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(active) = active {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow binding already has an active run: {active}"
                )));
            }
        }
        let definition_json = transaction
            .query_row(
                "SELECT definition_json FROM workflow_definitions \
                 WHERE definition_id = ?1 AND version = ?2",
                (&run.definition_id, run.definition_version),
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(definition_json) = definition_json else {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow definition not found: {} v{}",
                run.definition_id, run.definition_version
            )));
        };
        let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
        let input_json = run
            .input
            .as_ref()
            .map(|input| validate_run_input(&definition, input))
            .transpose()?;
        let (owner_plugin_id, workflow_kind, scope_key, display_label, single_active) = run
            .binding
            .as_ref()
            .map_or((None, None, None, None, false), |binding| {
                (
                    Some(binding.owner_plugin_id.as_str()),
                    Some(binding.workflow_kind.as_str()),
                    Some(binding.scope_key.as_str()),
                    binding.display_label.as_deref(),
                    binding.single_active,
                )
            });
        transaction.execute(
            "INSERT INTO workflow_runs \
             (run_id, definition_id, definition_version, workspace_snapshot, parent_session_id, \
              owner_plugin_id, workflow_kind, scope_key, display_label, single_active, \
              input_json, status, deadline_at_ms, node_execution_cap, concurrency_cap, cycle_cap, retry_cap, \
              created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?18)",
            rusqlite::params![
                &run.run_id,
                &run.definition_id,
                run.definition_version,
                &run.workspace_snapshot,
                &run.parent_session_id,
                owner_plugin_id,
                workflow_kind,
                scope_key,
                display_label,
                single_active,
                &input_json,
                RunStatus::Running.as_str(),
                run.limits.deadline_at_ms,
                run.limits.node_execution_cap,
                run.limits.concurrency_cap,
                run.limits.cycle_cap,
                run.limits.retry_cap,
                run.created_at_ms,
            ],
        )?;
        append_event(
            &transaction,
            &run.run_id,
            "run_created",
            &serde_json::to_string(run)?,
            run.created_at_ms,
        )?;
        if definition.entries.is_empty() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow definition has no entry nodes".to_string(),
            ));
        }
        if definition.entries.len()
            > usize::try_from(run.limits.concurrency_cap).unwrap_or(usize::MAX)
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow entry set exceeds run concurrency cap".to_string(),
            ));
        }
        for node_id in &definition.entries {
            let node = definition.nodes.get(node_id).ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow entry node is missing: {node_id}"
                ))
            })?;
            let entry_input = run.input.clone().unwrap_or(serde_json::Value::Null);
            validate_json_schema(
                &format!("workflow entry input for node {node_id}"),
                &node.input.schema,
                &entry_input,
            )?;
            let activation = NewActivation {
                run_id: run.run_id.clone(),
                node_id: node_id.clone(),
                activation_id: activation_identity(&run.run_id, node_id, 0),
                dependency_generation: 0,
                input: Some(entry_input),
                created_at_ms: run.created_at_ms,
            };
            validate_activation(&activation)?;
            insert_activation_with_status(
                &transaction,
                &activation,
                activation_status_for_node(node),
            )?;
        }
        transaction.commit()?;
        Ok(())
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
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT output_id, run_id, node_id, activation_id, schema_id, schema_version, \
             artifact_reference, checksum_sha256, created_at_ms FROM workflow_outputs \
             WHERE run_id = ?1 ORDER BY created_at_ms, output_id LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok(WorkflowOutputSummary {
                    output_id: row.get(0)?,
                    run_id: row.get(1)?,
                    node_id: row.get(2)?,
                    activation_id: row.get(3)?,
                    schema_id: row.get(4)?,
                    schema_version: row.get(5)?,
                    artifact_reference: row.get(6)?,
                    checksum_sha256: row.get(7)?,
                    created_at_ms: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(WorkflowStoreError::from)
    }

    /// Return bounded grants for one run ordered by grant time and identity.
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
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT grant_id, run_id, node_id, scope_json, granted_at_ms, expires_at_ms \
             FROM workflow_grants WHERE run_id = ?1 ORDER BY granted_at_ms, grant_id LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, Option<u64>>(5)?,
                ))
            })?
            .map(|row| {
                let (grant_id, run_id, node_id, scope_json, granted_at_ms, expires_at_ms) = row?;
                Ok(WorkflowGrant {
                    grant_id,
                    run_id,
                    node_id,
                    scope: serde_json::from_str(&scope_json)?,
                    granted_at_ms,
                    expires_at_ms,
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
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT lease_id, run_id, node_id, activation_id, resource_key, mode, \
             acquired_at_ms, expires_at_ms FROM workflow_resource_leases \
             WHERE run_id = ?1 AND released_at_ms IS NULL ORDER BY acquired_at_ms, lease_id LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, u64>(6)?,
                    row.get::<_, Option<u64>>(7)?,
                ))
            })?
            .map(|row| {
                let (
                    lease_id,
                    run_id,
                    node_id,
                    activation_id,
                    resource_key,
                    mode,
                    acquired_at_ms,
                    expires_at_ms,
                ) = row?;
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
    /// Returns an error for malformed identity, missing run, duplicate activation, or database
    /// failure.
    pub fn create_activation(
        &mut self,
        activation: &NewActivation,
    ) -> Result<(), WorkflowStoreError> {
        validate_activation(activation)?;
        enforce_activation_limits(&self.connection, activation)?;
        let transaction = self.connection.transaction()?;
        insert_activation(&transaction, activation)?;
        transaction.commit()?;
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
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO workflow_decisions \
             (decision_id, run_id, node_id, decision_type, value_json, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                &decision.decision_id,
                &decision.run_id,
                &decision.node_id,
                &decision.decision_type,
                &value_json,
                decision.created_at_ms,
            ),
        )?;
        if changed == 0 {
            let existing = transaction.query_row(
                "SELECT run_id, node_id, decision_type, value_json, created_at_ms \
                 FROM workflow_decisions WHERE decision_id = ?1",
                [&decision.decision_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                    ))
                },
            )?;
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
                &transaction,
                &decision.run_id,
                "decision_recorded",
                &serde_json::to_string(decision)?,
                decision.created_at_ms,
            )?;
        }
        transaction.commit()?;
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
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT decision_id, node_id, decision_type, value_json, created_at_ms \
             FROM workflow_decisions WHERE run_id = ?1 \
             ORDER BY created_at_ms DESC, decision_id DESC LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            })?
            .map(|row| {
                let (decision_id, node_id, decision_type, value_json, created_at_ms) = row?;
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
        self.connection
            .query_row(
                "SELECT run_id, node_id, decision_type, value_json, created_at_ms \
                 FROM workflow_decisions WHERE decision_id = ?1",
                [decision_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(run_id, node_id, decision_type, value_json, created_at_ms)| {
                    Ok(WorkflowDecision {
                        decision_id: decision_id.to_string(),
                        run_id,
                        node_id,
                        decision_type,
                        value: serde_json::from_str(&value_json)?,
                        created_at_ms,
                    })
                },
            )
            .transpose()
    }

    /// Persist an immutable bounded workflow permission grant.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, oversized scope, missing run/node, identity
    /// conflict, or database failure.
    pub fn persist_grant(&mut self, grant: &WorkflowGrant) -> Result<(), WorkflowStoreError> {
        validate_grant(grant)?;
        let scope_json = bounded_json("workflow grant scope", &grant.scope)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO workflow_grants \
             (grant_id, run_id, node_id, scope_json, granted_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                &grant.grant_id,
                &grant.run_id,
                &grant.node_id,
                &scope_json,
                grant.granted_at_ms,
                grant.expires_at_ms,
            ),
        )?;
        if changed == 0 {
            let existing = transaction.query_row(
                "SELECT run_id, node_id, scope_json, granted_at_ms, expires_at_ms \
                 FROM workflow_grants WHERE grant_id = ?1",
                [&grant.grant_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                    ))
                },
            )?;
            if existing
                != (
                    grant.run_id.clone(),
                    grant.node_id.clone(),
                    scope_json,
                    grant.granted_at_ms,
                    grant.expires_at_ms,
                )
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow grant identity conflict: {}",
                    grant.grant_id
                )));
            }
        } else {
            append_event(
                &transaction,
                &grant.run_id,
                "grant_recorded",
                &serde_json::to_string(grant)?,
                grant.granted_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Persist one exact mutation approval wait before a mutating activation becomes dispatchable.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/scope, stale input, conflicting requests, or storage
    /// failure. No prepared attempt or external dispatch is created by this operation.
    pub fn request_mutation_approval(
        &mut self,
        approval: &WorkflowMutationApproval,
    ) -> Result<(), WorkflowStoreError> {
        validate_mutation_approval(approval)?;
        let scope_json = bounded_json("workflow mutation approval scope", &approval.scope)?;
        let transaction = self.connection.transaction()?;
        let (status, definition_id, definition_version, workspace_snapshot, input_json) =
            transaction.query_row(
                "SELECT activation.status, run.definition_id, run.definition_version, \
                 run.workspace_snapshot, activation.input_json \
                 FROM workflow_activations activation \
                 JOIN workflow_runs run ON run.run_id = activation.run_id \
                 WHERE activation.run_id = ?1 AND activation.node_id = ?2 \
                   AND activation.activation_id = ?3",
                (&approval.run_id, &approval.node_id, &approval.activation_id),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )?;
        if !matches!(status.as_str(), "pending" | "waiting_mutation_approval")
            || definition_id != approval.scope.definition_id
            || definition_version != approval.scope.definition_version
            || workspace_snapshot != approval.scope.workspace_snapshot
        {
            return Err(WorkflowStoreError::InvalidData(
                "mutation approval does not match dispatchable activation identity".to_string(),
            ));
        }
        let input = input_json
            .as_deref()
            .map_or_else(|| "null".to_string(), ToString::to_string);
        if sha256_hex(input.as_bytes()) != approval.scope.input_checksum_sha256 {
            return Err(WorkflowStoreError::InvalidData(
                "mutation approval input checksum is stale".to_string(),
            ));
        }
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO workflow_mutation_approvals \
             (approval_id, run_id, node_id, activation_id, scope_json, status, requested_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
            (
                &approval.approval_id,
                &approval.run_id,
                &approval.node_id,
                &approval.activation_id,
                &scope_json,
                approval.requested_at_ms,
                approval.expires_at_ms,
            ),
        )?;
        if changed == 0 {
            let existing: (String, String, String, String, String, u64, Option<u64>) =
                transaction.query_row(
                    "SELECT run_id, node_id, activation_id, scope_json, status, requested_at_ms, expires_at_ms \
                     FROM workflow_mutation_approvals WHERE approval_id = ?1",
                    [&approval.approval_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
                )?;
            if existing
                != (
                    approval.run_id.clone(),
                    approval.node_id.clone(),
                    approval.activation_id.clone(),
                    scope_json,
                    "pending".to_string(),
                    approval.requested_at_ms,
                    approval.expires_at_ms,
                )
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "mutation approval identity conflict: {}",
                    approval.approval_id
                )));
            }
        } else {
            transaction.execute(
                "UPDATE workflow_activations SET status = 'waiting_mutation_approval' \
                 WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND status = 'pending'",
                (&approval.run_id, &approval.node_id, &approval.activation_id),
            )?;
            append_event(
                &transaction,
                &approval.run_id,
                "mutation_approval_requested",
                &serde_json::to_string(approval)?,
                approval.requested_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Resolve one exact pending mutation approval in one transaction.
    ///
    /// Approval persists the exact grant before restoring dispatch eligibility. Denial records the
    /// decision and fails the activation/run without creating an attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for stale/expired/wrong identity, conflicting duplicate decisions, or
    /// storage failure. Any error rolls back decision, grant, and activation state together.
    #[allow(clippy::too_many_lines)]
    pub fn resolve_mutation_approval(
        &mut self,
        approval_id: &str,
        decision: WorkflowMutationApprovalDecision,
        resolved_at_ms: u64,
    ) -> Result<WorkflowMutationApprovalResolution, WorkflowStoreError> {
        validate_id("approval_id", approval_id)?;
        let transaction = self.connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT approval.run_id, approval.node_id, approval.activation_id, \
                 approval.scope_json, approval.status, approval.requested_at_ms, \
                 approval.expires_at_ms, approval.grant_id, activation.status, \
                 activation.input_json, run.definition_id, run.definition_version, \
                 run.workspace_snapshot, run.status, run.cancellation_requested_at_ms \
                 FROM workflow_mutation_approvals approval \
                 JOIN workflow_activations activation ON activation.run_id = approval.run_id \
                   AND activation.node_id = approval.node_id \
                   AND activation.activation_id = approval.activation_id \
                 JOIN workflow_runs run ON run.run_id = approval.run_id \
                 WHERE approval.approval_id = ?1",
                [approval_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, u64>(5)?,
                        row.get::<_, Option<u64>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, u32>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, String>(13)?,
                        row.get::<_, Option<u64>>(14)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "mutation approval not found: {approval_id}"
                ))
            })?;
        let (
            run_id,
            node_id,
            activation_id,
            scope_json,
            status,
            _requested_at_ms,
            expires_at_ms,
            existing_grant_id,
            activation_status,
            input_json,
            definition_id,
            definition_version,
            workspace_snapshot,
            run_status,
            cancellation_requested_at_ms,
        ) = row;
        let expected_status = match decision {
            WorkflowMutationApprovalDecision::Approve => "approved",
            WorkflowMutationApprovalDecision::Deny => "denied",
        };
        if status == expected_status {
            return Ok(WorkflowMutationApprovalResolution {
                approval_id: approval_id.to_string(),
                status,
                grant_id: existing_grant_id,
                activation_status,
            });
        }
        if status != "pending" || activation_status != "waiting_mutation_approval" {
            return Err(WorkflowStoreError::InvalidData(format!(
                "mutation approval decision conflict: {approval_id} is {status}"
            )));
        }
        if parse_run_status(&run_status)? != RunStatus::Running
            || cancellation_requested_at_ms.is_some()
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow run is not accepting mutation approval decisions".to_string(),
            ));
        }
        if expires_at_ms.is_some_and(|expires| resolved_at_ms >= expires) {
            transaction.execute(
                "UPDATE workflow_mutation_approvals SET status = 'expired', resolved_at_ms = ?2 \
                 WHERE approval_id = ?1 AND status = 'pending'",
                (approval_id, resolved_at_ms),
            )?;
            transaction.execute(
                "UPDATE workflow_activations SET status = 'failed' \
                 WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
                   AND status = 'waiting_mutation_approval'",
                (&run_id, &node_id, &activation_id),
            )?;
            transaction.execute(
                "UPDATE workflow_runs SET status = 'failed', updated_at_ms = ?2 \
                 WHERE run_id = ?1 AND status = 'running'",
                (&run_id, resolved_at_ms),
            )?;
            append_event(
                &transaction,
                &run_id,
                "mutation_approval_expired",
                &serde_json::json!({"approval_id": approval_id}).to_string(),
                resolved_at_ms,
            )?;
            transaction.commit()?;
            return Ok(WorkflowMutationApprovalResolution {
                approval_id: approval_id.to_string(),
                status: "expired".to_string(),
                grant_id: None,
                activation_status: "failed".to_string(),
            });
        }
        let scope: bcode_workflow::WorkflowMutationGrantScope = serde_json::from_str(&scope_json)?;
        scope
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let input = input_json.unwrap_or_else(|| "null".to_string());
        if scope.definition_id != definition_id
            || scope.definition_version != definition_version
            || scope.run_id != run_id
            || scope.node_id != node_id
            || scope.activation_id != activation_id
            || scope.workspace_snapshot != workspace_snapshot
            || sha256_hex(input.as_bytes()) != scope.input_checksum_sha256
        {
            return Err(WorkflowStoreError::InvalidData(
                "mutation approval scope became stale before decision".to_string(),
            ));
        }
        let decision_id = format!("mutation-approval:{approval_id}");
        let decision_value = serde_json::json!({
            "approval_id": approval_id,
            "decision": expected_status,
            "scope": scope,
        });
        transaction.execute(
            "INSERT INTO workflow_decisions \
             (decision_id, run_id, node_id, decision_type, value_json, created_at_ms) \
             VALUES (?1, ?2, ?3, 'mutation_approval', ?4, ?5)",
            (
                &decision_id,
                &run_id,
                &node_id,
                &serde_json::to_string(&decision_value)?,
                resolved_at_ms,
            ),
        )?;
        match decision {
            WorkflowMutationApprovalDecision::Approve => {
                let grant_id = format!("mutation-grant:{approval_id}");
                transaction.execute(
                    "INSERT INTO workflow_grants \
                     (grant_id, run_id, node_id, scope_json, granted_at_ms, expires_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    (
                        &grant_id,
                        &run_id,
                        &node_id,
                        &scope_json,
                        resolved_at_ms,
                        expires_at_ms,
                    ),
                )?;
                transaction.execute(
                    "UPDATE workflow_mutation_approvals SET status = 'approved', \
                     resolved_at_ms = ?2, grant_id = ?3 WHERE approval_id = ?1 AND status = 'pending'",
                    (approval_id, resolved_at_ms, &grant_id),
                )?;
                let changed = transaction.execute(
                    "UPDATE workflow_activations SET status = 'pending' \
                     WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
                       AND status = 'waiting_mutation_approval'",
                    (&run_id, &node_id, &activation_id),
                )?;
                if changed != 1 {
                    return Err(WorkflowStoreError::InvalidData(
                        "approved mutation activation is no longer waiting".to_string(),
                    ));
                }
                append_event(
                    &transaction,
                    &run_id,
                    "mutation_approval_approved",
                    &decision_value.to_string(),
                    resolved_at_ms,
                )?;
                transaction.commit()?;
                Ok(WorkflowMutationApprovalResolution {
                    approval_id: approval_id.to_string(),
                    status: "approved".to_string(),
                    grant_id: Some(grant_id),
                    activation_status: "pending".to_string(),
                })
            }
            WorkflowMutationApprovalDecision::Deny => {
                transaction.execute(
                    "UPDATE workflow_mutation_approvals SET status = 'denied', resolved_at_ms = ?2 \
                     WHERE approval_id = ?1 AND status = 'pending'",
                    (approval_id, resolved_at_ms),
                )?;
                transaction.execute(
                    "UPDATE workflow_activations SET status = 'failed' \
                     WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
                       AND status = 'waiting_mutation_approval'",
                    (&run_id, &node_id, &activation_id),
                )?;
                transaction.execute(
                    "UPDATE workflow_runs SET status = 'failed', updated_at_ms = ?2 \
                     WHERE run_id = ?1 AND status = 'running'",
                    (&run_id, resolved_at_ms),
                )?;
                append_event(
                    &transaction,
                    &run_id,
                    "mutation_approval_denied",
                    &decision_value.to_string(),
                    resolved_at_ms,
                )?;
                transaction.commit()?;
                Ok(WorkflowMutationApprovalResolution {
                    approval_id: approval_id.to_string(),
                    status: "denied".to_string(),
                    grant_id: None,
                    activation_status: "failed".to_string(),
                })
            }
        }
    }

    /// Load the request timestamp for one exact mutation approval without mutating it.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity or database failure.
    pub fn mutation_approval_requested_at(
        &self,
        approval_id: &str,
    ) -> Result<Option<u64>, WorkflowStoreError> {
        validate_id("approval_id", approval_id)?;
        self.connection
            .query_row(
                "SELECT requested_at_ms FROM workflow_mutation_approvals WHERE approval_id = ?1",
                [approval_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(WorkflowStoreError::from)
    }

    /// Load bounded pending mutation approval requests for one run.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/limit or malformed durable state.
    pub fn pending_mutation_approvals(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowMutationApproval>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT approval_id, node_id, activation_id, scope_json, requested_at_ms, expires_at_ms \
             FROM workflow_mutation_approvals WHERE run_id = ?1 AND status = 'pending' \
             ORDER BY requested_at_ms, approval_id LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, Option<u64>>(5)?,
                ))
            })?
            .map(|row| {
                let (
                    approval_id,
                    node_id,
                    activation_id,
                    scope_json,
                    requested_at_ms,
                    expires_at_ms,
                ) = row?;
                Ok(WorkflowMutationApproval {
                    approval_id,
                    run_id: run_id.to_string(),
                    node_id,
                    activation_id,
                    scope: serde_json::from_str(&scope_json)?,
                    requested_at_ms,
                    expires_at_ms,
                })
            })
            .collect()
    }

    /// Load one exact durable workflow grant.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid persisted JSON, or database failure.
    pub fn grant(&self, grant_id: &str) -> Result<Option<WorkflowGrant>, WorkflowStoreError> {
        validate_id("grant_id", grant_id)?;
        self.connection
            .query_row(
                "SELECT run_id, node_id, scope_json, granted_at_ms, expires_at_ms \
                 FROM workflow_grants WHERE grant_id = ?1",
                [grant_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(run_id, node_id, scope_json, granted_at_ms, expires_at_ms)| {
                    Ok(WorkflowGrant {
                        grant_id: grant_id.to_string(),
                        run_id,
                        node_id,
                        scope: serde_json::from_str(&scope_json)?,
                        granted_at_ms,
                        expires_at_ms,
                    })
                },
            )
            .transpose()
    }

    /// Acquire a durable workflow resource lease while enforcing reader/writer exclusion.
    ///
    /// Reacquiring the exact lease is idempotent. Conflicting active leases fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing activation, lease conflict, identity
    /// conflict, or database failure.
    pub fn acquire_resource_lease(
        &mut self,
        lease: &WorkflowResourceLease,
    ) -> Result<(), WorkflowStoreError> {
        validate_resource_lease(lease)?;
        let transaction = self.connection.transaction()?;
        let existing = resource_lease(&transaction, &lease.lease_id)?;
        if let Some((existing, released_at_ms)) = existing {
            if existing == *lease && released_at_ms.is_none() {
                return Ok(());
            }
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow resource lease identity conflict: {}",
                lease.lease_id
            )));
        }
        let conflict: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_resource_leases \
             WHERE run_id = ?1 AND resource_key = ?2 AND released_at_ms IS NULL \
             AND (expires_at_ms IS NULL OR expires_at_ms > ?3) \
             AND (?4 = 'write' OR mode = 'write'))",
            (
                &lease.run_id,
                &lease.resource_key,
                lease.acquired_at_ms,
                lease.mode.as_str(),
            ),
            |row| row.get(0),
        )?;
        if conflict {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow resource is already leased incompatibly: {}",
                lease.resource_key
            )));
        }
        transaction.execute(
            "INSERT INTO workflow_resource_leases \
             (lease_id, run_id, node_id, activation_id, resource_key, mode, acquired_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                &lease.lease_id,
                &lease.run_id,
                &lease.node_id,
                &lease.activation_id,
                &lease.resource_key,
                lease.mode.as_str(),
                lease.acquired_at_ms,
                lease.expires_at_ms,
            ),
        )?;
        append_event(
            &transaction,
            &lease.run_id,
            "resource_lease_acquired",
            &serde_json::to_string(lease)?,
            lease.acquired_at_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Release one exact durable resource lease.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/missing/already released lease or database failure.
    pub fn release_resource_lease(
        &mut self,
        lease_id: &str,
        released_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("lease_id", lease_id)?;
        let transaction = self.connection.transaction()?;
        let run_id = transaction
            .query_row(
                "SELECT run_id FROM workflow_resource_leases \
                 WHERE lease_id = ?1 AND released_at_ms IS NULL",
                [lease_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "active workflow resource lease not found: {lease_id}"
                ))
            })?;
        transaction.execute(
            "UPDATE workflow_resource_leases SET released_at_ms = ?2 WHERE lease_id = ?1",
            (lease_id, released_at_ms),
        )?;
        append_event(
            &transaction,
            &run_id,
            "resource_lease_released",
            &serde_json::json!({"lease_id": lease_id}).to_string(),
            released_at_ms,
        )?;
        transaction.commit()?;
        Ok(())
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
        let transaction = self.connection.transaction()?;
        let event_tail: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(event_seq), 0) FROM workflow_events",
            [],
            |row| row.get(0),
        )?;
        if checkpoint.last_event_seq > event_tail {
            return Err(WorkflowStoreError::InvalidData(format!(
                "projection checkpoint {} is beyond event tail {event_tail}",
                checkpoint.last_event_seq
            )));
        }
        let existing = transaction
            .query_row(
                "SELECT projection_version, last_event_seq FROM workflow_projection_checkpoints \
                 WHERE projection_name = ?1",
                [&checkpoint.projection_name],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u64>(1)?)),
            )
            .optional()?;
        if let Some((version, sequence)) = existing
            && (version != checkpoint.projection_version || sequence > checkpoint.last_event_seq)
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow projection checkpoint cannot regress or change version".to_string(),
            ));
        }
        transaction.execute(
            "INSERT INTO workflow_projection_checkpoints \
             (projection_name, projection_version, last_event_seq) VALUES (?1, ?2, ?3) \
             ON CONFLICT(projection_name) DO UPDATE SET last_event_seq = excluded.last_event_seq",
            (
                &checkpoint.projection_name,
                checkpoint.projection_version,
                checkpoint.last_event_seq,
            ),
        )?;
        transaction.commit()?;
        Ok(())
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
        self.connection
            .query_row(
                "SELECT projection_version, last_event_seq FROM workflow_projection_checkpoints \
                 WHERE projection_name = ?1",
                [projection_name],
                |row| {
                    Ok(WorkflowProjectionCheckpoint {
                        projection_name: projection_name.to_string(),
                        projection_version: row.get(0)?,
                        last_event_seq: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(WorkflowStoreError::from)
    }

    /// Acquire all declared resources for one activation before dispatch admission.
    ///
    /// Acquisitions use deterministic claim order and stable lease identities. Any conflict rolls
    /// back the complete set, so parallel branches never partially hold resources.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed claims, incompatible active leases, or database failure.
    fn acquire_activation_resources(
        &mut self,
        activation: &PendingActivation,
        acquired_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        let mut claims = activation.node.resources.clone();
        claims.sort_by(|left, right| left.resource.cmp(&right.resource));
        let transaction = self.connection.transaction()?;
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
            let existing = resource_lease(&transaction, &lease.lease_id)?;
            if let Some((existing, released_at_ms)) = existing {
                if existing == lease && released_at_ms.is_none() {
                    continue;
                }
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow resource lease identity conflict: {}",
                    lease.lease_id
                )));
            }
            let conflict: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_resource_leases \
                 WHERE run_id = ?1 AND resource_key = ?2 AND released_at_ms IS NULL \
                 AND (expires_at_ms IS NULL OR expires_at_ms > ?3) \
                 AND (?4 = 'write' OR mode = 'write'))",
                (
                    &lease.run_id,
                    &lease.resource_key,
                    lease.acquired_at_ms,
                    lease.mode.as_str(),
                ),
                |row| row.get(0),
            )?;
            if conflict {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow resource is already leased incompatibly: {}",
                    lease.resource_key
                )));
            }
            transaction.execute(
                "INSERT INTO workflow_resource_leases \
                 (lease_id, run_id, node_id, activation_id, resource_key, mode, acquired_at_ms, expires_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (
                    &lease.lease_id,
                    &lease.run_id,
                    &lease.node_id,
                    &lease.activation_id,
                    &lease.resource_key,
                    lease.mode.as_str(),
                    lease.acquired_at_ms,
                    lease.expires_at_ms,
                ),
            )?;
            append_event(
                &transaction,
                &lease.run_id,
                "resource_lease_acquired",
                &serde_json::to_string(&lease)?,
                acquired_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Admit and dispatch a bounded snapshot of pending executable activations.
    ///
    /// For each activation, owner planning happens before mutation, then activation reservation and
    /// intent persistence commit atomically before external dispatch. Owner receipts are persisted
    /// immediately after acceptance. A dispatch error leaves the attempt prepared for explicit
    /// reconciliation and stops the batch; it is never silently retried.
    ///
    /// # Errors
    ///
    /// Returns an error when pending discovery, owner planning/dispatch, admission, receipt
    /// persistence, or durable state access fails.
    pub async fn dispatch_pending_activations<O>(
        &mut self,
        owner: &O,
        limit: usize,
        dispatched_at_ms: u64,
    ) -> Result<ActivationDispatchSummary, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        self.dispatch_pending_activations_with_fault(
            owner,
            &NoopWorkflowDispatchFault,
            limit,
            dispatched_at_ms,
        )
        .await
    }

    /// Dispatch pending activations with deterministic post-boundary fault injection.
    ///
    /// # Errors
    ///
    /// Returns an error from discovery, planning, admission, dispatch, persistence, or the fault
    /// hook. Already committed boundaries remain durable when a later hook fails.
    pub async fn dispatch_pending_activations_with_fault<O, F>(
        &mut self,
        owner: &O,
        fault: &F,
        limit: usize,
        dispatched_at_ms: u64,
    ) -> Result<ActivationDispatchSummary, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
        F: WorkflowDispatchFault + ?Sized,
    {
        let pending = self.pending_activations(limit)?;
        let mut summary = ActivationDispatchSummary::default();
        for activation in pending {
            let Some(plan) = owner.plan(&activation)? else {
                summary.unsupported.push(activation.activation_id);
                continue;
            };
            if let Err(error) = self.acquire_activation_resources(&activation, dispatched_at_ms) {
                if error.to_string().contains("already leased incompatibly") {
                    summary.raced.push(activation.activation_id);
                    continue;
                }
                return Err(error);
            }
            let Some(prepared) = self.prepare_pending_activation(
                &activation.run_id,
                &activation.node_id,
                &activation.activation_id,
                plan.side_effect,
                plan.intent,
                dispatched_at_ms,
            )?
            else {
                summary.raced.push(activation.activation_id);
                continue;
            };
            fault.after_boundary(WorkflowDispatchBoundary::IntentCommitted, &prepared)?;
            let receipt = owner.dispatch(&prepared).await?;
            fault.after_boundary(WorkflowDispatchBoundary::OwnerAccepted, &prepared)?;
            self.persist_dispatch_receipt(&DispatchReceipt {
                run_id: prepared.activation.run_id.clone(),
                node_id: prepared.activation.node_id.clone(),
                activation_id: prepared.activation.activation_id.clone(),
                attempt: prepared.attempt,
                dispatch_identity: prepared.dispatch_identity.clone(),
                receipt,
                admitted_at_ms: dispatched_at_ms,
            })?;
            fault.after_boundary(WorkflowDispatchBoundary::ReceiptCommitted, &prepared)?;
            summary.admitted.push(prepared.dispatch_identity);
        }
        Ok(summary)
    }

    /// Atomically reserve one exact pending activation and persist its dispatch intent.
    ///
    /// This is the scheduler admission boundary: only one caller can transition a pending
    /// activation to running and create its next attempt. External dispatch must happen only after
    /// this method commits. Repeated admission returns `Ok(None)` without creating another attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, oversized intent, missing durable state, exhausted
    /// run limits, or database failure.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_pending_activation(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        side_effect: DispatchSideEffect,
        intent: serde_json::Value,
        prepared_at_ms: u64,
    ) -> Result<Option<PreparedActivationDispatch>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        let intent_json = bounded_json("dispatch intent", &intent)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let activation =
            pending_activation_by_identity(&transaction, run_id, node_id, activation_id)?;
        let Some(activation) = activation else {
            return Ok(None);
        };
        let attempt: u32 = transaction.query_row(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM workflow_attempts \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3",
            (run_id, node_id, activation_id),
            |row| row.get(0),
        )?;
        let prepared = PreparedAttempt {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            activation_id: activation_id.to_string(),
            attempt,
            side_effect,
            intent,
            prepared_at_ms,
        };
        enforce_attempt_limits(&transaction, &prepared)?;
        let dispatch_identity = prepared.dispatch_identity();
        let checksum = sha256_hex(intent_json.as_bytes());
        let changed = transaction.execute(
            "UPDATE workflow_activations SET status = 'running' \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND status = 'pending'",
            (run_id, node_id, activation_id),
        )?;
        if changed != 1 {
            return Ok(None);
        }
        transaction.execute(
            "INSERT INTO workflow_attempts \
             (run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, status, \
              intent_json, intent_checksum, prepared_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'prepared', ?7, ?8, ?9)",
            (
                run_id,
                node_id,
                activation_id,
                attempt,
                &dispatch_identity,
                side_effect.as_str(),
                &intent_json,
                &checksum,
                prepared_at_ms,
            ),
        )?;
        append_event(
            &transaction,
            run_id,
            "attempt_prepared",
            &serde_json::to_string(&prepared)?,
            prepared_at_ms,
        )?;
        transaction.commit()?;
        Ok(Some(PreparedActivationDispatch {
            activation,
            attempt,
            dispatch_identity,
            intent: prepared.intent,
        }))
    }

    /// Persist prepared intent before an external operation is dispatched.
    ///
    /// This operation is idempotent only for byte-equivalent intent at the same durable attempt
    /// identity. Conflicting intent fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, unbounded intent, missing activation, conflicting
    /// prepared intent, or database failure.
    pub fn prepare_attempt(
        &mut self,
        attempt: &PreparedAttempt,
    ) -> Result<String, WorkflowStoreError> {
        validate_prepared_attempt(attempt)?;
        enforce_attempt_limits(&self.connection, attempt)?;
        let identity = attempt.dispatch_identity();
        let intent_json = serde_json::to_string(&attempt.intent)?;
        if intent_json.len() > MAX_INLINE_JSON_BYTES {
            return Err(WorkflowStoreError::InvalidData(format!(
                "dispatch intent exceeds {MAX_INLINE_JSON_BYTES} bytes"
            )));
        }
        let checksum = sha256_hex(intent_json.as_bytes());
        let transaction = self.connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT dispatch_identity, intent_checksum FROM workflow_attempts \
                 WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND attempt = ?4",
                (
                    &attempt.run_id,
                    &attempt.node_id,
                    &attempt.activation_id,
                    attempt.attempt,
                ),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((existing_identity, existing_checksum)) = existing {
            if existing_identity == identity && existing_checksum == checksum {
                return Ok(identity);
            }
            return Err(WorkflowStoreError::InvalidData(format!(
                "prepared attempt identity conflict: {identity}"
            )));
        }
        transaction.execute(
            "INSERT INTO workflow_attempts \
             (run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, status, \
              intent_json, intent_checksum, prepared_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'prepared', ?7, ?8, ?9)",
            (
                &attempt.run_id,
                &attempt.node_id,
                &attempt.activation_id,
                attempt.attempt,
                &identity,
                attempt.side_effect.as_str(),
                &intent_json,
                &checksum,
                attempt.prepared_at_ms,
            ),
        )?;
        append_event(
            &transaction,
            &attempt.run_id,
            "attempt_prepared",
            &serde_json::to_string(attempt)?,
            attempt.prepared_at_ms,
        )?;
        transaction.commit()?;
        Ok(identity)
    }

    /// Persist an eligible automatic-retry schedule without sleeping or requeueing work.
    ///
    /// The exact next attempt and due time are durable and idempotent. Production capability
    /// advertisement remains disabled until a scheduler consumes this state safely.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, an ineligible policy decision, invalid/overflowing
    /// backoff, conflicting schedule identity, or database failure.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_automatic_retry(
        &mut self,
        eligibility: bcode_workflow::AutomaticRetryEligibility,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        failure_kind: bcode_workflow::AutomaticRetryFailureKind,
        backoff_ms: u64,
        scheduled_at_ms: u64,
    ) -> Result<AutomaticRetrySchedule, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        if eligibility.failure != failure_kind {
            return Err(WorkflowStoreError::InvalidData(
                "automatic retry failure kind does not match eligibility facts".to_string(),
            ));
        }
        let next_attempt = match bcode_workflow::automatic_retry_decision(eligibility) {
            bcode_workflow::AutomaticRetryDecision::Eligible { next_attempt } => next_attempt,
            bcode_workflow::AutomaticRetryDecision::Ineligible { reason } => {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "automatic retry is ineligible: {reason:?}"
                )));
            }
        };
        let next_attempt_at_ms = scheduled_at_ms.checked_add(backoff_ms).ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "automatic retry backoff timestamp overflow".to_string(),
            )
        })?;
        let schedule = AutomaticRetrySchedule {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            activation_id: activation_id.to_string(),
            failed_attempt: eligibility.attempts_completed,
            next_attempt,
            failure_kind,
            backoff_ms,
            next_attempt_at_ms,
            scheduled_at_ms,
        };
        let failure_json = serde_json::to_string(&schedule.failure_kind)?;
        let transaction = self.connection.transaction()?;
        let inserted = transaction.execute(
            "INSERT INTO workflow_retry_schedules \
             (run_id, node_id, activation_id, failed_attempt, next_attempt, failure_kind, \
              backoff_ms, next_attempt_at_ms, scheduled_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(run_id, node_id, activation_id) DO NOTHING",
            (
                &schedule.run_id,
                &schedule.node_id,
                &schedule.activation_id,
                schedule.failed_attempt,
                schedule.next_attempt,
                &failure_json,
                schedule.backoff_ms,
                schedule.next_attempt_at_ms,
                schedule.scheduled_at_ms,
            ),
        )?;
        let stored = automatic_retry_schedule(&transaction, run_id, node_id, activation_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "automatic retry schedule was not persisted".to_string(),
                )
            })?;
        if stored != schedule {
            return Err(WorkflowStoreError::InvalidData(
                "automatic retry schedule identity conflict".to_string(),
            ));
        }
        if inserted == 1 {
            append_event(
                &transaction,
                run_id,
                "automatic_retry_scheduled",
                &serde_json::to_string(&schedule)?,
                scheduled_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(schedule)
    }

    /// Load one exact persisted automatic-retry schedule.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid stored failure data, or database failure.
    pub fn automatic_retry_schedule(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
    ) -> Result<Option<AutomaticRetrySchedule>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        automatic_retry_schedule(&self.connection, run_id, node_id, activation_id)
    }

    /// Persist validated node output before marking its activation complete.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/bounded data, missing activation, conflicting output, or
    /// database failure.
    pub fn persist_validated_output(
        &mut self,
        output: &ValidatedOutput,
    ) -> Result<OutputPersistenceResult, WorkflowStoreError> {
        self.persist_validated_output_with_fault(output, &NoopWorkflowOutputFault)
    }

    /// Persist validated output with deterministic in-transaction fault injection.
    ///
    /// # Errors
    ///
    /// Returns an error from validation, persistence, successor scheduling, or the fault hook. Any
    /// error rolls back output, activation, successor, decision, and run projection changes.
    pub fn persist_validated_output_with_fault<F>(
        &mut self,
        output: &ValidatedOutput,
        fault: &F,
    ) -> Result<OutputPersistenceResult, WorkflowStoreError>
    where
        F: WorkflowOutputFault + ?Sized,
    {
        validate_output(output)?;
        let transaction = self.connection.transaction()?;
        let result = persist_validated_output_transaction_with_fault(&transaction, output, fault)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Persist an external admission/service receipt after dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, unbounded receipt, identity mismatch, a non-
    /// prepared attempt, or database failure.
    pub fn persist_dispatch_receipt(
        &mut self,
        receipt: &DispatchReceipt,
    ) -> Result<(), WorkflowStoreError> {
        validate_dispatch_receipt(receipt)?;
        let receipt_json = serde_json::to_string(&receipt.receipt)?;
        if receipt_json.len() > MAX_INLINE_JSON_BYTES {
            return Err(WorkflowStoreError::InvalidData(format!(
                "dispatch receipt exceeds {MAX_INLINE_JSON_BYTES} bytes"
            )));
        }
        let expected = dispatch_identity(
            &receipt.run_id,
            &receipt.node_id,
            &receipt.activation_id,
            receipt.attempt,
        );
        if receipt.dispatch_identity != expected {
            return Err(WorkflowStoreError::InvalidData(
                "dispatch receipt identity does not match durable attempt identity".to_string(),
            ));
        }
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE workflow_attempts SET status = 'admitted', receipt_json = ?6, admitted_at_ms = ?7 \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND attempt = ?4 \
               AND dispatch_identity = ?5 AND status = 'prepared'",
            (
                &receipt.run_id,
                &receipt.node_id,
                &receipt.activation_id,
                receipt.attempt,
                &receipt.dispatch_identity,
                &receipt_json,
                receipt.admitted_at_ms,
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "attempt is not prepared: {}",
                receipt.dispatch_identity
            )));
        }
        append_event(
            &transaction,
            &receipt.run_id,
            "attempt_admitted",
            &serde_json::to_string(receipt)?,
            receipt.admitted_at_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Safely redispatch bounded receipt-less prepared read-only attempts after restart.
    ///
    /// Mutating attempts are never returned by this operation. Each read-only operation retains
    /// its original stable dispatch identity and becomes admitted only after a new receipt commits.
    ///
    /// # Errors
    ///
    /// Returns an error when bounded discovery, durable decoding, owner dispatch, or receipt
    /// persistence fails. An owner error leaves the attempt prepared for another explicit retry.
    pub async fn redispatch_prepared_read_only<O>(
        &mut self,
        owner: &O,
        limit: usize,
        admitted_at_ms: u64,
    ) -> Result<Vec<String>, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        let prepared = prepared_read_only_dispatches(&self.connection, bounded_limit(limit)?)?;
        let mut admitted = Vec::with_capacity(prepared.len());
        for request in prepared {
            let receipt = owner.dispatch(&request).await?;
            self.persist_dispatch_receipt(&DispatchReceipt {
                run_id: request.activation.run_id.clone(),
                node_id: request.activation.node_id.clone(),
                activation_id: request.activation.activation_id.clone(),
                attempt: request.attempt,
                dispatch_identity: request.dispatch_identity.clone(),
                receipt,
                admitted_at_ms,
            })?;
            admitted.push(request.dispatch_identity);
        }
        Ok(admitted)
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
        if limit == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "reconciliation limit must be positive".to_string(),
            ));
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let transaction = self.connection.transaction()?;
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT run_id, dispatch_identity, side_effect FROM workflow_attempts \
                 WHERE status = 'prepared' ORDER BY prepared_at_ms, dispatch_identity LIMIT ?1",
            )?;
            statement
                .query_map([limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut summary = ReconciliationSummary::default();
        for (run_id, identity, side_effect) in rows {
            if side_effect == DispatchSideEffect::Mutating.as_str() {
                transaction.execute(
                    "UPDATE workflow_attempts SET status = 'repair_required', terminal_at_ms = ?2 \
                     WHERE dispatch_identity = ?1 AND status = 'prepared'",
                    (&identity, reconciled_at_ms),
                )?;
                transaction.execute(
                    "UPDATE workflow_runs SET status = ?2, updated_at_ms = ?3 WHERE run_id = ?1",
                    (
                        &run_id,
                        RunStatus::RepairRequired.as_str(),
                        reconciled_at_ms,
                    ),
                )?;
                append_event(
                    &transaction,
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
        transaction.commit()?;
        Ok(summary)
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
        receipt_backed_attempts(&self.connection, bounded_limit(limit)?)
    }

    /// Persist one exact owner observation for a receipt-backed attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt is missing/not receipt-backed or the durable terminal
    /// transition fails validation.
    pub fn apply_attempt_observation(
        &mut self,
        dispatch_identity: &str,
        observation: AttemptObservation,
        observed_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        let request =
            receipt_backed_attempt(&self.connection, dispatch_identity)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "receipt-backed workflow attempt not found: {dispatch_identity}"
                ))
            })?;
        let transaction = self.connection.transaction()?;
        let mut summary = ReceiptReconciliationSummary::default();
        apply_attempt_observation(
            &transaction,
            &request,
            observation,
            observed_at_ms,
            &mut summary,
        )?;
        transaction.commit()?;
        Ok(summary)
    }

    /// Reconcile receipt-backed admitted/running attempts through a bounded owner status API.
    ///
    /// Observation is read-only. Each returned state is then persisted atomically. Unknown
    /// mutating outcomes become repair-required and are never redispatched.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, receipt data is malformed, owner observation
    /// fails, or a durable transition cannot be committed.
    pub fn reconcile_receipt_backed_attempts<O>(
        &mut self,
        observer: &O,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AttemptStatusObserver + ?Sized,
    {
        let limit = bounded_limit(limit)?;
        let requests = receipt_backed_attempts(&self.connection, limit)?;
        let observations = requests
            .iter()
            .map(|request| observer.observe(request).map(|status| (request, status)))
            .collect::<Result<Vec<_>, _>>()?;
        let transaction = self.connection.transaction()?;
        let mut summary = ReceiptReconciliationSummary::default();
        for (request, observation) in observations {
            apply_attempt_observation(
                &transaction,
                request,
                observation,
                reconciled_at_ms,
                &mut summary,
            )?;
        }
        transaction.commit()?;
        Ok(summary)
    }

    /// Asynchronously reconcile receipt-backed attempts through a bounded owner status API.
    ///
    /// # Errors
    ///
    /// Returns an error when bounded discovery/observation or a durable transition fails.
    pub async fn reconcile_receipt_backed_attempts_async<O>(
        &mut self,
        observer: &O,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        let limit = bounded_limit(limit)?;
        let requests = receipt_backed_attempts(&self.connection, limit)?;
        let mut observations = Vec::with_capacity(requests.len());
        for request in requests {
            let observation = if cancellation_requested_for_run(&self.connection, &request.run_id)?
            {
                AttemptObservation::Cancelled
            } else {
                observer.observe_async(&request).await?
            };
            observations.push((request, observation));
        }
        let transaction = self.connection.transaction()?;
        let mut summary = ReceiptReconciliationSummary::default();
        for (request, observation) in observations {
            apply_attempt_observation(
                &transaction,
                &request,
                observation,
                reconciled_at_ms,
                &mut summary,
            )?;
        }
        transaction.commit()?;
        Ok(summary)
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
        let run_status = self
            .connection
            .query_row(
                "SELECT status FROM workflow_runs WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!("workflow run not found: {run_id}"))
            })?;
        let run_status = parse_run_status(&run_status)?;
        let repair_required_attempts = self.connection.query_row(
            "SELECT COUNT(*) FROM workflow_attempts WHERE run_id = ?1 \
             AND status = 'repair_required'",
            [run_id],
            |row| row.get::<_, u64>(0),
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
            let mut statement = self.connection.prepare(
                "SELECT grant_id, node_id, scope_json, expires_at_ms FROM workflow_grants \
                 WHERE run_id = ?1 ORDER BY granted_at_ms, grant_id LIMIT ?2",
            )?;
            let mut invalid_count = 0;
            for row in statement.query_map(
                (
                    run_id,
                    i64::try_from(remaining.saturating_add(1)).unwrap_or(i64::MAX),
                ),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<u64>>(3)?,
                    ))
                },
            )? {
                let (grant_id, node_id, scope_json, expires_at_ms) = row?;
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
            let mut statement = self.connection.prepare(
                "SELECT dispatch_identity, status, side_effect FROM workflow_attempts \
                 WHERE run_id = ?1 AND status IN ('prepared', 'admitted', 'running') \
                   AND receipt_json IS NULL ORDER BY prepared_at_ms, dispatch_identity LIMIT ?2",
            )?;
            for row in statement.query_map(
                (
                    run_id,
                    i64::try_from(remaining.saturating_add(1)).unwrap_or(i64::MAX),
                ),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )? {
                let (dispatch_identity, status, side_effect) = row?;
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
            let mut statement = self.connection.prepare(
                "SELECT node_id, activation_id, status, output_id FROM workflow_activations \
                 WHERE run_id = ?1 AND ((status = 'completed' AND (output_id IS NULL OR NOT EXISTS (\
                     SELECT 1 FROM workflow_outputs output WHERE output.run_id = workflow_activations.run_id \
                     AND output.node_id = workflow_activations.node_id \
                     AND output.activation_id = workflow_activations.activation_id \
                     AND output.output_id = workflow_activations.output_id))) \
                 OR (status != 'completed' AND output_id IS NOT NULL)) \
                 ORDER BY node_id, activation_id LIMIT ?2",
            )?;
            let mismatches = statement
                .query_map(
                    (run_id, i64::try_from(remaining).unwrap_or(i64::MAX)),
                    |row| {
                        Ok(WorkflowDoctorIssue::ActivationOutputMismatch {
                            node_id: row.get(0)?,
                            activation_id: row.get(1)?,
                            activation_status: row.get(2)?,
                            output_id: row.get(3)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            issues.extend(mismatches);
        }

        if issues.len() < fetch_limit {
            let remaining = fetch_limit - issues.len();
            let mut statement = self.connection.prepare(
                "SELECT node_id, activation_id, attempt, dispatch_identity FROM workflow_attempts \
                 WHERE run_id = ?1 ORDER BY prepared_at_ms, dispatch_identity",
            )?;
            let rows = statement.query_map([run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let scan_limit = report_limit.saturating_mul(16).max(report_limit);
            let mut mismatch_count = 0;
            for (inspected, row) in rows.enumerate() {
                if inspected >= scan_limit {
                    scan_truncated = true;
                    break;
                }
                let (node_id, activation_id, attempt, identity) = row?;
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
        validate_id("dispatch_identity", dispatch_identity)?;
        validate_repair_resolution(resolution)?;
        let transaction = self.connection.transaction()?;
        let attempt = repair_required_attempt(&transaction, dispatch_identity)?;
        let (run_id, node_id, activation_id, attempt_number, side_effect) = attempt;
        if parse_side_effect(&side_effect)? != DispatchSideEffect::Mutating {
            return Err(WorkflowStoreError::InvalidData(
                "only ambiguous mutating attempts require explicit repair".to_string(),
            ));
        }
        let expected = crate::dispatch_identity(&run_id, &node_id, &activation_id, attempt_number);
        if expected != dispatch_identity {
            return Err(WorkflowStoreError::InvalidData(
                "repair attempt identity does not match durable attempt identity".to_string(),
            ));
        }

        let (attempt_status, event_type, payload) = match resolution {
            RepairResolution::ConfirmSucceeded { output } => {
                if output.run_id != run_id
                    || output.node_id != node_id
                    || output.activation_id != activation_id
                {
                    return Err(WorkflowStoreError::InvalidData(
                        "repair output identity does not match durable attempt".to_string(),
                    ));
                }
                persist_validated_output_transaction(&transaction, output)?;
                (
                    "succeeded",
                    "attempt_repair_succeeded",
                    serde_json::to_value(resolution)?,
                )
            }
            RepairResolution::ConfirmFailed { .. } => (
                "failed",
                "attempt_repair_failed",
                serde_json::to_value(resolution)?,
            ),
            RepairResolution::ConfirmCancelled { .. } => (
                "cancelled",
                "attempt_repair_cancelled",
                serde_json::to_value(resolution)?,
            ),
            RepairResolution::AbandonForExplicitRetry { .. } => (
                "abandoned",
                "attempt_repair_abandoned",
                serde_json::to_value(resolution)?,
            ),
        };
        let changed = transaction.execute(
            "UPDATE workflow_attempts SET status = ?2, terminal_at_ms = ?3 \
             WHERE dispatch_identity = ?1 AND status = 'repair_required'",
            (dispatch_identity, attempt_status, repaired_at_ms),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "attempt cannot be repaired: {dispatch_identity}"
            )));
        }
        append_event(
            &transaction,
            &run_id,
            event_type,
            &serde_json::json!({
                "dispatch_identity": dispatch_identity,
                "resolution": payload,
            })
            .to_string(),
            repaired_at_ms,
        )?;
        let remaining: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_attempts WHERE run_id = ?1 \
             AND status = 'repair_required')",
            [&run_id],
            |row| row.get(0),
        )?;
        let run_status = if remaining {
            RunStatus::RepairRequired
        } else if matches!(resolution, RepairResolution::AbandonForExplicitRetry { .. }) {
            RunStatus::Running
        } else {
            RunStatus::Paused
        };
        transaction.execute(
            "UPDATE workflow_runs SET status = ?2, updated_at_ms = ?3 WHERE run_id = ?1",
            (&run_id, run_status.as_str(), repaired_at_ms),
        )?;
        transaction.commit()?;
        Ok(RepairResult {
            dispatch_identity: dispatch_identity.to_string(),
            attempt_status: attempt_status.to_string(),
            run_status,
        })
    }

    /// Pause one running workflow before any further activation or attempt admission.
    ///
    /// Active external attempts are not cancelled; they remain observable and reconcilable. Calling
    /// this for an already-paused run is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/terminal/cancelling run, or database
    /// failure.
    pub fn pause_run(
        &mut self,
        run_id: &str,
        paused_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        transition_run_control_state(
            &mut self.connection,
            run_id,
            RunStatus::Running,
            RunStatus::Paused,
            "run_paused",
            paused_at_ms,
        )
    }

    /// Resume one paused workflow for subsequent scheduler admission.
    ///
    /// Calling this for an already-running run is idempotent. A cancellation request permanently
    /// prevents resume.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/terminal/cancelling run, or database
    /// failure.
    pub fn resume_run(
        &mut self,
        run_id: &str,
        resumed_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        transition_run_control_state(
            &mut self.connection,
            run_id,
            RunStatus::Paused,
            RunStatus::Running,
            "run_resumed",
            resumed_at_ms,
        )
    }

    /// Explicitly requeue one exact failed activation for its next bounded attempt.
    ///
    /// Retry is accepted only when the run and activation are failed, the selected attempt is the
    /// latest terminal failed attempt, no output or downstream activation exists, no cancellation
    /// was requested, and the persisted retry cap permits another attempt. The transition is
    /// atomic and idempotently rejects stale operator requests.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/stale identity, unsafe downstream state, exhausted retry
    /// budget, cancellation, or database failure.
    #[allow(clippy::too_many_lines)]
    pub fn retry_failed_node(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        failed_attempt: u32,
        retried_at_ms: u64,
    ) -> Result<WorkflowNodeRetryResult, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        if failed_attempt == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "failed attempt must be positive".to_string(),
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (run_status, cancellation_requested, retry_cap): (String, bool, u32) = transaction
            .query_row(
                "SELECT status, cancellation_requested_at_ms IS NOT NULL, retry_cap \
                 FROM workflow_runs WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        if cancellation_requested || parse_run_status(&run_status)? != RunStatus::Failed {
            return Err(WorkflowStoreError::InvalidData(
                "workflow run is not eligible for node retry".to_string(),
            ));
        }
        let (activation_status, output_id): (String, Option<String>) = transaction.query_row(
            "SELECT status, output_id FROM workflow_activations \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3",
            (run_id, node_id, activation_id),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if activation_status != "failed" || output_id.is_some() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow activation is not an output-free failed activation".to_string(),
            ));
        }
        let (latest_attempt, attempt_status): (u32, String) = transaction.query_row(
            "SELECT attempt, status FROM workflow_attempts \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
             ORDER BY attempt DESC LIMIT 1",
            (run_id, node_id, activation_id),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if latest_attempt != failed_attempt || attempt_status != "failed" {
            return Err(WorkflowStoreError::InvalidData(
                "workflow retry requires the latest failed attempt identity".to_string(),
            ));
        }
        let next_attempt = failed_attempt.saturating_add(1);
        if next_attempt > retry_cap.saturating_add(1) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow retry cap exceeded".to_string(),
            ));
        }
        let definition_json: String = transaction.query_row(
            "SELECT definition.definition_json FROM workflow_runs run \
             JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
               AND definition.version = run.definition_version WHERE run.run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?;
        let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
        let direct_targets = definition
            .edges
            .iter()
            .filter(|edge| {
                edge.from == node_id
                    && !matches!(
                        edge.kind,
                        bcode_workflow::EdgeKind::Retry { .. }
                            | bcode_workflow::EdgeKind::Back { .. }
                    )
            })
            .map(|edge| edge.to.as_str())
            .collect::<Vec<_>>();
        for target in direct_targets {
            let downstream_exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_activations \
                 WHERE run_id = ?1 AND node_id = ?2)",
                (run_id, target),
                |row| row.get(0),
            )?;
            if downstream_exists {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow retry is unsafe after downstream activation".to_string(),
                ));
            }
        }
        transaction.execute(
            "UPDATE workflow_activations SET status = 'pending' \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND status = 'failed'",
            (run_id, node_id, activation_id),
        )?;
        transaction.execute(
            "UPDATE workflow_runs SET status = 'running', updated_at_ms = ?2 \
             WHERE run_id = ?1 AND status = 'failed'",
            (run_id, retried_at_ms),
        )?;
        let result = WorkflowNodeRetryResult {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            activation_id: activation_id.to_string(),
            previous_attempt: failed_attempt,
            next_attempt,
        };
        append_event(
            &transaction,
            run_id,
            "node_retry_requested",
            &serde_json::to_string(&result)?,
            retried_at_ms,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    /// Settle bounded pending deterministic control nodes without external dispatch.
    ///
    /// Repeat nodes either complete the run when their predicate clears/limit is reached or create
    /// the next generation's body entry activations. Other host-neutral control nodes are settled
    /// by forwarding their input as validated output through the normal atomic successor path.
    ///
    /// # Errors
    ///
    /// Returns an error when bounded discovery, control configuration, schemas, cycle limits, or
    /// persistence invariants are invalid.
    pub fn settle_pending_control_nodes(
        &mut self,
        run_id: &str,
        limit: usize,
        settled_at_ms: u64,
    ) -> Result<ControlSettlementSummary, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let pending = self.pending_activations(limit)?;
        let mut summary = ControlSettlementSummary::default();
        for activation in pending
            .into_iter()
            .filter(|activation| activation.run_id == run_id)
            .filter(|activation| {
                matches!(
                    activation.node.kind,
                    bcode_workflow::NodeKind::Branch
                        | bcode_workflow::NodeKind::Repeat
                        | bcode_workflow::NodeKind::Retry
                        | bcode_workflow::NodeKind::Parallel
                )
            })
        {
            let input = activation.input.clone().ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow control node has no input: {}",
                    activation.node_id
                ))
            })?;
            if activation.node.kind == bcode_workflow::NodeKind::Repeat {
                let result = self.settle_repeat_node(&activation, &input, settled_at_ms)?;
                summary.settled.push(activation.activation_id);
                summary.activated.extend(result);
                continue;
            }
            let output = ValidatedOutput {
                output_id: format!("{}:control-output", activation.activation_id),
                run_id: activation.run_id.clone(),
                node_id: activation.node_id.clone(),
                activation_id: activation.activation_id.clone(),
                schema_id: activation.node.output.type_name.clone(),
                schema_version: 1,
                value: input,
                artifact_reference: None,
                created_at_ms: settled_at_ms,
            };
            let result = self.persist_validated_output(&output)?;
            summary.settled.push(activation.activation_id);
            summary.activated.extend(result.activated);
        }
        Ok(summary)
    }

    #[allow(clippy::too_many_lines)]
    fn settle_repeat_node(
        &mut self,
        activation: &PendingActivation,
        input: &serde_json::Value,
        settled_at_ms: u64,
    ) -> Result<Vec<NewActivation>, WorkflowStoreError> {
        let transaction = self.connection.transaction()?;
        let definition_json: String = transaction.query_row(
            "SELECT definition.definition_json FROM workflow_runs run \
             JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
               AND definition.version = run.definition_version WHERE run.run_id = ?1",
            [&activation.run_id],
            |row| row.get(0),
        )?;
        let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
        let predicate: bcode_workflow::PredicateExpression = serde_json::from_value(
            activation
                .node
                .configuration
                .get("predicate")
                .cloned()
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "workflow repeat configuration is missing predicate".to_string(),
                    )
                })?,
        )?;
        let max_iterations = activation
            .node
            .configuration
            .get("max_iterations")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow repeat configuration is missing max_iterations".to_string(),
                )
            })?;
        let cycle_cap: u64 = transaction.query_row(
            "SELECT cycle_cap FROM workflow_runs WHERE run_id = ?1",
            [&activation.run_id],
            |row| row.get(0),
        )?;
        let effective_iteration_bound = max_iterations.min(cycle_cap);
        let should_repeat = evaluate_predicate(&predicate, input)?;
        let next_generation = activation
            .dependency_generation
            .checked_add(1)
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("workflow repeat generation overflow".to_string())
            })?;
        let within_iteration_bound = next_generation < effective_iteration_bound;
        transaction.execute(
            "UPDATE workflow_activations SET status = 'completed', output_id = ?4 \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND status = 'pending'",
            (
                &activation.run_id,
                &activation.node_id,
                &activation.activation_id,
                format!("{}:control-output", activation.activation_id),
            ),
        )?;
        let value_json = serde_json::to_string(&input)?;
        transaction.execute(
            "INSERT INTO workflow_outputs \
             (output_id, run_id, node_id, activation_id, schema_id, schema_version, value_json, \
              artifact_reference, checksum_sha256, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, NULL, ?7, ?8)",
            (
                format!("{}:control-output", activation.activation_id),
                &activation.run_id,
                &activation.node_id,
                &activation.activation_id,
                &activation.node.output.type_name,
                &value_json,
                sha256_hex(value_json.as_bytes()),
                settled_at_ms,
            ),
        )?;
        let mut activated = Vec::new();
        if should_repeat && within_iteration_bound {
            for edge in definition.edges.iter().filter(|edge| {
                edge.from == activation.node_id
                    && matches!(edge.kind, bcode_workflow::EdgeKind::Back { .. })
            }) {
                let node = definition.node(&edge.to).ok_or_else(|| {
                    WorkflowStoreError::InvalidData(format!(
                        "workflow repeat target is missing: {}",
                        edge.to
                    ))
                })?;
                let transformed_input = if let Some(transform) = &edge.transform {
                    let run_input_json: Option<String> = transaction.query_row(
                        "SELECT input_json FROM workflow_runs WHERE run_id = ?1",
                        [&activation.run_id],
                        |row| row.get(0),
                    )?;
                    let run_input = run_input_json
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .unwrap_or(serde_json::Value::Null);
                    transform
                        .evaluate(&[
                            bcode_workflow::WorkflowTransformInput {
                                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                                value: input,
                            },
                            bcode_workflow::WorkflowTransformInput {
                                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                                value: &run_input,
                            },
                        ])
                        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
                } else {
                    input.clone()
                };
                let next = NewActivation {
                    run_id: activation.run_id.clone(),
                    node_id: edge.to.clone(),
                    activation_id: activation_identity(
                        &activation.run_id,
                        &edge.to,
                        next_generation,
                    ),
                    dependency_generation: next_generation,
                    input: Some(transformed_input.clone()),
                    created_at_ms: settled_at_ms,
                };
                validate_json_schema(
                    &format!("workflow repeat input for node {}", node.id),
                    &node.input.schema,
                    &transformed_input,
                )
                .map_err(|error| {
                    WorkflowStoreError::TargetInputValidation(Box::new(
                        TargetInputValidationFailure {
                            run_id: activation.run_id.clone(),
                            source_node_id: activation.node_id.clone(),
                            source_activation_id: activation.activation_id.clone(),
                            target_node_id: node.id.clone(),
                            target_schema_id: node.input.type_name.clone(),
                            message: error.to_string(),
                        },
                    ))
                })?;
                insert_activation_with_status(
                    &transaction,
                    &next,
                    activation_status_for_node(node),
                )?;
                activated.push(next);
            }
        } else if should_repeat {
            transaction.execute(
                "UPDATE workflow_runs SET status = 'failed', updated_at_ms = ?2 \
                 WHERE run_id = ?1 AND status = 'running'",
                (&activation.run_id, settled_at_ms),
            )?;
            append_event(
                &transaction,
                &activation.run_id,
                "run_failed",
                &serde_json::json!({
                    "node_id": activation.node_id,
                    "reason": "repeat_iteration_limit_exhausted",
                    "max_iterations": max_iterations,
                    "cycle_cap": cycle_cap,
                    "effective_iteration_bound": effective_iteration_bound,
                    "generation": activation.dependency_generation,
                })
                .to_string(),
                settled_at_ms,
            )?;
        } else {
            transaction.execute(
                "UPDATE workflow_runs SET status = 'completed', updated_at_ms = ?2 \
                 WHERE run_id = ?1 AND status = 'running'",
                (&activation.run_id, settled_at_ms),
            )?;
            append_event(
                &transaction,
                &activation.run_id,
                "run_completed",
                "{}",
                settled_at_ms,
            )?;
        }
        append_event(
            &transaction,
            &activation.run_id,
            "control_node_settled",
            &serde_json::json!({
                "node_id": activation.node_id,
                "activation_id": activation.activation_id,
                "repeat": should_repeat && within_iteration_bound,
                "generation": activation.dependency_generation,
                "next_generation": should_repeat.then_some(next_generation),
                "max_iterations": max_iterations,
                "cycle_cap": cycle_cap,
                "effective_iteration_bound": effective_iteration_bound,
                "iteration_bound_exhausted": should_repeat && !within_iteration_bound,
            })
            .to_string(),
            settled_at_ms,
        )?;
        transaction.commit()?;
        Ok(activated)
    }

    /// Persist cancellation intent before an executor signals active children.
    ///
    /// The returned value indicates whether this call recorded the first cancellation request.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/terminal run, or database failure.
    pub fn request_cancellation(
        &mut self,
        run_id: &str,
        requested_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE workflow_runs SET cancellation_requested_at_ms = ?2, updated_at_ms = ?2 \
             WHERE run_id = ?1 AND cancellation_requested_at_ms IS NULL \
               AND status IN ('running', 'paused')",
            (run_id, requested_at_ms),
        )?;
        if changed == 1 {
            let cancelled_approvals = transaction.execute(
                "UPDATE workflow_mutation_approvals SET status = 'cancelled', resolved_at_ms = ?2 \
                 WHERE run_id = ?1 AND status = 'pending'",
                (run_id, requested_at_ms),
            )?;
            append_event(
                &transaction,
                run_id,
                "cancellation_requested",
                &serde_json::json!({"requested_at_ms": requested_at_ms}).to_string(),
                requested_at_ms,
            )?;
            if cancelled_approvals > 0 {
                append_event(
                    &transaction,
                    run_id,
                    "mutation_approvals_cancelled",
                    &serde_json::json!({"count": cancelled_approvals}).to_string(),
                    requested_at_ms,
                )?;
            }
        } else if transaction
            .query_row(
                "SELECT cancellation_requested_at_ms IS NOT NULL FROM workflow_runs \
                 WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .is_none()
        {
            return Err(WorkflowStoreError::RunNotFound {
                run_id: run_id.to_string(),
            });
        }
        finalize_run_cancellation_if_settled(&transaction, run_id, requested_at_ms)?;
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// Return bounded active attempts only after cancellation intent is durable.
    ///
    /// Receipts are included when present so the owner can route cancellation to the exact turn or
    /// plugin operation. Receipt-less prepared attempts remain discoverable by stable identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity/limit is invalid, cancellation was not persisted,
    /// receipt JSON is malformed, or the bounded query fails.
    pub fn active_attempt_cancellations(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        require_cancellation_requested(&self.connection, run_id)?;
        let mut statement = self.connection.prepare(
            "SELECT run_id, node_id, activation_id, attempt, dispatch_identity, receipt_json \
             FROM workflow_attempts WHERE run_id = ?1 \
             AND status IN ('prepared', 'admitted', 'running') \
             ORDER BY prepared_at_ms, dispatch_identity LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .map(|row| {
                let (run_id, node_id, activation_id, attempt, dispatch_identity, receipt_json) =
                    row?;
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
            })
            .collect()
    }

    /// Settle cancellation for an attempt whose process-local owner is absent.
    ///
    /// Receipt-less/read-only attempts can be cancelled without external ambiguity. Any mutating
    /// attempt instead becomes repair-required because its external outcome cannot be proven after
    /// owner loss.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt is missing, cancellation was not requested, or the
    /// durable transition fails.
    pub fn settle_orphaned_attempt_cancellation(
        &mut self,
        dispatch_identity: &str,
        settled_at_ms: u64,
    ) -> Result<RunStatus, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        let transaction = self.connection.transaction()?;
        let (run_id, node_id, activation_id, status, side_effect) = transaction.query_row(
            "SELECT run_id, node_id, activation_id, status, side_effect \
                 FROM workflow_attempts WHERE dispatch_identity = ?1",
            [dispatch_identity],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?;
        require_cancellation_requested(&transaction, &run_id)?;
        if !matches!(
            status.as_str(),
            "prepared" | "admitted" | "running" | "cancelling"
        ) {
            let run_status = transaction.query_row(
                "SELECT status FROM workflow_runs WHERE run_id = ?1",
                [&run_id],
                |row| row.get::<_, String>(0),
            )?;
            return parse_run_status(&run_status);
        }
        if parse_side_effect(&side_effect)? == DispatchSideEffect::Mutating {
            transaction.execute(
                "UPDATE workflow_attempts SET status = 'repair_required', terminal_at_ms = ?2 \
                 WHERE dispatch_identity = ?1",
                (dispatch_identity, settled_at_ms),
            )?;
            transaction.execute(
                "UPDATE workflow_runs SET status = 'repair_required', updated_at_ms = ?2 \
                 WHERE run_id = ?1",
                (&run_id, settled_at_ms),
            )?;
            append_event(
                &transaction,
                &run_id,
                "attempt_repair_required",
                &serde_json::json!({
                    "dispatch_identity": dispatch_identity,
                    "reason": "cancellation_owner_unavailable",
                })
                .to_string(),
                settled_at_ms,
            )?;
            transaction.commit()?;
            return Ok(RunStatus::RepairRequired);
        }
        transaction.execute(
            "UPDATE workflow_attempts SET status = 'cancelled', terminal_at_ms = ?2 \
             WHERE dispatch_identity = ?1",
            (dispatch_identity, settled_at_ms),
        )?;
        transaction.execute(
            "UPDATE workflow_activations SET status = 'cancelled' \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
               AND status IN ('pending', 'running') AND output_id IS NULL",
            (&run_id, &node_id, &activation_id),
        )?;
        append_event(
            &transaction,
            &run_id,
            "attempt_cancelled",
            &serde_json::json!({
                "dispatch_identity": dispatch_identity,
                "reason": "cancellation_owner_unavailable",
            })
            .to_string(),
            settled_at_ms,
        )?;
        finalize_run_cancellation_if_settled(&transaction, &run_id, settled_at_ms)?;
        let run_status = transaction.query_row(
            "SELECT status FROM workflow_runs WHERE run_id = ?1",
            [&run_id],
            |row| row.get::<_, String>(0),
        )?;
        let run_status = parse_run_status(&run_status)?;
        transaction.commit()?;
        Ok(run_status)
    }

    /// Mark one successfully signalled active attempt as cancelling.
    ///
    /// This is the durable second half of host-side two-phase cancellation propagation. Calling it
    /// again for an already-cancelling attempt is idempotent and does not append a duplicate event.
    /// Terminal attempts are reported as unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing cancellation intent, or database failure.
    pub fn mark_cancellation_signalled(
        &mut self,
        dispatch_identity: &str,
        signalled_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        let transaction = self.connection.transaction()?;
        let run_id = transaction
            .query_row(
                "SELECT run_id FROM workflow_attempts WHERE dispatch_identity = ?1",
                [dispatch_identity],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow attempt not found: {dispatch_identity}"
                ))
            })?;
        require_cancellation_requested(&transaction, &run_id)?;
        let changed = transaction.execute(
            "UPDATE workflow_attempts SET status = 'cancelling' \
             WHERE dispatch_identity = ?1 AND status IN ('prepared', 'admitted', 'running')",
            [dispatch_identity],
        )?;
        if changed == 1 {
            append_event(
                &transaction,
                &run_id,
                "attempt_cancellation_signalled",
                &serde_json::json!({"dispatch_identity": dispatch_identity}).to_string(),
                signalled_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// Signal active attempt owners after durable cancellation intent is confirmed.
    ///
    /// Signaling occurs outside a database transaction. Successful signals are marked
    /// `cancelling`; a crash before that marker is safe because owner signaling must be idempotent
    /// and the attempt remains discoverable.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, owner signaling, or durable marking fails. Unsignalled
    /// attempts remain discoverable for a later retry.
    pub async fn propagate_cancellation<O>(
        &mut self,
        run_id: &str,
        owner: &O,
        limit: usize,
        signalled_at_ms: u64,
    ) -> Result<CancellationPropagationSummary, WorkflowStoreError>
    where
        O: AttemptCancellationOwner + ?Sized,
    {
        let attempts = self.active_attempt_cancellations(run_id, limit)?;
        let mut summary = CancellationPropagationSummary::default();
        for attempt in attempts {
            owner.cancel_attempt(&attempt).await?;
            if self.mark_cancellation_signalled(&attempt.dispatch_identity, signalled_at_ms)? {
                summary.signalled.push(attempt.dispatch_identity);
            } else {
                summary.already_terminal.push(attempt.dispatch_identity);
            }
        }
        Ok(summary)
    }

    /// Return dispatch identities for active children after cancellation intent is durable.
    ///
    /// New owner routers should use [`Self::active_attempt_cancellations`] to retain receipts.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity/limit is invalid, cancellation was not persisted,
    /// receipt JSON is malformed, or the bounded query fails.
    pub fn active_attempts_for_cancellation(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        self.active_attempt_cancellations(run_id, limit)
            .map(|attempts| {
                attempts
                    .into_iter()
                    .map(|attempt| attempt.dispatch_identity)
                    .collect()
            })
    }

    /// Return one run summary without replaying workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded row query fails or contains invalid status data.
    pub fn run_summary(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
        self.connection
            .query_row(
                "SELECT run_id, definition_id, definition_version, workspace_snapshot, \
                 parent_session_id, owner_plugin_id, workflow_kind, scope_key, display_label, \
                 single_active, status, cancellation_requested_at_ms, created_at_ms, updated_at_ms \
                 FROM workflow_runs WHERE run_id = ?1",
                [run_id],
                run_summary_from_row,
            )
            .optional()?
            .map(parse_run_summary)
            .transpose()
    }

    /// Return bounded activations for one run without event replay.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/limit or query failure.
    pub fn activations_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowActivationSummary>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT node_id, activation_id, dependency_generation, status, output_id IS NOT NULL, \
             created_at_ms FROM workflow_activations WHERE run_id = ?1 \
             ORDER BY dependency_generation DESC, created_at_ms DESC, node_id LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok(WorkflowActivationSummary {
                    run_id: run_id.to_string(),
                    node_id: row.get(0)?,
                    activation_id: row.get(1)?,
                    dependency_generation: row.get(2)?,
                    status: row.get(3)?,
                    has_output: row.get(4)?,
                    created_at_ms: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(WorkflowStoreError::from)
    }

    /// Return bounded pending activations with exact compiled node definitions.
    ///
    /// This normal scheduler read uses normalized rows and one exact stored definition per run. It
    /// performs no event replay, repair, or external dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, persisted definitions are malformed, a node is
    /// missing, or the bounded query fails.
    pub fn pending_activations(
        &self,
        limit: usize,
    ) -> Result<Vec<PendingActivation>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT activation.run_id, activation.node_id, activation.activation_id, \
             activation.dependency_generation, activation.input_json, activation.created_at_ms, \
             definition.definition_json \
             FROM workflow_activations activation \
             JOIN workflow_runs run ON run.run_id = activation.run_id \
             JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
               AND definition.version = run.definition_version \
             WHERE activation.status = 'pending' AND run.status = 'running' \
               AND run.cancellation_requested_at_ms IS NULL \
             ORDER BY activation.created_at_ms, activation.run_id, activation.node_id LIMIT ?1",
        )?;
        statement
            .query_map([limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .map(|row| {
                let (
                    run_id,
                    node_id,
                    activation_id,
                    dependency_generation,
                    input_json,
                    created_at_ms,
                    json,
                ) = row?;
                let definition: WorkflowDefinition = serde_json::from_str(&json)?;
                let node = definition.node(&node_id).cloned().ok_or_else(|| {
                    WorkflowStoreError::InvalidData(format!(
                        "workflow activation references missing node: {node_id}"
                    ))
                })?;
                Ok(PendingActivation {
                    run_id,
                    node_id,
                    activation_id,
                    dependency_generation,
                    input: input_json
                        .map(|json| serde_json::from_str(&json))
                        .transpose()?,
                    node,
                    created_at_ms,
                })
            })
            .collect()
    }

    /// Return a bounded newest-first run list without replaying workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid or the bounded query fails.
    pub fn list_runs(&self, limit: usize) -> Result<Vec<WorkflowRunSummary>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT run_id, definition_id, definition_version, workspace_snapshot, \
             parent_session_id, owner_plugin_id, workflow_kind, scope_key, display_label, \
             single_active, status, cancellation_requested_at_ms, created_at_ms, updated_at_ms \
             FROM workflow_runs ORDER BY updated_at_ms DESC, run_id LIMIT ?1",
        )?;
        statement
            .query_map([limit], run_summary_from_row)?
            .map(|row| {
                row.map_err(WorkflowStoreError::from)
                    .and_then(parse_run_summary)
            })
            .collect()
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
        validate_id("owner_plugin_id", &key.owner_plugin_id)?;
        validate_id("workflow_kind", &key.workflow_kind)?;
        validate_id("scope_key", &key.scope_key)?;
        self.connection
            .query_row(
                "SELECT run_id, definition_id, definition_version, workspace_snapshot, \
                 parent_session_id, owner_plugin_id, workflow_kind, scope_key, display_label, \
                 single_active, status, cancellation_requested_at_ms, created_at_ms, updated_at_ms \
                 FROM workflow_runs WHERE owner_plugin_id = ?1 AND workflow_kind = ?2 \
                 AND scope_key = ?3 ORDER BY updated_at_ms DESC, run_id LIMIT 1",
                (&key.owner_plugin_id, &key.workflow_kind, &key.scope_key),
                run_summary_from_row,
            )
            .optional()?
            .map(parse_run_summary)
            .transpose()
    }

    /// Return bounded run IDs whose pending activations are safe to continue after startup.
    ///
    /// Eligible runs are running without cancellation intent, active attempts, or unresolved
    /// input/approval waits. The query is projection-backed and does not replay workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit is invalid or the bounded query fails.
    pub fn resumable_pending_run_ids(
        &self,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT activation.run_id FROM workflow_activations activation \
             JOIN workflow_runs run ON run.run_id = activation.run_id \
             WHERE activation.status = 'pending' AND run.status = 'running' \
               AND run.cancellation_requested_at_ms IS NULL \
               AND NOT EXISTS(SELECT 1 FROM workflow_attempts attempt \
                 WHERE attempt.run_id = run.run_id \
                   AND attempt.status IN ('prepared', 'admitted', 'running', 'cancelling')) \
               AND NOT EXISTS(SELECT 1 FROM workflow_activations waiting \
                 WHERE waiting.run_id = run.run_id \
                   AND waiting.status IN ('waiting_input', 'waiting_approval')) \
             ORDER BY activation.run_id LIMIT ?1",
        )?;
        statement
            .query_map([limit], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(WorkflowStoreError::from)
    }

    /// Return bounded durable input/approval waits ordered deterministically.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid limit, malformed persisted JSON, or database failure.
    pub fn waiting_activations(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WaitingActivation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT run_id, node_id, activation_id, status, input_json, created_at_ms \
             FROM workflow_activations WHERE run_id = ?1 \
               AND status IN ('waiting_input', 'waiting_approval') \
             ORDER BY created_at_ms, node_id, activation_id LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, u64>(5)?,
                ))
            })?
            .map(|row| {
                let (run_id, node_id, activation_id, status, input_json, requested_at_ms) = row?;
                let kind = match status.as_str() {
                    "waiting_input" => WorkflowWaitKind::Input,
                    "waiting_approval" => WorkflowWaitKind::Approval,
                    _ => {
                        return Err(WorkflowStoreError::InvalidData(format!(
                            "invalid workflow wait status: {status}"
                        )));
                    }
                };
                Ok(WaitingActivation {
                    run_id,
                    node_id,
                    activation_id,
                    kind,
                    input: input_json
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?,
                    requested_at_ms,
                })
            })
            .collect()
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
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn resolve_waiting_activation(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        kind: WorkflowWaitKind,
        supplied_value: Option<serde_json::Value>,
        accepted: bool,
        resolved_at_ms: u64,
    ) -> Result<WaitingResolutionResult, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        let transaction = self.connection.transaction()?;
        let (status, input_json, definition_json, run_status, cancellation_requested): (
            String,
            Option<String>,
            String,
            String,
            bool,
        ) = transaction
            .query_row(
                "SELECT activation.status, activation.input_json, definition.definition_json, \
                 run.status, run.cancellation_requested_at_ms IS NOT NULL \
                 FROM workflow_activations activation \
                 JOIN workflow_runs run ON run.run_id = activation.run_id \
                 JOIN workflow_definitions definition \
                   ON definition.definition_id = run.definition_id \
                  AND definition.version = run.definition_version \
                 WHERE activation.run_id = ?1 AND activation.node_id = ?2 \
                   AND activation.activation_id = ?3",
                (run_id, node_id, activation_id),
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "waiting activation not found: {run_id}/{node_id}/{activation_id}"
                ))
            })?;
        if cancellation_requested || parse_run_status(&run_status)? != RunStatus::Running {
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
        let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
        let node = definition.node(node_id).ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("workflow node not found: {node_id}"))
        })?;
        let value = match kind {
            WorkflowWaitKind::Input => supplied_value.ok_or_else(|| {
                WorkflowStoreError::InvalidData("input gate requires a value".to_string())
            })?,
            WorkflowWaitKind::Approval => input_json
                .map(|value| serde_json::from_str(&value))
                .transpose()?
                .unwrap_or(serde_json::Value::Null),
        };
        if accepted {
            validate_json_schema("waiting activation output", &node.output.schema, &value)?;
            transaction.execute(
                "UPDATE workflow_activations SET status = 'running' \
                 WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND status = ?4",
                (run_id, node_id, activation_id, expected_status),
            )?;
            let output = ValidatedOutput {
                output_id: format!("{activation_id}:wait-output"),
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                activation_id: activation_id.to_string(),
                schema_id: node.output.type_name.clone(),
                schema_version: 1,
                value,
                artifact_reference: None,
                created_at_ms: resolved_at_ms,
            };
            let result = persist_validated_output_transaction(&transaction, &output)?;
            append_event(
                &transaction,
                run_id,
                "waiting_activation_resolved",
                &serde_json::json!({"node_id": node_id, "activation_id": activation_id, "kind": kind, "accepted": true}).to_string(),
                resolved_at_ms,
            )?;
            transaction.commit()?;
            return Ok(WaitingResolutionResult {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                activation_id: activation_id.to_string(),
                outcome: "accepted".to_string(),
                activated: result.activated,
                run_status: result.run_status,
            });
        }
        transaction.execute(
            "UPDATE workflow_activations SET status = 'failed' \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND status = ?4",
            (run_id, node_id, activation_id, expected_status),
        )?;
        transaction.execute(
            "UPDATE workflow_runs SET status = 'failed', updated_at_ms = ?2 \
             WHERE run_id = ?1 AND status = 'running'",
            (run_id, resolved_at_ms),
        )?;
        append_event(
            &transaction,
            run_id,
            "waiting_activation_resolved",
            &serde_json::json!({"node_id": node_id, "activation_id": activation_id, "kind": kind, "accepted": false}).to_string(),
            resolved_at_ms,
        )?;
        append_event(&transaction, run_id, "run_failed", "{}", resolved_at_ms)?;
        transaction.commit()?;
        Ok(WaitingResolutionResult {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            activation_id: activation_id.to_string(),
            outcome: "denied".to_string(),
            activated: Vec::new(),
            run_status: RunStatus::Failed,
        })
    }

    /// Return a keyset-paged bounded attempt history for one run.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, the cursor is malformed, or the query fails.
    pub fn attempt_history(
        &self,
        run_id: &str,
        cursor: Option<&AttemptCursor>,
        limit: usize,
    ) -> Result<Vec<AttemptSummary>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        if let Some(cursor) = cursor {
            validate_id("cursor.dispatch_identity", &cursor.dispatch_identity)?;
        }
        let mut statement = self.connection.prepare(
            "SELECT run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, \
             status, receipt_json IS NOT NULL, prepared_at_ms, admitted_at_ms, terminal_at_ms \
             FROM workflow_attempts WHERE run_id = ?1 \
               AND (?2 IS NULL OR prepared_at_ms < ?2 \
                    OR (prepared_at_ms = ?2 AND dispatch_identity > ?3)) \
             ORDER BY prepared_at_ms DESC, dispatch_identity LIMIT ?4",
        )?;
        let prepared_at = cursor.map(|cursor| cursor.prepared_at_ms);
        let identity = cursor.map(|cursor| cursor.dispatch_identity.as_str());
        statement
            .query_map(
                (run_id, prepared_at, identity, limit),
                attempt_summary_from_row,
            )?
            .map(|row| {
                row.map_err(WorkflowStoreError::from)
                    .and_then(parse_attempt_summary)
            })
            .collect()
    }

    /// Return keyset-paged workflow events after `after_sequence`.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, JSON is malformed, or the query fails.
    pub fn event_history(
        &self,
        run_id: &str,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<WorkflowEventRow>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let after = after_sequence.unwrap_or(0);
        let mut statement = self.connection.prepare(
            "SELECT event_seq, run_id, event_type, payload_json, created_at_ms \
             FROM workflow_events WHERE run_id = ?1 AND event_seq > ?2 \
             ORDER BY event_seq LIMIT ?3",
        )?;
        statement
            .query_map((run_id, after, limit), |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            })?
            .map(|row| {
                let (event_seq, run_id, event_type, payload_json, created_at_ms) = row?;
                Ok(WorkflowEventRow {
                    event_seq,
                    run_id,
                    event_type,
                    payload: serde_json::from_str(&payload_json)?,
                    created_at_ms,
                })
            })
            .collect()
    }
}

fn persist_validated_output_transaction(
    transaction: &Transaction<'_>,
    output: &ValidatedOutput,
) -> Result<OutputPersistenceResult, WorkflowStoreError> {
    persist_validated_output_transaction_with_fault(transaction, output, &NoopWorkflowOutputFault)
}

fn persist_validated_output_transaction_with_fault<F>(
    transaction: &Transaction<'_>,
    output: &ValidatedOutput,
    fault: &F,
) -> Result<OutputPersistenceResult, WorkflowStoreError>
where
    F: WorkflowOutputFault + ?Sized,
{
    validate_output(output)?;
    validate_output_against_node_schema(transaction, output)?;
    let value_json = serde_json::to_string(&output.value)?;
    if value_json.len() > MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "validated output exceeds {MAX_INLINE_JSON_BYTES} bytes"
        )));
    }
    let checksum = sha256_hex(value_json.as_bytes());
    transaction.execute(
        "INSERT INTO workflow_outputs \
         (output_id, run_id, node_id, activation_id, schema_id, schema_version, value_json, \
          artifact_reference, checksum_sha256, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        (
            &output.output_id,
            &output.run_id,
            &output.node_id,
            &output.activation_id,
            &output.schema_id,
            output.schema_version,
            &value_json,
            &output.artifact_reference,
            &checksum,
            output.created_at_ms,
        ),
    )?;
    fault.after_boundary(WorkflowOutputBoundary::OutputInserted, output)?;
    let changed = transaction.execute(
        "UPDATE workflow_activations SET status = 'completed', output_id = ?4 \
         WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
           AND status IN ('pending', 'running')",
        (
            &output.run_id,
            &output.node_id,
            &output.activation_id,
            &output.output_id,
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "activation is not pending or running: {}/{}/{}",
            output.run_id, output.node_id, output.activation_id
        )));
    }
    fault.after_boundary(WorkflowOutputBoundary::ActivationCompleted, output)?;
    append_event(
        transaction,
        &output.run_id,
        "output_validated",
        &serde_json::to_string(output)?,
        output.created_at_ms,
    )?;
    let (activated, completed_is_exit) = materialize_direct_successors(transaction, output, fault)?;
    let parallel_failure = settle_wait_all_parallel_failure(
        transaction,
        &output.run_id,
        &output.node_id,
        output.created_at_ms,
    )?
    .unwrap_or(false);
    fault.after_boundary(WorkflowOutputBoundary::SuccessorsMaterialized, output)?;
    let active_count: u64 = transaction.query_row(
        "SELECT COUNT(*) FROM workflow_activations WHERE run_id = ?1 \
         AND status IN ('pending', 'running')",
        [&output.run_id],
        |row| row.get(0),
    )?;
    let run_status = if parallel_failure {
        RunStatus::Failed
    } else if active_count == 0 && completed_is_exit {
        transaction.execute(
            "UPDATE workflow_runs SET status = 'completed', updated_at_ms = ?2 \
             WHERE run_id = ?1 AND status = 'running'",
            (&output.run_id, output.created_at_ms),
        )?;
        append_event(
            transaction,
            &output.run_id,
            "run_completed",
            "{}",
            output.created_at_ms,
        )?;
        RunStatus::Completed
    } else {
        RunStatus::Running
    };
    Ok(OutputPersistenceResult {
        completed_activation_id: output.activation_id.clone(),
        activated,
        run_status,
    })
}

fn evaluate_predicate(
    expression: &bcode_workflow::PredicateExpression,
    value: &serde_json::Value,
) -> Result<bool, WorkflowStoreError> {
    expression
        .evaluate_value(value)
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))
}

fn configured_node_ids(
    configuration: &serde_json::Value,
    field: &str,
) -> Result<Vec<String>, WorkflowStoreError> {
    configuration
        .get(field)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow branch configuration is missing {field}"
            ))
        })?
        .iter()
        .map(|value| {
            value.as_str().map(ToString::to_string).ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow branch configuration {field} must contain node IDs"
                ))
            })
        })
        .collect()
}

fn skip_branch_nodes(
    transaction: &Transaction<'_>,
    run_id: &str,
    generation: u64,
    node_ids: &[String],
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    for node_id in node_ids {
        transaction.execute(
            "INSERT INTO workflow_activations \
             (run_id, node_id, activation_id, dependency_generation, input_json, status, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, NULL, 'skipped', ?5) \
             ON CONFLICT(run_id, node_id, activation_id) DO NOTHING",
            (
                run_id,
                node_id,
                activation_identity(run_id, node_id, generation),
                generation,
                created_at_ms,
            ),
        )?;
    }
    Ok(())
}

fn activation_output_value(
    transaction: &Transaction<'_>,
    run_id: &str,
    node_id: &str,
    generation: u64,
) -> Result<serde_json::Value, WorkflowStoreError> {
    let value_json: String = transaction.query_row(
        "SELECT output.value_json FROM workflow_activations activation \
         JOIN workflow_outputs output ON output.output_id = activation.output_id \
         WHERE activation.run_id = ?1 AND activation.node_id = ?2 \
           AND activation.dependency_generation = ?3 AND activation.status = 'completed'",
        (run_id, node_id, generation),
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&value_json)?)
}

fn parallel_join_members(
    transaction: &Transaction<'_>,
    definition: &WorkflowDefinition,
    run_id: &str,
    target: &bcode_workflow::NodeDefinition,
    generation: u64,
) -> Result<(serde_json::Value, serde_json::Value), WorkflowStoreError> {
    let member = |field: &str| {
        let exits = configured_node_ids(&target.configuration, field)?;
        exits
            .iter()
            .find_map(|node_id| {
                definition.node(node_id).and_then(|_| {
                    activation_output_value(transaction, run_id, node_id, generation).ok()
                })
            })
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "parallel join {} has no completed {field} member output",
                    target.id
                ))
            })
    };
    Ok((member("left_exits")?, member("right_exits")?))
}

fn activation_input(
    transaction: &Transaction<'_>,
    definition: &WorkflowDefinition,
    output: &ValidatedOutput,
    target: &bcode_workflow::NodeDefinition,
    generation: u64,
) -> Result<serde_json::Value, WorkflowStoreError> {
    let edge_transform = definition
        .edges
        .iter()
        .find(|edge| edge.from == output.node_id && edge.to == target.id)
        .and_then(|edge| edge.transform.as_ref());
    let input = if let Some(transform) = edge_transform {
        let run_input_json: Option<String> = transaction.query_row(
            "SELECT input_json FROM workflow_runs WHERE run_id = ?1",
            [&output.run_id],
            |row| row.get(0),
        )?;
        let run_input = run_input_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or(serde_json::Value::Null);
        let join_members = (target.kind == bcode_workflow::NodeKind::Parallel)
            .then(|| {
                parallel_join_members(transaction, definition, &output.run_id, target, generation)
            })
            .transpose()?;
        let mut inputs = vec![
            bcode_workflow::WorkflowTransformInput {
                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                value: &output.value,
            },
            bcode_workflow::WorkflowTransformInput {
                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                value: &run_input,
            },
        ];
        if let Some((left, right)) = &join_members {
            inputs.extend([
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_JOIN_LEFT,
                    value: left,
                },
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_JOIN_RIGHT,
                    value: right,
                },
            ]);
        }
        transform
            .evaluate(&inputs)
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
    } else if target.kind == bcode_workflow::NodeKind::Parallel {
        let (left, right) =
            parallel_join_members(transaction, definition, &output.run_id, target, generation)?;
        serde_json::Value::Array(vec![left, right])
    } else {
        output.value.clone()
    };
    validate_json_schema(
        &format!("workflow activation input for node {}", target.id),
        &target.input.schema,
        &input,
    )
    .map_err(|error| {
        WorkflowStoreError::TargetInputValidation(Box::new(TargetInputValidationFailure {
            run_id: output.run_id.clone(),
            source_node_id: output.node_id.clone(),
            source_activation_id: output.activation_id.clone(),
            target_node_id: target.id.clone(),
            target_schema_id: target.input.type_name.clone(),
            message: error.to_string(),
        }))
    })?;
    Ok(input)
}

#[allow(clippy::too_many_lines)]
fn materialize_direct_successors<F>(
    transaction: &Transaction<'_>,
    output: &ValidatedOutput,
    fault: &F,
) -> Result<(Vec<NewActivation>, bool), WorkflowStoreError>
where
    F: WorkflowOutputFault + ?Sized,
{
    let (definition_json, generation): (String, u64) = transaction.query_row(
        "SELECT definition.definition_json, activation.dependency_generation \
         FROM workflow_runs run \
         JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
           AND definition.version = run.definition_version \
         JOIN workflow_activations activation ON activation.run_id = run.run_id \
           AND activation.node_id = ?2 AND activation.activation_id = ?3 \
         WHERE run.run_id = ?1",
        (&output.run_id, &output.node_id, &output.activation_id),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
    let completed_is_exit = definition
        .exits
        .iter()
        .any(|node_id| node_id == &output.node_id);
    let mut targets = definition
        .edges
        .iter()
        .filter(|edge| {
            edge.from == output.node_id && matches!(edge.kind, bcode_workflow::EdgeKind::Direct)
        })
        .map(|edge| edge.to.clone())
        .collect::<Vec<_>>();
    for target in &targets {
        if definition
            .node(target)
            .is_some_and(|node| node.kind == bcode_workflow::NodeKind::Parallel)
        {
            bcode_workflow::validate_parallel_join_configuration(
                &definition,
                definition.node(target).expect("checked"),
            )
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        }
    }
    if let Some(branch) = definition
        .node(&output.node_id)
        .filter(|node| node.kind == bcode_workflow::NodeKind::Branch)
    {
        let expression: bcode_workflow::PredicateExpression =
            serde_json::from_value(branch.configuration.get("predicate").cloned().ok_or_else(
                || {
                    WorkflowStoreError::InvalidData(
                        "workflow branch configuration is missing predicate".to_string(),
                    )
                },
            )?)?;
        let selected = evaluate_predicate(&expression, &output.value)?;
        let selected_entries = configured_node_ids(
            &branch.configuration,
            if selected {
                "true_entries"
            } else {
                "false_entries"
            },
        )?;
        let skipped_nodes = configured_node_ids(
            &branch.configuration,
            if selected {
                "false_nodes"
            } else {
                "true_nodes"
            },
        )?;
        skip_branch_nodes(
            transaction,
            &output.run_id,
            generation,
            &skipped_nodes,
            output.created_at_ms,
        )?;
        targets.extend(selected_entries.iter().cloned());
        transaction.execute(
            "INSERT INTO workflow_decisions \
             (decision_id, run_id, node_id, decision_type, value_json, created_at_ms) \
             VALUES (?1, ?2, ?3, 'branch', ?4, ?5)",
            (
                format!("{}:branch", output.activation_id),
                &output.run_id,
                &output.node_id,
                serde_json::to_string(&serde_json::json!({
                    "predicate_version": bcode_workflow::WORKFLOW_PREDICATE_VERSION,
                    "selected": selected,
                    "predicate": expression,
                    "selected_entries": selected_entries,
                    "skipped_nodes": skipped_nodes,
                }))?,
                output.created_at_ms,
            ),
        )?;
        fault.after_boundary(WorkflowOutputBoundary::BranchDecisionPersisted, output)?;
    }
    targets.sort();
    targets.dedup();
    let mut activated = Vec::new();
    for node_id in targets {
        let dependencies = definition
            .edges
            .iter()
            .filter(|edge| edge.to == node_id)
            .filter(|edge| {
                matches!(edge.kind, bcode_workflow::EdgeKind::Direct)
                    || (edge.from == output.node_id
                        && matches!(edge.kind, bcode_workflow::EdgeKind::Conditional { .. }))
            })
            .map(|edge| edge.from.as_str())
            .collect::<Vec<_>>();
        let mut ready = true;
        for dependency in dependencies {
            let completed = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_activations WHERE run_id = ?1 \
                 AND node_id = ?2 AND dependency_generation = ?3 \
                 AND status IN ('completed', 'skipped'))",
                (&output.run_id, dependency, generation),
                |row| row.get::<_, bool>(0),
            )?;
            if !completed {
                ready = false;
                break;
            }
        }
        if !ready {
            continue;
        }
        let target = definition.node(&node_id).ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow successor node is missing: {node_id}"
            ))
        })?;
        let input = activation_input(transaction, &definition, output, target, generation)?;
        let activation = NewActivation {
            run_id: output.run_id.clone(),
            node_id: node_id.clone(),
            activation_id: activation_identity(&output.run_id, &node_id, generation),
            dependency_generation: generation,
            input: Some(input),
            created_at_ms: output.created_at_ms,
        };
        let node = definition.node(&node_id).ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow successor references missing node: {node_id}"
            ))
        })?;
        let status = activation_status_for_node(node);
        let changed = transaction.execute(
            "INSERT INTO workflow_activations \
             (run_id, node_id, activation_id, dependency_generation, input_json, status, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(run_id, node_id, activation_id) DO NOTHING",
            (
                &activation.run_id,
                &activation.node_id,
                &activation.activation_id,
                activation.dependency_generation,
                activation
                    .input
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                status,
                activation.created_at_ms,
            ),
        )?;
        if changed == 1 {
            append_event(
                transaction,
                &activation.run_id,
                if status == "pending" {
                    "activation_created"
                } else {
                    "activation_waiting"
                },
                &serde_json::json!({"activation": activation, "status": status}).to_string(),
                activation.created_at_ms,
            )?;
            activated.push(activation);
        }
    }
    Ok((activated, completed_is_exit))
}

fn prepared_read_only_dispatches(
    connection: &Connection,
    limit: i64,
) -> Result<Vec<PreparedActivationDispatch>, WorkflowStoreError> {
    let mut statement = connection.prepare(
        "SELECT attempt.run_id, attempt.node_id, attempt.activation_id, attempt.attempt, \
         attempt.dispatch_identity, activation.dependency_generation, activation.input_json, \
         activation.created_at_ms, definition.definition_json, attempt.intent_json \
         FROM workflow_attempts attempt \
         JOIN workflow_activations activation ON activation.run_id = attempt.run_id \
           AND activation.node_id = attempt.node_id \
           AND activation.activation_id = attempt.activation_id \
         JOIN workflow_runs run ON run.run_id = attempt.run_id \
         JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
           AND definition.version = run.definition_version \
         WHERE attempt.status = 'prepared' AND attempt.side_effect = 'read_only' \
           AND attempt.receipt_json IS NULL AND run.status = 'running' \
           AND run.cancellation_requested_at_ms IS NULL \
         ORDER BY attempt.prepared_at_ms, attempt.dispatch_identity LIMIT ?1",
    )?;
    statement
        .query_map([limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u32>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, u64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
            ))
        })?
        .map(|row| {
            let (
                run_id,
                node_id,
                activation_id,
                attempt,
                dispatch_identity,
                dependency_generation,
                input_json,
                created_at_ms,
                definition_json,
                intent_json,
            ) = row?;
            let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
            let node = definition.node(&node_id).cloned().ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "prepared workflow attempt references missing node: {node_id}"
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
    row: &rusqlite::Row<'_>,
) -> Result<AttemptReconciliationRow, rusqlite::Error> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn receipt_backed_attempt(
    connection: &Connection,
    dispatch_identity: &str,
) -> Result<Option<AttemptReconciliationRequest>, WorkflowStoreError> {
    connection
        .query_row(
            "SELECT run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, \
             receipt_json FROM workflow_attempts WHERE dispatch_identity = ?1 \
             AND status IN ('admitted', 'running', 'cancelling') AND receipt_json IS NOT NULL",
            [dispatch_identity],
            attempt_reconciliation_row,
        )
        .optional()?
        .map(attempt_reconciliation_request)
        .transpose()
}

fn receipt_backed_attempts(
    connection: &Connection,
    limit: i64,
) -> Result<Vec<AttemptReconciliationRequest>, WorkflowStoreError> {
    let mut statement = connection.prepare(
        "SELECT run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, \
         receipt_json FROM workflow_attempts WHERE status IN ('admitted', 'running', 'cancelling') \
         AND receipt_json IS NOT NULL ORDER BY prepared_at_ms, dispatch_identity LIMIT ?1",
    )?;
    statement
        .query_map([limit], attempt_reconciliation_row)?
        .map(|row| attempt_reconciliation_request(row?))
        .collect()
}

fn cancellation_requested_for_run(
    connection: &Connection,
    run_id: &str,
) -> Result<bool, WorkflowStoreError> {
    connection
        .query_row(
            "SELECT cancellation_requested_at_ms IS NOT NULL FROM workflow_runs WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .map_err(WorkflowStoreError::from)
}

fn parallel_member_ids(
    join: &bcode_workflow::NodeDefinition,
    member_node_id: &str,
) -> Result<Vec<String>, WorkflowStoreError> {
    let left = configured_node_ids(&join.configuration, "left_exits")?;
    let right = configured_node_ids(&join.configuration, "right_exits")?;
    if left.iter().any(|member| member == member_node_id)
        && right.iter().any(|member| member == member_node_id)
    {
        return Err(WorkflowStoreError::InvalidData(format!(
            "parallel join {} ambiguously includes member {} on both sides",
            join.id, member_node_id
        )));
    }
    Ok(left.into_iter().chain(right).collect())
}

fn settle_wait_all_parallel_failure(
    transaction: &Transaction<'_>,
    run_id: &str,
    member_node_id: &str,
    settled_at_ms: u64,
) -> Result<Option<bool>, WorkflowStoreError> {
    let (definition_json, generation): (String, u64) = transaction.query_row(
        "SELECT definition.definition_json, activation.dependency_generation \
         FROM workflow_runs run \
         JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
           AND definition.version = run.definition_version \
         JOIN workflow_activations activation ON activation.run_id = run.run_id \
           AND activation.node_id = ?2 \
         WHERE run.run_id = ?1",
        (run_id, member_node_id),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
    for join in definition
        .nodes
        .values()
        .filter(|node| node.kind == bcode_workflow::NodeKind::Parallel)
    {
        let members = parallel_member_ids(join, member_node_id)?;
        if !members.iter().any(|member| member == member_node_id) {
            continue;
        }
        let policy: bcode_workflow::ParallelFailurePolicy = serde_json::from_value(
            join.configuration
                .get("failure_policy")
                .cloned()
                .unwrap_or_else(|| serde_json::json!("wait_all")),
        )?;
        if policy != bcode_workflow::ParallelFailurePolicy::WaitAll {
            return Err(WorkflowStoreError::InvalidData(format!(
                "unsupported durable parallel failure policy for {}: {policy:?}",
                join.id
            )));
        }
        let mut outcomes = Vec::with_capacity(members.len());
        let mut all_terminal = true;
        let mut has_failure = false;
        for member in members {
            let status: Option<String> = transaction
                .query_row(
                    "SELECT status FROM workflow_activations WHERE run_id = ?1 AND node_id = ?2 \
                     AND dependency_generation = ?3",
                    (run_id, &member, generation),
                    |row| row.get(0),
                )
                .optional()?;
            let terminal = status.as_deref().is_some_and(|status| {
                matches!(status, "completed" | "skipped" | "failed" | "cancelled")
            });
            all_terminal &= terminal;
            has_failure |= status
                .as_deref()
                .is_some_and(|status| matches!(status, "failed" | "cancelled"));
            outcomes.push(serde_json::json!({"node_id": member, "status": status}));
        }
        if all_terminal && has_failure {
            let value = serde_json::json!({
                "policy": policy,
                "generation": generation,
                "members": outcomes,
                "outcome": "failed",
            });
            transaction.execute(
                "INSERT INTO workflow_decisions \
                 (decision_id, run_id, node_id, decision_type, value_json, created_at_ms) \
                 VALUES (?1, ?2, ?3, 'parallel_wait_all', ?4, ?5) \
                 ON CONFLICT(decision_id) DO NOTHING",
                (
                    format!("{run_id}:{}:{generation}:parallel-wait-all", join.id),
                    run_id,
                    &join.id,
                    serde_json::to_string(&value)?,
                    settled_at_ms,
                ),
            )?;
            transaction.execute(
                "UPDATE workflow_runs SET status = 'failed', updated_at_ms = ?2 \
                 WHERE run_id = ?1 AND status = 'running'",
                (run_id, settled_at_ms),
            )?;
            append_event(
                transaction,
                run_id,
                "parallel_join_failed",
                &serde_json::json!({"join_node_id": join.id, "decision": value}).to_string(),
                settled_at_ms,
            )?;
            return Ok(Some(true));
        }
        return Ok(Some(false));
    }
    Ok(None)
}

#[allow(clippy::too_many_lines)]
fn apply_attempt_observation(
    transaction: &Transaction<'_>,
    request: &AttemptReconciliationRequest,
    mut observation: AttemptObservation,
    reconciled_at_ms: u64,
    summary: &mut ReceiptReconciliationSummary,
) -> Result<(), WorkflowStoreError> {
    if cancellation_requested_for_run(transaction, &request.run_id)? {
        observation = AttemptObservation::Cancelled;
    }
    match observation {
        AttemptObservation::Admitted => {
            transition_attempt(transaction, request, "admitted", None)?;
            summary.admitted.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Running => {
            transition_attempt(transaction, request, "running", None)?;
            summary.running.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Succeeded { output } => {
            validate_observed_output(request, &output)?;
            persist_validated_output_transaction(transaction, &output)?;
            transition_attempt(transaction, request, "succeeded", Some(reconciled_at_ms))?;
            summary.succeeded.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Failed { message } => {
            transition_attempt(transaction, request, "failed", Some(reconciled_at_ms))?;
            let activation_changed = transaction.execute(
                "UPDATE workflow_activations SET status = 'failed' \
                 WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
                   AND status = 'running' AND output_id IS NULL",
                (&request.run_id, &request.node_id, &request.activation_id),
            )?;
            if activation_changed != 1 {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "failed workflow attempt has no running output-free activation: {}",
                    request.dispatch_identity
                )));
            }
            let parallel_state = settle_wait_all_parallel_failure(
                transaction,
                &request.run_id,
                &request.node_id,
                reconciled_at_ms,
            )?;
            if parallel_state.is_none() {
                transaction.execute(
                    "UPDATE workflow_runs SET status = 'failed', updated_at_ms = ?2 \
                     WHERE run_id = ?1 AND status = 'running'",
                    (&request.run_id, reconciled_at_ms),
                )?;
            }
            append_event(
                transaction,
                &request.run_id,
                "attempt_failed",
                &serde_json::json!({
                    "dispatch_identity": request.dispatch_identity,
                    "message": message,
                })
                .to_string(),
                reconciled_at_ms,
            )?;
            summary.failed.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Paused { reason, message } => {
            apply_paused_attempt_observation(
                transaction,
                request,
                reason,
                &message,
                reconciled_at_ms,
            )?;
            summary.paused.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Cancelled => {
            transition_attempt(transaction, request, "cancelled", Some(reconciled_at_ms))?;
            transaction.execute(
                "UPDATE workflow_activations SET status = 'cancelled' \
                 WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
                   AND status = 'running' AND output_id IS NULL",
                (&request.run_id, &request.node_id, &request.activation_id),
            )?;
            finalize_run_cancellation_if_settled(transaction, &request.run_id, reconciled_at_ms)?;
            append_event(
                transaction,
                &request.run_id,
                "attempt_cancelled",
                &serde_json::json!({"dispatch_identity": request.dispatch_identity}).to_string(),
                reconciled_at_ms,
            )?;
            summary.cancelled.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Unknown => {
            if request.side_effect == DispatchSideEffect::Mutating {
                transition_attempt(
                    transaction,
                    request,
                    "repair_required",
                    Some(reconciled_at_ms),
                )?;
                transaction.execute(
                    "UPDATE workflow_runs SET status = ?2, updated_at_ms = ?3 WHERE run_id = ?1",
                    (
                        &request.run_id,
                        RunStatus::RepairRequired.as_str(),
                        reconciled_at_ms,
                    ),
                )?;
                summary
                    .repair_required
                    .push(request.dispatch_identity.clone());
            } else {
                summary
                    .unresolved_read_only
                    .push(request.dispatch_identity.clone());
            }
        }
    }
    Ok(())
}

fn apply_paused_attempt_observation(
    transaction: &Transaction<'_>,
    request: &AttemptReconciliationRequest,
    reason: AttemptPauseReason,
    message: &str,
    reconciled_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    transition_attempt(transaction, request, "paused", Some(reconciled_at_ms))?;
    let activation_changed = transaction.execute(
        "UPDATE workflow_activations SET status = 'pending' \
         WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 \
           AND status = 'running' AND output_id IS NULL",
        (&request.run_id, &request.node_id, &request.activation_id),
    )?;
    if activation_changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "paused workflow attempt has no running output-free activation: {}",
            request.dispatch_identity
        )));
    }
    let run_changed = transaction.execute(
        "UPDATE workflow_runs SET status = 'paused', updated_at_ms = ?2 \
         WHERE run_id = ?1 AND status = 'running' \
           AND cancellation_requested_at_ms IS NULL",
        (&request.run_id, reconciled_at_ms),
    )?;
    if run_changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "paused workflow attempt has no resumable running run: {}",
            request.dispatch_identity
        )));
    }
    append_event(
        transaction,
        &request.run_id,
        "attempt_paused",
        &serde_json::json!({
            "dispatch_identity": request.dispatch_identity,
            "reason": reason,
            "message": message,
        })
        .to_string(),
        reconciled_at_ms,
    )?;
    Ok(())
}

fn transition_attempt(
    transaction: &Transaction<'_>,
    request: &AttemptReconciliationRequest,
    status: &str,
    terminal_at_ms: Option<u64>,
) -> Result<(), WorkflowStoreError> {
    let changed = transaction.execute(
        "UPDATE workflow_attempts SET status = ?2, terminal_at_ms = COALESCE(?3, terminal_at_ms) \
         WHERE dispatch_identity = ?1 AND status IN ('admitted', 'running', 'cancelling')",
        (&request.dispatch_identity, status, terminal_at_ms),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "attempt cannot transition during reconciliation: {}",
            request.dispatch_identity
        )));
    }
    Ok(())
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

fn pending_activation_by_identity(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<PendingActivation>, WorkflowStoreError> {
    let row = connection
        .query_row(
            "SELECT activation.dependency_generation, activation.input_json, \
             activation.created_at_ms, definition.definition_json \
             FROM workflow_activations activation \
             JOIN workflow_runs run ON run.run_id = activation.run_id \
             JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
               AND definition.version = run.definition_version \
             WHERE activation.run_id = ?1 AND activation.node_id = ?2 \
               AND activation.activation_id = ?3 AND activation.status = 'pending' \
               AND run.status = 'running' AND run.cancellation_requested_at_ms IS NULL",
            (run_id, node_id, activation_id),
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(dependency_generation, input_json, created_at_ms, definition_json)| {
            let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
            let node = definition.node(node_id).cloned().ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow activation references missing node: {node_id}"
                ))
            })?;
            Ok(PendingActivation {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                activation_id: activation_id.to_string(),
                dependency_generation,
                input: input_json
                    .map(|json| serde_json::from_str(&json))
                    .transpose()?,
                node,
                created_at_ms,
            })
        },
    )
    .transpose()
}

fn enforce_activation_limits(
    connection: &Connection,
    activation: &NewActivation,
) -> Result<(), WorkflowStoreError> {
    let (cycle_cap, status, cancellation_requested): (u64, String, bool) = connection.query_row(
        "SELECT cycle_cap, status, cancellation_requested_at_ms IS NOT NULL \
         FROM workflow_runs WHERE run_id = ?1",
        [&activation.run_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if cancellation_requested || status != RunStatus::Running.as_str() {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow run does not accept activations while {status}"
        )));
    }
    if activation.dependency_generation >= cycle_cap {
        return Err(WorkflowStoreError::InvalidData(
            "workflow cycle cap exceeded".to_string(),
        ));
    }
    Ok(())
}

fn enforce_attempt_limits(
    connection: &Connection,
    attempt: &PreparedAttempt,
) -> Result<(), WorkflowStoreError> {
    let (
        deadline_at_ms,
        node_execution_cap,
        concurrency_cap,
        retry_cap,
        cancellation_requested,
        run_status,
    ): (Option<u64>, u32, u32, u32, bool, String) = connection.query_row(
        "SELECT deadline_at_ms, node_execution_cap, concurrency_cap, retry_cap, \
         cancellation_requested_at_ms IS NOT NULL, status FROM workflow_runs WHERE run_id = ?1",
        [&attempt.run_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    if run_status != RunStatus::Running.as_str() {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow run does not accept attempts while {run_status}"
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
    let execution_count: u32 = connection.query_row(
        "SELECT COUNT(*) FROM workflow_attempts WHERE run_id = ?1",
        [&attempt.run_id],
        |row| row.get(0),
    )?;
    if execution_count >= node_execution_cap {
        return Err(WorkflowStoreError::InvalidData(
            "workflow node-execution cap exceeded".to_string(),
        ));
    }
    let active_count: u32 = connection.query_row(
        "SELECT COUNT(*) FROM workflow_attempts WHERE run_id = ?1 \
         AND status IN ('prepared', 'admitted', 'running', 'cancelling')",
        [&attempt.run_id],
        |row| row.get(0),
    )?;
    if active_count >= concurrency_cap {
        return Err(WorkflowStoreError::InvalidData(
            "workflow concurrency cap reached".to_string(),
        ));
    }
    Ok(())
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
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    String,
    Option<u64>,
    u64,
    u64,
);

fn run_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRunSummary> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn parse_run_summary(raw: RawRunSummary) -> Result<WorkflowRunSummary, WorkflowStoreError> {
    let (
        run_id,
        definition_id,
        definition_version,
        workspace_snapshot,
        parent_session_id,
        owner_plugin_id,
        workflow_kind,
        scope_key,
        display_label,
        single_active,
        status,
        cancellation_requested_at_ms,
        created_at_ms,
        updated_at_ms,
    ) = raw;
    let binding = match (owner_plugin_id, workflow_kind, scope_key) {
        (Some(owner_plugin_id), Some(workflow_kind), Some(scope_key)) => Some(WorkflowRunBinding {
            owner_plugin_id,
            workflow_kind,
            scope_key,
            display_label,
            single_active,
        }),
        (None, None, None) => None,
        _ => {
            return Err(WorkflowStoreError::InvalidData(
                "stored workflow run has an incomplete binding".to_string(),
            ));
        }
    };
    Ok(WorkflowRunSummary {
        run_id,
        definition_id,
        definition_version,
        workspace_snapshot,
        parent_session_id,
        binding,
        status: parse_run_status(&status)?,
        cancellation_requested_at_ms,
        created_at_ms,
        updated_at_ms,
    })
}

fn parse_run_status(status: &str) -> Result<RunStatus, WorkflowStoreError> {
    match status {
        "running" => Ok(RunStatus::Running),
        "paused" => Ok(RunStatus::Paused),
        "completed" => Ok(RunStatus::Completed),
        "failed" => Ok(RunStatus::Failed),
        "cancelled" => Ok(RunStatus::Cancelled),
        "repair_required" => Ok(RunStatus::RepairRequired),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "unknown workflow run status: {status}"
        ))),
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

fn attempt_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawAttemptSummary> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn parse_side_effect(side_effect: &str) -> Result<DispatchSideEffect, WorkflowStoreError> {
    match side_effect {
        "read_only" => Ok(DispatchSideEffect::ReadOnly),
        "mutating" => Ok(DispatchSideEffect::Mutating),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "unknown workflow side effect: {side_effect}"
        ))),
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
    let side_effect = parse_side_effect(&side_effect)?;
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
    transaction: &Transaction<'_>,
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

fn insert_activation_with_status(
    transaction: &Transaction<'_>,
    activation: &NewActivation,
    status: &str,
) -> Result<(), WorkflowStoreError> {
    let input = activation.input.as_ref().ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "workflow activation input is required: {}/{}/{}",
            activation.run_id, activation.node_id, activation.activation_id
        ))
    })?;
    let definition_json: String = transaction.query_row(
        "SELECT definition.definition_json FROM workflow_runs run \
         JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
           AND definition.version = run.definition_version WHERE run.run_id = ?1",
        [&activation.run_id],
        |row| row.get(0),
    )?;
    let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
    let node = definition.node(&activation.node_id).ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "workflow activation references missing node: {}",
            activation.node_id
        ))
    })?;
    validate_json_schema(
        &format!("workflow activation input for node {}", activation.node_id),
        &node.input.schema,
        input,
    )?;
    transaction.execute(
        "INSERT INTO workflow_activations \
         (run_id, node_id, activation_id, dependency_generation, input_json, status, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
            &activation.run_id,
            &activation.node_id,
            &activation.activation_id,
            activation.dependency_generation,
            activation
                .input
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            status,
            activation.created_at_ms,
        ),
    )?;
    let event_type = if status == "pending" {
        "activation_created"
    } else {
        "activation_waiting"
    };
    append_event(
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

fn append_event(
    transaction: &Transaction<'_>,
    run_id: &str,
    event_type: &str,
    payload_json: &str,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    transaction.execute(
        "INSERT INTO workflow_events (run_id, event_type, payload_json, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4)",
        (run_id, event_type, payload_json, created_at_ms),
    )?;
    Ok(())
}

type RepairRequiredAttempt = (String, String, String, u32, String);

fn repair_required_attempt(
    transaction: &Transaction<'_>,
    dispatch_identity: &str,
) -> Result<RepairRequiredAttempt, WorkflowStoreError> {
    transaction
        .query_row(
            "SELECT run_id, node_id, activation_id, attempt, side_effect \
             FROM workflow_attempts WHERE dispatch_identity = ?1 AND status = 'repair_required'",
            [dispatch_identity],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "repair-required workflow attempt not found: {dispatch_identity}"
            ))
        })
}

fn finalize_run_cancellation_if_settled(
    transaction: &Transaction<'_>,
    run_id: &str,
    cancelled_at_ms: u64,
) -> Result<bool, WorkflowStoreError> {
    let active_attempts: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_attempts WHERE run_id = ?1 \
         AND status IN ('prepared', 'admitted', 'running', 'cancelling'))",
        [run_id],
        |row| row.get(0),
    )?;
    if active_attempts {
        return Ok(false);
    }
    let changed = transaction.execute(
        "UPDATE workflow_runs SET status = 'cancelled', updated_at_ms = ?2 \
         WHERE run_id = ?1 AND status IN ('running', 'paused') \
           AND cancellation_requested_at_ms IS NOT NULL",
        (run_id, cancelled_at_ms),
    )?;
    if changed == 1 {
        transaction.execute(
            "UPDATE workflow_activations SET status = 'cancelled' \
             WHERE run_id = ?1 AND status IN ('pending', 'running', 'waiting_input', \
             'waiting_approval', 'waiting_mutation_approval') AND output_id IS NULL",
            [run_id],
        )?;
        append_event(transaction, run_id, "run_cancelled", "{}", cancelled_at_ms)?;
    }
    Ok(changed == 1)
}

fn transition_run_control_state(
    connection: &mut Connection,
    run_id: &str,
    expected: RunStatus,
    target: RunStatus,
    event_type: &str,
    changed_at_ms: u64,
) -> Result<bool, WorkflowStoreError> {
    validate_id("run_id", run_id)?;
    let transaction = connection.transaction()?;
    let (status, cancellation_requested) = transaction
        .query_row(
            "SELECT status, cancellation_requested_at_ms IS NOT NULL FROM workflow_runs WHERE run_id = ?1",
            [run_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()?
        .ok_or_else(|| WorkflowStoreError::RunNotFound {
            run_id: run_id.to_string(),
        })?;
    let status = parse_run_status(&status)?;
    if cancellation_requested {
        return Err(WorkflowStoreError::CancellationPreventsControl);
    }
    if status == target {
        return Ok(false);
    }
    if status != expected {
        return Err(WorkflowStoreError::InvalidRunTransition {
            current: status,
            target,
        });
    }
    transaction.execute(
        "UPDATE workflow_runs SET status = ?2, updated_at_ms = ?3 WHERE run_id = ?1",
        (run_id, target.as_str(), changed_at_ms),
    )?;
    append_event(
        &transaction,
        run_id,
        event_type,
        &serde_json::json!({"status": target.as_str()}).to_string(),
        changed_at_ms,
    )?;
    transaction.commit()?;
    Ok(true)
}

fn require_cancellation_requested(
    connection: &Connection,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    let cancellation_requested = connection
        .query_row(
            "SELECT cancellation_requested_at_ms IS NOT NULL FROM workflow_runs WHERE run_id = ?1",
            [run_id],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("workflow run not found: {run_id}"))
        })?;
    if !cancellation_requested {
        return Err(WorkflowStoreError::InvalidData(
            "workflow cancellation intent must be persisted before signaling children".to_string(),
        ));
    }
    Ok(())
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
    transaction: &Transaction<'_>,
    lease_id: &str,
) -> Result<Option<(WorkflowResourceLease, Option<u64>)>, WorkflowStoreError> {
    transaction
        .query_row(
            "SELECT run_id, node_id, activation_id, resource_key, mode, acquired_at_ms, expires_at_ms, \
             released_at_ms FROM workflow_resource_leases WHERE lease_id = ?1",
            [lease_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, Option<u64>>(6)?,
                    row.get::<_, Option<u64>>(7)?,
                ))
            },
        )
        .optional()?
        .map(
            |(
                run_id,
                node_id,
                activation_id,
                resource_key,
                mode,
                acquired_at_ms,
                expires_at_ms,
                released_at_ms,
            )| {
                Ok((
                    WorkflowResourceLease {
                        lease_id: lease_id.to_string(),
                        run_id,
                        node_id,
                        activation_id,
                        resource_key,
                        mode: parse_resource_lease_mode(&mode)?,
                        acquired_at_ms,
                        expires_at_ms,
                    },
                    released_at_ms,
                ))
            },
        )
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

fn validate_run(run: &NewWorkflowRun) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", &run.run_id)?;
    validate_id("definition_id", &run.definition_id)?;
    validate_id("workspace_snapshot", &run.workspace_snapshot)?;
    validate_run_limits(&run.limits)?;
    if run.definition_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "definition version must be positive".to_string(),
        ));
    }
    if let Some(parent_session_id) = &run.parent_session_id {
        validate_id("parent_session_id", parent_session_id)?;
    }
    if let Some(binding) = &run.binding {
        validate_binding(binding)?;
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
    for (label, value) in [
        ("node_execution_cap", limits.node_execution_cap),
        ("concurrency_cap", limits.concurrency_cap),
        ("cycle_cap", limits.cycle_cap),
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
    transaction: &Transaction<'_>,
    output: &ValidatedOutput,
) -> Result<(), WorkflowStoreError> {
    let definition_json: String = transaction.query_row(
        "SELECT definition.definition_json FROM workflow_runs run \
         JOIN workflow_definitions definition ON definition.definition_id = run.definition_id \
           AND definition.version = run.definition_version \
         WHERE run.run_id = ?1",
        [&output.run_id],
        |row| row.get(0),
    )?;
    let definition: WorkflowDefinition = serde_json::from_str(&definition_json)?;
    let node = definition.node(&output.node_id).ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "validated output references missing node: {}",
            output.node_id
        ))
    })?;
    if output.schema_id != node.output.type_name {
        return Err(WorkflowStoreError::InvalidData(format!(
            "validated output schema identity mismatch for node {}: expected {}, received {}",
            output.node_id, node.output.type_name, output.schema_id
        )));
    }
    let validator = jsonschema::validator_for(&node.output.schema).map_err(|error| {
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
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<AutomaticRetrySchedule>, WorkflowStoreError> {
    connection
        .query_row(
            "SELECT failed_attempt, next_attempt, failure_kind, backoff_ms, next_attempt_at_ms, \
             scheduled_at_ms FROM workflow_retry_schedules \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3",
            (run_id, node_id, activation_id),
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, u64>(5)?,
                ))
            },
        )
        .optional()?
        .map(
            |(
                failed_attempt,
                next_attempt,
                failure_kind,
                backoff_ms,
                next_attempt_at_ms,
                scheduled_at_ms,
            )| {
                Ok(AutomaticRetrySchedule {
                    run_id: run_id.to_string(),
                    node_id: node_id.to_string(),
                    activation_id: activation_id.to_string(),
                    failed_attempt,
                    next_attempt,
                    failure_kind: serde_json::from_str(&failure_kind)?,
                    backoff_ms,
                    next_attempt_at_ms,
                    scheduled_at_ms,
                })
            },
        )
        .transpose()
}

fn persist_definition_transaction(
    transaction: &Transaction<'_>,
    stored: &StoredWorkflowDefinition,
) -> Result<(), WorkflowStoreError> {
    let existing = transaction
        .query_row(
            "SELECT checksum_sha256 FROM workflow_definitions \
             WHERE definition_id = ?1 AND version = ?2",
            (&stored.definition_id, stored.version),
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing == stored.checksum_sha256 {
            return Ok(());
        }
        return Err(WorkflowStoreError::InvalidData(format!(
            "definition identity conflict: {} v{}",
            stored.definition_id, stored.version
        )));
    }
    transaction.execute(
        "INSERT INTO workflow_definitions \
         (definition_id, version, checksum_sha256, definition_json) VALUES (?1, ?2, ?3, ?4)",
        (
            &stored.definition_id,
            stored.version,
            &stored.checksum_sha256,
            &stored.definition_json,
        ),
    )?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn migrate(connection: &mut Connection) -> Result<(), WorkflowStoreError> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_store_contract (\
             contract_id INTEGER PRIMARY KEY CHECK (contract_id = 1),\
             schema_version INTEGER NOT NULL\
         );\
         INSERT OR IGNORE INTO workflow_store_contract (contract_id, schema_version) VALUES (1, 3);\
         UPDATE workflow_store_contract SET schema_version = 2 WHERE contract_id = 1 AND schema_version = 1;\
         UPDATE workflow_store_contract SET schema_version = 3 WHERE contract_id = 1 AND schema_version = 2;\
         CREATE TABLE IF NOT EXISTS workflow_definitions (\
             definition_id TEXT NOT NULL,\
             version INTEGER NOT NULL CHECK (version > 0),\
             checksum_sha256 TEXT NOT NULL,\
             definition_json TEXT NOT NULL,\
             PRIMARY KEY (definition_id, version)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_runs (\
             run_id TEXT PRIMARY KEY NOT NULL,\
             definition_id TEXT NOT NULL,\
             definition_version INTEGER NOT NULL,\
             workspace_snapshot TEXT NOT NULL,\
             parent_session_id TEXT,\
             owner_plugin_id TEXT,\
             workflow_kind TEXT,\
             scope_key TEXT,\
             display_label TEXT,\
             single_active INTEGER NOT NULL DEFAULT 0,\
             input_json TEXT,\
             status TEXT NOT NULL,\
             cancellation_requested_at_ms INTEGER,\
             deadline_at_ms INTEGER,\
             node_execution_cap INTEGER NOT NULL,\
             concurrency_cap INTEGER NOT NULL,\
             cycle_cap INTEGER NOT NULL,\
             retry_cap INTEGER NOT NULL,\
             created_at_ms INTEGER NOT NULL,\
             updated_at_ms INTEGER NOT NULL,\
             FOREIGN KEY (definition_id, definition_version)\
                 REFERENCES workflow_definitions(definition_id, version)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_activations (\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             dependency_generation INTEGER NOT NULL,\
             input_json TEXT,\
             status TEXT NOT NULL,\
             output_id TEXT,\
             created_at_ms INTEGER NOT NULL,\
             PRIMARY KEY (run_id, node_id, activation_id),\
             FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_attempts (\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             attempt INTEGER NOT NULL CHECK (attempt > 0),\
             dispatch_identity TEXT NOT NULL UNIQUE,\
             side_effect TEXT NOT NULL,\
             status TEXT NOT NULL,\
             intent_json TEXT NOT NULL,\
             intent_checksum TEXT NOT NULL,\
             receipt_json TEXT,\
             prepared_at_ms INTEGER NOT NULL,\
             admitted_at_ms INTEGER,\
             terminal_at_ms INTEGER,\
             PRIMARY KEY (run_id, node_id, activation_id, attempt),\
             FOREIGN KEY (run_id, node_id, activation_id)\
                 REFERENCES workflow_activations(run_id, node_id, activation_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_retry_schedules (\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             failed_attempt INTEGER NOT NULL CHECK (failed_attempt > 0),\
             next_attempt INTEGER NOT NULL CHECK (next_attempt > failed_attempt),\
             failure_kind TEXT NOT NULL,\
             backoff_ms INTEGER NOT NULL,\
             next_attempt_at_ms INTEGER NOT NULL,\
             scheduled_at_ms INTEGER NOT NULL,\
             PRIMARY KEY (run_id, node_id, activation_id),\
             FOREIGN KEY (run_id, node_id, activation_id)\
                 REFERENCES workflow_activations(run_id, node_id, activation_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_outputs (\
             output_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             schema_id TEXT NOT NULL,\
             schema_version INTEGER NOT NULL CHECK (schema_version > 0),\
             value_json TEXT NOT NULL,\
             artifact_reference TEXT,\
             checksum_sha256 TEXT NOT NULL,\
             created_at_ms INTEGER NOT NULL,\
             FOREIGN KEY (run_id, node_id, activation_id)\
                 REFERENCES workflow_activations(run_id, node_id, activation_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_decisions (\
             decision_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT,\
             decision_type TEXT NOT NULL,\
             value_json TEXT NOT NULL,\
             created_at_ms INTEGER NOT NULL,\
             FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_grants (\
             grant_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             scope_json TEXT NOT NULL,\
             granted_at_ms INTEGER NOT NULL,\
             expires_at_ms INTEGER,\
             FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_mutation_approvals (\
             approval_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             scope_json TEXT NOT NULL,\
             status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'denied', 'cancelled', 'expired')),\
             requested_at_ms INTEGER NOT NULL,\
             resolved_at_ms INTEGER,\
             expires_at_ms INTEGER,\
             grant_id TEXT,\
             FOREIGN KEY (run_id, node_id, activation_id)\
                 REFERENCES workflow_activations(run_id, node_id, activation_id)\
         );\
         CREATE INDEX IF NOT EXISTS idx_workflow_mutation_approvals_pending \
             ON workflow_mutation_approvals(run_id, status, requested_at_ms);\
         CREATE TABLE IF NOT EXISTS workflow_resource_leases (\
             lease_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             resource_key TEXT NOT NULL,\
             mode TEXT NOT NULL CHECK (mode IN ('read', 'write')),\
             acquired_at_ms INTEGER NOT NULL,\
             expires_at_ms INTEGER,\
             released_at_ms INTEGER,\
             FOREIGN KEY (run_id, node_id, activation_id)\
                 REFERENCES workflow_activations(run_id, node_id, activation_id)\
         );\
         CREATE INDEX IF NOT EXISTS idx_workflow_resource_leases_active \
             ON workflow_resource_leases(run_id, resource_key, released_at_ms);\
         CREATE TABLE IF NOT EXISTS workflow_events (\
             event_seq INTEGER PRIMARY KEY AUTOINCREMENT,\
             run_id TEXT NOT NULL,\
             event_type TEXT NOT NULL,\
             payload_json TEXT NOT NULL,\
             created_at_ms INTEGER NOT NULL,\
             FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id)\
         );\
         CREATE INDEX IF NOT EXISTS idx_workflow_events_run_seq \
             ON workflow_events(run_id, event_seq);\
         CREATE TABLE IF NOT EXISTS workflow_projection_checkpoints (\
             projection_name TEXT PRIMARY KEY NOT NULL,\
             projection_version INTEGER NOT NULL,\
             last_event_seq INTEGER NOT NULL\
         );",
    )?;
    let columns = transaction
        .prepare("PRAGMA table_info(workflow_activations)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == "input_json") {
        transaction.execute(
            "ALTER TABLE workflow_activations ADD COLUMN input_json TEXT",
            [],
        )?;
    }
    transaction.execute(
        "UPDATE workflow_store_contract SET schema_version = 4 \
         WHERE contract_id = 1 AND schema_version = 3",
        [],
    )?;
    let run_columns = transaction
        .prepare("PRAGMA table_info(workflow_runs)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    for (column, sql) in [
        (
            "owner_plugin_id",
            "ALTER TABLE workflow_runs ADD COLUMN owner_plugin_id TEXT",
        ),
        (
            "workflow_kind",
            "ALTER TABLE workflow_runs ADD COLUMN workflow_kind TEXT",
        ),
        (
            "scope_key",
            "ALTER TABLE workflow_runs ADD COLUMN scope_key TEXT",
        ),
        (
            "display_label",
            "ALTER TABLE workflow_runs ADD COLUMN display_label TEXT",
        ),
        (
            "single_active",
            "ALTER TABLE workflow_runs ADD COLUMN single_active INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        if !run_columns.iter().any(|existing| existing == column) {
            transaction.execute(sql, [])?;
        }
    }
    transaction.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_workflow_runs_binding_updated \
             ON workflow_runs(owner_plugin_id, workflow_kind, scope_key, updated_at_ms DESC, run_id);",
    )?;
    transaction.execute(
        "UPDATE workflow_store_contract SET schema_version = 5 \
         WHERE contract_id = 1 AND schema_version = 4",
        [],
    )?;
    transaction.execute(
        "UPDATE workflow_store_contract SET schema_version = 6 \
         WHERE contract_id = 1 AND schema_version = 5",
        [],
    )?;
    let actual: u32 = transaction.query_row(
        "SELECT schema_version FROM workflow_store_contract WHERE contract_id = 1",
        [],
        |row| row.get(0),
    )?;
    if actual != SCHEMA_VERSION {
        return Err(WorkflowStoreError::InvalidData(format!(
            "unsupported workflow schema version {actual}; expected {SCHEMA_VERSION}"
        )));
    }
    transaction.commit()?;
    Ok(())
}

fn validate_id(label: &str, value: &str) -> Result<(), WorkflowStoreError> {
    if value.trim().is_empty() || value.len() > MAX_ID_BYTES {
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
mod tests {
    use super::*;
    use bcode_workflow::{Step, WorkflowBuilder};
    use std::collections::BTreeMap;

    fn definition(name: &str) -> WorkflowDefinition {
        WorkflowBuilder::new(
            name,
            Step::task(
                "review",
                |value: u32, _context| async move { Ok(value + 1) },
            ),
        )
        .build()
        .expect("workflow")
        .definition()
        .clone()
    }

    fn sequential_definition() -> WorkflowDefinition {
        WorkflowBuilder::new(
            "sequential",
            Step::task("first", |value: u32, _context| async move { Ok(value + 1) }).then(
                Step::task(
                    "second",
                    |value: u32, _context| async move { Ok(value + 1) },
                ),
            ),
        )
        .build()
        .expect("workflow")
        .definition()
        .clone()
    }

    fn repeat_definition() -> WorkflowDefinition {
        WorkflowBuilder::new(
            "repeat",
            Step::map("body", |mut value: serde_json::Value| {
                value["condition_met"] = serde_json::json!(false);
                Ok(value)
            })
            .repeat_while(
                "repeat-control",
                bcode_workflow::field::<serde_json::Value>("condition_met").eq(false),
                2,
            ),
        )
        .build()
        .expect("workflow")
        .definition()
        .clone()
    }

    fn parallel_join_definition() -> WorkflowDefinition {
        let left = Step::task("left", |value: u32, _context| async move { Ok(value + 1) });
        let right = Step::task("right", |value: u32, _context| async move { Ok(value + 2) });
        let workflow = WorkflowBuilder::new(
            "parallel",
            bcode_workflow::parallel_named("join", left, right),
        )
        .build()
        .expect("workflow");
        assert_eq!(
            workflow.definition().nodes["join"].input,
            bcode_workflow::ValueSchema::of::<(u32, u32)>()
        );
        workflow.definition().clone()
    }

    fn parallel_join_transform_definition() -> WorkflowDefinition {
        let mut definition = parallel_join_definition();
        let tuple_schema = bcode_workflow::ValueSchema::of::<(u32, u32)>();
        for edge in &mut definition.edges {
            if edge.to == "join" {
                edge.transform = Some(bcode_workflow::WorkflowTransform {
                    version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
                    expression: bcode_workflow::WorkflowTransformExpression::Array {
                        items: vec![
                            bcode_workflow::WorkflowTransformExpression::Input {
                                source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_JOIN_LEFT
                                    .to_string(),
                                path: String::new(),
                            },
                            bcode_workflow::WorkflowTransformExpression::Input {
                                source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_JOIN_RIGHT
                                    .to_string(),
                                path: String::new(),
                            },
                        ],
                    },
                    output: tuple_schema.clone(),
                });
            }
        }
        definition
    }

    fn conditional_definition() -> WorkflowDefinition {
        let inspect = Step::task("inspect", |value: u32, _context| async move { Ok(value) });
        let selected = Step::task("selected", |value: u32, _context| async move { Ok(value) });
        let other = Step::task("other", |value: u32, _context| async move { Ok(value) });
        WorkflowBuilder::new(
            "conditional",
            inspect.branch(
                "choose",
                bcode_workflow::field::<u32>("").eq(1_u32),
                selected,
                other,
            ),
        )
        .build()
        .expect("workflow")
        .definition()
        .clone()
    }

    fn conditional_nested_definition() -> WorkflowDefinition {
        let inspect = Step::task("inspect", |value: serde_json::Value, _context| async move {
            Ok(value)
        });
        let selected = Step::task(
            "selected",
            |value: serde_json::Value, _context| async move { Ok(value) },
        );
        let other = Step::task("other", |value: serde_json::Value, _context| async move {
            Ok(value)
        });
        WorkflowBuilder::new(
            "conditional-nested",
            inspect.branch(
                "choose",
                bcode_workflow::field::<serde_json::Value>("review.result.passed").eq(true),
                selected,
                other,
            ),
        )
        .build()
        .expect("workflow")
        .definition()
        .clone()
    }

    fn mutation_approval_definition() -> WorkflowDefinition {
        let schema = bcode_workflow::ValueSchema::of::<serde_json::Value>();
        WorkflowDefinition {
            schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "mutation-approval".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            nodes: BTreeMap::from([(
                "git.commit".to_string(),
                bcode_workflow::NodeDefinition {
                    id: "git.commit".to_string(),
                    name: "commit".to_string(),
                    kind: bcode_workflow::NodeKind::PluginBlock,
                    input: schema.clone(),
                    output: schema,
                    resources: Vec::new(),
                    configuration: serde_json::json!({}),
                },
            )]),
            entries: vec!["git.commit".to_string()],
            exits: vec!["git.commit".to_string()],
            edges: Vec::new(),
        }
    }

    fn mutation_approval(input: &serde_json::Value) -> WorkflowMutationApproval {
        let input_json = serde_json::to_string(input).expect("input");
        WorkflowMutationApproval {
            approval_id: "approval-1".to_string(),
            run_id: "mutation-run".to_string(),
            node_id: "git.commit".to_string(),
            activation_id: activation_identity("mutation-run", "git.commit", 0),
            scope: bcode_workflow::WorkflowMutationGrantScope {
                version: bcode_workflow::WORKFLOW_MUTATION_GRANT_SCOPE_VERSION,
                definition_id: "mutation-definition".to_string(),
                definition_version: 1,
                run_id: "mutation-run".to_string(),
                node_id: "git.commit".to_string(),
                activation_id: activation_identity("mutation-run", "git.commit", 0),
                workspace_snapshot: "snapshot-1".to_string(),
                plugin_id: "bcode.git".to_string(),
                block_id: "git.commit".to_string(),
                block_version: 1,
                operation: "git.commit".to_string(),
                input_checksum_sha256: sha256_hex(input_json.as_bytes()),
                input_summary: input.clone(),
                resource_claims: vec![bcode_workflow::ResourceClaim {
                    resource: "repository".to_string(),
                    access: bcode_workflow::ResourceAccess::Write,
                }],
                reconciliation: bcode_workflow::WorkflowBlockReconciliation::RepairRequired,
                capability: bcode_workflow::WorkflowToolCapability::Mutating,
            },
            requested_at_ms: 20,
            expires_at_ms: Some(100),
        }
    }

    #[test]
    fn mutation_approval_cancellation_settles_wait_without_grant_or_attempt() {
        let temp = tempfile::tempdir().expect("temp");
        let input = serde_json::json!({"expected_head": "abc"});
        let approval = mutation_approval(&input);
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("mutation-definition", 1, &mutation_approval_definition())
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "mutation-run".to_string(),
                definition_id: "mutation-definition".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot-1".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(input),
                created_at_ms: 10,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        store.request_mutation_approval(&approval).expect("request");
        assert!(
            store
                .request_cancellation("mutation-run", 30)
                .expect("cancel")
        );
        assert!(
            store
                .pending_mutation_approvals("mutation-run", 10)
                .expect("approvals")
                .is_empty()
        );
        assert!(
            store
                .grants_for_run("mutation-run", 10)
                .expect("grants")
                .is_empty()
        );
        assert!(
            store
                .attempt_history("mutation-run", None, 10)
                .expect("attempts")
                .is_empty()
        );
        assert_eq!(
            store
                .run_summary("mutation-run")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
        assert!(
            store
                .resolve_mutation_approval(
                    &approval.approval_id,
                    WorkflowMutationApprovalDecision::Approve,
                    31,
                )
                .is_err()
        );
    }

    struct MutationDispatchOwner(std::sync::atomic::AtomicUsize);

    impl ActivationDispatchOwner for MutationDispatchOwner {
        fn plan(
            &self,
            _activation: &PendingActivation,
        ) -> Result<Option<ActivationDispatchPlan>, WorkflowStoreError> {
            Ok(Some(ActivationDispatchPlan {
                side_effect: DispatchSideEffect::Mutating,
                intent: serde_json::json!({"operation": "git.commit"}),
            }))
        }

        fn dispatch<'a>(
            &'a self,
            request: &'a PreparedActivationDispatch,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(serde_json::json!({"dispatch_identity": request.dispatch_identity}))
            })
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn mutation_approval_approve_persists_grant_before_restart_dispatchability() {
        let temp = tempfile::tempdir().expect("temp");
        let input = serde_json::json!({"expected_head": "abc"});
        let approval = mutation_approval(&input);
        {
            let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
            store
                .persist_definition("mutation-definition", 1, &mutation_approval_definition())
                .expect("definition");
            store
                .create_run(&NewWorkflowRun {
                    run_id: "mutation-run".to_string(),
                    definition_id: "mutation-definition".to_string(),
                    definition_version: 1,
                    workspace_snapshot: "snapshot-1".to_string(),
                    parent_session_id: None,
                    binding: None,
                    input: Some(input),
                    created_at_ms: 10,
                    limits: WorkflowRunLimits::default(),
                })
                .expect("run");
            store.request_mutation_approval(&approval).expect("request");
            let resolved = store
                .resolve_mutation_approval(
                    &approval.approval_id,
                    WorkflowMutationApprovalDecision::Approve,
                    30,
                )
                .expect("approve");
            assert_eq!(resolved.status, "approved");
            assert_eq!(resolved.activation_status, "pending");
            let grant_id = resolved.grant_id.expect("grant");
            let grant = store
                .grant(&grant_id)
                .expect("grant lookup")
                .expect("grant");
            assert_eq!(
                grant.scope,
                serde_json::to_value(&approval.scope).expect("scope")
            );
            assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
            assert_eq!(
                store
                    .resolve_mutation_approval(
                        &approval.approval_id,
                        WorkflowMutationApprovalDecision::Approve,
                        31,
                    )
                    .expect("duplicate approve")
                    .grant_id
                    .as_deref(),
                Some(grant_id.as_str())
            );
            assert!(
                store
                    .resolve_mutation_approval(
                        &approval.approval_id,
                        WorkflowMutationApprovalDecision::Deny,
                        32,
                    )
                    .is_err()
            );
        }
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
        assert!(
            store
                .attempt_history("mutation-run", None, 10)
                .expect("attempts")
                .is_empty()
        );
        let owner = MutationDispatchOwner(std::sync::atomic::AtomicUsize::new(0));
        let first = store
            .dispatch_pending_activations(&owner, 10, 40)
            .await
            .expect("dispatch after restart");
        assert_eq!(first.admitted.len(), 1);
        let dispatch_identity = first.admitted[0].clone();
        assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            store
                .dispatch_pending_activations(&owner, 10, 41)
                .await
                .expect("second scan")
                .admitted
                .is_empty()
        );
        assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        let attempts = store
            .attempt_history("mutation-run", None, 10)
            .expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].dispatch_identity, dispatch_identity);
    }

    #[test]
    fn mutation_approval_deny_and_expiry_never_dispatch() {
        for (decision, resolved_at_ms, expected) in [
            (WorkflowMutationApprovalDecision::Deny, 30, "denied"),
            (WorkflowMutationApprovalDecision::Approve, 100, "expired"),
        ] {
            let temp = tempfile::tempdir().expect("temp");
            let input = serde_json::json!({"expected_head": "abc"});
            let approval = mutation_approval(&input);
            let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
            store
                .persist_definition("mutation-definition", 1, &mutation_approval_definition())
                .expect("definition");
            store
                .create_run(&NewWorkflowRun {
                    run_id: "mutation-run".to_string(),
                    definition_id: "mutation-definition".to_string(),
                    definition_version: 1,
                    workspace_snapshot: "snapshot-1".to_string(),
                    parent_session_id: None,
                    binding: None,
                    input: Some(input),
                    created_at_ms: 10,
                    limits: WorkflowRunLimits::default(),
                })
                .expect("run");
            store.request_mutation_approval(&approval).expect("request");
            let resolved = store
                .resolve_mutation_approval(&approval.approval_id, decision, resolved_at_ms)
                .expect("resolution");
            assert_eq!(resolved.status, expected);
            assert!(resolved.grant_id.is_none());
            assert!(store.pending_activations(10).expect("pending").is_empty());
            assert!(
                store
                    .attempt_history("mutation-run", None, 10)
                    .expect("attempts")
                    .is_empty()
            );
            assert_eq!(
                store
                    .run_summary("mutation-run")
                    .expect("run")
                    .expect("run")
                    .status,
                RunStatus::Failed
            );
        }
    }

    #[test]
    fn mutation_approval_wait_is_exact_persisted_and_restart_safe() {
        let temp = tempfile::tempdir().expect("temp");
        let input = serde_json::json!({"expected_head": "abc", "paths": ["src/lib.rs"]});
        {
            let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
            store
                .persist_definition("mutation-definition", 1, &mutation_approval_definition())
                .expect("definition");
            store
                .create_run(&NewWorkflowRun {
                    run_id: "mutation-run".to_string(),
                    definition_id: "mutation-definition".to_string(),
                    definition_version: 1,
                    workspace_snapshot: "snapshot-1".to_string(),
                    parent_session_id: None,
                    binding: None,
                    input: Some(input.clone()),
                    created_at_ms: 10,
                    limits: WorkflowRunLimits::default(),
                })
                .expect("run");
            let approval = mutation_approval(&input);
            store
                .request_mutation_approval(&approval)
                .expect("request approval");
            store
                .request_mutation_approval(&approval)
                .expect("idempotent request");
            assert!(store.pending_activations(10).expect("pending").is_empty());
            assert_eq!(
                store
                    .pending_mutation_approvals("mutation-run", 10)
                    .expect("approvals"),
                vec![approval]
            );
            assert!(
                store
                    .attempt_history("mutation-run", None, 10)
                    .expect("attempts")
                    .is_empty()
            );
        }
        let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        assert_eq!(
            store
                .pending_mutation_approvals("mutation-run", 10)
                .expect("reopened approvals")
                .len(),
            1
        );
        assert!(store.pending_activations(10).expect("pending").is_empty());
    }

    #[test]
    fn mutation_approval_rejects_stale_or_wrong_identity_without_waiting() {
        let temp = tempfile::tempdir().expect("temp");
        let input = serde_json::json!({"expected_head": "abc"});
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("mutation-definition", 1, &mutation_approval_definition())
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "mutation-run".to_string(),
                definition_id: "mutation-definition".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot-1".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(input.clone()),
                created_at_ms: 10,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        let mut stale = mutation_approval(&input);
        stale.scope.input_checksum_sha256 = "0".repeat(64);
        assert!(store.request_mutation_approval(&stale).is_err());
        let mut wrong_workspace = mutation_approval(&input);
        wrong_workspace.scope.workspace_snapshot = "other".to_string();
        assert!(store.request_mutation_approval(&wrong_workspace).is_err());
        let mut wrong_run = mutation_approval(&input);
        wrong_run.run_id = "other-run".to_string();
        wrong_run.scope.run_id = "other-run".to_string();
        assert!(store.request_mutation_approval(&wrong_run).is_err());
        let mut wrong_node = mutation_approval(&input);
        wrong_node.node_id = "other-node".to_string();
        wrong_node.scope.node_id = "other-node".to_string();
        assert!(store.request_mutation_approval(&wrong_node).is_err());
        let mut wrong_activation = mutation_approval(&input);
        wrong_activation.activation_id = "other-activation".to_string();
        wrong_activation.scope.activation_id = "other-activation".to_string();
        assert!(store.request_mutation_approval(&wrong_activation).is_err());
        assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
        assert!(
            store
                .pending_mutation_approvals("mutation-run", 10)
                .expect("approvals")
                .is_empty()
        );
    }

    fn new_run() -> NewWorkflowRun {
        NewWorkflowRun {
            run_id: "run-1".to_string(),
            definition_id: "example".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot-1".to_string(),
            parent_session_id: Some("session-1".to_string()),
            binding: None,
            input: Some(serde_json::json!(1)),
            created_at_ms: 10,
            limits: WorkflowRunLimits::default(),
        }
    }

    #[test]
    fn associated_run_lookup_is_indexed_and_single_active_is_atomic() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        let run = NewWorkflowRun {
            binding: Some(WorkflowRunBinding {
                owner_plugin_id: "bcode.test".to_string(),
                workflow_kind: "test.workflow".to_string(),
                scope_key: "session-1".to_string(),
                display_label: Some("Test workflow".to_string()),
                single_active: true,
            }),
            ..new_run()
        };
        store.create_run(&run).expect("run");
        let key = WorkflowRunBindingKey {
            owner_plugin_id: "bcode.test".to_string(),
            workflow_kind: "test.workflow".to_string(),
            scope_key: "session-1".to_string(),
        };
        let associated = store
            .associated_run(&key)
            .expect("lookup")
            .expect("associated run");
        assert_eq!(associated.run_id, run.run_id);
        assert_eq!(associated.binding, run.binding);

        let conflict = NewWorkflowRun {
            run_id: "run-2".to_string(),
            created_at_ms: 15,
            ..run.clone()
        };
        let error = store.create_run(&conflict).expect_err("single active");
        assert!(error.to_string().contains("already has an active run"));

        assert!(store.pause_run(&run.run_id, 12).expect("pause"));
        assert!(store.resume_run(&run.run_id, 13).expect("resume"));
        assert!(store.request_cancellation(&run.run_id, 14).expect("cancel"));
        assert_eq!(
            store
                .run_summary(&run.run_id)
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
        store
            .create_run(&conflict)
            .expect("replacement after terminal");
        assert_eq!(
            store
                .associated_run(&key)
                .expect("lookup")
                .expect("latest")
                .run_id,
            "run-2"
        );
    }

    #[test]
    fn stable_run_creation_is_idempotent_and_checks_all_immutable_context() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        let run = new_run();
        assert!(store.create_run_idempotent(&run).expect("create"));
        assert!(!store.create_run_idempotent(&run).expect("idempotent"));
        assert_eq!(store.list_runs(10).expect("runs").len(), 1);

        for conflict in [
            NewWorkflowRun {
                input: Some(serde_json::json!(2)),
                ..run.clone()
            },
            NewWorkflowRun {
                limits: WorkflowRunLimits {
                    retry_cap: run.limits.retry_cap + 1,
                    ..run.limits.clone()
                },
                ..run.clone()
            },
            NewWorkflowRun {
                workspace_snapshot: "snapshot-2".to_string(),
                ..run.clone()
            },
        ] {
            assert!(store.create_run_idempotent(&conflict).is_err());
        }
        assert_eq!(store.list_runs(10).expect("runs").len(), 1);
    }

    fn activation_id() -> String {
        activation_identity("run-1", "review", 0)
    }

    fn new_activation() -> NewActivation {
        NewActivation {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            dependency_generation: 0,
            input: Some(serde_json::json!(1)),
            created_at_ms: 11,
        }
    }

    fn initialized_store() -> (tempfile::TempDir, WorkflowStore) {
        let temp = tempfile::tempdir().expect("temp");
        let store = initialized_store_at(temp.path());
        (temp, store)
    }

    fn initialized_store_at(state_dir: &Path) -> WorkflowStore {
        let mut store = WorkflowStore::open_in_state_dir(state_dir).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        store.create_run(&new_run()).expect("run");
        store
    }

    fn prepare_receipt_backed_attempt(
        store: &mut WorkflowStore,
        side_effect: DispatchSideEffect,
    ) -> String {
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect,
            intent: serde_json::json!({"operation": "review"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&attempt).expect("prepare");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: attempt.run_id,
                node_id: attempt.node_id,
                activation_id: attempt.activation_id,
                attempt: 1,
                dispatch_identity: identity.clone(),
                receipt: serde_json::json!({"turn_id": "turn-1"}),
                admitted_at_ms: 13,
            })
            .expect("receipt");
        identity
    }

    #[tokio::test]
    async fn owner_dispatch_persists_intent_before_call_and_receipt_after_acceptance() {
        use std::sync::Mutex;

        struct Owner {
            store_path: PathBuf,
            observed_status: Mutex<Option<String>>,
        }
        impl ActivationDispatchOwner for Owner {
            fn plan(
                &self,
                activation: &PendingActivation,
            ) -> Result<Option<ActivationDispatchPlan>, WorkflowStoreError> {
                Ok(
                    (activation.node.kind == bcode_workflow::NodeKind::Task).then(|| {
                        ActivationDispatchPlan {
                            side_effect: DispatchSideEffect::ReadOnly,
                            intent: serde_json::json!({"operation": "review"}),
                        }
                    }),
                )
            }

            fn dispatch<'a>(
                &'a self,
                request: &'a PreparedActivationDispatch,
            ) -> Pin<
                Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>,
            > {
                Box::pin(async move {
                    let connection = Connection::open(&self.store_path)?;
                    let status = connection.query_row(
                        "SELECT status FROM workflow_attempts WHERE dispatch_identity = ?1",
                        [&request.dispatch_identity],
                        |row| row.get::<_, String>(0),
                    )?;
                    *self.observed_status.lock().expect("observed status") = Some(status);
                    Ok(serde_json::json!({
                        "work_id": request.dispatch_identity,
                        "owner": "test"
                    }))
                })
            }
        }

        let (temp, mut store) = initialized_store();
        let owner = Owner {
            store_path: workflow_database_path(temp.path()),
            observed_status: Mutex::new(None),
        };
        let summary = store
            .dispatch_pending_activations(&owner, 10, 20)
            .await
            .expect("dispatch");
        assert_eq!(summary.admitted.len(), 1);
        assert!(summary.unsupported.is_empty());
        assert_eq!(
            owner.observed_status.lock().expect("status").as_deref(),
            Some("prepared")
        );
        let attempt = store
            .attempt_history("run-1", None, 10)
            .expect("attempt")
            .pop()
            .expect("attempt row");
        assert_eq!(attempt.status, "admitted");
        assert!(attempt.has_receipt);
    }

    #[tokio::test]
    async fn dispatch_failure_stays_prepared_and_is_not_redispatched() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Owner(AtomicUsize);
        impl ActivationDispatchOwner for Owner {
            fn plan(
                &self,
                _activation: &PendingActivation,
            ) -> Result<Option<ActivationDispatchPlan>, WorkflowStoreError> {
                Ok(Some(ActivationDispatchPlan {
                    side_effect: DispatchSideEffect::Mutating,
                    intent: serde_json::json!({"operation": "apply"}),
                }))
            }

            fn dispatch<'a>(
                &'a self,
                _request: &'a PreparedActivationDispatch,
            ) -> Pin<
                Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>,
            > {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Err(WorkflowStoreError::InvalidData(
                        "owner acceptance unknown".to_string(),
                    ))
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let owner = Owner(AtomicUsize::new(0));
        assert!(
            store
                .dispatch_pending_activations(&owner, 10, 20)
                .await
                .is_err()
        );
        assert_eq!(owner.0.load(Ordering::SeqCst), 1);
        assert!(store.pending_activations(10).expect("pending").is_empty());
        assert!(
            store
                .dispatch_pending_activations(&owner, 10, 21)
                .await
                .expect("retry scan")
                .admitted
                .is_empty()
        );
        assert_eq!(owner.0.load(Ordering::SeqCst), 1);
        let attempt = store
            .attempt_history("run-1", None, 10)
            .expect("attempt")
            .pop()
            .expect("attempt row");
        assert_eq!(attempt.status, "prepared");
        assert!(!attempt.has_receipt);
    }

    #[test]
    fn pending_activation_admission_is_atomic_and_single_winner() {
        let (temp, mut first_store) = initialized_store();
        let mut second_store = WorkflowStore::open_in_state_dir(temp.path()).expect("second store");
        let activation = activation_id();
        let prepared = first_store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "review"}),
                12,
            )
            .expect("admission")
            .expect("prepared");
        assert_eq!(prepared.activation.node_id, "review");
        assert_eq!(prepared.activation.input, Some(serde_json::json!(1)));
        assert_eq!(prepared.attempt, 1);
        assert_eq!(
            prepared.dispatch_identity,
            dispatch_identity("run-1", "review", &activation, 1)
        );
        assert!(
            second_store
                .prepare_pending_activation(
                    "run-1",
                    "review",
                    &activation,
                    DispatchSideEffect::ReadOnly,
                    serde_json::json!({"operation": "review"}),
                    13,
                )
                .expect("second admission")
                .is_none()
        );
        assert!(
            first_store
                .pending_activations(10)
                .expect("pending")
                .is_empty()
        );
        let attempts = first_store
            .attempt_history("run-1", None, 10)
            .expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].dispatch_identity, prepared.dispatch_identity);
        let status: String = first_store
            .connection
            .query_row(
                "SELECT status FROM workflow_activations WHERE run_id = 'run-1' AND node_id = 'review'",
                [],
                |row| row.get(0),
            )
            .expect("activation status");
        assert_eq!(status, "running");
    }

    #[test]
    fn pending_activation_admission_rolls_back_when_intent_is_oversized() {
        let (_temp, mut store) = initialized_store();
        let error = store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation_id(),
                DispatchSideEffect::ReadOnly,
                serde_json::Value::String("x".repeat(MAX_INLINE_JSON_BYTES)),
                12,
            )
            .expect_err("oversized intent");
        assert!(error.to_string().contains("exceeds"));
        assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
        assert!(
            store
                .attempt_history("run-1", None, 10)
                .expect("attempts")
                .is_empty()
        );
    }

    #[test]
    fn prepared_attempt_identity_is_stable_and_conflicting_intent_fails_closed() {
        let (_temp, mut store) = initialized_store();
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        };
        let first = store.prepare_attempt(&attempt).expect("prepare");
        let second = store.prepare_attempt(&attempt).expect("idempotent");
        assert_eq!(first, second);
        assert_eq!(
            first,
            dispatch_identity("run-1", "review", activation_id().as_str(), 1)
        );
        let error = store
            .prepare_attempt(&PreparedAttempt {
                intent: serde_json::json!({"operation": "different"}),
                ..attempt
            })
            .expect_err("conflicting intent");
        assert!(error.to_string().contains("identity conflict"));
    }

    #[test]
    fn dispatch_receipt_requires_exact_prepared_identity() {
        let (_temp, mut store) = initialized_store();
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({"operation": "review"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&attempt).expect("prepare");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: attempt.run_id,
                node_id: attempt.node_id,
                activation_id: attempt.activation_id,
                attempt: 1,
                dispatch_identity: identity,
                receipt: serde_json::json!({"turn_id": "turn-1"}),
                admitted_at_ms: 13,
            })
            .expect("receipt");
        let status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_attempts WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("status");
        assert_eq!(status, "admitted");
    }

    #[test]
    fn restart_reconciliation_never_retries_ambiguous_mutation() {
        let (_temp, mut store) = initialized_store();
        let mutating = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&mutating).expect("prepare");

        let summary = store
            .reconcile_prepared_attempts(10, 20)
            .expect("reconcile");

        assert_eq!(summary.repair_required, [identity]);
        assert!(summary.safe_prepared.is_empty());
        let run_status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_runs WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("run status");
        assert_eq!(run_status, "repair_required");
    }

    #[test]
    fn normal_status_and_history_reads_are_bounded_and_projection_backed() {
        let (_temp, mut store) = initialized_store();
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({"operation": "review"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&attempt).expect("prepare");

        let summary = store.run_summary("run-1").expect("summary").expect("run");
        assert_eq!(summary.status, RunStatus::Running);
        assert_eq!(store.list_runs(10).expect("runs"), [summary]);
        let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].dispatch_identity, identity);
        assert!(!attempts[0].has_receipt);
        let events = store.event_history("run-1", None, 10).expect("events");
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["run_created", "activation_created", "attempt_prepared"]
        );
        let page = store
            .event_history("run-1", Some(events[0].event_seq), 1)
            .expect("paged events");
        assert_eq!(page[0].event_type, "activation_created");
        assert!(store.list_runs(0).is_err());
        assert!(store.event_history("run-1", None, 1_001).is_err());
    }

    #[test]
    fn durable_repeat_control_settlement_advances_generation_then_fails_at_bound() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let definition = repeat_definition();
        store
            .persist_definition("repeat", 1, &definition)
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "repeat-run".to_string(),
                definition_id: "repeat".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
                created_at_ms: 1,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        let body = store
            .pending_activations(10)
            .expect("body")
            .pop()
            .expect("body");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "body-0-output".to_string(),
                run_id: "repeat-run".to_string(),
                node_id: "body".to_string(),
                activation_id: body.activation_id,
                schema_id: definition.nodes["body"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!({"condition_met": false, "iteration": 1}),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect("body output");
        let settled = store
            .settle_pending_control_nodes("repeat-run", 10, 3)
            .expect("repeat");
        assert_eq!(settled.activated.len(), 1);
        assert_eq!(settled.activated[0].dependency_generation, 1);
        assert_eq!(settled.activated[0].input.as_ref().unwrap()["iteration"], 2);
        let body = store
            .pending_activations(10)
            .expect("body")
            .pop()
            .expect("body");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "body-1-output".to_string(),
                run_id: "repeat-run".to_string(),
                node_id: "body".to_string(),
                activation_id: body.activation_id,
                schema_id: definition.nodes["body"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!({"condition_met": false, "iteration": 2}),
                artifact_reference: None,
                created_at_ms: 4,
            })
            .expect("body output");
        let settled = store
            .settle_pending_control_nodes("repeat-run", 10, 5)
            .expect("complete");
        assert!(settled.activated.is_empty());
        assert_eq!(
            store
                .run_summary("repeat-run")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Failed
        );
        assert!(
            store
                .event_history("repeat-run", None, 20)
                .expect("events")
                .iter()
                .any(|event| event.event_type == "run_failed"
                    && event.payload["reason"] == "repeat_iteration_limit_exhausted")
        );
    }

    #[test]
    fn repeat_restart_cannot_duplicate_or_skip_generation() {
        let temp = tempfile::tempdir().expect("temp");
        let definition = repeat_definition();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("repeat-restart", 1, &definition)
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "repeat-restart-run".to_string(),
                definition_id: "repeat-restart".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
                created_at_ms: 1,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        let body = store
            .pending_activations(10)
            .expect("body")
            .pop()
            .expect("body");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "restart-body-0".to_string(),
                run_id: "repeat-restart-run".to_string(),
                node_id: "body".to_string(),
                activation_id: body.activation_id,
                schema_id: definition.nodes["body"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!({"condition_met": false, "iteration": 1}),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect("output");
        drop(store);

        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let first = reopened
            .settle_pending_control_nodes("repeat-restart-run", 10, 3)
            .expect("settle after restart");
        assert_eq!(first.activated.len(), 1);
        assert_eq!(first.activated[0].dependency_generation, 1);
        let second = reopened
            .settle_pending_control_nodes("repeat-restart-run", 10, 4)
            .expect("idempotent settle");
        assert!(second.activated.is_empty());
        drop(reopened);

        let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("second reopen");
        let rows = reopened
            .connection
            .prepare(
                "SELECT dependency_generation, status FROM workflow_activations \
                 WHERE run_id = 'repeat-restart-run' AND node_id = 'body' \
                 ORDER BY dependency_generation",
            )
            .expect("query")
            .query_map([], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        assert_eq!(
            rows,
            vec![(0, "completed".to_string()), (1, "pending".to_string())]
        );
        let repeat_rows: u64 = reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM workflow_activations \
                 WHERE run_id = 'repeat-restart-run' AND node_id = 'repeat-control'",
                [],
                |row| row.get(0),
            )
            .expect("repeat count");
        assert_eq!(repeat_rows, 1);
    }

    #[test]
    fn repeat_predicate_clearing_at_bound_completes_and_records_accounting() {
        let temp = tempfile::tempdir().expect("temp");
        let definition = repeat_definition();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("repeat-clear", 1, &definition)
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "repeat-clear-run".to_string(),
                definition_id: "repeat-clear".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
                created_at_ms: 1,
                limits: WorkflowRunLimits {
                    cycle_cap: 2,
                    ..WorkflowRunLimits::default()
                },
            })
            .expect("run");
        let body = store
            .pending_activations(10)
            .expect("body")
            .pop()
            .expect("body");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "clear-body-0".to_string(),
                run_id: "repeat-clear-run".to_string(),
                node_id: "body".to_string(),
                activation_id: body.activation_id,
                schema_id: definition.nodes["body"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!({"condition_met": false, "iteration": 1}),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect("output");
        store
            .settle_pending_control_nodes("repeat-clear-run", 10, 3)
            .expect("advance");
        let body = store
            .pending_activations(10)
            .expect("body")
            .pop()
            .expect("body");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "clear-body-1".to_string(),
                run_id: "repeat-clear-run".to_string(),
                node_id: "body".to_string(),
                activation_id: body.activation_id,
                schema_id: definition.nodes["body"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!({"condition_met": true, "iteration": 2}),
                artifact_reference: None,
                created_at_ms: 4,
            })
            .expect("output");
        store
            .settle_pending_control_nodes("repeat-clear-run", 10, 5)
            .expect("complete");
        assert_eq!(
            store
                .run_summary("repeat-clear-run")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Completed
        );
        let settled = store
            .event_history("repeat-clear-run", None, 30)
            .expect("events")
            .into_iter()
            .filter(|event| event.event_type == "control_node_settled")
            .collect::<Vec<_>>();
        assert_eq!(settled.len(), 2);
        assert_eq!(settled[0].payload["generation"], 0);
        assert_eq!(settled[0].payload["next_generation"], 1);
        assert_eq!(settled[1].payload["generation"], 1);
        assert_eq!(settled[1].payload["repeat"], false);
        assert_eq!(settled[1].payload["effective_iteration_bound"], 2);
    }

    #[test]
    fn repeat_cycle_cap_is_enforced_with_definition_bound() {
        let temp = tempfile::tempdir().expect("temp");
        let definition = repeat_definition();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("repeat-cap", 1, &definition)
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "repeat-cap-run".to_string(),
                definition_id: "repeat-cap".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
                created_at_ms: 1,
                limits: WorkflowRunLimits {
                    cycle_cap: 1,
                    ..WorkflowRunLimits::default()
                },
            })
            .expect("run");
        let body = store
            .pending_activations(10)
            .expect("body")
            .pop()
            .expect("body");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "cap-body".to_string(),
                run_id: "repeat-cap-run".to_string(),
                node_id: "body".to_string(),
                activation_id: body.activation_id,
                schema_id: definition.nodes["body"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!({"condition_met": false, "iteration": 1}),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect("output");
        store
            .settle_pending_control_nodes("repeat-cap-run", 10, 3)
            .expect("settle");
        assert_eq!(
            store
                .run_summary("repeat-cap-run")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Failed
        );
        let failure = store
            .event_history("repeat-cap-run", None, 20)
            .expect("events")
            .into_iter()
            .find(|event| event.event_type == "run_failed")
            .expect("failure");
        assert_eq!(failure.payload["max_iterations"], 2);
        assert_eq!(failure.payload["cycle_cap"], 1);
        assert_eq!(failure.payload["effective_iteration_bound"], 1);
    }

    #[test]
    fn explicit_failed_node_retry_is_bounded_exact_and_reuses_activation() {
        struct FailedObserver;
        impl AttemptStatusObserver for FailedObserver {
            fn observe(
                &self,
                _request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Failed {
                    message: "review failed".to_string(),
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let prepared = store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation_id(),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "review"}),
                12,
            )
            .expect("prepare")
            .expect("prepared");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: prepared.attempt,
                dispatch_identity: prepared.dispatch_identity,
                receipt: serde_json::json!({"turn_id": "turn-1"}),
                admitted_at_ms: 13,
            })
            .expect("receipt");
        store
            .reconcile_receipt_backed_attempts(&FailedObserver, 10, 20)
            .expect("failed reconciliation");
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Failed
        );
        let result = store
            .retry_failed_node("run-1", "review", &activation_id(), 1, 21)
            .expect("retry");
        assert_eq!(result.next_attempt, 2);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Running
        );
        let prepared = store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation_id(),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "review"}),
                22,
            )
            .expect("prepare")
            .expect("prepared");
        assert_eq!(prepared.attempt, 2);
        assert!(
            store
                .retry_failed_node("run-1", "review", &activation_id(), 1, 23)
                .is_err()
        );
    }

    #[test]
    fn explicit_operator_retry_remains_distinct_from_ambiguity_repair() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = initialized_store_at(temp.path());
        let identity = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::Mutating,
                intent: serde_json::json!({"operation": "mutate"}),
                prepared_at_ms: 12,
            })
            .expect("prepare");
        store
            .reconcile_prepared_attempts(10, 20)
            .expect("repair required");
        assert!(
            store
                .retry_failed_node("run-1", "review", &activation_id(), 1, 21)
                .is_err()
        );
        drop(store);

        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let repaired = reopened
            .repair_attempt(
                &identity,
                &RepairResolution::AbandonForExplicitRetry {
                    reason: "operator verified no side effect".to_string(),
                },
                22,
            )
            .expect("repair");
        assert_eq!(repaired.attempt_status, "abandoned");
        assert_eq!(repaired.run_status, RunStatus::Running);
        assert!(
            reopened
                .retry_failed_node("run-1", "review", &activation_id(), 1, 23)
                .is_err()
        );
        let prepared = reopened
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation_id(),
                DispatchSideEffect::Mutating,
                serde_json::json!({"operation": "mutate"}),
                24,
            )
            .expect("prepare")
            .expect("next attempt");
        assert_eq!(prepared.attempt, 2);
    }

    #[tokio::test]
    async fn cancelled_async_reconciliation_does_not_query_owner() {
        struct Observer;
        impl AsyncAttemptStatusObserver for Observer {
            fn observe_async<'a>(
                &'a self,
                _request: &'a AttemptReconciliationRequest,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a,
                >,
            > {
                Box::pin(async {
                    Err(WorkflowStoreError::InvalidData(
                        "owner must not be queried after cancellation".to_string(),
                    ))
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
        store.request_cancellation("run-1", 20).expect("intent");
        store
            .mark_cancellation_signalled(&identity, 21)
            .expect("signal");

        let summary = store
            .reconcile_receipt_backed_attempts_async(&Observer, 10, 22)
            .await
            .expect("cancel without owner");

        assert_eq!(summary.cancelled, [identity]);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
    }

    #[test]
    fn receipt_backed_restart_reconciliation_persists_terminal_success() {
        struct Observer;
        impl AttemptStatusObserver for Observer {
            fn observe(
                &self,
                request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Succeeded {
                    output: ValidatedOutput {
                        output_id: "output-1".to_string(),
                        run_id: request.run_id.clone(),
                        node_id: request.node_id.clone(),
                        activation_id: request.activation_id.clone(),
                        schema_id: "u32".to_string(),
                        schema_version: 1,
                        value: serde_json::json!(1),
                        artifact_reference: None,
                        created_at_ms: 20,
                    },
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        let summary = store
            .reconcile_receipt_backed_attempts(&Observer, 10, 20)
            .expect("reconcile");
        assert_eq!(summary.succeeded, [identity]);
        let attempt = store
            .attempt_history("run-1", None, 10)
            .expect("attempt")
            .pop()
            .expect("row");
        assert_eq!(attempt.status, "succeeded");
        let activation_status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_activations WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("activation");
        assert_eq!(activation_status, "completed");
    }

    #[test]
    fn automatic_retry_backoff_schedule_survives_restart_without_requeueing() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = initialized_store_at(temp.path());
        let eligibility = bcode_workflow::AutomaticRetryEligibility {
            version: bcode_workflow::WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION,
            effect: bcode_workflow::WorkflowBlockEffect::ReadOnly,
            reconciliation: bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus,
            failure: bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
            attempts_completed: 1,
            max_attempts: 3,
            retry_cap: 2,
        };
        let schedule = store
            .schedule_automatic_retry(
                eligibility,
                "run-1",
                "review",
                &activation_id(),
                bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
                5_000,
                100,
            )
            .expect("schedule");
        assert_eq!(schedule.next_attempt, 2);
        assert_eq!(schedule.next_attempt_at_ms, 5_100);
        drop(store);

        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        assert_eq!(
            reopened
                .automatic_retry_schedule("run-1", "review", &activation_id())
                .expect("load"),
            Some(schedule.clone())
        );
        assert_eq!(
            reopened
                .attempt_history("run-1", None, 10)
                .expect("attempts")
                .len(),
            0
        );
        assert_eq!(
            reopened
                .schedule_automatic_retry(
                    eligibility,
                    "run-1",
                    "review",
                    &activation_id(),
                    bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
                    5_000,
                    100,
                )
                .expect("idempotent"),
            schedule
        );
        assert!(
            reopened
                .schedule_automatic_retry(
                    eligibility,
                    "run-1",
                    "review",
                    &activation_id(),
                    bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
                    6_000,
                    100,
                )
                .is_err()
        );
        assert_eq!(
            reopened
                .event_history("run-1", None, 20)
                .expect("events")
                .iter()
                .filter(|event| event.event_type == "automatic_retry_scheduled")
                .count(),
            1
        );
    }

    #[test]
    fn retry_boundaries_survive_restart_without_automatic_scheduling() {
        struct FailedObserver;
        impl AttemptStatusObserver for FailedObserver {
            fn observe(
                &self,
                _request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Failed {
                    message: "terminal failure".to_string(),
                })
            }
        }

        let temp = tempfile::tempdir().expect("temp");
        let mut store = initialized_store_at(temp.path());
        let prepared = store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation_id(),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "review"}),
                12,
            )
            .expect("prepare")
            .expect("attempt");
        drop(store);

        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("prepared restart");
        let prepared_rows = reopened
            .attempt_history("run-1", None, 10)
            .expect("history");
        assert_eq!(prepared_rows[0].status, "prepared");
        assert_eq!(prepared_rows[0].attempt, 1);
        reopened
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: prepared.attempt,
                dispatch_identity: prepared.dispatch_identity,
                receipt: serde_json::json!({"turn_id": "turn-1"}),
                admitted_at_ms: 13,
            })
            .expect("receipt");
        drop(reopened);

        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("receipt restart");
        assert_eq!(
            reopened
                .attempt_history("run-1", None, 10)
                .expect("history")[0]
                .status,
            "admitted"
        );
        reopened
            .reconcile_receipt_backed_attempts(&FailedObserver, 10, 20)
            .expect("terminal failure");
        drop(reopened);

        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("terminal restart");
        assert!(
            reopened
                .pending_activations(10)
                .expect("pending")
                .is_empty()
        );
        assert_eq!(
            reopened
                .attempt_history("run-1", None, 10)
                .expect("history")[0]
                .status,
            "failed"
        );
        let retry = reopened
            .retry_failed_node("run-1", "review", &activation_id(), 1, 21)
            .expect("operator retry");
        assert_eq!(retry.next_attempt, 2);
        drop(reopened);

        let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("retry restart");
        assert_eq!(
            reopened
                .pending_activations(10)
                .expect("pending")
                .pop()
                .expect("activation")
                .activation_id,
            activation_id()
        );
        assert_eq!(
            reopened
                .attempt_history("run-1", None, 10)
                .expect("history")
                .len(),
            1
        );
    }

    #[test]
    fn unknown_receipt_backed_mutation_becomes_repair_required() {
        struct Observer;
        impl AttemptStatusObserver for Observer {
            fn observe(
                &self,
                _request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Unknown)
            }
        }

        let (temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
        drop(store);
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
        let summary = store
            .reconcile_receipt_backed_attempts(&Observer, 10, 20)
            .expect("reconcile");
        assert_eq!(summary.repair_required, [identity]);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::RepairRequired
        );
    }

    #[test]
    fn terminal_output_survives_restart_without_duplicate_materialization() {
        let (temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        let output = ValidatedOutput {
            output_id: "restart-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(7),
            artifact_reference: None,
            created_at_ms: 20,
        };
        store
            .apply_attempt_observation(&identity, AttemptObservation::Succeeded { output }, 20)
            .expect("success");
        drop(store);
        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
        assert_eq!(
            reopened
                .output_summaries("run-1", 10)
                .expect("outputs")
                .len(),
            1
        );
        assert_eq!(
            reopened
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Completed
        );
        assert!(
            reopened
                .apply_attempt_observation(&identity, AttemptObservation::Unknown, 21,)
                .is_err()
        );
        assert_eq!(
            reopened
                .output_summaries("run-1", 10)
                .expect("outputs")
                .len(),
            1
        );
    }

    #[test]
    fn terminal_run_rejects_resume_with_typed_transition_error() {
        let (_temp, mut store) = initialized_store();
        store
            .connection
            .execute(
                "UPDATE workflow_runs SET status = 'completed' WHERE run_id = 'run-1'",
                [],
            )
            .expect("complete run");

        let error = store
            .resume_run("run-1", 20)
            .expect_err("completed run must not resume");
        assert!(matches!(
            error,
            WorkflowStoreError::InvalidRunTransition {
                current: RunStatus::Completed,
                target: RunStatus::Running,
            }
        ));
    }

    #[test]
    fn missing_run_control_uses_typed_not_found_error() {
        let (_temp, mut store) = initialized_store();
        let error = store
            .pause_run("missing-run", 20)
            .expect_err("missing run must fail");
        assert!(matches!(
            error,
            WorkflowStoreError::RunNotFound { run_id } if run_id == "missing-run"
        ));
    }

    #[test]
    fn pause_and_resume_are_durable_idempotent_admission_gates() {
        let (_temp, mut store) = initialized_store();
        assert!(store.pause_run("run-1", 20).expect("pause"));
        assert!(!store.pause_run("run-1", 21).expect("idempotent pause"));
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Paused
        );
        assert!(
            store
                .create_activation(&NewActivation {
                    node_id: "later".to_string(),
                    activation_id: "activation-2".to_string(),
                    created_at_ms: 22,
                    ..new_activation()
                })
                .is_err()
        );
        assert!(
            store
                .prepare_attempt(&PreparedAttempt {
                    run_id: "run-1".to_string(),
                    node_id: "review".to_string(),
                    activation_id: activation_id(),
                    attempt: 1,
                    side_effect: DispatchSideEffect::ReadOnly,
                    intent: serde_json::json!({}),
                    prepared_at_ms: 22,
                })
                .is_err()
        );
        assert!(store.resume_run("run-1", 23).expect("resume"));
        assert!(!store.resume_run("run-1", 24).expect("idempotent resume"));
        store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({}),
                prepared_at_ms: 25,
            })
            .expect("admission after resume");
        store.request_cancellation("run-1", 26).expect("cancel");
        assert!(store.pause_run("run-1", 27).is_err());
    }

    #[test]
    fn cancellation_intent_is_persisted_before_further_admission() {
        let (_temp, mut store) = initialized_store();
        assert!(store.request_cancellation("run-1", 20).expect("first"));
        assert!(!store.request_cancellation("run-1", 21).expect("idempotent"));
        let summary = store.run_summary("run-1").expect("summary").expect("run");
        assert_eq!(summary.cancellation_requested_at_ms, Some(20));
        let error = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({}),
                prepared_at_ms: 22,
            })
            .expect_err("cancelled run rejects admission");
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(summary.status, RunStatus::Cancelled);
        assert_eq!(
            store
                .active_attempts_for_cancellation("run-1", 10)
                .expect("active children"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn repeated_cancellation_finalizes_legacy_settled_run() {
        let (_temp, mut store) = initialized_store();
        store
            .connection
            .execute(
                "UPDATE workflow_runs SET cancellation_requested_at_ms = 20 \
                 WHERE run_id = 'run-1'",
                [],
            )
            .expect("legacy cancellation intent");

        assert!(
            !store
                .request_cancellation("run-1", 21)
                .expect("finalize existing intent")
        );
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
    }

    #[test]
    fn cancellation_intent_wins_over_late_success_observation() {
        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
        store.request_cancellation("run-1", 20).expect("intent");
        assert!(
            store
                .mark_cancellation_signalled(&identity, 21)
                .expect("signal")
        );
        let output = ValidatedOutput {
            output_id: "late-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 22,
        };

        let summary = store
            .apply_attempt_observation(&identity, AttemptObservation::Succeeded { output }, 22)
            .expect("cancelled late success");

        assert_eq!(summary.cancelled, [identity]);
        assert!(
            store
                .output_summaries("run-1", 10)
                .expect("outputs")
                .is_empty()
        );
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
    }

    #[test]
    fn orphaned_read_only_attempt_cancellation_finalizes_run() {
        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        store.request_cancellation("run-1", 20).expect("intent");

        let status = store
            .settle_orphaned_attempt_cancellation(&identity, 21)
            .expect("settle orphaned read-only attempt");

        assert_eq!(status, RunStatus::Cancelled);
        assert_eq!(
            store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
            "cancelled"
        );
    }

    #[test]
    fn terminal_orphan_cancellation_releases_single_active_binding() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        let first = NewWorkflowRun {
            binding: Some(WorkflowRunBinding {
                owner_plugin_id: "bcode.test".to_string(),
                workflow_kind: "test.workflow".to_string(),
                scope_key: "session-1".to_string(),
                display_label: None,
                single_active: true,
            }),
            ..new_run()
        };
        store.create_run(&first).expect("first run");
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        store.request_cancellation("run-1", 20).expect("cancel");
        assert_eq!(
            store
                .settle_orphaned_attempt_cancellation(&identity, 21)
                .expect("settle orphan"),
            RunStatus::Cancelled
        );

        store
            .create_run(&NewWorkflowRun {
                run_id: "run-2".to_string(),
                created_at_ms: 22,
                ..first
            })
            .expect("replacement after orphan cancellation");
    }

    #[test]
    fn orphaned_receipt_less_mutation_requires_repair() {
        let (_temp, mut store) = initialized_store();
        let identity = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::Mutating,
                intent: serde_json::json!({"operation": "mutate"}),
                prepared_at_ms: 12,
            })
            .expect("prepare");
        store.request_cancellation("run-1", 20).expect("intent");

        assert_eq!(
            store
                .settle_orphaned_attempt_cancellation(&identity, 21)
                .expect("classify orphaned mutation"),
            RunStatus::RepairRequired
        );
    }

    #[test]
    fn orphaned_receipt_backed_mutation_requires_repair() {
        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
        store.request_cancellation("run-1", 20).expect("intent");

        let status = store
            .settle_orphaned_attempt_cancellation(&identity, 21)
            .expect("classify orphaned mutation");

        assert_eq!(status, RunStatus::RepairRequired);
        assert_eq!(
            store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
            "repair_required"
        );
    }

    #[test]
    fn cancelled_attempt_finalizes_run_after_owner_settles() {
        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        store.request_cancellation("run-1", 20).expect("intent");
        assert!(
            store
                .mark_cancellation_signalled(&identity, 21)
                .expect("signal")
        );

        let summary = store
            .apply_attempt_observation(&identity, AttemptObservation::Cancelled, 22)
            .expect("cancelled observation");

        assert_eq!(
            summary.cancelled.as_slice(),
            std::slice::from_ref(&identity)
        );
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
        assert_eq!(
            store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
            "cancelled"
        );
        assert!(
            store
                .apply_attempt_observation(&identity, AttemptObservation::Cancelled, 23)
                .is_err(),
            "terminal cancellation cannot settle a second time"
        );
        assert_eq!(
            store
                .event_history("run-1", None, 100)
                .expect("events")
                .iter()
                .filter(|event| event.event_type == "attempt_cancelled")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancellation_propagates_to_receipt_backed_owner_after_durable_intent() {
        #[derive(Default)]
        struct Owner {
            requests: std::sync::Mutex<Vec<ActiveAttemptCancellation>>,
        }

        impl AttemptCancellationOwner for Owner {
            fn cancel_attempt<'a>(
                &'a self,
                request: &'a ActiveAttemptCancellation,
            ) -> Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>>
            {
                Box::pin(async move {
                    self.requests
                        .lock()
                        .expect("owner requests")
                        .push(request.clone());
                    Ok(())
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        let owner = Owner::default();
        assert!(
            store
                .propagate_cancellation("run-1", &owner, 10, 20)
                .await
                .is_err(),
            "owner signaling must be impossible before durable cancellation intent"
        );
        assert!(owner.requests.lock().expect("requests").is_empty());

        store.request_cancellation("run-1", 20).expect("intent");
        let summary = store
            .propagate_cancellation("run-1", &owner, 10, 21)
            .await
            .expect("propagate");
        assert_eq!(
            summary.signalled.as_slice(),
            std::slice::from_ref(&identity)
        );
        let requests = owner.requests.lock().expect("requests").clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].dispatch_identity, identity);
        assert_eq!(
            requests[0].receipt,
            Some(serde_json::json!({"turn_id": "turn-1"}))
        );
        let repeated = store
            .propagate_cancellation("run-1", &owner, 10, 22)
            .await
            .expect("already signalled");
        assert!(repeated.signalled.is_empty());
        assert_eq!(owner.requests.lock().expect("requests").len(), 1);
        assert_eq!(
            store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
            "cancelling"
        );
    }

    #[tokio::test]
    async fn cancellation_propagation_is_retryable_after_owner_failure() {
        struct Owner;
        impl AttemptCancellationOwner for Owner {
            fn cancel_attempt<'a>(
                &'a self,
                _request: &'a ActiveAttemptCancellation,
            ) -> Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>>
            {
                Box::pin(async {
                    Err(WorkflowStoreError::InvalidData(
                        "owner unavailable".to_string(),
                    ))
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        store.request_cancellation("run-1", 20).expect("intent");
        assert!(
            store
                .propagate_cancellation("run-1", &Owner, 10, 21)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .active_attempts_for_cancellation("run-1", 10)
                .expect("retryable"),
            [identity]
        );
    }

    #[test]
    fn active_children_are_exposed_only_after_cancellation_is_durable() {
        let (_temp, mut store) = initialized_store();
        let identity = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({}),
                prepared_at_ms: 12,
            })
            .expect("attempt");
        assert!(store.active_attempts_for_cancellation("run-1", 10).is_err());
        store.request_cancellation("run-1", 20).expect("intent");
        assert_eq!(
            store
                .active_attempts_for_cancellation("run-1", 10)
                .expect("children"),
            [identity]
        );
    }

    #[test]
    fn persisted_limits_fail_closed_at_activation_and_attempt_admission() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        let mut run = new_run();
        run.limits = WorkflowRunLimits {
            deadline_at_ms: Some(20),
            node_execution_cap: 1,
            concurrency_cap: 1,
            cycle_cap: 1,
            retry_cap: 0,
        };
        store.create_run(&run).expect("run");
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 12,
        };
        store.prepare_attempt(&attempt).expect("first attempt");
        assert!(
            store
                .prepare_attempt(&PreparedAttempt {
                    activation_id: "activation-2".to_string(),
                    ..attempt.clone()
                })
                .is_err()
        );
        assert!(
            store
                .create_activation(&NewActivation {
                    activation_id: "cycle-overflow".to_string(),
                    dependency_generation: 1,
                    ..new_activation()
                })
                .is_err()
        );
        let (_temp, mut deadline_store) = initialized_store();
        deadline_store
            .connection
            .execute(
                "UPDATE workflow_runs SET deadline_at_ms = 12 WHERE run_id = 'run-1'",
                [],
            )
            .expect("deadline");
        assert!(deadline_store.prepare_attempt(&attempt).is_err());
    }

    #[test]
    fn oversized_definition_is_rejected_before_persistence() {
        let (_temp, mut store) = initialized_store();
        let mut definition = definition("oversized");
        definition
            .nodes
            .get_mut("review")
            .expect("node")
            .configuration = serde_json::json!({"padding": "x".repeat(MAX_INLINE_JSON_BYTES)});
        assert!(
            store
                .persist_definition("oversized-definition", 1, &definition)
                .is_err()
        );
        assert!(
            store
                .definition("oversized-definition", 1)
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn doctor_is_bounded_non_mutating_and_reports_corruption() {
        let (_temp, mut store) = initialized_store();
        let identity = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::Mutating,
                intent: serde_json::json!({"operation": "apply"}),
                prepared_at_ms: 12,
            })
            .expect("prepare");
        store
            .connection
            .execute(
                "UPDATE workflow_attempts SET dispatch_identity = 'corrupt', status = 'repair_required' \
                 WHERE dispatch_identity = ?1",
                [identity],
            )
            .expect("corrupt identity");
        store
            .connection
            .execute(
                "UPDATE workflow_activations SET status = 'completed' WHERE run_id = 'run-1'",
                [],
            )
            .expect("corrupt activation");
        store
            .connection
            .execute(
                "INSERT INTO workflow_grants \
                 (grant_id, run_id, node_id, scope_json, granted_at_ms, expires_at_ms) \
                 VALUES ('corrupt-grant', 'run-1', 'review', '{}', 10, NULL)",
                [],
            )
            .expect("corrupt grant");
        let event_count_before: u64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM workflow_events", [], |row| row.get(0))
            .expect("events");

        let first = store.doctor_run("run-1", 1).expect("doctor");
        assert!(first.truncated);
        assert_eq!(first.issues.len(), 1);
        let full = store.doctor_run("run-1", 10).expect("doctor");
        assert_eq!(full.issues.len(), 4);
        assert!(
            full.issues
                .iter()
                .any(|issue| matches!(issue, WorkflowDoctorIssue::RepairStatusMismatch { .. }))
        );
        assert!(
            full.issues
                .iter()
                .any(|issue| matches!(issue, WorkflowDoctorIssue::ActivationOutputMismatch { .. }))
        );
        assert!(
            full.issues
                .iter()
                .any(|issue| matches!(issue, WorkflowDoctorIssue::InvalidGrant { .. }))
        );
        assert!(
            full.issues
                .iter()
                .any(|issue| matches!(issue, WorkflowDoctorIssue::AttemptIdentityMismatch { .. }))
        );
        let event_count_after: u64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM workflow_events", [], |row| row.get(0))
            .expect("events");
        assert_eq!(event_count_before, event_count_after);
    }

    #[test]
    fn explicit_repair_requires_proof_and_never_dispatches() {
        let (_temp, mut store) = initialized_store();
        let identity = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::Mutating,
                intent: serde_json::json!({"operation": "apply"}),
                prepared_at_ms: 12,
            })
            .expect("prepare");
        store
            .reconcile_prepared_attempts(10, 20)
            .expect("repair required");
        let error = store
            .repair_attempt(
                &identity,
                &RepairResolution::ConfirmSucceeded {
                    output: ValidatedOutput {
                        output_id: "output-1".to_string(),
                        run_id: "wrong-run".to_string(),
                        node_id: "review".to_string(),
                        activation_id: activation_id(),
                        schema_id: "u32".to_string(),
                        schema_version: 1,
                        value: serde_json::json!(1),
                        artifact_reference: None,
                        created_at_ms: 21,
                    },
                },
                21,
            )
            .expect_err("mismatched proof");
        assert!(error.to_string().contains("does not match"));

        let result = store
            .repair_attempt(
                &identity,
                &RepairResolution::ConfirmSucceeded {
                    output: ValidatedOutput {
                        output_id: "output-1".to_string(),
                        run_id: "run-1".to_string(),
                        node_id: "review".to_string(),
                        activation_id: activation_id(),
                        schema_id: "u32".to_string(),
                        schema_version: 1,
                        value: serde_json::json!(1),
                        artifact_reference: None,
                        created_at_ms: 21,
                    },
                },
                21,
            )
            .expect("repair");
        assert_eq!(result.attempt_status, "succeeded");
        assert_eq!(result.run_status, RunStatus::Paused);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Paused
        );
        assert!(
            store
                .doctor_run("run-1", 10)
                .expect("doctor")
                .issues
                .is_empty()
        );
    }

    #[test]
    fn explicit_abandonment_is_required_before_retrying_ambiguous_mutation() {
        let (_temp, mut store) = initialized_store();
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&attempt).expect("prepare");
        store
            .reconcile_prepared_attempts(10, 20)
            .expect("repair required");
        assert!(
            store
                .prepare_attempt(&PreparedAttempt {
                    attempt: 2,
                    prepared_at_ms: 21,
                    ..attempt.clone()
                })
                .is_err()
        );
        let result = store
            .repair_attempt(
                &identity,
                &RepairResolution::AbandonForExplicitRetry {
                    reason: "operator verified that no external mutation occurred".to_string(),
                },
                22,
            )
            .expect("abandon");
        assert_eq!(result.attempt_status, "abandoned");
        store
            .prepare_attempt(&PreparedAttempt {
                attempt: 2,
                prepared_at_ms: 23,
                ..attempt
            })
            .expect("explicit retry");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn durable_input_gate_waits_validates_and_activates_successor() {
        let (_temp, mut store) = initialized_store();
        let schema = bcode_workflow::ValueSchema {
            type_name: "u32".to_string(),
            schema: serde_json::json!({"type": "integer", "minimum": 0}),
        };
        let definition = bcode_workflow::WorkflowDefinition {
            schema_version: 1,
            name: "input-gate".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            nodes: std::collections::BTreeMap::from([
                (
                    "input".to_string(),
                    bcode_workflow::NodeDefinition {
                        id: "input".to_string(),
                        name: "input".to_string(),
                        kind: bcode_workflow::NodeKind::Input,
                        input: schema.clone(),
                        output: schema.clone(),
                        resources: Vec::new(),
                        configuration: serde_json::json!({"gate_version": 1}),
                    },
                ),
                (
                    "next".to_string(),
                    bcode_workflow::NodeDefinition {
                        id: "next".to_string(),
                        name: "next".to_string(),
                        kind: bcode_workflow::NodeKind::Task,
                        input: schema.clone(),
                        output: schema,
                        resources: Vec::new(),
                        configuration: serde_json::Value::Null,
                    },
                ),
            ]),
            entries: vec!["input".to_string()],
            exits: vec!["next".to_string()],
            edges: vec![bcode_workflow::EdgeDefinition {
                from: "input".to_string(),
                to: "next".to_string(),
                kind: bcode_workflow::EdgeKind::Direct,
                transform: None,
            }],
        };
        store
            .persist_definition("input-gate", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.run_id = "input-run".to_string();
        run.definition_id = "input-gate".to_string();
        store.create_run(&run).expect("run");
        assert!(
            store
                .pending_activations(10)
                .expect("pending")
                .into_iter()
                .all(|activation| activation.run_id != "input-run")
        );
        let wait = store
            .waiting_activations("input-run", 10)
            .expect("waits")
            .pop()
            .expect("wait");
        assert_eq!(wait.kind, WorkflowWaitKind::Input);
        assert!(
            store
                .provide_input(
                    "input-run",
                    "input",
                    &wait.activation_id,
                    serde_json::json!("wrong"),
                    20,
                )
                .is_err()
        );
        let result = store
            .provide_input(
                "input-run",
                "input",
                &wait.activation_id,
                serde_json::json!(7),
                21,
            )
            .expect("resolve");
        assert_eq!(result.outcome, "accepted");
        assert_eq!(result.activated.len(), 1);
        let next = store
            .pending_activations(10)
            .expect("pending")
            .into_iter()
            .find(|activation| activation.run_id == "input-run")
            .expect("next");
        assert_eq!(next.node_id, "next");
        assert_eq!(next.input, Some(serde_json::json!(7)));
        assert!(
            store
                .waiting_activations("input-run", 10)
                .expect("waits")
                .is_empty()
        );
    }

    #[test]
    fn durable_approval_denial_is_terminal_and_never_activates_downstream() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let schema = bcode_workflow::ValueSchema {
            type_name: "u32".to_string(),
            schema: serde_json::json!({"type": "integer"}),
        };
        let definition = bcode_workflow::WorkflowDefinition {
            schema_version: 1,
            name: "approval".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            nodes: std::collections::BTreeMap::from([
                (
                    "approve".to_string(),
                    bcode_workflow::NodeDefinition {
                        id: "approve".to_string(),
                        name: "approve".to_string(),
                        kind: bcode_workflow::NodeKind::Approval,
                        input: schema.clone(),
                        output: schema.clone(),
                        resources: Vec::new(),
                        configuration: serde_json::json!({"gate_version": 1}),
                    },
                ),
                (
                    "mutate".to_string(),
                    bcode_workflow::NodeDefinition {
                        id: "mutate".to_string(),
                        name: "mutate".to_string(),
                        kind: bcode_workflow::NodeKind::Task,
                        input: schema.clone(),
                        output: schema,
                        resources: Vec::new(),
                        configuration: serde_json::Value::Null,
                    },
                ),
            ]),
            entries: vec!["approve".to_string()],
            exits: vec!["mutate".to_string()],
            edges: vec![bcode_workflow::EdgeDefinition {
                from: "approve".to_string(),
                to: "mutate".to_string(),
                kind: bcode_workflow::EdgeKind::Direct,
                transform: None,
            }],
        };
        store
            .persist_definition("approval", 1, &definition)
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "approval-run".to_string(),
                definition_id: "approval".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!(3)),
                created_at_ms: 1,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        let wait = store
            .waiting_activations("approval-run", 10)
            .expect("waits")
            .pop()
            .expect("wait");
        let result = store
            .resolve_approval("approval-run", "approve", &wait.activation_id, false, 20)
            .expect("deny");
        assert_eq!(result.outcome, "denied");
        assert_eq!(result.run_status, RunStatus::Failed);
        assert!(store.pending_activations(10).expect("pending").is_empty());
        assert_eq!(
            store
                .run_summary("approval-run")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Failed
        );
    }

    #[test]
    fn output_faults_roll_back_every_atomic_boundary() {
        struct Fault(WorkflowOutputBoundary);
        impl WorkflowOutputFault for Fault {
            fn after_boundary(
                &self,
                boundary: WorkflowOutputBoundary,
                _output: &ValidatedOutput,
            ) -> Result<(), WorkflowStoreError> {
                if boundary == self.0 {
                    return Err(WorkflowStoreError::InvalidData(format!(
                        "crash after {boundary:?}"
                    )));
                }
                Ok(())
            }
        }

        for boundary in [
            WorkflowOutputBoundary::OutputInserted,
            WorkflowOutputBoundary::ActivationCompleted,
            WorkflowOutputBoundary::SuccessorsMaterialized,
        ] {
            let temp = tempfile::tempdir().expect("temp");
            let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
            store
                .persist_definition("sequential", 1, &sequential_definition())
                .expect("definition");
            let mut run = new_run();
            run.run_id = "fault-run".to_string();
            run.definition_id = "sequential".to_string();
            store.create_run(&run).expect("run");
            let output = ValidatedOutput {
                output_id: "fault-output".to_string(),
                run_id: "fault-run".to_string(),
                node_id: "first".to_string(),
                activation_id: activation_identity("fault-run", "first", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 20,
            };
            assert!(
                store
                    .persist_validated_output_with_fault(&output, &Fault(boundary))
                    .is_err()
            );
            drop(store);
            let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
            let output_count: u64 = reopened
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM workflow_outputs WHERE run_id = 'fault-run'",
                    [],
                    |row| row.get(0),
                )
                .expect("outputs");
            let first_status: String = reopened
                .connection
                .query_row(
                    "SELECT status FROM workflow_activations WHERE run_id = 'fault-run' AND node_id = 'first'",
                    [],
                    |row| row.get(0),
                )
                .expect("first");
            let successor_count: u64 = reopened
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM workflow_activations WHERE run_id = 'fault-run' AND node_id = 'second'",
                    [],
                    |row| row.get(0),
                )
                .expect("successor");
            assert_eq!(output_count, 0);
            assert_eq!(first_status, "pending");
            assert_eq!(successor_count, 0);
        }
    }

    #[tokio::test]
    async fn dispatch_faults_preserve_exact_boundary_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Owner(AtomicUsize);
        impl ActivationDispatchOwner for Owner {
            fn plan(
                &self,
                _activation: &PendingActivation,
            ) -> Result<Option<ActivationDispatchPlan>, WorkflowStoreError> {
                Ok(Some(ActivationDispatchPlan {
                    side_effect: DispatchSideEffect::ReadOnly,
                    intent: serde_json::json!({"operation": "review"}),
                }))
            }

            fn dispatch<'a>(
                &'a self,
                request: &'a PreparedActivationDispatch,
            ) -> Pin<
                Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>,
            > {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({"identity": request.dispatch_identity}))
                })
            }
        }

        struct Fault(WorkflowDispatchBoundary);
        impl WorkflowDispatchFault for Fault {
            fn after_boundary(
                &self,
                boundary: WorkflowDispatchBoundary,
                _request: &PreparedActivationDispatch,
            ) -> Result<(), WorkflowStoreError> {
                if boundary == self.0 {
                    return Err(WorkflowStoreError::InvalidData(format!(
                        "crash after {boundary:?}"
                    )));
                }
                Ok(())
            }
        }

        for (boundary, expected_calls, expected_status, has_receipt) in [
            (
                WorkflowDispatchBoundary::IntentCommitted,
                0,
                "prepared",
                false,
            ),
            (
                WorkflowDispatchBoundary::OwnerAccepted,
                1,
                "prepared",
                false,
            ),
            (
                WorkflowDispatchBoundary::ReceiptCommitted,
                1,
                "admitted",
                true,
            ),
        ] {
            let (temp, mut store) = initialized_store();
            let owner = Owner(AtomicUsize::new(0));
            assert!(
                store
                    .dispatch_pending_activations_with_fault(&owner, &Fault(boundary), 10, 20)
                    .await
                    .is_err()
            );
            assert_eq!(owner.0.load(Ordering::SeqCst), expected_calls);
            drop(store);
            let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
            let attempt = reopened
                .attempt_history("run-1", None, 10)
                .expect("attempt")
                .pop()
                .expect("row");
            assert_eq!(attempt.status, expected_status);
            assert_eq!(attempt.has_receipt, has_receipt);
        }
    }

    #[tokio::test]
    async fn prepared_read_only_redispatch_reuses_identity_and_persists_receipt() {
        use std::sync::Mutex;

        struct Owner(Mutex<Vec<String>>);
        impl ActivationDispatchOwner for Owner {
            fn plan(
                &self,
                _activation: &PendingActivation,
            ) -> Result<Option<ActivationDispatchPlan>, WorkflowStoreError> {
                unreachable!("redispatch uses the already committed intent")
            }

            fn dispatch<'a>(
                &'a self,
                request: &'a PreparedActivationDispatch,
            ) -> Pin<
                Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>,
            > {
                Box::pin(async move {
                    self.0
                        .lock()
                        .expect("calls")
                        .push(request.dispatch_identity.clone());
                    Ok(serde_json::json!({"owner": "test"}))
                })
            }
        }

        let (temp, mut store) = initialized_store();
        let activation = activation_id();
        let identity = store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "review"}),
                12,
            )
            .expect("prepare")
            .expect("prepared")
            .dispatch_identity;
        drop(store);
        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
        let owner = Owner(Mutex::new(Vec::new()));
        let admitted = reopened
            .redispatch_prepared_read_only(&owner, 10, 20)
            .await
            .expect("redispatch");
        assert_eq!(admitted.as_slice(), std::slice::from_ref(&identity));
        assert_eq!(
            owner.0.lock().expect("calls").as_slice(),
            std::slice::from_ref(&identity)
        );
        let attempt = reopened
            .attempt_history("run-1", None, 10)
            .expect("attempt")
            .pop()
            .expect("row");
        assert_eq!(attempt.status, "admitted");
        assert!(attempt.has_receipt);
        assert!(
            reopened
                .redispatch_prepared_read_only(&owner, 10, 21)
                .await
                .expect("idempotent scan")
                .is_empty()
        );
    }

    #[test]
    fn crash_boundaries_reopen_without_blind_mutation_redispatch() {
        struct Observer;
        impl AttemptStatusObserver for Observer {
            fn observe(
                &self,
                request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Succeeded {
                    output: ValidatedOutput {
                        output_id: "output-after-restart".to_string(),
                        run_id: request.run_id.clone(),
                        node_id: request.node_id.clone(),
                        activation_id: request.activation_id.clone(),
                        schema_id: "u32".to_string(),
                        schema_version: 1,
                        value: serde_json::json!(1),
                        artifact_reference: None,
                        created_at_ms: 30,
                    },
                })
            }
        }

        let temp = tempfile::tempdir().expect("temp");
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        };
        let identity = {
            let mut store = initialized_store_at(temp.path());
            store.prepare_attempt(&attempt).expect("prepare")
        };
        {
            let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
            let summary = reopened
                .reconcile_prepared_attempts(10, 20)
                .expect("reconcile missing receipt");
            assert_eq!(summary.repair_required, [identity]);
        }

        let temp = tempfile::tempdir().expect("temp");
        let identity = {
            let mut store = initialized_store_at(temp.path());
            let identity = store.prepare_attempt(&attempt).expect("prepare");
            store
                .persist_dispatch_receipt(&DispatchReceipt {
                    run_id: attempt.run_id.clone(),
                    node_id: attempt.node_id.clone(),
                    activation_id: attempt.activation_id.clone(),
                    attempt: attempt.attempt,
                    dispatch_identity: identity.clone(),
                    receipt: serde_json::json!({"turn_id": "turn-1"}),
                    admitted_at_ms: 13,
                })
                .expect("receipt");
            identity
        };
        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let summary = reopened
            .reconcile_receipt_backed_attempts(&Observer, 10, 30)
            .expect("receipt reconciliation");
        assert_eq!(summary.succeeded, [identity]);
        assert_eq!(
            reopened
                .attempt_history("run-1", None, 10)
                .expect("attempts")[0]
                .status,
            "succeeded"
        );
    }

    #[test]
    fn parallel_and_output_crash_boundaries_reconcile_independently() {
        struct Observer;
        impl AttemptStatusObserver for Observer {
            fn observe(
                &self,
                request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Succeeded {
                    output: ValidatedOutput {
                        output_id: format!("output-{}", request.activation_id),
                        run_id: request.run_id.clone(),
                        node_id: request.node_id.clone(),
                        activation_id: request.activation_id.clone(),
                        schema_id: "u32".to_string(),
                        schema_version: 1,
                        value: serde_json::json!(1),
                        artifact_reference: None,
                        created_at_ms: 30,
                    },
                })
            }
        }

        let temp = tempfile::tempdir().expect("temp");
        {
            let mut store = initialized_store_at(temp.path());
            store
                .create_activation(&NewActivation {
                    activation_id: "activation-2".to_string(),
                    created_at_ms: 12,
                    ..new_activation()
                })
                .expect("parallel activation");
            let first = PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({"operation": "review"}),
                prepared_at_ms: 13,
            };
            let first_identity = store.prepare_attempt(&first).expect("first prepare");
            store
                .persist_dispatch_receipt(&DispatchReceipt {
                    run_id: first.run_id.clone(),
                    node_id: first.node_id.clone(),
                    activation_id: first.activation_id.clone(),
                    attempt: first.attempt,
                    dispatch_identity: first_identity,
                    receipt: serde_json::json!({"turn_id": "turn-1"}),
                    admitted_at_ms: 14,
                })
                .expect("first receipt");
            store
                .prepare_attempt(&PreparedAttempt {
                    activation_id: "activation-2".to_string(),
                    prepared_at_ms: 15,
                    ..first
                })
                .expect("second prepare");
        }
        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let receipt_summary = reopened
            .reconcile_receipt_backed_attempts(&Observer, 10, 30)
            .expect("first branch");
        assert_eq!(receipt_summary.succeeded.len(), 1);
        let prepared_summary = reopened
            .reconcile_prepared_attempts(10, 31)
            .expect("second branch");
        assert_eq!(prepared_summary.safe_prepared.len(), 1);
        let attempts = reopened
            .attempt_history("run-1", None, 10)
            .expect("attempts");
        assert_eq!(attempts.len(), 2);
        assert!(attempts.iter().any(|attempt| attempt.status == "succeeded"));
        assert!(attempts.iter().any(|attempt| attempt.status == "prepared"));

        drop(reopened);
        let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("second reopen");
        let outputs: u64 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM workflow_outputs", [], |row| {
                row.get(0)
            })
            .expect("outputs");
        let completed: u64 = reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM workflow_activations WHERE status = 'completed'",
                [],
                |row| row.get(0),
            )
            .expect("completed activations");
        assert_eq!(outputs, 1);
        assert_eq!(completed, 1);
    }

    #[tokio::test]
    async fn dispatch_admission_enforces_declared_resources_and_run_concurrency() {
        struct Owner;
        impl ActivationDispatchOwner for Owner {
            fn plan(
                &self,
                _activation: &PendingActivation,
            ) -> Result<Option<ActivationDispatchPlan>, WorkflowStoreError> {
                Ok(Some(ActivationDispatchPlan {
                    side_effect: DispatchSideEffect::ReadOnly,
                    intent: serde_json::json!({"operation": "read"}),
                }))
            }

            fn dispatch<'a>(
                &'a self,
                _request: &'a PreparedActivationDispatch,
            ) -> Pin<
                Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>,
            > {
                Box::pin(async { Ok(serde_json::json!({"accepted": true})) })
            }
        }

        let temp = tempfile::tempdir().expect("temp");
        let mut definition = parallel_join_definition();
        definition.nodes.get_mut("left").expect("left").resources =
            vec![bcode_workflow::ResourceClaim::write("repository")];
        definition.nodes.get_mut("right").expect("right").resources =
            vec![bcode_workflow::ResourceClaim::write("repository")];
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("parallel-resource", 1, &definition)
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "parallel-resource-run".to_string(),
                definition_id: "parallel-resource".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!(1)),
                created_at_ms: 1,
                limits: WorkflowRunLimits {
                    concurrency_cap: 2,
                    ..WorkflowRunLimits::default()
                },
            })
            .expect("run");
        let summary = store
            .dispatch_pending_activations(&Owner, 10, 2)
            .await
            .expect("dispatch");
        assert_eq!(summary.admitted.len(), 1);
        assert_eq!(summary.raced.len(), 1);
        assert_eq!(
            store
                .resource_leases_for_run("parallel-resource-run", 10)
                .expect("leases")
                .len(),
            1
        );
        assert_eq!(
            store
                .attempt_history("parallel-resource-run", None, 10)
                .expect("attempts")
                .len(),
            1
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn decisions_grants_leases_and_checkpoints_are_durable_and_fail_closed() {
        let (_temp, mut store) = initialized_store();
        let decision = WorkflowDecision {
            decision_id: "decision-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: Some("review".to_string()),
            decision_type: "branch".to_string(),
            value: serde_json::json!({"selected": "approve"}),
            created_at_ms: 12,
        };
        store.persist_decision(&decision).expect("decision");
        store
            .persist_decision(&decision)
            .expect("idempotent decision");
        assert_eq!(
            store.decision("decision-1").expect("load decision"),
            Some(decision.clone())
        );
        assert!(
            store
                .persist_decision(&WorkflowDecision {
                    value: serde_json::json!({"selected": "reject"}),
                    ..decision.clone()
                })
                .is_err()
        );

        let grant = WorkflowGrant {
            grant_id: "grant-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            scope: serde_json::json!({"workspace": "snapshot-1", "tools": ["git_commit"]}),
            granted_at_ms: 13,
            expires_at_ms: Some(100),
        };
        store.persist_grant(&grant).expect("grant");
        store.persist_grant(&grant).expect("idempotent grant");
        assert_eq!(
            store.grant("grant-1").expect("load grant"),
            Some(grant.clone())
        );

        let read = WorkflowResourceLease {
            lease_id: "lease-read-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            resource_key: "repository".to_string(),
            mode: ResourceLeaseMode::Read,
            acquired_at_ms: 14,
            expires_at_ms: None,
        };
        store.acquire_resource_lease(&read).expect("read lease");
        store
            .acquire_resource_lease(&read)
            .expect("idempotent lease");
        let second_read = WorkflowResourceLease {
            lease_id: "lease-read-2".to_string(),
            ..read.clone()
        };
        store
            .acquire_resource_lease(&second_read)
            .expect("compatible read lease");
        assert!(
            store
                .acquire_resource_lease(&WorkflowResourceLease {
                    lease_id: "lease-write".to_string(),
                    mode: ResourceLeaseMode::Write,
                    ..read.clone()
                })
                .is_err()
        );
        store
            .release_resource_lease("lease-read-1", 15)
            .expect("release first");
        store
            .release_resource_lease("lease-read-2", 16)
            .expect("release second");
        assert!(store.acquire_resource_lease(&second_read).is_err());
        store
            .acquire_resource_lease(&WorkflowResourceLease {
                lease_id: "lease-write".to_string(),
                mode: ResourceLeaseMode::Write,
                acquired_at_ms: 17,
                ..read
            })
            .expect("write after readers");

        let event_tail: u64 = store
            .connection
            .query_row("SELECT MAX(event_seq) FROM workflow_events", [], |row| {
                row.get(0)
            })
            .expect("tail");
        let checkpoint = WorkflowProjectionCheckpoint {
            projection_name: "run_summary".to_string(),
            projection_version: 1,
            last_event_seq: event_tail,
        };
        store
            .advance_projection_checkpoint(&checkpoint)
            .expect("checkpoint");
        assert_eq!(
            store
                .projection_checkpoint("run_summary")
                .expect("load checkpoint"),
            Some(checkpoint.clone())
        );
        assert!(
            store
                .advance_projection_checkpoint(&WorkflowProjectionCheckpoint {
                    last_event_seq: event_tail.saturating_sub(1),
                    ..checkpoint
                })
                .is_err()
        );
    }

    #[test]
    fn target_input_mismatch_returns_actionable_typed_diagnostic_atomically() {
        let temp = tempfile::tempdir().expect("temp");
        let mut definition = sequential_definition();
        definition
            .nodes
            .get_mut("second")
            .expect("second")
            .input
            .schema = serde_json::json!({"type": "string"});
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("typed-mismatch", 1, &definition)
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "typed-mismatch-run".to_string(),
                definition_id: "typed-mismatch".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!(1)),
                created_at_ms: 1,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        let error = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "first-output".to_string(),
                run_id: "typed-mismatch-run".to_string(),
                node_id: "first".to_string(),
                activation_id: activation_identity("typed-mismatch-run", "first", 0),
                schema_id: definition.nodes["first"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect_err("mismatch");
        assert!(matches!(
            &error,
            WorkflowStoreError::TargetInputValidation(failure)
                if failure.run_id == "typed-mismatch-run"
                    && failure.source_node_id == "first"
                    && failure.source_activation_id
                        == activation_identity("typed-mismatch-run", "first", 0)
                    && failure.target_node_id == "second"
                    && failure.target_schema_id == definition.nodes["second"].input.type_name
                    && failure.message.contains("does not match schema")
        ));
        let output_count: u64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM workflow_outputs WHERE run_id = 'typed-mismatch-run'",
                [],
                |row| row.get(0),
            )
            .expect("outputs");
        assert_eq!(output_count, 0);
        assert!(
            store
                .pending_activations(10)
                .expect("pending")
                .iter()
                .all(|activation| activation.node_id != "second")
        );
    }

    #[test]
    fn branch_target_input_validation_rolls_back_selected_activation() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let mut definition = conditional_definition();
        definition
            .nodes
            .get_mut("selected")
            .expect("selected")
            .input
            .schema = serde_json::json!({"type": "string"});
        store
            .persist_definition("invalid-branch", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.run_id = "invalid-branch-run".to_string();
        run.definition_id = "invalid-branch".to_string();
        store.create_run(&run).expect("run");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "inspect-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: "inspect".to_string(),
                activation_id: activation_identity(&run.run_id, "inspect", 0),
                schema_id: definition.nodes["inspect"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!(1),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect("inspect output");
        let branch = store
            .pending_activations(10)
            .expect("branch")
            .into_iter()
            .find(|activation| activation.node_id == "choose")
            .expect("branch activation");
        let error = store
            .settle_pending_control_nodes(&run.run_id, 10, 3)
            .expect_err("selected target mismatch");
        assert!(matches!(
            &error,
            WorkflowStoreError::TargetInputValidation(failure)
                if failure.run_id == "invalid-branch-run"
                    && failure.source_node_id == "choose"
                    && failure.target_node_id == "selected"
        ));
        assert!(
            store
                .pending_activations(10)
                .expect("pending")
                .iter()
                .any(|activation| activation.activation_id == branch.activation_id)
        );
        assert!(
            store
                .pending_activations(10)
                .expect("pending")
                .iter()
                .all(|activation| activation.node_id != "selected")
        );
    }

    #[test]
    fn repeat_back_input_validation_rolls_back_generation() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let mut definition = repeat_definition();
        definition.nodes.get_mut("body").expect("body").input.schema = serde_json::json!({
            "type": "object",
            "required": ["condition_met", "iteration"],
            "properties": {
                "condition_met": {"type": "boolean"},
                "iteration": {"const": 1}
            }
        });
        let back_edge = definition
            .edges
            .iter_mut()
            .find(|edge| matches!(edge.kind, bcode_workflow::EdgeKind::Back { .. }))
            .expect("back edge");
        back_edge
            .transform
            .as_mut()
            .expect("repeat transform")
            .output = definition.nodes["body"].input.clone();
        store
            .persist_definition("invalid-repeat", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.run_id = "invalid-repeat-run".to_string();
        run.definition_id = "invalid-repeat".to_string();
        run.input = Some(serde_json::json!({"condition_met": false, "iteration": 1}));
        store.create_run(&run).expect("run");
        let body = store
            .pending_activations(10)
            .expect("body")
            .pop()
            .expect("body");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "body-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: "body".to_string(),
                activation_id: body.activation_id,
                schema_id: definition.nodes["body"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!({"condition_met": false, "iteration": 1}),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect("body output");
        let repeat = store
            .pending_activations(10)
            .expect("repeat")
            .into_iter()
            .find(|activation| activation.node_id == "repeat-control")
            .expect("repeat activation");
        let error = store
            .settle_pending_control_nodes(&run.run_id, 10, 3)
            .expect_err("repeat target mismatch");
        assert!(error.to_string().contains("transform output"));
        assert!(
            store
                .pending_activations(10)
                .expect("pending")
                .iter()
                .any(|activation| activation.activation_id == repeat.activation_id)
        );
        assert_eq!(
            store
                .pending_activations(10)
                .expect("pending")
                .iter()
                .filter(|activation| activation.node_id == "body")
                .count(),
            0
        );
    }

    #[test]
    fn run_creation_validates_each_entry_schema_and_rolls_back_atomically() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let mut definition = definition("entry-validation");
        definition.input.schema = serde_json::json!({});
        definition
            .nodes
            .get_mut("review")
            .expect("entry")
            .input
            .schema = serde_json::json!({"type": "string"});
        store
            .persist_definition("entry-validation", 1, &definition)
            .expect("definition");
        let error = store
            .create_run(&NewWorkflowRun {
                run_id: "entry-validation-run".to_string(),
                definition_id: "entry-validation".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!(1)),
                created_at_ms: 1,
                limits: WorkflowRunLimits::default(),
            })
            .expect_err("entry schema mismatch");
        assert!(error.to_string().contains("workflow entry input"));
        assert!(
            store
                .run_summary("entry-validation-run")
                .expect("summary")
                .is_none()
        );
        assert!(
            store
                .pending_activations(10)
                .expect("activations")
                .is_empty()
        );
    }

    #[test]
    fn invalid_successor_input_rolls_back_output_and_activation_atomically() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let mut definition = sequential_definition();
        definition
            .nodes
            .get_mut("second")
            .expect("second")
            .input
            .schema = serde_json::json!({"type": "string"});
        store
            .persist_definition("invalid-successor", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.run_id = "invalid-successor-run".to_string();
        run.definition_id = "invalid-successor".to_string();
        store.create_run(&run).expect("run");
        let error = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "invalid-successor-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: "first".to_string(),
                activation_id: activation_identity(&run.run_id, "first", 0),
                schema_id: definition.nodes["first"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect_err("target input mismatch");
        assert!(error.to_string().contains("workflow activation input"));
        assert_eq!(
            store
                .pending_activations(10)
                .expect("pending")
                .into_iter()
                .map(|activation| activation.node_id)
                .collect::<Vec<_>>(),
            ["first"]
        );
        assert!(
            store
                .output_summaries(&run.run_id, 10)
                .expect("outputs")
                .is_empty()
        );
    }

    #[test]
    fn create_activation_validates_target_schema() {
        let (_temp, mut store) = initialized_store();
        let error = store
            .create_activation(&NewActivation {
                input: Some(serde_json::json!("wrong")),
                ..new_activation()
            })
            .expect_err("invalid activation input");
        assert!(error.to_string().contains("workflow activation input"));
    }

    #[test]
    fn validated_output_atomically_activates_direct_successor_and_completes_run() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("sequential", 1, &sequential_definition())
            .expect("definition");
        let mut run = new_run();
        run.definition_id = "sequential".to_string();
        run.run_id = "sequential-run".to_string();
        store.create_run(&run).expect("run");
        let first_id = activation_identity("sequential-run", "first", 0);
        let first = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "first-output".to_string(),
                run_id: "sequential-run".to_string(),
                node_id: "first".to_string(),
                activation_id: first_id,
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect("first output");
        assert_eq!(first.run_status, RunStatus::Running);
        assert_eq!(first.activated.len(), 1);
        assert_eq!(first.activated[0].node_id, "second");
        assert_eq!(first.activated[0].input, Some(serde_json::json!(2)));
        let second = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "second-output".to_string(),
                run_id: "sequential-run".to_string(),
                node_id: "second".to_string(),
                activation_id: activation_identity("sequential-run", "second", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(3),
                artifact_reference: None,
                created_at_ms: 21,
            })
            .expect("second output");
        assert_eq!(second.run_status, RunStatus::Completed);
        assert!(second.activated.is_empty());
        assert_eq!(
            store
                .run_summary("sequential-run")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Completed
        );
    }

    #[test]
    fn join_activates_once_only_after_all_direct_dependencies_complete() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let definition = parallel_join_definition();
        store
            .persist_definition("parallel", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.run_id = "parallel-run".to_string();
        run.definition_id = "parallel".to_string();
        store.create_run(&run).expect("run");
        for (node_id, output_id, timestamp) in
            [("left", "left-output", 20), ("right", "right-output", 21)]
        {
            let result = store
                .persist_validated_output(&ValidatedOutput {
                    output_id: output_id.to_string(),
                    run_id: "parallel-run".to_string(),
                    node_id: node_id.to_string(),
                    activation_id: activation_identity("parallel-run", node_id, 0),
                    schema_id: "u32".to_string(),
                    schema_version: 1,
                    value: serde_json::json!(2),
                    artifact_reference: None,
                    created_at_ms: timestamp,
                })
                .expect("branch output");
            if node_id == "left" {
                assert!(result.activated.is_empty());
            } else {
                assert_eq!(result.activated.len(), 1);
                assert_eq!(result.activated[0].node_id, "join");
                assert_eq!(result.activated[0].input, Some(serde_json::json!([2, 2])));
            }
        }
        let join_count: u64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM workflow_activations WHERE run_id = 'parallel-run' AND node_id = 'join'",
                [],
                |row| row.get(0),
            )
            .expect("join count");
        assert_eq!(join_count, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn wait_all_parallel_failure_persists_only_after_all_members_settle() {
        struct FailedObserver;
        impl AttemptStatusObserver for FailedObserver {
            fn observe(
                &self,
                _request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Failed {
                    message: "branch failed".to_string(),
                })
            }
        }

        let temp = tempfile::tempdir().expect("temp");
        let definition = parallel_join_definition();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("parallel-failure", 1, &definition)
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "parallel-failure-run".to_string(),
                definition_id: "parallel-failure".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                binding: None,
                input: Some(serde_json::json!(1)),
                created_at_ms: 1,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        let left = store
            .prepare_pending_activation(
                "parallel-failure-run",
                "left",
                &activation_identity("parallel-failure-run", "left", 0),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"branch": "left"}),
                2,
            )
            .expect("prepare")
            .expect("left");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: "parallel-failure-run".to_string(),
                node_id: "left".to_string(),
                activation_id: left.activation.activation_id,
                attempt: left.attempt,
                dispatch_identity: left.dispatch_identity,
                receipt: serde_json::json!({"accepted": true}),
                admitted_at_ms: 3,
            })
            .expect("receipt");
        store
            .reconcile_receipt_backed_attempts(&FailedObserver, 10, 4)
            .expect("left failure");
        assert_eq!(
            store
                .run_summary("parallel-failure-run")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Running
        );
        drop(store);
        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        assert!(
            reopened
                .decision("parallel-failure-run:join:0:parallel-wait-all")
                .expect("decision before settlement")
                .is_none()
        );
        assert_eq!(
            reopened
                .attempt_history("parallel-failure-run", None, 10)
                .expect("attempts")
                .len(),
            1
        );
        reopened
            .persist_validated_output(&ValidatedOutput {
                output_id: "right-output".to_string(),
                run_id: "parallel-failure-run".to_string(),
                node_id: "right".to_string(),
                activation_id: activation_identity("parallel-failure-run", "right", 0),
                schema_id: definition.nodes["right"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 5,
            })
            .expect("right output");
        let decision = reopened
            .decision("parallel-failure-run:join:0:parallel-wait-all")
            .expect("decision")
            .expect("settlement");
        assert_eq!(decision.value["policy"], "wait_all");
        assert_eq!(decision.value["outcome"], "failed");
        assert_eq!(
            reopened
                .run_summary("parallel-failure-run")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Failed
        );
    }

    #[test]
    fn transformed_parallel_join_is_deterministic_across_store_restart() {
        let temp = tempfile::tempdir().expect("temp");
        let definition = parallel_join_transform_definition();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("parallel-transform", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.run_id = "parallel-transform-run".to_string();
        run.definition_id = "parallel-transform".to_string();
        store.create_run(&run).expect("run");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "right-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: "right".to_string(),
                activation_id: activation_identity(&run.run_id, "right", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(20),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect("first member");
        drop(store);

        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let result = reopened
            .persist_validated_output(&ValidatedOutput {
                output_id: "left-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: "left".to_string(),
                activation_id: activation_identity(&run.run_id, "left", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(10),
                artifact_reference: None,
                created_at_ms: 21,
            })
            .expect("second member");
        assert_eq!(result.activated.len(), 1);
        assert_eq!(result.activated[0].node_id, "join");
        assert_eq!(result.activated[0].input, Some(serde_json::json!([10, 20])));
    }

    #[test]
    fn parallel_join_rejects_ambiguous_member_configuration_atomically() {
        let temp = tempfile::tempdir().expect("temp");
        let mut definition = parallel_join_definition();
        definition
            .nodes
            .get_mut("join")
            .expect("join")
            .configuration["right_exits"] = serde_json::json!(["left"]);
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("ambiguous-parallel", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.run_id = "ambiguous-parallel-run".to_string();
        run.definition_id = "ambiguous-parallel".to_string();
        store.create_run(&run).expect("run");
        let output_count = |store: &WorkflowStore| {
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM workflow_outputs WHERE run_id = 'ambiguous-parallel-run'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("output count")
        };
        let error = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "left-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: "left".to_string(),
                activation_id: activation_identity(&run.run_id, "left", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(10),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect_err("ambiguous members");
        assert!(error.to_string().contains("exactly one side"));
        assert_eq!(output_count(&store), 0);
        assert_eq!(
            store
                .pending_activations(10)
                .expect("pending")
                .iter()
                .filter(|activation| activation.node_id == "join")
                .count(),
            0
        );
    }

    #[test]
    fn durable_branch_predicate_failures_roll_back_output_decision_and_paths() {
        for (name, value, expected) in [
            (
                "missing",
                serde_json::json!({"review": {"result": {}}}),
                "was not present",
            ),
            (
                "incompatible",
                serde_json::json!({"review": {"result": {"passed": "true"}}}),
                "incompatible",
            ),
            (
                "nested",
                serde_json::json!({"review": []}),
                "was not present",
            ),
        ] {
            let temp = tempfile::tempdir().expect("temp");
            let definition = conditional_nested_definition();
            let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
            store
                .persist_definition("conditional-nested", 1, &definition)
                .expect("definition");
            let run_id = format!("branch-{name}-run");
            store
                .create_run(&NewWorkflowRun {
                    run_id: run_id.clone(),
                    definition_id: "conditional-nested".to_string(),
                    definition_version: 1,
                    workspace_snapshot: "snapshot".to_string(),
                    parent_session_id: None,
                    binding: None,
                    input: Some(value.clone()),
                    created_at_ms: 1,
                    limits: WorkflowRunLimits::default(),
                })
                .expect("run");
            store
                .persist_validated_output(&ValidatedOutput {
                    output_id: format!("{name}-inspect-output"),
                    run_id: run_id.clone(),
                    node_id: "inspect".to_string(),
                    activation_id: activation_identity(&run_id, "inspect", 0),
                    schema_id: definition.nodes["inspect"].output.type_name.clone(),
                    schema_version: 1,
                    value: value.clone(),
                    artifact_reference: None,
                    created_at_ms: 2,
                })
                .expect("inspect output");
            let branch_output = ValidatedOutput {
                output_id: format!("{name}-branch-output"),
                run_id: run_id.clone(),
                node_id: "choose".to_string(),
                activation_id: activation_identity(&run_id, "choose", 0),
                schema_id: definition.nodes["choose"].output.type_name.clone(),
                schema_version: 1,
                value,
                artifact_reference: None,
                created_at_ms: 3,
            };
            let error = store
                .persist_validated_output(&branch_output)
                .expect_err("predicate failure");
            assert!(error.to_string().contains(expected));
            assert!(
                store
                    .decision(&format!("{}:branch", branch_output.activation_id))
                    .expect("decision")
                    .is_none()
            );
            let output_count: u64 = store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM workflow_outputs WHERE output_id = ?1",
                    [&branch_output.output_id],
                    |row| row.get(0),
                )
                .expect("output count");
            assert_eq!(output_count, 0);
            assert!(
                store
                    .pending_activations(10)
                    .expect("pending")
                    .iter()
                    .all(|activation| {
                        activation.node_id != "selected" && activation.node_id != "other"
                    })
            );
        }
    }

    #[test]
    fn branch_decision_commits_before_successor_materialization_and_survives_restart() {
        struct Fault;
        impl WorkflowOutputFault for Fault {
            fn after_boundary(
                &self,
                boundary: WorkflowOutputBoundary,
                _output: &ValidatedOutput,
            ) -> Result<(), WorkflowStoreError> {
                if boundary == WorkflowOutputBoundary::SuccessorsMaterialized {
                    return Err(WorkflowStoreError::InvalidData(
                        "crash after successors".to_string(),
                    ));
                }
                Ok(())
            }
        }

        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("conditional", 1, &conditional_definition())
            .expect("definition");
        let mut run = new_run();
        run.run_id = "branch-restart-run".to_string();
        run.definition_id = "conditional".to_string();
        store.create_run(&run).expect("run");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "inspect-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: "inspect".to_string(),
                activation_id: activation_identity(&run.run_id, "inspect", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(1),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect("inspect output");
        let branch_output = ValidatedOutput {
            output_id: "branch-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "choose".to_string(),
            activation_id: activation_identity(&run.run_id, "choose", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 21,
        };
        store
            .persist_validated_output_with_fault(&branch_output, &Fault)
            .expect_err("fault");
        assert!(
            store
                .decision(&format!("{}:branch", branch_output.activation_id))
                .expect("decision query")
                .is_none()
        );
        drop(store);

        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let result = reopened
            .persist_validated_output(&branch_output)
            .expect("branch output");
        assert_eq!(result.activated.len(), 1);
        assert_eq!(result.activated[0].node_id, "selected");
        let decision = reopened
            .decision(&format!("{}:branch", branch_output.activation_id))
            .expect("decision query")
            .expect("decision");
        assert_eq!(
            decision.value["selected_entries"],
            serde_json::json!(["selected"])
        );
        let skipped: String = reopened
            .connection
            .query_row(
                "SELECT status FROM workflow_activations WHERE run_id = 'branch-restart-run' AND node_id = 'other'",
                [],
                |row| row.get(0),
            )
            .expect("skipped");
        assert_eq!(skipped, "skipped");
    }

    #[test]
    fn durable_branch_selects_one_path_and_persists_decision() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("conditional", 1, &conditional_definition())
            .expect("definition");
        let mut run = new_run();
        run.run_id = "conditional-run".to_string();
        run.definition_id = "conditional".to_string();
        store.create_run(&run).expect("run");
        let inspect = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "inspect-output".to_string(),
                run_id: "conditional-run".to_string(),
                node_id: "inspect".to_string(),
                activation_id: activation_identity("conditional-run", "inspect", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(1),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect("inspect output");
        assert_eq!(inspect.activated[0].node_id, "choose");
        let choose = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "choose-output".to_string(),
                run_id: "conditional-run".to_string(),
                node_id: "choose".to_string(),
                activation_id: activation_identity("conditional-run", "choose", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(1),
                artifact_reference: None,
                created_at_ms: 21,
            })
            .expect("controller output");
        assert_eq!(choose.run_status, RunStatus::Running);
        assert_eq!(choose.activated.len(), 1);
        assert_eq!(choose.activated[0].node_id, "selected");
        let other_status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_activations WHERE run_id = 'conditional-run' AND node_id = 'other'",
                [],
                |row| row.get(0),
            )
            .expect("other status");
        assert_eq!(other_status, "skipped");
        let decision = store
            .decision(&format!(
                "{}:branch",
                activation_identity("conditional-run", "choose", 0)
            ))
            .expect("decision query")
            .expect("decision");
        assert_eq!(decision.value["predicate_version"], 1);
        assert_eq!(decision.value["selected"], true);
        assert_eq!(
            decision.value["selected_entries"],
            serde_json::json!(["selected"])
        );
        assert_eq!(
            decision.value["skipped_nodes"],
            serde_json::json!(["other"])
        );
    }

    #[test]
    fn validated_output_must_match_exact_compiled_node_schema() {
        let (_temp, mut store) = initialized_store();
        let activation_id = activation_identity("run-1", "review", 0);
        let output_count = |store: &WorkflowStore| {
            store
                .connection
                .query_row("SELECT COUNT(*) FROM workflow_outputs", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("output count")
        };
        let activation_status = |store: &WorkflowStore| {
            store
                .connection
                .query_row(
                    "SELECT status FROM workflow_activations WHERE run_id = 'run-1' AND node_id = 'review'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("activation status")
        };

        let mut output = ValidatedOutput {
            output_id: "output-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id,
            schema_id: "wrong-schema".to_string(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 20,
        };
        assert!(store.persist_validated_output(&output).is_err());
        assert_eq!(output_count(&store), 0);
        assert_eq!(activation_status(&store), "pending");

        output.schema_id = "u32".to_string();
        output.value = serde_json::json!({"not": "an integer"});
        assert!(store.persist_validated_output(&output).is_err());
        assert_eq!(output_count(&store), 0);
        assert_eq!(activation_status(&store), "pending");
    }

    #[test]
    fn validated_output_and_activation_complete_atomically() {
        let (_temp, mut store) = initialized_store();
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "output-1".to_string(),
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(1),
                artifact_reference: None,
                created_at_ms: 13,
            })
            .expect("output");
        let status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_activations WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("status");
        assert_eq!(status, "completed");
    }

    #[test]
    fn resumable_pending_runs_exclude_paused_cancelled_repair_and_active_work() {
        let (temp, mut store) = initialized_store();
        assert_eq!(
            store.resumable_pending_run_ids(10).expect("resumable"),
            ["run-1"]
        );

        store.pause_run("run-1", 20).expect("pause");
        assert!(
            store
                .resumable_pending_run_ids(10)
                .expect("paused")
                .is_empty()
        );
        store.resume_run("run-1", 21).expect("resume");
        let prepared = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({}),
                prepared_at_ms: 22,
            })
            .expect("prepare");
        assert!(
            store
                .resumable_pending_run_ids(10)
                .expect("active attempt")
                .is_empty()
        );
        store.request_cancellation("run-1", 23).expect("cancel");
        store
            .settle_orphaned_attempt_cancellation(&prepared, 24)
            .expect("settle");
        assert!(
            store
                .resumable_pending_run_ids(10)
                .expect("cancelled")
                .is_empty()
        );

        let mut repair = initialized_store_at(temp.path().join("repair").as_path());
        let identity = prepare_receipt_backed_attempt(&mut repair, DispatchSideEffect::Mutating);
        repair.request_cancellation("run-1", 20).expect("cancel");
        repair
            .settle_orphaned_attempt_cancellation(&identity, 21)
            .expect("repair required");
        assert!(
            repair
                .resumable_pending_run_ids(10)
                .expect("repair")
                .is_empty()
        );
    }

    #[test]
    fn pending_activation_reads_are_bounded_and_definition_backed() {
        let (_temp, mut store) = initialized_store();
        let pending = store.pending_activations(10).expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].node_id, "review");
        assert_eq!(pending[0].input, Some(serde_json::json!(1)));
        assert_eq!(pending[0].node.kind, bcode_workflow::NodeKind::Task);
        store.pause_run("run-1", 20).expect("pause");
        assert!(store.pending_activations(10).expect("paused").is_empty());
        store.resume_run("run-1", 21).expect("resume");
        assert_eq!(store.pending_activations(10).expect("resumed").len(), 1);
        store.request_cancellation("run-1", 22).expect("cancel");
        assert!(store.pending_activations(10).expect("cancelled").is_empty());
    }

    #[test]
    fn run_creation_atomically_materializes_stable_entry_activations() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        store.create_run(&new_run()).expect("run");
        let activation: (String, String, u64, String) = store
            .connection
            .query_row(
                "SELECT node_id, activation_id, dependency_generation, status \
                 FROM workflow_activations WHERE run_id = 'run-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("activation");
        assert_eq!(activation.0, "review");
        assert_eq!(activation.1, activation_identity("run-1", "review", 0));
        assert_eq!(activation.2, 0);
        assert_eq!(activation.3, "pending");
        let events = store.event_history("run-1", None, 10).expect("events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "run_created");
        assert_eq!(events[1].event_type, "activation_created");
    }

    #[test]
    fn run_input_is_bounded_and_validated_against_registered_definition() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        let mut invalid = new_run();
        invalid.input = Some(serde_json::json!("not-a-number"));
        assert!(store.create_run(&invalid).is_err());
        let mut valid = new_run();
        valid.run_id = "run-valid".to_string();
        store.create_run(&valid).expect("valid input");
        let stored: String = store
            .connection
            .query_row(
                "SELECT input_json FROM workflow_runs WHERE run_id = 'run-valid'",
                [],
                |row| row.get(0),
            )
            .expect("input");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored).expect("json"),
            serde_json::json!(1)
        );
    }

    #[test]
    fn canonical_path_is_below_workflows_directory() {
        assert_eq!(
            workflow_database_path(Path::new("/state")),
            Path::new("/state/workflows/workflow.db")
        );
    }

    #[test]
    fn definitions_persist_idempotently_and_verify_checksum() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let definition = definition("example");
        let first = store
            .persist_definition("example", 1, &definition)
            .expect("persist");
        let second = store
            .persist_definition("example", 1, &definition)
            .expect("idempotent");
        assert_eq!(first, second);
        assert_eq!(
            store.definition("example", 1).expect("load"),
            Some(first.clone())
        );
        assert_eq!(store.list_definitions(10).expect("list"), [first]);
    }

    #[test]
    fn bounded_run_inspection_queries_exclude_inline_output_values() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        store.create_run(&new_run()).expect("run");
        let activation_id = activation_identity("run-1", "review", 0);
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "output-1".to_string(),
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id,
                schema_id: definition("example").nodes["review"]
                    .output
                    .type_name
                    .clone(),
                schema_version: 1,
                value: serde_json::json!(7),
                artifact_reference: Some("artifact-1".to_string()),
                created_at_ms: 2,
            })
            .expect("output");
        let outputs = store.output_summaries("run-1", 10).expect("outputs");
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].artifact_reference.as_deref(), Some("artifact-1"));
        assert!(
            store
                .grants_for_run("run-1", 10)
                .expect("grants")
                .is_empty()
        );
        assert!(
            store
                .resource_leases_for_run("run-1", 10)
                .expect("leases")
                .is_empty()
        );
    }

    #[test]
    fn definition_registration_rejects_deserialized_invalid_structure() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let mut definition = definition("example");
        definition.entries = vec!["missing".to_string()];
        let error = store
            .persist_definition("example", 1, &definition)
            .expect_err("invalid definition");
        assert!(error.to_string().contains("unknown step 'missing'"));
        assert!(store.list_definitions(10).expect("list").is_empty());
    }

    #[test]
    fn definition_identity_conflicts_fail_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("first"))
            .expect("first");
        let error = store
            .persist_definition("example", 1, &definition("second"))
            .expect_err("conflict");
        assert!(error.to_string().contains("identity conflict"));
    }
}
