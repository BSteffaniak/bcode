//! AI questions on review anchors (spec 6.2, inventory A.8).
//!
//! * Asking creates a durable AI exchange (`review.ai_exchange.save`, pending), a Bcode session
//!   in the repository titled `Review: <path>`, sends the bounded prompt, links the session to
//!   the anchor's thread (`thread.link_session`), and saves the exchange as linked. A failure
//!   after the exchange exists records it as failed; a failed save is never retried
//!   automatically, so no second session is created for one question.
//! * Linked sessions are observed through the host's semantic session view
//!   ([`PluginTuiHost::subscribe_session_view`]); each update replaces the session's derived
//!   status (waiting, working, answered, failed) and the latest completed answer.

use super::backend::{SharedBackend, call_json};
use super::notes::{NoteScope, OP_THREAD_LINK_SESSION};
use bcode_code_review_models::{
    LinkThreadSessionRequest, LinkThreadSessionResponse, OP_REVIEW_AI_EXCHANGE_SAVE,
    ReviewAiExchange, ReviewAiExchangeStatus, SaveReviewAiExchangeRequest,
    SaveReviewAiExchangeResponse,
};
use bcode_plugin_sdk::tui::{
    PluginSessionViewSubscription, PluginSessionViewSubscriptionRequest, PluginSessionViewUpdate,
    PluginTuiHost,
};
use bcode_session_models::{
    ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
    ProjectionWindowRequest, ProjectionWindowTarget, SessionProjectionKind,
};
use bcode_session_view_models::{
    SessionConnectionViewStatus, SessionViewSnapshot, TranscriptViewItemKind,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Lines of the anchored diff sent with a question.
pub const MAX_PROMPT_LINES: usize = 80;
/// Bytes of the whole prompt.
const MAX_PROMPT_BYTES: usize = 16 * 1024;

/// What a question is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskContext {
    /// Review title.
    pub title: String,
    /// Repository root.
    pub repo: PathBuf,
    /// File path, or the review-level marker.
    pub path: String,
    /// Old and new line ranges.
    pub old: Option<(u32, u32)>,
    /// New-side range.
    pub new: Option<(u32, u32)>,
    /// Anchored diff lines (`+`, `-`, or ` ` prefixed), bounded by [`MAX_PROMPT_LINES`].
    pub lines: Vec<String>,
    /// The question.
    pub question: String,
}

/// The bounded prompt sent to the session.
#[must_use]
pub fn prompt(context: &AskContext) -> String {
    let range = |range: Option<(u32, u32)>| {
        range.map_or_else(
            || "none".to_owned(),
            |(start, end)| format!("{start}-{end}"),
        )
    };
    let lines: Vec<&str> = context
        .lines
        .iter()
        .take(MAX_PROMPT_LINES)
        .map(String::as_str)
        .collect();
    let mut text = format!(
        "You are helping with a local code review in Bcode.\n\nReview: {}\nRepository: {}\nFile: {}\nOld lines: {}\nNew lines: {}\n\nSelected diff lines:\n```diff\n{}\n```\n\nReview context is also available through the code-review plugin service `bcode.code_review/v1` (`review.context.get`, `review.comments.list`, `review.thread.get`, `review.diff.get`).\n\nQuestion:\n{}\n",
        context.title,
        context.repo.display(),
        context.path,
        range(context.old),
        range(context.new),
        lines.join("\n"),
        context.question.trim(),
    );
    if text.len() > MAX_PROMPT_BYTES {
        let mut end = MAX_PROMPT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Ask: persist the exchange, create and prompt the session, link it, and persist the link.
///
/// # Errors
///
/// Returns the exchange as recorded (failed, when it could be recorded) and the message.
pub async fn ask(
    backend: SharedBackend,
    scope: NoteScope,
    mut exchange: ReviewAiExchange,
    text: String,
    now_ms: u64,
) -> Result<ReviewAiExchange, Box<(ReviewAiExchange, String)>> {
    let save = |exchange: ReviewAiExchange| {
        let backend = std::sync::Arc::clone(&backend);
        let scope = scope.clone();
        async move {
            call_json::<_, SaveReviewAiExchangeResponse>(
                backend.as_ref(),
                OP_REVIEW_AI_EXCHANGE_SAVE,
                &SaveReviewAiExchangeRequest {
                    repo_path: scope.repo_path,
                    target: scope.target,
                    scope: scope.scope,
                    exchange,
                },
            )
            .await
            .map(|response| response.exchange)
            .map_err(|error| error.message)
        }
    };
    exchange = match save(exchange.clone()).await {
        Ok(saved) => saved,
        Err(error) => return Err(Box::new((exchange, error))),
    };
    let linked = async {
        let session = backend
            .create_session(
                format!("Review: {}", exchange.anchor.file_path),
                scope.repo_path.clone(),
            )
            .await
            .map_err(|error| error.message)?;
        backend
            .send_message(session.clone(), text)
            .await
            .map_err(|error| error.message)?;
        call_json::<_, LinkThreadSessionResponse>(
            backend.as_ref(),
            OP_THREAD_LINK_SESSION,
            &LinkThreadSessionRequest {
                repo_path: scope.repo_path.clone(),
                target: scope.target.clone(),
                scope: scope.scope.clone(),
                anchor: exchange.anchor.clone(),
                session_id: session.clone(),
            },
        )
        .await
        .map_err(|error| error.message)?;
        Ok::<_, String>(session)
    }
    .await;
    match linked {
        Ok(session) => {
            exchange.session_id = Some(session);
            exchange.status = ReviewAiExchangeStatus::Linked;
            exchange.error = None;
            exchange.updated_at_ms = now_ms;
            save(exchange.clone())
                .await
                .map_err(|error| Box::new((exchange, error)))
        }
        Err(error) => {
            exchange.status = ReviewAiExchangeStatus::Failed;
            exchange.error = Some(error.clone());
            exchange.updated_at_ms = now_ms;
            let recorded = save(exchange.clone()).await.unwrap_or(exchange);
            Err(Box::new((recorded, error)))
        }
    }
}

/// Phase of a linked session's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhase {
    /// Waiting for the first snapshot.
    Waiting,
    /// The session is working.
    Working,
    /// The latest turn finished with an answer.
    Answered,
    /// The session failed or the observation stopped.
    Failed,
}

/// Derived live state of one linked session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentState {
    /// Phase.
    pub phase: AgentPhase,
    /// One-line status.
    pub status: String,
    /// Latest completed answer (empty until there is one).
    pub answer: String,
}

impl AgentState {
    const fn waiting() -> Self {
        Self {
            phase: AgentPhase::Waiting,
            status: String::new(),
            answer: String::new(),
        }
    }

    const fn failed(message: String) -> Self {
        Self {
            phase: AgentPhase::Failed,
            status: message,
            answer: String::new(),
        }
    }

    /// State derived from a session snapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: &SessionViewSnapshot) -> Self {
        let answer = snapshot
            .transcript
            .items
            .iter()
            .rev()
            .find_map(|item| match &item.kind {
                TranscriptViewItemKind::AssistantMessage { message }
                    if !item.streaming && !message.text.trim().is_empty() =>
                {
                    Some(message.text.clone())
                }
                _ => None,
            })
            .unwrap_or_default();
        let failed = matches!(
            snapshot.runtime.last_turn_outcome,
            Some(
                bcode_session_models::ModelTurnOutcome::Cancelled
                    | bcode_session_models::ModelTurnOutcome::Error
                    | bcode_session_models::ModelTurnOutcome::IdleTimeout
                    | bcode_session_models::ModelTurnOutcome::ProviderUnavailable
            )
        );
        let working = snapshot.runtime.active_turn_id.is_some()
            || snapshot.runtime_work.iter().any(|work| !work.is_terminal())
            || snapshot.transcript.items.iter().any(|item| item.streaming);
        if let SessionConnectionViewStatus::Error(message) = &snapshot.connection_status {
            return Self {
                answer,
                ..Self::failed(message.clone())
            };
        }
        if failed {
            return Self {
                answer,
                ..Self::failed(
                    snapshot
                        .runtime
                        .last_turn_message
                        .clone()
                        .unwrap_or_else(|| "linked Bcode turn failed".to_owned()),
                )
            };
        }
        let (phase, status) =
            if !snapshot.permissions.is_empty() || !snapshot.interactions.is_empty() {
                (
                    AgentPhase::Working,
                    "action needed in the linked session".to_owned(),
                )
            } else if working || answer.is_empty() {
                (AgentPhase::Working, "working…".to_owned())
            } else {
                (AgentPhase::Answered, "answered".to_owned())
            };
        Self {
            phase,
            status,
            answer,
        }
    }
}

const fn projection() -> ProjectionWindowRequest {
    ProjectionWindowRequest {
        projection: SessionProjectionKind::Transcript,
        anchor: ProjectionWindowAnchor::Latest,
        direction: ProjectionWindowDirection::Backward,
        target: ProjectionWindowTarget {
            min_items: Some(16),
            min_estimated_rows: Some(40),
            min_bytes: None,
            width_columns: Some(100),
        },
        limits: ProjectionWindowLimits {
            max_items: 32,
            max_events_scanned: 1_024,
            max_bytes: 256 * 1024,
        },
    }
}

/// Live observation of every linked session of the review.
#[derive(Debug, Default)]
pub struct AgentStreams {
    subscriptions: BTreeMap<String, PluginSessionViewSubscription>,
    states: BTreeMap<String, AgentState>,
    failed: BTreeSet<String>,
    revision: u64,
}

impl AgentStreams {
    /// Changes whenever any session state changed.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// State of a linked session, once observed.
    #[must_use]
    pub fn state(&self, session_id: &str) -> Option<&AgentState> {
        self.states.get(session_id)
    }

    /// Subscribe to every session in `sessions` not observed yet. Returns whether any state
    /// changed.
    pub fn ensure<'a>(
        &mut self,
        host: &dyn PluginTuiHost,
        sessions: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let mut changed = false;
        for session in sessions {
            if self.subscriptions.contains_key(session) || self.failed.contains(session) {
                continue;
            }
            changed = true;
            let Ok(id) = session.parse::<bcode_session_models::SessionId>() else {
                self.failed.insert(session.to_owned());
                self.states.insert(
                    session.to_owned(),
                    AgentState::failed("invalid linked session id".to_owned()),
                );
                continue;
            };
            match host.subscribe_session_view(PluginSessionViewSubscriptionRequest {
                session_id: id,
                projection: projection(),
                reasoning_policy: bcode_session_view_models::ReasoningPresentationPolicy::Summary,
                buffer: 16,
            }) {
                Ok(subscription) => {
                    self.subscriptions.insert(session.to_owned(), subscription);
                    self.states
                        .insert(session.to_owned(), AgentState::waiting());
                }
                Err(error) => {
                    self.failed.insert(session.to_owned());
                    self.states.insert(
                        session.to_owned(),
                        AgentState::failed(format!("live status unavailable: {error}")),
                    );
                }
            }
        }
        self.revision += u64::from(changed);
        changed
    }

    /// Apply every pending update. Returns whether any state changed.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        let mut stopped = Vec::new();
        for (session, subscription) in &mut self.subscriptions {
            loop {
                let next = match subscription.receiver.try_recv() {
                    Ok(PluginSessionViewUpdate::Snapshot(snapshot)) => {
                        AgentState::from_snapshot(&snapshot)
                    }
                    Ok(PluginSessionViewUpdate::Disconnected { message }) => {
                        stopped.push(session.clone());
                        AgentState::failed(message)
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        stopped.push(session.clone());
                        AgentState::failed("session stream disconnected".to_owned())
                    }
                };
                let finished = next.phase == AgentPhase::Failed;
                if self.states.get(session) != Some(&next) {
                    self.states.insert(session.clone(), next);
                    changed = true;
                }
                if finished && stopped.last() == Some(session) {
                    break;
                }
            }
        }
        for session in stopped {
            self.subscriptions.remove(&session);
            self.failed.insert(session);
        }
        self.revision += u64::from(changed);
        changed
    }

    /// Forget a stopped observation so the next [`Self::ensure`] subscribes again (retry).
    pub fn retry(&mut self, session: &str) {
        self.failed.remove(session);
        self.subscriptions.remove(session);
        self.states.remove(session);
        self.revision += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_are_bounded_and_carry_the_anchor() {
        let context = AskContext {
            title: "Review".to_owned(),
            repo: PathBuf::from("/repo"),
            path: "src/lib.rs".to_owned(),
            old: Some((3, 4)),
            new: None,
            lines: (0..500)
                .map(|line| format!("+line {line} {}", "x".repeat(80)))
                .collect(),
            question: "  Why?  ".to_owned(),
        };
        let text = prompt(&context);
        assert!(text.len() <= MAX_PROMPT_BYTES);
        assert!(text.contains("File: src/lib.rs"));
        assert!(text.contains("Old lines: 3-4"));
        assert!(text.contains("New lines: none"));
        assert!(
            !text.contains("+line 80 "),
            "at most {MAX_PROMPT_LINES} lines"
        );
    }
}
