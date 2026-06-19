//! Background effect runner for TUI work that may touch daemon/client services.

use std::collections::BTreeMap;

use bcode_client::{BcodeClient, ClientError};
use bcode_ipc::{ComposerDraftScope, PermissionSummary};
use bcode_session_models::{
    ProjectionWindowRequest, SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage,
    SessionHistoryQuery, SessionId,
};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{
    TuiError, history_flow,
    session_flow::{self, AgentCatalog},
    slash_palette, thinking_flow,
};

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
    /// Poll pending permission requests.
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
        /// Default model status, if available.
        model: Option<bcode_ipc::SessionModelStatus>,
        /// Restored composer draft, if available.
        composer_draft: Option<String>,
        /// First non-critical error encountered.
        error: Option<String>,
    },
    /// Attached session status hydration completed.
    SessionStatusLoaded {
        /// Session that was hydrated.
        session_id: SessionId,
        /// Model status, if available.
        model: Option<bcode_ipc::SessionModelStatus>,
        /// Active skill count, if available.
        active_skill_count: Option<usize>,
        /// Runtime work snapshots, if available.
        runtime_work: Option<Vec<bcode_ipc::RuntimeWorkSnapshot>>,
        /// First non-critical error encountered.
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
    AgentCatalog,
    OlderHistory,
    NewerHistory,
    PermissionList,
    DraftSave,
    SlashPalette,
    CancelTurn(SessionId),
    CycleThinkingEffort(Option<SessionId>),
}

/// Owns and polls daemon-backed TUI background work.
pub struct TuiEffectRunner {
    client: BcodeClient,
    tasks: BTreeMap<EffectKey, tokio::task::JoinHandle<TuiEffectResult>>,
    queued_latest: BTreeMap<EffectKey, TuiEffect>,
}

impl TuiEffectRunner {
    /// Create an effect runner using the provided client.
    #[must_use]
    pub fn new(client: &BcodeClient) -> Self {
        Self {
            client: client.clone(),
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
        let client = self.client.clone();
        let task = tokio::spawn(async move { Box::pin(effect.run(client)).await });
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

    /// Abort all in-flight effects.
    pub fn abort_all(&mut self) {
        self.queued_latest.clear();
        for (_key, task) in std::mem::take(&mut self.tasks) {
            task.abort();
        }
    }
}

impl TuiEffect {
    const fn key(&self) -> EffectKey {
        match self {
            Self::OpenSession { .. } => EffectKey::SessionOpen,
            Self::LoadConfig => EffectKey::Config,
            Self::ReconcileAuthSecurity { .. } => EffectKey::AuthSecurity,
            Self::LoadDraftStatus { .. } => EffectKey::DraftStatus,
            Self::LoadSessionStatus { .. } => EffectKey::SessionStatus,
            Self::LoadAgentCatalog => EffectKey::AgentCatalog,
            Self::LoadOlderHistory { .. } => EffectKey::OlderHistory,
            Self::LoadNewerHistory { .. } => EffectKey::NewerHistory,
            Self::ListPermissions => EffectKey::PermissionList,
            Self::SaveDraft { .. } => EffectKey::DraftSave,
            Self::LoadSlashPalette { .. } => EffectKey::SlashPalette,
            Self::CancelTurn { session_id } => EffectKey::CancelTurn(*session_id),
            Self::CycleThinkingEffort { session_id, .. } => {
                EffectKey::CycleThinkingEffort(*session_id)
            }
        }
    }

    async fn run(self, client: BcodeClient) -> TuiEffectResult {
        match self {
            Self::OpenSession {
                session_id,
                initial_window_request,
                event_sender,
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
                Box::pin(load_session_status(&client, session_id)).await
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
                let result = client.set_composer_draft(scope, text.clone()).await;
                TuiEffectResult::SaveDraft { text, result }
            }
            Self::LoadSlashPalette { query, session_id } => {
                let palette = slash_palette::SlashPalette::new(&client, session_id, &query).await;
                TuiEffectResult::SlashPaletteLoaded { query, palette }
            }
            Self::CancelTurn { session_id } => TuiEffectResult::CancelTurn {
                session_id,
                result: client.cancel_session_turn(session_id).await,
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
        model,
        composer_draft: composer_draft.flatten(),
        error: model_error.or(draft_error),
    }
}

async fn load_session_status(client: &BcodeClient, session_id: SessionId) -> TuiEffectResult {
    let (model, model_error) =
        optional_client_result(client.session_model_status(session_id)).await;
    let ((active_skills, skills_error), (runtime_work, runtime_work_error)) = tokio::join!(
        optional_client_result(client.active_skills(session_id)),
        optional_client_result(client.list_runtime_work(session_id)),
    );
    TuiEffectResult::SessionStatusLoaded {
        session_id,
        model,
        active_skill_count: active_skills.map(|skills| skills.len()),
        runtime_work,
        error: model_error.or(skills_error).or(runtime_work_error),
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
        client
            .set_session_reasoning(session_id, Some(next_effort.clone()), summary.clone())
            .await?;
    }
    Ok(ThinkingCycleResult {
        next_effort: Some(next_effort),
        summary,
        visible,
        status: Some(status),
    })
}
