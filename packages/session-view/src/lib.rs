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
    ChatMessageView, CompactionView, CompactionViewStatus, ComposerViewState, InteractionViewState,
    InteractionViewSummary, PluginStatusView, ProviderProgressView, SessionViewSnapshot, SkillView,
    SkillViewStatus, StreamingInterpolationCurve, StreamingPresentationPolicy, TextFormat,
    TextStreamViewState, TextStreamViewStatus, ThinkingViewState, ToolInvocationView,
    ToolInvocationViewStatus, ToolRequestDraftView, ToolResultView, ToolTimingView,
    TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind, TurnOutputLocation,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

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

fn tool_invocation_id(event: &SessionEvent) -> Option<&str> {
    match &event.kind {
        SessionEventKind::ToolCallRequested { tool_call_id, .. }
        | SessionEventKind::PositionedToolCallRequested { tool_call_id, .. } => Some(tool_call_id),
        SessionEventKind::ToolInvocationLifecycle { event } => Some(&event.invocation_id),
        SessionEventKind::ToolInvocationResultRecorded { record } => Some(&record.invocation_id),
        _ => None,
    }
}

fn interaction_from_exchange_request(
    request: &bcode_session_models::ToolExchangeRequest,
) -> InteractionViewSummary {
    InteractionViewSummary {
        interaction_id: request.exchange_id.clone(),
        producer_id: Some(request.producer_id.clone()),
        exchange_schema: Some(request.schema.clone()),
        exchange_schema_version: Some(request.schema_version),
        kind: request.schema.clone(),
        tool_call_id: Some(request.invocation_id.clone()),
        title: Some(request.producer_id.clone()),
        required: request.response_policy
            == bcode_session_models::ToolExchangeResponsePolicy::Required,
        snapshot: Some(request.payload.clone()),
        state: InteractionViewState::Pending,
        status_detail: None,
        resolved: false,
        resolution: None,
    }
}

fn merge_hydrated_interaction(
    existing: Option<&InteractionViewSummary>,
    mut hydrated: InteractionViewSummary,
) -> InteractionViewSummary {
    let Some(existing) = existing else {
        return hydrated;
    };
    if hydrated.producer_id.is_none() {
        hydrated.producer_id.clone_from(&existing.producer_id);
    }
    if hydrated.exchange_schema.is_none() {
        hydrated
            .exchange_schema
            .clone_from(&existing.exchange_schema);
    }
    if hydrated.exchange_schema_version.is_none() {
        hydrated.exchange_schema_version = existing.exchange_schema_version;
    }
    hydrated
}

fn terminal_interaction_without_request(
    interaction_id: &str,
    resolution: &bcode_session_models::ToolExchangeResolution,
) -> InteractionViewSummary {
    InteractionViewSummary {
        interaction_id: interaction_id.to_owned(),
        producer_id: None,
        exchange_schema: None,
        exchange_schema_version: None,
        kind: "bcode.tool.exchange".to_owned(),
        tool_call_id: None,
        title: Some("Interaction".to_owned()),
        required: false,
        snapshot: None,
        state: interaction_state_for_resolution(resolution),
        status_detail: interaction_status_detail(resolution),
        resolved: true,
        resolution: Some(resolution.clone()),
    }
}

const fn interaction_state_for_resolution(
    resolution: &bcode_session_models::ToolExchangeResolution,
) -> InteractionViewState {
    match resolution {
        bcode_session_models::ToolExchangeResolution::Responded { .. } => {
            InteractionViewState::Resolved
        }
        bcode_session_models::ToolExchangeResolution::Cancelled
        | bcode_session_models::ToolExchangeResolution::TimedOut
        | bcode_session_models::ToolExchangeResolution::NoCompatibleConsumer
        | bcode_session_models::ToolExchangeResolution::ConsumerDetached => {
            InteractionViewState::Cancelled
        }
        bcode_session_models::ToolExchangeResolution::Failed { .. } => {
            InteractionViewState::ActionError
        }
    }
}

fn interaction_status_detail(
    resolution: &bcode_session_models::ToolExchangeResolution,
) -> Option<String> {
    match resolution {
        bcode_session_models::ToolExchangeResolution::Responded { .. } => None,
        bcode_session_models::ToolExchangeResolution::Cancelled => Some("cancelled".to_owned()),
        bcode_session_models::ToolExchangeResolution::TimedOut => Some("timed out".to_owned()),
        bcode_session_models::ToolExchangeResolution::NoCompatibleConsumer => {
            Some("no compatible consumer".to_owned())
        }
        bcode_session_models::ToolExchangeResolution::ConsumerDetached => {
            Some("consumer detached".to_owned())
        }
        bcode_session_models::ToolExchangeResolution::Failed { code, message } => {
            Some(format!("{code}: {message}"))
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ToolInvocationAggregate {
    projection: ToolInvocationProjection,
    terminal_status: Option<ToolInvocationViewStatus>,
    request_draft: Option<ToolRequestDraftView>,
    terminal_request_draft: Option<(u64, u64)>,
    presentations:
        BTreeMap<bcode_tool::ToolPresentationIdentity, bcode_tool::ToolPresentationUpdate>,
    presentation_scope: bcode_tool::ToolPresentationUpdateScope,
}

impl ToolInvocationAggregate {
    const fn is_terminal(&self) -> bool {
        self.terminal_status.is_some()
    }
}

const MAX_RETAINED_PENDING_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
enum PendingTextDestination {
    Transcript {
        turn_id: String,
    },
    ReasoningPart {
        turn_id: String,
        activity_id: String,
        activity_order: u32,
        part_id: String,
        kind: bcode_session_models::ReasoningContentKind,
        role: bcode_session_models::ReasoningContentRole,
        part_order: u32,
    },
}

impl PendingTextDestination {
    fn turn_id(&self) -> &str {
        match self {
            Self::Transcript { turn_id } | Self::ReasoningPart { turn_id, .. } => turn_id,
        }
    }
}

#[derive(Debug, Clone)]
struct PendingTextPresentation {
    target: String,
    presented_bytes: usize,
    paced_from: Instant,
    presented_since_pace: usize,
    remaining_graphemes: usize,
    graphemes_per_second: u32,
    max_lag: Duration,
    destination: PendingTextDestination,
}

impl PendingTextPresentation {
    fn new(
        target: String,
        presented_bytes: usize,
        now: Instant,
        graphemes_per_second: u32,
        max_lag: Duration,
        destination: PendingTextDestination,
    ) -> Self {
        let remaining_graphemes = target[presented_bytes..].graphemes(true).count();
        Self {
            target,
            presented_bytes,
            paced_from: now,
            presented_since_pace: 0,
            remaining_graphemes,
            graphemes_per_second,
            max_lag,
            destination,
        }
    }

    fn rebase(
        &mut self,
        target: String,
        now: Instant,
        graphemes_per_second: u32,
        max_lag: Duration,
        destination: PendingTextDestination,
    ) {
        let _ = now;
        self.target = target;
        self.remaining_graphemes = self
            .presented_since_pace
            .saturating_add(self.target[self.presented_bytes..].graphemes(true).count());
        self.graphemes_per_second = graphemes_per_second;
        self.max_lag = max_lag;
        self.destination = destination;
    }

    const fn adopt_policy(&mut self, policy: StreamingPresentationPolicy) {
        self.graphemes_per_second = policy.graphemes_per_second;
        self.max_lag = Duration::from_millis(policy.max_lag_ms);
    }

    fn effective_rate(&self) -> u128 {
        let lag_nanos = self.max_lag.as_nanos().max(1);
        let catch_up_rate = u128::try_from(self.remaining_graphemes)
            .unwrap_or(u128::MAX)
            .saturating_mul(1_000_000_000)
            .saturating_add(lag_nanos.saturating_sub(1))
            .checked_div(lag_nanos)
            .unwrap_or(u128::MAX);
        u128::from(self.graphemes_per_second)
            .max(catch_up_rate)
            .max(1)
    }

    fn due_graphemes(&self, now: Instant, curve: StreamingInterpolationCurve) -> usize {
        const SCALE: u128 = 1_000_000;
        let elapsed_nanos = now.saturating_duration_since(self.paced_from).as_nanos();
        let rate = self.effective_rate();
        let linear_due = rate
            .saturating_mul(elapsed_nanos)
            .checked_div(1_000_000_000)
            .unwrap_or(u128::MAX);
        let total_remaining = self.remaining_graphemes;
        if total_remaining == 0 {
            return 0;
        }
        let linear_due = usize::try_from(linear_due)
            .unwrap_or(total_remaining)
            .min(total_remaining);
        if rate <= u128::from(self.graphemes_per_second)
            || curve == StreamingInterpolationCurve::Linear
        {
            return linear_due;
        }
        let progress = u128::try_from(linear_due)
            .unwrap_or(u128::MAX)
            .saturating_mul(SCALE)
            .checked_div(u128::try_from(total_remaining).unwrap_or(u128::MAX).max(1))
            .unwrap_or(SCALE)
            .min(SCALE);
        let cubic = |value: u128| {
            value
                .saturating_mul(value)
                .checked_div(SCALE)
                .unwrap_or(SCALE)
                .saturating_mul(value)
                .checked_div(SCALE)
                .unwrap_or(SCALE)
        };
        let curved = match curve {
            StreamingInterpolationCurve::Linear => progress,
            StreamingInterpolationCurve::EaseIn => cubic(progress),
            StreamingInterpolationCurve::EaseOut => {
                SCALE.saturating_sub(cubic(SCALE.saturating_sub(progress)))
            }
            StreamingInterpolationCurve::EaseInOut if progress < SCALE / 2 => {
                cubic(progress).saturating_mul(4).min(SCALE)
            }
            StreamingInterpolationCurve::EaseInOut => SCALE.saturating_sub(
                cubic(SCALE.saturating_sub(progress))
                    .saturating_mul(4)
                    .min(SCALE),
            ),
        };
        usize::try_from(
            u128::try_from(total_remaining)
                .unwrap_or(u128::MAX)
                .saturating_mul(curved)
                .checked_div(SCALE)
                .unwrap_or(0),
        )
        .unwrap_or(total_remaining)
        .min(total_remaining)
    }

    fn due_bytes(&self, now: Instant, curve: StreamingInterpolationCurve) -> usize {
        let total_due = self.due_graphemes(now, curve);
        let newly_due = total_due.saturating_sub(self.presented_since_pace);
        if newly_due == 0 {
            return self.presented_bytes;
        }
        self.target[self.presented_bytes..]
            .grapheme_indices(true)
            .nth(newly_due)
            .map_or(self.target.len(), |(offset, _)| {
                self.presented_bytes + offset
            })
    }

    fn record_presented(&mut self, bytes: usize) {
        let newly_presented = self.target[self.presented_bytes..bytes]
            .graphemes(true)
            .count();
        self.presented_bytes = bytes;
        self.presented_since_pace = self.presented_since_pace.saturating_add(newly_presented);
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let interval_nanos = 1_000_000_000_u128
            .saturating_add(self.effective_rate().saturating_sub(1))
            .checked_div(self.effective_rate())
            .unwrap_or(1)
            .max(1_000_000);
        let interval = Duration::from_nanos(u64::try_from(interval_nanos).unwrap_or(u64::MAX));
        now.checked_add(interval).unwrap_or(now)
    }
}

/// Renderer-neutral session view projection.
#[derive(Debug, Clone)]
pub struct SessionView {
    snapshot: SessionViewSnapshot,
    tool_item_ids: BTreeMap<String, TranscriptViewItemId>,
    interaction_item_ids: BTreeMap<String, TranscriptViewItemId>,
    tool_invocations: BTreeMap<String, ToolInvocationAggregate>,
    contribution_sequences: BTreeMap<String, u64>,
    contribution_placements: BTreeMap<String, bcode_session_models::ToolContributionPlacement>,
    terminal_runtime_work: BTreeSet<bcode_session_models::WorkId>,
    terminal_interactions: BTreeMap<String, bcode_session_models::ToolExchangeResolution>,
    live_reasoning: BTreeMap<(String, String), LiveReasoningActivity>,
    last_text_stream_updates:
        BTreeMap<TranscriptViewItemId, bcode_session_models::TextStreamUpdate>,
    streaming_presentation_policy: StreamingPresentationPolicy,
    pending_text_presentations: BTreeMap<TranscriptViewItemId, PendingTextPresentation>,
    usage_by_request: BTreeMap<String, bcode_session_models::SessionTokenUsage>,
    unattributed_metered_tokens: u64,
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
            tool_invocations: BTreeMap::new(),
            contribution_sequences: BTreeMap::new(),
            contribution_placements: BTreeMap::new(),
            terminal_runtime_work: BTreeSet::new(),
            terminal_interactions: BTreeMap::new(),
            live_reasoning: BTreeMap::new(),
            last_text_stream_updates: BTreeMap::new(),
            streaming_presentation_policy: StreamingPresentationPolicy::immediate(),
            pending_text_presentations: BTreeMap::new(),
            usage_by_request: BTreeMap::new(),
            unattributed_metered_tokens: 0,
        }
    }

    #[cfg(test)]
    fn tool_request_drafts(&self) -> BTreeMap<String, ToolRequestDraftView> {
        self.tool_invocations
            .iter()
            .filter_map(|(invocation_id, aggregate)| {
                aggregate
                    .request_draft
                    .clone()
                    .map(|draft| (invocation_id.clone(), draft))
            })
            .collect()
    }

    #[cfg(test)]
    fn terminal_tool_request_drafts(&self) -> BTreeMap<String, (u64, u64)> {
        self.tool_invocations
            .iter()
            .filter_map(|(invocation_id, aggregate)| {
                aggregate
                    .terminal_request_draft
                    .map(|terminal| (invocation_id.clone(), terminal))
            })
            .collect()
    }

    /// Return the currently accepted presentation update for an invocation identity.
    #[must_use]
    pub fn presentation_update(
        &self,
        invocation_id: &str,
        identity: &bcode_tool::ToolPresentationIdentity,
    ) -> Option<&bcode_tool::ToolPresentationUpdate> {
        self.tool_invocations
            .get(invocation_id)
            .and_then(|aggregate| aggregate.presentations.get(identity))
    }

    /// Set the renderer-neutral live text presentation policy.
    ///
    /// Switching to immediate presentation flushes all accepted pending text.
    pub fn set_streaming_presentation_policy(
        &mut self,
        policy: StreamingPresentationPolicy,
    ) -> bool {
        let policy = policy.normalized();
        if self.streaming_presentation_policy == policy {
            return false;
        }
        self.streaming_presentation_policy = policy;
        if policy.is_immediate() {
            return self.flush_pending_text_presentations();
        }
        for pending in self.pending_text_presentations.values_mut() {
            pending.adopt_policy(policy);
        }
        false
    }

    /// Return the next time at which shared visible text may advance.
    #[must_use]
    pub fn next_streaming_presentation_deadline(&self, now: Instant) -> Option<Instant> {
        self.pending_text_presentations
            .values()
            .map(|pending| pending.next_deadline(now))
            .min()
    }

    /// Advance due shared visible text prefixes.
    ///
    /// Returns whether renderer-visible state changed.
    pub fn advance_streaming_presentation(&mut self, now: Instant) -> bool {
        let curve = self.streaming_presentation_policy.curve;
        let due = self
            .pending_text_presentations
            .iter()
            .filter_map(|(id, pending)| {
                let due_bytes = pending.due_bytes(now, curve);
                (due_bytes > pending.presented_bytes).then(|| (id.clone(), due_bytes))
            })
            .collect::<Vec<_>>();
        let mut changed = false;
        for (id, due_bytes) in due {
            let Some(pending) = self.pending_text_presentations.get_mut(&id) else {
                continue;
            };
            let text = pending.target[..due_bytes].to_owned();
            let destination = pending.destination.clone();
            pending.record_presented(due_bytes);
            changed |= self.replace_presented_text(&id, &text, &destination);
        }
        self.pending_text_presentations
            .retain(|_, pending| pending.presented_bytes < pending.target.len());
        changed
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

    /// Replace the renderer-neutral daemon/session connection status.
    pub fn set_connection_status(
        &mut self,
        connection_status: bcode_session_view_models::SessionConnectionViewStatus,
    ) {
        if self.snapshot.connection_status != connection_status {
            self.snapshot.connection_status = connection_status;
            self.bump_revision();
        }
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
            if self
                .terminal_interactions
                .contains_key(&interaction.interaction_id)
            {
                continue;
            }
            self.upsert_interaction(merge_hydrated_interaction(
                self.snapshot
                    .interactions
                    .iter()
                    .find(|existing| existing.interaction_id == interaction.interaction_id),
                interaction,
            ));
        }
    }

    /// Insert or replace renderer-neutral interaction state hydrated from the daemon.
    pub fn upsert_interaction(&mut self, interaction: InteractionViewSummary) {
        if self
            .terminal_interactions
            .contains_key(&interaction.interaction_id)
            && !interaction.resolved
        {
            return;
        }
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
    #[allow(clippy::too_many_lines)] // One replacement preserves all live supplemental state atomically.
    pub fn clear_history_window(&mut self) {
        let previous = self.snapshot.clone();
        let terminal_runtime_work = self.terminal_runtime_work.clone();
        let live_tool_request_drafts = self
            .tool_invocations
            .values()
            .filter_map(|aggregate| aggregate.request_draft.clone())
            .collect::<Vec<_>>();
        let terminal_tool_request_drafts = self
            .tool_invocations
            .iter()
            .filter_map(|(invocation_id, aggregate)| {
                aggregate
                    .terminal_request_draft
                    .map(|terminal| (invocation_id.clone(), terminal))
            })
            .collect::<BTreeMap<_, _>>();
        let live_presentations = self
            .tool_invocations
            .values()
            .flat_map(|aggregate| aggregate.presentations.values().cloned())
            .collect::<Vec<_>>();
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
        let retained_interactions = previous
            .interactions
            .iter()
            .cloned()
            .map(|interaction| {
                let metadata = previous
                    .transcript
                    .items
                    .iter()
                    .find_map(|item| match &item.kind {
                        TranscriptViewItemKind::Interaction {
                            interaction: item_interaction,
                        } if item_interaction.interaction_id == interaction.interaction_id => {
                            Some((item.sequence.unwrap_or(0), item.timestamp_ms))
                        }
                        _ => None,
                    });
                let (sequence, timestamp_ms) = metadata.unwrap_or((0, None));
                (interaction, sequence, timestamp_ms)
            })
            .collect::<Vec<_>>();
        let mut replacement = Self::new();
        replacement.streaming_presentation_policy = self.streaming_presentation_policy;
        replacement.terminal_interactions = self.terminal_interactions.clone();
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
        replacement.usage_by_request = self.usage_by_request.clone();
        replacement.unattributed_metered_tokens = self.unattributed_metered_tokens;
        replacement.snapshot.session_summary = previous.session_summary;
        replacement.snapshot.transcript.source_start_sequence =
            previous.transcript.source_start_sequence;
        replacement.snapshot.transcript.source_end_sequence =
            previous.transcript.source_end_sequence;
        replacement.snapshot.transcript.has_older_history = previous.transcript.has_older_history;
        replacement.snapshot.transcript.has_newer_history = previous.transcript.has_newer_history;
        replacement.terminal_runtime_work = terminal_runtime_work;
        for (invocation_id, terminal) in terminal_tool_request_drafts {
            replacement
                .tool_invocations
                .entry(invocation_id)
                .or_default()
                .terminal_request_draft = Some(terminal);
        }
        replacement.snapshot.revision = self.snapshot.revision.saturating_add(1);
        for draft in live_tool_request_drafts {
            replacement.apply_tool_request_draft(&bcode_session_models::ToolRequestDraftEvent {
                output_position: draft
                    .output_location
                    .as_ref()
                    .map(|location| location.position),
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
        for update in live_presentations {
            replacement.apply_presentation_update(&update);
        }
        for (contribution, placement) in live_contributions {
            replacement.apply_contribution_event(0, None, &contribution, placement);
        }
        for (interaction, sequence, timestamp_ms) in retained_interactions {
            replacement.upsert_interaction_item(interaction, sequence, timestamp_ms);
        }
        *self = replacement;
    }

    /// Set the authoritative bounded-history metadata supplied by the daemon.
    pub const fn set_history_window_metadata(
        &mut self,
        source_start_sequence: Option<u64>,
        source_end_sequence: Option<u64>,
        has_older_history: bool,
        has_newer_history: bool,
    ) {
        self.snapshot.transcript.source_start_sequence = source_start_sequence;
        self.snapshot.transcript.source_end_sequence = source_end_sequence;
        self.snapshot.transcript.has_older_history = has_older_history;
        self.snapshot.transcript.has_newer_history = has_newer_history;
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
        if let Some(invocation_id) = tool_invocation_id(event) {
            let aggregate = self
                .tool_invocations
                .entry(invocation_id.to_owned())
                .or_default();
            let projection = if aggregate.projection.tool_call_id.is_empty() {
                ToolInvocationProjection {
                    tool_call_id: invocation_id.to_owned(),
                    ..ToolInvocationProjection::default()
                }
            } else {
                std::mem::take(&mut aggregate.projection)
            };
            let mut projections = BTreeMap::from([(invocation_id.to_owned(), projection)]);
            apply_tool_invocation_projection_event(&mut projections, event);
            aggregate.projection =
                projections
                    .remove(invocation_id)
                    .unwrap_or_else(|| ToolInvocationProjection {
                        tool_call_id: invocation_id.to_owned(),
                        ..ToolInvocationProjection::default()
                    });
        }

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
            SessionEventKind::AssistantResponseSegment {
                turn_id,
                segment_id,
                text,
                ..
            } => {
                let id = TranscriptViewItemId::new(format!(
                    "assistant-turn:{turn_id}:segment:{segment_id}"
                ));
                // The durable segment is canonical. It replaces degraded live presentation state
                // rather than deferring to it; the state overwrite below restores coherence.
                self.finish_or_push_message(
                    id.clone(),
                    event.sequence,
                    Some(event.timestamp_ms),
                    StreamingMessageKind::Assistant,
                    text,
                );
                let accepted_bytes = text.len();
                self.snapshot.text_streams.insert(
                    id.clone(),
                    TextStreamViewState {
                        generation: self
                            .snapshot
                            .text_streams
                            .get(&id)
                            .map_or(0, |state| state.generation),
                        revision: self
                            .snapshot
                            .text_streams
                            .get(&id)
                            .map_or(0, |state| state.revision.saturating_add(1)),
                        accepted_bytes,
                        truncated: false,
                        status: TextStreamViewStatus::Terminal(
                            bcode_session_models::TextStreamTerminalStatus::Completed,
                        ),
                    },
                );
                self.last_text_stream_updates.remove(&id);
            }
            SessionEventKind::PositionedAssistantResponseSegment {
                turn_id,
                output_position,
                segment_id,
                text,
                ..
            } => {
                let id = TranscriptViewItemId::new(format!(
                    "assistant-turn:{turn_id}:segment:{segment_id}"
                ));
                // The durable segment is canonical. It replaces degraded live presentation state
                // rather than deferring to it; the state overwrite below restores coherence.
                self.finish_or_push_message(
                    id.clone(),
                    event.sequence,
                    Some(event.timestamp_ms),
                    StreamingMessageKind::Assistant,
                    text,
                );
                let accepted_bytes = text.len();
                self.snapshot.text_streams.insert(
                    id.clone(),
                    TextStreamViewState {
                        generation: self
                            .snapshot
                            .text_streams
                            .get(&id)
                            .map_or(0, |state| state.generation),
                        revision: self
                            .snapshot
                            .text_streams
                            .get(&id)
                            .map_or(0, |state| state.revision.saturating_add(1)),
                        accepted_bytes,
                        truncated: false,
                        status: TextStreamViewStatus::Terminal(
                            bcode_session_models::TextStreamTerminalStatus::Completed,
                        ),
                    },
                );
                self.last_text_stream_updates.remove(&id);
                self.assign_output_location(&id, turn_id, Some(*output_position));
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
            SessionEventKind::PositionedAssistantReasoningActivity {
                turn_id,
                output_position,
                activity,
            } => {
                let key = (turn_id.clone(), activity.activity_id.clone());
                self.live_reasoning
                    .insert(key, LiveReasoningActivity::from_complete(activity));
                let id = TranscriptViewItemId::reasoning(turn_id, &activity.activity_id);
                self.upsert_item(
                    id.clone(),
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
                self.assign_output_location(&id, turn_id, Some(*output_position));
            }
            SessionEventKind::ToolInvocationLifecycle { event: lifecycle } => {
                use bcode_session_models::ToolInvocationLifecycleStage;
                if self
                    .tool_invocations
                    .get(&lifecycle.invocation_id)
                    .is_some_and(ToolInvocationAggregate::is_terminal)
                {
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
                            .tool_invocations
                            .get(&lifecycle.invocation_id)
                            .is_some_and(|aggregate| aggregate.projection.tool_name.is_some())
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
                            self.tool_invocations
                                .entry(lifecycle.invocation_id.clone())
                                .or_default()
                                .terminal_status = Some(status);
                            self.apply_terminal_tool_status(
                                &lifecycle.invocation_id,
                                status,
                                lifecycle.message.as_deref(),
                                event.sequence,
                                Some(event.timestamp_ms),
                            );
                            if let Some(aggregate) =
                                self.tool_invocations.get_mut(&lifecycle.invocation_id)
                            {
                                aggregate.request_draft = None;
                            }
                            let result_id = TranscriptViewItemId::tool(&lifecycle.invocation_id);
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
                        } else if let Some(projection) = self
                            .tool_invocations
                            .get(&lifecycle.invocation_id)
                            .map(|aggregate| aggregate.projection.clone())
                            .filter(|projection| projection.tool_name.is_some())
                        {
                            let mut tool = tool_invocation_view_from_projection(projection);
                            self.attach_primary_presentation(&mut tool);
                            self.snapshot
                                .tools
                                .insert(lifecycle.invocation_id.clone(), tool);
                            self.sync_contribution_invocation_context(&lifecycle.invocation_id);
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
                        let aggregate = self
                            .tool_invocations
                            .entry(lifecycle.invocation_id.clone())
                            .or_default();
                        aggregate.terminal_status = Some(status);
                        self.close_presentation_scope(&lifecycle.invocation_id);
                    }
                }
                self.bump_revision();
            }
            SessionEventKind::ToolCallRequested { tool_call_id, .. } => {
                if self
                    .tool_invocations
                    .get(tool_call_id)
                    .and_then(|aggregate| aggregate.request_draft.as_ref())
                    .is_some_and(|draft| {
                        draft.placement == bcode_session_models::ToolContributionPlacement::Request
                    })
                    && let Some(aggregate) = self.tool_invocations.get_mut(tool_call_id)
                {
                    aggregate.request_draft = None;
                }
                if let Some(aggregate) = self.tool_invocations.get_mut(tool_call_id) {
                    aggregate.terminal_request_draft = None;
                }
                self.upsert_tool_item(tool_call_id, event.sequence, Some(event.timestamp_ms));
            }
            SessionEventKind::PositionedToolCallRequested {
                turn_id,
                output_position,
                tool_call_id,
                ..
            } => {
                if self
                    .tool_invocations
                    .get(tool_call_id)
                    .and_then(|aggregate| aggregate.request_draft.as_ref())
                    .is_some_and(|draft| {
                        draft.placement == bcode_session_models::ToolContributionPlacement::Request
                    })
                    && let Some(aggregate) = self.tool_invocations.get_mut(tool_call_id)
                {
                    aggregate.request_draft = None;
                }
                if let Some(aggregate) = self.tool_invocations.get_mut(tool_call_id) {
                    aggregate.terminal_request_draft = None;
                }
                self.upsert_tool_item(tool_call_id, event.sequence, Some(event.timestamp_ms));
                let id = self
                    .tool_item_ids
                    .get(tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| TranscriptViewItemId::tool(tool_call_id));
                self.assign_output_location(&id, turn_id, Some(*output_position));
            }
            SessionEventKind::ToolInvocationResultRecorded { record } => {
                let already_terminal = self
                    .tool_invocations
                    .get(&record.invocation_id)
                    .is_some_and(ToolInvocationAggregate::is_terminal);
                if already_terminal
                    && self
                        .snapshot
                        .tools
                        .get(&record.invocation_id)
                        .is_some_and(|tool| tool.result.is_some() || tool.result_text.is_some())
                {
                    return;
                }
                let aggregate = self
                    .tool_invocations
                    .entry(record.invocation_id.clone())
                    .or_default();
                if let Some(presentation) = record.presentation.as_ref()
                    && presentation.retention == bcode_tool::ToolPresentationRetention::RetainLatest
                {
                    let replace = aggregate
                        .presentations
                        .get(&presentation.identity)
                        .is_none_or(|current| {
                            (presentation.generation, presentation.revision)
                                > (current.generation, current.revision)
                        });
                    if replace {
                        aggregate
                            .presentations
                            .insert(presentation.identity.clone(), presentation.clone());
                    }
                }
                aggregate.terminal_status.get_or_insert(if record.is_error {
                    ToolInvocationViewStatus::Failed
                } else {
                    ToolInvocationViewStatus::Finished
                });
                self.close_presentation_scope(&record.invocation_id);
                self.snapshot
                    .active_invocations
                    .remove(&record.invocation_id);
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
            SessionEventKind::ModelChanged {
                provider, model, ..
            } => {
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
            SessionEventKind::ReasoningChanged {
                effort, summary, ..
            } => {
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
                let _ = self.flush_turn_text_presentations(turn_id);
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
            SessionEventKind::ModelUsage { turn_id: _, usage } => {
                let mut accepted = true;
                if let Some(request_id) = &usage.request_id {
                    accepted = self.usage_by_request.get(request_id).is_none_or(|current| {
                        !current.terminal
                            && (usage.observation_ordinal > current.observation_ordinal
                                || (usage.observation_ordinal == current.observation_ordinal
                                    && usage == current))
                    });
                    if accepted {
                        self.usage_by_request
                            .insert(request_id.clone(), usage.clone());
                    }
                    self.snapshot.runtime.cumulative_metered_tokens =
                        self.unattributed_metered_tokens.saturating_add(
                            self.usage_by_request
                                .values()
                                .filter_map(
                                    bcode_session_models::SessionTokenUsage::metered_total_tokens,
                                )
                                .map(u64::from)
                                .fold(0_u64, u64::saturating_add),
                        );
                    self.snapshot.runtime.cost =
                        bcode_session_view_models::SessionCostSummary::rebuild(
                            self.usage_by_request.values(),
                        );
                } else {
                    if let Some(tokens) = usage.metered_total_tokens() {
                        self.unattributed_metered_tokens = self
                            .unattributed_metered_tokens
                            .saturating_add(u64::from(tokens));
                        self.snapshot.runtime.cumulative_metered_tokens = self
                            .snapshot
                            .runtime
                            .cumulative_metered_tokens
                            .saturating_add(u64::from(tokens));
                    }
                    self.snapshot.runtime.cost.observe(usage);
                }
                if accepted {
                    self.snapshot.runtime.latest_usage = Some(usage.clone());
                }
                self.bump_revision();
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
                if self
                    .terminal_interactions
                    .contains_key(&request.exchange_id)
                {
                    return;
                }
                self.snapshot.active_exchanges.insert(
                    format!("{}:{}", request.invocation_id, request.exchange_id),
                    request.clone(),
                );
                self.upsert_interaction_item(
                    interaction_from_exchange_request(request),
                    event.sequence,
                    Some(event.timestamp_ms),
                );
            }
            SessionEventKind::ToolExchangeResolved { event: resolution } => {
                self.snapshot.active_exchanges.remove(&format!(
                    "{}:{}",
                    resolution.invocation_id, resolution.exchange_id
                ));
                if self
                    .terminal_interactions
                    .contains_key(&resolution.exchange_id)
                {
                    return;
                }
                self.terminal_interactions.insert(
                    resolution.exchange_id.clone(),
                    resolution.resolution.clone(),
                );
                self.resolve_interaction_item(
                    &resolution.exchange_id,
                    &resolution.resolution,
                    event.sequence,
                    Some(event.timestamp_ms),
                );
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
                let presentation_only = metadata
                    .get("presentation_only")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or_default();
                if !presentation_only {
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
                }
                let format = metadata.get("format").and_then(serde_json::Value::as_str);
                let mut message = match format {
                    Some("markdown") => ChatMessageView::markdown(text.clone()),
                    Some("json") => ChatMessageView {
                        text: text.clone(),
                        display_label: None,
                        format: bcode_session_view_models::TextFormat::Json,
                    },
                    _ => ChatMessageView::plain(text.clone()),
                };
                message.display_label = Some(plugin_id.clone());
                let item_id = if presentation_only {
                    TranscriptViewItemId::event(event.sequence)
                } else {
                    TranscriptViewItemId::new(format!("plugin-status:{plugin_id}:{note_id}"))
                };
                self.upsert_item(
                    item_id,
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
            SessionEventKind::RuntimeWorkFinished { work_id, .. } => {
                if !self.terminal_runtime_work.insert(work_id.clone()) {
                    return;
                }
                self.finish_runtime_work(work_id);
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
    #[allow(clippy::too_many_lines)] // Exhaustive live-event routing remains explicit at the projection boundary.
    pub fn apply_live_event(&mut self, event: &SessionLiveEvent) {
        self.snapshot.session_id = Some(event.session_id);
        match &event.kind {
            SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position,
                turn_id,
                segment_id,
                update,
                ..
            } => {
                let id = TranscriptViewItemId::new(format!(
                    "assistant-turn:{turn_id}:segment:{segment_id}"
                ));
                self.apply_ordered_assistant_update(id.clone(), turn_id, update);
                self.assign_output_location(&id, turn_id, *output_position);
            }
            SessionLiveEventKind::AssistantTextDelta {
                turn_id,
                segment_id,
                text,
                ..
            } => {
                let id = TranscriptViewItemId::new(format!(
                    "assistant-turn:{turn_id}:segment:{segment_id}"
                ));
                let mut target = self.accepted_presentation_target(&id);
                target.push_str(text);
                self.present_transcript_target(
                    id,
                    target,
                    turn_id,
                    StreamingMessageKind::Assistant,
                    Instant::now(),
                );
            }
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                output_position,
                turn_id,
                activity_id,
                activity_order,
                part_id,
                kind,
                role,
                part_order,
                update,
            } => {
                self.apply_ordered_reasoning_update(
                    turn_id,
                    activity_id,
                    *activity_order,
                    part_id,
                    *kind,
                    *role,
                    *part_order,
                    update,
                );
                self.assign_output_location(
                    &TranscriptViewItemId::reasoning(turn_id, activity_id),
                    turn_id,
                    *output_position,
                );
            }
            SessionLiveEventKind::AssistantReasoningDelta { turn_id, text } => {
                self.snapshot.thinking = ThinkingViewState {
                    visible: self.snapshot.thinking.visible,
                    mode: self.snapshot.thinking.mode,
                    active_text: Some(text.clone()),
                    streaming: true,
                };
                let id = TranscriptViewItemId::new(format!("reasoning-turn:{turn_id}"));
                let mut target = self.accepted_presentation_target(&id);
                target.push_str(text);
                self.present_transcript_target(
                    id,
                    target,
                    turn_id,
                    StreamingMessageKind::Reasoning,
                    Instant::now(),
                );
            }
            SessionLiveEventKind::AssistantReasoningActivity {
                output_position,
                turn_id,
                event,
            } => {
                self.apply_live_reasoning_activity(turn_id, event);
                self.assign_output_location(
                    &TranscriptViewItemId::reasoning(turn_id, event.activity_id()),
                    turn_id,
                    *output_position,
                );
            }
            SessionLiveEventKind::ToolContributionPlaced { envelope } => {
                self.apply_contribution_event(0, None, &envelope.contribution, envelope.placement);
            }
            SessionLiveEventKind::ToolPresentationUpdated { update } => {
                self.apply_presentation_update(update);
            }
            SessionLiveEventKind::ToolRequestDraft { event } => {
                self.apply_tool_request_draft(event);
                let id = self
                    .tool_item_ids
                    .get(&event.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| TranscriptViewItemId::tool(&event.tool_call_id));
                self.assign_output_location(&id, &event.turn_id, event.output_position);
            }
            SessionLiveEventKind::ToolInvocationProgress { event } => {
                if event.stage == bcode_session_models::ToolInvocationLifecycleStage::Progress
                    && !self
                        .tool_invocations
                        .get(&event.invocation_id)
                        .is_some_and(ToolInvocationAggregate::is_terminal)
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
    fn apply_presentation_update(&mut self, update: &bcode_tool::ToolPresentationUpdate) {
        if self
            .tool_invocations
            .get(&update.invocation_id)
            .is_some_and(ToolInvocationAggregate::is_terminal)
        {
            return;
        }
        let aggregate = self
            .tool_invocations
            .entry(update.invocation_id.clone())
            .or_default();
        if aggregate
            .presentation_scope
            .accept(update, usize::MAX)
            .is_err()
        {
            return;
        }
        aggregate
            .presentations
            .insert(update.identity.clone(), update.clone());
        if matches!(
            update.identity,
            bcode_tool::ToolPresentationIdentity::Primary
        ) {
            let presentation = self.current_primary_presentation(&update.invocation_id);
            if let Some(tool) = self.snapshot.tools.get_mut(&update.invocation_id) {
                tool.presentation.clone_from(&presentation);
            }
            if let Some(id) = self.tool_item_ids.get(&update.invocation_id).cloned()
                && let Some(index) = self
                    .snapshot
                    .transcript
                    .items
                    .iter()
                    .position(|item| item.id == id)
                && let Some(tool) = self.snapshot.tools.get(&update.invocation_id).cloned()
            {
                self.replace_tool_item(index, tool, &update.invocation_id, 0, None);
                return;
            }
            if let Some(projection) = self
                .tool_invocations
                .get(&update.invocation_id)
                .map(|aggregate| aggregate.projection.clone())
                .filter(|projection| projection.tool_name.is_some())
            {
                let mut tool = tool_invocation_view_from_projection(projection);
                tool.presentation = presentation;
                self.snapshot
                    .tools
                    .insert(update.invocation_id.clone(), tool);
                self.upsert_tool_item(&update.invocation_id, 0, None);
                return;
            }
        } else if let bcode_tool::ToolPresentationIdentity::Supplemental { item_id } =
            &update.identity
        {
            let id = TranscriptViewItemId::tool_supplemental(&update.invocation_id, item_id);
            self.upsert_item(
                id,
                0,
                None,
                true,
                TranscriptViewItemKind::ToolContribution {
                    invocation: self
                        .snapshot
                        .tools
                        .get(&update.invocation_id)
                        .cloned()
                        .map(Box::new),
                    contribution: presentation_update_contribution(update),
                    placement: bcode_session_models::ToolContributionPlacement::Supplemental,
                },
            );
            return;
        }
        self.bump_revision();
    }

    #[allow(clippy::too_many_lines)] // Draft generation, revision, and UTF-8 offsets form one atomic reducer transition.
    fn apply_tool_request_draft(&mut self, event: &bcode_session_models::ToolRequestDraftEvent) {
        use bcode_session_models::ToolRequestDraftOperation;

        let key = event.tool_call_id.clone();
        let previous_placement = self
            .tool_invocations
            .get(&key)
            .and_then(|aggregate| aggregate.request_draft.as_ref())
            .map(|draft| draft.placement);
        let current = self
            .tool_invocations
            .get(&key)
            .and_then(|aggregate| aggregate.request_draft.clone());
        let terminal_update = matches!(event.operation, ToolRequestDraftOperation::Remove { .. });
        if self
            .tool_invocations
            .get(&key)
            .and_then(|aggregate| aggregate.terminal_request_draft)
            .is_some_and(|(generation, revision)| {
                event.generation < generation
                    || (event.generation == generation && event.revision <= revision)
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
            self.tool_invocations
                .entry(key.clone())
                .or_default()
                .request_draft = None;
        }
        if let ToolRequestDraftOperation::Remove { reason } = event.operation {
            let aggregate = self.tool_invocations.entry(key).or_default();
            if reason != bcode_session_models::ToolRequestDraftTerminalReason::Completed {
                aggregate.request_draft = None;
            }
            aggregate.terminal_request_draft =
                Some((event.generation, event.revision.saturating_add(1)));
            if reason == bcode_session_models::ToolRequestDraftTerminalReason::Completed {
                self.refresh_tool_from_aggregate(&event.tool_call_id, 0, None);
            } else if aggregate.projection.started_at_ms.is_none() {
                self.snapshot.tools.remove(&event.tool_call_id);
                self.remove_transcript_item(&TranscriptViewItemId::tool(&event.tool_call_id));
            } else {
                self.refresh_tool_from_aggregate(&event.tool_call_id, 0, None);
            }
            return;
        }
        self.tool_invocations
            .entry(key.clone())
            .or_default()
            .terminal_request_draft = None;
        let mut draft = current
            .filter(|current| current.generation == event.generation)
            .unwrap_or_else(|| ToolRequestDraftView {
                output_location: event.output_position.map(|position| TurnOutputLocation {
                    turn_id: event.turn_id.clone(),
                    position,
                }),
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
        if let Some(position) = event.output_position {
            draft.output_location = Some(TurnOutputLocation {
                turn_id: event.turn_id.clone(),
                position,
            });
        }
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
        let aggregate = self.tool_invocations.entry(key).or_default();
        aggregate
            .projection
            .tool_call_id
            .clone_from(&event.tool_call_id);
        aggregate
            .projection
            .producer_plugin_id
            .clone_from(&event.producer_plugin_id);
        aggregate.projection.tool_name = Some(event.tool_name.clone());
        aggregate.projection.arguments_json = Some(draft.preview.clone());
        aggregate.request_draft = Some(draft);
        if previous_placement.is_some_and(|placement| placement != event.placement) {
            let previous_id = legacy_contribution_projection::item_id(
                &event.tool_call_id,
                "request-draft",
                previous_placement.expect("checked draft placement"),
            );
            self.remove_transcript_item(&previous_id);
        }
        self.refresh_tool_from_aggregate(&event.tool_call_id, 0, None);
        self.refresh_legacy_contribution_projection(
            &event.tool_call_id,
            event.placement,
            None,
            0,
            None,
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

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn apply_ordered_reasoning_update(
        &mut self,
        turn_id: &str,
        activity_id: &str,
        activity_order: u32,
        part_id: &str,
        kind: bcode_session_models::ReasoningContentKind,
        role: bcode_session_models::ReasoningContentRole,
        part_order: u32,
        update: &bcode_session_models::TextStreamUpdate,
    ) {
        use bcode_session_models::TextStreamOperation;

        let stream_id = TranscriptViewItemId::new(format!(
            "reasoning-stream:{turn_id}:activity:{activity_id}:part:{part_id}"
        ));
        let existing = self.snapshot.text_streams.get(&stream_id).cloned();
        if existing.as_ref().is_some_and(|state| {
            matches!(state.status, TextStreamViewStatus::Terminal(_))
                || update.generation < state.generation
        }) {
            return;
        }
        if existing
            .as_ref()
            .is_some_and(|state| update.generation > state.generation)
        {
            self.snapshot.text_streams.remove(&stream_id);
            self.last_text_stream_updates.remove(&stream_id);
            self.pending_text_presentations.remove(&stream_id);
            if let Some(activity) = self
                .live_reasoning
                .get_mut(&(turn_id.to_owned(), activity_id.to_owned()))
            {
                activity.parts.remove(part_id);
            }
        }
        let state = self
            .snapshot
            .text_streams
            .entry(stream_id.clone())
            .or_insert(TextStreamViewState {
                generation: update.generation,
                revision: 0,
                accepted_bytes: 0,
                truncated: false,
                status: TextStreamViewStatus::Healthy,
            });
        if update.revision < state.revision {
            return;
        }
        if update.revision == state.revision {
            if self.last_text_stream_updates.get(&stream_id) != Some(update) {
                state.status = TextStreamViewStatus::Degraded;
                self.bump_revision();
            }
            return;
        }
        let is_checkpoint = matches!(update.operation, TextStreamOperation::Checkpoint { .. });
        if !is_checkpoint && update.first_revision != state.revision.saturating_add(1) {
            state.status = TextStreamViewStatus::Degraded;
            self.bump_revision();
            return;
        }

        let activity_key = (turn_id.to_owned(), activity_id.to_owned());
        let current_text = self.pending_text_presentations.get(&stream_id).map_or_else(
            || {
                self.live_reasoning
                    .get(&activity_key)
                    .and_then(|activity| activity.parts.get(part_id))
                    .map_or_else(String::new, |part| part.text.clone())
            },
            |pending| pending.target.clone(),
        );
        let next_text = match &update.operation {
            TextStreamOperation::Append {
                expected_offset,
                text,
            } => {
                if *expected_offset != state.accepted_bytes {
                    state.status = TextStreamViewStatus::Degraded;
                    self.bump_revision();
                    return;
                }
                let mut next = current_text;
                next.push_str(text);
                state.generation = update.generation;
                state.revision = update.revision;
                state.accepted_bytes = state.accepted_bytes.saturating_add(text.len());
                state.status = TextStreamViewStatus::Healthy;
                Some(next)
            }
            TextStreamOperation::Checkpoint {
                start_offset,
                text,
                total_bytes,
                truncated,
            } => {
                if start_offset.saturating_add(text.len()) > *total_bytes {
                    state.status = TextStreamViewStatus::Degraded;
                    self.bump_revision();
                    return;
                }
                state.generation = update.generation;
                state.revision = update.revision;
                state.accepted_bytes = *total_bytes;
                state.truncated = *truncated || *start_offset != 0;
                state.status = if state.truncated {
                    TextStreamViewStatus::Incomplete
                } else {
                    TextStreamViewStatus::Healthy
                };
                Some(text.clone())
            }
            TextStreamOperation::Terminal { status } => {
                state.generation = update.generation;
                state.revision = update.revision;
                state.status = TextStreamViewStatus::Terminal(*status);
                None
            }
        };
        self.last_text_stream_updates
            .insert(stream_id.clone(), update.clone());
        let destination = PendingTextDestination::ReasoningPart {
            turn_id: turn_id.to_owned(),
            activity_id: activity_id.to_owned(),
            activity_order,
            part_id: part_id.to_owned(),
            kind,
            role,
            part_order,
        };
        match (&update.operation, next_text) {
            (TextStreamOperation::Append { .. }, Some(text)) => {
                self.present_reasoning_target(stream_id, text, destination, Instant::now());
            }
            (TextStreamOperation::Checkpoint { .. }, Some(text)) => {
                self.pending_text_presentations.remove(&stream_id);
                let _ = self.replace_presented_text(&stream_id, &text, &destination);
            }
            (TextStreamOperation::Terminal { .. }, None) => {
                if let Some(pending) = self.pending_text_presentations.remove(&stream_id) {
                    let _ = self.replace_presented_text(
                        &stream_id,
                        &pending.target,
                        &pending.destination,
                    );
                }
                self.bump_revision();
            }
            _ => {}
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
        self.refresh_live_reasoning_activity(&key);
    }

    fn refresh_live_reasoning_activity(&mut self, key: &(String, String)) {
        let activity = self
            .live_reasoning
            .get(key)
            .expect("live reasoning activity was inserted");
        let item_id = TranscriptViewItemId::reasoning(&key.0, &key.1);
        let view = live_reasoning_activity_view(
            &key.0,
            &key.1,
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

    #[allow(clippy::too_many_lines)] // Compatibility inputs collapse into one canonical primary item here.
    fn refresh_legacy_contribution_projection(
        &mut self,
        invocation_id: &str,
        placement: bcode_session_models::ToolContributionPlacement,
        supplemental_id: Option<&str>,
        sequence: u64,
        timestamp_ms: Option<u64>,
    ) {
        use bcode_session_models::ToolContributionPlacement;

        if placement == ToolContributionPlacement::Hidden {
            return;
        }
        let id = if placement == ToolContributionPlacement::Supplemental {
            TranscriptViewItemId::tool_supplemental(
                invocation_id,
                supplemental_id.expect("supplemental contributions require stable identity"),
            )
        } else {
            TranscriptViewItemId::tool(invocation_id)
        };
        let latest_contribution = |candidate_placement| {
            self.snapshot
                .contributions
                .iter()
                .filter(|(key, contribution)| {
                    contribution.invocation_id == invocation_id
                        && self.contribution_placements.get(*key) == Some(&candidate_placement)
                        && (candidate_placement != ToolContributionPlacement::Supplemental
                            || supplemental_id == Some(contribution.contribution_id.as_str()))
                })
                .max_by(|(left_key, left), (right_key, right)| {
                    (left.sequence, left_key.as_str()).cmp(&(right.sequence, right_key.as_str()))
                })
                .map(|(_, contribution)| contribution.clone())
        };
        let tool = self.snapshot.tools.get(invocation_id).cloned();
        let kind = if placement == ToolContributionPlacement::Supplemental {
            latest_contribution(ToolContributionPlacement::Supplemental).map(|contribution| {
                TranscriptViewItemKind::ToolContribution {
                    invocation: tool.clone().map(Box::new),
                    contribution,
                    placement: ToolContributionPlacement::Supplemental,
                }
            })
        } else {
            let contribution = [
                ToolContributionPlacement::Result,
                ToolContributionPlacement::Progress,
                ToolContributionPlacement::Request,
            ]
            .into_iter()
            .find_map(|candidate_placement| {
                latest_contribution(candidate_placement).map(|contribution| {
                    TranscriptViewItemKind::ToolContribution {
                        invocation: tool.clone().map(Box::new),
                        contribution,
                        placement: candidate_placement,
                    }
                })
            });
            contribution.or_else(|| {
                tool.map(|tool| TranscriptViewItemKind::ToolInvocation {
                    tool: Box::new(tool),
                })
            })
        };
        let Some(kind) = kind else {
            let item_count = self.snapshot.transcript.items.len();
            self.snapshot.transcript.items.retain(|item| item.id != id);
            if self.snapshot.transcript.items.len() != item_count {
                self.snapshot.transcript.revision =
                    self.snapshot.transcript.revision.saturating_add(1);
                self.bump_revision();
            }
            return;
        };
        let streaming = match &kind {
            TranscriptViewItemKind::ToolRequestDraft { .. } => true,
            TranscriptViewItemKind::ToolContribution { contribution, .. } => {
                contribution.persistence
                    == bcode_session_models::ToolContributionPersistence::Transient
                    || self.snapshot.tools.get(invocation_id).is_some_and(|tool| {
                        matches!(
                            tool.status,
                            ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
                        )
                    })
            }
            TranscriptViewItemKind::ToolInvocation { tool } => {
                tool.request_draft.is_some()
                    || matches!(
                        tool.status,
                        ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
                    )
            }
            _ => false,
        };
        self.upsert_item(id, sequence, timestamp_ms, streaming, kind);
    }

    fn apply_contribution_event(
        &mut self,
        event_sequence: u64,
        timestamp_ms: Option<u64>,
        contribution: &bcode_session_models::ToolContributionEvent,
        placement: bcode_session_models::ToolContributionPlacement,
    ) {
        if self
            .tool_invocations
            .get(&contribution.invocation_id)
            .is_some_and(ToolInvocationAggregate::is_terminal)
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
        let previous_item_id = previous_placement.map(|previous| {
            legacy_contribution_projection::item_id(
                &contribution.invocation_id,
                &contribution.contribution_id,
                previous,
            )
        });
        let item_id = legacy_contribution_projection::item_id(
            &contribution.invocation_id,
            &contribution.contribution_id,
            placement,
        );
        match contribution.operation {
            bcode_session_models::ToolContributionOperation::Remove => {
                self.snapshot.contributions.remove(&key);
                self.contribution_placements.remove(&key);
                if let Some(previous_item_id) = previous_item_id.as_ref() {
                    self.remove_owned_legacy_contribution_item(previous_item_id, &key);
                }
                if previous_item_id.as_ref() != Some(&item_id) {
                    self.remove_owned_legacy_contribution_item(&item_id, &key);
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
                    self.remove_legacy_contribution_item(previous_item_id);
                }
                if placement == bcode_session_models::ToolContributionPlacement::Hidden {
                    self.remove_legacy_contribution_item(&item_id);
                } else if placement == bcode_session_models::ToolContributionPlacement::Result
                    && self
                        .snapshot
                        .tools
                        .get(&contribution.invocation_id)
                        .is_some_and(|tool| is_terminal_tool_status(tool.status))
                {
                    self.remove_owned_legacy_contribution_item(&item_id, &key);
                } else {
                    self.upsert_legacy_contribution_item(
                        item_id,
                        &key,
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

    fn remove_owned_legacy_contribution_item(
        &mut self,
        id: &TranscriptViewItemId,
        owner_key: &str,
    ) {
        let Some(index) = self.snapshot.transcript.items.iter().position(|item| {
            item.id == *id
                && matches!(
                    &item.kind,
                    TranscriptViewItemKind::ToolContribution { contribution, .. }
                        if format!("{}:{}", contribution.invocation_id, contribution.contribution_id)
                            == owner_key
                )
        }) else {
            return;
        };
        self.remove_transcript_item_at(index);
    }

    fn remove_transcript_item_at(&mut self, index: usize) {
        self.snapshot.transcript.items.remove(index);
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
    }

    fn remove_legacy_contribution_item(&mut self, id: &TranscriptViewItemId) {
        let Some(index) = self
            .snapshot
            .transcript
            .items
            .iter()
            .position(|item| item.id == *id)
        else {
            return;
        };
        self.remove_transcript_item_at(index);
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_legacy_contribution_item(
        &mut self,
        id: TranscriptViewItemId,
        owner_key: &str,
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
            matches!(
                invocation.status,
                ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
            )
        }) || streaming;
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| item.id == id)
        {
            if let TranscriptViewItemKind::ToolContribution {
                contribution: previous,
                ..
            } = &item.kind
            {
                let previous_owner =
                    format!("{}:{}", previous.invocation_id, previous.contribution_id);
                if previous_owner != owner_key {
                    self.contribution_placements.remove(&previous_owner);
                }
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

    fn assign_output_location(
        &mut self,
        id: &TranscriptViewItemId,
        turn_id: &str,
        position: Option<bcode_session_models::TurnOutputPosition>,
    ) {
        let Some(position) = position else {
            return;
        };
        let Some(index) = self
            .snapshot
            .transcript
            .items
            .iter()
            .position(|item| &item.id == id)
        else {
            return;
        };
        let location = TurnOutputLocation {
            turn_id: turn_id.to_owned(),
            position,
        };
        let location_changed = self.snapshot.transcript.items[index]
            .output_location
            .as_ref()
            != Some(&location);
        if location_changed {
            self.snapshot.transcript.items[index].output_location = Some(location.clone());
            self.snapshot.transcript.items[index].revision = self.snapshot.transcript.items[index]
                .revision
                .saturating_add(1);
        }

        let mut ordered_index = index;
        while ordered_index > 0
            && output_location_precedes(
                Some(&location),
                self.snapshot.transcript.items[ordered_index - 1]
                    .output_location
                    .as_ref(),
            )
        {
            self.snapshot
                .transcript
                .items
                .swap(ordered_index - 1, ordered_index);
            ordered_index -= 1;
        }
        while ordered_index + 1 < self.snapshot.transcript.items.len()
            && output_location_precedes(
                self.snapshot.transcript.items[ordered_index + 1]
                    .output_location
                    .as_ref(),
                Some(&location),
            )
        {
            self.snapshot
                .transcript
                .items
                .swap(ordered_index, ordered_index + 1);
            ordered_index += 1;
        }

        if location_changed || ordered_index != index {
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
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
            output_location: None,
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

    fn close_presentation_scope(&mut self, invocation_id: &str) {
        let Some(aggregate) = self.tool_invocations.get_mut(invocation_id) else {
            return;
        };
        aggregate.presentation_scope.close();
        let active_supplementals = aggregate
            .presentations
            .iter()
            .filter_map(|(identity, update)| {
                (update.retention == bcode_tool::ToolPresentationRetention::ActiveOnly)
                    .then_some(identity)
            })
            .filter_map(|identity| match identity {
                bcode_tool::ToolPresentationIdentity::Primary => None,
                bcode_tool::ToolPresentationIdentity::Supplemental { item_id } => Some(
                    TranscriptViewItemId::tool_supplemental(invocation_id, item_id),
                ),
            })
            .collect::<BTreeSet<_>>();
        aggregate.presentations.retain(|_, update| {
            update.retention == bcode_tool::ToolPresentationRetention::RetainLatest
        });
        if active_supplementals.is_empty() {
            return;
        }
        let item_count = self.snapshot.transcript.items.len();
        self.snapshot
            .transcript
            .items
            .retain(|item| !active_supplementals.contains(&item.id));
        if self.snapshot.transcript.items.len() != item_count {
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        }
    }

    fn current_primary_presentation(
        &self,
        invocation_id: &str,
    ) -> Option<bcode_session_view_models::ToolPresentationView> {
        self.tool_invocations
            .get(invocation_id)
            .and_then(|aggregate| {
                aggregate
                    .presentations
                    .get(&bcode_tool::ToolPresentationIdentity::Primary)
            })
            .map(Into::into)
    }

    fn refresh_tool_from_aggregate(
        &mut self,
        tool_call_id: &str,
        sequence: u64,
        timestamp_ms: Option<u64>,
    ) {
        let Some(aggregate) = self.tool_invocations.get(tool_call_id) else {
            return;
        };
        let mut tool = tool_invocation_view_from_projection(aggregate.projection.clone());
        tool.request_draft.clone_from(&aggregate.request_draft);
        self.attach_primary_presentation(&mut tool);
        if let Some(status) = aggregate.terminal_status {
            tool.status = status;
        }
        self.snapshot
            .tools
            .insert(tool_call_id.to_owned(), tool.clone());
        self.refresh_canonical_tool_item(tool_call_id, sequence, timestamp_ms, &tool);
    }

    fn attach_request_draft(&self, tool: &mut ToolInvocationView) {
        tool.request_draft = self
            .tool_invocations
            .get(&tool.tool_call_id)
            .and_then(|aggregate| aggregate.request_draft.clone());
    }

    fn attach_primary_presentation(&self, tool: &mut ToolInvocationView) {
        if let Some(presentation) = self.current_primary_presentation(&tool.tool_call_id) {
            tool.presentation = Some(presentation);
        }
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
        let canonical_id = TranscriptViewItemId::tool(tool_call_id);
        self.tool_item_ids
            .insert(tool_call_id.to_owned(), canonical_id.clone());
        if self
            .snapshot
            .transcript
            .items
            .iter()
            .any(|item| item.id == canonical_id)
        {
            self.update_existing_tool_item(
                &canonical_id,
                tool_call_id,
                sequence,
                timestamp_ms,
                tool,
            );
        } else {
            self.upsert_terminal_tool_item(tool_call_id, sequence, timestamp_ms, tool);
        }
        self.sync_contribution_invocation_context(tool_call_id);
    }

    fn refresh_canonical_tool_item(
        &mut self,
        tool_call_id: &str,
        sequence: u64,
        timestamp_ms: Option<u64>,
        tool: &ToolInvocationView,
    ) {
        let id = TranscriptViewItemId::tool(tool_call_id);
        self.upsert_item(
            id.clone(),
            sequence,
            timestamp_ms,
            tool.request_draft.is_some()
                || matches!(
                    tool.status,
                    ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
                ),
            TranscriptViewItemKind::ToolInvocation {
                tool: Box::new(tool.clone()),
            },
        );
        self.tool_item_ids.insert(tool_call_id.to_owned(), id);
    }

    fn upsert_tool_item(&mut self, tool_call_id: &str, sequence: u64, timestamp_ms: Option<u64>) {
        let Some(aggregate) = self.tool_invocations.get(tool_call_id) else {
            return;
        };
        let projection = aggregate.projection.clone();
        let terminal_status = aggregate.terminal_status;
        let mut tool = tool_invocation_view_from_projection(projection);
        self.attach_request_draft(&mut tool);
        self.attach_primary_presentation(&mut tool);
        if let Some(status) = terminal_status {
            tool.status = status;
            if matches!(status, ToolInvocationViewStatus::Failed) {
                tool.is_error = Some(true);
            }
        }
        let already_terminal = self
            .snapshot
            .tools
            .get(tool_call_id)
            .is_some_and(|tool| is_terminal_tool_status(tool.status));
        self.snapshot
            .tools
            .insert(tool_call_id.to_owned(), tool.clone());
        let canonical_id = TranscriptViewItemId::tool(tool_call_id);
        if is_terminal_tool_status(tool.status) {
            if already_terminal
                && !self
                    .snapshot
                    .transcript
                    .items
                    .iter()
                    .any(|item| item.id == canonical_id)
            {
                return;
            }
            self.upsert_terminal_tool_item(tool_call_id, sequence, timestamp_ms, tool);
            return;
        }
        self.refresh_canonical_tool_item(tool_call_id, sequence, timestamp_ms, &tool);
        self.sync_contribution_invocation_context(tool_call_id);
    }

    fn upsert_terminal_tool_item(
        &mut self,
        tool_call_id: &str,
        sequence: u64,
        timestamp_ms: Option<u64>,
        tool: ToolInvocationView,
    ) {
        let result_id = TranscriptViewItemId::tool(tool_call_id);
        if let Some(index) = self
            .snapshot
            .transcript
            .items
            .iter()
            .position(|item| item.id == result_id)
        {
            if let TranscriptViewItemKind::ToolContribution { invocation, .. } =
                &mut self.snapshot.transcript.items[index].kind
            {
                *invocation = Some(Box::new(tool));
                self.snapshot.transcript.items[index].streaming = false;
                self.snapshot.transcript.items[index].revision = self.snapshot.transcript.items
                    [index]
                    .revision
                    .saturating_add(1);
                self.snapshot.transcript.revision =
                    self.snapshot.transcript.revision.saturating_add(1);
                self.bump_revision();
            } else {
                self.replace_tool_item(index, tool, tool_call_id, sequence, timestamp_ms);
            }
            return;
        }
        if let Some(id) = self.tool_item_ids.get(tool_call_id).cloned() {
            self.update_existing_tool_item(&id, tool_call_id, sequence, timestamp_ms, tool);
        } else {
            let id = TranscriptViewItemId::tool(tool_call_id);
            let id = self.push_item(
                id,
                sequence,
                timestamp_ms,
                false,
                TranscriptViewItemKind::ToolInvocation {
                    tool: Box::new(tool),
                },
            );
            self.tool_item_ids.insert(tool_call_id.to_owned(), id);
        }
        self.sync_contribution_invocation_context(tool_call_id);
    }

    fn sync_contribution_invocation_context(&mut self, tool_call_id: &str) {
        let invocation = self.snapshot.tools.get(tool_call_id).cloned().map(Box::new);
        let streaming = invocation.as_ref().is_some_and(|invocation| {
            matches!(
                invocation.status,
                ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
            )
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
        id: &TranscriptViewItemId,
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
            .position(|item| &item.id == id)
        else {
            return;
        };
        if is_terminal_tool_status(tool.status)
            && matches!(
                self.snapshot.transcript.items[index].kind,
                TranscriptViewItemKind::ToolContribution { .. }
            )
        {
            if let TranscriptViewItemKind::ToolContribution {
                invocation,
                contribution: _,
                placement: _,
            } = &mut self.snapshot.transcript.items[index].kind
            {
                *invocation = Some(Box::new(tool));
            }
            self.snapshot.transcript.items[index].sequence = (sequence != 0)
                .then_some(sequence)
                .or(self.snapshot.transcript.items[index].sequence);
            self.snapshot.transcript.items[index].timestamp_ms =
                timestamp_ms.or(self.snapshot.transcript.items[index].timestamp_ms);
            self.snapshot.transcript.items[index].streaming = false;
            self.snapshot.transcript.items[index].revision = self.snapshot.transcript.items[index]
                .revision
                .saturating_add(1);
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
            return;
        }
        self.replace_tool_item(index, tool, tool_call_id, sequence, timestamp_ms);
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
            ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
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
            if *existing == work {
                return;
            }
            *existing = work;
        } else {
            self.snapshot.runtime_work.push(work);
        }
        self.bump_revision();
    }

    fn finish_runtime_work(&mut self, work_id: &bcode_session_models::WorkId) {
        let before = self.snapshot.runtime_work.len();
        self.snapshot
            .runtime_work
            .retain(|active| active.work_id != *work_id);
        if self.snapshot.runtime_work.len() != before {
            self.bump_revision();
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
            if existing.resolved && !interaction.resolved {
                return;
            }
            if existing.resolved && interaction.resolved {
                return;
            }
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

    fn resolve_interaction_item(
        &mut self,
        interaction_id: &str,
        resolution: &bcode_session_models::ToolExchangeResolution,
        sequence: u64,
        timestamp_ms: Option<u64>,
    ) {
        let Some(index) = self
            .snapshot
            .interactions
            .iter()
            .position(|interaction| interaction.interaction_id == interaction_id)
        else {
            // A bounded history window can begin after the request. Retain a generic terminal row
            // rather than guessing producer-owned request semantics that are no longer resident.
            self.upsert_interaction_item(
                terminal_interaction_without_request(interaction_id, resolution),
                sequence,
                timestamp_ms,
            );
            return;
        };
        if self.snapshot.interactions[index].resolved {
            return;
        }
        let interaction = &mut self.snapshot.interactions[index];
        interaction.state = interaction_state_for_resolution(resolution);
        interaction.status_detail = interaction_status_detail(resolution);
        interaction.resolved = true;
        interaction.resolution = Some(resolution.clone());
        let interaction = interaction.clone();
        let Some(id) = self.interaction_item_ids.get(interaction_id).cloned() else {
            self.upsert_interaction_item(interaction, sequence, timestamp_ms);
            return;
        };
        let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| item.id == id)
        else {
            self.upsert_interaction_item(interaction, sequence, timestamp_ms);
            return;
        };
        item.kind = TranscriptViewItemKind::Interaction { interaction };
        item.sequence = (sequence != 0).then_some(sequence).or(item.sequence);
        item.timestamp_ms = timestamp_ms.or(item.timestamp_ms);
        item.revision = item.revision.saturating_add(1);
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        self.bump_revision();
    }

    #[allow(clippy::too_many_lines)] // One reducer keeps generation/revision/offset transitions atomic.
    fn apply_ordered_assistant_update(
        &mut self,
        id: TranscriptViewItemId,
        turn_id: &str,
        update: &bcode_session_models::TextStreamUpdate,
    ) {
        use bcode_session_models::TextStreamOperation;

        let existing = self.snapshot.text_streams.get(&id).cloned();
        if existing.as_ref().is_some_and(|state| {
            matches!(state.status, TextStreamViewStatus::Terminal(_))
                || update.generation < state.generation
        }) {
            return;
        }
        if existing
            .as_ref()
            .is_some_and(|state| update.generation > state.generation)
        {
            self.remove_transcript_item(&id);
            self.snapshot.text_streams.remove(&id);
            self.pending_text_presentations.remove(&id);
        }
        let mut accepted_target = self.accepted_presentation_target(&id);
        let state = self
            .snapshot
            .text_streams
            .entry(id.clone())
            .or_insert(TextStreamViewState {
                generation: update.generation,
                revision: 0,
                accepted_bytes: 0,
                truncated: false,
                status: TextStreamViewStatus::Healthy,
            });
        if update.revision < state.revision {
            return;
        }
        if update.revision == state.revision {
            if self.last_text_stream_updates.get(&id) != Some(update) {
                state.status = TextStreamViewStatus::Degraded;
                self.bump_revision();
            }
            return;
        }
        let is_checkpoint = matches!(update.operation, TextStreamOperation::Checkpoint { .. });
        if !is_checkpoint && update.first_revision != state.revision.saturating_add(1) {
            state.status = TextStreamViewStatus::Degraded;
            self.bump_revision();
            return;
        }

        match &update.operation {
            TextStreamOperation::Append {
                expected_offset,
                text,
            } => {
                if !matches!(
                    state.status,
                    TextStreamViewStatus::Healthy | TextStreamViewStatus::Incomplete
                ) || state.accepted_bytes != *expected_offset
                {
                    state.status = TextStreamViewStatus::Degraded;
                    self.bump_revision();
                    return;
                }
                state.revision = update.revision;
                state.accepted_bytes = state.accepted_bytes.saturating_add(text.len());
                self.last_text_stream_updates
                    .insert(id.clone(), update.clone());
                accepted_target.push_str(text);
                self.present_transcript_target(
                    id,
                    accepted_target,
                    turn_id,
                    StreamingMessageKind::Assistant,
                    Instant::now(),
                );
            }
            TextStreamOperation::Checkpoint {
                start_offset,
                text,
                total_bytes,
                truncated,
            } => {
                if start_offset.saturating_add(text.len()) > *total_bytes {
                    state.status = TextStreamViewStatus::Degraded;
                    self.bump_revision();
                    return;
                }
                state.generation = update.generation;
                state.revision = update.revision;
                state.accepted_bytes = *total_bytes;
                state.truncated = *truncated || *start_offset != 0;
                state.status = if state.truncated {
                    TextStreamViewStatus::Incomplete
                } else {
                    TextStreamViewStatus::Healthy
                };
                self.last_text_stream_updates
                    .insert(id.clone(), update.clone());
                self.pending_text_presentations.remove(&id);
                self.replace_streaming_message_by_id(id, text);
            }
            TextStreamOperation::Terminal { status } => {
                state.revision = update.revision;
                state.status = TextStreamViewStatus::Terminal(*status);
                self.last_text_stream_updates
                    .insert(id.clone(), update.clone());
                if let Some(item) = self
                    .snapshot
                    .transcript
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                {
                    item.streaming = false;
                    item.revision = item.revision.saturating_add(1);
                    self.snapshot.transcript.revision =
                        self.snapshot.transcript.revision.saturating_add(1);
                }
                self.bump_revision();
            }
        }
    }

    fn replace_streaming_message_by_id(&mut self, id: TranscriptViewItemId, text: &str) {
        if let Some(item) = self
            .snapshot
            .transcript
            .items
            .iter_mut()
            .find(|item| item.id == id)
        {
            let _ = replace_text_in_item(item, text);
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
            StreamingMessageKind::Assistant.item_kind(text.to_owned()),
        );
    }

    fn present_reasoning_target(
        &mut self,
        stream_id: TranscriptViewItemId,
        target: String,
        destination: PendingTextDestination,
        now: Instant,
    ) {
        if self.streaming_presentation_policy.is_immediate() {
            self.pending_text_presentations.remove(&stream_id);
            let _ = self.replace_presented_text(&stream_id, &target, &destination);
            return;
        }
        let current_bytes = match &destination {
            PendingTextDestination::ReasoningPart {
                turn_id,
                activity_id,
                part_id,
                ..
            } => self
                .live_reasoning
                .get(&(turn_id.clone(), activity_id.clone()))
                .and_then(|activity| activity.parts.get(part_id))
                .map_or(0, |part| part.text.len()),
            PendingTextDestination::Transcript { .. } => 0,
        };
        if target.len().saturating_sub(current_bytes) > MAX_RETAINED_PENDING_TEXT_BYTES {
            self.pending_text_presentations.remove(&stream_id);
            let _ = self.replace_presented_text(&stream_id, &target, &destination);
            return;
        }
        let max_lag = Duration::from_millis(self.streaming_presentation_policy.max_lag_ms);
        let rate = self.streaming_presentation_policy.graphemes_per_second;
        if current_bytes == 0 {
            let first_bytes = target
                .grapheme_indices(true)
                .nth(1)
                .map_or(target.len(), |(offset, _)| offset);
            let _ = self.replace_presented_text(&stream_id, &target[..first_bytes], &destination);
            if first_bytes < target.len() {
                self.pending_text_presentations.insert(
                    stream_id,
                    PendingTextPresentation::new(
                        target,
                        first_bytes,
                        now,
                        rate,
                        max_lag,
                        destination,
                    ),
                );
            }
            return;
        }
        self.pending_text_presentations
            .entry(stream_id)
            .and_modify(|pending| {
                pending.rebase(target.clone(), now, rate, max_lag, destination.clone());
            })
            .or_insert_with(|| {
                PendingTextPresentation::new(target, current_bytes, now, rate, max_lag, destination)
            });
    }

    fn accepted_presentation_target(&self, id: &TranscriptViewItemId) -> String {
        self.pending_text_presentations.get(id).map_or_else(
            || {
                self.snapshot
                    .transcript
                    .items
                    .iter()
                    .find(|item| item.id == *id)
                    .and_then(transcript_item_text)
                    .unwrap_or_default()
                    .to_owned()
            },
            |pending| pending.target.clone(),
        )
    }

    fn present_transcript_target(
        &mut self,
        id: TranscriptViewItemId,
        target: String,
        turn_id: &str,
        kind: StreamingMessageKind,
        now: Instant,
    ) {
        let destination = PendingTextDestination::Transcript {
            turn_id: turn_id.to_owned(),
        };
        if self.streaming_presentation_policy.is_immediate() {
            self.pending_text_presentations.remove(&id);
            if self.replace_presented_text(&id, &target, &destination) {
                return;
            }
            self.push_item(id, 0, None, true, kind.item_kind(target));
            return;
        }

        let current_bytes = self
            .snapshot
            .transcript
            .items
            .iter()
            .find(|item| item.id == id)
            .and_then(transcript_item_text)
            .map_or(0, str::len);
        if target.len().saturating_sub(current_bytes) > MAX_RETAINED_PENDING_TEXT_BYTES {
            self.pending_text_presentations.remove(&id);
            if !self.replace_presented_text(&id, &target, &destination) {
                self.push_item(id, 0, None, true, kind.item_kind(target));
            }
            return;
        }
        let max_lag = Duration::from_millis(self.streaming_presentation_policy.max_lag_ms);
        let rate = self.streaming_presentation_policy.graphemes_per_second;
        if current_bytes == 0 {
            let first_bytes = target
                .grapheme_indices(true)
                .nth(1)
                .map_or(target.len(), |(offset, _)| offset);
            let visible = target[..first_bytes].to_owned();
            self.push_item(id.clone(), 0, None, true, kind.item_kind(visible));
            if first_bytes < target.len() {
                self.pending_text_presentations.insert(
                    id,
                    PendingTextPresentation::new(
                        target,
                        first_bytes,
                        now,
                        rate,
                        max_lag,
                        destination,
                    ),
                );
            }
            return;
        }
        self.pending_text_presentations
            .entry(id)
            .and_modify(|pending| {
                pending.rebase(target.clone(), now, rate, max_lag, destination.clone());
            })
            .or_insert_with(|| {
                PendingTextPresentation::new(target, current_bytes, now, rate, max_lag, destination)
            });
    }

    fn replace_presented_text(
        &mut self,
        id: &TranscriptViewItemId,
        text: &str,
        destination: &PendingTextDestination,
    ) -> bool {
        match destination {
            PendingTextDestination::Transcript { .. } => {
                let Some(item) = self
                    .snapshot
                    .transcript
                    .items
                    .iter_mut()
                    .find(|item| item.id == *id)
                else {
                    return false;
                };
                if !replace_text_in_item(item, text) {
                    return false;
                }
                item.revision = item.revision.saturating_add(1);
                self.snapshot.transcript.revision =
                    self.snapshot.transcript.revision.saturating_add(1);
                self.bump_revision();
                true
            }
            PendingTextDestination::ReasoningPart {
                turn_id,
                activity_id,
                activity_order,
                part_id,
                kind,
                role,
                part_order,
            } => {
                self.apply_live_reasoning_activity(
                    turn_id,
                    &bcode_session_models::ReasoningActivityEvent::PartCompleted {
                        activity_id: activity_id.clone(),
                        activity_order: *activity_order,
                        part_id: part_id.clone(),
                        kind: *kind,
                        role: *role,
                        part_order: *part_order,
                        text: text.to_owned(),
                    },
                );
                true
            }
        }
    }

    fn flush_turn_text_presentations(&mut self, turn_id: &str) -> bool {
        let pending_ids = self
            .pending_text_presentations
            .iter()
            .filter(|(_, pending)| pending.destination.turn_id() == turn_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut changed = false;
        for id in pending_ids {
            if let Some(pending) = self.pending_text_presentations.remove(&id) {
                changed |= self.replace_presented_text(&id, &pending.target, &pending.destination);
            }
        }
        changed
    }

    fn flush_pending_text_presentations(&mut self) -> bool {
        let pending = std::mem::take(&mut self.pending_text_presentations);
        let mut changed = false;
        for (id, pending) in pending {
            changed |= self.replace_presented_text(&id, &pending.target, &pending.destination);
        }
        changed
    }

    fn remove_transcript_item(&mut self, id: &TranscriptViewItemId) {
        let before = self.snapshot.transcript.items.len();
        self.snapshot.transcript.items.retain(|item| item.id != *id);
        if self.snapshot.transcript.items.len() != before {
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
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

    /// Finish a streaming transcript message, or push it when no stream is open.
    ///
    /// Every caller delivers canonical durable event content. A degraded live stream must not
    /// block that text: the durable event is authoritative and the live projection is derived,
    /// disposable presentation state. Refusing the update here left renderers showing text
    /// truncated at the point live delivery was lost.
    fn finish_or_push_message(
        &mut self,
        id: TranscriptViewItemId,
        sequence: u64,
        timestamp_ms: Option<u64>,
        kind: StreamingMessageKind,
        text: &str,
    ) {
        if let Some(pending) = self.pending_text_presentations.get_mut(&id) {
            if text.starts_with(&pending.target) {
                text.clone_into(&mut pending.target);
                pending.remaining_graphemes = pending.presented_since_pace.saturating_add(
                    pending.target[pending.presented_bytes..]
                        .graphemes(true)
                        .count(),
                );
                if let Some(item) = self
                    .snapshot
                    .transcript
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                {
                    item.sequence = (sequence != 0).then_some(sequence).or(item.sequence);
                    item.timestamp_ms = timestamp_ms.or(item.timestamp_ms);
                    item.streaming = false;
                }
                return;
            }
            let pending = self
                .pending_text_presentations
                .remove(&id)
                .expect("pending exists");
            let _ = self.replace_presented_text(&id, &pending.target, &pending.destination);
        }
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

fn presentation_update_contribution(
    update: &bcode_tool::ToolPresentationUpdate,
) -> bcode_session_models::ToolContributionEvent {
    let contribution_id = match &update.identity {
        bcode_tool::ToolPresentationIdentity::Primary => "primary".to_owned(),
        bcode_tool::ToolPresentationIdentity::Supplemental { item_id } => item_id.clone(),
    };
    bcode_session_models::ToolContributionEvent {
        invocation_id: update.invocation_id.clone(),
        contribution_id,
        sequence: update.revision,
        producer_id: update.producer_id.clone(),
        schema: update.schema.clone(),
        schema_version: update.schema_version,
        operation: bcode_session_models::ToolContributionOperation::Upsert,
        persistence: match update.retention {
            bcode_tool::ToolPresentationRetention::RetainLatest => {
                bcode_session_models::ToolContributionPersistence::Durable
            }
            bcode_tool::ToolPresentationRetention::ActiveOnly => {
                bcode_session_models::ToolContributionPersistence::Transient
            }
        },
        artifact: update.artifact.clone(),
        payload: update.payload.clone(),
    }
}

/// Decode-only adapter from historical placement events into canonical invocation identities.
///
/// This module may be removed only when the supported session-history contract no longer includes
/// schemas that encoded `ToolContributionPlacement`. Until then it must remain decode-only: new
/// producers are rejected by `scripts/check-loop-runtime-architecture.sh`, and canonical update
/// APIs must not call into this adapter.
mod legacy_contribution_projection {
    use super::TranscriptViewItemId;

    pub fn item_id(
        invocation_id: &str,
        contribution_id: &str,
        placement: bcode_session_models::ToolContributionPlacement,
    ) -> TranscriptViewItemId {
        match placement {
            bcode_session_models::ToolContributionPlacement::Supplemental => {
                TranscriptViewItemId::tool_supplemental(invocation_id, contribution_id)
            }
            bcode_session_models::ToolContributionPlacement::Hidden => TranscriptViewItemId::new(
                format!("legacy-hidden:{invocation_id}:{contribution_id}"),
            ),
            bcode_session_models::ToolContributionPlacement::Request
            | bcode_session_models::ToolContributionPlacement::Progress
            | bcode_session_models::ToolContributionPlacement::Result => {
                TranscriptViewItemId::tool(invocation_id)
            }
        }
    }
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
        duration_ms: projection
            .duration_ms
            .or_else(|| projection_duration_ms(projection.raw_result.as_ref())),
    };
    ToolInvocationView {
        tool_call_id: projection.tool_call_id,
        producer_plugin_id: projection.producer_plugin_id,
        tool_name: projection.tool_name,
        arguments_json: projection.arguments_json,
        working_directory: projection.working_directory,
        request_draft: None,
        status: projection.status.into(),
        result_text: projection.result_text,
        is_error: projection.is_error,
        result: projection.raw_result.map(ToolResultView::from),
        presentation: projection.presentation.as_ref().map(Into::into),
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
    let had_readable_parts = parts.iter().any(|part| !part.text.is_empty());
    if visible {
        parts.retain(|part| reasoning_part_selected(part.kind, mode));
    } else {
        parts.clear();
    }
    let readable_parts_filtered =
        had_readable_parts && !parts.iter().any(|part| !part.text.is_empty());
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
        readable_parts_filtered,
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
    let had_readable_parts = parts.iter().any(|part| !part.text.is_empty());
    if visible {
        parts.retain(|part| reasoning_part_selected(part.kind, mode));
    } else {
        parts.clear();
    }
    let readable_parts_filtered =
        had_readable_parts && !parts.iter().any(|part| !part.text.is_empty());
    bcode_session_view_models::ReasoningActivityView {
        turn_id: turn_id.to_owned(),
        activity_id: activity.activity_id.clone(),
        order: activity.order,
        status: activity.status,
        parts,
        opaque: activity.opaque,
        readable_parts_filtered,
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

const fn transcript_item_text(item: &TranscriptViewItem) -> Option<&str> {
    match &item.kind {
        TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => Some(message.text.as_str()),
        _ => None,
    }
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

fn output_location_precedes(
    candidate: Option<&TurnOutputLocation>,
    neighbor: Option<&TurnOutputLocation>,
) -> bool {
    matches!(
        (candidate, neighbor),
        (Some(candidate), Some(neighbor))
            if candidate.turn_id == neighbor.turn_id
                && candidate.position < neighbor.position
    )
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

    fn ordered_assistant_update(
        session_id: SessionId,
        update: bcode_session_models::TextStreamUpdate,
    ) -> SessionLiveEvent {
        SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-1".to_owned(),
                segment_order: 0,
                update,
            },
        }
    }

    fn ordered_reasoning_update(
        session_id: SessionId,
        generation: u64,
        first_revision: u64,
        revision: u64,
        expected_offset: usize,
        text: &str,
    ) -> SessionLiveEvent {
        SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                output_position: None,
                turn_id: "turn-1".to_owned(),
                activity_id: "activity-1".to_owned(),
                activity_order: 0,
                part_id: "part-1".to_owned(),
                kind: bcode_session_models::ReasoningContentKind::Summary,
                role: bcode_session_models::ReasoningContentRole::Detail,
                part_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation,
                    first_revision,
                    revision,
                    operation: bcode_session_models::TextStreamOperation::Append {
                        expected_offset,
                        text: text.to_owned(),
                    },
                },
            },
        }
    }

    fn projected_reasoning_text(view: &SessionView) -> String {
        view.snapshot()
            .transcript
            .items
            .iter()
            .find_map(|item| match &item.kind {
                TranscriptViewItemKind::ReasoningActivity { activity } => Some(activity.text()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn smoothed_reasoning_overlapping_arrivals_preserve_exact_target() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_reasoning_update(session_id, 0, 1, 1, 0, "first"));
        assert_eq!(projected_reasoning_text(&view), "f");
        view.apply_live_event(&ordered_reasoning_update(session_id, 0, 2, 2, 5, " second"));
        assert!("first second".starts_with(&projected_reasoning_text(&view)));
        let deadline = view
            .next_streaming_presentation_deadline(Instant::now())
            .expect("reasoning presentation deadline");
        assert!(view.advance_streaming_presentation(deadline + Duration::from_millis(100)));
        assert_eq!(projected_reasoning_text(&view), "first second");
    }

    #[test]
    fn generation_replacement_discards_stale_assistant_backlog() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "obsolete response".to_owned(),
                },
            },
        ));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 1,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "replacement".to_owned(),
                },
            },
        ));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("r"));
        let deadline = view
            .next_streaming_presentation_deadline(Instant::now())
            .expect("replacement presentation deadline");
        assert!(view.advance_streaming_presentation(deadline + Duration::from_millis(100)));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("replacement")
        );
    }

    #[test]
    fn legacy_deltas_share_smoothing_and_exact_terminal_flush() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        for text in ["legacy", " delta"] {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantTextDelta {
                    turn_id: "turn-1".to_owned(),
                    segment_id: "segment-1".to_owned(),
                    segment_order: 0,
                    text: text.to_owned(),
                },
            });
        }
        assert!("legacy delta".starts_with(transcript_item_text(view.snapshot(), &id).unwrap()));
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ModelTurnFinished {
                turn_id: "turn-1".to_owned(),
                outcome: bcode_session_models::ModelTurnOutcome::Completed,
                message: None,
            },
        ));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("legacy delta")
        );
    }

    #[test]
    fn live_policy_changes_update_existing_backlog_without_hiding_text() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        let initial = StreamingPresentationPolicy {
            graphemes_per_second: 5,
            max_lag_ms: 1_000,
            ..StreamingPresentationPolicy::default()
        };
        assert!(!view.set_streaming_presentation_policy(initial));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "abcdefghijklmnopqrstuvwxyz".to_owned(),
                },
            },
        ));
        let visible_before = transcript_item_text(view.snapshot(), &id)
            .expect("visible prefix")
            .to_owned();
        let pace_anchor = view.pending_text_presentations[&id].paced_from;
        let changed = StreamingPresentationPolicy {
            curve: StreamingInterpolationCurve::EaseInOut,
            graphemes_per_second: 900,
            max_lag_ms: 10,
            ..initial
        };
        assert!(!view.set_streaming_presentation_policy(changed));
        let pending = &view.pending_text_presentations[&id];
        assert_eq!(pending.paced_from, pace_anchor);
        assert_eq!(pending.graphemes_per_second, 900);
        assert_eq!(pending.max_lag, Duration::from_millis(10));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some(visible_before.as_str())
        );
        assert!(view.advance_streaming_presentation(pace_anchor + Duration::from_millis(11)));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("abcdefghijklmnopqrstuvwxyz")
        );
    }

    #[test]
    fn immediate_policy_flushes_and_reenabling_never_hides_text() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "whole accepted target".to_owned(),
                },
            },
        ));
        assert!(view.set_streaming_presentation_policy(StreamingPresentationPolicy::immediate()));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("whole accepted target")
        );
        assert!(view.pending_text_presentations.is_empty());
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("whole accepted target")
        );
    }

    #[test]
    fn policy_change_before_cancel_keeps_terminal_state_absorbing() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "accepted before cancellation".to_owned(),
                },
            },
        ));
        let changed = StreamingPresentationPolicy {
            curve: StreamingInterpolationCurve::EaseOut,
            graphemes_per_second: 25,
            max_lag_ms: 500,
            ..StreamingPresentationPolicy::default()
        };
        assert!(!view.set_streaming_presentation_policy(changed));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 2,
                revision: 2,
                operation: bcode_session_models::TextStreamOperation::Terminal {
                    status: bcode_session_models::TextStreamTerminalStatus::Cancelled,
                },
            },
        ));
        let visible_at_cancel = transcript_item_text(view.snapshot(), &id)
            .expect("visible prefix at cancellation")
            .to_owned();
        assert!("accepted before cancellation".starts_with(&visible_at_cancel));
        assert!(matches!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Terminal(
                bcode_session_models::TextStreamTerminalStatus::Cancelled
            )
        ));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 3,
                revision: 3,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: "accepted before cancellation".len(),
                    text: " ignored".to_owned(),
                },
            },
        ));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some(visible_at_cancel.as_str())
        );
    }

    #[test]
    fn performance_contract_first_grapheme_lag_flush_and_memory_are_bounded() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        let append_started = Instant::now();
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "abcdefghijklmnopqrstuvwxyz".to_owned(),
                },
            },
        ));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("a"));
        assert!(append_started.elapsed() < Duration::from_millis(10));
        let next_deadline = view
            .next_streaming_presentation_deadline(append_started)
            .expect("presentation deadline");
        let pending = view
            .pending_text_presentations
            .get(&id)
            .expect("pending presentation");
        let terminal_deadline = pending.paced_from + pending.max_lag;
        assert!(next_deadline <= terminal_deadline);
        assert!(
            terminal_deadline.saturating_duration_since(append_started)
                <= Duration::from_millis(StreamingPresentationPolicy::DEFAULT_MAX_LAG_MS + 1)
        );
        assert!(view.advance_streaming_presentation(terminal_deadline + Duration::from_millis(1)));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("abcdefghijklmnopqrstuvwxyz")
        );

        let terminal_started = Instant::now();
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 2,
                revision: 2,
                operation: bcode_session_models::TextStreamOperation::Terminal {
                    status: bcode_session_models::TextStreamTerminalStatus::Completed,
                },
            },
        ));
        assert!(terminal_started.elapsed() < Duration::from_millis(10));
        assert!(view.pending_text_presentations.is_empty());

        let pending_size = std::mem::size_of::<PendingTextPresentation>();
        assert!(pending_size < 512);
        assert_eq!(view.pending_text_presentations.len(), 0);
    }

    #[test]
    fn oversized_pending_chunk_catches_up_without_retaining_backlog() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let text = "x".repeat(MAX_RETAINED_PENDING_TEXT_BYTES + 1);
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: text.clone(),
                },
            },
        ));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some(text.as_str())
        );
        assert!(
            view.next_streaming_presentation_deadline(Instant::now())
                .is_none()
        );
    }

    #[test]
    fn interpolation_curves_have_deterministic_relative_progress() {
        let started = Instant::now();
        let target = "abcdefghij".to_owned();
        let pending = PendingTextPresentation::new(
            target,
            0,
            started,
            10,
            Duration::from_millis(100),
            PendingTextDestination::Transcript {
                turn_id: "turn-1".to_owned(),
            },
        );
        let midpoint = started + Duration::from_millis(50);
        let linear = pending.due_bytes(midpoint, StreamingInterpolationCurve::Linear);
        let ease_in = pending.due_bytes(midpoint, StreamingInterpolationCurve::EaseIn);
        let ease_out = pending.due_bytes(midpoint, StreamingInterpolationCurve::EaseOut);
        let ease_in_out = pending.due_bytes(midpoint, StreamingInterpolationCurve::EaseInOut);
        assert_eq!(linear, 5);
        assert_eq!(ease_in, 1);
        assert_eq!(ease_out, 8);
        assert_eq!(ease_in_out, 5);
        assert_eq!(
            pending.due_bytes(
                started + Duration::from_millis(100),
                StreamingInterpolationCurve::EaseIn,
            ),
            10
        );
    }

    #[test]
    fn pacing_anchor_survives_target_extension() {
        let started = Instant::now();
        let destination = PendingTextDestination::Transcript {
            turn_id: "turn-1".to_owned(),
        };
        let mut pending = PendingTextPresentation::new(
            "abcdef".to_owned(),
            1,
            started,
            10,
            Duration::from_secs(1),
            destination.clone(),
        );

        pending.record_presented(pending.due_bytes(
            started + Duration::from_millis(200),
            StreamingInterpolationCurve::Linear,
        ));
        let paced_from = pending.paced_from;
        pending.rebase(
            "abcdefghijkl".to_owned(),
            started + Duration::from_millis(200),
            10,
            Duration::from_secs(1),
            destination,
        );

        assert_eq!(pending.paced_from, paced_from);
        assert_eq!(
            pending.due_bytes(
                started + Duration::from_millis(300),
                StreamingInterpolationCurve::Linear,
            ),
            4
        );
    }

    #[test]
    fn backlog_catch_up_reaches_target_within_ceiling() {
        let started = Instant::now();
        let target = "x".repeat(2_000);
        let pending = PendingTextPresentation::new(
            target.clone(),
            0,
            started,
            1,
            Duration::from_millis(StreamingPresentationPolicy::MAX_LAG_MS),
            PendingTextDestination::Transcript {
                turn_id: "turn-1".to_owned(),
            },
        );

        assert_eq!(
            pending.due_bytes(
                started + Duration::from_millis(StreamingPresentationPolicy::MAX_LAG_MS),
                StreamingInterpolationCurve::Linear,
            ),
            target.len()
        );
    }

    #[test]
    fn pacing_reveals_only_complete_grapheme_clusters() {
        let started = Instant::now();
        let target = "👩🏽‍💻e\u{301}終";
        let pending = PendingTextPresentation::new(
            target.to_owned(),
            0,
            started,
            3,
            Duration::from_secs(1),
            PendingTextDestination::Transcript {
                turn_id: "turn-1".to_owned(),
            },
        );

        let first = pending.due_bytes(
            started + Duration::from_millis(334),
            StreamingInterpolationCurve::Linear,
        );
        let second = pending.due_bytes(
            started + Duration::from_millis(667),
            StreamingInterpolationCurve::Linear,
        );
        assert_eq!(&target[..first], "👩🏽‍💻");
        assert_eq!(&target[..second], "👩🏽‍💻e\u{301}");
        assert_eq!(
            pending.due_bytes(
                started + Duration::from_secs(1),
                StreamingInterpolationCurve::Linear,
            ),
            target.len()
        );
    }

    #[test]
    fn durable_segment_mid_pace_preserves_visible_prefix_and_exact_target() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "durable target".to_owned(),
                },
            },
        ));
        let visible_before = transcript_item_text(view.snapshot(), &id)
            .expect("visible assistant prefix")
            .to_owned();

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::AssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-1".to_owned(),
                segment_order: 0,
                text: "durable target extended".to_owned(),
            },
        ));

        let visible_after = transcript_item_text(view.snapshot(), &id)
            .expect("visible assistant prefix after durable event");
        assert_eq!(visible_after, visible_before);
        let pending = view
            .pending_text_presentations
            .get(&id)
            .expect("durable target remains paced");
        assert_eq!(pending.target, "durable target extended");
        let deadline = pending.paced_from + pending.max_lag;
        assert!(view.advance_streaming_presentation(deadline));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("durable target extended")
        );
    }

    #[test]
    fn zero_rate_policy_presents_immediately() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(
            !view.set_streaming_presentation_policy(StreamingPresentationPolicy {
                enabled: true,
                graphemes_per_second: 0,
                ..StreamingPresentationPolicy::default()
            })
        );
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "whole chunk".to_owned(),
                },
            },
        ));

        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("whole chunk")
        );
        assert!(view.pending_text_presentations.is_empty());
    }

    #[test]
    fn checkpoint_replaces_pending_presentation_immediately() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "pending text".to_owned(),
                },
            },
        ));
        assert!(view.pending_text_presentations.contains_key(&id));

        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 2,
                revision: 2,
                operation: bcode_session_models::TextStreamOperation::Checkpoint {
                    start_offset: 0,
                    text: "hydrated checkpoint".to_owned(),
                    total_bytes: "hydrated checkpoint".len(),
                    truncated: false,
                },
            },
        ));

        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("hydrated checkpoint")
        );
        assert!(!view.pending_text_presentations.contains_key(&id));
    }

    #[test]
    fn stream_terminal_keeps_committed_text_pacing_until_turn_finish() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "committed output".to_owned(),
                },
            },
        ));
        let visible_before_terminal = transcript_item_text(view.snapshot(), &id)
            .expect("visible prefix")
            .to_owned();

        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 2,
                revision: 2,
                operation: bcode_session_models::TextStreamOperation::Terminal {
                    status: bcode_session_models::TextStreamTerminalStatus::Completed,
                },
            },
        ));

        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some(visible_before_terminal.as_str())
        );
        assert!(view.pending_text_presentations.contains_key(&id));
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ModelTurnFinished {
                turn_id: "turn-1".to_owned(),
                outcome: bcode_session_models::ModelTurnOutcome::Completed,
                message: None,
            },
        ));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("committed output")
        );
        assert!(!view.pending_text_presentations.contains_key(&id));
    }

    #[test]
    fn policy_disable_flushes_pending_text_immediately() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "pending text".to_owned(),
                },
            },
        ));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("p"));
        assert!(view.set_streaming_presentation_policy(StreamingPresentationPolicy::immediate()));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("pending text")
        );
        assert!(
            view.next_streaming_presentation_deadline(Instant::now())
                .is_none()
        );
    }

    #[test]
    fn history_window_rebuild_discards_pending_presentation_and_preserves_policy() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "pending".to_owned(),
                },
            },
        ));
        assert!(
            view.next_streaming_presentation_deadline(Instant::now())
                .is_some()
        );
        view.rebuild_history_window(&[]);
        assert!(
            view.next_streaming_presentation_deadline(Instant::now())
                .is_none()
        );
        assert_eq!(
            view.streaming_presentation_policy,
            StreamingPresentationPolicy::default()
        );
    }

    #[test]
    fn model_turn_finish_flushes_pending_text_without_stream_terminal() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "cancelled output".to_owned(),
                },
            },
        ));
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ModelTurnFinished {
                turn_id: "turn-1".to_owned(),
                outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
                message: None,
            },
        ));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("cancelled output")
        );
        assert!(
            view.next_streaming_presentation_deadline(Instant::now())
                .is_none()
        );
    }

    #[test]
    fn smoothed_assistant_stream_exposes_first_grapheme_then_exact_target() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::default()));
        let started = Instant::now();
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "e\u{301}clair".to_owned(),
                },
            },
        ));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("e\u{301}"));
        assert!(view.next_streaming_presentation_deadline(started).is_some());
        assert!(view.advance_streaming_presentation(started + Duration::from_millis(100)));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("e\u{301}clair")
        );
        assert!(
            view.next_streaming_presentation_deadline(Instant::now())
                .is_none()
        );
    }

    #[test]
    fn immediate_policy_preserves_whole_chunk_behavior() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        assert!(!view.set_streaming_presentation_policy(StreamingPresentationPolicy::immediate()));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "whole chunk".to_owned(),
                },
            },
        ));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("whole chunk")
        );
        assert!(
            view.next_streaming_presentation_deadline(Instant::now())
                .is_none()
        );
    }

    #[test]
    fn terminal_update_flushes_pending_text_and_is_absorbing() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-1");
        let mut view = SessionView::new();
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "complete".to_owned(),
                },
            },
        ));
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 2,
                revision: 2,
                operation: bcode_session_models::TextStreamOperation::Terminal {
                    status: bcode_session_models::TextStreamTerminalStatus::Completed,
                },
            },
        ));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("complete"));
        assert!(
            view.next_streaming_presentation_deadline(Instant::now())
                .is_none()
        );
        view.apply_live_event(&ordered_assistant_update(
            session_id,
            bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 3,
                revision: 3,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 8,
                    text: " stale".to_owned(),
                },
            },
        ));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("complete"));
    }

    #[test]
    fn request_keyed_cost_projection_replaces_usage_and_separates_currencies() {
        let session_id = SessionId::new();
        let usage =
            |request_id: &str, currency: &str, revision: &str, total_micros| SessionTokenUsage {
                observation_id: Some(format!("{request_id}:{total_micros}")),
                observation_ordinal: u32::try_from(total_micros).expect("fixture ordinal"),
                request_id: Some(request_id.to_string()),
                cost: Some(bcode_session_models::SessionCostEstimate::Estimated {
                    currency: currency.to_string(),
                    total_micros,
                    components: Vec::new(),
                    source: "fixture".to_string(),
                    revision: Some(revision.to_string()),
                }),
                ..SessionTokenUsage::default()
            };
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_string(),
                usage: usage("request-1", "USD", "catalog-v1", 10),
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_string(),
                usage: usage("request-1", "USD", "catalog-v1", 15),
            },
        ));
        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ModelUsage {
                turn_id: "turn-2".to_string(),
                usage: usage("request-2", "EUR", "catalog-v2", 20),
            },
        ));

        assert_eq!(view.snapshot().runtime.cost.totals_micros["USD"], 15);
        assert_eq!(view.snapshot().runtime.cost.totals_micros["EUR"], 20);
        assert_eq!(view.snapshot().runtime.cost.estimated_usage_count, 2);
        assert_eq!(view.snapshot().runtime.cost.observed_usage_count, 2);
        assert!(view.snapshot().runtime.cost.has_complete_coverage());
        let revisions = view
            .usage_by_request
            .values()
            .filter_map(|usage| match usage.cost.as_ref() {
                Some(bcode_session_models::SessionCostEstimate::Estimated { revision, .. }) => {
                    revision.as_deref()
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(revisions, BTreeSet::from(["catalog-v1", "catalog-v2"]));
        assert_eq!(view.snapshot().runtime.cumulative_metered_tokens, 0);
    }

    #[test]
    fn cost_projection_exposes_partial_coverage() {
        let mut summary = bcode_session_view_models::SessionCostSummary::default();
        summary.observe(&SessionTokenUsage {
            cost: Some(bcode_session_models::SessionCostEstimate::Estimated {
                currency: "USD".to_string(),
                total_micros: 1,
                components: Vec::new(),
                source: "fixture".to_string(),
                revision: None,
            }),
            ..SessionTokenUsage::default()
        });
        summary.observe(&SessionTokenUsage {
            cost: Some(bcode_session_models::SessionCostEstimate::Unavailable {
                reason: bcode_session_models::SessionCostUnavailableReason::ProviderUsageIncomplete,
            }),
            ..SessionTokenUsage::default()
        });

        assert_eq!(summary.observed_usage_count, 2);
        assert_eq!(summary.estimated_usage_count, 1);
        assert_eq!(summary.unavailable_usage_count, 1);
        assert!(!summary.has_complete_coverage());
    }

    #[test]
    fn conflicting_duplicate_usage_observation_is_rejected() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for (sequence, tokens) in [(1, 10), (2, 30)] {
            view.apply_event(&event(
                session_id,
                sequence,
                SessionEventKind::ModelUsage {
                    turn_id: "turn-1".to_string(),
                    usage: SessionTokenUsage {
                        request_id: Some("request-1".to_string()),
                        observation_id: Some("request-1:0".to_string()),
                        observation_ordinal: 0,
                        total_tokens: Some(tokens),
                        ..SessionTokenUsage::default()
                    },
                },
            ));
        }

        assert_eq!(view.snapshot().runtime.cumulative_metered_tokens, 10);
        assert_eq!(
            view.snapshot()
                .runtime
                .latest_usage
                .as_ref()
                .and_then(|usage| usage.total_tokens),
            Some(10)
        );
    }

    #[test]
    fn terminal_request_usage_is_absorbing_and_cumulative_tokens_are_replaced() {
        let session_id = SessionId::new();
        let usage = |ordinal, terminal, total_tokens| SessionTokenUsage {
            request_id: Some("request-1".to_string()),
            observation_id: Some(format!("request-1:{ordinal}")),
            observation_ordinal: ordinal,
            terminal,
            total_tokens: Some(total_tokens),
            ..SessionTokenUsage::default()
        };
        let mut view = SessionView::new();
        for (sequence, item) in [usage(0, false, 10), usage(1, true, 15), usage(2, false, 30)]
            .into_iter()
            .enumerate()
        {
            view.apply_event(&event(
                session_id,
                u64::try_from(sequence + 1).expect("sequence"),
                SessionEventKind::ModelUsage {
                    turn_id: "turn-1".to_string(),
                    usage: item,
                },
            ));
        }

        assert_eq!(view.snapshot().runtime.cumulative_metered_tokens, 15);
        assert_eq!(
            view.snapshot()
                .runtime
                .latest_usage
                .as_ref()
                .and_then(|usage| usage.total_tokens),
            Some(15)
        );
    }

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

    fn transcript_item_text<'a>(
        snapshot: &'a SessionViewSnapshot,
        id: &TranscriptViewItemId,
    ) -> Option<&'a str> {
        snapshot
            .transcript
            .items
            .iter()
            .find(|item| item.id == *id)
            .and_then(|item| match &item.kind {
                TranscriptViewItemKind::AssistantMessage { message } => Some(message.text.as_str()),
                _ => None,
            })
    }

    fn tool_request_draft(item: &TranscriptViewItem) -> Option<&ToolRequestDraftView> {
        match &item.kind {
            TranscriptViewItemKind::ToolInvocation { tool } => tool.request_draft.as_ref(),
            TranscriptViewItemKind::ToolRequestDraft { draft } => Some(draft),
            _ => None,
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
                        presentation: None,
                        result: Some(ToolInvocationResult::Json {
                            value: r#"{"opaque_result":true}"#.to_owned(),
                        }),
                        content: Vec::new(),
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
        let interaction_id = TranscriptViewItemId::interaction("question");
        let pending = view
            .snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.id == interaction_id)
            .expect("pending interaction row");
        let pending_revision = pending.revision;
        assert!(matches!(
            &pending.kind,
            TranscriptViewItemKind::Interaction { interaction }
                if interaction.kind == "future.question/schema"
                    && interaction.required
                    && interaction.snapshot
                        == Some(serde_json::json!({"opaque_question": true}))
                    && interaction.state == InteractionViewState::Pending
                    && !interaction.resolved
        ));

        view.apply_event(&resolved);
        assert!(view.snapshot().active_exchanges.is_empty());
        let terminal = view
            .snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.id == interaction_id)
            .expect("terminal interaction row");
        assert!(terminal.revision > pending_revision);
        assert_eq!(terminal.sequence, Some(10));
        assert!(matches!(
            &terminal.kind,
            TranscriptViewItemKind::Interaction { interaction }
                if interaction.state == InteractionViewState::Resolved
                    && interaction.resolved
                    && interaction.resolution
                        == Some(bcode_session_models::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({"opaque_answer": 42}),
                        })
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
    fn terminal_interaction_ignores_stale_request_and_pending_hydration() {
        let session_id = SessionId::new();
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "call".to_owned(),
            exchange_id: "interaction".to_owned(),
            producer_id: "example.plugin".to_owned(),
            schema: "example.request".to_owned(),
            schema_version: 1,
            payload: serde_json::json!({"request": true}),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let resolution = bcode_session_models::ToolExchangeResolution::Responded {
            payload: serde_json::json!({"opaque": true}),
        };
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolExchangeRequested {
                request: request.clone(),
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolExchangeResolved {
                event: bcode_session_models::ToolExchangeResolutionEvent {
                    invocation_id: request.invocation_id.clone(),
                    exchange_id: request.exchange_id.clone(),
                    resolution: resolution.clone(),
                },
            },
        ));

        view.upsert_interaction(interaction_from_exchange_request(&request));
        view.set_pending_interactions(vec![interaction_from_exchange_request(&request)]);
        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ToolExchangeRequested { request },
        ));

        assert!(view.snapshot().active_exchanges.is_empty());
        let interaction = view
            .snapshot()
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == "interaction")
            .expect("terminal interaction");
        assert!(interaction.resolved);
        assert_eq!(interaction.resolution, Some(resolution));
    }

    #[test]
    fn exchange_terminal_resolution_without_resident_request_is_retained_generically() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            10,
            SessionEventKind::ToolExchangeResolved {
                event: bcode_session_models::ToolExchangeResolutionEvent {
                    invocation_id: "call".to_owned(),
                    exchange_id: "question".to_owned(),
                    resolution: bcode_session_models::ToolExchangeResolution::TimedOut,
                },
            },
        ));

        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::Interaction { interaction }
                if interaction.kind == "bcode.tool.exchange"
                    && interaction.state == InteractionViewState::Cancelled
                    && interaction.resolved
                    && interaction.status_detail.as_deref() == Some("timed out")
                    && interaction.resolution
                        == Some(bcode_session_models::ToolExchangeResolution::TimedOut)
        ));
    }

    #[test]
    fn duplicate_and_conflicting_terminal_resolutions_keep_first_authoritative_outcome() {
        let session_id = SessionId::new();
        let terminal_event = |sequence, resolution| {
            event(
                session_id,
                sequence,
                SessionEventKind::ToolExchangeResolved {
                    event: bcode_session_models::ToolExchangeResolutionEvent {
                        invocation_id: "call".to_owned(),
                        exchange_id: "interaction".to_owned(),
                        resolution,
                    },
                },
            )
        };
        let first = bcode_session_models::ToolExchangeResolution::Responded {
            payload: serde_json::json!({"first": true}),
        };
        let mut view = SessionView::new();
        view.apply_event(&terminal_event(1, first.clone()));
        view.apply_event(&terminal_event(2, first.clone()));
        view.apply_event(&terminal_event(
            3,
            bcode_session_models::ToolExchangeResolution::Cancelled,
        ));

        let interaction = view
            .snapshot()
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == "interaction")
            .expect("terminal interaction");
        assert_eq!(interaction.resolution, Some(first));
        assert_eq!(interaction.state, InteractionViewState::Resolved);
    }

    #[test]
    fn terminal_interaction_survives_history_window_rebuild() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolExchangeResolved {
                event: bcode_session_models::ToolExchangeResolutionEvent {
                    invocation_id: "call".to_owned(),
                    exchange_id: "interaction".to_owned(),
                    resolution: bcode_session_models::ToolExchangeResolution::TimedOut,
                },
            },
        ));

        view.rebuild_history_window(&[]);
        view.set_pending_interactions(vec![InteractionViewSummary {
            producer_id: Some("example.plugin".to_owned()),
            exchange_schema: Some("example.request".to_owned()),
            exchange_schema_version: Some(1),
            interaction_id: "interaction".to_owned(),
            kind: "example.interaction".to_owned(),
            tool_call_id: Some("call".to_owned()),
            title: Some("Example".to_owned()),
            required: true,
            snapshot: Some(serde_json::json!({})),
            state: InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        }]);

        let interaction = view
            .snapshot()
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == "interaction")
            .expect("retained terminal interaction");
        assert!(interaction.resolved);
        assert_eq!(
            interaction.resolution,
            Some(bcode_session_models::ToolExchangeResolution::TimedOut)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One scenario exercises every ordering/hydration/rebuild path against one terminal authority.
    fn generic_terminal_lifecycle_paths_share_one_absorbing_authority() {
        let session_id = SessionId::new();
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "call-generic".to_owned(),
            exchange_id: "generic-interaction".to_owned(),
            producer_id: "future.plugin".to_owned(),
            schema: "future.interaction".to_owned(),
            schema_version: 9,
            payload: serde_json::json!({"opaque": true}),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let first = bcode_session_models::ToolExchangeResolution::Failed {
            code: "unavailable".to_owned(),
            message: "consumer disappeared".to_owned(),
        };
        let resolution_event = |sequence, resolution| {
            event(
                session_id,
                sequence,
                SessionEventKind::ToolExchangeResolved {
                    event: bcode_session_models::ToolExchangeResolutionEvent {
                        invocation_id: request.invocation_id.clone(),
                        exchange_id: request.exchange_id.clone(),
                        resolution,
                    },
                },
            )
        };
        let pending = || InteractionViewSummary {
            producer_id: Some(request.producer_id.clone()),
            exchange_schema: Some(request.schema.clone()),
            exchange_schema_version: Some(request.schema_version),
            interaction_id: request.exchange_id.clone(),
            kind: "future.interaction".to_owned(),
            tool_call_id: Some(request.invocation_id.clone()),
            title: Some("Future interaction".to_owned()),
            required: true,
            snapshot: Some(request.payload.clone()),
            state: InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        };

        let mut resolution_first = SessionView::new();
        resolution_first.apply_event(&resolution_event(1, first.clone()));
        resolution_first.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolExchangeRequested {
                request: request.clone(),
            },
        ));
        resolution_first.set_pending_interactions(vec![pending()]);
        resolution_first.apply_event(&resolution_event(
            3,
            bcode_session_models::ToolExchangeResolution::Cancelled,
        ));
        resolution_first.rebuild_history_window(&[]);
        resolution_first.set_pending_interactions(vec![pending()]);

        let mut request_first = SessionView::new();
        request_first.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolExchangeRequested {
                request: request.clone(),
            },
        ));
        request_first.apply_event(&resolution_event(2, first.clone()));
        request_first.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ToolExchangeRequested {
                request: request.clone(),
            },
        ));
        request_first.set_pending_interactions(vec![pending()]);

        for view in [&resolution_first, &request_first] {
            let matching = view
                .snapshot()
                .interactions
                .iter()
                .filter(|interaction| interaction.interaction_id == "generic-interaction")
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), 1);
            assert_eq!(matching[0].resolution.as_ref(), Some(&first));
            assert!(matching[0].resolved);
            assert_eq!(matching[0].state, InteractionViewState::ActionError);
            let transcript = view
                .snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| {
                    matches!(
                        &item.kind,
                        TranscriptViewItemKind::Interaction { interaction }
                            if interaction.interaction_id == "generic-interaction"
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(transcript.len(), 1);
            assert!(matches!(
                &transcript[0].kind,
                TranscriptViewItemKind::Interaction { interaction }
                    if interaction.resolution.as_ref() == Some(&first) && interaction.resolved
            ));
        }
    }

    #[test]
    fn exchange_resolution_variants_have_explicit_terminal_semantics() {
        let variants = [
            (
                bcode_session_models::ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"status": "dismissed"}),
                },
                InteractionViewState::Resolved,
                None,
            ),
            (
                bcode_session_models::ToolExchangeResolution::Cancelled,
                InteractionViewState::Cancelled,
                Some("cancelled"),
            ),
            (
                bcode_session_models::ToolExchangeResolution::NoCompatibleConsumer,
                InteractionViewState::Cancelled,
                Some("no compatible consumer"),
            ),
            (
                bcode_session_models::ToolExchangeResolution::ConsumerDetached,
                InteractionViewState::Cancelled,
                Some("consumer detached"),
            ),
            (
                bcode_session_models::ToolExchangeResolution::TimedOut,
                InteractionViewState::Cancelled,
                Some("timed out"),
            ),
            (
                bcode_session_models::ToolExchangeResolution::Failed {
                    code: "route_failed".to_owned(),
                    message: "offline".to_owned(),
                },
                InteractionViewState::ActionError,
                Some("route_failed: offline"),
            ),
        ];

        for (index, (resolution, expected_state, expected_detail)) in
            variants.into_iter().enumerate()
        {
            let mut view = SessionView::new();
            let exchange_id = format!("question-{index}");
            view.apply_event(&event(
                SessionId::new(),
                1,
                SessionEventKind::ToolExchangeRequested {
                    request: bcode_session_models::ToolExchangeRequest {
                        invocation_id: format!("call-{index}"),
                        exchange_id: exchange_id.clone(),
                        producer_id: "producer".to_owned(),
                        schema: "schema".to_owned(),
                        schema_version: 1,
                        payload: serde_json::json!({}),
                        response_policy: bcode_session_models::ToolExchangeResponsePolicy::Optional,
                    },
                },
            ));
            view.apply_event(&event(
                view.snapshot().session_id.expect("session id"),
                2,
                SessionEventKind::ToolExchangeResolved {
                    event: bcode_session_models::ToolExchangeResolutionEvent {
                        invocation_id: format!("call-{index}"),
                        exchange_id,
                        resolution,
                    },
                },
            ));
            let interaction = &view.snapshot().interactions[0];
            assert_eq!(interaction.state, expected_state);
            assert_eq!(interaction.status_detail.as_deref(), expected_detail);
            assert!(interaction.resolved);
            let transcript_interaction = view
                .snapshot()
                .transcript
                .items
                .iter()
                .find(|item| {
                    matches!(
                        &item.kind,
                        TranscriptViewItemKind::Interaction { interaction: projected }
                            if projected.interaction_id == interaction.interaction_id
                    )
                })
                .expect("terminal interaction transcript identity");
            assert!(matches!(
                &transcript_interaction.kind,
                TranscriptViewItemKind::Interaction { interaction: projected }
                    if projected.interaction_id == interaction.interaction_id
                        && projected.state == expected_state
                        && projected.status_detail.as_deref() == expected_detail
                        && projected.resolved
            ));
        }
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
                    output_position: None,
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
            full_cadence.tool_request_drafts(),
            coalesced.tool_request_drafts()
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
                    output_position: None,
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
        assert_eq!(
            full_cadence.tool_request_drafts()["call-cadence"].preview,
            "latest"
        );
        assert_eq!(
            coalesced.tool_request_drafts()["call-cadence"].preview,
            "latest"
        );
        assert_eq!(full_cadence.snapshot().transcript.items.len(), 1);
        assert_eq!(coalesced.snapshot().transcript.items.len(), 1);
    }

    #[test]
    fn shared_live_delivery_attach_checkpoint_cancellation_and_finalization_are_equivalent() {
        let session_id = SessionId::new();
        let draft = |revision, operation| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
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
    fn draft_placement_change_replaces_the_same_primary_item() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |revision, placement| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-write".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement,
                    generation: 1,
                    revision,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: format!("preview-{revision}"),
                    },
                    argument_bytes: 9,
                    truncated: false,
                },
            },
        };
        view.apply_live_event(&draft(
            1,
            bcode_session_models::ToolContributionPlacement::Request,
        ));
        view.apply_live_event(&draft(
            2,
            bcode_session_models::ToolContributionPlacement::Result,
        ));

        let request_id = TranscriptViewItemId::tool_presentation_slot(
            "call-write",
            bcode_session_models::ToolContributionPlacement::Request,
            None,
        );
        let result_id = TranscriptViewItemId::tool_presentation_slot(
            "call-write",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        assert_eq!(request_id, result_id);
        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| item.id == request_id)
                .count(),
            1
        );
        let projected_draft = view
            .snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.id == result_id)
            .and_then(tool_request_draft)
            .expect("unified request draft");
        assert_eq!(
            projected_draft.placement,
            bcode_session_models::ToolContributionPlacement::Result
        );
        assert_eq!(projected_draft.preview, "preview-2");
    }

    #[test]
    fn request_draft_live_updates_apply_append_checkpoint_and_terminal_dominance() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |revision, operation| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
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
            .find(|item| tool_request_draft(item).is_some())
            .expect("request draft item");
        let projected_draft = tool_request_draft(item).expect("unified request draft");
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
        assert_eq!(
            view.tool_request_drafts()["call-write"].preview,
            "hello world"
        );
        assert_eq!(
            view.terminal_tool_request_drafts().get("call-write"),
            Some(&(1, 4))
        );
        assert!(view.snapshot().transcript.items.iter().any(|item| {
            tool_request_draft(item).is_some_and(|draft| draft.preview == "hello world")
        }));
    }

    #[test]
    fn request_draft_append_offsets_are_utf8_byte_offsets() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |revision, offset, text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
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
        assert_eq!(view.tool_request_drafts()["call-write"].preview, "λ🙂ok");

        let character_offset = "λ🙂ok".chars().count();
        assert_ne!(character_offset, "λ🙂ok".len());
        view.apply_live_event(&draft(3, character_offset, "wrong-unit"));
        assert_eq!(view.tool_request_drafts()["call-write"].preview, "λ🙂ok");
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
                        output_position: None,
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
            assert_eq!(view.tool_request_drafts().len(), 1);
            assert_eq!(view.snapshot().transcript.items.len(), 1);
        }

        assert_eq!(view.tool_request_drafts()["call-write"].preview, "10000");
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
                    output_position: None,
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

        let drafts = view.tool_request_drafts();
        let projected = drafts.get("call-write").expect("current request draft");
        assert_eq!(projected.generation, 2);
        assert_eq!(projected.revision, 1);
        assert_eq!(projected.preview, "replacement preview");
        let slot_id = TranscriptViewItemId::tool_presentation_slot(
            "call-write",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        let draft = view
            .snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.id == slot_id)
            .and_then(tool_request_draft)
            .expect("replacement draft");
        assert_eq!(draft.generation, 2);
        assert_eq!(draft.preview, "replacement preview");
    }

    #[test]
    fn interleaved_request_drafts_remain_isolated() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let draft = |tool_call_id: &str, revision, offset, text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
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

        assert_eq!(view.tool_request_drafts()["call-a"].preview, "alpha one");
        assert_eq!(view.tool_request_drafts()["call-b"].preview, "bravo two");
        let snapshot = view.snapshot();
        assert_eq!(
            snapshot
                .transcript
                .items
                .iter()
                .filter(|item| tool_request_draft(item).is_some())
                .count(),
            2
        );
        for (tool_call_id, expected) in [("call-a", "alpha one"), ("call-b", "bravo two")] {
            let slot_id = TranscriptViewItemId::tool_presentation_slot(
                tool_call_id,
                bcode_session_models::ToolContributionPlacement::Result,
                None,
            );
            let draft = snapshot
                .transcript
                .items
                .iter()
                .find(|item| item.id == slot_id)
                .and_then(tool_request_draft)
                .expect("isolated unified draft");
            assert_eq!(draft.tool_call_id, tool_call_id);
            assert_eq!(draft.preview, expected);
        }
    }

    #[test]
    fn historical_filesystem_change_request_replays_without_rewriting_history() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-edit".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    tool_name: "filesystem.edit".to_owned(),
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
                            invocation_id: "call-edit".to_owned(),
                            contribution_id: "filesystem-change-request".to_owned(),
                            sequence: 1,
                            producer_id: "bcode.filesystem".to_owned(),
                            schema: "bcode.filesystem.change".to_owned(),
                            schema_version: 1,
                            operation: bcode_session_models::ToolContributionOperation::Upsert,
                            persistence: bcode_session_models::ToolContributionPersistence::Durable,
                            artifact: None,
                            payload: serde_json::json!({
                                "operation": "filesystem.edit",
                                "path": "src/lib.rs",
                                "old_text": "before\n",
                                "new_text": "after\n"
                            }),
                        },
                    ),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call-edit".to_owned(),
                        model_output: "edited src/lib.rs".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: None,
                        content: Vec::new(),
                    },
                },
            ),
        ];
        let original = events.clone();

        let snapshot = build_session_view_snapshot(&events);

        assert_eq!(
            events, original,
            "projection must not rewrite historical events"
        );
        assert_eq!(
            snapshot
                .transcript
                .items
                .iter()
                .filter(|item| {
                    matches!(
                        &item.kind,
                        TranscriptViewItemKind::ToolContribution {
                            contribution,
                            invocation: Some(invocation),
                            ..
                        } if contribution.schema == "bcode.filesystem.change"
                            && contribution.payload["old_text"] == "before\n"
                            && contribution.payload["new_text"] == "after\n"
                            && invocation.status == ToolInvocationViewStatus::Finished
                    )
                })
                .count(),
            1
        );
    }

    #[test]
    fn durable_tool_request_reconciles_live_draft_without_duplicate_rows() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
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

        assert!(view.tool_request_drafts().is_empty());
        assert!(view.terminal_tool_request_drafts().is_empty());
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
                    output_position: None,
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
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "written".to_owned(),
                    }),
                    content: Vec::new(),
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
    #[allow(clippy::too_many_lines)] // One scenario proves every preview-to-final ordering boundary without weakening assertions.
    fn result_preview_survives_request_permission_and_execution_until_final_retirement() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let result_id = TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Result,
            None,
        );
        let assert_preview = |view: &SessionView| {
            let slots = view
                .snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| item.id == result_id)
                .collect::<Vec<_>>();
            assert_eq!(slots.len(), 1);
            assert!(tool_request_draft(slots[0]).is_some());
        };
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision: 1,
                    operation: bcode_session_models::ToolRequestDraftOperation::Append {
                        offset: 0,
                        text: "partial".to_owned(),
                    },
                    argument_bytes: 7,
                    truncated: false,
                },
            },
        });
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision: 2,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: "complete preview".to_owned(),
                    },
                    argument_bytes: 16,
                    truncated: false,
                },
            },
        });
        assert_preview(&view);

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
        assert_preview(&view);
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::PermissionRequested {
                permission_id: "permission-1".to_owned(),
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.write".to_owned(),
                arguments_json: "{}".to_owned(),
                batch: None,
                policy_source: None,
                policy_reason: Some("approval required".to_owned()),
            },
        ));
        assert_preview(&view);
        for (sequence, stage) in [
            (
                3,
                bcode_session_models::ToolInvocationLifecycleStage::Waiting,
            ),
            (
                4,
                bcode_session_models::ToolInvocationLifecycleStage::Started,
            ),
        ] {
            view.apply_event(&event(
                session_id,
                sequence,
                SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: "call-1".to_owned(),
                        sequence,
                        stage,
                        message: None,
                        metadata: serde_json::Value::Null,
                    },
                },
            ));
            assert_preview(&view);
        }

        view.apply_event(&event(
            session_id,
            5,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "written".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "written".to_owned(),
                    }),
                    content: Vec::new(),
                },
            },
        ));
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision: u64::MAX,
                    operation: bcode_session_models::ToolRequestDraftOperation::Remove {
                        reason: bcode_session_models::ToolRequestDraftTerminalReason::Completed,
                    },
                    argument_bytes: 16,
                    truncated: false,
                },
            },
        });
        let final_slots = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| item.id == result_id)
            .collect::<Vec<_>>();
        assert_eq!(final_slots.len(), 1);
        assert!(matches!(
            final_slots[0].kind,
            TranscriptViewItemKind::ToolInvocation { .. }
        ));
    }

    #[test]
    fn final_result_without_lifecycle_completion_retires_result_draft() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
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
        assert!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .find(|item| item.id == slot_id)
                .is_some_and(|item| tool_request_draft(item).is_some())
        );

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
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "written".to_owned(),
                    }),
                    content: Vec::new(),
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
                    output_position: None,
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
        assert!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .find(|item| item.id == slot_id)
                .is_some_and(|item| tool_request_draft(item).is_some())
        );

        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "written".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "written".to_owned(),
                    }),
                    content: Vec::new(),
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
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "canonical".to_owned(),
                    }),
                    content: Vec::new(),
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

        let result_items = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| {
                matches!(
                    &item.kind,
                    TranscriptViewItemKind::ToolInvocation { tool }
                        if tool.tool_call_id == "call-1"
                )
            })
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
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "semantic".to_owned(),
                    }),
                    content: Vec::new(),
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
    #[allow(clippy::too_many_lines)] // One table-driven fixture covers cancellation at each invocation boundary.
    fn cancellation_before_request_during_permission_and_during_execution_is_terminal() {
        for cancellation_point in ["before-request", "permission", "execution"] {
            let session_id = SessionId::new();
            let mut view = SessionView::new();
            let result_id = TranscriptViewItemId::tool_presentation_slot(
                "call-cancel",
                bcode_session_models::ToolContributionPlacement::Result,
                None,
            );
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        tool_call_id: "call-cancel".to_owned(),
                        tool_name: "filesystem.write".to_owned(),
                        producer_plugin_id: Some("bcode.filesystem".to_owned()),
                        schema: "bcode.filesystem.request-draft.write".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Result,
                        generation: 1,
                        revision: 1,
                        operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                            start_offset: 0,
                            text: "preview".to_owned(),
                        },
                        argument_bytes: 7,
                        truncated: false,
                    },
                },
            });
            if cancellation_point != "before-request" {
                view.apply_event(&event(
                    session_id,
                    1,
                    SessionEventKind::ToolCallRequested {
                        tool_call_id: "call-cancel".to_owned(),
                        producer_plugin_id: Some("bcode.filesystem".to_owned()),
                        tool_name: "filesystem.write".to_owned(),
                        arguments_json: "{}".to_owned(),
                        working_directory: None,
                    },
                ));
            }
            if cancellation_point == "permission" {
                view.apply_event(&event(
                    session_id,
                    2,
                    SessionEventKind::PermissionRequested {
                        permission_id: "permission-cancel".to_owned(),
                        tool_call_id: "call-cancel".to_owned(),
                        producer_plugin_id: Some("bcode.filesystem".to_owned()),
                        tool_name: "filesystem.write".to_owned(),
                        arguments_json: "{}".to_owned(),
                        batch: None,
                        policy_source: None,
                        policy_reason: Some("approval required".to_owned()),
                    },
                ));
            } else if cancellation_point == "execution" {
                view.apply_event(&event(
                    session_id,
                    2,
                    SessionEventKind::ToolInvocationLifecycle {
                        event: bcode_session_models::ToolInvocationLifecycleEvent {
                            invocation_id: "call-cancel".to_owned(),
                            sequence: 2,
                            stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                            message: None,
                            metadata: serde_json::Value::Null,
                        },
                    },
                ));
            }
            view.apply_event(&event(
                session_id,
                3,
                SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: "call-cancel".to_owned(),
                        sequence: 3,
                        stage: bcode_session_models::ToolInvocationLifecycleStage::Cancelled,
                        message: Some(format!("cancelled at {cancellation_point}")),
                        metadata: serde_json::Value::Null,
                    },
                },
            ));
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        tool_call_id: "call-cancel".to_owned(),
                        tool_name: "filesystem.write".to_owned(),
                        producer_plugin_id: Some("bcode.filesystem".to_owned()),
                        schema: "bcode.filesystem.request-draft.write".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Result,
                        generation: 1,
                        revision: u64::MAX,
                        operation: bcode_session_models::ToolRequestDraftOperation::Remove {
                            reason: bcode_session_models::ToolRequestDraftTerminalReason::Cancelled,
                        },
                        argument_bytes: 7,
                        truncated: false,
                    },
                },
            });

            if cancellation_point == "before-request" {
                assert_eq!(
                    view.snapshot()
                        .transcript
                        .items
                        .iter()
                        .filter(|item| item.id == result_id)
                        .count(),
                    0
                );
                assert!(!view.snapshot().tools.contains_key("call-cancel"));
            } else {
                let result_items = view
                    .snapshot()
                    .transcript
                    .items
                    .iter()
                    .filter(|item| item.id == result_id)
                    .collect::<Vec<_>>();
                assert_eq!(result_items.len(), 1, "{cancellation_point}");
                assert!(matches!(
                    &result_items[0].kind,
                    TranscriptViewItemKind::ToolInvocation { tool }
                        if tool.status == ToolInvocationViewStatus::Cancelled
                ));
            }
            assert!(
                !view
                    .snapshot()
                    .active_invocations
                    .contains_key("call-cancel")
            );
        }
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
                    output_position: None,
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
        assert!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .any(|item| { item.id == result_id && tool_request_draft(item).is_some() })
        );
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
    fn failure_with_and_without_typed_result_has_one_authoritative_result_slot() {
        for typed_result in [false, true] {
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
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        tool_call_id: "call-1".to_owned(),
                        tool_name: "example.tool".to_owned(),
                        producer_plugin_id: Some("example.plugin".to_owned()),
                        schema: "example.request-draft".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Result,
                        generation: 1,
                        revision: 1,
                        operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                            start_offset: 0,
                            text: "preview".to_owned(),
                        },
                        argument_bytes: 7,
                        truncated: false,
                    },
                },
            });
            view.apply_event(&event(
                session_id,
                2,
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call-1".to_owned(),
                        model_output: "failed".to_owned(),
                        is_error: true,
                        presentation: None,
                        result: typed_result.then(|| ToolInvocationResult::Text {
                            text: "typed failure".to_owned(),
                        }),
                        content: Vec::new(),
                    },
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
            let TranscriptViewItemKind::ToolInvocation { tool } = &result_items[0].kind else {
                panic!("failure must replace the preview with a tool result");
            };
            assert!(matches!(
                tool.status,
                ToolInvocationViewStatus::Finished | ToolInvocationViewStatus::Failed
            ));
            assert_eq!(tool.is_error, Some(true));
            assert_eq!(
                tool.result,
                typed_result.then(|| ToolResultView::Text {
                    text: "typed failure".to_owned(),
                })
            );
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
                    output_position: None,
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
    fn replayed_opaque_activity_reports_withheld_rather_than_filtered() {
        // An activity carrying only opaque evidence must be distinguishable from one the local
        // display policy emptied, so renderers can explain why no text is present.
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-opaque".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: Vec::new(),
                    opaque: true,
                },
            },
        )]);

        let TranscriptViewItemKind::ReasoningActivity { activity } =
            &snapshot.transcript.items[0].kind
        else {
            panic!("expected a reasoning activity item");
        };
        assert!(!activity.readable_parts_filtered);
        assert_eq!(
            activity.content_availability(),
            bcode_session_view_models::ReasoningContentAvailability::Withheld
        );
    }

    #[test]
    fn display_mode_filtering_reports_filtered_rather_than_withheld() {
        // Raw-only content hidden by `summary` mode is a local presentation choice, not a
        // provider decision, even when opaque evidence also exists.
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.set_reasoning_display_mode(bcode_session_view_models::ReasoningDisplayMode::Summary);
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-raw".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![bcode_session_models::ReasoningPart {
                        part_id: "raw-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Raw,
                        role: bcode_session_models::ReasoningContentRole::Detail,
                        order: 0,
                        text: "raw detail".to_owned(),
                    }],
                    opaque: true,
                },
            },
        ));

        let TranscriptViewItemKind::ReasoningActivity { activity } =
            &view.snapshot().transcript.items[0].kind
        else {
            panic!("expected a reasoning activity item");
        };
        assert!(activity.readable_parts_filtered);
        assert_eq!(
            activity.content_availability(),
            bcode_session_view_models::ReasoningContentAvailability::Filtered
        );
    }

    #[test]
    fn hidden_reasoning_reports_filtered_and_readable_content_reports_readable() {
        let session_id = SessionId::new();
        let reasoning_event = event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-1".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![bcode_session_models::ReasoningPart {
                        part_id: "raw-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Raw,
                        role: bcode_session_models::ReasoningContentRole::Detail,
                        order: 0,
                        text: "raw detail".to_owned(),
                    }],
                    opaque: false,
                },
            },
        );

        let mut visible = SessionView::new();
        visible.apply_event(&reasoning_event);
        let TranscriptViewItemKind::ReasoningActivity { activity } =
            &visible.snapshot().transcript.items[0].kind
        else {
            panic!("expected a reasoning activity item");
        };
        assert!(!activity.readable_parts_filtered);
        assert_eq!(
            activity.content_availability(),
            bcode_session_view_models::ReasoningContentAvailability::Readable
        );

        let mut hidden = SessionView::new();
        hidden.set_reasoning_visible(false);
        hidden.apply_event(&reasoning_event);
        let TranscriptViewItemKind::ReasoningActivity { activity } =
            &hidden.snapshot().transcript.items[0].kind
        else {
            panic!("expected a reasoning activity item");
        };
        assert!(activity.readable_parts_filtered);
        assert_eq!(
            activity.content_availability(),
            bcode_session_view_models::ReasoningContentAvailability::Filtered
        );
    }

    #[test]
    fn live_opaque_only_activity_reports_withheld() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for reasoning_event in [
            bcode_session_models::ReasoningActivityEvent::Started {
                activity_id: "reasoning-live".to_owned(),
                order: 0,
            },
            bcode_session_models::ReasoningActivityEvent::OpaqueObserved {
                activity_id: "reasoning-live".to_owned(),
                activity_order: 0,
            },
            bcode_session_models::ReasoningActivityEvent::Finished {
                activity_id: "reasoning-live".to_owned(),
                activity_order: 0,
                status: bcode_session_models::ReasoningActivityStatus::Completed,
            },
        ] {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantReasoningActivity {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    event: reasoning_event,
                },
            });
        }

        let TranscriptViewItemKind::ReasoningActivity { activity } =
            &view.snapshot().transcript.items[0].kind
        else {
            panic!("expected a reasoning activity item");
        };
        assert!(activity.opaque);
        assert!(!activity.readable_parts_filtered);
        assert_eq!(
            activity.content_availability(),
            bcode_session_view_models::ReasoningContentAvailability::Withheld
        );
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
    fn presentation_only_plugin_notes_do_not_accumulate_active_status() {
        let session_id = SessionId::new();
        let snapshot = build_session_view_snapshot(&[
            event(
                session_id,
                1,
                SessionEventKind::PluginStatusNote {
                    plugin_id: "bcode.host".to_owned(),
                    note_id: "notice-1".to_owned(),
                    text: "first".to_owned(),
                    metadata: BTreeMap::from([(
                        "presentation_only".to_owned(),
                        serde_json::Value::Bool(true),
                    )]),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::PluginStatusNote {
                    plugin_id: "bcode.host".to_owned(),
                    note_id: "notice-2".to_owned(),
                    text: "second".to_owned(),
                    metadata: BTreeMap::from([(
                        "presentation_only".to_owned(),
                        serde_json::Value::Bool(true),
                    )]),
                },
            ),
        ]);

        assert!(snapshot.plugin_status.is_empty());
        assert_eq!(snapshot.transcript.items.len(), 2);
        assert_eq!(
            snapshot.transcript.items[0].id,
            TranscriptViewItemId::event(1)
        );
        assert_eq!(
            snapshot.transcript.items[1].id,
            TranscriptViewItemId::event(2)
        );
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
                    metadata: BTreeMap::from([
                        ("iteration".to_owned(), serde_json::json!(2)),
                        ("format".to_owned(), serde_json::json!("markdown")),
                    ]),
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
        assert!(matches!(
            &status_items[0].kind,
            TranscriptViewItemKind::SystemMessage { message }
                if message.format == bcode_session_view_models::TextFormat::Markdown
        ));
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
    fn parallel_tools_keep_request_order_when_finishing_out_of_order() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for (sequence, invocation_id) in [(1, "call-a"), (2, "call-b")] {
            view.apply_event(&event(
                session_id,
                sequence,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: invocation_id.to_owned(),
                    producer_plugin_id: Some("test.plugin".to_owned()),
                    tool_name: "test.tool".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            ));
        }
        for (sequence, invocation_id) in [(3, "call-b"), (4, "call-a")] {
            view.apply_event(&event(
                session_id,
                sequence,
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: invocation_id.to_owned(),
                        model_output: format!("finished {invocation_id}"),
                        is_error: false,
                        presentation: None,
                        result: Some(ToolInvocationResult::Text {
                            text: format!("finished {invocation_id}"),
                        }),
                        content: Vec::new(),
                    },
                },
            ));
        }

        let ids = view
            .snapshot()
            .transcript
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                TranscriptViewItemId::tool("call-a"),
                TranscriptViewItemId::tool("call-b")
            ]
        );
        assert!(view.snapshot().transcript.items.iter().all(|item| {
            matches!(
                &item.kind,
                TranscriptViewItemKind::ToolInvocation { tool }
                    if tool.status == ToolInvocationViewStatus::Finished
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
    fn model_change_events_normalize_runtime_selection_for_renderers() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ModelChanged {
                provider: "<auto>".to_owned(),
                model: "<default>".to_owned(),
                selection_source: bcode_session_models::ModelSelectionSource::UserExplicit,
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
                selection_source: bcode_session_models::ModelSelectionSource::UserExplicit,
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
                    output_position: None,
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
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated {
                update: bcode_tool::ToolPresentationUpdate {
                    invocation_id: "call-presentation".to_owned(),
                    producer_id: "test.plugin".to_owned(),
                    generation: 0,
                    revision: 4,
                    identity: bcode_tool::ToolPresentationIdentity::Primary,
                    retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                    schema: "test.presentation".to_owned(),
                    schema_version: 1,
                    artifact: None,
                    payload: serde_json::json!({"status": "current"}),
                },
            },
        });

        view.rebuild_history_window(&[event(
            session_id,
            10,
            SessionEventKind::AssistantMessage {
                text: "durable".to_owned(),
            },
        )]);

        assert_eq!(view.tool_request_drafts().len(), 1);
        assert_eq!(view.snapshot().contributions.len(), 1);
        assert_eq!(
            view.presentation_update(
                "call-presentation",
                &bcode_tool::ToolPresentationIdentity::Primary,
            )
            .map(|update| (update.revision, update.payload.clone())),
            Some((4, serde_json::json!({"status": "current"})))
        );
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
    fn bounded_history_reset_then_active_update_preserves_one_primary_item() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let update = |revision| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated {
                update: bcode_tool::ToolPresentationUpdate {
                    invocation_id: "call-active".to_owned(),
                    producer_id: "test.plugin".to_owned(),
                    generation: 0,
                    revision,
                    identity: bcode_tool::ToolPresentationIdentity::Primary,
                    retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                    schema: "test.presentation".to_owned(),
                    schema_version: 1,
                    artifact: None,
                    payload: serde_json::json!({"revision": revision}),
                },
            },
        };
        view.apply_live_event(&update(1));
        view.set_history_window_metadata(Some(10), Some(10), true, false);
        view.rebuild_history_window(&[
            event(
                session_id,
                9,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-active".to_owned(),
                    producer_plugin_id: Some("test.plugin".to_owned()),
                    tool_name: "test.tool".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            ),
            event(
                session_id,
                10,
                SessionEventKind::AssistantMessage {
                    text: "bounded history".to_owned(),
                },
            ),
        ]);
        view.set_history_window_metadata(Some(10), Some(10), true, false);
        view.apply_live_event(&update(2));

        let snapshot = view.snapshot();
        let primary_id = TranscriptViewItemId::tool("call-active");
        assert_eq!(
            snapshot
                .transcript
                .items
                .iter()
                .filter(|item| item.id == primary_id)
                .count(),
            1
        );
        assert!(snapshot.transcript.has_older_history);
        assert!(snapshot.transcript.items.iter().any(|item| {
            matches!(
                &item.kind,
                TranscriptViewItemKind::ToolInvocation { tool }
                    if tool.presentation.as_ref().is_some_and(|presentation|
                        presentation.revision == 2
                            && presentation.payload == serde_json::json!({"revision": 2}))
            )
        }));
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
        view.set_history_window_metadata(Some(10), Some(10), true, true);
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
        assert_eq!(snapshot.transcript.source_start_sequence, Some(10));
        assert_eq!(snapshot.transcript.source_end_sequence, Some(10));
        assert!(snapshot.transcript.has_older_history);
        assert!(snapshot.transcript.has_newer_history);
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
            TranscriptViewItemKind::SystemMessage { message } if message.text == "status"
        )));
    }

    #[test]
    fn hydration_enriches_event_projected_interaction_without_losing_exchange_facts() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolExchangeRequested {
                request: bcode_session_models::ToolExchangeRequest {
                    invocation_id: "call-1".to_owned(),
                    exchange_id: "question-1".to_owned(),
                    producer_id: "bcode.question".to_owned(),
                    schema: "bcode.question.request".to_owned(),
                    schema_version: 1,
                    payload: serde_json::json!({"questions": []}),
                    response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                },
            },
        ));

        view.set_pending_interactions(vec![InteractionViewSummary {
            producer_id: Some("bcode.question".to_owned()),
            exchange_schema: Some("bcode.question.request".to_owned()),
            exchange_schema_version: Some(1),
            interaction_id: "question-1".to_owned(),
            kind: "bcode.question".to_owned(),
            tool_call_id: Some("call-1".to_owned()),
            title: Some("Question".to_owned()),
            required: true,
            snapshot: Some(serde_json::json!({"questions": []})),
            state: InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        }]);

        let interaction = &view.snapshot().interactions[0];
        assert_eq!(interaction.kind, "bcode.question");
        assert_eq!(interaction.producer_id.as_deref(), Some("bcode.question"));
        assert_eq!(
            interaction.exchange_schema.as_deref(),
            Some("bcode.question.request")
        );
        assert_eq!(interaction.exchange_schema_version, Some(1));
        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| item.id == TranscriptViewItemId::interaction("question-1"))
                .count(),
            1
        );
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
            producer_id: None,
            exchange_schema: None,
            exchange_schema_version: None,
            interaction_id: "question-1".to_owned(),
            kind: "bcode.question".to_owned(),
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
            producer_id: None,
            exchange_schema: None,
            exchange_schema_version: None,
            interaction_id: "question-1".to_owned(),
            kind: "bcode.question".to_owned(),
            tool_call_id: Some("call-1".to_owned()),
            title: Some("Question".to_owned()),
            required: true,
            snapshot: None,
            state: bcode_session_view_models::InteractionViewState::Resolved,
            status_detail: None,
            resolved: true,
            resolution: Some(bcode_session_models::ToolExchangeResolution::Responded {
                payload: serde_json::json!({"status": "answered"}),
            }),
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
                    && interaction.resolution
                        == Some(bcode_session_models::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({"status": "answered"}),
                        })
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
            producer_id: None,
            exchange_schema: None,
            exchange_schema_version: None,
            interaction_id: id.to_owned(),
            kind: "question".to_owned(),
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
                    output_position: None,
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
        assert_eq!(compact.id, TranscriptViewItemId::tool("call-1"));
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
                    presentation: None,
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
                    content: Vec::new(),
                },
            },
        ));

        let items = &view.snapshot().transcript.items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, compact.id);
        assert!(matches!(
            &items[0].kind,
            TranscriptViewItemKind::ToolContribution {
                invocation: Some(tool),
                contribution,
                ..
            } if tool.status == ToolInvocationViewStatus::Finished
                && contribution.schema == "test.request"
                && matches!(
                    tool.result,
                    Some(ToolResultView::Artifact { ref artifact, .. })
                        if artifact.artifact.schema == "test.change"
                )
        ));
    }

    #[test]
    fn explicit_supplemental_presentation_materializes_without_changing_primary_identity() {
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
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated {
                update: bcode_tool::ToolPresentationUpdate {
                    invocation_id: "call-1".to_owned(),
                    producer_id: "test.plugin".to_owned(),
                    generation: 0,
                    revision: 1,
                    identity: bcode_tool::ToolPresentationIdentity::Supplemental {
                        item_id: "details".to_owned(),
                    },
                    retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                    schema: "test.details".to_owned(),
                    schema_version: 1,
                    artifact: None,
                    payload: serde_json::json!({"details": true}),
                },
            },
        });

        assert_eq!(view.snapshot().transcript.items.len(), 2);
        assert_eq!(
            view.snapshot().transcript.items[0].id,
            TranscriptViewItemId::tool("call-1")
        );
        assert_eq!(
            view.snapshot().transcript.items[1].id,
            TranscriptViewItemId::tool_supplemental("call-1", "details")
        );
        assert!(matches!(
            &view.snapshot().transcript.items[1].kind,
            TranscriptViewItemKind::ToolContribution {
                contribution,
                placement: bcode_session_models::ToolContributionPlacement::Supplemental,
                ..
            } if contribution.schema == "test.details"
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
    fn primary_presentation_replaces_existing_tool_item_without_changing_identity() {
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
        let id = view.snapshot().transcript.items[0].id.clone();
        let revision = view.snapshot().transcript.items[0].revision;
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated {
                update: bcode_tool::ToolPresentationUpdate {
                    invocation_id: "call-1".to_owned(),
                    producer_id: "test.plugin".to_owned(),
                    generation: 0,
                    revision: 1,
                    identity: bcode_tool::ToolPresentationIdentity::Primary,
                    retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                    schema: "test.presentation".to_owned(),
                    schema_version: 1,
                    artifact: None,
                    payload: serde_json::json!({"current": true}),
                },
            },
        });

        assert_eq!(view.snapshot().transcript.items.len(), 1);
        let item = &view.snapshot().transcript.items[0];
        assert_eq!(item.id, id);
        assert!(item.revision > revision);
        assert!(matches!(
            &item.kind,
            TranscriptViewItemKind::ToolInvocation { tool }
                if tool.presentation.as_ref().is_some_and(|presentation|
                    presentation.payload == serde_json::json!({"current": true}))
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fixture keeps schema replacement and interleaved ordering in one transition sequence.
    fn presentation_schema_changes_and_interleaved_items_preserve_primary_identity_and_order() {
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
        let primary_id = TranscriptViewItemId::tool("call-1");
        let update = |revision, schema: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated {
                update: bcode_tool::ToolPresentationUpdate {
                    invocation_id: "call-1".to_owned(),
                    producer_id: "test.plugin".to_owned(),
                    generation: 0,
                    revision,
                    identity: bcode_tool::ToolPresentationIdentity::Primary,
                    retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                    schema: schema.to_owned(),
                    schema_version: 1,
                    artifact: None,
                    payload: serde_json::json!({"revision": revision}),
                },
            },
        };
        view.apply_live_event(&update(1, "test.request"));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_owned(),
                usage: bcode_session_models::SessionTokenUsage::default(),
            },
        ));
        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::AssistantMessage {
                text: "assistant interleave".to_owned(),
            },
        ));
        view.apply_event(&event(
            session_id,
            4,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-interleave".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![bcode_session_models::ReasoningPart {
                        part_id: "summary-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Summary,
                        role: bcode_session_models::ReasoningContentRole::Milestone,
                        order: 0,
                        text: "reasoning interleave".to_owned(),
                    }],
                    opaque: false,
                },
            },
        ));
        view.apply_event(&event(
            session_id,
            5,
            SessionEventKind::PermissionRequested {
                permission_id: "permission-1".to_owned(),
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("test.plugin".to_owned()),
                tool_name: "test.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                batch: None,
                policy_source: None,
                policy_reason: Some("approval required".to_owned()),
            },
        ));
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated {
                update: bcode_tool::ToolPresentationUpdate {
                    invocation_id: "call-1".to_owned(),
                    producer_id: "test.plugin".to_owned(),
                    generation: 0,
                    revision: 1,
                    identity: bcode_tool::ToolPresentationIdentity::Supplemental {
                        item_id: "details".to_owned(),
                    },
                    retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                    schema: "test.details".to_owned(),
                    schema_version: 1,
                    artifact: None,
                    payload: serde_json::json!({"details": true}),
                },
            },
        });
        let before = view
            .snapshot()
            .transcript
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();

        view.apply_live_event(&update(2, "test.result"));

        let after = &view.snapshot().transcript.items;
        assert_eq!(
            after.iter().map(|item| item.id.clone()).collect::<Vec<_>>(),
            before
        );
        assert_eq!(after.iter().filter(|item| item.id == primary_id).count(), 1);
        assert!(matches!(
            &after[0].kind,
            TranscriptViewItemKind::ToolInvocation { tool }
                if tool.presentation.as_ref().is_some_and(|presentation|
                    presentation.schema == "test.result" && presentation.revision == 2)
        ));
        assert!(
            after
                .iter()
                .any(|item| item.id == TranscriptViewItemId::event(3))
        );
        assert!(after.iter().any(|item| {
            matches!(
                &item.kind,
                TranscriptViewItemKind::ReasoningActivity { activity }
                    if activity.activity_id == "reasoning-interleave"
            )
        }));
        assert!(
            after
                .iter()
                .any(|item| item.id == TranscriptViewItemId::permission("permission-1"))
        );
        assert!(after.iter().any(|item| {
            item.id == TranscriptViewItemId::tool_supplemental("call-1", "details")
        }));
    }

    #[test]
    fn presentation_updates_are_monotonic_and_terminal_closure_is_absorbing() {
        let session_id = SessionId::new();
        let update = |revision, retention| bcode_tool::ToolPresentationUpdate {
            invocation_id: "call-1".to_owned(),
            producer_id: "test.plugin".to_owned(),
            generation: 0,
            revision,
            identity: bcode_tool::ToolPresentationIdentity::Primary,
            retention,
            schema: "test.presentation".to_owned(),
            schema_version: 1,
            artifact: None,
            payload: serde_json::json!({"revision": revision}),
        };
        let live = |update| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated { update },
        };
        let mut view = SessionView::new();
        view.apply_live_event(&live(update(
            1,
            bcode_tool::ToolPresentationRetention::RetainLatest,
        )));
        view.apply_live_event(&live(update(
            1,
            bcode_tool::ToolPresentationRetention::RetainLatest,
        )));
        assert_eq!(
            view.presentation_update("call-1", &bcode_tool::ToolPresentationIdentity::Primary)
                .map(|update| update.revision),
            Some(1)
        );
        view.apply_event(&event(
            session_id,
            1,
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
        view.apply_live_event(&live(update(
            2,
            bcode_tool::ToolPresentationRetention::RetainLatest,
        )));
        assert_eq!(
            view.presentation_update("call-1", &bcode_tool::ToolPresentationIdentity::Primary)
                .map(|update| update.revision),
            Some(1)
        );
    }

    #[test]
    fn terminal_checkpoint_wins_reconnect_races_without_mixed_live_state() {
        let session_id = SessionId::new();
        let update = |revision| bcode_tool::ToolPresentationUpdate {
            invocation_id: "call-race".to_owned(),
            producer_id: "test.plugin".to_owned(),
            generation: 0,
            revision,
            identity: bcode_tool::ToolPresentationIdentity::Primary,
            retention: bcode_tool::ToolPresentationRetention::RetainLatest,
            schema: "test.presentation".to_owned(),
            schema_version: 1,
            artifact: None,
            payload: serde_json::json!({"revision": revision}),
        };
        let live = |revision| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated {
                update: update(revision),
            },
        };
        let terminal = event(
            session_id,
            1,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-race".to_owned(),
                    model_output: "done".to_owned(),
                    is_error: false,
                    presentation: Some(update(2)),
                    result: Some(ToolInvocationResult::Text {
                        text: "done".to_owned(),
                    }),
                    content: Vec::new(),
                },
            },
        );

        let mut active_then_terminal = SessionView::new();
        active_then_terminal.apply_live_event(&live(1));
        active_then_terminal.apply_event(&terminal);
        active_then_terminal.apply_live_event(&live(3));

        let mut terminal_then_stale_checkpoint = SessionView::new();
        terminal_then_stale_checkpoint.apply_event(&terminal);
        terminal_then_stale_checkpoint.apply_live_event(&live(1));

        for view in [&active_then_terminal, &terminal_then_stale_checkpoint] {
            let tool = view
                .snapshot()
                .tools
                .get("call-race")
                .expect("terminal tool");
            assert_eq!(tool.status, ToolInvocationViewStatus::Finished);
            assert_eq!(
                tool.presentation
                    .as_ref()
                    .map(|presentation| presentation.revision),
                Some(2)
            );
            assert_eq!(
                view.snapshot()
                    .transcript
                    .items
                    .iter()
                    .filter(|item| item.id == TranscriptViewItemId::tool("call-race"))
                    .count(),
                1
            );
        }
        assert_eq!(
            active_then_terminal.snapshot().tools,
            terminal_then_stale_checkpoint.snapshot().tools
        );
    }

    #[test]
    fn active_only_presentation_is_removed_at_terminal_closure() {
        let session_id = SessionId::new();
        let update = bcode_tool::ToolPresentationUpdate {
            invocation_id: "call-1".to_owned(),
            producer_id: "test.plugin".to_owned(),
            generation: 0,
            revision: 1,
            identity: bcode_tool::ToolPresentationIdentity::Primary,
            retention: bcode_tool::ToolPresentationRetention::ActiveOnly,
            schema: "test.presentation".to_owned(),
            schema_version: 1,
            artifact: None,
            payload: serde_json::Value::Null,
        };
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
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated { update },
        });
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence: 1,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Failed,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        ));
        assert!(
            view.presentation_update("call-1", &bcode_tool::ToolPresentationIdentity::Primary)
                .is_none()
        );
        let primary_id = TranscriptViewItemId::tool("call-1");
        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| item.id == primary_id)
                .count(),
            1
        );
        assert!(matches!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .find(|item| item.id == primary_id)
                .map(|item| &item.kind),
            Some(TranscriptViewItemKind::ToolInvocation { tool })
                if tool.status == ToolInvocationViewStatus::Failed
        ));
    }

    #[test]
    fn supplemental_active_only_is_removed_while_retained_supplemental_survives_closure() {
        let session_id = SessionId::new();
        let update = |item_id: &str, retention| bcode_tool::ToolPresentationUpdate {
            invocation_id: "call-1".to_owned(),
            producer_id: "test.plugin".to_owned(),
            generation: 0,
            revision: 1,
            identity: bcode_tool::ToolPresentationIdentity::Supplemental {
                item_id: item_id.to_owned(),
            },
            retention,
            schema: "test.supplemental".to_owned(),
            schema_version: 1,
            artifact: None,
            payload: serde_json::json!({"item": item_id}),
        };
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
        for update in [
            update(
                "retained",
                bcode_tool::ToolPresentationRetention::RetainLatest,
            ),
            update("active", bcode_tool::ToolPresentationRetention::ActiveOnly),
        ] {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::ToolPresentationUpdated { update },
            });
        }
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "done".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "done".to_owned(),
                    }),
                    content: Vec::new(),
                },
            },
        ));

        assert!(view.snapshot().transcript.items.iter().any(|item| {
            item.id == TranscriptViewItemId::tool_supplemental("call-1", "retained")
        }));
        assert!(!view.snapshot().transcript.items.iter().any(|item| {
            item.id == TranscriptViewItemId::tool_supplemental("call-1", "active")
        }));
        assert!(
            view.presentation_update(
                "call-1",
                &bcode_tool::ToolPresentationIdentity::Supplemental {
                    item_id: "active".to_owned(),
                },
            )
            .is_none()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One historical stream proves primary replacement, supplemental identity, and hidden-state behavior together.
    fn historical_placement_events_project_chronologically_with_supplementals_and_hidden_state() {
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
            "hidden",
            4,
            bcode_session_models::ToolContributionPlacement::Hidden,
        ));
        view.apply_event(&contribution(
            "supplemental-one",
            5,
            bcode_session_models::ToolContributionPlacement::Supplemental,
        ));
        view.apply_event(&contribution(
            "supplemental-two",
            6,
            bcode_session_models::ToolContributionPlacement::Supplemental,
        ));
        let remove_replaced_request = event(
            session_id,
            7,
            SessionEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Request,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call-1".to_owned(),
                        contribution_id: "request-one".to_owned(),
                        sequence: 7,
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
        assert_eq!(items.len(), 3);
        let request_id = TranscriptViewItemId::tool("call-1");
        let request_revision = items
            .iter()
            .find(|item| item.id == request_id)
            .expect("request slot")
            .revision;
        assert_eq!(request_revision, 2);
        assert_eq!(items.iter().filter(|item| item.id == request_id).count(), 1);
        assert!(!items.iter().any(|item| {
            matches!(
                &item.kind,
                TranscriptViewItemKind::ToolContribution { contribution, .. }
                    if contribution.contribution_id == "hidden"
            )
        }));
        assert!(matches!(
            &items[0].kind,
            TranscriptViewItemKind::ToolContribution {
                contribution,
                placement: bcode_session_models::ToolContributionPlacement::Progress,
                ..
            } if contribution.contribution_id == "progress"
        ));
        assert_eq!(
            items[1].id,
            TranscriptViewItemId::tool_supplemental("call-1", "supplemental-one")
        );
        assert_eq!(
            items[2].id,
            TranscriptViewItemId::tool_supplemental("call-1", "supplemental-two")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One ordered stream fixture covers identity, ordering, gap, checkpoint, and terminal behavior.
    fn ordered_reasoning_stream_validates_identity_order_and_integrity() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let live = |part_id: &str,
                    activity_order,
                    part_order,
                    generation,
                    first_revision,
                    revision,
                    operation| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                output_position: None,
                turn_id: "turn-1".to_owned(),
                activity_id: "activity-1".to_owned(),
                activity_order,
                part_id: part_id.to_owned(),
                kind: bcode_session_models::ReasoningContentKind::Summary,
                role: bcode_session_models::ReasoningContentRole::Milestone,
                part_order,
                update: bcode_session_models::TextStreamUpdate {
                    generation,
                    first_revision,
                    revision,
                    operation,
                },
            },
        };
        view.apply_live_event(&live(
            "part-1",
            2,
            1,
            0,
            1,
            1,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 0,
                text: "second".to_owned(),
            },
        ));
        view.apply_live_event(&live(
            "part-0",
            2,
            0,
            0,
            1,
            1,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 0,
                text: "first".to_owned(),
            },
        ));
        view.apply_live_event(&live(
            "part-0",
            2,
            0,
            0,
            1,
            1,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 0,
                text: "first".to_owned(),
            },
        ));
        let item = &view.snapshot().transcript.items[0];
        assert_eq!(
            item.id,
            TranscriptViewItemId::reasoning("turn-1", "activity-1")
        );
        assert!(matches!(
            &item.kind,
            TranscriptViewItemKind::ReasoningActivity { activity }
                if activity.order == 2
                    && activity.parts[0].part_id == "part-0"
                    && activity.parts[0].text == "first"
                    && activity.parts[1].part_id == "part-1"
                    && activity.parts[1].text == "second"
        ));

        view.apply_live_event(&live(
            "part-0",
            2,
            0,
            0,
            1,
            1,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 0,
                text: "conflict".to_owned(),
            },
        ));
        let stream_id =
            TranscriptViewItemId::new("reasoning-stream:turn-1:activity:activity-1:part:part-0");
        assert!(matches!(
            view.snapshot().text_streams[&stream_id].status,
            TextStreamViewStatus::Degraded
        ));

        view.apply_live_event(&live(
            "part-0",
            2,
            0,
            0,
            3,
            3,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 5,
                text: " gap".to_owned(),
            },
        ));
        let stream_id =
            TranscriptViewItemId::new("reasoning-stream:turn-1:activity:activity-1:part:part-0");
        assert!(matches!(
            view.snapshot().text_streams[&stream_id].status,
            TextStreamViewStatus::Degraded
        ));
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::ReasoningActivity { activity }
                if activity.parts[0].text == "first"
        ));

        view.apply_live_event(&live(
            "part-0",
            2,
            0,
            0,
            4,
            4,
            bcode_session_models::TextStreamOperation::Checkpoint {
                start_offset: 3,
                text: "tail".to_owned(),
                total_bytes: 7,
                truncated: true,
            },
        ));
        assert!(matches!(
            view.snapshot().text_streams[&stream_id].status,
            TextStreamViewStatus::Incomplete
        ));
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::ReasoningActivity { activity }
                if activity.parts[0].text == "tail"
        ));

        view.apply_live_event(&live(
            "part-0",
            2,
            0,
            0,
            5,
            5,
            bcode_session_models::TextStreamOperation::Terminal {
                status: bcode_session_models::TextStreamTerminalStatus::Cancelled,
            },
        ));
        view.apply_live_event(&live(
            "part-0",
            2,
            0,
            0,
            6,
            6,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 7,
                text: " ignored".to_owned(),
            },
        ));
        view.apply_live_event(&live(
            "part-0",
            2,
            0,
            1,
            1,
            1,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 0,
                text: "new generation".to_owned(),
            },
        ));
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::ReasoningActivity { activity }
                if activity.parts[0].text == "tail"
        ));
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
    fn healthy_assistant_stream_preserves_exact_source_bytes_after_every_revision() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-bytes:segment:segment-0");
        let chunks = [
            "# Héllo\n",
            "- 世界\n",
            "```rust\n",
            "let crab = \"🦀\";\n",
            "```",
        ];
        let mut expected = String::new();
        let mut view = SessionView::new();

        for (index, chunk) in chunks.iter().enumerate() {
            let revision = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                    output_position: None,
                    turn_id: "turn-bytes".to_owned(),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: revision,
                        revision,
                        operation: bcode_session_models::TextStreamOperation::Append {
                            expected_offset: expected.len(),
                            text: (*chunk).to_owned(),
                        },
                    },
                },
            });
            expected.push_str(chunk);
            assert_eq!(
                transcript_item_text(view.snapshot(), &id),
                Some(expected.as_str())
            );
            assert_eq!(
                view.snapshot().text_streams[&id].accepted_bytes,
                expected.len()
            );
            assert_eq!(
                view.snapshot().text_streams[&id].status,
                TextStreamViewStatus::Healthy
            );
        }
    }

    #[test]
    fn healthy_reasoning_stream_preserves_exact_source_bytes_after_every_revision() {
        let session_id = SessionId::new();
        let stream_id = TranscriptViewItemId::new(
            "reasoning-stream:turn-bytes:activity:activity-0:part:part-0",
        );
        let item_id = TranscriptViewItemId::reasoning("turn-bytes", "activity-0");
        let chunks = ["Résumé ", "世界", "\n\n**done** 🦀"];
        let mut expected = String::new();
        let mut view = SessionView::new();

        for (index, chunk) in chunks.iter().enumerate() {
            let revision = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                    output_position: None,
                    turn_id: "turn-bytes".to_owned(),
                    activity_id: "activity-0".to_owned(),
                    activity_order: 0,
                    part_id: "part-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    part_order: 0,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: revision,
                        revision,
                        operation: bcode_session_models::TextStreamOperation::Append {
                            expected_offset: expected.len(),
                            text: (*chunk).to_owned(),
                        },
                    },
                },
            });
            expected.push_str(chunk);
            let item = view
                .snapshot()
                .transcript
                .items
                .iter()
                .find(|item| item.id == item_id)
                .expect("reasoning item");
            assert_reasoning_text(item, &expected, true);
            assert_eq!(
                view.snapshot().text_streams[&stream_id].accepted_bytes,
                expected.len()
            );
            assert_eq!(
                view.snapshot().text_streams[&stream_id].status,
                TextStreamViewStatus::Healthy
            );
        }
    }

    #[test]
    fn ordered_assistant_stream_validates_offsets_duplicates_checkpoints_and_terminal_state() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let update = |revision, operation| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation: 0,
                    first_revision: revision,
                    revision,
                    operation,
                },
            },
        };
        let append = |revision, expected_offset, text: &str| {
            update(
                revision,
                bcode_session_models::TextStreamOperation::Append {
                    expected_offset,
                    text: text.to_owned(),
                },
            )
        };
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-0");

        view.apply_live_event(&append(1, 0, "abc"));
        view.apply_live_event(&append(2, 3, "def"));
        view.apply_live_event(&append(2, 3, "def"));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("abcdef"));
        assert_eq!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Healthy
        );

        view.apply_live_event(&append(2, 3, "conflict"));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("abcdef"));
        assert_eq!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Degraded
        );

        view.apply_live_event(&append(4, 6, "gap"));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("abcdef"));
        view.apply_live_event(&update(
            5,
            bcode_session_models::TextStreamOperation::Checkpoint {
                start_offset: 0,
                text: "authoritative".to_owned(),
                total_bytes: 13,
                truncated: false,
            },
        ));
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("authoritative")
        );
        assert_eq!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Healthy
        );
        view.apply_live_event(&update(
            6,
            bcode_session_models::TextStreamOperation::Checkpoint {
                start_offset: 8,
                text: "suffix".to_owned(),
                total_bytes: 14,
                truncated: true,
            },
        ));
        assert_eq!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Incomplete
        );

        view.apply_live_event(&update(
            7,
            bcode_session_models::TextStreamOperation::Terminal {
                status: bcode_session_models::TextStreamTerminalStatus::Cancelled,
            },
        ));
        view.apply_live_event(&append(8, 14, "late"));
        assert_eq!(transcript_item_text(view.snapshot(), &id), Some("suffix"));
        assert_eq!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Terminal(
                bcode_session_models::TextStreamTerminalStatus::Cancelled
            )
        );
    }

    #[test]
    fn ordered_assistant_stream_targets_exact_segment_identity() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for (segment_order, text) in [(0, "before"), (1, "after")] {
            view.apply_live_event(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    segment_id: format!("segment-{segment_order}"),
                    segment_order,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: 1,
                        revision: 1,
                        operation: bcode_session_models::TextStreamOperation::Append {
                            expected_offset: 0,
                            text: text.to_owned(),
                        },
                    },
                },
            });
        }

        let items = &view.snapshot().transcript.items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id.get(), "assistant-turn:turn-1:segment:segment-0");
        assert_eq!(items[1].id.get(), "assistant-turn:turn-1:segment:segment-1");
        assert_eq!(
            transcript_item_text(view.snapshot(), &items[0].id),
            Some("before")
        );
        assert_eq!(
            transcript_item_text(view.snapshot(), &items[1].id),
            Some("after")
        );
    }

    /// Canonical durable text must repair a degraded live stream.
    ///
    /// This previously asserted the opposite: a degraded stream stayed truncated forever because
    /// the durable segment was refused. That concealed the real outcome, since dropped live events
    /// (the live broker is a bounded broadcast channel) left renderers showing text cut off at the
    /// point delivery was lost, even though the complete text was persisted.
    ///
    /// The event store is canonical and `text_streams` is derived, disposable presentation state,
    /// so authoritative content wins. A live `Checkpoint` already repairs a degraded stream the
    /// same way; this brings the durable segment in line with that precedent.
    #[test]
    fn durable_segment_repairs_a_degraded_live_stream() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-0");
        let mut view = SessionView::new();
        let live = |revision, text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation: 0,
                    first_revision: revision,
                    revision,
                    operation: bcode_session_models::TextStreamOperation::Append {
                        expected_offset: 0,
                        text: text.to_owned(),
                    },
                },
            },
        };

        view.apply_live_event(&live(1, "healthy bytes"));
        view.apply_live_event(&live(1, "conflicting bytes"));
        assert_eq!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Degraded
        );

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::AssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "durable replacement".to_owned(),
            },
        ));

        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("durable replacement")
        );
        assert_eq!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Terminal(
                bcode_session_models::TextStreamTerminalStatus::Completed
            )
        );
        assert_eq!(view.snapshot().text_streams[&id].accepted_bytes, 19);
        assert!(!view.snapshot().text_streams[&id].truncated);
    }

    /// Reproduces the reported truncation: lost live deltas must not survive the durable event.
    ///
    /// A gap in `first_revision` degrades the stream mid-response, which is what happens when the
    /// bounded live broadcast channel drops events during a long turn. The canonical segment must
    /// still render in full.
    #[test]
    fn dropped_live_deltas_do_not_leave_truncated_text_after_durable_segment() {
        let session_id = SessionId::new();
        let id = TranscriptViewItemId::new("assistant-turn:turn-1:segment:segment-0");
        let mut view = SessionView::new();
        let append = |revision: u64, expected_offset: usize, text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation: 0,
                    first_revision: revision,
                    revision,
                    operation: bcode_session_models::TextStreamOperation::Append {
                        expected_offset,
                        text: text.to_owned(),
                    },
                },
            },
        };

        view.apply_live_event(&append(1, 0, "the beginning "));
        // Revisions 2..=9 were dropped by the bounded live channel, so this arrives with a gap.
        view.apply_live_event(&append(10, 14, "the tail"));
        assert_eq!(
            view.snapshot().text_streams[&id].status,
            TextStreamViewStatus::Degraded,
            "a revision gap must degrade the stream"
        );
        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("the beginning "),
            "degraded live text is truncated at the point delivery was lost"
        );

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::AssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "the beginning the middle the tail".to_owned(),
            },
        ));

        assert_eq!(
            transcript_item_text(view.snapshot(), &id),
            Some("the beginning the middle the tail"),
            "the canonical durable segment must render in full"
        );
    }

    #[test]
    fn assistant_stream_keeps_live_identity_when_durable_segment_finishes_it() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "live answer".to_owned(),
            },
        });
        let live_id = view.snapshot().transcript.items[0].id.clone();
        assert_eq!(live_id.get(), "assistant-turn:turn-1:segment:segment-0");

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
            SessionEventKind::AssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "durable answer".to_owned(),
            },
        ));

        assert_eq!(view.snapshot().transcript.items.len(), 1);
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
    fn historical_assistant_and_reasoning_replay_matches_bounded_rebuild() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::AssistantDelta {
                    text: "legacy answer ".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::AssistantMessage {
                    text: "legacy answer complete".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::AssistantReasoningDelta {
                    text: "legacy thought ".to_owned(),
                },
            ),
            event(
                session_id,
                4,
                SessionEventKind::AssistantReasoningMessage {
                    text: "legacy thought complete".to_owned(),
                },
            ),
            event(
                session_id,
                5,
                SessionEventKind::AssistantReasoningActivity {
                    turn_id: "turn-1".to_owned(),
                    activity: bcode_session_models::ReasoningActivity {
                        activity_id: "reasoning-1".to_owned(),
                        order: 0,
                        status: bcode_session_models::ReasoningActivityStatus::Completed,
                        parts: vec![bcode_session_models::ReasoningPart {
                            part_id: "summary-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Summary,
                            role: bcode_session_models::ReasoningContentRole::Milestone,
                            order: 0,
                            text: "structured thought".to_owned(),
                        }],
                        opaque: false,
                    },
                },
            ),
            event(
                session_id,
                6,
                SessionEventKind::AssistantResponseSegment {
                    turn_id: "turn-1".to_owned(),
                    segment_id: "segment-1".to_owned(),
                    segment_order: 1,
                    text: "structured answer".to_owned(),
                },
            ),
        ];

        let fresh = build_session_view_snapshot(&events);
        let mut rebuilt = SessionView::new();
        rebuilt.apply_history(&events);
        rebuilt.rebuild_history_window(&events);
        let rebuilt = rebuilt.into_snapshot();
        let projection = |snapshot: &SessionViewSnapshot| {
            snapshot
                .transcript
                .items
                .iter()
                .map(|item| {
                    (
                        item.id.clone(),
                        item.sequence,
                        item.streaming,
                        item.kind.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(projection(&fresh), projection(&rebuilt));
        assert_eq!(
            fresh
                .transcript
                .items
                .iter()
                .map(|item| item.id.get())
                .collect::<Vec<_>>(),
            [
                "event:1",
                "event:3",
                "reasoning-turn:turn-1:reasoning-1",
                "assistant-turn:turn-1:segment:segment-1",
            ]
        );
        assert_reasoning_text(&fresh.transcript.items[1], "legacy thought complete", false);
        assert_reasoning_text(&fresh.transcript.items[2], "structured thought", false);
    }

    #[test]
    fn durable_assistant_segments_replay_with_stable_distinct_identities() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for (sequence, segment_order, text) in [(4, 0, "before tool"), (9, 1, "after tool")] {
            view.apply_event(&event(
                session_id,
                sequence,
                SessionEventKind::AssistantResponseSegment {
                    turn_id: "turn-1".to_owned(),
                    segment_id: format!("segment-{segment_order}"),
                    segment_order,
                    text: text.to_owned(),
                },
            ));
        }

        let items = &view.snapshot().transcript.items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id.get(), "assistant-turn:turn-1:segment:segment-0");
        assert_eq!(items[1].id.get(), "assistant-turn:turn-1:segment:segment-1");
        assert_ne!(items[0].id, items[1].id);
    }

    #[test]
    fn bounded_replay_segment_identity_needs_no_earlier_turn_context() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            9,
            SessionEventKind::AssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-1".to_owned(),
                segment_order: 1,
                text: "bounded later segment".to_owned(),
            },
        ));

        let item = &view.snapshot().transcript.items[0];
        assert_eq!(item.id.get(), "assistant-turn:turn-1:segment:segment-1");
        assert_eq!(item.sequence, Some(9));
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
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
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
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
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
    fn result_terminal_rejects_late_lifecycle_and_presentation_revival() {
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
                    model_output: "done".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: None,
                    content: Vec::new(),
                },
            },
        ));
        let terminal = view.snapshot().tools["call-1"].clone();
        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence: 3,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Progress,
                    message: Some("late".to_owned()),
                    metadata: serde_json::Value::Null,
                },
            },
        ));
        view.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolPresentationUpdated {
                update: bcode_tool::ToolPresentationUpdate {
                    invocation_id: "call-1".to_owned(),
                    producer_id: "test.plugin".to_owned(),
                    generation: 0,
                    revision: 1,
                    identity: bcode_tool::ToolPresentationIdentity::Primary,
                    retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                    schema: "test.presentation".to_owned(),
                    schema_version: 1,
                    artifact: None,
                    payload: serde_json::json!({"late": true}),
                },
            },
        });

        assert_eq!(view.snapshot().tools["call-1"], terminal);
        assert!(view.snapshot().active_invocations.is_empty());
        assert!(
            view.presentation_update("call-1", &bcode_tool::ToolPresentationIdentity::Primary)
                .is_none()
        );
    }

    #[test]
    fn presentation_free_terminal_result_retains_legacy_model_output_fallback() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "legacy-tool".to_owned(),
                producer_plugin_id: None,
                tool_name: "legacy.tool".to_owned(),
                arguments_json: r#"{"target":"fixture"}"#.to_owned(),
                working_directory: None,
            },
        ));
        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "legacy-tool".to_owned(),
                    model_output: "legacy imported output".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: None,
                    content: Vec::new(),
                },
            },
        ));

        let tool = view
            .snapshot()
            .tools
            .get("legacy-tool")
            .expect("legacy tool");
        assert_eq!(tool.status, ToolInvocationViewStatus::Finished);
        assert_eq!(tool.presentation, None);
        assert_eq!(tool.result, None);
        assert_eq!(tool.result_text.as_deref(), Some("legacy imported output"));
        assert!(matches!(
            &view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::ToolInvocation { tool }
                if tool.result_text.as_deref() == Some("legacy imported output")
                    && tool.presentation.is_none()
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
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
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
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "durable output".to_owned(),
                    }),
                    content: Vec::new(),
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
        view.apply_event(&event(session_id, 8, started(cancelled, "late cancelled")));
        assert_eq!(view.snapshot().runtime_work.len(), 1);
        assert_eq!(view.snapshot().runtime_work[0].work_id, second);
    }

    #[test]
    fn terminal_runtime_work_without_visible_start_leaves_no_history() {
        let session_id = SessionId::new();
        let work_id = bcode_session_models::WorkId::new("work-terminal-only");
        let mut view = SessionView::new();

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status: bcode_session_models::RuntimeWorkStatus::Completed,
                finished_at_ms: Some(10),
                message: Some("complete".to_owned()),
            },
        ));

        assert!(view.snapshot().runtime_work.is_empty());
        assert!(view.snapshot().transcript.items.is_empty());
    }

    #[test]
    fn runtime_work_updates_active_collection_without_transcript_item() {
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
        assert!(snapshot.transcript.items.is_empty());
    }

    #[test]
    fn model_usage_updates_runtime_accounting_without_transcript_mutation() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::SystemMessage {
                text: "before usage".to_owned(),
            },
        ));
        let transcript_before = view.snapshot().transcript.clone();

        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_owned(),
                usage: bcode_session_models::SessionTokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_tokens: Some(15),
                    ..SessionTokenUsage::default()
                },
            },
        ));

        assert_eq!(view.snapshot().transcript, transcript_before);
        assert_eq!(view.snapshot().runtime.cumulative_metered_tokens, 15);
        assert_eq!(
            view.snapshot()
                .runtime
                .latest_usage
                .as_ref()
                .and_then(bcode_session_models::SessionTokenUsage::metered_total_tokens),
            Some(15)
        );
    }

    #[test]
    fn runtime_work_updates_active_state_without_transcript_mutation() {
        let session_id = SessionId::new();
        let work_id = bcode_session_models::WorkId::new("work-state-only");
        let mut view = SessionView::new();
        let transcript_before = view.snapshot().transcript.clone();

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::RuntimeWorkStarted {
                work_id: work_id.clone(),
                kind: bcode_session_models::RuntimeWorkKind::Tool,
                label: "state-only work".to_owned(),
                tool_call_id: Some("call-1".to_owned()),
                plugin_id: Some("test.plugin".to_owned()),
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(1),
                cancellable: true,
            },
        ));
        assert_eq!(view.snapshot().transcript, transcript_before);
        assert_eq!(view.snapshot().runtime_work.len(), 1);

        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status: bcode_session_models::RuntimeWorkStatus::Completed,
                finished_at_ms: Some(2),
                message: Some("done".to_owned()),
            },
        ));
        assert_eq!(view.snapshot().transcript, transcript_before);
        assert!(view.snapshot().runtime_work.is_empty());
    }

    #[test]
    fn tool_runtime_work_correlates_without_creating_a_second_transcript_item() {
        let session_id = SessionId::new();
        let tool_call_id = "call-runtime-correlation";
        let work_id = bcode_session_models::WorkId::new("work-runtime-correlation");
        let mut view = SessionView::new();

        view.apply_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: tool_call_id.to_owned(),
                producer_plugin_id: Some("test.plugin".to_owned()),
                tool_name: "test.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        let initial_item = view.snapshot().transcript.items[0].clone();
        assert_eq!(initial_item.id, TranscriptViewItemId::tool(tool_call_id));

        view.apply_event(&event(
            session_id,
            2,
            SessionEventKind::RuntimeWorkStarted {
                work_id: work_id.clone(),
                kind: bcode_session_models::RuntimeWorkKind::Tool,
                label: "test.tool".to_owned(),
                tool_call_id: Some(tool_call_id.to_owned()),
                plugin_id: Some("test.plugin".to_owned()),
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(2),
                cancellable: true,
            },
        ));
        view.apply_event(&event(
            session_id,
            3,
            SessionEventKind::RuntimeWorkProgress {
                work_id: work_id.clone(),
                message: "halfway".to_owned(),
                completed_units: Some(1),
                total_units: Some(2),
                progress_at_ms: Some(3),
            },
        ));

        assert_eq!(
            view.snapshot().transcript.items,
            std::slice::from_ref(&initial_item)
        );
        assert_eq!(view.snapshot().runtime_work.len(), 1);

        view.apply_event(&event(
            session_id,
            4,
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status: bcode_session_models::RuntimeWorkStatus::Completed,
                finished_at_ms: Some(4),
                message: Some("done".to_owned()),
            },
        ));
        assert_eq!(
            view.snapshot().transcript.items,
            std::slice::from_ref(&initial_item)
        );
        assert!(view.snapshot().runtime_work.is_empty());

        view.apply_event(&event(
            session_id,
            5,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: tool_call_id.to_owned(),
                    model_output: "done".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(bcode_session_models::ToolInvocationResult::Text {
                        text: "done".to_owned(),
                    }),
                    content: Vec::new(),
                },
            },
        ));

        let matching_items = view
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| item.id == initial_item.id)
            .collect::<Vec<_>>();
        assert_eq!(matching_items.len(), 1);
        assert!(matching_items[0].revision > initial_item.revision);
        assert!(matches!(
            &matching_items[0].kind,
            TranscriptViewItemKind::ToolInvocation { tool }
                if tool.tool_call_id == tool_call_id
                    && tool.status == ToolInvocationViewStatus::Finished
                    && tool.result_text.as_deref() == Some("done")
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
    #[allow(clippy::too_many_lines)] // One compact fixture covers system, user, and different-turn ordering barriers.
    fn positioned_ordering_stops_at_unpositioned_and_different_turn_boundaries() {
        let mut view = SessionView::new();
        let high = TranscriptViewItemId::new("high");
        let system = TranscriptViewItemId::new("system");
        let low = TranscriptViewItemId::new("low");
        let other_turn = TranscriptViewItemId::new("other-turn");
        let earliest = TranscriptViewItemId::new("earliest");
        let user = TranscriptViewItemId::new("user");
        let after_user = TranscriptViewItemId::new("after-user");

        view.push_item(
            high.clone(),
            1,
            Some(10),
            false,
            TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown("high"),
            },
        );
        view.assign_output_location(
            &high,
            "turn-1",
            Some(bcode_session_models::TurnOutputPosition::new(2)),
        );
        view.push_item(
            system.clone(),
            2,
            Some(20),
            false,
            TranscriptViewItemKind::SystemMessage {
                message: ChatMessageView::markdown("barrier"),
            },
        );
        view.push_item(
            low.clone(),
            3,
            Some(30),
            false,
            TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown("low"),
            },
        );
        view.assign_output_location(
            &low,
            "turn-1",
            Some(bcode_session_models::TurnOutputPosition::new(1)),
        );
        view.push_item(
            other_turn.clone(),
            4,
            Some(40),
            false,
            TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown("other"),
            },
        );
        view.assign_output_location(
            &other_turn,
            "turn-2",
            Some(bcode_session_models::TurnOutputPosition::new(0)),
        );
        view.push_item(
            earliest.clone(),
            5,
            Some(50),
            false,
            TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown("earliest"),
            },
        );
        view.assign_output_location(
            &earliest,
            "turn-1",
            Some(bcode_session_models::TurnOutputPosition::new(0)),
        );
        view.push_item(
            user.clone(),
            6,
            Some(60),
            false,
            TranscriptViewItemKind::UserMessage {
                message: ChatMessageView::markdown("user barrier"),
            },
        );
        view.push_item(
            after_user.clone(),
            7,
            Some(70),
            false,
            TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown("after user"),
            },
        );
        view.assign_output_location(
            &after_user,
            "turn-1",
            Some(bcode_session_models::TurnOutputPosition::new(0)),
        );

        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            vec![high, system, low, other_turn, earliest, user, after_user]
        );
    }

    #[test]
    fn equal_output_positions_preserve_canonical_order() {
        let mut view = SessionView::new();
        for (sequence, id) in [(1, "first"), (2, "second"), (3, "third")] {
            let id = TranscriptViewItemId::new(id);
            view.push_item(
                id.clone(),
                sequence,
                Some(sequence * 10),
                false,
                TranscriptViewItemKind::AssistantMessage {
                    message: ChatMessageView::markdown(id.get()),
                },
            );
            view.assign_output_location(
                &id,
                "turn-1",
                Some(bcode_session_models::TurnOutputPosition::new(1)),
            );
        }

        assert_eq!(
            view.snapshot()
                .transcript
                .items
                .iter()
                .map(|item| item.id.get())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
    }

    #[test]
    fn live_finalization_and_durable_replay_preserve_system_message_barrier() {
        let session_id = SessionId::new();
        let turn_id = "turn-barrier";
        let durable = [
            event(
                session_id,
                1,
                SessionEventKind::PositionedAssistantResponseSegment {
                    turn_id: turn_id.to_owned(),
                    output_position: bcode_session_models::TurnOutputPosition::new(2),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    text: "answer".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::SystemMessage {
                    text: "status".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::PositionedAssistantReasoningActivity {
                    turn_id: turn_id.to_owned(),
                    output_position: bcode_session_models::TurnOutputPosition::new(0),
                    activity: bcode_session_models::ReasoningActivity {
                        activity_id: "reasoning-0".to_owned(),
                        order: 0,
                        status: bcode_session_models::ReasoningActivityStatus::Completed,
                        parts: vec![bcode_session_models::ReasoningPart {
                            part_id: "summary-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Summary,
                            role: bcode_session_models::ReasoningContentRole::Milestone,
                            order: 0,
                            text: "reasoning".to_owned(),
                        }],
                        opaque: false,
                    },
                },
            ),
        ];
        let mut live_then_durable = SessionView::new();
        live_then_durable.apply_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: Some(bcode_session_models::TurnOutputPosition::new(2)),
                turn_id: turn_id.to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation: 0,
                    first_revision: 1,
                    revision: 1,
                    operation: bcode_session_models::TextStreamOperation::Append {
                        expected_offset: 0,
                        text: "answer".to_owned(),
                    },
                },
            },
        });
        live_then_durable.apply_history(&durable);
        let mut replayed = SessionView::new();
        replayed.apply_history(&durable);
        let document = |view: &SessionView| {
            view.snapshot()
                .transcript
                .items
                .iter()
                .map(|item| {
                    (
                        item.id.clone(),
                        item.output_location.clone(),
                        item.kind.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(document(&live_then_durable), document(&replayed));
        assert!(matches!(
            replayed.snapshot().transcript.items[1].kind,
            TranscriptViewItemKind::SystemMessage { .. }
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One scenario proves live ordering, durable handoff, and rebuild convergence.
    fn positioned_live_outputs_use_cross_type_order_not_arrival_order() {
        let session_id = SessionId::new();
        let turn_id = "turn-positioned";
        let mut view = SessionView::new();
        let live = |kind| SessionLiveEvent { session_id, kind };

        view.apply_live_event(&live(SessionLiveEventKind::AssistantTextStreamUpdated {
            output_position: Some(bcode_session_models::TurnOutputPosition::new(2)),
            turn_id: turn_id.to_owned(),
            segment_id: "segment-0".to_owned(),
            segment_order: 0,
            update: bcode_session_models::TextStreamUpdate {
                generation: 0,
                first_revision: 1,
                revision: 1,
                operation: bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "answer".to_owned(),
                },
            },
        }));
        view.apply_live_event(&live(SessionLiveEventKind::ToolRequestDraft {
            event: bcode_session_models::ToolRequestDraftEvent {
                output_position: Some(bcode_session_models::TurnOutputPosition::new(1)),
                turn_id: turn_id.to_owned(),
                tool_call_id: "call-1".to_owned(),
                tool_name: "filesystem.write".to_owned(),
                producer_plugin_id: None,
                schema: "bcode.tool.request-draft".to_owned(),
                schema_version: 0,
                placement: bcode_session_models::ToolContributionPlacement::Request,
                generation: 1,
                revision: 1,
                operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                    start_offset: 0,
                    text: r#"{"path":"src/lib.rs"}"#.to_owned(),
                },
                argument_bytes: 21,
                truncated: false,
            },
        }));
        view.apply_live_event(&live(SessionLiveEventKind::AssistantReasoningActivity {
            output_position: Some(bcode_session_models::TurnOutputPosition::new(0)),
            turn_id: turn_id.to_owned(),
            event: bcode_session_models::ReasoningActivityEvent::PartDelta {
                activity_id: "reasoning-1".to_owned(),
                activity_order: 0,
                part_id: "summary-0".to_owned(),
                kind: bcode_session_models::ReasoningContentKind::Summary,
                role: bcode_session_models::ReasoningContentRole::Milestone,
                part_order: 0,
                text: "reasoning".to_owned(),
            },
        }));

        let locations = view
            .snapshot()
            .transcript
            .items
            .iter()
            .map(|item| {
                let location = item.output_location.as_ref().expect("positioned item");
                assert_eq!(location.turn_id, turn_id);
                location.position.get()
            })
            .collect::<Vec<_>>();
        assert_eq!(locations, vec![0, 1, 2]);
        assert!(matches!(
            view.snapshot().transcript.items[0].kind,
            TranscriptViewItemKind::ReasoningActivity { .. }
        ));
        assert_eq!(
            view.snapshot().transcript.items[1].id,
            TranscriptViewItemId::tool("call-1")
        );
        assert!(matches!(
            view.snapshot().transcript.items[2].kind,
            TranscriptViewItemKind::AssistantMessage { .. }
        ));

        let live_items = view
            .snapshot()
            .transcript
            .items
            .iter()
            .map(|item| (item.id.clone(), item.revision))
            .collect::<Vec<_>>();
        let complete_reasoning = bcode_session_models::ReasoningActivity {
            activity_id: "reasoning-1".to_owned(),
            order: 0,
            status: bcode_session_models::ReasoningActivityStatus::Completed,
            parts: vec![bcode_session_models::ReasoningPart {
                part_id: "summary-0".to_owned(),
                kind: bcode_session_models::ReasoningContentKind::Summary,
                role: bcode_session_models::ReasoningContentRole::Milestone,
                order: 0,
                text: "reasoning".to_owned(),
            }],
            opaque: false,
        };
        let durable = [
            event(
                session_id,
                1,
                SessionEventKind::PositionedAssistantResponseSegment {
                    turn_id: turn_id.to_owned(),
                    output_position: bcode_session_models::TurnOutputPosition::new(2),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    text: "answer".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::PositionedToolCallRequested {
                    turn_id: turn_id.to_owned(),
                    output_position: bcode_session_models::TurnOutputPosition::new(1),
                    tool_call_id: "call-1".to_owned(),
                    producer_plugin_id: None,
                    tool_name: "filesystem.write".to_owned(),
                    arguments_json: r#"{"path":"src/lib.rs"}"#.to_owned(),
                    working_directory: None,
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::PositionedAssistantReasoningActivity {
                    turn_id: turn_id.to_owned(),
                    output_position: bcode_session_models::TurnOutputPosition::new(0),
                    activity: complete_reasoning,
                },
            ),
        ];
        view.apply_history(&durable);
        assert_eq!(view.snapshot().transcript.items.len(), 3);
        for ((live_id, live_revision), durable_item) in
            live_items.iter().zip(&view.snapshot().transcript.items)
        {
            assert_eq!(live_id, &durable_item.id);
            assert!(durable_item.revision > *live_revision);
        }

        let mut rebuilt = SessionView::new();
        rebuilt.apply_history(&durable);
        let semantic_document = |view: &SessionView| {
            view.snapshot()
                .transcript
                .items
                .iter()
                .map(|item| {
                    (
                        item.id.clone(),
                        item.output_location.clone(),
                        item.kind.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(semantic_document(&view), semantic_document(&rebuilt));
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
