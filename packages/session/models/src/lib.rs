#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared session models for bcode.

use bcode_skill_models::{SkillActivationMode, SkillId, SkillSource};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;
use uuid::Uuid;

/// Current persisted session event schema version.
pub const CURRENT_SESSION_EVENT_SCHEMA_VERSION: u16 = 23;

/// Unique session identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
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

/// Unique connected-client identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
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
    /// Return the best user-visible title for this session.
    #[must_use]
    pub fn display_title(&self) -> &str {
        self.name
            .as_deref()
            .or(self.explicit_name.as_deref())
            .or(self.derived_title.as_deref())
            .unwrap_or("empty draft")
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
    /// Live-only tool argument preview derived from partial tool-call arguments.
    ToolArgumentPreview {
        /// Model turn associated with this preview update.
        turn_id: String,
        /// Provider tool call identifier.
        tool_call_id: String,
        /// User-facing tool name.
        tool_name: String,
        /// Total assembled argument bytes received so far.
        argument_bytes: usize,
        /// Partial tool argument preview.
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

/// Live-only tool argument preview derived from partial tool-call arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveToolArgumentPreview {
    /// File edit/write preview.
    FileEdit(LiveFileEditPreview),
    /// Shell command preview.
    ShellCommand(LiveShellCommandPreview),
    /// Query/search preview.
    Query(LiveQueryPreview),
}

/// Live-only query-like tool preview derived from partial tool-call arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveQueryPreview {
    /// Extracted string fields for display.
    pub fields: BTreeMap<String, String>,
    /// Total assembled argument bytes received so far.
    pub argument_bytes: usize,
    /// Whether the preview content was truncated by live-preview limits.
    pub truncated: bool,
}

/// Live-only file edit/write preview derived from partial tool-call arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveFileEditPreview {
    /// Best-effort file path extracted from partial arguments.
    pub path: Option<String>,
    /// Best-effort old text prefix extracted from partial arguments.
    pub old_text_prefix: Option<String>,
    /// Best-effort new text prefix extracted from partial arguments.
    pub new_text_prefix: String,
    /// Total assembled argument bytes received so far.
    pub argument_bytes: usize,
    /// Whether the preview content was truncated by live-preview limits.
    pub truncated: bool,
}

/// Live-only shell command preview derived from partial tool-call arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveShellCommandPreview {
    /// Best-effort command prefix extracted from partial arguments.
    pub command_prefix: String,
    /// Best-effort working directory extracted from partial arguments.
    pub cwd: Option<String>,
    /// Total assembled argument bytes received so far.
    pub argument_bytes: usize,
    /// Whether the preview content was truncated by live-preview limits.
    pub truncated: bool,
}

/// Product-facing derived view over durable session history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProjectionKind {
    /// Conversation transcript intended for chat-oriented presentation.
    Transcript,
    /// Model-context view used for prompt/context inspection.
    ModelContext,
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
    /// Shell command result.
    ShellRun { result: ShellRunResult },
    /// Filesystem or file-edit result.
    FileChange { result: FileChangeResult },
}

/// Semantic shell execution result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ShellRunResult {
    /// Terminal-backed execution with a single bounded output stream.
    Terminal {
        /// Process exit code, or `None` when unavailable.
        #[serde(default)]
        exit_code: Option<i32>,
        /// Whether execution timed out.
        #[serde(default)]
        timed_out: bool,
        /// Whether execution was cancelled.
        #[serde(default)]
        cancelled: bool,
        /// Bounded tail of the PTY output stream.
        #[serde(default, alias = "output")]
        output_tail: String,
        /// Whether the output was truncated.
        #[serde(default)]
        output_truncated: bool,
        /// Original output byte count.
        #[serde(default)]
        output_bytes: Option<u64>,
        /// Retained output byte count.
        #[serde(default)]
        retained_output_bytes: Option<u64>,
        /// Terminal columns used for execution.
        #[serde(default = "default_terminal_columns")]
        columns: u16,
        /// Terminal rows used for execution.
        #[serde(default = "default_terminal_rows")]
        rows: u16,
    },
    /// Non-terminal execution with separately captured streams.
    Captured {
        /// Process exit code, or `None` when unavailable.
        #[serde(default)]
        exit_code: Option<i32>,
        /// Whether execution timed out.
        #[serde(default)]
        timed_out: bool,
        /// Whether execution was cancelled.
        #[serde(default)]
        cancelled: bool,
        /// Bounded stdout text.
        #[serde(default)]
        stdout: String,
        /// Bounded stderr text.
        #[serde(default)]
        stderr: String,
        /// Whether stdout was truncated.
        #[serde(default)]
        stdout_truncated: bool,
        /// Whether stderr was truncated.
        #[serde(default)]
        stderr_truncated: bool,
        /// Original stdout byte count.
        #[serde(default)]
        stdout_bytes: Option<u64>,
        /// Original stderr byte count.
        #[serde(default)]
        stderr_bytes: Option<u64>,
    },
}

/// Semantic file-change result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeResult {
    /// Tool name that produced the change.
    pub tool_name: String,
    /// Human-readable summary of the change.
    pub summary: String,
    /// Best-effort target path.
    #[serde(default)]
    pub path: Option<String>,
}

const fn default_terminal_columns() -> u16 {
    80
}

const fn default_terminal_rows() -> u16 {
    24
}

/// Bounded durable presentation state for a completed tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationPresentation {
    /// Pseudo-terminal execution result.
    Terminal {
        /// Process exit code, or `None` when the process was terminated by signal.
        #[serde(default)]
        exit_code: Option<i32>,
        /// Whether execution timed out.
        #[serde(default)]
        timed_out: bool,
        /// Whether execution was cancelled.
        #[serde(default)]
        cancelled: bool,
        /// Bounded terminal byte stream decoded as UTF-8.
        #[serde(default)]
        output: String,
        /// Whether the terminal stream was truncated before serialization.
        #[serde(default)]
        output_truncated: bool,
        /// Original terminal stream byte count before truncation.
        #[serde(default)]
        output_bytes: Option<u64>,
        /// Retained terminal stream byte count after truncation.
        #[serde(default)]
        retained_output_bytes: Option<u64>,
        /// Terminal columns used for execution.
        #[serde(default = "default_terminal_columns")]
        columns: u16,
        /// Terminal rows used for execution.
        #[serde(default = "default_terminal_rows")]
        rows: u16,
    },
    /// Filesystem write/edit result.
    FileChange {
        /// Tool name that produced the change.
        tool_name: String,
        /// Human-readable plugin output.
        summary: String,
        /// Best-effort target path extracted from tool arguments.
        #[serde(default)]
        path: Option<String>,
    },
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
    /// Human-readable progress status from a long-running tool.
    Status {
        tool_call_id: String,
        sequence: u64,
        message: String,
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
        tool_name: String,
        arguments_json: String,
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
        tool_name: String,
        arguments_json: String,
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
    /// Durable bounded presentation state for a completed tool invocation.
    ToolInvocationPresentation {
        tool_call_id: String,
        #[serde(default)]
        started_at_ms: Option<u64>,
        #[serde(default)]
        finished_at_ms: Option<u64>,
        is_error: bool,
        presentation: ToolInvocationPresentation,
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
    fn tool_call_finished_semantic_result_json_decodes_current_shape() {
        let payload = serde_json::json!({
            "tool_call_finished": {
                "tool_call_id": "call-1",
                "result": "tool result",
                "is_error": false,
                "semantic_result": {
                    "type": "shell_run",
                    "result": {
                        "mode": "terminal",
                        "exit_code": 0,
                        "timed_out": false,
                        "cancelled": false,
                        "output_tail": "hello\n",
                        "output_truncated": false,
                        "output_bytes": 6,
                        "retained_output_bytes": 6,
                        "columns": 120,
                        "rows": 30
                    }
                }
            }
        })
        .to_string();

        let decoded: SessionEventKind =
            serde_json::from_str(&payload).expect("tool call finished event kind should decode");

        let SessionEventKind::ToolCallFinished {
            semantic_result: Some(ToolInvocationResult::ShellRun { result }),
            ..
        } = decoded
        else {
            panic!("expected shell semantic result");
        };
        assert_eq!(
            result,
            ShellRunResult::Terminal {
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                output_tail: "hello\n".to_string(),
                output_truncated: false,
                output_bytes: Some(6),
                retained_output_bytes: Some(6),
                columns: 120,
                rows: 30,
            }
        );
    }

    #[test]
    fn semantic_tool_result_json_decodes_legacy_terminal_output_alias() {
        let decoded: ToolInvocationResult = serde_json::from_str(
            r#"{"type":"shell_run","result":{"mode":"terminal","exit_code":7,"timed_out":true,"cancelled":false,"output":"legacy tail","output_truncated":false,"columns":120,"rows":30}}"#,
        )
        .expect("legacy terminal semantic result should decode");

        assert_eq!(
            decoded,
            ToolInvocationResult::ShellRun {
                result: ShellRunResult::Terminal {
                    exit_code: Some(7),
                    timed_out: true,
                    cancelled: false,
                    output_tail: "legacy tail".to_string(),
                    output_truncated: false,
                    output_bytes: None,
                    retained_output_bytes: None,
                    columns: 120,
                    rows: 30,
                },
            }
        );
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
    fn semantic_tool_result_json_decodes_missing_optional_fields() {
        let decoded: ToolInvocationResult = serde_json::from_str(
            r#"{"type":"shell_run","result":{"mode":"terminal","output_tail":"minimal"}}"#,
        )
        .expect("minimal terminal semantic result should decode");

        assert_eq!(
            decoded,
            ToolInvocationResult::ShellRun {
                result: ShellRunResult::Terminal {
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    output_tail: "minimal".to_string(),
                    output_truncated: false,
                    output_bytes: None,
                    retained_output_bytes: None,
                    columns: 80,
                    rows: 24,
                },
            }
        );
    }

    #[test]
    fn semantic_tool_result_json_ignores_unknown_extra_fields() {
        let decoded: ToolInvocationResult = serde_json::from_str(
            r#"{"type":"file_change","result":{"tool_name":"filesystem.write","summary":"wrote bytes","path":"file.txt","future_field":"ignored"},"future_top_level":"ignored"}"#,
        )
        .expect("semantic result with future fields should decode");

        assert_eq!(
            decoded,
            ToolInvocationResult::FileChange {
                result: FileChangeResult {
                    tool_name: "filesystem.write".to_string(),
                    summary: "wrote bytes".to_string(),
                    path: Some("file.txt".to_string()),
                },
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
                r#"{"type":"file_change","result":{"tool_name":"filesystem.write","summary":"wrote 171 bytes","path":"/tmp/hello.txt"}}"#,
                ToolInvocationResult::FileChange {
                    result: FileChangeResult {
                        tool_name: "filesystem.write".to_string(),
                        summary: "wrote 171 bytes".to_string(),
                        path: Some("/tmp/hello.txt".to_string()),
                    },
                },
            ),
            (
                r#"{"type":"shell_run","result":{"mode":"terminal","exit_code":0,"timed_out":false,"cancelled":false,"output_tail":"hello\n","output_truncated":false,"output_bytes":6,"retained_output_bytes":6,"columns":120,"rows":30}}"#,
                ToolInvocationResult::ShellRun {
                    result: ShellRunResult::Terminal {
                        exit_code: Some(0),
                        timed_out: false,
                        cancelled: false,
                        output_tail: "hello\n".to_string(),
                        output_truncated: false,
                        output_bytes: Some(6),
                        retained_output_bytes: Some(6),
                        columns: 120,
                        rows: 30,
                    },
                },
            ),
            (
                r#"{"type":"shell_run","result":{"mode":"captured","exit_code":0,"timed_out":false,"cancelled":false,"stdout":"hello\n","stderr":"","stdout_truncated":false,"stderr_truncated":false,"stdout_bytes":6,"stderr_bytes":0}}"#,
                ToolInvocationResult::ShellRun {
                    result: ShellRunResult::Captured {
                        exit_code: Some(0),
                        timed_out: false,
                        cancelled: false,
                        stdout: "hello\n".to_string(),
                        stderr: String::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                        stdout_bytes: Some(6),
                        stderr_bytes: Some(0),
                    },
                },
            ),
        ]
    }
}
