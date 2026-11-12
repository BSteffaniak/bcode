//! AI questions on review anchors.
//!
//! * Asking creates a durable AI exchange (`review.ai_exchange.save`, pending), a Bcode session
//!   in the repository titled `Review: <path>`, sends the bounded prompt, links the session to
//!   the anchor's thread (`thread.link_session`), and saves the exchange as linked. A failure
//!   after the exchange exists records it as failed; a failed save is never retried
//!   automatically, so no second session is created for one question.
//! * A later question on an anchor that already has a linked session is a follow-up: it is
//!   sent to that session (no new session, no new link) and saved as linked to it.
//! * Structured commands ([`AiCommand`]) ask a fixed instruction about the anchor.
//! * Linked sessions are observed through the host's semantic session view
//!   ([`crate::host::ReviewHost::follow_session`]); each update replaces the session's derived
//!   status (waiting, working, answered, failed) and the latest completed answer.

use super::notes::NoteScope;
use crate::context::UiContext;
use crate::host::SessionFollow;
use crate::services::ReviewServices;
use bcode_code_review_models::{
    LinkThreadSessionRequest, ReviewAiExchange, ReviewAiExchangeStatus, SaveReviewAiExchangeRequest,
};
use bcode_session_models::{
    ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
    ProjectionWindowRequest, ProjectionWindowTarget, SessionProjectionKind,
};
use bcode_session_view::follow::SessionViewFollowUpdate;
use bcode_session_view_models::{
    SessionConnectionViewStatus, SessionViewSnapshot, TranscriptViewItemKind,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Lines of the anchored diff sent with a question.
pub const MAX_PROMPT_LINES: usize = 80;
/// Bytes of the whole prompt.
const MAX_PROMPT_BYTES: usize = 16 * 1024;
/// Other threads of the file listed in a prompt, and characters kept of each.
pub const MAX_PROMPT_THREADS: usize = 12;
const MAX_PROMPT_THREAD_CHARS: usize = 160;

/// A structured AI review command with a fixed instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiCommand {
    /// Explain the selected change.
    Explain,
    /// Identify concrete review risks.
    Risks,
    /// Draft one review comment.
    SuggestComment,
    /// Assess test coverage.
    Tests,
    /// Summarize the file's changes.
    Summarize,
}

impl AiCommand {
    /// Every command, in menu order.
    pub const ALL: [Self; 5] = [
        Self::Explain,
        Self::Risks,
        Self::SuggestComment,
        Self::Tests,
        Self::Summarize,
    ];

    /// Keymap action id (without the `bcode.code_review.` prefix).
    #[must_use]
    pub const fn action(self) -> &'static str {
        match self {
            Self::Explain => "ai_explain",
            Self::Risks => "ai_risks",
            Self::SuggestComment => "ai_suggest_comment",
            Self::Tests => "ai_tests",
            Self::Summarize => "ai_summarize",
        }
    }

    /// The command of a keymap action id.
    #[must_use]
    pub fn from_action(action: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| command.action() == action)
    }

    /// The instruction sent as the question.
    #[must_use]
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::Explain => {
                "Explain what the selected change does, why it may exist, and any important behavior changes."
            }
            Self::Risks => {
                "Identify concrete correctness, security, performance, and maintainability risks in the selected change."
            }
            Self::SuggestComment => {
                "Draft one concise, actionable code review comment for the strongest supported issue; say when no comment is warranted."
            }
            Self::Tests => {
                "Assess test coverage for the selected change and identify specific missing or weak tests."
            }
            Self::Summarize => {
                "Summarize the selected file's changes, intent, and review-relevant consequences."
            }
        }
    }

    /// Short label for status messages.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Explain => "explain the change",
            Self::Risks => "find risks",
            Self::SuggestComment => "suggest a comment",
            Self::Tests => "check tests",
            Self::Summarize => "summarize the file",
        }
    }
}

/// The instruction of a refinement request for a suggested comment.
#[must_use]
pub fn refine_question(body: &str) -> String {
    format!(
        "Refine this suggested review comment. Keep it concise, actionable, and suitable for a code review comment.\n\n{body}"
    )
}

/// What a question is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskContext {
    /// Review title.
    pub title: String,
    /// Repository root.
    pub repo: PathBuf,
    /// Labels of the review's included sources.
    pub sources: Vec<String>,
    /// Other threads on the same file (or the review): `location: first comment`, bounded by
    /// [`MAX_PROMPT_THREADS`].
    pub threads: Vec<String>,
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
    let threads: Vec<String> = context
        .threads
        .iter()
        .take(MAX_PROMPT_THREADS)
        .map(|thread| {
            let mut thread = thread.replace('\n', " ");
            if let Some((end, _)) = thread.char_indices().nth(MAX_PROMPT_THREAD_CHARS) {
                thread.truncate(end);
                thread.push('…');
            }
            format!("- {thread}")
        })
        .collect();
    let threads = if threads.is_empty() {
        "none".to_owned()
    } else {
        threads.join("\n")
    };
    let mut text = format!(
        "You are helping with a local code review in Bcode.\n\nReview: {}\nRepository: {}\nSources: {}\nFile: {}\nOld lines: {}\nNew lines: {}\n\nSelected diff lines:\n```diff\n{}\n```\n\nOther review threads on this file:\n{}\n\nReview context is also available through the code-review plugin service `bcode.code_review/v1` (`review.context.get`, `review.comments.list`, `review.thread.get`, `review.diff.get`).\n\nQuestion:\n{}\n",
        context.title,
        context.repo.display(),
        context.sources.join(", "),
        context.path,
        range(context.old),
        range(context.new),
        lines.join("\n"),
        threads,
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
/// When the exchange already names a session (a follow-up), the prompt goes to that session
/// instead and nothing new is created or linked.
///
/// # Errors
///
/// Returns the exchange as recorded (failed, when it could be recorded) and the message.
pub async fn ask(
    services: ReviewServices,
    scope: NoteScope,
    mut exchange: ReviewAiExchange,
    text: String,
    now_ms: u64,
) -> Result<ReviewAiExchange, Box<(ReviewAiExchange, String)>> {
    let save = |exchange: ReviewAiExchange| {
        let services = services.clone();
        let scope = scope.clone();
        async move {
            services
                .call(SaveReviewAiExchangeRequest {
                    repo_path: scope.repo_path,
                    target: scope.target,
                    scope: scope.scope,
                    exchange,
                })
                .await
                .map(|response| response.exchange)
                .map_err(|error| error.message)
        }
    };
    exchange = match save(exchange.clone()).await {
        Ok(saved) => saved,
        Err(error) => return Err(Box::new((exchange, error))),
    };
    let follow_up = exchange.session_id.clone();
    let linked = async {
        if let Some(session) = follow_up {
            services
                .host()
                .send_message(session.clone(), text)
                .await
                .map_err(|error| error.message)?;
            return Ok::<_, String>(session);
        }
        let session = services
            .host()
            .create_session(
                format!("Review: {}", exchange.anchor.file_path),
                scope.repo_path.clone(),
            )
            .await
            .map_err(|error| error.message)?;
        services
            .host()
            .send_message(session.clone(), text)
            .await
            .map_err(|error| error.message)?;
        services
            .call(LinkThreadSessionRequest {
                repo_path: scope.repo_path.clone(),
                target: scope.target.clone(),
                scope: scope.scope.clone(),
                anchor: exchange.anchor.clone(),
                session_id: session.clone(),
            })
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
    subscriptions: BTreeMap<String, SessionFollow>,
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
        host: &UiContext,
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
            match host.follow_session(
                id,
                projection(),
                bcode_session_view_models::ReasoningPresentationPolicy::Summary,
            ) {
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
                let next = match subscription.updates.try_recv() {
                    Ok(SessionViewFollowUpdate::Snapshot(snapshot)) => {
                        AgentState::from_snapshot(&snapshot)
                    }
                    Ok(SessionViewFollowUpdate::Disconnected { message }) => {
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
            sources: vec!["Last commit".to_owned()],
            threads: (0..40)
                .map(|index| format!("src/lib.rs:{index}: {}", "y".repeat(400)))
                .collect(),
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
        assert!(text.contains("Sources: Last commit"));
        assert!(text.contains("- src/lib.rs:11: "));
        assert!(
            !text.contains("src/lib.rs:12: "),
            "at most {MAX_PROMPT_THREADS} threads"
        );
        assert!(!text.contains(&"y".repeat(200)), "threads are shortened");
        assert!(text.contains("Question:\nWhy?"));
    }
}
