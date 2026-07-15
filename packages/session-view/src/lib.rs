#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral session view projection for Bcode.
//!
//! This crate owns the application of durable and live session events into semantic view state that
//! terminal, web, and future renderers can consume without inheriting terminal layout concerns.

mod actions;

pub use actions::execute_session_view_action;

use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionId, SessionLiveEvent, SessionLiveEventKind,
    ToolInvocationProjection, ToolInvocationStreamEvent, apply_tool_invocation_projection_event,
};
use bcode_session_view_models::{
    ChatMessageView, ComposerViewState, PluginVisualView, SessionViewSnapshot, TextFormat,
    ThinkingViewState, ToolInvocationView, ToolInvocationViewStatus, ToolOutputView,
    ToolResultView, ToolTimingView, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};
use std::collections::{BTreeMap, btree_map::Entry};

/// Renderer-neutral session view projection.
#[derive(Debug, Clone)]
pub struct SessionView {
    snapshot: SessionViewSnapshot,
    next_item_id: u64,
    tool_item_ids: BTreeMap<String, TranscriptViewItemId>,
    tool_invocation_projections: BTreeMap<String, ToolInvocationProjection>,
}

impl Default for SessionView {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionView {
    /// Create an empty session view.
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshot: SessionViewSnapshot::empty(),
            next_item_id: 1,
            tool_item_ids: BTreeMap::new(),
            tool_invocation_projections: BTreeMap::new(),
        }
    }

    /// Return the current snapshot.
    #[must_use]
    pub const fn snapshot(&self) -> &SessionViewSnapshot {
        &self.snapshot
    }

    /// Consume this view and return the current snapshot.
    #[must_use]
    pub fn into_snapshot(self) -> SessionViewSnapshot {
        self.snapshot
    }

    /// Replace composer draft state.
    pub fn set_composer(&mut self, composer: ComposerViewState) {
        if self.snapshot.composer != composer {
            self.snapshot.composer = composer;
            self.bump_revision();
        }
    }

    /// Apply replayed history events in chronological order.
    pub fn apply_history(&mut self, events: &[SessionEvent]) {
        for event in events {
            self.apply_event(event);
        }
    }

    /// Apply one durable session event.
    #[allow(clippy::too_many_lines)]
    pub fn apply_event(&mut self, event: &SessionEvent) {
        self.snapshot.session_id = Some(event.session_id);
        self.snapshot.latest_sequence = Some(event.sequence);
        apply_tool_invocation_projection_event(&mut self.tool_invocation_projections, event);

        match &event.kind {
            SessionEventKind::SessionCreated {
                name,
                working_directory,
            } => {
                self.snapshot.title.clone_from(name);
                self.snapshot.working_directory = Some(working_directory.clone());
                self.bump_revision();
            }
            SessionEventKind::UserMessage { text, .. } => {
                if self.snapshot.title.is_none() {
                    self.snapshot.title = Some(derive_session_title_from_prompt(text));
                }
                self.push_item(
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::UserMessage {
                        message: ChatMessageView::markdown(text.clone()),
                    },
                );
            }
            SessionEventKind::AssistantDelta { text } => {
                self.push_or_append_streaming_message(
                    event.sequence,
                    Some(event.timestamp_ms),
                    StreamingMessageKind::Assistant,
                    text,
                );
            }
            SessionEventKind::AssistantMessage { text } => {
                self.finish_or_push_message(
                    event.sequence,
                    Some(event.timestamp_ms),
                    StreamingMessageKind::Assistant,
                    text,
                );
            }
            SessionEventKind::AssistantReasoningDelta { text } => {
                self.snapshot.thinking = ThinkingViewState {
                    visible: true,
                    active_text: Some(text.clone()),
                    streaming: true,
                };
                self.push_or_append_streaming_message(
                    event.sequence,
                    Some(event.timestamp_ms),
                    StreamingMessageKind::Reasoning,
                    text,
                );
            }
            SessionEventKind::AssistantReasoningMessage { text } => {
                self.snapshot.thinking = ThinkingViewState {
                    visible: true,
                    active_text: Some(text.clone()),
                    streaming: false,
                };
                self.finish_or_push_message(
                    event.sequence,
                    Some(event.timestamp_ms),
                    StreamingMessageKind::Reasoning,
                    text,
                );
            }
            SessionEventKind::ToolCallRequested { tool_call_id, .. }
            | SessionEventKind::ToolCallFinished { tool_call_id, .. } => {
                self.upsert_tool_item(tool_call_id, event.sequence, Some(event.timestamp_ms));
            }
            SessionEventKind::ToolInvocationStream { event: stream } => {
                let tool_call_id = stream_tool_call_id(stream);
                self.upsert_tool_item(tool_call_id, event.sequence, Some(event.timestamp_ms));
                if let ToolInvocationStreamEvent::VisualUpdate {
                    visual, streaming, ..
                } = stream
                {
                    self.push_item(
                        event.sequence,
                        Some(event.timestamp_ms),
                        *streaming,
                        TranscriptViewItemKind::PluginVisual {
                            visual: PluginVisualView::from(visual.clone()),
                        },
                    );
                }
            }
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                policy_reason,
                ..
            } => {
                let permission = bcode_session_view_models::PermissionView {
                    permission_id: permission_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    title: Some(format!("Permission requested: {tool_name}")),
                    detail: policy_reason.clone(),
                    resolved: false,
                    approved: None,
                    can_remember: true,
                };
                upsert_by(
                    &mut self.snapshot.permissions,
                    permission.clone(),
                    |permission| permission.permission_id.as_str(),
                );
                self.push_item(
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::Permission { permission },
                );
            }
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
                ..
            } => {
                if let Some(permission) = self
                    .snapshot
                    .permissions
                    .iter_mut()
                    .find(|permission| permission.permission_id == *permission_id)
                {
                    permission.resolved = true;
                    permission.approved = Some(*approved);
                    self.bump_revision();
                }
            }
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                label,
                started_at_ms,
                ..
            } => {
                self.upsert_runtime_work(bcode_session_view_models::RuntimeWorkView {
                    work_id: work_id.clone(),
                    status: bcode_session_models::RuntimeWorkStatus::Running,
                    message: Some(label.clone()),
                    completed_units: None,
                    total_units: None,
                    updated_at_ms: *started_at_ms,
                });
            }
            SessionEventKind::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            } => {
                self.upsert_runtime_work(bcode_session_view_models::RuntimeWorkView {
                    work_id: work_id.clone(),
                    status: self
                        .snapshot
                        .runtime_work
                        .iter()
                        .find(|work| work.work_id == *work_id)
                        .map_or(bcode_session_models::RuntimeWorkStatus::Running, |work| {
                            work.status
                        }),
                    message: Some(message.clone()),
                    completed_units: *completed_units,
                    total_units: *total_units,
                    updated_at_ms: *progress_at_ms,
                });
            }
            SessionEventKind::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                ..
            } => {
                self.upsert_runtime_work(bcode_session_view_models::RuntimeWorkView {
                    work_id: work_id.clone(),
                    status: bcode_session_models::RuntimeWorkStatus::Cancelling,
                    message: Some("Cancellation requested".to_owned()),
                    completed_units: None,
                    total_units: None,
                    updated_at_ms: *requested_at_ms,
                });
            }
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                message,
                finished_at_ms,
                ..
            } => {
                self.upsert_runtime_work(bcode_session_view_models::RuntimeWorkView {
                    work_id: work_id.clone(),
                    status: *status,
                    message: message.clone(),
                    completed_units: None,
                    total_units: None,
                    updated_at_ms: *finished_at_ms,
                });
            }
            SessionEventKind::WorkingDirectoryChanged {
                new_working_directory,
                ..
            } => {
                self.snapshot.working_directory = Some(new_working_directory.clone());
                self.bump_revision();
            }
            SessionEventKind::SessionRenamed { name } => {
                self.snapshot.title.clone_from(name);
                self.bump_revision();
            }
            _ => {}
        }
    }

    /// Apply one live-only session event.
    pub fn apply_live_event(&mut self, event: &SessionLiveEvent) {
        self.snapshot.session_id = Some(event.session_id);
        match &event.kind {
            SessionLiveEventKind::AssistantTextDelta { text, .. } => {
                self.push_or_append_streaming_message(
                    0,
                    None,
                    StreamingMessageKind::Assistant,
                    text,
                );
            }
            SessionLiveEventKind::AssistantReasoningDelta { text, .. } => {
                self.snapshot.thinking = ThinkingViewState {
                    visible: true,
                    active_text: Some(text.clone()),
                    streaming: true,
                };
                self.push_or_append_streaming_message(
                    0,
                    None,
                    StreamingMessageKind::Reasoning,
                    text,
                );
            }
            SessionLiveEventKind::ToolOutputDelta { event: stream } => {
                let synthetic = SessionEvent {
                    schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    sequence: 0,
                    timestamp_ms: bcode_session_models::current_unix_timestamp_ms(),
                    session_id: event.session_id,
                    provenance: None,
                    kind: SessionEventKind::ToolInvocationStream {
                        event: stream.clone(),
                    },
                };
                self.apply_event(&synthetic);
            }
            SessionLiveEventKind::ToolArgumentPreview {
                tool_call_id,
                tool_name,
                preview,
                ..
            } => {
                self.push_item(
                    0,
                    None,
                    true,
                    TranscriptViewItemKind::PluginVisual {
                        visual: PluginVisualView::from(preview.visual.clone()),
                    },
                );
                self.tool_invocation_projections
                    .entry(tool_call_id.clone())
                    .or_insert_with(|| ToolInvocationProjection {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: Some(tool_name.clone()),
                        request_visual: Some(preview.visual.clone()),
                        ..ToolInvocationProjection::default()
                    });
                self.upsert_tool_item(tool_call_id, 0, None);
            }
            SessionLiveEventKind::ProviderStreamProgress { .. }
            | SessionLiveEventKind::ContextOccupancyChanged { .. } => {}
        }
    }

    const fn bump_revision(&mut self) {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
    }

    const fn next_transcript_item_id(&mut self) -> TranscriptViewItemId {
        let id = TranscriptViewItemId(self.next_item_id);
        self.next_item_id = self.next_item_id.saturating_add(1);
        id
    }

    fn push_item(
        &mut self,
        sequence: u64,
        timestamp_ms: Option<u64>,
        streaming: bool,
        kind: TranscriptViewItemKind,
    ) -> TranscriptViewItemId {
        let id = self.next_transcript_item_id();
        self.snapshot.transcript.items.push(TranscriptViewItem {
            id,
            revision: 0,
            sequence: (sequence != 0).then_some(sequence),
            timestamp_ms,
            streaming,
            kind,
        });
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        self.bump_revision();
        id
    }

    fn upsert_tool_item(&mut self, tool_call_id: &str, sequence: u64, timestamp_ms: Option<u64>) {
        let Some(projection) = self.tool_invocation_projections.get(tool_call_id).cloned() else {
            return;
        };
        let tool = tool_invocation_view_from_projection(projection);
        self.snapshot
            .tools
            .insert(tool_call_id.to_owned(), tool.clone());
        match self.tool_item_ids.entry(tool_call_id.to_owned()) {
            Entry::Occupied(entry) => {
                let id = *entry.get();
                if let Some(item) = self
                    .snapshot
                    .transcript
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                {
                    item.kind = TranscriptViewItemKind::ToolInvocation {
                        tool: Box::new(tool),
                    };
                    item.streaming = matches!(
                        self.snapshot.tools[tool_call_id].status,
                        ToolInvocationViewStatus::Running
                    );
                    item.revision = item.revision.saturating_add(1);
                    self.snapshot.transcript.revision =
                        self.snapshot.transcript.revision.saturating_add(1);
                    self.bump_revision();
                }
            }
            Entry::Vacant(_) => {
                let id = self.push_item(
                    sequence,
                    timestamp_ms,
                    matches!(tool.status, ToolInvocationViewStatus::Running),
                    TranscriptViewItemKind::ToolInvocation {
                        tool: Box::new(tool),
                    },
                );
                self.tool_item_ids.insert(tool_call_id.to_owned(), id);
            }
        }
    }

    fn upsert_runtime_work(&mut self, work: bcode_session_view_models::RuntimeWorkView) {
        if let Some(existing) = self
            .snapshot
            .runtime_work
            .iter_mut()
            .find(|existing| existing.work_id == work.work_id)
        {
            *existing = work;
        } else {
            self.snapshot.runtime_work.push(work.clone());
            self.push_item(
                0,
                work.updated_at_ms,
                false,
                TranscriptViewItemKind::RuntimeWork { work },
            );
            return;
        }
        self.bump_revision();
    }

    fn push_or_append_streaming_message(
        &mut self,
        sequence: u64,
        timestamp_ms: Option<u64>,
        kind: StreamingMessageKind,
        text: &str,
    ) {
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .last_mut()
            .filter(|item| item.streaming && streaming_item_matches(&item.kind, kind))
        {
            append_text_to_item(item, text);
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
            return;
        }
        self.push_item(
            sequence,
            timestamp_ms,
            true,
            kind.item_kind(text.to_owned()),
        );
    }

    fn finish_or_push_message(
        &mut self,
        sequence: u64,
        timestamp_ms: Option<u64>,
        kind: StreamingMessageKind,
        text: &str,
    ) {
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .last_mut()
            .filter(|item| item.streaming && streaming_item_matches(&item.kind, kind))
        {
            replace_text_in_item(item, text);
            item.streaming = false;
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
            return;
        }
        self.push_item(
            sequence,
            timestamp_ms,
            false,
            kind.item_kind(text.to_owned()),
        );
    }
}

fn tool_invocation_view_from_projection(
    projection: ToolInvocationProjection,
) -> ToolInvocationView {
    ToolInvocationView {
        tool_call_id: projection.tool_call_id,
        producer_plugin_id: projection.producer_plugin_id,
        tool_name: projection.tool_name,
        arguments_json: projection.arguments_json,
        request_visual: projection.request_visual.map(PluginVisualView::from),
        status: projection.status.into(),
        result_text: projection.result_text,
        is_error: projection.is_error,
        result: projection.raw_result.map(ToolResultView::from),
        output: projection.stream_output.map(|output| ToolOutputView {
            text: output.output,
            columns: output.columns,
            rows: output.rows,
        }),
        timing: ToolTimingView {
            started_at_ms: projection.started_at_ms,
            finished_at_ms: projection.finished_at_ms,
            timeout_ms: None,
            timed_out: None,
            duration_ms: None,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingMessageKind {
    Assistant,
    Reasoning,
}

impl StreamingMessageKind {
    const fn item_kind(self, text: String) -> TranscriptViewItemKind {
        let message = ChatMessageView {
            text,
            format: TextFormat::Markdown,
        };
        match self {
            Self::Assistant => TranscriptViewItemKind::AssistantMessage { message },
            Self::Reasoning => TranscriptViewItemKind::ReasoningMessage { message },
        }
    }
}

const fn streaming_item_matches(
    kind: &TranscriptViewItemKind,
    streaming_kind: StreamingMessageKind,
) -> bool {
    matches!(
        (kind, streaming_kind),
        (
            TranscriptViewItemKind::AssistantMessage { .. },
            StreamingMessageKind::Assistant
        ) | (
            TranscriptViewItemKind::ReasoningMessage { .. },
            StreamingMessageKind::Reasoning
        )
    )
}

fn append_text_to_item(item: &mut TranscriptViewItem, text: &str) {
    match &mut item.kind {
        TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => message.text.push_str(text),
        TranscriptViewItemKind::ToolInvocation { .. }
        | TranscriptViewItemKind::Permission { .. }
        | TranscriptViewItemKind::RuntimeWork { .. }
        | TranscriptViewItemKind::Interaction { .. }
        | TranscriptViewItemKind::PluginVisual { .. } => {}
    }
}

fn replace_text_in_item(item: &mut TranscriptViewItem, text: &str) {
    match &mut item.kind {
        TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => text.clone_into(&mut message.text),
        TranscriptViewItemKind::ToolInvocation { .. }
        | TranscriptViewItemKind::Permission { .. }
        | TranscriptViewItemKind::RuntimeWork { .. }
        | TranscriptViewItemKind::Interaction { .. }
        | TranscriptViewItemKind::PluginVisual { .. } => {}
    }
}

fn stream_tool_call_id(event: &ToolInvocationStreamEvent) -> &str {
    match event {
        ToolInvocationStreamEvent::Started { tool_call_id, .. }
        | ToolInvocationStreamEvent::OutputDelta { tool_call_id, .. }
        | ToolInvocationStreamEvent::VisualUpdate { tool_call_id, .. }
        | ToolInvocationStreamEvent::ArtifactUpdate { tool_call_id, .. }
        | ToolInvocationStreamEvent::Status { tool_call_id, .. }
        | ToolInvocationStreamEvent::LegacyPresentation { tool_call_id, .. }
        | ToolInvocationStreamEvent::Finished { tool_call_id, .. } => tool_call_id,
    }
}

fn upsert_by<T>(items: &mut Vec<T>, value: T, key: impl Fn(&T) -> &str) {
    let value_key = key(&value).to_owned();
    if let Some(existing) = items.iter_mut().find(|item| key(item) == value_key) {
        *existing = value;
    } else {
        items.push(value);
    }
}

fn derive_session_title_from_prompt(prompt: &str) -> String {
    let title = prompt
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        "Untitled session".to_owned()
    } else {
        title
    }
}

/// Build a session view snapshot from chronological durable events.
#[must_use]
pub fn build_session_view_snapshot(events: &[SessionEvent]) -> SessionViewSnapshot {
    let mut view = SessionView::new();
    view.apply_history(events);
    view.into_snapshot()
}

/// Build a session view snapshot from chronological durable events for a specific session id.
#[must_use]
pub fn build_session_view_snapshot_for(
    session_id: SessionId,
    events: &[SessionEvent],
) -> SessionViewSnapshot {
    let mut view = SessionView::new();
    view.snapshot.session_id = Some(session_id);
    view.apply_history(events);
    view.into_snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionId,
        ToolOutputStream,
    };
    use std::path::PathBuf;

    fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence * 10,
            session_id,
            provenance: None,
            kind,
        }
    }

    #[test]
    fn projects_user_and_assistant_messages() {
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[
            event(
                session_id,
                1,
                SessionEventKind::SessionCreated {
                    name: None,
                    working_directory: PathBuf::from("/tmp/project"),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "Explain renderer neutrality".to_owned(),
                    origin: None,
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::AssistantDelta {
                    text: "It ".to_owned(),
                },
            ),
            event(
                session_id,
                4,
                SessionEventKind::AssistantDelta {
                    text: "means".to_owned(),
                },
            ),
            event(
                session_id,
                5,
                SessionEventKind::AssistantMessage {
                    text: "It means shared semantic state.".to_owned(),
                },
            ),
        ]);

        assert_eq!(snapshot.session_id, Some(session_id));
        assert_eq!(
            snapshot.working_directory,
            Some(PathBuf::from("/tmp/project"))
        );
        assert_eq!(snapshot.transcript.items.len(), 2);
        assert!(!snapshot.transcript.items[1].streaming);
        match &snapshot.transcript.items[1].kind {
            TranscriptViewItemKind::AssistantMessage { message } => {
                assert_eq!(message.text, "It means shared semantic state.");
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn projects_tool_invocation_output() {
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[
            event(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "tool-1".to_owned(),
                    producer_plugin_id: Some("shell".to_owned()),
                    tool_name: "shell.run".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                    request_visual: None,
                    legacy_request_presentation: None,
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::Started {
                        tool_call_id: "tool-1".to_owned(),
                        tool_name: "shell.run".to_owned(),
                        sequence: 1,
                        terminal: true,
                        columns: Some(80),
                        rows: Some(24),
                        started_at_ms: Some(20),
                    },
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::OutputDelta {
                        tool_call_id: "tool-1".to_owned(),
                        stream: ToolOutputStream::Stdout,
                        sequence: 2,
                        text: "hello".to_owned(),
                        byte_len: 5,
                    },
                },
            ),
        ]);

        let tool = snapshot.tools.get("tool-1").expect("tool projected");
        assert_eq!(tool.tool_name.as_deref(), Some("shell.run"));
        assert_eq!(tool.status, ToolInvocationViewStatus::Running);
        assert_eq!(
            tool.output.as_ref().map(|output| output.text.as_str()),
            Some("hello")
        );
        assert_eq!(snapshot.transcript.items.len(), 1);
    }
}
