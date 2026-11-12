#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Wire models of the Bcode session bridge.
//!
//! An application built outside the Bcode workspace cannot speak the daemon's internal
//! protocol, which must match the daemon's build exactly. It starts `bcode session-bridge
//! --stdio` from the installed Bcode instead and exchanges JSON lines with it:
//!
//! * The application writes one [`ClientFrame`] per line: a request id and a [`BridgeRequest`].
//! * The bridge writes one [`BridgeFrame`] per line: a response to a request id, or an
//!   unsolicited [`BridgeEvent`] (observation updates, tool invocations).
//!
//! The first request must be [`BridgeRequest::Hello`], which negotiates the protocol version
//! both sides support ([`negotiate`]). Within one version, requests reject unknown fields, while
//! responses and events may gain optional fields that older applications ignore; transcript
//! items of kinds an application does not know decode as [`ViewItemKind::Other`] with display
//! text. Changing the meaning of a field requires a new version.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Bridge protocol version this build speaks natively.
pub const BRIDGE_PROTOCOL_VERSION: u32 = 1;

/// How many earlier protocol versions a bridge keeps accepting.
pub const BRIDGE_COMPATIBILITY_WINDOW: u32 = 2;

/// Inclusive range of protocol versions one side supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionRange {
    /// Oldest supported version.
    pub min: u32,
    /// Newest supported version.
    pub max: u32,
}

impl VersionRange {
    /// The versions this build supports: the current one and up to
    /// [`BRIDGE_COMPATIBILITY_WINDOW`] earlier ones.
    #[must_use]
    pub const fn supported() -> Self {
        Self {
            min: BRIDGE_PROTOCOL_VERSION.saturating_sub(BRIDGE_COMPATIBILITY_WINDOW),
            max: BRIDGE_PROTOCOL_VERSION,
        }
        .clamped()
    }

    /// A range of one version.
    #[must_use]
    pub const fn exactly(version: u32) -> Self {
        Self {
            min: version,
            max: version,
        }
    }

    const fn clamped(self) -> Self {
        Self {
            min: if self.min == 0 { 1 } else { self.min },
            max: self.max,
        }
    }

    /// Whether the range is non-empty and starts at version 1 or later.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.min >= 1 && self.min <= self.max
    }
}

/// Why two sides share no protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionMismatch {
    /// The application only speaks versions older than the bridge accepts: update the app.
    ApplicationTooOld,
    /// The application only speaks versions newer than the bridge knows: update Bcode.
    BcodeTooOld,
    /// The application sent an empty or zero range.
    InvalidRange,
}

impl VersionMismatch {
    /// What to tell the user, naming both ranges and which side to update.
    #[must_use]
    pub fn message(self, application: VersionRange, bridge: VersionRange) -> String {
        let ranges = format!(
            "the application speaks session bridge protocol {}-{}, the installed Bcode {}-{}",
            application.min, application.max, bridge.min, bridge.max
        );
        match self {
            Self::ApplicationTooOld => format!("{ranges}: update the application"),
            Self::BcodeTooOld => format!("{ranges}: update Bcode"),
            Self::InvalidRange => format!("{ranges}: the application's range is invalid"),
        }
    }
}

/// The highest version both ranges contain.
///
/// # Errors
///
/// * [`VersionMismatch::InvalidRange`] when the application's range is empty or starts at 0.
/// * [`VersionMismatch::ApplicationTooOld`] when every application version predates the bridge's.
/// * [`VersionMismatch::BcodeTooOld`] when every application version is newer than the bridge's.
pub const fn negotiate(
    application: VersionRange,
    bridge: VersionRange,
) -> Result<u32, VersionMismatch> {
    if !application.is_valid() {
        return Err(VersionMismatch::InvalidRange);
    }
    if application.max < bridge.min {
        return Err(VersionMismatch::ApplicationTooOld);
    }
    if application.min > bridge.max {
        return Err(VersionMismatch::BcodeTooOld);
    }
    Ok(if application.max < bridge.max {
        application.max
    } else {
        bridge.max
    })
}

/// One line the application writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFrame {
    /// Application-chosen id echoed by the response.
    pub id: u64,
    /// The request.
    pub request: BridgeRequest,
}

/// Requests an application sends to the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum BridgeRequest {
    /// Negotiate the protocol version; must be the first request.
    Hello {
        /// Name shown in daemon diagnostics, such as `bdata`.
        client_name: String,
        /// Versions the application speaks.
        versions: VersionRange,
    },
    /// Create a session in the user's canonical store.
    CreateSession {
        /// Optional session name.
        #[serde(default)]
        name: Option<String>,
        /// Absolute working directory; the session's workspace scope.
        working_directory: String,
        /// The application that creates it.
        #[serde(default)]
        origin: Option<Origin>,
        /// Agent to select for the session.
        #[serde(default)]
        agent: Option<String>,
        /// Model to select for the session.
        #[serde(default)]
        model: Option<ModelChoice>,
    },
    /// List a bounded page of sessions, newest activity first.
    ListSessions {
        /// Case-insensitive title or directory substring.
        #[serde(default)]
        text: Option<String>,
        /// Only sessions created by this application (and context).
        #[serde(default)]
        origin: Option<OriginFilter>,
        /// Only sessions of this working directory; all directories when absent.
        #[serde(default)]
        working_directory: Option<String>,
        /// Continuation from a previous page.
        #[serde(default)]
        cursor: Option<String>,
        /// Page size, at most [`MAX_LIST_LIMIT`].
        limit: u32,
    },
    /// One session's summary.
    Summary {
        /// The session.
        session_id: String,
    },
    /// A bounded transcript page ending before `before_sequence` (the newest when absent).
    HistoryPage {
        /// The session.
        session_id: String,
        /// Source sequence the page ends before.
        #[serde(default)]
        before_sequence: Option<u64>,
        /// Items wanted, at most [`MAX_VIEW_ITEMS`].
        max_items: u32,
    },
    /// Start observing a session: a snapshot now, a new snapshot whenever it changes.
    Observe {
        /// The session.
        session_id: String,
        /// Newest transcript items per snapshot, at most [`MAX_VIEW_ITEMS`].
        max_items: u32,
    },
    /// Stop an observation.
    StopObserving {
        /// The observation.
        observation_id: String,
    },
    /// Submit a message as a new turn.
    Submit {
        /// The session.
        session_id: String,
        /// The message.
        text: String,
        /// Context blocks sent with the message.
        #[serde(default)]
        context: Vec<ContextBlock>,
        /// Model for this turn.
        #[serde(default)]
        model: Option<ModelChoice>,
        /// Reasoning effort for this turn.
        #[serde(default)]
        effort: Option<String>,
        /// Agent for this turn.
        #[serde(default)]
        agent: Option<String>,
    },
    /// Cancel the session's running turn.
    CancelTurn {
        /// The session.
        session_id: String,
    },
    /// Rename a session.
    Rename {
        /// The session.
        session_id: String,
        /// New name.
        name: String,
    },
    /// Delete a session.
    Delete {
        /// The session.
        session_id: String,
    },
    /// Permission requests waiting for an answer.
    PendingPermissions {
        /// Only this session's requests.
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Answer a permission request.
    ResolvePermission {
        /// The request.
        permission_id: String,
        /// Allow or deny.
        approve: bool,
        /// Remember the answer as a rule.
        #[serde(default)]
        remember: bool,
    },
    /// Register the application's tools and agent profiles, replacing earlier ones.
    RegisterTools {
        /// The application's namespace; tool and profile names start with `<provider_id>.`.
        provider_id: String,
        /// Tools the application runs.
        tools: Vec<ToolDescriptor>,
        /// Agent profiles the application contributes.
        #[serde(default)]
        profiles: Vec<AgentProfile>,
    },
    /// Offer the application's tools in a session from its next turn on.
    ClaimSession {
        /// The session.
        session_id: String,
    },
    /// Stop offering the application's tools in a session.
    ReleaseSession {
        /// The session.
        session_id: String,
    },
    /// Answer a [`BridgeEvent::ToolInvoke`].
    ToolResult {
        /// The invocation.
        invocation_id: String,
        /// What the tool produced.
        outcome: ToolOutcome,
    },
    /// Agents that can be selected.
    ListAgents,
    /// Models that can be selected, with pricing and effort values.
    ListModels,
    /// A session's token and cost totals.
    Usage {
        /// The session.
        session_id: String,
    },
    /// Import one JSON conversation file the application kept with the in-process SDK.
    ImportSdkSession {
        /// The file.
        path: String,
        /// Working directory for imported history without its own.
        working_directory: String,
        /// Application recorded as the imported session's origin.
        #[serde(default)]
        origin: Option<Origin>,
    },
    /// The command that opens a session in `bcode tui` with the installed executable.
    OpenCommand {
        /// The session.
        session_id: String,
    },
}

/// Largest page [`BridgeRequest::ListSessions`] returns.
pub const MAX_LIST_LIMIT: u32 = 200;
/// Most transcript items one snapshot or history page carries.
pub const MAX_VIEW_ITEMS: u32 = 200;
/// Bytes of text kept per transcript item; longer text ends with an ellipsis.
pub const MAX_ITEM_TEXT_BYTES: usize = 64 * 1024;
/// Bytes kept of a tool call's arguments or result.
pub const MAX_TOOL_TEXT_BYTES: usize = 16 * 1024;

/// One line the bridge writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum BridgeFrame {
    /// The answer to the request with `id`.
    Response {
        /// The request's id.
        id: u64,
        /// The result.
        #[serde(flatten)]
        result: BridgeResult,
    },
    /// An unsolicited event.
    Event {
        /// The event.
        event: BridgeEvent,
    },
}

/// The outcome of one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeResult {
    /// The request succeeded.
    Ok(Box<BridgeResponse>),
    /// The request failed.
    Error(BridgeError),
}

/// A failed request: a stable code and a message for the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeError {
    /// Stable code, such as `hello_required`, `version_mismatch`, `session_not_found` or a
    /// daemon error code like `agent_profile_unavailable`.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BridgeError {}

/// Successful responses, one kind per request family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeResponse {
    /// Answer to [`BridgeRequest::Hello`].
    Hello(Hello),
    /// One session.
    Session {
        /// The session.
        session: SessionSummary,
    },
    /// A page of sessions.
    Sessions(SessionPage),
    /// A transcript page.
    History {
        /// The page.
        view: SessionView,
    },
    /// An observation started; its snapshots arrive as [`BridgeEvent::Observation`].
    Observing {
        /// The observation.
        observation_id: String,
    },
    /// A submitted turn.
    Submitted {
        /// The turn.
        turn_id: String,
        /// Whether the turn waits behind another one.
        queued: bool,
    },
    /// Permission requests waiting for an answer.
    Permissions {
        /// The requests.
        permissions: Vec<PendingPermission>,
    },
    /// The application's tools are registered.
    ToolsRegistered {
        /// Registered tools.
        tool_count: usize,
        /// Registered profiles.
        profile_count: usize,
    },
    /// Selectable agents.
    Agents {
        /// The agents.
        agents: Vec<AgentInfo>,
    },
    /// Selectable models.
    Models {
        /// The models.
        models: Vec<ModelInfo>,
    },
    /// Token and cost totals.
    Usage(Usage),
    /// An imported session; importing identical content again returns the same session.
    Imported {
        /// The session.
        session: SessionSummary,
        /// What the import could not carry over.
        warnings: Vec<String>,
    },
    /// A command line.
    Command {
        /// Program and arguments.
        argv: Vec<String>,
    },
    /// The request succeeded and has nothing to report.
    Done,
}

/// Answer to [`BridgeRequest::Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The negotiated protocol version.
    pub version: u32,
    /// Versions the bridge accepts.
    pub bridge_versions: VersionRange,
    /// The installed Bcode's version.
    pub bcode_version: String,
}

/// Events the bridge sends without a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BridgeEvent {
    /// A complete snapshot of an observed session; it replaces earlier ones.
    Observation {
        /// The observation.
        observation_id: String,
        /// Increases by one per snapshot of this observation.
        sequence: u64,
        /// The snapshot.
        view: Box<SessionView>,
    },
    /// An observation ended; observe again to continue.
    ObservationEnded {
        /// The observation.
        observation_id: String,
        /// Why it ended.
        reason: String,
    },
    /// Run one of the application's tools. The call already passed Bcode's permission policy.
    ToolInvoke(ToolInvocation),
    /// Stop an invocation; a later result is dropped.
    ToolCancel {
        /// The invocation.
        invocation_id: String,
    },
    /// Another connection of the same application claimed the session.
    ClaimLost {
        /// The session.
        session_id: String,
    },
    /// The bridge lost the application's tool registration, for example when the daemon
    /// restarted; register again and re-claim sessions.
    ToolsLost {
        /// Why.
        reason: String,
    },
}

/// The application that creates a session. Display and filter data only; it grants nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// Stable application id, such as `bdata`.
    pub app_id: String,
    /// Label for pickers.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Secret-free context values, such as a connection label.
    #[serde(default)]
    pub context: BTreeMap<String, String>,
    /// Whether normal pickers hide the session.
    #[serde(default)]
    pub background: bool,
}

/// Filter on the creating application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginFilter {
    /// Required application id.
    #[serde(default)]
    pub app_id: Option<String>,
    /// Context entries that must all match.
    #[serde(default)]
    pub context: BTreeMap<String, String>,
}

/// A model choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// Provider plugin; the configured default when absent.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model id.
    pub model: String,
}

/// One context block sent with a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBlock {
    /// Short label, such as `screen`.
    pub label: String,
    /// Text the model reads after the message.
    pub text: String,
    /// Keep the block out of session history; only this turn reads it.
    #[serde(default)]
    pub request_only: bool,
}

/// A tool the application runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// `<provider_id>.<tool>`.
    pub name: String,
    /// Description the model reads.
    pub description: String,
    /// JSON Schema of the arguments.
    pub input_schema: serde_json::Value,
    /// Whether calls only read (`read_only`) or may change things (`mutating`); mutating calls
    /// ask for permission unless the user's rules say otherwise.
    pub policy: ToolPolicy,
    /// Longest a call may take before it fails as timed out.
    pub max_duration_ms: u32,
}

/// Permission class of an application tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// Calls only read.
    ReadOnly,
    /// Calls may change state.
    Mutating,
}

/// An agent profile the application contributes. It narrows `base_agent` and never grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    /// `<provider_id>.<name>`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Short description.
    #[serde(default)]
    pub description: String,
    /// Agent whose tools and permission policy the profile narrows.
    pub base_agent: String,
    /// Instructions added after the base agent's.
    #[serde(default)]
    pub instructions: String,
    /// Tools the profile may use.
    pub tools: Vec<String>,
}

/// A call to one of the application's tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocation {
    /// Answer with this id.
    pub invocation_id: String,
    /// The calling session.
    pub session_id: String,
    /// The model's call id.
    pub tool_call_id: String,
    /// The tool.
    pub tool_name: String,
    /// Arguments as JSON text.
    pub arguments_json: String,
    /// Milliseconds the application has to answer.
    pub deadline_ms: u32,
}

/// What an application tool produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolOutcome {
    /// The tool ran; `content` is what the model reads.
    Completed {
        /// The result.
        content: String,
    },
    /// The tool failed.
    Failed {
        /// Stable code.
        code: String,
        /// Message the model reads.
        message: String,
    },
}

/// A session as lists show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session id.
    pub id: String,
    /// Display title.
    #[serde(default)]
    pub title: Option<String>,
    /// Working directory.
    pub working_directory: String,
    /// Creation time, Unix milliseconds.
    pub created_at_ms: u64,
    /// Latest activity, Unix milliseconds.
    pub updated_at_ms: u64,
    /// Attached clients.
    pub client_count: usize,
    /// The creating application, when recorded.
    #[serde(default)]
    pub origin: Option<Origin>,
}

/// A page of sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPage {
    /// The sessions.
    pub sessions: Vec<SessionSummary>,
    /// Continue with this cursor; `None` at the end.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// Examined sessions whose origin could not be read, so an origin filter could not
    /// classify them. They are not hidden from unfiltered listing.
    #[serde(default)]
    pub unclassified: usize,
    /// Whether discovery was incomplete, so the page may miss sessions.
    #[serde(default)]
    pub degraded: bool,
}

/// A session's transcript and live state, in a form that stays stable across Bcode versions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    /// Session id.
    pub session_id: String,
    /// Display title.
    #[serde(default)]
    pub title: Option<String>,
    /// Transcript items, oldest first.
    pub items: Vec<ViewItem>,
    /// Whether older items exist before the first one.
    #[serde(default)]
    pub has_older: bool,
    /// Permission requests waiting for an answer.
    #[serde(default)]
    pub pending_permissions: Vec<PendingPermission>,
    /// Whether a turn is running.
    #[serde(default)]
    pub turn_active: bool,
    /// Whether the running turn is being cancelled.
    #[serde(default)]
    pub cancelling: bool,
    /// Reasoning text streaming right now, bounded.
    #[serde(default)]
    pub thinking: Option<String>,
    /// Selected agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// Selected model.
    #[serde(default)]
    pub model: Option<String>,
    /// Attached clients.
    #[serde(default)]
    pub client_count: usize,
    /// Token and cost totals.
    #[serde(default)]
    pub usage: Usage,
    /// Error the session reports, if any.
    #[serde(default)]
    pub error: Option<String>,
    /// How the most recent finished turn ended, so an application sees a turn that failed
    /// without producing any transcript item.
    #[serde(default)]
    pub last_turn: Option<LastTurn>,
}

/// How a finished turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastTurn {
    /// The outcome.
    pub outcome: TurnOutcome,
    /// What Bcode reported about it, bounded.
    #[serde(default)]
    pub message: Option<String>,
}

/// Outcome of a finished turn. Outcomes a newer Bcode adds decode as [`Self::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    /// The model finished its answer.
    Completed,
    /// The turn was cancelled.
    Cancelled,
    /// The turn failed; see [`LastTurn::message`].
    Failed,
    /// The model stopped answering within its idle limit.
    TimedOut,
    /// The turn reached its limit of tool rounds.
    ToolRoundLimit,
    /// No model provider could serve the turn, for example because none is configured.
    ProviderUnavailable,
    /// An outcome this application does not know.
    #[serde(other)]
    Other,
}

/// Kind of a transcript item. Kinds a newer Bcode adds decode as [`Self::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewItemKind {
    /// A user message.
    User,
    /// Assistant text.
    Assistant,
    /// Model reasoning.
    Reasoning,
    /// A tool call; see [`ViewItem::tool`].
    Tool,
    /// A permission request.
    Permission,
    /// An interactive request.
    Interaction,
    /// A system or status note.
    Notice,
    /// A kind this application does not know; show [`ViewItem::text`].
    #[serde(other)]
    Other,
}

/// One transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewItem {
    /// Stable id within the session.
    pub id: String,
    /// Kind.
    pub kind: ViewItemKind,
    /// Display text; every kind has one, so unknown kinds stay readable.
    #[serde(default)]
    pub text: String,
    /// Source sequence, when known.
    #[serde(default)]
    pub sequence: Option<u64>,
    /// Whether the item is still streaming.
    #[serde(default)]
    pub streaming: bool,
    /// Tool call details, for [`ViewItemKind::Tool`].
    #[serde(default)]
    pub tool: Option<ToolCallView>,
    /// Request id, for [`ViewItemKind::Permission`] and [`ViewItemKind::Interaction`].
    #[serde(default)]
    pub request_id: Option<String>,
}

/// A tool call in the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallView {
    /// The model's call id.
    pub tool_call_id: String,
    /// Tool name.
    pub name: String,
    /// Status.
    pub status: ToolCallStatus,
    /// Arguments as JSON text, bounded.
    #[serde(default)]
    pub arguments: Option<String>,
    /// Result text, bounded.
    #[serde(default)]
    pub result: Option<String>,
    /// Whether the result is an error.
    #[serde(default)]
    pub is_error: bool,
}

/// Status of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Requested, not started.
    Requested,
    /// Running.
    Running,
    /// Waiting for input or a resource.
    Waiting,
    /// Finished.
    Finished,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
    /// A status this application does not know.
    #[serde(other)]
    Other,
}

/// A permission request waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPermission {
    /// Answer with this id.
    pub permission_id: String,
    /// The asking session.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The tool.
    pub tool_name: String,
    /// Its arguments as JSON text, bounded.
    #[serde(default)]
    pub arguments: String,
    /// Title for the prompt.
    #[serde(default)]
    pub title: Option<String>,
}

/// Token and cost totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Metered tokens.
    #[serde(default)]
    pub tokens: u64,
    /// Estimated cost in US dollars, when every request was priced.
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

/// A selectable agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Id to select.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// Whether it is the default.
    #[serde(default)]
    pub is_default: bool,
}

/// A selectable model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Provider plugin.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Reasoning effort values the model accepts.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// US dollars per million input tokens, when priced.
    #[serde(default)]
    pub input_usd_per_million: Option<f64>,
    /// US dollars per million output tokens, when priced.
    #[serde(default)]
    pub output_usd_per_million: Option<f64>,
}

/// Shorten `text` to at most `max_bytes`, on a character boundary, ending with `…` when cut.
#[must_use]
pub fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let ellipsis = "…";
    let mut end = max_bytes.saturating_sub(ellipsis.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ellipsis}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_picks_the_newest_shared_version_and_names_the_side_to_update() {
        let bridge = VersionRange { min: 3, max: 5 };
        assert_eq!(negotiate(VersionRange::exactly(4), bridge), Ok(4));
        assert_eq!(negotiate(VersionRange { min: 1, max: 9 }, bridge), Ok(5));
        assert_eq!(
            negotiate(VersionRange { min: 1, max: 2 }, bridge),
            Err(VersionMismatch::ApplicationTooOld)
        );
        assert_eq!(
            negotiate(VersionRange::exactly(6), bridge),
            Err(VersionMismatch::BcodeTooOld)
        );
        assert_eq!(
            negotiate(VersionRange { min: 2, max: 1 }, bridge),
            Err(VersionMismatch::InvalidRange)
        );
        assert!(
            VersionMismatch::BcodeTooOld
                .message(VersionRange::exactly(6), bridge)
                .ends_with("update Bcode")
        );
    }

    #[test]
    fn the_supported_window_ends_at_the_current_version() {
        let supported = VersionRange::supported();
        assert!(supported.is_valid());
        assert_eq!(supported.max, BRIDGE_PROTOCOL_VERSION);
        assert!(BRIDGE_PROTOCOL_VERSION - supported.min <= BRIDGE_COMPATIBILITY_WINDOW);
    }

    #[test]
    fn requests_reject_unknown_fields() {
        let known = r#"{"id":1,"request":{"op":"summary","session_id":"s"}}"#;
        assert!(serde_json::from_str::<ClientFrame>(known).is_ok());
        let unknown = r#"{"id":1,"request":{"op":"summary","session_id":"s","extra":1}}"#;
        assert!(serde_json::from_str::<ClientFrame>(unknown).is_err());
    }

    #[test]
    fn unknown_item_kinds_and_fields_degrade_instead_of_failing() {
        let item: ViewItem =
            serde_json::from_str(r#"{"id":"7","kind":"hologram","text":"a 3D chart","depth":3}"#)
                .expect("decodes");
        assert_eq!(item.kind, ViewItemKind::Other);
        assert_eq!(item.text, "a 3D chart");
        let status: ToolCallStatus = serde_json::from_str(r#""paused""#).expect("decodes");
        assert_eq!(status, ToolCallStatus::Other);
        let outcome: TurnOutcome = serde_json::from_str(r#""handed_off""#).expect("decodes");
        assert_eq!(outcome, TurnOutcome::Other);
    }

    #[test]
    fn frames_round_trip() {
        let frame = BridgeFrame::Response {
            id: 3,
            result: BridgeResult::Ok(Box::new(BridgeResponse::Submitted {
                turn_id: "t".into(),
                queued: false,
            })),
        };
        let line = serde_json::to_string(&frame).expect("encode");
        assert_eq!(
            serde_json::from_str::<BridgeFrame>(&line).expect("decode"),
            frame
        );
        let error = BridgeFrame::Response {
            id: 4,
            result: BridgeResult::Error(BridgeError {
                code: "hello_required".into(),
                message: "say hello first".into(),
            }),
        };
        let line = serde_json::to_string(&error).expect("encode");
        assert!(
            line.contains(r#""error":{"code":"hello_required""#),
            "{line}"
        );
        assert_eq!(
            serde_json::from_str::<BridgeFrame>(&line).expect("decode"),
            error
        );
    }

    #[test]
    fn bounded_text_cuts_on_a_character_boundary() {
        assert_eq!(bounded_text("short", 10), "short");
        let cut = bounded_text("ééééé", 6);
        assert!(cut.len() <= 6, "{cut}");
        assert!(cut.ends_with('…'));
    }
}
