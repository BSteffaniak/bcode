//! Portable run admission and observation data, independent of persistence implementation.
//! Existing wire representations are preserved. Authored provenance compatibility is recognized
//! by its version; run status variants are explicit and unknown variants are rejected.

/// Diagnostic artifact range compatibility boundary.
pub const WORKFLOW_DIAGNOSTIC_ARTIFACT_VERSION: u32 = 1;
/// Maximum bytes returned by one diagnostic artifact range.
pub const MAX_WORKFLOW_DIAGNOSTIC_RANGE_BYTES: u32 = 32_768;
/// Maximum retained diagnostic artifact bytes accepted by this export boundary.
pub const MAX_WORKFLOW_DIAGNOSTIC_ARTIFACT_BYTES: u64 = 524_288;

/// Exact dispatch-scoped sensitive diagnostic read; never accepts a storage path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDiagnosticArtifactRequest {
    /// Supported export contract version.
    pub version: u32,
    /// Canonical owning run.
    pub run_id: String,
    /// Exact persisted dispatch identity.
    pub dispatch_identity: String,
    /// Artifact identity recorded by the dispatch owner.
    pub artifact_id: String,
    /// Byte offset into the recorded artifact.
    pub offset: u64,
    /// Positive bounded range length.
    pub length: u32,
}

/// Producer-declared retained content completeness, not full workflow capture coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDiagnosticCompleteness {
    /// Entire declared diagnostic payload retained.
    Complete,
    /// All published retained bytes verified; original payload coverage is not certified.
    Retained,
    /// Only a bounded prefix retained.
    Truncated,
}

/// Verified bounded diagnostic bytes; private storage references are not exposed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDiagnosticArtifactRange {
    /// Export contract version.
    pub version: u32,
    /// Canonical owning run.
    pub run_id: String,
    /// Exact producing dispatch.
    pub dispatch_identity: String,
    /// Recorded diagnostic artifact identity.
    pub artifact_id: String,
    /// SHA-256 of the complete retained artifact, not this range.
    pub sha256: String,
    /// Complete retained artifact length.
    pub byte_len: u64,
    /// Producer-declared completeness.
    pub completeness: WorkflowDiagnosticCompleteness,
    /// Returned range offset.
    pub offset: u64,
    /// Sensitive bytes explicitly requested by the caller.
    pub bytes: Vec<u8>,
    /// Whether the returned range reaches the retained artifact end.
    pub eof: bool,
}

/// Recorded diagnostic metadata; discovery does not verify that physical bytes remain available.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDiagnosticArtifactSummary {
    /// Producer-recorded artifact identity.
    pub artifact_id: String,
    /// Publication-time SHA-256 of retained bytes.
    pub sha256: String,
    /// Publication-time retained byte length.
    pub byte_len: u64,
    /// Retained bytes do not certify original payload coverage.
    pub completeness: WorkflowDiagnosticCompleteness,
}

/// Bounded exact-dispatch diagnostic discovery; no private storage references are exposed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDiagnosticArtifacts {
    /// Discovery contract version.
    pub version: u32,
    /// Canonical owning run.
    pub run_id: String,
    /// Exact producing dispatch identity.
    pub dispatch_identity: String,
    /// Exportable recorded references, bounded to 100.
    pub artifacts: Vec<WorkflowDiagnosticArtifactSummary>,
    /// Recorded references unavailable for export; never silently discarded.
    pub unavailable: Vec<WorkflowDiagnosticUnavailableArtifact>,
    /// Successful writes omitted from bounded discovery. `None` means historical coverage unknown.
    /// `Some(0)` certifies no collection overflow, not full workflow capture.
    #[serde(default)]
    pub omitted_artifacts: Option<u64>,
}

/// One recorded artifact whose version, integrity metadata or size prevents export.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDiagnosticUnavailableArtifact {
    /// Recorded identity, not a guessed file locator.
    pub artifact_id: String,
    /// Normalized failure without private paths or producer error text.
    pub reason: WorkflowDiagnosticArtifactError,
}

/// Versioned finite transactional observation page, not reconstructed execution history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowObservationPage {
    /// Independently evolving diagnostic contract version.
    pub version: u32,
    /// Owning run identity.
    pub run_id: String,
    /// Whether observation preceded admission; historical runs are never backfilled.
    pub origin: WorkflowObservationOrigin,
    /// Exact finite source scope.
    pub sources: Vec<WorkflowObservationSource>,
    /// Explicit exclusions; finite coverage never implies complete execution history.
    pub limitations: Vec<String>,
    /// Fixed scan upper sequence; not a durable resume guarantee.
    pub through_sequence: u64,
    /// Exclusive continuation cursor.
    pub next_after_sequence: Option<u64>,
    /// Actual committed scalar transitions.
    pub observations: Vec<WorkflowTransitionObservation>,
}

/// Explicit observation start policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowObservationOrigin {
    /// Coverage started before canonical admission.
    Admission,
    /// Coverage was never enabled; empty results do not prove no transitions.
    NotEnabled,
}

/// Selected representation source facts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowObservationSource {
    /// Source identity.
    pub table: String,
    /// Exact selected scalar columns.
    pub columns: Vec<String>,
    /// Observed transition kinds.
    pub operations: Vec<String>,
}

/// Selected scalar fact with explicit type and null semantics.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum WorkflowObservationFact {
    /// Recorded null.
    Null,
    /// Recorded integer, including `SQLite` boolean representation.
    Integer(i64),
    /// Bounded identifier or status text.
    Text(String),
    /// Fact not retained, distinct from recorded null.
    Unavailable(WorkflowObservationUnavailableReason),
}

/// Explicit bounded fact omission reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowObservationUnavailableReason {
    /// Text exceeded the transactional byte allowance.
    Oversized,
    /// Unsupported source storage type.
    UnsupportedType,
}

/// One retained transition without private owner payloads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTransitionObservation {
    /// Canonical observation order, potentially sparse for this run.
    pub sequence: u64,
    /// Source table.
    pub source: String,
    /// Exact transition operation.
    pub operation: String,
    /// Scalar column values; unknown private payloads are not exported.
    pub facts: std::collections::BTreeMap<String, WorkflowObservationFact>,
    /// Actual source timestamp when available, never guessed.
    pub recorded_at_ms: Option<u64>,
    /// Source column for the recorded timestamp.
    pub timestamp_source: Option<String>,
}

/// Normalized export failures; no paths, owner messages or provider metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDiagnosticArtifactError {
    /// Unsupported request or recorded diagnostic version.
    UnsupportedVersion,
    /// Invalid identities or range bounds.
    InvalidRequest,
    /// No exact recorded receipt/reference, including interruption before settlement.
    Unavailable,
    /// Inconsistent identity, metadata or retained bytes.
    VerificationFailed,
    /// Storage/ownership admission could not be established.
    StorageUnavailable,
}

impl std::fmt::Display for WorkflowDiagnosticArtifactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedVersion => "workflow diagnostic version unsupported",
            Self::InvalidRequest => "invalid workflow diagnostic request",
            Self::Unavailable => "workflow diagnostic artifact unavailable",
            Self::VerificationFailed => "workflow diagnostic verification failed",
            Self::StorageUnavailable => "workflow diagnostic storage unavailable",
        })
    }
}

impl std::error::Error for WorkflowDiagnosticArtifactError {}

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

    /// Read exact bounded run lifecycle history without execution authority.
    ///
    /// # Errors
    /// Rejects unsupported queries, unavailable state and inconsistent pages.
    fn workflow_lifecycle_history(
        &self,
        query: WorkflowLifecycleHistoryQuery,
    ) -> impl std::future::Future<Output = Result<WorkflowLifecycleHistoryPage, Self::Error>> + Send;

    /// Read bounded pinned attempt history, not full workflow semantics.
    ///
    /// # Errors
    /// Rejects invalid queries, unavailable state and inconsistent pages.
    fn workflow_semantic_history(
        &self,
        query: WorkflowSemanticHistoryQuery,
    ) -> impl std::future::Future<Output = Result<WorkflowSemanticHistoryPage, Self::Error>> + Send;

    /// Read bounded legacy event history.
    ///
    /// # Errors
    /// Returns an error on unavailable/invalid state or transport failure.
    fn workflow_event_history(
        &self,
        run_id: String,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<WorkflowHistoryEvent>, Self::Error>> + Send;

    /// Read bounded finite transactional observations with explicit coverage.
    ///
    /// # Errors
    /// Returns an error for unsupported observation contracts, missing or damaged state,
    /// invalid bounds, or transport failure. Historical state is never reconstructed.
    fn workflow_observations(
        &self,
        run_id: String,
        after_sequence: Option<u64>,
        through_sequence: Option<u64>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<WorkflowObservationPage, Self::Error>> + Send;

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

    /// Persist replacement intent through ordinary admission and ownership checks.
    ///
    /// # Errors
    /// Rejects invalid successors, denied policy, foreign ownership, or conflicting intent.
    fn request_workflow_replacement(
        &self,
        request: WorkflowReplacementRequest,
    ) -> impl std::future::Future<Output = Result<WorkflowReplacementResponse, Self::Error>> + Send;

    /// Read a verified exhausted-repeat checkpoint without replay or mutation.
    ///
    /// # Errors
    /// Rejects ineligible, damaged, or ambiguous execution state.
    fn workflow_continuation_source(
        &self,
        run_id: String,
    ) -> impl std::future::Future<Output = Result<WorkflowContinuationSource, Self::Error>> + Send;

    /// Admit an explicitly authorized successor without reopening the predecessor.
    ///
    /// # Errors
    /// Rejects stale checkpoints, ownership conflicts, invalid grants, and conflicting retries.
    fn continue_workflow(
        &self,
        request: WorkflowContinuationRequest,
    ) -> impl std::future::Future<Output = Result<WorkflowRunStartResponse, Self::Error>> + Send;

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
    /// Control one exact run without associated-binding selection.
    ///
    /// # Errors
    /// Rejects missing runs, foreign ownership, denied policy, or invalid transitions.
    fn control_workflow_run(
        &self,
        run_id: String,
        action: WorkflowRunControlAction,
    ) -> impl std::future::Future<Output = Result<(Option<WorkflowRunSummary>, bool), Self::Error>> + Send;

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
    /// Maximum concurrently running nodes; `None` means no configured cap.
    /// Numeric legacy values retain their exact cap; the field must be present.
    #[serde(deserialize_with = "deserialize_concurrency_cap")]
    pub concurrency_cap: Option<u32>,
    /// Maximum cycle/repeat activations.
    pub cycle_cap: u32,
    /// Maximum attempts per activation.
    pub retry_cap: u32,
    /// Maximum run depth, counting the root as depth one.
    #[serde(default = "default_recursion_depth_cap")]
    pub recursion_depth_cap: u32,
    /// Maximum descendants of the root run.
    #[serde(default = "default_descendant_cap")]
    pub descendant_cap: u32,
}

/// Decode an explicitly present optional cap, preserving historical numeric limits.
/// Missing fields are rejected by Serde rather than interpreted as uncapped policy.
pub fn deserialize_concurrency_cap<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<u32>::deserialize(deserializer)
}

pub const fn default_recursion_depth_cap() -> u32 {
    8
}
pub const fn default_descendant_cap() -> u32 {
    64
}

impl Default for WorkflowRunLimits {
    fn default() -> Self {
        Self {
            deadline_at_ms: None,
            node_execution_cap: 1_000,
            concurrency_cap: None,
            cycle_cap: 100,
            retry_cap: 3,
            recursion_depth_cap: default_recursion_depth_cap(),
            descendant_cap: default_descendant_cap(),
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

/// Verified, bounded checkpoint for an exhausted repeat. Reading never acquires execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowContinuationSource {
    pub run: WorkflowRunSummary,
    pub definition: crate::WorkflowDefinition,
    pub input: serde_json::Value,
    pub repeat_node_id: String,
    pub graph_revision: u64,
    pub output_checksum: String,
    pub iterations_completed: u64,
    pub total_iterations_completed: u64,
    pub document_scope_id: String,
    pub limits: WorkflowRunLimits,
}

/// Explicit, retry-safe admission of a successor to an exhausted repeat.
/// The successor run ID is the idempotency key; conflicting content is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowContinuationRequest {
    pub source_run_id: String,
    pub expected_graph_revision: u64,
    pub expected_output_checksum: String,
    pub additional_iterations: u32,
    pub successor: WorkflowStartRequest,
}

/// Durable continuation lineage, independent of mutable presentation or model output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowContinuationLineage {
    pub predecessor_run_id: String,
    pub document_scope_id: String,
    pub prior_iterations: u64,
    pub additional_iterations: u32,
}

/// Request a durable replacement of one exact run. A pending response is not admission
/// of the successor; completion requires quiescence, compatible ownership, and policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowReplacementRequest {
    /// Exact run to stop and replace.
    pub old_run_id: String,
    /// Optional published authored selection. The resolved definition must match `successor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_selection: Option<AuthoredWorkflowRunSelection>,
    /// Select an export from the old run's pinned package. Mutually exclusive with authored selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_export: Option<crate::WorkflowPackageExportIdentity>,
    /// Configuration for the selected authored revision or preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<serde_json::Value>,
    /// Successor prepared through ordinary workflow-start checks. A stable run ID is required.
    pub successor: WorkflowRunStartRequest,
}

/// Durable replacement intent acknowledgment, not a running-successor acknowledgment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowReplacementResponse {
    pub old_run_id: String,
    pub successor_run_id: String,
    /// False for an identical already-persisted request.
    pub created: bool,
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
    /// Whether durable recovery fencing prohibits new execution. Absent in older
    /// observations means unknown, not permission to execute; commands remain authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_only: Option<bool>,
    /// Original execution artifact retained by a recovery barrier. Absence does not
    /// establish cross-artifact compatibility or authorize execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_source_artifact_id: Option<String>,
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
    /// Remove a repair-required run's session binding without changing execution outcomes.
    Detach,
    /// Withdraw the currently pending replacement without undoing cancellation.
    WithdrawReplacement,
    /// Reauthorize and complete a quiescent pending replacement.
    CompleteReplacement,
    /// Explicit operator grant for an active run. Does not resume or dispatch work.
    /// The expected cap fences stale grants; exact retries are idempotent.
    IncreaseExecutionAllowance {
        expected_cap: u64,
        target_cap: u64,
    },
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

/// Durable attempt-history wire boundary. Unknown versions reject.
pub const WORKFLOW_SEMANTIC_HISTORY_VERSION: u32 = 1;

/// Finite event-source declaration, never full workflow semantic coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSemanticHistoryScope {
    /// Exactly preparation, receipt commitment and terminal attempt settlement.
    /// Excludes graph, control, run lifecycle and all other workflow sources.
    AttemptsV1,
}

/// Immutable scoped coverage. Exhausting retained rows never erases a gap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowSemanticHistoryCoverage {
    /// Declared obligations guarded from admission through the pinned high-water.
    CompleteFromAdmission,
    /// Earlier or intervening obligations were not recorded; no synthesis allowed.
    HistoricalGap,
    /// Owning representation cannot provide the declared scope.
    Unsupported,
    /// Availability or integrity could not be established.
    Unavailable,
}

/// Authoritative terminal attempt outcome, without private owner messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSemanticAttemptOutcome {
    /// Successful settlement.
    Succeeded,
    /// Failed settlement.
    Failed,
    /// Cancelled settlement.
    Cancelled,
}

/// Only guarded attempt obligations, not a parallel inventory of runtime events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowSemanticHistoryPayload {
    /// Preparation committed; does not prove dispatch or absence of effects.
    AttemptPrepared {
        /// Canonical operation side-effect classification.
        side_effect: DispatchSideEffect,
    },
    /// Dispatch receipt committed; private receipt contents are absent.
    ReceiptCommitted {},
    /// Authoritative settlement committed.
    AttemptTerminal {
        /// Normalized terminal outcome.
        outcome: WorkflowSemanticAttemptOutcome,
    },
}

/// Run-local durable observation. Unknown payload variants reject, never guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSemanticHistoryEvent {
    /// Exact owning run.
    pub run_id: String,
    /// Run-local sequence starting at one, contiguous when coverage is complete.
    pub sequence: u64,
    /// Stable lowercase SHA-256 operation identity, retained on duplicate commitment.
    pub operation_id: String,
    /// Recorded Unix timestamp in milliseconds, not reconstruction time.
    pub recorded_at_ms: u64,
    /// Exact normalized attempt coordinates; no fencing credentials.
    pub attempt: WorkflowHistoryAttemptCorrelation,
    /// Lowercase SHA-256 canonical dispatch identity.
    pub dispatch_identity: String,
    /// Typed facts only, never receipt, provider or free-form message JSON.
    pub payload: WorkflowSemanticHistoryPayload,
}

/// Exclusive continuation pinned to exact run, scope, high-water and coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSemanticHistoryCursor {
    /// Wire version, independent of storage schema.
    pub version: u32,
    /// Exact owning run.
    pub run_id: String,
    /// Finite event-source declaration.
    pub scope: WorkflowSemanticHistoryScope,
    /// Immutable scoped coverage, including historical gaps.
    pub coverage: WorkflowSemanticHistoryCoverage,
    /// Immutable inclusive high-water; zero denotes no recorded events.
    pub through_sequence: u64,
    /// Exclusive last consumed sequence.
    pub after_sequence: u64,
}

/// Bounded page, not a durable resume grant or complete workflow-history claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSemanticHistoryPage {
    /// Pinned observation including the exclusive input position.
    pub cursor: WorkflowSemanticHistoryCursor,
    /// At most 100 strictly ordered events through the pinned high-water.
    pub events: Vec<WorkflowSemanticHistoryEvent>,
    /// Continuation; absent only when the retained range is exhausted or unavailable.
    pub next_cursor: Option<WorkflowSemanticHistoryCursor>,
}

/// Exact bounded read, independent of frontend, storage representation or execution owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSemanticHistoryQuery {
    /// Wire compatibility version.
    pub version: u32,
    /// Exact owning run.
    pub run_id: String,
    /// Explicit limited obligation scope.
    pub scope: WorkflowSemanticHistoryScope,
    /// Absent initially; subsequent reads retain the exact pinned cursor.
    pub cursor: Option<WorkflowSemanticHistoryCursor>,
    /// Positive bound, at most 100 events.
    pub limit: usize,
}

impl WorkflowSemanticHistoryQuery {
    /// Validate before any storage access.
    ///
    /// # Errors
    /// Rejects unsupported versions, invalid bounds or inconsistent cursor identities.
    pub fn validate(&self) -> Result<(), WorkflowSemanticHistoryError> {
        if self.version != WORKFLOW_SEMANTIC_HISTORY_VERSION {
            return Err(WorkflowSemanticHistoryError::UnsupportedVersion);
        }
        if !semantic_history_id(&self.run_id) || !(1..=100).contains(&self.limit) {
            return Err(WorkflowSemanticHistoryError::InvalidPage);
        }
        if let Some(cursor) = &self.cursor {
            if cursor.version != self.version {
                return Err(WorkflowSemanticHistoryError::UnsupportedVersion);
            }
            if cursor.run_id != self.run_id
                || cursor.scope != self.scope
                || cursor.after_sequence > cursor.through_sequence
            {
                return Err(WorkflowSemanticHistoryError::ChangedBoundary);
            }
        }
        Ok(())
    }

    /// Check the returned page against the exact query, including requested size.
    ///
    /// # Errors
    /// Rejects invalid queries, changed boundaries, versions, scope or oversized replies.
    pub fn validate_page(
        &self,
        page: &WorkflowSemanticHistoryPage,
    ) -> Result<(), WorkflowSemanticHistoryError> {
        self.validate()?;
        if page.events.len() > self.limit || page.cursor.scope != self.scope {
            return Err(WorkflowSemanticHistoryError::InvalidPage);
        }
        page.validate(&self.run_id, self.cursor.as_ref())
    }
}

/// Content-free validation failures; no private payload is echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowSemanticHistoryError {
    /// Future wire version.
    UnsupportedVersion,
    /// Inconsistent identity, bounds, ordering or normalized facts.
    InvalidPage,
    /// Changed pinned identity, scope, coverage or high-water.
    ChangedBoundary,
}

impl WorkflowSemanticHistoryPage {
    /// Validate a bounded page against its exact run and optional requested cursor.
    /// Checks contract consistency, not canonical authority or producer honesty.
    ///
    /// # Errors
    /// Rejects future versions, changed boundaries, malformed identities, oversized
    /// pages, inconsistent ordering and invalid continuations.
    pub fn validate(
        &self,
        run_id: &str,
        requested: Option<&WorkflowSemanticHistoryCursor>,
    ) -> Result<(), WorkflowSemanticHistoryError> {
        use WorkflowSemanticHistoryError::{ChangedBoundary, InvalidPage, UnsupportedVersion};
        let cursor = &self.cursor;
        if cursor.version != WORKFLOW_SEMANTIC_HISTORY_VERSION {
            return Err(UnsupportedVersion);
        }
        if requested.is_some_and(|value| value != cursor) {
            return Err(ChangedBoundary);
        }
        if cursor.run_id != run_id
            || !semantic_history_id(run_id)
            || cursor.after_sequence > cursor.through_sequence
            || self.events.len() > 100
            || (requested.is_none() && cursor.after_sequence != 0)
        {
            return Err(InvalidPage);
        }
        if matches!(
            cursor.coverage,
            WorkflowSemanticHistoryCoverage::Unsupported
                | WorkflowSemanticHistoryCoverage::Unavailable
        ) {
            return if self.events.is_empty() && self.next_cursor.is_none() {
                Ok(())
            } else {
                Err(InvalidPage)
            };
        }
        let mut last = cursor.after_sequence;
        let mut operations = std::collections::BTreeSet::new();
        for event in &self.events {
            if event.run_id != run_id
                || event.attempt.run_id != run_id
                || !semantic_history_id(&event.attempt.node_id)
                || !semantic_history_id(&event.attempt.activation_id)
                || event.attempt.attempt == 0
                || !semantic_history_digest(&event.operation_id)
                || !semantic_history_digest(&event.dispatch_identity)
                || !operations.insert(&event.operation_id)
                || event.sequence <= last
                || event.sequence > cursor.through_sequence
                || (cursor.coverage == WorkflowSemanticHistoryCoverage::CompleteFromAdmission
                    && last.checked_add(1) != Some(event.sequence))
            {
                return Err(InvalidPage);
            }
            last = event.sequence;
        }
        match &self.next_cursor {
            Some(next) => {
                let mut expected = cursor.clone();
                expected.after_sequence = last;
                if next != &expected {
                    return Err(ChangedBoundary);
                }
                if self.events.is_empty() || last == cursor.through_sequence {
                    return Err(InvalidPage);
                }
            }
            None if cursor.coverage == WorkflowSemanticHistoryCoverage::CompleteFromAdmission
                && last != cursor.through_sequence =>
            {
                return Err(InvalidPage);
            }
            None => {}
        }
        Ok(())
    }
}

fn semantic_history_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

fn semantic_history_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Independent graph-history wire boundary; attempts and lifecycle stay unchanged.
pub const WORKFLOW_GRAPH_HISTORY_VERSION: u32 = 1;

/// Finite direct-run graph source declaration, never full workflow history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowGraphHistoryScope {
    /// Initial graph and committed revision publication, including canonical rows.
    /// Excludes candidate staging, cancellation acceptance and runtime activations.
    GraphRevisionV1,
}

/// Fixed-size normalized graph facts; no executable definitions or private blobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowGraphHistoryPayload {
    /// Initial graph committed at admission, not reconstructed from historical state.
    GraphInitialized {
        /// Initial positive revision.
        revision: u64,
        /// Full normalized definition and canonical edge-ID map SHA-256.
        graph_sha256: String,
    },
    /// Exact graph edit committed; does not certify cancellation acceptance history.
    GraphPublished {
        /// Exact authored mutation identity, at most 256 UTF-8 bytes.
        mutation_id: String,
        /// Previous committed revision.
        previous_revision: u64,
        /// Committed successor revision.
        revision: u64,
        /// Full previous definition and canonical edge-ID map SHA-256.
        previous_graph_sha256: String,
        /// Full committed definition and canonical edge-ID map SHA-256.
        graph_sha256: String,
        /// SHA-256 of the exact typed reconciliation declaration, not effect proof.
        reconciliation_sha256: String,
    },
}
impl WorkflowGraphHistoryPayload {
    fn valid(&self) -> bool {
        match self {
            Self::GraphInitialized {
                revision,
                graph_sha256,
            } => *revision == 1 && semantic_history_digest(graph_sha256),
            Self::GraphPublished {
                mutation_id,
                previous_revision,
                revision,
                previous_graph_sha256,
                graph_sha256,
                reconciliation_sha256,
            } => {
                !mutation_id.is_empty()
                    && mutation_id.len() <= 256
                    && !mutation_id.chars().any(char::is_control)
                    && *previous_revision > 0
                    && previous_revision.checked_add(1) == Some(*revision)
                    && [previous_graph_sha256, graph_sha256, reconciliation_sha256]
                        .into_iter()
                        .all(|value| semantic_history_digest(value))
            }
        }
    }
}

/// Run-local graph observation; no attempt coordinates are invented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphHistoryEvent {
    /// Exact owning run.
    pub run_id: String,
    /// Run-local observation sequence.
    pub sequence: u64,
    /// Stable lowercase SHA-256 operation identity.
    pub operation_id: String,
    /// Unix milliseconds recorded in the source transaction.
    pub recorded_at_ms: u64,
    /// Normalized committed facts.
    pub payload: WorkflowGraphHistoryPayload,
}

/// Exclusive continuation pinned to exact scope, coverage and high-water.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphHistoryCursor {
    /// Independent wire version.
    pub version: u32,
    /// Exact owning run.
    pub run_id: String,
    /// Finite source scope.
    pub scope: WorkflowGraphHistoryScope,
    /// Immutable coverage; exhaustion never heals a gap.
    pub coverage: WorkflowSemanticHistoryCoverage,
    /// Inclusive pinned observation high-water.
    pub through_sequence: u64,
    /// Exclusive last consumed sequence.
    pub after_sequence: u64,
}

/// Bounded graph facts, not durable resume or full semantic coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphHistoryPage {
    /// Exact input observation boundary.
    pub cursor: WorkflowGraphHistoryCursor,
    /// At most 100 ordered observations.
    pub events: Vec<WorkflowGraphHistoryEvent>,
    /// Exact continuation when retained observations remain.
    pub next_cursor: Option<WorkflowGraphHistoryCursor>,
}

/// Frontend-independent bounded graph-history query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphHistoryQuery {
    /// Independent wire version.
    pub version: u32,
    /// Exact owning run.
    pub run_id: String,
    /// Explicit finite source scope.
    pub scope: WorkflowGraphHistoryScope,
    /// Absent initially; otherwise exact pinned continuation.
    pub cursor: Option<WorkflowGraphHistoryCursor>,
    /// Positive event bound, at most 100.
    pub limit: usize,
}
impl WorkflowGraphHistoryQuery {
    /// Validate query compatibility and pinned coordinates.
    /// # Errors
    /// Rejects unknown versions, invalid bounds or inconsistent cursor identity.
    pub fn validate(&self) -> Result<(), WorkflowSemanticHistoryError> {
        use WorkflowSemanticHistoryError::{InvalidPage, UnsupportedVersion};
        if self.version != WORKFLOW_GRAPH_HISTORY_VERSION {
            return Err(UnsupportedVersion);
        }
        if !semantic_history_id(&self.run_id) || !(1..=100).contains(&self.limit) {
            return Err(InvalidPage);
        }
        if let Some(cursor) = &self.cursor
            && (cursor.version != self.version
                || cursor.run_id != self.run_id
                || cursor.scope != self.scope
                || cursor.after_sequence > cursor.through_sequence)
        {
            return Err(InvalidPage);
        }
        Ok(())
    }
    /// Verify the response against this exact query.
    /// # Errors
    /// Rejects an invalid query, changed boundary or oversized response.
    pub fn validate_page(
        &self,
        page: &WorkflowGraphHistoryPage,
    ) -> Result<(), WorkflowSemanticHistoryError> {
        self.validate()?;
        if page.cursor.scope != self.scope || page.events.len() > self.limit {
            return Err(WorkflowSemanticHistoryError::InvalidPage);
        }
        page.validate(&self.run_id, self.cursor.as_ref())
    }
}
impl WorkflowGraphHistoryPage {
    /// Validate portable consistency, not producer honesty or canonical authority.
    /// # Errors
    /// Rejects unsupported versions, changed boundaries and malformed observations.
    pub fn validate(
        &self,
        run: &str,
        requested: Option<&WorkflowGraphHistoryCursor>,
    ) -> Result<(), WorkflowSemanticHistoryError> {
        use WorkflowSemanticHistoryError::{ChangedBoundary, InvalidPage, UnsupportedVersion};
        let cursor = &self.cursor;
        if cursor.version != WORKFLOW_GRAPH_HISTORY_VERSION {
            return Err(UnsupportedVersion);
        }
        if requested.is_some_and(|value| value != cursor) {
            return Err(ChangedBoundary);
        }
        if cursor.run_id != run
            || !semantic_history_id(run)
            || cursor.after_sequence > cursor.through_sequence
            || self.events.len() > 100
            || (requested.is_none() && cursor.after_sequence != 0)
        {
            return Err(InvalidPage);
        }
        if matches!(
            cursor.coverage,
            WorkflowSemanticHistoryCoverage::Unsupported
                | WorkflowSemanticHistoryCoverage::Unavailable
        ) {
            return if self.events.is_empty() && self.next_cursor.is_none() {
                Ok(())
            } else {
                Err(InvalidPage)
            };
        }
        let mut last = cursor.after_sequence;
        let mut operations = std::collections::BTreeSet::new();
        for event in &self.events {
            if event.run_id != run
                || !event.payload.valid()
                || !semantic_history_digest(&event.operation_id)
                || !operations.insert(&event.operation_id)
                || event.sequence <= last
                || event.sequence > cursor.through_sequence
                || (cursor.coverage == WorkflowSemanticHistoryCoverage::CompleteFromAdmission
                    && last.checked_add(1) != Some(event.sequence))
            {
                return Err(InvalidPage);
            }
            last = event.sequence;
        }
        match &self.next_cursor {
            Some(next) => {
                let mut expected = cursor.clone();
                expected.after_sequence = last;
                if self.events.is_empty() || last >= cursor.through_sequence || *next != expected {
                    return Err(InvalidPage);
                }
            }
            None if cursor.coverage == WorkflowSemanticHistoryCoverage::CompleteFromAdmission
                && last != cursor.through_sequence =>
            {
                return Err(InvalidPage);
            }
            None => {}
        }
        Ok(())
    }
}

/// Independent lifecycle-history compatibility boundary; attempts v1 is unchanged.
pub const WORKFLOW_LIFECYCLE_HISTORY_VERSION: u32 = 1;

/// Only run admission and committed status changes, not full workflow semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowLifecycleHistoryScope {
    /// Admission and status transitions, including terminal transitions exactly once.
    RunLifecycleV1,
}

/// Normalized canonical run facts; no messages, receipts or authority tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowLifecycleHistoryPayload {
    /// Run insertion committed.
    RunAdmitted {
        /// Status recorded at admission.
        status: RunStatus,
    },
    /// Status change committed. Terminal status is this same fact, not another event.
    RunStatusChanged {
        /// Previous committed status.
        previous: RunStatus,
        /// New committed status.
        status: RunStatus,
    },
}

impl WorkflowLifecycleHistoryPayload {
    fn valid(&self) -> bool {
        match self {
            Self::RunAdmitted { .. } => true,
            Self::RunStatusChanged { previous, status } => previous != status,
        }
    }
}

/// Run-local durable observation. Unknown payload variants reject, never guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLifecycleHistoryEvent {
    /// Exact owning run.
    pub run_id: String,
    /// Run-local sequence starting at one, contiguous when coverage is complete.
    pub sequence: u64,
    /// Stable lowercase SHA-256 operation identity, retained on duplicate commitment.
    pub operation_id: String,
    /// Recorded Unix timestamp in milliseconds, not reconstruction time.
    pub recorded_at_ms: u64,
    /// Typed facts only, never receipt, provider or free-form message JSON.
    pub payload: WorkflowLifecycleHistoryPayload,
}

/// Exclusive continuation pinned to exact run, scope, high-water and coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLifecycleHistoryCursor {
    /// Wire version, independent of storage schema.
    pub version: u32,
    /// Exact owning run.
    pub run_id: String,
    /// Finite event-source declaration.
    pub scope: WorkflowLifecycleHistoryScope,
    /// Immutable scoped coverage, including historical gaps.
    pub coverage: WorkflowSemanticHistoryCoverage,
    /// Immutable inclusive high-water; zero denotes no recorded events.
    pub through_sequence: u64,
    /// Exclusive last consumed sequence.
    pub after_sequence: u64,
}

/// Bounded page, not a durable resume grant or complete workflow-history claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLifecycleHistoryPage {
    /// Pinned observation including the exclusive input position.
    pub cursor: WorkflowLifecycleHistoryCursor,
    /// At most 100 strictly ordered events through the pinned high-water.
    pub events: Vec<WorkflowLifecycleHistoryEvent>,
    /// Continuation; absent only when the retained range is exhausted or unavailable.
    pub next_cursor: Option<WorkflowLifecycleHistoryCursor>,
}

/// Exact bounded read, independent of frontend, storage representation or execution owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLifecycleHistoryQuery {
    /// Wire compatibility version.
    pub version: u32,
    /// Exact owning run.
    pub run_id: String,
    /// Explicit limited obligation scope.
    pub scope: WorkflowLifecycleHistoryScope,
    /// Absent initially; subsequent reads retain the exact pinned cursor.
    pub cursor: Option<WorkflowLifecycleHistoryCursor>,
    /// Positive bound, at most 100 events.
    pub limit: usize,
}

impl WorkflowLifecycleHistoryQuery {
    /// Validate before any storage access.
    ///
    /// # Errors
    /// Rejects unsupported versions, invalid bounds or inconsistent cursor identities.
    pub fn validate(&self) -> Result<(), WorkflowSemanticHistoryError> {
        if self.version != WORKFLOW_LIFECYCLE_HISTORY_VERSION {
            return Err(WorkflowSemanticHistoryError::UnsupportedVersion);
        }
        if !semantic_history_id(&self.run_id) || !(1..=100).contains(&self.limit) {
            return Err(WorkflowSemanticHistoryError::InvalidPage);
        }
        if let Some(cursor) = &self.cursor {
            if cursor.version != self.version {
                return Err(WorkflowSemanticHistoryError::UnsupportedVersion);
            }
            if cursor.run_id != self.run_id
                || cursor.scope != self.scope
                || cursor.after_sequence > cursor.through_sequence
            {
                return Err(WorkflowSemanticHistoryError::ChangedBoundary);
            }
        }
        Ok(())
    }

    /// Check the returned page against the exact query, including requested size.
    ///
    /// # Errors
    /// Rejects invalid queries, changed boundaries, versions, scope or oversized replies.
    pub fn validate_page(
        &self,
        page: &WorkflowLifecycleHistoryPage,
    ) -> Result<(), WorkflowSemanticHistoryError> {
        self.validate()?;
        if page.events.len() > self.limit || page.cursor.scope != self.scope {
            return Err(WorkflowSemanticHistoryError::InvalidPage);
        }
        page.validate(&self.run_id, self.cursor.as_ref())
    }
}

impl WorkflowLifecycleHistoryPage {
    /// Validate a bounded page against its exact run and optional requested cursor.
    /// Checks contract consistency, not canonical authority or producer honesty.
    ///
    /// # Errors
    /// Rejects future versions, changed boundaries, malformed identities, oversized
    /// pages, inconsistent ordering and invalid continuations.
    pub fn validate(
        &self,
        run_id: &str,
        requested: Option<&WorkflowLifecycleHistoryCursor>,
    ) -> Result<(), WorkflowSemanticHistoryError> {
        use WorkflowSemanticHistoryError::{ChangedBoundary, InvalidPage, UnsupportedVersion};
        let cursor = &self.cursor;
        if cursor.version != WORKFLOW_LIFECYCLE_HISTORY_VERSION {
            return Err(UnsupportedVersion);
        }
        if requested.is_some_and(|value| value != cursor) {
            return Err(ChangedBoundary);
        }
        if cursor.run_id != run_id
            || !semantic_history_id(run_id)
            || cursor.after_sequence > cursor.through_sequence
            || self.events.len() > 100
            || (requested.is_none() && cursor.after_sequence != 0)
        {
            return Err(InvalidPage);
        }
        if matches!(
            cursor.coverage,
            WorkflowSemanticHistoryCoverage::Unsupported
                | WorkflowSemanticHistoryCoverage::Unavailable
        ) {
            return if self.events.is_empty() && self.next_cursor.is_none() {
                Ok(())
            } else {
                Err(InvalidPage)
            };
        }
        let mut last = cursor.after_sequence;
        let mut operations = std::collections::BTreeSet::new();
        for event in &self.events {
            if event.run_id != run_id
                || !event.payload.valid()
                || !semantic_history_digest(&event.operation_id)
                || !operations.insert(&event.operation_id)
                || event.sequence <= last
                || event.sequence > cursor.through_sequence
                || (cursor.coverage == WorkflowSemanticHistoryCoverage::CompleteFromAdmission
                    && last.checked_add(1) != Some(event.sequence))
            {
                return Err(InvalidPage);
            }
            last = event.sequence;
        }
        match &self.next_cursor {
            Some(next) => {
                let mut expected = cursor.clone();
                expected.after_sequence = last;
                if next != &expected {
                    return Err(ChangedBoundary);
                }
                if self.events.is_empty() || last == cursor.through_sequence {
                    return Err(InvalidPage);
                }
            }
            None if cursor.coverage == WorkflowSemanticHistoryCoverage::CompleteFromAdmission
                && last != cursor.through_sequence =>
            {
                return Err(InvalidPage);
            }
            None => {}
        }
        Ok(())
    }
}

/// Current inventory wire boundary, independent of semantic history.
pub const WORKFLOW_INVENTORY_VERSION: u32 = 1;

/// Independently generation-fenced canonical inventory sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowInventorySource {
    /// Retained canonical outputs.
    Outputs,
    /// Canonical attempts, including unavailable execution-session links.
    AttemptContributors,
    /// Canonical direct child lineage; recursively query each verified child separately.
    ChildRuns,
}

/// Exclusive examined source position; never a guessed session position.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "identity",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkflowInventoryKey {
    /// Canonical output identity.
    Output(String),
    /// Canonical dispatch identity, including attempts without session metadata.
    Dispatch(String),
    /// Canonical direct child run identity.
    ChildRun(String),
}
impl WorkflowInventoryKey {
    fn valid_for(&self, source: WorkflowInventorySource) -> bool {
        match (self, source) {
            (Self::Output(id), WorkflowInventorySource::Outputs)
            | (Self::ChildRun(id), WorkflowInventorySource::ChildRuns) => semantic_history_id(id),
            (Self::Dispatch(id), WorkflowInventorySource::AttemptContributors) => {
                semantic_history_digest(id)
            }
            _ => false,
        }
    }
}

/// Current source membership pin. No historical snapshot or durable resume promise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowInventoryCursor {
    /// Independent wire version.
    pub version: u32,
    /// Exact queried run.
    pub run_id: String,
    /// Exact independently fenced source.
    pub source: WorkflowInventorySource,
    /// Verified observer installation identity.
    pub installation_id: String,
    /// Verified graph revision.
    pub graph_revision: u64,
    /// Durable source-specific generation; zero is valid at verified installation.
    pub source_generation: u64,
    /// Exclusive examined source key.
    pub after: Option<WorkflowInventoryKey>,
}

/// Bounded current inventory request; reads never install or repair observers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowInventoryQuery {
    /// Independent wire version.
    pub version: u32,
    /// Exact queried run.
    pub run_id: String,
    /// Explicit source, never inferred from an empty response.
    pub source: WorkflowInventorySource,
    /// Exact prior continuation, absent initially.
    pub cursor: Option<WorkflowInventoryCursor>,
    /// Maximum entries, one through 100.
    pub limit: usize,
}

/// Explicit current-source availability; never admission-history coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowInventoryCoverage {
    /// Installed source observers verify current membership; exhaustion needs no continuation.
    CurrentInventoryComplete,
    /// Source verification is unavailable; no empty-inventory assertion.
    Unavailable {
        /// Normalized reason only.
        reason: WorkflowInventoryUnavailableReason,
    },
}

/// Secret-safe current inventory failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowInventoryUnavailableReason {
    /// Installation missing; normal reads must not install it.
    MissingInstallation,
    /// Unsupported representation or source.
    Unsupported,
    /// Required durable proof or provenance absent.
    MissingProof,
    /// Durable coordinates conflict.
    Inconsistent,
}

/// Diagnostic applicability is unavailable until operation/receipt semantics classify it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowInventoryDiagnosticApplicability {
    /// Domain classification unsupported; neither applicable nor not-applicable inferred.
    Unsupported,
}

/// Verified durable session membership, independent of live session ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowInventorySessionLink {
    /// Exact canonical attempt/session relation.
    Verified {
        /// Opaque session-domain identity.
        session_id: String,
        /// Immutable workspace snapshot identity.
        workspace_snapshot: String,
        /// Durable link acceptance timestamp.
        created_at_ms: u64,
    },
    /// Missing or unsupported link metadata remains explicitly unavailable.
    Unavailable {
        /// Normalized provenance failure.
        reason: WorkflowInventoryUnavailableReason,
    },
}

/// Current canonical source facts, not a runtime inventory or terminal-idle inference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowInventoryEntry {
    /// Canonical output whose stored proof was verified, not synthesized on read.
    Output {
        /// Exact verified output and checksum proof.
        output: WorkflowOutputInspection,
    },
    /// Output identity retained while required stored proof is unavailable.
    OutputUnavailable {
        /// Canonical output identity.
        output_id: String,
        /// Normalized missing proof reason.
        reason: WorkflowInventoryUnavailableReason,
    },
    /// Canonical dispatch coordinates with explicit link and applicability status.
    AttemptContributor {
        /// Canonical attempt fact.
        attempt: AttemptSummary,
        /// Verified link or explicit unavailable provenance.
        session: WorkflowInventorySessionLink,
        /// Domain-owned applicability, currently unsupported.
        diagnostic_applicability: WorkflowInventoryDiagnosticApplicability,
    },
    /// Canonical direct child membership.
    ChildRun {
        /// Exact child identity.
        child_run_id: String,
        /// Exact composition root lineage identity; not an allowance-generation fence.
        root_run_id: String,
        /// Parent activation that admitted the child.
        parent: WorkflowHistoryAttemptCorrelation,
    },
}
impl WorkflowInventoryEntry {
    /// Source key used for ordered continuation validation.
    #[must_use]
    pub fn key(&self) -> WorkflowInventoryKey {
        match self {
            Self::Output { output } => WorkflowInventoryKey::Output(output.output_id.clone()),
            Self::OutputUnavailable { output_id, .. } => {
                WorkflowInventoryKey::Output(output_id.clone())
            }
            Self::AttemptContributor { attempt, .. } => {
                WorkflowInventoryKey::Dispatch(attempt.dispatch_identity.clone())
            }
            Self::ChildRun { child_run_id, .. } => {
                WorkflowInventoryKey::ChildRun(child_run_id.clone())
            }
        }
    }
}

/// Bounded generation-verified current membership. Changes invalidate continuation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowInventoryPage {
    /// Exact input pin, including exclusive examined key.
    pub cursor: WorkflowInventoryCursor,
    /// Current source verification, never complete-from-admission history.
    pub coverage: WorkflowInventoryCoverage,
    /// Strictly ordered bounded facts.
    pub entries: Vec<WorkflowInventoryEntry>,
    /// Exact same pin with advanced source key; none means scoped exhaustion only.
    pub next_cursor: Option<WorkflowInventoryCursor>,
}
impl WorkflowInventoryQuery {
    /// Validate exact source, cursor coordinates and request bounds.
    /// # Errors
    /// Rejects future versions, wrong source/key/run, invalid installation or bounds.
    pub fn validate(&self) -> Result<(), WorkflowSemanticHistoryError> {
        use WorkflowSemanticHistoryError::{InvalidPage, UnsupportedVersion};
        if self.version != WORKFLOW_INVENTORY_VERSION {
            return Err(UnsupportedVersion);
        }
        if !semantic_history_id(&self.run_id) || !(1..=100).contains(&self.limit) {
            return Err(InvalidPage);
        }
        if let Some(c) = &self.cursor
            && (c.version != self.version
                || c.run_id != self.run_id
                || c.source != self.source
                || !semantic_history_id(&c.installation_id)
                || c.graph_revision == 0
                || c.after
                    .as_ref()
                    .is_some_and(|key| !key.valid_for(self.source)))
        {
            return Err(InvalidPage);
        }
        Ok(())
    }
    /// Validate exact response pin, ordered entries and advancing continuation.
    /// # Errors
    /// Rejects mismatched boundaries, future versions, inconsistent facts or stalled pages.
    pub fn validate_page(
        &self,
        page: &WorkflowInventoryPage,
    ) -> Result<(), WorkflowSemanticHistoryError> {
        use WorkflowSemanticHistoryError::{ChangedBoundary, InvalidPage};
        self.validate()?;
        let c = &page.cursor;
        let pinned = Self {
            cursor: Some(c.clone()),
            ..self.clone()
        };
        pinned.validate()?;
        if self.cursor.as_ref().is_some_and(|requested| requested != c) {
            return Err(ChangedBoundary);
        }
        if self.cursor.is_none() && c.after.is_some() || page.entries.len() > self.limit {
            return Err(InvalidPage);
        }
        if matches!(page.coverage, WorkflowInventoryCoverage::Unavailable { .. }) {
            return if page.entries.is_empty() && page.next_cursor.is_none() {
                Ok(())
            } else {
                Err(InvalidPage)
            };
        }
        let mut last = c.after.clone();
        for entry in &page.entries {
            let key = entry.key();
            if !key.valid_for(self.source) || last.as_ref().is_some_and(|previous| previous >= &key)
            {
                return Err(InvalidPage);
            }
            match entry {
                WorkflowInventoryEntry::Output { output }
                    if output.run_id != self.run_id
                        || output.version != 1
                        || !semantic_history_digest(&output.checksum_sha256) =>
                {
                    return Err(InvalidPage);
                }
                WorkflowInventoryEntry::AttemptContributor { attempt, .. }
                    if attempt.run_id != self.run_id
                        || attempt.attempt == 0
                        || !semantic_history_id(&attempt.node_id)
                        || !semantic_history_id(&attempt.activation_id) =>
                {
                    return Err(InvalidPage);
                }
                WorkflowInventoryEntry::ChildRun {
                    parent,
                    root_run_id,
                    ..
                } if parent.run_id != self.run_id
                    || parent.attempt == 0
                    || !semantic_history_id(root_run_id) =>
                {
                    return Err(InvalidPage);
                }
                _ => {}
            }
            match entry {
                WorkflowInventoryEntry::AttemptContributor {
                    session:
                        WorkflowInventorySessionLink::Verified {
                            session_id,
                            workspace_snapshot,
                            ..
                        },
                    ..
                } if !semantic_history_id(session_id)
                    || !semantic_history_id(workspace_snapshot) =>
                {
                    return Err(InvalidPage);
                }
                WorkflowInventoryEntry::ChildRun { parent, .. }
                    if !semantic_history_id(&parent.node_id)
                        || !semantic_history_id(&parent.activation_id) =>
                {
                    return Err(InvalidPage);
                }
                _ => {}
            }
            last = Some(key);
        }
        if let Some(next) = &page.next_cursor {
            let expected = WorkflowInventoryCursor {
                after: last,
                ..c.clone()
            };
            if next != &expected {
                return Err(ChangedBoundary);
            }
            if next.after == c.after {
                return Err(InvalidPage);
            }
        }
        Ok(())
    }
}

/// Current canonical execution allowance facts. Counts saturate at their respective caps.
/// Exhaustion blocks new attempts, not reconciliation of existing work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowExecutionAllowanceObservation {
    /// Run-local attempt cap.
    pub run_cap: u64,
    /// Run-local attempts consumed, capped at `run_cap`; absent when the read bound is reached.
    pub run_consumed: Option<u64>,
    /// Composition-root attempt cap.
    pub root_cap: u64,
    /// Root and descendant attempts consumed, capped at `root_cap`; absent when the read bound is reached.
    pub root_consumed: Option<u64>,
}

impl WorkflowExecutionAllowanceObservation {
    /// Whether current run/root facts prove execution allowance exhaustion.
    /// Unknown consumption remains unknown unless the other budget proves exhaustion.
    /// Invalid zero caps or counts above a cap fail closed as unknown.
    // Repository convention: Option already communicates a must-use result.
    #[allow(clippy::must_use_candidate)]
    pub fn exhausted(&self) -> Option<bool> {
        if self.run_cap == 0
            || self.root_cap == 0
            || self.run_consumed.is_some_and(|used| used > self.run_cap)
            || self.root_consumed.is_some_and(|used| used > self.root_cap)
        {
            return None;
        }
        if self.run_consumed == Some(self.run_cap) || self.root_consumed == Some(self.root_cap) {
            return Some(true);
        }
        self.run_consumed.zip(self.root_consumed).map(|_| false)
    }
}

/// Admission boundary whose acceptance could not be verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAdmissionPhase {
    /// Owner dispatch did not return an admission receipt.
    OwnerDispatch,
    /// Receipt persistence did not establish the durable handoff.
    ReceiptPersistence,
}

/// Secret-safe category, not a retry decision or a claim of no effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAdmissionCategory {
    /// Owner access was explicitly deferred.
    OwnerAccessDeferred,
    /// Owner returned an error; private details are withheld.
    OwnerError,
    /// Durable receipt persistence was unavailable.
    ReceiptPersistenceUnavailable,
}

/// Immutable normalized observation. Side effects and acceptance remain unverified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAdmissionObservation {
    /// Owning run identity.
    pub run_id: String,
    /// Exact prepared node identity.
    pub node_id: String,
    /// Exact prepared activation identity.
    pub activation_id: String,
    /// Exact prepared attempt number.
    pub attempt: u32,
    /// Validated dispatch digest for correlation, not authority.
    pub dispatch_identity: String,
    /// Boundary observed, without guessed ordering or event sequence.
    pub phase: WorkflowAdmissionPhase,
    /// Normalized category; never raw owner error text.
    pub category: WorkflowAdmissionCategory,
    /// First durable observation time in Unix milliseconds.
    pub observed_at_ms: u64,
}

/// Availability of one bounded canonical collection at a transactional snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCanonicalCollectionCoverage {
    /// All applicable rows were verified and the bounded inventory exhausted.
    Exhausted,
    /// Truncated, unsettled, unsupported or otherwise unverified inventory.
    Incomplete,
}

impl WorkflowCanonicalCollectionCoverage {
    /// Convert the producer's verified exhaustion predicate into its portable fact.
    #[must_use]
    pub const fn from_verified(exhausted: bool) -> Self {
        if exhausted {
            Self::Exhausted
        } else {
            Self::Incomplete
        }
    }
}

/// Version-one bounded terminal canonical observation, not full semantic history.
/// Missing coverage or unsupported versions must remain unknown to consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCanonicalEvalCoverage {
    /// Independently evolving compatibility boundary; currently one.
    pub version: u32,
    /// Exact canonical run identity.
    pub run_id: String,
    /// Topology shared by the graph and transactional execution snapshot.
    pub graph_revision: u64,
    /// Terminal state read with attempts, outputs and bindings in one read transaction.
    pub terminal_status: crate::RunStatus,
    /// Each collection was exhausted within the requested bound, not merely returned empty.
    pub graph_complete: WorkflowCanonicalCollectionCoverage,
    /// Exhausted canonical activation inventory.
    pub activations_complete: WorkflowCanonicalCollectionCoverage,
    /// Exhausted canonical attempt inventory, all attempts terminal.
    pub attempts_complete: WorkflowCanonicalCollectionCoverage,
    /// Exhausted checksum-verified canonical outputs.
    pub outputs_complete: WorkflowCanonicalCollectionCoverage,
    /// Exhausted canonical admission observations.
    pub admission_complete: WorkflowCanonicalCollectionCoverage,
    /// Exhausted verified execution-session provenance bindings.
    pub sessions_complete: WorkflowCanonicalCollectionCoverage,
    /// Direct child links exhausted and empty. Descendant settlement is not yet supported.
    pub no_child_runs: WorkflowCanonicalCollectionCoverage,
}

impl WorkflowCanonicalEvalCoverage {
    /// Whether this exact terminal snapshot closes only its declared canonical scope.
    #[must_use]
    pub fn complete_for(&self, run_id: &str, revision: u64, status: crate::RunStatus) -> bool {
        self.version == 1
            && self.run_id == run_id
            && self.graph_revision == revision
            && self.terminal_status == status
            && matches!(
                status,
                crate::RunStatus::Completed
                    | crate::RunStatus::Failed
                    | crate::RunStatus::Cancelled
            )
            && [
                self.graph_complete,
                self.activations_complete,
                self.attempts_complete,
                self.outputs_complete,
                self.admission_complete,
                self.sessions_complete,
                self.no_child_runs,
            ]
            .iter()
            .all(|coverage| *coverage == WorkflowCanonicalCollectionCoverage::Exhausted)
    }
}

/// Bounded aggregate workflow inspection snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunInspection {
    /// Scoped transactional terminal coverage; absence is unknown, never full-history closure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_eval_coverage: Option<WorkflowCanonicalEvalCoverage>,
    /// Normalized admission diagnostics; absent older senders do not prove absence.
    #[serde(default)]
    pub admission_observations: Vec<WorkflowAdmissionObservation>,
    /// Complete bounded observation at read time; None means unavailable/older sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_observations_complete: Option<bool>,
    /// Collections not supported by this representation; empty values are not evidence of absence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_collections: Vec<String>,
    /// Current allowance observation; absent on older senders means unknown.
    /// Does not imply runnable work or authorization to increase limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_allowance: Option<WorkflowExecutionAllowanceObservation>,
    /// Explicit lineage; absent on original runs and older senders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<WorkflowContinuationLineage>,
    /// Replacement blockers observed without acquiring control. Missing in older senders
    /// means unknown, not readiness or authorization to execute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_readiness: Option<ReplacementReadiness>,
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
    #[test]
    fn graph_history_preserves_canonical_mutation_identity_and_rejects_future_facts() {
        use super::*;
        let mutation = "héllo / @ graph";
        let payload = WorkflowGraphHistoryPayload::GraphPublished {
            mutation_id: mutation.into(),
            previous_revision: 1,
            revision: 2,
            previous_graph_sha256: "a".repeat(64),
            graph_sha256: "b".repeat(64),
            reconciliation_sha256: "c".repeat(64),
        };
        assert!(payload.valid());
        let page = WorkflowGraphHistoryPage {
            cursor: WorkflowGraphHistoryCursor {
                version: WORKFLOW_GRAPH_HISTORY_VERSION,
                run_id: "graph-run".into(),
                scope: WorkflowGraphHistoryScope::GraphRevisionV1,
                coverage: WorkflowSemanticHistoryCoverage::HistoricalGap,
                through_sequence: 1,
                after_sequence: 0,
            },
            events: vec![WorkflowGraphHistoryEvent {
                run_id: "graph-run".into(),
                sequence: 1,
                operation_id: "d".repeat(64),
                recorded_at_ms: 10,
                payload,
            }],
            next_cursor: None,
        };
        page.validate("graph-run", None).unwrap();
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkflowGraphHistoryPage>(json.clone()).unwrap(),
            page
        );
        let mut future = json.clone();
        future["events"][0]["payload"]["kind"] = serde_json::json!("future");
        assert!(serde_json::from_value::<WorkflowGraphHistoryPage>(future).is_err());
        let mut omitted = json;
        omitted["cursor"]
            .as_object_mut()
            .unwrap()
            .remove("coverage");
        assert!(serde_json::from_value::<WorkflowGraphHistoryPage>(omitted).is_err());
        let mut unavailable = page.clone();
        unavailable.cursor.coverage = WorkflowSemanticHistoryCoverage::Unavailable;
        assert!(unavailable.validate("graph-run", None).is_err());
        unavailable.events.clear();
        unavailable.validate("graph-run", None).unwrap();
        let mut changed = page;
        changed.cursor.version += 1;
        assert_eq!(
            changed.validate("graph-run", None),
            Err(WorkflowSemanticHistoryError::UnsupportedVersion)
        );
    }

    #[test]
    fn graph_history_pins_query_and_complete_sequence_boundaries() {
        use super::*;
        let query = WorkflowGraphHistoryQuery {
            version: WORKFLOW_GRAPH_HISTORY_VERSION,
            run_id: "graph-run".into(),
            scope: WorkflowGraphHistoryScope::GraphRevisionV1,
            cursor: None,
            limit: 1,
        };
        let mut page = WorkflowGraphHistoryPage {
            cursor: WorkflowGraphHistoryCursor {
                version: WORKFLOW_GRAPH_HISTORY_VERSION,
                run_id: query.run_id.clone(),
                scope: query.scope,
                coverage: WorkflowSemanticHistoryCoverage::CompleteFromAdmission,
                through_sequence: 1,
                after_sequence: 0,
            },
            events: vec![WorkflowGraphHistoryEvent {
                run_id: query.run_id.clone(),
                sequence: 1,
                operation_id: "a".repeat(64),
                recorded_at_ms: 1,
                payload: WorkflowGraphHistoryPayload::GraphInitialized {
                    revision: 1,
                    graph_sha256: "b".repeat(64),
                },
            }],
            next_cursor: None,
        };
        query.validate_page(&page).unwrap();
        page.cursor.through_sequence = 2;
        assert!(query.validate_page(&page).is_err());
        let mut next = page.cursor.clone();
        next.after_sequence = 1;
        page.next_cursor = Some(next);
        query.validate_page(&page).unwrap();
        let mut bad = query;
        bad.limit = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn lifecycle_history_roundtrip_pins_boundary_and_rejects_future_or_private_facts() {
        use super::*;
        let query = WorkflowLifecycleHistoryQuery {
            version: WORKFLOW_LIFECYCLE_HISTORY_VERSION,
            run_id: "run".into(),
            scope: WorkflowLifecycleHistoryScope::RunLifecycleV1,
            cursor: None,
            limit: 1,
        };
        let mut page = WorkflowLifecycleHistoryPage {
            cursor: WorkflowLifecycleHistoryCursor {
                version: query.version,
                run_id: query.run_id.clone(),
                scope: query.scope,
                coverage: WorkflowSemanticHistoryCoverage::CompleteFromAdmission,
                through_sequence: 1,
                after_sequence: 0,
            },
            events: vec![WorkflowLifecycleHistoryEvent {
                run_id: "run".into(),
                sequence: 1,
                operation_id: "a".repeat(64),
                recorded_at_ms: 9,
                payload: WorkflowLifecycleHistoryPayload::RunAdmitted {
                    status: RunStatus::Running,
                },
            }],
            next_cursor: None,
        };
        for payload in [
            WorkflowLifecycleHistoryPayload::RunAdmitted {
                status: RunStatus::Running,
            },
            WorkflowLifecycleHistoryPayload::RunStatusChanged {
                previous: RunStatus::Running,
                status: RunStatus::Completed,
            },
            WorkflowLifecycleHistoryPayload::RunStatusChanged {
                previous: RunStatus::Running,
                status: RunStatus::Failed,
            },
            WorkflowLifecycleHistoryPayload::RunStatusChanged {
                previous: RunStatus::Running,
                status: RunStatus::Cancelled,
            },
        ] {
            page.events[0].payload = payload;
            let value = serde_json::to_value(&page).unwrap();
            let decoded: WorkflowLifecycleHistoryPage =
                serde_json::from_value(value.clone()).unwrap();
            assert_eq!(decoded, page);
            assert_eq!(query.validate_page(&decoded), Ok(()));
            let mut private = value;
            private["events"][0]["payload"]["message"] = serde_json::json!("SECRET");
            assert!(serde_json::from_value::<WorkflowLifecycleHistoryPage>(private).is_err());
        }
        page.cursor.coverage = WorkflowSemanticHistoryCoverage::HistoricalGap;
        let mut next = page.cursor.clone();
        next.after_sequence = 1;
        next.through_sequence = 2;
        page.cursor.through_sequence = 2;
        page.next_cursor = Some(next);
        assert_eq!(query.validate_page(&page), Ok(()));
        page.next_cursor.as_mut().unwrap().coverage =
            WorkflowSemanticHistoryCoverage::CompleteFromAdmission;
        assert_eq!(
            query.validate_page(&page),
            Err(WorkflowSemanticHistoryError::ChangedBoundary)
        );
        let mut future = serde_json::to_value(&page).unwrap();
        future["events"][0]["payload"] = serde_json::json!({"kind":"future"});
        assert!(serde_json::from_value::<WorkflowLifecycleHistoryPage>(future).is_err());
        let mut future_query = query.clone();
        future_query.version = 99;
        assert_eq!(
            future_query.validate(),
            Err(WorkflowSemanticHistoryError::UnsupportedVersion)
        );
        for limit in [0, 101] {
            let mut invalid = query.clone();
            invalid.limit = limit;
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn inventory_current_pin_rejects_wrong_scope_future_generation_and_stalls() {
        use super::*;
        let query = WorkflowInventoryQuery {
            version: 1,
            run_id: "run".into(),
            source: WorkflowInventorySource::Outputs,
            cursor: None,
            limit: 1,
        };
        let cursor = WorkflowInventoryCursor {
            version: 1,
            run_id: "run".into(),
            source: query.source,
            installation_id: "install".into(),
            graph_revision: 1,
            source_generation: 0,
            after: None,
        };
        let mut page = WorkflowInventoryPage {
            cursor: cursor.clone(),
            coverage: WorkflowInventoryCoverage::CurrentInventoryComplete,
            entries: vec![WorkflowInventoryEntry::OutputUnavailable {
                output_id: "output".into(),
                reason: WorkflowInventoryUnavailableReason::MissingProof,
            }],
            next_cursor: Some(WorkflowInventoryCursor {
                after: Some(WorkflowInventoryKey::Output("output".into())),
                ..cursor.clone()
            }),
        };
        assert_eq!(query.validate_page(&page), Ok(()));
        let encoded = serde_json::to_value(&page).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkflowInventoryPage>(encoded.clone()).unwrap(),
            page
        );
        page.next_cursor.as_mut().unwrap().source_generation = 1;
        assert_eq!(
            query.validate_page(&page),
            Err(WorkflowSemanticHistoryError::ChangedBoundary)
        );
        page.next_cursor = Some(cursor.clone());
        assert_eq!(
            query.validate_page(&page),
            Err(WorkflowSemanticHistoryError::ChangedBoundary)
        );
        let wrong = WorkflowInventoryQuery {
            cursor: Some(WorkflowInventoryCursor {
                after: Some(WorkflowInventoryKey::Dispatch("a".repeat(64))),
                ..cursor
            }),
            ..query.clone()
        };
        assert!(wrong.validate().is_err());
        let mut future = encoded;
        future["coverage"] = serde_json::json!({"kind":"complete_from_admission"});
        assert!(serde_json::from_value::<WorkflowInventoryPage>(future).is_err());
        page.cursor.source = WorkflowInventorySource::ChildRuns;
        assert!(query.validate_page(&page).is_err());
        for limit in [0, 101] {
            assert!(
                WorkflowInventoryQuery {
                    limit,
                    ..query.clone()
                }
                .validate()
                .is_err()
            );
        }
    }

    fn semantic_page() -> super::WorkflowSemanticHistoryPage {
        use super::*;
        WorkflowSemanticHistoryPage {
            cursor: WorkflowSemanticHistoryCursor {
                version: WORKFLOW_SEMANTIC_HISTORY_VERSION,
                run_id: "run".into(),
                scope: WorkflowSemanticHistoryScope::AttemptsV1,
                coverage: WorkflowSemanticHistoryCoverage::CompleteFromAdmission,
                through_sequence: 1,
                after_sequence: 0,
            },
            events: vec![WorkflowSemanticHistoryEvent {
                run_id: "run".into(),
                sequence: 1,
                operation_id: "a".repeat(64),
                recorded_at_ms: 7,
                attempt: WorkflowHistoryAttemptCorrelation {
                    run_id: "run".into(),
                    node_id: "node".into(),
                    activation_id: "activation".into(),
                    attempt: 1,
                },
                dispatch_identity: "b".repeat(64),
                payload: WorkflowSemanticHistoryPayload::ReceiptCommitted {},
            }],
            next_cursor: None,
        }
    }

    #[test]
    fn semantic_history_contract_roundtrips_typed_obligations_and_rejects_private_payloads() {
        use super::*;
        for payload in [
            WorkflowSemanticHistoryPayload::AttemptPrepared {
                side_effect: DispatchSideEffect::ReadOnly,
            },
            WorkflowSemanticHistoryPayload::ReceiptCommitted {},
            WorkflowSemanticHistoryPayload::AttemptTerminal {
                outcome: WorkflowSemanticAttemptOutcome::Succeeded,
            },
            WorkflowSemanticHistoryPayload::AttemptTerminal {
                outcome: WorkflowSemanticAttemptOutcome::Failed,
            },
            WorkflowSemanticHistoryPayload::AttemptTerminal {
                outcome: WorkflowSemanticAttemptOutcome::Cancelled,
            },
        ] {
            let mut page = semantic_page();
            page.events[0].payload = payload;
            let value = serde_json::to_value(&page).unwrap();
            let decoded: WorkflowSemanticHistoryPage =
                serde_json::from_value(value.clone()).unwrap();
            assert_eq!(decoded, page);
            assert_eq!(decoded.validate("run", None), Ok(()));
            let mut private = value;
            private["events"][0]["payload"]["receipt"] = serde_json::json!({"secret":"TOKEN"});
            assert!(serde_json::from_value::<WorkflowSemanticHistoryPage>(private).is_err());
        }
        let mut future = serde_json::to_value(semantic_page()).unwrap();
        future["events"][0]["payload"] = serde_json::json!({"kind":"future_event"});
        assert!(serde_json::from_value::<WorkflowSemanticHistoryPage>(future).is_err());
        let mut page = semantic_page();
        page.cursor.version += 1;
        assert_eq!(
            page.validate("run", None),
            Err(WorkflowSemanticHistoryError::UnsupportedVersion)
        );
    }

    #[test]
    fn semantic_history_paging_pins_coverage_highwater_and_preserves_gaps() {
        use super::*;
        let mut first = semantic_page();
        first.cursor.coverage = WorkflowSemanticHistoryCoverage::HistoricalGap;
        first.cursor.through_sequence = 9;
        first.events[0].sequence = 5;
        let mut next = first.cursor.clone();
        next.after_sequence = 5;
        first.next_cursor = Some(next.clone());
        assert_eq!(first.validate("run", None), Ok(()));
        let mut tail = first.clone();
        tail.cursor = next.clone();
        tail.events.clear();
        tail.next_cursor = None;
        assert_eq!(tail.validate("run", Some(&next)), Ok(()));
        assert_eq!(
            tail.cursor.coverage,
            WorkflowSemanticHistoryCoverage::HistoricalGap
        );
        tail.cursor.coverage = WorkflowSemanticHistoryCoverage::CompleteFromAdmission;
        assert_eq!(
            tail.validate("run", Some(&next)),
            Err(WorkflowSemanticHistoryError::ChangedBoundary)
        );
        tail.cursor = next.clone();
        tail.cursor.through_sequence += 1;
        assert_eq!(
            tail.validate("run", Some(&next)),
            Err(WorkflowSemanticHistoryError::ChangedBoundary)
        );
        first.next_cursor.as_mut().unwrap().coverage =
            WorkflowSemanticHistoryCoverage::CompleteFromAdmission;
        assert_eq!(
            first.validate("run", None),
            Err(WorkflowSemanticHistoryError::ChangedBoundary)
        );
    }

    #[test]
    fn semantic_history_query_rejects_invalid_versions_bounds_and_cursor_binding() {
        use super::*;
        let page = semantic_page();
        let query = WorkflowSemanticHistoryQuery {
            version: WORKFLOW_SEMANTIC_HISTORY_VERSION,
            run_id: "run".into(),
            scope: WorkflowSemanticHistoryScope::AttemptsV1,
            cursor: None,
            limit: 1,
        };
        assert_eq!(query.validate_page(&page), Ok(()));
        for limit in [0, 101] {
            let mut invalid = query.clone();
            invalid.limit = limit;
            assert_eq!(
                invalid.validate(),
                Err(WorkflowSemanticHistoryError::InvalidPage)
            );
        }
        let mut invalid = query.clone();
        invalid.version += 1;
        assert_eq!(
            invalid.validate(),
            Err(WorkflowSemanticHistoryError::UnsupportedVersion)
        );
        invalid = query.clone();
        invalid.cursor = Some(page.cursor.clone());
        invalid.cursor.as_mut().unwrap().run_id = "foreign".into();
        assert_eq!(
            invalid.validate(),
            Err(WorkflowSemanticHistoryError::ChangedBoundary)
        );
        let mut oversized = page.clone();
        oversized.events.push(page.events[0].clone());
        assert_eq!(
            query.validate_page(&oversized),
            Err(WorkflowSemanticHistoryError::InvalidPage)
        );
    }

    #[test]
    fn semantic_history_rejects_false_exhaustion_and_invalid_bounded_pages() {
        use super::*;
        let original = semantic_page();
        for index in 0..6 {
            let mut page = original.clone();
            match index {
                0 => page.events.clear(),
                1 => page.events[0].attempt.run_id = "foreign".into(),
                2 => page.events[0].operation_id = "TOKEN".into(),
                3 => page.events[0].sequence = 0,
                4 => page.events = vec![page.events[0].clone(); 101],
                _ => page.events[0].attempt.attempt = 0,
            }
            assert_eq!(
                page.validate("run", None),
                Err(WorkflowSemanticHistoryError::InvalidPage)
            );
        }
        for coverage in [
            WorkflowSemanticHistoryCoverage::Unsupported,
            WorkflowSemanticHistoryCoverage::Unavailable,
        ] {
            let mut page = original.clone();
            page.cursor.coverage = coverage;
            assert_eq!(
                page.validate("run", None),
                Err(WorkflowSemanticHistoryError::InvalidPage)
            );
            page.events.clear();
            assert_eq!(page.validate("run", None), Ok(()));
        }
    }

    #[test]
    fn scoped_terminal_coverage_rejects_unknown_partial_and_foreign_snapshots() {
        use super::{
            RunStatus, WorkflowCanonicalCollectionCoverage, WorkflowCanonicalEvalCoverage,
        };
        use WorkflowCanonicalCollectionCoverage::{Exhausted, Incomplete};
        let coverage = WorkflowCanonicalEvalCoverage {
            version: 1,
            run_id: "run".into(),
            graph_revision: 3,
            terminal_status: RunStatus::Completed,
            graph_complete: Exhausted,
            activations_complete: Exhausted,
            attempts_complete: Exhausted,
            outputs_complete: Exhausted,
            admission_complete: Exhausted,
            sessions_complete: Exhausted,
            no_child_runs: Exhausted,
        };
        assert!(coverage.complete_for("run", 3, RunStatus::Completed));
        assert!(!coverage.complete_for("foreign", 3, RunStatus::Completed));
        assert!(!coverage.complete_for("run", 4, RunStatus::Completed));
        assert!(!coverage.complete_for("run", 3, RunStatus::Failed));
        for status in [
            RunStatus::Running,
            RunStatus::Paused,
            RunStatus::RepairRequired,
        ] {
            let mut unsettled = coverage.clone();
            unsettled.terminal_status = status;
            assert!(!unsettled.complete_for("run", 3, status));
        }
        assert!(
            serde_json::from_value::<WorkflowCanonicalCollectionCoverage>(serde_json::json!(
                "future"
            ))
            .is_err()
        );
        for index in 0..8 {
            let mut partial = coverage.clone();
            match index {
                0 => partial.version = 2,
                1 => partial.graph_complete = Incomplete,
                2 => partial.activations_complete = Incomplete,
                3 => partial.attempts_complete = Incomplete,
                4 => partial.outputs_complete = Incomplete,
                5 => partial.admission_complete = Incomplete,
                6 => partial.sessions_complete = Incomplete,
                _ => partial.no_child_runs = Incomplete,
            }
            assert!(!partial.complete_for("run", 3, RunStatus::Completed));
        }
    }

    #[test]
    fn allowance_exhaustion_preserves_unknown_and_invalid_facts() {
        use super::WorkflowExecutionAllowanceObservation;
        for (local, root, expected) in [
            (None, None, None),
            (Some(0), None, None),
            (None, Some(0), None),
            (Some(2), None, Some(true)),
            (None, Some(3), Some(true)),
            (Some(1), Some(2), Some(false)),
            (Some(2), Some(3), Some(true)),
            (Some(3), Some(3), None),
            (Some(2), Some(4), None),
        ] {
            let observation = WorkflowExecutionAllowanceObservation {
                run_cap: 2,
                run_consumed: local,
                root_cap: 3,
                root_consumed: root,
            };
            assert_eq!(observation.exhausted(), expected);
            let encoded = serde_json::to_value(&observation).unwrap();
            let decoded: WorkflowExecutionAllowanceObservation =
                serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded.exhausted(), expected);
        }
        assert_eq!(
            WorkflowExecutionAllowanceObservation {
                run_cap: 0,
                run_consumed: Some(0),
                root_cap: 3,
                root_consumed: Some(3)
            }
            .exhausted(),
            None
        );
    }

    #[test]
    fn replacement_readiness_preserves_states_and_rejects_future_variants() {
        use super::ReplacementReadiness;
        for state in [
            ReplacementReadiness::Absent,
            ReplacementReadiness::WaitingForOldRun,
            ReplacementReadiness::WaitingForEffects,
            ReplacementReadiness::DescendantProofRequired,
            ReplacementReadiness::ExecutionCompatibilityRequired,
            ReplacementReadiness::ReadyForAuthorization,
        ] {
            let value = serde_json::to_value(state).expect("encode");
            assert_eq!(
                serde_json::from_value::<ReplacementReadiness>(value).expect("decode"),
                state
            );
        }
        assert!(
            serde_json::from_value::<ReplacementReadiness>(serde_json::json!("future_ready"))
                .is_err()
        );
        assert_eq!(
            serde_json::from_value::<Option<ReplacementReadiness>>(serde_json::Value::Null)
                .expect("unknown"),
            None
        );
    }

    #[test]
    fn coordinator_recovery_observation_preserves_unknown_older_state() {
        let older = serde_json::json!({"target_artifact_id":"artifact", "daemon_instance_id":"daemon",
            "owned_by_this_daemon":true,"controllable_from_this_daemon":true});
        let mut status: super::WorkflowCoordinatorStatus = serde_json::from_value(older).unwrap();
        assert_eq!(status.recovery_only, None);
        assert_eq!(status.recovery_source_artifact_id, None);
        status.recovery_source_artifact_id = Some("original-artifact".into());
        status.recovery_only = Some(true);
        let decoded: super::WorkflowCoordinatorStatus =
            serde_json::from_value(serde_json::to_value(&status).unwrap()).unwrap();
        assert_eq!(decoded.recovery_only, Some(true));
    }
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
    fn persisted_concurrency_requires_explicit_policy_and_preserves_numeric_caps() {
        for cap in [None, Some(1), Some(8), Some(100)] {
            let limits = WorkflowRunLimits {
                concurrency_cap: cap,
                ..Default::default()
            };
            let wire = serde_json::to_value(&limits).unwrap();
            assert_eq!(wire["concurrency_cap"], serde_json::json!(cap));
            assert_eq!(
                serde_json::from_value::<WorkflowRunLimits>(wire.clone()).unwrap(),
                limits
            );
            let mut missing = wire;
            missing.as_object_mut().unwrap().remove("concurrency_cap");
            assert!(serde_json::from_value::<WorkflowRunLimits>(missing).is_err());
        }
        for invalid in [
            serde_json::json!(-1),
            serde_json::json!(4_294_967_296_u64),
            serde_json::json!({"unlimited":true}),
        ] {
            let mut wire = serde_json::to_value(WorkflowRunLimits::default()).unwrap();
            wire["concurrency_cap"] = invalid;
            assert!(serde_json::from_value::<WorkflowRunLimits>(wire).is_err());
        }
    }

    #[test]
    fn run_start_defaults_preserve_allowances() {
        let request: WorkflowRunStartRequest = serde_json::from_value(serde_json::json!({
            "definition_id":"definition", "definition_version":1,
            "workspace_snapshot":"workspace", "parent_session_id": "00000000-0000-0000-0000-000000000001"
        })).unwrap();
        assert_eq!(request.limits, WorkflowRunLimits::default());
        assert_eq!(request.limits.node_execution_cap, 1_000);
        assert_eq!(request.limits.concurrency_cap, None);
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

/// Store-level replacement readiness; never an authorization or execution grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementReadiness {
    /// No pending successor exists.
    Absent,
    /// The old run has not reached a terminal outcome.
    WaitingForOldRun,
    /// Operation-owner evidence has not settled all effects.
    WaitingForEffects,
    /// Child workflows require independently qualified quiescence.
    DescendantProofRequired,
    /// The successor's execution artifact is not the current coordinator artifact.
    ExecutionCompatibilityRequired,
    /// Leaf storage conditions permit an independently authorized handoff.
    ReadyForAuthorization,
}
