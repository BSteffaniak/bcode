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

/// Complete local state for the streaming configurator surface.
pub struct StreamingConfiguratorState {
    controller: StreamingPreviewController,
    original_policy: StreamingPresentationPolicy,
    override_policy: StreamingPresentationPolicy,
    declarative_fallback: StreamingPresentationPolicy,
    focus: StreamingConfiguratorFocus,
    reset_pending: bool,
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
        Self {
            controller: StreamingPreviewController::new(now, effective_policy),
            original_policy: effective_policy,
            override_policy: effective_policy,
            declarative_fallback: declarative_fallback.normalized(),
            focus: StreamingConfiguratorFocus::Enabled,
            reset_pending: false,
        }
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

    /// Return the policy that was active when the surface opened.
    #[must_use]
    pub const fn original_policy(&self) -> StreamingPresentationPolicy {
        self.original_policy
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
        let became_smoothing =
            self.override_policy.is_immediate() && !policy.normalized().is_immediate();
        self.override_policy = policy.normalized();
        let _ = self.controller.set_policy(self.override_policy);
        if became_smoothing && self.controller.is_completed() {
            self.controller.restart(now);
        }
    }
}

const fn cycle_curve(
    curve: StreamingInterpolationCurve,
    direction: i32,
) -> StreamingInterpolationCurve {
    match (curve, direction < 0) {
        (StreamingInterpolationCurve::Linear, false) => StreamingInterpolationCurve::EaseIn,
        (StreamingInterpolationCurve::EaseIn, false) => StreamingInterpolationCurve::EaseOut,
        (StreamingInterpolationCurve::EaseOut, false) => StreamingInterpolationCurve::EaseInOut,
        (StreamingInterpolationCurve::EaseInOut, false) => StreamingInterpolationCurve::Linear,
        (StreamingInterpolationCurve::Linear, true) => StreamingInterpolationCurve::EaseInOut,
        (StreamingInterpolationCurve::EaseIn, true) => StreamingInterpolationCurve::Linear,
        (StreamingInterpolationCurve::EaseOut, true) => StreamingInterpolationCurve::EaseIn,
        (StreamingInterpolationCurve::EaseInOut, true) => StreamingInterpolationCurve::EaseOut,
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

    /// Return the complete deterministic sample text.
    #[must_use]
    pub fn final_text() -> String {
        SAMPLE_CHUNKS.iter().map(|(_, chunk)| *chunk).collect()
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

    /// Return the selected normalized smoothing policy.
    #[must_use]
    pub const fn selected_policy(&self) -> StreamingPresentationPolicy {
        self.selected_policy
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
    pub fn pause(&mut self, now: Instant) {
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
        assert_eq!(state.original_policy(), original);
        assert_eq!(state.focus(), StreamingConfiguratorFocus::Enabled);
        assert_eq!(
            state.handle_key(KeyStroke::simple(KeyCode::Space), now),
            StreamingConfiguratorOutcome::Handled
        );
        assert!(!state.selected_policy().enabled);
        assert_eq!(state.original_policy(), original);

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
        assert_eq!(
            controller.raw_text(),
            StreamingPreviewController::final_text()
        );
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
