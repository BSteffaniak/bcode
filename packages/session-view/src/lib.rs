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
    ToolInvocationProjection, apply_tool_invocation_projection_event,
};
use bcode_session_view_models::{
    ChatMessageView, CompactionView, CompactionViewStatus, ComposerViewState,
    InteractionViewSummary, PluginStatusView, ProviderProgressView, SessionViewSnapshot, SkillView,
    SkillViewStatus, TextFormat, ThinkingViewState, ToolInvocationView, ToolInvocationViewStatus,
    ToolRequestDraftView, ToolResultView, ToolTimingView, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

#[derive(Debug, Clone, Default)]
struct LiveReasoningActivity {
    order: u32,
    parts: BTreeMap<String, bcode_session_models::ReasoningPart>,
    opaque: bool,
    finished: bool,
}

impl LiveReasoningActivity {
    fn from_complete(activity: &bcode_session_models::ReasoningActivity) -> Self {
        Self {
            order: activity.order,
            parts: activity
                .parts
                .iter()
                .cloned()
                .map(|part| (part.part_id.clone(), part))
                .collect(),
            opaque: activity.opaque,
            finished: true,
        }
    }

    fn apply(&mut self, event: &bcode_session_models::ReasoningActivityEvent) {
        use bcode_session_models::ReasoningActivityEvent;
        self.order = event.activity_order();
        match event {
            ReasoningActivityEvent::Started { .. } => {}
            ReasoningActivityEvent::PartDelta {
                part_id,
                kind,
                role,
                part_order,
                text,
                ..
            } => {
                self.parts
                    .entry(part_id.clone())
                    .and_modify(|part| part.text.push_str(text))
                    .or_insert_with(|| bcode_session_models::ReasoningPart {
                        part_id: part_id.clone(),
                        kind: *kind,
                        role: *role,
                        order: *part_order,
                        text: text.clone(),
                    });
            }
            ReasoningActivityEvent::PartCompleted {
                part_id,
                kind,
                role,
                part_order,
                text,
                ..
            } => {
                self.parts.insert(
                    part_id.clone(),
                    bcode_session_models::ReasoningPart {
                        part_id: part_id.clone(),
                        kind: *kind,
                        role: *role,
                        order: *part_order,
                        text: text.clone(),
                    },
                );
            }
            ReasoningActivityEvent::OpaqueObserved { .. } => self.opaque = true,
            ReasoningActivityEvent::Finished { .. } => self.finished = true,
        }
    }
}

/// Renderer-neutral session view projection.
#[derive(Debug, Clone)]
pub struct SessionView {
    snapshot: SessionViewSnapshot,
    tool_item_ids: BTreeMap<String, TranscriptViewItemId>,
    interaction_item_ids: BTreeMap<String, TranscriptViewItemId>,
    tool_invocation_projections: BTreeMap<String, ToolInvocationProjection>,
    contribution_sequences: BTreeMap<String, u64>,
    contribution_placements: BTreeMap<String, bcode_session_models::ToolContributionPlacement>,
    contribution_slot_items: BTreeMap<TranscriptViewItemId, usize>,
    contribution_slot_owners: BTreeMap<TranscriptViewItemId, String>,
    terminal_invocations: BTreeSet<String>,
    terminal_invocation_statuses: BTreeMap<String, ToolInvocationViewStatus>,
    terminal_runtime_work: BTreeSet<bcode_session_models::WorkId>,
    tool_request_drafts: BTreeMap<String, ToolRequestDraftView>,
    terminal_tool_request_drafts: BTreeMap<String, (u64, u64)>,
    live_reasoning: BTreeMap<(String, String), LiveReasoningActivity>,
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
            contribution_sequences: BTreeMap::new(),
            contribution_placements: BTreeMap::new(),
            contribution_slot_items: BTreeMap::new(),
            contribution_slot_owners: BTreeMap::new(),
            terminal_invocations: BTreeSet::new(),
            terminal_invocation_statuses: BTreeMap::new(),
            terminal_runtime_work: BTreeSet::new(),
            tool_request_drafts: BTreeMap::new(),
            terminal_tool_request_drafts: BTreeMap::new(),
            live_reasoning: BTreeMap::new(),
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

    /// Apply canonical session metadata from an attach or catalog response.
    pub fn set_session_summary(&mut self, summary: bcode_session_models::SessionSummary) {
        let title = summary.title().map(ToOwned::to_owned);
        let changed = self.snapshot.session_id != Some(summary.id)
            || title
                .as_ref()
                .is_some_and(|title| self.snapshot.title.as_ref() != Some(title))
            || self.snapshot.working_directory != Some(summary.working_directory.clone())
            || self.snapshot.session_summary.as_ref() != Some(&summary);
        if !changed {
            return;
        }
        self.snapshot.session_id = Some(summary.id);
        if title.is_some() {
            self.snapshot.title = title;
        }
        self.snapshot.working_directory = Some(summary.working_directory.clone());
        self.snapshot.session_summary = Some(summary);
        self.bump_revision();
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

    /// Replace renderer-selected model identity while retaining unrelated runtime state.
    pub fn set_model_selection(
        &mut self,
        provider_plugin_id: Option<String>,
        requested_model_id: Option<String>,
        effective_model_id: Option<String>,
    ) {
        let runtime = &mut self.snapshot.runtime;
        if runtime.provider_plugin_id != provider_plugin_id
            || runtime.requested_model_id != requested_model_id
            || runtime.effective_model_id != effective_model_id
        {
            runtime.provider_plugin_id = provider_plugin_id;
            runtime.requested_model_id = requested_model_id;
            runtime.effective_model_id = effective_model_id;
            self.bump_revision();
        }
    }

    /// Replace renderer-selected reasoning identity while retaining unrelated runtime state.
    pub fn set_reasoning_selection(
        &mut self,
        reasoning_effort: Option<String>,
        reasoning_summary: Option<String>,
    ) {
        let runtime = &mut self.snapshot.runtime;
        if runtime.reasoning_effort != reasoning_effort
            || runtime.reasoning_summary != reasoning_summary
        {
            runtime.reasoning_effort = reasoning_effort;
            runtime.reasoning_summary = reasoning_summary;
            self.bump_revision();
        }
    }

    /// Set a complete renderer-local reasoning presentation policy.
    pub fn set_reasoning_presentation_policy(
        &mut self,
        policy: bcode_session_view_models::ReasoningPresentationPolicy,
    ) {
        let (visible, mode) = match policy {
            bcode_session_view_models::ReasoningPresentationPolicy::All => {
                (true, bcode_session_view_models::ReasoningDisplayMode::All)
            }
            bcode_session_view_models::ReasoningPresentationPolicy::Summary => (
                true,
                bcode_session_view_models::ReasoningDisplayMode::Summary,
            ),
            bcode_session_view_models::ReasoningPresentationPolicy::Raw => {
                (true, bcode_session_view_models::ReasoningDisplayMode::Raw)
            }
            bcode_session_view_models::ReasoningPresentationPolicy::Hidden => {
                (false, self.snapshot.thinking.mode)
            }
        };
        if self.snapshot.thinking.visible != visible || self.snapshot.thinking.mode != mode {
            self.snapshot.thinking.visible = visible;
            self.snapshot.thinking.mode = mode;
            self.refresh_reasoning_items();
            self.bump_revision();
        }
    }

    /// Set the renderer-selected readable reasoning representation.
    pub fn set_reasoning_display_mode(
        &mut self,
        mode: bcode_session_view_models::ReasoningDisplayMode,
    ) {
        if self.snapshot.thinking.mode != mode {
            self.snapshot.thinking.mode = mode;
            self.refresh_reasoning_items();
            self.bump_revision();
        }
    }

    /// Set whether renderers should expose reasoning transcript content.
    pub fn set_reasoning_visible(&mut self, visible: bool) {
        if self.snapshot.thinking.visible != visible {
            self.snapshot.thinking.visible = visible;
            self.refresh_reasoning_items();
            self.bump_revision();
        }
    }

    /// Replace authoritative request-context occupancy when it is newer than current state.
    pub fn set_context_occupancy(
        &mut self,
        occupancy: Option<bcode_session_models::RequestContextOccupancy>,
    ) {
        let should_replace = match (&self.snapshot.runtime.context_occupancy, &occupancy) {
            (_, None) | (None, Some(_)) => true,
            (Some(current), Some(next)) => {
                (next.context_epoch, next.observation_sequence)
                    >= (current.context_epoch, current.observation_sequence)
            }
        };
        if should_replace && self.snapshot.runtime.context_occupancy != occupancy {
            self.snapshot.runtime.context_occupancy = occupancy;
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
            || runtime.reasoning_summary != reasoning_summary;
        if changed {
            runtime.provider_plugin_id = provider_plugin_id;
            runtime.requested_model_id = requested_model_id;
            runtime.effective_model_id = effective_model_id;
            runtime.reasoning_effort = reasoning_effort;
            runtime.reasoning_summary = reasoning_summary;
            self.bump_revision();
        }
        self.set_context_occupancy(context_occupancy);
    }

    /// Replace attached agent selection supplied by the daemon.
    pub fn set_agent_id(&mut self, agent_id: Option<String>) {
        if self.snapshot.runtime.agent_id != agent_id {
            self.snapshot.runtime.agent_id = agent_id;
            self.bump_revision();
        }
    }

    /// Replace authoritative pending permissions supplied by daemon hydration.
    pub fn set_pending_permissions(
        &mut self,
        permissions: Vec<bcode_session_view_models::PermissionView>,
    ) {
        if self.snapshot.permissions != permissions {
            self.snapshot.permissions = permissions;
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

    /// Replace authoritative pending interactions supplied by daemon hydration.
    pub fn set_pending_interactions(&mut self, interactions: Vec<InteractionViewSummary>) {
        let pending_ids = interactions
            .iter()
            .map(|interaction| interaction.interaction_id.clone())
            .collect::<BTreeSet<_>>();
        let stale_ids = self
            .snapshot
            .interactions
            .iter()
            .filter(|interaction| {
                !interaction.resolved && !pending_ids.contains(&interaction.interaction_id)
            })
            .map(|interaction| interaction.interaction_id.clone())
            .collect::<BTreeSet<_>>();
        if !stale_ids.is_empty() {
            self.snapshot.interactions.retain(|interaction| {
                interaction.resolved || !stale_ids.contains(&interaction.interaction_id)
            });
            self.snapshot.transcript.items.retain(|item| {
                !matches!(
                    &item.kind,
                    TranscriptViewItemKind::Interaction { interaction }
                        if stale_ids.contains(&interaction.interaction_id)
                )
            });
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
        }
        for interaction in interactions {
            self.upsert_interaction(interaction);
        }
    }

    /// Insert or replace renderer-neutral interaction state hydrated from the daemon.
    pub fn upsert_interaction(&mut self, interaction: InteractionViewSummary) {
        self.upsert_interaction_item(interaction, 0, None);
    }

    /// Rebuild the bounded durable-history projection while retaining authoritative hydrated state.
    ///
    /// This is used when a renderer changes its resident history window. Transcript and tool
    /// projection are rebuilt from the supplied window, while daemon-hydrated selections,
    /// pending checkpoints, runtime work, plugin status, skills, and composer state remain
    /// available until newer authoritative data replaces them.
    pub fn rebuild_history_window(&mut self, events: &[SessionEvent]) {
        self.clear_history_window();
        self.apply_history(events);
    }

    /// Clear bounded history projection while retaining authoritative hydrated session state.
    pub fn clear_history_window(&mut self) {
        let previous = self.snapshot.clone();
        let terminal_runtime_work = self.terminal_runtime_work.clone();
        let live_tool_request_drafts = self
            .tool_request_drafts
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let terminal_tool_request_drafts = self.terminal_tool_request_drafts.clone();
        let live_contributions = self
            .snapshot
            .contributions
            .iter()
            .filter(|(_, contribution)| {
                contribution.persistence
                    == bcode_session_models::ToolContributionPersistence::Transient
            })
            .filter_map(|(key, contribution)| {
                self.contribution_placements
                    .get(key)
                    .copied()
                    .map(|placement| (contribution.clone(), placement))
            })
            .collect::<Vec<_>>();
        let mut replacement = Self::new();
        replacement.snapshot.session_id = previous.session_id;
        replacement.snapshot.title = previous.title;
        replacement.snapshot.working_directory = previous.working_directory;
        replacement.snapshot.permissions = previous.permissions;
        replacement.snapshot.runtime_work = previous.runtime_work;
        replacement.snapshot.active_skills = previous.active_skills;
        replacement.snapshot.plugin_status = previous.plugin_status;
        replacement.snapshot.composer = previous.composer;
        replacement.snapshot.thinking = previous.thinking;
        replacement.snapshot.runtime = previous.runtime;
        replacement.snapshot.interactions = previous.interactions;
        replacement.snapshot.session_summary = previous.session_summary;
        replacement.terminal_runtime_work = terminal_runtime_work;
        replacement.terminal_tool_request_drafts = terminal_tool_request_drafts;
        replacement.snapshot.revision = self.snapshot.revision.saturating_add(1);
        for draft in live_tool_request_drafts {
            replacement.apply_tool_request_draft(&bcode_session_models::ToolRequestDraftEvent {
                turn_id: draft.turn_id,
                tool_call_id: draft.tool_call_id,
                tool_name: draft.tool_name,
                producer_plugin_id: draft.producer_plugin_id,
                schema: draft.schema,
                schema_version: draft.schema_version,
                placement: draft.placement,
                generation: draft.generation,
                revision: draft.revision,
                operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                    start_offset: draft.preview_start_offset,
                    text: draft.preview,
                },
                argument_bytes: draft.argument_bytes,
                truncated: draft.truncated,
            });
        }
        for (contribution, placement) in live_contributions {
            replacement.apply_contribution_event(0, None, &contribution, placement);
        }
        *self = replacement;
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
            SessionEventKind::UserMessage {
                text, admission, ..
            } => {
                if self.snapshot.title.is_none() {
                    self.snapshot.title = Some(derive_session_title_from_prompt(text));
                }
                let display_label = admission
                    .origin
                    .as_ref()
                    .and_then(|origin| origin.display_label.clone());
                let mut message = ChatMessageView::markdown(text.clone());
                message.display_label = display_label;
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::UserMessage { message },
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
                    visible: self.snapshot.thinking.visible,
                    mode: self.snapshot.thinking.mode,
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
                    visible: self.snapshot.thinking.visible,
                    mode: self.snapshot.thinking.mode,
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
            SessionEventKind::AssistantReasoningActivity { turn_id, activity } => {
                let key = (turn_id.clone(), activity.activity_id.clone());
                self.live_reasoning
                    .insert(key, LiveReasoningActivity::from_complete(activity));
                self.upsert_item(
                    TranscriptViewItemId::reasoning(turn_id, &activity.activity_id),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::ReasoningActivity {
                        activity: reasoning_activity_view(
                            turn_id,
                            activity,
                            self.snapshot.thinking.visible,
                            self.snapshot.thinking.mode,
                        ),
                    },
                );
            }
            SessionEventKind::ToolInvocationLifecycle { event: lifecycle } => {
                use bcode_session_models::ToolInvocationLifecycleStage;
                if self.terminal_invocations.contains(&lifecycle.invocation_id) {
                    return;
                }
                match lifecycle.stage {
                    ToolInvocationLifecycleStage::Started
                    | ToolInvocationLifecycleStage::Progress
                    | ToolInvocationLifecycleStage::Waiting => {
                        self.snapshot
                            .active_invocations
                            .insert(lifecycle.invocation_id.clone(), lifecycle.clone());
                        if self
                            .tool_invocation_projections
                            .get(&lifecycle.invocation_id)
                            .is_some_and(|projection| projection.tool_name.is_some())
                        {
                            self.upsert_tool_item(
                                &lifecycle.invocation_id,
                                event.sequence,
                                Some(event.timestamp_ms),
                            );
                        }
                    }
                    ToolInvocationLifecycleStage::Completed
                    | ToolInvocationLifecycleStage::Cancelled
                    | ToolInvocationLifecycleStage::Failed => {
                        let status = match lifecycle.stage {
                            ToolInvocationLifecycleStage::Completed => {
                                ToolInvocationViewStatus::Finished
                            }
                            ToolInvocationLifecycleStage::Cancelled => {
                                ToolInvocationViewStatus::Cancelled
                            }
                            ToolInvocationLifecycleStage::Failed => {
                                ToolInvocationViewStatus::Failed
                            }
                            ToolInvocationLifecycleStage::Started
                            | ToolInvocationLifecycleStage::Progress
                            | ToolInvocationLifecycleStage::Waiting => unreachable!(),
                        };
                        if !matches!(status, ToolInvocationViewStatus::Finished) {
                            self.terminal_invocation_statuses
                                .insert(lifecycle.invocation_id.clone(), status);
                            self.apply_terminal_tool_status(
                                &lifecycle.invocation_id,
                                status,
                                lifecycle.message.as_deref(),
                                event.sequence,
                                Some(event.timestamp_ms),
                            );
                            self.tool_request_drafts.remove(&lifecycle.invocation_id);
                            let result_id = TranscriptViewItemId::tool_presentation_slot(
                                &lifecycle.invocation_id,
                                bcode_session_models::ToolContributionPlacement::Result,
                                None,
                            );
                            if !matches!(
                                self.snapshot
                                    .transcript
                                    .items
                                    .iter()
                                    .find(|item| item.id == result_id)
                                    .map(|item| &item.kind),
                                Some(TranscriptViewItemKind::ToolInvocation { .. })
                            ) {
                                self.snapshot
                                    .transcript
                                    .items
                                    .retain(|item| item.id != result_id);
                            }
                        } else if self
                            .tool_invocation_projections
                            .get(&lifecycle.invocation_id)
                            .is_some_and(|projection| projection.tool_name.is_some())
                        {
                            self.upsert_tool_item(
                                &lifecycle.invocation_id,
                                event.sequence,
                                Some(event.timestamp_ms),
                            );
                        }
                        self.snapshot
                            .active_invocations
                            .remove(&lifecycle.invocation_id);
                        self.snapshot.contributions.retain(|_, contribution| {
                            contribution.invocation_id != lifecycle.invocation_id
                                || contribution.persistence
                                    == bcode_session_models::ToolContributionPersistence::Durable
                        });
                        let live_prefix = format!("live-contribution:{}:", lifecycle.invocation_id);
                        let item_count = self.snapshot.transcript.items.len();
                        self.snapshot
                            .transcript
                            .items
                            .retain(|item| !item.id.get().starts_with(&live_prefix));
                        if self.snapshot.transcript.items.len() != item_count {
                            self.snapshot.transcript.revision =
                                self.snapshot.transcript.revision.saturating_add(1);
                        }
                        self.terminal_invocations
                            .insert(lifecycle.invocation_id.clone());
                    }
                }
                self.bump_revision();
            }
            SessionEventKind::ToolCallRequested { tool_call_id, .. } => {
                if self.tool_request_drafts.remove(tool_call_id).is_some()
                    || self
                        .terminal_tool_request_drafts
                        .remove(tool_call_id)
                        .is_some()
                {
                    let id = TranscriptViewItemId::tool_presentation_slot(
                        tool_call_id,
                        bcode_session_models::ToolContributionPlacement::Request,
                        None,
                    );
                    let item_count = self.snapshot.transcript.items.len();
                    self.snapshot.transcript.items.retain(|item| item.id != id);
                    if self.snapshot.transcript.items.len() != item_count {
                        self.snapshot.transcript.revision =
                            self.snapshot.transcript.revision.saturating_add(1);
                    }
                }
                self.upsert_tool_item(tool_call_id, event.sequence, Some(event.timestamp_ms));
            }
            SessionEventKind::ToolInvocationResultRecorded { record } => {
                self.upsert_tool_item(
                    &record.invocation_id,
                    event.sequence,
                    Some(event.timestamp_ms),
                );
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
                self.set_model_selection(
                    provider_to_display_selection(provider),
                    model_to_display_selection(model),
                    model_to_display_selection(model),
                );
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
            SessionEventKind::ModelUsage { turn_id, usage } => {
                if let Some(tokens) = usage.metered_total_tokens() {
                    self.snapshot.runtime.cumulative_metered_tokens = self
                        .snapshot
                        .runtime
                        .cumulative_metered_tokens
                        .saturating_add(u64::from(tokens));
                }
                self.snapshot.runtime.latest_usage = Some(usage.clone());
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::Usage {
                        usage: bcode_session_view_models::UsageView {
                            turn_id: turn_id.clone(),
                            usage: usage.clone(),
                        },
                    },
                );
            }
            SessionEventKind::ContextCompacted { summary, .. } => {
                self.set_context_occupancy(None);
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::Compaction {
                        compaction: CompactionView {
                            status: CompactionViewStatus::Local,
                            text: format!("local context compaction: {summary}"),
                            provider_plugin_id: None,
                            model_id: None,
                        },
                    },
                );
            }
            SessionEventKind::ProviderContextCompacted { snapshot, .. } => {
                self.set_context_occupancy(None);
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::Compaction {
                        compaction: CompactionView {
                            status: CompactionViewStatus::Provider,
                            text: format!(
                                "{} context compaction ({})",
                                provider_compaction_origin_label(snapshot.origin),
                                snapshot.provider_plugin_id
                            ),
                            provider_plugin_id: Some(snapshot.provider_plugin_id.clone()),
                            model_id: Some(snapshot.model_id.clone()),
                        },
                    },
                );
            }
            SessionEventKind::RequestContextObserved { observation } => {
                self.set_context_occupancy(Some(bcode_session_models::RequestContextOccupancy {
                    context_epoch: observation.request.context_epoch,
                    observation_sequence: event.sequence,
                    observation: observation.clone(),
                }));
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
                    TranscriptViewItemKind::Skill {
                        skill: SkillView {
                            skill_id: skill_id.to_string(),
                            status: SkillViewStatus::Invoked,
                            text: format!("invoked {skill_id}{source}\nArguments: {arguments}"),
                        },
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
                    TranscriptViewItemKind::Skill {
                        skill: SkillView {
                            skill_id: skill_id.to_string(),
                            status: SkillViewStatus::Suggested,
                            text: format!("suggested {skill_id}\nReason: {reason}"),
                        },
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
                let source = source.as_ref().map_or_else(String::new, |source| {
                    let path = source
                        .path
                        .as_deref()
                        .map_or_else(String::new, |path| format!("\nFile: {path}"));
                    format!("\nSource: {}{path}", source.label)
                });
                let preview = preview.as_deref().map_or_else(String::new, |preview| {
                    if preview.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\n\nPreview:\n{preview}")
                    }
                });
                let suffix = if *truncated { " truncated" } else { "" };
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::Skill {
                        skill: SkillView {
                            skill_id: skill_id.to_string(),
                            status: SkillViewStatus::ContextLoaded,
                            text: format!(
                                "loaded {skill_id}{source}\nBytes: {bytes_loaded}{suffix}{preview}"
                            ),
                        },
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
                    TranscriptViewItemKind::Skill {
                        skill: SkillView {
                            skill_id: skill_id.to_string(),
                            status: SkillViewStatus::Failed,
                            text: format!("{skill_id}: {error}"),
                        },
                    },
                );
            }
            SessionEventKind::ToolExchangeRequested { request } => {
                self.snapshot.active_exchanges.insert(
                    format!("{}:{}", request.invocation_id, request.exchange_id),
                    request.clone(),
                );
                self.bump_revision();
            }
            SessionEventKind::ToolExchangeResolved { event: resolution } => {
                self.snapshot.active_exchanges.remove(&format!(
                    "{}:{}",
                    resolution.invocation_id, resolution.exchange_id
                ));
                self.bump_revision();
            }
            SessionEventKind::ToolContribution {
                event: contribution,
            } => self.apply_contribution_event(
                event.sequence,
                Some(event.timestamp_ms),
                contribution,
                bcode_session_models::ToolContributionPlacement::Hidden,
            ),
            SessionEventKind::ToolContributionPlaced { envelope } => self.apply_contribution_event(
                event.sequence,
                Some(event.timestamp_ms),
                &envelope.contribution,
                envelope.placement,
            ),
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
                let mut message = ChatMessageView::plain(text.clone());
                message.display_label = Some(plugin_id.clone());
                self.upsert_item(
                    TranscriptViewItemId::new(format!("plugin-status:{plugin_id}:{note_id}")),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage { message },
                );
            }
            SessionEventKind::RalphLifecycle {
                loop_name,
                state_dir,
                kind,
                message,
                ..
            } => {
                let state_dir = self.snapshot.working_directory.as_ref().map_or_else(
                    || bcode_plugin_sdk::path::display_from_current_dir(state_dir),
                    |working_directory| {
                        bcode_plugin_sdk::path::display(state_dir, working_directory)
                    },
                );
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::plain(format!(
                            "Ralph {kind}\n* Loop: {loop_name}\n* {message}\n* State: {state_dir}"
                        )),
                    },
                );
            }
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
                policy_source,
                batch,
                policy_reason,
                ..
            } => {
                let permission = bcode_session_view_models::PermissionView {
                    permission_id: permission_id.clone(),
                    session_id: Some(event.session_id),
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    arguments_json: arguments_json.clone(),
                    batch: batch.clone().map(|batch| {
                        bcode_session_view_models::PermissionBatchView {
                            batch_id: batch.batch_id,
                            call_index: batch.call_index,
                            call_count: batch.call_count,
                        }
                    }),
                    agent_id: String::new(),
                    title: Some(format!("Permission requested: {tool_name}")),
                    policy_source: policy_source.clone(),
                    detail: policy_reason.clone(),
                    resolved: false,
                    approved: None,
                    can_remember: false,
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
                old_working_directory,
                new_working_directory,
            } => {
                self.snapshot.working_directory = Some(new_working_directory.clone());
                self.push_item(
                    TranscriptViewItemId::event(event.sequence),
                    event.sequence,
                    Some(event.timestamp_ms),
                    false,
                    TranscriptViewItemKind::SystemMessage {
                        message: ChatMessageView::markdown(working_directory_changed_message(
                            old_working_directory,
                            new_working_directory,
                        )),
                    },
                );
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
                    visible: self.snapshot.thinking.visible,
                    mode: self.snapshot.thinking.mode,
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
            SessionLiveEventKind::AssistantReasoningActivity { turn_id, event } => {
                self.apply_live_reasoning_activity(turn_id, event);
            }
            SessionLiveEventKind::ToolContributionPlaced { envelope } => {
                self.apply_contribution_event(0, None, &envelope.contribution, envelope.placement);
            }
            SessionLiveEventKind::ToolRequestDraft { event } => {
                self.apply_tool_request_draft(event);
            }
            SessionLiveEventKind::ToolInvocationProgress { event } => {
                if event.stage == bcode_session_models::ToolInvocationLifecycleStage::Progress
                    && !self.terminal_invocations.contains(&event.invocation_id)
                {
                    self.snapshot
                        .active_invocations
                        .insert(event.invocation_id.clone(), event.clone());
                    self.bump_revision();
                }
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
                self.set_context_occupancy((**occupancy).clone());
            }
        }
    }

    #[allow(clippy::too_many_lines)] // One state-machine handler keeps generation/revision/offset transitions atomic.
    fn apply_tool_request_draft(&mut self, event: &bcode_session_models::ToolRequestDraftEvent) {
        use bcode_session_models::ToolRequestDraftOperation;

        let key = event.tool_call_id.clone();
        let id = tool_request_draft_item_id(event);
        let current = self.tool_request_drafts.get(&key).cloned();
        let terminal_update = matches!(event.operation, ToolRequestDraftOperation::Remove { .. });
        if self
            .terminal_tool_request_drafts
            .get(&key)
            .is_some_and(|(generation, revision)| {
                event.generation < *generation
                    || (event.generation == *generation && event.revision <= *revision)
            })
            || current.as_ref().is_some_and(|current| {
                event.generation < current.generation
                    || (event.generation == current.generation
                        && (event.revision < current.revision
                            || (event.revision == current.revision
                                && !(terminal_update && event.revision == u64::MAX))))
            })
        {
            return;
        }
        if current
            .as_ref()
            .is_some_and(|current| event.generation > current.generation)
        {
            self.tool_request_drafts.remove(&key);
        }
        if let ToolRequestDraftOperation::Remove { .. } = event.operation {
            self.tool_request_drafts.remove(&key);
            self.terminal_tool_request_drafts
                .insert(key, (event.generation, event.revision.saturating_add(1)));
            let authoritative_result = self
                .snapshot
                .tools
                .get(&event.tool_call_id)
                .is_some_and(|tool| is_terminal_tool_status(tool.status));
            if !authoritative_result {
                self.snapshot.transcript.items.retain(|item| item.id != id);
                self.snapshot.transcript.revision =
                    self.snapshot.transcript.revision.saturating_add(1);
            }
            self.bump_revision();
            return;
        }
        self.terminal_tool_request_drafts.remove(&key);
        let mut draft = current
            .filter(|current| current.generation == event.generation)
            .unwrap_or_else(|| ToolRequestDraftView {
                turn_id: event.turn_id.clone(),
                tool_call_id: event.tool_call_id.clone(),
                tool_name: event.tool_name.clone(),
                producer_plugin_id: event.producer_plugin_id.clone(),
                schema: event.schema.clone(),
                schema_version: event.schema_version,
                placement: event.placement,
                generation: event.generation,
                revision: 0,
                argument_bytes: 0,
                preview_start_offset: 0,
                preview: String::new(),
                truncated: false,
            });
        match &event.operation {
            ToolRequestDraftOperation::Append { offset, text } => {
                let expected = draft
                    .preview_start_offset
                    .saturating_add(draft.preview.len());
                if *offset != expected {
                    return;
                }
                draft.preview.push_str(text);
            }
            ToolRequestDraftOperation::Checkpoint { start_offset, text } => {
                draft.preview_start_offset = *start_offset;
                draft.preview.clone_from(text);
            }
            ToolRequestDraftOperation::Remove { .. } => unreachable!(),
        }
        draft.turn_id.clone_from(&event.turn_id);
        draft.tool_name.clone_from(&event.tool_name);
        draft
            .producer_plugin_id
            .clone_from(&event.producer_plugin_id);
        draft.schema.clone_from(&event.schema);
        draft.schema_version = event.schema_version;
        draft.placement = event.placement;
        draft.revision = event.revision;
        draft.argument_bytes = event.argument_bytes;
        draft.truncated = event.truncated;
        self.tool_request_drafts.insert(key, draft.clone());
        let slot_has_contribution = self
            .snapshot
            .transcript
            .items
            .iter()
            .find(|item| item.id == id)
            .is_some_and(|item| {
                matches!(item.kind, TranscriptViewItemKind::ToolContribution { .. })
            });
        if slot_has_contribution
            || self
                .snapshot
                .tools
                .get(&event.tool_call_id)
                .is_some_and(|tool| is_terminal_tool_status(tool.status))
        {
            return;
        }
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| item.id == id)
        {
            item.kind = TranscriptViewItemKind::ToolRequestDraft { draft };
            item.streaming = true;
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
            return;
        }
        self.push_item(
            id,
            0,
            None,
            true,
            TranscriptViewItemKind::ToolRequestDraft { draft },
        );
    }

    fn refresh_reasoning_items(&mut self) {
        let visible = self.snapshot.thinking.visible;
        let mode = self.snapshot.thinking.mode;
        let mut changed = false;
        for ((turn_id, activity_id), activity) in &self.live_reasoning {
            let item_id = TranscriptViewItemId::reasoning(turn_id, activity_id);
            if let Some(item) = self
                .snapshot
                .transcript
                .items
                .iter_mut()
                .find(|item| item.id == item_id)
            {
                let next =
                    live_reasoning_activity_view(turn_id, activity_id, activity, visible, mode);
                let item_changed = match &mut item.kind {
                    TranscriptViewItemKind::ReasoningActivity { activity } => {
                        if activity == &next {
                            false
                        } else {
                            *activity = next;
                            true
                        }
                    }
                    TranscriptViewItemKind::ReasoningMessage { .. } => {
                        let text = filtered_reasoning_text(activity.parts.values(), visible, mode);
                        replace_text_in_item(item, &text)
                    }
                    _ => false,
                };
                if item_changed {
                    item.revision = item.revision.saturating_add(1);
                    changed = true;
                }
            }
        }
        if changed {
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        }
    }

    fn apply_live_reasoning_activity(
        &mut self,
        turn_id: &str,
        event: &bcode_session_models::ReasoningActivityEvent,
    ) {
        let key = (turn_id.to_owned(), event.activity_id().to_owned());
        self.live_reasoning
            .entry(key.clone())
            .or_default()
            .apply(event);
        let activity = self
            .live_reasoning
            .get(&key)
            .expect("live reasoning activity was inserted");
        let item_id = TranscriptViewItemId::reasoning(turn_id, event.activity_id());
        let view = live_reasoning_activity_view(
            turn_id,
            event.activity_id(),
            activity,
            self.snapshot.thinking.visible,
            self.snapshot.thinking.mode,
        );
        let streaming = !activity.finished;
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| item.id == item_id)
        {
            item.kind = TranscriptViewItemKind::ReasoningActivity { activity: view };
            item.streaming = streaming;
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
        } else {
            self.push_item(
                item_id,
                0,
                None,
                streaming,
                TranscriptViewItemKind::ReasoningActivity { activity: view },
            );
        }
    }

    fn apply_contribution_event(
        &mut self,
        event_sequence: u64,
        timestamp_ms: Option<u64>,
        contribution: &bcode_session_models::ToolContributionEvent,
        placement: bcode_session_models::ToolContributionPlacement,
    ) {
        if self
            .terminal_invocations
            .contains(&contribution.invocation_id)
            && contribution.persistence
                == bcode_session_models::ToolContributionPersistence::Transient
        {
            return;
        }
        let key = format!(
            "{}:{}",
            contribution.invocation_id, contribution.contribution_id
        );
        if self
            .contribution_sequences
            .get(&key)
            .is_some_and(|sequence| contribution.sequence <= *sequence)
        {
            return;
        }
        self.contribution_sequences
            .insert(key.clone(), contribution.sequence);
        let previous_placement = self.contribution_placements.get(&key).copied();
        let previous_item_id =
            previous_placement.map(|previous| contribution_item_id(contribution, previous));
        let item_id = contribution_item_id(contribution, placement);
        match contribution.operation {
            bcode_session_models::ToolContributionOperation::Remove => {
                self.snapshot.contributions.remove(&key);
                self.contribution_placements.remove(&key);
                if let Some(previous_item_id) = previous_item_id.as_ref() {
                    self.remove_owned_contribution_slot_item(previous_item_id, &key);
                }
                if previous_item_id.as_ref() != Some(&item_id) {
                    self.remove_owned_contribution_slot_item(&item_id, &key);
                }
            }
            bcode_session_models::ToolContributionOperation::Upsert
            | bcode_session_models::ToolContributionOperation::Append => {
                self.snapshot
                    .contributions
                    .insert(key.clone(), contribution.clone());
                self.contribution_placements.insert(key.clone(), placement);
                if let Some(previous_item_id) = previous_item_id.as_ref()
                    && previous_item_id != &item_id
                {
                    self.remove_contribution_slot_item(previous_item_id);
                }
                if placement == bcode_session_models::ToolContributionPlacement::Hidden {
                    self.remove_contribution_slot_item(&item_id);
                } else if placement == bcode_session_models::ToolContributionPlacement::Result
                    && self
                        .snapshot
                        .tools
                        .get(&contribution.invocation_id)
                        .is_some_and(|tool| is_terminal_tool_status(tool.status))
                {
                    self.remove_owned_contribution_slot_item(&item_id, &key);
                } else {
                    self.upsert_contribution_slot_item(
                        item_id,
                        key,
                        event_sequence,
                        timestamp_ms,
                        contribution.persistence
                            == bcode_session_models::ToolContributionPersistence::Transient,
                        contribution.clone(),
                        placement,
                    );
                }
            }
        }
        self.bump_revision();
    }

    fn remove_owned_contribution_slot_item(&mut self, id: &TranscriptViewItemId, owner_key: &str) {
        if self
            .contribution_slot_owners
            .get(id)
            .is_some_and(|owner| owner == owner_key)
        {
            self.remove_contribution_slot_item(id);
        }
    }

    fn remove_contribution_slot_item(&mut self, id: &TranscriptViewItemId) {
        let Some(index) = self.contribution_slot_items.remove(id) else {
            return;
        };
        self.contribution_slot_owners.remove(id);
        self.snapshot.transcript.items.remove(index);
        for slot_index in self.contribution_slot_items.values_mut() {
            if *slot_index > index {
                *slot_index = slot_index.saturating_sub(1);
            }
        }
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_contribution_slot_item(
        &mut self,
        id: TranscriptViewItemId,
        owner_key: String,
        sequence: u64,
        timestamp_ms: Option<u64>,
        streaming: bool,
        contribution: bcode_session_models::ToolContributionEvent,
        placement: bcode_session_models::ToolContributionPlacement,
    ) {
        let invocation = self
            .snapshot
            .tools
            .get(&contribution.invocation_id)
            .cloned()
            .map(Box::new);
        let streaming = invocation.as_ref().is_some_and(|invocation| {
            matches!(invocation.status, ToolInvocationViewStatus::Running)
        }) || streaming;
        if let Some(index) = self.contribution_slot_items.get(&id).copied()
            && let Some(item) = self.snapshot.transcript.items.get_mut(index)
        {
            if let Some(previous_owner) = self
                .contribution_slot_owners
                .insert(id.clone(), owner_key.clone())
                && previous_owner != owner_key
            {
                self.contribution_placements.remove(&previous_owner);
            }
            item.kind = TranscriptViewItemKind::ToolContribution {
                contribution,
                placement,
                invocation,
            };
            item.streaming = streaming;
            item.sequence = (sequence != 0).then_some(sequence).or(item.sequence);
            item.timestamp_ms = timestamp_ms.or(item.timestamp_ms);
            item.revision = item.revision.saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
            return;
        }
        let index = self.snapshot.transcript.items.len();
        self.contribution_slot_items.insert(id.clone(), index);
        self.contribution_slot_owners.insert(id.clone(), owner_key);
        self.push_item(
            id,
            sequence,
            timestamp_ms,
            streaming,
            TranscriptViewItemKind::ToolContribution {
                contribution,
                placement,
                invocation,
            },
        );
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

    fn apply_terminal_tool_status(
        &mut self,
        tool_call_id: &str,
        status: ToolInvocationViewStatus,
        message: Option<&str>,
        sequence: u64,
        timestamp_ms: Option<u64>,
    ) {
        let Some(mut tool) = self.snapshot.tools.get(tool_call_id).cloned() else {
            return;
        };
        tool.status = status;
        if matches!(status, ToolInvocationViewStatus::Failed) {
            tool.is_error = Some(true);
        }
        if matches!(
            status,
            ToolInvocationViewStatus::Cancelled | ToolInvocationViewStatus::Failed
        ) && tool.result.is_none()
            && tool.result_text.is_none()
        {
            tool.result_text = message.map(ToOwned::to_owned);
        }
        self.snapshot
            .tools
            .insert(tool_call_id.to_owned(), tool.clone());
        if let Some(id) = self.tool_item_ids.get(tool_call_id).cloned() {
            self.update_existing_tool_item(id, tool_call_id, sequence, timestamp_ms, tool);
        }
        self.sync_contribution_invocation_context(tool_call_id);
    }

    fn upsert_tool_item(&mut self, tool_call_id: &str, sequence: u64, timestamp_ms: Option<u64>) {
        let Some(projection) = self.tool_invocation_projections.get(tool_call_id).cloned() else {
            return;
        };
        let mut tool = tool_invocation_view_from_projection(projection);
        if let Some(status) = self.terminal_invocation_statuses.get(tool_call_id).copied() {
            tool.status = status;
            if matches!(status, ToolInvocationViewStatus::Failed) {
                tool.is_error = Some(true);
            }
        }
        self.snapshot
            .tools
            .insert(tool_call_id.to_owned(), tool.clone());
        if is_terminal_tool_status(tool.status) {
            self.upsert_terminal_tool_item(tool_call_id, sequence, timestamp_ms, tool);
            return;
        }
        match self.tool_item_ids.entry(tool_call_id.to_owned()) {
            Entry::Occupied(entry) => {
                let id = entry.get().clone();
                self.update_existing_tool_item(id, tool_call_id, sequence, timestamp_ms, tool);
            }
            Entry::Vacant(_) => {
                let id = TranscriptViewItemId::tool_presentation_slot(
                    tool_call_id,
                    bcode_session_models::ToolContributionPlacement::Request,
                    None,
                );
                let id = self.push_item(
                    id,
                    sequence,
                    timestamp_ms,
                    matches!(tool.status, ToolInvocationViewStatus::Running),
                    TranscriptViewItemKind::ToolInvocation {
                        tool: Box::new(tool),
                    },
                );
                if id
                    == TranscriptViewItemId::tool_presentation_slot(
                        tool_call_id,
                        bcode_session_models::ToolContributionPlacement::Request,
                        None,
                    )
                {
                    self.contribution_slot_items.insert(
                        id.clone(),
                        self.snapshot.transcript.items.len().saturating_sub(1),
                    );
                }
                self.tool_item_ids.insert(tool_call_id.to_owned(), id);
            }
        }
        self.sync_contribution_invocation_context(tool_call_id);
    }

    fn upsert_terminal_tool_item(
        &mut self,
        tool_call_id: &str,
        sequence: u64,
        timestamp_ms: Option<u64>,
        tool: ToolInvocationView,
    ) {
        let request_tool = tool.clone();
        if let Some(request_id) = self.tool_item_ids.remove(tool_call_id)
            && let Some(index) = self
                .snapshot
                .transcript
                .items
                .iter()
                .position(|item| item.id == request_id)
            && matches!(
                self.snapshot.transcript.items[index].kind,
                TranscriptViewItemKind::ToolInvocation { .. }
            )
        {
            self.snapshot.transcript.items[index].kind = TranscriptViewItemKind::ToolRequest {
                tool: Box::new(request_tool),
            };
            self.snapshot.transcript.items[index].streaming = false;
            self.snapshot.transcript.items[index].revision = self.snapshot.transcript.items[index]
                .revision
                .saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        }
        let result_id = TranscriptViewItemId::tool_presentation_slot(
            tool_call_id,
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        if let Some(index) = self
            .snapshot
            .transcript
            .items
            .iter()
            .position(|item| item.id == result_id)
        {
            self.replace_tool_item(index, tool, tool_call_id, sequence, timestamp_ms);
        } else {
            self.push_item(
                result_id,
                sequence,
                timestamp_ms,
                false,
                TranscriptViewItemKind::ToolInvocation {
                    tool: Box::new(tool),
                },
            );
        }
        self.sync_contribution_invocation_context(tool_call_id);
    }

    fn sync_contribution_invocation_context(&mut self, tool_call_id: &str) {
        let invocation = self.snapshot.tools.get(tool_call_id).cloned().map(Box::new);
        let streaming = invocation.as_ref().is_some_and(|invocation| {
            matches!(invocation.status, ToolInvocationViewStatus::Running)
        });
        let mut changed = false;
        for item in &mut self.snapshot.transcript.items {
            let TranscriptViewItemKind::ToolContribution {
                contribution,
                invocation: item_invocation,
                ..
            } = &mut item.kind
            else {
                continue;
            };
            if contribution.invocation_id != tool_call_id
                || (*item_invocation == invocation && item.streaming == streaming)
            {
                continue;
            }
            item_invocation.clone_from(&invocation);
            item.streaming = streaming;
            item.revision = item.revision.saturating_add(1);
            changed = true;
        }
        if changed {
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
        }
    }

    fn update_existing_tool_item(
        &mut self,
        id: TranscriptViewItemId,
        tool_call_id: &str,
        sequence: u64,
        timestamp_ms: Option<u64>,
        tool: ToolInvocationView,
    ) {
        let Some(index) = self
            .snapshot
            .transcript
            .items
            .iter()
            .position(|item| item.id == id)
        else {
            return;
        };
        if is_terminal_tool_status(tool.status)
            && matches!(
                self.snapshot.transcript.items[index].kind,
                TranscriptViewItemKind::ToolContribution { .. }
            )
        {
            self.contribution_slot_items.remove(&id);
            self.contribution_slot_owners.remove(&id);
            self.replace_tool_item(index, tool, tool_call_id, sequence, timestamp_ms);
            return;
        }
        let existing_tool = match &self.snapshot.transcript.items[index].kind {
            TranscriptViewItemKind::ToolInvocation { tool } => Some((**tool).clone()),
            TranscriptViewItemKind::ToolRequest { .. }
            | TranscriptViewItemKind::ToolRequestDraft { .. }
            | TranscriptViewItemKind::AssistantMessage { .. }
            | TranscriptViewItemKind::ReasoningMessage { .. }
            | TranscriptViewItemKind::ReasoningActivity { .. }
            | TranscriptViewItemKind::UserMessage { .. }
            | TranscriptViewItemKind::SystemMessage { .. }
            | TranscriptViewItemKind::Permission { .. }
            | TranscriptViewItemKind::RuntimeWork { .. }
            | TranscriptViewItemKind::Usage { .. }
            | TranscriptViewItemKind::Compaction { .. }
            | TranscriptViewItemKind::Skill { .. }
            | TranscriptViewItemKind::Interaction { .. }
            | TranscriptViewItemKind::ToolContribution { .. } => None,
        };
        if should_split_terminal_tool_item(existing_tool.as_ref(), &tool) {
            self.split_terminal_tool_item(
                index,
                id,
                tool_call_id,
                (sequence, timestamp_ms),
                existing_tool.expect("split requires existing tool"),
                tool,
            );
        } else {
            self.replace_tool_item(index, tool, tool_call_id, sequence, timestamp_ms);
        }
    }

    fn split_terminal_tool_item(
        &mut self,
        index: usize,
        id: TranscriptViewItemId,
        tool_call_id: &str,
        event_metadata: (u64, Option<u64>),
        existing_tool: ToolInvocationView,
        tool: ToolInvocationView,
    ) {
        let item = &mut self.snapshot.transcript.items[index];
        item.id = TranscriptViewItemId::tool_request(tool_call_id);
        item.kind = TranscriptViewItemKind::ToolRequest {
            tool: Box::new(existing_tool),
        };
        item.streaming = false;
        item.revision = item.revision.saturating_add(1);
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        self.push_item(
            id,
            event_metadata.0,
            event_metadata.1,
            false,
            TranscriptViewItemKind::ToolInvocation {
                tool: Box::new(tool),
            },
        );
    }

    fn replace_tool_item(
        &mut self,
        index: usize,
        tool: ToolInvocationView,
        tool_call_id: &str,
        sequence: u64,
        timestamp_ms: Option<u64>,
    ) {
        let item = &mut self.snapshot.transcript.items[index];
        item.kind = TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(tool),
        };
        item.sequence = (sequence != 0).then_some(sequence).or(item.sequence);
        item.timestamp_ms = timestamp_ms.or(item.timestamp_ms);
        item.streaming = matches!(
            self.snapshot.tools[tool_call_id].status,
            ToolInvocationViewStatus::Running
        );
        item.revision = item.revision.saturating_add(1);
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        self.bump_revision();
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
        if let Some(item) = streaming_delta_target_mut(&mut self.snapshot.transcript.items, kind) {
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
        if self.finish_split_streaming_message(kind) {
            return;
        }
        if let Some(item) = streaming_finish_target_mut(&mut self.snapshot.transcript.items, kind) {
            item.sequence = (sequence != 0).then_some(sequence).or(item.sequence);
            item.timestamp_ms = timestamp_ms.or(item.timestamp_ms);
            let _ = replace_text_in_item(item, text);
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

    fn finish_split_streaming_message(&mut self, kind: StreamingMessageKind) -> bool {
        if !matches!(kind, StreamingMessageKind::Reasoning) {
            return false;
        }
        let matching_stream_count = self
            .snapshot
            .transcript
            .items
            .iter()
            .filter(|item| item.streaming && streaming_item_matches(&item.kind, kind))
            .count();
        if matching_stream_count <= 1 {
            return false;
        }
        for item in self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .filter(|item| item.streaming && streaming_item_matches(&item.kind, kind))
        {
            item.streaming = false;
            item.revision = item.revision.saturating_add(1);
        }
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        self.bump_revision();
        true
    }
}

fn contribution_item_id(
    contribution: &bcode_session_models::ToolContributionEvent,
    placement: bcode_session_models::ToolContributionPlacement,
) -> TranscriptViewItemId {
    TranscriptViewItemId::tool_presentation_slot(
        &contribution.invocation_id,
        placement,
        (placement == bcode_session_models::ToolContributionPlacement::Supplemental)
            .then_some(contribution.contribution_id.as_str()),
    )
}

fn tool_request_draft_item_id(
    draft: &bcode_session_models::ToolRequestDraftEvent,
) -> TranscriptViewItemId {
    TranscriptViewItemId::tool_presentation_slot(&draft.tool_call_id, draft.placement, None)
}

fn working_directory_changed_message(
    old_working_directory: &std::path::Path,
    new_working_directory: &std::path::Path,
) -> String {
    use bcode_plugin_sdk::path::display;

    format!(
        "Working directory changed from `{}` to `{}`. Treat prior file/path assumptions as possibly stale unless reconfirmed.",
        display(old_working_directory, old_working_directory),
        display(new_working_directory, old_working_directory)
    )
}

const fn provider_compaction_origin_label(
    origin: bcode_session_models::ProviderContextSnapshotOrigin,
) -> &'static str {
    match origin {
        bcode_session_models::ProviderContextSnapshotOrigin::Explicit => "explicit provider-native",
        bcode_session_models::ProviderContextSnapshotOrigin::ProviderManaged => "provider-managed",
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
        } => format!(
            "assembling {tool_name} arguments ({} received)",
            format_provider_bytes(*argument_bytes)
        ),
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

fn format_provider_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if bytes >= MIB {
        let whole = bytes / MIB;
        let decimal = (bytes % MIB) * 10 / MIB;
        format!("{whole}.{decimal} MiB")
    } else if bytes >= KIB {
        let whole = bytes / KIB;
        let decimal = (bytes % KIB) * 10 / KIB;
        format!("{whole}.{decimal} KiB")
    } else {
        format!("{bytes} B")
    }
}

fn tool_invocation_view_from_projection(
    projection: ToolInvocationProjection,
) -> ToolInvocationView {
    let timing = ToolTimingView {
        started_at_ms: projection.started_at_ms,
        finished_at_ms: projection.finished_at_ms,
        timeout_ms: projection_timeout_ms(projection.raw_result.as_ref()),
        timed_out: projection_timed_out(projection.raw_result.as_ref()),
        duration_ms: projection_duration_ms(projection.raw_result.as_ref()),
    };
    ToolInvocationView {
        tool_call_id: projection.tool_call_id,
        producer_plugin_id: projection.producer_plugin_id,
        tool_name: projection.tool_name,
        arguments_json: projection.arguments_json,
        working_directory: projection.working_directory,
        status: projection.status.into(),
        result_text: projection.result_text,
        is_error: projection.is_error,
        result: projection.raw_result.map(ToolResultView::from),
        timing,
    }
}

fn projection_result_metadata(
    result: Option<&bcode_session_models::ToolInvocationResult>,
) -> Option<&serde_json::Value> {
    let bcode_session_models::ToolInvocationResult::Artifact { artifact } = result? else {
        return None;
    };
    Some(&artifact.metadata)
}

fn projection_timeout_ms(
    result: Option<&bcode_session_models::ToolInvocationResult>,
) -> Option<u64> {
    projection_result_metadata(result)?
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64)
}

fn projection_timed_out(
    result: Option<&bcode_session_models::ToolInvocationResult>,
) -> Option<bool> {
    projection_result_metadata(result)?
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
}

fn projection_duration_ms(
    result: Option<&bcode_session_models::ToolInvocationResult>,
) -> Option<u64> {
    projection_result_metadata(result)?
        .get("duration_ms")
        .and_then(serde_json::Value::as_u64)
}

fn should_split_terminal_tool_item(
    existing_tool: Option<&ToolInvocationView>,
    next_tool: &ToolInvocationView,
) -> bool {
    is_terminal_tool_status(next_tool.status)
        && existing_tool.is_some_and(|tool| !is_terminal_tool_status(tool.status))
}

const fn is_terminal_tool_status(status: ToolInvocationViewStatus) -> bool {
    matches!(
        status,
        ToolInvocationViewStatus::Finished
            | ToolInvocationViewStatus::Cancelled
            | ToolInvocationViewStatus::Failed
    )
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
            display_label: None,
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

fn streaming_delta_target_mut(
    items: &mut [TranscriptViewItem],
    kind: StreamingMessageKind,
) -> Option<&mut TranscriptViewItem> {
    match kind {
        StreamingMessageKind::Assistant => items
            .iter_mut()
            .rev()
            .find(|item| item.streaming && streaming_item_matches(&item.kind, kind)),
        StreamingMessageKind::Reasoning => items
            .last_mut()
            .filter(|item| item.streaming && streaming_item_matches(&item.kind, kind)),
    }
}

fn streaming_finish_target_mut(
    items: &mut [TranscriptViewItem],
    kind: StreamingMessageKind,
) -> Option<&mut TranscriptViewItem> {
    match kind {
        StreamingMessageKind::Assistant => items
            .iter_mut()
            .rev()
            .find(|item| item.streaming && streaming_item_matches(&item.kind, kind)),
        StreamingMessageKind::Reasoning => items
            .last_mut()
            .filter(|item| item.streaming && streaming_item_matches(&item.kind, kind)),
    }
}

fn append_text_to_item(item: &mut TranscriptViewItem, text: &str) {
    match &mut item.kind {
        TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => message.text.push_str(text),
        TranscriptViewItemKind::ReasoningActivity { .. }
        | TranscriptViewItemKind::ToolInvocation { .. }
        | TranscriptViewItemKind::ToolRequestDraft { .. }
        | TranscriptViewItemKind::ToolRequest { .. }
        | TranscriptViewItemKind::Permission { .. }
        | TranscriptViewItemKind::RuntimeWork { .. }
        | TranscriptViewItemKind::Usage { .. }
        | TranscriptViewItemKind::Compaction { .. }
        | TranscriptViewItemKind::Skill { .. }
        | TranscriptViewItemKind::Interaction { .. }
        | TranscriptViewItemKind::ToolContribution { .. } => {}
    }
}

fn live_reasoning_activity_view(
    turn_id: &str,
    activity_id: &str,
    activity: &LiveReasoningActivity,
    visible: bool,
    mode: bcode_session_view_models::ReasoningDisplayMode,
) -> bcode_session_view_models::ReasoningActivityView {
    let mut parts = activity.parts.values().cloned().collect::<Vec<_>>();
    parts.sort_by_key(|part| (part.order, part.kind, part.part_id.clone()));
    if visible {
        parts.retain(|part| reasoning_part_selected(part.kind, mode));
    } else {
        parts.clear();
    }
    bcode_session_view_models::ReasoningActivityView {
        turn_id: turn_id.to_owned(),
        activity_id: activity_id.to_owned(),
        order: activity.order,
        status: if activity.finished {
            bcode_session_models::ReasoningActivityStatus::Completed
        } else {
            bcode_session_models::ReasoningActivityStatus::Interrupted
        },
        parts,
        opaque: activity.opaque,
    }
}

const fn reasoning_part_selected(
    kind: bcode_session_models::ReasoningContentKind,
    mode: bcode_session_view_models::ReasoningDisplayMode,
) -> bool {
    match mode {
        bcode_session_view_models::ReasoningDisplayMode::All => true,
        bcode_session_view_models::ReasoningDisplayMode::Summary => matches!(
            kind,
            bcode_session_models::ReasoningContentKind::Summary
                | bcode_session_models::ReasoningContentKind::Legacy
        ),
        bcode_session_view_models::ReasoningDisplayMode::Raw => {
            matches!(kind, bcode_session_models::ReasoningContentKind::Raw)
        }
    }
}

fn reasoning_activity_view(
    turn_id: &str,
    activity: &bcode_session_models::ReasoningActivity,
    visible: bool,
    mode: bcode_session_view_models::ReasoningDisplayMode,
) -> bcode_session_view_models::ReasoningActivityView {
    let mut parts = activity.parts.clone();
    parts.sort_by_key(|part| (part.order, part.kind, part.part_id.clone()));
    if visible {
        parts.retain(|part| reasoning_part_selected(part.kind, mode));
    } else {
        parts.clear();
    }
    bcode_session_view_models::ReasoningActivityView {
        turn_id: turn_id.to_owned(),
        activity_id: activity.activity_id.clone(),
        order: activity.order,
        status: activity.status,
        parts,
        opaque: activity.opaque,
    }
}

fn filtered_reasoning_text<'a>(
    parts: impl Iterator<Item = &'a bcode_session_models::ReasoningPart>,
    visible: bool,
    mode: bcode_session_view_models::ReasoningDisplayMode,
) -> String {
    if !visible {
        return String::new();
    }
    let mut parts = parts.collect::<Vec<_>>();
    parts.sort_by_key(|part| (part.order, part.kind, part.part_id.as_str()));
    parts
        .into_iter()
        .filter(|part| match mode {
            bcode_session_view_models::ReasoningDisplayMode::All => true,
            bcode_session_view_models::ReasoningDisplayMode::Summary => matches!(
                part.kind,
                bcode_session_models::ReasoningContentKind::Summary
                    | bcode_session_models::ReasoningContentKind::Legacy
            ),
            bcode_session_view_models::ReasoningDisplayMode::Raw => {
                matches!(part.kind, bcode_session_models::ReasoningContentKind::Raw)
            }
        })
        .map(|part| part.text.as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn replace_text_in_item(item: &mut TranscriptViewItem, text: &str) -> bool {
    match &mut item.kind {
        TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => {
            if message.text == text {
                false
            } else {
                text.clone_into(&mut message.text);
                true
            }
        }
        TranscriptViewItemKind::ReasoningActivity { .. }
        | TranscriptViewItemKind::ToolInvocation { .. }
        | TranscriptViewItemKind::ToolRequestDraft { .. }
        | TranscriptViewItemKind::ToolRequest { .. }
        | TranscriptViewItemKind::Permission { .. }
        | TranscriptViewItemKind::RuntimeWork { .. }
        | TranscriptViewItemKind::Usage { .. }
        | TranscriptViewItemKind::Compaction { .. }
        | TranscriptViewItemKind::Skill { .. }
        | TranscriptViewItemKind::Interaction { .. }
        | TranscriptViewItemKind::ToolContribution { .. } => false,
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

fn provider_to_display_selection(provider: &str) -> Option<String> {
    if provider == "<auto>" || provider.is_empty() {
        None
    } else {
        Some(provider.to_owned())
    }
}

fn model_to_display_selection(model: &str) -> Option<String> {
    if model == "<default>" || model.is_empty() {
        None
    } else {
        Some(model.to_owned())
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
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, LocalContextEstimate, ModelRequestIdentity,
        RequestContextObservation, RequestContextTokenCount, SessionEvent, SessionEventKind,
        SessionId, SessionLiveEvent, SessionLiveEventKind, SessionTokenUsage, ToolInvocationResult,
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

    fn assert_reasoning_text(item: &TranscriptViewItem, text: &str, streaming: bool) {
        assert_eq!(item.streaming, streaming);
        assert!(match &item.kind {
            TranscriptViewItemKind::ReasoningMessage { message } => message.text == text,
            TranscriptViewItemKind::ReasoningActivity { activity } => activity.text() == text,
            _ => false,
        });
    }

    #[allow(clippy::too_many_lines)]
    fn durable_generic_history(session_id: SessionId) -> Vec<SessionEvent> {
        let lifecycle = |sequence, stage, message| {
            event(
                session_id,
                sequence,
                SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: "call".to_owned(),
                        sequence,
                        stage,
                        message,
                        metadata: serde_json::json!({"opaque": sequence}),
                    },
                },
            )
        };
        let contribution = |source_sequence, contribution_sequence, operation, payload| {
            event(
                session_id,
                source_sequence,
                SessionEventKind::ToolContribution {
                    event: bcode_session_models::ToolContributionEvent {
                        invocation_id: "call".to_owned(),
                        contribution_id: "surface".to_owned(),
                        sequence: contribution_sequence,
                        producer_id: "future.producer".to_owned(),
                        schema: "future.unknown/schema".to_owned(),
                        schema_version: 77,
                        operation,
                        persistence: bcode_session_models::ToolContributionPersistence::Durable,
                        artifact: None,
                        payload,
                    },
                },
            )
        };
        vec![
            event(
                session_id,
                1,
                SessionEventKind::SessionCreated {
                    name: Some("deterministic".to_owned()),
                    working_directory: PathBuf::from("/tmp/deterministic"),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call".to_owned(),
                    producer_plugin_id: Some("future.producer".to_owned()),
                    tool_name: "future.tool".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            ),
            lifecycle(
                3,
                bcode_session_models::ToolInvocationLifecycleStage::Started,
                None,
            ),
            contribution(
                4,
                1,
                bcode_session_models::ToolContributionOperation::Upsert,
                serde_json::json!({"opaque": [1, 2]}),
            ),
            event(
                session_id,
                5,
                SessionEventKind::ToolExchangeRequested {
                    request: bcode_session_models::ToolExchangeRequest {
                        invocation_id: "call".to_owned(),
                        exchange_id: "exchange".to_owned(),
                        producer_id: "future.producer".to_owned(),
                        schema: "future.exchange".to_owned(),
                        schema_version: 9,
                        payload: serde_json::json!({"opaque_request": true}),
                        response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                    },
                },
            ),
            lifecycle(
                6,
                bcode_session_models::ToolInvocationLifecycleStage::Waiting,
                Some("waiting".to_owned()),
            ),
            event(
                session_id,
                7,
                SessionEventKind::ToolExchangeResolved {
                    event: bcode_session_models::ToolExchangeResolutionEvent {
                        invocation_id: "call".to_owned(),
                        exchange_id: "exchange".to_owned(),
                        resolution: bcode_session_models::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({"opaque_response": 42}),
                        },
                    },
                },
            ),
            contribution(
                8,
                2,
                bcode_session_models::ToolContributionOperation::Append,
                serde_json::json!({"future_append": true}),
            ),
            contribution(
                9,
                3,
                bcode_session_models::ToolContributionOperation::Remove,
                serde_json::Value::Null,
            ),
            event(
                session_id,
                10,
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call".to_owned(),
                        model_output: "done".to_owned(),
                        is_error: false,
                        result: Some(ToolInvocationResult::Json {
                            value: r#"{"opaque_result":true}"#.to_owned(),
                        }),
                    },
                },
            ),
            lifecycle(
                11,
                bcode_session_models::ToolInvocationLifecycleStage::Completed,
                None,
            ),
            event(
                session_id,
                12,
                SessionEventKind::SystemMessage {
                    text: "finished".to_owned(),
                },
            ),
        ]
    }

    #[test]
    fn durable_mixed_history_replays_to_byte_identical_generic_snapshots() {
        let history = durable_generic_history(SessionId::new());
        let decoded = history
            .iter()
            .map(|event| {
                let encoded = bcode_session::persisted::encode_session_event(event)
                    .expect("durable event should encode");
                bcode_session::persisted::decode_session_event(&encoded)
                    .expect("durable event should decode")
            })
            .collect::<Vec<_>>();
        assert_eq!(decoded, history);

        let first = build_session_view_snapshot(&decoded);
        let second = build_session_view_snapshot(&decoded);
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_vec(&first).expect("first snapshot should encode"),
            serde_json::to_vec(&second).expect("second snapshot should encode")
        );
        assert!(first.active_invocations.is_empty());
        assert!(first.contributions.is_empty());
        assert_eq!(first.latest_sequence, Some(12));
    }

    #[test]
    fn exchange_lifecycle_projects_opaque_active_state_and_terminal_resolution() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let request_event = bcode_session_models::ToolExchangeRequest {
            invocation_id: "call".to_owned(),
            exchange_id: "question".to_owned(),
            producer_id: "future.producer".to_owned(),
            schema: "future.question/schema".to_owned(),
            schema_version: 9,
            payload: serde_json::json!({"opaque_question": true}),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let requested = event(
            session_id,
            9,
            SessionEventKind::ToolExchangeRequested {
                request: request_event.clone(),
            },
        );
        let resolved = event(
            session_id,
            10,
            SessionEventKind::ToolExchangeResolved {
                event: bcode_session_models::ToolExchangeResolutionEvent {
                    invocation_id: "call".to_owned(),
                    exchange_id: "question".to_owned(),
                    resolution: bcode_session_models::ToolExchangeResolution::Responded {
                        payload: serde_json::json!({"opaque_answer": 42}),
                    },
                },
            },
        );
        view.apply_event(&requested);
        assert_eq!(
            view.snapshot().active_exchanges["call:question"],
            request_event
        );
        view.apply_event(&resolved);
        assert!(view.snapshot().active_exchanges.is_empty());
    }

    #[test]
    fn unknown_contribution_is_retained_without_transcript_projection() {
        let session_id = SessionId::new();
        let contribution = |source_sequence, contribution_sequence, operation, payload| {
            event(
                session_id,
                source_sequence,
                SessionEventKind::ToolContribution {
                    event: bcode_session_models::ToolContributionEvent {
                        invocation_id: "call".to_owned(),
                        contribution_id: "surface".to_owned(),
                        sequence: contribution_sequence,
                        producer_id: "future.producer".to_owned(),
                        schema: "future.unknown/schema".to_owned(),
                        schema_version: 77,
                        operation,
                        persistence: bcode_session_models::ToolContributionPersistence::Durable,
                        artifact: None,
                        payload,
                    },
                },
            )
        };
        let mut view = SessionView::new();
        view.apply_event(&contribution(
            1,
            2,
            bcode_session_models::ToolContributionOperation::Upsert,
            serde_json::json!({"opaque": [1, 2]}),
        ));
        view.apply_event(&contribution(
            2,
            1,
            bcode_session_models::ToolContributionOperation::Append,
            serde_json::json!({"late": true}),
        ));
        let projected = &view.snapshot().contributions["call:surface"];
        assert_eq!(projected.sequence, 2);
        assert_eq!(projected.payload, serde_json::json!({"opaque": [1, 2]}));
        assert!(view.snapshot().transcript.items.is_empty());

        view.apply_event(&contribution(
            3,
            3,
            bcode_session_models::ToolContributionOperation::Remove,
            serde_json::Value::Null,
        ));
        view.apply_event(&contribution(
            4,
            2,
            bcode_session_models::ToolContributionOperation::Upsert,
            serde_json::json!({"revive": true}),
        ));
        assert!(view.snapshot().contributions.is_empty());
    }

    #[test]
    fn coalesced_and_full_cadence_updates_converge_to_the_same_semantic_state() {
        let session_id = SessionId::new();
        let draft = |revision, text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-cadence".to_owned(),
                    tool_call_id: "call-cadence".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: text.to_owned(),
                    },
                    argument_bytes: text.len(),
                    truncated: false,
                },
            },
        };
        let mut full_cadence = SessionView::new();
        full_cadence.apply_live_event(&draft(1, "first"));
        full_cadence.apply_live_event(&draft(2, "second"));
        full_cadence.apply_live_event(&draft(3, "latest"));
        let mut coalesced = SessionView::new();
        coalesced.apply_live_event(&draft(3, "latest"));

        assert_eq!(
            full_cadence.tool_request_drafts,
            coalesced.tool_request_drafts
        );
        let full_item = &full_cadence.snapshot().transcript.items[0];
        let coalesced_item = &coalesced.snapshot().transcript.items[0];
        assert_eq!(full_item.id, coalesced_item.id);
        assert_eq!(full_item.kind, coalesced_item.kind);
        assert_eq!(full_item.streaming, coalesced_item.streaming);

        let remove = SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-cadence".to_owned(),
                    tool_call_id: "call-cadence".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision: 4,
                    operation: bcode_session_models::ToolRequestDraftOperation::Remove {
                        reason: bcode_session_models::ToolRequestDraftTerminalReason::Completed,
                    },
                    argument_bytes: 6,
                    truncated: false,
                },
            },
        };
        full_cadence.apply_live_event(&remove);
        coalesced.apply_live_event(&remove);
        assert!(full_cadence.tool_request_drafts.is_empty());
        assert!(coalesced.tool_request_drafts.is_empty());
        assert!(full_cadence.snapshot().transcript.items.is_empty());
        assert!(coalesced.snapshot().transcript.items.is_empty());
    }

    #[test]
    fn shared_live_delivery_attach_checkpoint_cancellation_and_finalization_are_equivalent() {
        let session_id = SessionId::new();
        let draft = |revision, operation| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-parity".to_owned(),
                    tool_call_id: "call-parity".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision,
                    operation,
                    argument_bytes: usize::try_from(revision).unwrap_or(usize::MAX),
                    truncated: false,
                },
            },
        };
        let append = draft(
            1,
            bcode_session_models::ToolRequestDraftOperation::Append {
                offset: 0,
                text: r#"{"path":"src/lib.rs","contents":"partial"}"#.to_owned(),
            },
        );
        let checkpoint = draft(
            1,
            bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                start_offset: 0,
                text: r#"{"path":"src/lib.rs","contents":"partial"}"#.to_owned(),
            },
        );
        let mut direct = SessionView::new();
        direct.apply_live_event(&append);
        let mut attached = SessionView::new();
        attached.apply_live_event(&checkpoint);
        assert_eq!(direct.snapshot(), attached.snapshot());
        assert_eq!(
            direct.snapshot().transcript.items[0].id,
            attached.snapshot().transcript.items[0].id
        );

        let cancel = draft(
            2,
            bcode_session_models::ToolRequestDraftOperation::Remove {
                reason: bcode_session_models::ToolRequestDraftTerminalReason::Cancelled,
            },
        );
        direct.apply_live_event(&cancel);
        attached.apply_live_event(&cancel);
        assert_eq!(direct.snapshot(), attached.snapshot());
        assert!(direct.snapshot().transcript.items.is_empty());

        let request = event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-parity".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.write".to_owned(),
                arguments_json: r#"{"path":"src/lib.rs","contents":"partial"}"#.to_owned(),
                working_directory: None,
            },
        );
        direct.apply_event(&request);
        attached.apply_event(&request);
        assert_eq!(direct.snapshot(), attached.snapshot());
        assert!(
            direct.snapshot().transcript.items.iter().all(|item| {
                !matches!(item.kind, TranscriptViewItemKind::ToolRequestDraft { .. })
            })
        );
    }

    #[test]
    fn request_draft_live_updates_apply_append_checkpoint_and_terminal_dominance() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |revision, operation| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-write".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision,
                    operation,
                    argument_bytes: 10,
                    truncated: false,
                },
            },
        };

        view.apply_live_event(&draft(
            1,
            bcode_session_models::ToolRequestDraftOperation::Append {
                offset: 0,
                text: "hello".to_owned(),
            },
        ));
        view.apply_live_event(&draft(
            2,
            bcode_session_models::ToolRequestDraftOperation::Append {
                offset: 5,
                text: " world".to_owned(),
            },
        ));
        let item = view
            .snapshot()
            .transcript
            .items
            .iter()
            .find(|item| matches!(item.kind, TranscriptViewItemKind::ToolRequestDraft { .. }))
            .expect("request draft item");
        let TranscriptViewItemKind::ToolRequestDraft {
            draft: projected_draft,
        } = &item.kind
        else {
            unreachable!();
        };
        assert_eq!(projected_draft.preview, "hello world");
        assert!(item.streaming);

        view.apply_live_event(&draft(
            3,
            bcode_session_models::ToolRequestDraftOperation::Remove {
                reason: bcode_session_models::ToolRequestDraftTerminalReason::Completed,
            },
        ));
        view.apply_live_event(&draft(
            2,
            bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                start_offset: 0,
                text: "stale".to_owned(),
            },
        ));
        view.apply_live_event(&draft(
            3,
            bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                start_offset: 0,
                text: "post-terminal".to_owned(),
            },
        ));
        assert!(view.tool_request_drafts.is_empty());
        assert_eq!(
            view.terminal_tool_request_drafts.get("call-write"),
            Some(&(1, 4))
        );
        assert!(
            !view
                .snapshot()
                .transcript
                .items
                .iter()
                .any(|item| matches!(item.kind, TranscriptViewItemKind::ToolRequestDraft { .. }))
        );
    }

    #[test]
    fn request_draft_append_offsets_are_utf8_byte_offsets() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |revision, offset, text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-write".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision,
                    operation: bcode_session_models::ToolRequestDraftOperation::Append {
                        offset,
                        text: text.to_owned(),
                    },
                    argument_bytes: offset.saturating_add(text.len()),
                    truncated: false,
                },
            },
        };
        let unicode = "λ🙂";
        view.apply_live_event(&draft(1, 0, unicode));
        view.apply_live_event(&draft(2, unicode.len(), "ok"));
        assert_eq!(view.tool_request_drafts["call-write"].preview, "λ🙂ok");

        let character_offset = "λ🙂ok".chars().count();
        assert_ne!(character_offset, "λ🙂ok".len());
        view.apply_live_event(&draft(3, character_offset, "wrong-unit"));
        assert_eq!(view.tool_request_drafts["call-write"].preview, "λ🙂ok");
    }

    #[test]
    fn request_draft_flood_keeps_one_item_and_latest_preview() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for revision in 1..=10_000 {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        turn_id: "turn-1".to_owned(),
                        tool_call_id: "call-write".to_owned(),
                        tool_name: "filesystem.write".to_owned(),
                        producer_plugin_id: Some("bcode.filesystem".to_owned()),
                        schema: "bcode.filesystem.request-draft.write".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Request,
                        generation: 1,
                        revision,
                        operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                            start_offset: 0,
                            text: revision.to_string(),
                        },
                        argument_bytes: usize::try_from(revision).expect("bounded revision"),
                        truncated: false,
                    },
                },
            });
            assert_eq!(view.tool_request_drafts.len(), 1);
            assert_eq!(view.snapshot().transcript.items.len(), 1);
        }

        assert_eq!(view.tool_request_drafts["call-write"].preview, "10000");
        assert_eq!(view.snapshot().transcript.items.len(), 1);
    }

    #[test]
    fn transient_contribution_projects_live_and_remove_is_terminal() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let live = |sequence, operation, payload| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Hidden,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call".to_owned(),
                        contribution_id: "surface".to_owned(),
                        sequence,
                        producer_id: "future.producer".to_owned(),
                        schema: "future.unknown/schema".to_owned(),
                        schema_version: 77,
                        operation,
                        persistence: bcode_session_models::ToolContributionPersistence::Transient,
                        artifact: None,
                        payload,
                    },
                ),
            },
        };

        view.apply_live_event(&live(
            1,
            bcode_session_models::ToolContributionOperation::Upsert,
            serde_json::json!({"opaque": 1}),
        ));
        view.apply_live_event(&live(
            2,
            bcode_session_models::ToolContributionOperation::Append,
            serde_json::json!({"opaque": 2}),
        ));
        let durable = event(
            session_id,
            10,
            SessionEventKind::ToolContribution {
                event: bcode_session_models::ToolContributionEvent {
                    invocation_id: "call".to_owned(),
                    contribution_id: "surface".to_owned(),
                    sequence: 3,
                    producer_id: "future.producer".to_owned(),
                    schema: "future.unknown/schema".to_owned(),
                    schema_version: 77,
                    operation: bcode_session_models::ToolContributionOperation::Upsert,
                    persistence: bcode_session_models::ToolContributionPersistence::Durable,
                    artifact: None,
                    payload: serde_json::json!({"opaque": "durable"}),
                },
            },
        );
        view.apply_event(&durable);
        assert_eq!(
            view.snapshot().contributions["call:surface"].payload,
            serde_json::json!({"opaque": "durable"})
        );
        assert_eq!(view.snapshot().transcript.items.len(), 0);

        view.apply_live_event(&live(
            4,
            bcode_session_models::ToolContributionOperation::Remove,
            serde_json::Value::Null,
        ));
        view.apply_live_event(&live(
            2,
            bcode_session_models::ToolContributionOperation::Upsert,
            serde_json::json!({"revive": true}),
        ));
        assert!(view.snapshot().contributions.is_empty());
        assert_eq!(view.snapshot().transcript.items.len(), 0);
    }

    #[test]
    fn newer_generation_replaces_preview_and_older_generation_cannot_revive() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |generation, revision, text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: format!("turn-{generation}"),
                    tool_call_id: "call-write".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation,
                    revision,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: text.to_owned(),
                    },
                    argument_bytes: text.len(),
                    truncated: false,
                },
            },
        };

        view.apply_live_event(&draft(1, 8, "old preview"));
        view.apply_live_event(&draft(2, 1, "replacement preview"));
        view.apply_live_event(&draft(1, 9, "revived old preview"));

        let projected = view
            .tool_request_drafts
            .get("call-write")
            .expect("current request draft");
        assert_eq!(projected.generation, 2);
        assert_eq!(projected.revision, 1);
        assert_eq!(projected.preview, "replacement preview");
        let slot_id = TranscriptViewItemId::tool_presentation_slot(
            "call-write",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        assert!(matches!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .find(|item| item.id == slot_id)
                .map(|item| &item.kind),
            Some(TranscriptViewItemKind::ToolRequestDraft { draft })
                if draft.generation == 2 && draft.preview == "replacement preview"
        ));
    }

    #[test]
    fn interleaved_request_drafts_remain_isolated() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |tool_call_id: &str, revision, offset, text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: tool_call_id.to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision,
                    operation: bcode_session_models::ToolRequestDraftOperation::Append {
                        offset,
                        text: text.to_owned(),
                    },
                    argument_bytes: offset.saturating_add(text.len()),
                    truncated: false,
                },
            },
        };

        view.apply_live_event(&draft("call-a", 1, 0, "alpha"));
        view.apply_live_event(&draft("call-b", 1, 0, "bravo"));
        view.apply_live_event(&draft("call-a", 2, 5, " one"));
        view.apply_live_event(&draft("call-b", 2, 5, " two"));

        assert_eq!(view.tool_request_drafts["call-a"].preview, "alpha one");
        assert_eq!(view.tool_request_drafts["call-b"].preview, "bravo two");
        let snapshot = view.snapshot();
        assert_eq!(
            snapshot
                .transcript
                .items
                .iter()
                .filter(|item| matches!(item.kind, TranscriptViewItemKind::ToolRequestDraft { .. }))
                .count(),
            2
        );
        for (tool_call_id, expected) in [("call-a", "alpha one"), ("call-b", "bravo two")] {
            let slot_id = TranscriptViewItemId::tool_presentation_slot(
                tool_call_id,
                bcode_session_models::ToolContributionPlacement::Result,
                None,
            );
            assert!(matches!(
                snapshot
                    .transcript
                    .items
                    .iter()
                    .find(|item| item.id == slot_id)
                    .map(|item| &item.kind),
                Some(TranscriptViewItemKind::ToolRequestDraft { draft })
                    if draft.tool_call_id == tool_call_id && draft.preview == expected
            ));
        }
    }

    #[test]
    fn durable_tool_request_reconciles_live_draft_without_duplicate_rows() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision: 1,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: r#"{"path":"README.md"}"#.to_owned(),
                    },
                    argument_bytes: 20,
                    truncated: false,
                },
            },
        });
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.write".to_owned(),
                arguments_json: r#"{"path":"README.md"}"#.to_owned(),
                working_directory: None,
            },
        ));

        assert!(view.tool_request_drafts.is_empty());
        assert!(view.terminal_tool_request_drafts.is_empty());
        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| matches!(item.kind, TranscriptViewItemKind::ToolInvocation { .. }))
                .count(),
            1
        );
        assert!(
            !view
                .snapshot()
                .transcript
                .items
                .iter()
                .any(|item| matches!(item.kind, TranscriptViewItemKind::ToolRequestDraft { .. }))
        );
    }

    #[test]
    fn final_result_dominates_late_result_draft_and_cleanup() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |revision, operation| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision,
                    operation,
                    argument_bytes: 32,
                    truncated: false,
                },
            },
        };
        view.apply_live_event(&draft(
            1,
            bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                start_offset: 0,
                text: r#"{"path":"README.md","contents":"new"}"#.to_owned(),
            },
        ));
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.write".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "written".to_owned(),
                    is_error: false,
                    result: Some(ToolInvocationResult::Text {
                        text: "written".to_owned(),
                    }),
                },
            },
        ));
        view.apply_live_event(&draft(
            2,
            bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                start_offset: 0,
                text: "must not replace final".to_owned(),
            },
        ));
        view.apply_live_event(&draft(
            3,
            bcode_session_models::ToolRequestDraftOperation::Remove {
                reason: bcode_session_models::ToolRequestDraftTerminalReason::Completed,
            },
        ));

        let result_id = TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        let result_items = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| item.id == result_id)
            .collect::<Vec<_>>();
        assert_eq!(result_items.len(), 1);
        assert!(matches!(
            result_items[0].kind,
            TranscriptViewItemKind::ToolInvocation { .. }
        ));
        assert!(
            !view.snapshot().transcript.items.iter().any(|item| {
                matches!(item.kind, TranscriptViewItemKind::ToolRequestDraft { .. })
            })
        );
    }

    #[test]
    fn final_result_without_lifecycle_completion_retires_result_draft() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision: 1,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: r#"{"path":"README.md","contents":"new"}"#.to_owned(),
                    },
                    argument_bytes: 37,
                    truncated: false,
                },
            },
        });
        let slot_id = TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        assert!(matches!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .find(|item| item.id == slot_id)
                .map(|item| &item.kind),
            Some(TranscriptViewItemKind::ToolRequestDraft { .. })
        ));

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.write".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "written".to_owned(),
                    is_error: false,
                    result: Some(ToolInvocationResult::Text {
                        text: "written".to_owned(),
                    }),
                },
            },
        ));

        let slot_items = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| item.id == slot_id)
            .collect::<Vec<_>>();
        assert_eq!(slot_items.len(), 1);
        assert!(matches!(
            slot_items[0].kind,
            TranscriptViewItemKind::ToolInvocation { .. }
        ));
        assert!(view.snapshot().active_invocations.is_empty());
    }

    #[test]
    fn lifecycle_completion_before_final_result_preserves_preview_until_replacement() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision: 1,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: r#"{"path":"README.md","contents":"new"}"#.to_owned(),
                    },
                    argument_bytes: 37,
                    truncated: false,
                },
            },
        });
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.write".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence: 1,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Completed,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        ));
        let slot_id = TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        assert!(matches!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .find(|item| item.id == slot_id)
                .map(|item| &item.kind),
            Some(TranscriptViewItemKind::ToolRequestDraft { .. })
        ));

        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "written".to_owned(),
                    is_error: false,
                    result: Some(ToolInvocationResult::Text {
                        text: "written".to_owned(),
                    }),
                },
            },
        ));
        let slot_items = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| item.id == slot_id)
            .collect::<Vec<_>>();
        assert_eq!(slot_items.len(), 1);
        assert!(matches!(
            slot_items[0].kind,
            TranscriptViewItemKind::ToolInvocation { .. }
        ));
    }

    #[test]
    fn durable_result_contribution_cannot_override_canonical_result_record() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("test.plugin".to_owned()),
                tool_name: "test.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "canonical".to_owned(),
                    is_error: false,
                    result: Some(ToolInvocationResult::Text {
                        text: "canonical".to_owned(),
                    }),
                },
            },
        ));
        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Result,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call-1".to_owned(),
                        contribution_id: "late-result".to_owned(),
                        sequence: 1,
                        producer_id: "test.plugin".to_owned(),
                        schema: "test.result".to_owned(),
                        schema_version: 1,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Durable,
                        artifact: None,
                        payload: serde_json::json!({"late": true}),
                    },
                ),
            },
        ));

        let result_id = TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        let result_items = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| item.id == result_id)
            .collect::<Vec<_>>();
        assert_eq!(result_items.len(), 1);
        assert!(matches!(
            result_items[0].kind,
            TranscriptViewItemKind::ToolInvocation { .. }
        ));
    }

    #[test]
    fn session_view_projects_generic_final_result_without_legacy_finish_event() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("example.plugin".to_owned()),
                tool_name: "example.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "done".to_owned(),
                    is_error: false,
                    result: Some(ToolInvocationResult::Text {
                        text: "semantic".to_owned(),
                    }),
                },
            },
        ));

        let snapshot = view.snapshot();
        let tool = snapshot.tools.get("call-1").expect("projected tool");
        assert_eq!(tool.status, ToolInvocationViewStatus::Finished);
        assert_eq!(tool.result_text.as_deref(), Some("done"));
        assert_eq!(
            tool.result,
            Some(ToolResultView::Text {
                text: "semantic".to_owned(),
            })
        );
        assert_eq!(
            snapshot
                .transcript
                .items
                .iter()
                .filter(|item| matches!(item.kind, TranscriptViewItemKind::ToolInvocation { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn reconnect_replay_reconstructs_cancelled_tool_without_duplicate_lifecycle_state() {
        let session_id = SessionId::new();
        let events = [
            event(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-reconnect".to_owned(),
                    producer_plugin_id: Some("example.plugin".to_owned()),
                    tool_name: "example.tool".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: "call-reconnect".to_owned(),
                        sequence: 2,
                        stage: bcode_session_models::ToolInvocationLifecycleStage::Cancelled,
                        message: Some("cancelled during reconnect fixture".to_owned()),
                        metadata: serde_json::Value::Null,
                    },
                },
            ),
        ];

        let mut live_view = SessionView::new();
        let mut reconnected_view = SessionView::new();
        for event in &events {
            live_view.apply_event(event);
        }
        for event in &events {
            reconnected_view.apply_event(event);
        }

        assert_eq!(live_view.snapshot(), reconnected_view.snapshot());
        assert_eq!(
            reconnected_view.snapshot().tools["call-reconnect"].status,
            ToolInvocationViewStatus::Cancelled
        );
        assert_eq!(
            reconnected_view
                .snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| matches!(item.kind, TranscriptViewItemKind::ToolInvocation { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn successful_lifecycle_completion_keeps_result_draft_until_result_arrives() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.write".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision: 1,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: "{}".to_owned(),
                    },
                    argument_bytes: 2,
                    truncated: false,
                },
            },
        });
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence: 2,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Completed,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        ));

        let result_id = TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        assert!(view.snapshot().transcript.items.iter().any(|item| {
            item.id == result_id
                && matches!(item.kind, TranscriptViewItemKind::ToolRequestDraft { .. })
        }));
    }

    #[test]
    fn lifecycle_cancellation_and_failure_update_existing_tool_semantics() {
        for (stage, expected_status, is_error) in [
            (
                bcode_session_models::ToolInvocationLifecycleStage::Cancelled,
                ToolInvocationViewStatus::Cancelled,
                None,
            ),
            (
                bcode_session_models::ToolInvocationLifecycleStage::Failed,
                ToolInvocationViewStatus::Failed,
                Some(true),
            ),
        ] {
            let session_id = SessionId::new();
            let mut view = SessionView::new();
            view.apply_event(&event(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-1".to_owned(),
                    producer_plugin_id: Some("example.plugin".to_owned()),
                    tool_name: "example.tool".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            ));
            view.apply_event(&event(
                session_id,
                2,
                SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: "call-1".to_owned(),
                        sequence: 2,
                        stage,
                        message: Some(format!("{stage:?}")),
                        metadata: serde_json::Value::Null,
                    },
                },
            ));

            let tool = &view.snapshot().tools["call-1"];
            assert_eq!(tool.status, expected_status);
            assert_eq!(tool.is_error, is_error);
            assert_eq!(
                tool.result_text.as_deref(),
                Some(format!("{stage:?}").as_str())
            );
            assert!(view.snapshot().active_invocations.is_empty());
            let transcript_tool = view
                .snapshot()
                .transcript
                .items
                .iter()
                .find_map(|item| match &item.kind {
                    TranscriptViewItemKind::ToolInvocation { tool } => Some(tool.as_ref()),
                    _ => None,
                })
                .expect("terminal tool transcript item");
            assert_eq!(transcript_tool.status, expected_status);
        }
    }

    #[test]
    fn generic_lifecycle_projection_tracks_only_active_invocations_and_rejects_revival() {
        let session_id = SessionId::new();
        let lifecycle = |sequence, stage| {
            event(
                session_id,
                sequence,
                SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: "call-1".to_owned(),
                        sequence,
                        stage,
                        message: Some(format!("{stage:?}")),
                        metadata: serde_json::json!({"opaque": sequence}),
                    },
                },
            )
        };
        let mut view = SessionView::new();
        view.apply_event(&lifecycle(
            1,
            bcode_session_models::ToolInvocationLifecycleStage::Started,
        ));
        view.apply_event(&lifecycle(
            2,
            bcode_session_models::ToolInvocationLifecycleStage::Waiting,
        ));
        assert_eq!(
            view.snapshot().active_invocations["call-1"].stage,
            bcode_session_models::ToolInvocationLifecycleStage::Waiting
        );
        let contribution = |sequence| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Hidden,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call-1".to_owned(),
                        contribution_id: "surface".to_owned(),
                        sequence,
                        producer_id: "future.producer".to_owned(),
                        schema: "future.unknown/schema".to_owned(),
                        schema_version: 77,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Transient,
                        artifact: None,
                        payload: serde_json::json!({"sequence": sequence}),
                    },
                ),
            },
        };
        view.apply_live_event(&contribution(1));
        assert_eq!(view.snapshot().contributions.len(), 1);

        view.apply_event(&lifecycle(
            3,
            bcode_session_models::ToolInvocationLifecycleStage::Completed,
        ));
        view.apply_event(&lifecycle(
            4,
            bcode_session_models::ToolInvocationLifecycleStage::Progress,
        ));
        view.apply_live_event(&contribution(2));
        assert!(view.snapshot().active_invocations.is_empty());
        assert!(view.snapshot().contributions.is_empty());
        assert!(view.snapshot().transcript.items.is_empty());
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
    fn complete_presentation_policy_filters_reasoning_without_removing_activity() {
        let session_id = SessionId::new();
        let activity = || SessionEventKind::AssistantReasoningActivity {
            turn_id: "turn-1".to_owned(),
            activity: bcode_session_models::ReasoningActivity {
                activity_id: "reasoning-1".to_owned(),
                order: 0,
                status: bcode_session_models::ReasoningActivityStatus::Completed,
                parts: vec![
                    bcode_session_models::ReasoningPart {
                        part_id: "summary-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Summary,
                        role: bcode_session_models::ReasoningContentRole::Milestone,
                        order: 0,
                        text: "Summary".to_owned(),
                    },
                    bcode_session_models::ReasoningPart {
                        part_id: "raw-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Raw,
                        role: bcode_session_models::ReasoningContentRole::Detail,
                        order: 1,
                        text: "Raw".to_owned(),
                    },
                ],
                opaque: true,
            },
        };

        for (policy, expected) in [
            (
                bcode_session_view_models::ReasoningPresentationPolicy::All,
                "Summary\n\nRaw",
            ),
            (
                bcode_session_view_models::ReasoningPresentationPolicy::Summary,
                "Summary",
            ),
            (
                bcode_session_view_models::ReasoningPresentationPolicy::Raw,
                "Raw",
            ),
            (
                bcode_session_view_models::ReasoningPresentationPolicy::Hidden,
                "",
            ),
        ] {
            let mut view = SessionView::new();
            view.set_reasoning_presentation_policy(policy);
            view.apply_event(&event(session_id, 1, activity()));
            assert_eq!(view.snapshot().transcript.items.len(), 1);
            assert_reasoning_text(&view.snapshot().transcript.items[0], expected, false);
        }
    }

    #[test]
    fn every_display_mode_preserves_opaque_only_activity_chrome() {
        let session_id = SessionId::new();
        for mode in [
            bcode_session_view_models::ReasoningDisplayMode::All,
            bcode_session_view_models::ReasoningDisplayMode::Summary,
            bcode_session_view_models::ReasoningDisplayMode::Raw,
        ] {
            let mut view = SessionView::new();
            view.set_reasoning_display_mode(mode);
            view.apply_event(&event(
                session_id,
                1,
                SessionEventKind::AssistantReasoningActivity {
                    turn_id: "turn-1".to_owned(),
                    activity: bcode_session_models::ReasoningActivity {
                        activity_id: "reasoning-1".to_owned(),
                        order: 0,
                        status: bcode_session_models::ReasoningActivityStatus::Completed,
                        parts: Vec::new(),
                        opaque: true,
                    },
                },
            ));
            assert_eq!(view.snapshot().transcript.items.len(), 1);
            assert_reasoning_text(&view.snapshot().transcript.items[0], "", false);
        }

        let mut hidden = SessionView::new();
        hidden.set_reasoning_visible(false);
        hidden.apply_event(&event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-1".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: Vec::new(),
                    opaque: true,
                },
            },
        ));
        assert_eq!(hidden.snapshot().transcript.items.len(), 1);
        assert_reasoning_text(&hidden.snapshot().transcript.items[0], "", false);
    }

    #[test]
    fn live_reasoning_reconciles_authoritative_parts_and_matches_durable_projection() {
        let session_id = SessionId::new();
        let mut live = SessionView::new();
        for event in [
            bcode_session_models::ReasoningActivityEvent::Started {
                activity_id: "reasoning-1".to_owned(),
                order: 0,
            },
            bcode_session_models::ReasoningActivityEvent::PartDelta {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 0,
                part_id: "summary-0".to_owned(),
                kind: bcode_session_models::ReasoningContentKind::Summary,
                role: bcode_session_models::ReasoningContentRole::Milestone,
                part_order: 0,
                text: "Fir".to_owned(),
            },
            bcode_session_models::ReasoningActivityEvent::PartCompleted {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 0,
                part_id: "summary-0".to_owned(),
                kind: bcode_session_models::ReasoningContentKind::Summary,
                role: bcode_session_models::ReasoningContentRole::Milestone,
                part_order: 0,
                text: "First".to_owned(),
            },
            bcode_session_models::ReasoningActivityEvent::PartCompleted {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 0,
                part_id: "summary-1".to_owned(),
                kind: bcode_session_models::ReasoningContentKind::Summary,
                role: bcode_session_models::ReasoningContentRole::Milestone,
                part_order: 1,
                text: "Second".to_owned(),
            },
            bcode_session_models::ReasoningActivityEvent::Finished {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 0,
                status: bcode_session_models::ReasoningActivityStatus::Completed,
            },
        ] {
            live.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantReasoningActivity {
                    turn_id: "turn-1".to_owned(),
                    event,
                },
            });
        }
        let live_item_id = live.snapshot().transcript.items[0].id.clone();
        assert_reasoning_text(
            &live.snapshot().transcript.items[0],
            "First\n\nSecond",
            false,
        );

        let durable = build_session_view_snapshot(&[event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-1".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![
                        bcode_session_models::ReasoningPart {
                            part_id: "summary-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Summary,
                            role: bcode_session_models::ReasoningContentRole::Milestone,
                            order: 0,
                            text: "First".to_owned(),
                        },
                        bcode_session_models::ReasoningPart {
                            part_id: "summary-1".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Summary,
                            role: bcode_session_models::ReasoningContentRole::Milestone,
                            order: 1,
                            text: "Second".to_owned(),
                        },
                    ],
                    opaque: false,
                },
            },
        )]);
        assert_eq!(durable.transcript.items[0].id, live_item_id);
        assert_reasoning_text(&durable.transcript.items[0], "First\n\nSecond", false);
    }

    #[test]
    fn durable_reasoning_activity_respects_silent_display_modes() {
        let session_id = SessionId::new();
        let source = event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-1".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![
                        bcode_session_models::ReasoningPart {
                            part_id: "summary-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Summary,
                            role: bcode_session_models::ReasoningContentRole::Milestone,
                            order: 0,
                            text: "summary".to_owned(),
                        },
                        bcode_session_models::ReasoningPart {
                            part_id: "raw-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Raw,
                            role: bcode_session_models::ReasoningContentRole::Detail,
                            order: 1,
                            text: "raw".to_owned(),
                        },
                    ],
                    opaque: false,
                },
            },
        );

        for (mode, expected) in [
            (
                bcode_session_view_models::ReasoningDisplayMode::All,
                "summary\n\nraw",
            ),
            (
                bcode_session_view_models::ReasoningDisplayMode::Summary,
                "summary",
            ),
            (bcode_session_view_models::ReasoningDisplayMode::Raw, "raw"),
        ] {
            let mut view = SessionView::new();
            view.set_reasoning_display_mode(mode);
            view.apply_event(&source);
            assert_reasoning_text(&view.snapshot().transcript.items[0], expected, false);
        }
        let mut hidden = SessionView::new();
        hidden.set_reasoning_visible(false);
        hidden.apply_event(&source);
        assert_reasoning_text(&hidden.snapshot().transcript.items[0], "", false);
    }

    #[test]
    fn reasoning_display_mode_changes_rebuild_replayed_semantic_content() {
        let session_id = SessionId::new();
        let source = event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-1".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![
                        bcode_session_models::ReasoningPart {
                            part_id: "summary-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Summary,
                            role: bcode_session_models::ReasoningContentRole::Milestone,
                            order: 0,
                            text: "summary".to_owned(),
                        },
                        bcode_session_models::ReasoningPart {
                            part_id: "raw-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Raw,
                            role: bcode_session_models::ReasoningContentRole::Detail,
                            order: 1,
                            text: "raw".to_owned(),
                        },
                    ],
                    opaque: false,
                },
            },
        );
        let mut view = SessionView::new();
        view.apply_event(&source);
        assert_reasoning_text(
            &view.snapshot().transcript.items[0],
            "summary\n\nraw",
            false,
        );

        view.set_reasoning_display_mode(bcode_session_view_models::ReasoningDisplayMode::Summary);
        assert_reasoning_text(&view.snapshot().transcript.items[0], "summary", false);
        view.set_reasoning_display_mode(bcode_session_view_models::ReasoningDisplayMode::Raw);
        assert_reasoning_text(&view.snapshot().transcript.items[0], "raw", false);
        view.set_reasoning_visible(false);
        assert_reasoning_text(&view.snapshot().transcript.items[0], "", false);
        view.set_reasoning_visible(true);
        view.set_reasoning_display_mode(bcode_session_view_models::ReasoningDisplayMode::All);
        assert_reasoning_text(
            &view.snapshot().transcript.items[0],
            "summary\n\nraw",
            false,
        );
    }

    #[test]
    fn durable_terminal_reasoning_states_and_opaque_activity_survive_replay() {
        let session_id = SessionId::new();
        for status in [
            bcode_session_models::ReasoningActivityStatus::Completed,
            bcode_session_models::ReasoningActivityStatus::Interrupted,
            bcode_session_models::ReasoningActivityStatus::Failed,
        ] {
            let snapshot = build_session_view_snapshot(&[event(
                session_id,
                1,
                SessionEventKind::AssistantReasoningActivity {
                    turn_id: "turn-1".to_owned(),
                    activity: bcode_session_models::ReasoningActivity {
                        activity_id: format!("reasoning-{status:?}"),
                        order: 0,
                        status,
                        parts: Vec::new(),
                        opaque: true,
                    },
                },
            )]);

            assert_eq!(snapshot.transcript.items.len(), 1);
            assert_reasoning_text(&snapshot.transcript.items[0], "", false);
        }
    }

    #[test]
    fn durable_reasoning_activity_preserves_part_boundaries() {
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-1".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![
                        bcode_session_models::ReasoningPart {
                            part_id: "summary-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Summary,
                            role: bcode_session_models::ReasoningContentRole::Milestone,
                            order: 0,
                            text: "First milestone".to_owned(),
                        },
                        bcode_session_models::ReasoningPart {
                            part_id: "summary-1".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Summary,
                            role: bcode_session_models::ReasoningContentRole::Milestone,
                            order: 1,
                            text: "Second milestone".to_owned(),
                        },
                    ],
                    opaque: false,
                },
            },
        )]);

        assert_eq!(snapshot.transcript.items.len(), 1);
        assert_reasoning_text(
            &snapshot.transcript.items[0],
            "First milestone\n\nSecond milestone",
            false,
        );
    }

    #[test]
    fn reasoning_streaming_starts_new_item_after_interleaved_transcript_item() {
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[
            event(
                session_id,
                1,
                SessionEventKind::AssistantReasoningDelta {
                    text: "first thought".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::SystemMessage {
                    text: "tool output".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::AssistantReasoningDelta {
                    text: "second thought".to_owned(),
                },
            ),
        ]);

        assert_eq!(snapshot.transcript.items.len(), 3);
        assert_reasoning_text(&snapshot.transcript.items[0], "first thought", true);
        assert!(matches!(
            &snapshot.transcript.items[1].kind,
            TranscriptViewItemKind::SystemMessage { message } if message.text == "tool output"
        ));
        assert_reasoning_text(&snapshot.transcript.items[2], "second thought", true);
    }

    #[test]
    fn reasoning_finish_preserves_split_streaming_items() {
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[
            event(
                session_id,
                1,
                SessionEventKind::AssistantReasoningDelta {
                    text: "first thought".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::SystemMessage {
                    text: "tool output".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::AssistantReasoningDelta {
                    text: "second thought".to_owned(),
                },
            ),
            event(
                session_id,
                4,
                SessionEventKind::AssistantReasoningMessage {
                    text: "first thought second thought final aggregate".to_owned(),
                },
            ),
        ]);

        assert_eq!(snapshot.transcript.items.len(), 3);
        assert_reasoning_text(&snapshot.transcript.items[0], "first thought", false);
        assert_reasoning_text(&snapshot.transcript.items[2], "second thought", false);
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
            "assembling shell.run arguments (128 B received)"
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
                TranscriptViewItemKind::Skill { skill }
                    if skill.skill_id == "renderer-skill"
                        && skill.status == SkillViewStatus::Invoked
                        && skill.text.contains("invoked renderer-skill")
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
        assert_eq!(full_ids[1].get(), "tool-slot:tool-1:request");
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
    fn model_change_events_normalize_runtime_selection_for_renderers() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ModelChanged {
                provider: "<auto>".to_owned(),
                model: "<default>".to_owned(),
            },
        ));

        let runtime = &view.snapshot().runtime;
        assert_eq!(runtime.provider_plugin_id, None);
        assert_eq!(runtime.requested_model_id, None);
        assert_eq!(runtime.effective_model_id, None);

        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ModelChanged {
                provider: "provider".to_owned(),
                model: "model".to_owned(),
            },
        ));

        let runtime = &view.snapshot().runtime;
        assert_eq!(runtime.provider_plugin_id.as_deref(), Some("provider"));
        assert_eq!(runtime.requested_model_id.as_deref(), Some("model"));
        assert_eq!(runtime.effective_model_id.as_deref(), Some("model"));
    }

    #[test]
    fn placed_slot_history_rebuild_matches_fresh_replay() {
        let session_id = SessionId::new();
        let events = [
            event(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-1".to_owned(),
                    producer_plugin_id: Some("test.plugin".to_owned()),
                    tool_name: "test.tool".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::ToolContributionPlaced {
                    envelope: bcode_session_models::ToolContributionEnvelope::new(
                        bcode_session_models::ToolContributionPlacement::Request,
                        bcode_session_models::ToolContributionEvent {
                            invocation_id: "call-1".to_owned(),
                            contribution_id: "request".to_owned(),
                            sequence: 1,
                            producer_id: "test.plugin".to_owned(),
                            schema: "test.request".to_owned(),
                            schema_version: 1,
                            operation: bcode_session_models::ToolContributionOperation::Upsert,
                            persistence: bcode_session_models::ToolContributionPersistence::Durable,
                            artifact: None,
                            payload: serde_json::json!({"label": "rich"}),
                        },
                    ),
                },
            ),
        ];
        let expected = build_session_view_snapshot(&events);
        let mut rebuilt = SessionView::new();
        rebuilt.apply_history(&events);
        rebuilt.rebuild_history_window(&events);

        assert_eq!(rebuilt.snapshot().transcript, expected.transcript);
        assert_eq!(rebuilt.snapshot().contributions, expected.contributions);
    }

    #[test]
    fn history_window_rebuild_preserves_live_state_without_reconstructing_it_from_history() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-draft".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision: 1,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: "draft".to_owned(),
                    },
                    argument_bytes: 5,
                    truncated: false,
                },
            },
        });
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Progress,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call-progress".to_owned(),
                        contribution_id: "preview".to_owned(),
                        sequence: 1,
                        producer_id: "test.plugin".to_owned(),
                        schema: "test.progress".to_owned(),
                        schema_version: 1,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Transient,
                        artifact: None,
                        payload: serde_json::json!({"status": "running"}),
                    },
                ),
            },
        });

        view.rebuild_history_window(&[event(
            session_id,
            10,
            SessionEventKind::AssistantMessage {
                text: "durable".to_owned(),
            },
        )]);

        assert_eq!(view.tool_request_drafts.len(), 1);
        assert_eq!(view.snapshot().contributions.len(), 1);
        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| item.streaming)
                .count(),
            2
        );
        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| matches!(item.kind, TranscriptViewItemKind::AssistantMessage { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn history_window_rebuild_retains_authoritative_runtime_state() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.set_model_selection(
            Some("provider".to_owned()),
            Some("requested".to_owned()),
            Some("effective".to_owned()),
        );
        view.set_agent_id(Some("build".to_owned()));
        view.set_active_skill_ids(BTreeSet::from(["review".to_owned()]));
        view.rebuild_history_window(&[event(
            session_id,
            10,
            SessionEventKind::AssistantMessage {
                text: "bounded".to_owned(),
            },
        )]);

        let snapshot = view.snapshot();
        assert_eq!(
            snapshot.runtime.provider_plugin_id.as_deref(),
            Some("provider")
        );
        assert_eq!(
            snapshot.runtime.requested_model_id.as_deref(),
            Some("requested")
        );
        assert_eq!(snapshot.runtime.agent_id.as_deref(), Some("build"));
        assert!(snapshot.active_skills.contains("review"));
        assert!(matches!(
            &snapshot.transcript.items[0].kind,
            TranscriptViewItemKind::AssistantMessage { message } if message.text == "bounded"
        ));
    }

    #[test]
    fn context_occupancy_rejects_stale_epochs_and_sequences() {
        let occupancy = |context_epoch, observation_sequence, tokens| {
            let observation = RequestContextObservation {
                request: ModelRequestIdentity {
                    provider_plugin_id: "provider".to_owned(),
                    requested_model_id: None,
                    effective_model_id: "model".to_owned(),
                    request_id: format!("request-{context_epoch}-{observation_sequence}"),
                    model_turn_id: "turn".to_owned(),
                    round: 0,
                    request_fingerprint: "fingerprint".to_owned(),
                    effective_auth_profile: None,
                    context_format_version: None,
                    compatibility_key: None,
                    context_epoch,
                },
                context_through_sequence: observation_sequence,
                context_tokens: bcode_session_models::RequestContextTokenCount::Estimated(tokens),
                local_estimate: bcode_session_models::LocalContextEstimate {
                    tokens,
                    algorithm_version: 1,
                },
            };
            bcode_session_models::RequestContextOccupancy {
                context_epoch,
                observation_sequence,
                observation,
            }
        };
        let mut view = SessionView::new();
        view.set_context_occupancy(Some(occupancy(2, 10, 2_000)));
        view.set_context_occupancy(Some(occupancy(1, 100, 1_000)));
        view.set_context_occupancy(Some(occupancy(2, 9, 1_500)));

        let current = view
            .snapshot()
            .runtime
            .context_occupancy
            .as_ref()
            .expect("context occupancy");
        assert_eq!(current.context_epoch, 2);
        assert_eq!(current.observation_sequence, 10);
        assert_eq!(current.observation.context_tokens.tokens(), 2_000);

        view.set_context_occupancy(Some(occupancy(3, 1, 500)));
        let current = view
            .snapshot()
            .runtime
            .context_occupancy
            .as_ref()
            .expect("new context epoch");
        assert_eq!(current.context_epoch, 3);
        assert_eq!(current.observation.context_tokens.tokens(), 500);
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
        assert_eq!(runtime.cumulative_metered_tokens, 15);
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
        assert!(view.snapshot().transcript.items.iter().any(|item| matches!(
            &item.kind,
            TranscriptViewItemKind::Usage { usage }
                if usage.turn_id == "turn-1" && usage.usage.total_tokens == Some(15)
        )));
        assert!(view.snapshot().transcript.items.iter().any(|item| matches!(
            &item.kind,
            TranscriptViewItemKind::SystemMessage { message } if message.text == "status"
        )));
    }

    #[test]
    fn assistant_context_precedes_pending_question_and_resolution_updates_same_item() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "Read this context before answering.".to_owned(),
            },
        ));
        view.upsert_interaction(InteractionViewSummary {
            interaction_id: "question-1".to_owned(),
            kind: "bcode.question".to_owned(),
            surface_kind: "bcode.question.inline".to_owned(),
            tool_call_id: Some("call-1".to_owned()),
            title: Some("Question".to_owned()),
            required: true,
            snapshot: Some(serde_json::json!({"questions": [{"question": "Proceed?"}]})),
            state: bcode_session_view_models::InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        });

        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "Read this context before answering."
        ));
        let interaction_id = TranscriptViewItemId::interaction("question-1");
        let pending_revision = view
            .snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.id == interaction_id)
            .expect("pending interaction item")
            .revision;

        view.upsert_interaction(InteractionViewSummary {
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
            resolution: Some(serde_json::json!({"status": "answered"})),
        });
        let resolved = view
            .snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.id == interaction_id)
            .expect("resolved interaction item");
        assert!(resolved.revision > pending_revision);
        assert!(matches!(
            &resolved.kind,
            TranscriptViewItemKind::Interaction { interaction }
                if interaction.resolved
                    && interaction.resolution == Some(serde_json::json!({"status": "answered"}))
        ));
        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| item.id == interaction_id)
                .count(),
            1
        );
    }

    #[test]
    fn authoritative_interaction_hydration_removes_stale_pending_state() {
        let interaction = |id: &str| InteractionViewSummary {
            interaction_id: id.to_owned(),
            kind: "question".to_owned(),
            surface_kind: "question.inline".to_owned(),
            tool_call_id: Some("call".to_owned()),
            title: Some("Question".to_owned()),
            required: true,
            snapshot: Some(serde_json::json!({"questions": []})),
            state: bcode_session_view_models::InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        };
        let mut view = SessionView::new();
        view.set_pending_interactions(vec![interaction("interaction-1")]);
        assert_eq!(view.snapshot().interactions.len(), 1);

        view.set_pending_interactions(Vec::new());
        assert!(view.snapshot().interactions.is_empty());
        assert!(view.snapshot().transcript.items.is_empty());
    }

    #[test]
    fn authoritative_permission_hydration_removes_stale_pending_state() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.set_pending_permissions(vec![bcode_session_view_models::PermissionView {
            permission_id: "permission-1".to_owned(),
            session_id: Some(session_id),
            tool_call_id: "call-1".to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
            batch: None,
            agent_id: "build".to_owned(),
            title: Some("Permission requested: shell.run".to_owned()),
            policy_source: None,
            detail: None,
            resolved: false,
            approved: None,
            can_remember: false,
        }]);
        assert_eq!(view.snapshot().permissions.len(), 1);

        view.set_pending_permissions(Vec::new());
        assert!(view.snapshot().permissions.is_empty());
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
                batch: None,
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
    fn permission_batch_correlation_survives_session_view_projection() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::PermissionRequested {
                permission_id: "permission-batched".to_owned(),
                tool_call_id: "tool-batched".to_owned(),
                producer_plugin_id: None,
                tool_name: "example.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                batch: Some(bcode_session_models::PermissionBatchCorrelation {
                    batch_id: "batch-1".to_owned(),
                    call_index: 1,
                    call_count: 3,
                }),
                policy_source: None,
                policy_reason: None,
            },
        ));

        assert_eq!(
            view.snapshot().permissions[0].batch,
            Some(bcode_session_view_models::PermissionBatchView {
                batch_id: "batch-1".to_owned(),
                call_index: 1,
                call_count: 3,
            })
        );
    }

    #[test]
    fn working_directory_change_projects_safety_warning() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory: std::path::PathBuf::from("/tmp/old"),
                new_working_directory: std::path::PathBuf::from("/tmp/new"),
            },
        ));

        assert_eq!(
            view.snapshot().working_directory.as_deref(),
            Some(std::path::Path::new("/tmp/new"))
        );
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::SystemMessage { message }
                if message.text.contains("Treat prior file/path assumptions as possibly stale")
        ));
    }

    #[test]
    fn many_invocations_replace_progress_in_place_and_hidden_storms_stay_invisible() {
        const INVOCATIONS: u64 = 256;
        const REPLACEMENTS: u64 = 8;
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for invocation in 0..INVOCATIONS {
            for sequence in 1..=REPLACEMENTS {
                let contribution = bcode_session_models::ToolContributionEvent {
                    invocation_id: format!("call-{invocation}"),
                    contribution_id: "progress".to_owned(),
                    sequence,
                    producer_id: "test.plugin".to_owned(),
                    schema: "test.progress".to_owned(),
                    schema_version: 1,
                    operation: bcode_session_models::ToolContributionOperation::Upsert,
                    persistence: bcode_session_models::ToolContributionPersistence::Transient,
                    artifact: None,
                    payload: serde_json::json!({"sequence": sequence}),
                };
                view.apply_live_event(&SessionLiveEvent {
                    session_id,
                    kind: SessionLiveEventKind::ToolContributionPlaced {
                        envelope: bcode_session_models::ToolContributionEnvelope::new(
                            bcode_session_models::ToolContributionPlacement::Progress,
                            contribution,
                        ),
                    },
                });
            }
            for sequence in 1..=REPLACEMENTS {
                let contribution = bcode_session_models::ToolContributionEvent {
                    invocation_id: format!("call-{invocation}"),
                    contribution_id: "hidden".to_owned(),
                    sequence,
                    producer_id: "test.plugin".to_owned(),
                    schema: "test.hidden".to_owned(),
                    schema_version: 1,
                    operation: bcode_session_models::ToolContributionOperation::Upsert,
                    persistence: bcode_session_models::ToolContributionPersistence::Transient,
                    artifact: None,
                    payload: serde_json::json!({"sequence": sequence}),
                };
                view.apply_live_event(&SessionLiveEvent {
                    session_id,
                    kind: SessionLiveEventKind::ToolContributionPlaced {
                        envelope: bcode_session_models::ToolContributionEnvelope::new(
                            bcode_session_models::ToolContributionPlacement::Hidden,
                            contribution,
                        ),
                    },
                });
            }
        }

        let snapshot = view.snapshot();
        assert_eq!(
            snapshot.transcript.items.len(),
            usize::try_from(INVOCATIONS).expect("workload fits usize")
        );
        assert_eq!(
            snapshot.contributions.len(),
            usize::try_from(INVOCATIONS * 2).expect("workload fits usize")
        );
        assert!(snapshot.transcript.items.iter().all(|item| {
            item.revision == REPLACEMENTS.saturating_sub(1)
                && matches!(
                    &item.kind,
                    TranscriptViewItemKind::ToolContribution {
                        contribution,
                        placement: bcode_session_models::ToolContributionPlacement::Progress,
                        ..
                    } if contribution.sequence == REPLACEMENTS
                )
        }));
    }

    #[test]
    fn durable_rich_request_dominates_late_request_draft() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("test.plugin".to_owned()),
                tool_name: "test.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        let contribution = bcode_session_models::ToolContributionEvent {
            invocation_id: "call-1".to_owned(),
            contribution_id: "request".to_owned(),
            sequence: 1,
            producer_id: "test.plugin".to_owned(),
            schema: "test.request".to_owned(),
            schema_version: 1,
            operation: bcode_session_models::ToolContributionOperation::Upsert,
            persistence: bcode_session_models::ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({"rich": true}),
        };
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Request,
                    contribution.clone(),
                ),
            },
        ));
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "test.tool".to_owned(),
                    producer_plugin_id: Some("test.plugin".to_owned()),
                    schema: "test.draft".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision: 1,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: "must not replace rich request".to_owned(),
                    },
                    argument_bytes: 29,
                    truncated: false,
                },
            },
        });

        let request_id = TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Request,
            None,
        );
        let request_items = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| item.id == request_id)
            .collect::<Vec<_>>();
        assert_eq!(request_items.len(), 1);
        assert!(matches!(
            &request_items[0].kind,
            TranscriptViewItemKind::ToolContribution {
                contribution: current,
                ..
            } if current == &contribution
        ));
    }

    #[test]
    fn rich_request_replaces_compact_request_with_stable_slot_identity() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("test.plugin".to_owned()),
                tool_name: "test.tool".to_owned(),
                arguments_json: serde_json::json!({"secret": true}).to_string(),
                working_directory: None,
            },
        ));
        let compact = view.snapshot().transcript.items[0].clone();
        assert_eq!(
            compact.id,
            TranscriptViewItemId::new("tool-slot:call-1:request")
        );
        assert!(matches!(
            compact.kind,
            TranscriptViewItemKind::ToolInvocation { .. }
        ));

        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Request,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call-1".to_owned(),
                        contribution_id: "request".to_owned(),
                        sequence: 1,
                        producer_id: "test.plugin".to_owned(),
                        schema: "test.request".to_owned(),
                        schema_version: 1,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Durable,
                        artifact: None,
                        payload: serde_json::json!({"label": "rich"}),
                    },
                ),
            },
        ));

        let items = &view.snapshot().transcript.items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, compact.id);
        assert_eq!(items[0].revision, compact.revision.saturating_add(1));
        assert!(matches!(
            &items[0].kind,
            TranscriptViewItemKind::ToolContribution {
                invocation: Some(invocation),
                ..
            } if invocation.tool_call_id == "call-1"
                && invocation.tool_name.as_deref() == Some("test.tool")
        ));

        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "applied 1 replacement".to_owned(),
                    is_error: false,
                    result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(bcode_session_models::ToolArtifact {
                            artifact_id: "call-1-filesystem-change".to_owned(),
                            producer_plugin_id: "test.plugin".to_owned(),
                            schema: "test.change".to_owned(),
                            schema_version: 1,
                            tool_call_id: Some("call-1".to_owned()),
                            title: Some("File change".to_owned()),
                            metadata: serde_json::json!({"old_start_line": 2218}),
                            refs: Vec::new(),
                        }),
                    }),
                },
            },
        ));

        let items = &view.snapshot().transcript.items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, compact.id);
        assert_eq!(
            items[1].id,
            TranscriptViewItemId::tool_presentation_slot(
                "call-1",
                bcode_session_models::ToolContributionPlacement::Result,
                None,
            )
        );
        assert!(matches!(
            items[1].kind,
            TranscriptViewItemKind::ToolInvocation { .. }
        ));
    }

    #[test]
    fn placed_contribution_live_and_replay_snapshots_are_equivalent() {
        let session_id = SessionId::new();
        let contribution = bcode_session_models::ToolContributionEvent {
            invocation_id: "call-1".to_owned(),
            contribution_id: "progress".to_owned(),
            sequence: 1,
            producer_id: "test.plugin".to_owned(),
            schema: "test.visual".to_owned(),
            schema_version: 1,
            operation: bcode_session_models::ToolContributionOperation::Upsert,
            persistence: bcode_session_models::ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({"progress": 1}),
        };
        let envelope = bcode_session_models::ToolContributionEnvelope::new(
            bcode_session_models::ToolContributionPlacement::Progress,
            contribution,
        );
        let mut live = SessionView::new();
        live.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolContributionPlaced {
                envelope: envelope.clone(),
            },
        });
        let mut replay = SessionView::new();
        replay.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolContributionPlaced { envelope },
        ));

        let live_item = &live.snapshot().transcript.items[0];
        let replay_item = &replay.snapshot().transcript.items[0];
        assert_eq!(live_item.id, replay_item.id);
        assert_eq!(live_item.kind, replay_item.kind);
        assert_eq!(
            live.snapshot().contributions,
            replay.snapshot().contributions
        );
    }

    #[test]
    fn placed_slots_replace_by_placement_and_supplementals_coexist() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let contribution = |id: &str, sequence, placement| {
            event(
                session_id,
                sequence,
                SessionEventKind::ToolContributionPlaced {
                    envelope: bcode_session_models::ToolContributionEnvelope::new(
                        placement,
                        bcode_session_models::ToolContributionEvent {
                            invocation_id: "call-1".to_owned(),
                            contribution_id: id.to_owned(),
                            sequence,
                            producer_id: "test.plugin".to_owned(),
                            schema: "test.visual".to_owned(),
                            schema_version: 1,
                            operation: bcode_session_models::ToolContributionOperation::Upsert,
                            persistence: bcode_session_models::ToolContributionPersistence::Durable,
                            artifact: None,
                            payload: serde_json::json!({"id": id}),
                        },
                    ),
                },
            )
        };

        view.apply_event(&contribution(
            "request-one",
            1,
            bcode_session_models::ToolContributionPlacement::Request,
        ));
        view.apply_event(&contribution(
            "request-two",
            2,
            bcode_session_models::ToolContributionPlacement::Request,
        ));
        view.apply_event(&contribution(
            "progress",
            3,
            bcode_session_models::ToolContributionPlacement::Progress,
        ));
        view.apply_event(&contribution(
            "supplemental-one",
            4,
            bcode_session_models::ToolContributionPlacement::Supplemental,
        ));
        view.apply_event(&contribution(
            "supplemental-two",
            5,
            bcode_session_models::ToolContributionPlacement::Supplemental,
        ));
        let remove_replaced_request = event(
            session_id,
            6,
            SessionEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Request,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call-1".to_owned(),
                        contribution_id: "request-one".to_owned(),
                        sequence: 6,
                        producer_id: "test.plugin".to_owned(),
                        schema: "test.visual".to_owned(),
                        schema_version: 1,
                        operation: bcode_session_models::ToolContributionOperation::Remove,
                        persistence: bcode_session_models::ToolContributionPersistence::Durable,
                        artifact: None,
                        payload: serde_json::Value::Null,
                    },
                ),
            },
        );
        view.apply_event(&remove_replaced_request);

        let items = &view.snapshot().transcript.items;
        assert_eq!(items.len(), 4);
        let request_id = TranscriptViewItemId::new("tool-slot:call-1:request");
        let request_revision = items
            .iter()
            .find(|item| item.id == request_id)
            .expect("request slot")
            .revision;
        assert_eq!(request_revision, 1);
        assert_eq!(items.iter().filter(|item| item.id == request_id).count(), 1);
        assert!(matches!(
            &items[0].kind,
            TranscriptViewItemKind::ToolContribution { contribution, .. }
                if contribution.contribution_id == "request-two"
        ));
        assert_eq!(
            items[1].id,
            TranscriptViewItemId::new("tool-slot:call-1:progress")
        );
        assert_eq!(
            items[2].id,
            TranscriptViewItemId::new("tool-slot:call-1:supplemental:supplemental-one")
        );
        assert_eq!(
            items[3].id,
            TranscriptViewItemId::new("tool-slot:call-1:supplemental:supplemental-two")
        );
    }

    #[test]
    fn durable_contribution_survives_terminal_lifecycle_and_late_delivery() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence: u64::MAX,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Completed,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        ));
        let contribution = bcode_session_models::ToolContributionEvent {
            invocation_id: "call-1".to_owned(),
            contribution_id: "request".to_owned(),
            sequence: 1,
            producer_id: "bcode.test".to_owned(),
            schema: "bcode.test.request".to_owned(),
            schema_version: 1,
            operation: bcode_session_models::ToolContributionOperation::Upsert,
            persistence: bcode_session_models::ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({"value": "rich"}),
        };
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Request,
                    contribution.clone(),
                ),
            },
        ));

        assert_eq!(
            view.snapshot()
                .contributions
                .get("call-1:request")
                .map(|event| &event.payload),
            Some(&contribution.payload)
        );
        assert!(view.snapshot().transcript.items.iter().any(|item| {
            matches!(
                &item.kind,
                TranscriptViewItemKind::ToolContribution {
                    contribution: item,
                    ..
                }
                    if item == &contribution
            )
        }));
    }

    #[test]
    fn assistant_stream_keeps_live_identity_when_durable_message_finishes_it() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                text: "live answer".to_owned(),
            },
        });
        let live_id = view.snapshot().transcript.items[0].id.clone();

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_owned(),
                usage: bcode_session_models::SessionTokenUsage::default(),
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::AssistantMessage {
                text: "durable answer".to_owned(),
            },
        ));

        assert_eq!(view.snapshot().transcript.items.len(), 2);
        let assistant = &view.snapshot().transcript.items[0];
        assert_eq!(assistant.id, live_id);
        assert_eq!(assistant.sequence, Some(2));
        assert!(!assistant.streaming);
        assert!(matches!(
            &assistant.kind,
            TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "durable answer"
        ));
    }

    #[test]
    fn reasoning_visibility_survives_durable_and_live_projection() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.set_reasoning_visible(false);
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningDelta {
                text: "durable".to_owned(),
            },
        ));
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantReasoningDelta {
                turn_id: "turn-1".to_owned(),
                text: " live".to_owned(),
            },
        });

        assert!(!view.snapshot().thinking.visible);
        assert_eq!(
            view.snapshot().thinking.active_text.as_deref(),
            Some(" live")
        );
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
    fn durable_results_reconcile_cumulative_assistant_live_state() {
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
        assert!(matches!(
            &view.snapshot().transcript.items[1].kind,
            TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "live answer"
        ));
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
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "tool-1".to_owned(),
                    model_output: "durable output".to_owned(),
                    is_error: false,
                    result: Some(ToolInvocationResult::Text {
                        text: "durable output".to_owned(),
                    }),
                },
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

        let TranscriptViewItemKind::Compaction { compaction } = &snapshot.transcript.items[0].kind
        else {
            panic!("expected provider compaction item");
        };
        assert_eq!(compaction.status, CompactionViewStatus::Provider);
        assert_eq!(
            compaction.text,
            "explicit provider-native context compaction (provider)"
        );
        assert_eq!(compaction.provider_plugin_id.as_deref(), Some("provider"));
        assert_eq!(compaction.model_id.as_deref(), Some("model"));
        assert!(!compaction.text.contains(secret));
        assert!(!compaction.text.contains("portable summary"));
    }

    #[test]
    fn ralph_lifecycle_projects_terminal_compatible_status_text() {
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
                SessionEventKind::RalphLifecycle {
                    loop_name: "loop".to_owned(),
                    state_dir: PathBuf::from("/tmp/project/.bcode/ralph/loop"),
                    kind: "started".to_owned(),
                    message: "running".to_owned(),
                    occurred_at_ms: 2,
                },
            ),
        ]);

        assert!(matches!(
            &snapshot.transcript.items[0].kind,
            TranscriptViewItemKind::SystemMessage { message }
                if message.text
                    == "Ralph started\n* Loop: loop\n* running\n* State: .bcode/ralph/loop"
        ));
    }

    #[test]
    fn projects_skill_events_as_skill_items() {
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[
            event(
                session_id,
                1,
                SessionEventKind::SkillInvoked {
                    skill_id: bcode_skill_models::SkillId::new("review"),
                    arguments: "{}".to_owned(),
                    source: None,
                    invoked_at_ms: 1,
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::SkillContextLoaded {
                    skill_id: bcode_skill_models::SkillId::new("review"),
                    bytes_loaded: 42,
                    truncated: true,
                    source: Some(bcode_skill_models::SkillSource {
                        kind: bcode_skill_models::SkillSourceKind::User,
                        label: "user skills".to_owned(),
                        path: Some("/skills/review/SKILL.md".to_owned()),
                        precedence: 10,
                    }),
                    preview: Some("preview".to_owned()),
                    loaded_at_ms: 2,
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::SkillInvocationFailed {
                    skill_id: bcode_skill_models::SkillId::new("review"),
                    error: "boom".to_owned(),
                    failed_at_ms: 3,
                },
            ),
        ]);

        assert!(matches!(
            &snapshot.transcript.items[0].kind,
            TranscriptViewItemKind::Skill { skill }
                if skill.skill_id == "review"
                    && skill.status == SkillViewStatus::Invoked
                    && skill.text == "invoked review\nArguments: {}"
        ));
        assert!(matches!(
            &snapshot.transcript.items[1].kind,
            TranscriptViewItemKind::Skill { skill }
                if skill.skill_id == "review"
                    && skill.status == SkillViewStatus::ContextLoaded
                    && skill.text == "loaded review\nSource: user skills\nFile: /skills/review/SKILL.md\nBytes: 42 truncated\n\nPreview:\npreview"
        ));
        assert!(matches!(
            &snapshot.transcript.items[2].kind,
            TranscriptViewItemKind::Skill { skill }
                if skill.skill_id == "review"
                    && skill.status == SkillViewStatus::Failed
                    && skill.text == "review: boom"
        ));
    }
}
