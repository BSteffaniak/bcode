//! Portable authored-workflow application contracts, independent of daemon transport.
//!
//! Established JSON names, defaults, and externally tagged outcomes are preserved. Document
//! compatibility is governed by its version; computation control is not durable resume state.
//! Unknown request fields and unknown outcome variants are rejected rather than guessed.

use crate::{WorkflowAuthoringConflict, WorkflowDraftSnapshot};
use serde::{Deserialize, Serialize};

/// Typed authoring operations supplied by a connected application adapter.
///
/// Implementations retain connection identity and authorization context; callers supply only
/// domain requests. Errors are adapter-owned to preserve transport failures without pretending
/// a disconnected request was rejected by the domain. Completion does not imply retry safety.
pub trait WorkflowAuthoringApplication: Sync {
    /// Domain or transport failure returned by this adapter.
    type Error;

    /// Apply semantic edits against an exact draft generation.
    ///
    /// # Errors
    /// Returns an error on unavailable state, denied authorization, invalid input, or transport failure.
    fn apply_workflow_draft_edits(
        &self,
        request: crate::ApplyWorkflowDraftEditsRequest,
    ) -> impl std::future::Future<Output = Result<crate::WorkflowDraftEditResult, Self::Error>> + Send;

    /// Replace a draft against an exact generation.
    ///
    /// # Errors
    /// Returns an error on unavailable state, denied authorization, invalid input, or transport failure.
    fn update_workflow_draft(
        &self,
        request: UpdateWorkflowDraftRequest,
    ) -> impl std::future::Future<Output = Result<crate::WorkflowDraftUpdateResult, Self::Error>> + Send;

    /// Activate a published revision without starting execution.
    ///
    /// # Errors
    /// Returns an error on unavailable state, denied authorization, invalid input, or transport failure.
    fn activate_workflow_revision(
        &self,
        request: ActivateWorkflowRevisionRequest,
    ) -> impl std::future::Future<
        Output = Result<crate::WorkflowAuthoringMutationResult, Self::Error>,
    > + Send;

    /// Compile and publish an exact draft generation, optionally activating it.
    ///
    /// Control identifies the cancellable compilation; cancellation is not rollback of a
    /// committed publication. A conflict is a typed result, not a transport failure.
    ///
    /// # Errors
    /// Returns an error on invalid control, compilation deadline/cancellation, denied
    /// authorization, unavailable state, unsupported definition, or transport failure.
    fn publish_workflow_draft(
        &self,
        request: PublishWorkflowDraftRequest,
    ) -> impl std::future::Future<Output = Result<crate::WorkflowPublicationResult, Self::Error>> + Send;

    /// Publish a draft and attempt admission of its exact published revision.
    ///
    /// A published result remains committed even when run admission fails. Compilation
    /// cancellation is not rollback, and a transport failure does not authorize retry.
    ///
    /// # Errors
    /// Returns an error on publication validation, authorization, compilation control,
    /// unavailable state, or transport failure. Post-publication admission failures are
    /// retained inside the published result.
    fn publish_and_start_workflow(
        &self,
        request: crate::PublishAndStartWorkflowRequest,
    ) -> impl std::future::Future<Output = Result<crate::WorkflowPublishAndStartResult, Self::Error>>
    + Send;

    /// Discard a draft against an exact generation.
    ///
    /// # Errors
    /// Returns an error on unavailable state, denied authorization, invalid input, or transport failure.
    fn discard_workflow_draft(
        &self,
        request: DiscardWorkflowDraftRequest,
    ) -> impl std::future::Future<
        Output = Result<crate::WorkflowAuthoringMutationResult, Self::Error>,
    > + Send;
}

/// Secret-safe failure of a workflow authoring application call.
///
/// Variant names are stable application categories; unknown variants are rejected.
/// Mutation failures do not promise rollback or authorize automatic retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAuthoringFailure {
    /// Request contract validation failed.
    #[error("workflow request does not satisfy its contract")]
    InvalidContract,
    /// The host does not support this definition.
    #[error("workflow definition is unsupported by this host")]
    UnsupportedDefinition,
    /// Required capability or storage initialization is unavailable.
    #[error("a required workflow capability is unavailable")]
    CapabilityUnavailable,
    /// Canonical operation authorization denied the call.
    #[error("workflow operation is not authorized")]
    Unauthorized,
    /// Authoring generation changed.
    #[error("workflow authoring state changed; refresh and retry")]
    Conflict,
    /// Another owner blocks safe upgrade.
    #[error(
        "workflow storage upgrade is blocked by another owner; retry startup after that owner releases the store"
    )]
    UpgradeBlocked,
    /// Explicit storage maintenance is required.
    #[error("workflow store requires explicit maintenance")]
    MaintenanceRequired,
    /// Durable state could not be accessed or verified.
    #[error("workflow state is unavailable")]
    StateUnavailable,
    /// Compilation exceeded the bounded deadline.
    #[error("workflow computation timed out")]
    TimedOut,
    /// Compilation was cancelled before publication.
    #[error("workflow computation was cancelled")]
    Cancelled,
    /// Computation identity or deadline is invalid.
    #[error("workflow computation control request is invalid")]
    InvalidControl,
    /// Unclassified failures disclose no host details.
    #[error("request failed")]
    Failed,
}

impl WorkflowAuthoringFailure {
    /// Stable transport code, independent of a transport implementation.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidContract => "workflow_contract_invalid",
            Self::UnsupportedDefinition => "workflow_definition_unsupported",
            Self::CapabilityUnavailable => "workflow_capability_unavailable",
            Self::Unauthorized => "workflow_operation_unauthorized",
            Self::Conflict => "workflow_authoring_conflict",
            Self::UpgradeBlocked => "workflow_store_upgrade_blocked",
            Self::MaintenanceRequired => "workflow_store_reset_required",
            Self::StateUnavailable => "workflow_unavailable",
            Self::TimedOut => "workflow_computation_timed_out",
            Self::Cancelled => "workflow_computation_cancelled",
            Self::InvalidControl => "workflow_computation_control_invalid",
            Self::Failed => "request_failed",
        }
    }
}

/// Default bounded deadline for authored-workflow validation and compilation.
pub const DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS: u64 = 30_000;
/// Maximum caller-selected authored-workflow computation deadline.
pub const MAX_WORKFLOW_COMPUTATION_TIMEOUT_MS: u64 = 120_000;

/// Explicit control for one bounded authored-workflow validation or compilation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowComputationControl {
    /// Stable caller identity used to target cancellation while the request is in flight.
    pub operation_id: String,
    /// Server-enforced computation deadline in milliseconds.
    pub timeout_ms: u64,
}

impl Default for WorkflowComputationControl {
    fn default() -> Self {
        Self {
            operation_id: String::new(),
            timeout_ms: DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS,
        }
    }
}

/// One source-aware create-or-single-replace request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyWorkflowSourceRequest {
    /// Declared source representation.
    pub source_format: crate::WorkflowSourceFormat,
    /// Source text to lower and apply.
    pub source: String,
    /// Stable mutable draft identity.
    pub draft_id: String,
}

/// One typed authored-workflow creation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAuthoredWorkflowRequest {
    /// Authored workflow document.
    pub document: crate::WorkflowAuthoringDocument,
    /// Stable mutable draft identity.
    pub draft_id: String,
}

/// One generation-checked draft replacement request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateWorkflowDraftRequest {
    /// Owning workflow identity.
    pub workflow_id: String,
    /// Stable mutable draft identity.
    pub draft_id: String,
    /// Exact draft generation required for mutation.
    pub expected_generation: u64,
    /// Authored workflow document.
    pub document: crate::WorkflowAuthoringDocument,
    /// Producer provenance evaluated by authorization.
    pub producer: crate::WorkflowProducerProvenance,
}

/// One exact draft publication request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishWorkflowDraftRequest {
    /// Owning workflow identity.
    pub workflow_id: String,
    /// Stable mutable draft identity.
    pub draft_id: String,
    /// Exact draft generation required for mutation.
    pub expected_generation: u64,
    /// Optional publication configuration.
    pub configuration: Option<serde_json::Value>,
    /// Whether publication also changes the active revision.
    pub activate: bool,
    /// Expected active revision for optimistic activation.
    pub expected_active_revision: Option<u64>,
    /// Cancellation identity and computation deadline.
    #[serde(default)]
    pub control: WorkflowComputationControl,
}

/// Typed result of an optimistic draft replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDraftUpdateResult {
    /// Committed replacement snapshot.
    Updated(Box<WorkflowDraftSnapshot>),
    /// Optimistic conflict without mutation.
    Conflict(WorkflowAuthoringConflict),
}

/// Typed atomic publication outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPublicationResult {
    /// Successfully committed publication.
    Published {
        /// Immutable published revision.
        revision: Box<WorkflowRevisionSnapshot>,
        /// Active revision after publication.
        active_revision: Option<u64>,
    },
    /// Optimistic conflict without mutation.
    Conflict(WorkflowAuthoringConflict),
}

/// Portable immutable published-revision snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRevisionSnapshot {
    /// Immutable workflow revision identity.
    pub identity: crate::WorkflowRevisionIdentity,
    /// Checksum of authored source.
    pub source_checksum_sha256: String,
    /// Checksum of executable source.
    pub executable_source_checksum_sha256: String,
    /// Compiled definition identity.
    pub definition_identity: crate::WorkflowDefinitionIdentity,
    /// Authored workflow document.
    pub document: crate::WorkflowAuthoringDocument,
    /// Producer provenance evaluated by authorization.
    pub producer: crate::WorkflowProducerProvenance,
    /// Publication time in milliseconds since the Unix epoch.
    pub published_at_ms: u64,
}

/// Portable logical authored-workflow snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredWorkflowSnapshot {
    pub workflow_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_revision: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Immutable publication facts plus current derived requirement availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRevisionRequirementInspection {
    pub revision: Box<WorkflowRevisionSnapshot>,
    pub current_availability: crate::WorkflowRequirementAvailabilityReport,
}

/// One portable bounded authored lifecycle event with normalized, content-minimized facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringEventSnapshot {
    pub event_seq: u64,
    pub workflow_id: String,
    pub event_type: String,
    pub revision: Option<u64>,
    pub definition_id: Option<String>,
    pub definition_version: Option<u32>,
    pub activated: Option<bool>,
    pub created_at_ms: u64,
}

/// One portable authored-state consistency issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "issue", rename_all = "snake_case")]
pub enum WorkflowAuthoringIssueSnapshot {
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

/// Content-minimized mutable draft summary for public diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDraftInspectionSummary {
    pub identity: crate::WorkflowDraftIdentity,
    pub base_revision: Option<u64>,
    pub generation: u64,
    pub checksum_sha256: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Content-minimized immutable revision summary for public diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRevisionInspectionSummary {
    pub identity: crate::WorkflowRevisionIdentity,
    pub source_checksum_sha256: String,
    pub executable_source_checksum_sha256: String,
    pub definition_identity: crate::WorkflowDefinitionIdentity,
    pub published_at_ms: u64,
}

/// Content-minimized preset summary for public diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPresetInspectionSummary {
    pub workflow_id: String,
    pub preset_id: String,
    pub revision: u64,
    pub generation: u64,
    pub has_run_limit_override: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Bounded aggregate authored-workflow inspection from canonical indexed rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredWorkflowInspection {
    pub workflow: AuthoredWorkflowSnapshot,
    pub drafts: Vec<WorkflowDraftInspectionSummary>,
    pub revisions: Vec<WorkflowRevisionInspectionSummary>,
    pub presets: Vec<WorkflowPresetInspectionSummary>,
    pub events: Vec<WorkflowAuthoringEventSnapshot>,
    pub issues: Vec<WorkflowAuthoringIssueSnapshot>,
}

/// Portable reusable revision-bound preset snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPresetSnapshot {
    pub workflow_id: String,
    pub preset_id: String,
    pub revision: u64,
    pub name: String,
    pub generation: u64,
    pub configuration: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_limits: Option<crate::WorkflowRunLimitPolicy>,
    pub producer: crate::WorkflowProducerProvenance,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Compare-and-set one exact immutable revision as active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateWorkflowRevisionRequest {
    pub workflow_id: String,
    pub revision: u64,
    pub expected_active_revision: Option<u64>,
}

/// Archive or unarchive one logical authored workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetAuthoredWorkflowArchivedRequest {
    pub workflow_id: String,
    pub archived: bool,
}

/// Discard one exact mutable draft generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscardWorkflowDraftRequest {
    pub workflow_id: String,
    pub draft_id: String,
    pub expected_generation: u64,
}

/// Exact source used to fork a new mutable generation-1 draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDraftForkSource {
    Draft { draft_id: String },
    Revision { revision: u64 },
}

/// Fork one exact draft or immutable revision into a new mutable draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkWorkflowDraftRequest {
    pub workflow_id: String,
    pub source: WorkflowDraftForkSource,
    pub draft_id: String,
    pub producer: crate::WorkflowProducerProvenance,
}

/// Typed optimistic mutation outcome without an entity payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAuthoringMutationResult {
    Applied,
    Conflict(WorkflowAuthoringConflict),
}

/// One preset create/update payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPresetMutation {
    pub workflow_id: String,
    pub preset_id: String,
    pub revision: u64,
    pub name: String,
    pub configuration: serde_json::Value,
    pub run_limits: Option<crate::WorkflowRunLimitPolicy>,
    pub producer: crate::WorkflowProducerProvenance,
}

/// Create one revision-bound preset at generation 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkflowPresetRequest {
    pub preset: WorkflowPresetMutation,
}

/// Replace one exact preset generation without changing its revision binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateWorkflowPresetRequest {
    pub expected_generation: u64,
    pub preset: WorkflowPresetMutation,
}

/// Delete one exact preset generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteWorkflowPresetRequest {
    pub workflow_id: String,
    pub preset_id: String,
    pub expected_generation: u64,
}

/// Typed result of an optimistic preset replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPresetUpdateResult {
    Updated(WorkflowPresetSnapshot),
    Conflict(WorkflowAuthoringConflict),
}

/// Export one exact immutable authored revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportWorkflowRevisionRequest {
    pub workflow_id: String,
    pub revision: u64,
}

/// Preview one portable import without mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewWorkflowImportRequest {
    pub bundle: crate::WorkflowExportBundle,
    pub target_workflow_id: String,
    pub control: WorkflowComputationControl,
}

/// Explicit collision policy for importing a bundle into authored state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowImportCollisionPolicy {
    /// Require the target logical workflow identity to be absent.
    RequireNewWorkflow,
    /// Require the target logical workflow to exist and the requested draft identity to be absent.
    RequireExistingWorkflowNewDraft,
    /// Require the target logical workflow to exist and its next revision to match exactly.
    RequireExistingWorkflowNextRevision,
}

/// Import one portable bundle as a new logical workflow and generation-1 draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportWorkflowRequest {
    pub bundle: crate::WorkflowExportBundle,
    pub target_workflow_id: String,
    pub draft_id: String,
    /// Must be [`WorkflowImportCollisionPolicy::RequireNewWorkflow`].
    pub collision_policy: WorkflowImportCollisionPolicy,
    pub control: WorkflowComputationControl,
}

/// Import one portable bundle as a generation-1 draft in an existing logical workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportWorkflowDraftRequest {
    pub bundle: crate::WorkflowExportBundle,
    pub workflow_id: String,
    pub draft_id: String,
    /// Must be [`WorkflowImportCollisionPolicy::RequireExistingWorkflowNewDraft`].
    pub collision_policy: WorkflowImportCollisionPolicy,
    pub control: WorkflowComputationControl,
}

/// Import one portable bundle directly as the exact next immutable revision of an existing workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportWorkflowRevisionRequest {
    pub bundle: crate::WorkflowExportBundle,
    pub workflow_id: String,
    pub revision: u64,
    pub activate: bool,
    pub expected_active_revision: Option<u64>,
    /// Must be [`WorkflowImportCollisionPolicy::RequireExistingWorkflowNextRevision`].
    pub collision_policy: WorkflowImportCollisionPolicy,
    pub control: WorkflowComputationControl,
}

/// Typed exact-revision import outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRevisionImportResult {
    Imported {
        revision: Box<WorkflowRevisionSnapshot>,
        active_revision: Option<u64>,
    },
    Conflict(WorkflowAuthoringConflict),
}

/// Typed existing-workflow import outcome with collision-safe draft identity handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDraftImportResult {
    Imported {
        workflow: AuthoredWorkflowSnapshot,
        draft: Box<WorkflowDraftSnapshot>,
    },
    DraftAlreadyExists {
        workflow_id: String,
        draft_id: String,
    },
}

/// Portable bounded keyset page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringPage<T, C> {
    /// Items in stable query order.
    pub items: Vec<T>,
    /// Cursor for the next page, absent when this page exhausted the query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<C>,
}

/// Atomic package apply request through the daemon-owned workflow store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyWorkflowPackageRequest {
    pub request: crate::WorkflowPackageApplyRequest,
    pub applied_at_ms: u64,
}

/// Atomic package publication request through the daemon-owned workflow store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishWorkflowPackageRequest {
    pub request: crate::WorkflowPackagePublishRequest,
    pub published_at_ms: u64,
}

/// One bounded portable package validation/planning request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageComputationRequest {
    /// One bounded portable transitive package closure.
    pub closure: crate::WorkflowPackageClosure,
    #[serde(default)]
    pub control: WorkflowComputationControl,
}

/// One side-effect-free preview request for an already planned package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackagePreviewRequest {
    pub plan: crate::WorkflowPackagePlan,
    /// Exact dependency-before-importer closure plans used to make immutable child definitions
    /// available while previewing the entry package.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependency_plans: Vec<crate::WorkflowPackagePlan>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub configurations: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub control: WorkflowComputationControl,
}

/// Portable successful package planning response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageValidationResult {
    pub plan: crate::WorkflowPackageClosurePlan,
}

/// One bounded raw-source validation/lowering request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourceComputationRequest {
    pub source_format: crate::WorkflowSourceFormat,
    pub source: String,
    #[serde(default)]
    pub control: WorkflowComputationControl,
}

/// One bounded raw-source compilation preview request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourcePreviewRequest {
    pub source_format: crate::WorkflowSourceFormat,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<serde_json::Value>,
    #[serde(default)]
    pub control: WorkflowComputationControl,
}

/// Portable source validation/lowering response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourceValidationResult {
    pub source_format: crate::WorkflowSourceFormat,
    pub lowering: crate::WorkflowSourceLoweringResult,
}

/// Portable source lowering plus canonical compilation preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourcePreviewResult {
    pub source_format: crate::WorkflowSourceFormat,
    pub lowering: crate::WorkflowSourceLoweringResult,
    pub preview: crate::WorkflowCompilationPreview,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authoring_page_retains_cursor_and_exhaustion_encoding() {
        let page = WorkflowAuthoringPage {
            items: vec!["item"],
            next_cursor: Some("next"),
        };
        assert_eq!(
            serde_json::to_value(page).unwrap(),
            serde_json::json!({"items":["item"],"next_cursor":"next"})
        );
        let exhausted: WorkflowAuthoringPage<String, String> =
            serde_json::from_value(serde_json::json!({"items":[]})).unwrap();
        assert!(exhausted.next_cursor.is_none());
        assert_eq!(
            serde_json::to_value(exhausted).unwrap(),
            serde_json::json!({"items":[]})
        );
    }

    #[test]
    fn lifecycle_and_inspection_contract_wire_shapes() {
        let value = serde_json::json!({"workflow":{"workflow_id":"wf","title":"Workflow",
            "archived":false,"created_at_ms":1,"updated_at_ms":2},
            "drafts":[],"revisions":[],"presets":[],"events":[],
            "issues":[{"issue":"stale_draft_base","draft_id":"draft","base_revision":3}]});
        let inspection: AuthoredWorkflowInspection = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(inspection).unwrap(), value);
        assert_eq!(
            serde_json::to_value(WorkflowAuthoringMutationResult::Applied).unwrap(),
            "applied"
        );
        let mut request =
            serde_json::json!({"workflow_id":"wf","preset_id":"preset","expected_generation":2});
        assert!(serde_json::from_value::<DeleteWorkflowPresetRequest>(request.clone()).is_ok());
        request["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<DeleteWorkflowPresetRequest>(request).is_err());
    }

    #[test]
    fn publication_contract_defaults_and_conflict_wire_shape() {
        let value = serde_json::json!({"workflow_id":"workflow", "draft_id":"draft",
            "expected_generation":2, "configuration":null, "activate":false,
            "expected_active_revision":null});
        let request: PublishWorkflowDraftRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.control, WorkflowComputationControl::default());
        let mut future = value;
        future["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PublishWorkflowDraftRequest>(future).is_err());
        let conflict = serde_json::json!({"conflict":{"entity_id":"draft",
            "expected_generation":2,"current_generation":3}});
        let publication: WorkflowPublicationResult =
            serde_json::from_value(conflict.clone()).unwrap();
        let update: WorkflowDraftUpdateResult = serde_json::from_value(conflict.clone()).unwrap();
        assert_eq!(serde_json::to_value(publication).unwrap(), conflict);
        assert_eq!(serde_json::to_value(update).unwrap(), conflict);
    }
}
