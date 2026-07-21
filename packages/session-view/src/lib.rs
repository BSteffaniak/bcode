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
    ChatMessageView, ComposerViewState, InteractionViewSummary, PluginStatusView, PluginVisualView,
    ProviderProgressView, SessionViewSnapshot, TextFormat, ThinkingViewState, ToolInvocationView,
    ToolInvocationViewStatus, ToolOutputView, ToolResultView, ToolTimingView, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

/// Renderer-neutral session view projection.
#[derive(Debug, Clone)]
pub struct SessionView {
    snapshot: SessionViewSnapshot,
    tool_item_ids: BTreeMap<String, TranscriptViewItemId>,
    interaction_item_ids: BTreeMap<String, TranscriptViewItemId>,
    tool_invocation_projections: BTreeMap<String, ToolInvocationProjection>,
    terminal_runtime_work: BTreeSet<bcode_session_models::WorkId>,
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
            tool_item_ids: BTreeMap::new(),
            interaction_item_ids: BTreeMap::new(),
            tool_invocation_projections: BTreeMap::new(),
            terminal_runtime_work: BTreeSet::new(),
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

    /// Replace active plugin status supplied by renderer attachment hydration.
    pub fn set_plugin_status(&mut self, plugin_status: impl IntoIterator<Item = PluginStatusView>) {
        let plugin_status = plugin_status
            .into_iter()
            .map(|status| (format!("{}:{}", status.plugin_id, status.note_id), status))
            .collect();
        if self.snapshot.plugin_status != plugin_status {
            self.snapshot.plugin_status = plugin_status;
            self.bump_revision();
        }
    }

    /// Replace active skill identifiers supplied by the daemon.
    pub fn set_active_skill_ids(&mut self, skill_ids: BTreeSet<String>) {
        if self.snapshot.active_skills != skill_ids {
            self.snapshot.active_skills = skill_ids;
            self.bump_revision();
        }
    }

    /// Replace active runtime work from an authoritative daemon snapshot.
    pub fn set_runtime_work_snapshots(&mut self, snapshots: &[bcode_ipc::RuntimeWorkSnapshot]) {
        for snapshot in snapshots {
            if matches!(
                snapshot.status,
                bcode_session_models::RuntimeWorkStatus::Completed
                    | bcode_session_models::RuntimeWorkStatus::Cancelled
                    | bcode_session_models::RuntimeWorkStatus::Failed
                    | bcode_session_models::RuntimeWorkStatus::TimedOut
            ) {
                self.terminal_runtime_work.insert(snapshot.work_id.clone());
            } else {
                self.terminal_runtime_work.remove(&snapshot.work_id);
            }
        }
        let runtime_work = snapshots
            .iter()
            .filter(|snapshot| {
                !matches!(
                    snapshot.status,
                    bcode_session_models::RuntimeWorkStatus::Completed
                        | bcode_session_models::RuntimeWorkStatus::Cancelled
                        | bcode_session_models::RuntimeWorkStatus::Failed
                        | bcode_session_models::RuntimeWorkStatus::TimedOut
                )
            })
            .map(|snapshot| bcode_session_view_models::RuntimeWorkView {
                work_id: snapshot.work_id.clone(),
                kind: snapshot.kind,
                label: snapshot.label.clone(),
                status: snapshot.status,
                cancellable: snapshot.cancellable,
                message: None,
                completed_units: None,
                total_units: None,
                updated_at_ms: None,
            })
            .collect::<Vec<_>>();
        if self.snapshot.runtime_work != runtime_work {
            self.snapshot.runtime_work = runtime_work;
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

    /// Replace attached agent selection supplied by the daemon.
    pub fn set_agent_id(&mut self, agent_id: Option<String>) {
        if self.snapshot.runtime.agent_id != agent_id {
            self.snapshot.runtime.agent_id = agent_id;
            self.bump_revision();
        }
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
        if event.sequence != 0 {
            if self
                .snapshot
                .latest_sequence
                .is_some_and(|sequence| event.sequence <= sequence)
            {
                return;
            }
            self.snapshot.latest_sequence = Some(event.sequence);
        }
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
                    TranscriptViewItemId::event(event.sequence),
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
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    StreamingMessageKind::Assistant,
                    text,
                );
            }
            SessionEventKind::AssistantMessage { text } => {
                self.finish_or_push_message(
                    TranscriptViewItemId::event(event.sequence),
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
                    TranscriptViewItemId::event(event.sequence),
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
                    TranscriptViewItemId::event(event.sequence),
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
                        TranscriptViewItemId::new(format!(
                            "tool-visual:{tool_call_id}:{}",
                            stream_sequence(stream)
                        )),
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
                    TranscriptViewItemId::event(event.sequence),
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
                self.snapshot.runtime.provider_progress = None;
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
                self.snapshot.runtime.provider_progress = None;
                self.snapshot.runtime.last_turn_outcome = Some(*outcome);
                self.snapshot.runtime.last_turn_message.clone_from(message);
                if *outcome == bcode_session_models::ModelTurnOutcome::Error {
                    self.push_item(
                        TranscriptViewItemId::event(event.sequence),
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
                    TranscriptViewItemId::event(event.sequence),
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
                    TranscriptViewItemId::event(event.sequence),
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
            SessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                ..
            } => {
                let source = source
                    .as_ref()
                    .map_or_else(String::new, |source| format!("\nSource: {}", source.label));
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::plain(format!(
                            "Invoked skill {skill_id}{source}\nArguments: {arguments}"
                        )),
                    },
                );
            }
            SessionEventKind::SkillSuggested {
                skill_id,
                reason: Some(reason),
                ..
            } => {
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::plain(format!(
                            "Suggested skill {skill_id}\nReason: {reason}"
                        )),
                    },
                );
            }
            SessionEventKind::SkillActivated { skill_id, .. }
                if self.snapshot.active_skills.insert(skill_id.to_string()) =>
            {
                self.bump_revision();
            }
            SessionEventKind::SkillDeactivated { skill_id, .. }
                if self.snapshot.active_skills.remove(skill_id.as_str()) =>
            {
                self.bump_revision();
            }
            SessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                source,
                preview,
                ..
            } => {
                let source = source
                    .as_ref()
                    .map_or_else(String::new, |source| format!("\nSource: {}", source.label));
                let preview = preview.as_deref().map_or_else(String::new, |preview| {
                    if preview.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\n\nPreview:\n{preview}")
                    }
                });
                let suffix = if *truncated { " (truncated)" } else { "" };
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::plain(format!(
                            "Loaded skill context {skill_id}{source}\nBytes: {bytes_loaded}{suffix}{preview}"
                        )),
                    },
                );
            }
            SessionEventKind::SkillInvocationFailed {
                skill_id, error, ..
            } => {
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::plain(format!(
                            "Skill {skill_id} failed: {error}"
                        )),
                    },
                );
            }
            SessionEventKind::PluginStatusNote {
                plugin_id,
                note_id,
                text,
                metadata,
            } => {
                let key = format!("{plugin_id}:{note_id}");
                self.snapshot.plugin_status.insert(
                    key,
                    PluginStatusView {
                        plugin_id: plugin_id.clone(),
                        note_id: note_id.clone(),
                        text: text.clone(),
                        priority: 0,
                        metadata: metadata.clone(),
                    },
                );
                self.upsert_item(
                    TranscriptViewItemId::new(format!("plugin-status:{plugin_id}:{note_id}")),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::plain(text.clone()),
                    },
                );
            }
            SessionEventKind::RalphLifecycle {
                loop_name,
                state_dir,
                kind,
                message,
                ..
            } => {
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::plain(format!(
                            "Ralph {kind}\nLoop: {loop_name}\n{message}\nState: {}",
                            state_dir.display()
                        )),
                    },
                );
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
                    TranscriptViewItemId::permission(permission_id),
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
                if let Some(index) = self
                    .snapshot
                    .permissions
                    .iter()
                    .position(|permission| permission.permission_id == *permission_id)
                {
                    let mut permission = self.snapshot.permissions.remove(index);
                    permission.resolved = true;
                    permission.approved = Some(*approved);
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
                kind,
                label,
                started_at_ms,
                cancellable,
                ..
            } => {
                if self.terminal_runtime_work.contains(work_id) {
                    return;
                }
                self.upsert_runtime_work(bcode_session_view_models::RuntimeWorkView {
                    work_id: work_id.clone(),
                    kind: *kind,
                    label: label.clone(),
                    status: bcode_session_models::RuntimeWorkStatus::Running,
                    cancellable: *cancellable,
                    message: None,
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
                if self.terminal_runtime_work.contains(work_id) {
                    return;
                }
                let existing = self
                    .snapshot
                    .runtime_work
                    .iter()
                    .find(|work| work.work_id == *work_id);
                let kind = existing.map_or(bcode_session_models::RuntimeWorkKind::Tool, |work| {
                    work.kind
                });
                let label = existing.map_or_else(|| work_id.to_string(), |work| work.label.clone());
                let cancellable = existing.is_some_and(|work| work.cancellable);
                let status = existing
                    .map_or(bcode_session_models::RuntimeWorkStatus::Running, |work| {
                        work.status
                    });
                self.upsert_runtime_work(bcode_session_view_models::RuntimeWorkView {
                    work_id: work_id.clone(),
                    kind,
                    label,
                    status,
                    cancellable,
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
                if self.terminal_runtime_work.contains(work_id) {
                    return;
                }
                let existing = self
                    .snapshot
                    .runtime_work
                    .iter()
                    .find(|work| work.work_id == *work_id);
                let kind = existing.map_or(bcode_session_models::RuntimeWorkKind::Tool, |work| {
                    work.kind
                });
                let label = existing.map_or_else(|| work_id.to_string(), |work| work.label.clone());
                let cancellable = existing.is_some_and(|work| work.cancellable);
                let message = existing.and_then(|work| work.message.clone());
                self.upsert_runtime_work(bcode_session_view_models::RuntimeWorkView {
                    work_id: work_id.clone(),
                    kind,
                    label,
                    status: bcode_session_models::RuntimeWorkStatus::Cancelling,
                    cancellable,
                    message,
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
                if !self.terminal_runtime_work.insert(work_id.clone()) {
                    return;
                }
                let existing = self
                    .snapshot
                    .runtime_work
                    .iter()
                    .find(|work| work.work_id == *work_id);
                let kind = existing.map_or(bcode_session_models::RuntimeWorkKind::Tool, |work| {
                    work.kind
                });
                let label = existing.map_or_else(|| work_id.to_string(), |work| work.label.clone());
                let cancellable = existing.is_some_and(|work| work.cancellable);
                self.finish_runtime_work(bcode_session_view_models::RuntimeWorkView {
                    work_id: work_id.clone(),
                    kind,
                    label,
                    status: *status,
                    cancellable,
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
            SessionLiveEventKind::AssistantTextDelta { turn_id, text } => {
                self.push_or_append_streaming_message(
                    TranscriptViewItemId::new(format!("assistant-turn:{turn_id}")),
                    0,
                    None,
                    StreamingMessageKind::Assistant,
                    text,
                );
            }
            SessionLiveEventKind::AssistantReasoningDelta { turn_id, text } => {
                self.snapshot.thinking = ThinkingViewState {
                    visible: true,
                    active_text: Some(text.clone()),
                    streaming: true,
                };
                self.push_or_append_streaming_message(
                    TranscriptViewItemId::new(format!("reasoning-turn:{turn_id}")),
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
                let id = TranscriptViewItemId::new(format!("tool-preview:{tool_call_id}"));
                self.upsert_item(
                    id,
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
            SessionLiveEventKind::ProviderStreamProgress { turn_id, event } => {
                self.snapshot.runtime.provider_progress = Some(ProviderProgressView {
                    turn_id: turn_id.clone(),
                    detail: provider_progress_detail(event),
                    retry_at_unix: match event {
                        bcode_session_models::ProviderStreamEvent::RetryScheduled {
                            retry_at_unix,
                            ..
                        } => Some(*retry_at_unix),
                        _ => None,
                    },
                });
                self.bump_revision();
            }
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

    fn push_item(
        &mut self,
        id: TranscriptViewItemId,
        sequence: u64,
        timestamp_ms: Option<u64>,
        streaming: bool,
        kind: TranscriptViewItemKind,
    ) -> TranscriptViewItemId {
        self.snapshot.transcript.items.push(TranscriptViewItem {
            id: id.clone(),
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

    fn upsert_item(
        &mut self,
        id: TranscriptViewItemId,
        sequence: u64,
        timestamp_ms: Option<u64>,
        streaming: bool,
        kind: TranscriptViewItemKind,
    ) {
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| item.id == id)
        {
            item.kind = kind;
            item.streaming = streaming;
            item.sequence = (sequence != 0).then_some(sequence).or(item.sequence);
            item.timestamp_ms = timestamp_ms.or(item.timestamp_ms);
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
            return;
        }
        self.push_item(id, sequence, timestamp_ms, streaming, kind);
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
                let id = entry.get().clone();
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
                    TranscriptViewItemId::tool(tool_call_id),
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
            *existing = work.clone();
            let id = TranscriptViewItemId::runtime_work(&work.work_id);
            if let Some(item) = self
                .snapshot
                .transcript
                .items
                .iter_mut()
                .find(|item| item.id == id)
            {
                item.kind = TranscriptViewItemKind::RuntimeWork { work };
                item.revision = item.revision.saturating_add(1);
                self.snapshot.transcript.revision =
                    self.snapshot.transcript.revision.saturating_add(1);
            }
        } else {
            self.snapshot.runtime_work.push(work.clone());
            self.push_item(
                TranscriptViewItemId::runtime_work(&work.work_id),
                0,
                work.updated_at_ms,
                false,
                TranscriptViewItemKind::RuntimeWork { work },
            );
            return;
        }
        self.bump_revision();
    }

    fn finish_runtime_work(&mut self, work: bcode_session_view_models::RuntimeWorkView) {
        self.snapshot
            .runtime_work
            .retain(|active| active.work_id != work.work_id);
        let id = TranscriptViewItemId::runtime_work(&work.work_id);
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| item.id == id)
        {
            item.kind = TranscriptViewItemKind::RuntimeWork { work };
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
        } else {
            self.push_item(
                id,
                0,
                work.updated_at_ms,
                false,
                TranscriptViewItemKind::RuntimeWork { work },
            );
        }
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
            TranscriptViewItemId::interaction(&interaction.interaction_id),
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
            .cloned()
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
        id: TranscriptViewItemId,
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
            id,
            sequence,
            timestamp_ms,
            true,
            kind.item_kind(text.to_owned()),
        );
    }

    fn finish_or_push_message(
        &mut self,
        id: TranscriptViewItemId,
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
            item.id = id;
            replace_text_in_item(item, text);
            item.streaming = false;
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
            return;
        }
        self.push_item(
            id,
            sequence,
            timestamp_ms,
            false,
            kind.item_kind(text.to_owned()),
        );
    }
}

fn provider_progress_detail(event: &bcode_session_models::ProviderStreamEvent) -> String {
    match event {
        bcode_session_models::ProviderStreamEvent::TurnStarted => {
            "provider stream started".to_owned()
        }
        bcode_session_models::ProviderStreamEvent::ToolCallStarted { tool_name, .. } => {
            format!("provider stream tool started: {tool_name}")
        }
        bcode_session_models::ProviderStreamEvent::ToolCallProgress {
            tool_name,
            argument_bytes,
            ..
        } => format!("assembling {tool_name} arguments ({argument_bytes} bytes received)"),
        bcode_session_models::ProviderStreamEvent::ToolCallFinished { tool_name, .. } => {
            format!("provider stream tool finished: {tool_name}")
        }
        bcode_session_models::ProviderStreamEvent::NoProgressWarning {
            idle_seconds,
            active_tool_call,
        } => active_tool_call.as_ref().map_or_else(
            || format!("provider stream idle for {idle_seconds}s"),
            |tool| {
                format!(
                    "provider stream idle for {idle_seconds}s while assembling {}",
                    tool.tool_name
                )
            },
        ),
        bcode_session_models::ProviderStreamEvent::RetryScheduled { message, .. } => {
            message.clone()
        }
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

const fn stream_sequence(event: &ToolInvocationStreamEvent) -> u64 {
    match event {
        ToolInvocationStreamEvent::Started { sequence, .. }
        | ToolInvocationStreamEvent::OutputDelta { sequence, .. }
        | ToolInvocationStreamEvent::VisualUpdate { sequence, .. }
        | ToolInvocationStreamEvent::ArtifactUpdate { sequence, .. }
        | ToolInvocationStreamEvent::Status { sequence, .. }
        | ToolInvocationStreamEvent::LegacyPresentation { sequence, .. }
        | ToolInvocationStreamEvent::Finished { sequence, .. } => *sequence,
        ToolInvocationStreamEvent::LegacyTransientPruned { .. } => 0,
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
        SessionId, SessionLiveEvent, SessionLiveEventKind, SessionTokenUsage, ToolInvocationResult,
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
    fn projects_provider_stream_progress() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ProviderStreamProgress {
                turn_id: "turn-1".to_owned(),
                event: bcode_session_models::ProviderStreamEvent::ToolCallProgress {
                    tool_call_id: "tool-1".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    argument_bytes: 128,
                },
            },
        });

        let progress = view
            .snapshot()
            .runtime
            .provider_progress
            .as_ref()
            .expect("provider progress should be projected");
        assert_eq!(progress.turn_id, "turn-1");
        assert_eq!(
            progress.detail,
            "assembling shell.run arguments (128 bytes received)"
        );
        assert_eq!(progress.retry_at_unix, None);

        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ProviderStreamProgress {
                turn_id: "turn-1".to_owned(),
                event: bcode_session_models::ProviderStreamEvent::RetryScheduled {
                    message: "retrying".to_owned(),
                    retry_at_unix: 42,
                },
            },
        });
        let progress = view
            .snapshot()
            .runtime
            .provider_progress
            .as_ref()
            .expect("retry progress should be projected");
        assert_eq!(progress.detail, "retrying");
        assert_eq!(progress.retry_at_unix, Some(42));
    }

    #[test]
    fn projects_skill_and_plugin_status_semantics() {
        let session_id = SessionId::new();
        let skill_id = bcode_skill_models::SkillId::new("renderer-skill");
        let snapshot = build_session_view_snapshot(&[
            event(
                session_id,
                1,
                SessionEventKind::SkillActivated {
                    skill_id: skill_id.clone(),
                    source: None,
                    mode: bcode_skill_models::SkillActivationMode::Explicit,
                    activated_at_ms: 10,
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::SkillInvoked {
                    skill_id: skill_id.clone(),
                    arguments: "carefully".to_owned(),
                    source: None,
                    invoked_at_ms: 20,
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::PluginStatusNote {
                    plugin_id: "bcode.loop".to_owned(),
                    note_id: "run".to_owned(),
                    text: "iteration running".to_owned(),
                    metadata: BTreeMap::from([("iteration".to_owned(), serde_json::json!(2))]),
                },
            ),
            event(
                session_id,
                4,
                SessionEventKind::PluginStatusNote {
                    plugin_id: "bcode.loop".to_owned(),
                    note_id: "run".to_owned(),
                    text: "iteration finished".to_owned(),
                    metadata: BTreeMap::from([("iteration".to_owned(), serde_json::json!(2))]),
                },
            ),
            event(
                session_id,
                5,
                SessionEventKind::SkillDeactivated {
                    skill_id,
                    deactivated_at_ms: 50,
                },
            ),
        ]);

        assert!(snapshot.active_skills.is_empty());
        let status = snapshot
            .plugin_status
            .get("bcode.loop:run")
            .expect("plugin status should be projected");
        assert_eq!(status.text, "iteration finished");
        let status_items = snapshot
            .transcript
            .items
            .iter()
            .filter(|item| item.id.get() == "plugin-status:bcode.loop:run")
            .collect::<Vec<_>>();
        assert_eq!(status_items.len(), 1);
        assert_eq!(status_items[0].revision, 1);
        assert!(snapshot.transcript.items.iter().any(|item| {
            matches!(
                &item.kind,
                TranscriptViewItemKind::SystemMessage { message }
                    if message.text.contains("Invoked skill renderer-skill")
            )
        }));
    }

    #[test]
    fn source_derived_item_ids_survive_bounded_window_shifts() {
        let session_id = SessionId::new();
        let events = vec![
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
                    text: "hello".to_owned(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            event(
                session_id,
                3,
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
        ];

        let full = build_session_view_snapshot(&events);
        let shifted = build_session_view_snapshot(&events[1..]);
        let full_ids = full
            .transcript
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        let shifted_ids = shifted
            .transcript
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();

        assert_eq!(full_ids, shifted_ids);
        assert_eq!(full_ids[0].get(), "event:2");
        assert_eq!(full_ids[1].get(), "tool:tool-1");
    }

    #[test]
    fn duplicate_durable_events_do_not_mutate_the_view() {
        let session_id = SessionId::new();
        let event = event(
            session_id,
            1,
            SessionEventKind::SystemMessage {
                text: "once".to_owned(),
            },
        );
        let mut view = SessionView::new();
        view.apply_event(&event);
        let snapshot = view.snapshot().clone();

        view.apply_event(&event);

        assert_eq!(view.snapshot(), &snapshot);
    }

    #[test]
    fn repeated_live_tool_previews_replace_one_stable_item() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for (argument_bytes, title) in [(3, "first"), (6, "second")] {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::ToolArgumentPreview {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "tool-1".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    argument_bytes,
                    preview: bcode_session_models::LiveToolArgumentPreview {
                        visual: bcode_session_models::PluginVisualDescriptor {
                            visual_id: Some("preview-1".to_owned()),
                            producer_plugin_id: Some("shell".to_owned()),
                            schema: "shell.preview".to_owned(),
                            schema_version: 1,
                            title: Some(title.to_owned()),
                            subtitle: None,
                            payload: serde_json::json!({"title": title}),
                        },
                        streaming_status: None,
                        argument_bytes,
                    },
                },
            });
        }

        let previews = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| matches!(item.kind, TranscriptViewItemKind::PluginVisual { .. }))
            .collect::<Vec<_>>();
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].id.get(), "tool-preview:tool-1");
        assert_eq!(previews[0].revision, 1);
        assert!(matches!(
            &previews[0].kind,
            TranscriptViewItemKind::PluginVisual { visual }
                if visual.descriptor.title.as_deref() == Some("second")
        ));
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

        assert!(view.snapshot().permissions.is_empty());
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
    fn durable_results_reconcile_cumulative_live_state_without_losing_it_early() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
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
        ));
        for text in ["live ", "answer"] {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantTextDelta {
                    turn_id: "turn-1".to_owned(),
                    text: text.to_owned(),
                },
            });
        }
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolOutputDelta {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "tool-1".to_owned(),
                    sequence: 1,
                    stream: ToolOutputStream::Stdout,
                    text: "live output".to_owned(),
                    byte_len: 11,
                },
            },
        });

        assert!(matches!(
            &view.snapshot().transcript.items[1].kind,
            TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "live answer"
        ));
        assert_eq!(
            view.snapshot()
                .tools
                .get("tool-1")
                .and_then(|tool| tool.output.as_ref())
                .map(|output| output.text.as_str()),
            Some("live output")
        );

        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::AssistantMessage {
                text: "durable answer".to_owned(),
            },
        ));
        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "tool-1".to_owned(),
                result: "durable output".to_owned(),
                is_error: false,
                output: None,
                semantic_result: Some(ToolInvocationResult::Text {
                    text: "durable output".to_owned(),
                }),
            },
        ));

        assert!(matches!(
            &view.snapshot().transcript.items[1].kind,
            TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "durable answer"
        ));
        let tool = view.snapshot().tools.get("tool-1").expect("tool");
        assert_eq!(tool.status, ToolInvocationViewStatus::Finished);
        assert_eq!(tool.result_text.as_deref(), Some("durable output"));
    }

    #[test]
    fn authoritative_plugin_status_replaces_shared_state_atomically() {
        let mut view = SessionView::new();
        view.set_plugin_status([PluginStatusView {
            plugin_id: "plugin".to_owned(),
            note_id: "loop".to_owned(),
            text: "Loop active".to_owned(),
            priority: 7,
            metadata: BTreeMap::new(),
        }]);

        let status = view
            .snapshot()
            .plugin_status
            .get("plugin:loop")
            .expect("plugin status");
        assert_eq!(status.text, "Loop active");
        assert_eq!(status.priority, 7);

        view.set_plugin_status([]);
        assert!(view.snapshot().plugin_status.is_empty());
    }

    #[test]
    fn authoritative_runtime_work_snapshots_replace_state_and_block_terminal_revival() {
        let session_id = SessionId::new();
        let work_id = bcode_session_models::WorkId::new("snapshot-work");
        let mut view = SessionView::new();
        view.set_runtime_work_snapshots(&[bcode_ipc::RuntimeWorkSnapshot {
            work_id: work_id.clone(),
            kind: bcode_session_models::RuntimeWorkKind::PluginInvocation,
            label: "plugin call".to_owned(),
            tool_call_id: None,
            status: bcode_session_models::RuntimeWorkStatus::Running,
            cancellable: true,
        }]);

        let work = &view.snapshot().runtime_work[0];
        assert_eq!(
            work.kind,
            bcode_session_models::RuntimeWorkKind::PluginInvocation
        );
        assert_eq!(work.label, "plugin call");
        assert!(work.cancellable);

        view.set_runtime_work_snapshots(&[bcode_ipc::RuntimeWorkSnapshot {
            work_id: work_id.clone(),
            kind: bcode_session_models::RuntimeWorkKind::PluginInvocation,
            label: "plugin call".to_owned(),
            tool_call_id: None,
            status: bcode_session_models::RuntimeWorkStatus::Cancelled,
            cancellable: true,
        }]);
        assert!(view.snapshot().runtime_work.is_empty());
        view.set_runtime_work_snapshots(&[]);

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind: bcode_session_models::RuntimeWorkKind::PluginInvocation,
                label: "late start".to_owned(),
                tool_call_id: None,
                plugin_id: Some("plugin".to_owned()),
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(1),
                cancellable: true,
            },
        ));
        assert!(view.snapshot().runtime_work.is_empty());
    }

    #[test]
    fn runtime_work_terminal_state_leaves_sibling_active_and_rejects_late_revival() {
        let session_id = SessionId::new();
        let first = bcode_session_models::WorkId::new("work-1");
        let second = bcode_session_models::WorkId::new("work-2");
        let started = |work_id: bcode_session_models::WorkId, label: &str| {
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind: bcode_session_models::RuntimeWorkKind::Tool,
                label: label.to_owned(),
                tool_call_id: None,
                plugin_id: None,
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(10),
                cancellable: true,
            }
        };
        let mut view = SessionView::new();
        view.apply_event(&event(session_id, 1, started(first.clone(), "first")));
        view.apply_event(&event(session_id, 2, started(second.clone(), "second")));
        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::RuntimeWorkFinished {
                work_id: first.clone(),
                status: bcode_session_models::RuntimeWorkStatus::Completed,
                finished_at_ms: Some(30),
                message: Some("done".to_owned()),
            },
        ));

        assert_eq!(view.snapshot().runtime_work.len(), 1);
        assert_eq!(view.snapshot().runtime_work[0].work_id, second);
        assert_eq!(
            view.snapshot().runtime_work[0].kind,
            bcode_session_models::RuntimeWorkKind::Tool
        );
        assert_eq!(view.snapshot().runtime_work[0].label, "second");
        assert!(view.snapshot().runtime_work[0].cancellable);
        assert!(view.snapshot().transcript.items.iter().any(|item| {
            matches!(
                &item.kind,
                TranscriptViewItemKind::RuntimeWork { work }
                    if work.work_id == first
                        && work.status == bcode_session_models::RuntimeWorkStatus::Completed
            )
        }));

        view.apply_event(&event(session_id, 4, started(first.clone(), "late")));
        view.apply_event(&event(
            session_id,
            5,
            SessionEventKind::RuntimeWorkProgress {
                work_id: first,
                message: "late progress".to_owned(),
                completed_units: None,
                total_units: None,
                progress_at_ms: Some(50),
            },
        ));
        assert_eq!(view.snapshot().runtime_work.len(), 1);
        assert_eq!(view.snapshot().runtime_work[0].work_id, second);

        let cancelled = bcode_session_models::WorkId::new("work-cancelled");
        view.apply_event(&event(
            session_id,
            6,
            started(cancelled.clone(), "cancelled"),
        ));
        view.apply_event(&event(
            session_id,
            7,
            SessionEventKind::RuntimeWorkFinished {
                work_id: cancelled.clone(),
                status: bcode_session_models::RuntimeWorkStatus::Cancelled,
                finished_at_ms: Some(70),
                message: None,
            },
        ));
        view.apply_event(&event(
            session_id,
            8,
            started(cancelled.clone(), "late cancelled"),
        ));
        assert_eq!(view.snapshot().runtime_work.len(), 1);
        assert_eq!(view.snapshot().runtime_work[0].work_id, second);
        assert!(view.snapshot().transcript.items.iter().any(|item| {
            matches!(
                &item.kind,
                TranscriptViewItemKind::RuntimeWork { work }
                    if work.work_id == cancelled
                        && work.status == bcode_session_models::RuntimeWorkStatus::Cancelled
            )
        }));
    }

    #[test]
    fn terminal_runtime_work_without_visible_start_is_history_only() {
        let session_id = SessionId::new();
        let work_id = bcode_session_models::WorkId::new("work-terminal-only");
        let mut view = SessionView::new();

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::RuntimeWorkFinished {
                work_id: work_id.clone(),
                status: bcode_session_models::RuntimeWorkStatus::Completed,
                finished_at_ms: Some(10),
                message: Some("complete".to_owned()),
            },
        ));

        assert!(view.snapshot().runtime_work.is_empty());
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::RuntimeWork { work }
                if work.work_id == work_id
                    && work.status == bcode_session_models::RuntimeWorkStatus::Completed
        ));
    }

    #[test]
    fn runtime_work_updates_collection_and_transcript_item() {
        let session_id = SessionId::new();
        let work_id = bcode_session_models::WorkId::new("work-1");
        let snapshot = build_session_view_snapshot(&[
            event(
                session_id,
                1,
                SessionEventKind::RuntimeWorkStarted {
                    work_id: work_id.clone(),
                    kind: bcode_session_models::RuntimeWorkKind::Tool,
                    label: "tool".to_owned(),
                    tool_call_id: Some("tool-1".to_owned()),
                    plugin_id: Some("plugin".to_owned()),
                    service_interface: None,
                    operation: None,
                    parent_work_id: None,
                    started_at_ms: Some(10),
                    cancellable: true,
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::RuntimeWorkProgress {
                    work_id,
                    message: "halfway".to_owned(),
                    completed_units: Some(1),
                    total_units: Some(2),
                    progress_at_ms: Some(20),
                },
            ),
        ]);

        assert_eq!(snapshot.runtime_work[0].message.as_deref(), Some("halfway"));
        assert!(matches!(
            &snapshot.transcript.items[0].kind,
            TranscriptViewItemKind::RuntimeWork { work }
                if work.message.as_deref() == Some("halfway")
                    && work.completed_units == Some(1)
                    && work.total_units == Some(2)
        ));
    }

    #[test]
    fn provider_compaction_view_hides_opaque_payloads() {
        let secret = "secret-opaque-view-value";
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[event(
            session_id,
            1,
            SessionEventKind::ProviderContextCompacted {
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
        )]);

        let TranscriptViewItemKind::SystemMessage { message } = &snapshot.transcript.items[0].kind
        else {
            panic!("expected provider compaction system message");
        };
        assert!(message.text.contains("Provider context compacted"));
        assert!(!message.text.contains(secret));
        assert!(!message.text.contains("portable summary"));
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
