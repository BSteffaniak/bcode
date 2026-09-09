//! Portable run admission and observation data, independent of persistence implementation.
//! Existing wire representations are preserved. Authored provenance compatibility is recognized
//! by its version; run status variants are explicit and unknown variants are rejected.

/// Keyset cursor for bounded attempt history; existing serialized shape is preserved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttemptCursor {
    /// Preparation time in Unix milliseconds.
    pub prepared_at_ms: u64,
    /// Dispatch identity used to break timestamp ties.
    pub dispatch_identity: String,
}

/// Result of an explicit operator retry; existing serialized representation is preserved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkflowNodeRetryResult {
    /// Owning run.
    pub run_id: String,
    /// Retried node.
    pub node_id: String,
    /// Exact activation identity.
    pub activation_id: String,
    /// Failed attempt replaced by this transition.
    pub previous_attempt: u32,
    /// Newly scheduled attempt.
    pub next_attempt: u32,
}

/// One durable node activation. Existing serialized representation is preserved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NewActivation {
    /// Owning run.
    pub run_id: String,
    /// Activated node.
    pub node_id: String,
    /// Exact activation identity.
    pub activation_id: String,
    /// Dependency generation.
    pub dependency_generation: u64,
    /// Optional schema-validated activation input.
    pub input: Option<serde_json::Value>,
    /// Creation time in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Result of resolving one exact durable wait. Existing serialized representation is preserved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WaitingResolutionResult {
    /// Owning run.
    pub run_id: String,
    /// Waiting node.
    pub node_id: String,
    /// Exact activation identity.
    pub activation_id: String,
    /// Recorded resolution outcome.
    pub outcome: String,
    /// Newly activated work.
    pub activated: Vec<NewActivation>,
    /// Resulting run status.
    pub run_status: crate::RunStatus,
}

/// A connected source of workflow notifications, independent of transport details.
///
/// The returned subscription owns its delivery resources. Dropping it ends observation;
/// it does not cancel workflow execution. Reconnection does not imply durable resume.
pub trait WorkflowRunObservationApplication: Sync {
    /// Transport or normalized domain failure.
    type Error;
    /// Connection-owning notification subscription.
    type Subscription: WorkflowRunSubscription<Error = Self::Error>;

    /// Subscribe to canonical-state change notifications.
    ///
    /// # Errors
    /// Returns an error when connection or subscription admission fails.
    fn watch_workflow_runs(
        &self,
    ) -> impl std::future::Future<Output = Result<Self::Subscription, Self::Error>> + Send;
}

/// An owned, live-only workflow notification subscription.
///
/// Consumers refetch bounded projections on changes and replace snapshots on resync.
/// Delivery resources are released on drop without cancelling the observed runs.
pub trait WorkflowRunSubscription: Send {
    /// Transport or normalized domain failure.
    type Error;

    /// Wait for the next notification after duplicate and gap handling.
    ///
    /// # Errors
    /// Returns an error on connection closure or decoding failure.
    fn next_event(
        &mut self,
    ) -> impl std::future::Future<Output = Result<WorkflowRunWatchEvent, Self::Error>> + Send;
}

/// Outcome of receiving one workflow live notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowRunWatchEvent {
    /// Canonical state changed; refetch this run's bounded projection.
    Changed(bcode_workflow_view_models::WorkflowLiveEvent),
    /// Delivery skipped beyond the bounded catch-up window; replace bounded snapshots.
    ResyncRequired,
    /// A future event contract was received and cannot be interpreted.
    UnsupportedVersion {
        /// Unsupported event contract version.
        version: u32,
    },
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
    /// An activation's completion status and validated output disagree.
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
/// Existing tagged issue representations are preserved; unknown variants are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDoctorReport {
    /// Inspected run identity.
    pub run_id: String,
    /// Inconsistencies found within the requested bound.
    pub issues: Vec<WorkflowDoctorIssue>,
    /// The requested bound prevented a complete inspection, so additional issues may exist.
    pub truncated: bool,
}

/// Output supplied for validated workflow completion. Construction alone does not prove validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedOutput {
    /// Output identity.
    pub output_id: String,
    /// Owning run.
    pub run_id: String,
    /// Producing node.
    pub node_id: String,
    /// Producing activation.
    pub activation_id: String,
    /// Output schema identity.
    pub schema_id: String,
    /// Output schema version.
    pub schema_version: u32,
    /// Output value checked by the accepting operation.
    pub value: serde_json::Value,
    /// Optional artifact reference.
    pub artifact_reference: Option<String>,
    /// Creation time in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Explicit operator resolution for an ambiguous attempt. Existing tagged wire forms are
/// preserved; unknown resolution variants are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resolution", rename_all = "snake_case")]
pub enum RepairResolution {
    /// Confirm success with output that the accepting operation must validate.
    ConfirmSucceeded { output: ValidatedOutput },
    /// Confirm terminal failure.
    ConfirmFailed { message: String },
    /// Confirm cancellation.
    ConfirmCancelled { message: String },
    /// Abandon ambiguity to permit a later explicit retry; does not dispatch work.
    AbandonForExplicitRetry { reason: String },
}

/// Result of an explicit repair operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairResult {
    /// Repaired dispatch identity.
    pub dispatch_identity: String,
    /// Resulting attempt status.
    pub attempt_status: String,
    /// Resulting run status.
    pub run_status: RunStatus,
}

/// Connected workflow operations retaining caller identity and authorization.
/// Errors do not promise rollback or permit automatic retry.
pub trait WorkflowRunApplication: Sync {
    /// Start one exact plugin-owned template through the normal workflow execution path.
    ///
    /// # Errors
    /// Returns an error for unavailable storage, missing/disabled templates, invalid configuration,
    /// denied authorization, or failure to start execution.
    fn start_workflow_template(
        &self,
        request: WorkflowTemplateStartRequest,
    ) -> impl std::future::Future<Output = Result<WorkflowRunStartResponse, Self::Error>> + Send;

    /// Apply an explicit repair resolution without dispatching a retry.
    ///
    /// # Errors
    /// Returns an error for unavailable state, unverifiable ownership, invalid resolution or transport failure.
    fn repair_workflow_attempt(
        &self,
        dispatch_identity: String,
        resolution: RepairResolution,
    ) -> impl std::future::Future<Output = Result<RepairResult, Self::Error>> + Send;
    /// Inspect a run for bounded inconsistencies without mutation or repair.
    ///
    /// # Errors
    /// Returns an error on unavailable or inconsistent state, missing run, or transport failure.
    fn doctor_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<WorkflowDoctorReport, Self::Error>> + Send;

    /// Adapter-owned transport or normalized domain failure.
    type Error;

    /// Inspect orphaned runs, or explicitly reconcile them when `apply` is true.
    ///
    /// Implementations must preserve ownership verification and defer live or unverifiable owners.
    ///
    /// # Errors
    /// Returns an error on unavailable state, failed ownership checks, or transport failure.
    fn reconcile_orphaned_workflow_runs(
        &self,
        apply: bool,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<OrphanedWorkflowRunReport, Self::Error>> + Send;

    /// List bounded pending mutation approvals across runs without repair.
    ///
    /// # Errors
    /// Returns an error on unavailable state or transport failure.
    fn list_all_workflow_mutation_approvals(
        &self,
        limit: usize,
    ) -> impl std::future::Future<
        Output = Result<Vec<WorkflowMutationApprovalInspection>, Self::Error>,
    > + Send;

    /// List bounded pending mutation approvals for one run without repair.
    ///
    /// # Errors
    /// Returns an error on unavailable state or transport failure.
    fn list_workflow_mutation_approvals(
        &self,
        run_id: String,
        limit: usize,
    ) -> impl std::future::Future<
        Output = Result<Vec<WorkflowMutationApprovalInspection>, Self::Error>,
    > + Send;

    /// Resolve an exact mutation approval and continue admitted work.
    ///
    /// # Errors
    /// Returns an error on invalid identity/decision, unavailable state, ownership conflict,
    /// or transport failure. Errors do not imply rollback of a committed decision.
    fn resolve_workflow_mutation_approval(
        &self,
        approval_id: String,
        decision: WorkflowMutationApprovalDecision,
    ) -> impl std::future::Future<Output = Result<WorkflowMutationApprovalResolution, Self::Error>> + Send;

    /// Read bounded attempt history using a keyset cursor without repair or full replay.
    ///
    /// # Errors
    /// Returns an error on unavailable/invalid state or transport failure.
    fn workflow_attempt_history(
        &self,
        run_id: String,
        cursor: Option<AttemptCursor>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<crate::AttemptSummary>, Self::Error>> + Send;

    /// Read bounded semantic event history after an optional run sequence.
    ///
    /// # Errors
    /// Returns an error on unavailable/invalid state or transport failure.
    fn workflow_event_history(
        &self,
        run_id: String,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<WorkflowHistoryEvent>, Self::Error>> + Send;

    /// Retry one exact failed attempt under verified execution authority.
    ///
    /// # Errors
    /// Returns an error for stale attempt identity, invalid transitions, foreign ownership,
    /// unavailable state, scheduling failure, or transport failure. Errors do not imply rollback.
    fn retry_workflow_node(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        failed_attempt: u32,
    ) -> impl std::future::Future<Output = Result<WorkflowNodeRetryResult, Self::Error>> + Send;

    /// List a bounded set of current waits without repairing durable state.
    ///
    /// # Errors
    /// Returns an error on unavailable or invalid durable state or transport failure.
    fn list_workflow_waits(
        &self,
        run_id: String,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<crate::WaitingActivation>, Self::Error>> + Send;

    /// Supply schema-validated input to an exact wait under verified execution authority.
    ///
    /// # Errors
    /// Returns an error for invalid input/identity, foreign ownership, unavailable state,
    /// scheduling failure, or transport failure. Errors do not imply rollback.
    fn provide_workflow_input(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        value: serde_json::Value,
    ) -> impl std::future::Future<Output = Result<WaitingResolutionResult, Self::Error>> + Send;

    /// Resolve an exact approval wait under verified execution authority.
    ///
    /// # Errors
    /// Returns an error for invalid identity/state, foreign ownership, scheduling failure,
    /// or transport failure. Errors do not imply rollback.
    fn resolve_workflow_approval(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        approved: bool,
    ) -> impl std::future::Future<Output = Result<WaitingResolutionResult, Self::Error>> + Send;

    /// Admit a run of an exact registered definition through normal admission checks.
    ///
    /// # Errors
    /// Returns an error for unavailable definitions/state, denied authorization,
    /// conflicting identity, invalid context, or transport failure. Errors do not imply rollback.
    fn start_workflow_run(
        &self,
        request: WorkflowRunStartRequest,
    ) -> impl std::future::Future<Output = Result<WorkflowRunStartResponse, Self::Error>> + Send;

    /// Admit an exact definition and binding through normal authorization and ownership checks.
    /// Stable run IDs permit identical retries, but conflicting requests fail closed.
    ///
    /// # Errors
    /// Returns an error on invalid definitions, denied admission, ownership conflicts,
    /// unavailable state, or transport failure. Errors do not imply rollback.
    fn start_workflow(
        &self,
        request: WorkflowStartRequest,
    ) -> impl std::future::Future<Output = Result<WorkflowRunStartResponse, Self::Error>> + Send;

    /// Read a bounded page of ordered live notifications after a global sequence.
    /// This is gap catch-up, not durable resume. When `resync_required` is set,
    /// consumers must replace their view from a fresh bounded snapshot.
    ///
    /// # Errors
    /// Returns an error for limits outside 1..=1000, unavailable state, or transport failure.
    fn workflow_live_event_catch_up(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> impl std::future::Future<
        Output = Result<bcode_workflow_view_models::WorkflowLiveEventPage, Self::Error>,
    > + Send;

    /// Look up the newest run for an exact binding without repairing durable state.
    ///
    /// # Errors
    /// Returns an error on unavailable state or transport failure.
    fn associated_workflow_run(
        &self,
        key: WorkflowRunBindingLookup,
    ) -> impl std::future::Future<Output = Result<Option<WorkflowRunSummary>, Self::Error>> + Send;

    /// Inspect bounded collections for the newest run for an exact binding.
    ///
    /// # Errors
    /// Returns an error on unverifiable state or transport failure.
    fn inspect_associated_workflow_run(
        &self,
        key: WorkflowRunBindingLookup,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Option<WorkflowRunInspection>, Self::Error>> + Send;

    /// Control the newest associated run under its verified execution authority.
    /// The flag reports a recorded change, not terminal completion. Errors do not imply rollback.
    ///
    /// # Errors
    /// Returns an error on unavailable state, foreign ownership, invalid transitions,
    /// cancellation propagation failure, or transport failure.
    fn control_associated_workflow_run(
        &self,
        key: WorkflowRunBindingLookup,
        action: WorkflowRunControlAction,
    ) -> impl std::future::Future<Output = Result<(Option<WorkflowRunSummary>, bool), Self::Error>> + Send;

    /// Read one revision-checked graph page without reconstructing the full graph.
    ///
    /// # Errors
    /// Returns an error for invalid cursors, changed revisions, unavailable state, or transport failure.
    fn inspect_workflow_run_graph(
        &self,
        request: crate::WorkflowRunGraphPageRequest,
    ) -> impl std::future::Future<Output = Result<crate::WorkflowRunGraphInspection, Self::Error>> + Send;

    /// List a bounded set of recent runs without repairing durable state.
    ///
    /// # Errors
    /// Returns an error on unavailable state or transport failure.
    fn list_workflow_runs(
        &self,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<WorkflowRunSummary>, Self::Error>> + Send;

    /// Inspect bounded, checksum-verified output disclosures for one run.
    ///
    /// # Errors
    /// Returns an error on unavailable or unverifiable state or transport failure.
    fn workflow_run_outputs(
        &self,
        run_id: String,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<crate::WorkflowOutputInspection>, Self::Error>>
    + Send;

    /// Read one run summary without loading aggregate inspection collections.
    ///
    /// An absent run returns `None`; unavailable or damaged state is an error, not absence.
    ///
    /// # Errors
    /// Returns an error when state cannot be verified or transport fails.
    fn workflow_run_status(
        &self,
        run_id: String,
    ) -> impl std::future::Future<Output = Result<Option<WorkflowRunSummary>, Self::Error>> + Send;

    /// Pause scheduling through the current execution owner.
    ///
    /// The flag reports a changed state, not completion of in-flight work.
    ///
    /// # Errors
    /// Returns an error on absent state, foreign ownership, invalid transition, or transport failure.
    fn pause_workflow_run(
        &self,
        run_id: String,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send;

    /// Resume a paused run through its execution owner without reopening terminal outcomes.
    ///
    /// # Errors
    /// Returns an error on absent state, foreign ownership, cancellation, invalid transition,
    /// scheduling failure, or transport failure.
    fn resume_workflow_run(
        &self,
        run_id: String,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send;

    /// Record cancellation through the current execution owner and propagate it to active work.
    ///
    /// The returned flag reports whether cancellation was recorded, not terminal completion.
    /// A lost response does not authorize repeating other committed operations.
    ///
    /// # Errors
    /// Returns an error for absent/unverifiable state, foreign execution ownership,
    /// cancellation propagation failure, or transport failure.
    fn cancel_workflow_run(
        &self,
        run_id: String,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send;

    /// Inspect a bounded projection of a run without repair or history replay.
    ///
    /// The limit bounds each collection, not total run lifetime. This snapshot is not a
    /// durable stream-resume token and does not authorize mutation or ownership transfer.
    ///
    /// # Errors
    /// Returns an error when the run is absent, durable state cannot be verified, or transport fails.
    fn inspect_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<WorkflowRunInspection, Self::Error>> + Send;

    /// Resolve and admit an authored revision, active revision, or preset.
    ///
    /// # Errors
    /// Returns an error on invalid selection/configuration, denied authorization, unavailable
    /// capabilities or state, ownership conflict, admission failure, or transport failure.
    fn start_authored_workflow(
        &self,
        request: crate::StartAuthoredWorkflowRequest,
    ) -> impl std::future::Future<
        Output = Result<crate::AuthoredWorkflowRunStartResponse, Self::Error>,
    > + Send;
}

use serde::{Deserialize, Serialize};
/// Current authored-run provenance representation.
pub const AUTHORED_WORKFLOW_RUN_PROVENANCE_VERSION: u32 = 1;

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
    /// Stable persisted status spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
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

/// Exact authored-workflow source and resolved configuration used to create one durable run.
///
/// This is diagnostic provenance only. Runtime dispatch and authorization continue to use the
/// compiled definition and normalized operation facts rather than this metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredWorkflowRunProvenance {
    /// Provenance contract version.
    pub version: u32,
    /// Stable authored workflow identity.
    pub workflow_id: String,
    /// Exact immutable published revision.
    pub revision: u64,
    /// Exact compiled definition identity recorded redundantly for bounded consistency checks.
    pub definition_identity: crate::WorkflowDefinitionIdentity,
    /// Optional exact preset identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// Optional exact preset generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_generation: Option<u64>,
    /// Exact resolved, schema-validated runtime configuration.
    pub configuration: serde_json::Value,
}

impl AuthoredWorkflowRunProvenance {
    /// Construct current-version authored-run provenance.
    #[must_use]
    pub const fn new(
        workflow_id: String,
        revision: u64,
        definition_identity: crate::WorkflowDefinitionIdentity,
        preset_id: Option<String>,
        preset_generation: Option<u64>,
        configuration: serde_json::Value,
    ) -> Self {
        Self {
            version: AUTHORED_WORKFLOW_RUN_PROVENANCE_VERSION,
            workflow_id,
            revision,
            definition_identity,
            preset_id,
            preset_generation,
            configuration,
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
    /// Exact accepted parent generation for fixed-generation workflow prompt contexts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_generation: Option<u64>,
    #[serde(default)]
    pub binding: Option<WorkflowRunBinding>,
    /// Exact authored source when this run originated from a published authored workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_provenance: Option<AuthoredWorkflowRunProvenance>,
    /// Canonical successful terminal output identity, absent for non-successful or active runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_output_id: Option<String>,
    /// Checksum of the canonical successful terminal output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_output_checksum_sha256: Option<String>,
    /// Persisted immutable normalized policy/profile identity for this run and descendants.
    pub authorization_profile: crate::WorkflowAuthorizationProfileIdentity,
    /// Persisted immutable pinned authorization profile for this run and its descendants.
    #[serde(default)]
    pub authorization_ceiling: crate::WorkflowToolCapability,
    pub status: RunStatus,
    pub cancellation_requested_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
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

use crate::{PublishWorkflowDraftRequest, WorkflowAuthoringConflict, WorkflowRevisionSnapshot};
use bcode_session_models::WorkId;

/// Normalized failure of run admission, observation, or control.
///
/// Existing code/message serialization is preserved. Unknown codes are not authorization
/// to retry; mutation errors do not promise rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunOperationFailure {
    /// Stable public error code supplied by application error normalization.
    pub code: String,
    /// Secret-safe public explanation, not an implementation error dump.
    pub message: String,
}

/// Compatibility name for a failed admission after successful publication.
/// Publication is not rolled back by this failure.
pub type WorkflowRunAdmissionFailure = WorkflowRunOperationFailure;

/// Publish one exact draft and then attempt a separately reported durable run admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishAndStartWorkflowRequest {
    /// Exact publication operation.
    pub publication: PublishWorkflowDraftRequest,
    /// Caller-stable run identity when retrying run admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub parent_session_id: bcode_session_models::SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_snapshot: Option<String>,
}

/// Run-admission outcome following a successful publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunAdmissionResult {
    Started(Box<AuthoredWorkflowRunStartResponse>),
    Failed(WorkflowRunAdmissionFailure),
}

/// Typed publish-and-start result preserving the publication boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPublishAndStartResult {
    PublicationConflict(WorkflowAuthoringConflict),
    Published {
        revision: Box<WorkflowRevisionSnapshot>,
        active_revision: Option<u64>,
        run_admission: WorkflowRunAdmissionResult,
    },
}

/// Successful authored-workflow run start with exact durable provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredWorkflowRunStartResponse {
    pub started: WorkflowRunStartResponse,
    pub workflow_id: String,
    pub revision: u64,
    pub definition_identity: crate::WorkflowDefinitionIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_generation: Option<u64>,
    pub configuration: serde_json::Value,
}

/// Successful durable workflow run start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunStartResponse {
    pub run: WorkflowRunSummary,
    pub runtime_work_id: WorkId,
}

/// Exact published authored-workflow selection used for a durable run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthoredWorkflowRunSelection {
    Revision {
        workflow_id: String,
        revision: u64,
    },
    Active {
        workflow_id: String,
    },
    Preset {
        workflow_id: String,
        preset_id: String,
        preset_generation: u64,
    },
}

/// Start a durable run from an immutable authored-workflow revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartAuthoredWorkflowRequest {
    pub selection: AuthoredWorkflowRunSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub parent_session_id: bcode_session_models::SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_snapshot: Option<String>,
    /// Exact accepted parent-session generation required when the published workflow contains
    /// fixed-generation agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<serde_json::Value>,
    /// Optional invocation-specific typed run input validated against the published interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartWorkflowPackageExportRequest {
    pub package_export: crate::WorkflowPackageExportIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub parent_session_id: bcode_session_models::SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
}

/// Successful package-export run start with exact publication provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageExportRunStartResponse {
    pub package_export: crate::WorkflowPackageExportIdentity,
    pub package_lock_digest_sha256: String,
    pub exported: crate::WorkflowPackageLockedExport,
    pub started: AuthoredWorkflowRunStartResponse,
}

/// Persisted workflow execution limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunLimits {
    /// Absolute wall-clock deadline, when configured.
    pub deadline_at_ms: Option<u64>,
    /// Maximum total node attempts in the run.
    pub node_execution_cap: u64,
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

/// Typed request to instantiate a maintainable template as normal mutable authored state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTemplateInstantiationRequest {
    pub owner_plugin_id: String,
    pub template_id: String,
    pub template_version: u32,
    /// New stable logical authored-workflow identity.
    pub workflow_id: String,
    /// New mutable draft identity.
    pub draft_id: String,
}

/// Typed request to start one exact plugin-owned template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTemplateStartRequest {
    pub owner_plugin_id: String,
    pub template_id: String,
    pub template_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Optional immutable workspace snapshot override. Empty/absent derives the canonical parent
    /// session working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_snapshot: Option<String>,
    pub parent_session_id: bcode_session_models::SessionId,
    pub configuration: serde_json::Value,
    #[serde(default)]
    pub limits: WorkflowRunLimits,
}

/// Generic request to register and start one exact durable workflow atomically from the caller's
/// perspective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStartRequest {
    pub identity: crate::WorkflowDefinitionIdentity,
    pub definition: crate::WorkflowDefinition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Optional immutable workspace snapshot override. Empty/absent derives the canonical parent
    /// session working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_snapshot: Option<String>,
    pub parent_session_id: bcode_session_models::SessionId,
    pub input: serde_json::Value,
    pub binding: WorkflowRunBinding,
    #[serde(default)]
    pub limits: WorkflowRunLimits,
}

/// Generic request to start one durable workflow from a registered exact definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunStartRequest {
    pub definition_id: String,
    pub definition_version: u32,
    /// Optional caller-stable identity used to make start retries idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Immutable repository/worktree snapshot identity required for every durable run.
    pub workspace_snapshot: String,
    /// Session used for compact generic runtime-work presentation.
    pub parent_session_id: bcode_session_models::SessionId,
    /// Exact accepted parent-session generation required by fixed-generation workflow prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_generation: Option<u64>,
    /// Optional bounded product ownership and discovery association.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<WorkflowRunBinding>,
    /// Optional typed input validated against the registered definition input schema.
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    #[serde(default)]
    pub limits: WorkflowRunLimits,
}

/// Result of one explicit orphaned-workflow-run reconciliation pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanedWorkflowRunReport {
    /// Whether mutations were applied (`true`) or only planned (`false`).
    pub applied: bool,
    /// Runs whose coordinator verifiably ended and that were (or would be) reassigned and
    /// cancelled.
    pub reconciled: Vec<OrphanedWorkflowRun>,
    /// Nonterminal runs that were inspected but left untouched, with the reason.
    pub skipped: Vec<SkippedWorkflowRun>,
}

/// One nonterminal run whose coordinator daemon verifiably ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanedWorkflowRun {
    pub run_id: String,
    pub workflow_kind: Option<String>,
    pub parent_session_id: Option<String>,
    pub status: RunStatus,
    pub ended_daemon_instance_id: String,
    pub ended_target_artifact_id: String,
    pub artifact_image_available: bool,
    pub updated_at_ms: u64,
}

/// One nonterminal run that reconciliation deliberately left alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedWorkflowRun {
    pub run_id: String,
    pub status: RunStatus,
    pub reason: String,
}

/// Normalized ownership status of one workflow run's coordinator daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCoordinatorStatus {
    /// Artifact the run is fenced to.
    pub target_artifact_id: String,
    /// Daemon instance recorded as the current coordinator.
    pub daemon_instance_id: String,
    /// Whether the responding daemon is that coordinator.
    pub owned_by_this_daemon: bool,
    /// Whether the responding daemon could take control of the run on demand.
    ///
    /// `false` means another daemon still owns the run (or ownership cannot be verified), and
    /// control requests from this daemon will be refused until that daemon ends.
    pub controllable_from_this_daemon: bool,
}

/// Lifecycle transition applied to one workflow run found through a generic binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunControlAction {
    Pause,
    Resume,
    Cancel,
}

/// Generic associated workflow run lookup key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunBindingLookup {
    pub owner_plugin_id: String,
    pub workflow_kind: String,
    pub scope_key: String,
}

/// Current bounded output inspection representation.
pub const WORKFLOW_OUTPUT_INSPECTION_VERSION: u32 = 1;

/// Current bounded output inspection representation.
pub const WORKFLOW_TERMINAL_OUTPUT_INSPECTION_VERSION: u32 = 1;
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

/// Bounded canonical validated workflow output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowOutputInspection {
    pub version: u32,
    pub output_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub checksum_sha256: String,
    pub value: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_reference: Option<String>,
    pub created_at_ms: u64,
}

/// Bounded canonical terminal value exposed through normal workflow inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTerminalOutputInspection {
    pub version: u32,
    pub output_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub checksum_sha256: String,
    pub value: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_reference: Option<String>,
    pub created_at_ms: u64,
}

/// Typed definition exposed by bounded run inspection, independent of storage records.
///
/// The established `definition_json` wire field contains encoded JSON for compatibility;
/// application callers consume the typed definition instead of interpreting storage text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDefinitionSnapshot {
    /// Stable registered definition identity.
    pub definition_id: String,
    /// Registered definition version.
    pub version: u32,
    /// Checksum of the canonical stored representation.
    pub checksum_sha256: String,
    /// Parsed workflow definition.
    #[serde(rename = "definition_json")]
    pub definition: WorkflowDefinitionRepresentation,
}

/// Immutable typed definition retaining the exact JSON bytes covered by its stored checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowDefinitionRepresentation {
    encoded: String,
    parsed: crate::WorkflowDefinition,
}

impl WorkflowDefinitionRepresentation {
    /// Parse a stored representation without changing its bytes.
    ///
    /// # Errors
    /// Returns a fixed diagnostic when the definition cannot be decoded.
    pub fn parse(encoded: String) -> Result<Self, &'static str> {
        let parsed = serde_json::from_str(&encoded)
            .map_err(|_| "invalid workflow definition representation")?;
        Ok(Self { encoded, parsed })
    }

    /// Access the typed definition without permitting divergence from its representation.
    #[must_use]
    pub const fn definition(&self) -> &crate::WorkflowDefinition {
        &self.parsed
    }

    /// Consume this representation and return its typed definition.
    #[must_use]
    pub fn into_definition(self) -> crate::WorkflowDefinition {
        self.parsed
    }
}

impl Serialize for WorkflowDefinitionRepresentation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encoded)
    }
}

impl<'de> Deserialize<'de> for WorkflowDefinitionRepresentation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
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
    /// Stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Mutating => "mutating",
        }
    }
}

/// Durable waiting-gate kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowWaitKind {
    Input,
    Approval,
}

impl WorkflowWaitKind {
    /// Stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
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

/// Bounded canonical activation input available to normal status projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum WorkflowActivationInputSummary {
    Absent,
    Inline { value: serde_json::Value },
    Omitted { byte_count: usize },
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
    pub input_summary: WorkflowActivationInputSummary,
    pub created_at_ms: u64,
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
    /// Maximum permitted consumptions for a scoped static plan. `None` retains legacy unlimited use.
    #[serde(default)]
    pub max_uses: Option<u32>,
    /// Durable successful consumption count.
    #[serde(default)]
    pub uses_consumed: u32,
}

/// Public decision observation, separate from producer-private decision content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDecisionInspection {
    /// Stable decision identity.
    pub decision_id: String,
    /// Owning run identity.
    pub run_id: String,
    /// Optional node association.
    pub node_id: Option<String>,
    /// Producer-defined classification, not an executable instruction.
    pub decision_type: String,
    /// Explicit disclosure status, not the original decision value.
    pub value: WorkflowDecisionValueDisclosure,
    /// Recorded time in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Availability of private decision content in public inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum WorkflowDecisionValueDisclosure {
    /// Recorded approval resolution; private authorization scope is excluded.
    MutationApproval { approval_id: String, approved: bool },
    /// Content is withheld; callers must not interpret this as an empty decision.
    Withheld,
}

/// Public grant inspection without private authorization scope content.
///
/// The `scope` field reports validated review content or explicit withheld disclosure;
/// neither representation is an executable authorization scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGrantInspection {
    /// Stable grant identity.
    pub grant_id: String,
    /// Owning run identity.
    pub run_id: String,
    /// Node to which the grant applies.
    pub node_id: String,
    /// Scope disclosure status; never usable to authorize execution.
    pub scope: WorkflowGrantScopeDisclosure,
    /// Grant creation time.
    pub granted_at_ms: u64,
    /// Optional expiration time.
    pub expires_at_ms: Option<u64>,
    /// Optional consumption limit.
    pub max_uses: Option<u32>,
    /// Successful consumption count.
    pub uses_consumed: u32,
}

/// Public scope availability, separate from the private authorization record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum WorkflowGrantScopeDisclosure {
    /// Validated mutation scope review content, not executable authorization facts.
    Mutation {
        scope: Box<WorkflowMutationApprovalScopeInspection>,
    },
    /// Validated policy scope and capability for audit, not dispatch authority.
    Policy {
        scope: crate::WorkflowGrantScope,
        capability: crate::WorkflowToolCapability,
    },
    /// Private, unsupported, invalid, or mismatched scope details are not included.
    Withheld,
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
    /// Request-local continuation result; absent for store-only or older observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<WorkflowApprovalContinuation>,
}

/// Continuation attempted after a durable approval resolution, not the run's terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowApprovalContinuation {
    /// Resolution did not admit pending work.
    NotRequired,
    /// The bounded drive operation returned successfully; work may still be active.
    Driven,
    /// Resolution committed, but continuation failed. Inspect the run before retrying.
    Failed,
}

impl WorkflowMutationApprovalResolution {
    /// Whether the requested decision is recorded, independently of continuation.
    /// Expired or unknown outcomes never count as an applied decision.
    #[must_use]
    pub fn decision_applied(&self, decision: crate::WorkflowMutationApprovalDecision) -> bool {
        match decision {
            crate::WorkflowMutationApprovalDecision::Approve => {
                self.status == "approved"
                    && self
                        .grant_id
                        .as_ref()
                        .is_some_and(|id| !id.trim().is_empty())
            }
            crate::WorkflowMutationApprovalDecision::Deny => {
                self.status == "denied" && self.grant_id.is_none()
            }
        }
    }

    /// Whether this resolution admits pending work for continuation.
    ///
    /// An approve request may resolve as expired. Only a recorded approval with a
    /// grant and pending activation admits continuation; observations of already
    /// running or terminal work do not request another dispatch.
    #[must_use]
    pub fn admits_continuation(&self) -> bool {
        self.decision_applied(crate::WorkflowMutationApprovalDecision::Approve)
            && self.activation_status == "pending"
    }
}

/// Public pending approval, resolved by identity rather than resubmitted authorization facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMutationApprovalInspection {
    pub approval_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub scope: WorkflowMutationApprovalScopeInspection,
    pub requested_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

/// Informational scope for informed approval; never sufficient to authorize dispatch.
///
/// Owner-prepared canonical operation facts and preparation credentials are intentionally
/// excluded. The bounded input summary is retained for informed approval and may contain
/// user-authored input; it is not a secret-filtered diagnostic. Resolution loads the exact
/// private scope from the owning store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMutationApprovalScopeInspection {
    pub plugin_id: String,
    pub block_id: String,
    pub block_version: u32,
    pub operation: String,
    pub workspace_snapshot: String,
    pub input_summary: serde_json::Value,
    pub resource_claims: Vec<crate::ResourceClaim>,
    pub reconciliation: crate::WorkflowBlockReconciliation,
    pub capability: crate::WorkflowToolCapability,
}

/// One exact pending workflow mutation approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMutationApproval {
    pub approval_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub scope: crate::WorkflowMutationGrantScope,
    pub requested_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

/// Access mode for one durable workflow resource lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLeaseMode {
    Read,
    Write,
}

impl ResourceLeaseMode {
    /// Stable persisted access-mode spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
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

/// Immutable parent activation/attempt to child run relationship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunLink {
    pub version: u32,
    pub root_run_id: String,
    pub parent_run_id: String,
    pub parent_node_id: String,
    pub parent_activation_id: String,
    pub parent_attempt: u32,
    pub child_run_id: String,
    pub target: crate::WorkflowCallTarget,
    pub depth: u32,
    pub created_at_ms: u64,
}

/// Bounded durable descendant status joined to its immutable parent/child link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDescendantRunSummary {
    pub link: WorkflowRunLink,
    pub run: WorkflowRunSummary,
}

/// Bounded durable repeat outcome returned by normal inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRepeatOutcomeSummary {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub output_id: String,
    pub outcome: crate::WorkflowRepeatOutcomeKind,
    pub iterations_completed: u64,
    pub max_iterations: u64,
    pub cycle_cap: u64,
    pub effective_iteration_bound: u64,
    pub checksum_sha256: String,
    pub created_at_ms: u64,
}

/// Historical run lifecycle observation, not a claim about the run's current status.
/// Later events and the current run snapshot supersede this observation; it grants no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunLifecycleObservation {
    /// Status established by this recorded transition.
    pub status: RunStatus,
}

/// Public output-validation facts, excluding output values and artifact locations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowOutputValidationObservation {
    /// Version of the output schema used by the producer.
    pub schema_version: u32,
    /// Whether an artifact reference was recorded; not permission to access it.
    pub has_artifact: bool,
    /// Recorded output creation time in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Public wait-resolution facts, excluding submitted input and approval content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowWaitResolutionObservation {
    /// The kind of wait resolved; unknown kinds must not be guessed.
    pub kind: WorkflowWaitKind,
    /// Whether the submitted resolution was accepted.
    pub accepted: bool,
}

/// Public activation lifecycle facts, excluding activation input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowActivationObservation {
    pub node_id: String,
    pub activation_id: String,
    pub dependency_generation: u64,
    pub created_at_ms: u64,
    pub waiting: bool,
    /// Required action for waiting activations; absent for pending activations.
    pub wait_kind: Option<WorkflowWaitKind>,
}

/// Public preparation facts, excluding private dispatch intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowAttemptPreparationObservation {
    /// One-based attempt number.
    pub attempt: u32,
    /// Declared side-effect classification, not authorization to dispatch.
    pub side_effect: DispatchSideEffect,
    /// Time preparation was recorded in Unix milliseconds.
    pub prepared_at_ms: u64,
}

/// Public attempt admission facts; owner receipts and dispatch intent remain private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowAttemptAdmissionObservation {
    /// One-based attempt number.
    pub attempt: u32,
    /// Time admission was durably recorded, in Unix milliseconds.
    pub admitted_at_ms: u64,
}

/// Public grant-use observation, excluding scope and private authorization content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGrantUseObservation {
    /// Grant creation time in milliseconds since the Unix epoch.
    pub granted_at_ms: u64,
    /// Expiration when configured; absence means no time-based expiration.
    pub expires_at_ms: Option<u64>,
    /// Configured consumption limit; absence means unlimited consumption.
    #[serde(default)]
    pub max_uses: Option<u32>,
    /// Successful consumptions recorded by the store.
    #[serde(default)]
    pub uses_consumed: u32,
}

/// Public repeat settlement facts. Unknown outcome/policy variants are rejected, not guessed.
/// Producer identities and arbitrary content are deliberately not part of this diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRepeatSettlementObservation {
    /// Whether the repeat continues within its configured bound.
    pub repeat: bool,
    /// Settled dependency generation.
    pub generation: u64,
    /// Completed iterations.
    pub iterations_completed: u64,
    /// Next generation, if requested by the predicate.
    pub next_generation: Option<u64>,
    /// Authored iteration limit.
    pub max_iterations: u64,
    /// Configured cycle allowance.
    pub cycle_cap: u64,
    /// Effective iteration bound.
    pub effective_iteration_bound: u64,
    /// Whether that bound was exhausted.
    pub iteration_bound_exhausted: bool,
    /// Authored behavior on exhaustion.
    pub exhaustion_policy: crate::WorkflowRepeatExhaustionPolicy,
    /// Typed outcome when emitted by the producer.
    pub outcome: Option<crate::WorkflowRepeatOutcomeKind>,
}

/// Public fan-out materialization facts, excluding member inputs and private content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowFanOutObservation {
    /// Number of materialized members.
    pub member_count: u64,
    /// Configured concurrent member allowance.
    pub max_concurrency: u64,
}

/// Normalized public failure/pause classification. Raw owner messages are never included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowHistoryDiagnostic {
    /// An attempt failed; consult current run state before deciding recovery.
    AttemptFailed,
    /// A composed member failed.
    FanOutMemberFailed,
    /// A run failed.
    RunFailed,
    /// A parallel join failed.
    ParallelJoinFailed,
    /// A mutation approval was denied.
    MutationApprovalDenied,
    /// A mutation approval expired.
    MutationApprovalExpired,
    /// The run deadline elapsed.
    RunDeadlineElapsed,
    /// A fan-out failed.
    FanOutFailed,
    /// A provider is unavailable.
    ProviderUnavailable,
    /// An attempt reached its idle timeout.
    IdleTimeout,
    /// An attempt exhausted its tool rounds.
    ToolRoundLimitReached,
    /// An attempt was steered independently of whole-run cancellation.
    Steering,
    /// A pause reason is absent or unsupported; do not guess recovery semantics.
    PauseReasonUnavailable,
}

/// Public authority-transfer observation. Fencing credentials and private ownership evidence
/// are intentionally absent; this diagnostic does not grant execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowAuthorityTransferObservation {
    /// Previous ownership generation.
    pub previous_generation: u64,
    /// Replacement ownership generation.
    pub generation: u64,
    /// Transfer time in milliseconds since the Unix epoch.
    pub reassigned_at_ms: u64,
}

/// Verified coordinates for a history event's indexed execution attempt.
/// This observation does not confer execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowHistoryAttemptCorrelation {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
}

/// One bounded history observation, not an instruction or a replayable execution command.
///
/// `event_type` retains its established wire representation. Authority-transfer payloads contain
/// only `WorkflowAuthorityTransferObservation` numeric facts, or an `unavailable` diagnostic when
/// malformed; fencing credentials and private evidence are withheld. Failure events and attempt
/// pauses expose only a `reason` encoded as `WorkflowHistoryDiagnostic`, never owner messages,
/// plus an optional validated digest dispatch identity for correlation. Unreviewed event payloads
/// are withheld with `unavailable: unreviewed_event_payload`; this is explicitly incomplete
/// diagnostic detail, not an empty event or guessed semantics. Canonical history is unchanged.
/// This snapshot confers neither canonical storage authority nor durable stream-resume guarantees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowHistoryEvent {
    /// Sequence in the owning run's event history.
    pub event_seq: u64,
    /// Owning run identity.
    pub run_id: String,
    /// Producer-defined diagnostic event kind, preserved without guessing unknown kinds.
    pub event_type: String,
    /// Opaque diagnostic content; not a normalized executable operation.
    pub payload: serde_json::Value,
    /// Recorded time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
}

/// Bounded aggregate workflow inspection snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunInspection {
    pub run: crate::WorkflowRunSummary,
    /// Absent for older senders; absence must not be interpreted as an empty graph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<crate::WorkflowRunGraphInspection>,
    pub definition: crate::WorkflowDefinitionSnapshot,
    /// Canonical successful terminal value, present only for a completed run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_output: Option<WorkflowTerminalOutputInspection>,
    pub activations: Vec<crate::WorkflowActivationSummary>,
    pub waits: Vec<crate::WaitingActivation>,
    pub mutation_approvals: Vec<crate::WorkflowMutationApprovalInspection>,
    pub attempts: Vec<crate::AttemptSummary>,
    pub events: Vec<crate::WorkflowHistoryEvent>,
    pub decisions: Vec<crate::WorkflowDecisionInspection>,
    pub grants: Vec<crate::WorkflowGrantInspection>,
    pub resource_leases: Vec<crate::WorkflowResourceLease>,
    pub outputs: Vec<crate::WorkflowOutputSummary>,
    /// Bounded direct child-run links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_run_links: Vec<crate::WorkflowRunLink>,
    /// Bounded descendants across the complete composed run hierarchy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descendant_runs: Vec<crate::WorkflowDescendantRunSummary>,
    /// Typed repeat outcomes for the root and bounded descendants, projected by normalized queries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repeat_outcomes: Vec<crate::WorkflowRepeatOutcomeSummary>,
    pub child_sessions: Vec<bcode_session_models::SessionSummary>,
    /// Which daemon currently coordinates this run and whether it is this daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<WorkflowCoordinatorStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_continuation_requires_resolved_admission() {
        let approved = WorkflowMutationApprovalResolution {
            continuation: None,
            approval_id: "approval".to_owned(),
            status: "approved".to_owned(),
            grant_id: Some("grant".to_owned()),
            activation_status: "pending".to_owned(),
        };
        assert!(approved.admits_continuation());
        assert!(approved.decision_applied(crate::WorkflowMutationApprovalDecision::Approve));
        assert!(!approved.decision_applied(crate::WorkflowMutationApprovalDecision::Deny));
        let denied = WorkflowMutationApprovalResolution {
            status: "denied".to_owned(),
            grant_id: None,
            ..approved.clone()
        };
        assert!(denied.decision_applied(crate::WorkflowMutationApprovalDecision::Deny));
        assert!(!denied.decision_applied(crate::WorkflowMutationApprovalDecision::Approve));
        let expired = WorkflowMutationApprovalResolution {
            status: "expired".to_owned(),
            ..denied
        };
        assert!(!expired.decision_applied(crate::WorkflowMutationApprovalDecision::Approve));
        assert!(!expired.decision_applied(crate::WorkflowMutationApprovalDecision::Deny));
        for status in ["expired", "denied", "pending", "future"] {
            assert!(
                !WorkflowMutationApprovalResolution {
                    status: status.to_owned(),
                    ..approved.clone()
                }
                .admits_continuation()
            );
        }
        for activation_status in ["running", "completed", "failed", "cancelled", "future"] {
            assert!(
                !WorkflowMutationApprovalResolution {
                    activation_status: activation_status.to_owned(),
                    ..approved.clone()
                }
                .admits_continuation()
            );
        }
        for grant_id in [None, Some(String::new()), Some(" ".to_owned())] {
            assert!(
                !WorkflowMutationApprovalResolution {
                    grant_id,
                    ..approved.clone()
                }
                .admits_continuation()
            );
        }
    }

    #[test]
    fn definition_snapshot_preserves_wire_shape_and_rejects_malformed_text() {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/workflows/source-defined-input.workflow.json"
        ))
        .unwrap();
        let definition: crate::WorkflowDefinition =
            serde_json::from_value(source["definition"].clone()).unwrap();
        // Whitespace is significant to the stored checksum even though parsing ignores it.
        let encoded = format!("\n{}\n", serde_json::to_string_pretty(&definition).unwrap());
        let wire = serde_json::json!({"definition_id":"definition","version":1,"checksum_sha256":"checksum","definition_json":encoded});
        let snapshot: WorkflowDefinitionSnapshot = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(snapshot.definition.definition(), &definition);
        assert_eq!(serde_json::to_value(snapshot).unwrap(), wire);
        let mut malformed = wire;
        malformed["definition_json"] = serde_json::json!("secret malformed text");
        let error = serde_json::from_value::<WorkflowDefinitionSnapshot>(malformed).unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn orphan_report_retains_skipped_status_and_apply_mode() {
        let value = serde_json::json!({"applied":false,"reconciled":[],"skipped":[{
            "run_id":"run","status":"paused","reason":"ownership not verified"}]});
        let report: OrphanedWorkflowRunReport = serde_json::from_value(value.clone()).unwrap();
        assert!(!report.applied);
        assert_eq!(report.skipped[0].status, RunStatus::Paused);
        assert_eq!(serde_json::to_value(report).unwrap(), value);
        assert!(
            serde_json::from_value::<WorkflowRunControlAction>(serde_json::json!("future"))
                .is_err()
        );
    }

    #[test]
    fn run_start_defaults_preserve_allowances() {
        let request: WorkflowRunStartRequest = serde_json::from_value(serde_json::json!({
            "definition_id":"definition", "definition_version":1,
            "workspace_snapshot":"workspace", "parent_session_id": "00000000-0000-0000-0000-000000000001"
        })).unwrap();
        assert_eq!(request.limits, WorkflowRunLimits::default());
        assert_eq!(request.limits.node_execution_cap, 1_000);
        assert_eq!(request.limits.concurrency_cap, 8);
        assert_eq!(request.limits.cycle_cap, 100);
        assert_eq!(request.limits.retry_cap, 3);
        assert!(request.parent_session_generation.is_none());
        assert!(request.run_id.is_none());
    }

    #[test]
    fn run_selection_preserves_exact_preset_generation() {
        let value = serde_json::json!({"preset":{"workflow_id":"wf","preset_id":"preset","preset_generation":7}});
        let selection: AuthoredWorkflowRunSelection =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(selection).unwrap(), value);
        assert!(
            serde_json::from_value::<AuthoredWorkflowRunSelection>(
                serde_json::json!({"future":{}})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<AuthoredWorkflowRunSelection>(
                serde_json::json!({"preset":{"workflow_id":"wf","preset_id":"preset"}})
            )
            .is_err()
        );
    }

    #[test]
    fn admission_failure_preserves_wire_shape() {
        let value = serde_json::json!({"failed":{"code":"denied","message":"Not authorized"}});
        let result: WorkflowRunAdmissionResult = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(result).unwrap(), value);
        assert!(
            serde_json::from_value::<WorkflowRunAdmissionResult>(serde_json::json!({"future":{}}))
                .is_err()
        );
    }
}
