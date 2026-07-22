//! Transcript item projection for the TUI.

use std::collections::BTreeMap;

use bcode_plugin_sdk::path::display;
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionTokenUsage, ToolArtifact, ToolInvocationProjection,
    ToolInvocationResult, ToolInvocationStreamEvent,
};

/// Generic timing metadata for a tool invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolTiming {
    /// Tool start time as UNIX epoch milliseconds.
    pub started_at_ms: Option<u64>,
    /// Tool finish time as UNIX epoch milliseconds.
    pub finished_at_ms: Option<u64>,
    /// Tool timeout duration in milliseconds, when known.
    pub timeout_ms: Option<u64>,
    /// Whether the tool timed out, when known.
    pub timed_out: Option<bool>,
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
        /// Producer plugin id, when known.
        producer_plugin_id: Option<String>,
        /// Tool name.
        tool_name: String,
        /// Working directory captured for this invocation.
        working_directory: Option<std::path::PathBuf>,
        /// Plugin-owned request visual.
        request_visual: Option<bcode_session_models::PluginVisualDescriptor>,
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
        /// Working directory captured for this invocation.
        working_directory: Option<std::path::PathBuf>,
        /// Raw tool result.
        result: String,
        /// Raw artifact result, when the result is artifact-backed.
        artifact: Option<Box<ToolArtifact>>,
        /// Whether the tool failed.
        is_error: bool,
        /// Generic timing metadata for the tool invocation.
        timing: ToolTiming,
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
        /// Raw tool arguments JSON.
        arguments_json: String,
        /// Policy source that requested approval.
        policy_source: Option<String>,
        /// Human-readable policy reason.
        policy_reason: Option<String>,
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
    /// Generic schema-versioned tool contribution.
    ToolContribution {
        /// Opaque contribution envelope.
        contribution: Box<bcode_session_models::ToolContributionEvent>,
    },
    /// Generic fallback item.
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ToolCallContext {
    tool_name: String,
    arguments_json: String,
    working_directory: Option<std::path::PathBuf>,
    request_visual: Option<bcode_session_models::PluginVisualDescriptor>,
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
    display_label: Option<String>,
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

    pub(crate) fn with_kind(
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
            display_label: None,
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

    /// Return a copy annotated with a generic display label.
    #[must_use]
    pub fn with_display_label(mut self, display_label: String) -> Self {
        self.display_label = Some(display_label);
        self
    }

    /// Return the rendered role, including a generic origin label when present.
    #[must_use]
    pub fn display_role(&self) -> String {
        self.display_label.as_ref().map_or_else(
            || self.role.to_owned(),
            |label| format!("{} · {label}", self.role),
        )
    }

    /// Return the source event sequence associated with this item, when known.
    #[must_use]
    pub const fn event_sequence(&self) -> Option<u64> {
        self.event_sequence
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

    /// Replace an opaque contribution envelope and its generic fallback text.
    pub fn replace_tool_contribution(
        &mut self,
        contribution: bcode_session_models::ToolContributionEvent,
    ) {
        self.text = serde_json::to_string_pretty(&contribution)
            .unwrap_or_else(|_| contribution.payload.to_string());
        self.kind = TranscriptItemKind::ToolContribution {
            contribution: Box::new(contribution),
        };
        self.bump_revision();
    }

    /// Append text to this transcript item.
    pub fn append_text(&mut self, text: &str) {
        self.text.push_str(text);
        if let TranscriptItemKind::ToolResult { result, .. } = &mut self.kind {
            result.push_str(text);
        }
        self.bump_revision();
    }

    /// Replace the plugin-owned visual on a tool request and set its live state.
    pub fn set_tool_request_visual(
        &mut self,
        visual: bcode_session_models::PluginVisualDescriptor,
        streaming: bool,
    ) {
        if let TranscriptItemKind::ToolRequest { request_visual, .. } = &mut self.kind {
            *request_visual = Some(visual);
            self.streaming = streaming;
            self.bump_revision();
        }
    }

    /// Mark this transcript item as no longer streaming.
    pub const fn finish_streaming(&mut self) {
        self.streaming = false;
        self.bump_revision();
    }

    /// Return generic tool timing metadata, when this item represents a tool result.
    #[must_use]
    pub const fn tool_timing(&self) -> Option<ToolTiming> {
        match &self.kind {
            TranscriptItemKind::ToolResult { timing, .. } => Some(*timing),
            _ => None,
        }
    }

    /// Set generic tool start time metadata on a tool result item.
    pub const fn set_tool_started_at_ms(&mut self, started_at_ms: Option<u64>) {
        if let TranscriptItemKind::ToolResult { timing, .. } = &mut self.kind {
            timing.started_at_ms = started_at_ms;
            self.bump_revision();
        }
    }

    /// Set generic tool finish time metadata on a tool result item.
    pub const fn set_tool_finished_at_ms(&mut self, finished_at_ms: Option<u64>) {
        if let TranscriptItemKind::ToolResult { timing, .. } = &mut self.kind {
            timing.finished_at_ms = finished_at_ms;
            self.bump_revision();
        }
    }

    /// Set generic tool timeout duration metadata on a tool result item.
    pub const fn set_tool_timeout_ms(&mut self, timeout_ms: Option<u64>) {
        if let TranscriptItemKind::ToolResult { timing, .. } = &mut self.kind {
            timing.timeout_ms = timeout_ms;
            self.bump_revision();
        }
    }

    /// Set generic tool timeout result metadata on a tool result item.
    pub const fn set_tool_timed_out(&mut self, timed_out: Option<bool>) {
        if let TranscriptItemKind::ToolResult { timing, .. } = &mut self.kind {
            timing.timed_out = timed_out;
            self.bump_revision();
        }
    }

    /// Copy generic tool timing from another tool item.
    pub const fn copy_tool_timing_from(&mut self, other: &Self) {
        if let Some(source_timing) = other.tool_timing()
            && let TranscriptItemKind::ToolResult { timing, .. } = &mut self.kind
        {
            *timing = source_timing;
            self.bump_revision();
        }
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    include_reasoning: bool,
}

impl TranscriptProjector {
    const fn new(include_reasoning: bool) -> Self {
        Self {
            items: Vec::new(),
            tool_calls: BTreeMap::new(),
            streamed_tool_results: BTreeMap::new(),
            include_reasoning,
        }
    }

    fn push_event(&mut self, event: &SessionEvent) {
        push_transcript_item_from_event(
            &mut self.items,
            &mut self.tool_calls,
            &mut self.streamed_tool_results,
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
pub fn tool_request_item_from_projection(projection: &ToolInvocationProjection) -> TranscriptItem {
    let tool_name = projection.tool_name.as_deref().unwrap_or("unknown tool");
    let arguments_json = projection.arguments_json.as_deref().unwrap_or("{}");
    tool_request_item(
        &projection.tool_call_id,
        projection.producer_plugin_id.as_deref(),
        tool_name,
        arguments_json,
        projection.working_directory.clone(),
        projection.request_visual.clone(),
    )
}

/// Build a transcript item for a generic tool result from renderer-neutral projection state.
#[must_use]
pub fn generic_tool_result_item_from_projection(
    projection: &ToolInvocationProjection,
) -> Option<TranscriptItem> {
    let mut item = tool_result_item(
        &projection.tool_call_id,
        projection.tool_name.as_deref(),
        projection.arguments_json.as_deref(),
        &display_tool_result_text(projection.result_text.as_deref()?),
        projection.is_error.unwrap_or(false),
    );
    item.set_tool_started_at_ms(projection.started_at_ms);
    item.set_tool_finished_at_ms(projection.finished_at_ms);
    Some(item)
}

/// Build a transcript item for a tool request.
#[must_use]
pub fn tool_request_item(
    tool_call_id: &str,
    producer_plugin_id: Option<&str>,
    tool_name: &str,
    arguments_json: &str,
    working_directory: Option<std::path::PathBuf>,
    request_visual: Option<bcode_session_models::PluginVisualDescriptor>,
) -> TranscriptItem {
    TranscriptItem::with_kind(
        "Tool",
        pretty_jsonish(arguments_json),
        false,
        TranscriptItemKind::ToolRequest {
            tool_call_id: tool_call_id.to_owned(),
            producer_plugin_id: producer_plugin_id.map(ToOwned::to_owned),
            tool_name: tool_name.to_owned(),
            working_directory,
            request_visual,
            live_preview: false,
        },
    )
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

/// Build a streaming transcript item for a plugin-owned visual update.
#[must_use]
pub fn streaming_tool_visual_item(
    tool_call_id: &str,
    tool_name: Option<&str>,
    working_directory: Option<&std::path::Path>,
    visual: &bcode_session_models::PluginVisualDescriptor,
    streaming: bool,
) -> TranscriptItem {
    let artifact = ToolArtifact {
        artifact_id: visual
            .visual_id
            .clone()
            .unwrap_or_else(|| format!("{tool_call_id}-stream-visual")),
        producer_plugin_id: visual
            .producer_plugin_id
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        schema: visual.schema.clone(),
        schema_version: visual.schema_version,
        tool_call_id: Some(tool_call_id.to_owned()),
        title: visual.title.clone(),
        metadata: visual.payload.clone(),
        refs: Vec::new(),
    };
    TranscriptItem::with_kind(
        "Tool",
        artifact_summary_text(&artifact),
        streaming,
        TranscriptItemKind::ToolResult {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.map(ToOwned::to_owned),
            arguments_json: None,
            working_directory: working_directory.map(std::path::Path::to_path_buf),
            result: artifact_summary_text(&artifact),
            artifact: Some(Box::new(artifact)),
            is_error: false,
            timing: ToolTiming::default(),
        },
    )
}

/// Upsert a plugin-owned visual update item for a tool call.
pub fn upsert_tool_visual_item(items: &mut Vec<TranscriptItem>, item: TranscriptItem) -> usize {
    let Some((tool_call_id, visual_key)) = tool_visual_identity(&item) else {
        items.push(item);
        return items.len().saturating_sub(1);
    };
    let tool_call_id = tool_call_id.to_owned();
    let visual_key = visual_key.to_owned();
    if let Some(index) = items.iter().position(|existing| {
        tool_visual_identity(existing) == Some((tool_call_id.as_str(), visual_key.as_str()))
    }) {
        let mut item = item;
        item.copy_tool_timing_from(&items[index]);
        items[index] = item;
        return index;
    }
    if let Some(index) = items.iter().position(|existing| {
        existing.is_live_preview_anchor_for(&tool_call_id)
            || matches!(
                existing.kind(),
                TranscriptItemKind::ToolRequest {
                    tool_call_id: item_tool_call_id,
                    ..
                } if item_tool_call_id == &tool_call_id
            )
    }) {
        let mut item = item;
        item.copy_tool_timing_from(&items[index]);
        items[index] = item;
        return index;
    }
    items.push(item);
    items.len().saturating_sub(1)
}

fn tool_visual_identity(item: &TranscriptItem) -> Option<(&str, &str)> {
    let TranscriptItemKind::ToolResult {
        tool_call_id,
        artifact: Some(artifact),
        ..
    } = item.kind()
    else {
        return None;
    };
    Some((tool_call_id.as_str(), artifact.artifact_id.as_str()))
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
            working_directory: None,
            result: text.to_owned(),
            artifact: None,
            is_error: false,
            timing: ToolTiming::default(),
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
            working_directory: None,
            result: result.to_owned(),
            artifact: None,
            is_error,
            timing: ToolTiming::default(),
        },
    )
}

/// Build a transcript item for an artifact-backed tool result.
#[must_use]
pub fn artifact_tool_result_item(
    tool_call_id: &str,
    tool_name: Option<&str>,
    arguments_json: Option<&str>,
    artifact: &ToolArtifact,
    is_error: bool,
) -> TranscriptItem {
    let result = artifact_summary_text(artifact);
    let mut artifact = artifact.clone();
    if let Some(arguments_json) = arguments_json
        && let Some(object) = artifact.metadata.as_object_mut()
        && !object.contains_key("arguments")
        && let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments_json)
    {
        object.insert("arguments".to_owned(), arguments);
    }
    TranscriptItem::with_kind(
        if is_error { "Tool error" } else { "Tool" },
        result.clone(),
        false,
        TranscriptItemKind::ToolResult {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.map(ToOwned::to_owned),
            arguments_json: arguments_json.map(ToOwned::to_owned),
            working_directory: None,
            result,
            artifact: Some(Box::new(artifact.clone())),
            is_error,
            timing: tool_timing_from_artifact(&artifact),
        },
    )
}

fn tool_timing_from_artifact(artifact: &ToolArtifact) -> ToolTiming {
    ToolTiming {
        timed_out: artifact
            .metadata
            .get("timed_out")
            .and_then(serde_json::Value::as_bool),
        ..ToolTiming::default()
    }
}

/// Build a renderer-neutral transcript item for an opaque tool contribution.
#[must_use]
pub fn tool_contribution_item(
    contribution: &bcode_session_models::ToolContributionEvent,
    streaming: bool,
) -> TranscriptItem {
    let text = serde_json::to_string_pretty(contribution)
        .unwrap_or_else(|_| contribution.payload.to_string());
    TranscriptItem::with_kind(
        "Tool contribution",
        text,
        streaming,
        TranscriptItemKind::ToolContribution {
            contribution: Box::new(contribution.clone()),
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
    policy_source: Option<&str>,
    policy_reason: Option<&str>,
) -> TranscriptItem {
    let mut body = pretty_jsonish(arguments_json);
    if let Some(reason) = policy_reason.filter(|reason| !reason.trim().is_empty()) {
        body = format!(
            "Policy: {}\nReason: {reason}\n\n{body}",
            policy_source.unwrap_or("policy")
        );
    }
    TranscriptItem::with_kind(
        "Permission",
        body,
        false,
        TranscriptItemKind::PermissionRequest {
            permission_id: permission_id.to_owned(),
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            arguments_json: arguments_json.to_owned(),
            policy_source: policy_source.map(str::to_owned),
            policy_reason: policy_reason.map(str::to_owned),
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
            push_streaming_transcript_item(items, "Reasoning summary", text);
        }
        SessionEventKind::AssistantReasoningMessage { text } if include_reasoning => {
            finish_streaming_transcript_item(items, "Reasoning summary", text);
        }
        SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. } => {}
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result: _,
            is_error,
            semantic_result,
            ..
        } => {
            if let Some(semantic_result) = semantic_result {
                if let Some(replay) = streamed_tool_results.get_mut(tool_call_id)
                    && let Some(index) = replay.index
                {
                    let mut item = semantic_tool_result_item(
                        tool_call_id,
                        tool_calls.get(tool_call_id),
                        semantic_result,
                        *is_error,
                    );
                    apply_replay_timing(&mut item, replay);
                    if replay.saw_output {
                        if let Some(existing) = items.get_mut(index) {
                            existing.set_tool_started_at_ms(replay.started_at_ms);
                            existing.set_tool_finished_at_ms(replay.finished_at_ms);
                            existing.finish_streaming();
                        }
                    } else if let Some(existing) = items.get_mut(index) {
                        *existing = item;
                    } else {
                        items.push(item);
                    }
                    return;
                }
                let mut item = semantic_tool_result_item(
                    tool_call_id,
                    tool_calls.get(tool_call_id),
                    semantic_result,
                    *is_error,
                );
                if let Some(replay) = streamed_tool_results.get(tool_call_id) {
                    apply_replay_timing(&mut item, replay);
                }
                items.push(item);
                return;
            }
            let should_render_final =
                if let Some(replay) = streamed_tool_results.get_mut(tool_call_id) {
                    if let Some(index) = replay.index
                        && let Some(item) = items.get_mut(index)
                    {
                        item.set_tool_started_at_ms(replay.started_at_ms);
                        item.set_tool_finished_at_ms(replay.finished_at_ms);
                        item.finish_streaming();
                    }
                    !replay.saw_output
                } else {
                    true
                };
            if should_render_final
                && let Some(mut item) = non_streaming_transcript_item_from_event(
                    event,
                    tool_calls,
                    streamed_tool_results,
                )
            {
                if let Some(replay) = streamed_tool_results.get(tool_call_id) {
                    apply_replay_timing(&mut item, replay);
                }
                items.push(item);
            }
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
    if let Some(item) = active_streaming_item_mut(items, role) {
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
    if role_requires_last_item_stream_boundary(role) {
        finish_boundary_streaming_transcript_item(items, role, text);
        return;
    }
    if let Some(item) = latest_streaming_item_mut(items, role) {
        item.text.clear();
        item.text.push_str(text);
        item.streaming = false;
        return;
    }
    items.push(TranscriptItem::new(role, text.to_owned()));
}

fn active_streaming_item_mut<'items>(
    items: &'items mut [TranscriptItem],
    role: &'static str,
) -> Option<&'items mut TranscriptItem> {
    if role_requires_last_item_stream_boundary(role) {
        return latest_item_mut_if_streaming_role(items, role);
    }
    latest_streaming_item_mut(items, role)
}

fn finish_boundary_streaming_transcript_item(
    items: &mut Vec<TranscriptItem>,
    role: &'static str,
    text: &str,
) {
    let matching_stream_count = items
        .iter()
        .filter(|item| item.role == role && item.streaming)
        .count();
    if matching_stream_count > 1 {
        for item in items
            .iter_mut()
            .filter(|item| item.role == role && item.streaming)
        {
            item.streaming = false;
        }
        return;
    }
    if let Some(item) = latest_item_mut_if_streaming_role(items, role) {
        item.text.clear();
        item.text.push_str(text);
        item.streaming = false;
        return;
    }
    items.push(TranscriptItem::new(role, text.to_owned()));
}

fn latest_item_mut_if_streaming_role<'items>(
    items: &'items mut [TranscriptItem],
    role: &'static str,
) -> Option<&'items mut TranscriptItem> {
    let item = items.last_mut()?;
    if item.role == role && item.streaming {
        Some(item)
    } else {
        None
    }
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

fn role_requires_last_item_stream_boundary(role: &'static str) -> bool {
    role == "Reasoning summary"
}

#[allow(clippy::too_many_lines)]
fn non_streaming_transcript_item_from_event(
    event: &SessionEvent,
    tool_calls: &mut BTreeMap<String, ToolCallContext>,
    streamed_tool_results: &BTreeMap<String, StreamedToolReplayContext>,
) -> Option<TranscriptItem> {
    match &event.kind {
        SessionEventKind::UserMessage {
            text, admission, ..
        } => Some(
            admission
                .origin
                .as_ref()
                .and_then(|origin| origin.display_label.clone())
                .map_or_else(
                    || TranscriptItem::new("You", text.clone()),
                    |label| TranscriptItem::new("You", text.clone()).with_display_label(label),
                )
                .with_event_metadata(event.sequence, event.timestamp_ms),
        ),
        SessionEventKind::SystemMessage { text } => Some(
            TranscriptItem::new("System", text.clone())
                .with_event_metadata(event.sequence, event.timestamp_ms),
        ),
        SessionEventKind::PluginStatusNote {
            plugin_id, text, ..
        } => Some(
            TranscriptItem::new("Plugin", text.clone())
                .with_display_label(plugin_id.clone())
                .with_event_metadata(event.sequence, event.timestamp_ms),
        ),
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
            working_directory,
            request_visual,
            legacy_request_presentation: _legacy_request_presentation,
            ..
        } => {
            tool_calls.insert(
                tool_call_id.clone(),
                ToolCallContext {
                    tool_name: tool_name.clone(),
                    arguments_json: arguments_json.clone(),
                    working_directory: working_directory.clone(),
                    request_visual: request_visual.clone(),
                },
            );
            let projection = ToolInvocationProjection {
                tool_call_id: tool_call_id.clone(),
                tool_name: Some(tool_name.clone()),
                arguments_json: Some(arguments_json.clone()),
                request_visual: request_visual.clone(),
                ..ToolInvocationProjection::default()
            };
            Some(tool_request_item_from_projection(&projection))
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
            semantic_result,
            ..
        } => {
            if let Some(semantic_result) = semantic_result {
                return Some(semantic_tool_result_item(
                    tool_call_id,
                    tool_calls.get(tool_call_id),
                    semantic_result,
                    *is_error,
                ));
            }
            if streamed_tool_results
                .get(tool_call_id)
                .is_some_and(|replay| replay.saw_output)
            {
                return None;
            }
            let context = tool_calls.get(tool_call_id);
            let projection = ToolInvocationProjection {
                tool_call_id: tool_call_id.clone(),
                tool_name: context.map(|context| context.tool_name.clone()),
                arguments_json: context.map(|context| context.arguments_json.clone()),
                result_text: Some(result.clone()),
                is_error: Some(*is_error),
                ..ToolInvocationProjection::default()
            };
            generic_tool_result_item_from_projection(&projection)
        }
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
            legacy_request_presentation: _legacy_request_presentation,
            policy_source,
            policy_reason,
            ..
        } => Some(permission_request_item(
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
            policy_source.as_deref(),
            policy_reason.as_deref(),
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
        SessionEventKind::ProviderContextCompacted { snapshot, .. } => {
            Some(TranscriptItem::with_kind(
                "Compaction",
                format!(
                    "provider compacted context ({})",
                    snapshot.provider_plugin_id
                ),
                false,
                TranscriptItemKind::Meta,
            ))
        }
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
        display(old_working_directory, old_working_directory),
        display(new_working_directory, old_working_directory)
    )
}

#[allow(clippy::single_match_else, clippy::needless_pass_by_ref_mut)]
#[allow(dead_code)]
fn apply_semantic_tool_result(
    items: &mut Vec<TranscriptItem>,
    tool_call_id: &str,
    context: Option<&ToolCallContext>,
    _replay: &mut Option<&mut StreamedToolReplayContext>,
    result: &ToolInvocationResult,
    is_error: bool,
) {
    let item = semantic_tool_result_item(tool_call_id, context, result, is_error);
    items.push(item);
}

fn semantic_tool_result_item(
    tool_call_id: &str,
    context: Option<&ToolCallContext>,
    result: &ToolInvocationResult,
    is_error: bool,
) -> TranscriptItem {
    semantic_tool_result_item_from_raw(
        tool_call_id,
        context.map(|context| context.tool_name.as_str()),
        context.map(|context| context.arguments_json.as_str()),
        context.and_then(|context| context.working_directory.as_deref()),
        result,
        is_error,
    )
}

/// Build a transcript item from a raw semantic tool result.
#[must_use]
pub fn semantic_tool_result_item_from_raw(
    tool_call_id: &str,
    tool_name: Option<&str>,
    arguments_json: Option<&str>,
    working_directory: Option<&std::path::Path>,
    result: &ToolInvocationResult,
    is_error: bool,
) -> TranscriptItem {
    let mut item = match result {
        ToolInvocationResult::Text { text } => {
            tool_result_item(tool_call_id, tool_name, arguments_json, text, is_error)
        }
        ToolInvocationResult::Json { value } => {
            tool_result_item(tool_call_id, tool_name, arguments_json, value, is_error)
        }
        ToolInvocationResult::Artifact { artifact } => {
            artifact_tool_result_item(tool_call_id, tool_name, arguments_json, artifact, is_error)
        }
    };
    if let TranscriptItemKind::ToolResult {
        working_directory: item_cwd,
        ..
    } = &mut item.kind
    {
        *item_cwd = working_directory.map(std::path::Path::to_path_buf);
    }
    item
}

pub fn display_tool_result_text(result: &str) -> String {
    if let Ok(result) = serde_json::from_str::<ToolInvocationResult>(result) {
        return match result {
            ToolInvocationResult::Text { text } | ToolInvocationResult::Json { value: text } => {
                text
            }
            ToolInvocationResult::Artifact { artifact } => artifact_summary_text(&artifact),
        };
    }
    serde_json::from_str::<ToolArtifact>(result).map_or_else(
        |_| result.to_owned(),
        |artifact| artifact_summary_text(&artifact),
    )
}

pub fn artifact_summary_text(artifact: &ToolArtifact) -> String {
    let title = artifact.title.as_deref().unwrap_or("Tool artifact");
    let summary = artifact
        .metadata
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&artifact.schema);
    let path = artifact
        .metadata
        .get("path")
        .and_then(serde_json::Value::as_str);
    let text = path.map_or_else(|| summary.to_owned(), |path| format!("{summary}\n{path}"));
    format!("{title}\n{text}")
}

const fn apply_replay_timing(item: &mut TranscriptItem, replay: &StreamedToolReplayContext) {
    item.set_tool_started_at_ms(replay.started_at_ms);
    item.set_tool_finished_at_ms(replay.finished_at_ms);
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
            started_at_ms,
            ..
        } => {
            let replay = streamed_tool_results
                .entry(tool_call_id.clone())
                .or_default();
            replay.started_at_ms = *started_at_ms;
        }
        ToolInvocationStreamEvent::VisualUpdate {
            tool_call_id,
            visual,
            streaming,
            ..
        } => {
            let context = tool_calls.get(tool_call_id);
            let mut item = streaming_tool_visual_item(
                tool_call_id,
                context.map(|context| context.tool_name.as_str()),
                context.and_then(|context| context.working_directory.as_deref()),
                visual,
                *streaming,
            );
            let replay = streamed_tool_results
                .entry(tool_call_id.clone())
                .or_default();
            item.set_tool_started_at_ms(replay.started_at_ms);
            item.set_tool_finished_at_ms(replay.finished_at_ms);
            item.set_tool_timeout_ms(tool_visual_timeout_ms(visual));
            let index = upsert_tool_visual_item(items, item);
            replay.index = Some(index);
            replay.saw_output = true;
        }
        ToolInvocationStreamEvent::Finished {
            tool_call_id,
            finished_at_ms,
            ..
        } => {
            let replay = streamed_tool_results
                .entry(tool_call_id.clone())
                .or_default();
            replay.finished_at_ms = *finished_at_ms;
            if let Some(index) = replay.index
                && let Some(item) = items.get_mut(index)
            {
                item.set_tool_started_at_ms(replay.started_at_ms);
                item.set_tool_finished_at_ms(replay.finished_at_ms);
                item.finish_streaming();
            }
        }
        ToolInvocationStreamEvent::OutputDelta { .. }
        | ToolInvocationStreamEvent::ArtifactUpdate { .. }
        | ToolInvocationStreamEvent::Status { .. }
        | ToolInvocationStreamEvent::LegacyPresentation { .. }
        | ToolInvocationStreamEvent::LegacyTransientPruned { .. } => {}
    }
}

fn tool_visual_timeout_ms(visual: &bcode_session_models::PluginVisualDescriptor) -> Option<u64> {
    visual
        .payload
        .get("_bcode_runtime")
        .and_then(|runtime| runtime.get("timeout_ms"))
        .and_then(serde_json::Value::as_u64)
}

fn kind_for_role(role: &str) -> TranscriptItemKind {
    match role {
        "You" => TranscriptItemKind::UserMessage,
        "Assistant" => TranscriptItemKind::AssistantMessage,
        "Reasoning summary" => TranscriptItemKind::ReasoningMessage,
        "System" => TranscriptItemKind::System,
        "Skill" => TranscriptItemKind::Skill,
        "Skill error" => TranscriptItemKind::SkillError,
        "Compaction" | "Meta" => TranscriptItemKind::Meta,
        _ => TranscriptItemKind::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_turn_origin_labels_only_the_matching_user_turn() {
        let session_id = bcode_session_models::SessionId::new();
        let events = vec![
            SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 5,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "automated prompt".to_owned(),
                    admission: bcode_session_models::TurnAdmissionMetadata {
                        origin: Some(bcode_session_models::TurnOrigin {
                            producer: "test.producer".to_owned(),
                            correlation_id: Some("operation-1".to_owned()),
                            display_label: Some("Background pass 4".to_owned()),
                        }),
                        ..bcode_session_models::TurnAdmissionMetadata::default()
                    },
                },
            },
            SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 7,
                timestamp_ms: 3,
                session_id,
                provenance: None,
                kind: SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "manual steering".to_owned(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            },
        ];

        let items = transcript_items_from_events_with_reasoning(&events, false);
        assert_eq!(items[0].display_role(), "You · Background pass 4");
        assert_eq!(items[0].text(), "automated prompt");
        assert_eq!(items[1].display_role(), "You");
        assert_eq!(items[1].text(), "manual steering");
    }

    #[test]
    fn provider_compaction_transcript_hides_opaque_payloads() {
        let secret = "secret-opaque-transcript-value";
        let event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id: bcode_session_models::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::ProviderContextCompacted {
                compacted_through_sequence: 0,
                snapshot: bcode_session_models::ProviderContextSnapshot {
                    format_version: 1,
                    request_fingerprint: None,
                    request_id: None,
                    provider_plugin_id: "provider".to_owned(),
                    model_id: "model".to_owned(),
                    compatibility_key: "surface".to_owned(),
                    auth_profile: None,
                    origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
                    messages_json: format!(r#"[{{"encrypted":"{secret}"}}]"#),
                    portable_summary: "portable summary".to_owned(),
                },
            },
        };

        let items = transcript_items_from_events_with_reasoning(&[event], false);
        assert_eq!(items.len(), 1);
        assert!(items[0].text().contains("provider compacted context"));
        assert!(!items[0].text().contains(secret));
        assert!(!items[0].text().contains("portable summary"));
    }

    #[test]
    fn plugin_status_note_projects_as_compact_plugin_transcript_item() {
        let session_id = bcode_session_models::SessionId::new();
        let events = [SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::PluginStatusNote {
                plugin_id: "bcode.loop".to_owned(),
                note_id: "run-1:lifecycle:Completed".to_owned(),
                text: "Loop completed · evaluator accepted: done".to_owned(),
                metadata: std::collections::BTreeMap::new(),
            },
        }];

        let items = transcript_items_from_events_with_reasoning(&events, false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].display_role(), "Plugin · bcode.loop");
        assert_eq!(items[0].text(), "Loop completed · evaluator accepted: done");
    }

    #[test]
    fn transcript_item_display_label_is_generic() {
        let item = TranscriptItem::new("You", "text".to_owned())
            .with_display_label("Plugin operation".to_owned());
        assert_eq!(item.display_role(), "You · Plugin operation");
    }
}
