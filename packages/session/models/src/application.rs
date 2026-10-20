//! Typed session application operations shared by Rust consumers and transport adapters.
//!
//! Implementations retain verified caller authority. These contracts do not grant authority,
//! select storage, repair history, or define a host lifecycle. Failures do not imply rollback;
//! dropping an operation future does not establish that an admitted mutation was cancelled.

use crate::{SessionId, SessionInspectionPage, SessionInspectionQuery, SessionSummary};
use serde::{Deserialize, Serialize};
use std::{future::Future, path::PathBuf};

/// Version of bounded ephemeral runtime-owner pages.
pub const RUNTIME_WORK_PAGE_VERSION: u32 = 1;

/// Bounded owner inventory query; cursor is exclusive, not a durable replay token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeWorkPageRequest {
    /// Contract version.
    pub version: u32,
    /// Canonical session identity.
    pub session_id: SessionId,
    /// Exclusive last owner identity from an earlier ephemeral page.
    pub after_work_id: Option<crate::WorkId>,
    /// Requested item budget, at most 200.
    pub limit: usize,
    /// Encoded item-array byte budget, from 2 through 262144.
    pub max_bytes: usize,
}

/// One bounded owner range; exhaustion is not proof of current idleness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeWorkPage {
    /// Contract version.
    pub version: u32,
    /// Canonical session identity.
    pub session_id: SessionId,
    /// Normalized active owner snapshots.
    pub items: Vec<crate::RuntimeWorkSnapshot>,
    /// Exclusive continuation cursor when the range has more work.
    pub next_after: Option<crate::WorkId>,
    /// Whether this ephemeral range was exhausted; not an atomic snapshot guarantee.
    pub complete: bool,
}

/// Caller-bound access to canonical runtime work; observations do not confer execution authority.
pub trait RuntimeWorkApplication: Sync {
    /// Read one bounded ephemeral owner range without replaying durable history.
    /// # Errors
    /// Rejects invalid versions/budgets, oversized items, or incompatible caller authority.
    fn runtime_work_page(
        &self,
        request: RuntimeWorkPageRequest,
    ) -> impl Future<Output = Result<RuntimeWorkPage, SessionApplicationFailure>> + Send;

    /// Read active work registered by the runtime owner, not stale durable Running rows.
    ///
    /// Compatibility inventory materializes all local work and does not provide a bounded
    /// pagination guarantee. Do not use this list as a bounded interactive observation.
    /// # Errors
    /// Rejects unavailable sessions or incompatible caller authority.
    fn list_runtime_work(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<Vec<crate::RuntimeWorkSnapshot>, SessionApplicationFailure>> + Send;

    /// Read bounded durable lifecycle events in canonical sequence order.
    /// # Errors
    /// Rejects unavailable canonical history. Limits are clamped to the domain read budget.
    fn runtime_work_history(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<crate::SessionEvent>, SessionApplicationFailure>> + Send;

    /// Request cancellation of exact registered work through its owner.
    ///
    /// An acknowledgement is not proof of terminal settlement or rollback.
    /// # Errors
    /// Rejects unavailable sessions or incompatible caller authority.
    fn cancel_runtime_work(
        &self,
        session_id: SessionId,
        work_id: crate::WorkId,
    ) -> impl Future<Output = Result<bool, SessionApplicationFailure>> + Send;
}

/// Portable text format for a canonical presentation-only note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionPresentationNoteFormat {
    /// Unformatted text.
    PlainText,
    /// Markdown content.
    Markdown,
    /// JSON content retained as presentation text, not an execution request.
    Json,
}

/// Bounded presentation-only append; acceptance does not submit a model turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPresentationNoteRequest {
    /// Canonical target session.
    pub session_id: SessionId,
    /// Presentation producer identity.
    pub source_id: String,
    /// Producer-owned note identity; does not imply idempotency.
    pub note_id: String,
    /// Bounded UTF-8 presentation content.
    pub text: String,
    /// Content presentation format.
    pub format: SessionPresentationNoteFormat,
}

/// Request to create a canonical session in the application's selected state location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// Optional user-facing session name.
    pub name: Option<String>,
    /// Absolute canonical session working directory.
    pub working_directory: PathBuf,
}

/// Result of changing an idle session's canonical working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorkingDirectoryChange {
    /// Canonical summary after the operation.
    pub session: SessionSummary,
    /// Whether the operation appended a working-directory change event.
    pub changed: bool,
}

/// Secret-safe application failure; private storage and transport errors are not exposed.
///
/// Codes are extensible: consumers must preserve unknown codes rather than assuming success.
/// An error does not promise rollback or make automatic mutation retries safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct SessionApplicationFailure {
    /// Stable operation failure code.
    pub code: String,
    /// Normalized public message, never a private error's display representation.
    pub message: String,
}

/// Portable result of an explicit milestone-generation operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneGenerationOutcome {
    /// Whether the plugin-owned generation operation reported success.
    pub success: bool,
    /// Normalized plugin-owned result text.
    pub text: String,
}

/// Terminal result of explicit context compaction; a no-op remains distinct from execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionCompactionResult {
    /// Canonical context was compacted.
    Compacted(String),
    /// The owning operation determined that no compaction was needed.
    Noop(String),
}

/// Session operations bound to a verified caller in an existing application.
///
/// The owning application supplies access; callers do not construct private server state or
/// provide a transport identity on each request. Implementations retain namespace fencing,
/// canonical event publication, bounded inspection, and derived catalog maintenance.
/// Full aggregate compatibility snapshot of the session discovery cache.
///
/// This is not a bounded catalog page. Loading and degraded source coverage remain
/// explicit; discovery does not confer canonical storage authority or repair history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCatalogSnapshot {
    /// All sessions in the aggregate compatibility snapshot.
    pub sessions: Vec<crate::SessionSummary>,
    /// Current aggregate discovery status.
    pub catalog_status: crate::SessionCatalogStatus,
    /// Per-source discovery coverage.
    pub catalog_sources: Vec<crate::SessionCatalogSourceStatus>,
    /// Current cache revision, not a durable replay cursor.
    pub catalog_revision: u64,
}

/// An admitted compaction whose terminal outcome is observed separately.
///
/// Dropping this operation or its completion future does not cancel admitted work.
/// Cancellation remains an explicit application operation.
pub trait SessionCompactionOperation: Send {
    /// Await the authoritative terminal result, consuming this observation handle.
    /// # Errors
    /// Returns normalized execution, cancellation, provider, or observation failures.
    fn complete(
        self,
    ) -> impl Future<Output = Result<SessionCompactionResult, SessionApplicationFailure>> + Send;
}

/// Session operations bound to a verified caller in an existing application.
///
/// The owning application supplies access; implementations retain namespace fencing,
/// canonical event publication and bounded inspection.
pub trait SessionApplication: Sync {
    /// Completion observation returned only after canonical compaction admission succeeds.
    type CompactionOperation: SessionCompactionOperation;

    /// Admit explicit compaction without waiting for its terminal outcome.
    /// # Errors
    /// Rejects unavailable authority, incompatible callers, or failed queue admission.
    fn begin_session_compaction(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<Self::CompactionOperation, SessionApplicationFailure>> + Send;

    /// Read the full aggregate compatibility snapshot for an explicit discovery directory.
    /// Waits while the snapshot is empty and loading, matching legacy list semantics.
    /// Use `session_catalog_page` for bounded query/cursor access instead.
    /// # Errors
    /// Rejects unavailable applications, invalid directories or closed discovery subscriptions.
    fn session_catalog_snapshot(
        &self,
        working_directory: PathBuf,
    ) -> impl Future<Output = Result<SessionCatalogSnapshot, SessionApplicationFailure>> + Send;
    /// Explicitly refresh selected sources and return the full aggregate compatibility snapshot.
    /// The result may still be loading; this is not repair or terminal discovery completion.
    /// # Errors
    /// Rejects unavailable applications or invalid discovery directories.
    fn refresh_session_catalog(
        &self,
        working_directory: PathBuf,
        sources: Option<Vec<String>>,
    ) -> impl Future<Output = Result<SessionCatalogSnapshot, SessionApplicationFailure>> + Send;
    /// Export complete canonical history explicitly, outside normal interactive read paths.
    ///
    /// This opt-in diagnostic operation is not bounded by a page and must not be used for
    /// attach, rendering, context construction or ordinary history browsing.
    /// # Errors
    /// Rejects incompatible active callers or unavailable canonical history.
    fn export_session_history(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<Vec<crate::SessionEvent>, SessionApplicationFailure>> + Send;

    /// Create and admit a canonical child using the complete immutable composed request.
    ///
    /// Partial failures preserve all verified receipts, never imply rollback, and are not
    /// collapsed into an ordinary error. Supervision uses normal session operations.
    fn spawn_session(
        &self,
        request: crate::SessionSpawnRequest,
    ) -> impl Future<Output = crate::SessionSpawnResult> + Send;

    /// Reprice an explicit canonical usage range against a validated typed catalog snapshot.
    ///
    /// This is named explicit maintenance, not a side effect of normal usage reads.
    /// # Errors
    /// Rejects invalid ranges/catalogs, oversized snapshots or unavailable canonical ownership.
    fn reprice_session(
        &self,
        session_id: SessionId,
        range: crate::SessionCostRange,
        catalog: bcode_model_catalog_models::CatalogDocument,
    ) -> impl Future<Output = Result<crate::SessionRepriceReport, SessionApplicationFailure>> + Send;

    /// Inspect current advisory categorization without changing canonical session history.
    /// # Errors
    /// Rejects unavailable sessions or incompatible caller authority.
    fn session_categorization(
        &self,
        session_id: SessionId,
    ) -> impl Future<
        Output = Result<Option<crate::SessionCategorization>, SessionApplicationFailure>,
    > + Send;

    /// Read a bounded cross-session usage catalog page from current native discovery.
    /// # Errors
    /// Rejects incomplete discovery or unavailable catalog projections.
    fn usage_catalog_page(
        &self,
        after: Option<SessionId>,
    ) -> impl Future<Output = Result<Vec<SessionId>, SessionApplicationFailure>> + Send;

    /// Collect an explicit bounded usage page into the disposable reporting owner.
    /// # Errors
    /// Rejects invalid queries, projection changes, or collection conflicts.
    fn collect_usage(
        &self,
        session_id: SessionId,
        query: crate::SessionUsageQuery,
    ) -> impl Future<Output = Result<crate::SessionUsagePage, SessionApplicationFailure>> + Send;

    /// Prepare bounded request-only context using the canonical generation owner.
    ///
    /// Captured source context is not implicitly made durable destination history.
    /// # Errors
    /// Rejects invalid contracts, changed generations, opaque context or capture budgets.
    fn prepare_context_generation(
        &self,
        request: crate::PrepareContextGeneration,
    ) -> impl Future<Output = Result<crate::PreparedContextGeneration, SessionApplicationFailure>> + Send;

    /// Read an explicitly requested bounded sensitive diagnostic trace range.
    ///
    /// This operation neither enables capture nor repairs missing history. The owning host
    /// supplies verified diagnostic access; request payloads do not confer authority.
    /// # Errors
    /// Rejects unsupported versions, invalid ranges, missing references or damaged storage.
    fn session_trace_blob_range(
        &self,
        request: crate::ReadSessionTraceBlobRequest,
    ) -> impl Future<Output = Result<crate::SessionTraceBlobRange, SessionApplicationFailure>> + Send;

    /// Read one bounded portable projection-history page.
    /// # Errors
    /// Rejects invalid windows, incompatible active callers, or unavailable projections.
    fn session_projection_history_page(
        &self,
        session_id: SessionId,
        request: crate::ProjectionWindowRequest,
    ) -> impl Future<Output = Result<crate::ProjectionHistoryPage, SessionApplicationFailure>> + Send;

    /// Admit an explicit conversational submission using a caller-retained retry identity.
    ///
    /// Strict steering (`require_active_turn`) fails rather than silently queueing a new turn.
    /// # Errors
    /// Rejects conflicting retries, invalid metadata, unavailable steering or scheduling.
    fn submit_prompt(
        &self,
        session_id: SessionId,
        submission_id: crate::SubmissionId,
        text: String,
        placement: crate::SubmissionPlacement,
        require_active_turn: bool,
        admission: crate::TurnAdmissionMetadata,
    ) -> impl Future<Output = Result<crate::SubmissionReceipt, SessionApplicationFailure>> + Send;

    /// Read a bounded mutable submission queue; rescan for arrivals behind the cursor.
    /// # Errors
    /// Rejects limits outside 1..=128 or unavailable canonical projections.
    fn pending_submissions(
        &self,
        session_id: SessionId,
        after: Option<u64>,
        limit: u32,
    ) -> impl Future<Output = Result<crate::PendingSubmissionsPage, SessionApplicationFailure>> + Send;

    /// Read one retained canonical submission without replaying history.
    /// # Errors
    /// Rejects unavailable projections or incompatible caller authority.
    fn get_submission(
        &self,
        session_id: SessionId,
        submission_id: crate::SubmissionId,
    ) -> impl Future<Output = Result<Option<crate::SubmissionRecord>, SessionApplicationFailure>> + Send;

    /// Generate a milestone through the existing plugin owner and registered cancellable work.
    /// # Errors
    /// Rejects unavailable policy, ownership, model selection, or plugin generation.
    fn generate_session_milestone(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<SessionMilestoneGenerationOutcome, SessionApplicationFailure>> + Send;

    /// Submit explicit follow-up/steering placement through canonical queued scheduling.
    ///
    /// Placement acceptance is not a durable turn receipt; use `submit_turn` for admission.
    /// # Errors
    /// Rejects incompatible callers, invalid execution options, or failed scheduling.
    fn send_message(
        &self,
        session_id: SessionId,
        text: String,
        placement: crate::PromptPlacement,
        execution: crate::TurnExecutionOptions,
    ) -> impl Future<Output = Result<crate::MessageAcceptance, SessionApplicationFailure>> + Send;

    /// Create an ordinary canonical child without deriving context or controlling its parent.
    /// # Errors
    /// Rejects invalid directories, conflicting retries, or unavailable parent authority.
    fn create_child_session(
        &self,
        request: crate::SessionChildCreateRequest,
    ) -> impl Future<Output = Result<SessionSummary, SessionApplicationFailure>> + Send;

    /// Append a bounded canonical presentation-only note without submitting a turn.
    ///
    /// This acknowledgement does not promise idempotency or rollback.
    /// # Errors
    /// Rejects invalid fields, unavailable storage, or incompatible caller authority.
    fn append_presentation_note(
        &self,
        request: SessionPresentationNoteRequest,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Compact context explicitly through canonical scheduling and await its terminal result.
    ///
    /// Dropping this future does not cancel admitted provider work.
    /// # Errors
    /// Rejects incompatible callers, admission failures, or failed provider execution.
    fn compact_session(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<SessionCompactionResult, SessionApplicationFailure>> + Send;

    /// Read bounded direct-child relationships without acquiring execution authority.
    /// # Errors
    /// Rejects invalid queries or unavailable relationship projections.
    fn query_child_sessions(
        &self,
        query: crate::SessionChildQuery,
    ) -> impl Future<Output = Result<crate::SessionChildPage, SessionApplicationFailure>> + Send;

    /// Read canonical milestones through a bounded cursor query.
    /// # Errors
    /// Rejects invalid queries or unavailable canonical projections.
    fn session_milestones(
        &self,
        session_id: SessionId,
        query: crate::SessionMilestoneQuery,
    ) -> impl Future<Output = Result<crate::SessionMilestonePage, SessionApplicationFailure>> + Send;

    /// Read a confined artifact range validated against canonical references.
    /// # Errors
    /// Rejects invalid ranges, missing references, or unsafe/unavailable storage.
    fn session_artifact_range(
        &self,
        session_id: SessionId,
        artifact_id: String,
        reference_key: String,
        offset: u64,
        length: u32,
    ) -> impl Future<Output = Result<crate::SessionArtifactRange, SessionApplicationFailure>> + Send;

    /// Access the canonical confined working document using its explicit read/write request.
    /// # Errors
    /// Rejects ownership conflicts, unsupported versions, invalid sizes or confined paths.
    fn session_working_document(
        &self,
        request: crate::SessionWorkingDocumentRequest,
    ) -> impl Future<
        Output = Result<Option<crate::SessionWorkingDocument>, SessionApplicationFailure>,
    > + Send;

    /// Inspect storage admission; recovery effects require explicit `apply`.
    /// # Errors
    /// Rejects unsupported platforms or unsafe/unverifiable storage ownership.
    fn session_admission(
        &self,
        session_id: SessionId,
        apply: bool,
    ) -> impl Future<Output = Result<crate::StorageAdmissionReport, SessionApplicationFailure>> + Send;

    /// Execute explicit bounded storage compression, never normal-read maintenance.
    /// # Errors
    /// Rejects invalid cutoffs, ownership conflicts, or unavailable storage.
    fn compress_session_page(
        &self,
        request: crate::StorageCompressionRequest,
    ) -> impl Future<Output = Result<crate::StorageCompressionResult, SessionApplicationFailure>> + Send;

    /// Invoke a skill through canonical queued scheduling, preserving execution options.
    ///
    /// The result reports placement, not durable turn admission or terminal completion.
    /// # Errors
    /// Rejects disabled/unknown skills, invalid options, ownership or scheduling failure.
    fn invoke_skill(
        &self,
        session_id: SessionId,
        skill_id: bcode_skill_models::SkillId,
        arguments: String,
        display_text: String,
        execution: crate::TurnExecutionOptions,
    ) -> impl Future<Output = Result<crate::MessageAcceptance, SessionApplicationFailure>> + Send;

    /// Read one bounded catalog page. Cursor conflicts require restarting the observation.
    /// # Errors
    /// Rejects invalid queries or unavailable catalog generations.
    fn session_catalog_page(
        &self,
        scope: crate::SessionDiscoveryScope,
        query: crate::SessionCatalogQuery,
        cursor: Option<crate::SessionCatalogCursor>,
        limit: usize,
    ) -> impl Future<Output = Result<crate::SessionCatalogPage, SessionApplicationFailure>> + Send;

    /// Read canonical metadata without taking runtime ownership.
    /// # Errors
    /// Rejects ambiguous locations and unavailable metadata.
    fn session_summary(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<SessionSummary, SessionApplicationFailure>> + Send;

    /// Delete an idle canonical session through its owning domain.
    /// # Errors
    /// Rejects active work, unavailable storage, or failed deletion.
    fn delete_session(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<SessionSummary, SessionApplicationFailure>> + Send;

    /// Read one bounded canonical history page, never a full replay.
    /// # Errors
    /// Rejects invalid queries, incompatible active callers, or unavailable history.
    fn session_history_page(
        &self,
        session_id: SessionId,
        query: crate::SessionHistoryQuery,
    ) -> impl Future<Output = Result<crate::SessionHistoryPage, SessionApplicationFailure>> + Send;

    /// Read a bounded window around one canonical sequence.
    /// # Errors
    /// Rejects invalid windows or unavailable history.
    fn session_history_around(
        &self,
        session_id: SessionId,
        query: crate::SessionHistoryAroundQuery,
    ) -> impl Future<Output = Result<crate::SessionHistoryWindow, SessionApplicationFailure>> + Send;

    /// Read bounded source usage accounting without collecting or repairing indexes.
    /// # Errors
    /// Rejects invalid queries, ambiguous locations, or unavailable usage projections.
    fn session_usage(
        &self,
        session_id: SessionId,
        query: crate::SessionUsageQuery,
    ) -> impl Future<Output = Result<crate::SessionUsagePage, SessionApplicationFailure>> + Send;

    /// Measure physical session storage with an explicit traversal budget.
    /// # Errors
    /// Rejects invalid budgets or unsafe/unavailable storage.
    fn session_storage_usage(
        &self,
        session_id: SessionId,
        entry_budget: u32,
    ) -> impl Future<Output = Result<crate::SessionStorageUsage, SessionApplicationFailure>> + Send;

    /// Read an explicitly persisted composer draft; renderer-local editing is separate.
    /// # Errors
    /// Rejects unavailable draft storage or incompatible session authority.
    fn composer_draft(
        &self,
        scope: crate::ComposerDraftScope,
    ) -> impl Future<Output = Result<Option<String>, SessionApplicationFailure>> + Send;

    /// Persist an explicit composer draft through the canonical owner.
    /// # Errors
    /// Rejects unavailable draft storage or failed mutation.
    fn set_composer_draft(
        &self,
        scope: crate::ComposerDraftScope,
        text: String,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Persist provider-neutral reasoning selections.
    /// # Errors
    /// Rejects unavailable sessions or failed canonical mutation.
    fn set_session_reasoning(
        &self,
        session_id: SessionId,
        effort: Option<String>,
        summary: Option<String>,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Activate a skill using the existing skill-owned model policy.
    /// # Errors
    /// Rejects disabled/unknown skills, model policy conflicts, or failed mutation.
    fn activate_skill(
        &self,
        session_id: SessionId,
        skill_id: bcode_skill_models::SkillId,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Deactivate a skill and restore its canonical model override.
    /// # Errors
    /// Rejects unavailable sessions or failed mutation; errors do not imply rollback.
    fn deactivate_skill(
        &self,
        session_id: SessionId,
        skill_id: bcode_skill_models::SkillId,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Read current skill contexts without changing skill state.
    /// # Errors
    /// Rejects unavailable sessions or incompatible caller authority.
    fn active_skills(
        &self,
        session_id: SessionId,
    ) -> impl Future<
        Output = Result<Vec<bcode_skill_models::SkillContextResponse>, SessionApplicationFailure>,
    > + Send;

    /// Read the bounded source snapshot used for explicit derivation.
    /// # Errors
    /// Rejects unavailable canonical source projections.
    fn session_derivation_snapshot(
        &self,
        session_id: SessionId,
    ) -> impl Future<
        Output = Result<crate::SessionDerivationSourceSnapshot, SessionApplicationFailure>,
    > + Send;

    /// Read bounded prompt candidates from a canonical source.
    /// # Errors
    /// Rejects invalid queries or unavailable source projections.
    fn session_derivation_prompts(
        &self,
        session_id: SessionId,
        query: crate::SessionDerivationPromptQuery,
    ) -> impl Future<Output = Result<crate::SessionDerivationPromptPage, SessionApplicationFailure>> + Send;

    /// Derive canonical history using the domain's exact retry and conflict semantics.
    /// # Errors
    /// Rejects invalid requests, changed source generations, or canonical derivation failures.
    fn derive_session(
        &self,
        request: crate::SessionDerivationRequest,
    ) -> impl Future<
        Output = Result<crate::SessionDerivationTerminalOutcome, SessionApplicationFailure>,
    > + Send;

    /// Inspect a known derivation operation. Terminal outcomes remain authoritative.
    /// # Errors
    /// Rejects unknown operations or unavailable operation state.
    fn session_derivation_status(
        &self,
        operation_id: crate::SessionDerivationOperationId,
    ) -> impl Future<
        Output = Result<crate::SessionDerivationOperationSnapshot, SessionApplicationFailure>,
    > + Send;

    /// Request derivation cancellation; acknowledgement is not terminal settlement.
    /// # Errors
    /// Rejects a stopped application; unknown operations return false.
    fn cancel_session_derivation(
        &self,
        operation_id: crate::SessionDerivationOperationId,
    ) -> impl Future<Output = Result<bool, SessionApplicationFailure>> + Send;

    /// Read bounded activity for a canonical session and its direct children.
    /// # Errors
    /// Rejects invalid budgets or unavailable canonical metadata.
    fn session_activity(
        &self,
        session_id: SessionId,
        query: crate::SessionActivityQuery,
    ) -> impl Future<Output = Result<crate::SessionActivityPage, SessionApplicationFailure>> + Send;

    /// Create and publish one canonical session.
    ///
    /// # Errors
    /// Rejects relative working directories, unavailable storage, or failed canonical creation.
    fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> impl Future<Output = Result<SessionSummary, SessionApplicationFailure>> + Send;

    /// Inspect one bounded category of canonical events without repair or full replay.
    ///
    /// # Errors
    /// Rejects incompatible active namespaces, invalid queries, or unavailable canonical history.
    fn inspect_session(
        &self,
        session_id: SessionId,
        query: SessionInspectionQuery,
    ) -> impl Future<Output = Result<SessionInspectionPage, SessionApplicationFailure>> + Send;

    /// Rename a canonical session, publishing its event and updating its derived catalog entry.
    ///
    /// # Errors
    /// Returns a normalized failure if the canonical session mutation cannot complete.
    fn rename_session(
        &self,
        session_id: SessionId,
        name: Option<String>,
    ) -> impl Future<Output = Result<SessionSummary, SessionApplicationFailure>> + Send;

    /// Change an idle session's working directory, resolving the absolute directory first.
    ///
    /// # Errors
    /// Rejects relative, unavailable, or non-directory paths, active turns, or failed mutation.
    fn change_session_working_directory(
        &self,
        session_id: SessionId,
        working_directory: PathBuf,
    ) -> impl Future<Output = Result<SessionWorkingDirectoryChange, SessionApplicationFailure>> + Send;

    /// Admit and enqueue an interactive turn through the existing canonical scheduler.
    ///
    /// Admission preserves producer/idempotency semantics and clears caller-supplied activity
    /// display metadata. Non-interactive priority returns `Rejected(ExecutionPolicy)`.
    ///
    /// # Errors
    /// Rejects incompatible namespaces, ownership or admission failures, or queue dispatch
    /// failure. A dispatch failure may follow durable admission and does not permit blind retry.
    fn submit_turn(
        &self,
        session_id: SessionId,
        text: String,
        admission: crate::TurnAdmissionMetadata,
    ) -> impl Future<Output = Result<crate::TurnAdmission, SessionApplicationFailure>> + Send;

    /// Persist an explicit model selection using the existing provider/model selection path.
    ///
    /// # Errors
    /// Rejects required-model skill conflicts or failed canonical selection mutation.
    fn set_session_model(
        &self,
        session_id: SessionId,
        provider_plugin_id: Option<String>,
        model_id: String,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Resolve and persist an existing agent profile selection.
    ///
    /// # Errors
    /// Rejects unknown profiles or failed canonical selection mutation.
    fn set_session_agent(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Request cancellation through canonical runtime scheduling.
    ///
    /// `expected_turn_id` targets an exact active turn when supplied. The returned boolean is
    /// an acknowledgement, not a terminal outcome. Clearing queued work is explicit. Dropping
    /// observation or this future does not itself cancel execution.
    ///
    /// # Errors
    /// Returns a normalized failure if cancellation cannot be admitted or dispatched.
    fn cancel_session_turn(
        &self,
        session_id: SessionId,
        clear_queue: bool,
        expected_turn_id: Option<String>,
    ) -> impl Future<Output = Result<bool, SessionApplicationFailure>> + Send;
}
