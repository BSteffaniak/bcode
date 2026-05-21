//! Transcript item projection for the BMUX backend.

use bcode_session_models::{SessionEvent, SessionEventKind};

use super::diff_extract::diff_from_tool_request;

/// Renderable transcript item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptItem {
    pub(super) role: &'static str,
    pub(super) text: String,
    pub(super) streaming: bool,
}

impl TranscriptItem {
    pub(super) const fn new(role: &'static str, text: String) -> Self {
        Self {
            role,
            text,
            streaming: false,
        }
    }

    pub(super) const fn new_streaming(role: &'static str, text: String) -> Self {
        Self {
            role,
            text,
            streaming: true,
        }
    }

    /// Return display role.
    #[must_use]
    pub(super) const fn role(&self) -> &'static str {
        self.role
    }

    /// Return display text.
    #[must_use]
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// Return whether this item is currently streaming.
    #[must_use]
    pub(super) const fn streaming(&self) -> bool {
        self.streaming
    }
}

/// Project session events into transcript items.
#[must_use]
pub(super) fn transcript_items_from_events(events: &[SessionEvent]) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    for event in events {
        push_transcript_item_from_event(&mut items, event);
    }
    items
}

/// Merge streaming transcript items across a prepended history boundary.
pub(super) fn merge_transcript_boundary(
    older: &mut Vec<TranscriptItem>,
    current: &mut Vec<TranscriptItem>,
) {
    let (Some(last_older), Some(first_current)) = (older.last_mut(), current.first()) else {
        return;
    };
    if last_older.role != first_current.role || !last_older.streaming {
        return;
    }
    if first_current.streaming {
        last_older.text.push_str(&first_current.text);
        current.remove(0);
    } else {
        older.pop();
    }
}

/// Build a transcript item for a tool request.
#[must_use]
pub(super) fn tool_request_item(
    tool_call_id: &str,
    tool_name: &str,
    arguments_json: &str,
) -> TranscriptItem {
    let diff_note = diff_from_tool_request(tool_name, arguments_json).map_or_else(
        String::new,
        |(summary, _lines)| {
            format!(
                "\nDiff: {} (+{} -{})",
                summary.display_path(),
                summary.added,
                summary.removed
            )
        },
    );
    TranscriptItem::new(
        "Tool",
        format!(
            "request {tool_name}\nCall: {tool_call_id}{diff_note}\nArguments:\n{}",
            pretty_jsonish(arguments_json)
        ),
    )
}

/// Format optional token counts.
#[must_use]
pub(super) fn optional_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

/// Truncate JSON-like values for transcript display.
#[must_use]
pub(super) fn pretty_jsonish(value: &str) -> String {
    truncate_block(value, 2_000)
}

/// Truncate long transcript blocks.
#[must_use]
pub(super) fn truncate_block(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            output.push_str("\n… truncated");
            return output;
        }
        output.push(ch);
    }
    output
}

fn push_transcript_item_from_event(items: &mut Vec<TranscriptItem>, event: &SessionEvent) {
    match &event.kind {
        SessionEventKind::AssistantDelta { text } => {
            push_streaming_transcript_item(items, "Assistant", text);
        }
        SessionEventKind::AssistantMessage { text } => {
            finish_streaming_transcript_item(items, "Assistant", text);
        }
        SessionEventKind::AssistantReasoningDelta { text } => {
            push_streaming_transcript_item(items, "Reasoning", text);
        }
        SessionEventKind::AssistantReasoningMessage { text } => {
            finish_streaming_transcript_item(items, "Reasoning", text);
        }
        _ => {
            if let Some(item) = non_streaming_transcript_item_from_event(event) {
                items.push(item);
            }
        }
    }
}

fn push_streaming_transcript_item(items: &mut Vec<TranscriptItem>, role: &'static str, text: &str) {
    if let Some(last) = items.last_mut()
        && last.role == role
        && last.streaming
    {
        last.text.push_str(text);
        return;
    }
    items.push(TranscriptItem::new_streaming(role, text.to_owned()));
}

fn finish_streaming_transcript_item(
    items: &mut Vec<TranscriptItem>,
    role: &'static str,
    text: &str,
) {
    if let Some(last) = items.last_mut()
        && last.role == role
        && last.streaming
    {
        last.text.clear();
        last.text.push_str(text);
        last.streaming = false;
        return;
    }
    items.push(TranscriptItem::new(role, text.to_owned()));
}

fn non_streaming_transcript_item_from_event(event: &SessionEvent) -> Option<TranscriptItem> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => {
            Some(TranscriptItem::new("You", text.clone()))
        }
        SessionEventKind::SystemMessage { text } => {
            Some(TranscriptItem::new("System", text.clone()))
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
        } => Some(tool_request_item(tool_call_id, tool_name, arguments_json)),
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
        } => Some(TranscriptItem::new(
            if *is_error { "Tool error" } else { "Tool" },
            format!(
                "result for {tool_call_id}\n{}",
                truncate_block(result, 4_000)
            ),
        )),
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        } => Some(TranscriptItem::new(
            "Permission",
            format!(
                "waiting for approval: {tool_name}\nPermission: {permission_id}\nCall: {tool_call_id}\nArguments:\n{}",
                pretty_jsonish(arguments_json)
            ),
        )),
        SessionEventKind::ContextCompacted { summary, .. } => Some(TranscriptItem::new(
            "Compaction",
            format!("context compacted: {summary}"),
        )),
        SessionEventKind::SkillInvoked {
            skill_id,
            arguments,
            source,
            ..
        } => Some(TranscriptItem::new(
            "Skill",
            format!(
                "invoked {skill_id}{}\nArguments: {arguments}",
                source
                    .as_ref()
                    .map_or_else(String::new, |source| format!("\nSource: {}", source.label))
            ),
        )),
        SessionEventKind::SkillInvocationFailed {
            skill_id, error, ..
        } => Some(TranscriptItem::new(
            "Skill error",
            format!("{skill_id}: {error}"),
        )),
        SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. }
        | SessionEventKind::PermissionResolved { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::ModelUsage { .. }
        | SessionEventKind::SessionRenamed { .. }
        | SessionEventKind::SkillSuggested { .. }
        | SessionEventKind::SkillActivated { .. }
        | SessionEventKind::SkillDeactivated { .. }
        | SessionEventKind::SkillContextLoaded { .. }
        | SessionEventKind::TraceEvent { .. }
        | SessionEventKind::SessionCreated { .. }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::AgentChanged { .. } => None,
    }
}
