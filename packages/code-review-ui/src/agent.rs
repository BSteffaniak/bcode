//! AI questions on review anchors.
//!
//! * Asking creates a durable AI exchange (`review.ai_exchange.save`, pending), a Bcode session
//!   in the repository titled after the file (or pull request) and, for a question asked from a
//!   thread, its author ([`session_title`]), sends the bounded prompt, links the session to
//!   the anchor's thread (`thread.link_session`), and saves the exchange as linked. A failure
//!   after the exchange exists records it as failed; a failed save is never retried
//!   automatically, so no second session is created for one question.
//! * A later question on an anchor that already has a linked session is a follow-up: it is
//!   sent to that session (no new session, no new link) and saved as linked to it.
//! * Structured commands ([`AiCommand`]) ask a fixed instruction about the anchor.
//! * Linked sessions are observed through the host's semantic session view
//!   ([`crate::host::ReviewHost::follow_session`]); each update replaces the session's derived
//!   status (waiting, working, answered, failed), its bounded live [`Feed`] (questions,
//!   reasoning, tool lines, the streaming answer, errors, a pending permission request, tokens
//!   and cost), and the latest completed answer. Each session has its own revision, so an update
//!   or a spinner frame lays out only the cards of that session again.

pub mod feed;

pub use feed::{Feed, FeedItem};

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
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Lines of the anchored diff sent with a question.
pub const MAX_PROMPT_LINES: usize = 80;
/// Bytes of the whole prompt.
const MAX_PROMPT_BYTES: usize = 24 * 1024;
/// Bytes of the question kept (it is never cut to make room for context).
const MAX_QUESTION_BYTES: usize = 4 * 1024;
/// Other threads of the file listed in a prompt, and characters kept of each.
pub const MAX_PROMPT_THREADS: usize = 12;
const MAX_PROMPT_THREAD_CHARS: usize = 160;
/// Comments of the asked-about thread sent at most (the first and the latest).
pub const MAX_THREAD_COMMENTS: usize = 12;
/// Bytes of one comment of the asked-about thread.
const MAX_THREAD_COMMENT_BYTES: usize = 2 * 1024;
/// Bytes of all comments of the asked-about thread.
const MAX_THREAD_BYTES: usize = 8 * 1024;
/// Lines and bytes of the diff hunk the asked-about thread was written on (its end is kept).
const MAX_HUNK_LINES: usize = 40;
const MAX_HUNK_BYTES: usize = 3 * 1024;
/// Characters of a session title.
const MAX_TITLE_CHARS: usize = 80;

/// Marks the context cut to leave room for the question.
const CONTEXT_CUT: &str = "\n[… context cut to fit; the code_review tools read the rest]\n";

/// The review tools a question's session can call to read more.
const TOOLS_LINE: &str = "Read more of this review with the code_review tools: \
code_review.list_threads and code_review.get_thread (its local threads and its pull requests' \
threads), code_review.file_diff, code_review.file_content, code_review.summary, and \
code_review.pull_request.";

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
    /// Other threads on the same file (or the review): one line each, bounded by
    /// [`MAX_PROMPT_THREADS`].
    pub threads: Vec<String>,
    /// Other threads left out of `threads`.
    pub threads_omitted: usize,
    /// What `threads` are on: a file path, or the review-level marker.
    pub threads_on: String,
    /// File path, or the review-level marker.
    pub path: String,
    /// Old and new line ranges.
    pub old: Option<(u32, u32)>,
    /// New-side range.
    pub new: Option<(u32, u32)>,
    /// Anchored diff lines (`+`, `-`, or ` ` prefixed), bounded by [`MAX_PROMPT_LINES`].
    pub lines: Vec<String>,
    /// The thread the question was asked from.
    pub thread: Option<AskThread>,
    /// The question.
    pub question: String,
}

/// The thread a question was asked from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AskThread {
    /// Where it comes from (`a local draft thread`, `a GitHub thread on pull request o/r#7`).
    pub origin: String,
    /// Its id for `code_review.get_thread`.
    pub id: String,
    /// Resolved.
    pub resolved: bool,
    /// Its forge considers its code changed since it was written.
    pub outdated: bool,
    /// Where it was written, when that differs from where the review shows it.
    pub written_on: Option<WrittenOn>,
    /// Where the review shows it, and why there.
    pub shown_at: String,
    /// Comments, oldest first.
    pub comments: Vec<AskComment>,
    /// The diff its first comment was written on.
    pub diff_hunk: Option<String>,
    /// Web page.
    pub url: Option<String>,
}

/// Where a pull request thread was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenOn {
    /// File path.
    pub path: String,
    /// Lines as written (first, last), when on lines.
    pub lines: Option<(u32, u32)>,
    /// On the old side of the diff.
    pub old_side: bool,
    /// The head commit it was written on.
    pub commit: Option<String>,
}

/// One comment of the asked-about thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskComment {
    /// Author.
    pub author: String,
    /// Body.
    pub body: String,
}

/// `text` cut to `max` bytes on a character boundary, with `marker` when cut.
fn cut(text: &str, max: usize, marker: &str) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &text[..end])
}

/// The end of `hunk` within [`MAX_HUNK_LINES`] and [`MAX_HUNK_BYTES`], saying what was left out.
fn hunk_tail(hunk: &str) -> String {
    let lines: Vec<&str> = hunk.lines().collect();
    let mut kept = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev() {
        if kept.len() == MAX_HUNK_LINES || bytes + line.len() + 1 > MAX_HUNK_BYTES {
            break;
        }
        bytes += line.len() + 1;
        kept.push(*line);
    }
    kept.reverse();
    let omitted = lines.len() - kept.len();
    if omitted == 0 {
        return kept.join("\n");
    }
    format!("[{omitted} earlier lines left out]\n{}", kept.join("\n"))
}

/// The thread section of a prompt: its state, where it was written and shown, the diff it was
/// written on, and its comments (the first and as many of the latest as fit, the rest counted).
fn thread_section(thread: &AskThread) -> String {
    let mut state = Vec::new();
    if thread.outdated {
        state.push("outdated: its code changed since it was written");
    }
    state.push(if thread.resolved { "resolved" } else { "open" });
    let mut text = format!(
        "The question is about {} ({}).\nThread id: {} (code_review.get_thread reads it in full)\n",
        thread.origin,
        state.join(", "),
        thread.id,
    );
    if let Some(written) = &thread.written_on {
        let lines = written.lines.map_or_else(
            || "the whole file".to_owned(),
            |(start, end)| {
                let side = if written.old_side { "old" } else { "new" };
                if start == end {
                    format!("{side} line {end}")
                } else {
                    format!("{side} lines {start}-{end}")
                }
            },
        );
        let _ = write!(text, "Written on: {}, {lines}", written.path);
        if let Some(commit) = &written.commit {
            let _ = write!(text, ", at commit {commit}");
        }
        text.push('\n');
    }
    let _ = writeln!(text, "Shown in this review: {}", thread.shown_at);
    if let Some(url) = &thread.url {
        let _ = writeln!(text, "Link: {url}");
    }
    if let Some(hunk) = thread
        .diff_hunk
        .as_deref()
        .filter(|hunk| !hunk.trim().is_empty())
    {
        let _ = write!(
            text,
            "\nThe diff it was written on (the comment is on its last lines):\n```diff\n{}\n```\n",
            hunk_tail(hunk)
        );
    }
    let comment = |item: &AskComment| {
        format!(
            "--- {}:\n{}\n",
            item.author,
            cut(
                item.body.trim(),
                MAX_THREAD_COMMENT_BYTES,
                "\n[comment cut]"
            )
        )
    };
    let Some((first, rest)) = thread.comments.split_first() else {
        text.push_str("\nThe thread has no comments.\n");
        return text;
    };
    let first = comment(first);
    let mut budget = MAX_THREAD_BYTES.saturating_sub(first.len());
    let mut latest = Vec::new();
    for item in rest.iter().rev().take(MAX_THREAD_COMMENTS - 1) {
        let item = comment(item);
        if item.len() > budget {
            break;
        }
        budget -= item.len();
        latest.push(item);
    }
    latest.reverse();
    let omitted = rest.len() - latest.len();
    text.push_str("\nComments, oldest first:\n");
    text.push_str(&first);
    if omitted > 0 {
        let _ = writeln!(
            text,
            "[{omitted} replies left out; code_review.get_thread reads them all]"
        );
    }
    for item in latest {
        text.push_str(&item);
    }
    text
}

/// The bounded prompt sent to the session. Every part has its own bound; the question is kept
/// whole (up to its own bound) and what comes before it is cut, marked, when the whole is too
/// long.
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
    let mut threads: Vec<String> = context
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
    let omitted =
        context.threads_omitted + context.threads.len().saturating_sub(MAX_PROMPT_THREADS);
    if omitted > 0 {
        threads.push(format!(
            "- … {omitted} more (code_review.list_threads lists them)"
        ));
    }
    let threads = if threads.is_empty() {
        "none".to_owned()
    } else {
        threads.join("\n")
    };
    let mut text = format!(
        "You are helping with a local code review in Bcode.\n\nReview: {}\nRepository: {}\nSources: {}\n",
        context.title,
        context.repo.display(),
        context.sources.join(", "),
    );
    if let Some(thread) = &context.thread {
        text.push('\n');
        text.push_str(&thread_section(thread));
    }
    let place = |path: &str| {
        if path == bcode_code_review::document::REVIEW_LEVEL_PATH {
            "the review".to_owned()
        } else {
            path.to_owned()
        }
    };
    if context.path == bcode_code_review::document::REVIEW_LEVEL_PATH {
        text.push_str("\nAsked about: the whole review\n");
    } else {
        let _ = write!(
            text,
            "\nFile: {}\nOld lines: {}\nNew lines: {}\n",
            context.path,
            range(context.old),
            range(context.new),
        );
    }
    if !lines.is_empty() {
        let _ = write!(
            text,
            "\nSelected diff lines:\n```diff\n{}\n```\n",
            lines.join("\n")
        );
    }
    let _ = write!(
        text,
        "\nOther review threads on {}:\n{threads}\n\n{TOOLS_LINE}\n",
        place(&context.threads_on)
    );
    let question = format!(
        "\nQuestion:\n{}\n",
        cut(
            context.question.trim(),
            MAX_QUESTION_BYTES,
            "\n[question cut]"
        )
    );
    let room = MAX_PROMPT_BYTES.saturating_sub(question.len());
    if text.len() > room {
        text = cut(&text, room.saturating_sub(CONTEXT_CUT.len()), CONTEXT_CUT);
    }
    text.push_str(&question);
    text
}

/// The title of a question's new session: the file (or pull request) and, for a question asked
/// from a thread, its author; the review's title for a question about the whole review.
#[must_use]
pub fn session_title(context: &AskContext) -> String {
    let review_level = context.path == bcode_code_review::document::REVIEW_LEVEL_PATH;
    let place = context
        .thread
        .as_ref()
        .and_then(|thread| thread.written_on.as_ref())
        .map(|written| written.path.clone())
        .or_else(|| (!review_level).then(|| context.path.clone()))
        .unwrap_or_else(|| context.title.clone());
    let author = context
        .thread
        .as_ref()
        .and_then(|thread| thread.comments.first())
        .map(|comment| comment.author.as_str());
    let title = author.map_or_else(
        || format!("Review: {place}"),
        |author| format!("Review: {place} · {author}"),
    );
    match title.char_indices().nth(MAX_TITLE_CHARS) {
        Some((end, _)) => format!("{}…", &title[..end]),
        None => title,
    }
}

/// A question ready to send: its prompt, and the title of its session when it starts one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    /// The prompt.
    pub prompt: String,
    /// The new session's title ([`session_title`]).
    pub title: String,
}

/// Ask: persist the exchange, create the session and bind it to the review (`binding`, whose
/// session id is filled in), prompt it, link it, and persist the link.
///
/// When the exchange already names a session (a follow-up), the prompt goes to that session
/// instead and nothing new is created, bound, or linked.
///
/// # Errors
///
/// Returns the exchange as recorded (failed, when it could be recorded) and the message.
pub async fn ask(
    services: ReviewServices,
    scope: NoteScope,
    mut exchange: ReviewAiExchange,
    question: Asked,
    binding: bcode_code_review_models::ReviewAssistantBinding,
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
    let Asked {
        prompt: text,
        title,
    } = question;
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
                title,
                scope.repo_path.clone(),
                crate::host::review_session_origin(
                    crate::host::ReviewSessionPurpose::Question,
                    &binding.workspace.id,
                ),
            )
            .await
            .map_err(|error| error.message)?;
        services
            .call(bcode_code_review_models::BindReviewAssistantRequest {
                repo_path: scope.repo_path.clone(),
                binding: bcode_code_review_models::ReviewAssistantBinding {
                    session_id: session.clone(),
                    ..binding
                },
            })
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
    /// Latest completed answer of the session (empty until there is one).
    pub answer: String,
    /// The bounded live feed.
    pub feed: Feed,
    /// Whether a turn is running (the spinner turns and stop applies).
    pub working: bool,
    /// Whether stopping the running turn was requested.
    pub cancelling: bool,
    /// Whether following the session stopped (the daemon went away, or it never started).
    pub disconnected: bool,
}

impl AgentState {
    const fn waiting() -> Self {
        Self {
            phase: AgentPhase::Waiting,
            status: String::new(),
            answer: String::new(),
            feed: Feed {
                turns: Vec::new(),
                permission: None,
                tokens: 0,
                cost: None,
            },
            working: false,
            cancelling: false,
            disconnected: false,
        }
    }

    fn failed(message: String) -> Self {
        let mut feed = Feed::default();
        feed.push(FeedItem::Error(message.clone()));
        Self {
            phase: AgentPhase::Failed,
            status: message,
            feed,
            disconnected: true,
            ..Self::waiting()
        }
    }

    /// Whether the session waits for the reviewer (a pending permission request).
    #[must_use]
    pub const fn needs_you(&self) -> bool {
        self.feed.permission.is_some()
    }

    /// State derived from a session snapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: &SessionViewSnapshot) -> Self {
        let mut feed = Feed::from_snapshot(snapshot);
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
        let outcome = snapshot.runtime.last_turn_outcome;
        let working = snapshot.runtime.active_turn_id.is_some()
            || snapshot.runtime_work.iter().any(|work| !work.is_terminal())
            || snapshot.transcript.items.iter().any(|item| item.streaming);
        let cancelling = working && snapshot.runtime.cancelling;
        if let SessionConnectionViewStatus::Error(message) = &snapshot.connection_status {
            feed.push(FeedItem::Error(message.clone()));
            return Self {
                answer,
                feed,
                ..Self::failed_quietly(message.clone())
            };
        }
        if !working && outcome == Some(bcode_session_models::ModelTurnOutcome::Cancelled) {
            feed.push(FeedItem::Notice("stopped".to_owned()));
            return Self {
                phase: AgentPhase::Answered,
                status: "stopped".to_owned(),
                answer,
                feed,
                working: false,
                cancelling: false,
                disconnected: false,
            };
        }
        let failed = !working
            && matches!(
                outcome,
                Some(
                    bcode_session_models::ModelTurnOutcome::Error
                        | bcode_session_models::ModelTurnOutcome::IdleTimeout
                        | bcode_session_models::ModelTurnOutcome::ProviderUnavailable
                )
            );
        if failed {
            let message = snapshot
                .runtime
                .last_turn_message
                .clone()
                .unwrap_or_else(|| "linked Bcode turn failed".to_owned());
            feed.push(FeedItem::Error(message.clone()));
            return Self {
                answer,
                feed,
                ..Self::failed_quietly(message)
            };
        }
        let (phase, status) = if feed.permission.is_some() {
            (AgentPhase::Working, "needs your permission".to_owned())
        } else if !snapshot.interactions.is_empty() {
            (
                AgentPhase::Working,
                "action needed in the linked session".to_owned(),
            )
        } else if cancelling {
            (AgentPhase::Working, "stopping…".to_owned())
        } else if working || answer.is_empty() {
            (AgentPhase::Working, "working…".to_owned())
        } else {
            (AgentPhase::Answered, "answered".to_owned())
        };
        Self {
            phase,
            status,
            answer,
            feed,
            working: working || phase == AgentPhase::Working,
            cancelling,
            disconnected: false,
        }
    }

    /// Following the session stopped with `message`: what was last observed (`last`) stays
    /// readable, nothing runs any more, and a pending permission request can no longer be
    /// answered from the review.
    fn stopped_following(last: Option<&Self>, message: String) -> Self {
        let Some(last) = last else {
            return Self::failed(message);
        };
        let mut feed = last.feed.clone();
        feed.permission = None;
        feed.push(FeedItem::Error(message.clone()));
        Self {
            answer: last.answer.clone(),
            feed,
            disconnected: true,
            ..Self::failed_quietly(message)
        }
    }

    /// Failed with `message` as the status, without adding it to the feed.
    fn failed_quietly(message: String) -> Self {
        Self {
            phase: AgentPhase::Failed,
            status: message,
            ..Self::waiting()
        }
    }
}

const fn projection() -> ProjectionWindowRequest {
    ProjectionWindowRequest {
        projection: SessionProjectionKind::Transcript,
        anchor: ProjectionWindowAnchor::Latest,
        direction: ProjectionWindowDirection::Backward,
        target: ProjectionWindowTarget {
            min_items: Some(24),
            min_estimated_rows: Some(60),
            min_bytes: None,
            width_columns: Some(100),
        },
        limits: ProjectionWindowLimits {
            max_items: 64,
            max_events_scanned: 2_048,
            max_bytes: 512 * 1024,
        },
    }
}

/// Keys shown on the feed's buttons (the first key of each feed action), set from the keymap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedKeys {
    /// Allow the pending permission request once.
    pub allow: String,
    /// Allow it and remember the rule.
    pub always: String,
    /// Decline it once.
    pub decline: String,
    /// Decline it and remember the rule.
    pub never: String,
    /// Stop the running turn.
    pub stop: String,
    /// Open the session in Bcode.
    pub open: String,
}

/// What changed in a session that the reviewer may need to know about while its card is off
/// screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attention {
    /// A permission request is waiting; the tool's name.
    Permission(String),
    /// A turn finished with an answer.
    Answered,
    /// A turn failed; the message.
    Failed(String),
}

/// Result of answering a permission request or stopping a turn from an AI card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedOutcome {
    /// The card's session.
    pub session: String,
    /// The answered permission request; `None` for a stop.
    pub permission_id: Option<String>,
    /// Why it failed; `None` when it succeeded.
    pub error: Option<String>,
}

/// Spinner frames and timing of a working session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveClock {
    /// Spinner frames elapsed since the session started working.
    pub step: u64,
    /// Whole seconds elapsed since the session started working.
    pub seconds: u64,
}

/// Live observation of every linked session of the review.
#[derive(Debug, Default)]
pub struct AgentStreams {
    subscriptions: BTreeMap<String, SessionFollow>,
    states: BTreeMap<String, AgentState>,
    failed: BTreeSet<String>,
    /// Per-session revision: changes when that session's card must be laid out again.
    revisions: BTreeMap<String, u64>,
    /// When each working session started working.
    since: BTreeMap<String, Instant>,
    clocks: BTreeMap<String, LiveClock>,
    /// Permission requests answered from the review whose resolution has not arrived yet.
    answering: BTreeSet<String>,
    attention: Vec<(String, Attention)>,
    keys: FeedKeys,
    keys_revision: u64,
    revision: u64,
}

impl AgentStreams {
    /// Changes whenever any session state changed.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Changes whenever the card of `session_id` must be laid out again: its state, spinner
    /// frame, elapsed time, a permission answer in flight, or the feed keys changed.
    #[must_use]
    pub fn session_revision(&self, session_id: &str) -> u64 {
        self.revisions
            .get(session_id)
            .copied()
            .unwrap_or_default()
            .wrapping_mul(0x2545_f491)
            ^ self.keys_revision
    }

    /// State of a linked session, once observed.
    #[must_use]
    pub fn state(&self, session_id: &str) -> Option<&AgentState> {
        self.states.get(session_id)
    }

    /// Spinner and elapsed time of a working session.
    #[must_use]
    pub fn clock(&self, session_id: &str) -> LiveClock {
        self.clocks.get(session_id).copied().unwrap_or_default()
    }

    /// Keys shown on the feed's buttons.
    #[must_use]
    pub const fn keys(&self) -> &FeedKeys {
        &self.keys
    }

    /// Use `keys` on the feed's buttons from now on.
    pub fn set_keys(&mut self, keys: FeedKeys) {
        if self.keys != keys {
            self.keys = keys;
            self.keys_revision = self.keys_revision.wrapping_add(1);
            self.revision += 1;
        }
    }

    /// Whether the answer to `permission_id` was sent and has not been applied yet.
    #[must_use]
    pub fn is_answering(&self, permission_id: &str) -> bool {
        self.answering.contains(permission_id)
    }

    /// Note that the answer to `permission_id` of `session_id` was sent (`true`) or failed
    /// (`false`, the prompt shows again).
    pub fn set_answering(&mut self, session_id: &str, permission_id: &str, answering: bool) {
        let changed = if answering {
            self.answering.insert(permission_id.to_owned())
        } else {
            self.answering.remove(permission_id)
        };
        if changed {
            self.bump(session_id);
        }
    }

    /// Changes noted since the last call, for attention toasts.
    pub fn take_attention(&mut self) -> Vec<(String, Attention)> {
        std::mem::take(&mut self.attention)
    }

    fn bump(&mut self, session_id: &str) {
        let revision = self.revisions.entry(session_id.to_owned()).or_default();
        *revision = revision.wrapping_add(1);
        self.revision += 1;
    }

    /// Subscribe to every session in `sessions` not observed yet. Returns whether any state
    /// changed.
    pub fn ensure<'a>(
        &mut self,
        host: &UiContext,
        sessions: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let sessions: BTreeSet<&str> = sessions.into_iter().collect();
        let mut changed = self.forget_unlinked(&sessions);
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
                self.bump(session);
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
            self.bump(session);
        }
        changed
    }

    /// Apply every pending update. Returns whether any state changed.
    pub fn drain(&mut self) -> bool {
        let mut changed = Vec::new();
        let mut stopped = Vec::new();
        for (session, subscription) in &mut self.subscriptions {
            let mut latest = None;
            loop {
                match subscription.updates.try_recv() {
                    Ok(SessionViewFollowUpdate::Snapshot(snapshot)) => {
                        latest = Some(AgentState::from_snapshot(&snapshot));
                    }
                    Ok(SessionViewFollowUpdate::Disconnected { message }) => {
                        stopped.push(session.clone());
                        let last = latest.as_ref().or_else(|| self.states.get(session));
                        latest = Some(AgentState::stopped_following(last, message));
                        break;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        stopped.push(session.clone());
                        let last = latest.as_ref().or_else(|| self.states.get(session));
                        latest = Some(AgentState::stopped_following(
                            last,
                            "session stream disconnected".to_owned(),
                        ));
                        break;
                    }
                }
            }
            if let Some(next) = latest
                && self.states.get(session) != Some(&next)
            {
                changed.push((session.clone(), next));
            }
        }
        let any = !changed.is_empty();
        for (session, next) in changed {
            self.apply(&session, next);
        }
        for session in stopped {
            self.subscriptions.remove(&session);
            self.failed.insert(session);
        }
        any
    }

    /// Replace the state of `session`, noting attention changes and the working clock.
    fn apply(&mut self, session: &str, next: AgentState) {
        let previous = self.states.get(session);
        let previous_permission = previous
            .and_then(|state| state.feed.permission.as_ref())
            .map(|permission| permission.permission_id.clone());
        let previous_phase = previous.map(|state| state.phase);
        if let Some(permission) = &next.feed.permission
            && previous_permission.as_deref() != Some(permission.permission_id.as_str())
        {
            self.attention.push((
                session.to_owned(),
                Attention::Permission(permission.tool.clone()),
            ));
        }
        if previous_phase != Some(next.phase) {
            match next.phase {
                AgentPhase::Answered if previous_phase == Some(AgentPhase::Working) => {
                    self.attention
                        .push((session.to_owned(), Attention::Answered));
                }
                AgentPhase::Failed => self
                    .attention
                    .push((session.to_owned(), Attention::Failed(next.status.clone()))),
                _ => {}
            }
        }
        // An answered request leaves the feed; forget that it was being answered.
        let pending: BTreeSet<&str> = next
            .feed
            .permission
            .iter()
            .map(|permission| permission.permission_id.as_str())
            .collect();
        if let Some(previous) = previous_permission
            && !pending.contains(previous.as_str())
        {
            self.answering.remove(&previous);
        }
        if next.working {
            self.since
                .entry(session.to_owned())
                .or_insert_with(Instant::now);
        } else {
            self.since.remove(session);
            self.clocks.remove(session);
        }
        self.states.insert(session.to_owned(), next);
        self.bump(session);
    }

    /// Advance the spinners of working sessions to `now`, with frames of `interval`. Returns
    /// how long until the next frame changes, when any session is working.
    pub fn tick(&mut self, now: Instant, interval: Duration) -> Option<Duration> {
        if interval.is_zero() {
            return None;
        }
        let mut next: Option<Duration> = None;
        let mut moved = Vec::new();
        for (session, since) in &self.since {
            let elapsed = now.saturating_duration_since(*since);
            let clock = LiveClock {
                step: u64::try_from(elapsed.as_nanos() / interval.as_nanos()).unwrap_or(u64::MAX),
                seconds: elapsed.as_secs(),
            };
            if self.clocks.get(session) != Some(&clock) {
                moved.push((session.clone(), clock));
            }
            let remaining = interval.as_nanos() - elapsed.as_nanos() % interval.as_nanos();
            let remaining = Duration::from_nanos(u64::try_from(remaining).unwrap_or(u64::MAX));
            next = Some(next.map_or(remaining, |next| next.min(remaining)));
        }
        for (session, clock) in moved {
            self.clocks.insert(session.clone(), clock);
            self.bump(&session);
        }
        next
    }

    /// Stop following sessions no card links any more (their questions were deleted), and drop
    /// what was observed of them. Returns whether any was dropped.
    fn forget_unlinked(&mut self, linked: &BTreeSet<&str>) -> bool {
        let unlinked: Vec<String> = self
            .states
            .keys()
            .chain(self.subscriptions.keys())
            .chain(self.failed.iter())
            .filter(|session| !linked.contains(session.as_str()))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for session in &unlinked {
            if let Some(permission) = self
                .states
                .get(session)
                .and_then(|state| state.feed.permission.as_ref())
            {
                self.answering.remove(&permission.permission_id);
            }
            self.retry(session);
        }
        !unlinked.is_empty()
    }

    /// Forget a stopped observation so the next [`Self::ensure`] subscribes again (retry).
    pub fn retry(&mut self, session: &str) {
        self.failed.remove(session);
        self.subscriptions.remove(session);
        self.states.remove(session);
        self.since.remove(session);
        self.clocks.remove(session);
        self.bump(session);
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
            threads_omitted: 0,
            threads_on: "src/lib.rs".to_owned(),
            path: "src/lib.rs".to_owned(),
            old: Some((3, 4)),
            new: None,
            lines: (0..500)
                .map(|line| format!("+line {line} {}", "x".repeat(80)))
                .collect(),
            thread: None,
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

    fn context(thread: Option<AskThread>, path: &str) -> AskContext {
        AskContext {
            title: "#7 Retry sends".to_owned(),
            repo: PathBuf::from("/repo"),
            sources: vec!["Pull request o/r#7".to_owned()],
            threads: Vec::new(),
            threads_omitted: 0,
            threads_on: path.to_owned(),
            path: path.to_owned(),
            old: None,
            new: None,
            lines: Vec::new(),
            thread,
            question: "Still a problem?".to_owned(),
        }
    }

    fn thread(comments: Vec<AskComment>) -> AskThread {
        AskThread {
            origin: "a GitHub thread on pull request o/r#7".to_owned(),
            id: "o/r#7:T1".to_owned(),
            resolved: false,
            outdated: true,
            written_on: Some(WrittenOn {
                path: "src/retry.rs".to_owned(),
                lines: Some((12, 14)),
                old_side: false,
                commit: Some("abc".to_owned()),
            }),
            shown_at: "the whole review".to_owned(),
            comments,
            diff_hunk: Some(
                (0..100)
                    .map(|line| format!("+line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            url: None,
        }
    }

    fn said(author: &str, body: String) -> AskComment {
        AskComment {
            author: author.to_owned(),
            body,
        }
    }

    #[test]
    fn a_long_thread_keeps_its_first_and_latest_comments_and_its_hunks_end() {
        let mut comments = vec![said("kevin", "The opening comment.".to_owned())];
        comments.extend((1..=30).map(|index| said("ana", format!("reply {index}"))));
        comments.push(said("ana", "y".repeat(5_000)));
        let text = prompt(&context(Some(thread(comments)), REVIEW));
        assert!(text.contains("--- kevin:\nThe opening comment."), "{text}");
        assert!(text.contains("replies left out; code_review.get_thread reads them all"));
        assert!(text.contains("reply 30"), "the latest replies are kept");
        assert!(!text.contains("reply 1\n"), "the middle ones are left out");
        assert!(
            text.contains("[comment cut]"),
            "a long comment is cut, marked"
        );
        assert!(!text.contains(&"y".repeat(2_100)));
        assert!(text.contains("[60 earlier lines left out]\n+line 60"));
        assert!(text.contains("+line 99\n```"));
        assert!(text.contains("Written on: src/retry.rs, new lines 12-14, at commit abc"));
        assert!(text.contains("outdated: its code changed since it was written, open"));
        assert!(text.ends_with("Question:\nStill a problem?\n"));
    }

    #[test]
    fn the_question_survives_when_the_context_is_too_long() {
        let mut long = context(None, "src/lib.rs");
        long.lines = (0..MAX_PROMPT_LINES)
            .map(|line| format!("+{line} {}", "x".repeat(400)))
            .collect();
        long.question = "q".repeat(10_000);
        let text = prompt(&long);
        assert!(text.len() <= MAX_PROMPT_BYTES, "{}", text.len());
        assert!(text.contains("[… context cut to fit; the code_review tools read the rest]"));
        assert!(text.contains(&format!("Question:\n{}", "q".repeat(4_000))));
        assert!(text.ends_with("[question cut]\n"));
    }

    #[test]
    fn session_titles_name_the_file_or_review_and_the_threads_author() {
        let from_thread = context(Some(thread(vec![said("kevin", "Why?".to_owned())])), REVIEW);
        assert_eq!(session_title(&from_thread), "Review: src/retry.rs · kevin");
        assert_eq!(
            session_title(&context(None, "src/lib.rs")),
            "Review: src/lib.rs"
        );
        assert_eq!(
            session_title(&context(None, REVIEW)),
            "Review: #7 Retry sends"
        );
        let mut long = context(None, REVIEW);
        long.title = "t".repeat(200);
        assert!(session_title(&long).chars().count() <= MAX_TITLE_CHARS + 1);
    }

    const REVIEW: &str = bcode_code_review::document::REVIEW_LEVEL_PATH;
}
