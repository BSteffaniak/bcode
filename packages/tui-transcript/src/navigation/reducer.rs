//! The navigation reducer: transitions T1–T27 (spec section 4.4).
//!
//! Inputs are reduced against a [`NavView`] that already reflects every content change (N11),
//! so positions are re-derived from keys and the current measured index on every input and no
//! deferred resynchronization exists. Preparation and paint never call into this module
//! (CK1, CK2).

use std::time::{Duration, Instant};

use bcode_session_view_models::{TranscriptChange, TranscriptChanges};
use bmux_tui_components::virtual_list::{
    Edge, ItemFocusTarget, PositionEstimate as BmuxEstimate, anchor_for, reveal_top_offset,
};
use bmux_tui_components::virtual_list::{FractionTarget, ResolvedPosition};

use super::auto::{AutoAction, automatic_actions};
use super::geometry::{mode_for, resting_top, visual_top};
use super::{
    AnchorOffset, ApproachEdge, Correspondence, GRACE, InputSource, JumpGoal, ListPosition,
    NavEffect, NavInput, NavIntent, NavMode, NavState, NavTimer, NavView, PAGE_STEP_ROWS,
    PendingJump, PendingMovement, Placement, ReadingAnchor, ReadingPosition, Region, RevealRequest,
    ScrollMotion, ScrollTarget, VerticalDirection,
};
use crate::units::UnitKey;
use crate::viewport::{bmux_motion, bmux_policy, bmux_target};
use crate::window::{
    BoundaryStatus, PAGE_EVENTS, PageDirection, PageRequest, RESIDENT_CEILING_EVENTS,
    WindowRequest, WindowTarget,
};

/// Reduce one input; see [`super::reduce`].
pub(super) fn reduce(state: &mut NavState, input: &NavInput, view: &NavView<'_>) -> Vec<NavEffect> {
    let mut reducer = Reducer {
        state,
        view,
        effects: Vec::new(),
    };
    reducer.input(input);
    reducer.effects
}

struct Reducer<'s, 'v, 'a> {
    state: &'s mut NavState,
    view: &'v NavView<'a>,
    effects: Vec<NavEffect>,
}

impl Reducer<'_, '_, '_> {
    fn input(&mut self, input: &NavInput) {
        match input {
            NavInput::Intent { intent, at } => self.intent(intent.clone(), *at),
            NavInput::Content { changes, at } => self.content(changes, input, *at),
            NavInput::PageResult {
                direction,
                inserted_rows,
                exhausted,
                ..
            } => self.page_result(*direction, *inserted_rows, *exhausted),
            NavInput::WindowResult { generation, target } => {
                self.window_result(*generation, target);
            }
            NavInput::Geometry { .. } => self.invalidate(),
            NavInput::Timer { key, at } => self.timer(*key, input, *at),
            NavInput::SubmissionAccepted { at, .. } => self.apply_auto(input, *at),
            // CK3: commit records nothing in navigation; motion frames are scheduled by the
            // motion itself (T25), so a commit never changes the keyed position (CK1, CK2).
            NavInput::PresentationCommitted { .. } => {}
        }
    }

    // ----- effects -------------------------------------------------------------------------

    fn invalidate(&mut self) {
        let effect = NavEffect::Invalidate(Region::Transcript);
        if !self.effects.contains(&effect) {
            self.effects.push(effect);
        }
    }

    // ----- intents -------------------------------------------------------------------------

    fn intent(&mut self, intent: NavIntent, at: Instant) {
        match intent {
            NavIntent::ScrollBy { rows, source } => self.manual(rows, source, at),
            NavIntent::PageBy { pages } => {
                let step = i64::try_from(PAGE_STEP_ROWS).unwrap_or(i64::MAX);
                self.manual(i64::from(pages).saturating_mul(step), InputSource::Key, at);
            }
            NavIntent::Follow | NavIntent::JumpToLatest => {
                self.jump_to_latest(JumpGoal::Latest, at);
            }
            NavIntent::RevealItem { item, sticky } => self.reveal_intent(item, sticky, at),
            NavIntent::JumpToItem { item, placement } => {
                if self.view.contains(&item) {
                    self.cancel_pending();
                    self.place(&item, placement, ApproachEdge::Top, Some(at));
                }
            }
            NavIntent::JumpToSequence {
                sequence,
                placement,
            } => self.jump_to_sequence(sequence, placement),
            NavIntent::JumpToFraction { fraction } => self.jump_to_fraction(fraction),
            NavIntent::JumpToSessionStart => self.jump_to_session_start(at),
            NavIntent::FocusTarget { target, edge, rows } => {
                // T19: no movement when the target is fully visible.
                if self.view.contains(&target.unit) {
                    match rows {
                        Some(rows) => {
                            self.place_rows(&target.unit, &rows, edge, at);
                        }
                        None => {
                            self.place(&target.unit, Placement::EnsureVisible, edge, Some(at));
                        }
                    }
                }
            }
            NavIntent::ReleaseHold => self.release_hold(at),
            NavIntent::RestorePosition { position } => self.restore(position, at),
        }
    }

    /// Cancel pending movement, pending jump and pending submission reveals (T17, T22).
    fn cancel_pending(&mut self) {
        self.state.pending_movement = None;
        self.state.pending_jump = None;
        self.state.pending_reveals.clear();
    }

    /// T8–T12, T22, N6, N8: manual relative movement.
    fn manual(&mut self, rows: i64, _source: InputSource, at: Instant) {
        if rows == 0 {
            return;
        }
        let direction = if rows < 0 {
            VerticalDirection::Up
        } else {
            VerticalDirection::Down
        };
        // N6: materialize a running motion at its visual position before applying the delta.
        let top = visual_top(self.state, self.view, Some(at));
        self.state.motion = None;
        // T8 / T22: cancel the pending jump, pending submission reveals and opposite movement.
        self.state.pending_jump = None;
        self.state.pending_reveals.clear();
        if self
            .state
            .pending_movement
            .is_some_and(|pending| pending.direction != direction)
        {
            self.state.pending_movement = None;
        }
        let until = at + GRACE;
        self.state.grace_until = Some(until);
        self.effects.push(NavEffect::ScheduleTimer {
            key: NavTimer::GraceExpired,
            at: until,
        });
        self.invalidate();
        if self.view.index.is_empty() {
            // N12: no anchor unit, so the position stays `Following`.
            self.state.mode = NavMode::Following;
            return;
        }
        let max_content = self.view.max_content_top();
        let max = self.view.max_top();
        let top = top.min(max);
        let distance = rows.unsigned_abs();
        let (new_top, remainder) = match direction {
            VerticalDirection::Up => {
                let moved = distance.min(top);
                (top - moved, distance - moved)
            }
            VerticalDirection::Down => {
                let moved = distance.min(max - top);
                (top + moved, distance - moved)
            }
        };
        self.state.mode = if self.view.is_live()
            && direction == VerticalDirection::Down
            && new_top == max_content
        {
            // T9 (or T10 without tail-space room): content bottom on the viewport bottom.
            NavMode::Following
        } else {
            mode_for(self.view.position_at(new_top))
        };
        // N8: the remainder past a resident boundary waits for a page. In a live window the
        // bottom is the session tail and tail space is bounded, so no newer page exists.
        if remainder > 0 && (direction == VerticalDirection::Up || !self.view.is_live()) {
            self.store_pending(direction, remainder);
        }
    }

    /// N8: store (or extend) pending movement and request a page when possible.
    fn store_pending(&mut self, direction: VerticalDirection, rows: u64) {
        let edge = direction.page_direction().edge();
        if self.view.boundary_status(edge) == BoundaryStatus::Exhausted {
            self.state.pending_movement = None;
            return;
        }
        let rows = match self.state.pending_movement {
            Some(pending) if pending.direction == direction => pending.rows.saturating_add(rows),
            _ => rows,
        };
        self.state.pending_movement = Some(PendingMovement { direction, rows });
        self.request_page(direction.page_direction());
    }

    /// Request a page in a direction unless one is in flight, a window replacement is in
    /// flight, or the W7a ceiling forbids it.
    fn request_page(&mut self, direction: PageDirection) {
        let window = self.view.window;
        let in_flight = match direction {
            PageDirection::Older => window.in_flight.older,
            PageDirection::Newer => window.in_flight.newer,
        };
        if in_flight
            || window.in_flight.replace.is_some()
            || window.event_count.saturating_add(PAGE_EVENTS) > RESIDENT_CEILING_EVENTS
        {
            return;
        }
        let request = match direction {
            PageDirection::Older => window
                .first_sequence
                .map(|before_sequence| PageRequest::Older { before_sequence }),
            PageDirection::Newer => window
                .last_sequence
                .map(|after_sequence| PageRequest::Newer { after_sequence }),
        };
        if let Some(request) = request {
            let effect = NavEffect::Page(request);
            if !self.effects.contains(&effect) {
                self.effects.push(effect);
            }
        }
    }

    /// Begin a motion from `from_top` toward a keyed target (N7). Instant under reduced motion.
    fn begin_motion(&mut self, from_top: u64, target: ScrollTarget, at: Instant) {
        self.state.motion = None;
        self.invalidate();
        let duration = self.view.motion.duration;
        if duration == Duration::ZERO {
            return;
        }
        let Some(from) = self.view.anchor_at(from_top) else {
            return;
        };
        let motion = ScrollMotion {
            from,
            target,
            started: at,
            duration,
            easing: self.view.motion.easing,
        };
        let sampled = bmux_motion(&motion);
        let destination = sampled.target.resolve_top(
            self.view.index,
            from_top,
            self.view.geometry(),
            self.view.sources,
        );
        if destination.is_none_or(|top| top == from_top) {
            return;
        }
        if let Some(next) = sampled.next_frame(at, self.view.motion.frame_interval) {
            self.effects.push(NavEffect::ScheduleTimer {
                key: NavTimer::MotionFrame,
                at: next,
            });
        }
        self.state.motion = Some(motion);
    }

    /// Hold a unit's top on the viewport top via motion `ItemTop` (T2, T3).
    fn hold(&mut self, item: UnitKey, sticky: bool, at: Instant) {
        let from_top = visual_top(self.state, self.view, Some(at));
        self.state.deferred.remove(&item);
        self.state.admitted.insert(item.clone());
        if !sticky {
            // T2 clears earlier submission reveals.
            self.state.pending_reveals.clear();
        }
        self.state.mode = NavMode::Holding {
            item: item.clone(),
            sticky,
        };
        self.begin_motion(from_top, ScrollTarget::ItemTop(item), at);
    }

    /// Place a resident unit by placement (T17, T19). With an input time the change uses a
    /// motion started then; without one (window results) it applies at once. Returns whether
    /// the position changed.
    fn place(
        &mut self,
        item: &UnitKey,
        placement: Placement,
        edge: ApproachEdge,
        at: Option<Instant>,
    ) -> bool {
        let view = self.view;
        let geometry = view.geometry();
        let current = visual_top(self.state, view, at);
        let policy = bmux_policy(placement, edge);
        let mut target = match placement {
            Placement::ItemTopAtTop => ScrollTarget::ItemTop(item.clone()),
            Placement::ItemBottomAtBottom => ScrollTarget::ItemBottom(item.clone()),
            Placement::Center | Placement::EnsureVisible => {
                let Some(position) =
                    anchor_for(view.index, item, policy, current, geometry, view.sources)
                else {
                    return false;
                };
                match view.position_from_bmux(position) {
                    ListPosition::Anchored { anchor } | ListPosition::TailSpace { anchor } => {
                        ScrollTarget::Anchor(anchor)
                    }
                    ListPosition::FollowBottom => ScrollTarget::LatestBottom,
                }
            }
        };
        let Some(mut destination) =
            bmux_target(&target).resolve_top(view.index, current, geometry, view.sources)
        else {
            return false;
        };
        if destination > view.max_top() {
            // A detached window has no tail space (C31): clamp to the content bottom.
            destination = view.max_top();
            let Some(anchor) = view.anchor_at(destination) else {
                return false;
            };
            target = ScrollTarget::Anchor(anchor);
        }
        if placement == Placement::EnsureVisible && destination == current {
            return false;
        }
        self.state.pending_movement = None;
        self.state.pending_jump = None;
        self.state.pending_reveals.clear();
        self.state.mode = mode_for(self.final_position(&target, current));
        if let Some(at) = at {
            self.begin_motion(current, target, at);
        } else {
            self.state.motion = None;
            self.invalidate();
        }
        true
    }

    /// C32/T19: bring the unit-local `rows` of `unit` into view from the approach edge: moving
    /// up puts their first row on the top edge, moving down their last row on the bottom edge
    /// (taller ranges keep their first row on top). Fully visible rows do not move.
    ///
    /// The placement is BMUX's focus reveal (FO7, BMUX 12): the rows become an
    /// [`ItemFocusTarget`] of the unit, revealed with `EnsureVisibleFrom(edge)`; a detached
    /// window then clamps to its content bottom (C31).
    fn place_rows(
        &mut self,
        unit: &UnitKey,
        rows: &std::ops::Range<u64>,
        edge: ApproachEdge,
        at: Instant,
    ) -> bool {
        let view = self.view;
        let current = visual_top(self.state, view, Some(at));
        let target = ItemFocusTarget::new(unit.clone(), (), rows.clone());
        let edge = match edge {
            ApproachEdge::Top => Edge::Top,
            ApproachEdge::Bottom => Edge::Bottom,
        };
        let Some(destination) =
            reveal_top_offset(view.index, &target, edge, current, view.geometry())
        else {
            return false;
        };
        let destination = destination.min(view.max_top());
        if destination == current {
            return false;
        }
        let Some(anchor) = view.anchor_at(destination) else {
            return false;
        };
        let target = ScrollTarget::Anchor(anchor);
        self.state.pending_movement = None;
        self.state.pending_jump = None;
        self.state.pending_reveals.clear();
        self.state.mode = mode_for(self.final_position(&target, current));
        self.begin_motion(current, target, at);
        true
    }

    /// Keyed position a completed motion toward `target` rests at (N7).
    fn final_position(&self, target: &ScrollTarget, from_top: u64) -> ListPosition {
        let view = self.view;
        bmux_target(target)
            .final_position(view.index, from_top, view.geometry(), view.sources)
            .map_or_else(
                || self.state.resting_position(),
                |position| view.position_from_bmux(position),
            )
    }

    /// T13 / T14 (and the latest half of T21).
    fn jump_to_latest(&mut self, goal: JumpGoal, at: Instant) {
        self.state.pending_movement = None;
        self.state.pending_reveals.clear();
        if self.view.is_live() {
            self.state.pending_jump = None;
            let from_top = visual_top(self.state, self.view, Some(at));
            self.state.mode = NavMode::Following;
            if self.view.index.is_empty() {
                self.state.motion = None;
                self.invalidate();
            } else {
                self.begin_motion(from_top, ScrollTarget::LatestBottom, at);
            }
        } else {
            self.request_window(WindowRequest::Tail, goal);
        }
    }

    /// Emit a window request and remember what to do once it is incorporated.
    fn request_window(&mut self, request: WindowRequest, goal: JumpGoal) {
        self.state.pending_jump = Some(PendingJump {
            generation: self.view.window.generation,
            request,
            goal,
        });
        self.effects.push(NavEffect::Window(request));
    }

    /// T15 / T16.
    fn jump_to_session_start(&mut self, at: Instant) {
        self.cancel_pending();
        let start = self.view.first_unit().cloned();
        match start {
            Some(first) if self.view.window.contains_head() => {
                let from_top = visual_top(self.state, self.view, Some(at));
                let anchor = ReadingAnchor::top(first);
                self.state.mode = NavMode::Reading {
                    anchor: anchor.clone(),
                };
                self.begin_motion(from_top, ScrollTarget::Anchor(anchor), at);
            }
            _ => self.request_window(WindowRequest::Head, JumpGoal::SessionStart),
        }
    }

    /// T17 / T18 for a canonical sequence.
    fn jump_to_sequence(&mut self, sequence: u64, placement: Placement) {
        let resident = self
            .view
            .window
            .contains_sequence(sequence)
            .then(|| self.view.facts.unit_for_sequence(sequence))
            .flatten()
            .filter(|unit| self.view.contains(unit));
        self.cancel_pending();
        if let Some(unit) = resident {
            // T17: a timeline jump is discontinuous navigation; it lands at once so the next
            // committed frame shows the target (BUG-B4). Item jumps animate.
            self.place(&unit, placement, ApproachEdge::Top, None);
        } else {
            self.request_window(
                WindowRequest::Around { sequence },
                JumpGoal::Sequence {
                    sequence,
                    placement,
                },
            );
        }
    }

    /// T18 for a scrollbar fraction (W12): resident rows are reached directly; otherwise the
    /// fraction maps to a sequence estimate and an `Around` window.
    fn jump_to_fraction(&mut self, fraction: f64) {
        let view = self.view;
        let window = view.window;
        let estimate = window.position_estimate(view.total_height());
        let estimate = BmuxEstimate {
            before: estimate.rows_before_window,
            resident: estimate.resident_rows,
            after: estimate.rows_after_window,
        };
        self.cancel_pending();
        let rate = if window.rows_per_event_estimate > 0.0 {
            window.rows_per_event_estimate
        } else {
            1.0
        };
        let events = |rows: u64| -> u64 {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let events = (rows as f64 / rate).round() as u64;
            events
        };
        let sequence = match estimate.target_for_fraction(fraction, view.viewport.height) {
            FractionTarget::Resident { offset } => {
                self.state.motion = None;
                self.state.mode = mode_for(view.position_at(offset));
                self.invalidate();
                return;
            }
            FractionTarget::Before { rows_before_window } => window.first_sequence.map(|first| {
                first
                    .saturating_sub(events(rows_before_window))
                    .max(window.first_available_sequence.unwrap_or(0))
            }),
            FractionTarget::After { rows_after_window } => window.last_sequence.map(|last| {
                let sequence = last.saturating_add(events(rows_after_window));
                window
                    .last_available_sequence
                    .map_or(sequence, |available| sequence.min(available))
            }),
        };
        if let Some(sequence) = sequence {
            self.request_window(
                WindowRequest::Around { sequence },
                JumpGoal::Sequence {
                    sequence,
                    placement: Placement::ItemTopAtTop,
                },
            );
        }
    }

    /// Explicit `RevealItem` intent: a programmatic hold, never suppressed by grace.
    fn reveal_intent(&mut self, item: UnitKey, sticky: bool, at: Instant) {
        if self.view.is_visible(&item) {
            self.state.pending_jump = None;
            self.state.pending_movement = None;
            self.hold(item, sticky, at);
        } else if !sticky {
            self.state.pending_reveals.clear();
            self.state
                .pending_reveals
                .insert(item, RevealRequest { requested: at });
        }
    }

    /// T20: stop following/holding where the content currently is.
    fn release_hold(&mut self, at: Instant) {
        if !matches!(
            self.state.mode,
            NavMode::Following | NavMode::Holding { .. }
        ) || self.view.index.is_empty()
        {
            return;
        }
        let top = visual_top(self.state, self.view, Some(at));
        self.state.motion = None;
        self.state.mode = mode_for(self.view.position_at(top));
        self.invalidate();
    }

    /// T21: restore a saved reading position.
    fn restore(&mut self, position: ReadingPosition, at: Instant) {
        match position {
            ReadingPosition::Latest => {
                if self.view.is_live() {
                    self.cancel_pending();
                    self.state.motion = None;
                    self.state.mode = NavMode::Following;
                    self.invalidate();
                } else {
                    self.jump_to_latest(
                        JumpGoal::Restore {
                            position: ReadingPosition::Latest,
                        },
                        at,
                    );
                }
            }
            ReadingPosition::At { sequence, anchor } => {
                self.cancel_pending();
                if !self.settle_restored(sequence, &anchor) {
                    self.request_window(
                        WindowRequest::Around { sequence },
                        JumpGoal::Restore {
                            position: ReadingPosition::At { sequence, anchor },
                        },
                    );
                }
            }
        }
    }

    /// Apply a restored anchor when it (or its sequence) is resident. Returns whether it was.
    fn settle_restored(&mut self, sequence: u64, anchor: &ReadingAnchor) -> bool {
        let anchor = if self.view.contains(&anchor.unit) {
            anchor.clone()
        } else if let Some(unit) = self
            .view
            .facts
            .unit_for_sequence(sequence)
            .filter(|unit| self.view.contains(unit))
        {
            ReadingAnchor::top(unit)
        } else {
            return false;
        };
        self.state.motion = None;
        let tail = ListPosition::TailSpace {
            anchor: anchor.clone(),
        };
        let overscrolled = self.view.is_live()
            && self
                .view
                .resolve(&tail)
                .is_some_and(ResolvedPosition::is_overscrolled);
        self.state.mode = if overscrolled {
            NavMode::TailSpace { anchor }
        } else {
            NavMode::Reading { anchor }
        };
        self.invalidate();
        true
    }

    // ----- content and results -------------------------------------------------------------

    /// T1, T4–T7, T11, T23 and automatic policy after a content change (N11).
    fn content(&mut self, changes: &TranscriptChanges, input: &NavInput, at: Instant) {
        self.repair();
        for change in &changes.changes {
            if let TranscriptChange::Removed { ids } = change {
                // C8: a cleared submission never anchors.
                for id in ids {
                    self.state
                        .pending_reveals
                        .remove(&UnitKey::Item(id.clone()));
                }
            }
        }
        // T11: content filled the virtual space to the viewport bottom, regardless of grace.
        if let NavMode::TailSpace { .. } = self.state.mode
            && self.state.motion.is_none()
            && self.view.is_live()
            && self
                .view
                .resolve(&self.state.resting_position())
                .is_some_and(|resolved| !resolved.is_overscrolled())
        {
            self.state.mode = NavMode::Following;
            self.invalidate();
        }
        self.apply_auto(input, at);
        // W7a: pending movement resumes once a page may be requested again.
        if let Some(pending) = self.state.pending_movement {
            self.request_page(pending.direction.page_direction());
        }
    }

    /// Apply automatic decisions for an input (N9).
    fn apply_auto(&mut self, input: &NavInput, at: Instant) {
        let actions = automatic_actions(self.state, input, self.view);
        if matches!(
            input,
            NavInput::Content { .. }
                | NavInput::Timer {
                    key: NavTimer::GraceExpired,
                    ..
                }
        ) && self.state.grace_until.is_some_and(|until| until <= at)
        {
            // T24: grace ended; the re-evaluation above ran once against current content.
            self.state.grace_until = None;
        }
        for action in actions {
            match action {
                AutoAction::Admit(unit) => {
                    self.state.deferred.remove(&unit);
                    self.state.admitted.insert(unit);
                }
                AutoAction::Defer(unit) => {
                    self.state.deferred.insert(unit);
                }
                AutoAction::Reveal { item, sticky } => {
                    self.state.pending_jump = None;
                    self.state.pending_movement = None;
                    self.hold(item, sticky, at);
                }
                AutoAction::QueueReveal { item } => {
                    self.state.pending_reveals.clear();
                    self.state
                        .pending_reveals
                        .insert(item, RevealRequest { requested: at });
                }
                AutoAction::LoadTailForReveal { item } => {
                    self.state.pending_reveals.clear();
                    self.state
                        .pending_reveals
                        .insert(item.clone(), RevealRequest { requested: at });
                    self.state.pending_movement = None;
                    self.request_window(WindowRequest::Tail, JumpGoal::Submission { item });
                }
                AutoAction::Unstick => {
                    if let NavMode::Holding { sticky, .. } = &mut self.state.mode {
                        *sticky = false;
                    }
                }
                AutoAction::Handoff => {
                    // T4: follow; a running reveal is retargeted from where it is drawn.
                    let from_top = visual_top(self.state, self.view, Some(at));
                    let animating = self.state.motion.is_some();
                    self.state.mode = NavMode::Following;
                    if animating {
                        self.begin_motion(from_top, ScrollTarget::LatestBottom, at);
                    } else {
                        self.invalidate();
                    }
                }
            }
        }
    }

    /// T26 and boundary-row correspondence for an incorporated page (N8, C28, C29).
    fn page_result(&mut self, direction: PageDirection, inserted_rows: u64, exhausted: bool) {
        self.repair();
        if direction == PageDirection::Older && inserted_rows > 0 {
            self.keep_older_boundary_reader(inserted_rows);
        }
        self.admit_page(direction);
        self.invalidate();
        let Some(pending) = self.state.pending_movement else {
            return;
        };
        // Every page that reaches the reducer was accepted by the window manager for the
        // current generation (stale results are dropped before reduction), so any page on the
        // movement's side satisfies it, including one requested after a generation bump (an
        // eviction that cancelled a page, an abandoned replacement) or by prefetch (N8).
        if pending.direction.page_direction() != direction {
            return;
        }
        let spend = pending.rows.min(inserted_rows);
        let remainder = pending.rows - spend;
        if spend > 0
            && !matches!(
                self.state.mode,
                NavMode::Following | NavMode::Holding { .. }
            )
        {
            let top = resting_top(self.state, self.view).unwrap_or(0);
            let new_top = match direction {
                PageDirection::Older => top.saturating_sub(spend),
                PageDirection::Newer => (top + spend).min(self.view.max_content_top()),
            };
            self.state.mode = mode_for(self.view.position_at(new_top));
        }
        if remainder == 0 || exhausted {
            self.state.pending_movement = None;
        } else {
            self.state.pending_movement = Some(PendingMovement {
                rows: remainder,
                ..pending
            });
            self.request_page(direction);
        }
    }

    /// The `Older` boundary row stays at row 0 while a page is inserted below it. A reader
    /// anchored on it keeps the content it was reading in place: the anchor moves down by the
    /// inserted rows (C29, BUG-A1).
    fn keep_older_boundary_reader(&mut self, inserted_rows: u64) {
        let view = self.view;
        let shifted = |anchor: &ReadingAnchor| -> Option<ReadingAnchor> {
            let UnitKey::Boundary(crate::units::BoundaryEdge::Older) = anchor.unit else {
                return None;
            };
            let row = match anchor.offset {
                AnchorOffset::Row(row) => row,
                AnchorOffset::Source { row_delta, .. } => row_delta,
            };
            let start = view.unit_offset(&anchor.unit)?;
            let height = view.unit_height(&anchor.unit)?;
            let top = start + row.min(height.saturating_sub(1)) + inserted_rows;
            view.anchor_at(top)
        };
        match &mut self.state.mode {
            NavMode::Reading { anchor } | NavMode::TailSpace { anchor } => {
                if let Some(next) = shifted(anchor) {
                    *anchor = next;
                }
            }
            NavMode::Following | NavMode::Holding { .. } => {}
        }
        if let Some(motion) = &mut self.state.motion
            && let Some(next) = shifted(&motion.from)
        {
            motion.from = next;
        }
    }

    /// Units a page made resident are history, not arrivals: admit them silently (N10).
    fn admit_page(&mut self, direction: PageDirection) {
        self.state
            .deferred
            .retain(|key| self.view.index.contains_key(key));
        let admitted = &mut self.state.admitted;
        admitted.retain(|key| self.view.index.contains_key(key));
        if admitted.is_empty() {
            return;
        }
        let keys: Vec<&UnitKey> = self.view.unit_keys().collect();
        let region: Vec<UnitKey> = match direction {
            PageDirection::Older => keys
                .iter()
                .take_while(|key| !admitted.contains(**key))
                .map(|key| (*key).clone())
                .collect(),
            PageDirection::Newer => keys
                .iter()
                .rev()
                .take_while(|key| !admitted.contains(**key))
                .map(|key| (*key).clone())
                .collect(),
        };
        for key in region {
            if !key.is_boundary() && !key.is_local() && self.view.is_visible(&key) {
                admitted.insert(key);
            }
        }
    }

    /// T14, T16, T18, T21 and T27 completion once the requested window is incorporated.
    fn window_result(&mut self, generation: u64, target: &WindowTarget) {
        // A replaced window has no arrivals: everything resident is admitted silently.
        let view = self.view;
        self.state
            .admitted
            .retain(|key| view.index.contains_key(key));
        for key in view.unit_keys() {
            if !key.is_boundary() && !key.is_local() && view.is_visible(key) {
                self.state.admitted.insert(key.clone());
            }
        }
        self.state.deferred.clear();
        // A replaced window moves both boundaries: movement waiting at the old one is void.
        self.state.pending_movement = None;
        self.invalidate();
        let matches = self
            .state
            .pending_jump
            .as_ref()
            .is_some_and(|jump| &jump.request == target && generation >= jump.generation);
        if !matches {
            // Cancelled or superseded jump (C14): drop its result, keep the reader keyed.
            self.repair();
            return;
        }
        let Some(jump) = self.state.pending_jump.take() else {
            return;
        };
        self.state.motion = None;
        self.state.pending_movement = None;
        match jump.goal {
            JumpGoal::Latest
            | JumpGoal::Restore {
                position: ReadingPosition::Latest,
            } => self.state.mode = NavMode::Following,
            JumpGoal::SessionStart => {
                self.state.mode =
                    view.first_unit()
                        .map_or(NavMode::Following, |first| NavMode::Reading {
                            anchor: ReadingAnchor::top(first.clone()),
                        });
            }
            JumpGoal::Sequence {
                sequence,
                placement,
            } => {
                let unit = view
                    .facts
                    .unit_for_sequence(sequence)
                    .filter(|unit| view.contains(unit));
                let placed =
                    unit.is_some_and(|unit| self.place(&unit, placement, ApproachEdge::Top, None));
                if !placed {
                    self.repair();
                }
            }
            JumpGoal::Restore {
                position: ReadingPosition::At { sequence, anchor },
            } => {
                if !self.settle_restored(sequence, &anchor) {
                    self.repair();
                }
            }
            JumpGoal::Submission { item } => {
                // T27 → T2 once the tail window is incorporated.
                let request = self.state.pending_reveals.remove(&item);
                if view.is_live() && view.is_visible(&item) {
                    self.state.pending_reveals.clear();
                    self.state.admitted.insert(item.clone());
                    self.state.mode = NavMode::Holding {
                        item,
                        sticky: false,
                    };
                } else {
                    self.repair();
                    if let Some(request) = request {
                        self.state.pending_reveals.insert(item, request);
                    }
                }
            }
        }
    }

    // ----- timers --------------------------------------------------------------------------

    /// T24 and T25.
    fn timer(&mut self, key: NavTimer, input: &NavInput, at: Instant) {
        match key {
            NavTimer::GraceExpired => {
                if self.state.grace_until.is_some_and(|until| until <= at) {
                    self.apply_auto(input, at);
                    self.state.grace_until = None;
                }
            }
            NavTimer::MotionFrame => {
                let Some(motion) = self.state.motion.clone() else {
                    return;
                };
                self.invalidate();
                let sampled = bmux_motion(&motion);
                // C22: switching to reduced motion mid-animation settles at the target.
                if motion.is_done(at) || self.view.motion.duration.is_zero() {
                    self.complete_motion(&motion, at);
                } else if let Some(next) = sampled.next_frame(at, self.view.motion.frame_interval) {
                    self.effects.push(NavEffect::ScheduleTimer {
                        key: NavTimer::MotionFrame,
                        at: next,
                    });
                }
            }
        }
    }

    /// T25 completion: the keyed target is resolved against current geometry (N7).
    fn complete_motion(&mut self, motion: &ScrollMotion, at: Instant) {
        let from_top = visual_top(self.state, self.view, Some(at));
        self.state.motion = None;
        if matches!(
            self.state.mode,
            NavMode::Reading { .. } | NavMode::TailSpace { .. }
        ) && !matches!(motion.target, ScrollTarget::LatestBottom)
        {
            let position = self.final_position(&motion.target, from_top);
            self.state.mode = mode_for(position);
        }
    }

    // ----- correspondence ------------------------------------------------------------------

    /// P4 / U2 / N12: remap anchors whose unit is no longer resident.
    fn repair(&mut self) {
        let view = self.view;
        if view.index.is_empty() {
            self.state.mode = NavMode::Following;
            self.state.motion = None;
            return;
        }
        let remapped = match &self.state.mode {
            NavMode::Reading { anchor } if !view.contains(&anchor.unit) => Some(NavMode::Reading {
                anchor: self.remap(anchor),
            }),
            NavMode::TailSpace { anchor } if !view.contains(&anchor.unit) => {
                Some(self.detached_mode(self.remap(anchor)))
            }
            NavMode::Holding { item, .. } if !view.contains(item) => {
                Some(self.detached_mode(self.remap(&ReadingAnchor::top(item.clone()))))
            }
            _ => None,
        };
        if let Some(mode) = remapped {
            self.state.mode = mode;
            self.invalidate();
        }
        self.settle_clamped_reading();
        if let Some(motion) = &self.state.motion {
            let target_unit = match &motion.target {
                ScrollTarget::ItemTop(unit) | ScrollTarget::ItemBottom(unit) => Some(unit),
                ScrollTarget::Anchor(anchor) => Some(&anchor.unit),
                ScrollTarget::LatestBottom => None,
            };
            if target_unit.is_some_and(|unit| !view.contains(unit)) {
                let anchor = match &motion.target {
                    ScrollTarget::Anchor(anchor) => anchor.clone(),
                    ScrollTarget::ItemTop(unit) | ScrollTarget::ItemBottom(unit) => {
                        ReadingAnchor::top(unit.clone())
                    }
                    ScrollTarget::LatestBottom => return,
                };
                let target = ScrollTarget::Anchor(self.remap(&anchor));
                if let Some(motion) = &mut self.state.motion {
                    motion.target = target;
                }
            }
            let from = self.state.motion.as_ref().map(|motion| motion.from.clone());
            if let Some(from) = from.filter(|from| !view.contains(&from.unit)) {
                let from = self.remap(&from);
                if let Some(motion) = &mut self.state.motion {
                    motion.from = from;
                }
            }
        }
    }

    /// A reading anchor past the content bottom resolves clamped (no blank rows while
    /// reading). Re-key it where it is drawn, so later growth below does not move the reader.
    fn settle_clamped_reading(&mut self) {
        let NavMode::Reading { anchor } = &self.state.mode else {
            return;
        };
        let view = self.view;
        let drawn = view.resolve(&ListPosition::Anchored {
            anchor: anchor.clone(),
        });
        let requested = view.resolve(&ListPosition::TailSpace {
            anchor: anchor.clone(),
        });
        if let (Some(drawn), Some(requested)) = (drawn, requested)
            && drawn.top_offset < requested.top_offset
            && let Some(anchor) = view.anchor_at(drawn.top_offset)
        {
            self.state.mode = NavMode::Reading { anchor };
        }
    }

    /// `TailSpace` when the remapped anchor still shows blank rows below content in a live
    /// window, otherwise `Reading`.
    fn detached_mode(&self, anchor: ReadingAnchor) -> NavMode {
        let tail = ListPosition::TailSpace {
            anchor: anchor.clone(),
        };
        if self.view.is_live()
            && self
                .view
                .resolve(&tail)
                .is_some_and(ResolvedPosition::is_overscrolled)
        {
            NavMode::TailSpace { anchor }
        } else {
            NavMode::Reading { anchor }
        }
    }

    /// Correspondence for a removed anchor unit (P4, U2). Without correspondence the first
    /// resident unit is used; missing correspondence never implies following (C17).
    fn remap(&self, anchor: &ReadingAnchor) -> ReadingAnchor {
        let view = self.view;
        match view.facts.correspondence(&anchor.unit) {
            Some(Correspondence::Collapsed(header)) if view.contains(&header) => {
                let row = match anchor.offset {
                    AnchorOffset::Row(row) => row,
                    AnchorOffset::Source { .. } => 0,
                };
                ReadingAnchor::row(header, row)
            }
            Some(Correspondence::Next(next)) if view.contains(&next) => ReadingAnchor::top(next),
            Some(Correspondence::Previous(previous)) if view.contains(&previous) => {
                let last = view.unit_height(&previous).unwrap_or(1).saturating_sub(1);
                ReadingAnchor::row(previous, last)
            }
            _ => view
                .first_unit()
                .map_or_else(|| anchor.clone(), |first| ReadingAnchor::top(first.clone())),
        }
    }
}
