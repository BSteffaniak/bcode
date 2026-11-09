//! Transcript input and keyboard focus for the V2 engine (spec sections 3.5, 4.2 and 6).
//!
//! * `alt+u` enters transcript unit focus on the initial target (FO2, C33) and leaves it again
//!   (FO3); Escape leaves it too, and any key or paste the `transcript_focus` scope does not
//!   bind returns focus to the composer and reaches the composer (FO3,
//!   [`TranscriptV2Host::release_focus_for_event`]).
//! * Focus keys move focus (`FocusTarget` with the approach edge, C32) and change disclosure
//!   (FO6); Home/End jump to the session start / latest content and focus its first / last
//!   target (FO4).
//! * Page Up/Down, `ctrl+up`/`ctrl+down` and `ctrl+home`/`ctrl+end` scroll or jump from either
//!   focus without changing it (FO5).
//! * The wheel scrolls (`ScrollBy`); a press and release without drag on a disclosure header
//!   or section label toggles it, and on the latest bar jumps to the latest content. Pointer
//!   input never moves keyboard focus (FO2, FO9).
//! * A press on the optional scrollbar track and the drag that follows go to the BMUX list,
//!   whose scrollbar reports a fraction of the whole session that navigation maps to a
//!   resident row or an `Around` window (`JumpToFraction`, VP4, W12, X6).

use std::time::Instant;

use bcode_presentation_policy::models::Disclosure;
use bcode_tui_transcript::engine::LIST_ID;
use bcode_tui_transcript::focus::{FocusEdge, FocusStep};
use bcode_tui_transcript::navigation::{InputSource, NavIntent};
use bcode_tui_transcript::units::structure::DisclosureTarget;
use bcode_tui_transcript::units::structure::disclosure::next_detail;
use bmux_keyboard::KeyStroke;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Point;

use super::TranscriptV2Host;
use super::focus::{FocusKeyAction, ScrollKey, TranscriptKey, resolve_key};
use super::paint::{ITEM_HIT_PREFIX, LATEST_BAR_HIT};
use crate::keymap::BmuxKeyMap;

/// What the V2 transcript did with one input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptInputOutcome {
    /// Not a transcript input; the host shell (composer, overlays) handles it.
    Ignored,
    /// Consumed without a visible change.
    Consumed,
    /// Consumed; the transcript needs repainting.
    Redraw,
}

impl TranscriptInputOutcome {
    /// `None` when the host shell should handle the event, else whether to repaint.
    #[must_use]
    pub const fn consumed_redraw(self) -> Option<bool> {
        match self {
            Self::Ignored => None,
            Self::Consumed => Some(false),
            Self::Redraw => Some(true),
        }
    }

    const fn from_changed(changed: bool) -> Self {
        if changed {
            Self::Redraw
        } else {
            Self::Consumed
        }
    }
}

/// What the V2 transcript did with a transcript command-palette route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptRouteOutcome {
    /// The route ran; `true` when the transcript changed.
    Ran(bool),
    /// The route has no V2 implementation yet; nothing changed, and the hidden V1 transcript
    /// must not be changed either.
    Unavailable,
}

/// A primary-button press inside the transcript region awaiting its release (FO9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingClick {
    /// Press position in terminal cells.
    pub position: Point,
    /// The pointer moved since the press; the release is not a click.
    pub dragged: bool,
}

impl TranscriptV2Host {
    /// Whether the transcript holds keyboard focus (FO1). While it does, transcript focus keys
    /// are routed here ahead of inline interactions and completion.
    #[must_use]
    pub fn transcript_focus_active(&self) -> bool {
        self.engine.get().focus().transcript_target().is_some()
    }

    /// Before shell routing: with transcript focus, a paste or a key that is not a transcript
    /// key returns focus to the composer, which then receives it (FO3). Returns whether focus
    /// was released.
    pub fn release_focus_for_event(&mut self, keymap: &BmuxKeyMap, event: &Event) -> bool {
        if !self.transcript_focus_active() {
            return false;
        }
        let releases = match event {
            Event::Key(stroke) => resolve_key(keymap, *stroke, true).is_none(),
            Event::Paste(_) => true,
            _ => false,
        };
        releases && self.release_focus()
    }

    /// Route one key to the transcript: focus entry and exit, focus traversal and transcript
    /// navigation keys. Keys the transcript does not bind return
    /// [`TranscriptInputOutcome::Ignored`] and reach the composer; with transcript focus they
    /// first return focus to the composer (FO3).
    pub fn handle_key(
        &mut self,
        keymap: &BmuxKeyMap,
        stroke: KeyStroke,
        at: Instant,
    ) -> TranscriptInputOutcome {
        let focused = self.transcript_focus_active();
        let Some(key) = resolve_key(keymap, stroke, focused) else {
            if focused {
                let _ = self.release_focus();
            }
            return TranscriptInputOutcome::Ignored;
        };
        match key {
            // `alt+u` is always the transcript's, even with no target to focus.
            TranscriptKey::ToggleFocus => {
                TranscriptInputOutcome::from_changed(self.toggle_focus(at))
            }
            // Detail cycling of every unit is a phase-6 gap in V2; the key is consumed so the
            // hidden V1 transcript does not change.
            TranscriptKey::CycleDetailAll => TranscriptInputOutcome::Consumed,
            TranscriptKey::Scroll(scroll) => {
                TranscriptInputOutcome::from_changed(self.scroll_key(scroll, at))
            }
            TranscriptKey::Focus(action) => {
                TranscriptInputOutcome::from_changed(self.focus_key(action, at))
            }
        }
    }

    /// Run a transcript command-palette route (`transcript.focusUnits`,
    /// `transcript.cycleDetail`, `transcript.inspect`, `transcript.makeDefault`, `tools.*`).
    /// "Focus transcript units" enters or leaves transcript focus exactly like `alt+u` (FO2,
    /// FO3), and detail cycling with transcript focus cycles the focused target (FO6). Returns
    /// `None` for routes that are not transcript routes.
    pub fn run_transcript_route(
        &mut self,
        route: &str,
        at: Instant,
    ) -> Option<TranscriptRouteOutcome> {
        Some(match route {
            "transcript.focusUnits" => TranscriptRouteOutcome::Ran(self.toggle_focus(at)),
            "transcript.cycleDetail" if self.transcript_focus_active() => {
                TranscriptRouteOutcome::Ran(self.focus_key(FocusKeyAction::CycleDetail, at))
            }
            // Phase-6 gaps in V2: the structure inspector, make-default, detail cycling of
            // every unit and the bulk disclosure actions.
            "transcript.cycleDetail"
            | "transcript.inspect"
            | "transcript.makeDefault"
            | "tools.expandAll"
            | "tools.collapseAll"
            | "tools.resetDisclosure" => TranscriptRouteOutcome::Unavailable,
            _ => return None,
        })
    }

    /// Enter transcript focus, or leave it when it is active (FO2, FO3). Returns whether
    /// focus changed.
    fn toggle_focus(&mut self, at: Instant) -> bool {
        if self.transcript_focus_active() {
            self.release_focus()
        } else {
            self.enter_focus(at)
        }
    }

    /// Route one pointer event: wheel scrolling and disclosure or latest-bar clicks inside the
    /// transcript region (terminal coordinates). `wheel_rows` is the configured rows per wheel
    /// tick. `top_hit` is the id of the topmost hit region the presented frame registered at
    /// the pointer: a press or release over a region another painter put above the transcript
    /// (an overlay) is not a transcript click (B7). Pointer input never moves keyboard focus
    /// (FO2, FO9); a press in the composer returns focus to it (FO3) and is left to the shell.
    pub fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        wheel_rows: usize,
        top_hit: Option<&str>,
        at: Instant,
    ) -> TranscriptInputOutcome {
        let in_region = self
            .regions
            .transcript
            .is_some_and(|region| region.contains(mouse.position));
        let presented =
            top_hit.is_none_or(|id| id == LATEST_BAR_HIT || id.starts_with(ITEM_HIT_PREFIX));
        if let Some(outcome) = self.scrollbar_mouse(mouse, top_hit, at) {
            return outcome;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if in_region => {
                let rows = i64::try_from(wheel_rows.max(1)).unwrap_or(i64::MAX);
                let rows = if mouse.kind == MouseEventKind::ScrollUp {
                    -rows
                } else {
                    rows
                };
                TranscriptInputOutcome::from_changed(self.navigate(
                    NavIntent::ScrollBy {
                        rows,
                        source: InputSource::Wheel,
                    },
                    at,
                ))
            }
            MouseEventKind::Down(MouseButton::Left) if in_region && presented => {
                self.pending_click = Some(PendingClick {
                    position: mouse.position,
                    dragged: false,
                });
                TranscriptInputOutcome::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) if self.pending_click.is_some() => {
                if let Some(click) = &mut self.pending_click
                    && click.position != mouse.position
                {
                    click.dragged = true;
                }
                // Transcript selection is phase-6 work; the gesture stays with the transcript.
                TranscriptInputOutcome::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) if self.pending_click.is_some() => {
                let click = self.pending_click.take();
                let changed = click
                    .filter(|click| presented && !click.dragged && click.position == mouse.position)
                    .is_some_and(|click| self.click(click.position, at));
                TranscriptInputOutcome::from_changed(changed)
            }
            MouseEventKind::Down(_)
                if self
                    .regions
                    .composer
                    .is_some_and(|composer| composer.contains(mouse.position)) =>
            {
                // FO3: a press in the composer returns focus to it; the shell handles the press.
                let _ = self.release_focus();
                TranscriptInputOutcome::Ignored
            }
            _ => TranscriptInputOutcome::Ignored,
        }
    }

    /// Route scrollbar pointer input to the list (VP4): a primary press on the presented track,
    /// then every drag and the release of that gesture. Returns `None` for other input. A press
    /// under an overlay another painter put above the transcript is not the scrollbar's (B7),
    /// and a press the list does not take (an overlay scrollbar with nothing to scroll) is left
    /// to the transcript click path.
    fn scrollbar_mouse(
        &mut self,
        mouse: MouseEvent,
        top_hit: Option<&str>,
        at: Instant,
    ) -> Option<TranscriptInputOutcome> {
        let pressed = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        let continues = matches!(
            mouse.kind,
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        );
        if pressed {
            let on_track = self
                .regions
                .scrollbar
                .is_some_and(|track| track.contains(mouse.position));
            let uncovered = top_hit.is_none_or(is_transcript_hit);
            if !on_track || !uncovered {
                return None;
            }
        } else if !(continues && self.scrollbar_drag) {
            return None;
        }
        let viewport = self.regions.viewport?;
        let mut local = mouse;
        local.position = Point::new(
            mouse.position.x.saturating_sub(viewport.x),
            mouse.position.y.saturating_sub(viewport.y),
        );
        let (update, handled) = self.engine.get_mut().list_event(&Event::Mouse(local), at);
        let changed = update.invalidated || !update.requests.is_empty();
        self.absorb(update);
        if pressed {
            if !handled {
                return None;
            }
            self.scrollbar_drag = true;
            self.pending_click = None;
        } else if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.scrollbar_drag = false;
        }
        Some(TranscriptInputOutcome::from_changed(changed))
    }

    /// Activate what a click at `position` hits: the latest bar jumps to the latest content,
    /// a disclosure region toggles (X1). Returns whether the transcript changed. The scrollbar
    /// track is never an item click.
    fn click(&mut self, position: Point, at: Instant) -> bool {
        if self
            .regions
            .latest_bar
            .is_some_and(|bar| bar.contains(position))
        {
            return self.jump_to_edge(FocusEdge::Last, at);
        }
        let (Some(viewport), Some(frame)) = (self.regions.viewport, self.last_frame.as_ref())
        else {
            return false;
        };
        if !viewport.contains(position)
            || self
                .regions
                .scrollbar
                .is_some_and(|track| track.contains(position))
        {
            return false;
        }
        let column = u64::from(position.x - viewport.x);
        let row = u64::from(position.y - viewport.y);
        let (update, _hit) = self.engine.get_mut().click(frame, column, row, at);
        let changed = update.invalidated;
        self.absorb(update);
        changed
    }

    fn scroll_key(&mut self, key: ScrollKey, at: Instant) -> bool {
        match key {
            ScrollKey::Page(pages) => self.navigate(NavIntent::PageBy { pages }, at),
            ScrollKey::Line(rows) => self.navigate(
                NavIntent::ScrollBy {
                    rows,
                    source: InputSource::Key,
                },
                at,
            ),
            ScrollKey::SessionStart => self.jump_to_edge(FocusEdge::First, at),
            ScrollKey::Latest => self.jump_to_edge(FocusEdge::Last, at),
        }
    }

    fn focus_key(&mut self, action: FocusKeyAction, at: Instant) -> bool {
        match action {
            FocusKeyAction::Next => self.step_focus(FocusStep::Next, at),
            FocusKeyAction::Previous => self.step_focus(FocusStep::Previous, at),
            FocusKeyAction::Toggle => self.with_focused_disclosure(|host, target| {
                let (update, disclosure) = host.engine.get_mut().toggle_disclosure(&target, at);
                host.absorb(update);
                disclosure.is_some()
            }),
            FocusKeyAction::Expand => self.expand_focused(at),
            FocusKeyAction::Collapse => self.collapse_or_focus_parent(at),
            FocusKeyAction::ExpandAll => self.expand_focused_subtree(at),
            FocusKeyAction::CycleDetail => {
                let next = self.focused_disclosure().and_then(|target| {
                    self.engine
                        .get()
                        .structure()
                        .effective_disclosure(&target)
                        .map(next_detail)
                });
                next.is_some_and(|next| self.set_focused(next, at))
            }
            // The structure inspector and make-default are phase-6 work in V2: the keys are
            // transcript keys (FO3) and are consumed without effect.
            FocusKeyAction::Inspect
            | FocusKeyAction::InspectorScrollDown
            | FocusKeyAction::InspectorScrollUp
            | FocusKeyAction::MakeDefault => false,
            FocusKeyAction::Exit => self.release_focus(),
            FocusKeyAction::SessionStart => self.jump_to_edge(FocusEdge::First, at),
            FocusKeyAction::Latest => self.jump_to_edge(FocusEdge::Last, at),
        }
    }

    /// Enter transcript focus (FO2, C33). Returns whether focus entered.
    fn enter_focus(&mut self, at: Instant) -> bool {
        let (update, entered) = self.engine.get_mut().enter_focus(at);
        self.absorb(update);
        entered
    }

    /// Return keyboard focus to the composer (FO3). Returns whether the transcript had focus.
    pub fn release_focus(&mut self) -> bool {
        let released = self.engine.get_mut().release_focus();
        self.invalidated |= released;
        released
    }

    fn step_focus(&mut self, step: FocusStep, at: Instant) -> bool {
        let update = self.engine.get_mut().focus_step(step, at);
        let changed = update.invalidated;
        self.absorb(update);
        changed
    }

    /// The focused disclosure stepped one level with `step` (the engine's focus-mode levels),
    /// or `None` without a focused disclosure or when it cannot step further.
    fn focused_level(
        &self,
        step: impl FnOnce(&DisclosureTarget, Disclosure) -> Option<Disclosure>,
    ) -> Option<Disclosure> {
        let target = self.focused_disclosure()?;
        let current = self
            .engine
            .get()
            .structure()
            .effective_disclosure(&target)?;
        step(&target, current)
    }

    /// Expand the focused disclosure one level (`l`, groups through summary).
    fn expand_focused(&mut self, at: Instant) -> bool {
        self.focused_level(DisclosureTarget::expand_level)
            .is_some_and(|next| self.set_focused(next, at))
    }

    /// Collapse the focused disclosure one level (`h`, groups through summary), else focus its
    /// parent target.
    fn collapse_or_focus_parent(&mut self, at: Instant) -> bool {
        if self
            .focused_level(DisclosureTarget::collapse_level)
            .is_some_and(|next| self.set_focused(next, at))
        {
            return true;
        }
        let update = self.engine.get_mut().focus_parent(at);
        let changed = update.invalidated;
        self.absorb(update);
        changed
    }

    /// Expand the focused target and every disclosure nested under it.
    fn expand_focused_subtree(&mut self, at: Instant) -> bool {
        let Some(target) = self.engine.get().focus().transcript_target().cloned() else {
            return false;
        };
        let disclosures = self.engine.get().focus_view().subtree_disclosures(&target);
        let mut changed = false;
        for disclosure in disclosures {
            let (update, set) =
                self.engine
                    .get_mut()
                    .set_disclosure(&disclosure, Disclosure::Expanded, at);
            self.absorb(update);
            changed |= set;
        }
        changed
    }

    /// `JumpToSessionStart` / `JumpToLatest` with direct loads; with transcript focus, focus
    /// follows to the first / last target (FO4, FO5).
    fn jump_to_edge(&mut self, edge: FocusEdge, at: Instant) -> bool {
        let update = self.engine.get_mut().jump_to_edge(edge, at);
        let changed = update.invalidated || !update.requests.is_empty();
        self.absorb(update);
        changed
    }

    fn focused_disclosure(&self) -> Option<DisclosureTarget> {
        self.engine.get().focus_state().disclosure().cloned()
    }

    fn with_focused_disclosure(
        &mut self,
        apply: impl FnOnce(&mut Self, DisclosureTarget) -> bool,
    ) -> bool {
        self.focused_disclosure()
            .is_some_and(|target| apply(self, target))
    }

    fn set_focused(&mut self, disclosure: Disclosure, at: Instant) -> bool {
        self.with_focused_disclosure(|host, target| {
            let (update, changed) = host
                .engine
                .get_mut()
                .set_disclosure(&target, disclosure, at);
            host.absorb(update);
            changed
        })
    }
}

#[cfg(test)]
mod tests;

/// Whether hit region `id` belongs to the transcript itself rather than to a painter above it:
/// the list's scroll region ([`LIST_ID`]), the list's own per-item regions (`<LIST_ID>.item.…`,
/// which span the full list width, the overlay scrollbar column included) and the host's item
/// regions ([`ITEM_HIT_PREFIX`]).
fn is_transcript_hit(id: &str) -> bool {
    id.starts_with(ITEM_HIT_PREFIX)
        || id
            .strip_prefix(LIST_ID)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}
