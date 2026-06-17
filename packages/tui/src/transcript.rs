//! Transcript item projection for the TUI.

use std::collections::{BTreeMap, BTreeSet};

use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionTokenUsage, ShellRunResult, ToolInvocationPresentation,
    ToolInvocationResult, ToolInvocationStreamEvent, ToolOutputStream,
};

use super::diff_extract::{FileEditTranscript, file_edit_from_tool_request};
use super::tool_invocation_view::{
    TerminalInvocationItemContext, ToolInvocationPresentationInput, ToolInvocationRequestContext,
    apply_tool_invocation_presentation,
};

/// Lifecycle phase for a file edit/write preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEditPhase {
    /// Tool request has been observed but not started.
    Pending,
    /// Tool is waiting for user permission.
    WaitingPermission,
    /// Tool is actively applying the change.
    Applying,
    /// Tool finished successfully.
    Applied,
    /// Tool failed or permission was denied.
    Failed,
}

impl FileEditPhase {
    /// Return user-facing phase text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "Ready to apply",
            Self::WaitingPermission => "Waiting for permission",
            Self::Applying => "Applying",
            Self::Applied => "Applied",
            Self::Failed => "Failed",
        }
    }
}

/// Semantic transcript item type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItemKind {
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
        /// Raw tool arguments JSON.
        arguments_json: String,
        /// Optional semantic file edit extracted from filesystem tools.
        file_edit: Option<FileEditTranscript>,
        /// Lifecycle phase for file edit/write previews.
        file_edit_phase: Option<FileEditPhase>,
        /// Whether this item was derived from live-only partial tool arguments.
        live_preview: bool,
    },
    /// Live-only tool preview anchor resolved from ephemeral app state.
    LiveToolPreviewAnchor {
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
    },
    /// Tool-call result with structured metadata.
    ToolResult {
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Tool name, when the matching request is known.
        tool_name: Option<String>,
        /// Raw tool arguments JSON, when the matching request is known.
        arguments_json: Option<String>,
        /// Raw tool result.
        result: String,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// Durable filesystem write/edit presentation.
    FileChangePresentation {
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Tool name that produced the change.
        tool_name: String,
        /// Human-readable result summary.
        summary: String,
        /// Best-effort target path.
        path: Option<String>,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// Live or replayed terminal output from a tool.
    TerminalOutput {
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Tool name, when the matching request is known.
        tool_name: Option<String>,
        /// Raw terminal stream decoded as UTF-8.
        output: String,
        /// Terminal columns used by the producer.
        columns: u16,
        /// Terminal rows used by the producer.
        rows: u16,
        /// Unix timestamp in milliseconds when execution started.
        started_at_ms: Option<u64>,
        /// Unix timestamp in milliseconds when execution finished.
        finished_at_ms: Option<u64>,
        /// Process exit code once known.
        exit_code: Option<i32>,
        /// Whether execution timed out once known.
        timed_out: Option<bool>,
        /// Whether the terminal command failed once finished.
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ToolCallContext {
    tool_name: String,
    arguments_json: String,
}

/// Stable identity for a rendered transcript item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TranscriptItemId(u64);

impl TranscriptItemId {
    /// Return the raw item id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Renderable transcript item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptItem {
    id: TranscriptItemId,
    revision: u64,
    pub role: &'static str,
    pub text: String,
    pub streaming: bool,
    event_sequence: Option<u64>,
    timestamp_ms: Option<u64>,
    kind: TranscriptItemKind,
}

impl TranscriptItem {
    pub fn new(role: &'static str, text: String) -> Self {
        Self::with_identity(role, text, false, kind_for_role(role))
    }

    pub fn new_streaming(role: &'static str, text: String) -> Self {
        Self::with_identity(role, text, true, kind_for_role(role))
    }

    fn with_kind(
        role: &'static str,
        text: String,
        streaming: bool,
        kind: TranscriptItemKind,
    ) -> Self {
        Self::with_identity(role, text, streaming, kind)
    }

    fn with_identity(
        role: &'static str,
        text: String,
        streaming: bool,
        kind: TranscriptItemKind,
    ) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: TranscriptItemId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
            revision: 0,
            role,
            text,
            streaming,
            event_sequence: None,
            timestamp_ms: None,
            kind,
        }
    }

    /// Return a copy annotated with event metadata.
    #[must_use]
    pub const fn with_event_metadata(mut self, sequence: u64, timestamp_ms: u64) -> Self {
        self.event_sequence = Some(sequence);
        self.timestamp_ms = Some(timestamp_ms);
        self
    }

    /// Return the source event sequence associated with this item, when known.
    #[must_use]
    pub const fn event_sequence(&self) -> Option<u64> {
        self.event_sequence
    }

    const fn with_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self.bump_revision();
        self
    }

    /// Return stable item identity.
    #[must_use]
    pub const fn id(&self) -> TranscriptItemId {
        self.id
    }

    /// Return revision incremented whenever rendered state mutates.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    const fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    /// Return display role.
    #[must_use]
    pub const fn role(&self) -> &'static str {
        self.role
    }

    /// Return display text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Append text to this transcript item.
    pub fn append_text(&mut self, text: &str) {
        self.text.push_str(text);
        match &mut self.kind {
            TranscriptItemKind::ToolResult { result, .. }
            | TranscriptItemKind::TerminalOutput { output: result, .. } => {
                result.push_str(text);
            }
            _ => {}
        }
        self.bump_revision();
    }

    /// Mark a terminal output item as finished with final process metadata.
    pub const fn finish_terminal(
        &mut self,
        exit_code: Option<i32>,
        timed_out: bool,
        is_error: bool,
        finished_at_ms: Option<u64>,
    ) {
        if let TranscriptItemKind::TerminalOutput {
            finished_at_ms: terminal_finished_at_ms,
            exit_code: terminal_exit_code,
            timed_out: terminal_timed_out,
            is_error: terminal_error,
            ..
        } = &mut self.kind
        {
            if finished_at_ms.is_some() {
                *terminal_finished_at_ms = finished_at_ms;
            }
            *terminal_exit_code = exit_code;
            *terminal_timed_out = Some(timed_out);
            *terminal_error = is_error;
        }
        self.streaming = false;
        self.bump_revision();
    }

    /// Mark a terminal output item as failed or successful.
    pub const fn set_terminal_error(&mut self, is_error: bool) {
        if let TranscriptItemKind::TerminalOutput {
            is_error: terminal_error,
            ..
        } = &mut self.kind
        {
            *terminal_error = is_error;
            self.bump_revision();
        }
    }

    /// Set the terminal output finish timestamp.
    pub const fn set_terminal_finished_at(&mut self, finished_at_ms: Option<u64>) {
        if let TranscriptItemKind::TerminalOutput {
            finished_at_ms: terminal_finished_at_ms,
            ..
        } = &mut self.kind
        {
            *terminal_finished_at_ms = finished_at_ms;
            self.bump_revision();
        }
    }

    /// Replace terminal output with bounded final presentation text and metadata.
    pub fn apply_terminal_presentation(
        &mut self,
        output: String,
        exit_code: Option<i32>,
        timed_out: bool,
        is_error: bool,
        finished_at_ms: Option<u64>,
    ) {
        if let TranscriptItemKind::TerminalOutput {
            output: terminal_output,
            finished_at_ms: terminal_finished_at_ms,
            exit_code: terminal_exit_code,
            timed_out: terminal_timed_out,
            is_error: terminal_error,
            ..
        } = &mut self.kind
        {
            self.text.clone_from(&output);
            *terminal_output = output;
            if finished_at_ms.is_some() {
                *terminal_finished_at_ms = finished_at_ms;
            }
            *terminal_exit_code = exit_code;
            *terminal_timed_out = Some(timed_out);
            *terminal_error = is_error;
        }
        self.streaming = false;
        self.bump_revision();
    }

    /// Mark this transcript item as no longer streaming.
    pub const fn finish_streaming(&mut self) {
        self.streaming = false;
        self.bump_revision();
    }

    /// Set file-edit phase for a tool request item.
    pub fn set_file_edit_phase(&mut self, tool_call_id: &str, phase: FileEditPhase) -> bool {
        let TranscriptItemKind::ToolRequest {
            tool_call_id: item_tool_call_id,
            file_edit_phase,
            ..
        } = &mut self.kind
        else {
            return false;
        };
        if item_tool_call_id != tool_call_id {
            return false;
        }
        if file_edit_phase.is_some() {
            *file_edit_phase = Some(phase);
            self.bump_revision();
        }
        true
    }

    /// Return whether this item is a live preview anchor for `tool_call_id`.
    #[must_use]
    pub fn is_live_preview_anchor_for(&self, tool_call_id: &str) -> bool {
        matches!(
            &self.kind,
            TranscriptItemKind::LiveToolPreviewAnchor {
                tool_call_id: item_tool_call_id,
                ..
            } if item_tool_call_id == tool_call_id
        )
    }

    /// Return semantic item kind.
    #[must_use]
    pub const fn kind(&self) -> &TranscriptItemKind {
        &self.kind
    }

    /// Return whether this item is currently streaming.
    #[must_use]
    pub const fn streaming(&self) -> bool {
        self.streaming
    }
}

/// Project session events into transcript items, optionally hiding reasoning items.
#[must_use]
pub fn transcript_items_from_events_with_reasoning(
    events: &[SessionEvent],
    include_reasoning: bool,
) -> Vec<TranscriptItem> {
    let mut projector = TranscriptProjector::new(include_reasoning);
    for event in events {
        projector.push_event(event);
    }
    projector.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamedToolReplayContext {
    index: Option<usize>,
    columns: u16,
    rows: u16,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    saw_output: bool,
}

struct TranscriptProjector {
    items: Vec<TranscriptItem>,
    tool_calls: BTreeMap<String, ToolCallContext>,
    streamed_tool_results: BTreeMap<String, StreamedToolReplayContext>,
    presented_tool_results: BTreeSet<String>,
    include_reasoning: bool,
}

impl TranscriptProjector {
    const fn new(include_reasoning: bool) -> Self {
        Self {
            items: Vec::new(),
            tool_calls: BTreeMap::new(),
            streamed_tool_results: BTreeMap::new(),
            presented_tool_results: BTreeSet::new(),
            include_reasoning,
        }
    }

    fn push_event(&mut self, event: &SessionEvent) {
        push_transcript_item_from_event(
            &mut self.items,
            &mut self.tool_calls,
            &mut self.streamed_tool_results,
            &mut self.presented_tool_results,
            self.include_reasoning,
            event,
        );
    }

    fn finish(self) -> Vec<TranscriptItem> {
        self.items
    }
}

/// Merge streaming transcript items across a prepended history boundary.
pub fn merge_transcript_boundary(
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
pub fn tool_request_item(
    tool_call_id: &str,
    tool_name: &str,
    arguments_json: &str,
) -> TranscriptItem {
    let file_edit = file_edit_from_tool_request(tool_name, arguments_json);
    let streaming = file_edit.is_some();
    TranscriptItem::with_kind(
        "Tool",
        pretty_jsonish(arguments_json),
        false,
        TranscriptItemKind::ToolRequest {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            arguments_json: arguments_json.to_owned(),
            file_edit,
            file_edit_phase: streaming.then_some(FileEditPhase::Pending),
            live_preview: false,
        },
    )
    .with_streaming(streaming)
}

/// Build a transcript item anchoring a live-only partial tool argument preview.
#[must_use]
pub fn live_tool_preview_anchor_item(tool_call_id: &str, tool_name: &str) -> TranscriptItem {
    TranscriptItem::with_kind(
        "Tool",
        String::new(),
        true,
        TranscriptItemKind::LiveToolPreviewAnchor {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
        },
    )
}

/// Build a durable file-change presentation item.
#[must_use]
pub fn file_change_presentation_item(
    tool_call_id: &str,
    tool_name: &str,
    summary: &str,
    path: Option<&str>,
    is_error: bool,
) -> TranscriptItem {
    let text = path.map_or_else(|| summary.to_owned(), |path| format!("{summary}: {path}"));
    TranscriptItem::with_kind(
        if is_error { "Tool error" } else { "Tool" },
        text,
        false,
        TranscriptItemKind::FileChangePresentation {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            summary: summary.to_owned(),
            path: path.map(ToOwned::to_owned),
            is_error,
        },
    )
}

/// Build a streaming transcript item for live terminal output.
#[must_use]
pub fn streaming_terminal_output_item(
    tool_call_id: &str,
    tool_name: Option<&str>,
    text: &str,
    columns: u16,
    rows: u16,
    started_at_ms: Option<u64>,
) -> TranscriptItem {
    TranscriptItem::with_kind(
        "Terminal",
        text.to_owned(),
        true,
        TranscriptItemKind::TerminalOutput {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.map(ToOwned::to_owned),
            output: text.to_owned(),
            columns,
            rows,
            started_at_ms,
            finished_at_ms: None,
            exit_code: None,
            timed_out: None,
            is_error: false,
        },
    )
}

/// Build a streaming transcript item for live tool output.
#[must_use]
pub fn streaming_tool_output_item(
    tool_call_id: &str,
    tool_name: Option<&str>,
    arguments_json: Option<&str>,
    text: &str,
) -> TranscriptItem {
    TranscriptItem::with_kind(
        "Tool",
        text.to_owned(),
        true,
        TranscriptItemKind::ToolResult {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.map(ToOwned::to_owned),
            arguments_json: arguments_json.map(ToOwned::to_owned),
            result: text.to_owned(),
            is_error: false,
        },
    )
}

/// Build a transcript item for a tool result.
#[must_use]
pub fn tool_result_item(
    tool_call_id: &str,
    tool_name: Option<&str>,
    arguments_json: Option<&str>,
    result: &str,
    is_error: bool,
) -> TranscriptItem {
    TranscriptItem::with_kind(
        if is_error { "Tool error" } else { "Tool" },
        result.to_owned(),
        false,
        TranscriptItemKind::ToolResult {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.map(ToOwned::to_owned),
            arguments_json: arguments_json.map(ToOwned::to_owned),
            result: result.to_owned(),
            is_error,
        },
    )
}

/// Build a transcript item for a permission request.
#[must_use]
pub fn permission_request_item(
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
pub fn permission_result_item(permission_id: &str, approved: bool) -> TranscriptItem {
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
pub fn model_usage_item(turn_id: &str, usage: &SessionTokenUsage) -> TranscriptItem {
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
pub fn optional_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

/// Format JSON-like values for transcript display.
#[must_use]
pub fn pretty_jsonish(value: &str) -> String {
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
pub fn truncate_block(value: &str, max_chars: usize) -> String {
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

#[allow(clippy::too_many_lines)]
fn push_transcript_item_from_event(
    items: &mut Vec<TranscriptItem>,
    tool_calls: &mut BTreeMap<String, ToolCallContext>,
    streamed_tool_results: &mut BTreeMap<String, StreamedToolReplayContext>,
    presented_tool_results: &mut BTreeSet<String>,
    include_reasoning: bool,
    event: &SessionEvent,
) {
    match &event.kind {
        SessionEventKind::AssistantDelta { text } => {
            push_streaming_transcript_item(items, "Assistant", text);
        }
        SessionEventKind::AssistantMessage { text } => {
            finish_streaming_transcript_item(items, "Assistant", text);
        }
        SessionEventKind::AssistantReasoningDelta { text } if include_reasoning => {
            push_streaming_transcript_item(items, "Reasoning", text);
        }
        SessionEventKind::AssistantReasoningMessage { text } if include_reasoning => {
            finish_streaming_transcript_item(items, "Reasoning", text);
        }
        SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. } => {}
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
            semantic_result,
            ..
        } => {
            if let Some(semantic_result) = semantic_result {
                apply_semantic_tool_result(
                    items,
                    tool_call_id,
                    tool_calls.get(tool_call_id),
                    &mut streamed_tool_results.get_mut(tool_call_id),
                    semantic_result,
                    *is_error,
                );
                presented_tool_results.insert(tool_call_id.clone());
                return;
            }
            if let Some(legacy_result) = legacy_semantic_tool_result(
                tool_calls
                    .get(tool_call_id)
                    .map(|context| context.tool_name.as_str()),
                result,
            ) {
                apply_semantic_tool_result(
                    items,
                    tool_call_id,
                    tool_calls.get(tool_call_id),
                    &mut streamed_tool_results.get_mut(tool_call_id),
                    &legacy_result,
                    *is_error,
                );
                presented_tool_results.insert(tool_call_id.clone());
                return;
            }
            let should_render_final = if let Some(replay) =
                streamed_tool_results.get_mut(tool_call_id)
            {
                if let Some(index) = replay.index
                    && let Some(item) = items.get_mut(index)
                {
                    if let Some((exit_code, timed_out)) = terminal_shell_result_metadata(result) {
                        item.finish_terminal(
                            exit_code,
                            timed_out,
                            *is_error,
                            replay.finished_at_ms,
                        );
                    } else {
                        item.set_terminal_error(*is_error);
                        item.finish_streaming();
                    }
                }
                !replay.saw_output
            } else {
                true
            };
            if should_render_final
                && !presented_tool_results.contains(tool_call_id)
                && let Some(item) = non_streaming_transcript_item_from_event(
                    event,
                    tool_calls,
                    streamed_tool_results,
                )
            {
                items.push(item);
            }
        }
        SessionEventKind::ToolInvocationPresentation {
            tool_call_id,
            started_at_ms,
            finished_at_ms,
            is_error,
            presentation,
        } => {
            if presented_tool_results.contains(tool_call_id) {
                return;
            }
            apply_tool_invocation_presentation_event(
                items,
                tool_calls,
                streamed_tool_results,
                presented_tool_results,
                ToolPresentationEventRef {
                    tool_call_id,
                    started_at_ms: *started_at_ms,
                    finished_at_ms: *finished_at_ms,
                    is_error: *is_error,
                    presentation,
                },
            );
        }
        SessionEventKind::ToolInvocationStream { event } => {
            apply_tool_invocation_stream_event(items, tool_calls, streamed_tool_results, event);
        }
        _ => {
            if let Some(item) =
                non_streaming_transcript_item_from_event(event, tool_calls, streamed_tool_results)
            {
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
pub fn push_streaming_transcript_item(
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
pub fn finish_streaming_transcript_item(
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

#[allow(clippy::too_many_lines)]
fn non_streaming_transcript_item_from_event(
    event: &SessionEvent,
    tool_calls: &mut BTreeMap<String, ToolCallContext>,
    streamed_tool_results: &BTreeMap<String, StreamedToolReplayContext>,
) -> Option<TranscriptItem> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => Some(
            TranscriptItem::new("You", text.clone())
                .with_event_metadata(event.sequence, event.timestamp_ms),
        ),
        SessionEventKind::SystemMessage { text } => {
            Some(TranscriptItem::new("System", text.clone()))
        }
        SessionEventKind::WorkingDirectoryChanged {
            old_working_directory,
            new_working_directory,
        } => Some(TranscriptItem::new(
            "System",
            working_directory_changed_message(old_working_directory, new_working_directory),
        )),
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
        } => {
            tool_calls.insert(
                tool_call_id.clone(),
                ToolCallContext {
                    tool_name: tool_name.clone(),
                    arguments_json: arguments_json.clone(),
                },
            );
            Some(tool_request_item(tool_call_id, tool_name, arguments_json))
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
            semantic_result,
            ..
        } => {
            if let Some(semantic_result) = semantic_result {
                let mut replay = None;
                return semantic_tool_result_item(
                    tool_call_id,
                    tool_calls.get(tool_call_id),
                    &mut replay,
                    semantic_result,
                    *is_error,
                );
            }
            if let Some(legacy_result) = legacy_semantic_tool_result(
                tool_calls
                    .get(tool_call_id)
                    .map(|context| context.tool_name.as_str()),
                result,
            ) {
                let mut replay = None;
                return semantic_tool_result_item(
                    tool_call_id,
                    tool_calls.get(tool_call_id),
                    &mut replay,
                    &legacy_result,
                    *is_error,
                );
            }
            if streamed_tool_results
                .get(tool_call_id)
                .is_some_and(|replay| replay.saw_output)
            {
                return None;
            }
            let context = tool_calls.get(tool_call_id);
            Some(tool_result_item(
                tool_call_id,
                context.map(|context| context.tool_name.as_str()),
                context.map(|context| context.arguments_json.as_str()),
                result,
                *is_error,
            ))
        }
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
        _ => None,
    }
}

fn working_directory_changed_message(
    old_working_directory: &std::path::Path,
    new_working_directory: &std::path::Path,
) -> String {
    format!(
        "Working directory changed from `{}` to `{}`. Treat prior file/path assumptions as possibly stale unless reconfirmed.",
        old_working_directory.display(),
        new_working_directory.display()
    )
}

fn legacy_semantic_tool_result(
    tool_name: Option<&str>,
    result: &str,
) -> Option<ToolInvocationResult> {
    let value = serde_json::from_str::<serde_json::Value>(result).ok()?;
    if value.get("mode")?.as_str()? != "terminal" {
        return None;
    }
    let output = value
        .get("output")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Some(ToolInvocationResult::ShellRun {
        result: ShellRunResult::Terminal {
            exit_code: value
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .and_then(|code| i32::try_from(code).ok()),
            timed_out: value
                .get("timed_out")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or_default(),
            cancelled: value
                .get("cancelled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or_default(),
            output_tail: legacy_terminal_output_text(tool_name, &value, &output),
            output_truncated: value
                .get("output_truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or_default(),
            output_bytes: value
                .get("output_bytes")
                .and_then(serde_json::Value::as_u64),
            retained_output_bytes: value
                .get("retained_output_bytes")
                .and_then(serde_json::Value::as_u64),
            columns: value
                .get("columns")
                .and_then(serde_json::Value::as_u64)
                .and_then(|columns| u16::try_from(columns).ok())
                .unwrap_or(120),
            rows: value
                .get("rows")
                .and_then(serde_json::Value::as_u64)
                .and_then(|rows| u16::try_from(rows).ok())
                .unwrap_or(24),
        },
    })
}

fn legacy_terminal_output_text(
    tool_name: Option<&str>,
    value: &serde_json::Value,
    output: &str,
) -> String {
    if !value
        .get("output_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_default()
    {
        return output.to_owned();
    }
    let Some(output_bytes) = value
        .get("output_bytes")
        .and_then(serde_json::Value::as_u64)
    else {
        return output.to_owned();
    };
    let Some(retained_output_bytes) = value
        .get("retained_output_bytes")
        .and_then(serde_json::Value::as_u64)
    else {
        return output.to_owned();
    };
    format!(
        "{} output truncated; showing {retained_output_bytes} of {output_bytes} bytes\n{output}",
        tool_name.unwrap_or("terminal")
    )
}

fn terminal_shell_result_metadata(result: &str) -> Option<(Option<i32>, bool)> {
    let value = serde_json::from_str::<serde_json::Value>(result).ok()?;
    if value.get("mode")?.as_str()? != "terminal" {
        return None;
    }
    let exit_code = value
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    let timed_out = value.get("timed_out")?.as_bool()?;
    Some((exit_code, timed_out))
}

#[derive(Clone, Copy)]
struct ToolPresentationEventRef<'a> {
    tool_call_id: &'a str,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    is_error: bool,
    presentation: &'a ToolInvocationPresentation,
}

fn apply_tool_invocation_presentation_event(
    items: &mut Vec<TranscriptItem>,
    tool_calls: &BTreeMap<String, ToolCallContext>,
    streamed_tool_results: &mut BTreeMap<String, StreamedToolReplayContext>,
    presented_tool_results: &mut BTreeSet<String>,
    event: ToolPresentationEventRef<'_>,
) {
    let request_context =
        tool_calls
            .get(event.tool_call_id)
            .map(|context| ToolInvocationRequestContext {
                tool_name: context.tool_name.as_str(),
            });
    let terminal_context = streamed_tool_results
        .get(event.tool_call_id)
        .map(|context| TerminalInvocationItemContext {
            index: context.index,
        });
    if let ToolInvocationPresentation::Terminal {
        exit_code,
        timed_out,
        ..
    } = event.presentation
        && let Some(streamed) = streamed_tool_results.get(event.tool_call_id)
        && streamed.saw_output
    {
        if let Some(index) = streamed.index
            && let Some(item) = items.get_mut(index)
        {
            item.finish_terminal(*exit_code, *timed_out, event.is_error, event.finished_at_ms);
        }
        presented_tool_results.insert(event.tool_call_id.to_owned());
        return;
    }
    let effects = apply_tool_invocation_presentation(
        items,
        ToolInvocationPresentationInput {
            tool_call_id: event.tool_call_id,
            started_at_ms: event.started_at_ms,
            finished_at_ms: event.finished_at_ms,
            is_error: event.is_error,
            presentation: event.presentation,
            request_context,
            terminal_context,
        },
    );
    if effects.suppress_final_result {
        presented_tool_results.insert(event.tool_call_id.to_owned());
    }
    if effects.terminal_saw_output {
        streamed_tool_results.insert(
            event.tool_call_id.to_owned(),
            StreamedToolReplayContext {
                index: effects.terminal_index,
                columns: terminal_dimensions(event.presentation)
                    .map_or(120, |(columns, _)| columns),
                rows: terminal_dimensions(event.presentation).map_or(24, |(_, rows)| rows),
                started_at_ms: event.started_at_ms,
                finished_at_ms: event.finished_at_ms,
                saw_output: true,
            },
        );
    }
}

#[allow(clippy::single_match_else, clippy::needless_pass_by_ref_mut)]
fn apply_semantic_tool_result(
    items: &mut Vec<TranscriptItem>,
    tool_call_id: &str,
    context: Option<&ToolCallContext>,
    replay: &mut Option<&mut StreamedToolReplayContext>,
    result: &ToolInvocationResult,
    is_error: bool,
) {
    if let ToolInvocationResult::ShellRun {
        result:
            ShellRunResult::Terminal {
                exit_code,
                timed_out,
                ..
            },
    } = result
        && let Some(replay) = replay.as_deref_mut()
        && replay.saw_output
    {
        if let Some(index) = replay.index
            && let Some(item) = items.get_mut(index)
        {
            item.finish_terminal(*exit_code, *timed_out, is_error, replay.finished_at_ms);
        }
        return;
    }

    match result {
        ToolInvocationResult::ShellRun {
            result:
                ShellRunResult::Terminal {
                    exit_code,
                    timed_out,
                    output_tail,
                    columns,
                    rows,
                    ..
                },
        } => {
            let started_at_ms = replay.as_ref().and_then(|replay| replay.started_at_ms);
            let finished_at_ms = replay.as_ref().and_then(|replay| replay.finished_at_ms);
            if let Some(index) = replay.as_ref().and_then(|replay| replay.index)
                && let Some(item) = items.get_mut(index)
            {
                item.apply_terminal_presentation(
                    output_tail.clone(),
                    *exit_code,
                    *timed_out,
                    is_error,
                    finished_at_ms,
                );
                return;
            }
            let mut item = streaming_terminal_output_item(
                tool_call_id,
                context.map(|context| context.tool_name.as_str()),
                output_tail,
                (*columns).max(1),
                (*rows).max(1),
                started_at_ms,
            );
            item.apply_terminal_presentation(
                output_tail.clone(),
                *exit_code,
                *timed_out,
                is_error,
                finished_at_ms,
            );
            items.push(item);
        }
        _ => {
            let mut replay = None;
            if let Some(item) =
                semantic_tool_result_item(tool_call_id, context, &mut replay, result, is_error)
            {
                items.push(item);
            }
        }
    }
}

#[allow(clippy::needless_pass_by_ref_mut)]
fn semantic_tool_result_item(
    tool_call_id: &str,
    context: Option<&ToolCallContext>,
    replay: &mut Option<&mut StreamedToolReplayContext>,
    result: &ToolInvocationResult,
    is_error: bool,
) -> Option<TranscriptItem> {
    match result {
        ToolInvocationResult::ShellRun { result } => {
            semantic_shell_result_item(tool_call_id, context, replay, result, is_error)
        }
        ToolInvocationResult::FileChange { result } => context.is_none().then(|| {
            file_change_presentation_item(
                tool_call_id,
                &result.tool_name,
                &result.summary,
                result.path.as_deref(),
                is_error,
            )
        }),
        ToolInvocationResult::Text { text } => Some(tool_result_item(
            tool_call_id,
            context.map(|context| context.tool_name.as_str()),
            context.map(|context| context.arguments_json.as_str()),
            text,
            is_error,
        )),
        ToolInvocationResult::Json { value } => Some(tool_result_item(
            tool_call_id,
            context.map(|context| context.tool_name.as_str()),
            context.map(|context| context.arguments_json.as_str()),
            value,
            is_error,
        )),
    }
}

#[allow(clippy::needless_pass_by_ref_mut)]
fn semantic_shell_result_item(
    tool_call_id: &str,
    context: Option<&ToolCallContext>,
    replay: &mut Option<&mut StreamedToolReplayContext>,
    result: &ShellRunResult,
    is_error: bool,
) -> Option<TranscriptItem> {
    match result {
        ShellRunResult::Terminal {
            exit_code,
            timed_out,
            output_tail,
            columns,
            rows,
            ..
        } => {
            if replay.as_ref().and_then(|replay| replay.index).is_some() {
                return None;
            }
            let mut item = streaming_terminal_output_item(
                tool_call_id,
                context.map(|context| context.tool_name.as_str()),
                output_tail,
                (*columns).max(1),
                (*rows).max(1),
                replay.as_ref().and_then(|replay| replay.started_at_ms),
            );
            item.apply_terminal_presentation(
                output_tail.clone(),
                *exit_code,
                *timed_out,
                is_error,
                replay.as_ref().and_then(|replay| replay.finished_at_ms),
            );
            Some(item)
        }
        ShellRunResult::Captured { .. } => Some(tool_result_item(
            tool_call_id,
            context.map(|context| context.tool_name.as_str()),
            context.map(|context| context.arguments_json.as_str()),
            &captured_shell_result_text(result),
            is_error,
        )),
    }
}

fn captured_shell_result_text(result: &ShellRunResult) -> String {
    let ShellRunResult::Captured {
        exit_code,
        timed_out,
        cancelled,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        ..
    } = result
    else {
        return String::new();
    };
    let mut text = format!(
        "exit_code: {}\ntimed_out: {timed_out}\ncancelled: {cancelled}",
        exit_code.map_or_else(|| "null".to_owned(), |code| code.to_string())
    );
    text.push_str("\nstdout:\n");
    text.push_str(stdout);
    if *stdout_truncated {
        text.push_str("\n[stdout truncated]");
    }
    text.push_str("\nstderr:\n");
    text.push_str(stderr);
    if *stderr_truncated {
        text.push_str("\n[stderr truncated]");
    }
    text
}

fn terminal_dimensions(presentation: &ToolInvocationPresentation) -> Option<(u16, u16)> {
    match presentation {
        ToolInvocationPresentation::Terminal { columns, rows, .. } => {
            Some(((*columns).max(1), (*rows).max(1)))
        }
        ToolInvocationPresentation::FileChange { .. } => None,
    }
}

fn apply_tool_invocation_stream_event(
    items: &mut Vec<TranscriptItem>,
    tool_calls: &BTreeMap<String, ToolCallContext>,
    streamed_tool_results: &mut BTreeMap<String, StreamedToolReplayContext>,
    event: &ToolInvocationStreamEvent,
) {
    match event {
        ToolInvocationStreamEvent::Started {
            tool_call_id,
            terminal,
            columns,
            rows,
            started_at_ms,
            ..
        } => {
            if *terminal {
                streamed_tool_results.insert(
                    tool_call_id.clone(),
                    StreamedToolReplayContext {
                        index: None,
                        columns: columns.unwrap_or(120).max(1),
                        rows: rows.unwrap_or(24).max(1),
                        started_at_ms: *started_at_ms,
                        finished_at_ms: None,
                        saw_output: false,
                    },
                );
            }
        }
        ToolInvocationStreamEvent::OutputDelta {
            tool_call_id,
            stream,
            text,
            ..
        } if *stream == ToolOutputStream::Pty => {
            let replay = streamed_tool_results.get(tool_call_id).cloned();
            if let Some(replay) = replay.as_ref()
                && let Some(index) = replay.index
            {
                if let Some(item) = items.get_mut(index) {
                    item.append_text(text);
                }
                return;
            }
            let context = tool_calls.get(tool_call_id);
            let (columns, rows) = replay
                .as_ref()
                .map_or((120, 24), |replay| (replay.columns, replay.rows));
            items.push(streaming_terminal_output_item(
                tool_call_id,
                context.map(|context| context.tool_name.as_str()),
                text,
                columns,
                rows,
                replay.as_ref().and_then(|replay| replay.started_at_ms),
            ));
            streamed_tool_results.insert(
                tool_call_id.clone(),
                StreamedToolReplayContext {
                    index: Some(items.len().saturating_sub(1)),
                    columns,
                    rows,
                    started_at_ms: replay.as_ref().and_then(|replay| replay.started_at_ms),
                    finished_at_ms: replay.as_ref().and_then(|replay| replay.finished_at_ms),
                    saw_output: true,
                },
            );
        }
        ToolInvocationStreamEvent::Finished {
            tool_call_id,
            is_error,
            finished_at_ms,
            ..
        } => {
            if let Some(replay) = streamed_tool_results.get_mut(tool_call_id)
                && let Some(index) = replay.index
                && let Some(item) = items.get_mut(index)
            {
                item.set_terminal_error(*is_error);
                item.set_terminal_finished_at(*finished_at_ms);
                replay.finished_at_ms = *finished_at_ms;
            }
        }
        _ => {}
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
