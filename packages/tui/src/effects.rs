//! Background effect runner for TUI work that may touch daemon/client services.

use std::collections::{BTreeMap, VecDeque};

use bcode_client::{BcodeClient, ClientError, MessageAcceptance};
use bcode_ipc::{ComposerDraftScope, PermissionSummary, PromptPlacement};
use bcode_session_models::{
    ProjectionWindowRequest, SessionForkResult, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryPage, SessionHistoryQuery, SessionId, SessionSummary, WorkId,
};
use bcode_session_view::execute_session_view_action;
use bcode_session_view_models::{SessionViewAction, SessionViewActionOutcome};
use bcode_skill_models::SkillId;
use bcode_worktree_models::{WorktreeCreateRequest, WorktreeCreateResponse};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::{
    TuiError, clipboard_image, history_flow,
    session_flow::{self, AgentCatalog},
    slash_palette,
};

/// Submit-message effect request payload.
pub struct SubmitMessageRequest {
    /// Existing session, if already attached.
    pub session_id: Option<SessionId>,
    /// Working directory to use when creating a draft session.
    pub launch_working_directory: std::path::PathBuf,
    /// Message text to submit.
    pub message: String,
    /// Prompt placement semantics.
    pub placement: PromptPlacement,
    /// Provider to apply before sending, if any.
    pub provider_plugin_id: Option<String>,
    /// Model to apply before sending, if any.
    pub model_id: Option<String>,
    /// Agent to apply before sending, if any.
    pub agent_id: Option<String>,
    /// Reasoning effort to apply before sending.
    pub reasoning_effort: Option<String>,
    /// Reasoning summary to apply before sending.
    pub reasoning_summary: Option<String>,
    /// Reasoning effort generation captured by this submission, if locally pending.
    pub reasoning_effort_generation: Option<u64>,
    /// Event sender for a newly-created session stream.
    pub event_sender: mpsc::Sender<super::history_flow::SessionStreamUpdate>,
}

/// Skill action kind requested by the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillActionKind {
    /// Activate the skill for the session.
    Activate,
    /// Deactivate the skill for the session.
    Deactivate,
    /// Invoke the skill for one turn.
    Invoke,
}

/// Skill action effect request payload.
pub struct SkillActionRequest {
    /// Existing session, if already attached.
    pub session_id: Option<SessionId>,
    /// Working directory to use when creating a draft session.
    pub launch_working_directory: std::path::PathBuf,
    /// Skill to act on.
    pub skill_id: SkillId,
    /// Skill action kind.
    pub action: SkillActionKind,
    /// Arguments for invocation.
    pub arguments: String,
    /// Provider to apply before invoking a skill, if any.
    pub provider_plugin_id: Option<String>,
    /// Model to apply before invoking a skill, if any.
    pub model_id: Option<String>,
    /// Agent to apply before invoking a skill, if any.
    pub agent_id: Option<String>,
    /// Reasoning effort captured for the invocation.
    pub reasoning_effort: Option<String>,
    /// Reasoning summary captured for the invocation.
    pub reasoning_summary: Option<String>,
    /// Pending reasoning effort generation captured by the invocation.
    pub reasoning_effort_generation: Option<u64>,
    /// Event sender for a newly-created session stream.
    pub event_sender: mpsc::Sender<super::history_flow::SessionStreamUpdate>,
}

/// Background work requested by local TUI event handling.
pub enum TuiEffect {
    /// Attach to a session and start its event stream.
    OpenSession {
        /// Session to open.
        session_id: SessionId,
        /// Initial projection window request.
        initial_window_request: ProjectionWindowRequest,
        /// Event sender for the live session stream.
        event_sender: mpsc::Sender<super::history_flow::SessionStreamUpdate>,
        /// Whether this explicit open may start the daemon.
        allow_daemon_start: bool,
    },
    /// Load user configuration.
    LoadConfig,
    /// Reconcile auth security status for a loaded config.
    ReconcileAuthSecurity {
        /// Loaded configuration.
        config: Box<bcode_config::BcodeConfig>,
    },
    /// Load draft-session status.
    LoadDraftStatus {
        /// Directory for draft-session draft scope.
        launch_working_directory: std::path::PathBuf,
    },
    /// Load non-critical status for an attached session.
    LoadSessionStatus {
        /// Session to hydrate.
        session_id: SessionId,
    },
    /// Refresh resolved model metadata after a model event.
    LoadSessionModelStatus { session_id: SessionId },
    /// Refresh plugin-owned status after a plugin lifecycle event.
    LoadPluginStatus { session_id: SessionId },
    /// Load agent metadata.
    LoadAgentCatalog,
    /// Load an older history page before the currently displayed timeline.
    LoadOlderHistory {
        /// Session to load.
        session_id: SessionId,
        /// Pagination cursor.
        cursor: SessionHistoryCursor,
    },
    /// Load a newer history page after the currently displayed timeline.
    LoadNewerHistory {
        /// Session to load.
        session_id: SessionId,
        /// Pagination cursor.
        cursor: SessionHistoryCursor,
    },
    /// Load the bounded pending-permission snapshot during attach/reconnect.
    ListPermissions,
    /// Save composer draft text for a scope.
    SaveDraft {
        /// Draft scope to save.
        scope: ComposerDraftScope,
        /// Draft text.
        text: String,
    },
    /// Load slash command completions for a composer query.
    LoadSlashPalette {
        /// Current slash query.
        query: String,
        /// Active session, if any.
        session_id: Option<SessionId>,
    },
    /// Load host and plugin command-palette contributions.
    LoadCommandPalette,
    /// Execute a resolved slash command without terminal navigation.
    ExecuteSlashCommand {
        /// Current session.
        session_id: Option<SessionId>,
        /// Working directory context.
        working_directory: std::path::PathBuf,
        /// Current agent identity.
        current_agent_id: String,
        /// Current reasoning display mode.
        reasoning_display_mode: bcode_config::TuiThinkingMode,
        /// Whether reasoning is displayed.
        reasoning_visible: bool,
        /// Complete command text.
        message: String,
    },
    /// Invoke a plugin-owned command without terminal navigation effects.
    InvokePluginCommand {
        /// Plugin owner.
        plugin_id: String,
        /// Command identifier.
        command_id: String,
        /// Optional command arguments.
        arguments: Option<String>,
        /// Working directory context.
        working_directory: std::path::PathBuf,
        /// Active session context.
        session_id: Option<SessionId>,
    },
    /// Submit a user message through the daemon-backed session pipeline.
    SubmitMessage {
        /// Submit request.
        request: Box<SubmitMessageRequest>,
    },
    /// Rename a session.
    RenameSession {
        /// Session to rename.
        session_id: SessionId,
        /// New optional name.
        name: Option<String>,
    },
    /// Delete a session.
    DeleteSession {
        /// Session to delete.
        session_id: SessionId,
    },
    /// Fork a session from a prompt.
    ForkSession {
        /// Source session id.
        session_id: SessionId,
        /// Prompt sequence to fork from.
        prompt_sequence: u64,
        /// Optional new session name.
        name: Option<String>,
        /// Draft text to install after completion.
        draft: Option<String>,
        /// Whether to switch to the forked session.
        switch_after_create: bool,
        /// Whether to install draft text.
        install_draft: bool,
        /// Initial transcript window when switching.
        initial_window_request: ProjectionWindowRequest,
    },
    /// Clone a session.
    CloneSession {
        /// Source session id.
        session_id: SessionId,
        /// Optional new session name.
        name: Option<String>,
        /// Whether to switch to the cloned session.
        switch_after_create: bool,
        /// Whether to keep current draft text.
        install_draft: bool,
        /// Initial transcript window when switching.
        initial_window_request: ProjectionWindowRequest,
    },
    /// Perform a skill action for a session.
    SkillAction {
        /// Skill action request.
        request: Box<SkillActionRequest>,
    },
    /// Set the active model for a session.
    SetSessionModel {
        /// Session to update.
        session_id: SessionId,
        /// Provider plugin id, when explicitly selected.
        provider_plugin_id: Option<String>,
        /// Model id to set.
        model_id: String,
    },
    /// Set session reasoning preferences.
    SetSessionReasoning {
        /// Session to update.
        session_id: SessionId,
        /// Optional reasoning effort.
        effort: Option<String>,
        /// Optional reasoning summary.
        summary: Option<String>,
        /// Pending effort generation staged for this update.
        effort_generation: Option<u64>,
        /// Success status text.
        status: String,
    },
    /// Append one durable presentation-only transcript note.
    AppendPresentationNote {
        /// Session that owns the note.
        session_id: SessionId,
        /// Stable producer identity.
        source_id: String,
        /// Unique note identity within the producer.
        note_id: String,
        /// Complete bounded note text.
        text: String,
        /// Renderer-neutral text format.
        format: bcode_command::CommandTextFormat,
    },
    /// Cancel runtime work for a session.
    CancelRuntimeWork {
        /// Session that owns the work.
        session_id: SessionId,
        /// Runtime work id.
        work_id: WorkId,
    },
    /// Request context compaction for the current session.
    CompactContext {
        /// Session to compact.
        session_id: SessionId,
    },
    /// Attach current session to a worktree path.
    AttachWorktree {
        /// Session to attach.
        session_id: SessionId,
        /// Selected worktree path.
        path: std::path::PathBuf,
    },
    /// Create a worktree.
    CreateWorktree {
        /// Request payload.
        request: WorktreeCreateRequest,
    },
    /// Request cancellation of the active turn for a session.
    CancelTurn { session_id: SessionId },
    /// Resolve one pending permission request.
    ResolvePermission {
        /// Permission request id.
        permission_id: String,
        /// Whether to approve the request.
        approved: bool,
        /// Whether to remember the decision.
        remember: bool,
        /// Whether the action targets every request in the authorization batch.
        apply_to_batch: bool,
        /// Authorization batch id when `apply_to_batch` is true.
        batch_id: Option<String>,
    },
}

/// Daemon connectivity observation reported by completed effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonObservation {
    /// The effect does not say anything about daemon connectivity.
    None,
    /// A daemon-backed request completed successfully.
    Success,
    /// The daemon was unavailable or unreachable.
    Unavailable(String),
    /// A daemon-backed request failed after reaching the daemon or for an unknown reason.
    Failed(String),
}

impl DaemonObservation {
    fn from_client_error(error: &ClientError) -> Self {
        if error.is_daemon_unavailable() {
            Self::Unavailable(error.to_string())
        } else {
            Self::Failed(error.to_string())
        }
    }

    fn from_client_result<T>(result: &Result<T, ClientError>) -> Self {
        match result {
            Ok(_) => Self::Success,
            Err(error) => Self::from_client_error(error),
        }
    }

    fn from_tui_result<T>(result: &Result<T, TuiError>) -> Self {
        match result {
            Ok(_) => Self::Success,
            Err(TuiError::Client(error)) => Self::from_client_error(error),
            Err(_) => Self::None,
        }
    }

    fn from_optional_error(connected: bool, error: Option<&str>) -> Self {
        if connected {
            Self::Success
        } else if let Some(error) = error {
            Self::Unavailable(error.to_owned())
        } else {
            Self::None
        }
    }
}

/// Completed TUI background work.
pub enum TuiEffectResult {
    /// Session-open preparation progress emitted while the open effect is running.
    SessionOpenProgress {
        snapshot: bcode_session_models::SessionOpenOperationSnapshot,
    },
    /// Session open completed.
    SessionOpened {
        /// Session that was opened.
        session_id: SessionId,
        /// Whether older history exists before the attached window.
        has_older_history: bool,
        /// Attach result and event-stream task.
        result: Result<(bcode_client::AttachedSessionHistory, JoinHandle<()>), TuiError>,
    },
    /// User configuration load completed.
    ConfigLoaded {
        /// Config load result.
        config: Box<Result<bcode_config::BcodeConfig, String>>,
    },
    /// Auth security reconciliation completed.
    AuthSecurityReconciled {
        /// Status to display, if any.
        status: Option<String>,
    },
    /// Draft-session status hydration completed.
    DraftStatusLoaded {
        /// Whether at least one daemon-backed request completed successfully.
        daemon_connected: bool,
        /// Default model status, if available.
        model: Option<bcode_ipc::SessionModelStatus>,
        /// Restored composer draft, if available.
        composer_draft: Option<String>,
        /// First non-critical error encountered.
        error: Option<String>,
    },
    /// Attached session status hydration completed.
    SessionStatusLoaded {
        /// Whether at least one daemon-backed request completed successfully.
        daemon_connected: bool,
        /// Session that was hydrated.
        session_id: SessionId,
        /// Hydrated semantic/runtime status.
        hydration: Box<SessionStatusHydration>,
    },
    /// Targeted model projection refresh completed.
    SessionModelStatusLoaded {
        session_id: SessionId,
        result: Result<bcode_ipc::SessionModelStatus, ClientError>,
    },
    /// Targeted plugin status projection refresh completed.
    PluginStatusLoaded {
        session_id: SessionId,
        plugin_status: Vec<bcode_session_view_models::PluginStatusView>,
        error: Option<String>,
    },
    /// Agent metadata load completed.
    AgentCatalogLoaded {
        /// Agent catalog result.
        agents: Result<AgentCatalog, String>,
    },
    /// Older history page load completed.
    OlderHistoryLoaded {
        /// Session that was requested.
        session_id: SessionId,
        /// History page result.
        result: Result<SessionHistoryPage, ClientError>,
    },
    /// Newer history page load completed.
    NewerHistoryLoaded {
        /// Session that was requested.
        session_id: SessionId,
        /// History page result.
        result: Result<SessionHistoryPage, ClientError>,
    },
    /// Permission poll completed.
    PermissionList {
        /// Permission list result.
        result: Result<Vec<PermissionSummary>, ClientError>,
    },
    /// Composer draft save completed.
    SaveDraft {
        /// Saved draft text.
        text: String,
        /// Save result.
        result: Result<(), ClientError>,
    },
    /// Slash palette load completed.
    SlashPaletteLoaded {
        /// Query used to build completions.
        query: String,
        /// Loaded palette state.
        palette: slash_palette::SlashPalette,
    },
    /// Command palette contributions load completed.
    CommandPaletteLoaded {
        /// Contributions result from the application client boundary.
        result: Result<Vec<bcode_command::CommandContribution>, ClientError>,
    },
    /// Resolved slash command execution completed.
    SlashCommandExecuted {
        /// Submitted command text.
        message: String,
        /// Command outcome.
        result: Result<super::slash_commands::SlashCommandOutcome, ClientError>,
    },
    /// Plugin command invocation completed.
    PluginCommandInvoked {
        /// Plugin owner.
        plugin_id: String,
        /// Typed command response.
        result: Result<bcode_command::InvokeCommandResponse, TuiError>,
    },
    /// Submit message completed.
    SubmitMessage {
        /// Message text originally submitted.
        message: String,
        /// Submit result.
        result: Box<Result<SubmitMessageResult, ClientError>>,
    },
    /// Session rename completed.
    RenameSession {
        /// Rename result.
        result: Result<SessionSummary, ClientError>,
    },
    /// Session delete completed.
    DeleteSession {
        /// Deleted session id.
        session_id: SessionId,
        /// Delete result.
        result: Result<SessionSummary, ClientError>,
    },
    /// Session fork completed.
    ForkSession {
        /// Whether to switch to the forked session.
        switch_after_create: bool,
        /// Whether to install draft text.
        install_draft: bool,
        /// Fallback draft text.
        draft: Option<String>,
        /// Initial transcript window when switching.
        initial_window_request: ProjectionWindowRequest,
        /// Fork result.
        result: Result<SessionForkResult, ClientError>,
    },
    /// Session clone completed.
    CloneSession {
        /// Whether to switch to the cloned session.
        switch_after_create: bool,
        /// Whether to keep current draft text.
        install_draft: bool,
        /// Initial transcript window when switching.
        initial_window_request: ProjectionWindowRequest,
        /// Clone result.
        result: Result<SessionForkResult, ClientError>,
    },
    /// Skill action completed.
    SkillAction {
        /// Skill action kind.
        action: SkillActionKind,
        /// Skill acted on.
        skill_id: SkillId,
        /// Skill action result.
        result: Box<Result<SkillActionResult, ClientError>>,
    },
    /// Session model selection completed.
    SetSessionModel {
        /// Session that was updated.
        session_id: SessionId,
        /// Provider plugin id, when explicitly selected.
        provider_plugin_id: Option<String>,
        /// Model id that was requested.
        model_id: String,
        /// Daemon response.
        result: Result<(), ClientError>,
    },
    /// Session reasoning update completed.
    SetSessionReasoning {
        /// Session that was updated.
        session_id: SessionId,
        /// Effort value requested by the update.
        effort: Option<String>,
        /// Pending effort generation staged for the update.
        effort_generation: Option<u64>,
        /// Success status text.
        status: String,
        /// Daemon response.
        result: Result<(), ClientError>,
    },
    /// Durable presentation note append completed.
    AppendPresentationNote {
        /// Session whose ordered note append completed.
        session_id: SessionId,
        /// Daemon response.
        result: Result<(), ClientError>,
    },
    /// Runtime work cancellation completed.
    CancelRuntimeWork {
        /// Cancelled work id.
        work_id: WorkId,
        /// Daemon response.
        result: Result<bool, ClientError>,
    },
    /// Context compaction completed.
    CompactContext {
        /// Session the request targeted.
        session_id: SessionId,
        /// Daemon response.
        result: Result<String, ClientError>,
    },
    /// Worktree attach completed.
    AttachWorktree {
        /// Selected worktree path.
        path: std::path::PathBuf,
        /// Attach result.
        result: Result<SessionSummary, ClientError>,
    },
    /// Worktree creation completed.
    CreateWorktree {
        /// Worktree creation result.
        result: Result<WorktreeCreateResponse, ClientError>,
    },
    /// Result for active turn cancellation.
    CancelTurn {
        /// Session the request targeted.
        session_id: SessionId,
        /// Daemon response.
        result: Result<bool, ClientError>,
    },
    /// Result for a permission resolution.
    PermissionResolved {
        /// Permission request id.
        permission_id: String,
        /// Whether the request was approved.
        approved: bool,
        /// Whether the decision was remembered.
        remember: bool,
        /// Whether the action targeted the authorization batch.
        apply_to_batch: bool,
        /// Daemon response indicating whether any request was resolved.
        result: Result<bool, ClientError>,
    },
}

#[allow(clippy::match_same_arms)]
impl TuiEffectResult {
    /// Return the daemon connectivity observation implied by this effect result.
    #[must_use]
    pub fn daemon_observation(&self) -> DaemonObservation {
        match self {
            Self::SessionOpenProgress { .. } => DaemonObservation::Success,
            Self::SessionOpened { result, .. } => DaemonObservation::from_tui_result(result),
            Self::DraftStatusLoaded {
                daemon_connected,
                error,
                ..
            } => DaemonObservation::from_optional_error(*daemon_connected, error.as_deref()),
            Self::SessionStatusLoaded {
                daemon_connected,
                hydration,
                ..
            } => DaemonObservation::from_optional_error(
                *daemon_connected,
                hydration.error.as_deref(),
            ),
            Self::SessionModelStatusLoaded { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::PluginStatusLoaded { error, .. } => {
                error.as_ref().map_or(DaemonObservation::Success, |error| {
                    DaemonObservation::Failed(error.clone())
                })
            }
            Self::AgentCatalogLoaded { agents } => match agents {
                Ok(_) => DaemonObservation::Success,
                Err(error) => DaemonObservation::Unavailable(error.clone()),
            },
            Self::OlderHistoryLoaded { result, .. } | Self::NewerHistoryLoaded { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::PermissionList { result } => DaemonObservation::from_client_result(result),
            Self::SaveDraft { result, .. } => DaemonObservation::from_client_result(result),
            Self::RenameSession { result } => DaemonObservation::from_client_result(result),
            Self::DeleteSession { result, .. } => DaemonObservation::from_client_result(result),
            Self::ForkSession { result, .. } => DaemonObservation::from_client_result(result),
            Self::CloneSession { result, .. } => DaemonObservation::from_client_result(result),
            Self::SkillAction { result, .. } => DaemonObservation::from_client_result(result),
            Self::SetSessionModel { result, .. } => DaemonObservation::from_client_result(result),
            Self::SetSessionReasoning { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::AppendPresentationNote { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::SubmitMessage { result, .. } => DaemonObservation::from_client_result(result),
            Self::CompactContext { result, .. } => DaemonObservation::from_client_result(result),
            Self::CancelRuntimeWork { result, .. } => DaemonObservation::from_client_result(result),
            Self::AttachWorktree { result, .. } => DaemonObservation::from_client_result(result),
            Self::CreateWorktree { result } => DaemonObservation::from_client_result(result),
            Self::CancelTurn { result, .. } | Self::PermissionResolved { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::ConfigLoaded { .. }
            | Self::AuthSecurityReconciled { .. }
            | Self::SlashPaletteLoaded { .. }
            | Self::CommandPaletteLoaded { .. }
            | Self::SlashCommandExecuted { .. }
            | Self::PluginCommandInvoked { .. } => DaemonObservation::None,
        }
    }
}

/// Skill action effect success payload.
#[derive(Debug)]
pub struct SkillActionResult {
    /// Session that received the skill action.
    pub session_id: SessionId,
    /// Newly-created/attached session summary, if the action created a session.
    pub created_session: Option<SessionSummary>,
    /// Event stream task for a newly-created session.
    pub event_task: Option<JoinHandle<()>>,
    /// Invocation acceptance when invoking a skill.
    pub acceptance: Option<MessageAcceptance>,
    /// Agent committed during invocation.
    pub committed_agent_id: Option<String>,
    /// Pending reasoning effort generation committed during invocation.
    pub committed_reasoning_effort_generation: Option<u64>,
    /// Releases a newly-created session stream after the TUI installs the session id.
    pub event_stream_release: Option<oneshot::Sender<()>>,
}

/// Attached session status hydration payload.
#[derive(Debug)]
pub struct SessionStatusHydration {
    /// Model status, if available.
    pub model: Option<bcode_ipc::SessionModelStatus>,
    /// Active skills captured during bounded attach hydration.
    pub active_skills: Option<Vec<bcode_skill_models::SkillContextResponse>>,
    /// Runtime work snapshots, if available.
    pub runtime_work: Option<Vec<bcode_ipc::RuntimeWorkSnapshot>>,
    /// Pending interactive requests, if available.
    pub interactions: Option<Vec<bcode_session_view_models::InteractionViewSummary>>,
    /// Active plugin-owned status contributions.
    pub plugin_status: Vec<bcode_session_view_models::PluginStatusView>,
    /// First non-critical error encountered.
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct SubmitMessageResult {
    /// Session that received the message.
    pub session_id: SessionId,
    /// Newly-created/attached session summary, if the submit created a session.
    pub created_session: Option<SessionSummary>,
    /// Server acceptance for the submitted message.
    pub acceptance: MessageAcceptance,
    /// Agent committed during submission.
    pub committed_agent_id: Option<String>,
    /// Pending reasoning effort generation committed during submission.
    pub committed_reasoning_effort_generation: Option<u64>,
    /// Event stream task for a newly-created session.
    pub event_task: Option<JoinHandle<()>>,
    /// Releases a newly-created session stream after the TUI installs the session id.
    pub event_stream_release: Option<oneshot::Sender<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum EffectKey {
    SessionOpen,
    Config,
    AuthSecurity,
    DraftStatus,
    SessionStatus,
    SessionModelStatus,
    PluginStatus,
    AgentCatalog,
    OlderHistory,
    NewerHistory,
    PermissionList,
    DraftSave,
    SlashPalette,
    CommandPalette,
    SlashCommand,
    PluginCommand(String, String),
    RenameSession(SessionId),
    DeleteSession(SessionId),
    ForkSession(SessionId),
    CloneSession(SessionId),
    SubmitMessage(usize),
    SkillAction(SkillId),
    SetSessionModel(SessionId),
    SetSessionReasoning(SessionId),
    AppendPresentationNote(SessionId, String),
    CancelRuntimeWork(SessionId),
    CompactContext(SessionId),
    AttachWorktree(SessionId),
    CreateWorktree,
    CancelTurn(SessionId),
    ResolvePermission(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectSchedule {
    StartIfIdle,
    Replace,
    QueueLatest,
    Cancel,
}

/// Daemon-backed effect scheduling class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectDaemonIntent {
    /// Use the background client clone for non-foreground work.
    Background,
    /// Use the foreground client clone for explicit user actions.
    Foreground,
}

impl TuiEffect {
    #[allow(clippy::too_many_lines, dead_code)]
    fn daemon_start_failed(self, client_error: ClientError) -> TuiEffectResult {
        match self {
            Self::OpenSession { session_id, .. } => TuiEffectResult::SessionOpened {
                session_id,
                has_older_history: true,
                result: Err(TuiError::Client(client_error)),
            },
            Self::LoadDraftStatus { .. } => TuiEffectResult::DraftStatusLoaded {
                daemon_connected: false,
                model: None,
                composer_draft: None,
                error: Some(client_error.to_string()),
            },
            Self::LoadSessionStatus { session_id } => TuiEffectResult::SessionStatusLoaded {
                daemon_connected: false,
                session_id,
                hydration: Box::new(SessionStatusHydration {
                    model: None,
                    active_skills: None,
                    runtime_work: None,
                    interactions: None,
                    plugin_status: Vec::new(),
                    error: Some(client_error.to_string()),
                }),
            },
            Self::LoadSessionModelStatus { session_id } => {
                TuiEffectResult::SessionModelStatusLoaded {
                    session_id,
                    result: Err(client_error),
                }
            }
            Self::LoadPluginStatus { session_id } => TuiEffectResult::PluginStatusLoaded {
                session_id,
                plugin_status: Vec::new(),
                error: Some(client_error.to_string()),
            },
            Self::LoadAgentCatalog => TuiEffectResult::AgentCatalogLoaded {
                agents: Err(client_error.to_string()),
            },
            Self::RenameSession { .. } => TuiEffectResult::RenameSession {
                result: Err(client_error),
            },
            Self::DeleteSession { session_id } => TuiEffectResult::DeleteSession {
                session_id,
                result: Err(client_error),
            },
            Self::ForkSession {
                switch_after_create,
                install_draft,
                draft,
                initial_window_request,
                ..
            } => TuiEffectResult::ForkSession {
                switch_after_create,
                install_draft,
                draft,
                initial_window_request,
                result: Err(client_error),
            },
            Self::CloneSession {
                switch_after_create,
                install_draft,
                initial_window_request,
                ..
            } => TuiEffectResult::CloneSession {
                switch_after_create,
                install_draft,
                initial_window_request,
                result: Err(client_error),
            },
            Self::SubmitMessage { request } => TuiEffectResult::SubmitMessage {
                message: request.message,
                result: Box::new(Err(client_error)),
            },
            Self::SkillAction { request } => TuiEffectResult::SkillAction {
                action: request.action,
                skill_id: request.skill_id,
                result: Box::new(Err(client_error)),
            },
            Self::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
            } => TuiEffectResult::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
                result: Err(client_error),
            },
            Self::SetSessionReasoning {
                session_id,
                effort,
                effort_generation,
                status,
                ..
            } => TuiEffectResult::SetSessionReasoning {
                session_id,
                effort,
                effort_generation,
                status,
                result: Err(client_error),
            },
            Self::AppendPresentationNote { session_id, .. } => {
                TuiEffectResult::AppendPresentationNote {
                    session_id,
                    result: Err(client_error),
                }
            }
            Self::CancelRuntimeWork { work_id, .. } => TuiEffectResult::CancelRuntimeWork {
                work_id,
                result: Err(client_error),
            },
            Self::CompactContext { session_id } => TuiEffectResult::CompactContext {
                session_id,
                result: Err(client_error),
            },
            Self::AttachWorktree { path, .. } => TuiEffectResult::AttachWorktree {
                path,
                result: Err(client_error),
            },
            Self::CreateWorktree { .. } => TuiEffectResult::CreateWorktree {
                result: Err(client_error),
            },
            Self::CancelTurn { session_id } => TuiEffectResult::CancelTurn {
                session_id,
                result: Err(client_error),
            },
            Self::ResolvePermission {
                permission_id,
                approved,
                remember,
                apply_to_batch,
                ..
            } => TuiEffectResult::PermissionResolved {
                permission_id,
                approved,
                remember,
                apply_to_batch,
                result: Err(client_error),
            },
            Self::LoadConfig
            | Self::ReconcileAuthSecurity { .. }
            | Self::LoadOlderHistory { .. }
            | Self::LoadNewerHistory { .. }
            | Self::ListPermissions
            | Self::SaveDraft { .. }
            | Self::LoadSlashPalette { .. }
            | Self::LoadCommandPalette
            | Self::ExecuteSlashCommand { .. }
            | Self::InvokePluginCommand { .. } => {
                unreachable!("daemon start failure for non-foreground effect")
            }
        }
    }

    const fn daemon_intent(&self) -> EffectDaemonIntent {
        match self {
            Self::OpenSession {
                allow_daemon_start: true,
                ..
            }
            | Self::LoadDraftStatus { .. }
            | Self::LoadSessionStatus { .. }
            | Self::LoadAgentCatalog
            | Self::RenameSession { .. }
            | Self::DeleteSession { .. }
            | Self::ForkSession { .. }
            | Self::CloneSession { .. }
            | Self::SubmitMessage { .. }
            | Self::SkillAction { .. }
            | Self::SetSessionModel { .. }
            | Self::SetSessionReasoning { .. }
            | Self::AppendPresentationNote { .. }
            | Self::CancelRuntimeWork { .. }
            | Self::CompactContext { .. }
            | Self::AttachWorktree { .. }
            | Self::CreateWorktree { .. }
            | Self::CancelTurn { .. }
            | Self::ResolvePermission { .. }
            | Self::InvokePluginCommand { .. } => EffectDaemonIntent::Foreground,
            Self::OpenSession {
                allow_daemon_start: false,
                ..
            }
            | Self::LoadConfig
            | Self::ReconcileAuthSecurity { .. }
            | Self::LoadSessionModelStatus { .. }
            | Self::LoadPluginStatus { .. }
            | Self::LoadOlderHistory { .. }
            | Self::LoadNewerHistory { .. }
            | Self::ListPermissions
            | Self::SaveDraft { .. }
            | Self::LoadSlashPalette { .. }
            | Self::LoadCommandPalette
            | Self::ExecuteSlashCommand { .. } => EffectDaemonIntent::Background,
        }
    }
}

type OrderedPresentationEffects = BTreeMap<SessionId, VecDeque<TuiEffect>>;
type ScheduledEffects = Vec<(EffectSchedule, TuiEffect)>;

/// Queue of effects requested before the chat loop runner can start them.
///
/// The queue keeps only the latest pending request for each effect key. This
/// mirrors runner semantics and avoids spawning then immediately aborting stale
/// work when multiple state transitions request the same background effect
/// before the loop has a chance to drain the queue.
#[derive(Default)]
pub struct TuiEffectQueue {
    effects: BTreeMap<EffectKey, (EffectSchedule, TuiEffect)>,
}

impl TuiEffectQueue {
    /// Queue an effect using normal start-if-idle scheduling.
    pub fn start(&mut self, effect: TuiEffect) {
        self.push(effect, EffectSchedule::StartIfIdle);
    }

    /// Queue an effect that should replace any in-flight effect with the same key.
    pub fn replace(&mut self, effect: TuiEffect) {
        self.push(effect, EffectSchedule::Replace);
    }

    /// Queue the latest effect with this key to run after the current one finishes.
    pub fn queue_latest(&mut self, effect: TuiEffect) {
        self.push(effect, EffectSchedule::QueueLatest);
    }

    /// Queue an ordered presentation note without collapsing another note for the same session.
    pub fn start_ordered(&mut self, effect: TuiEffect) {
        let key = effect.key();
        debug_assert!(matches!(key, EffectKey::AppendPresentationNote(_, _)));
        self.effects
            .insert(key, (EffectSchedule::QueueLatest, effect));
    }

    /// Cancel active and pending work matching this effect key.
    pub fn cancel(&mut self, effect: TuiEffect) {
        self.push(effect, EffectSchedule::Cancel);
    }

    fn push(&mut self, effect: TuiEffect, schedule: EffectSchedule) {
        self.effects.insert(effect.key(), (schedule, effect));
    }

    /// Return whether an open-session effect is queued for the given session.
    #[cfg(test)]
    pub fn has_open_session(&self, session_id: SessionId) -> bool {
        self.effects.values().any(|(_, effect)| {
            matches!(
                effect,
                TuiEffect::OpenSession {
                    session_id: queued,
                    ..
                } if *queued == session_id
            )
        })
    }

    /// Return the projection request queued for one open session.
    #[cfg(test)]
    pub fn open_session_request(&self, session_id: SessionId) -> Option<&ProjectionWindowRequest> {
        self.effects.values().find_map(|(_, effect)| match effect {
            TuiEffect::OpenSession {
                session_id: queued,
                initial_window_request,
                ..
            } if *queued == session_id => Some(initial_window_request),
            _ => None,
        })
    }

    /// Drain non-note effects while retaining ordered presentation notes for serialized release.
    pub(super) fn drain_runtime(&mut self) -> (ScheduledEffects, OrderedPresentationEffects) {
        let mut effects = Vec::new();
        let mut notes = BTreeMap::<SessionId, VecDeque<TuiEffect>>::new();
        for (schedule, effect) in std::mem::take(&mut self.effects).into_values() {
            if let TuiEffect::AppendPresentationNote { session_id, .. } = &effect {
                notes.entry(*session_id).or_default().push_back(effect);
            } else {
                effects.push((schedule, effect));
            }
        }
        (effects, notes)
    }

    /// Drain queued effects.
    pub(super) fn drain(&mut self) -> Vec<(EffectSchedule, TuiEffect)> {
        std::mem::take(&mut self.effects).into_values().collect()
    }
}

const TUI_EFFECT_STREAM_CAPACITY: usize = 64;

pub struct TuiEffectRunner {
    foreground_client: BcodeClient,
    passive_client: BcodeClient,
    tasks: BTreeMap<EffectKey, tokio::task::JoinHandle<TuiEffectResult>>,
    streaming_sender: mpsc::Sender<TuiEffectResult>,
    streaming_receiver: mpsc::Receiver<TuiEffectResult>,
    queued_latest: BTreeMap<EffectKey, TuiEffect>,
    queued_presentation_notes: BTreeMap<SessionId, VecDeque<TuiEffect>>,
}

impl TuiEffectRunner {
    /// Create an effect runner using foreground and passive clients.
    #[must_use]
    pub fn new(foreground_client: &BcodeClient, passive_client: &BcodeClient) -> Self {
        let (streaming_sender, streaming_receiver) = mpsc::channel(TUI_EFFECT_STREAM_CAPACITY);
        Self {
            foreground_client: foreground_client.clone(),
            passive_client: passive_client.clone(),
            tasks: BTreeMap::new(),
            streaming_sender,
            streaming_receiver,
            queued_latest: BTreeMap::new(),
            queued_presentation_notes: BTreeMap::new(),
        }
    }

    /// Return a clone of the foreground client used for user-requested work.
    #[must_use]
    pub fn foreground_client(&self) -> BcodeClient {
        self.foreground_client.clone()
    }

    /// Return the bounded streaming completion capacity.
    #[cfg(test)]
    fn streaming_capacity(&self) -> usize {
        self.streaming_receiver.max_capacity()
    }

    /// Start an effect if another effect with the same key is not running.
    pub fn start(&mut self, effect: TuiEffect) -> bool {
        let key = effect.key();
        if let TuiEffect::AppendPresentationNote { session_id, .. } = &effect {
            let session_id = *session_id;
            let active = self
                .tasks
                .keys()
                .any(|key| matches!(key, EffectKey::AppendPresentationNote(active, _) if *active == session_id));
            let queue = self
                .queued_presentation_notes
                .entry(session_id)
                .or_default();
            if active || !queue.is_empty() {
                queue.push_back(effect);
                return false;
            }
        }
        if self.tasks.contains_key(&key) {
            return false;
        }
        self.spawn(key, effect);
        true
    }

    /// Replace any in-flight effect with the same key.
    pub fn replace(&mut self, effect: TuiEffect) {
        let key = effect.key();
        if let Some(task) = self.tasks.remove(&key) {
            task.abort();
        }
        self.spawn(key, effect);
    }

    /// Queue the latest effect with this key to run after the current one finishes.
    pub fn queue_latest(&mut self, effect: TuiEffect) -> bool {
        let key = effect.key();
        if self.tasks.contains_key(&key) {
            self.queued_latest.insert(key, effect);
            return false;
        }
        self.spawn(key, effect);
        true
    }

    /// Abort an in-flight effect with the same key as the supplied effect.
    pub fn abort_matching(&mut self, effect: &TuiEffect) {
        if let Some(task) = self.tasks.remove(&effect.key()) {
            task.abort();
        }
        self.queued_latest.remove(&effect.key());
    }

    fn spawn(&mut self, key: EffectKey, effect: TuiEffect) {
        let daemon_intent = effect.daemon_intent();
        let client = match daemon_intent {
            EffectDaemonIntent::Background => self.passive_client.clone(),
            EffectDaemonIntent::Foreground => self.foreground_client.clone(),
        };
        let streaming_sender = self.streaming_sender.clone();
        let task =
            tokio::spawn(async move { Box::pin(effect.run(client, streaming_sender)).await });
        self.tasks.insert(key, task);
    }

    /// Convert queued non-note Bcode effects into runtime-owned commands and return ordered notes.
    pub fn runtime_work(
        &self,
        pending_effects: &mut TuiEffectQueue,
        handle: &bmux_tui_runtime::RuntimeHandle<super::root_program::BcodeRuntimeMessage>,
    ) -> (
        Vec<bmux_tui_runtime::Command<super::root_program::BcodeRuntimeMessage>>,
        BTreeMap<SessionId, VecDeque<TuiEffect>>,
    ) {
        let (effects, notes) = pending_effects.drain_runtime();
        let commands = effects
            .into_iter()
            .map(|(schedule, effect)| {
                effect.command(
                    schedule,
                    &self.foreground_client,
                    &self.passive_client,
                    handle.clone(),
                )
            })
            .collect();
        (commands, notes)
    }

    pub fn ordered_command(
        &self,
        effect: TuiEffect,
        handle: &bmux_tui_runtime::RuntimeHandle<super::root_program::BcodeRuntimeMessage>,
    ) -> bmux_tui_runtime::Command<super::root_program::BcodeRuntimeMessage> {
        effect.command(
            EffectSchedule::StartIfIdle,
            &self.foreground_client,
            &self.passive_client,
            handle.clone(),
        )
    }

    /// Poll completed effects without blocking on running tasks.
    pub async fn poll_finished(&mut self) -> Vec<TuiEffectResult> {
        let finished = self
            .tasks
            .iter()
            .filter_map(|(key, task)| task.is_finished().then_some(key.clone()))
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(finished.len());
        while let Ok(result) = self.streaming_receiver.try_recv() {
            results.push(result);
        }
        results.reserve(finished.len());
        for key in finished {
            let Some(task) = self.tasks.remove(&key) else {
                continue;
            };
            match task.await {
                Ok(result) => results.push(result),
                Err(_error) => {}
            }
            if let Some(effect) = self.queued_latest.remove(&key) {
                self.spawn(key.clone(), effect);
            }
            let EffectKey::AppendPresentationNote(session_id, _) = key else {
                continue;
            };
            let next = self
                .queued_presentation_notes
                .get_mut(&session_id)
                .and_then(VecDeque::pop_front);
            if let Some(effect) = next {
                let key = effect.key();
                self.spawn(key, effect);
            } else {
                self.queued_presentation_notes.remove(&session_id);
            }
        }
        results
    }

    /// Start all pending effects produced before or during the loop iteration.
    ///
    /// Returns true when at least one pending effect was started.
    pub fn drain_pending(&mut self, pending_effects: &mut TuiEffectQueue) -> bool {
        let mut started = false;
        for (schedule, effect) in pending_effects.drain() {
            match schedule {
                EffectSchedule::StartIfIdle => {
                    started |= self.start(effect);
                }
                EffectSchedule::Replace => {
                    self.replace(effect);
                    started = true;
                }
                EffectSchedule::QueueLatest => {
                    started |= self.queue_latest(effect);
                }
                EffectSchedule::Cancel => {
                    self.abort_matching(&effect);
                }
            }
        }
        started
    }

    /// Abort all in-flight effects.
    pub fn abort_all(&mut self) {
        self.queued_latest.clear();
        self.queued_presentation_notes.clear();
        for (_key, task) in std::mem::take(&mut self.tasks) {
            task.abort();
        }
    }
}

impl TuiEffect {
    pub(super) fn command_key(&self) -> bmux_tui_runtime::CommandKey {
        let key = match self {
            Self::AppendPresentationNote { session_id, .. } => {
                return bmux_tui_runtime::CommandKey::new(format!(
                    "bcode.effect.AppendPresentationNote({session_id})"
                ));
            }
            _ => self.key(),
        };
        bmux_tui_runtime::CommandKey::new(format!("bcode.effect.{key:?}"))
    }

    pub(super) fn command(
        self,
        schedule: EffectSchedule,
        foreground_client: &BcodeClient,
        passive_client: &BcodeClient,
        handle: bmux_tui_runtime::RuntimeHandle<super::root_program::BcodeRuntimeMessage>,
    ) -> bmux_tui_runtime::Command<super::root_program::BcodeRuntimeMessage> {
        let key = self.command_key();
        let client = match self.daemon_intent() {
            EffectDaemonIntent::Background => passive_client.clone(),
            EffectDaemonIntent::Foreground => foreground_client.clone(),
        };
        let future = async move {
            let (streaming_sender, mut streaming_receiver) =
                mpsc::channel(TUI_EFFECT_STREAM_CAPACITY);
            let effect = Box::pin(self.run(client, streaming_sender));
            tokio::pin!(effect);
            loop {
                tokio::select! {
                    result = &mut effect => {
                        break Some(super::root_program::BcodeRuntimeMessage::EffectCompleted(
                            Box::new(result),
                        ));
                    }
                    progress = streaming_receiver.recv() => {
                        let Some(progress) = progress else {
                            continue;
                        };
                        if handle
                            .send(super::root_program::BcodeRuntimeMessage::EffectCompleted(
                                Box::new(progress),
                            ))
                            .await
                            .is_err()
                        {
                            break None;
                        }
                    }
                }
            }
        };
        match schedule {
            EffectSchedule::StartIfIdle => bmux_tui_runtime::Command::start_if_idle(key, future),
            EffectSchedule::Replace => bmux_tui_runtime::Command::replace(key, future),
            EffectSchedule::QueueLatest => bmux_tui_runtime::Command::queue_latest(key, future),
            EffectSchedule::Cancel => bmux_tui_runtime::Command::cancel(key),
        }
    }

    fn key(&self) -> EffectKey {
        match self {
            Self::OpenSession { .. } => EffectKey::SessionOpen,
            Self::LoadConfig => EffectKey::Config,
            Self::ReconcileAuthSecurity { .. } => EffectKey::AuthSecurity,
            Self::LoadDraftStatus { .. } => EffectKey::DraftStatus,
            Self::LoadSessionStatus { .. } => EffectKey::SessionStatus,
            Self::LoadSessionModelStatus { .. } => EffectKey::SessionModelStatus,
            Self::LoadPluginStatus { .. } => EffectKey::PluginStatus,
            Self::LoadAgentCatalog => EffectKey::AgentCatalog,
            Self::LoadOlderHistory { .. } => EffectKey::OlderHistory,
            Self::LoadNewerHistory { .. } => EffectKey::NewerHistory,
            Self::ListPermissions => EffectKey::PermissionList,
            Self::SaveDraft { .. } => EffectKey::DraftSave,
            Self::LoadSlashPalette { .. } => EffectKey::SlashPalette,
            Self::LoadCommandPalette => EffectKey::CommandPalette,
            Self::ExecuteSlashCommand { .. } => EffectKey::SlashCommand,
            Self::InvokePluginCommand {
                plugin_id,
                command_id,
                ..
            } => EffectKey::PluginCommand(plugin_id.clone(), command_id.clone()),
            Self::RenameSession { session_id, .. } => EffectKey::RenameSession(*session_id),
            Self::DeleteSession { session_id } => EffectKey::DeleteSession(*session_id),
            Self::ForkSession { session_id, .. } => EffectKey::ForkSession(*session_id),
            Self::CloneSession { session_id, .. } => EffectKey::CloneSession(*session_id),
            Self::SubmitMessage { request } => EffectKey::SubmitMessage(request.message.len()),
            Self::SkillAction { request } => EffectKey::SkillAction(request.skill_id.clone()),
            Self::SetSessionModel { session_id, .. } => EffectKey::SetSessionModel(*session_id),
            Self::SetSessionReasoning { session_id, .. } => {
                EffectKey::SetSessionReasoning(*session_id)
            }
            Self::AppendPresentationNote {
                session_id,
                note_id,
                ..
            } => EffectKey::AppendPresentationNote(*session_id, note_id.clone()),
            Self::CancelRuntimeWork { session_id, .. } => EffectKey::CancelRuntimeWork(*session_id),
            Self::CompactContext { session_id } => EffectKey::CompactContext(*session_id),
            Self::AttachWorktree { session_id, .. } => EffectKey::AttachWorktree(*session_id),
            Self::CreateWorktree { .. } => EffectKey::CreateWorktree,
            Self::CancelTurn { session_id } => EffectKey::CancelTurn(*session_id),
            Self::ResolvePermission { permission_id, .. } => {
                EffectKey::ResolvePermission(permission_id.clone())
            }
        }
    }

    async fn run_session_status_effect(
        client: &BcodeClient,
        session_id: SessionId,
    ) -> TuiEffectResult {
        Box::pin(load_session_status(client, session_id)).await
    }

    #[allow(clippy::too_many_lines, clippy::large_stack_frames)]
    async fn run(
        self,
        client: BcodeClient,
        streaming_sender: mpsc::Sender<TuiEffectResult>,
    ) -> TuiEffectResult {
        match self {
            Self::OpenSession {
                session_id,
                initial_window_request,
                event_sender,
                allow_daemon_start: _,
            } => TuiEffectResult::SessionOpened {
                session_id,
                has_older_history: true,
                result: history_flow::attach_session_event_stream_with_window_request(
                    &client,
                    session_id,
                    event_sender,
                    initial_window_request,
                    |snapshot| {
                        let _result =
                            streaming_sender.try_send(TuiEffectResult::SessionOpenProgress {
                                snapshot: snapshot.clone(),
                            });
                    },
                )
                .await,
            },
            Self::LoadConfig => TuiEffectResult::ConfigLoaded {
                config: Box::new(bcode_config::load_config().map_err(|error| error.to_string())),
            },
            Self::ReconcileAuthSecurity { config } => TuiEffectResult::AuthSecurityReconciled {
                status: session_flow::auth_security_status(&config),
            },
            Self::LoadDraftStatus {
                launch_working_directory,
            } => load_draft_status(&client, launch_working_directory).await,
            Self::LoadSessionStatus { session_id } => {
                Box::pin(Self::run_session_status_effect(&client, session_id)).await
            }
            Self::LoadSessionModelStatus { session_id } => {
                TuiEffectResult::SessionModelStatusLoaded {
                    session_id,
                    result: client.session_model_status(session_id).await,
                }
            }
            Self::LoadPluginStatus { session_id } => {
                let (plugin_status, error) = load_plugin_session_status(&client, session_id).await;
                TuiEffectResult::PluginStatusLoaded {
                    session_id,
                    plugin_status,
                    error,
                }
            }
            Self::LoadAgentCatalog => TuiEffectResult::AgentCatalogLoaded {
                agents: AgentCatalog::load(&client)
                    .await
                    .map_err(|error| error.to_string()),
            },
            Self::LoadOlderHistory { session_id, cursor } => TuiEffectResult::OlderHistoryLoaded {
                session_id,
                result: client
                    .session_history_page(
                        session_id,
                        SessionHistoryQuery {
                            cursor: Some(cursor),
                            limit: super::OLDER_HISTORY_EVENT_LIMIT,
                            direction: SessionHistoryDirection::Backward,
                        },
                    )
                    .await,
            },
            Self::LoadNewerHistory { session_id, cursor } => TuiEffectResult::NewerHistoryLoaded {
                session_id,
                result: client
                    .session_history_page(
                        session_id,
                        SessionHistoryQuery {
                            cursor: Some(cursor),
                            limit: super::OLDER_HISTORY_EVENT_LIMIT,
                            direction: SessionHistoryDirection::Forward,
                        },
                    )
                    .await,
            },
            Self::ListPermissions => TuiEffectResult::PermissionList {
                result: client.list_permissions().await,
            },
            Self::SaveDraft { scope, text } => {
                let scope = match scope {
                    ComposerDraftScope::Session { session_id } => {
                        bcode_session_view_models::ComposerDraftViewScope::Session { session_id }
                    }
                    ComposerDraftScope::DraftSession {
                        launch_working_directory,
                    } => bcode_session_view_models::ComposerDraftViewScope::DraftSession {
                        launch_working_directory,
                    },
                };
                let result = execute_session_view_action(
                    &client,
                    SessionViewAction::UpdateDraft {
                        scope,
                        text: text.clone(),
                    },
                )
                .await
                .map(|_| ());
                TuiEffectResult::SaveDraft { text, result }
            }
            Self::LoadSlashPalette { query, session_id } => {
                let palette = slash_palette::SlashPalette::new(&client, session_id, &query).await;
                TuiEffectResult::SlashPaletteLoaded { query, palette }
            }
            Self::LoadCommandPalette => TuiEffectResult::CommandPaletteLoaded {
                result: client
                    .plugin_contributions()
                    .await
                    .map(|contributions| contributions.command_contributions),
            },
            Self::ExecuteSlashCommand {
                session_id,
                working_directory,
                current_agent_id,
                reasoning_display_mode,
                reasoning_visible,
                message,
            } => {
                let result = match super::slash_registry::resolve(&client, &message).await {
                    Ok(resolution) => {
                        super::slash_commands::execute_resolved(
                            &client,
                            session_id,
                            super::slash_commands::SlashExecutionContext {
                                working_directory: &working_directory,
                                current_agent_id: &current_agent_id,
                                reasoning_display_mode,
                                reasoning_visible,
                            },
                            &message,
                            resolution,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                TuiEffectResult::SlashCommandExecuted { message, result }
            }
            Self::InvokePluginCommand {
                plugin_id,
                command_id,
                arguments,
                working_directory,
                session_id,
            } => {
                let mut args = BTreeMap::new();
                args.insert("cwd".to_owned(), working_directory.display().to_string());
                if let Some(session_id) = session_id {
                    args.insert("session_id".to_owned(), session_id.to_string());
                }
                if let Some(arguments) = arguments.filter(|value| !value.is_empty()) {
                    args.insert("arguments".to_owned(), arguments);
                }
                let result = async {
                    let payload = serde_json::to_vec(&bcode_command::InvokeCommandRequest {
                        command_id,
                        args,
                    })?;
                    let response = client
                        .invoke_plugin_service(
                            plugin_id.clone(),
                            bcode_command::COMMAND_INTERFACE_ID.to_owned(),
                            bcode_command::OP_INVOKE_COMMAND.to_owned(),
                            payload,
                        )
                        .await?;
                    if let Some(error) = response.error {
                        return Err(TuiError::PluginService {
                            code: error.code,
                            message: error.message,
                        });
                    }
                    Ok(serde_json::from_slice::<
                        bcode_command::InvokeCommandResponse,
                    >(&response.payload)?)
                }
                .await;
                TuiEffectResult::PluginCommandInvoked { plugin_id, result }
            }
            Self::RenameSession { session_id, name } => TuiEffectResult::RenameSession {
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::RenameSession { session_id, name },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::SessionRenamed { session }) => Ok(*session),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::DeleteSession { session_id } => TuiEffectResult::DeleteSession {
                session_id,
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::DeleteSession { session_id },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::SessionDeleted { session }) => Ok(*session),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::ForkSession {
                session_id,
                prompt_sequence,
                name,
                draft,
                switch_after_create,
                install_draft,
                initial_window_request,
            } => TuiEffectResult::ForkSession {
                switch_after_create,
                install_draft,
                draft,
                initial_window_request,
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::ForkSession {
                        session_id,
                        prompt_sequence,
                        name,
                    },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::SessionForked { fork }) => Ok(*fork),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::CloneSession {
                session_id,
                name,
                switch_after_create,
                install_draft,
                initial_window_request,
            } => TuiEffectResult::CloneSession {
                switch_after_create,
                install_draft,
                initial_window_request,
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::CloneSession { session_id, name },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::SessionCloned { fork }) => Ok(*fork),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::SubmitMessage { request } => run_submit_message(&client, *request).await,
            Self::SkillAction { request } => run_skill_action(&client, *request).await,
            Self::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
            } => TuiEffectResult::SetSessionModel {
                session_id,
                provider_plugin_id: provider_plugin_id.clone(),
                model_id: model_id.clone(),
                result: execute_session_view_action(
                    &client,
                    SessionViewAction::SetModel {
                        session_id,
                        provider_plugin_id,
                        model_id,
                    },
                )
                .await
                .map(|_| ()),
            },
            Self::SetSessionReasoning {
                session_id,
                effort,
                summary,
                effort_generation,
                status,
            } => TuiEffectResult::SetSessionReasoning {
                session_id,
                effort: effort.clone(),
                effort_generation,
                status,
                result: execute_session_view_action(
                    &client,
                    SessionViewAction::SetReasoning {
                        session_id,
                        effort,
                        summary,
                    },
                )
                .await
                .map(|_| ()),
            },
            Self::AppendPresentationNote {
                session_id,
                source_id,
                note_id,
                text,
                format,
            } => TuiEffectResult::AppendPresentationNote {
                session_id,
                result: client
                    .append_presentation_note(session_id, source_id, note_id, text, format)
                    .await,
            },
            Self::CancelRuntimeWork {
                session_id,
                work_id,
            } => TuiEffectResult::CancelRuntimeWork {
                work_id: work_id.clone(),
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::CancelRuntimeWork {
                        session_id,
                        work_id,
                    },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::RuntimeWorkCancellationRequested {
                        cancelled,
                    }) => Ok(cancelled),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::CompactContext { session_id } => TuiEffectResult::CompactContext {
                session_id,
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::CompactContext { session_id },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::ContextCompacted { message }) => Ok(message),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::AttachWorktree { session_id, path } => TuiEffectResult::AttachWorktree {
                path: path.clone(),
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::ChangeWorkingDirectory { session_id, path },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::WorkingDirectoryChanged { session }) => {
                        Ok(*session)
                    }
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::CreateWorktree { request } => TuiEffectResult::CreateWorktree {
                result: client.create_worktree(request).await,
            },
            Self::CancelTurn { session_id } => TuiEffectResult::CancelTurn {
                session_id,
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::CancelTurn {
                        session_id,
                        clear_queue: true,
                    },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::Cancelled { cancelled }) => Ok(cancelled),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::ResolvePermission {
                permission_id,
                approved,
                remember,
                apply_to_batch,
                batch_id,
            } => {
                let result = if apply_to_batch {
                    let Some(batch_id) = batch_id else {
                        return TuiEffectResult::PermissionResolved {
                            permission_id,
                            approved,
                            remember,
                            apply_to_batch,
                            result: Err(ClientError::UnexpectedResponse),
                        };
                    };
                    match execute_session_view_action(
                        &client,
                        SessionViewAction::ResolvePermissionBatch { batch_id, approved },
                    )
                    .await
                    {
                        Ok(SessionViewActionOutcome::PermissionBatchResolved {
                            resolved_count,
                        }) => Ok(resolved_count > 0),
                        Ok(_) => Err(ClientError::UnexpectedResponse),
                        Err(error) => Err(error),
                    }
                } else {
                    match execute_session_view_action(
                        &client,
                        SessionViewAction::ResolvePermission {
                            permission_id: permission_id.clone(),
                            approved,
                            remember,
                        },
                    )
                    .await
                    {
                        Ok(SessionViewActionOutcome::PermissionResolved { resolved }) => {
                            Ok(resolved)
                        }
                        Ok(_) => Err(ClientError::UnexpectedResponse),
                        Err(error) => Err(error),
                    }
                };
                TuiEffectResult::PermissionResolved {
                    permission_id,
                    approved,
                    remember,
                    apply_to_batch,
                    result,
                }
            }
        }
    }
}

async fn ensure_session_for_foreground_action(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    launch_working_directory: std::path::PathBuf,
    event_sender: mpsc::Sender<super::history_flow::SessionStreamUpdate>,
) -> Result<
    (
        SessionId,
        Option<SessionSummary>,
        Option<JoinHandle<()>>,
        Option<oneshot::Sender<()>>,
    ),
    ClientError,
> {
    if let Some(session_id) = session_id {
        return Ok((session_id, None, None, None));
    }
    let session = client
        .create_session_in_working_directory(None, launch_working_directory.clone())
        .await?;
    let _ = execute_session_view_action(
        client,
        SessionViewAction::UpdateDraft {
            scope: bcode_session_view_models::ComposerDraftViewScope::DraftSession {
                launch_working_directory,
            },
            text: String::new(),
        },
    )
    .await;
    let (attached, task, release) =
        history_flow::attach_paused_session_event_stream(client, session.id, event_sender)
            .await
            .map_err(|error| match error {
                TuiError::Client(error) => error,
                other => ClientError::Server {
                    code: "tui_session_attach_failed".to_owned(),
                    message: other.to_string(),
                },
            })?;
    Ok((
        session.id,
        Some(attached.session),
        Some(task),
        Some(release),
    ))
}

async fn run_skill_action(client: &BcodeClient, request: SkillActionRequest) -> TuiEffectResult {
    let action = request.action;
    let skill_id = request.skill_id.clone();
    TuiEffectResult::SkillAction {
        action,
        skill_id,
        result: Box::new(skill_action(client, request).await),
    }
}

async fn skill_action(
    client: &BcodeClient,
    request: SkillActionRequest,
) -> Result<SkillActionResult, ClientError> {
    let SkillActionRequest {
        session_id,
        launch_working_directory,
        skill_id,
        action,
        arguments,
        provider_plugin_id,
        model_id,
        agent_id,
        reasoning_effort,
        reasoning_summary,
        reasoning_effort_generation,
        event_sender,
    } = request;
    let (session_id, created_session, event_task, event_stream_release) =
        ensure_session_for_foreground_action(
            client,
            session_id,
            launch_working_directory,
            event_sender,
        )
        .await?;
    let acceptance = match action {
        SkillActionKind::Activate => {
            execute_session_view_action(
                client,
                SessionViewAction::ActivateSkill {
                    session_id,
                    skill_id: skill_id.to_string(),
                },
            )
            .await?;
            None
        }
        SkillActionKind::Deactivate => {
            execute_session_view_action(
                client,
                SessionViewAction::DeactivateSkill {
                    session_id,
                    skill_id: skill_id.to_string(),
                },
            )
            .await?;
            None
        }
        SkillActionKind::Invoke => {
            apply_submit_runtime_selections(
                client,
                session_id,
                provider_plugin_id,
                model_id,
                agent_id.clone(),
                reasoning_effort.clone(),
                reasoning_summary.clone(),
            )
            .await?;
            let display_text = if arguments.trim().is_empty() {
                format!("Invoke skill {skill_id}")
            } else {
                format!("Invoke skill {skill_id}: {arguments}")
            };
            let outcome = execute_session_view_action(
                client,
                SessionViewAction::InvokeSkill {
                    session_id,
                    skill_id: skill_id.to_string(),
                    arguments,
                    display_text,
                    execution: Box::new(bcode_session_models::TurnExecutionOptions {
                        reasoning: Some(Box::new(bcode_session_models::TurnReasoningOptions {
                            effort: reasoning_effort.clone(),
                            summary: reasoning_summary.clone(),
                        })),
                        ..bcode_session_models::TurnExecutionOptions::default()
                    }),
                },
            )
            .await?;
            Some(message_acceptance_from_action_outcome(&outcome)?)
        }
    };
    Ok(SkillActionResult {
        session_id,
        created_session,
        event_task,
        acceptance,
        committed_agent_id: (action == SkillActionKind::Invoke)
            .then_some(agent_id)
            .flatten(),
        committed_reasoning_effort_generation: (action == SkillActionKind::Invoke)
            .then_some(reasoning_effort_generation)
            .flatten(),
        event_stream_release,
    })
}

async fn run_submit_message(
    client: &BcodeClient,
    request: SubmitMessageRequest,
) -> TuiEffectResult {
    let message = request.message.clone();
    TuiEffectResult::SubmitMessage {
        message,
        result: Box::new(submit_message(client, request).await),
    }
}

async fn apply_submit_runtime_selections(
    client: &BcodeClient,
    session_id: SessionId,
    provider_plugin_id: Option<String>,
    model_id: Option<String>,
    agent_id: Option<String>,
    reasoning_effort: Option<String>,
    reasoning_summary: Option<String>,
) -> Result<(), ClientError> {
    if let Some(model_id) = model_id {
        execute_session_view_action(
            client,
            SessionViewAction::SetModel {
                session_id,
                provider_plugin_id,
                model_id,
            },
        )
        .await?;
    }
    if let Some(agent_id) = agent_id {
        execute_session_view_action(
            client,
            SessionViewAction::SetAgent {
                session_id,
                agent_id,
            },
        )
        .await?;
    }
    execute_session_view_action(
        client,
        SessionViewAction::SetReasoning {
            session_id,
            effort: reasoning_effort,
            summary: reasoning_summary,
        },
    )
    .await?;
    Ok(())
}

fn message_acceptance_from_action_outcome(
    outcome: &SessionViewActionOutcome,
) -> Result<MessageAcceptance, ClientError> {
    let SessionViewActionOutcome::MessageAccepted {
        queued,
        queue_position,
        disposition,
        ..
    } = outcome
    else {
        return Err(ClientError::UnexpectedResponse);
    };
    Ok(MessageAcceptance {
        queued: *queued,
        queue_position: queue_position.and_then(|position| u32::try_from(position).ok()),
        disposition: match disposition {
            bcode_session_view_models::MessageAcceptanceDispositionView::AppliedSteering => {
                bcode_ipc::MessageAcceptanceDisposition::AppliedSteering
            }
            bcode_session_view_models::MessageAcceptanceDispositionView::QueuedFollowUp => {
                bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp
            }
            bcode_session_view_models::MessageAcceptanceDispositionView::QueuedTurn => {
                bcode_ipc::MessageAcceptanceDisposition::QueuedTurn
            }
            bcode_session_view_models::MessageAcceptanceDispositionView::StartedTurn => {
                bcode_ipc::MessageAcceptanceDisposition::StartedTurn
            }
        },
    })
}

#[allow(clippy::too_many_lines)]
async fn submit_message(
    client: &BcodeClient,
    request: SubmitMessageRequest,
) -> Result<SubmitMessageResult, ClientError> {
    let SubmitMessageRequest {
        session_id,
        launch_working_directory,
        message,
        placement,
        provider_plugin_id,
        model_id,
        agent_id,
        reasoning_effort,
        reasoning_summary,
        reasoning_effort_generation,
        event_sender,
    } = request;
    let mut message = message;
    let mut created_session = None;
    let mut event_task = None;
    let mut event_stream_release = None;
    let session_id = if let Some(session_id) = session_id {
        session_id
    } else {
        let session = client
            .create_session_in_working_directory(None, launch_working_directory.clone())
            .await?;
        let _ = execute_session_view_action(
            client,
            SessionViewAction::UpdateDraft {
                scope: bcode_session_view_models::ComposerDraftViewScope::DraftSession {
                    launch_working_directory: launch_working_directory.clone(),
                },
                text: String::new(),
            },
        )
        .await;
        let (attached, task, release) =
            history_flow::attach_paused_session_event_stream(client, session.id, event_sender)
                .await
                .map_err(|error| match error {
                    TuiError::Client(error) => error,
                    other => ClientError::Server {
                        code: "tui_session_attach_failed".to_owned(),
                        message: other.to_string(),
                    },
                })?;
        let session_id = session.id;
        message = clipboard_image::promote_draft_clipboard_images(
            &message,
            &launch_working_directory,
            session_id,
        )
        .map_err(|error| ClientError::Server {
            code: "tui_clipboard_image_promotion_failed".to_owned(),
            message: error.to_string(),
        })?;
        created_session = Some(attached.session);
        event_task = Some(task);
        event_stream_release = Some(release);
        session_id
    };
    apply_submit_runtime_selections(
        client,
        session_id,
        provider_plugin_id,
        model_id,
        agent_id.clone(),
        reasoning_effort.clone(),
        reasoning_summary.clone(),
    )
    .await?;
    let placement = match placement {
        PromptPlacement::Steering => bcode_session_view_models::PromptPlacementView::Steering,
        PromptPlacement::FollowUp => bcode_session_view_models::PromptPlacementView::FollowUp,
    };
    let outcome = execute_session_view_action(
        client,
        SessionViewAction::SubmitMessage {
            session_id: Some(session_id),
            launch_working_directory: None,
            text: message,
            placement,
            execution: Box::new(bcode_session_models::TurnExecutionOptions {
                reasoning: Some(Box::new(bcode_session_models::TurnReasoningOptions {
                    effort: reasoning_effort.clone(),
                    summary: reasoning_summary.clone(),
                })),
                ..bcode_session_models::TurnExecutionOptions::default()
            }),
        },
    )
    .await?;
    let acceptance = message_acceptance_from_action_outcome(&outcome)?;
    Ok(SubmitMessageResult {
        session_id,
        created_session,
        acceptance,
        committed_agent_id: agent_id,
        committed_reasoning_effort_generation: reasoning_effort_generation,
        event_task,
        event_stream_release,
    })
}

async fn optional_client_result<T>(
    future: impl std::future::Future<Output = Result<T, ClientError>>,
) -> (Option<T>, Option<String>) {
    match future.await {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error.to_string())),
    }
}

async fn load_draft_status(
    client: &BcodeClient,
    launch_working_directory: std::path::PathBuf,
) -> TuiEffectResult {
    let (model, model_error) = optional_client_result(client.default_model_status()).await;
    let draft_scope = ComposerDraftScope::DraftSession {
        launch_working_directory,
    };
    let (composer_draft, draft_error) =
        optional_client_result(client.composer_draft(draft_scope)).await;
    TuiEffectResult::DraftStatusLoaded {
        daemon_connected: model.is_some() || composer_draft.is_some(),
        model,
        composer_draft: composer_draft.flatten(),
        error: model_error.or(draft_error),
    }
}

async fn load_plugin_session_status(
    client: &BcodeClient,
    session_id: SessionId,
) -> (
    Vec<bcode_session_view_models::PluginStatusView>,
    Option<String>,
) {
    let services = match client.plugin_services().await {
        Ok(services) => services,
        Err(error) => return (Vec::new(), Some(error.to_string())),
    };
    let mut contributions = Vec::new();
    let mut first_error = None;
    for service in services
        .into_iter()
        .filter(|service| service.interface_id == bcode_plugin_sdk::SESSION_STATUS_INTERFACE_ID)
    {
        let plugin_id = service.plugin_id.clone();
        let payload =
            match serde_json::to_vec(&bcode_plugin_sdk::SessionStatusRequest { session_id }) {
                Ok(payload) => payload,
                Err(error) => {
                    first_error.get_or_insert_with(|| error.to_string());
                    continue;
                }
            };
        match client
            .invoke_plugin_service(
                service.plugin_id,
                bcode_plugin_sdk::SESSION_STATUS_INTERFACE_ID.to_owned(),
                bcode_plugin_sdk::OP_SESSION_STATUS.to_owned(),
                payload,
            )
            .await
        {
            Ok(response) if response.error.is_none() => {
                match serde_json::from_slice::<bcode_plugin_sdk::SessionStatusResponse>(
                    &response.payload,
                ) {
                    Ok(response) => {
                        contributions.extend(response.contribution.map(|contribution| {
                            bcode_session_view_models::PluginStatusView {
                                plugin_id: plugin_id.clone(),
                                note_id: contribution.contribution_id,
                                text: contribution.text,
                                priority: contribution.priority,
                                metadata: contribution.metadata,
                            }
                        }));
                    }
                    Err(error) => {
                        first_error.get_or_insert_with(|| error.to_string());
                    }
                }
            }
            Ok(response) => {
                if let Some(error) = response.error {
                    first_error.get_or_insert(error.message);
                }
            }
            Err(error) => {
                first_error.get_or_insert_with(|| error.to_string());
            }
        }
    }
    contributions.sort_by_key(|contribution| contribution.priority);
    (contributions, first_error)
}

#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
))]
fn interaction_adapter_for_exchange(
    producer_id: &str,
    schema: &str,
    schema_version: u32,
    platform_id: &str,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    bcode_bundled_plugins::interaction_adapter(producer_id, schema, schema_version, platform_id)
}

#[cfg(not(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
)))]
const fn interaction_adapter_for_exchange(
    _producer_id: &str,
    _schema: &str,
    _schema_version: u32,
    _platform_id: &str,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    None
}

pub async fn load_pending_interactions(
    client: &BcodeClient,
    session_id: SessionId,
) -> Result<Vec<bcode_session_view_models::InteractionViewSummary>, ClientError> {
    let mut interactions = Vec::new();
    for request in client
        .list_pending_tool_exchanges()
        .await?
        .into_iter()
        .filter(|request| request.session_id == session_id)
    {
        let exchange = request.request;
        let interaction_id = exchange.exchange_id.clone();
        let producer_id = exchange.producer_id.clone();
        let exchange_schema = exchange.schema.clone();
        let exchange_schema_version = exchange.schema_version;
        let snapshot = exchange.payload;
        let adapter = interaction_adapter_for_exchange(
            &exchange.producer_id,
            &exchange.schema,
            exchange.schema_version,
            "tui",
        );
        let kind = adapter.as_ref().map_or_else(
            || exchange.schema.clone(),
            |adapter| adapter.interaction_kind.clone(),
        );
        interactions.push(bcode_session_view_models::InteractionViewSummary {
            interaction_id,
            producer_id: Some(producer_id),
            exchange_schema: Some(exchange_schema),
            exchange_schema_version: Some(exchange_schema_version),
            kind,
            tool_call_id: Some(exchange.invocation_id),
            title: Some(exchange.producer_id),
            required: exchange.response_policy
                == bcode_session_models::ToolExchangeResponsePolicy::Required,
            snapshot: Some(snapshot),
            state: bcode_session_view_models::InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        });
    }
    Ok(interactions)
}

async fn load_session_status(client: &BcodeClient, session_id: SessionId) -> TuiEffectResult {
    let (model, model_error) =
        optional_client_result(client.session_model_status(session_id)).await;
    let (
        (active_skills, skills_error),
        (runtime_work, runtime_work_error),
        (interactions, interactions_error),
        (plugin_status, plugin_error),
    ) = tokio::join!(
        optional_client_result(client.active_skills(session_id)),
        optional_client_result(client.list_runtime_work(session_id)),
        optional_client_result(load_pending_interactions(client, session_id)),
        load_plugin_session_status(client, session_id),
    );
    TuiEffectResult::SessionStatusLoaded {
        daemon_connected: model.is_some() || active_skills.is_some() || runtime_work.is_some(),
        session_id,
        hydration: Box::new(SessionStatusHydration {
            model,
            active_skills,
            runtime_work,
            interactions,
            plugin_status,
            error: model_error
                .or(skills_error)
                .or(runtime_work_error)
                .or(interactions_error)
                .or(plugin_error),
        }),
    }
}

#[cfg(test)]
mod progress_routing_tests {
    use super::*;

    #[test]
    fn application_failures_do_not_claim_the_daemon_is_offline() {
        let server_error = ClientError::Server {
            code: "permission_denied".to_owned(),
            message: "request rejected".to_owned(),
        };
        assert!(matches!(
            DaemonObservation::from_client_result::<()>(&Err(server_error)),
            DaemonObservation::Failed(_)
        ));
        assert_eq!(
            DaemonObservation::from_tui_result::<()>(&Err(TuiError::SessionUnavailable {
                session_id: SessionId::new(),
                reason: "owned elsewhere".to_owned(),
            })),
            DaemonObservation::None
        );
    }

    #[test]
    fn transport_unavailability_remains_connectivity_evidence() {
        let error = ClientError::Transport(bcode_ipc::IpcTransportError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "offline",
        )));
        assert!(matches!(
            DaemonObservation::from_client_result::<()>(&Err(error)),
            DaemonObservation::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn presentation_note_queue_preserves_emission_order_per_session() {
        let client = BcodeClient::default_endpoint();
        let mut runner = TuiEffectRunner::new(&client, &client);
        let session_id = SessionId::new();
        let note = |note_id: &str| TuiEffect::AppendPresentationNote {
            session_id,
            source_id: "test".to_owned(),
            note_id: note_id.to_owned(),
            text: note_id.to_owned(),
            format: bcode_command::CommandTextFormat::PlainText,
        };
        let active_key = EffectKey::AppendPresentationNote(session_id, "0001".to_owned());
        runner.tasks.insert(
            active_key,
            tokio::spawn(async { std::future::pending::<TuiEffectResult>().await }),
        );

        assert!(!runner.start(note("0002")));
        assert!(!runner.start(note("0003")));

        let queued = runner
            .queued_presentation_notes
            .get(&session_id)
            .expect("notes should queue behind the active append")
            .iter()
            .map(TuiEffect::key)
            .collect::<Vec<_>>();
        assert_eq!(
            queued,
            vec![
                EffectKey::AppendPresentationNote(session_id, "0002".to_owned()),
                EffectKey::AppendPresentationNote(session_id, "0003".to_owned()),
            ]
        );
        runner.abort_all();
    }

    #[test]
    fn runtime_effect_drain_preserves_every_ordered_note_per_session() {
        let session_id = SessionId::new();
        let mut queue = TuiEffectQueue::default();
        for note_id in ["0001", "0002", "0003"] {
            queue.start_ordered(TuiEffect::AppendPresentationNote {
                session_id,
                source_id: "test".to_owned(),
                note_id: note_id.to_owned(),
                text: note_id.to_owned(),
                format: bcode_command::CommandTextFormat::PlainText,
            });
        }

        let (effects, notes) = queue.drain_runtime();

        assert!(effects.is_empty());
        let queued = notes
            .get(&session_id)
            .expect("ordered session queue")
            .iter()
            .map(TuiEffect::key)
            .collect::<Vec<_>>();
        assert_eq!(
            queued,
            vec![
                EffectKey::AppendPresentationNote(session_id, "0001".to_owned()),
                EffectKey::AppendPresentationNote(session_id, "0002".to_owned()),
                EffectKey::AppendPresentationNote(session_id, "0003".to_owned()),
            ]
        );
    }

    #[test]
    fn effect_streaming_completion_channel_is_bounded() {
        let client = BcodeClient::default_endpoint();
        let runner = TuiEffectRunner::new(&client, &client);
        assert_eq!(runner.streaming_capacity(), TUI_EFFECT_STREAM_CAPACITY);
    }

    #[tokio::test]
    async fn runner_drains_streaming_session_progress_through_effect_results() {
        let client = BcodeClient::default_endpoint();
        let mut runner = TuiEffectRunner::new(&client, &client);
        let session_id = SessionId::new();
        let snapshot = bcode_session_models::SessionOpenOperationSnapshot {
            operation_id: bcode_session_models::SessionOpenOperationId::new(),
            revision: 1,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: bcode_session_models::SessionMigrationProgress {
                stage: bcode_session_models::SessionMigrationStage::CopyingBackup,
                completed_units: Some(1),
                total_units: Some(2),
                unit: Some(bcode_session_models::SessionMigrationProgressUnit::Files),
                message: "Copying backup".to_owned(),
            },
            outcome: None,
            backup_path: None,
        };
        runner
            .streaming_sender
            .try_send(TuiEffectResult::SessionOpenProgress {
                snapshot: snapshot.clone(),
            })
            .expect("stream progress result");

        let results = runner.poll_finished().await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            &results[0],
            TuiEffectResult::SessionOpenProgress { snapshot: actual } if actual == &snapshot
        ));
    }
}
