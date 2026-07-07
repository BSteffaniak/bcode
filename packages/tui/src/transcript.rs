//! Transcript item projection for the TUI.

use std::collections::BTreeMap;

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
    /// Host-owned interactive tool request.
    InteractiveToolRequest {
        /// Interaction identifier.
        interaction_id: String,
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// Surface renderer key.
        surface_kind: String,
        /// Raw plugin-owned request JSON.
        request_json: String,
        /// Whether the interaction is required by the tool surface.
        required: bool,
    },
    /// Host-owned interactive tool request resolution.
    InteractiveToolResolution {
        /// Interaction identifier.
        interaction_id: String,
        /// Provider tool call identifier.
        tool_call_id: String,
        /// Generic core resolution JSON.
        resolution_json: String,
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
    /// Generic fallback item.
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ToolCallContext {
    tool_name: String,
    arguments_json: String,
    request_visual: Option<bcode_session_models::PluginVisualDescriptor>,
}

/// Lifecycle surface for a tool-related transcript item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolTranscriptSurface {
    /// Durable request context for what tool/arguments were requested.
    Request,
    /// Live-only argument preview used as richer request context.
    LiveArgumentPreview,
    /// Generic or semantic tool result.
    Result,
    /// Permission or interactive tool request/resolution flow.
    Interaction,
}

/// Return the tool call id and lifecycle surface for a tool transcript item.
#[must_use]
pub const fn tool_surface_for_item(item: &TranscriptItem) -> Option<(&str, ToolTranscriptSurface)> {
    match item.kind() {
        TranscriptItemKind::ToolRequest { tool_call_id, .. } => {
            Some((tool_call_id.as_str(), ToolTranscriptSurface::Request))
        }
        TranscriptItemKind::LiveToolPreviewAnchor { tool_call_id, .. } => Some((
            tool_call_id.as_str(),
            ToolTranscriptSurface::LiveArgumentPreview,
        )),
        TranscriptItemKind::ToolResult { tool_call_id, .. } => {
            Some((tool_call_id.as_str(), ToolTranscriptSurface::Result))
        }
        TranscriptItemKind::InteractiveToolRequest { tool_call_id, .. }
        | TranscriptItemKind::InteractiveToolResolution { tool_call_id, .. }
        | TranscriptItemKind::PermissionRequest { tool_call_id, .. } => {
            Some((tool_call_id.as_str(), ToolTranscriptSurface::Interaction))
        }
        TranscriptItemKind::UserMessage
        | TranscriptItemKind::AssistantMessage
        | TranscriptItemKind::ReasoningMessage
        | TranscriptItemKind::Usage { .. }
        | TranscriptItemKind::PermissionResult { .. }
        | TranscriptItemKind::System
        | TranscriptItemKind::Meta
        | TranscriptItemKind::Skill
        | TranscriptItemKind::SkillError
        | TranscriptItemKind::Generic => None,
    }
}

/// Return whether `item` belongs to one of `surfaces` for `tool_call_id`.
#[must_use]
pub fn item_is_tool_surface_for_tool_call(
    item: &TranscriptItem,
    tool_call_id: &str,
    surfaces: &[ToolTranscriptSurface],
) -> bool {
    tool_surface_for_item(item).is_some_and(|(item_tool_call_id, surface)| {
        item_tool_call_id == tool_call_id && surfaces.contains(&surface)
    })
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
        if let TranscriptItemKind::ToolResult { result, .. } = &mut self.kind {
            result.push_str(text);
        }
        self.bump_revision();
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
    visual: &bcode_session_models::PluginVisualDescriptor,
    streaming: bool,
) -> TranscriptItem {
    let artifact = ToolArtifact {
        artifact_id: format!("{tool_call_id}-stream-visual"),
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
            result: artifact_summary_text(&artifact),
            artifact: Some(Box::new(artifact)),
            is_error: false,
            timing: ToolTiming::default(),
        },
    )
}

/// Upsert a plugin-owned visual update item for a tool call.
pub fn upsert_tool_visual_item(items: &mut Vec<TranscriptItem>, item: TranscriptItem) -> usize {
    let Some((tool_call_id, schema)) = tool_visual_identity(&item) else {
        items.push(item);
        return items.len().saturating_sub(1);
    };
    let tool_call_id = tool_call_id.to_owned();
    let schema = schema.to_owned();
    if let Some(index) = items.iter().position(|existing| {
        tool_visual_identity(existing) == Some((tool_call_id.as_str(), schema.as_str()))
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
    Some((tool_call_id.as_str(), artifact.schema.as_str()))
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
            result,
            artifact: Some(Box::new(artifact)),
            is_error,
            timing: ToolTiming::default(),
        },
    )
}

/// Build an interactive tool request item.
#[must_use]
pub fn interactive_tool_request_item(
    interaction_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    surface_kind: &str,
    request_json: &str,
    required: bool,
) -> TranscriptItem {
    let label = if required { "required" } else { "optional" };
    let text = format!(
        "interactive request ({label}) via {surface_kind}:\n{}",
        pretty_jsonish(request_json)
    );
    TranscriptItem::with_kind(
        "Interactive tool",
        text,
        false,
        TranscriptItemKind::InteractiveToolRequest {
            interaction_id: interaction_id.to_owned(),
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            surface_kind: surface_kind.to_owned(),
            request_json: request_json.to_owned(),
            required,
        },
    )
}

/// Build an interactive tool resolution item.
#[must_use]
pub fn interactive_tool_resolution_item(
    interaction_id: &str,
    tool_call_id: &str,
    resolution_json: &str,
) -> TranscriptItem {
    TranscriptItem::with_kind(
        "Interactive tool",
        format!("interactive request resolved: {interaction_id}"),
        false,
        TranscriptItemKind::InteractiveToolResolution {
            interaction_id: interaction_id.to_owned(),
            tool_call_id: tool_call_id.to_owned(),
            resolution_json: resolution_json.to_owned(),
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
        SessionEventKind::UserMessage { text, .. } => Some(
            TranscriptItem::new("You", text.clone())
                .with_event_metadata(event.sequence, event.timestamp_ms),
        ),
        SessionEventKind::SystemMessage { text } => Some(
            TranscriptItem::new("System", text.clone())
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
            request_visual,
            legacy_request_presentation: _legacy_request_presentation,
            ..
        } => {
            tool_calls.insert(
                tool_call_id.clone(),
                ToolCallContext {
                    tool_name: tool_name.clone(),
                    arguments_json: arguments_json.clone(),
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
        SessionEventKind::InteractiveToolRequestCreated {
            interaction_id,
            tool_call_id,
            tool_name,
            surface_kind,
            request_json,
            required,
            ..
        } => Some(interactive_tool_request_item(
            interaction_id,
            tool_call_id,
            tool_name,
            surface_kind,
            request_json,
            *required,
        )),
        SessionEventKind::InteractiveToolRequestResolved {
            interaction_id,
            tool_call_id,
            resolution_json,
        } => Some(interactive_tool_resolution_item(
            interaction_id,
            tool_call_id,
            resolution_json,
        )),
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
    result: &ToolInvocationResult,
    is_error: bool,
) -> TranscriptItem {
    match result {
        ToolInvocationResult::Text { text } => {
            tool_result_item(tool_call_id, tool_name, arguments_json, text, is_error)
        }
        ToolInvocationResult::Json { value } => {
            tool_result_item(tool_call_id, tool_name, arguments_json, value, is_error)
        }
        ToolInvocationResult::Artifact { artifact } => {
            artifact_tool_result_item(tool_call_id, tool_name, arguments_json, artifact, is_error)
        }
    }
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
                visual,
                *streaming,
            );
            let replay = streamed_tool_results
                .entry(tool_call_id.clone())
                .or_default();
            item.set_tool_started_at_ms(replay.started_at_ms);
            item.set_tool_finished_at_ms(replay.finished_at_ms);
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
        | ToolInvocationStreamEvent::Status { .. }
        | ToolInvocationStreamEvent::LegacyPresentation { .. } => {}
    }
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
