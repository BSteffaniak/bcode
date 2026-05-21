//! Transcript item projection for the BMUX backend.

use bcode_session_models::{SessionEvent, SessionEventKind, SessionTokenUsage};

use super::diff_extract::diff_from_tool_request;

/// Semantic transcript item type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TranscriptItemKind {
    /// User-authored chat message.
    UserMessage,
    /// Assistant-authored chat message.
    AssistantMessage,
    /// Assistant reasoning/thinking content.
    ReasoningMessage,
    /// Tool-call request with structured metadata.
    ToolRequest {
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// Optional diff summary for filesystem edit/write tools.
        diff_summary: Option<String>,
    },
    /// Tool-call result with structured metadata.
    ToolResult {
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// Token usage telemetry for a model turn.
    Usage {
        /// Model turn identifier.
        turn_id: String,
    },
    /// Permission request for a tool call.
    PermissionRequest {
        /// Permission identifier.
        permission_id: String,
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
    },
    /// Permission resolution.
    PermissionResult {
        /// Whether the permission was approved.
        approved: bool,
    },
    /// System message.
    System,
    /// Low-prominence metadata.
    Meta,
    /// Skill-related note.
    Skill,
    /// Skill failure note.
    SkillError,
    /// Generic fallback item.
    Generic,
}

/// Renderable transcript item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptItem {
    pub(super) role: &'static str,
    pub(super) text: String,
    pub(super) streaming: bool,
    kind: TranscriptItemKind,
}

impl TranscriptItem {
    pub(super) fn new(role: &'static str, text: String) -> Self {
        Self {
            role,
            text,
            streaming: false,
            kind: kind_for_role(role),
        }
    }

    pub(super) fn new_streaming(role: &'static str, text: String) -> Self {
        Self {
            role,
            text,
            streaming: true,
            kind: kind_for_role(role),
        }
    }

    const fn with_kind(
        role: &'static str,
        text: String,
        streaming: bool,
        kind: TranscriptItemKind,
    ) -> Self {
        Self {
            role,
            text,
            streaming,
            kind,
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

    /// Return semantic item kind.
    #[must_use]
    pub(super) const fn kind(&self) -> &TranscriptItemKind {
        &self.kind
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
    let diff_summary =
        diff_from_tool_request(tool_name, arguments_json).map(|(summary, _lines)| {
            format!(
                "{} (+{} -{})",
                summary.display_path(),
                summary.added,
                summary.removed
            )
        });
    TranscriptItem::with_kind(
        "Tool",
        pretty_jsonish(arguments_json),
        false,
        TranscriptItemKind::ToolRequest {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            diff_summary,
        },
    )
}

/// Build a transcript item for a tool result.
#[must_use]
pub(super) fn tool_result_item(tool_call_id: &str, result: &str, is_error: bool) -> TranscriptItem {
    TranscriptItem::with_kind(
        if is_error { "Tool error" } else { "Tool" },
        truncate_block(result, 4_000),
        false,
        TranscriptItemKind::ToolResult {
            tool_call_id: tool_call_id.to_owned(),
            is_error,
        },
    )
}

/// Build a transcript item for a permission request.
#[must_use]
pub(super) fn permission_request_item(
    permission_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    arguments_json: &str,
) -> TranscriptItem {
    TranscriptItem::with_kind(
        "Permission",
        pretty_jsonish(arguments_json),
        false,
        TranscriptItemKind::PermissionRequest {
            permission_id: permission_id.to_owned(),
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
        },
    )
}

/// Build a transcript item for a permission result.
#[must_use]
pub(super) fn permission_result_item(permission_id: &str, approved: bool) -> TranscriptItem {
    let status = if approved {
        "permission approved"
    } else {
        "permission denied"
    };
    TranscriptItem::with_kind(
        "Permission",
        format!("{status}: {permission_id}"),
        false,
        TranscriptItemKind::PermissionResult { approved },
    )
}

/// Build a compact transcript item for model token usage.
#[must_use]
pub(super) fn model_usage_item(turn_id: &str, usage: &SessionTokenUsage) -> TranscriptItem {
    TranscriptItem::with_kind(
        "Usage",
        format!(
            "input {} · output {} · total {} · cached {} · cache write {} · reasoning {}",
            optional_u32(usage.input_tokens),
            optional_u32(usage.output_tokens),
            optional_u32(usage.metered_total_tokens()),
            optional_u32(usage.cached_input_tokens),
            optional_u32(usage.cache_write_input_tokens),
            optional_u32(usage.reasoning_tokens),
        ),
        false,
        TranscriptItemKind::Usage {
            turn_id: turn_id.to_owned(),
        },
    )
}

/// Format optional token counts.
#[must_use]
pub(super) fn optional_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

/// Format JSON-like values for transcript display.
#[must_use]
pub(super) fn pretty_jsonish(value: &str) -> String {
    serde_json::from_str::<serde_json::Value>(value).map_or_else(
        |_| truncate_block(value, 2_000),
        |json| {
            serde_json::to_string_pretty(&json).map_or_else(
                |_| truncate_block(value, 2_000),
                |pretty| truncate_block(&pretty, 2_000),
            )
        },
    )
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

/// Append streamed text to the currently open transcript stream for `role`.
///
/// Interleaved telemetry rows, such as token usage, may be appended while a model stream is open.
/// The open stream is therefore the newest streaming row for the same role, not necessarily the
/// final transcript row.
pub(super) fn push_streaming_transcript_item(
    items: &mut Vec<TranscriptItem>,
    role: &'static str,
    text: &str,
) {
    if let Some(item) = latest_streaming_item_mut(items, role) {
        item.text.push_str(text);
        return;
    }
    items.push(TranscriptItem::new_streaming(role, text.to_owned()));
}

/// Finish the currently open transcript stream for `role`, or append a final item if none exists.
pub(super) fn finish_streaming_transcript_item(
    items: &mut Vec<TranscriptItem>,
    role: &'static str,
    text: &str,
) {
    if let Some(item) = latest_streaming_item_mut(items, role) {
        item.text.clear();
        item.text.push_str(text);
        item.streaming = false;
        return;
    }
    items.push(TranscriptItem::new(role, text.to_owned()));
}

fn latest_streaming_item_mut<'items>(
    items: &'items mut [TranscriptItem],
    role: &'static str,
) -> Option<&'items mut TranscriptItem> {
    items
        .iter_mut()
        .rev()
        .find(|item| item.role == role && item.streaming)
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
        } => Some(tool_result_item(tool_call_id, result, *is_error)),
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        } => Some(permission_request_item(
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        )),
        SessionEventKind::PermissionResolved {
            permission_id,
            approved,
            ..
        } => Some(permission_result_item(permission_id, *approved)),
        SessionEventKind::ModelUsage { turn_id, usage } => Some(model_usage_item(turn_id, usage)),
        SessionEventKind::ContextCompacted { summary, .. } => Some(TranscriptItem::with_kind(
            "Compaction",
            format!("context compacted: {summary}"),
            false,
            TranscriptItemKind::Meta,
        )),
        SessionEventKind::SkillInvoked {
            skill_id,
            arguments,
            source,
            ..
        } => Some(TranscriptItem::with_kind(
            "Skill",
            format!(
                "invoked {skill_id}{}\nArguments: {arguments}",
                source
                    .as_ref()
                    .map_or_else(String::new, |source| format!("\nSource: {}", source.label))
            ),
            false,
            TranscriptItemKind::Skill,
        )),
        SessionEventKind::SkillInvocationFailed {
            skill_id, error, ..
        } => Some(TranscriptItem::with_kind(
            "Skill error",
            format!("{skill_id}: {error}"),
            false,
            TranscriptItemKind::SkillError,
        )),
        SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelTurnFinished { .. }
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

fn kind_for_role(role: &str) -> TranscriptItemKind {
    match role {
        "You" => TranscriptItemKind::UserMessage,
        "Assistant" => TranscriptItemKind::AssistantMessage,
        "Reasoning" => TranscriptItemKind::ReasoningMessage,
        "System" => TranscriptItemKind::System,
        "Skill" => TranscriptItemKind::Skill,
        "Skill error" => TranscriptItemKind::SkillError,
        "Compaction" | "Meta" => TranscriptItemKind::Meta,
        _ => TranscriptItemKind::Generic,
    }
}
