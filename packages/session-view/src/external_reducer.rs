//! `SessionView` reduction of managed external harness events in both reducer paths.
//!
//! The ordinary and bounded reducers share one mutation routine. Every mutation is journaled so the
//! bounded reducer can charge exactly the touched owners after applying, and restore the previous
//! state and ledger atomically when the retention budget rejects the event.

use std::collections::BTreeMap;

use bcode_harness_models::HarnessCapabilities;
use bcode_session_models::{SessionEvent, SessionTokenUsage, TextStreamUpdate};
use bcode_session_view_models::external::ExternalInteractionView;
use bcode_session_view_models::retention::{
    ObservationRetentionExceeded, ObservationRetentionLimits, ObservationRetentionUsage,
};
use bcode_session_view_models::{
    ChatMessageView, SessionRuntimeViewState, TextStreamViewState, TextStreamViewStatus,
    ToolInvocationView, ToolInvocationViewStatus, ToolResultView, ToolTimingView,
    TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};

use crate::external_projection::{
    ExternalEffect, ExternalProjection, ExternalProjectionContext,
    MAX_PENDING_EXTERNAL_INTERACTIONS, project_external_event,
};
use crate::{LiveReasoningActivity, PendingTextPresentation, SessionView};

/// Retained managed-external presentation state that outlives individual events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalSessionState {
    /// Harness display label for assistant items.
    pub label: Option<String>,
    /// Latest native capabilities retained by configuration.
    pub capabilities: Option<Box<HarnessCapabilities>>,
    /// Highest observed ownership generation.
    pub generation: Option<u64>,
}

impl ExternalSessionState {
    /// Inspect retained label and capability strings at projector-cache depth.
    pub(crate) fn retention_usage(
        &self,
        limits: ObservationRetentionLimits,
    ) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
        let Self {
            label,
            capabilities,
            generation: _,
        } = self;
        // An ordinary session retains no external state; only present owners are charged.
        let mut usage = ObservationRetentionUsage::default();
        if let Some(label) = label {
            usage.try_add(ObservationRetentionUsage::string(label, 2), limits)?;
        }
        if let Some(capabilities) = capabilities {
            let HarnessCapabilities {
                resume,
                steering,
                approval,
                questions,
                assistance,
                interrupt,
                authoritative_settlement,
                background_continuation,
                authentication,
                model_switch,
                permission_mode,
            } = capabilities.as_ref();
            for capability in [
                resume,
                steering,
                approval,
                questions,
                assistance,
                interrupt,
                authoritative_settlement,
                background_continuation,
                authentication,
                model_switch,
                permission_mode,
            ] {
                usage.try_add(
                    ObservationRetentionUsage {
                        value_nodes: 2,
                        depth: 3,
                        ..ObservationRetentionUsage::default()
                    },
                    limits,
                )?;
                usage.try_add(
                    ObservationRetentionUsage::collection(capability.constraints.capacity(), 4),
                    limits,
                )?;
                for constraint in &capability.constraints {
                    usage.try_add(ObservationRetentionUsage::string(constraint, 5), limits)?;
                }
            }
        }
        Ok(usage)
    }
}

/// Undo information for one external event; only first-touch values are retained.
#[derive(Default)]
pub struct ExternalJournal {
    snapshot_revision: u64,
    transcript_revision: u64,
    old_len: usize,
    old_capacity: usize,
    old_buffer: Option<Vec<TranscriptViewItem>>,
    replaced: Vec<(usize, TranscriptViewItem)>,
    streams: BTreeMap<TranscriptViewItemId, Option<TextStreamViewState>>,
    removed_updates: Vec<(TranscriptViewItemId, TextStreamUpdate)>,
    removed_pending_text: Vec<(TranscriptViewItemId, PendingTextPresentation)>,
    reasoning: BTreeMap<(String, String), Option<LiveReasoningActivity>>,
    runtime: Option<Box<SessionRuntimeViewState>>,
    usage_requests: BTreeMap<String, Option<SessionTokenUsage>>,
    unattributed: Option<(u64, bcode_session_view_models::SessionCostSummary)>,
    pending: Option<Vec<ExternalInteractionView>>,
    lifecycle: Option<bcode_session_view_models::execution::ExecutionLifecycleView>,
    external: Option<ExternalSessionState>,
}

impl ExternalJournal {
    fn save_item(&mut self, index: usize, old: TranscriptViewItem) {
        if index < self.old_len && !self.replaced.iter().any(|(saved, _)| *saved == index) {
            self.replaced.push((index, old));
        }
    }

    fn reserve(&mut self, items: &mut Vec<TranscriptViewItem>, additional: usize) {
        if items.len() < items.capacity() || self.old_buffer.is_some() {
            return;
        }
        // Keep the original allocation so a rejected event restores its exact capacity.
        let mut original = std::mem::take(items);
        let mut grown = Vec::with_capacity(original.len().saturating_add(additional.max(1)));
        grown.append(&mut original);
        *items = grown;
        self.old_buffer = Some(original);
    }
}

const UNBOUNDED: ObservationRetentionLimits = ObservationRetentionLimits {
    scalar_capacity_bytes: usize::MAX,
    collection_entries: usize::MAX,
    value_nodes: usize::MAX,
    depth: usize::MAX,
    copies: usize::MAX,
};

/// Move the original value into a journal, leaving an equal copy in place to be mutated.
///
/// The journal must hold the exact original allocation: retained-usage accounting measures
/// allocated capacity, which a clone does not preserve, so inspecting a clone would release
/// less than was charged and rollback would not restore the charged state.
fn retain_original<T: Clone>(value: &mut T) -> T {
    let copy = value.clone();
    std::mem::replace(value, copy)
}

fn nested(
    mut usage: ObservationRetentionUsage,
    depth: usize,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    usage.depth = usage
        .depth
        .checked_add(depth)
        .ok_or(ObservationRetentionExceeded)?;
    Ok(usage)
}

fn text_stream_entry_usage(id: &TranscriptViewItemId) -> ObservationRetentionUsage {
    let mut usage = id.retention_usage(3);
    usage.collection_entries = 1;
    usage.value_nodes = usage.value_nodes.saturating_add(5);
    usage.depth = usage.depth.max(4);
    usage
}

fn text_update_entry_usage(
    id: &TranscriptViewItemId,
    update: &TextStreamUpdate,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut usage = ObservationRetentionUsage {
        collection_entries: 1,
        ..ObservationRetentionUsage::default()
    };
    usage.try_add(id.retention_usage(3), limits)?;
    usage.try_add(
        nested(
            bcode_session_models::inspect_text_stream(update, &limits)?,
            2,
        )?,
        limits,
    )?;
    Ok(usage)
}

fn request_usage_entry_usage(
    key: &String,
    value: &SessionTokenUsage,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut usage = ObservationRetentionUsage {
        collection_entries: 1,
        ..ObservationRetentionUsage::default()
    };
    usage.try_add(ObservationRetentionUsage::string(key, 3), limits)?;
    usage.try_add(
        nested(
            bcode_session_models::retention_events::inspect_session_token_usage(value, &limits)?,
            2,
        )?,
        limits,
    )?;
    Ok(usage)
}

fn item_usage(
    item: &TranscriptViewItem,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    nested(
        bcode_session_view_models::retention::inspect_transcript_item(item, limits)?,
        3,
    )
}

const fn effect_appends(effect: &ExternalEffect) -> bool {
    !matches!(
        effect,
        ExternalEffect::Usage(_)
            | ExternalEffect::InteractionRequested(_)
            | ExternalEffect::InteractionResolved { .. }
    )
}

fn finish_external_tool(
    tool: &mut ToolInvocationView,
    mut output: String,
    is_error: bool,
    truncated: bool,
    finished_at_ms: u64,
) {
    if truncated {
        output.push_str("\n… output truncated");
    }
    tool.status = if is_error {
        ToolInvocationViewStatus::Failed
    } else {
        ToolInvocationViewStatus::Finished
    };
    tool.is_error = Some(is_error);
    tool.result = Some(ToolResultView::Text {
        text: output.clone(),
    });
    tool.result_text = Some(output);
    tool.timing.finished_at_ms = Some(finished_at_ms);
    tool.timing.duration_ms = tool
        .timing
        .started_at_ms
        .and_then(|started| finished_at_ms.checked_sub(started))
        .filter(|duration| *duration > 0);
}

impl SessionView {
    fn external_projection_context(&self) -> ExternalProjectionContext<'_> {
        ExternalProjectionContext {
            label: self.external.label.as_deref(),
            capabilities: self.external.capabilities.as_deref(),
            generation: self.external.generation,
        }
    }

    /// Apply one external event: lifecycle evidence plus native-like presentation.
    ///
    /// The projection is validated before any mutation, so an invalid event leaves the view
    /// unchanged. The returned journal can undo the event. Callers own the revision bump.
    ///
    /// # Errors
    /// Returns the projection error for malformed canonical evidence.
    pub(crate) fn reduce_external_event(
        &mut self,
        event: &SessionEvent,
    ) -> Result<ExternalJournal, String> {
        let projection = project_external_event(event, self.external_projection_context())?;
        let mut journal = ExternalJournal {
            snapshot_revision: self.snapshot.revision,
            transcript_revision: self.snapshot.transcript.revision,
            old_len: self.snapshot.transcript.items.len(),
            old_capacity: self.snapshot.transcript.items.capacity(),
            ..ExternalJournal::default()
        };
        let old_lifecycle = retain_original(&mut self.snapshot.execution_lifecycle);
        let _ = crate::execution::apply(&mut self.snapshot.execution_lifecycle, event);
        journal.lifecycle = Some(old_lifecycle);
        self.apply_external_projection(event, projection, &mut journal);
        Ok(journal)
    }

    /// Restore the state captured by a journal.
    pub(crate) fn rollback_external(&mut self, journal: ExternalJournal) {
        let ExternalJournal {
            snapshot_revision,
            transcript_revision,
            old_len,
            old_capacity: _,
            old_buffer,
            replaced,
            streams,
            removed_updates,
            removed_pending_text,
            reasoning,
            runtime,
            usage_requests,
            unattributed,
            pending,
            lifecycle,
            external,
        } = journal;
        let items = &mut self.snapshot.transcript.items;
        for (index, old) in replaced.into_iter().rev() {
            items[index] = old;
        }
        items.truncate(old_len);
        if let Some(mut original) = old_buffer {
            original.append(items);
            *items = original;
        }
        for (id, old) in streams {
            if let Some(old) = old {
                self.snapshot.text_streams.insert(id, old);
            } else {
                self.snapshot.text_streams.remove(&id);
            }
        }
        self.last_text_stream_updates.extend(removed_updates);
        self.pending_text_presentations.extend(removed_pending_text);
        for (key, old) in reasoning {
            if let Some(old) = old {
                self.live_reasoning.insert(key, old);
            } else {
                self.live_reasoning.remove(&key);
            }
        }
        if let Some(runtime) = runtime {
            self.snapshot.runtime = *runtime;
        }
        for (key, old) in usage_requests {
            if let Some(old) = old {
                self.usage_by_request.insert(key, old);
            } else {
                self.usage_by_request.remove(&key);
            }
        }
        if let Some((metered, cost)) = unattributed {
            self.unattributed_metered_tokens = metered;
            self.unattributed_cost = cost;
        }
        if let Some(pending) = pending {
            self.snapshot.pending_external_interactions = pending;
        }
        if let Some(lifecycle) = lifecycle {
            self.snapshot.execution_lifecycle = lifecycle;
        }
        if let Some(external) = external {
            self.external = external;
        }
        self.snapshot.revision = snapshot_revision;
        self.snapshot.transcript.revision = transcript_revision;
    }

    /// Exact retained usage of every owner the journal touched, before and after the event.
    ///
    /// # Errors
    /// Returns an error when an owning inspector rejects a value or accounting overflows.
    pub(crate) fn external_journal_usage(
        &self,
        journal: &ExternalJournal,
        limits: ObservationRetentionLimits,
    ) -> Result<(ObservationRetentionUsage, ObservationRetentionUsage), ObservationRetentionExceeded>
    {
        let mut before = ObservationRetentionUsage::collection(journal.old_capacity, 3);
        let items = &self.snapshot.transcript.items;
        let mut after = ObservationRetentionUsage::collection(items.capacity(), 3);
        for (index, old) in &journal.replaced {
            before.try_add(item_usage(old, limits)?, UNBOUNDED)?;
            let current = items.get(*index).ok_or(ObservationRetentionExceeded)?;
            after.try_add(item_usage(current, limits)?, UNBOUNDED)?;
        }
        for current in items.iter().skip(journal.old_len) {
            after.try_add(item_usage(current, limits)?, UNBOUNDED)?;
        }
        for (id, old) in &journal.streams {
            if old.is_some() {
                before.try_add(text_stream_entry_usage(id), UNBOUNDED)?;
            }
            if self.snapshot.text_streams.contains_key(id) {
                after.try_add(text_stream_entry_usage(id), UNBOUNDED)?;
            }
        }
        for (id, update) in &journal.removed_updates {
            before.try_add(text_update_entry_usage(id, update, limits)?, UNBOUNDED)?;
        }
        for (id, pending) in &journal.removed_pending_text {
            let mut usage = nested(crate::inspect_pending_text(id, pending, limits)?, 1)?;
            usage.collection_entries = usage.collection_entries.saturating_add(1);
            before.try_add(usage, UNBOUNDED)?;
        }
        for (key, old) in &journal.reasoning {
            if let Some(old) = old {
                before.try_add(crate::reasoning_entry_usage(key, old, limits)?, UNBOUNDED)?;
            }
            if let Some(current) = self.live_reasoning.get(key) {
                after.try_add(
                    crate::reasoning_entry_usage(key, current, limits)?,
                    UNBOUNDED,
                )?;
            }
        }
        if let Some(old) = &journal.runtime {
            let inspect = bcode_session_view_models::retention::inspect_runtime;
            before.try_add(nested(inspect(old, limits)?, 1)?, UNBOUNDED)?;
            after.try_add(
                nested(inspect(&self.snapshot.runtime, limits)?, 1)?,
                UNBOUNDED,
            )?;
        }
        for (key, old) in &journal.usage_requests {
            if let Some(old) = old {
                before.try_add(request_usage_entry_usage(key, old, limits)?, UNBOUNDED)?;
            }
            if let Some(current) = self.usage_by_request.get(key) {
                after.try_add(request_usage_entry_usage(key, current, limits)?, UNBOUNDED)?;
            }
        }
        if let Some((_, old)) = &journal.unattributed {
            let inspect = bcode_session_view_models::retention::inspect_cost;
            before.try_add(nested(inspect(old, limits)?, 2)?, UNBOUNDED)?;
            after.try_add(
                nested(inspect(&self.unattributed_cost, limits)?, 2)?,
                UNBOUNDED,
            )?;
        }
        if let Some(old) = &journal.pending {
            let inspect =
                bcode_session_view_models::retention::inspect_pending_external_interaction_list;
            before.try_add(inspect(old, limits)?, UNBOUNDED)?;
            after.try_add(
                inspect(&self.snapshot.pending_external_interactions, limits)?,
                UNBOUNDED,
            )?;
        }
        if let Some(old) = &journal.lifecycle {
            let inspect = bcode_session_view_models::retention::inspect_execution_lifecycle;
            before.try_add(nested(inspect(old, limits)?, 1)?, UNBOUNDED)?;
            after.try_add(
                nested(inspect(&self.snapshot.execution_lifecycle, limits)?, 1)?,
                UNBOUNDED,
            )?;
        }
        if let Some(old) = &journal.external {
            before.try_add(nested(old.retention_usage(limits)?, 1)?, UNBOUNDED)?;
            after.try_add(
                nested(self.external.retention_usage(limits)?, 1)?,
                UNBOUNDED,
            )?;
        }
        Ok((before, after))
    }

    fn update_external_state(
        &mut self,
        projection: &mut ExternalProjection,
        journal: &mut ExternalJournal,
    ) {
        let mut next = self.external.clone();
        if let Some(label) = projection.label.take() {
            next.label = Some(label);
        }
        if let Some(capabilities) = projection.capabilities.take() {
            next.capabilities = Some(Box::new(capabilities));
        }
        if let Some(generation) = projection.generation {
            next.generation = Some(
                next.generation
                    .map_or(generation, |old| old.max(generation)),
            );
        }
        if next == self.external {
            return;
        }
        let advanced = next.generation > self.external.generation;
        journal.external = Some(std::mem::replace(&mut self.external, next));
        if advanced && let Some(current) = self.external.generation {
            // A newer ownership generation makes earlier native requests unanswerable.
            if self
                .snapshot
                .pending_external_interactions
                .iter()
                .any(|pending| pending.provenance.generation < current)
            {
                self.journal_pending(journal);
                self.snapshot
                    .pending_external_interactions
                    .retain(|pending| pending.provenance.generation >= current);
            }
        }
    }

    fn journal_pending(&mut self, journal: &mut ExternalJournal) {
        if journal.pending.is_none() {
            journal.pending = Some(retain_original(
                &mut self.snapshot.pending_external_interactions,
            ));
        }
    }

    fn apply_external_projection(
        &mut self,
        event: &SessionEvent,
        mut projection: ExternalProjection,
        journal: &mut ExternalJournal,
    ) {
        self.update_external_state(&mut projection, journal);
        let mut remaining = projection
            .effects
            .iter()
            .filter(|effect| effect_appends(effect))
            .count();
        for effect in projection.effects {
            if effect_appends(&effect) {
                remaining = remaining.saturating_sub(1);
            }
            self.apply_external_effect(journal, event, effect, remaining);
        }
    }

    fn apply_external_effect(
        &mut self,
        journal: &mut ExternalJournal,
        event: &SessionEvent,
        effect: ExternalEffect,
        remaining: usize,
    ) {
        match effect {
            ExternalEffect::User { id, text, steering } => {
                let mut message = ChatMessageView::markdown(text);
                if steering {
                    message.display_label = Some("Steering".to_owned());
                }
                self.upsert_external_item(
                    journal,
                    event,
                    TranscriptViewItemId::new(id),
                    TranscriptViewItemKind::UserMessage { message },
                    remaining,
                );
            }
            ExternalEffect::Assistant {
                id,
                text,
                segmented,
            } => {
                let id = TranscriptViewItemId::new(id);
                if segmented {
                    self.terminalize_external_stream(journal, &id, text.len());
                }
                self.upsert_external_assistant(journal, event, id, text, remaining);
            }
            ExternalEffect::Result { id, text } => {
                if self.latest_assistant_text().map(str::trim) != Some(text.trim()) {
                    self.upsert_external_assistant(
                        journal,
                        event,
                        TranscriptViewItemId::new(id),
                        text,
                        remaining,
                    );
                }
            }
            ExternalEffect::Reasoning {
                turn_id,
                activity_id,
                text,
                opaque,
            } => self.apply_external_reasoning(
                journal,
                event,
                (&turn_id, &activity_id),
                text,
                opaque,
                remaining,
            ),
            tool @ (ExternalEffect::ToolStarted { .. } | ExternalEffect::ToolFinished { .. }) => {
                self.apply_external_tool_effect(journal, event, tool, remaining);
            }
            ExternalEffect::Usage(usage) => self.apply_external_usage(journal, &usage),
            ExternalEffect::Notice { id, text } => self.upsert_external_item(
                journal,
                event,
                TranscriptViewItemId::new(id),
                TranscriptViewItemKind::SystemMessage {
                    message: ChatMessageView::plain(text),
                },
                remaining,
            ),
            ExternalEffect::InteractionRequested(view) => {
                self.request_external_interaction(journal, *view);
            }
            ExternalEffect::InteractionResolved {
                generation,
                request_id,
            } => self.resolve_external_interaction(journal, generation, &request_id),
        }
    }

    fn apply_external_tool_effect(
        &mut self,
        journal: &mut ExternalJournal,
        event: &SessionEvent,
        effect: ExternalEffect,
        remaining: usize,
    ) {
        match effect {
            ExternalEffect::ToolStarted {
                tool_call_id,
                name,
                input_json,
                plugin_id,
            } => {
                let id = TranscriptViewItemId::tool(&tool_call_id);
                let mut tool = self.external_tool(&id, tool_call_id);
                tool.tool_name = Some(name);
                if plugin_id.is_some() {
                    tool.producer_plugin_id = plugin_id;
                }
                if input_json.is_some() {
                    tool.arguments_json = input_json;
                }
                if tool.timing.started_at_ms.is_none() {
                    tool.timing.started_at_ms = Some(event.timestamp_ms);
                }
                let kind = TranscriptViewItemKind::ToolInvocation {
                    tool: Box::new(tool),
                };
                self.upsert_external_item(journal, event, id, kind, remaining);
            }
            ExternalEffect::ToolFinished {
                tool_call_id,
                output,
                is_error,
                truncated,
            } => {
                let id = TranscriptViewItemId::tool(&tool_call_id);
                let mut tool = self.external_tool(&id, tool_call_id);
                finish_external_tool(&mut tool, output, is_error, truncated, event.timestamp_ms);
                let kind = TranscriptViewItemKind::ToolInvocation {
                    tool: Box::new(tool),
                };
                self.upsert_external_item(journal, event, id, kind, remaining);
            }
            _ => {}
        }
    }

    fn request_external_interaction(
        &mut self,
        journal: &mut ExternalJournal,
        view: ExternalInteractionView,
    ) {
        self.journal_pending(journal);
        let pending = &mut self.snapshot.pending_external_interactions;
        pending.retain(|existing| {
            existing.provenance.generation != view.provenance.generation
                || existing.request_id != view.request_id
        });
        pending.push(view);
        if pending.len() > MAX_PENDING_EXTERNAL_INTERACTIONS {
            let excess = pending.len() - MAX_PENDING_EXTERNAL_INTERACTIONS;
            pending.drain(..excess);
        }
    }

    fn resolve_external_interaction(
        &mut self,
        journal: &mut ExternalJournal,
        generation: u64,
        request_id: &str,
    ) {
        let matches = |pending: &ExternalInteractionView| {
            pending.provenance.generation == generation && pending.request_id == request_id
        };
        if self
            .snapshot
            .pending_external_interactions
            .iter()
            .any(matches)
        {
            self.journal_pending(journal);
            self.snapshot
                .pending_external_interactions
                .retain(|pending| !matches(pending));
        }
    }

    fn latest_assistant_text(&self) -> Option<&str> {
        self.snapshot
            .transcript
            .items
            .iter()
            .rev()
            .find_map(|item| match &item.kind {
                TranscriptViewItemKind::AssistantMessage { message } => Some(message.text.as_str()),
                _ => None,
            })
    }

    fn external_tool(&self, id: &TranscriptViewItemId, tool_call_id: String) -> ToolInvocationView {
        self.snapshot
            .transcript
            .items
            .iter()
            .find(|item| &item.id == id)
            .and_then(|item| match &item.kind {
                TranscriptViewItemKind::ToolInvocation { tool } => Some(tool.as_ref().clone()),
                _ => None,
            })
            .unwrap_or_else(|| ToolInvocationView {
                tool_call_id,
                producer_plugin_id: None,
                tool_name: None,
                arguments_json: None,
                working_directory: None,
                request_draft: None,
                status: ToolInvocationViewStatus::Running,
                result_text: None,
                is_error: None,
                result: None,
                presentation: None,
                timing: ToolTimingView::default(),
            })
    }

    fn upsert_external_assistant(
        &mut self,
        journal: &mut ExternalJournal,
        event: &SessionEvent,
        id: TranscriptViewItemId,
        text: String,
        remaining: usize,
    ) {
        let mut message = ChatMessageView::markdown(text);
        message.display_label.clone_from(&self.external.label);
        self.upsert_external_item(
            journal,
            event,
            id,
            TranscriptViewItemKind::AssistantMessage { message },
            remaining,
        );
    }

    fn upsert_external_item(
        &mut self,
        journal: &mut ExternalJournal,
        event: &SessionEvent,
        id: TranscriptViewItemId,
        kind: TranscriptViewItemKind,
        remaining: usize,
    ) {
        let sequence = (event.sequence != 0).then_some(event.sequence);
        let items = &mut self.snapshot.transcript.items;
        if let Some(index) = items.iter().position(|item| item.id == id) {
            let old = &items[index];
            let replacement = TranscriptViewItem {
                id,
                revision: old.revision.saturating_add(1),
                sequence: sequence.or(old.sequence),
                timestamp_ms: Some(event.timestamp_ms),
                output_location: old.output_location.clone(),
                streaming: false,
                kind,
            };
            let old = std::mem::replace(&mut items[index], replacement);
            journal.save_item(index, old);
        } else {
            journal.reserve(items, remaining.saturating_add(1));
            items.push(TranscriptViewItem {
                id,
                revision: 0,
                sequence,
                timestamp_ms: Some(event.timestamp_ms),
                output_location: None,
                streaming: false,
                kind,
            });
        }
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
    }

    /// Durable text with a live identity supersedes the stream; late live updates are ignored.
    fn terminalize_external_stream(
        &mut self,
        journal: &mut ExternalJournal,
        id: &TranscriptViewItemId,
        accepted_bytes: usize,
    ) {
        let existing = self.snapshot.text_streams.get(id).cloned();
        let state = TextStreamViewState {
            generation: existing.as_ref().map_or(0, |state| state.generation),
            revision: existing
                .as_ref()
                .map_or(0, |state| state.revision.saturating_add(1)),
            accepted_bytes,
            truncated: false,
            status: TextStreamViewStatus::Terminal(
                bcode_session_models::TextStreamTerminalStatus::Completed,
            ),
        };
        journal.streams.entry(id.clone()).or_insert(existing);
        self.snapshot.text_streams.insert(id.clone(), state);
        if let Some(update) = self.last_text_stream_updates.remove(id) {
            journal.removed_updates.push((id.clone(), update));
        }
        if let Some(pending) = self.pending_text_presentations.remove(id) {
            journal.removed_pending_text.push((id.clone(), pending));
        }
    }

    fn apply_external_reasoning(
        &mut self,
        journal: &mut ExternalJournal,
        event: &SessionEvent,
        (turn_id, activity_id): (&str, &str),
        text: String,
        opaque: bool,
        remaining: usize,
    ) {
        let activity = bcode_session_models::ReasoningActivity {
            activity_id: activity_id.to_owned(),
            order: 0,
            status: bcode_session_models::ReasoningActivityStatus::Completed,
            parts: if text.is_empty() {
                Vec::new()
            } else {
                vec![bcode_session_models::ReasoningPart {
                    part_id: activity_id.to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Legacy,
                    role: bcode_session_models::ReasoningContentRole::Unknown,
                    order: 0,
                    text,
                }]
            },
            opaque,
        };
        let stream_id = TranscriptViewItemId::new(format!(
            "reasoning-stream:{turn_id}:activity:{activity_id}:part:{activity_id}"
        ));
        let accepted = activity.parts.first().map_or(0, |part| part.text.len());
        self.terminalize_external_stream(journal, &stream_id, accepted);
        let key = (turn_id.to_owned(), activity_id.to_owned());
        let old = self
            .live_reasoning
            .insert(key.clone(), LiveReasoningActivity::from_complete(&activity));
        journal.reasoning.entry(key).or_insert(old);
        let view = crate::reasoning_activity_view(
            turn_id,
            &activity,
            self.snapshot.thinking.visible,
            self.snapshot.thinking.mode,
        );
        self.upsert_external_item(
            journal,
            event,
            TranscriptViewItemId::reasoning(turn_id, activity_id),
            TranscriptViewItemKind::ReasoningActivity { activity: view },
            remaining,
        );
    }

    fn apply_external_usage(&mut self, journal: &mut ExternalJournal, usage: &SessionTokenUsage) {
        // The daemon's usage summary never folds in native harness usage, so its accounting
        // checkpoint does not cover these events. Keyed entries make replays idempotent.
        if journal.runtime.is_none() {
            journal.runtime = Some(Box::new(retain_original(&mut self.snapshot.runtime)));
        }
        if journal.unattributed.is_none() {
            journal.unattributed = Some((
                self.unattributed_metered_tokens,
                retain_original(&mut self.unattributed_cost),
            ));
        }
        if let Some(key) = &usage.request_id
            && !journal.usage_requests.contains_key(key)
        {
            let old = self.usage_by_request.get_mut(key).map(retain_original);
            journal.usage_requests.insert(key.clone(), old);
        }
        self.observe_model_usage(usage);
    }

    /// Label a live assistant item of a managed external session with the harness name.
    pub(crate) fn label_external_live_item(&mut self, id: &TranscriptViewItemId) -> bool {
        let Some(label) = self.external.label.as_ref() else {
            return false;
        };
        let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| &item.id == id)
        else {
            return false;
        };
        let TranscriptViewItemKind::AssistantMessage { message } = &mut item.kind else {
            return false;
        };
        if message.display_label.is_some() {
            return false;
        }
        message.display_label = Some(label.clone());
        true
    }

    /// Bounded variant of [`Self::label_external_live_item`]; presentation-only, so a rejected
    /// charge leaves the item unlabeled and the ledger unchanged.
    pub(crate) fn label_external_live_item_bounded(
        &mut self,
        id: &TranscriptViewItemId,
        retained: &mut ObservationRetentionUsage,
        limits: ObservationRetentionLimits,
    ) {
        if self.external.label.is_none() {
            return;
        }
        let Some(index) = self
            .snapshot
            .transcript
            .items
            .iter()
            .position(|item| &item.id == id)
        else {
            return;
        };
        let Ok(before) = item_usage(&self.snapshot.transcript.items[index], limits) else {
            return;
        };
        if !self.label_external_live_item(id) {
            return;
        }
        let charged =
            item_usage(&self.snapshot.transcript.items[index], limits).and_then(|after| {
                let mut next = *retained;
                next.try_release(before)?;
                next.try_add(after, limits)?;
                Ok(next)
            });
        match charged {
            Ok(next) => *retained = next,
            Err(_) => {
                if let TranscriptViewItemKind::AssistantMessage { message } =
                    &mut self.snapshot.transcript.items[index].kind
                {
                    message.display_label = None;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "external_reducer_tests.rs"]
mod tests;
