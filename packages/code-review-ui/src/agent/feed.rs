//! A bounded live feed of one linked session, derived from its semantic view.
//!
//! A [`Feed`] keeps only what an AI card shows: per turn (one question sent to the session), the
//! reasoning as a word count with bounded text, tool lines with their state and the tail of
//! their output, the streamed answer, notices, and errors. Session-wide it keeps the first
//! pending permission request and the token and cost totals. Everything is bounded: the turns,
//! the items per turn, output lines per tool, and the bytes of every text, so a long session
//! costs the same memory as a short one; the full transcript stays in Bcode (`o` opens it).

use bcode_session_view_models::{
    SessionViewSnapshot, ToolInvocationView, ToolInvocationViewStatus, ToolResultView,
    TranscriptViewItem, TranscriptViewItemKind,
};

/// Turns kept per session (the newest).
pub const MAX_TURNS: usize = 8;
/// Items kept per turn (the newest).
pub const MAX_TURN_ITEMS: usize = 24;
/// Output lines kept per tool (the last).
pub const MAX_OUTPUT_LINES: usize = 200;
/// Output lines a collapsed tool line shows.
pub const TOOL_TAIL_LINES: usize = 3;
/// Bytes kept of an answer (its start; an answer is read from the top).
const MAX_ANSWER_BYTES: usize = 64 * 1024;
/// Bytes kept of reasoning, a question, a notice, or an error.
const MAX_TEXT_BYTES: usize = 8 * 1024;
/// Bytes kept of one output line or a tool summary.
const MAX_LINE_BYTES: usize = 512;

/// Where the question starts in the prompts the review sends.
const QUESTION_MARKER: &str = "\nQuestion:\n";

/// State of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    /// Requested or running.
    Running,
    /// Finished successfully.
    Done,
    /// Failed or cancelled.
    Failed,
}

/// One tool call of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLine {
    /// Stable id within the session (the provider's tool call id).
    pub id: String,
    /// Tool name.
    pub name: String,
    /// One-line summary of its arguments.
    pub summary: String,
    /// State.
    pub state: ToolState,
    /// The last [`MAX_OUTPUT_LINES`] lines of its output.
    pub output: Vec<String>,
    /// Lines of the whole output.
    pub total_lines: usize,
}

/// One item of a turn, in session order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedItem {
    /// Model reasoning, shown collapsed as a word count.
    Reasoning {
        /// Stable id within the session.
        id: String,
        /// Words of the whole reasoning.
        words: usize,
        /// The reasoning (bounded).
        text: String,
    },
    /// A tool call.
    Tool(ToolLine),
    /// Answer text, markdown.
    Answer {
        /// The text (bounded).
        text: String,
        /// Whether it is still streaming.
        streaming: bool,
    },
    /// A neutral notice (a system message, a compaction).
    Notice(String),
    /// An error.
    Error(String),
}

/// One question sent to the session and what followed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Turn {
    /// The question: the review prompt's question, or the whole user message otherwise.
    /// Empty for items before the first question inside the observed window.
    pub question: String,
    /// Items, oldest first (the newest [`MAX_TURN_ITEMS`]).
    pub items: Vec<FeedItem>,
    /// Items dropped before the first kept one.
    pub earlier: usize,
}

impl Turn {
    /// The latest finished answer of the turn, trimmed; empty without one.
    #[must_use]
    pub fn answer(&self) -> &str {
        self.items
            .iter()
            .rev()
            .find_map(|item| match item {
                FeedItem::Answer {
                    text,
                    streaming: false,
                } if !text.trim().is_empty() => Some(text.trim()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn push(&mut self, item: FeedItem) {
        self.items.push(item);
        if self.items.len() > MAX_TURN_ITEMS {
            self.items.remove(0);
            self.earlier += 1;
        }
    }
}

/// A pending permission request of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    /// Permission id, to answer it.
    pub permission_id: String,
    /// Requested tool.
    pub tool: String,
    /// One-line summary of the tool's arguments.
    pub summary: String,
    /// Why the policy asks, when it says.
    pub detail: Option<String>,
    /// Whether the answer can be remembered as a rule (Always, Never).
    pub can_remember: bool,
    /// Further pending requests behind this one.
    pub more: usize,
}

/// The bounded feed of one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Feed {
    /// Turns, oldest first (the newest [`MAX_TURNS`]).
    pub turns: Vec<Turn>,
    /// The first pending permission request.
    pub permission: Option<PendingPermission>,
    /// Metered tokens of the session.
    pub tokens: u64,
    /// Estimated cost of the session, formatted (`$0.0123`), when known.
    pub cost: Option<String>,
}

impl Feed {
    /// The feed of a session view.
    #[must_use]
    pub fn from_snapshot(snapshot: &SessionViewSnapshot) -> Self {
        let mut turns: Vec<Turn> = Vec::new();
        for item in &snapshot.transcript.items {
            if let TranscriptViewItemKind::UserMessage { message } = &item.kind {
                turns.push(Turn {
                    question: question_of(&message.text),
                    ..Turn::default()
                });
                if turns.len() > MAX_TURNS {
                    turns.remove(0);
                }
                continue;
            }
            let Some(converted) = feed_item(item) else {
                continue;
            };
            if turns.is_empty() {
                turns.push(Turn::default());
            }
            if let Some(turn) = turns.last_mut() {
                turn.push(converted);
            }
        }
        let permission = snapshot
            .permissions
            .iter()
            .find(|permission| !permission.resolved)
            .map(|permission| PendingPermission {
                permission_id: permission.permission_id.clone(),
                tool: permission.tool_name.clone(),
                summary: arguments_summary(Some(&permission.arguments_json)),
                detail: permission
                    .detail
                    .as_deref()
                    .map(|detail| bounded(detail.trim(), MAX_LINE_BYTES)),
                can_remember: permission.can_remember,
                more: snapshot
                    .permissions
                    .iter()
                    .filter(|permission| !permission.resolved)
                    .count()
                    .saturating_sub(1),
            });
        Self {
            turns,
            permission,
            tokens: snapshot.runtime.cumulative_metered_tokens,
            cost: cost_label(&snapshot.runtime.cost.totals_micros),
        }
    }

    /// Append `item` (an error or a notice about the session) to the newest turn.
    pub fn push(&mut self, item: FeedItem) {
        let item = match item {
            FeedItem::Error(text) => FeedItem::Error(bounded(&text, MAX_TEXT_BYTES)),
            FeedItem::Notice(text) => FeedItem::Notice(bounded(&text, MAX_TEXT_BYTES)),
            item => item,
        };
        if self.turns.is_empty() {
            self.turns.push(Turn::default());
        }
        if let Some(turn) = self.turns.last_mut() {
            turn.push(item);
        }
    }

    /// The newest turn asking `question` (trimmed), with its index.
    #[must_use]
    pub fn turn_for(&self, question: &str) -> Option<(usize, &Turn)> {
        let question = question.trim();
        self.turns
            .iter()
            .enumerate()
            .rev()
            .find(|(_, turn)| turn.question == question)
    }
}

/// The question of a user message: the text after the review prompt's `Question:` marker, else
/// the whole message, trimmed and bounded.
#[must_use]
pub fn question_of(text: &str) -> String {
    let question = text
        .rfind(QUESTION_MARKER)
        .map_or(text, |at| &text[at + QUESTION_MARKER.len()..]);
    bounded(question.trim(), MAX_TEXT_BYTES)
}

fn feed_item(item: &TranscriptViewItem) -> Option<FeedItem> {
    match &item.kind {
        TranscriptViewItemKind::AssistantMessage { message } => (!message.text.trim().is_empty())
            .then(|| FeedItem::Answer {
                text: bounded(&message.text, MAX_ANSWER_BYTES),
                streaming: item.streaming,
            }),
        TranscriptViewItemKind::ReasoningMessage { message } => Some(reasoning(
            format!("reasoning:{}", item.sequence.unwrap_or_default()),
            &message.text,
        )),
        TranscriptViewItemKind::ReasoningActivity { activity } => Some(reasoning(
            format!("reasoning:{}:{}", activity.turn_id, activity.activity_id),
            &activity.text(),
        )),
        TranscriptViewItemKind::ToolInvocation { tool } => Some(FeedItem::Tool(tool_line(tool))),
        TranscriptViewItemKind::ToolRequestDraft { draft } => Some(FeedItem::Tool(ToolLine {
            id: draft.tool_call_id.clone(),
            name: draft.tool_name.clone(),
            summary: String::new(),
            state: ToolState::Running,
            output: Vec::new(),
            total_lines: 0,
        })),
        TranscriptViewItemKind::SystemMessage { message } => (!message.text.trim().is_empty())
            .then(|| FeedItem::Notice(bounded(message.text.trim(), MAX_TEXT_BYTES))),
        TranscriptViewItemKind::Compaction { compaction } => Some(FeedItem::Notice(bounded(
            compaction.text.trim(),
            MAX_TEXT_BYTES,
        ))),
        TranscriptViewItemKind::Interaction { interaction } if !interaction.resolved => {
            Some(FeedItem::Notice(format!(
                "{} is waiting for input in Bcode",
                interaction.title.as_deref().unwrap_or("a tool")
            )))
        }
        _ => None,
    }
}

fn reasoning(id: String, text: &str) -> FeedItem {
    FeedItem::Reasoning {
        id,
        words: text.split_whitespace().count(),
        text: bounded(text.trim(), MAX_TEXT_BYTES),
    }
}

fn tool_line(tool: &ToolInvocationView) -> ToolLine {
    let state = match tool.status {
        ToolInvocationViewStatus::Requested
        | ToolInvocationViewStatus::Running
        | ToolInvocationViewStatus::Waiting => ToolState::Running,
        ToolInvocationViewStatus::Finished if tool.is_error == Some(true) => ToolState::Failed,
        ToolInvocationViewStatus::Finished => ToolState::Done,
        ToolInvocationViewStatus::Cancelled | ToolInvocationViewStatus::Failed => ToolState::Failed,
    };
    let output = tool.result_text.as_deref().or(match &tool.result {
        Some(ToolResultView::Text { text }) => Some(text.as_str()),
        Some(ToolResultView::Json { value }) => Some(value.as_str()),
        _ => None,
    });
    let (output, total_lines) = output.map_or_else(|| (Vec::new(), 0), output_tail);
    ToolLine {
        id: tool.tool_call_id.clone(),
        name: tool.tool_name.clone().unwrap_or_else(|| "tool".to_owned()),
        summary: arguments_summary(tool.arguments_json.as_deref()),
        state,
        output,
        total_lines,
    }
}

/// The last [`MAX_OUTPUT_LINES`] non-trailing-blank lines of `output`, and the line count.
fn output_tail(output: &str) -> (Vec<String>, usize) {
    let lines: Vec<&str> = output.trim_end().lines().collect();
    let total = lines.len();
    let kept = lines[total.saturating_sub(MAX_OUTPUT_LINES)..]
        .iter()
        .map(|line| plain(line))
        .collect();
    (kept, total)
}

/// Argument keys that best summarize a call, in order of preference.
const SUMMARY_KEYS: &[&str] = &[
    "command",
    "cmd",
    "path",
    "file_path",
    "pattern",
    "query",
    "url",
    "description",
];

/// One line summarizing JSON tool arguments: the first preferred string argument, else the
/// first string argument, else nothing.
fn arguments_summary(arguments: Option<&str>) -> String {
    let Some(serde_json::Value::Object(map)) =
        arguments.and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok())
    else {
        return String::new();
    };
    let preferred = SUMMARY_KEYS
        .iter()
        .find_map(|key| map.get(*key).and_then(serde_json::Value::as_str));
    let text = preferred
        .or_else(|| map.values().find_map(serde_json::Value::as_str))
        .unwrap_or_default();
    plain(
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .as_str(),
    )
}

/// `text` as one plain line: tabs become spaces, other control characters are dropped, and
/// it is bounded to [`MAX_LINE_BYTES`].
fn plain(text: &str) -> String {
    let text: String = text
        .chars()
        .filter_map(|character| match character {
            '\t' => Some(' '),
            character if character.is_control() => None,
            character => Some(character),
        })
        .collect();
    bounded(&text, MAX_LINE_BYTES)
}

/// `text` cut to at most `max` bytes on a character boundary.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Estimated cost as `$0.0123` (US dollars) or `0.0123 EUR`, the largest total first.
fn cost_label(totals: &std::collections::BTreeMap<String, u64>) -> Option<String> {
    let (currency, micros) = totals
        .iter()
        .filter(|(_, micros)| **micros > 0)
        .max_by_key(|(_, micros)| **micros)?;
    let whole = micros / 1_000_000;
    let fraction = micros % 1_000_000 / 100;
    Some(if currency == "USD" {
        format!("${whole}.{fraction:04}")
    } else {
        format!("{whole}.{fraction:04} {currency}")
    })
}

/// Tokens as `950 tokens`, `12.3k tokens` or `1.2M tokens`.
#[must_use]
pub fn tokens_label(tokens: u64) -> String {
    match tokens {
        0..1_000 => format!("{tokens} tokens"),
        1_000..1_000_000 => format!("{}.{}k tokens", tokens / 1_000, tokens % 1_000 / 100),
        _ => format!(
            "{}.{}M tokens",
            tokens / 1_000_000,
            tokens % 1_000_000 / 100_000
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::{ChatMessageView, TranscriptViewItemId};

    fn item(id: &str, kind: TranscriptViewItemKind) -> TranscriptViewItem {
        TranscriptViewItem {
            id: TranscriptViewItemId::new(id),
            revision: 1,
            sequence: Some(1),
            timestamp_ms: Some(1),
            output_location: None,
            streaming: false,
            kind,
        }
    }

    #[test]
    fn turns_split_at_questions_and_stay_bounded() {
        let mut snapshot = SessionViewSnapshot::empty();
        for turn in 0..(MAX_TURNS + 3) {
            snapshot.transcript.items.push(item(
                "user",
                TranscriptViewItemKind::UserMessage {
                    message: ChatMessageView::markdown(format!(
                        "You are helping…\n\nQuestion:\nquestion {turn}\n"
                    )),
                },
            ));
            for answer in 0..(MAX_TURN_ITEMS + 2) {
                snapshot.transcript.items.push(item(
                    "answer",
                    TranscriptViewItemKind::AssistantMessage {
                        message: ChatMessageView::markdown(format!("answer {turn}.{answer}")),
                    },
                ));
            }
        }
        let feed = Feed::from_snapshot(&snapshot);
        assert_eq!(feed.turns.len(), MAX_TURNS);
        assert_eq!(feed.turns[0].question, "question 3");
        let (index, turn) = feed.turn_for("  question 10 ").expect("the newest turn");
        assert_eq!(index, MAX_TURNS - 1);
        assert_eq!(turn.items.len(), MAX_TURN_ITEMS);
        assert_eq!(turn.earlier, 2);
        assert_eq!(turn.answer(), format!("answer 10.{}", MAX_TURN_ITEMS + 1));
        assert!(feed.turn_for("question 1").is_none(), "dropped turns");
    }

    #[test]
    fn tool_output_keeps_a_bounded_tail_and_summaries_are_one_line() {
        let output: Vec<String> = (0..(MAX_OUTPUT_LINES + 50))
            .map(|line| format!("line\t{line}\u{7}"))
            .collect();
        let (tail, total) = output_tail(&output.join("\n"));
        assert_eq!(total, MAX_OUTPUT_LINES + 50);
        assert_eq!(tail.len(), MAX_OUTPUT_LINES);
        assert_eq!(tail.last().map(String::as_str), Some("line 249"));
        assert_eq!(
            arguments_summary(Some(r#"{"timeout": 5, "command": "cargo\n  test -p x"}"#)),
            "cargo test -p x"
        );
        assert_eq!(arguments_summary(Some(r#"{"other": "value"}"#)), "value");
        assert_eq!(arguments_summary(Some("not json")), "");
    }

    #[test]
    fn labels_format_tokens_and_cost() {
        assert_eq!(tokens_label(950), "950 tokens");
        assert_eq!(tokens_label(12_345), "12.3k tokens");
        assert_eq!(tokens_label(1_250_000), "1.2M tokens");
        let totals = std::iter::once(("USD".to_owned(), 12_345)).collect();
        assert_eq!(cost_label(&totals).as_deref(), Some("$0.0123"));
        assert_eq!(cost_label(&std::collections::BTreeMap::new()), None);
    }

    #[test]
    fn questions_come_from_review_prompts_or_whole_messages() {
        assert_eq!(question_of("context\n\nQuestion:\n  Why?  \n"), "Why?");
        assert_eq!(question_of("  typed in Bcode "), "typed in Bcode");
    }
}
