use std::time::{Duration, Instant};

use bcode_session_models::{
    ModelTurnOutcome, SessionEvent, SessionEventKind, SessionId, SessionLiveEvent,
    SessionLiveEventKind, TextStreamOperation, TextStreamTerminalStatus, TextStreamUpdate,
};
use bcode_session_view::SessionView;
use bcode_session_view_models::{
    StreamingInterpolationCurve, StreamingPresentationPolicy, TranscriptViewItemKind,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseEvent};
use bmux_tui::geometry::Rect;
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowOutcome, ActionRowState};
use bmux_tui_components::checkbox::{Checkbox, CheckboxOutcome, CheckboxState};

const TURN_ID: &str = "streaming-configurator-turn";
const SEGMENT_ID: &str = "streaming-configurator-segment";
const COMPLETED_HOLD: Duration = Duration::from_millis(1_500);

const SAMPLE_CHUNKS: &[(u64, &str)] = &[
    (350, "# Bursty streaming\n\n"),
    (375, "A"),
    (390, " provider"),
    (405, " can deliver"),
    (700, " several words together,"),
    (715, " then"),
    (730, " pause."),
    (
        780,
        "\n\n**Smoothing** keeps the accepted text monotonic while ",
    ),
    (900, "Unicode stays intact: "),
    (930, "cafe\u{301}, "),
    (960, "👩🏽‍💻, "),
    (1_000, "and 東京."),
    (
        1_750,
        "\n\nThe final burst arrives after one conspicuous provider gap.",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackPhase {
    Running,
    Paused,
    Completed,
}

/// Configurator setting row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingConfiguratorFocus {
    /// Progressive presentation enabled toggle.
    Enabled,
    /// Interpolation curve.
    Curve,
    /// Nominal grapheme rate.
    GraphemesPerSecond,
    /// Maximum accepted-text backlog age.
    MaxLag,
    /// Apply declarative fallback by clearing the override.
    Reset,
}

impl StreamingConfiguratorFocus {
    const ALL: [Self; 5] = [
        Self::Enabled,
        Self::Curve,
        Self::GraphemesPerSecond,
        Self::MaxLag,
        Self::Reset,
    ];
}

/// Result of handling one configurator key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingConfiguratorOutcome {
    /// State changed and the surface remains open.
    Handled,
    /// Apply the selected override and close.
    Apply(StreamingPresentationPolicy),
    /// Clear the override, apply the declarative fallback, and close.
    Reset,
    /// Close without changing active or persisted policy.
    Cancel,
    /// The key is not owned by the configurator.
    Ignored,
}

/// Committed geometry for component-owned configurator controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamingConfiguratorGeometry {
    /// Enabled checkbox area.
    pub enabled: Rect,
    /// Curve action row area.
    pub curve: Rect,
    /// Rate decrement/increment action row area.
    pub rate: Rect,
    /// Lag decrement/increment action row area.
    pub lag: Rect,
    /// Reset/apply/cancel action row area.
    pub outcomes: Rect,
    /// Complete opaque surface area used to capture otherwise-ignored mouse events.
    pub surface: Rect,
}

/// Complete local state for the streaming configurator surface.
pub struct StreamingConfiguratorState {
    controller: StreamingPreviewController,
    override_policy: StreamingPresentationPolicy,
    declarative_fallback: StreamingPresentationPolicy,
    focus: StreamingConfiguratorFocus,
    reset_pending: bool,
    enabled_checkbox: CheckboxState,
    curve_actions: ActionRowState,
    rate_actions: ActionRowState,
    lag_actions: ActionRowState,
    outcome_actions: ActionRowState,
    committed_geometry: Option<StreamingConfiguratorGeometry>,
}

impl StreamingConfiguratorState {
    /// Create configurator state from the effective and declarative policies.
    #[must_use]
    pub fn new(
        now: Instant,
        effective_policy: StreamingPresentationPolicy,
        declarative_fallback: StreamingPresentationPolicy,
    ) -> Self {
        let effective_policy = effective_policy.normalized();
        let mut state = Self {
            controller: StreamingPreviewController::new(now, effective_policy),
            override_policy: effective_policy,
            declarative_fallback: declarative_fallback.normalized(),
            focus: StreamingConfiguratorFocus::Enabled,
            reset_pending: false,
            enabled_checkbox: CheckboxState::new(effective_policy.enabled),
            curve_actions: ActionRowState::new(),
            rate_actions: ActionRowState::new(),
            lag_actions: ActionRowState::new(),
            outcome_actions: ActionRowState::new(),
            committed_geometry: None,
        };
        state.sync_component_focus();
        state
    }

    /// Return the preview controller.
    #[must_use]
    pub const fn controller(&self) -> &StreamingPreviewController {
        &self.controller
    }

    /// Return the selected policy or declarative fallback when reset is pending.
    #[must_use]
    pub const fn selected_policy(&self) -> StreamingPresentationPolicy {
        if self.reset_pending {
            self.declarative_fallback
        } else {
            self.override_policy
        }
    }

    /// Return the active focus row.
    #[must_use]
    pub const fn focus(&self) -> StreamingConfiguratorFocus {
        self.focus
    }

    /// Return whether Apply will clear the user-state override.
    #[must_use]
    pub const fn reset_pending(&self) -> bool {
        self.reset_pending
    }

    /// Return the enabled checkbox interaction state.
    #[must_use]
    pub const fn enabled_checkbox(&self) -> &CheckboxState {
        &self.enabled_checkbox
    }

    /// Return curve action interaction state.
    #[must_use]
    pub const fn curve_actions(&self) -> &ActionRowState {
        &self.curve_actions
    }

    /// Return rate action interaction state.
    #[must_use]
    pub const fn rate_actions(&self) -> &ActionRowState {
        &self.rate_actions
    }

    /// Return lag action interaction state.
    #[must_use]
    pub const fn lag_actions(&self) -> &ActionRowState {
        &self.lag_actions
    }

    /// Return outcome action interaction state.
    #[must_use]
    pub const fn outcome_actions(&self) -> &ActionRowState {
        &self.outcome_actions
    }

    /// Return the earliest controller deadline.
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.controller.next_deadline(now)
    }

    /// Advance due preview work.
    pub fn advance(&mut self, now: Instant) -> bool {
        self.controller.advance(now)
    }

    /// Commit geometry produced by the most recent rendered frame.
    pub const fn commit_geometry(&mut self, geometry: Option<StreamingConfiguratorGeometry>) {
        self.committed_geometry = geometry;
    }

    /// Handle one mouse event using the most recently committed component geometry.
    pub fn handle_committed_mouse(
        &mut self,
        mouse: MouseEvent,
        now: Instant,
    ) -> StreamingConfiguratorOutcome {
        let Some(geometry) = self.committed_geometry else {
            return StreamingConfiguratorOutcome::Handled;
        };
        self.handle_mouse(mouse, geometry, now)
    }

    /// Handle one mouse event through BMUX interactive components.
    pub fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        geometry: StreamingConfiguratorGeometry,
        now: Instant,
    ) -> StreamingConfiguratorOutcome {
        let event = Event::Mouse(mouse);
        match Checkbox::new("Enabled").handle_event(
            geometry.enabled,
            &mut self.enabled_checkbox,
            &event,
        ) {
            CheckboxOutcome::Toggled(enabled) => {
                self.focus = StreamingConfiguratorFocus::Enabled;
                self.sync_component_focus();
                let mut policy = self.override_policy;
                policy.enabled = enabled;
                self.apply_override_policy(policy, now);
                return StreamingConfiguratorOutcome::Handled;
            }
            CheckboxOutcome::Redraw => return StreamingConfiguratorOutcome::Handled,
            CheckboxOutcome::Ignored => {}
        }

        let curve_actions = curve_action_buttons();
        match ActionRow::new(&curve_actions).handle_event(
            geometry.curve,
            &mut self.curve_actions,
            &event,
        ) {
            ActionRowOutcome::Activated { index, .. } => {
                self.focus = StreamingConfiguratorFocus::Curve;
                self.curve_actions.set_focused(Some(index));
                let curves = [
                    StreamingInterpolationCurve::Linear,
                    StreamingInterpolationCurve::EaseIn,
                    StreamingInterpolationCurve::EaseOut,
                    StreamingInterpolationCurve::EaseInOut,
                ];
                let mut policy = self.override_policy;
                if let Some(curve) = curves.get(index) {
                    policy.curve = *curve;
                    self.apply_override_policy(policy, now);
                }
                return StreamingConfiguratorOutcome::Handled;
            }
            outcome if outcome.is_handled() => return StreamingConfiguratorOutcome::Handled,
            _ => {}
        }
        if let Some(outcome) = self.handle_numeric_mouse(
            &event,
            geometry.rate,
            StreamingConfiguratorFocus::GraphemesPerSecond,
            now,
        ) {
            return outcome;
        }
        if let Some(outcome) = self.handle_numeric_mouse(
            &event,
            geometry.lag,
            StreamingConfiguratorFocus::MaxLag,
            now,
        ) {
            return outcome;
        }

        let actions = outcome_action_buttons();
        match ActionRow::new(&actions).handle_event(
            geometry.outcomes,
            &mut self.outcome_actions,
            &event,
        ) {
            ActionRowOutcome::Activated { id, .. } if id == "reset" => {
                self.focus = StreamingConfiguratorFocus::Reset;
                self.outcome_actions.set_focused(Some(0));
                self.reset_pending = true;
                let _ = self.controller.set_policy(self.declarative_fallback);
                StreamingConfiguratorOutcome::Handled
            }
            ActionRowOutcome::Activated { id, .. } if id == "apply" && self.reset_pending => {
                StreamingConfiguratorOutcome::Reset
            }
            ActionRowOutcome::Activated { id, .. } if id == "apply" => {
                StreamingConfiguratorOutcome::Apply(self.selected_policy())
            }
            ActionRowOutcome::Activated { id, .. } if id == "cancel" => {
                StreamingConfiguratorOutcome::Cancel
            }
            outcome if outcome.is_handled() => StreamingConfiguratorOutcome::Handled,
            _ if geometry.surface.contains(mouse.position) => StreamingConfiguratorOutcome::Handled,
            _ => StreamingConfiguratorOutcome::Ignored,
        }
    }

    fn handle_numeric_mouse(
        &mut self,
        event: &Event,
        area: Rect,
        focus: StreamingConfiguratorFocus,
        now: Instant,
    ) -> Option<StreamingConfiguratorOutcome> {
        let actions = numeric_action_buttons();
        let state = match focus {
            StreamingConfiguratorFocus::GraphemesPerSecond => &mut self.rate_actions,
            StreamingConfiguratorFocus::MaxLag => &mut self.lag_actions,
            _ => return None,
        };
        let outcome = ActionRow::new(&actions).handle_event(area, state, event);
        match outcome {
            ActionRowOutcome::Activated { index, .. } => {
                self.focus = focus;
                self.sync_component_focus();
                self.adjust(if index == 0 { -1 } else { 1 }, false, now);
                Some(StreamingConfiguratorOutcome::Handled)
            }
            outcome if outcome.is_handled() => Some(StreamingConfiguratorOutcome::Handled),
            _ => None,
        }
    }

    fn apply_override_policy(&mut self, policy: StreamingPresentationPolicy, now: Instant) {
        self.reset_pending = false;
        let became_smoothing =
            self.override_policy.is_immediate() && !policy.normalized().is_immediate();
        self.override_policy = policy.normalized();
        self.enabled_checkbox
            .set_checked(self.override_policy.enabled);
        self.sync_component_focus();
        let _ = self.controller.set_policy(self.override_policy);
        if became_smoothing && self.controller.is_completed() {
            self.controller.restart(now);
        }
    }

    /// Handle one keyboard input event.
    pub fn handle_key(&mut self, stroke: KeyStroke, now: Instant) -> StreamingConfiguratorOutcome {
        match stroke.key {
            KeyCode::Up => {
                self.move_focus(-1);
                StreamingConfiguratorOutcome::Handled
            }
            KeyCode::Down => {
                self.move_focus(1);
                StreamingConfiguratorOutcome::Handled
            }
            KeyCode::Left => {
                self.adjust(-1, stroke.modifiers.shift, now);
                StreamingConfiguratorOutcome::Handled
            }
            KeyCode::Right => {
                self.adjust(1, stroke.modifiers.shift, now);
                StreamingConfiguratorOutcome::Handled
            }
            KeyCode::Space => {
                self.adjust(1, false, now);
                StreamingConfiguratorOutcome::Handled
            }
            KeyCode::Char('r') => {
                self.controller.restart(now);
                StreamingConfiguratorOutcome::Handled
            }
            KeyCode::Char('p') => {
                if self.controller.is_paused() {
                    self.controller.resume(now);
                } else {
                    self.controller.pause(now);
                }
                StreamingConfiguratorOutcome::Handled
            }
            KeyCode::Enter if self.reset_pending => StreamingConfiguratorOutcome::Reset,
            KeyCode::Enter => StreamingConfiguratorOutcome::Apply(self.selected_policy()),
            KeyCode::Escape => StreamingConfiguratorOutcome::Cancel,
            _ => StreamingConfiguratorOutcome::Ignored,
        }
    }

    fn move_focus(&mut self, delta: isize) {
        let current = StreamingConfiguratorFocus::ALL
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or_default();
        let len = StreamingConfiguratorFocus::ALL.len();
        let next = if delta < 0 {
            current.checked_sub(1).unwrap_or(len - 1)
        } else {
            (current + 1) % len
        };
        self.focus = StreamingConfiguratorFocus::ALL[next];
        self.sync_component_focus();
    }

    fn sync_component_focus(&mut self) {
        self.enabled_checkbox
            .set_focused(self.focus == StreamingConfiguratorFocus::Enabled);
        self.curve_actions.set_focused(
            (self.focus == StreamingConfiguratorFocus::Curve)
                .then_some(curve_index(self.override_policy.curve)),
        );
        self.rate_actions.set_focused(
            (self.focus == StreamingConfiguratorFocus::GraphemesPerSecond).then_some(1),
        );
        self.lag_actions
            .set_focused((self.focus == StreamingConfiguratorFocus::MaxLag).then_some(1));
        self.outcome_actions
            .set_focused((self.focus == StreamingConfiguratorFocus::Reset).then_some(0));
    }

    fn adjust(&mut self, direction: i32, coarse: bool, now: Instant) {
        if self.focus == StreamingConfiguratorFocus::Reset {
            self.reset_pending = !self.reset_pending;
            let policy = self.selected_policy();
            let _ = self.controller.set_policy(policy);
            return;
        }
        self.reset_pending = false;
        let mut policy = self.override_policy;
        match self.focus {
            StreamingConfiguratorFocus::Enabled => policy.enabled = !policy.enabled,
            StreamingConfiguratorFocus::Curve => {
                policy.curve = cycle_curve(policy.curve, direction);
            }
            StreamingConfiguratorFocus::GraphemesPerSecond => {
                let step = if coarse { 100 } else { 25 };
                policy.graphemes_per_second = adjust_u32(
                    policy.graphemes_per_second,
                    direction,
                    step,
                    StreamingPresentationPolicy::MAX_GRAPHEMES_PER_SECOND,
                );
            }
            StreamingConfiguratorFocus::MaxLag => {
                let step = if coarse { 100 } else { 10 };
                policy.max_lag_ms = adjust_u64(
                    policy.max_lag_ms,
                    direction,
                    step,
                    StreamingPresentationPolicy::MAX_LAG_MS,
                );
            }
            StreamingConfiguratorFocus::Reset => {}
        }
        self.apply_override_policy(policy, now);
    }
}

pub fn curve_action_buttons() -> [ActionButton; 4] {
    [
        ActionButton::new("linear", "Linear"),
        ActionButton::new("ease_in", "Ease in"),
        ActionButton::new("ease_out", "Ease out"),
        ActionButton::new("ease_in_out", "Ease in/out"),
    ]
}

pub fn numeric_action_buttons() -> [ActionButton; 2] {
    [
        ActionButton::new("decrement", "−"),
        ActionButton::new("increment", "+"),
    ]
}

pub fn outcome_action_buttons() -> [ActionButton; 3] {
    [
        ActionButton::new("reset", "Reset"),
        ActionButton::new("apply", "Apply"),
        ActionButton::new("cancel", "Cancel"),
    ]
}

const fn curve_index(curve: StreamingInterpolationCurve) -> usize {
    match curve {
        StreamingInterpolationCurve::Linear => 0,
        StreamingInterpolationCurve::EaseIn => 1,
        StreamingInterpolationCurve::EaseOut => 2,
        StreamingInterpolationCurve::EaseInOut => 3,
    }
}

const fn cycle_curve(
    curve: StreamingInterpolationCurve,
    direction: i32,
) -> StreamingInterpolationCurve {
    let index = curve_index(curve);
    let next = if direction < 0 {
        (index + 3) % 4
    } else {
        (index + 1) % 4
    };
    match next {
        0 => StreamingInterpolationCurve::Linear,
        1 => StreamingInterpolationCurve::EaseIn,
        2 => StreamingInterpolationCurve::EaseOut,
        _ => StreamingInterpolationCurve::EaseInOut,
    }
}

fn adjust_u32(value: u32, direction: i32, step: u32, maximum: u32) -> u32 {
    if direction < 0 {
        value.saturating_sub(step)
    } else {
        value.saturating_add(step).min(maximum)
    }
}

fn adjust_u64(value: u64, direction: i32, step: u64, maximum: u64) -> u64 {
    if direction < 0 {
        value.saturating_sub(step)
    } else {
        value.saturating_add(step).min(maximum)
    }
}

/// Bounded deterministic controller for the streaming configurator preview.
pub struct StreamingPreviewController {
    session_id: SessionId,
    raw: SessionView,
    smoothed: SessionView,
    selected_policy: StreamingPresentationPolicy,
    started_at: Instant,
    paused_at: Option<Instant>,
    phase: PlaybackPhase,
    next_chunk: usize,
    accepted_bytes: usize,
    revision: u64,
    completion_at: Option<Instant>,
}

impl StreamingPreviewController {
    /// Create a preview using the selected normalized smoothing policy.
    #[must_use]
    pub fn new(now: Instant, policy: StreamingPresentationPolicy) -> Self {
        let mut raw = SessionView::new();
        let mut smoothed = SessionView::new();
        let _ = raw.set_streaming_presentation_policy(StreamingPresentationPolicy::immediate());
        let selected_policy = policy.normalized();
        let _ = smoothed.set_streaming_presentation_policy(selected_policy);
        Self {
            session_id: SessionId::new(),
            raw,
            smoothed,
            selected_policy,
            started_at: now,
            paused_at: None,
            phase: PlaybackPhase::Running,
            next_chunk: 0,
            accepted_bytes: 0,
            revision: 0,
            completion_at: None,
        }
    }

    /// Return the raw preview's currently visible text.
    #[must_use]
    pub fn raw_text(&self) -> &str {
        projected_text(&self.raw)
    }

    /// Return the smoothed preview's currently visible text.
    #[must_use]
    pub fn smoothed_text(&self) -> &str {
        projected_text(&self.smoothed)
    }

    /// Return the number of source chunks already accepted.
    #[must_use]
    pub const fn delivered_chunks(&self) -> usize {
        self.next_chunk
    }

    /// Return the fixed number of source chunks.
    #[must_use]
    pub const fn total_chunks() -> usize {
        SAMPLE_CHUNKS.len()
    }

    /// Return whether playback is paused.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        matches!(self.phase, PlaybackPhase::Paused)
    }

    /// Return whether the current loop reached authoritative completion.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        matches!(self.phase, PlaybackPhase::Completed)
    }

    /// Apply a live normalized policy to the real shared smoothed projection.
    pub fn set_policy(&mut self, policy: StreamingPresentationPolicy) -> bool {
        let policy = policy.normalized();
        self.selected_policy = policy;
        self.smoothed.set_streaming_presentation_policy(policy)
    }

    /// Pause all source and shared-presentation deadlines without discarding state.
    pub const fn pause(&mut self, now: Instant) {
        if matches!(self.phase, PlaybackPhase::Running) {
            self.phase = PlaybackPhase::Paused;
            self.paused_at = Some(now);
        }
    }

    /// Resume from the same logical playback instant.
    pub fn resume(&mut self, now: Instant) {
        if !matches!(self.phase, PlaybackPhase::Paused) {
            return;
        }
        if let Some(paused_at) = self.paused_at.take() {
            let paused_for = now.saturating_duration_since(paused_at);
            self.started_at += paused_for;
            if let Some(completion_at) = self.completion_at.as_mut() {
                *completion_at += paused_for;
            }
        }
        self.phase = PlaybackPhase::Running;
    }

    /// Restart the deterministic scenario from its initial state.
    pub fn restart(&mut self, now: Instant) {
        *self = Self::new(now, self.selected_policy);
    }

    /// Return the earliest semantic source, presentation, or loop deadline.
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.is_paused() {
            return None;
        }
        if self.is_completed() {
            return self.completion_at.map(|at| at + COMPLETED_HOLD);
        }
        let source = SAMPLE_CHUNKS
            .get(self.next_chunk)
            .map(|(offset_ms, _)| self.started_at + Duration::from_millis(*offset_ms))
            .or_else(|| Some(self.started_at + Duration::from_millis(1_751)));
        [
            source,
            self.smoothed.next_streaming_presentation_deadline(now),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Advance all work due at `now`; returns whether renderer-visible state changed.
    pub fn advance(&mut self, now: Instant) -> bool {
        if self.is_paused() {
            return false;
        }
        if self.is_completed() {
            if self
                .completion_at
                .is_some_and(|completed| now >= completed + COMPLETED_HOLD)
            {
                self.restart(now);
                return true;
            }
            return false;
        }

        let mut changed = self.smoothed.advance_streaming_presentation(now);
        while let Some(&(offset_ms, chunk)) = SAMPLE_CHUNKS.get(self.next_chunk) {
            if now < self.started_at + Duration::from_millis(offset_ms) {
                break;
            }
            self.revision = self.revision.saturating_add(1);
            let event = SessionLiveEvent {
                session_id: self.session_id,
                kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                    output_position: None,
                    turn_id: TURN_ID.to_owned(),
                    segment_id: SEGMENT_ID.to_owned(),
                    segment_order: 0,
                    update: TextStreamUpdate {
                        generation: 0,
                        first_revision: self.revision,
                        revision: self.revision,
                        operation: TextStreamOperation::Append {
                            expected_offset: self.accepted_bytes,
                            text: chunk.to_owned(),
                        },
                    },
                },
            };
            self.raw.apply_live_event(&event);
            self.smoothed.apply_live_event(&event);
            self.accepted_bytes = self.accepted_bytes.saturating_add(chunk.len());
            self.next_chunk = self.next_chunk.saturating_add(1);
            changed = true;
        }

        if self.next_chunk == SAMPLE_CHUNKS.len() {
            self.finish(now);
            changed = true;
        }
        changed
    }

    fn finish(&mut self, now: Instant) {
        self.revision = self.revision.saturating_add(1);
        let terminal = SessionLiveEvent {
            session_id: self.session_id,
            kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: TURN_ID.to_owned(),
                segment_id: SEGMENT_ID.to_owned(),
                segment_order: 0,
                update: TextStreamUpdate {
                    generation: 0,
                    first_revision: self.revision,
                    revision: self.revision,
                    operation: TextStreamOperation::Terminal {
                        status: TextStreamTerminalStatus::Completed,
                    },
                },
            },
        };
        self.raw.apply_live_event(&terminal);
        self.smoothed.apply_live_event(&terminal);
        let completed = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 0,
            session_id: self.session_id,
            provenance: None,
            kind: SessionEventKind::ModelTurnFinished {
                turn_id: TURN_ID.to_owned(),
                outcome: ModelTurnOutcome::Completed,
                message: None,
            },
        };
        self.raw.apply_event(&completed);
        self.smoothed.apply_event(&completed);
        self.phase = PlaybackPhase::Completed;
        self.completion_at = Some(now);
    }
}

fn projected_text(view: &SessionView) -> &str {
    view.snapshot()
        .transcript
        .items
        .iter()
        .find_map(|item| match &item.kind {
            TranscriptViewItemKind::AssistantMessage { message } => Some(message.text.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configurator_controls_normalize_cycle_and_preserve_cancel_baseline() {
        let now = Instant::now();
        let original = StreamingPresentationPolicy::default();
        let fallback = StreamingPresentationPolicy {
            enabled: false,
            ..original
        };
        let mut state = StreamingConfiguratorState::new(now, original, fallback);
        assert_eq!(state.focus(), StreamingConfiguratorFocus::Enabled);
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Space), now),
            StreamingConfiguratorOutcome::Handled
        );
        assert!(!state.selected_policy().enabled);

        let _ = state.handle_key(KeyStroke::simple(KeyCode::Down), now);
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Right), now);
        assert_eq!(
            state.selected_policy().curve,
            StreamingInterpolationCurve::EaseIn
        );
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Left), now);
        assert_eq!(
            state.selected_policy().curve,
            StreamingInterpolationCurve::Linear
        );

        for _ in 0..3 {
            let _ = state.handle_key(KeyStroke::simple(KeyCode::Down), now);
        }
        assert_eq!(state.focus(), StreamingConfiguratorFocus::Reset);
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Space), now);
        assert!(state.reset_pending());
        assert_eq!(state.selected_policy(), fallback.normalized());
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Enter), now),
            StreamingConfiguratorOutcome::Reset
        );
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Escape), now),
            StreamingConfiguratorOutcome::Cancel
        );
    }

    #[test]
    fn bmux_components_own_mouse_toggle_adjust_apply_reset_and_capture() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::{Point, Rect};

        let now = Instant::now();
        let mut state = StreamingConfiguratorState::new(
            now,
            StreamingPresentationPolicy::default(),
            StreamingPresentationPolicy::immediate(),
        );
        let geometry = StreamingConfiguratorGeometry {
            enabled: Rect::new(2, 2, 20, 1),
            curve: Rect::new(2, 3, 60, 1),
            rate: Rect::new(2, 4, 10, 1),
            lag: Rect::new(2, 5, 10, 1),
            outcomes: Rect::new(2, 6, 32, 1),
            surface: Rect::new(0, 0, 80, 24),
        };
        let click = |state: &mut StreamingConfiguratorState, point: Point| {
            assert_eq!(
                state.handle_mouse(
                    MouseEvent::new(MouseEventKind::Down(MouseButton::Left), point),
                    geometry,
                    now,
                ),
                StreamingConfiguratorOutcome::Handled
            );
            state.handle_mouse(
                MouseEvent::new(MouseEventKind::Up(MouseButton::Left), point),
                geometry,
                now,
            )
        };

        assert_eq!(
            click(&mut state, Point::new(3, 2)),
            StreamingConfiguratorOutcome::Handled
        );
        assert!(!state.selected_policy().enabled);
        let before_rate = state.selected_policy().graphemes_per_second;
        let _ = click(&mut state, Point::new(8, 4));
        assert!(state.selected_policy().graphemes_per_second > before_rate);
        assert_eq!(
            state.handle_mouse(
                MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(70, 20)),
                geometry,
                now,
            ),
            StreamingConfiguratorOutcome::Handled
        );

        let _ = click(&mut state, Point::new(3, 6));
        assert!(state.reset_pending());
        let apply = click(&mut state, Point::new(12, 6));
        assert_eq!(apply, StreamingConfiguratorOutcome::Reset);
    }

    #[test]
    fn every_key_path_and_numeric_boundary_is_owned_and_bounded() {
        let now = Instant::now();
        let mut state = StreamingConfiguratorState::new(
            now,
            StreamingPresentationPolicy {
                graphemes_per_second: 0,
                max_lag_ms: 0,
                ..StreamingPresentationPolicy::default()
            },
            StreamingPresentationPolicy::default(),
        );
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Left), now),
            StreamingConfiguratorOutcome::Handled
        );
        assert!(!state.selected_policy().enabled);
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Up), now),
            StreamingConfiguratorOutcome::Handled
        );
        assert_eq!(state.focus(), StreamingConfiguratorFocus::Reset);
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Down), now),
            StreamingConfiguratorOutcome::Handled
        );
        assert_eq!(state.focus(), StreamingConfiguratorFocus::Enabled);
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Char('p')), now),
            StreamingConfiguratorOutcome::Handled
        );
        assert!(state.controller().is_paused());
        assert!(state.next_deadline(now).is_none());
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Char('p')), now);
        assert!(!state.controller().is_paused());
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Char('r')), now);
        assert_eq!(state.controller().delivered_chunks(), 0);
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Enter), now),
            StreamingConfiguratorOutcome::Apply(state.selected_policy())
        );
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Char('x')), now),
            StreamingConfiguratorOutcome::Ignored
        );

        let mut maximum = StreamingConfiguratorState::new(
            now,
            StreamingPresentationPolicy {
                graphemes_per_second: StreamingPresentationPolicy::MAX_GRAPHEMES_PER_SECOND,
                max_lag_ms: StreamingPresentationPolicy::MAX_LAG_MS,
                ..StreamingPresentationPolicy::default()
            },
            StreamingPresentationPolicy::default(),
        );
        let _ = maximum.handle_key(KeyStroke::simple(KeyCode::Down), now);
        let _ = maximum.handle_key(KeyStroke::simple(KeyCode::Down), now);
        let _ = maximum.handle_key(KeyStroke::simple(KeyCode::Right), now);
        assert_eq!(
            maximum.selected_policy().graphemes_per_second,
            StreamingPresentationPolicy::MAX_GRAPHEMES_PER_SECOND
        );
        let _ = maximum.handle_key(KeyStroke::simple(KeyCode::Down), now);
        let _ = maximum.handle_key(KeyStroke::simple(KeyCode::Right), now);
        assert_eq!(
            maximum.selected_policy().max_lag_ms,
            StreamingPresentationPolicy::MAX_LAG_MS
        );
    }

    #[test]
    fn numeric_controls_use_fine_and_coarse_bounded_steps() {
        let now = Instant::now();
        let mut state = StreamingConfiguratorState::new(
            now,
            StreamingPresentationPolicy::default(),
            StreamingPresentationPolicy::default(),
        );
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Down), now);
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Down), now);
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Right), now);
        assert_eq!(state.selected_policy().graphemes_per_second, 325);
        let coarse = KeyStroke::with_modifiers(
            KeyCode::Right,
            bmux_keyboard::Modifiers {
                shift: true,
                ..bmux_keyboard::Modifiers::NONE
            },
        );
        let _ = state.handle_key(coarse, now);
        assert_eq!(state.selected_policy().graphemes_per_second, 425);
        let _ = state.handle_key(KeyStroke::simple(KeyCode::Down), now);
        for _ in 0..20 {
            let _ = state.handle_key(coarse, now);
        }
        assert_eq!(
            state.selected_policy().max_lag_ms,
            StreamingPresentationPolicy::MAX_LAG_MS
        );
    }

    #[test]
    fn deterministic_preview_is_bounded_monotonic_and_converges() {
        let started = Instant::now();
        let mut controller =
            StreamingPreviewController::new(started, StreamingPresentationPolicy::default());
        let mut previous_raw = String::new();
        let mut previous_smoothed = String::new();
        for millis in 0..=1_800 {
            let _ = controller.advance(started + Duration::from_millis(millis));
            assert!(controller.raw_text().starts_with(&previous_raw));
            assert!(controller.smoothed_text().starts_with(&previous_smoothed));
            previous_raw = controller.raw_text().to_owned();
            previous_smoothed = controller.smoothed_text().to_owned();
        }
        assert!(controller.is_completed());
        assert_eq!(
            controller.delivered_chunks(),
            StreamingPreviewController::total_chunks()
        );
        let expected = SAMPLE_CHUNKS
            .iter()
            .map(|(_, chunk)| *chunk)
            .collect::<String>();
        assert_eq!(controller.raw_text(), expected);
        assert_eq!(controller.smoothed_text(), controller.raw_text());
    }

    #[test]
    fn pause_resume_and_restart_preserve_logical_playback() {
        let started = Instant::now();
        let mut controller =
            StreamingPreviewController::new(started, StreamingPresentationPolicy::default());
        let first_due = started + Duration::from_millis(SAMPLE_CHUNKS[0].0);
        assert!(controller.advance(first_due));
        let raw = controller.raw_text().to_owned();
        controller.pause(first_due);
        assert!(controller.next_deadline(first_due).is_none());
        assert!(!controller.advance(first_due + Duration::from_secs(5)));
        assert_eq!(controller.raw_text(), raw);
        controller.resume(first_due + Duration::from_secs(5));
        let _ = controller.advance(first_due + Duration::from_secs(5));
        assert_eq!(controller.raw_text(), raw);
        controller.restart(first_due + Duration::from_secs(6));
        assert!(controller.raw_text().is_empty());
        assert_eq!(controller.delivered_chunks(), 0);
    }

    #[test]
    fn live_policy_update_uses_shared_projection_without_changing_raw() {
        let started = Instant::now();
        let mut controller =
            StreamingPreviewController::new(started, StreamingPresentationPolicy::default());
        let due = started + Duration::from_millis(800);
        assert!(controller.advance(due));
        let raw = controller.raw_text().to_owned();
        let _ = controller.set_policy(StreamingPresentationPolicy::immediate());
        assert_eq!(controller.raw_text(), raw);
        assert_eq!(controller.smoothed_text(), raw);
    }
}
