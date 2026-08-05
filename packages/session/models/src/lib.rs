#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared session models for Bcode.
//!
//! Compaction snapshots are durable replacement-context boundaries. Their event sequence orders
//! competing boundaries; `compacted_through_sequence` names the canonical prefix replaced by the
//! snapshot. Provider snapshots contain opaque messages that may be replayed only when provider,
//! model, auth profile, format version, and compatibility key match. The portable summary is the
//! required fallback for every other surface.

/// Durable session-storage writer epoch shared by runtime and daemon compatibility handshakes.
pub const CURRENT_SESSION_STORAGE_WRITER_EPOCH: u32 = 6;

use bcode_skill_models::{SkillActivationMode, SkillContextResponse, SkillId, SkillSource};
pub use bcode_tool_models::{
    ToolContributionArtifact, ToolContributionEnvelope, ToolContributionEvent,
    ToolContributionOperation, ToolContributionPersistence, ToolContributionPlacement,
    ToolExchangeRequest, ToolExchangeResolution, ToolExchangeResolutionEvent,
    ToolExchangeResponsePolicy, ToolInvocationLifecycleEvent, ToolInvocationLifecycleStage,
    ToolPresentationIdentity, ToolPresentationRetention, ToolPresentationScopeState,
    ToolPresentationUpdate, ToolPresentationUpdateError, ToolPresentationUpdateScope,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

mod context_management;
pub use context_management::{
    LocalContextEstimate, ModelRequestIdentity, ProviderContextSnapshot,
    ProviderContextSnapshotOrigin, RequestContextObservation, RequestContextOccupancy,
    RequestContextTokenCount,
};

/// Stable zero-based position of one semantic output unit within a provider round.
///
/// The host rebases provider-round positions into the application turn's ordering domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnOutputPosition(u64);

impl TurnOutputPosition {
    /// Create a turn output position.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the zero-based position.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Renderer-neutral state for one tool invocation reconstructed from raw session events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolInvocationProjection {
    /// Provider tool call identifier.
    pub tool_call_id: String,
    /// Plugin that produced/owns the tool, when known.
    pub producer_plugin_id: Option<String>,
    /// Tool name requested by the model.
    pub tool_name: Option<String>,
    /// Raw JSON arguments requested by the model.
    pub arguments_json: Option<String>,
    /// Working directory captured for this invocation.
    pub working_directory: Option<PathBuf>,
    /// Current lifecycle status.
    pub status: ToolInvocationProjectionStatus,
    /// Raw final text result returned by the tool, when finished.
    pub result_text: Option<String>,
    /// Whether the final tool result was an error.
    pub is_error: Option<bool>,
    /// Raw semantic result returned by the tool.
    pub raw_result: Option<ToolInvocationResult>,
    /// Latest retained plugin-owned presentation at the terminal boundary.
    pub presentation: Option<bcode_tool_models::ToolPresentationUpdate>,
    /// Tool start time as UNIX epoch milliseconds, when known.
    pub started_at_ms: Option<u64>,
    /// Tool finish time as UNIX epoch milliseconds, when known.
    pub finished_at_ms: Option<u64>,
    /// Authoritative terminal duration in milliseconds, when known.
    pub duration_ms: Option<u64>,
}

/// Renderer-neutral tool invocation lifecycle status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolInvocationProjectionStatus {
    /// Request was observed but no stream/final result has been seen.
    #[default]
    Requested,
    /// Canonical invocation lifecycle reported the tool as running.
    Running,
    /// The invocation is active but waiting for external input or a resource.
    Waiting,
    /// The invocation completed successfully or produced a final result.
    Finished,
    /// The owning invocation or turn was cancelled.
    Cancelled,
    /// The invocation lifecycle completed with an error.
    Failed,
}

/// Build renderer-neutral tool invocation projections from chronological session events.
#[must_use]
pub fn build_tool_invocation_projections(events: &[SessionEvent]) -> Vec<ToolInvocationProjection> {
    let mut projections = BTreeMap::new();
    let mut working_directory = None;
    for event in events {
        match &event.kind {
            SessionEventKind::SessionCreated {
                working_directory: created_working_directory,
                ..
            } if !created_working_directory.as_os_str().is_empty() => {
                working_directory = Some(created_working_directory.clone());
            }
            SessionEventKind::WorkingDirectoryChanged {
                new_working_directory,
                ..
            } => working_directory = Some(new_working_directory.clone()),
            _ => {}
        }
        apply_tool_invocation_projection_event(&mut projections, event);
        if let SessionEventKind::ToolCallRequested { tool_call_id, .. } = &event.kind
            && let Some(projection) = projections.get_mut(tool_call_id)
            && projection.working_directory.is_none()
        {
            projection.working_directory.clone_from(&working_directory);
        }
    }
    projections.into_values().collect()
}

/// Apply one session event to a renderer-neutral tool invocation projection map.
pub fn apply_tool_invocation_projection_event(
    projections: &mut BTreeMap<String, ToolInvocationProjection>,
    event: &SessionEvent,
) {
    match &event.kind {
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            producer_plugin_id,
            tool_name,
            arguments_json,
            working_directory,
            ..
        }
        | SessionEventKind::PositionedToolCallRequested {
            tool_call_id,
            producer_plugin_id,
            tool_name,
            arguments_json,
            working_directory,
            ..
        } => {
            let projection = tool_invocation_projection_mut(projections, tool_call_id);
            projection.producer_plugin_id.clone_from(producer_plugin_id);
            projection.tool_name = Some(tool_name.clone());
            projection.arguments_json = Some(arguments_json.clone());
            projection.working_directory.clone_from(working_directory);
            projection.started_at_ms.get_or_insert(event.timestamp_ms);
        }
        SessionEventKind::ToolInvocationLifecycle { event: lifecycle } => {
            let projection = tool_invocation_projection_mut(projections, &lifecycle.invocation_id);
            if is_terminal_projection_status(projection.status) {
                return;
            }
            match lifecycle.stage {
                ToolInvocationLifecycleStage::Started => {
                    projection.status = ToolInvocationProjectionStatus::Running;
                    projection.started_at_ms = Some(event.timestamp_ms);
                }
                ToolInvocationLifecycleStage::Progress => {
                    projection.status = ToolInvocationProjectionStatus::Running;
                    projection.started_at_ms.get_or_insert(event.timestamp_ms);
                }
                ToolInvocationLifecycleStage::Waiting => {
                    projection.status = ToolInvocationProjectionStatus::Waiting;
                    projection.started_at_ms.get_or_insert(event.timestamp_ms);
                }
                ToolInvocationLifecycleStage::Completed => {
                    projection.status = ToolInvocationProjectionStatus::Finished;
                    apply_projection_terminal_timing(projection, lifecycle, event.timestamp_ms);
                }
                ToolInvocationLifecycleStage::Cancelled => {
                    projection.status = ToolInvocationProjectionStatus::Cancelled;
                    apply_projection_terminal_timing(projection, lifecycle, event.timestamp_ms);
                }
                ToolInvocationLifecycleStage::Failed => {
                    projection.status = ToolInvocationProjectionStatus::Failed;
                    projection.is_error = Some(true);
                    apply_projection_terminal_timing(projection, lifecycle, event.timestamp_ms);
                }
            }
        }
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            let projection = tool_invocation_projection_mut(projections, &record.invocation_id);
            if is_terminal_projection_status(projection.status) {
                if projection.result_text.is_none() {
                    projection.result_text = Some(record.model_output.clone());
                    projection.is_error.get_or_insert(record.is_error);
                    if projection.raw_result.is_none() {
                        projection.raw_result.clone_from(&record.result);
                    }
                    projection.presentation.clone_from(&record.presentation);
                    if projection.duration_ms.is_none() {
                        projection.duration_ms = record
                            .result
                            .as_ref()
                            .and_then(tool_result_duration_ms)
                            .or_else(|| {
                                projection_wall_duration_ms(projection, event.timestamp_ms)
                            });
                    }
                }
                return;
            }
            projection.status = ToolInvocationProjectionStatus::Finished;
            projection.result_text = Some(record.model_output.clone());
            projection.is_error = Some(record.is_error);
            projection.raw_result.clone_from(&record.result);
            projection.presentation.clone_from(&record.presentation);
            projection.finished_at_ms = Some(event.timestamp_ms);
            projection.duration_ms = record
                .result
                .as_ref()
                .and_then(tool_result_duration_ms)
                .or_else(|| projection_wall_duration_ms(projection, event.timestamp_ms));
        }
        _ => {}
    }
}

const fn is_terminal_projection_status(status: ToolInvocationProjectionStatus) -> bool {
    matches!(
        status,
        ToolInvocationProjectionStatus::Finished
            | ToolInvocationProjectionStatus::Cancelled
            | ToolInvocationProjectionStatus::Failed
    )
}

fn apply_projection_terminal_timing(
    projection: &mut ToolInvocationProjection,
    lifecycle: &ToolInvocationLifecycleEvent,
    timestamp_ms: u64,
) {
    projection.finished_at_ms = Some(timestamp_ms);
    projection.duration_ms = lifecycle
        .metadata
        .get("duration_ms")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| projection_wall_duration_ms(projection, timestamp_ms));
}

fn projection_wall_duration_ms(
    projection: &ToolInvocationProjection,
    finished_at_ms: u64,
) -> Option<u64> {
    projection
        .started_at_ms
        .map(|started_at_ms| finished_at_ms.saturating_sub(started_at_ms))
}

fn tool_result_duration_ms(result: &ToolInvocationResult) -> Option<u64> {
    let ToolInvocationResult::Artifact { artifact } = result else {
        return None;
    };
    artifact
        .metadata
        .get("duration_ms")
        .and_then(serde_json::Value::as_u64)
}

fn tool_invocation_projection_mut<'a>(
    projections: &'a mut BTreeMap<String, ToolInvocationProjection>,
    tool_call_id: &str,
) -> &'a mut ToolInvocationProjection {
    projections
        .entry(tool_call_id.to_owned())
        .or_insert_with(|| ToolInvocationProjection {
            tool_call_id: tool_call_id.to_owned(),
            ..ToolInvocationProjection::default()
        })
}

/// Current persisted session event schema version.
pub const CURRENT_SESSION_EVENT_SCHEMA_VERSION: u16 = 42;

/// Return the current Unix timestamp in milliseconds.
#[must_use]
pub fn current_unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Unique session identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Generate a new random session identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for SessionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

impl Serialize for SessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            self.0.serialize(serializer)
        } else {
            serializer.serialize_str(&self.0.to_string())
        }
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            Uuid::deserialize(deserializer).map(Self)
        } else {
            let value = String::deserialize(deserializer)?;
            Uuid::parse_str(&value)
                .map(Self)
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Unique session-open preparation operation identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionOpenOperationId(pub Uuid);

impl SessionOpenOperationId {
    /// Generate a new operation identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionOpenOperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for SessionOpenOperationId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Serialize for SessionOpenOperationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            self.0.serialize(serializer)
        } else {
            serializer.serialize_str(&self.0.to_string())
        }
    }
}

impl<'de> Deserialize<'de> for SessionOpenOperationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            Uuid::deserialize(deserializer).map(Self)
        } else {
            let value = String::deserialize(deserializer)?;
            Uuid::parse_str(&value)
                .map(Self)
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Ordered stage of legacy storage preparation for session open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationStage {
    /// Waiting to acquire exclusive session maintenance ownership.
    WaitingForOwnership,
    /// Reading bounded storage-contract metadata.
    InspectingStorage,
    /// Enumerating files and bytes for a retained backup.
    PlanningBackup,
    /// Copying the retained backup.
    CopyingBackup,
    /// Verifying copied backup bytes.
    VerifyingBackup,
    /// Applying database schema migrations before projection replay.
    PreparingSchema,
    /// Reading and decoding canonical history.
    ReadingCanonicalHistory,
    /// Rebuilding all derived projections from canonical history.
    RebuildingProjections,
    /// Validating projection schemas and canonical-tail checkpoints.
    ValidatingProjections,
    /// Committing the atomic migration transaction.
    Committing,
    /// Validating post-commit write readiness.
    ValidatingWriteReadiness,
    /// Performing the final bounded session open.
    OpeningSession,
    /// Preparation completed successfully.
    Complete,
    /// Preparation terminated with a classified failure.
    Failed,
}

/// Natural unit for one determinate migration stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationProgressUnit {
    /// Files discovered or processed.
    Files,
    /// File bytes copied or verified.
    Bytes,
    /// Canonical events decoded or projected.
    Events,
}

/// Stable classification for a terminal session-open preparation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOpenFailureKind {
    /// Another daemon owns the session.
    OwnedByOtherDaemon,
    /// The durable writer contract is incompatible with this build.
    WriterIncompatible,
    /// A required projection is stale or incompatible.
    ProjectionStale,
    /// Canonical or derived storage requires explicit repair.
    RepairRequired,
    /// A retained pre-migration backup could not be verified.
    BackupFailed,
    /// Migration failed after ownership and backup preconditions succeeded.
    MigrationFailed,
    /// Session storage disappeared before preparation completed.
    NotFound,
}

/// Progress within the current session migration stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMigrationProgress {
    /// Current ordered migration stage.
    pub stage: SessionMigrationStage,
    /// Completed natural units for a determinate stage.
    #[serde(default)]
    pub completed_units: Option<u64>,
    /// Total natural units for a determinate stage.
    #[serde(default)]
    pub total_units: Option<u64>,
    /// Natural unit associated with completed and total values.
    #[serde(default)]
    pub unit: Option<SessionMigrationProgressUnit>,
    /// Stable user-facing description of the current work.
    pub message: String,
}

/// Terminal outcome of a session-open preparation operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOpenTerminalOutcome {
    /// Session storage is current and writable.
    Ready,
    /// Session is inspectable but contains unsupported semantic history.
    DegradedReadOnly { issue_count: u64 },
    /// Storage was written by an incompatible writer contract.
    WriterIncompatible { actual: Option<u64>, expected: u64 },
    /// Storage requires explicit repair before normal open.
    RepairRequired { reason: String },
    /// Preparation failed with a stable classification.
    Failed {
        kind: SessionOpenFailureKind,
        message: String,
        #[serde(default)]
        backup_path: Option<PathBuf>,
    },
}

/// Reconnectable point-in-time snapshot of one session-open operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOpenOperationSnapshot {
    /// Stable operation identity.
    pub operation_id: SessionOpenOperationId,
    /// Monotonic operation-local revision.
    pub revision: u64,
    /// Session being prepared.
    pub session_id: SessionId,
    /// Legacy writer epoch being migrated, when migration is required.
    #[serde(default)]
    pub source_writer_epoch: Option<u64>,
    /// Writer epoch expected by the current build.
    pub target_writer_epoch: u64,
    /// Latest stage-local progress.
    pub progress: SessionMigrationProgress,
    /// Terminal outcome, present only after operation completion.
    #[serde(default)]
    pub outcome: Option<SessionOpenTerminalOutcome>,
    /// Verified retained backup path, present only after verification succeeds.
    #[serde(default)]
    pub backup_path: Option<PathBuf>,
}

/// Unique connected-client identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub Uuid);

impl ClientId {
    /// Generate a new random client identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for ClientId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Serialize for ClientId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            self.0.serialize(serializer)
        } else {
            serializer.serialize_str(&self.0.to_string())
        }
    }
}

impl<'de> Deserialize<'de> for ClientId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            Uuid::deserialize(deserializer).map(Self)
        } else {
            let value = String::deserialize(deserializer)?;
            Uuid::parse_str(&value)
                .map(Self)
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Source used to determine a session's display title.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTitleSource {
    /// No user-visible title is available.
    #[default]
    EmptyDraft,
    /// Title was explicitly set by creation or rename.
    Explicit,
    /// Title was derived from the first user prompt.
    FirstUserMessage,
    /// Title came from an external imported session.
    Imported,
}

/// Visibility of a session in normal interactive pickers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionVisibility {
    /// Normal user-created interactive session.
    #[default]
    Visible,
    /// Background execution session hidden from normal pickers but directly inspectable.
    Background,
}

/// Context initialization mode for a background execution session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionSessionContextMode {
    /// Start with no copied transcript history.
    FreshIsolated,
    /// Copy the parent transcript at one exact durable generation.
    FixedGenerationFork,
    /// Reuse the parent transcript; hosts must serialize this mode.
    SharedSequential,
}

/// Current execution-session provenance contract version.
pub const EXECUTION_SESSION_PROVENANCE_VERSION: u32 = 2;

/// Generic durable provenance for a background execution session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSessionProvenance {
    /// Provenance contract version.
    pub version: u32,
    /// Domain owner that created the execution session.
    pub owner: String,
    /// Stable owner-defined run identity.
    pub run_id: String,
    /// Stable owner-defined node/unit identity.
    pub node_id: String,
    /// Exact owner-defined activation identity when the owner schedules repeatable units.
    ///
    /// This is optional for owners whose execution model has no activation concept. Workflow-owned
    /// sessions always set it so distinct activations of one node cannot share a transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_id: Option<String>,
    /// Positive attempt number.
    pub attempt: u32,
    /// Interactive parent session.
    pub parent_session_id: SessionId,
    /// Context initialization mode.
    pub context_mode: ExecutionSessionContextMode,
    /// Immutable repository/worktree snapshot identity supplied by its owning domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_snapshot: Option<String>,
    /// Fixed parent generation for `fixed_generation_fork`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_generation: Option<u64>,
}

/// Compact background-execution metadata attached to a session summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSessionSummary {
    pub provenance: ExecutionSessionProvenance,
    pub visibility: SessionVisibility,
}

/// Session summary used by list/select flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub name: Option<String>,
    #[serde(default)]
    pub explicit_name: Option<String>,
    #[serde(default)]
    pub derived_title: Option<String>,
    #[serde(default)]
    pub title_source: SessionTitleSource,
    pub client_count: usize,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub working_directory: PathBuf,
    #[serde(default)]
    pub import: Option<SessionImportSummary>,
    #[serde(default)]
    pub fork: Option<SessionForkSummary>,
    #[serde(default)]
    pub execution: Option<Box<ExecutionSessionSummary>>,
}

impl SessionSummary {
    /// Return the resolved display title for this session, if any.
    ///
    /// This is the canonical source of truth for a session's user-visible name.
    /// Callers should prefer this over inspecting `name`/`explicit_name`/`derived_title`
    /// directly. The precedence is: `name` → `explicit_name` → `derived_title`.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.explicit_name.as_deref())
            .or(self.derived_title.as_deref())
    }

    /// Return whether this session belongs in normal interactive pickers.
    #[must_use]
    pub fn is_picker_visible(&self) -> bool {
        self.execution
            .as_ref()
            .is_none_or(|execution| matches!(execution.visibility, SessionVisibility::Visible))
    }

    /// Return the best user-visible title for this session.
    #[must_use]
    pub fn display_title(&self) -> &str {
        self.title().unwrap_or("empty draft")
    }
}

/// Display/provenance metadata for imported sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImportSummary {
    pub source_id: String,
    pub source_display_name: String,
    pub external_session_id: String,
    pub imported_at_ms: u64,
}

/// Durable fork/clone operation kind for session provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionForkKind {
    /// A new session copied from a source session up to a selected prompt.
    Fork,
    /// A new session copied from the full source session history.
    Clone,
}

/// Display/provenance metadata for forked or cloned sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionForkSummary {
    pub source_session_id: SessionId,
    #[serde(default)]
    pub source_title: Option<String>,
    #[serde(default)]
    pub source_cutoff_sequence: Option<u64>,
    #[serde(default)]
    pub source_prompt_sequence: Option<u64>,
    pub forked_at_ms: u64,
    pub kind: SessionForkKind,
}

/// Result of creating a forked or cloned session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionForkResult {
    /// Newly created session summary.
    pub session: SessionSummary,
    /// Draft text the caller may install in the composer after attaching.
    #[serde(default)]
    pub draft: Option<String>,
}

/// Direction for paged session history reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionHistoryDirection {
    Forward,
    Backward,
}

/// Cursor for paged session history reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryCursor {
    pub sequence: u64,
}

/// Maximum events returned by one normal bounded session-history read.
pub const MAX_SESSION_HISTORY_READ_EVENTS: usize = 1_000;

/// Query for a bounded page of session history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryQuery {
    #[serde(default)]
    pub cursor: Option<SessionHistoryCursor>,
    pub limit: usize,
    pub direction: SessionHistoryDirection,
}

/// Query for a bounded canonical history window around one event sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryAroundQuery {
    /// Canonical sequence at the center of the requested window.
    pub sequence: u64,
    /// Maximum events requested before `sequence`.
    pub before: usize,
    /// Maximum events requested after `sequence`.
    pub after: usize,
}

impl SessionHistoryAroundQuery {
    /// Return the maximum number of events in this window, including the anchor.
    #[must_use]
    pub const fn event_limit(self) -> usize {
        self.before.saturating_add(self.after).saturating_add(1)
    }
}

/// Bounded canonical history window around one requested event sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryWindow {
    pub session_id: SessionId,
    pub requested_sequence: u64,
    pub events: Vec<SessionEvent>,
    /// Whether the requested canonical sequence was present in the returned window.
    pub anchor_present: bool,
    /// First canonical event sequence currently available for the session.
    pub first_available_sequence: Option<u64>,
    /// Last canonical event sequence currently available for the session.
    pub last_available_sequence: Option<u64>,
    /// Opaque-event diagnostics for events present in this window.
    #[serde(default)]
    pub compatibility_issues: Vec<SessionEventCompatibilityIssue>,
}

/// Maximum matching events returned by one structured session inspection read.
pub const MAX_SESSION_INSPECTION_EVENTS: usize = 200;

/// High-value semantic event category for bounded session investigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionInspectionCategory {
    /// Failed terminal tool invocation records and lifecycle events.
    FailedToolCalls,
    /// Permission requests and their resolutions.
    Permissions,
    /// Model, reasoning, agent, and working-directory selection changes.
    SelectionChanges,
    /// Durable runtime-work lifecycle and progress events.
    RuntimeWork,
    /// Local and provider-native context compaction boundaries.
    Compactions,
    /// Durable model-turn, tool-invocation, and runtime-work terminal outcomes.
    TerminalOutcomes,
}

/// Query for one bounded structured session investigation page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInspectionQuery {
    pub category: SessionInspectionCategory,
    #[serde(default)]
    pub cursor: Option<SessionHistoryCursor>,
    pub limit: usize,
    pub direction: SessionHistoryDirection,
}

/// Bounded page of canonical events matching a structured investigation category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInspectionPage {
    pub session_id: SessionId,
    pub category: SessionInspectionCategory,
    pub events: Vec<SessionEvent>,
    /// Number of bounded canonical candidate events decoded for this page.
    pub scanned_events: usize,
    /// Opaque-event diagnostics for events present in this page.
    #[serde(default)]
    pub compatibility_issues: Vec<SessionEventCompatibilityIssue>,
    /// Cursor for continuing through canonical candidates in the requested direction.
    #[serde(default)]
    pub next_cursor: Option<SessionHistoryCursor>,
    /// Whether additional canonical candidates may remain after this page.
    pub has_more: bool,
}

/// Compatibility classification for a canonical event that remains inspectable
/// but is not semantically understood by the current build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventCompatibilityKind {
    /// The event kind is not known to the current build.
    UnknownEventKind,
    /// The event schema is newer than the current build supports.
    FutureSchema,
}

/// Structured compatibility diagnostic for one opaque canonical history event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventCompatibilityIssue {
    /// Canonical sequence occupied by the opaque event.
    pub sequence: u64,
    /// Persisted event-kind name retained from the canonical payload.
    pub event_kind: String,
    /// Persisted event schema version.
    pub schema_version: u16,
    /// Why the event is opaque to the current build.
    pub compatibility: SessionEventCompatibilityKind,
    /// Actionable user-facing remediation.
    pub remediation: String,
}

/// Bounded page of replayable session history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryPage {
    pub session_id: SessionId,
    pub events: Vec<SessionEvent>,
    /// Opaque-event diagnostics for events present in this page.
    #[serde(default)]
    pub compatibility_issues: Vec<SessionEventCompatibilityIssue>,
    #[serde(default)]
    pub next_cursor: Option<SessionHistoryCursor>,
    pub has_more: bool,
}

/// User-submitted prompt entry used for composer input-history navigation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInputHistoryEntry {
    pub sequence: u64,
    #[serde(default)]
    pub timestamp_ms: u64,
    pub text: String,
}

/// Generic optional origin attached to an ordinary accepted turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnOrigin {
    /// Stable producer namespace. Core stores but does not interpret this value.
    pub producer: String,
    /// Optional producer-owned correlation identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Optional presentation label supplied by the producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
}

/// Stable identifier for one admitted model turn.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub String);

impl TurnId {
    /// Derive the canonical turn identity from its accepted user-message event.
    #[must_use]
    pub fn from_accepted_event(session_id: SessionId, accepted_event_sequence: u64) -> Self {
        Self(format!("{session_id}-{accepted_event_sequence}"))
    }
}

impl Display for TurnId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Durable receipt returned after a turn is admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnReceipt {
    pub work_id: WorkId,
    pub turn_id: TurnId,
    pub accepted_event_sequence: u64,
}

impl TurnReceipt {
    /// Derive the canonical receipt from an accepted user-message event.
    #[must_use]
    pub fn from_accepted_event(session_id: SessionId, accepted_event_sequence: u64) -> Self {
        let turn_id = TurnId::from_accepted_event(session_id, accepted_event_sequence);
        Self {
            work_id: WorkId::new(format!("model_{turn_id}")),
            turn_id,
            accepted_event_sequence,
        }
    }
}

/// Generic scheduling priority for an admitted turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPriority {
    #[default]
    Interactive,
    Background,
}

/// Generic tool availability for one turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnToolPolicy {
    #[default]
    Enabled,
    /// Expose only tools whose generic authorization metadata declares them read-only.
    ReadOnly,
    Disabled,
}

/// Generic correlation for one externally orchestrated execution unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnExecutionCorrelation {
    /// Stable owner-defined execution identity, such as a run ID.
    pub execution_id: String,
    /// Stable owner-defined unit identity within the execution.
    pub unit_id: String,
    /// Owner-defined attempt number for this unit.
    pub attempt: u32,
}

/// Persisted provider-neutral structured-output request for one admitted turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStructuredOutputRequest {
    /// Provider-portable output object name containing only ASCII letters, digits, underscores,
    /// and hyphens.
    pub name: String,
    /// JSON schema the provider should satisfy.
    pub schema: serde_json::Value,
    /// Whether provider-native strict validation should be requested where supported.
    #[serde(default)]
    pub strict: bool,
}

/// Provider-neutral reasoning request overrides applied to one admitted turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnReasoningOptions {
    /// Immutable provider-native effort label selected from advertised model capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Immutable provider-native reasoning summary label selected from advertised capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Generic execution options applied to one admitted turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnExecutionOptions {
    /// Persisted execution-options schema version.
    #[serde(default = "turn_execution_options_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub tools: TurnToolPolicy,
    /// Optional generic external-execution correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<TurnExecutionCorrelation>,
    /// Immutable agent/profile override for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    /// Exact tool names available to this turn, intersected with profile and generic tool policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_allowlist: Option<Vec<String>>,
    /// Immutable model-provider plugin override for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_plugin_id: Option<String>,
    /// Immutable provider-neutral requested model identifier for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Immutable provider-neutral reasoning request overrides for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Box<TurnReasoningOptions>>,
    /// Optional provider-neutral structured-output request for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<TurnStructuredOutputRequest>,
    /// Exact bounded skill contexts resolved for this turn before external dispatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_contexts: Vec<SkillContextResponse>,
}

/// Earliest persisted turn execution-options schema version accepted by this build.
pub const MIN_TURN_EXECUTION_OPTIONS_SCHEMA_VERSION: u32 = 1;
/// Current persisted turn execution-options schema version.
pub const TURN_EXECUTION_OPTIONS_SCHEMA_VERSION: u32 = 2;

const fn turn_execution_options_schema_version() -> u32 {
    TURN_EXECUTION_OPTIONS_SCHEMA_VERSION
}

impl Default for TurnExecutionOptions {
    fn default() -> Self {
        Self {
            schema_version: TURN_EXECUTION_OPTIONS_SCHEMA_VERSION,
            tools: TurnToolPolicy::default(),
            correlation: None,
            agent_profile: None,
            tool_allowlist: None,
            provider_plugin_id: None,
            model_id: None,
            reasoning: None,
            structured_output: None,
            skill_contexts: Vec::new(),
        }
    }
}

/// Generic metadata carried by an ordinary admitted turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAdmissionMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TurnOrigin>,
    #[serde(default)]
    pub priority: TurnPriority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub execution: TurnExecutionOptions,
}

const MAX_TURN_PRODUCER_BYTES: usize = 256;
const MAX_TURN_IDEMPOTENCY_KEY_BYTES: usize = 512;
const MAX_TURN_EXECUTION_ID_BYTES: usize = 512;
const MAX_TURN_EXECUTION_UNIT_ID_BYTES: usize = 512;
const MAX_TURN_AGENT_PROFILE_BYTES: usize = 256;
const MAX_TURN_PROVIDER_PLUGIN_ID_BYTES: usize = 256;
const MAX_TURN_MODEL_ID_BYTES: usize = 512;
const MAX_TURN_REASONING_VALUE_BYTES: usize = 128;
const MAX_TURN_TOOL_ALLOWLIST_ENTRIES: usize = 256;
const MAX_TURN_TOOL_NAME_BYTES: usize = 256;
const MAX_TURN_SKILL_CONTEXTS: usize = 32;
const MAX_TURN_SKILL_ID_BYTES: usize = 256;
/// Maximum bytes in a provider-portable structured-output name.
pub const MAX_TURN_STRUCTURED_OUTPUT_NAME_BYTES: usize = 64;
const MAX_TURN_STRUCTURED_OUTPUT_SCHEMA_BYTES: usize = 256 * 1024;

/// Return whether a structured-output name is portable across supported providers.
#[must_use]
pub fn is_valid_structured_output_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TURN_STRUCTURED_OUTPUT_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Derive a bounded provider-portable structured-output name from a diagnostic identity.
#[must_use]
pub fn structured_output_name(identity: &str) -> String {
    let mut normalized = String::with_capacity(identity.len());
    for character in identity.chars() {
        normalized.push(
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            },
        );
    }
    if normalized.is_empty() {
        return "structured_output".to_string();
    }
    if normalized.len() <= MAX_TURN_STRUCTURED_OUTPUT_NAME_BYTES {
        return normalized;
    }
    let hash = identity
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            hash.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(*byte)
        });
    let suffix = format!("_{hash:016x}");
    normalized.truncate(MAX_TURN_STRUCTURED_OUTPUT_NAME_BYTES - suffix.len());
    normalized.push_str(&suffix);
    normalized
}

/// Generic turn-admission metadata validation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnAdmissionMetadataError {
    MissingIdempotencyProducer,
    ProducerTooLong,
    IdempotencyKeyTooLong,
    EmptyIdempotencyKey,
    EmptyAgentProfile,
    AgentProfileTooLong,
    TooManyAllowedTools,
    EmptyAllowedTool,
    AllowedToolTooLong,
    DuplicateAllowedTool,
    UnsupportedExecutionOptionsVersion,
    EmptyExecutionId,
    ExecutionIdTooLong,
    EmptyExecutionUnitId,
    ExecutionUnitIdTooLong,
    InvalidExecutionAttempt,
    EmptyProviderPluginId,
    ProviderPluginIdTooLong,
    EmptyModelId,
    ModelIdTooLong,
    EmptyReasoningEffort,
    ReasoningEffortTooLong,
    EmptyReasoningSummary,
    ReasoningSummaryTooLong,
    EmptyStructuredOutputName,
    InvalidStructuredOutputName,
    StructuredOutputNameTooLong,
    StructuredOutputSchemaTooLarge,
    InvalidStructuredOutputSchema,
    TooManySkillContexts,
    EmptySkillId,
    SkillIdTooLong,
    DuplicateSkillContext,
    InvalidSkillContextLength,
    EmptySkillContext,
    TruncatedSkillContext,
}

impl Display for TurnAdmissionMetadataError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MissingIdempotencyProducer => {
                "idempotency key requires a non-empty origin producer"
            }
            Self::ProducerTooLong => "turn origin producer is too long",
            Self::IdempotencyKeyTooLong => "turn idempotency key is too long",
            Self::EmptyIdempotencyKey => "turn idempotency key must not be empty",
            Self::EmptyAgentProfile => "turn agent profile must not be empty",
            Self::AgentProfileTooLong => "turn agent profile is too long",
            Self::TooManyAllowedTools => "turn tool allowlist has too many entries",
            Self::EmptyAllowedTool => "turn tool allowlist entries must not be empty",
            Self::AllowedToolTooLong => "turn tool allowlist entry is too long",
            Self::DuplicateAllowedTool => "turn tool allowlist contains a duplicate entry",
            Self::UnsupportedExecutionOptionsVersion => {
                "turn execution options schema version is unsupported"
            }
            Self::EmptyExecutionId => "turn execution correlation ID must not be empty",
            Self::ExecutionIdTooLong => "turn execution correlation ID is too long",
            Self::EmptyExecutionUnitId => "turn execution unit ID must not be empty",
            Self::ExecutionUnitIdTooLong => "turn execution unit ID is too long",
            Self::InvalidExecutionAttempt => "turn execution attempt must be greater than zero",
            Self::EmptyProviderPluginId => "turn provider plugin ID must not be empty",
            Self::ProviderPluginIdTooLong => "turn provider plugin ID is too long",
            Self::EmptyModelId => "turn model ID must not be empty",
            Self::ModelIdTooLong => "turn model ID is too long",
            Self::EmptyReasoningEffort => "turn reasoning effort must not be empty",
            Self::ReasoningEffortTooLong => "turn reasoning effort is too long",
            Self::EmptyReasoningSummary => "turn reasoning summary must not be empty",
            Self::ReasoningSummaryTooLong => "turn reasoning summary is too long",
            Self::EmptyStructuredOutputName => "turn structured-output name must not be empty",
            Self::InvalidStructuredOutputName => {
                "turn structured-output name may contain only ASCII letters, digits, underscores, and hyphens"
            }
            Self::StructuredOutputNameTooLong => "turn structured-output name is too long",
            Self::StructuredOutputSchemaTooLarge => "turn structured-output schema is too large",
            Self::InvalidStructuredOutputSchema => {
                "turn structured-output schema must be a JSON object"
            }
            Self::TooManySkillContexts => "turn has too many resolved skill contexts",
            Self::EmptySkillId => "turn skill context ID must not be empty",
            Self::SkillIdTooLong => "turn skill context ID is too long",
            Self::DuplicateSkillContext => "turn has a duplicate resolved skill context",
            Self::InvalidSkillContextLength => {
                "turn skill context byte count does not match its content"
            }
            Self::EmptySkillContext => "turn skill context must contain at least one byte",
            Self::TruncatedSkillContext => {
                "turn skill context must be complete rather than truncated"
            }
        })
    }
}

impl std::error::Error for TurnAdmissionMetadataError {}

fn validate_structured_output_request(
    request: &TurnStructuredOutputRequest,
) -> Result<(), TurnAdmissionMetadataError> {
    if request.name.is_empty() {
        return Err(TurnAdmissionMetadataError::EmptyStructuredOutputName);
    }
    if request.name.len() > MAX_TURN_STRUCTURED_OUTPUT_NAME_BYTES {
        return Err(TurnAdmissionMetadataError::StructuredOutputNameTooLong);
    }
    if !is_valid_structured_output_name(&request.name) {
        return Err(TurnAdmissionMetadataError::InvalidStructuredOutputName);
    }
    let schema_bytes = serde_json::to_vec(&request.schema)
        .map_err(|_| TurnAdmissionMetadataError::InvalidStructuredOutputSchema)?;
    if schema_bytes.len() > MAX_TURN_STRUCTURED_OUTPUT_SCHEMA_BYTES {
        return Err(TurnAdmissionMetadataError::StructuredOutputSchemaTooLarge);
    }
    if !request.schema.is_object() {
        return Err(TurnAdmissionMetadataError::InvalidStructuredOutputSchema);
    }
    Ok(())
}

fn validate_skill_contexts(
    contexts: &[SkillContextResponse],
) -> Result<(), TurnAdmissionMetadataError> {
    if contexts.len() > MAX_TURN_SKILL_CONTEXTS {
        return Err(TurnAdmissionMetadataError::TooManySkillContexts);
    }
    let mut skill_ids = std::collections::BTreeSet::new();
    for skill in contexts {
        if skill.skill_id.as_str().is_empty() {
            return Err(TurnAdmissionMetadataError::EmptySkillId);
        }
        if skill.skill_id.as_str().len() > MAX_TURN_SKILL_ID_BYTES {
            return Err(TurnAdmissionMetadataError::SkillIdTooLong);
        }
        if !skill_ids.insert(skill.skill_id.as_str()) {
            return Err(TurnAdmissionMetadataError::DuplicateSkillContext);
        }
        if skill.bytes_loaded == 0 || skill.context.is_empty() {
            return Err(TurnAdmissionMetadataError::EmptySkillContext);
        }
        if skill.bytes_loaded != skill.context.len() {
            return Err(TurnAdmissionMetadataError::InvalidSkillContextLength);
        }
        if skill.truncated {
            return Err(TurnAdmissionMetadataError::TruncatedSkillContext);
        }
    }
    Ok(())
}

impl TurnAdmissionMetadata {
    /// Validate bounded generic admission metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when producer/idempotency fields are empty where identity requires them
    /// or exceed their durable bounds.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), TurnAdmissionMetadataError> {
        if !(MIN_TURN_EXECUTION_OPTIONS_SCHEMA_VERSION..=TURN_EXECUTION_OPTIONS_SCHEMA_VERSION)
            .contains(&self.execution.schema_version)
        {
            return Err(TurnAdmissionMetadataError::UnsupportedExecutionOptionsVersion);
        }
        if self.execution.schema_version < TURN_EXECUTION_OPTIONS_SCHEMA_VERSION
            && self.execution.reasoning.is_some()
        {
            return Err(TurnAdmissionMetadataError::UnsupportedExecutionOptionsVersion);
        }
        if self
            .origin
            .as_ref()
            .is_some_and(|origin| origin.producer.len() > MAX_TURN_PRODUCER_BYTES)
        {
            return Err(TurnAdmissionMetadataError::ProducerTooLong);
        }
        if let Some(key) = &self.idempotency_key {
            if key.is_empty() {
                return Err(TurnAdmissionMetadataError::EmptyIdempotencyKey);
            }
            if key.len() > MAX_TURN_IDEMPOTENCY_KEY_BYTES {
                return Err(TurnAdmissionMetadataError::IdempotencyKeyTooLong);
            }
            if self
                .origin
                .as_ref()
                .is_none_or(|origin| origin.producer.is_empty())
            {
                return Err(TurnAdmissionMetadataError::MissingIdempotencyProducer);
            }
        }
        if let Some(correlation) = &self.execution.correlation {
            if correlation.execution_id.is_empty() {
                return Err(TurnAdmissionMetadataError::EmptyExecutionId);
            }
            if correlation.execution_id.len() > MAX_TURN_EXECUTION_ID_BYTES {
                return Err(TurnAdmissionMetadataError::ExecutionIdTooLong);
            }
            if correlation.unit_id.is_empty() {
                return Err(TurnAdmissionMetadataError::EmptyExecutionUnitId);
            }
            if correlation.unit_id.len() > MAX_TURN_EXECUTION_UNIT_ID_BYTES {
                return Err(TurnAdmissionMetadataError::ExecutionUnitIdTooLong);
            }
            if correlation.attempt == 0 {
                return Err(TurnAdmissionMetadataError::InvalidExecutionAttempt);
            }
        }
        if let Some(profile) = &self.execution.agent_profile {
            if profile.is_empty() {
                return Err(TurnAdmissionMetadataError::EmptyAgentProfile);
            }
            if profile.len() > MAX_TURN_AGENT_PROFILE_BYTES {
                return Err(TurnAdmissionMetadataError::AgentProfileTooLong);
            }
        }
        if let Some(tools) = &self.execution.tool_allowlist {
            if tools.len() > MAX_TURN_TOOL_ALLOWLIST_ENTRIES {
                return Err(TurnAdmissionMetadataError::TooManyAllowedTools);
            }
            let mut unique = std::collections::BTreeSet::new();
            for tool in tools {
                if tool.is_empty() {
                    return Err(TurnAdmissionMetadataError::EmptyAllowedTool);
                }
                if tool.len() > MAX_TURN_TOOL_NAME_BYTES {
                    return Err(TurnAdmissionMetadataError::AllowedToolTooLong);
                }
                if !unique.insert(tool) {
                    return Err(TurnAdmissionMetadataError::DuplicateAllowedTool);
                }
            }
        }
        if let Some(provider) = &self.execution.provider_plugin_id {
            if provider.is_empty() {
                return Err(TurnAdmissionMetadataError::EmptyProviderPluginId);
            }
            if provider.len() > MAX_TURN_PROVIDER_PLUGIN_ID_BYTES {
                return Err(TurnAdmissionMetadataError::ProviderPluginIdTooLong);
            }
        }
        if let Some(model) = &self.execution.model_id {
            if model.is_empty() {
                return Err(TurnAdmissionMetadataError::EmptyModelId);
            }
            if model.len() > MAX_TURN_MODEL_ID_BYTES {
                return Err(TurnAdmissionMetadataError::ModelIdTooLong);
            }
        }
        if let Some(reasoning) = &self.execution.reasoning {
            if let Some(effort) = &reasoning.effort {
                if effort.is_empty() {
                    return Err(TurnAdmissionMetadataError::EmptyReasoningEffort);
                }
                if effort.len() > MAX_TURN_REASONING_VALUE_BYTES {
                    return Err(TurnAdmissionMetadataError::ReasoningEffortTooLong);
                }
            }
            if let Some(summary) = &reasoning.summary {
                if summary.is_empty() {
                    return Err(TurnAdmissionMetadataError::EmptyReasoningSummary);
                }
                if summary.len() > MAX_TURN_REASONING_VALUE_BYTES {
                    return Err(TurnAdmissionMetadataError::ReasoningSummaryTooLong);
                }
            }
        }
        if let Some(request) = &self.execution.structured_output {
            validate_structured_output_request(request)?;
        }
        validate_skill_contexts(&self.execution.skill_contexts)?;
        Ok(())
    }

    /// Return the producer/key pair used for idempotency lookup.
    #[must_use]
    pub fn idempotency_identity(&self) -> Option<(&str, &str)> {
        Some((
            self.origin.as_ref()?.producer.as_str(),
            self.idempotency_key.as_deref()?,
        ))
    }
}

/// Generic reason that turn admission was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRejectionReason {
    SessionUnavailable,
    ExecutionPolicy,
}

/// Result of admitting an ordinary turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAdmission {
    Accepted(TurnReceipt),
    Existing(TurnReceipt),
    Deferred(TurnReceipt),
    Rejected(TurnRejectionReason),
    CancelledBeforeStart(TurnReceipt),
}

/// Durable work identifier used across session history, IPC, and UI surfaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkId(pub String);

impl WorkId {
    /// Create a work identifier.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl Display for WorkId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Durable runtime work category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeWorkKind {
    /// Model-callable tool execution.
    #[default]
    Tool,
    /// Plugin service invocation.
    PluginInvocation,
    /// Model-provider turn.
    ModelTurn,
    /// Plugin event delivery.
    EventDelivery,
    /// Durable multi-node workflow run.
    Workflow,
    /// One node/attempt within a durable workflow run.
    WorkflowNode,
}

/// Durable runtime work terminal/current status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeWorkStatus {
    /// Work has been queued.
    Queued,
    /// Work is running.
    #[default]
    Running,
    /// Cancellation has been requested.
    Cancelling,
    /// Work completed successfully.
    Completed,
    /// Work failed.
    Failed,
    /// Work timed out.
    TimedOut,
    /// Work was cancelled.
    Cancelled,
}

/// Source provenance for an event imported from another agent/tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventProvenance {
    /// External source event identifier, when available.
    #[serde(default)]
    pub source_event_id: Option<String>,
    /// External source event timestamp in Unix milliseconds, when available.
    #[serde(default)]
    pub source_timestamp_ms: Option<u64>,
    /// External source locator such as a file path, when available.
    #[serde(default)]
    pub source_locator: Option<String>,
}

/// Replayable event emitted by a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub schema_version: u16,
    pub sequence: u64,
    /// Unix timestamp in milliseconds when the event was created or emitted.
    #[serde(default = "current_unix_timestamp_ms")]
    pub timestamp_ms: u64,
    pub session_id: SessionId,
    #[serde(default)]
    pub provenance: Option<SessionEventProvenance>,
    pub kind: SessionEventKind,
}

/// Live-only session event emitted to currently attached clients.
///
/// Live events are intentionally not persisted, indexed, or used for replay.
/// They are suitable for high-frequency UI streams where the durable event log
/// records the final semantic result separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionLiveEvent {
    pub session_id: SessionId,
    pub kind: SessionLiveEventKind,
}

/// Provider-neutral representation of readable reasoning content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningContentKind {
    /// Provider-generated summary or milestone content.
    Summary,
    /// Provider-exposed raw or detailed reasoning content.
    Raw,
    /// Untyped reasoning retained for compatibility with legacy producers.
    Legacy,
}

/// Provider-neutral semantic role of one readable reasoning part.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningContentRole {
    /// A provider-defined reasoning milestone or summary section.
    Milestone,
    /// Detailed reasoning content.
    Detail,
    /// The provider did not classify the part further.
    #[default]
    Unknown,
}

/// Terminal state of a provider-reported reasoning activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningActivityStatus {
    /// The provider completed the reasoning activity normally.
    Completed,
    /// The activity was interrupted before normal completion.
    Interrupted,
    /// The activity failed.
    Failed,
}

/// One complete readable part of a provider-reported reasoning activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningPart {
    /// Stable provider-neutral identifier within the owning activity.
    pub part_id: String,
    /// Whether this is summary, raw, or legacy untyped reasoning.
    pub kind: ReasoningContentKind,
    /// Provider-supplied semantic role when known.
    #[serde(default)]
    pub role: ReasoningContentRole,
    /// Stable provider order within the owning activity.
    pub order: u32,
    /// Complete readable text.
    pub text: String,
}

/// Complete durable semantic state for one provider-reported reasoning activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningActivity {
    /// Stable activity identifier scoped to the model turn.
    pub activity_id: String,
    /// Provider order within the model turn.
    pub order: u32,
    /// Terminal activity state.
    pub status: ReasoningActivityStatus,
    /// Ordered readable parts exposed by the provider.
    #[serde(default)]
    pub parts: Vec<ReasoningPart>,
    /// Whether the provider supplied opaque evidence of reasoning.
    ///
    /// This records only the fact that opaque state existed. Opaque bytes must never be copied
    /// into this model.
    #[serde(default)]
    pub opaque: bool,
}

/// Incremental provider-neutral reasoning operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningActivityEvent {
    /// A provider supplied evidence that a reasoning activity started.
    Started {
        /// Stable activity identifier scoped to the model turn.
        activity_id: String,
        /// Provider order within the model turn.
        order: u32,
    },
    /// Append readable text to one reasoning part.
    PartDelta {
        /// Stable activity identifier scoped to the model turn.
        activity_id: String,
        /// Provider order within the model turn.
        activity_order: u32,
        /// Stable part identifier within the activity.
        part_id: String,
        /// Whether this is summary, raw, or legacy untyped reasoning.
        kind: ReasoningContentKind,
        /// Provider-supplied semantic role when known.
        #[serde(default)]
        role: ReasoningContentRole,
        /// Stable provider order within the activity.
        part_order: u32,
        /// Nonempty incremental readable text.
        text: String,
    },
    /// Set authoritative complete text for one reasoning part.
    PartCompleted {
        /// Stable activity identifier scoped to the model turn.
        activity_id: String,
        /// Provider order within the model turn.
        activity_order: u32,
        /// Stable part identifier within the activity.
        part_id: String,
        /// Whether this is summary, raw, or legacy untyped reasoning.
        kind: ReasoningContentKind,
        /// Provider-supplied semantic role when known.
        #[serde(default)]
        role: ReasoningContentRole,
        /// Stable provider order within the activity.
        part_order: u32,
        /// Complete readable text. Empty text is valid when the provider completes an empty part.
        text: String,
    },
    /// Record opaque evidence without exposing provider-owned bytes.
    OpaqueObserved {
        /// Stable activity identifier scoped to the model turn.
        activity_id: String,
        /// Provider order within the model turn.
        activity_order: u32,
    },
    /// Finish a reasoning activity.
    Finished {
        /// Stable activity identifier scoped to the model turn.
        activity_id: String,
        /// Provider order within the model turn.
        activity_order: u32,
        /// Terminal activity state.
        status: ReasoningActivityStatus,
    },
}

impl ReasoningActivityEvent {
    /// Return the stable activity identifier.
    #[must_use]
    pub fn activity_id(&self) -> &str {
        match self {
            Self::Started { activity_id, .. }
            | Self::PartDelta { activity_id, .. }
            | Self::PartCompleted { activity_id, .. }
            | Self::OpaqueObserved { activity_id, .. }
            | Self::Finished { activity_id, .. } => activity_id,
        }
    }

    /// Return the stable provider order of the activity.
    #[must_use]
    pub const fn activity_order(&self) -> u32 {
        match self {
            Self::Started { order, .. } => *order,
            Self::PartDelta { activity_order, .. }
            | Self::PartCompleted { activity_order, .. }
            | Self::OpaqueObserved { activity_order, .. }
            | Self::Finished { activity_order, .. } => *activity_order,
        }
    }
}

/// Generation-scoped ordered text stream mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStreamUpdate {
    /// Stream generation. A replacement stream advances this value.
    pub generation: u64,
    /// First operation revision represented by this update.
    ///
    /// This equals `revision` for an unmerged operation and preserves contiguous revision spans
    /// when transport fan-out coalesces adjacent appends.
    pub first_revision: u64,
    /// Last monotonic operation revision represented within `generation`.
    pub revision: u64,
    /// Ordered mutation applied at this revision.
    pub operation: TextStreamOperation,
}

/// Ordered live-only text stream operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TextStreamOperation {
    /// Append UTF-8 text at the exact accepted byte offset.
    Append {
        /// Accepted byte count required before this append.
        expected_offset: usize,
        /// Contiguous UTF-8 text.
        text: String,
    },
    /// Replace local state with an authoritative bounded checkpoint.
    Checkpoint {
        /// Original stream offset represented by the first checkpoint byte.
        start_offset: usize,
        /// Bounded retained UTF-8 text.
        text: String,
        /// Total source bytes accepted by the producer.
        total_bytes: usize,
        /// Whether bytes before `start_offset` were omitted.
        truncated: bool,
    },
    /// Close the active stream. Terminal state is absorbing.
    Terminal {
        /// Terminal stream outcome.
        status: TextStreamTerminalStatus,
    },
}

/// Terminal outcome for an ordered live text stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStreamTerminalStatus {
    /// Complete durable content superseded the live stream.
    Completed,
    /// The owning turn was cancelled.
    Cancelled,
    /// The owning turn failed.
    Failed,
    /// A newer generation superseded the stream.
    Superseded,
}

/// Live-only session event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLiveEventKind {
    /// Ordered assistant segment update produced by a new application model turn.
    AssistantTextStreamUpdated {
        /// Cross-type semantic output position within the turn, when provided by a v2 provider.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_position: Option<TurnOutputPosition>,
        /// Application model turn that owns the segment.
        turn_id: String,
        /// Stable segment identifier scoped to `turn_id`.
        segment_id: String,
        /// Zero-based semantic order of the segment within the turn.
        segment_order: u32,
        /// Generation/revision/offset-validated stream operation.
        update: TextStreamUpdate,
    },
    /// Coalesced assistant text produced by an active model turn.
    ///
    /// Legacy compatibility adapter. New producers emit [`Self::AssistantTextStreamUpdated`].
    AssistantTextDelta {
        /// Application model turn that owns the segment.
        turn_id: String,
        /// Stable segment identifier scoped to `turn_id`.
        segment_id: String,
        /// Zero-based semantic order of the segment within the turn.
        segment_order: u32,
        /// Contiguous text appended to the segment.
        text: String,
    },
    /// Ordered readable text update for one structured reasoning part.
    AssistantReasoningTextStreamUpdated {
        /// Cross-type semantic output position within the turn, when provided by a v2 provider.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_position: Option<TurnOutputPosition>,
        /// Application model turn that owns the activity.
        turn_id: String,
        /// Stable reasoning activity identifier scoped to `turn_id`.
        activity_id: String,
        /// Stable activity order within the turn.
        activity_order: u32,
        /// Stable readable part identifier scoped to `activity_id`.
        part_id: String,
        /// Portable reasoning content kind.
        kind: ReasoningContentKind,
        /// Portable semantic role.
        role: ReasoningContentRole,
        /// Stable part order within the activity.
        part_order: u32,
        /// Generation/revision/offset-validated stream operation.
        update: TextStreamUpdate,
    },
    /// Coalesced provider-exposed reasoning text produced by an active model turn.
    ///
    /// Legacy compatibility adapter: the text has no representation kind, part identity, or
    /// lifecycle. New producers must emit [`Self::AssistantReasoningActivity`] or
    /// [`Self::AssistantReasoningTextStreamUpdated`].
    AssistantReasoningDelta { turn_id: String, text: String },
    /// Provider-neutral reasoning activity operation produced by an active model turn.
    AssistantReasoningActivity {
        /// Cross-type semantic output position within the turn, when provided by a v2 provider.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_position: Option<TurnOutputPosition>,
        /// Model turn associated with the activity.
        turn_id: String,
        /// Structured reasoning operation.
        event: ReasoningActivityEvent,
    },
    /// Renderer contribution with explicit placement published only to attached clients.
    ToolContributionPlaced { envelope: ToolContributionEnvelope },
    /// Invocation-owned current presentation replacement published only to attached clients.
    ToolPresentationUpdated { update: ToolPresentationUpdate },
    /// Authoritative current context occupancy after a durable projection update.
    RequestContextOccupancyChanged {
        /// Current occupancy, or `None` when a model/compaction boundary cleared it.
        occupancy: Box<Option<RequestContextOccupancy>>,
    },
    /// Live-only non-terminal progress for one active tool invocation.
    ///
    /// Only [`ToolInvocationLifecycleStage::Progress`] is valid here. Started, waiting, and
    /// terminal lifecycle facts remain canonical durable events.
    ToolInvocationProgress { event: ToolInvocationLifecycleEvent },
    /// Live-only provider stream progress for active model turns.
    ProviderStreamProgress {
        /// Model turn associated with this progress update.
        turn_id: String,
        /// Coalesced provider stream progress event.
        event: ProviderStreamEvent,
    },
    /// Live-only assembly state for one provider-emitted tool request.
    ///
    /// Draft fragments are observational presentation data. They are never valid for authorization
    /// or execution and are never persisted or replayed from durable history.
    ToolRequestDraft { event: ToolRequestDraftEvent },
}

const fn default_tool_request_draft_placement() -> ToolContributionPlacement {
    ToolContributionPlacement::Request
}

/// One live-only provider tool-request draft update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequestDraftEvent {
    /// Cross-type semantic output position within the turn, when provided by a v2 provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_position: Option<TurnOutputPosition>,
    /// Model turn that owns the draft generation.
    pub turn_id: String,
    /// Provider tool-call identifier.
    pub tool_call_id: String,
    /// Model-visible tool name.
    pub tool_name: String,
    /// Plugin that owns request-draft presentation, when resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_plugin_id: Option<String>,
    /// Plugin-owned request-draft schema.
    pub schema: String,
    /// Version of `schema` used by the draft payload.
    pub schema_version: u32,
    /// Semantic transcript slot updated by this draft.
    #[serde(default = "default_tool_request_draft_placement")]
    pub placement: ToolContributionPlacement,
    /// Monotonic draft generation. A new provider start for the same call advances it.
    pub generation: u64,
    /// Monotonic update revision within `generation`.
    pub revision: u64,
    /// Update operation applied to the bounded retained preview.
    pub operation: ToolRequestDraftOperation,
    /// Total argument bytes observed from the provider for this generation.
    pub argument_bytes: usize,
    /// Whether bytes were omitted from the bounded retained preview.
    pub truncated: bool,
}

/// Live-only request-draft mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolRequestDraftOperation {
    /// Append one contiguous byte batch at `offset`.
    Append {
        /// Expected retained-stream offset before applying `text`.
        offset: usize,
        /// Newly observed UTF-8 provider argument bytes.
        text: String,
    },
    /// Replace local draft state with a bounded checkpoint.
    Checkpoint {
        /// Original stream offset represented by the first retained byte.
        start_offset: usize,
        /// Bounded retained UTF-8 preview.
        text: String,
    },
    /// Remove the draft at a terminal boundary.
    Remove {
        /// Why the live-only draft ended.
        reason: ToolRequestDraftTerminalReason,
    },
}

/// Terminal reason for removing a live request draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRequestDraftTerminalReason {
    /// Complete arguments were accepted by the provider stream.
    Completed,
    /// The owning model turn was cancelled.
    Cancelled,
    /// Provider argument assembly failed or was invalid.
    Invalid,
    /// A newer generation superseded this draft.
    Superseded,
}

/// Session projection kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProjectionKind {
    /// Transcript conversation view.
    Transcript,
    /// User input history view.
    InputHistory,
    /// Runtime-work lifecycle view.
    RuntimeWork,
    /// Tool invocation timeline view.
    ToolTimeline,
    /// Audit-oriented chronological event view.
    AuditLog,
}

/// Stable source-event range covered by a projection item or window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSourceRange {
    /// First source event sequence included in the range.
    pub start_sequence: u64,
    /// Last source event sequence included in the range.
    pub end_sequence: u64,
}

/// Anchor point for a projection window query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionWindowAnchor {
    /// Start from the latest available projection content.
    Latest,
    /// Start before the item that covers the given source event sequence.
    BeforeSequence(u64),
    /// Start after the item that covers the given source event sequence.
    AfterSequence(u64),
    /// Center the window around the item that covers the given source event sequence.
    AroundSequence(u64),
}

/// Direction used when extending a projection window from its anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionWindowDirection {
    /// Select older content first.
    Backward,
    /// Select newer content first.
    Forward,
}

/// Semantic target for a projection window query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionWindowTarget {
    /// Minimum number of projection items to include when available.
    #[serde(default)]
    pub min_items: Option<usize>,
    /// Minimum estimated display rows to include when available.
    #[serde(default)]
    pub min_estimated_rows: Option<usize>,
    /// Minimum content bytes to include when available.
    #[serde(default)]
    pub min_bytes: Option<usize>,
    /// Width used by row estimation, when the caller has one.
    #[serde(default)]
    pub width_columns: Option<u16>,
}

/// Safety limits for bounded projection window queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionWindowLimits {
    /// Maximum projection items to return.
    pub max_items: usize,
    /// Maximum source events to scan while trying to satisfy the target.
    pub max_events_scanned: usize,
    /// Maximum content bytes to return.
    pub max_bytes: usize,
}

/// Request for a semantic window over a session projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionWindowRequest {
    /// Projection to query.
    pub projection: SessionProjectionKind,
    /// Anchor from which the window is selected.
    pub anchor: ProjectionWindowAnchor,
    /// Direction to extend the window from the anchor.
    pub direction: ProjectionWindowDirection,
    /// Desired semantic window size.
    pub target: ProjectionWindowTarget,
    /// Hard safety limits for the query.
    pub limits: ProjectionWindowLimits,
}

/// Semantic category for an item in the transcript projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptProjectionItemKind {
    /// User-authored message.
    UserMessage,
    /// Assistant-authored message.
    AssistantMessage,
    /// Assistant reasoning content.
    Reasoning,
    /// Tool invocation or tool output content.
    ToolInvocation,
    /// Permission request or resolution content.
    Permission,
    /// Context compaction marker or summary.
    ContextCompaction,
    /// Working-directory change marker.
    WorkingDirectoryChange,
    /// Other transcript-visible event group.
    Other,
}

/// Transcript projection item metadata returned by projection window queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptProjectionItem {
    /// Semantic item category.
    pub kind: TranscriptProjectionItemKind,
    /// Source events covered by this item.
    pub source_range: ProjectionSourceRange,
    /// Estimated display rows for this item at the requested width.
    #[serde(default)]
    pub estimated_rows: Option<usize>,
    /// Approximate content byte count represented by this item.
    pub content_bytes: usize,
}

/// Result of a projection window query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionWindow {
    /// Projection that produced the window.
    pub projection: SessionProjectionKind,
    /// Transcript items selected for the window.
    #[serde(default)]
    pub transcript_items: Vec<TranscriptProjectionItem>,
    /// Source range covered by the selected window.
    #[serde(default)]
    pub source_range: Option<ProjectionSourceRange>,
    /// Whether older projection content exists before this window.
    pub has_older: bool,
    /// Whether newer projection content exists after this window.
    pub has_newer: bool,
    /// Number of source events scanned to build this window.
    pub scanned_events: usize,
}

/// Typed semantic data returned by a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInvocationResult {
    /// Plain textual result.
    Text { text: String },
    /// Structured JSON result encoded as a JSON string for codec stability.
    Json { value: String },
    /// Opaque plugin artifact rendered by visual adapters.
    Artifact { artifact: Box<ToolArtifact> },
}

/// Durable renderer-neutral terminal result of one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationResultRecord {
    /// Invocation that produced this result.
    pub invocation_id: String,
    /// Model-visible text returned by the invocation.
    pub model_output: String,
    /// Whether the invocation reported an error.
    pub is_error: bool,
    /// Latest retained plugin-owned presentation checkpoint at the terminal boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<bcode_tool_models::ToolPresentationUpdate>,
    /// Optional typed semantic result supplied by the tool owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolInvocationResult>,
}

/// Opaque artifact produced by a tool plugin and rendered by visual adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArtifact {
    /// Stable artifact identifier within the session/tool call.
    pub artifact_id: String,
    /// Plugin that produced the artifact data.
    pub producer_plugin_id: String,
    /// Plugin-owned artifact schema identifier.
    pub schema: String,
    /// Artifact schema version.
    pub schema_version: u32,
    /// Tool call that produced the artifact, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Plugin-owned artifact metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
    /// Artifact byte/sidecar references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<ToolArtifactRef>,
}

/// Reference to plugin-owned artifact bytes or structured sidecar data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArtifactRef {
    /// Plugin-owned reference key.
    pub key: String,
    /// Media type of the referenced data, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Storage location for the referenced data, when externalized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_uri: Option<String>,
    /// Referenced data length in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_len: Option<u64>,
    /// Plugin-owned reference metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Model turn terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnOutcome {
    Completed,
    Cancelled,
    Error,
    IdleTimeout,
    ToolRoundLimitReached,
    ProviderUnavailable,
}

/// Provider-neutral token usage persisted with a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTokenUsage {
    /// Tokens supplied to the model for this turn or provider round.
    #[serde(default)]
    pub input_tokens: Option<u32>,
    /// Tokens generated by the model for this turn or provider round.
    #[serde(default)]
    pub output_tokens: Option<u32>,
    /// Provider-reported total tokens, when available.
    #[serde(default)]
    pub total_tokens: Option<u32>,
    /// Input tokens served from a provider cache, when available.
    #[serde(default)]
    pub cached_input_tokens: Option<u32>,
    /// Input tokens written to a provider prompt cache, when available.
    #[serde(default)]
    pub cache_write_input_tokens: Option<u32>,
    /// Reasoning tokens reported separately by a provider, when available.
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl SessionTokenUsage {
    /// Return the most reliable total token count for spend/session metering.
    #[must_use]
    pub fn metered_total_tokens(&self) -> Option<u32> {
        self.total_tokens.or_else(|| {
            let input = self.input_tokens.unwrap_or_default();
            let output = self.output_tokens.unwrap_or_default();
            (self.input_tokens.is_some() || self.output_tokens.is_some())
                .then_some(input.saturating_add(output))
        })
    }

    /// Return uncached input tokens when both input and cached counts are known.
    #[must_use]
    pub const fn uncached_input_tokens(&self) -> Option<u32> {
        match (self.input_tokens, self.cached_input_tokens) {
            (Some(input), Some(cached)) => Some(input.saturating_sub(cached)),
            _ => self.input_tokens,
        }
    }
}

/// Fine-grained diagnostic event persisted for session post-mortems.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTraceEvent {
    /// Milliseconds since the Unix epoch when this trace event was recorded.
    pub timestamp_ms: u64,
    /// Optional model turn associated with this trace event.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Diagnostic phase.
    pub phase: SessionTracePhase,
    /// Structured diagnostic payload.
    pub payload: SessionTracePayload,
}

/// Diagnostic phase for a [`SessionTraceEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTracePhase {
    ModelRequestBuilt,
    ModelProviderRoundStarted,
    ModelProviderRoundFinished,
    ModelProviderEvent,
    ToolInvocationStarted,
    ToolPolicyEvaluated,
    ToolPermissionWaitStarted,
    ToolPermissionWaitFinished,
    ToolInvocationFinished,
    SkillInvoked,
    SkillSuggested,
    SkillActivated,
    SkillDeactivated,
    SkillContextLoaded,
    SkillInvocationFailed,
    ContextCompactionSkipped,
    ContextCompactionStarted,
    ContextCompactionFinished,
    ToolInvocationOutput,
    /// Diagnostic compaction detail that does not represent lifecycle progress.
    ContextCompactionDiagnostic,
}

/// Structured model-provider streaming event for user-facing progress and debug correlation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamEvent {
    /// Provider turn started.
    TurnStarted,
    /// Provider started streaming a tool call.
    ToolCallStarted {
        /// Internal provider tool-call identifier for debugging and event correlation.
        tool_call_id: String,
        /// User-facing tool name.
        tool_name: String,
    },
    /// Provider assembled tool-call arguments.
    ToolCallProgress {
        /// Internal provider tool-call identifier for debugging and event correlation.
        tool_call_id: String,
        /// User-facing tool name.
        tool_name: String,
        /// Total assembled argument bytes received so far.
        argument_bytes: usize,
    },
    /// Provider finished a tool call.
    ToolCallFinished {
        /// Internal provider tool-call identifier for debugging and event correlation.
        tool_call_id: String,
        /// User-facing tool name.
        tool_name: String,
    },
    /// Provider stream has not produced meaningful progress for a warning threshold.
    NoProgressWarning {
        /// Seconds without meaningful provider progress.
        idle_seconds: u64,
        /// Active tool-call progress, when the provider was streaming tool arguments.
        active_tool_call: Option<ProviderToolCallProgress>,
    },
    /// Provider scheduled an automatic retry after a rate-limit/quota reset wait.
    RetryScheduled {
        /// User-facing retry message.
        message: String,
        /// Unix timestamp when retry should be attempted.
        retry_at_unix: u64,
    },
}

/// Structured provider tool-call argument progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderToolCallProgress {
    /// Internal provider tool-call identifier for debugging and event correlation.
    pub tool_call_id: String,
    /// User-facing tool name.
    pub tool_name: String,
    /// Total assembled argument bytes received so far.
    pub argument_bytes: usize,
}

/// Structured diagnostic payload for a [`SessionTraceEvent`].
///
/// IMPORTANT: This enum is persisted with `bmux_codec`, whose binary enum
/// representation is order-sensitive. Do not reorder existing variants or
/// insert new variants between existing ones. Add new variants only at the end,
/// and bump `CURRENT_SESSION_EVENT_SCHEMA_VERSION` when doing so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTracePayload {
    ModelRequestBuilt {
        provider: String,
        model: String,
        agent_id: String,
        message_count: usize,
        tool_count: usize,
        system_prompt_chars: usize,
        prompt_cache_mode: String,
        conversation_reuse_mode: String,
        uses_previous_provider_response: bool,
        metadata: BTreeMap<String, String>,
        request: Option<TraceBlobRef>,
    },
    ProviderRound {
        provider_turn_id: Option<String>,
        provider: String,
        round: Option<u32>,
        stop_reason: Option<String>,
        duration_ms: Option<u64>,
        error: Option<String>,
    },
    ProviderEvent {
        event_type: String,
        detail: Option<String>,
    },
    ToolInvocationStarted {
        tool_call_id: String,
        plugin_id: String,
        tool_name: String,
        side_effect: String,
        requires_permission: bool,
        arguments: Option<TraceBlobRef>,
    },
    ToolPolicyEvaluated {
        tool_call_id: String,
        agent_id: String,
        decision: String,
        reason: Option<String>,
    },
    ToolPermissionWait {
        permission_id: String,
        tool_call_id: String,
        approved: Option<bool>,
        duration_ms: Option<u64>,
    },
    ToolInvocationFinished {
        tool_call_id: String,
        duration_ms: u64,
        is_error: bool,
        output_bytes: usize,
        output: Option<TraceBlobRef>,
    },
    ContextCompaction {
        reason: String,
        projected_context_chars: usize,
        compacted: bool,
        message: Option<String>,
    },
    ProviderStreamEvent(ProviderStreamEvent),
}

/// Reference to a trace payload stored outside the main session event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceBlobRef {
    pub sha256: String,
    pub path: String,
    pub content_type: String,
    pub byte_len: u64,
    pub redaction: TraceRedaction,
    #[serde(default)]
    pub completeness: TraceBlobCompleteness,
}

/// Whether a trace blob represents complete or bounded retained content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceBlobCompleteness {
    /// The blob is the complete payload supplied to the trace store.
    #[default]
    Complete,
    /// The blob contains retained content, but the upstream tool or trace writer may have bounded it.
    Retained,
    /// The blob was truncated while being written by the trace store.
    Truncated,
}

/// Redaction status for a trace blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceRedaction {
    None,
    Automatic,
    ManualRequired,
}

/// Correlation metadata for one permission checkpoint in a simultaneous batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionBatchCorrelation {
    /// Identifier shared by every checkpoint in the batch.
    pub batch_id: String,
    /// Zero-based provider-order position.
    pub call_index: usize,
    /// Total checkpoints in the batch.
    pub call_count: usize,
}

/// Session event payload.
///
/// IMPORTANT: This enum is persisted with `bmux_codec`, whose binary enum
/// representation is order-sensitive. Do not reorder existing variants or
/// insert new variants between existing ones. Add new variants only at the end,
/// and bump `CURRENT_SESSION_EVENT_SCHEMA_VERSION` when doing so.
///
/// Reordering variants can make existing persisted `*.events` session files
/// decode as the wrong event type or fail daemon startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventKind {
    SessionCreated {
        name: Option<String>,
        #[serde(default)]
        working_directory: PathBuf,
    },
    ClientAttached {
        client_id: ClientId,
    },
    ClientDetached {
        client_id: ClientId,
    },
    UserMessage {
        client_id: ClientId,
        text: String,
        #[serde(default)]
        admission: TurnAdmissionMetadata,
    },
    AssistantDelta {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCallRequested {
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        producer_plugin_id: Option<String>,
        tool_name: String,
        arguments_json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_directory: Option<PathBuf>,
    },
    PermissionRequested {
        permission_id: String,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        producer_plugin_id: Option<String>,
        tool_name: String,
        arguments_json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        batch: Option<PermissionBatchCorrelation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_reason: Option<String>,
    },
    PermissionResolved {
        permission_id: String,
        approved: bool,
    },
    ModelChanged {
        provider: String,
        model: String,
    },
    SystemMessage {
        text: String,
    },
    AgentChanged {
        agent_id: String,
    },
    ModelTurnStarted {
        turn_id: String,
    },
    ModelTurnFinished {
        turn_id: String,
        outcome: ModelTurnOutcome,
        #[serde(default)]
        message: Option<String>,
    },
    ModelUsage {
        turn_id: String,
        usage: SessionTokenUsage,
    },
    ContextCompacted {
        summary: String,
        compacted_through_sequence: u64,
    },
    SessionRenamed {
        name: Option<String>,
    },
    TraceEvent {
        trace: Box<SessionTraceEvent>,
    },
    SkillInvoked {
        skill_id: SkillId,
        arguments: String,
        #[serde(default)]
        source: Option<SkillSource>,
        invoked_at_ms: u64,
    },
    SkillSuggested {
        skill_id: SkillId,
        #[serde(default)]
        reason: Option<String>,
        suggested_at_ms: u64,
    },
    SkillActivated {
        skill_id: SkillId,
        #[serde(default)]
        source: Option<SkillSource>,
        mode: SkillActivationMode,
        activated_at_ms: u64,
    },
    SkillDeactivated {
        skill_id: SkillId,
        deactivated_at_ms: u64,
    },
    SkillContextLoaded {
        skill_id: SkillId,
        bytes_loaded: usize,
        truncated: bool,
        loaded_at_ms: u64,
        #[serde(default)]
        source: Option<SkillSource>,
        #[serde(default)]
        preview: Option<String>,
    },
    SkillInvocationFailed {
        skill_id: SkillId,
        error: String,
        failed_at_ms: u64,
    },
    /// Provider-exposed reasoning text delta.
    ///
    /// Legacy persisted compatibility shape. It loses representation kind, part identity, order,
    /// and lifecycle; new sessions use [`Self::AssistantReasoningActivity`].
    AssistantReasoningDelta {
        text: String,
    },
    /// Completed provider-exposed reasoning text.
    ///
    /// Legacy persisted compatibility shape with the same structural loss as
    /// [`Self::AssistantReasoningDelta`].
    AssistantReasoningMessage {
        text: String,
    },
    /// Durable runtime work start marker.
    RuntimeWorkStarted {
        work_id: WorkId,
        kind: RuntimeWorkKind,
        label: String,
        #[serde(default)]
        tool_call_id: Option<String>,
        #[serde(default)]
        plugin_id: Option<String>,
        #[serde(default)]
        service_interface: Option<String>,
        #[serde(default)]
        operation: Option<String>,
        #[serde(default)]
        parent_work_id: Option<WorkId>,
        #[serde(default)]
        started_at_ms: Option<u64>,
        #[serde(default)]
        cancellable: bool,
    },
    /// Durable runtime work cancellation request marker.
    RuntimeWorkCancelRequested {
        work_id: WorkId,
        #[serde(default)]
        requested_at_ms: Option<u64>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Durable runtime work finish marker.
    RuntimeWorkFinished {
        work_id: WorkId,
        status: RuntimeWorkStatus,
        #[serde(default)]
        finished_at_ms: Option<u64>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Durable runtime work progress marker.
    RuntimeWorkProgress {
        work_id: WorkId,
        message: String,
        #[serde(default)]
        progress_at_ms: Option<u64>,
        #[serde(default)]
        completed_units: Option<u64>,
        #[serde(default)]
        total_units: Option<u64>,
    },
    /// Durable marker that a model turn cancellation was requested.
    ModelTurnCancelRequested {
        turn_id: String,
        #[serde(default)]
        requested_at_ms: Option<u64>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Durable marker that moves the session's canonical working directory.
    WorkingDirectoryChanged {
        old_working_directory: PathBuf,
        new_working_directory: PathBuf,
    },
    /// Durable provenance marker for sessions imported from external agents.
    SessionImported {
        source_id: String,
        source_display_name: String,
        external_session_id: String,
        imported_at_ms: u64,
    },
    /// Durable provenance marker for sessions forked or cloned from another session.
    SessionForked {
        source_session_id: SessionId,
        #[serde(default)]
        source_title: Option<String>,
        #[serde(default)]
        source_cutoff_sequence: Option<u64>,
        #[serde(default)]
        source_prompt_sequence: Option<u64>,
        forked_at_ms: u64,
        kind: SessionForkKind,
    },
    /// Durable marker for Ralph loop lifecycle events relevant to this session.
    RalphLifecycle {
        loop_name: String,
        state_dir: PathBuf,
        kind: String,
        message: String,
        occurred_at_ms: u64,
    },
    /// Durable session-specific model reasoning selection.
    ReasoningChanged {
        #[serde(default)]
        effort: Option<String>,
        #[serde(default)]
        summary: Option<String>,
    },
    /// Renderer-neutral exchange request emitted while an invocation remains active.
    ToolExchangeRequested {
        request: ToolExchangeRequest,
    },
    /// Terminal renderer-neutral exchange resolution.
    ToolExchangeResolved {
        event: ToolExchangeResolutionEvent,
    },
    /// Provider-native context installed at a durable compaction boundary.
    ProviderContextCompacted {
        snapshot: ProviderContextSnapshot,
        compacted_through_sequence: u64,
    },
    /// Exact or estimated context occupancy associated with a request boundary.
    RequestContextObserved {
        observation: RequestContextObservation,
    },
    /// Compact plugin-owned status note; presentation-only and excluded from model context.
    PluginStatusNote {
        plugin_id: String,
        note_id: String,
        text: String,
        #[serde(default)]
        metadata: BTreeMap<String, serde_json::Value>,
    },
    /// Semantically inert current history retained faithfully by migration.
    InertHistory {
        event_type: String,
        payload: serde_json::Value,
    },
    /// Renderer-neutral lifecycle for one admitted tool invocation.
    ToolInvocationLifecycle {
        event: ToolInvocationLifecycleEvent,
    },
    /// Opaque durable renderer contribution. Transient contributions are rejected by persistence.
    ToolContribution {
        event: ToolContributionEvent,
    },
    /// Durable renderer-neutral terminal result for one tool invocation.
    ToolInvocationResultRecorded {
        record: ToolInvocationResultRecord,
    },
    /// Versioned renderer contribution with explicit host composition semantics.
    ToolContributionPlaced {
        envelope: ToolContributionEnvelope,
    },
    /// Durable provenance for a background execution session.
    ExecutionSessionCreated {
        provenance: Box<ExecutionSessionProvenance>,
        visibility: SessionVisibility,
    },
    /// Complete durable semantic state for one provider-reported reasoning activity.
    AssistantReasoningActivity {
        /// Model turn that owns the activity.
        turn_id: String,
        /// Complete terminal reasoning activity.
        activity: ReasoningActivity,
    },
    /// Complete durable assistant response segment with stable turn-local identity.
    AssistantResponseSegment {
        /// Application model turn that owns the segment.
        turn_id: String,
        /// Stable segment identifier scoped to `turn_id`.
        segment_id: String,
        /// Zero-based semantic order of the segment within the turn.
        segment_order: u32,
        /// Complete visible assistant text.
        text: String,
    },
    /// Complete durable assistant response segment with provider-authoritative cross-type position.
    PositionedAssistantResponseSegment {
        /// Application model turn that owns the segment.
        turn_id: String,
        /// Stable cross-type position within the turn.
        output_position: TurnOutputPosition,
        /// Stable segment identifier scoped to `turn_id`.
        segment_id: String,
        /// Zero-based assistant segment order within the turn.
        segment_order: u32,
        /// Complete visible assistant text.
        text: String,
    },
    /// Complete durable reasoning activity with provider-authoritative cross-type position.
    PositionedAssistantReasoningActivity {
        /// Application model turn that owns the activity.
        turn_id: String,
        /// Stable cross-type position within the turn.
        output_position: TurnOutputPosition,
        /// Complete terminal reasoning activity.
        activity: ReasoningActivity,
    },
    /// Durable tool request with provider-authoritative cross-type position.
    PositionedToolCallRequested {
        /// Application model turn that owns the request.
        turn_id: String,
        /// Stable cross-type position within the turn.
        output_position: TurnOutputPosition,
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Plugin that owns the tool, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        producer_plugin_id: Option<String>,
        /// Model-visible tool name.
        tool_name: String,
        /// Complete JSON arguments.
        arguments_json: String,
        /// Working directory captured for execution, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_directory: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_live_segment_identity_round_trips() {
        let event = SessionLiveEventKind::AssistantTextDelta {
            turn_id: "turn-1".to_owned(),
            segment_id: "segment-2".to_owned(),
            segment_order: 2,
            text: "answer".to_owned(),
        };
        let encoded = serde_json::to_value(&event).expect("live event should serialize");
        let decoded: SessionLiveEventKind =
            serde_json::from_value(encoded).expect("live event should deserialize");

        assert_eq!(decoded, event);
    }

    #[test]
    fn all_reasoning_activity_events_round_trip_without_provider_payload_fields() {
        let events = [
            ReasoningActivityEvent::Started {
                activity_id: "reasoning-1".to_owned(),
                order: 2,
            },
            ReasoningActivityEvent::PartDelta {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 2,
                part_id: "summary-0".to_owned(),
                kind: ReasoningContentKind::Summary,
                role: ReasoningContentRole::Milestone,
                part_order: 0,
                text: "Planning".to_owned(),
            },
            ReasoningActivityEvent::PartCompleted {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 2,
                part_id: "raw-0".to_owned(),
                kind: ReasoningContentKind::Raw,
                role: ReasoningContentRole::Detail,
                part_order: 1,
                text: "Complete detail".to_owned(),
            },
            ReasoningActivityEvent::OpaqueObserved {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 2,
            },
            ReasoningActivityEvent::Finished {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 2,
                status: ReasoningActivityStatus::Interrupted,
            },
        ];

        for event in events {
            let encoded = serde_json::to_string(&event).expect("reasoning event should encode");
            let decoded = serde_json::from_str::<ReasoningActivityEvent>(&encoded)
                .expect("reasoning event should decode");
            assert_eq!(decoded, event);
            assert_eq!(decoded.activity_id(), "reasoning-1");
            assert_eq!(decoded.activity_order(), 2);
            assert!(!encoded.contains("encrypted_content"));
            assert!(!encoded.contains("provider_state"));
        }
    }

    #[test]
    fn opaque_reasoning_evidence_has_no_provider_payload_metadata() {
        let sentinel = "encrypted-sentinel-do-not-expose";
        let event = ReasoningActivityEvent::OpaqueObserved {
            activity_id: "reasoning-1".to_owned(),
            activity_order: 7,
        };
        let encoded = serde_json::to_string(&event).expect("serialize opaque evidence");
        assert!(encoded.contains(r#""type":"opaque_observed""#));
        assert!(encoded.contains(r#""activity_id":"reasoning-1""#));
        assert!(!encoded.contains(sentinel));
        for forbidden in [
            "encrypted_content",
            "provider_state",
            "payload",
            "length",
            "hash",
            "prefix",
            "secret",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "unexpected field: {forbidden}"
            );
        }
    }

    #[test]
    fn reasoning_activity_event_round_trips_without_opaque_bytes() {
        let event = ReasoningActivityEvent::PartCompleted {
            activity_id: "reasoning-1".to_owned(),
            activity_order: 2,
            part_id: "summary-0".to_owned(),
            kind: ReasoningContentKind::Summary,
            role: ReasoningContentRole::Milestone,
            part_order: 0,
            text: "Planning the change".to_owned(),
        };

        let encoded = serde_json::to_string(&event).expect("reasoning event should encode");
        let decoded = serde_json::from_str::<ReasoningActivityEvent>(&encoded)
            .expect("reasoning event should decode");

        assert_eq!(decoded, event);
        assert_eq!(decoded.activity_id(), "reasoning-1");
        assert_eq!(decoded.activity_order(), 2);
        assert!(!encoded.contains("encrypted_content"));
    }

    #[test]
    fn tool_request_draft_models_round_trip_all_operations() {
        let operations = [
            ToolRequestDraftOperation::Append {
                offset: 4,
                text: "hello".to_owned(),
            },
            ToolRequestDraftOperation::Checkpoint {
                start_offset: 2,
                text: "bounded".to_owned(),
            },
            ToolRequestDraftOperation::Remove {
                reason: ToolRequestDraftTerminalReason::Cancelled,
            },
        ];
        for (index, operation) in operations.into_iter().enumerate() {
            let event = ToolRequestDraftEvent {
                output_position: None,
                turn_id: "turn-1".to_owned(),
                tool_call_id: "call-1".to_owned(),
                tool_name: "filesystem.write".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                schema: "bcode.filesystem.request-draft.write".to_owned(),
                schema_version: 1,
                placement: ToolContributionPlacement::Request,
                generation: 1,
                revision: u64::try_from(index + 1).expect("revision"),
                operation,
                argument_bytes: 9,
                truncated: index == 1,
            };
            let encoded = serde_json::to_vec(&event).expect("encode request draft");
            let decoded: ToolRequestDraftEvent =
                serde_json::from_slice(&encoded).expect("decode request draft");
            assert_eq!(decoded, event);

            let live = SessionLiveEvent {
                session_id: SessionId::new(),
                kind: SessionLiveEventKind::ToolRequestDraft {
                    event: event.clone(),
                },
            };
            let encoded = serde_json::to_vec(&live).expect("encode live request draft");
            let decoded: SessionLiveEvent =
                serde_json::from_slice(&encoded).expect("decode live request draft");
            assert_eq!(decoded, live);
        }
    }

    #[test]
    fn legacy_tool_request_draft_defaults_to_request_placement() {
        let decoded: ToolRequestDraftEvent = serde_json::from_value(serde_json::json!({
            "turn_id": "turn-1",
            "tool_call_id": "call-1",
            "tool_name": "third_party.tool",
            "schema": "third-party.draft",
            "schema_version": 1,
            "generation": 1,
            "revision": 1,
            "operation": {"type": "checkpoint", "start_offset": 0, "text": "{}"},
            "argument_bytes": 2,
            "truncated": false
        }))
        .expect("legacy request draft should decode");

        assert_eq!(decoded.placement, ToolContributionPlacement::Request);
    }

    #[test]
    fn tool_request_draft_models_reject_malformed_operations() {
        let malformed = [
            serde_json::json!({
                "turn_id": "turn-1",
                "tool_call_id": "call-1",
                "tool_name": "filesystem.write",
                "schema": "bcode.filesystem.request-draft.write",
                "schema_version": 1,
                "generation": 1,
                "revision": 1,
                "argument_bytes": 0,
                "truncated": false
            }),
            serde_json::json!({
                "turn_id": "turn-1",
                "tool_call_id": "call-1",
                "tool_name": "filesystem.write",
                "schema": "bcode.filesystem.request-draft.write",
                "schema_version": 1,
                "generation": 1,
                "revision": 1,
                "operation": {"type": "append", "offset": "not-a-number", "text": "x"},
                "argument_bytes": 1,
                "truncated": false
            }),
            serde_json::json!({
                "turn_id": "turn-1",
                "tool_call_id": "call-1",
                "tool_name": "filesystem.write",
                "schema": "bcode.filesystem.request-draft.write",
                "schema_version": 1,
                "generation": 1,
                "revision": 1,
                "operation": {"type": "unknown"},
                "argument_bytes": 0,
                "truncated": false
            }),
        ];

        for value in malformed {
            assert!(serde_json::from_value::<ToolRequestDraftEvent>(value).is_err());
        }
    }

    #[test]
    fn session_open_operation_models_round_trip_and_preserve_semantics() {
        let snapshot = SessionOpenOperationSnapshot {
            operation_id: SessionOpenOperationId::new(),
            revision: 7,
            session_id: SessionId::new(),
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage: SessionMigrationStage::RebuildingProjections,
                completed_units: Some(16_742),
                total_units: Some(53_652),
                unit: Some(SessionMigrationProgressUnit::Events),
                message: "Rebuilding session indexes".to_owned(),
            },
            outcome: Some(SessionOpenTerminalOutcome::Failed {
                kind: SessionOpenFailureKind::MigrationFailed,
                message: "projection replay failed".to_owned(),
                backup_path: Some(PathBuf::from("/tmp/backup")),
            }),
            backup_path: Some(PathBuf::from("/tmp/backup")),
        };

        let encoded = serde_json::to_vec(&snapshot).expect("snapshot should encode");
        let decoded: SessionOpenOperationSnapshot =
            serde_json::from_slice(&encoded).expect("snapshot should decode");

        assert_eq!(decoded, snapshot);
        assert!(
            SessionMigrationStage::WaitingForOwnership
                < SessionMigrationStage::RebuildingProjections
        );
        assert!(SessionMigrationStage::RebuildingProjections < SessionMigrationStage::Complete);
        assert_eq!(
            snapshot.progress.unit,
            Some(SessionMigrationProgressUnit::Events)
        );
        assert!(
            snapshot.progress.completed_units.expect("completed")
                <= snapshot.progress.total_units.expect("total")
        );
    }

    #[test]
    fn turn_receipt_derives_the_existing_model_work_identity() {
        let session_id = SessionId::from_str("00000000-0000-0000-0000-000000000001")
            .expect("session id should parse");
        let receipt = TurnReceipt::from_accepted_event(session_id, 42);

        assert_eq!(receipt.turn_id, TurnId::from_accepted_event(session_id, 42));
        assert_eq!(receipt.turn_id.to_string(), format!("{session_id}-42"));
        assert_eq!(
            receipt.work_id,
            WorkId::new(format!("model_{session_id}-42"))
        );
        assert_eq!(receipt.accepted_event_sequence, 42);
    }

    #[test]
    fn turn_tool_policy_serializes_all_generic_modes() {
        for policy in [
            TurnToolPolicy::Enabled,
            TurnToolPolicy::ReadOnly,
            TurnToolPolicy::Disabled,
        ] {
            let encoded = serde_json::to_string(&policy).expect("policy should encode");
            let decoded: TurnToolPolicy =
                serde_json::from_str(&encoded).expect("policy should decode");
            assert_eq!(decoded, policy);
        }
    }

    #[test]
    fn turn_admission_metadata_defaults_to_tools_enabled() {
        let metadata = TurnAdmissionMetadata::default();

        assert_eq!(metadata.origin, None);
        assert_eq!(metadata.priority, TurnPriority::Interactive);
        assert_eq!(metadata.idempotency_key, None);
        assert_eq!(metadata.execution.tools, TurnToolPolicy::Enabled);
        assert_eq!(
            metadata.execution.schema_version,
            TURN_EXECUTION_OPTIONS_SCHEMA_VERSION
        );
        assert_eq!(metadata.execution.agent_profile, None);
        assert_eq!(metadata.execution.correlation, None);
        assert_eq!(metadata.execution.tool_allowlist, None);
        assert_eq!(metadata.execution.provider_plugin_id, None);
        assert_eq!(metadata.execution.model_id, None);
        assert_eq!(metadata.execution.structured_output, None);
        assert_eq!(metadata.validate(), Ok(()));
    }

    #[test]
    fn turn_execution_overrides_round_trip_and_validate() {
        let metadata = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                tools: TurnToolPolicy::ReadOnly,
                correlation: Some(TurnExecutionCorrelation {
                    execution_id: "run-1".to_string(),
                    unit_id: "review".to_string(),
                    attempt: 1,
                }),
                agent_profile: Some("review".to_string()),
                tool_allowlist: Some(vec!["filesystem.read".to_string(), "git.diff".to_string()]),
                provider_plugin_id: Some("bcode.fake-provider".to_string()),
                model_id: Some("fake-structured".to_string()),
                reasoning: Some(Box::new(TurnReasoningOptions {
                    effort: Some("high".to_string()),
                    summary: Some("detailed".to_string()),
                })),
                structured_output: Some(TurnStructuredOutputRequest {
                    name: "review_result".to_string(),
                    schema: serde_json::json!({
                        "type": "object",
                        "properties": {"approved": {"type": "boolean"}},
                        "required": ["approved"]
                    }),
                    strict: true,
                }),
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };

        assert_eq!(metadata.validate(), Ok(()));
        let encoded = serde_json::to_string(&metadata).expect("metadata should encode");
        let decoded: TurnAdmissionMetadata =
            serde_json::from_str(&encoded).expect("metadata should decode");
        assert_eq!(decoded, metadata);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn turn_execution_overrides_reject_empty_and_duplicate_values() {
        let empty_profile = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                agent_profile: Some(String::new()),
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            empty_profile.validate(),
            Err(TurnAdmissionMetadataError::EmptyAgentProfile)
        );

        let empty_effort = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                reasoning: Some(Box::new(TurnReasoningOptions {
                    effort: Some(String::new()),
                    summary: None,
                })),
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            empty_effort.validate(),
            Err(TurnAdmissionMetadataError::EmptyReasoningEffort)
        );

        let duplicate_tool = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                tool_allowlist: Some(vec!["git.diff".to_string(), "git.diff".to_string()]),
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            duplicate_tool.validate(),
            Err(TurnAdmissionMetadataError::DuplicateAllowedTool)
        );

        let invalid_correlation = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                correlation: Some(TurnExecutionCorrelation {
                    execution_id: "run-1".to_string(),
                    unit_id: "review".to_string(),
                    attempt: 0,
                }),
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            invalid_correlation.validate(),
            Err(TurnAdmissionMetadataError::InvalidExecutionAttempt)
        );

        let unsupported_version = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                schema_version: TURN_EXECUTION_OPTIONS_SCHEMA_VERSION + 1,
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            unsupported_version.validate(),
            Err(TurnAdmissionMetadataError::UnsupportedExecutionOptionsVersion)
        );

        let legacy_version = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                schema_version: MIN_TURN_EXECUTION_OPTIONS_SCHEMA_VERSION,
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(legacy_version.validate(), Ok(()));

        let legacy_reasoning = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                schema_version: MIN_TURN_EXECUTION_OPTIONS_SCHEMA_VERSION,
                reasoning: Some(Box::new(TurnReasoningOptions {
                    effort: Some("high".to_owned()),
                    summary: None,
                })),
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            legacy_reasoning.validate(),
            Err(TurnAdmissionMetadataError::UnsupportedExecutionOptionsVersion)
        );

        let invalid_name = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                structured_output: Some(TurnStructuredOutputRequest {
                    name: "crate::Result".to_string(),
                    schema: serde_json::json!({"type": "object"}),
                    strict: false,
                }),
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            invalid_name.validate(),
            Err(TurnAdmissionMetadataError::InvalidStructuredOutputName)
        );

        let invalid_schema = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                structured_output: Some(TurnStructuredOutputRequest {
                    name: "result".to_string(),
                    schema: serde_json::Value::Bool(true),
                    strict: false,
                }),
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            invalid_schema.validate(),
            Err(TurnAdmissionMetadataError::InvalidStructuredOutputSchema)
        );
    }

    #[test]
    fn structured_output_names_are_provider_portable_and_bounded() {
        assert!(is_valid_structured_output_name("loop_iteration-v1"));
        assert!(!is_valid_structured_output_name("crate::Result"));
        assert_eq!(
            structured_output_name("bcode_loop_plugin::LoopWorkflowIteration"),
            "bcode_loop_plugin__LoopWorkflowIteration"
        );
        assert_eq!(structured_output_name("::"), "__");
        assert_eq!(structured_output_name(""), "structured_output");
        let first = structured_output_name(&format!("{}a", "x".repeat(100)));
        let second = structured_output_name(&format!("{}b", "x".repeat(100)));
        assert!(first.len() <= 64);
        assert!(is_valid_structured_output_name(&first));
        assert_ne!(first, second);
    }

    #[test]
    fn idempotency_requires_bounded_nonempty_producer_and_key() {
        let metadata = TurnAdmissionMetadata {
            origin: Some(TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: None,
                display_label: None,
            }),
            idempotency_key: Some("operation-1".to_string()),
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            metadata.idempotency_identity(),
            Some(("test.producer", "operation-1"))
        );
        assert_eq!(metadata.validate(), Ok(()));

        let missing_producer = TurnAdmissionMetadata {
            idempotency_key: Some("operation-1".to_string()),
            ..TurnAdmissionMetadata::default()
        };
        assert_eq!(
            missing_producer.validate(),
            Err(TurnAdmissionMetadataError::MissingIdempotencyProducer)
        );
    }

    #[test]
    fn work_id_remains_a_transparent_serialized_identifier() {
        let work_id = WorkId::new("work-1");

        assert_eq!(
            serde_json::to_string(&work_id).expect("work id should serialize"),
            r#""work-1""#
        );
        assert_eq!(
            serde_json::from_str::<WorkId>(r#""work-1""#).expect("work id should deserialize"),
            work_id
        );
    }

    fn invocation(request_id: &str, context_epoch: u64) -> ModelRequestIdentity {
        ModelRequestIdentity {
            provider_plugin_id: "provider".to_string(),
            requested_model_id: Some("alias".to_string()),
            effective_model_id: "model".to_string(),
            request_id: request_id.to_string(),
            model_turn_id: "turn".to_string(),
            round: 0,
            request_fingerprint: format!("fingerprint-{request_id}"),
            effective_auth_profile: Some("openai-2".to_string()),
            context_format_version: None,
            compatibility_key: None,
            context_epoch,
        }
    }

    const fn estimate(tokens: u64, algorithm_version: u16) -> LocalContextEstimate {
        LocalContextEstimate {
            tokens,
            algorithm_version,
        }
    }

    #[test]
    fn terminal_projection_is_absorbing_for_late_lifecycle_and_result_events() {
        let session_id = SessionId::new();
        let lifecycle = |sequence, stage| SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence.saturating_mul(100),
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolInvocationLifecycle {
                event: ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence,
                    stage,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        };
        let mut projections = BTreeMap::new();
        apply_tool_invocation_projection_event(
            &mut projections,
            &lifecycle(1, ToolInvocationLifecycleStage::Started),
        );
        apply_tool_invocation_projection_event(
            &mut projections,
            &lifecycle(2, ToolInvocationLifecycleStage::Cancelled),
        );
        apply_tool_invocation_projection_event(
            &mut projections,
            &lifecycle(3, ToolInvocationLifecycleStage::Progress),
        );
        apply_tool_invocation_projection_event(
            &mut projections,
            &SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 4,
                timestamp_ms: 400,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolInvocationResultRecorded {
                    record: ToolInvocationResultRecord {
                        invocation_id: "call-1".to_owned(),
                        model_output: "late".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: Some(ToolInvocationResult::Text {
                            text: "late".to_owned(),
                        }),
                    },
                },
            },
        );

        let projection = &projections["call-1"];
        assert_eq!(projection.status, ToolInvocationProjectionStatus::Cancelled);
        assert_eq!(projection.finished_at_ms, Some(200));
        assert_eq!(projection.result_text.as_deref(), Some("late"));
    }

    #[test]
    fn terminal_lifecycle_prefers_authoritative_duration_metadata() {
        let session_id = SessionId::new();
        let event = |sequence, timestamp_ms, stage, metadata| SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolInvocationLifecycle {
                event: ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence,
                    stage,
                    message: None,
                    metadata,
                },
            },
        };
        let projections = build_tool_invocation_projections(&[
            event(
                1,
                1_000,
                ToolInvocationLifecycleStage::Started,
                serde_json::Value::Null,
            ),
            event(
                2,
                9_000,
                ToolInvocationLifecycleStage::Completed,
                serde_json::json!({"duration_ms": 2500}),
            ),
        ]);

        assert_eq!(projections[0].finished_at_ms, Some(9_000));
        assert_eq!(projections[0].duration_ms, Some(2_500));
    }

    #[test]
    fn tool_invocation_projection_preserves_waiting_state() {
        let session_id = SessionId::new();
        let event = |sequence, stage| SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence.saturating_mul(100),
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolInvocationLifecycle {
                event: ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence,
                    stage,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        };
        let projections = build_tool_invocation_projections(&[
            event(1, ToolInvocationLifecycleStage::Started),
            event(2, ToolInvocationLifecycleStage::Waiting),
        ]);

        assert_eq!(projections.len(), 1);
        assert_eq!(
            projections[0].status,
            ToolInvocationProjectionStatus::Waiting
        );
        assert_eq!(projections[0].started_at_ms, Some(100));
        assert_eq!(projections[0].finished_at_ms, None);
    }

    #[test]
    fn context_estimate_calibrates_from_compatible_anchor() {
        let projected = RequestContextOccupancy::project_estimate(
            None,
            invocation("one", 3),
            1,
            estimate(42, 1),
        );
        let current = RequestContextOccupancy::reconcile(None, 3, 4, projected.clone())
            .expect("estimate should establish occupancy");
        let exact = RequestContextObservation {
            context_tokens: RequestContextTokenCount::ProviderExact(40),
            ..projected
        };
        let confirmed = RequestContextOccupancy::reconcile(Some(&current), 3, 5, exact)
            .expect("same request should confirm occupancy");
        let next = RequestContextOccupancy::project_estimate(
            Some(&confirmed),
            invocation("two", 3),
            2,
            estimate(52, 1),
        );

        assert_eq!(confirmed.observation.context_tokens.tokens(), 40);
        assert_eq!(next.context_tokens.tokens(), 50);
    }

    #[test]
    fn exact_context_observation_requires_matching_request_surface_and_epoch() {
        let projected = RequestContextOccupancy::project_estimate(
            None,
            invocation("request", 3),
            7,
            estimate(80, 1),
        );
        let current = RequestContextOccupancy::reconcile(None, 3, 8, projected.clone())
            .expect("estimate should establish occupancy");
        let exact = RequestContextObservation {
            context_tokens: RequestContextTokenCount::ProviderExact(75),
            ..projected
        };

        for mutate in [
            |request: &mut ModelRequestIdentity| request.provider_plugin_id = "other".to_string(),
            |request: &mut ModelRequestIdentity| request.effective_model_id = "other".to_string(),
            |request: &mut ModelRequestIdentity| {
                request.effective_auth_profile = Some("other".to_string());
            },
            |request: &mut ModelRequestIdentity| {
                request.request_fingerprint = "other".to_string();
            },
            |request: &mut ModelRequestIdentity| request.context_epoch = 4,
        ] {
            let mut mismatched = exact.clone();
            mutate(&mut mismatched.request);
            assert_eq!(
                RequestContextOccupancy::reconcile(Some(&current), 3, 9, mismatched),
                Some(current.clone())
            );
        }

        let accepted = RequestContextOccupancy::reconcile(Some(&current), 3, 9, exact)
            .expect("matching exact observation should be accepted");
        assert_eq!(
            accepted.observation.context_tokens,
            RequestContextTokenCount::ProviderExact(75)
        );
    }

    #[test]
    fn compaction_epoch_invalidates_prior_exact_context_anchor() {
        let projected = RequestContextOccupancy::project_estimate(
            None,
            invocation("request", 3),
            7,
            estimate(80, 1),
        );
        let current = RequestContextOccupancy::reconcile(None, 3, 8, projected.clone())
            .expect("estimate should establish occupancy");
        let exact = RequestContextObservation {
            context_tokens: RequestContextTokenCount::ProviderExact(75),
            ..projected
        };

        assert_eq!(
            RequestContextOccupancy::reconcile(Some(&current), 4, 9, exact),
            Some(current)
        );
    }

    #[test]
    fn estimator_version_change_disables_calibration() {
        let anchor = RequestContextOccupancy {
            context_epoch: 3,
            observation_sequence: 5,
            observation: RequestContextObservation {
                request: invocation("one", 3),
                context_through_sequence: 1,
                context_tokens: RequestContextTokenCount::ProviderExact(100),
                local_estimate: estimate(120, 1),
            },
        };
        let projected = RequestContextOccupancy::project_estimate(
            Some(&anchor),
            invocation("two", 3),
            2,
            estimate(90, 2),
        );

        assert_eq!(projected.context_tokens.tokens(), 90);
    }

    #[test]
    fn context_estimate_supports_negative_delta() {
        let anchor = RequestContextOccupancy {
            context_epoch: 3,
            observation_sequence: 5,
            observation: RequestContextObservation {
                request: invocation("one", 3),
                context_through_sequence: 1,
                context_tokens: RequestContextTokenCount::ProviderExact(100),
                local_estimate: estimate(120, 1),
            },
        };
        let projected = RequestContextOccupancy::project_estimate(
            Some(&anchor),
            invocation("two", 3),
            2,
            estimate(90, 1),
        );

        assert_eq!(projected.context_tokens.tokens(), 70);
    }

    #[test]
    fn execution_session_provenance_rejects_unknown_future_versions() {
        let parent_session_id = SessionId::new();
        let encoded = serde_json::json!({
            "version": EXECUTION_SESSION_PROVENANCE_VERSION + 1,
            "owner": "workflow",
            "run_id": "run-1",
            "node_id": "review",
            "activation_id": "activation-1",
            "attempt": 1,
            "parent_session_id": parent_session_id,
            "context_mode": "fresh_isolated",
            "workspace_snapshot": "snapshot-1"
        });
        let provenance: ExecutionSessionProvenance =
            serde_json::from_value(encoded).expect("portable future provenance");
        assert_eq!(provenance.version, EXECUTION_SESSION_PROVENANCE_VERSION + 1);
    }

    #[test]
    fn semantic_tool_result_json_decodes_current_shapes() {
        for (payload, expected) in semantic_tool_result_fixtures() {
            let decoded: ToolInvocationResult =
                serde_json::from_str(payload).expect("semantic result should decode");

            assert_eq!(decoded, expected);
        }
    }

    fn semantic_tool_result_fixtures() -> Vec<(&'static str, ToolInvocationResult)> {
        vec![
            (
                r#"{"type":"text","text":"plain text"}"#,
                ToolInvocationResult::Text {
                    text: "plain text".to_string(),
                },
            ),
            (
                r#"{"type":"json","value":"{\"ok\":true}"}"#,
                ToolInvocationResult::Json {
                    value: r#"{"ok":true}"#.to_string(),
                },
            ),
            (
                r#"{"type":"artifact","artifact":{"artifact_id":"artifact-1","producer_plugin_id":"bcode.test","schema":"bcode.test.artifact","schema_version":1,"tool_call_id":"call-1","title":"Test artifact","metadata":{"ok":true},"refs":[{"key":"data","content_type":"application/json","byte_len":11}]}}"#,
                ToolInvocationResult::Artifact {
                    artifact: Box::new(ToolArtifact {
                        artifact_id: "artifact-1".to_string(),
                        producer_plugin_id: "bcode.test".to_string(),
                        schema: "bcode.test.artifact".to_string(),
                        schema_version: 1,
                        tool_call_id: Some("call-1".to_string()),
                        title: Some("Test artifact".to_string()),
                        metadata: serde_json::json!({"ok": true}),
                        refs: vec![ToolArtifactRef {
                            key: "data".to_string(),
                            content_type: Some("application/json".to_string()),
                            storage_uri: None,
                            byte_len: Some(11),
                            metadata: None,
                        }],
                    }),
                },
            ),
        ]
    }
}
