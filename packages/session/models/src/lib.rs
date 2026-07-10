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

use bcode_skill_models::{SkillActivationMode, SkillId, SkillSource};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

mod context_management;
pub use context_management::{
    ContextUsageSnapshot, ContextUsageSource, ProviderContextSnapshot,
    ProviderContextSnapshotOrigin,
};

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
    /// Plugin-owned request visual reconstructed at request time.
    pub request_visual: Option<PluginVisualDescriptor>,
    /// Current lifecycle status.
    pub status: ToolInvocationProjectionStatus,
    /// Raw final text result returned by the tool, when finished.
    pub result_text: Option<String>,
    /// Whether the final tool result was an error.
    pub is_error: Option<bool>,
    /// Raw semantic result returned by the tool.
    pub raw_result: Option<ToolInvocationResult>,
    /// Raw terminal/text stream output observed for the tool.
    pub terminal_output: Option<ToolInvocationProjectionTerminalOutput>,
    /// Legacy persisted presentation events, retained only as compatibility facts.
    pub legacy_presentations: Vec<LegacyToolPresentationEvent>,
    /// Tool start time as UNIX epoch milliseconds, when known.
    pub started_at_ms: Option<u64>,
    /// Tool finish time as UNIX epoch milliseconds, when known.
    pub finished_at_ms: Option<u64>,
}

/// Renderer-neutral tool invocation lifecycle status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolInvocationProjectionStatus {
    /// Request was observed but no stream/final result has been seen.
    #[default]
    Requested,
    /// Stream lifecycle/output was observed.
    Running,
    /// Final result was observed.
    Finished,
}

/// Renderer-neutral raw stream output captured for a tool invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolInvocationProjectionTerminalOutput {
    /// Raw stream output text.
    pub output: String,
    /// Terminal columns reported by the producer.
    pub columns: Option<u16>,
    /// Terminal rows reported by the producer.
    pub rows: Option<u16>,
}

/// Build renderer-neutral tool invocation projections from chronological session events.
#[must_use]
pub fn build_tool_invocation_projections(events: &[SessionEvent]) -> Vec<ToolInvocationProjection> {
    let mut projections = BTreeMap::new();
    for event in events {
        apply_tool_invocation_projection_event(&mut projections, event);
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
            request_visual,
            ..
        } => {
            let projection = tool_invocation_projection_mut(projections, tool_call_id);
            projection.producer_plugin_id.clone_from(producer_plugin_id);
            projection.tool_name = Some(tool_name.clone());
            projection.arguments_json = Some(arguments_json.clone());
            projection.request_visual.clone_from(request_visual);
        }
        SessionEventKind::ToolInvocationStream { event } => {
            apply_tool_invocation_stream_projection_event(projections, event);
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
            semantic_result,
            ..
        } => {
            let projection = tool_invocation_projection_mut(projections, tool_call_id);
            projection.status = ToolInvocationProjectionStatus::Finished;
            projection.result_text = Some(result.clone());
            projection.is_error = Some(*is_error);
            projection.raw_result.clone_from(semantic_result);
        }
        _ => {}
    }
}

fn apply_tool_invocation_stream_projection_event(
    projections: &mut BTreeMap<String, ToolInvocationProjection>,
    event: &ToolInvocationStreamEvent,
) {
    let tool_call_id = tool_projection_stream_tool_call_id(event);
    let projection = tool_invocation_projection_mut(projections, tool_call_id);
    match event {
        ToolInvocationStreamEvent::Started {
            columns,
            rows,
            started_at_ms,
            ..
        } => {
            projection.status = ToolInvocationProjectionStatus::Running;
            projection.started_at_ms = *started_at_ms;
            let terminal_output = projection
                .terminal_output
                .get_or_insert_with(Default::default);
            terminal_output.columns = *columns;
            terminal_output.rows = *rows;
        }
        ToolInvocationStreamEvent::OutputDelta { text, .. } => {
            projection.status = ToolInvocationProjectionStatus::Running;
            projection
                .terminal_output
                .get_or_insert_with(Default::default)
                .output
                .push_str(text);
        }
        ToolInvocationStreamEvent::VisualUpdate { .. }
        | ToolInvocationStreamEvent::Status { .. } => {
            projection.status = ToolInvocationProjectionStatus::Running;
        }
        ToolInvocationStreamEvent::Finished {
            is_error,
            finished_at_ms,
            ..
        } => {
            projection.status = ToolInvocationProjectionStatus::Finished;
            projection.is_error = Some(*is_error);
            projection.finished_at_ms = *finished_at_ms;
        }
        ToolInvocationStreamEvent::LegacyPresentation { presentation, .. } => {
            legacy_record_tool_presentation(projection, presentation);
        }
    }
}

fn legacy_record_tool_presentation(
    projection: &mut ToolInvocationProjection,
    presentation: &LegacyToolPresentationEvent,
) {
    projection.legacy_presentations.push(presentation.clone());
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

fn tool_projection_stream_tool_call_id(event: &ToolInvocationStreamEvent) -> &str {
    match event {
        ToolInvocationStreamEvent::Started { tool_call_id, .. }
        | ToolInvocationStreamEvent::OutputDelta { tool_call_id, .. }
        | ToolInvocationStreamEvent::VisualUpdate { tool_call_id, .. }
        | ToolInvocationStreamEvent::Status { tool_call_id, .. }
        | ToolInvocationStreamEvent::LegacyPresentation { tool_call_id, .. }
        | ToolInvocationStreamEvent::Finished { tool_call_id, .. } => tool_call_id,
    }
}

/// Current persisted session event schema version.
pub const CURRENT_SESSION_EVENT_SCHEMA_VERSION: u16 = 27;

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

/// Query for a bounded page of session history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryQuery {
    #[serde(default)]
    pub cursor: Option<SessionHistoryCursor>,
    pub limit: usize,
    pub direction: SessionHistoryDirection,
}

/// Bounded page of replayable session history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryPage {
    pub session_id: SessionId,
    pub events: Vec<SessionEvent>,
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

/// Durable runtime work identifier used across session history, IPC, and UI surfaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuntimeWorkId(pub String);

impl RuntimeWorkId {
    /// Create a runtime work identifier.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl Display for RuntimeWorkId {
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

/// Live-only session event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLiveEventKind {
    /// Coalesced assistant text produced by an active model turn.
    AssistantTextDelta { turn_id: String, text: String },
    /// Coalesced provider-exposed reasoning text produced by an active model turn.
    AssistantReasoningDelta { turn_id: String, text: String },
    /// Raw live tool output emitted while a tool is running.
    ToolOutputDelta { event: ToolInvocationStreamEvent },
    /// Live-only tool argument visual derived from partial tool-call arguments.
    ToolArgumentPreview {
        /// Model turn associated with this preview update.
        turn_id: String,
        /// Provider tool call identifier.
        tool_call_id: String,
        /// User-facing tool name.
        tool_name: String,
        /// Total assembled argument bytes received so far.
        argument_bytes: usize,
        /// Partial tool argument visual.
        preview: LiveToolArgumentPreview,
    },
    /// Live-only provider stream progress for active model turns.
    ProviderStreamProgress {
        /// Model turn associated with this progress update.
        turn_id: String,
        /// Coalesced provider stream progress event.
        event: ProviderStreamEvent,
    },
}

/// Plugin-owned visual descriptor for transcript rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginVisualDescriptor {
    /// Stable producer-owned visual instance id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_id: Option<String>,
    /// Producer plugin id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_plugin_id: Option<String>,
    /// Producer-owned schema identifier.
    pub schema: String,
    /// Producer-owned schema version.
    pub schema_version: u32,
    /// Optional human-readable fallback title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable fallback subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Opaque producer-owned payload.
    pub payload: serde_json::Value,
}

/// Live-only tool argument visual derived from partial tool-call arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveToolArgumentPreview {
    /// Plugin-owned visual descriptor.
    pub visual: PluginVisualDescriptor,
    /// Plugin-owned streaming status text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming_status: Option<String>,
    /// Total assembled argument bytes received so far.
    pub argument_bytes: usize,
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

/// Core-understood resolution for an interactive tool request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractiveToolResolution {
    /// Host submitted a surface/plugin-owned payload.
    Submitted { payload: serde_json::Value },
    /// Host/runtime could not complete the interaction.
    Aborted {
        reason: InteractiveToolAbortReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// Infrastructure-level reason an interactive tool request could not be submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractiveToolAbortReason {
    /// User dismissed the host-owned interaction UI.
    UserDismissed,
    /// The model turn was cancelled before submission.
    TurnCancelled,
    /// The client that owned the interaction detached before submission.
    ClientDetached,
    /// The interaction timed out before submission.
    Timeout,
    /// No host surface could render this interaction kind.
    UnsupportedSurface,
    /// Host-side error prevented submission.
    HostError,
}

/// How an interactive tool request affects the active model turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractiveToolTurnBehavior {
    /// Suspend the tool call and model turn until the interaction is resolved.
    #[default]
    AwaitBeforeContinuing,
    /// Finish the tool call while leaving the interaction answerable in the transcript.
    CompleteTurnWithPendingInteraction,
}

/// Host render target for an interactive tool request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractiveToolRenderTarget {
    /// Render inside the transcript tool-call block.
    #[default]
    TranscriptToolCall,
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

/// Incremental event emitted while a tool invocation is running.
///
/// This enum is persisted inside [`SessionEventKind`]. Keep the default
/// externally tagged representation so binary codecs do not need
/// self-describing `deserialize_any` support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationStreamEvent {
    /// Tool execution has started inside the provider plugin.
    Started {
        tool_call_id: String,
        tool_name: String,
        #[serde(default)]
        sequence: u64,
        #[serde(default)]
        terminal: bool,
        #[serde(default)]
        columns: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        started_at_ms: Option<u64>,
    },
    /// A chunk of live tool output is available.
    OutputDelta {
        tool_call_id: String,
        stream: ToolOutputStream,
        sequence: u64,
        text: String,
        #[serde(default)]
        byte_len: usize,
    },
    /// Plugin-owned visual update for transcript rendering.
    VisualUpdate {
        tool_call_id: String,
        sequence: u64,
        visual: PluginVisualDescriptor,
        #[serde(default)]
        streaming: bool,
    },
    /// Human-readable progress status from a long-running tool.
    Status {
        tool_call_id: String,
        sequence: u64,
        message: String,
    },
    /// Legacy plugin-owned presentation update retained only for old persisted logs.
    #[serde(rename = "presentation")]
    LegacyPresentation {
        tool_call_id: String,
        sequence: u64,
        presentation: LegacyToolPresentationEvent,
    },
    /// Tool execution has finished inside the provider plugin.
    Finished {
        tool_call_id: String,
        sequence: u64,
        is_error: bool,
        #[serde(default)]
        finished_at_ms: Option<u64>,
    },
}

/// A generic argument field selector for plugin-owned opaque presentation payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyToolPresentationPayloadSelector {
    /// Candidate top-level JSON argument names, in priority order.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Literal fallback value when no field is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<serde_json::Value>,
    /// Whether this selector must resolve before the payload can be emitted.
    #[serde(default)]
    pub required: bool,
}

/// Opaque plugin-owned presentation payload metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyToolPluginViewMetadata {
    /// Producer-owned schema identifier.
    pub schema: String,
    /// Producer-owned schema version.
    pub schema_version: u32,
    /// Producer plugin id for adapter routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_plugin_id: Option<String>,
    /// Optional human-readable fallback title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable fallback subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Payload keys mapped to tool argument fields/literals.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub payload: BTreeMap<String, LegacyToolPresentationPayloadSelector>,
}

/// Opaque plugin-owned presentation payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyToolPluginViewPresentation {
    /// Presentation target.
    pub target: LegacyToolPresentationTarget,
    /// Producer plugin id.
    pub producer_plugin_id: String,
    /// Producer-owned schema identifier.
    pub schema: String,
    /// Producer-owned schema version.
    pub schema_version: u32,
    /// Optional human-readable fallback title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable fallback subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Opaque producer-owned payload.
    pub payload: serde_json::Value,
}

/// Legacy request-preview metadata retained only for old persisted session events.
///
/// New tool definitions and new session writes must not use this as active UI input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegacyToolRequestPreviewMetadata {
    /// Plugin-owned generic presentation template.
    PluginView {
        /// Opaque plugin-owned view metadata.
        view: LegacyToolPluginViewMetadata,
    },
    /// File edit/write style preview.
    FileEdit {
        /// Candidate path fields.
        #[serde(default)]
        path_fields: Vec<String>,
        /// Candidate old-text fields.
        #[serde(default)]
        old_text_fields: Vec<String>,
        /// Candidate new-text/content fields.
        new_text_fields: Vec<String>,
    },
}

/// Legacy request-presentation metadata retained only for old persisted session events.
///
/// New tool definitions and new session writes must not use this as active UI input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyToolRequestPresentationMetadata {
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<LegacyToolPresentationField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<LegacyToolRequestPreviewMetadata>,
}

/// Declarative presentation metadata for one request argument field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyToolPresentationField {
    pub label: String,
    pub argument: String,
    pub kind: LegacyLegacyToolPresentationFieldKind,
    #[serde(default)]
    pub optional: bool,
}

/// Generic UI presentation hint for request argument fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LegacyLegacyToolPresentationFieldKind {
    #[default]
    Text,
    Path,
    Url,
    Command,
    Boolean,
    Count,
    DurationMs,
    Json,
}

impl LegacyLegacyToolPresentationFieldKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Path => "path",
            Self::Url => "url",
            Self::Command => "command",
            Self::Boolean => "boolean",
            Self::Count => "count",
            Self::DurationMs => "duration_ms",
            Self::Json => "json",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "path" => Some(Self::Path),
            "url" => Some(Self::Url),
            "command" => Some(Self::Command),
            "boolean" => Some(Self::Boolean),
            "count" => Some(Self::Count),
            "duration_ms" => Some(Self::DurationMs),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

impl Serialize for LegacyLegacyToolPresentationFieldKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LegacyLegacyToolPresentationFieldKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            serde::de::Error::unknown_variant(
                &value,
                &[
                    "text",
                    "path",
                    "url",
                    "command",
                    "boolean",
                    "count",
                    "duration_ms",
                    "json",
                ],
            )
        })
    }
}

/// Plugin-owned presentation update for a running tool invocation.
///
/// Keep the default externally tagged representation so direct typed-stable IPC
/// can carry nested presentation payloads without codec-specific DTO shims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyToolPresentationEvent {
    /// Status text for an activity, preview, or result target.
    Status(LegacyToolStatusPresentation),
    /// Card-style structured presentation.
    Card(LegacyToolCardPresentation),
    /// Progress update.
    Progress(LegacyToolProgressPresentation),
    /// Opaque plugin-owned presentation view.
    PluginView(LegacyToolPluginViewPresentation),
    /// Clear a previous presentation target.
    Clear {
        target: LegacyToolPresentationTarget,
    },
}

/// Tool presentation target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyToolPresentationTarget {
    Activity,
    Preview,
    Result,
}

/// Presentation severity/level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyToolPresentationLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// Tool status presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyToolStatusPresentation {
    pub target: LegacyToolPresentationTarget,
    pub text: String,
    #[serde(default = "default_presentation_level")]
    pub level: LegacyToolPresentationLevel,
}

/// Tool progress presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyToolProgressPresentation {
    pub target: LegacyToolPresentationTarget,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(default = "default_presentation_level")]
    pub level: LegacyToolPresentationLevel,
}

/// Tool card presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyToolCardPresentation {
    pub target: LegacyToolPresentationTarget,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<LegacyToolPresentationSection>,
}

/// Generic section in a tool presentation card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LegacyToolPresentationSection {
    Text {
        label: Option<String>,
        text: String,
    },
    Fields {
        fields: Vec<LegacyLegacyToolPresentationFieldValue>,
    },
    Terminal {
        output: String,
        columns: u16,
        rows: u16,
    },
}

/// Label/value field for a presentation section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyLegacyToolPresentationFieldValue {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub kind: LegacyLegacyToolPresentationFieldKind,
}

const fn default_presentation_level() -> LegacyToolPresentationLevel {
    LegacyToolPresentationLevel::Info
}

/// Logical output stream for an incremental tool output chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
    Pty,
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

    /// Return the token count that best represents current context pressure.
    #[must_use]
    pub const fn context_input_tokens(&self) -> Option<u32> {
        self.input_tokens
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
    ToolInvocationStreamEvent(ToolInvocationStreamEvent),
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
        request_visual: Option<PluginVisualDescriptor>,
        #[serde(
            default,
            rename = "request_presentation",
            skip_serializing_if = "Option::is_none"
        )]
        legacy_request_presentation: Option<LegacyToolRequestPresentationMetadata>,
    },
    ToolCallFinished {
        tool_call_id: String,
        result: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        output: Option<TraceBlobRef>,
        #[serde(default)]
        semantic_result: Option<ToolInvocationResult>,
    },
    PermissionRequested {
        permission_id: String,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        producer_plugin_id: Option<String>,
        tool_name: String,
        arguments_json: String,
        #[serde(
            default,
            rename = "request_presentation",
            skip_serializing_if = "Option::is_none"
        )]
        legacy_request_presentation: Option<LegacyToolRequestPresentationMetadata>,
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
    AssistantReasoningDelta {
        text: String,
    },
    /// Completed provider-exposed reasoning text.
    AssistantReasoningMessage {
        text: String,
    },
    /// Durable runtime work start marker.
    RuntimeWorkStarted {
        work_id: RuntimeWorkId,
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
        parent_work_id: Option<RuntimeWorkId>,
        #[serde(default)]
        started_at_ms: Option<u64>,
        #[serde(default)]
        cancellable: bool,
    },
    /// Durable runtime work cancellation request marker.
    RuntimeWorkCancelRequested {
        work_id: RuntimeWorkId,
        #[serde(default)]
        requested_at_ms: Option<u64>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Durable runtime work finish marker.
    RuntimeWorkFinished {
        work_id: RuntimeWorkId,
        status: RuntimeWorkStatus,
        #[serde(default)]
        finished_at_ms: Option<u64>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Durable runtime work progress marker.
    RuntimeWorkProgress {
        work_id: RuntimeWorkId,
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
    /// Incremental tool invocation event emitted while a tool is running.
    ToolInvocationStream {
        event: ToolInvocationStreamEvent,
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
    InteractiveToolRequestCreated {
        interaction_id: String,
        tool_call_id: String,
        tool_name: String,
        #[serde(default)]
        interaction_kind: Option<String>,
        surface_kind: String,
        request_json: String,
        #[serde(default)]
        required: bool,
        #[serde(default)]
        turn_behavior: InteractiveToolTurnBehavior,
        #[serde(default)]
        render_target: InteractiveToolRenderTarget,
    },
    InteractiveToolRequestResolved {
        interaction_id: String,
        tool_call_id: String,
        resolution_json: String,
    },
    /// Provider-native context installed at a durable compaction boundary.
    ProviderContextCompacted {
        snapshot: ProviderContextSnapshot,
        compacted_through_sequence: u64,
    },
    /// Exact or estimated context occupancy associated with a request boundary.
    ContextUsageObserved {
        snapshot: ContextUsageSnapshot,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_tool_result_json_decodes_current_shapes() {
        for (payload, expected) in semantic_tool_result_fixtures() {
            let decoded: ToolInvocationResult =
                serde_json::from_str(payload).expect("semantic result should decode");

            assert_eq!(decoded, expected);
        }
    }

    #[test]
    fn tool_call_finished_without_semantic_result_json_decodes() {
        let decoded: SessionEventKind = serde_json::from_str(
            r#"{"tool_call_finished":{"tool_call_id":"call-1","result":"legacy result"}}"#,
        )
        .expect("legacy tool call finished event kind should decode");

        assert_eq!(
            decoded,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_string(),
                result: "legacy result".to_string(),
                is_error: false,
                output: None,
                semantic_result: None,
            }
        );
    }

    #[test]
    fn legacy_serialized_tool_stream_presentation_decodes_to_legacy_variant() {
        let decoded: ToolInvocationStreamEvent = serde_json::from_str(
            r#"{"presentation":{"tool_call_id":"call-1","sequence":2,"presentation":{"status":{"target":"result","text":"done","level":"success"}}}}"#,
        )
        .expect("legacy presentation stream event should decode");

        assert_eq!(
            decoded,
            ToolInvocationStreamEvent::LegacyPresentation {
                tool_call_id: "call-1".to_string(),
                sequence: 2,
                presentation: LegacyToolPresentationEvent::Status(LegacyToolStatusPresentation {
                    target: LegacyToolPresentationTarget::Result,
                    text: "done".to_string(),
                    level: LegacyToolPresentationLevel::Success,
                }),
            }
        );
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
