//! Transcript item projection for the TUI.

#[cfg(test)]
use bcode_session_models::{SessionEvent, SessionEventKind};
use bcode_session_models::{ToolArtifact, ToolInvocationResult};
#[cfg(test)]
use bcode_session_view::SessionView;
use bcode_session_view_models::{
    InteractionViewSummary, TextFormat, ToolInvocationView, ToolInvocationViewStatus,
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
    /// Final tool duration in milliseconds, when known.
    pub duration_ms: Option<u64>,
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
        /// Whether host-owned lifecycle state is active or waiting.
        active: bool,
        /// Authoritative shared lifecycle status, when projected from `SessionView`.
        status: Option<ToolInvocationViewStatus>,
        /// Generic timing metadata for the tool invocation.
        timing: ToolTiming,
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
    /// Live provider tool request draft rendered through a plugin-owned schema.
    ToolRequestDraft {
        /// Current bounded draft state.
        draft: Box<bcode_session_view_models::ToolRequestDraftView>,
    },
    /// Generic schema-versioned tool contribution.
    ToolContribution {
        /// Opaque contribution envelope.
        contribution: Box<bcode_session_models::ToolContributionEvent>,
        /// Renderer-neutral semantic placement.
        placement: bcode_session_models::ToolContributionPlacement,
        /// Current semantic state of the owning invocation, when known.
        invocation: Option<Box<ToolInvocationView>>,
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
    text_format: TextFormat,
    display_label: Option<String>,
    event_sequence: Option<u64>,
    timestamp_ms: Option<u64>,
    kind: TranscriptItemKind,
}

impl TranscriptItem {
    #[cfg(test)]
    pub fn new(role: &'static str, text: String) -> Self {
        Self::with_identity(
            role,
            text,
            false,
            TextFormat::PlainText,
            kind_for_role(role),
        )
    }

    /// Create a local transcript item with an explicit text format.
    #[cfg(test)]
    #[must_use]
    pub fn with_format(role: &'static str, text: String, text_format: TextFormat) -> Self {
        Self::with_identity(role, text, false, text_format, kind_for_role(role))
    }

    pub(crate) fn with_kind(
        role: &'static str,
        text: String,
        streaming: bool,
        kind: TranscriptItemKind,
    ) -> Self {
        Self::with_identity(role, text, streaming, TextFormat::PlainText, kind)
    }

    pub(crate) fn with_identity(
        role: &'static str,
        text: String,
        streaming: bool,
        text_format: TextFormat,
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
            text_format,
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

    /// Return the renderer-neutral source revision, when this item adapts shared session state.
    #[cfg(test)]
    #[must_use]
    pub const fn source_view_item_revision(
        &self,
    ) -> Option<bcode_session_view_models::ViewRevision> {
        self.source_view_revision
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
        self.text_format = replacement.text_format;
        self.display_label = replacement.display_label;
        self.event_sequence = replacement.event_sequence;
        self.timestamp_ms = replacement.timestamp_ms;
        self.kind = replacement.kind;
        self.bump_revision();
        true
    }

    /// Return the generic tool invocation id that owns this visual item, when any.
    #[must_use]
    pub fn visual_invocation_id(&self) -> Option<&str> {
        match &self.kind {
            TranscriptItemKind::ToolRequest { tool_call_id, .. }
            | TranscriptItemKind::ToolResult { tool_call_id, .. }
            | TranscriptItemKind::PermissionRequest { tool_call_id, .. } => Some(tool_call_id),
            TranscriptItemKind::ToolContribution { contribution, .. } => {
                Some(&contribution.invocation_id)
            }
            TranscriptItemKind::ToolRequestDraft { draft } => Some(&draft.tool_call_id),
            TranscriptItemKind::UserMessage
            | TranscriptItemKind::AssistantMessage
            | TranscriptItemKind::ReasoningMessage
            | TranscriptItemKind::PermissionResult { .. }
            | TranscriptItemKind::System
            | TranscriptItemKind::Meta
            | TranscriptItemKind::Skill
            | TranscriptItemKind::SkillError
            | TranscriptItemKind::Generic => None,
        }
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
    #[cfg(test)]
    pub fn append_text(&mut self, text: &str) {
        self.text.push_str(text);
        if let TranscriptItemKind::ToolResult { result, .. } = &mut self.kind {
            result.push_str(text);
        }
        self.bump_revision();
    }

    /// Return whether this item represents an active tool invocation.
    #[must_use]
    pub const fn tool_is_active(&self) -> bool {
        match &self.kind {
            TranscriptItemKind::ToolContribution {
                invocation: Some(invocation),
                ..
            } => matches!(
                invocation.status,
                ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
            ),
            TranscriptItemKind::ToolRequest { active, .. } => *active,
            _ => false,
        }
    }

    /// Return generic tool timing metadata, when this item represents a tool invocation.
    #[must_use]
    pub const fn tool_timing(&self) -> Option<ToolTiming> {
        match &self.kind {
            TranscriptItemKind::ToolRequest { timing, .. }
            | TranscriptItemKind::ToolResult { timing, .. } => Some(*timing),
            TranscriptItemKind::ToolContribution {
                invocation: Some(invocation),
                ..
            } => Some(tool_timing_from_view(invocation)),
            _ => None,
        }
    }

    /// Set host-owned active/waiting lifecycle state on a tool request item.
    pub const fn set_tool_active(&mut self, is_active: bool) {
        if let TranscriptItemKind::ToolRequest { active, .. } = &mut self.kind {
            *active = is_active;
            self.bump_revision();
        }
    }

    /// Set generic tool start time metadata on a tool invocation item.
    pub const fn set_tool_started_at_ms(&mut self, started_at_ms: Option<u64>) {
        match &mut self.kind {
            TranscriptItemKind::ToolRequest { timing, .. }
            | TranscriptItemKind::ToolResult { timing, .. } => {
                timing.started_at_ms = started_at_ms;
                self.bump_revision();
            }
            _ => {}
        }
    }

    /// Set generic tool finish time metadata on a tool invocation item.
    pub const fn set_tool_finished_at_ms(&mut self, finished_at_ms: Option<u64>) {
        match &mut self.kind {
            TranscriptItemKind::ToolRequest { timing, .. }
            | TranscriptItemKind::ToolResult { timing, .. } => {
                timing.finished_at_ms = finished_at_ms;
                self.bump_revision();
            }
            _ => {}
        }
    }

    /// Set generic tool timeout duration metadata on a tool invocation item.
    pub const fn set_tool_timeout_ms(&mut self, timeout_ms: Option<u64>) {
        match &mut self.kind {
            TranscriptItemKind::ToolRequest { timing, .. }
            | TranscriptItemKind::ToolResult { timing, .. } => {
                timing.timeout_ms = timeout_ms;
                self.bump_revision();
            }
            _ => {}
        }
    }

    /// Set generic tool timeout result metadata on a tool invocation item.
    pub const fn set_tool_timed_out(&mut self, timed_out: Option<bool>) {
        match &mut self.kind {
            TranscriptItemKind::ToolRequest { timing, .. }
            | TranscriptItemKind::ToolResult { timing, .. } => {
                timing.timed_out = timed_out;
                self.bump_revision();
            }
            _ => {}
        }
    }

    /// Set final generic tool duration metadata on a tool invocation item.
    pub const fn set_tool_duration_ms(&mut self, duration_ms: Option<u64>) {
        match &mut self.kind {
            TranscriptItemKind::ToolRequest { timing, .. }
            | TranscriptItemKind::ToolResult { timing, .. } => {
                timing.duration_ms = duration_ms;
                self.bump_revision();
            }
            _ => {}
        }
    }

    /// Copy generic tool timing from another tool item.
    /// Return semantic item kind.
    #[must_use]
    pub const fn kind(&self) -> &TranscriptItemKind {
        &self.kind
    }

    /// Return the renderer-neutral text format.
    #[must_use]
    pub const fn text_format(&self) -> TextFormat {
        self.text_format
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

/// Build a transcript item for a tool request.
#[must_use]
pub fn tool_request_item(
    tool_call_id: &str,
    producer_plugin_id: Option<&str>,
    tool_name: &str,
    arguments_json: &str,
    working_directory: Option<std::path::PathBuf>,
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
            active: false,
            status: None,
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

/// Build a transcript item from a raw semantic tool result for isolated terminal adapter tests.
#[cfg(test)]
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
        duration_ms: artifact
            .metadata
            .get("duration_ms")
            .and_then(serde_json::Value::as_u64),
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

fn bounded_tool_request_draft_fallback(
    draft: &bcode_session_view_models::ToolRequestDraftView,
) -> String {
    const MAX_TOOL_NAME_CHARS: usize = 96;
    let tool_name = draft
        .tool_name
        .chars()
        .take(MAX_TOOL_NAME_CHARS)
        .collect::<String>();
    let truncation = if draft.truncated { " · truncated" } else { "" };
    format!(
        "{tool_name} request · {} bytes{truncation}",
        draft.argument_bytes
    )
}

fn terminal_tool_invocation_item_from_shared(
    tool: &bcode_session_view_models::ToolInvocationView,
) -> TranscriptItem {
    if let Some(draft) = tool.request_draft.as_ref()
        && tool.presentation.is_none()
        && tool.result.is_none()
        && tool.result_text.is_none()
    {
        TranscriptItem::with_kind(
            "Tool request draft",
            bounded_tool_request_draft_fallback(draft),
            true,
            TranscriptItemKind::ToolRequestDraft {
                draft: Box::new(draft.clone()),
            },
        )
    } else {
        terminal_tool_item_from_shared(tool)
    }
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
        TranscriptViewItemKind::ReasoningActivity { activity } => TranscriptItem::with_identity(
            reasoning_activity_title(activity.status),
            activity.text(),
            item.streaming,
            bcode_session_view_models::TextFormat::Markdown,
            TranscriptItemKind::ReasoningMessage,
        ),
        TranscriptViewItemKind::SystemMessage { message } => {
            let role = message
                .display_label
                .as_ref()
                .map_or("System", |_| "Plugin");
            message_text_item(role, message, item.streaming, TranscriptItemKind::System)
        }
        TranscriptViewItemKind::Compaction { compaction } => TranscriptItem::with_kind(
            "Compaction",
            compaction.text.clone(),
            item.streaming,
            TranscriptItemKind::Meta,
        ),
        TranscriptViewItemKind::Skill { skill } => terminal_skill_item_from_shared(skill),
        TranscriptViewItemKind::ToolInvocation { tool } => {
            terminal_tool_invocation_item_from_shared(tool)
        }
        TranscriptViewItemKind::ToolRequestDraft { draft } => TranscriptItem::with_kind(
            "Tool request draft",
            bounded_tool_request_draft_fallback(draft),
            true,
            TranscriptItemKind::ToolRequestDraft {
                draft: Box::new(draft.clone()),
            },
        ),
        TranscriptViewItemKind::ToolRequest { tool } => {
            terminal_tool_request_item_from_shared(tool)
        }
        TranscriptViewItemKind::Permission { permission } => {
            terminal_permission_item_from_shared(permission)
        }
        TranscriptViewItemKind::Interaction { interaction } => {
            terminal_interaction_item_from_shared(interaction)
        }
        TranscriptViewItemKind::ToolContribution {
            contribution,
            placement,
            invocation,
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
                    invocation: invocation.clone(),
                },
            )
        }
    };
    if let (Some(sequence), Some(timestamp_ms)) = (item.sequence, item.timestamp_ms) {
        terminal = terminal.with_event_metadata(sequence, timestamp_ms);
    }
    terminal.with_source_view_item(item.id.clone(), item.revision)
}

const fn reasoning_activity_title(
    status: bcode_session_models::ReasoningActivityStatus,
) -> &'static str {
    match status {
        bcode_session_models::ReasoningActivityStatus::Completed => "Reasoning",
        bcode_session_models::ReasoningActivityStatus::Interrupted => "Reasoning interrupted",
        bcode_session_models::ReasoningActivityStatus::Failed => "Reasoning failed",
    }
}

fn message_text_item(
    role: &'static str,
    message: &bcode_session_view_models::ChatMessageView,
    streaming: bool,
    kind: TranscriptItemKind,
) -> TranscriptItem {
    let item =
        TranscriptItem::with_identity(role, message.text.clone(), streaming, message.format, kind);
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
        ),
        tool,
    )
}

fn terminal_tool_item_from_shared(tool: &ToolInvocationView) -> TranscriptItem {
    if let Some(presentation) = &tool.presentation {
        let contribution = bcode_session_models::ToolContributionEvent {
            invocation_id: tool.tool_call_id.clone(),
            contribution_id: "primary".to_owned(),
            sequence: presentation.revision,
            producer_id: presentation.producer_id.clone(),
            schema: presentation.schema.clone(),
            schema_version: presentation.schema_version,
            operation: bcode_session_models::ToolContributionOperation::Upsert,
            persistence: match presentation.retention {
                bcode_tool::ToolPresentationRetention::RetainLatest => {
                    bcode_session_models::ToolContributionPersistence::Durable
                }
                bcode_tool::ToolPresentationRetention::ActiveOnly => {
                    bcode_session_models::ToolContributionPersistence::Transient
                }
            },
            artifact: presentation.artifact.clone(),
            payload: presentation.payload.clone(),
        };
        return TranscriptItem::with_kind(
            "Tool presentation",
            tool.tool_name
                .clone()
                .unwrap_or_else(|| "tool presentation".to_owned()),
            matches!(
                tool.status,
                ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
            ),
            TranscriptItemKind::ToolContribution {
                contribution: Box::new(contribution),
                placement: bcode_session_models::ToolContributionPlacement::Result,
                invocation: Some(Box::new(tool.clone())),
            },
        );
    }
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
    let mut item = tool_request_item(
        &tool.tool_call_id,
        tool.producer_plugin_id.as_deref(),
        tool.tool_name.as_deref().unwrap_or("unknown tool"),
        tool.arguments_json.as_deref().unwrap_or("{}"),
        tool.working_directory.clone(),
    );
    if let TranscriptItemKind::ToolRequest { status, .. } = &mut item.kind {
        *status = Some(tool.status);
    }
    item = apply_shared_tool_timing(item, tool);
    if matches!(
        tool.status,
        ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
    ) {
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

const fn tool_timing_from_view(tool: &ToolInvocationView) -> ToolTiming {
    ToolTiming {
        started_at_ms: tool.timing.started_at_ms,
        finished_at_ms: tool.timing.finished_at_ms,
        timeout_ms: tool.timing.timeout_ms,
        timed_out: tool.timing.timed_out,
        duration_ms: tool.timing.duration_ms,
    }
}

const fn apply_shared_tool_timing(
    mut item: TranscriptItem,
    tool: &ToolInvocationView,
) -> TranscriptItem {
    item.set_tool_active(matches!(
        tool.status,
        ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
    ));
    item.set_tool_started_at_ms(tool.timing.started_at_ms);
    item.set_tool_finished_at_ms(tool.timing.finished_at_ms);
    item.set_tool_timeout_ms(tool.timing.timeout_ms);
    item.set_tool_timed_out(tool.timing.timed_out);
    item.set_tool_duration_ms(tool.timing.duration_ms);
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

fn terminal_interaction_item_from_shared(interaction: &InteractionViewSummary) -> TranscriptItem {
    let state = if interaction.resolved {
        "resolved"
    } else if interaction.required {
        "response required"
    } else {
        "optional response pending"
    };
    let label = interaction.title.as_deref().unwrap_or(&interaction.kind);
    let text = if interaction.resolved {
        interaction.resolution.as_ref().map_or_else(
            || format!("{label} ({state})"),
            |resolution| {
                let resolution = serde_json::to_string_pretty(resolution)
                    .unwrap_or_else(|_| resolution.to_string());
                format!("{label} ({state})\n{resolution}")
            },
        )
    } else {
        format!("{label} ({state})\nAnswer in the active interaction panel.")
    };
    TranscriptItem::with_kind("Interaction", text, false, TranscriptItemKind::Generic)
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

#[cfg(test)]
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
    fn reconstructed_user_and_system_messages_preserve_formats() {
        let session_id = bcode_session_models::SessionId::new();
        let events = [
            SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "# User".to_owned(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            },
            SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 2,
                timestamp_ms: 2,
                session_id,
                provenance: None,
                kind: SessionEventKind::SystemMessage {
                    text: "* System".to_owned(),
                },
            },
        ];

        let items = transcript_items_from_events_with_reasoning(&events, false);
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|item| item.text_format() == TextFormat::Markdown)
        );
    }

    #[test]
    fn shared_message_projection_preserves_all_text_formats() {
        for format in [
            TextFormat::Markdown,
            TextFormat::PlainText,
            TextFormat::Json,
        ] {
            let item = TranscriptViewItem {
                output_location: None,
                id: bcode_session_view_models::TranscriptViewItemId::new("message"),
                revision: 0,
                sequence: Some(1),
                timestamp_ms: Some(1),
                streaming: false,
                kind: TranscriptViewItemKind::UserMessage {
                    message: bcode_session_view_models::ChatMessageView {
                        text: "* value".to_owned(),
                        display_label: None,
                        format,
                    },
                },
            };

            assert_eq!(terminal_item_from_shared(&item).text_format(), format);
        }
    }

    #[test]
    fn request_draft_fallback_is_bounded_and_never_exposes_preview_json() {
        let preview = "{\"secret\":\"value\"}";
        let draft = bcode_session_view_models::ToolRequestDraftView {
            output_location: None,
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call-1".to_owned(),
            tool_name: "x".repeat(1_000),
            producer_plugin_id: None,
            schema: "unknown.draft".to_owned(),
            schema_version: 1,
            placement: bcode_session_models::ToolContributionPlacement::Request,
            generation: 1,
            revision: 1,
            argument_bytes: preview.len(),
            preview_start_offset: 0,
            preview: preview.to_owned(),
            truncated: true,
        };

        let fallback = bounded_tool_request_draft_fallback(&draft);
        assert!(fallback.len() < 160);
        assert!(fallback.contains("request · 18 bytes · truncated"));
        assert!(!fallback.contains(preview));
        assert!(!fallback.contains("secret"));
    }

    #[test]
    fn replacing_shared_item_updates_text_format() {
        let source_id = bcode_session_view_models::TranscriptViewItemId::new("message");
        let item = |revision, format| TranscriptViewItem {
            output_location: None,
            id: source_id.clone(),
            revision,
            sequence: Some(1),
            timestamp_ms: Some(1),
            streaming: false,
            kind: TranscriptViewItemKind::UserMessage {
                message: bcode_session_view_models::ChatMessageView {
                    text: "* value".to_owned(),
                    display_label: None,
                    format,
                },
            },
        };
        let mut terminal = terminal_item_from_shared(&item(0, TextFormat::PlainText));

        assert!(
            terminal
                .replace_from_shared(terminal_item_from_shared(&item(1, TextFormat::Markdown,)))
        );
        assert_eq!(terminal.text_format(), TextFormat::Markdown);
    }

    #[test]
    fn generic_skill_and_interaction_items_remain_explicit_plain_text() {
        let skill = terminal_skill_item_from_shared(&bcode_session_view_models::SkillView {
            skill_id: "review".to_owned(),
            status: bcode_session_view_models::SkillViewStatus::ContextLoaded,
            text: "* literal context".to_owned(),
        });
        let interaction = terminal_interaction_item_from_shared(&InteractionViewSummary {
            interaction_id: "question-1".to_owned(),
            kind: "bcode.question".to_owned(),
            surface_kind: "bcode.question.inline".to_owned(),
            tool_call_id: None,
            title: Some("* literal title".to_owned()),
            required: true,
            snapshot: None,
            state: bcode_session_view_models::InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        });

        assert_eq!(skill.text_format(), TextFormat::PlainText);
        assert_eq!(interaction.text_format(), TextFormat::PlainText);
    }

    #[test]
    fn pending_interaction_summary_avoids_duplicate_raw_form_payload() {
        let interaction = InteractionViewSummary {
            interaction_id: "question-1".to_owned(),
            kind: "bcode.question".to_owned(),
            surface_kind: "bcode.question.inline".to_owned(),
            tool_call_id: Some("call-1".to_owned()),
            title: Some("Question".to_owned()),
            required: true,
            snapshot: Some(serde_json::json!({
                "questions": [{"question": "Secret raw form payload"}]
            })),
            state: bcode_session_view_models::InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        };

        let item = terminal_interaction_item_from_shared(&interaction);
        assert!(item.text().contains("response required"));
        assert!(item.text().contains("active interaction panel"));
        assert!(!item.text().contains("Secret raw form payload"));
    }

    #[test]
    fn resolved_interaction_summary_keeps_durable_outcome() {
        let interaction = InteractionViewSummary {
            interaction_id: "question-1".to_owned(),
            kind: "bcode.question".to_owned(),
            surface_kind: "bcode.question.inline".to_owned(),
            tool_call_id: Some("call-1".to_owned()),
            title: Some("Question".to_owned()),
            required: true,
            snapshot: None,
            state: bcode_session_view_models::InteractionViewState::Resolved,
            status_detail: None,
            resolved: true,
            resolution: Some(serde_json::json!({"status": "answered", "selected": ["yes"]})),
        };

        let item = terminal_interaction_item_from_shared(&interaction);
        assert!(item.text().contains("resolved"));
        assert!(item.text().contains("answered"));
        assert!(item.text().contains("yes"));
    }

    #[test]
    fn structured_reasoning_activity_adapts_terminal_lifecycle_chrome() {
        let sentinel = "encrypted-sentinel-do-not-expose";
        for (status, expected) in [
            (
                bcode_session_models::ReasoningActivityStatus::Completed,
                "Reasoning",
            ),
            (
                bcode_session_models::ReasoningActivityStatus::Interrupted,
                "Reasoning interrupted",
            ),
            (
                bcode_session_models::ReasoningActivityStatus::Failed,
                "Reasoning failed",
            ),
        ] {
            let item = TranscriptViewItem {
                output_location: None,
                id: bcode_session_view_models::TranscriptViewItemId::new(format!(
                    "reasoning:{status:?}"
                )),
                revision: 0,
                sequence: Some(1),
                timestamp_ms: None,
                streaming: false,
                kind: TranscriptViewItemKind::ReasoningActivity {
                    activity: bcode_session_view_models::ReasoningActivityView {
                        turn_id: "turn-1".to_owned(),
                        activity_id: "reasoning-1".to_owned(),
                        order: 0,
                        status,
                        parts: Vec::new(),
                        opaque: true,
                    },
                },
            };

            let terminal = terminal_item_from_shared(&item);
            assert_eq!(terminal.role(), expected);
            assert!(terminal.text().is_empty());
            assert!(!format!("{terminal:?}").contains(sentinel));
        }
    }

    #[test]
    fn shared_generic_items_adapt_without_renderer_types_crossing_the_boundary() {
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
        ];

        for (kind, role, expected_kind) in cases {
            let shared = TranscriptViewItem {
                output_location: None,
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
    fn working_directory_change_projects_as_markdown() {
        let session_id = bcode_session_models::SessionId::new();
        let events = [SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::WorkingDirectoryChanged {
                old_working_directory: std::path::PathBuf::from("/tmp/old"),
                new_working_directory: std::path::PathBuf::from("/tmp/new"),
            },
        }];

        let items = transcript_items_from_events_with_reasoning(&events, false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text_format(), TextFormat::Markdown);
        assert!(items[0].text().contains("`../new`"));
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
        assert_eq!(items[0].text_format(), TextFormat::PlainText);
    }

    #[test]
    fn transcript_item_display_label_is_generic() {
        let item = TranscriptItem::new("You", "text".to_owned())
            .with_display_label("Plugin operation".to_owned());
        assert_eq!(item.display_role(), "You · Plugin operation");
    }
}
