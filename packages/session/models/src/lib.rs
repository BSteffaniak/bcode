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
pub const CURRENT_SESSION_EVENT_SCHEMA_VERSION: u16 = 20;

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

/// Session summary used by list/select flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub name: Option<String>,
    pub client_count: usize,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub working_directory: PathBuf,
    #[serde(default)]
    pub import: Option<SessionImportSummary>,
}

/// Display/provenance metadata for imported sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImportSummary {
    pub source_id: String,
    pub source_display_name: String,
    pub external_session_id: String,
    pub imported_at_ms: u64,
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
}
