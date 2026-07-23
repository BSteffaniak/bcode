//! Transcript item projection for the TUI.

#[cfg(test)]
use bcode_session_models::{SessionEvent, SessionEventKind};
use bcode_session_models::{
    SessionTokenUsage, ToolArtifact, ToolInvocationProjection, ToolInvocationResult,
};
#[cfg(test)]
use bcode_session_view::SessionView;
use bcode_session_view_models::{
    InteractionViewSummary, RuntimeWorkView, ToolInvocationView, ToolInvocationViewStatus,
    ToolResultView, TranscriptViewItem, TranscriptViewItemKind,
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
        /// Renderer-neutral semantic placement.
        placement: bcode_session_models::ToolContributionPlacement,
    },
    /// Generic fallback item.
    Generic,
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
    source_view_item_id: Option<bcode_session_view_models::TranscriptViewItemId>,
    source_view_revision: Option<bcode_session_view_models::ViewRevision>,
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
            source_view_item_id: None,
            source_view_revision: None,
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

    /// Return the renderer-neutral source identity, when this item adapts shared session state.
    #[must_use]
    pub const fn source_view_item_id(
        &self,
    ) -> Option<&bcode_session_view_models::TranscriptViewItemId> {
        self.source_view_item_id.as_ref()
    }

    fn with_source_view_item(
        mut self,
        id: bcode_session_view_models::TranscriptViewItemId,
        revision: bcode_session_view_models::ViewRevision,
    ) -> Self {
        self.source_view_item_id = Some(id);
        self.source_view_revision = Some(revision);
        self
    }

    pub(crate) fn replace_from_shared(&mut self, replacement: Self) -> bool {
        debug_assert_eq!(self.source_view_item_id, replacement.source_view_item_id);
        if self.source_view_revision == replacement.source_view_revision {
            return false;
        }
        self.source_view_revision = replacement.source_view_revision;
        self.role = replacement.role;
        self.text = replacement.text;
        self.streaming = replacement.streaming;
        self.display_label = replacement.display_label;
        self.event_sequence = replacement.event_sequence;
        self.timestamp_ms = replacement.timestamp_ms;
        self.kind = replacement.kind;
        self.bump_revision();
        true
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

    /// Replace display text.
    pub fn replace_text(&mut self, text: String) {
        if let TranscriptItemKind::ToolResult { result, .. } = &mut self.kind {
            result.clone_from(&text);
        }
        self.text = text;
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

/// Project session events through the shared semantic view into terminal transcript items,
/// optionally hiding reasoning items.
#[cfg(test)]
#[must_use]
pub fn transcript_items_from_events_with_reasoning(
    events: &[SessionEvent],
    include_reasoning: bool,
) -> Vec<TranscriptItem> {
    let mut view = SessionView::new();
    for event in events {
        view.apply_event(event);
    }
    view.snapshot()
        .transcript
        .items
        .iter()
        .filter(|item| {
            include_reasoning
                || !matches!(item.kind, TranscriptViewItemKind::ReasoningMessage { .. })
        })
        .map(terminal_item_from_shared)
        .collect()
}

/// Append streamed text to the currently open transcript stream for `role`.
///
/// Interleaved telemetry rows, such as token usage, may be appended while a model stream is open.
/// The open stream is therefore the newest streaming row for the same role, not necessarily the
/// final transcript row.
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
    tool_result_item_with_working_directory(
        tool_call_id,
        tool_name,
        arguments_json,
        None,
        result,
        is_error,
    )
}

fn tool_result_item_with_working_directory(
    tool_call_id: &str,
    tool_name: Option<&str>,
    arguments_json: Option<&str>,
    working_directory: Option<&std::path::Path>,
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
            working_directory: working_directory.map(std::path::Path::to_path_buf),
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
    match result {
        ToolInvocationResult::Text { text } | ToolInvocationResult::Json { value: text } => {
            tool_result_item_with_working_directory(
                tool_call_id,
                tool_name,
                arguments_json,
                working_directory,
                text,
                is_error,
            )
        }
        ToolInvocationResult::Artifact { artifact } => {
            artifact_tool_result_item(tool_call_id, tool_name, arguments_json, artifact, is_error)
        }
    }
}

/// Render a tool result string, parsing structured result payloads when possible.
#[must_use]
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

/// Summarize a plugin-owned artifact for generic terminal rendering.
#[must_use]
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

fn tool_timing_from_artifact(artifact: &ToolArtifact) -> ToolTiming {
    ToolTiming {
        timed_out: artifact
            .metadata
            .get("timed_out")
            .and_then(serde_json::Value::as_bool),
        ..ToolTiming::default()
    }
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

/// Adapt one generic shared semantic item into terminal transcript presentation.
#[must_use]
pub fn terminal_item_from_shared(item: &TranscriptViewItem) -> TranscriptItem {
    let mut terminal = match &item.kind {
        TranscriptViewItemKind::UserMessage { message } => message_text_item(
            "You",
            message,
            item.streaming,
            TranscriptItemKind::UserMessage,
        ),
        TranscriptViewItemKind::AssistantMessage { message } => message_text_item(
            "Assistant",
            message,
            item.streaming,
            TranscriptItemKind::AssistantMessage,
        ),
        TranscriptViewItemKind::ReasoningMessage { message } => message_text_item(
            "Reasoning summary",
            message,
            item.streaming,
            TranscriptItemKind::ReasoningMessage,
        ),
        TranscriptViewItemKind::SystemMessage { message } => {
            let role = message
                .display_label
                .as_ref()
                .map_or("System", |_| "Plugin");
            message_text_item(role, message, item.streaming, TranscriptItemKind::System)
        }
        TranscriptViewItemKind::Usage { usage } => model_usage_item(&usage.turn_id, &usage.usage),
        TranscriptViewItemKind::Compaction { compaction } => TranscriptItem::with_kind(
            "Compaction",
            compaction.text.clone(),
            item.streaming,
            TranscriptItemKind::Meta,
        ),
        TranscriptViewItemKind::Skill { skill } => terminal_skill_item_from_shared(skill),
        TranscriptViewItemKind::ToolInvocation { tool } => terminal_tool_item_from_shared(tool),
        TranscriptViewItemKind::ToolRequest { tool } => {
            terminal_tool_request_item_from_shared(tool)
        }
        TranscriptViewItemKind::Permission { permission } => {
            terminal_permission_item_from_shared(permission)
        }
        TranscriptViewItemKind::RuntimeWork { work } => {
            terminal_runtime_work_item_from_shared(work)
        }
        TranscriptViewItemKind::Interaction { interaction } => {
            terminal_interaction_item_from_shared(interaction)
        }
        TranscriptViewItemKind::PluginVisual { visual } => {
            let fallback = serde_json::to_string_pretty(&visual.generic_payload)
                .unwrap_or_else(|_| visual.generic_payload.to_string());
            TranscriptItem::with_kind(
                "Plugin visual",
                fallback,
                item.streaming,
                TranscriptItemKind::Generic,
            )
        }
        TranscriptViewItemKind::ToolContribution {
            contribution,
            placement,
        } => {
            let fallback = match placement {
                bcode_session_models::ToolContributionPlacement::Request => "tool request",
                bcode_session_models::ToolContributionPlacement::Progress => "tool progress",
                bcode_session_models::ToolContributionPlacement::Result => "tool result",
                bcode_session_models::ToolContributionPlacement::Supplemental
                | bcode_session_models::ToolContributionPlacement::Hidden => "",
            };
            TranscriptItem::with_kind(
                "Tool contribution",
                fallback.to_owned(),
                item.streaming,
                TranscriptItemKind::ToolContribution {
                    contribution: Box::new(contribution.clone()),
                    placement: *placement,
                },
            )
        }
    };
    if let (Some(sequence), Some(timestamp_ms)) = (item.sequence, item.timestamp_ms) {
        terminal = terminal.with_event_metadata(sequence, timestamp_ms);
    }
    terminal.with_source_view_item(item.id.clone(), item.revision)
}

fn message_text_item(
    role: &'static str,
    message: &bcode_session_view_models::ChatMessageView,
    streaming: bool,
    kind: TranscriptItemKind,
) -> TranscriptItem {
    let item = TranscriptItem::with_kind(role, message.text.clone(), streaming, kind);
    if let Some(label) = &message.display_label {
        item.with_display_label(label.clone())
    } else {
        item
    }
}

fn terminal_skill_item_from_shared(skill: &bcode_session_view_models::SkillView) -> TranscriptItem {
    let (role, kind) = match skill.status {
        bcode_session_view_models::SkillViewStatus::Invoked
        | bcode_session_view_models::SkillViewStatus::Suggested => {
            ("Skill", TranscriptItemKind::Skill)
        }
        bcode_session_view_models::SkillViewStatus::ContextLoaded => {
            ("Skill context", TranscriptItemKind::Generic)
        }
        bcode_session_view_models::SkillViewStatus::Failed => {
            ("Skill error", TranscriptItemKind::SkillError)
        }
    };
    TranscriptItem::with_kind(role, skill.text.clone(), false, kind)
}

fn terminal_tool_request_item_from_shared(tool: &ToolInvocationView) -> TranscriptItem {
    apply_shared_tool_timing(
        tool_request_item(
            &tool.tool_call_id,
            tool.producer_plugin_id.as_deref(),
            tool.tool_name.as_deref().unwrap_or("unknown tool"),
            tool.arguments_json.as_deref().unwrap_or("{}"),
            tool.working_directory.clone(),
            tool.request_visual
                .as_ref()
                .map(|visual| visual.descriptor.clone()),
        ),
        tool,
    )
}

fn terminal_tool_item_from_shared(tool: &ToolInvocationView) -> TranscriptItem {
    if let Some(ToolResultView::Artifact { artifact }) = &tool.result {
        return apply_shared_tool_timing(
            artifact_tool_result_item(
                &tool.tool_call_id,
                tool.tool_name.as_deref(),
                tool.arguments_json.as_deref(),
                &artifact.artifact,
                tool.is_error.unwrap_or(false),
            ),
            tool,
        );
    }
    if let Some(result) = tool_result_text_from_shared(tool) {
        return apply_shared_tool_timing(
            tool_result_item(
                &tool.tool_call_id,
                tool.tool_name.as_deref(),
                tool.arguments_json.as_deref(),
                &result,
                tool.is_error.unwrap_or(false),
            ),
            tool,
        );
    }
    if let Some(output) = &tool.output
        && !output.text.is_empty()
    {
        let mut item = if matches!(tool.status, ToolInvocationViewStatus::Running) {
            streaming_tool_output_item(
                &tool.tool_call_id,
                tool.tool_name.as_deref(),
                tool.arguments_json.as_deref(),
                &output.text,
            )
        } else {
            tool_result_item(
                &tool.tool_call_id,
                tool.tool_name.as_deref(),
                tool.arguments_json.as_deref(),
                &output.text,
                tool.is_error.unwrap_or(false),
            )
        };
        if matches!(tool.status, ToolInvocationViewStatus::Finished) {
            item.finish_streaming();
        }
        return apply_shared_tool_timing(item, tool);
    }
    let mut item = tool_request_item(
        &tool.tool_call_id,
        tool.producer_plugin_id.as_deref(),
        tool.tool_name.as_deref().unwrap_or("unknown tool"),
        tool.arguments_json.as_deref().unwrap_or("{}"),
        tool.working_directory.clone(),
        tool.request_visual
            .as_ref()
            .map(|visual| visual.descriptor.clone()),
    );
    if matches!(tool.status, ToolInvocationViewStatus::Running) {
        item.streaming = true;
    }
    item
}

fn tool_result_text_from_shared(tool: &ToolInvocationView) -> Option<String> {
    match &tool.result {
        Some(ToolResultView::Text { text }) => Some(text.clone()),
        Some(ToolResultView::Json { value }) => Some(pretty_jsonish(value)),
        Some(ToolResultView::Artifact { .. }) | None => {
            tool.result_text.as_deref().map(display_tool_result_text)
        }
    }
}

const fn apply_shared_tool_timing(
    mut item: TranscriptItem,
    tool: &ToolInvocationView,
) -> TranscriptItem {
    item.set_tool_started_at_ms(tool.timing.started_at_ms);
    item.set_tool_finished_at_ms(tool.timing.finished_at_ms);
    item.set_tool_timeout_ms(tool.timing.timeout_ms);
    item.set_tool_timed_out(tool.timing.timed_out);
    item
}

fn terminal_permission_item_from_shared(
    permission: &bcode_session_view_models::PermissionView,
) -> TranscriptItem {
    if let Some(approved) = permission.approved {
        return permission_result_item(&permission.permission_id, approved);
    }
    permission_request_item(
        &permission.permission_id,
        &permission.tool_call_id,
        &permission.tool_name,
        &permission.arguments_json,
        permission.policy_source.as_deref(),
        permission.detail.as_deref(),
    )
}

fn terminal_runtime_work_item_from_shared(work: &RuntimeWorkView) -> TranscriptItem {
    let mut lines = vec![format!("{}: {:?}", work.work_id, work.status)];
    if !work.label.is_empty() {
        lines.push(format!("label: {}", work.label));
    }
    if let Some(message) = &work.message {
        lines.push(format!("message: {message}"));
    }
    if let (Some(completed), Some(total)) = (work.completed_units, work.total_units) {
        lines.push(format!("progress: {completed}/{total}"));
    }
    TranscriptItem::with_kind(
        "Runtime work",
        lines.join("\n"),
        !work.is_terminal(),
        TranscriptItemKind::Meta,
    )
}

fn terminal_interaction_item_from_shared(interaction: &InteractionViewSummary) -> TranscriptItem {
    let payload = if interaction.resolved {
        interaction.resolution.as_ref()
    } else {
        interaction.snapshot.as_ref()
    };
    let payload = payload.map_or_else(
        || "null".to_owned(),
        |value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    );
    let state = if interaction.resolved {
        "resolved"
    } else if interaction.required {
        "response required"
    } else {
        "optional"
    };
    TranscriptItem::with_kind(
        "Interaction",
        format!(
            "{} ({state})\n{payload}",
            interaction.title.as_deref().unwrap_or(&interaction.kind)
        ),
        false,
        TranscriptItemKind::Generic,
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
    fn shared_generic_items_adapt_without_renderer_types_crossing_the_boundary() {
        let usage = bcode_session_models::SessionTokenUsage {
            input_tokens: Some(2),
            output_tokens: Some(3),
            ..bcode_session_models::SessionTokenUsage::default()
        };
        let cases = [
            (
                TranscriptViewItemKind::UserMessage {
                    message: bcode_session_view_models::ChatMessageView::markdown("hello"),
                },
                "You",
                TranscriptItemKind::UserMessage,
            ),
            (
                TranscriptViewItemKind::AssistantMessage {
                    message: bcode_session_view_models::ChatMessageView::markdown("answer"),
                },
                "Assistant",
                TranscriptItemKind::AssistantMessage,
            ),
            (
                TranscriptViewItemKind::ReasoningMessage {
                    message: bcode_session_view_models::ChatMessageView::markdown("thought"),
                },
                "Reasoning summary",
                TranscriptItemKind::ReasoningMessage,
            ),
            (
                TranscriptViewItemKind::SystemMessage {
                    message: bcode_session_view_models::ChatMessageView::markdown("status"),
                },
                "System",
                TranscriptItemKind::System,
            ),
            (
                TranscriptViewItemKind::Usage {
                    usage: bcode_session_view_models::UsageView {
                        turn_id: "turn-1".to_owned(),
                        usage,
                    },
                },
                "Usage",
                TranscriptItemKind::Usage {
                    turn_id: "turn-1".to_owned(),
                },
            ),
        ];

        for (kind, role, expected_kind) in cases {
            let shared = TranscriptViewItem {
                id: bcode_session_view_models::TranscriptViewItemId::new("test:item"),
                revision: 1,
                sequence: Some(7),
                timestamp_ms: Some(9),
                streaming: false,
                kind,
            };
            let terminal = terminal_item_from_shared(&shared);
            assert_eq!(terminal.role, role);
            assert_eq!(terminal.kind(), &expected_kind);
            assert_eq!(terminal.event_sequence(), Some(7));
        }
    }

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
        assert!(items[0].text().contains("context compaction"));
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
