//! Background effect runner for TUI work that may touch daemon/client services.

use std::collections::BTreeMap;

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

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{
    TuiError, clipboard_image,
    daemon_host::TuiDaemonHost,
    daemon_issue, history_flow,
    session_flow::{self, AgentCatalog},
    slash_palette, thinking_flow,
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
    /// Event sender for a newly-created session stream.
    pub event_sender: mpsc::UnboundedSender<bcode_ipc::Event>,
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
    /// Event sender for a newly-created session stream.
    pub event_sender: mpsc::UnboundedSender<bcode_ipc::Event>,
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
        event_sender: mpsc::UnboundedSender<bcode_ipc::Event>,
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
        /// Success status text.
        status: String,
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
    /// Cycle reasoning effort for the current model/session.
    CycleThinkingEffort {
        /// Session to update, or `None` for draft/default model state.
        session_id: Option<SessionId>,
        /// Currently selected effort.
        current_effort: Option<String>,
        /// Currently selected summary.
        current_summary: Option<String>,
        /// Current local visibility state.
        visible: bool,
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
            Err(error) if daemon_issue::is_nonfatal_tui_error(error) => {
                Self::Unavailable(error.to_string())
            }
            Err(error) => Self::Failed(error.to_string()),
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
        /// Success status text.
        status: String,
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
    /// Result for reasoning effort cycling.
    CycleThinkingEffort {
        /// Session the request targeted, or `None` for draft/default state.
        session_id: Option<SessionId>,
        /// Cycle result.
        result: Box<Result<ThinkingCycleResult, ClientError>>,
    },
}

#[allow(clippy::match_same_arms)]
impl TuiEffectResult {
    /// Return the daemon connectivity observation implied by this effect result.
    #[must_use]
    pub fn daemon_observation(&self) -> DaemonObservation {
        match self {
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
            Self::SubmitMessage { result, .. } => DaemonObservation::from_client_result(result),
            Self::CompactContext { result, .. } => DaemonObservation::from_client_result(result),
            Self::CancelRuntimeWork { result, .. } => DaemonObservation::from_client_result(result),
            Self::AttachWorktree { result, .. } => DaemonObservation::from_client_result(result),
            Self::CreateWorktree { result } => DaemonObservation::from_client_result(result),
            Self::CancelTurn { result, .. } => DaemonObservation::from_client_result(result),
            Self::CycleThinkingEffort { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::ConfigLoaded { .. }
            | Self::AuthSecurityReconciled { .. }
            | Self::SlashPaletteLoaded { .. } => DaemonObservation::None,
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
    /// Event stream task for a newly-created session.
    pub event_task: Option<JoinHandle<()>>,
}

/// Reasoning effort cycle outcome.
#[derive(Debug)]
pub struct ThinkingCycleResult {
    /// Next effort, or `None` when unsupported/unavailable.
    pub next_effort: Option<String>,
    /// Summary to keep/apply.
    pub summary: Option<String>,
    /// Visibility to keep/apply.
    pub visible: bool,
    /// Model status fetched while cycling.
    pub status: Option<bcode_ipc::SessionModelStatus>,
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
    RenameSession(SessionId),
    DeleteSession(SessionId),
    ForkSession(SessionId),
    CloneSession(SessionId),
    SubmitMessage(usize),
    SkillAction(SkillId),
    SetSessionModel(SessionId),
    SetSessionReasoning(SessionId),
    CancelRuntimeWork(SessionId),
    CompactContext(SessionId),
    AttachWorktree(SessionId),
    CreateWorktree,
    CancelTurn(SessionId),
    CycleThinkingEffort(Option<SessionId>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectSchedule {
    StartIfIdle,
    Replace,
    QueueLatest,
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
    #[allow(clippy::too_many_lines)]
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
            Self::SetSessionReasoning { status, .. } => TuiEffectResult::SetSessionReasoning {
                status,
                result: Err(client_error),
            },
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
            Self::CycleThinkingEffort { session_id, .. } => TuiEffectResult::CycleThinkingEffort {
                session_id,
                result: Box::new(Err(client_error)),
            },
            Self::LoadConfig
            | Self::ReconcileAuthSecurity { .. }
            | Self::LoadOlderHistory { .. }
            | Self::LoadNewerHistory { .. }
            | Self::ListPermissions
            | Self::SaveDraft { .. }
            | Self::LoadSlashPalette { .. } => {
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
            | Self::CancelRuntimeWork { .. }
            | Self::CompactContext { .. }
            | Self::AttachWorktree { .. }
            | Self::CreateWorktree { .. }
            | Self::CancelTurn { .. }
            | Self::CycleThinkingEffort { .. } => EffectDaemonIntent::Foreground,
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
            | Self::LoadSlashPalette { .. } => EffectDaemonIntent::Background,
        }
    }
}

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

    fn push(&mut self, effect: TuiEffect, schedule: EffectSchedule) {
        self.effects.insert(effect.key(), (schedule, effect));
    }

    /// Drain queued effects.
    fn drain(&mut self) -> Vec<(EffectSchedule, TuiEffect)> {
        std::mem::take(&mut self.effects).into_values().collect()
    }
}

/// Owns and polls daemon-backed TUI background work.
pub struct TuiEffectRunner {
    foreground_client: BcodeClient,
    passive_client: BcodeClient,
    daemon_host: TuiDaemonHost,
    tasks: BTreeMap<EffectKey, tokio::task::JoinHandle<TuiEffectResult>>,
    queued_latest: BTreeMap<EffectKey, TuiEffect>,
}

impl TuiEffectRunner {
    /// Create an effect runner using foreground and passive clients.
    #[must_use]
    pub fn new(
        foreground_client: &BcodeClient,
        passive_client: &BcodeClient,
        daemon_host: TuiDaemonHost,
    ) -> Self {
        Self {
            foreground_client: foreground_client.clone(),
            passive_client: passive_client.clone(),
            daemon_host,
            tasks: BTreeMap::new(),
            queued_latest: BTreeMap::new(),
        }
    }

    /// Start an effect if another effect with the same key is not running.
    pub fn start(&mut self, effect: TuiEffect) -> bool {
        let key = effect.key();
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
        let daemon_host = self.daemon_host.clone();
        let task = tokio::spawn(async move {
            if daemon_intent == EffectDaemonIntent::Foreground
                && let Err(error) = ensure_foreground_daemon(&client, &daemon_host).await
            {
                return effect.daemon_start_failed(error);
            }
            Box::pin(effect.run(client)).await
        });
        self.tasks.insert(key, task);
    }

    /// Poll completed effects without blocking on running tasks.
    pub async fn poll_finished(&mut self) -> Vec<TuiEffectResult> {
        let finished = self
            .tasks
            .iter()
            .filter_map(|(key, task)| task.is_finished().then_some(key.clone()))
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(finished.len());
        for key in finished {
            let Some(task) = self.tasks.remove(&key) else {
                continue;
            };
            match task.await {
                Ok(result) => results.push(result),
                Err(_error) => {}
            }
            if let Some(effect) = self.queued_latest.remove(&key) {
                self.spawn(key, effect);
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
            }
        }
        started
    }

    /// Abort all in-flight effects.
    pub fn abort_all(&mut self) {
        self.queued_latest.clear();
        for (_key, task) in std::mem::take(&mut self.tasks) {
            task.abort();
        }
    }
}

async fn ensure_foreground_daemon(
    client: &BcodeClient,
    daemon_host: &TuiDaemonHost,
) -> Result<(), ClientError> {
    match client.ensure_daemon_available().await {
        Ok(()) => Ok(()),
        Err(error) if error.is_daemon_unavailable() => {
            tracing::warn!(%error, "detached daemon startup failed; falling back to in-process daemon");
            daemon_host.ensure_available().await?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

impl TuiEffect {
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
            Self::CancelRuntimeWork { session_id, .. } => EffectKey::CancelRuntimeWork(*session_id),
            Self::CompactContext { session_id } => EffectKey::CompactContext(*session_id),
            Self::AttachWorktree { session_id, .. } => EffectKey::AttachWorktree(*session_id),
            Self::CreateWorktree { .. } => EffectKey::CreateWorktree,
            Self::CancelTurn { session_id } => EffectKey::CancelTurn(*session_id),
            Self::CycleThinkingEffort { session_id, .. } => {
                EffectKey::CycleThinkingEffort(*session_id)
            }
        }
    }

    async fn run_session_status_effect(
        client: &BcodeClient,
        session_id: SessionId,
    ) -> TuiEffectResult {
        Box::pin(load_session_status(client, session_id)).await
    }

    #[allow(clippy::too_many_lines)]
    async fn run(self, client: BcodeClient) -> TuiEffectResult {
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
            Self::RenameSession { session_id, name } => TuiEffectResult::RenameSession {
                result: client.rename_session(session_id, name).await,
            },
            Self::DeleteSession { session_id } => TuiEffectResult::DeleteSession {
                session_id,
                result: client.delete_session(session_id).await,
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
                result: client.fork_session(session_id, prompt_sequence, name).await,
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
                result: client.clone_session(session_id, name).await,
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
                status,
            } => TuiEffectResult::SetSessionReasoning {
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
            Self::CancelRuntimeWork {
                session_id,
                work_id,
            } => TuiEffectResult::CancelRuntimeWork {
                work_id: work_id.clone(),
                result: client.cancel_runtime_work(session_id, work_id).await,
            },
            Self::CompactContext { session_id } => TuiEffectResult::CompactContext {
                session_id,
                result: client.compact_session(session_id).await,
            },
            Self::AttachWorktree { session_id, path } => TuiEffectResult::AttachWorktree {
                path: path.clone(),
                result: client
                    .change_session_working_directory(session_id, path)
                    .await,
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
                        clear_queue: false,
                    },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::Cancelled { cancelled }) => Ok(cancelled),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::CycleThinkingEffort {
                session_id,
                current_effort,
                current_summary,
                visible,
            } => {
                let result = cycle_thinking_effort(
                    &client,
                    session_id,
                    current_effort,
                    current_summary,
                    visible,
                )
                .await;
                TuiEffectResult::CycleThinkingEffort {
                    session_id,
                    result: Box::new(result),
                }
            }
        }
    }
}

async fn ensure_session_for_foreground_action(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    launch_working_directory: std::path::PathBuf,
    event_sender: mpsc::UnboundedSender<bcode_ipc::Event>,
) -> Result<(SessionId, Option<SessionSummary>, Option<JoinHandle<()>>), ClientError> {
    if let Some(session_id) = session_id {
        return Ok((session_id, None, None));
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
    let (attached, task) =
        history_flow::attach_session_event_stream(client, session.id, event_sender)
            .await
            .map_err(|error| match error {
                TuiError::Client(error) => error,
                other => ClientError::Server {
                    code: "tui_session_attach_failed".to_owned(),
                    message: other.to_string(),
                },
            })?;
    Ok((session.id, Some(attached.session), Some(task)))
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
        event_sender,
    } = request;
    let (session_id, created_session, event_task) = ensure_session_for_foreground_action(
        client,
        session_id,
        launch_working_directory,
        event_sender,
    )
    .await?;
    let acceptance = match action {
        SkillActionKind::Activate => {
            client.activate_skill(session_id, skill_id).await?;
            None
        }
        SkillActionKind::Deactivate => {
            client.deactivate_skill(session_id, skill_id).await?;
            None
        }
        SkillActionKind::Invoke => {
            let display_text = if arguments.trim().is_empty() {
                format!("Invoke skill {skill_id}")
            } else {
                format!("Invoke skill {skill_id}: {arguments}")
            };
            Some(
                client
                    .invoke_skill(session_id, skill_id, arguments, display_text)
                    .await?,
            )
        }
    };
    Ok(SkillActionResult {
        session_id,
        created_session,
        event_task,
        acceptance,
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
        event_sender,
    } = request;
    let mut message = message;
    let mut created_session = None;
    let mut event_task = None;
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
        let (attached, task) =
            history_flow::attach_session_event_stream(client, session.id, event_sender)
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
        session_id
    };
    apply_submit_runtime_selections(
        client,
        session_id,
        provider_plugin_id,
        model_id,
        agent_id.clone(),
        reasoning_effort,
        reasoning_summary,
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
        },
    )
    .await?;
    let acceptance = message_acceptance_from_action_outcome(&outcome)?;
    Ok(SubmitMessageResult {
        session_id,
        created_session,
        acceptance,
        committed_agent_id: agent_id,
        event_task,
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

async fn load_pending_interactions(
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
        let snapshot = exchange.payload;
        let adapter = bcode_bundled_plugins::interaction_adapter(
            &exchange.producer_id,
            &exchange.schema,
            exchange.schema_version,
            "tui",
        );
        let kind = adapter.as_ref().map_or_else(
            || exchange.schema.clone(),
            |adapter| adapter.interaction_kind.clone(),
        );
        let surface_kind = adapter
            .and_then(|adapter| adapter.tui_surface_kind)
            .unwrap_or_else(|| exchange.schema.clone());
        interactions.push(bcode_session_view_models::InteractionViewSummary {
            interaction_id,
            kind,
            surface_kind,
            tool_call_id: Some(exchange.invocation_id),
            title: Some(exchange.producer_id),
            required: exchange.response_policy
                == bcode_session_models::ToolExchangeResponsePolicy::Required,
            snapshot: Some(snapshot),
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

async fn cycle_thinking_effort(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    current_effort: Option<String>,
    current_summary: Option<String>,
    visible: bool,
) -> Result<ThinkingCycleResult, ClientError> {
    let status = if let Some(session_id) = session_id {
        client.session_model_status(session_id).await?
    } else {
        client.default_model_status().await?
    };
    let Some(next_effort) =
        thinking_flow::next_effort_for_status(&status, current_effort.as_deref())
    else {
        return Ok(ThinkingCycleResult {
            next_effort: None,
            summary: current_summary,
            visible,
            status: Some(status),
        });
    };
    let summary = current_summary.or_else(|| status.reasoning_summary.clone());
    if let Some(session_id) = session_id {
        execute_session_view_action(
            client,
            SessionViewAction::SetReasoning {
                session_id,
                effort: Some(next_effort.clone()),
                summary: summary.clone(),
            },
        )
        .await?;
    }
    Ok(ThinkingCycleResult {
        next_effort: Some(next_effort),
        summary,
        visible,
        status: Some(status),
    })
}
