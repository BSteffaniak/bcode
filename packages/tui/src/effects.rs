//! Background effect runner for TUI work that may touch daemon/client services.

use std::collections::BTreeMap;

use bcode_client::{BcodeClient, ClientError};
use bcode_ipc::{ComposerDraftScope, PermissionSummary};
use bcode_session_models::{
    SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery,
    SessionId,
};

use super::{session_flow::AgentCatalog, slash_palette, thinking_flow};

/// Background work requested by local TUI event handling.
pub enum TuiEffect {
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

    /// Start an effect according to its concurrency policy.
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
        let task = tokio::spawn(async move { effect.run(client).await });
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
