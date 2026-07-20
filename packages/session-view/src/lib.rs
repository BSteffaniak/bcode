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
    InteractiveToolRenderTarget, InteractiveToolTurnBehavior, SessionEvent, SessionEventKind,
    SessionId, SessionLiveEvent, SessionLiveEventKind, ToolInvocationProjection,
    ToolInvocationStreamEvent, apply_tool_invocation_projection_event,
};
use bcode_session_view_models::{
    ChatMessageView, ComposerViewState, InteractionViewSummary, PluginVisualView,
    SessionViewSnapshot, TextFormat, ThinkingViewState, ToolInvocationView,
    ToolInvocationViewStatus, ToolOutputView, ToolResultView, ToolTimingView, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};
use std::collections::{BTreeMap, btree_map::Entry};

/// Renderer-neutral session view projection.
#[derive(Debug, Clone)]
pub struct SessionView {
    snapshot: SessionViewSnapshot,
    next_item_id: u64,
    tool_item_ids: BTreeMap<String, TranscriptViewItemId>,
    interaction_item_ids: BTreeMap<String, TranscriptViewItemId>,
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
            interaction_item_ids: BTreeMap::new(),
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

    /// Replace attached runtime selections supplied by the daemon.
    pub fn set_runtime_selection(
        &mut self,
        provider_plugin_id: Option<String>,
        requested_model_id: Option<String>,
        effective_model_id: Option<String>,
        reasoning_effort: Option<String>,
        reasoning_summary: Option<String>,
        context_occupancy: Option<bcode_session_models::RequestContextOccupancy>,
    ) {
        let runtime = &mut self.snapshot.runtime;
        let changed = runtime.provider_plugin_id != provider_plugin_id
            || runtime.requested_model_id != requested_model_id
            || runtime.effective_model_id != effective_model_id
            || runtime.reasoning_effort != reasoning_effort
            || runtime.reasoning_summary != reasoning_summary
            || runtime.context_occupancy != context_occupancy;
        if !changed {
            return;
        }
        runtime.provider_plugin_id = provider_plugin_id;
        runtime.requested_model_id = requested_model_id;
        runtime.effective_model_id = effective_model_id;
        runtime.reasoning_effort = reasoning_effort;
        runtime.reasoning_summary = reasoning_summary;
        runtime.context_occupancy = context_occupancy;
        self.bump_revision();
    }

    /// Insert or replace an authoritative pending permission hydrated from the daemon.
    pub fn upsert_permission(&mut self, permission: bcode_session_view_models::PermissionView) {
        let existing = self
            .snapshot
            .permissions
            .iter_mut()
            .find(|existing| existing.permission_id == permission.permission_id);
        if let Some(existing) = existing {
            if *existing != permission {
                *existing = permission;
                self.bump_revision();
            }
        } else {
            self.snapshot.permissions.push(permission);
            self.bump_revision();
        }
    }

    /// Insert or replace renderer-neutral interaction state hydrated from the daemon.
    pub fn upsert_interaction(&mut self, interaction: InteractionViewSummary) {
        self.upsert_interaction_item(interaction, 0, None);
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
            SessionEventKind::SystemMessage { text } => {
                self.push_item(
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::markdown(text.clone()),
                    },
                );
            }
            SessionEventKind::ModelChanged { provider, model } => {
                self.snapshot.runtime.provider_plugin_id = Some(provider.clone());
                self.snapshot.runtime.requested_model_id = Some(model.clone());
                self.snapshot.runtime.effective_model_id = Some(model.clone());
                self.bump_revision();
            }
            SessionEventKind::AgentChanged { agent_id } => {
                self.snapshot.runtime.agent_id = Some(agent_id.clone());
                self.bump_revision();
            }
            SessionEventKind::ReasoningChanged { effort, summary } => {
                self.snapshot.runtime.reasoning_effort.clone_from(effort);
                self.snapshot.runtime.reasoning_summary.clone_from(summary);
                self.bump_revision();
            }
            SessionEventKind::ModelTurnStarted { turn_id } => {
                self.snapshot.runtime.active_turn_id = Some(turn_id.clone());
                self.snapshot.runtime.cancelling = false;
                self.bump_revision();
            }
            SessionEventKind::ModelTurnCancelRequested { turn_id, .. } => {
                self.snapshot.runtime.active_turn_id = Some(turn_id.clone());
                self.snapshot.runtime.cancelling = true;
                self.bump_revision();
            }
            SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            } => {
                if self.snapshot.runtime.active_turn_id.as_deref() == Some(turn_id) {
                    self.snapshot.runtime.active_turn_id = None;
                }
                self.snapshot.runtime.cancelling = false;
                self.snapshot.runtime.last_turn_outcome = Some(*outcome);
                self.snapshot.runtime.last_turn_message.clone_from(message);
                if *outcome == bcode_session_models::ModelTurnOutcome::Error {
                    self.push_item(
                        event.sequence,
                        Some(event.timestamp_ms),
                        false,
                        TranscriptViewItemKind::SystemMessage {
                            message: ChatMessageView::plain(format!(
                                "Model turn failed: {}",
                                message.as_deref().unwrap_or("no details recorded")
                            )),
                        },
                    );
                } else {
                    self.bump_revision();
                }
            }
            SessionEventKind::ModelUsage { usage, .. } => {
                self.snapshot.runtime.latest_usage = Some(usage.clone());
                self.bump_revision();
            }
            SessionEventKind::ContextCompacted { summary, .. } => {
                self.push_item(
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::markdown(format!(
                            "Context compacted\n\n{summary}"
                        )),
                    },
                );
            }
            SessionEventKind::ProviderContextCompacted { snapshot, .. } => {
                self.snapshot.runtime.context_occupancy = None;
                self.push_item(
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::plain(format!(
                            "Provider context compacted for {} / {}",
                            snapshot.provider_plugin_id, snapshot.model_id
                        )),
                    },
                );
            }
            SessionEventKind::RequestContextObserved { observation } => {
                self.snapshot.runtime.context_occupancy =
                    Some(bcode_session_models::RequestContextOccupancy {
                        context_epoch: observation.request.context_epoch,
                        observation_sequence: event.sequence,
                        observation: observation.clone(),
                    });
                self.bump_revision();
            }
            SessionEventKind::InteractiveToolRequestCreated {
                interaction_id,
                tool_call_id,
                tool_name,
                interaction_kind,
                surface_kind,
                request_json,
                required,
                turn_behavior,
                render_target,
                ..
            } => {
                self.upsert_interaction_item(
                    InteractionViewSummary {
                        interaction_id: interaction_id.clone(),
                        kind: interaction_kind
                            .clone()
                            .unwrap_or_else(|| surface_kind.clone()),
                        tool_call_id: Some(tool_call_id.clone()),
                        title: Some(tool_name.clone()),
                        required: *required,
                        snapshot: parse_json_value(request_json),
                        resolved: false,
                        resolution: None,
                        render_target: *render_target,
                        turn_behavior: *turn_behavior,
                    },
                    event.sequence,
                    Some(event.timestamp_ms),
                );
            }
            SessionEventKind::InteractiveToolRequestResolved {
                interaction_id,
                tool_call_id,
                resolution_json,
            } => {
                let resolution = parse_json_value(resolution_json)
                    .unwrap_or_else(|| serde_json::Value::String(resolution_json.clone()));
                let existing = self
                    .snapshot
                    .interactions
                    .iter()
                    .find(|interaction| interaction.interaction_id == *interaction_id)
                    .cloned();
                let interaction = if let Some(mut interaction) = existing {
                    interaction.resolved = true;
                    interaction.resolution = Some(resolution);
                    interaction
                } else {
                    InteractionViewSummary {
                        interaction_id: interaction_id.clone(),
                        kind: "unknown".to_owned(),
                        tool_call_id: Some(tool_call_id.clone()),
                        title: None,
                        required: false,
                        snapshot: None,
                        resolved: true,
                        resolution: Some(resolution),
                        render_target: InteractiveToolRenderTarget::default(),
                        turn_behavior: InteractiveToolTurnBehavior::default(),
                    }
                };
                self.upsert_interaction_item(interaction, event.sequence, Some(event.timestamp_ms));
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
                    let permission = permission.clone();
                    if let Some(item) = self.snapshot.transcript.items.iter_mut().find(|item| {
                        matches!(
                            &item.kind,
                            TranscriptViewItemKind::Permission { permission: existing }
                                if existing.permission_id == *permission_id
                        )
                    }) {
                        item.kind = TranscriptViewItemKind::Permission { permission };
                        item.revision = item.revision.saturating_add(1);
                        self.snapshot.transcript.revision =
                            self.snapshot.transcript.revision.saturating_add(1);
                    }
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
            SessionLiveEventKind::ProviderStreamProgress { .. } => {}
            SessionLiveEventKind::RequestContextOccupancyChanged { occupancy } => {
                self.snapshot
                    .runtime
                    .context_occupancy
                    .clone_from(occupancy.as_ref());
                self.bump_revision();
            }
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

    fn upsert_interaction_item(
        &mut self,
        interaction: InteractionViewSummary,
        sequence: u64,
        timestamp_ms: Option<u64>,
    ) {
        if let Some(existing) = self
            .snapshot
            .interactions
            .iter_mut()
            .find(|existing| existing.interaction_id == interaction.interaction_id)
        {
            if *existing == interaction {
                return;
            }
            *existing = interaction.clone();
            self.update_interaction_transcript_item(&interaction);
            self.bump_revision();
            return;
        }
        self.snapshot.interactions.push(interaction.clone());
        let id = self.push_item(
            sequence,
            timestamp_ms,
            false,
            TranscriptViewItemKind::Interaction {
                interaction: interaction.clone(),
            },
        );
        self.interaction_item_ids
            .insert(interaction.interaction_id, id);
    }

    fn update_interaction_transcript_item(&mut self, interaction: &InteractionViewSummary) {
        let Some(id) = self
            .interaction_item_ids
            .get(&interaction.interaction_id)
            .copied()
        else {
            return;
        };
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| item.id == id)
        {
            item.kind = TranscriptViewItemKind::Interaction {
                interaction: interaction.clone(),
            };
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        }
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
            .iter_mut()
            .rev()
            .find(|item| item.streaming && streaming_item_matches(&item.kind, kind))
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
            .iter_mut()
            .rev()
            .find(|item| item.streaming && streaming_item_matches(&item.kind, kind))
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

fn parse_json_value(value: &str) -> Option<serde_json::Value> {
    serde_json::from_str(value).ok()
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
        | ToolInvocationStreamEvent::LegacyTransientPruned { tool_call_id, .. }
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
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, InteractiveToolRenderTarget,
        InteractiveToolTurnBehavior, LocalContextEstimate, ModelRequestIdentity,
        RequestContextObservation, RequestContextTokenCount, SessionEvent, SessionEventKind,
        SessionId, SessionLiveEvent, SessionLiveEventKind, SessionTokenUsage, ToolOutputStream,
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
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
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
    fn projects_interaction_lifecycle_and_keeps_transcript_in_sync() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::InteractiveToolRequestCreated {
                interaction_id: "interaction-1".to_owned(),
                tool_call_id: "tool-1".to_owned(),
                tool_name: "question".to_owned(),
                interaction_kind: Some("bcode.question".to_owned()),
                surface_kind: "bcode.question.inline".to_owned(),
                request_json: r#"{"questions":[]}"#.to_owned(),
                required: true,
                turn_behavior: InteractiveToolTurnBehavior::AwaitBeforeContinuing,
                render_target: InteractiveToolRenderTarget::TranscriptToolCall,
            },
        ));

        assert_eq!(view.snapshot().interactions.len(), 1);
        assert_eq!(view.snapshot().interactions[0].kind, "bcode.question");
        assert_eq!(
            view.snapshot().interactions[0].snapshot,
            Some(serde_json::json!({"questions": []}))
        );
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::Interaction { interaction } if !interaction.resolved
        ));

        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::InteractiveToolRequestResolved {
                interaction_id: "interaction-1".to_owned(),
                tool_call_id: "tool-1".to_owned(),
                resolution_json: r#"{"type":"submitted"}"#.to_owned(),
            },
        ));

        assert!(view.snapshot().interactions[0].resolved);
        assert_eq!(
            view.snapshot().interactions[0].resolution,
            Some(serde_json::json!({"type": "submitted"}))
        );
        assert_eq!(view.snapshot().transcript.items.len(), 1);
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::Interaction { interaction } if interaction.resolved
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn projects_runtime_selection_turn_usage_context_and_system_state() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.set_runtime_selection(
            Some("provider".to_owned()),
            Some("requested-model".to_owned()),
            Some("effective-model".to_owned()),
            Some("high".to_owned()),
            Some("detailed".to_owned()),
            None,
        );
        view.apply_history(&[
            event(
                session_id,
                1,
                SessionEventKind::AgentChanged {
                    agent_id: "build".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::ModelTurnStarted {
                    turn_id: "turn-1".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::ModelUsage {
                    turn_id: "turn-1".to_owned(),
                    usage: SessionTokenUsage {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        total_tokens: Some(15),
                        ..SessionTokenUsage::default()
                    },
                },
            ),
            event(
                session_id,
                4,
                SessionEventKind::RequestContextObserved {
                    observation: RequestContextObservation {
                        request: ModelRequestIdentity {
                            provider_plugin_id: "provider".to_owned(),
                            requested_model_id: Some("requested-model".to_owned()),
                            effective_model_id: "effective-model".to_owned(),
                            request_id: "request-1".to_owned(),
                            model_turn_id: "turn-1".to_owned(),
                            round: 0,
                            request_fingerprint: "fingerprint".to_owned(),
                            effective_auth_profile: None,
                            context_format_version: None,
                            compatibility_key: None,
                            context_epoch: 2,
                        },
                        context_through_sequence: 3,
                        context_tokens: RequestContextTokenCount::ProviderExact(10),
                        local_estimate: LocalContextEstimate {
                            tokens: 9,
                            algorithm_version: 1,
                        },
                    },
                },
            ),
            event(
                session_id,
                5,
                SessionEventKind::SystemMessage {
                    text: "status".to_owned(),
                },
            ),
            event(
                session_id,
                6,
                SessionEventKind::ModelTurnFinished {
                    turn_id: "turn-1".to_owned(),
                    outcome: bcode_session_models::ModelTurnOutcome::Completed,
                    message: None,
                },
            ),
        ]);

        let runtime = &view.snapshot().runtime;
        assert_eq!(runtime.provider_plugin_id.as_deref(), Some("provider"));
        assert_eq!(
            runtime.requested_model_id.as_deref(),
            Some("requested-model")
        );
        assert_eq!(
            runtime.effective_model_id.as_deref(),
            Some("effective-model")
        );
        assert_eq!(runtime.agent_id.as_deref(), Some("build"));
        assert_eq!(runtime.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            runtime
                .latest_usage
                .as_ref()
                .and_then(|usage| usage.total_tokens),
            Some(15)
        );
        assert_eq!(
            runtime
                .context_occupancy
                .as_ref()
                .map(|occupancy| occupancy.context_epoch),
            Some(2)
        );
        assert_eq!(runtime.active_turn_id, None);
        assert_eq!(
            runtime.last_turn_outcome,
            Some(bcode_session_models::ModelTurnOutcome::Completed)
        );
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::SystemMessage { message } if message.text == "status"
        ));
    }

    #[test]
    fn permission_resolution_updates_collection_and_transcript() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::PermissionRequested {
                permission_id: "permission-1".to_owned(),
                tool_call_id: "tool-1".to_owned(),
                producer_plugin_id: Some("shell".to_owned()),
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
                legacy_request_presentation: None,
                policy_source: None,
                policy_reason: Some("requires approval".to_owned()),
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::PermissionResolved {
                permission_id: "permission-1".to_owned(),
                approved: true,
            },
        ));

        assert!(view.snapshot().permissions[0].resolved);
        assert_eq!(view.snapshot().permissions[0].approved, Some(true));
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::Permission { permission }
                if permission.resolved && permission.approved == Some(true)
        ));
    }

    #[test]
    fn live_events_accumulate_in_one_projection() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for text in ["hello ", "world"] {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantTextDelta {
                    turn_id: "turn-1".to_owned(),
                    text: text.to_owned(),
                },
            });
        }
        for text in ["reason ", "continued"] {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantReasoningDelta {
                    turn_id: "turn-1".to_owned(),
                    text: text.to_owned(),
                },
            });
        }

        assert_eq!(view.snapshot().transcript.items.len(), 2);
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::AssistantMessage { message } if message.text == "hello world"
        ));
        assert!(matches!(
            &view.snapshot().transcript.items[1].kind,
            TranscriptViewItemKind::ReasoningMessage { message }
                if message.text == "reason continued"
        ));

        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                text: " again".to_owned(),
            },
        });
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "hello world again"
        ));
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
