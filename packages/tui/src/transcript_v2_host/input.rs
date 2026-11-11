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
//! * The wheel scrolls (`ScrollBy`). Left-button gestures go through BMUX click-versus-drag
//!   recognition (BMUX 14): a press and release within the click slop on a disclosure header or
//!   section label toggles it, and on the latest bar jumps to the latest content (X1, FO9);
//!   dragging beyond the slop selects source text across virtualized units (C34, BMUX 13),
//!   scrolling at the viewport edges through the list's `SelectionAutoScroll` action. Pointer
//!   input never moves keyboard focus (FO2, FO9).
//! * Markdown contributions ([`super::markdown`]): a click on a link, footnote, `<details>`
//!   summary or Mermaid diagram focuses and activates it; the chat-scope `tui.markdown.*` keys
//!   move, activate and copy the Markdown focus from either keyboard focus without changing
//!   it (FO1). A click on the open Mermaid source view does nothing.
//! * A press on the optional scrollbar track and the drag that follows go to the BMUX list,
//!   whose scrollbar reports a fraction of the whole session that navigation maps to a
//!   resident row or an `Around` window (`JumpToFraction`, VP4, W12, X6).

use std::time::Instant;

use bcode_presentation_policy::models::Disclosure;
use bcode_tui_transcript::engine::LIST_ID;
use bcode_tui_transcript::focus::{FocusEdge, FocusStep};
use bcode_tui_transcript::navigation::{InputSource, NavIntent};
use bcode_tui_transcript::selection::{
    SELECTION_SCOPE, TranscriptSelection, TranscriptSelectionEndpoint,
};
use bcode_tui_transcript::units::structure::DisclosureTarget;
use bmux_keyboard::KeyStroke;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::gesture::{PointerGesture, PointerGestureRecognizer};
use bmux_tui::hit::{HitId, HitMap, HitRegion, HitRole};
use bmux_tui::selection::{
    SelectionAffinity, SelectionController, SelectionGranularity, SelectionOutcome, SelectionScene,
    SelectionScopeId, SelectionScrollDirection,
};

use super::TranscriptV2Host;
use super::focus::{FocusKeyAction, ScrollKey, TranscriptKey, resolve_key};
use super::markdown::resolve_markdown_key;
use super::paint::{ITEM_HIT_PREFIX, LATEST_BAR_HIT, item_hit_regions};
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
}

/// Pointer context of one event from the host shell's committed presentation.
#[derive(Debug, Clone, Copy)]
pub struct PointerInput<'a> {
    /// Configured rows per wheel tick.
    pub wheel_rows: usize,
    /// Id of the topmost hit region the presented frame registered at the pointer; a press
    /// under another painter's region (an overlay) is not the transcript's (B7).
    pub top_hit: Option<&'a str>,
    /// Committed selection scene the transcript's fragments were registered in; without one
    /// nothing is selectable.
    pub scene: Option<&'a SelectionScene>,
    /// Whether a drag at the viewport edges scrolls (`[tui.selection]` autoscroll).
    pub autoscroll: bool,
}

/// What selection gestures did since the host shell last asked ([`TranscriptV2Host::take_selection_events`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SelectionEvents {
    /// A new gesture started: the previous selection is gone and a pending post-copy clear no
    /// longer applies (C35).
    pub started: bool,
    /// The selection changed while dragging.
    pub changed: bool,
    /// A drag ended with a non-empty selection (auto-copy on release, C35).
    pub completed: bool,
}

/// Left-button pointer state: click-versus-drag recognition (BMUX 14) and the selection
/// gesture it feeds (BMUX 13).
#[derive(Debug, Clone, Default)]
pub struct PointerState {
    gesture: PointerGestureRecognizer,
    controller: SelectionController,
    drag: Option<SelectionDrag>,
    events: SelectionEvents,
}

impl PointerState {
    /// Whether a left-button gesture is in progress.
    #[must_use]
    pub const fn is_pressed(&self) -> bool {
        self.gesture.is_pressed()
    }

    /// Abandon the gesture in progress (a scrollbar drag took the press).
    fn cancel(&mut self) {
        let _ = self.gesture.cancel();
        let _ = self.controller.clear();
        self.drag = None;
    }
}

/// The selection gesture in progress: its locked scope and logical anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionDrag {
    scope: SelectionScopeId,
    anchor: TranscriptSelectionEndpoint,
}

impl TranscriptV2Host {
    /// Whether the transcript holds keyboard focus (FO1). While it does, transcript focus keys
    /// are routed here ahead of inline interactions and completion.
    #[must_use]
    pub const fn transcript_focus_active(&self) -> bool {
        self.engine.focus().transcript_target().is_some()
    }

    /// Before shell routing: with transcript focus, a paste or a key that is not a transcript
    /// key returns focus to the composer, which then receives it (FO3). A user-bound
    /// `tui.tools.*` key is a transcript key: it keeps focus and acts on the focused subtree
    /// ([`Self::handle_tools_key`], FO6). Returns whether focus was released.
    pub fn release_focus_for_event(&mut self, keymap: &BmuxKeyMap, event: &Event) -> bool {
        if !self.transcript_focus_active() {
            return false;
        }
        let releases = match event {
            Event::Key(stroke) => {
                resolve_key(keymap, *stroke, true).is_none()
                    && resolve_markdown_key(keymap, *stroke).is_none()
                    && super::focus::tools_route(keymap, *stroke).is_none()
            }
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
        // A focused shell screen takes its keys first (C39).
        if let Some(outcome) = self.shell_screen_key(keymap, stroke, at) {
            return outcome;
        }
        let focused = self.transcript_focus_active();
        let Some(key) = resolve_key(keymap, stroke, focused) else {
            // Markdown keys work from either focus and keep it (FO1).
            if let Some(markdown) = resolve_markdown_key(keymap, stroke) {
                return TranscriptInputOutcome::from_changed(self.markdown_key(markdown, at));
            }
            if focused {
                let _ = self.release_focus();
            }
            return TranscriptInputOutcome::Ignored;
        };
        // Focus-kind changes, the inspector overlay and tooling notices repaint the whole
        // surface; navigation, focus traversal and disclosure stay in the transcript (F5).
        self.full_damage |= match key {
            TranscriptKey::ToggleFocus | TranscriptKey::CycleDetailAll => true,
            TranscriptKey::Scroll(_) => false,
            TranscriptKey::Focus(action) => matches!(
                action,
                FocusKeyAction::CycleDetail
                    | FocusKeyAction::Inspect
                    | FocusKeyAction::InspectorScrollDown
                    | FocusKeyAction::InspectorScrollUp
                    | FocusKeyAction::MakeDefault
                    | FocusKeyAction::Exit
            ),
        };
        match key {
            // `alt+u` is always the transcript's, even with no target to focus.
            TranscriptKey::ToggleFocus => {
                TranscriptInputOutcome::from_changed(self.toggle_focus(at))
            }
            // Outside transcript focus detail cycling applies to every unit (FO6).
            TranscriptKey::CycleDetailAll => {
                TranscriptInputOutcome::from_changed(self.cycle_detail(at))
            }
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
    /// Each runs the same handler as its key: "Focus transcript units" enters or
    /// leaves transcript focus exactly like `alt+u` (FO2, FO3); "Inspect" enters focus if
    /// needed and opens the inspector; detail cycling and the bulk disclosure actions apply
    /// to the focused subtree with transcript focus and to every unit otherwise (FO6).
    /// Returns `None` for routes that are not transcript routes.
    pub fn run_transcript_route(
        &mut self,
        route: &str,
        at: Instant,
    ) -> Option<TranscriptRouteOutcome> {
        let known = matches!(
            route,
            "transcript.focusUnits"
                | "transcript.cycleDetail"
                | "transcript.inspect"
                | "transcript.makeDefault"
                | "tools.expandAll"
                | "tools.collapseAll"
                | "tools.resetDisclosure"
        );
        self.full_damage |= known;
        Some(TranscriptRouteOutcome::Ran(match route {
            "transcript.focusUnits" => self.toggle_focus(at),
            "transcript.cycleDetail" => self.cycle_detail(at),
            "transcript.inspect" => {
                let entered = !self.transcript_focus_active() && self.enter_focus(at);
                if self.inspector_open() {
                    entered
                } else {
                    self.toggle_inspector() || entered
                }
            }
            "transcript.makeDefault" => self.make_focused_default(),
            "tools.expandAll" => self.set_all_disclosure(Disclosure::Expanded, at),
            "tools.collapseAll" => self.set_all_disclosure(Disclosure::Collapsed, at),
            "tools.resetDisclosure" => self.reset_disclosure(at),
            _ => return None,
        }))
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

    /// Route one pointer event without a committed selection scene (nothing is selectable);
    /// see [`Self::handle_pointer`].
    pub fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        wheel_rows: usize,
        top_hit: Option<&str>,
        at: Instant,
    ) -> TranscriptInputOutcome {
        self.handle_pointer(
            mouse,
            PointerInput {
                wheel_rows,
                top_hit,
                scene: None,
                autoscroll: false,
            },
            at,
        )
    }

    /// Route one pointer event (terminal coordinates): wheel scrolling, and left-button
    /// gestures inside the transcript region, which click disclosure regions and the latest
    /// bar or select text (X1, C34). A press or release over a region another painter put above
    /// the transcript (an overlay) is not a transcript click (B7). Pointer input never moves
    /// keyboard focus (FO2, FO9); a press in the composer returns focus to it (FO3) and is left
    /// to the shell.
    pub fn handle_pointer(
        &mut self,
        mouse: MouseEvent,
        input: PointerInput<'_>,
        at: Instant,
    ) -> TranscriptInputOutcome {
        let in_region = self
            .regions
            .transcript
            .is_some_and(|region| region.contains(mouse.position));
        // The transcript's own regions (its items, the list's scroll and item regions, the
        // latest bar) are the transcript's; anything else on top is another painter's.
        let presented = input
            .top_hit
            .is_none_or(|id| id == LATEST_BAR_HIT || is_transcript_hit(id));
        // The open inspector panel lies above the transcript and owns input over it (B7).
        if let Some(changed) = self.inspector_mouse(mouse, input.top_hit) {
            self.full_damage |= changed;
            return TranscriptInputOutcome::from_changed(changed);
        }
        if let Some(outcome) = self.scrollbar_mouse(mouse, input.top_hit, at) {
            return outcome;
        }
        // Shell screens take horizontal wheel input and right-button focus (C39).
        if in_region
            && presented
            && let Some(outcome) = self.shell_screen_pointer(mouse, at)
        {
            return outcome;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if in_region => {
                let rows = i64::try_from(input.wheel_rows.max(1)).unwrap_or(i64::MAX);
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
                TranscriptInputOutcome::from_changed(self.pointer_press(mouse, input.scene))
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                if self.pointer.is_pressed() =>
            {
                TranscriptInputOutcome::from_changed(
                    self.pointer_continue(mouse, input, presented, at),
                )
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

    /// What selection gestures did since the last call, then reset.
    pub const fn take_selection_events(&mut self) -> SelectionEvents {
        std::mem::replace(
            &mut self.pointer.events,
            SelectionEvents {
                started: false,
                changed: false,
                completed: false,
            },
        )
    }

    /// The selected logical source text (C34), if any.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        self.engine.selected_text()
    }

    /// Clear the transcript selection and any selection gesture. Returns whether a selection
    /// was shown.
    pub fn clear_selection(&mut self) -> bool {
        self.pointer.drag = None;
        let _ = self.pointer.controller.clear();
        let update = self.engine.set_selection(None, false);
        let changed = update.invalidated;
        self.absorb(update);
        changed
    }

    /// A left press in the transcript: it replaces any previous selection (C35) and arms a
    /// selection anchored at the press when it lands on or near selectable text.
    fn pointer_press(&mut self, mouse: MouseEvent, scene: Option<&SelectionScene>) -> bool {
        let empty = SelectionScene::new();
        let scene = scene.unwrap_or(&empty);
        let hits = self.frame_hits();
        let had_selection = self.clear_selection();
        self.pointer.controller = SelectionController::new();
        let route = self.pointer.gesture.route_selection(
            &hits,
            None,
            &mut self.pointer.controller,
            scene,
            mouse,
        );
        self.pointer.events.started = true;
        if route.selection == SelectionOutcome::Armed
            && let Some(drag) = self.selection_anchor(scene, mouse.position)
        {
            let collapsed = TranscriptSelection::collapsed(drag.anchor.clone());
            self.pointer.drag = Some(drag);
            let update = self.engine.set_selection(Some(collapsed), true);
            self.absorb(update);
        }
        had_selection
    }

    /// A drag or release of the left-button gesture in progress.
    fn pointer_continue(
        &mut self,
        mouse: MouseEvent,
        input: PointerInput<'_>,
        presented: bool,
        at: Instant,
    ) -> bool {
        let empty = SelectionScene::new();
        let scene = input.scene.unwrap_or(&empty);
        let hits = self.frame_hits();
        let route = self.pointer.gesture.route_selection(
            &hits,
            None,
            &mut self.pointer.controller,
            scene,
            mouse,
        );
        match route.gesture {
            PointerGesture::Click { region, position } => {
                self.discard_collapsed_selection();
                presented && self.activate(&region, position, at)
            }
            PointerGesture::Tap { .. } | PointerGesture::Cancelled => {
                self.discard_collapsed_selection();
                false
            }
            PointerGesture::DragStarted { position, .. } | PointerGesture::Dragged { position } => {
                let mut changed = self.extend_selection(scene, position, true);
                if let SelectionOutcome::Changed {
                    auto_scroll: Some(request),
                } = &route.selection
                    && input.autoscroll
                    && self.at_viewport_edge(position, request.direction)
                {
                    let update = self.engine.selection_auto_scroll(request, at);
                    changed |= update.invalidated || !update.requests.is_empty();
                    self.absorb(update);
                }
                changed
            }
            PointerGesture::DragEnded { position } => {
                let changed = self.extend_selection(scene, position, false);
                self.pointer.drag = None;
                if self
                    .engine
                    .selection()
                    .is_some_and(TranscriptSelection::is_collapsed)
                {
                    self.discard_collapsed_selection();
                } else if self.engine.selection().is_some() {
                    self.pointer.events.completed = true;
                }
                changed
            }
            PointerGesture::Ignored
            | PointerGesture::Pressed { .. }
            | PointerGesture::Holding { .. } => false,
        }
    }

    /// Move the selection focus to the text nearest `point` in the locked scope; `gesture`
    /// keeps the gesture active. Returns whether the painted selection changed.
    fn extend_selection(&mut self, scene: &SelectionScene, point: Point, gesture: bool) -> bool {
        let Some(drag) = self.pointer.drag.clone() else {
            return false;
        };
        let focus = self
            .selection_focus(scene, &drag, point)
            .unwrap_or_else(|| {
                self.engine.selection().map_or_else(
                    || drag.anchor.clone(),
                    |selection| selection.focus().clone(),
                )
            });
        let update = self
            .engine
            .set_selection(Some(TranscriptSelection::new(drag.anchor, focus)), gesture);
        let changed = update.invalidated;
        self.absorb(update);
        self.pointer.events.changed |= changed;
        changed
    }

    /// A gesture that never selected anything leaves no selection.
    fn discard_collapsed_selection(&mut self) {
        self.pointer.drag = None;
        if self
            .engine
            .selection()
            .is_some_and(TranscriptSelection::is_collapsed)
        {
            let update = self.engine.set_selection(None, false);
            self.absorb(update);
        }
    }

    /// The selection gesture a press at `point` starts: the scope it locks to (a unit's own
    /// text, or the whole transcript from the space around units, C34) and the logical anchor
    /// before the grapheme nearest the press.
    fn selection_anchor(&self, scene: &SelectionScene, point: Point) -> Option<SelectionDrag> {
        let scope = scene.initiation_scope(point)?;
        if !is_transcript_scope(&scope.id) {
            return None;
        }
        let endpoint = scene.endpoint_at_granularity(
            &scope.id,
            point,
            SelectionAffinity::Before,
            SelectionGranularity::Grapheme,
        )?;
        let anchor = self
            .engine
            .selection_endpoint(self.pointer_frame()?, &endpoint)?;
        Some(SelectionDrag {
            scope: scope.id.clone(),
            anchor,
        })
    }

    /// The logical focus at `point`: after the nearest grapheme when it follows the anchor,
    /// before it when it precedes the anchor.
    fn selection_focus(
        &self,
        scene: &SelectionScene,
        drag: &SelectionDrag,
        point: Point,
    ) -> Option<TranscriptSelectionEndpoint> {
        let frame = self.pointer_frame()?;
        let resolve = |affinity| {
            let endpoint = scene.endpoint_at_granularity(
                &drag.scope,
                point,
                affinity,
                SelectionGranularity::Grapheme,
            )?;
            self.engine.selection_endpoint(frame, &endpoint)
        };
        let after = resolve(SelectionAffinity::After)?;
        if self
            .engine
            .compare_selection_endpoints(&after, &drag.anchor)
            == Some(std::cmp::Ordering::Less)
        {
            resolve(SelectionAffinity::Before)
        } else {
            Some(after)
        }
    }

    /// The frame pointer input resolves selection endpoints against: the committed frame,
    /// whose scene the host shell passes, else the last prepared one.
    fn pointer_frame(&self) -> Option<&bcode_tui_transcript::frame::FrameSnapshot> {
        self.committed_frame.as_ref().or(self.last_frame.as_ref())
    }

    /// Whether `point` is on the viewport edge row the autoscroll `direction` moves past.
    fn at_viewport_edge(&self, point: Point, direction: SelectionScrollDirection) -> bool {
        self.regions
            .viewport
            .is_some_and(|viewport| match direction {
                SelectionScrollDirection::Backward => point.y <= viewport.y,
                SelectionScrollDirection::Forward => point.y.saturating_add(1) >= viewport.bottom(),
            })
    }

    /// The transcript's interactive regions of the last prepared frame, in terminal cells:
    /// the regions it registered while painting (B3), for click recognition.
    fn frame_hits(&self) -> HitMap {
        let mut hits = HitMap::new();
        if let (Some(frame), Some(viewport)) = (self.last_frame.as_ref(), self.regions.viewport) {
            for (index, rect) in item_hit_regions(frame) {
                hits.push(
                    HitRegion::new(
                        format!("{ITEM_HIT_PREFIX}{index}"),
                        Rect::new(
                            viewport.x.saturating_add(rect.x),
                            viewport.y.saturating_add(rect.y),
                            rect.width,
                            rect.height,
                        ),
                    )
                    .role(HitRole::Action)
                    .layer(1),
                );
            }
        }
        if let Some(bar) = self.regions.latest_bar {
            hits.push(
                HitRegion::new(LATEST_BAR_HIT, bar)
                    .role(HitRole::ListItem)
                    .layer(1),
            );
        }
        hits
    }

    /// Activate a clicked region: the latest bar jumps to the latest content, an item region
    /// toggles its disclosure (X1).
    fn activate(&mut self, region: &HitId, position: Point, at: Instant) -> bool {
        if region.as_str() == LATEST_BAR_HIT {
            return self.jump_to_edge(FocusEdge::Last, at);
        }
        self.click(position, at)
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
        let (update, handled) = self.engine.list_event(&Event::Mouse(local), at);
        let changed = update.invalidated || !update.requests.is_empty();
        self.absorb(update);
        if pressed {
            if !handled {
                return None;
            }
            self.scrollbar_drag = true;
            self.pointer.cancel();
        } else if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.scrollbar_drag = false;
        }
        Some(TranscriptInputOutcome::from_changed(changed))
    }

    /// Activate what a click at `position` hits: the latest bar jumps to the latest content,
    /// a disclosure region toggles (X1), a Markdown contribution is focused and activated.
    /// Returns whether the transcript changed. The scrollbar track and the Mermaid source view
    /// are never an item click.
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
            || self.markdown_overlay_contains(position)
        {
            return false;
        }
        let column = u64::from(position.x - viewport.x);
        let row = u64::from(position.y - viewport.y);
        let (update, hit) = self.engine.click(frame, column, row, at);
        let mut changed = update.invalidated;
        self.absorb(update);
        if let Some(bcode_tui_transcript::engine::ItemHit {
            unit,
            action: bcode_tui_transcript::layout::geometry::ItemAction::Activate(key),
        }) = hit
        {
            changed |= self.activate_markdown_hit(&unit, &key, at);
        }
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
                let (update, disclosure) = host.engine.toggle_disclosure(&target, at);
                host.absorb(update);
                disclosure.is_some()
            }),
            FocusKeyAction::Expand => self.expand_focused(at),
            FocusKeyAction::Collapse => self.collapse_or_focus_parent(at),
            // `e` on a shell screen toggles its inline expansion (C39), else it expands the
            // focused subtree.
            FocusKeyAction::ExpandAll => {
                let shell = self.toggle_focused_shell(at);
                let subtree =
                    shell.is_none_or(|_| self.shell_expanded()) && self.expand_focused_subtree(at);
                shell.unwrap_or(false) || subtree
            }
            FocusKeyAction::CycleDetail => self.cycle_detail(at),
            FocusKeyAction::Inspect => self.toggle_inspector(),
            FocusKeyAction::InspectorScrollDown => {
                self.scroll_inspector(crate::inspector::INSPECTOR_SCROLL_ROWS)
            }
            FocusKeyAction::InspectorScrollUp => {
                self.scroll_inspector(-crate::inspector::INSPECTOR_SCROLL_ROWS)
            }
            FocusKeyAction::MakeDefault => self.make_focused_default(),
            // Escape closes the inspector before it leaves transcript focus (FO3).
            FocusKeyAction::Exit => self.close_inspector() || self.release_focus(),
            FocusKeyAction::SessionStart => self.jump_to_edge(FocusEdge::First, at),
            FocusKeyAction::Latest => self.jump_to_edge(FocusEdge::Last, at),
        }
    }

    /// Enter transcript focus (FO2, C33). Returns whether focus entered.
    fn enter_focus(&mut self, at: Instant) -> bool {
        let (update, entered) = self.engine.enter_focus(at);
        self.absorb(update);
        self.full_damage |= entered;
        entered
    }

    /// Return keyboard focus to the composer (FO3), closing an open inspector. Returns whether
    /// the transcript had focus.
    pub fn release_focus(&mut self) -> bool {
        self.discard_inspector();
        let released = self.engine.release_focus();
        self.invalidated |= released;
        self.full_damage |= released;
        released
    }

    fn step_focus(&mut self, step: FocusStep, at: Instant) -> bool {
        let update = self.engine.focus_step(step, at);
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
        let current = self.engine.structure().effective_disclosure(&target)?;
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
        let update = self.engine.focus_parent(at);
        let changed = update.invalidated;
        self.absorb(update);
        changed
    }

    /// Expand the focused target and every disclosure nested under it.
    fn expand_focused_subtree(&mut self, at: Instant) -> bool {
        let Some(target) = self.engine.focus().transcript_target().cloned() else {
            return false;
        };
        let disclosures = self.engine.focus_view().subtree_disclosures(&target);
        let mut changed = false;
        for disclosure in disclosures {
            let (update, set) = self
                .engine
                .set_disclosure(&disclosure, Disclosure::Expanded, at);
            self.absorb(update);
            changed |= set;
        }
        changed
    }

    /// `JumpToSessionStart` / `JumpToLatest` with direct loads; with transcript focus, focus
    /// follows to the first / last target (FO4, FO5).
    fn jump_to_edge(&mut self, edge: FocusEdge, at: Instant) -> bool {
        let update = self.engine.jump_to_edge(edge, at);
        let changed = update.invalidated || !update.requests.is_empty();
        self.absorb(update);
        changed
    }

    /// Whether the focused unit's shell screen is expanded inline (C39).
    fn shell_expanded(&self) -> bool {
        self.focused_disclosure().is_some_and(|target| {
            self.engine
                .structure()
                .tools()
                .shell_expanded(target.item_id())
        })
    }

    fn focused_disclosure(&self) -> Option<DisclosureTarget> {
        self.engine.focus_state().disclosure().cloned()
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
            let (update, changed) = host.engine.set_disclosure(&target, disclosure, at);
            host.absorb(update);
            changed
        })
    }
}

#[cfg(test)]
mod tests;

/// Whether `scope` is the transcript's selection scope or one of its unit scopes.
fn is_transcript_scope(scope: &SelectionScopeId) -> bool {
    scope
        .as_str()
        .strip_prefix(SELECTION_SCOPE)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

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
