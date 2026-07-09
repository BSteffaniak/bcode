#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral session view models for Bcode renderers.
//!
//! These types are intentionally presentation-semantic instead of renderer-specific: terminal,
//! web, and future renderers should be able to consume them without depending on terminal frames,
//! browser DOM primitives, daemon clients, or application orchestration.

use bcode_session_models::{
    ClientId, InteractiveToolRenderTarget, InteractiveToolTurnBehavior, PluginVisualDescriptor,
    RuntimeWorkId, RuntimeWorkStatus, SessionId, SessionSummary, ToolArtifact,
    ToolInvocationResult,
};
use bcode_tool::InteractionInput;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Monotonic revision for renderer-visible view state.
pub type ViewRevision = u64;

/// Stable identifier for a transcript item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TranscriptViewItemId(pub u64);

impl TranscriptViewItemId {
    /// Return the raw identifier value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
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
    /// Active or recently observed tool invocations keyed by provider tool call id.
    pub tools: BTreeMap<String, ToolInvocationView>,
    /// Pending permission requests visible to renderers.
    pub permissions: Vec<PermissionView>,
    /// Runtime work entries visible to renderers.
    pub runtime_work: Vec<RuntimeWorkView>,
    /// Composer state.
    pub composer: ComposerViewState,
    /// Current reasoning/thinking display state.
    pub thinking: ThinkingViewState,
    /// Known interactive requests.
    pub interactions: Vec<InteractionViewSummary>,
    /// Session summary metadata, when supplied by the daemon/catalog.
    pub session_summary: Option<SessionSummary>,
}

impl SessionViewSnapshot {
    /// Current snapshot schema version.
    pub const SCHEMA_VERSION: u16 = 1;

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
            tools: BTreeMap::new(),
            permissions: Vec::new(),
            runtime_work: Vec::new(),
            composer: ComposerViewState::default(),
            thinking: ThinkingViewState::default(),
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
    /// Tool updates keyed by tool call id.
    pub tools: BTreeMap<String, ToolInvocationView>,
    /// Permission updates.
    pub permissions: Vec<PermissionView>,
    /// Runtime-work updates.
    pub runtime_work: Vec<RuntimeWorkView>,
    /// Composer replacement, when changed.
    pub composer: Option<ComposerViewState>,
    /// Thinking state replacement, when changed.
    pub thinking: Option<ThinkingViewState>,
    /// Interaction updates.
    pub interactions: Vec<InteractionViewSummary>,
}

impl SessionViewPatch {
    /// Current patch schema version.
    pub const SCHEMA_VERSION: u16 = 1;

    /// Create an empty patch between two revisions.
    #[must_use]
    pub const fn empty(base_revision: ViewRevision, revision: ViewRevision) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            base_revision,
            revision,
            session_id: None,
            transcript: Vec::new(),
            tools: BTreeMap::new(),
            permissions: Vec::new(),
            runtime_work: Vec::new(),
            composer: None,
            thinking: None,
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
    /// Interactive request block.
    Interaction { interaction: InteractionViewSummary },
    /// System/status message.
    SystemMessage { message: ChatMessageView },
    /// Generic plugin visual payload.
    PluginVisual { visual: PluginVisualView },
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

/// Pending permission request visible to renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionView {
    /// Permission identifier.
    pub permission_id: String,
    /// Associated provider tool call identifier.
    pub tool_call_id: String,
    /// Human-readable title.
    pub title: Option<String>,
    /// Human-readable detail/body text.
    pub detail: Option<String>,
    /// Whether the permission has been resolved.
    pub resolved: bool,
    /// Decision, when resolved.
    pub approved: Option<bool>,
    /// Whether a remember option is available.
    pub can_remember: bool,
}

/// Runtime work visible to renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeWorkView {
    /// Work identifier.
    pub work_id: RuntimeWorkId,
    /// Current status.
    pub status: RuntimeWorkStatus,
    /// Latest human-readable message.
    pub message: Option<String>,
    /// Completed units, when known.
    pub completed_units: Option<u64>,
    /// Total units, when known.
    pub total_units: Option<u64>,
    /// Last status/progress timestamp in Unix milliseconds.
    pub updated_at_ms: Option<u64>,
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
    /// Associated tool call identifier, when known.
    pub tool_call_id: Option<String>,
    /// Optional title for display.
    pub title: Option<String>,
    /// Optional snapshot payload for generic rendering.
    pub snapshot: Option<serde_json::Value>,
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
    },
    /// Cancellation request result.
    Cancelled { cancelled: bool },
    /// Permission resolution result.
    PermissionResolved { resolved: bool },
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
    /// Submit semantic input to an interactive tool/controller.
    SubmitInteractionInput {
        /// Interaction id.
        interaction_id: String,
        /// Semantic interaction input.
        input: InteractionInput,
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
