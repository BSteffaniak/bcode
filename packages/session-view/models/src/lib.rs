#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral session view models for Bcode renderers.
//!
//! These types are intentionally presentation-semantic instead of renderer-specific: terminal,
//! web, and future renderers should be able to consume them without depending on terminal frames,
//! browser DOM primitives, daemon clients, or application orchestration.

use bcode_session_models::{
    ClientId, InteractiveToolRenderTarget, InteractiveToolResolution, InteractiveToolTurnBehavior,
    ModelTurnOutcome, PluginVisualDescriptor, RequestContextOccupancy, RuntimeWorkKind,
    RuntimeWorkStatus, SessionId, SessionSummary, SessionTokenUsage, ToolArtifact,
    ToolInvocationResult, WorkId,
};
use bcode_tool::InteractionInput;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[cfg(test)]
mod tests;

/// Monotonic revision for renderer-visible view state.
pub type ViewRevision = u64;

/// Stable, source-derived identifier for a transcript item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TranscriptViewItemId(String);

impl TranscriptViewItemId {
    /// Create an identifier from a stable namespaced key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Create an identifier for an event-owned transcript item.
    #[must_use]
    pub fn event(sequence: u64) -> Self {
        Self(format!("event:{sequence}"))
    }

    /// Create an identifier for a tool invocation.
    #[must_use]
    pub fn tool(tool_call_id: &str) -> Self {
        Self(format!("tool:{tool_call_id}"))
    }

    /// Create an identifier for a permission request.
    #[must_use]
    pub fn permission(permission_id: &str) -> Self {
        Self(format!("permission:{permission_id}"))
    }

    /// Create an identifier for runtime work.
    #[must_use]
    pub fn runtime_work(work_id: &WorkId) -> Self {
        Self(format!("runtime-work:{work_id}"))
    }

    /// Create an identifier for an interaction.
    #[must_use]
    pub fn interaction(interaction_id: &str) -> Self {
        Self(format!("interaction:{interaction_id}"))
    }

    /// Return the stable identifier value.
    #[must_use]
    pub fn get(&self) -> &str {
        &self.0
    }
}

/// Snapshot of the renderer-neutral state for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionViewSnapshot {
    /// Snapshot schema version.
    pub schema_version: u16,
    /// Current view revision.
    pub revision: ViewRevision,
    /// Active session identifier, when attached to a persisted session.
    pub session_id: Option<SessionId>,
    /// Human-readable session title.
    pub title: Option<String>,
    /// Current session working directory, when known.
    pub working_directory: Option<PathBuf>,
    /// Last source event sequence included in this snapshot.
    pub latest_sequence: Option<u64>,
    /// Renderer-neutral transcript items.
    pub transcript: TranscriptViewDocument,
    /// Active opaque contributions keyed by invocation and contribution identity.
    #[serde(default)]
    pub contributions: BTreeMap<String, bcode_session_models::ToolContributionEvent>,
    /// Active renderer-neutral exchange requests keyed by invocation and exchange identity.
    #[serde(default)]
    pub active_exchanges: BTreeMap<String, bcode_session_models::ToolExchangeRequest>,
    /// Active invocation lifecycle keyed by invocation identifier.
    #[serde(default)]
    pub active_invocations: BTreeMap<String, bcode_session_models::ToolInvocationLifecycleEvent>,
    /// Active or recently observed tool invocations keyed by provider tool call id.
    pub tools: BTreeMap<String, ToolInvocationView>,
    /// Pending permission requests visible to renderers.
    pub permissions: Vec<PermissionView>,
    /// Runtime work entries visible to renderers.
    pub runtime_work: Vec<RuntimeWorkView>,
    /// Active skills selected for the session.
    #[serde(default)]
    pub active_skills: BTreeSet<String>,
    /// Latest plugin-owned status notes keyed by plugin and note identity.
    #[serde(default)]
    pub plugin_status: BTreeMap<String, PluginStatusView>,
    /// Composer state.
    pub composer: ComposerViewState,
    /// Current reasoning/thinking display state.
    pub thinking: ThinkingViewState,
    /// Renderer-neutral runtime/model/agent/turn state.
    #[serde(default)]
    pub runtime: SessionRuntimeViewState,
    /// Known interactive requests.
    pub interactions: Vec<InteractionViewSummary>,
    /// Session summary metadata, when supplied by the daemon/catalog.
    pub session_summary: Option<SessionSummary>,
}

impl SessionViewSnapshot {
    /// Current snapshot schema version.
    pub const SCHEMA_VERSION: u16 = 9;

    /// Create an empty snapshot.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            revision: 0,
            session_id: None,
            title: None,
            working_directory: None,
            latest_sequence: None,
            transcript: TranscriptViewDocument::default(),
            contributions: BTreeMap::new(),
            active_exchanges: BTreeMap::new(),
            active_invocations: BTreeMap::new(),
            tools: BTreeMap::new(),
            permissions: Vec::new(),
            runtime_work: Vec::new(),
            active_skills: BTreeSet::new(),
            plugin_status: BTreeMap::new(),
            composer: ComposerViewState::default(),
            thinking: ThinkingViewState::default(),
            runtime: SessionRuntimeViewState::default(),
            interactions: Vec::new(),
            session_summary: None,
        }
    }
}

/// Incremental renderer-neutral session view update prepared for future patch streaming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionViewPatch {
    /// Patch schema version.
    pub schema_version: u16,
    /// Revision before applying this patch.
    pub base_revision: ViewRevision,
    /// Revision after applying this patch.
    pub revision: ViewRevision,
    /// Target session identifier, when known.
    pub session_id: Option<SessionId>,
    /// Transcript item operations.
    pub transcript: Vec<TranscriptViewPatchOp>,
    /// Opaque contribution updates keyed by invocation and contribution identity.
    pub contributions: BTreeMap<String, bcode_session_models::ToolContributionEvent>,
    /// Active exchange updates keyed by invocation and exchange identity.
    pub active_exchanges: BTreeMap<String, bcode_session_models::ToolExchangeRequest>,
    /// Invocation lifecycle updates keyed by invocation identifier.
    pub active_invocations: BTreeMap<String, bcode_session_models::ToolInvocationLifecycleEvent>,
    /// Tool updates keyed by tool call id.
    pub tools: BTreeMap<String, ToolInvocationView>,
    /// Permission updates.
    pub permissions: Vec<PermissionView>,
    /// Runtime-work updates.
    pub runtime_work: Vec<RuntimeWorkView>,
    /// Active skill-set replacement, when changed.
    pub active_skills: Option<BTreeSet<String>>,
    /// Plugin status updates keyed by plugin and note identity.
    pub plugin_status: BTreeMap<String, PluginStatusView>,
    /// Composer replacement, when changed.
    pub composer: Option<ComposerViewState>,
    /// Thinking state replacement, when changed.
    pub thinking: Option<ThinkingViewState>,
    /// Runtime/model/agent/turn state replacement, when changed.
    pub runtime: Option<SessionRuntimeViewState>,
    /// Interaction updates.
    pub interactions: Vec<InteractionViewSummary>,
}

impl SessionViewPatch {
    /// Current patch schema version.
    pub const SCHEMA_VERSION: u16 = 9;

    /// Create an empty patch between two revisions.
    #[must_use]
    pub const fn empty(base_revision: ViewRevision, revision: ViewRevision) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            base_revision,
            revision,
            session_id: None,
            transcript: Vec::new(),
            contributions: BTreeMap::new(),
            active_exchanges: BTreeMap::new(),
            active_invocations: BTreeMap::new(),
            tools: BTreeMap::new(),
            permissions: Vec::new(),
            runtime_work: Vec::new(),
            active_skills: None,
            plugin_status: BTreeMap::new(),
            composer: None,
            thinking: None,
            runtime: None,
            interactions: Vec::new(),
        }
    }
}

/// Transcript patch operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TranscriptViewPatchOp {
    /// Append a new transcript item.
    Append { item: TranscriptViewItem },
    /// Replace an existing transcript item by id.
    Replace { item: TranscriptViewItem },
    /// Remove a transcript item by id.
    Remove { id: TranscriptViewItemId },
    /// Replace the entire bounded transcript window.
    Reset { document: TranscriptViewDocument },
}

/// Renderer-neutral transcript document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptViewDocument {
    /// Document revision.
    pub revision: ViewRevision,
    /// Ordered transcript items.
    pub items: Vec<TranscriptViewItem>,
    /// First source event sequence covered by this bounded window.
    #[serde(default)]
    pub source_start_sequence: Option<u64>,
    /// Last source event sequence covered by this bounded window.
    #[serde(default)]
    pub source_end_sequence: Option<u64>,
    /// Whether older history exists before this document window.
    pub has_older_history: bool,
    /// Whether newer history exists after this document window.
    pub has_newer_history: bool,
}

/// Renderer-neutral transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptViewItem {
    /// Stable item identifier.
    pub id: TranscriptViewItemId,
    /// Item revision.
    pub revision: ViewRevision,
    /// Source event sequence that first produced this item, when known.
    pub sequence: Option<u64>,
    /// Source event timestamp in Unix milliseconds, when known.
    pub timestamp_ms: Option<u64>,
    /// Whether this item is currently receiving streamed updates.
    pub streaming: bool,
    /// Semantic item kind.
    pub kind: TranscriptViewItemKind,
}

/// Semantic renderer-neutral transcript item kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptViewItemKind {
    /// User-authored chat message.
    UserMessage { message: ChatMessageView },
    /// Assistant-authored chat message.
    AssistantMessage { message: ChatMessageView },
    /// Assistant reasoning/thinking content.
    ReasoningMessage { message: ChatMessageView },
    /// Tool request/result/stream block.
    ToolInvocation { tool: Box<ToolInvocationView> },
    /// Permission request block.
    Permission { permission: PermissionView },
    /// Runtime work status block.
    RuntimeWork { work: RuntimeWorkView },
    /// Provider-neutral model usage accounting.
    Usage { usage: UsageView },
    /// Interactive request block.
    Interaction { interaction: InteractionViewSummary },
    /// System/status message.
    SystemMessage { message: ChatMessageView },
    /// Generic plugin visual payload.
    PluginVisual { visual: PluginVisualView },
    /// Opaque schema-versioned tool contribution with generic fallback rendering.
    ToolContribution {
        contribution: bcode_session_models::ToolContributionEvent,
    },
}

/// Renderer-neutral model usage transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageView {
    /// Model turn identifier.
    pub turn_id: String,
    /// Provider-neutral usage accounting.
    pub usage: SessionTokenUsage,
}

/// Chat text plus renderer-neutral annotations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessageView {
    /// Plain text or markdown-compatible message content.
    pub text: String,
    /// Message format hint.
    pub format: TextFormat,
}

impl ChatMessageView {
    /// Create a markdown-compatible message.
    #[must_use]
    pub fn markdown(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            format: TextFormat::Markdown,
        }
    }

    /// Create a plain text message.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            format: TextFormat::PlainText,
        }
    }
}

/// Renderer text format hint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    /// Plain text.
    PlainText,
    /// Markdown-compatible text.
    #[default]
    Markdown,
    /// JSON text.
    Json,
}

/// Renderer-neutral tool invocation view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationView {
    /// Provider tool call identifier.
    pub tool_call_id: String,
    /// Producer plugin id, when known.
    pub producer_plugin_id: Option<String>,
    /// Tool name, when known.
    pub tool_name: Option<String>,
    /// Raw JSON arguments requested by the model, when retained.
    pub arguments_json: Option<String>,
    /// Plugin-owned request visual.
    pub request_visual: Option<PluginVisualView>,
    /// Current lifecycle status.
    pub status: ToolInvocationViewStatus,
    /// Raw final text result, when finished.
    pub result_text: Option<String>,
    /// Whether the final result represents an error.
    pub is_error: Option<bool>,
    /// Semantic result, when supplied by the tool.
    pub result: Option<ToolResultView>,
    /// Raw terminal/text stream output observed for the tool.
    pub output: Option<ToolOutputView>,
    /// Tool timing metadata.
    pub timing: ToolTimingView,
}

/// Renderer-neutral tool invocation lifecycle status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationViewStatus {
    /// Request was observed but no stream/final result has been seen.
    #[default]
    Requested,
    /// Stream lifecycle/output was observed.
    Running,
    /// Final result was observed.
    Finished,
}

impl From<bcode_session_models::ToolInvocationProjectionStatus> for ToolInvocationViewStatus {
    fn from(value: bcode_session_models::ToolInvocationProjectionStatus) -> Self {
        match value {
            bcode_session_models::ToolInvocationProjectionStatus::Requested => Self::Requested,
            bcode_session_models::ToolInvocationProjectionStatus::Running => Self::Running,
            bcode_session_models::ToolInvocationProjectionStatus::Finished => Self::Finished,
        }
    }
}

/// Renderer-neutral tool output view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputView {
    /// Raw stream output text.
    pub text: String,
    /// Terminal columns reported by the producer, when known.
    pub columns: Option<u16>,
    /// Terminal rows reported by the producer, when known.
    pub rows: Option<u16>,
}

/// Renderer-neutral tool timing metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTimingView {
    /// Tool start time as Unix milliseconds.
    pub started_at_ms: Option<u64>,
    /// Tool finish time as Unix milliseconds.
    pub finished_at_ms: Option<u64>,
    /// Timeout duration in milliseconds, when known.
    pub timeout_ms: Option<u64>,
    /// Whether the tool timed out, when known.
    pub timed_out: Option<bool>,
    /// Final duration in milliseconds, when known.
    pub duration_ms: Option<u64>,
}

/// Renderer-neutral tool result payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultView {
    /// Plain textual result.
    Text { text: String },
    /// Structured JSON result encoded as JSON text.
    Json { value: String },
    /// Plugin-owned artifact result.
    Artifact { artifact: ToolArtifactView },
}

impl From<ToolInvocationResult> for ToolResultView {
    fn from(value: ToolInvocationResult) -> Self {
        match value {
            ToolInvocationResult::Text { text } => Self::Text { text },
            ToolInvocationResult::Json { value } => Self::Json { value },
            ToolInvocationResult::Artifact { artifact } => Self::Artifact {
                artifact: ToolArtifactView::from(*artifact),
            },
        }
    }
}

/// Renderer-neutral plugin artifact view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArtifactView {
    /// Raw artifact data.
    pub artifact: ToolArtifact,
    /// Generic renderer payload for structured display.
    pub generic_payload: serde_json::Value,
}

impl From<ToolArtifact> for ToolArtifactView {
    fn from(artifact: ToolArtifact) -> Self {
        let generic_payload = serde_json::to_value(&artifact).unwrap_or(serde_json::Value::Null);
        Self {
            artifact,
            generic_payload,
        }
    }
}

/// Renderer-neutral plugin visual view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginVisualView {
    /// Raw plugin visual descriptor.
    pub descriptor: PluginVisualDescriptor,
    /// Generic renderer payload for structured display.
    pub generic_payload: serde_json::Value,
}

impl From<PluginVisualDescriptor> for PluginVisualView {
    fn from(descriptor: PluginVisualDescriptor) -> Self {
        let generic_payload = serde_json::to_value(&descriptor).unwrap_or(serde_json::Value::Null);
        Self {
            descriptor,
            generic_payload,
        }
    }
}

/// Renderer-neutral authorization-batch correlation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionBatchView {
    /// Host-assigned batch identifier.
    pub batch_id: String,
    /// Zero-based provider-order call index.
    pub call_index: usize,
    /// Total calls in the authorization batch.
    pub call_count: usize,
}

/// Pending permission request visible to renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionView {
    /// Permission identifier.
    pub permission_id: String,
    /// Session containing the checkpoint, when supplied by authoritative hydration.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Associated provider tool call identifier.
    pub tool_call_id: String,
    /// Tool name.
    #[serde(default)]
    pub tool_name: String,
    /// Raw tool argument JSON.
    #[serde(default)]
    pub arguments_json: String,
    /// Complete-batch correlation, when this checkpoint belongs to a batch.
    #[serde(default)]
    pub batch: Option<PermissionBatchView>,
    /// Agent requesting permission.
    #[serde(default)]
    pub agent_id: String,
    /// Human-readable title.
    pub title: Option<String>,
    /// Policy source requesting approval.
    #[serde(default)]
    pub policy_source: Option<String>,
    /// Human-readable detail/body text.
    pub detail: Option<String>,
    /// Whether the permission has been resolved.
    pub resolved: bool,
    /// Decision, when resolved.
    pub approved: Option<bool>,
    /// Whether a remember option is available.
    pub can_remember: bool,
}

/// Latest plugin-owned status note visible to renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStatusView {
    /// Plugin that owns the status.
    pub plugin_id: String,
    /// Stable note identity within the plugin/session.
    pub note_id: String,
    /// Human-readable status text.
    pub text: String,
    /// Lower values are retained before higher values in constrained layouts.
    #[serde(default)]
    pub priority: u16,
    /// Plugin-owned structured status metadata.
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Runtime work visible to renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeWorkView {
    /// Work identifier.
    pub work_id: WorkId,
    /// Runtime work category.
    #[serde(default)]
    pub kind: RuntimeWorkKind,
    /// Stable human-readable work label.
    #[serde(default)]
    pub label: String,
    /// Current status.
    pub status: RuntimeWorkStatus,
    /// Whether the work accepts cancellation requests.
    #[serde(default)]
    pub cancellable: bool,
    /// Latest human-readable message.
    pub message: Option<String>,
    /// Completed units, when known.
    pub completed_units: Option<u64>,
    /// Total units, when known.
    pub total_units: Option<u64>,
    /// Last status/progress timestamp in Unix milliseconds.
    pub updated_at_ms: Option<u64>,
}

impl RuntimeWorkView {
    /// Return whether this work has reached a terminal status.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            RuntimeWorkStatus::Completed
                | RuntimeWorkStatus::Cancelled
                | RuntimeWorkStatus::Failed
                | RuntimeWorkStatus::TimedOut
        )
    }
}

/// Return the renderer-neutral aggregate activity label for active runtime work.
#[must_use]
pub fn runtime_work_status_label(runtime_work: &[RuntimeWorkView]) -> Option<String> {
    let running_tools = runtime_work
        .iter()
        .filter(|work| {
            work.kind == RuntimeWorkKind::Tool && work.status == RuntimeWorkStatus::Running
        })
        .count();
    if running_tools > 1 {
        return Some(format!("running {running_tools} tools"));
    }
    let work = runtime_work
        .iter()
        .min_by(|left, right| left.work_id.cmp(&right.work_id))?;
    let prefix = match work.status {
        RuntimeWorkStatus::Queued => "queued",
        RuntimeWorkStatus::Cancelling => "cancelling",
        RuntimeWorkStatus::Running => match work.kind {
            RuntimeWorkKind::ModelTurn => "running",
            RuntimeWorkKind::Tool => "running tool",
            RuntimeWorkKind::PluginInvocation => "running plugin",
            RuntimeWorkKind::EventDelivery => "delivering event",
        },
        RuntimeWorkStatus::Completed
        | RuntimeWorkStatus::Cancelled
        | RuntimeWorkStatus::Failed
        | RuntimeWorkStatus::TimedOut => return None,
    };
    let detail = match (work.label.is_empty(), work.message.as_deref()) {
        (true, Some(message)) => message.to_owned(),
        (true, None) => work.work_id.to_string(),
        (false, Some(message)) if message != work.label => {
            format!("{} — {message}", work.label)
        }
        (false, _) => work.label.clone(),
    };
    Some(format!("{prefix}: {detail}"))
}

/// Renderer-neutral model, agent, context, and turn state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeViewState {
    /// Selected provider plugin, when known.
    pub provider_plugin_id: Option<String>,
    /// User-facing requested model selection, when known.
    pub requested_model_id: Option<String>,
    /// Concrete effective model, when known.
    pub effective_model_id: Option<String>,
    /// Selected agent, when known.
    pub agent_id: Option<String>,
    /// Selected reasoning effort, when configured.
    pub reasoning_effort: Option<String>,
    /// Selected reasoning summary mode, when configured.
    pub reasoning_summary: Option<String>,
    /// Authoritative active request-context occupancy.
    pub context_occupancy: Option<RequestContextOccupancy>,
    /// Cumulative metered tokens observed across model usage events in the current projection.
    #[serde(default)]
    pub cumulative_metered_tokens: u64,
    /// Most recently observed model usage.
    pub latest_usage: Option<SessionTokenUsage>,
    /// Active model turn identifier, when a turn is running or cancelling.
    pub active_turn_id: Option<String>,
    /// Whether cancellation has been requested for the active turn.
    pub cancelling: bool,
    /// Most recent completed turn outcome.
    pub last_turn_outcome: Option<ModelTurnOutcome>,
    /// Most recent completed turn message, when supplied.
    pub last_turn_message: Option<String>,
    /// Current provider-stream progress, when an active stream exposed status.
    pub provider_progress: Option<ProviderProgressView>,
}

/// Renderer-neutral provider stream progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProgressView {
    /// Model turn associated with the progress.
    pub turn_id: String,
    /// Human-readable semantic progress detail.
    pub detail: String,
    /// Scheduled retry time in Unix seconds, when waiting to retry.
    pub retry_at_unix: Option<u64>,
}

/// Composer state shared by renderers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerViewState {
    /// Current draft text.
    pub draft: String,
    /// Whether submitting is currently allowed.
    pub can_submit: bool,
    /// Human-readable disabled reason when submit is unavailable.
    pub disabled_reason: Option<String>,
}

/// Assistant reasoning/thinking display state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingViewState {
    /// Whether reasoning content should be visible by default.
    pub visible: bool,
    /// Current in-flight reasoning text.
    pub active_text: Option<String>,
    /// Whether the current reasoning text is streaming.
    pub streaming: bool,
}

/// Renderer-neutral interactive request summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionViewSummary {
    /// Interaction identifier.
    pub interaction_id: String,
    /// Interaction kind.
    pub kind: String,
    /// Renderer-specific surface key supplied by the interaction owner.
    #[serde(default)]
    pub surface_kind: String,
    /// Associated tool call identifier, when known.
    pub tool_call_id: Option<String>,
    /// Optional title for display.
    pub title: Option<String>,
    /// Whether the interaction requires a response before the turn can continue.
    #[serde(default)]
    pub required: bool,
    /// Optional snapshot payload for generic rendering.
    pub snapshot: Option<serde_json::Value>,
    /// Whether the interaction has been durably resolved.
    #[serde(default)]
    pub resolved: bool,
    /// Durable resolution payload, when resolved.
    #[serde(default)]
    pub resolution: Option<serde_json::Value>,
    /// Target renderer placement.
    pub render_target: InteractiveToolRenderTarget,
    /// Model turn behavior for the request.
    pub turn_behavior: InteractiveToolTurnBehavior,
}

/// Prompt placement semantics for renderer-neutral prompt submission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptPlacementView {
    /// Insert the prompt at the next safe conversation boundary.
    #[default]
    Steering,
    /// Queue the prompt as a follow-up turn after the active turn finishes.
    FollowUp,
}

/// Composer draft scope for renderer-neutral draft updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComposerDraftViewScope {
    /// Draft belongs to a persisted session.
    Session { session_id: SessionId },
    /// Draft belongs to the unsaved draft session for the launch working directory.
    DraftSession { launch_working_directory: PathBuf },
}

/// Renderer-neutral message acceptance disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAcceptanceDispositionView {
    /// Message was applied to the active turn as steering.
    AppliedSteering,
    /// Message was queued as a follow-up.
    QueuedFollowUp,
    /// Message was queued as a future turn.
    QueuedTurn,
    /// Message started a new turn.
    StartedTurn,
}

/// Result of executing a renderer-neutral session action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionViewActionOutcome {
    /// No response payload is required.
    None,
    /// A prompt was accepted and may have created a session.
    MessageAccepted {
        /// Session that received the message.
        session_id: SessionId,
        /// Whether the message was queued.
        queued: bool,
        /// Queue position, when queued.
        queue_position: Option<usize>,
        /// Authoritative admission disposition.
        disposition: MessageAcceptanceDispositionView,
    },
    /// Cancellation request result.
    Cancelled { cancelled: bool },
    /// Permission resolution result.
    PermissionResolved { resolved: bool },
    /// Permission batch resolution result.
    PermissionBatchResolved { resolved_count: usize },
    /// Interaction resolution result.
    InteractionResolved { resolved: bool },
    /// Interaction input response as generic JSON.
    InteractionInput { response: serde_json::Value },
}

/// Semantic renderer action shared by terminal, web, and future renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionViewAction {
    /// Submit a prompt for the active or specified session.
    SubmitMessage {
        /// Target session, when already attached.
        session_id: Option<SessionId>,
        /// Working directory to use when a draft/new session must be created.
        launch_working_directory: Option<PathBuf>,
        /// Prompt text.
        text: String,
        /// Prompt placement semantics.
        placement: PromptPlacementView,
    },
    /// Cancel the active model turn.
    CancelTurn {
        /// Target session.
        session_id: SessionId,
        /// Whether queued work should also be cleared.
        clear_queue: bool,
    },
    /// Resolve a permission request.
    ResolvePermission {
        /// Permission id.
        permission_id: String,
        /// Whether the request is approved.
        approved: bool,
        /// Whether the decision should be remembered.
        remember: bool,
    },
    /// Resolve every pending permission in one authorization batch.
    ResolvePermissionBatch {
        /// Authorization batch id.
        batch_id: String,
        /// Whether the batch is approved.
        approved: bool,
    },
    /// Submit semantic input to an interactive tool/controller.
    SubmitInteractionInput {
        /// Interaction id.
        interaction_id: String,
        /// Semantic interaction input.
        input: InteractionInput,
    },
    /// Resolve an interactive request with a final semantic resolution.
    ResolveInteraction {
        /// Interaction id.
        interaction_id: String,
        /// Final interaction resolution.
        resolution: InteractiveToolResolution,
    },
    /// Request a switch to another session.
    SwitchSession {
        /// Target session.
        session_id: SessionId,
    },
    /// Update the local composer draft.
    UpdateDraft {
        /// Draft scope to update.
        scope: ComposerDraftViewScope,
        /// Draft text.
        text: String,
    },
    /// Set the selected model for a session.
    SetModel {
        /// Target session.
        session_id: SessionId,
        /// Provider plugin id, when explicitly selected.
        provider_plugin_id: Option<String>,
        /// Model id.
        model_id: String,
    },
    /// Set reasoning selections for a session.
    SetReasoning {
        /// Target session.
        session_id: SessionId,
        /// Reasoning effort selection.
        effort: Option<String>,
        /// Reasoning summary selection.
        summary: Option<String>,
    },
    /// Set the selected agent for a session.
    SetAgent {
        /// Target session.
        session_id: SessionId,
        /// Agent id.
        agent_id: String,
    },
    /// Activate a skill for a session.
    ActivateSkill {
        /// Target session.
        session_id: SessionId,
        /// Skill id.
        skill_id: String,
    },
    /// Deactivate a skill for a session.
    DeactivateSkill {
        /// Target session.
        session_id: SessionId,
        /// Skill id.
        skill_id: String,
    },
    /// Load older transcript/history content.
    LoadOlderHistory {
        /// Target session.
        session_id: SessionId,
    },
    /// Load newer transcript/history content.
    LoadNewerHistory {
        /// Target session.
        session_id: SessionId,
    },
}

/// Renderer connection/client metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererClientView {
    /// Client id assigned by the daemon.
    pub client_id: ClientId,
    /// Human-readable renderer/client name.
    pub name: String,
}
