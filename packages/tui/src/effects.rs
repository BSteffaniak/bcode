//! Background effect runner for TUI work that may touch daemon/client services.

use std::collections::BTreeMap;

use bcode_client::{BcodeClient, ClientError};
use bcode_session_models::SessionId;

use super::thinking_flow;

/// Background work requested by local TUI event handling.
pub enum TuiEffect {
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
    CancelTurn(SessionId),
    CycleThinkingEffort(Option<SessionId>),
}

/// Owns and polls daemon-backed TUI background work.
pub struct TuiEffectRunner {
    client: BcodeClient,
    tasks: BTreeMap<EffectKey, tokio::task::JoinHandle<TuiEffectResult>>,
}

impl TuiEffectRunner {
    /// Create an effect runner using the provided client.
    #[must_use]
    pub fn new(client: &BcodeClient) -> Self {
        Self {
            client: client.clone(),
            tasks: BTreeMap::new(),
        }
    }

    /// Start an effect according to its concurrency policy.
    pub fn start(&mut self, effect: TuiEffect) -> bool {
        let key = effect.key();
        if self.tasks.contains_key(&key) {
            return false;
        }
        let client = self.client.clone();
        let task = tokio::spawn(async move { effect.run(client).await });
        self.tasks.insert(key, task);
        true
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
        }
        results
    }

    /// Abort all in-flight effects.
    pub fn abort_all(&mut self) {
        for (_key, task) in std::mem::take(&mut self.tasks) {
            task.abort();
        }
    }
}

impl TuiEffect {
    const fn key(&self) -> EffectKey {
        match self {
            Self::CancelTurn { session_id } => EffectKey::CancelTurn(*session_id),
            Self::CycleThinkingEffort { session_id, .. } => {
                EffectKey::CycleThinkingEffort(*session_id)
            }
        }
    }

    async fn run(self, client: BcodeClient) -> TuiEffectResult {
        match self {
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
