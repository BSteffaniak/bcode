//! Engine façade the host calls (spec sections 3.0 and 9.1).
//!
//! The engine owns one `SessionView` per attached session (spec section 3.1), the resident
//! window manager, units, the viewport projection, the navigation state and keyboard focus. It
//! performs no I/O and reads no clock: every input carries its time, page and window requests
//! leave as [`HostRequest`]s for the host to execute, and navigation timers are kept as
//! deadlines that the host fires through [`TranscriptEngine::advance`].
//!
//! Every update follows the W3 order in one call: window operation → units → exact measurement
//! of new or revised units → `MeasuredListIndex` splice → `NavInput` reduced against the updated
//! index → navigation effects → eviction (W7) → prefetch (W5). Frame preparation
//! ([`TranscriptEngine::prepare_frame`]) and paint ([`TranscriptEngine::paint_frame`]) never
//! change navigation, the window or units (CK1); commit ([`TranscriptEngine::commit`]) records
//! the frame (CK3).

use std::collections::BTreeMap;
use std::time::Instant;

use bcode_session_view::SessionView;
use bcode_session_view_models::{TranscriptChanges, TranscriptViewItem};
use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Size;
use bmux_tui::paint::PaintCx;
use bmux_tui_components::virtual_list::NoSourceMap;

use crate::focus::ChatFocus;
use crate::frame::{FrameClock, FrameId, FrameSnapshot};
use crate::latest_bar::LatestBarState;
use crate::navigation::{
    ListPosition, MotionConfig, NavEffect, NavInput, NavIntent, NavMode, NavState, NavTimer,
    RemovedUnits, ScrollTarget, presented_position, reduce,
};
use crate::units::facts::UnitFactsView;
use crate::units::producers::{PlainTextProducer, UnitProducer};
use crate::units::structure::{FlatStructure, UnitStructure};
use crate::units::{UnitChanges, UnitKey, Units};
use crate::viewport::source::TranscriptItemSource;
use crate::viewport::{
    NavViewContext, ScrollbarMode, TranscriptViewport, ViewportGeometry, ViewportOutput,
    transcript_chrome,
};
use crate::window::{
    EvictionContext, HistoryPageResult, HistoryWindowResult, IssuedRequest, LiveInput, LiveOutcome,
    PageDirection, PageRequest, ResidentWindow, StaleResult, WindowManager, WindowRequest,
};

/// Layout id of the transcript list component.
const LIST_ID: &str = "transcript";

/// A request the host must execute (W5, W10, W11). Results come back through
/// [`TranscriptEngine::apply_page`] and [`TranscriptEngine::apply_window`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRequest {
    /// Load one page of history, bounded by [`crate::window::PageCaps::PAGE`].
    Page(IssuedRequest<PageRequest>),
    /// Load one replacement window, bounded by [`crate::window::PageCaps::WINDOW`].
    Window(IssuedRequest<WindowRequest>),
}

impl HostRequest {
    /// Window generation the request was issued for.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        match self {
            Self::Page(issued) => issued.generation,
            Self::Window(issued) => issued.generation,
        }
    }
}

/// What one engine update asks of the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineUpdate {
    /// New history requests to execute, in issue order.
    pub requests: Vec<HostRequest>,
    /// The transcript needs repainting.
    pub invalidated: bool,
}

impl EngineUpdate {
    fn merge(&mut self, other: Self) {
        self.requests.extend(other.requests);
        self.invalidated |= other.invalidated;
    }
}

/// The V2 transcript engine.
pub struct TranscriptEngine {
    session: SessionView,
    window: WindowManager,
    units: Units,
    viewport: TranscriptViewport,
    nav: NavState,
    focus: ChatFocus,
    geometry: ViewportGeometry,
    motion: MotionConfig,
    producer: Box<dyn UnitProducer>,
    /// Item placement policy units are derived with (U5).
    structure: Box<dyn UnitStructure>,
    /// Navigation timers scheduled by the reducer, keyed by timer (one deadline each).
    timers: BTreeMap<NavTimer, Instant>,
    /// Latest input time seen.
    now: Option<Instant>,
    /// Generation of the engine's own attach request (`Window(Tail)` without a navigation jump).
    attach: Option<u64>,
    /// Last prepared frame id.
    last_frame: FrameId,
    /// Last committed frame id (CK3).
    committed: Option<FrameId>,
}

impl std::fmt::Debug for TranscriptEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptEngine")
            .field("window", self.window.summary())
            .field("units", &self.units.len())
            .field("nav", &self.nav)
            .field("focus", &self.focus)
            .field("geometry", &self.geometry)
            .field("motion", &self.motion)
            .field("timers", &self.timers)
            .finish_non_exhaustive()
    }
}

impl TranscriptEngine {
    /// Create an engine with the phase 2 plain-text producer.
    #[must_use]
    pub fn new(geometry: ViewportGeometry, motion: MotionConfig) -> Self {
        Self::with_producer(geometry, motion, Box::new(PlainTextProducer))
    }

    /// Create an engine whose units are built and measured by `producer`.
    #[must_use]
    pub fn with_producer(
        geometry: ViewportGeometry,
        motion: MotionConfig,
        producer: Box<dyn UnitProducer>,
    ) -> Self {
        Self {
            session: SessionView::new(),
            window: WindowManager::new(),
            units: Units::new(),
            viewport: TranscriptViewport::new(),
            nav: NavState::new(),
            focus: ChatFocus::Composer,
            geometry,
            motion,
            producer,
            structure: Box::new(FlatStructure),
            timers: BTreeMap::new(),
            now: None,
            attach: None,
            last_frame: FrameId::default(),
            committed: None,
        }
    }

    /// Return the engine with units derived by `structure` instead of the flat default (U5).
    /// Takes effect from the next document change.
    #[must_use]
    pub fn with_structure(mut self, structure: Box<dyn UnitStructure>) -> Self {
        self.structure = structure;
        self
    }

    // ---- read access ----------------------------------------------------------------------

    /// The semantic session view.
    #[must_use]
    pub const fn session_view(&self) -> &SessionView {
        &self.session
    }

    /// The resident window summary.
    #[must_use]
    pub const fn window(&self) -> &ResidentWindow {
        self.window.summary()
    }

    /// The resident window manager.
    #[must_use]
    pub const fn window_manager(&self) -> &WindowManager {
        &self.window
    }

    /// Current units.
    #[must_use]
    pub const fn units(&self) -> &Units {
        &self.units
    }

    /// The viewport projection.
    #[must_use]
    pub const fn viewport(&self) -> &TranscriptViewport {
        &self.viewport
    }

    /// The navigation state (read-only; changes go through the reducer).
    #[must_use]
    pub const fn nav(&self) -> &NavState {
        &self.nav
    }

    /// Keyboard focus.
    #[must_use]
    pub const fn focus(&self) -> &ChatFocus {
        &self.focus
    }

    /// Transcript viewport geometry.
    #[must_use]
    pub const fn geometry(&self) -> ViewportGeometry {
        self.geometry
    }

    /// Motion configuration.
    #[must_use]
    pub const fn motion(&self) -> MotionConfig {
        self.motion
    }

    /// Latest input time the engine has seen, if any.
    #[must_use]
    pub const fn now(&self) -> Option<Instant> {
        self.now
    }

    /// Rows one mouse-wheel tick scrolls (list chrome, BMUX 1).
    #[must_use]
    pub const fn wheel_rows(&self) -> u16 {
        transcript_chrome(ScrollbarMode::Off).wheel_rows
    }

    /// Earliest scheduled navigation timer, for the host's runtime timer.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.values().min().copied()
    }

    /// Scheduled navigation timers.
    #[must_use]
    pub const fn timers(&self) -> &BTreeMap<NavTimer, Instant> {
        &self.timers
    }

    /// Last committed frame (CK3).
    #[must_use]
    pub const fn committed_frame(&self) -> Option<FrameId> {
        self.committed
    }

    /// Whether `request` is still awaited: it belongs to the current window generation and its
    /// slot is in flight. Requests superseded by a replacement, an eviction or a cancelled jump
    /// are no longer awaited; their results would be dropped as stale.
    #[must_use]
    pub fn is_awaiting(&self, request: &HostRequest) -> bool {
        let window = self.window.summary();
        if request.generation() != window.generation {
            return false;
        }
        match request {
            HostRequest::Page(issued) => match issued.request.direction() {
                PageDirection::Older => window.in_flight.older,
                PageDirection::Newer => window.in_flight.newer,
            },
            HostRequest::Window(issued) => {
                window.in_flight.replace == Some(issued.request)
                    || (issued.request == WindowRequest::LiveState && window.in_flight.newer)
            }
        }
    }

    // ---- inputs ---------------------------------------------------------------------------

    /// Attach: request the bounded tail window (W10). The host answers with
    /// [`Self::apply_window`].
    pub fn attach(&mut self, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        let issued = self.window.request_window(WindowRequest::Tail);
        self.attach = Some(issued.generation);
        EngineUpdate {
            requests: vec![HostRequest::Window(issued)],
            invalidated: true,
        }
    }

    /// Reduce one navigation intent (keys, wheel, scrollbar, timeline, focus traversal).
    pub fn intent(&mut self, intent: NavIntent, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        let effects = self.reduce_input(NavInput::Intent { intent, at }, &RemovedUnits::new());
        let mut update = self.run_effects(effects);
        update.merge(self.settle(at));
        update
    }

    /// Route one terminal event to the transcript list (VP5): the list only reports actions,
    /// which become navigation intents. Returns the update and whether the list handled it.
    pub fn list_event(&mut self, event: &Event, at: Instant) -> (EngineUpdate, bool) {
        self.observe_time(at);
        let position = self.presented(at);
        self.viewport.apply_position(&position);
        let outcome = {
            let items = transcript_items(&self.session);
            let source = TranscriptItemSource::new(
                &self.units,
                items,
                self.window.summary(),
                self.producer.as_ref(),
            );
            let component =
                self.viewport
                    .component(LIST_ID, &source, &NoSourceMap, ScrollbarMode::Off);
            let layout = component.layout(
                Constraints::tight(self.terminal_size()),
                &mut LayoutCx::new(),
            );
            component.event(event, &layout, &mut EventCx::new(&layout))
        };
        let mut update = EngineUpdate::default();
        for output in self.viewport.drain_outputs() {
            if let ViewportOutput::Navigate(intent) = output {
                update.merge(self.intent(intent, at));
            }
        }
        (update, outcome == EventOutcome::Handled)
    }

    /// Apply one live input for the attached session (W2, W11).
    pub fn apply_live(&mut self, input: LiveInput, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        let LiveOutcome::Applied(changes) = self.window.apply_live(input, &mut self.session) else {
            return EngineUpdate::default();
        };
        self.content(changes, at)
    }

    /// Incorporate a page result in one update (W3, N8, N11).
    ///
    /// # Errors
    ///
    /// * [`StaleResult::Generation`] if the result belongs to another window generation.
    /// * [`StaleResult::NotInFlight`] if no page in that direction is in flight.
    /// * [`StaleResult::EdgeChanged`] if the window edge changed since the request.
    pub fn apply_page(
        &mut self,
        result: &HistoryPageResult,
        at: Instant,
    ) -> Result<EngineUpdate, StaleResult> {
        self.observe_time(at);
        let old_edge = match result.direction {
            PageDirection::Older => self.first_content_unit(),
            PageDirection::Newer => self.last_content_unit(),
        };
        let incorporated = self.window.apply_page(result, &mut self.session)?;
        let unit_changes = self.apply_changes(&incorporated.changes);
        let inserted_rows = self.inserted_rows(&unit_changes, result.direction, old_edge.as_ref());
        let mut effects = self.reduce_input(
            NavInput::PageResult {
                generation: incorporated.generation,
                direction: incorporated.direction,
                inserted_rows,
                exhausted: incorporated.exhausted,
            },
            &unit_changes.correspondence,
        );
        if incorporated.mode_changed {
            // W11: the window went live and buffered live events were applied.
            effects.extend(self.reduce_input(
                NavInput::Content {
                    changes: incorporated.changes.clone(),
                    at,
                },
                &unit_changes.correspondence,
            ));
        }
        let mut update = self.run_effects(effects);
        if let Some(follow_up) = incorporated.follow_up {
            update.requests.push(HostRequest::Window(follow_up));
        }
        update.merge(self.settle(at));
        Ok(update)
    }

    /// Incorporate a window result in one update (W3, W10, W11).
    ///
    /// # Errors
    ///
    /// * [`StaleResult::Generation`] if the result belongs to another window generation.
    /// * [`StaleResult::NotInFlight`] if no request for that target is in flight.
    pub fn apply_window(
        &mut self,
        result: &HistoryWindowResult,
        at: Instant,
    ) -> Result<EngineUpdate, StaleResult> {
        self.observe_time(at);
        let incorporated = self.window.apply_window(result, &mut self.session)?;
        if self.attach == Some(incorporated.generation) {
            self.attach = None;
        }
        let unit_changes = self.apply_changes(&incorporated.changes);
        let effects = self.reduce_input(
            NavInput::WindowResult {
                generation: incorporated.generation,
                target: incorporated.target,
            },
            &unit_changes.correspondence,
        );
        let mut update = self.run_effects(effects);
        update.merge(self.settle(at));
        Ok(update)
    }

    /// Resize the transcript viewport. A width change re-measures resident units (L9); keyed
    /// anchors keep the reading position (P1).
    pub fn resize(&mut self, geometry: ViewportGeometry, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        let width_changed = geometry.width != self.geometry.width;
        self.geometry = geometry;
        if width_changed {
            self.remeasure();
        }
        let effects = self.reduce_input(
            NavInput::Geometry { viewport: geometry },
            &RemovedUnits::new(),
        );
        let mut update = self.run_effects(effects);
        update.merge(self.settle(at));
        update
    }

    /// Change the motion configuration (reduced motion, C22). Under reduced motion a running
    /// motion settles at its target at once (N7).
    pub fn set_motion(&mut self, motion: MotionConfig, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        self.motion = motion;
        if self.nav.motion.is_some() && motion.duration.is_zero() {
            self.timers.insert(NavTimer::MotionFrame, at);
            return self.advance(at);
        }
        EngineUpdate {
            requests: Vec::new(),
            invalidated: true,
        }
    }

    /// Fire every navigation timer due at or before `now`, in deadline order (T24, T25).
    pub fn advance(&mut self, now: Instant) -> EngineUpdate {
        let mut update = EngineUpdate::default();
        while let Some((key, at)) = self
            .timers
            .iter()
            .filter(|(_, at)| **at <= now)
            .min_by_key(|(_, at)| **at)
            .map(|(key, at)| (*key, *at))
        {
            self.timers.remove(&key);
            self.observe_time(at);
            let effects = self.reduce_input(NavInput::Timer { key, at }, &RemovedUnits::new());
            update.merge(self.run_effects(effects));
            update.merge(self.settle(at));
        }
        self.observe_time(now);
        update
    }

    // ---- frames ---------------------------------------------------------------------------

    /// Prepare one frame at `clock` (spec 9.1 step 2). Writes only the derived list position
    /// (VP5); navigation, window and units are unchanged (CK1).
    pub fn prepare_frame(&mut self, clock: FrameClock) -> FrameSnapshot {
        let position = self.presented(clock.now);
        self.viewport.apply_position(&position);
        self.last_frame = self.last_frame.next();
        let viewport = self
            .viewport
            .frame(&position, self.geometry.height, &NoSourceMap);
        let motion_running = self.nav.motion.is_some();
        FrameSnapshot {
            id: self.last_frame,
            clock,
            geometry: self.geometry,
            following: self.nav.mode == NavMode::Following && !motion_running,
            motion_running,
            position,
            viewport,
            latest_bar: LatestBarState {
                visible: false,
                rows_below_latest: 0,
            },
        }
    }

    /// Paint a prepared frame into `cx` at the transcript region's origin (spec 9.1 step 3).
    /// Only visible, correctly positioned units are built and painted (VP3).
    pub fn paint_frame(&self, snapshot: &FrameSnapshot, cx: &mut PaintCx<'_, '_>) {
        let items = transcript_items(&self.session);
        let source = TranscriptItemSource::new(
            &self.units,
            items,
            self.window.summary(),
            self.producer.as_ref(),
        );
        let component = self
            .viewport
            .component(LIST_ID, &source, &NoSourceMap, ScrollbarMode::Off);
        let size = Size::new(
            u16::try_from(snapshot.geometry.width).unwrap_or(u16::MAX),
            u16::try_from(snapshot.geometry.height).unwrap_or(u16::MAX),
        );
        let layout = component.layout(Constraints::tight(size), &mut LayoutCx::new());
        component.paint(&layout, cx);
    }

    /// Record a presented frame (CK3). Navigation is unchanged.
    pub fn commit(&mut self, frame: FrameId) -> EngineUpdate {
        self.committed = Some(frame);
        let effects = self.reduce_input(
            NavInput::PresentationCommitted { frame },
            &RemovedUnits::new(),
        );
        self.run_effects(effects)
    }

    // ---- internals ------------------------------------------------------------------------

    fn observe_time(&mut self, at: Instant) {
        self.now = Some(self.now.map_or(at, |now| now.max(at)));
    }

    fn terminal_size(&self) -> Size {
        Size::new(
            u16::try_from(self.geometry.width).unwrap_or(u16::MAX),
            u16::try_from(self.geometry.height).unwrap_or(u16::MAX),
        )
    }

    /// Keyed position presented at `now` (a running motion is sampled, N7).
    fn presented(&self, now: Instant) -> ListPosition {
        let removed = RemovedUnits::new();
        let facts = UnitFactsView::new(&self.units, &removed);
        let nav = &self.nav;
        self.viewport
            .with_nav_view(self.nav_context(&facts), |view| {
                presented_position(nav, view, now)
            })
    }

    fn nav_context<'a>(&'a self, facts: &'a UnitFactsView<'a>) -> NavViewContext<'a> {
        NavViewContext {
            sources: &NoSourceMap,
            facts,
            geometry: self.geometry,
            window: self.window.summary(),
            motion: self.motion,
        }
    }

    /// Reduce one input against the current index (N11) with the P4 correspondence of the
    /// change being reduced.
    fn reduce_input(&mut self, input: NavInput, removed: &RemovedUnits) -> Vec<NavEffect> {
        let facts = UnitFactsView::new(&self.units, removed);
        let context = NavViewContext {
            sources: &NoSourceMap,
            facts: &facts,
            geometry: self.geometry,
            window: self.window.summary(),
            motion: self.motion,
        };
        let nav = &mut self.nav;
        self.viewport
            .with_nav_view(context, |view| reduce(nav, input, view))
    }

    /// Execute navigation effects: requests go to the window manager (which may refuse them
    /// under W5/W7a), timers are recorded, invalidations are reported.
    fn run_effects(&mut self, effects: Vec<NavEffect>) -> EngineUpdate {
        let mut update = EngineUpdate::default();
        for effect in effects {
            match effect {
                NavEffect::Page(request) => {
                    if let Some(issued) = self.window.request_page(request.direction()) {
                        update.requests.push(HostRequest::Page(issued));
                    }
                }
                NavEffect::Window(request) => {
                    let issued = self.window.request_window(request);
                    update.requests.push(HostRequest::Window(issued));
                }
                NavEffect::ScheduleTimer { key, at } => {
                    self.timers.insert(key, at);
                }
                NavEffect::CancelTimer(key) => {
                    self.timers.remove(&key);
                }
                NavEffect::Invalidate(_) => update.invalidated = true,
            }
        }
        update
    }

    /// A content change already applied to the window: units, index, then `Content` (N11).
    fn content(&mut self, changes: TranscriptChanges, at: Instant) -> EngineUpdate {
        let unit_changes = self.apply_changes(&changes);
        let effects = self.reduce_input(
            NavInput::Content { changes, at },
            &unit_changes.correspondence,
        );
        let mut update = self.run_effects(effects);
        update.invalidated = true;
        update.merge(self.settle(at));
        update
    }

    /// End of every update: cancel a replacement nobody waits for any more (T22), evict
    /// (W7), fold the measured rows into the estimate (W12) and prefetch (W5).
    fn settle(&mut self, at: Instant) -> EngineUpdate {
        let mut update = EngineUpdate::default();
        let window = self.window.summary();
        if self.attach.is_some_and(|generation| {
            generation != window.generation || window.in_flight.replace.is_none()
        }) {
            // The attach request was superseded (another replacement, a cancellation) or
            // incorporated: it no longer shields the in-flight replacement.
            self.attach = None;
        }
        if self.window.summary().in_flight.replace.is_some()
            && self.nav.pending_jump.is_none()
            && self.attach.is_none()
        {
            let _ = self.window.cancel_window();
        }
        if let Some(changes) = self.evict(at) {
            let unit_changes = self.apply_changes(&changes);
            let effects = self.reduce_input(
                NavInput::Content { changes, at },
                &unit_changes.correspondence,
            );
            update.merge(self.run_effects(effects));
            update.invalidated = true;
        }
        let total = self.viewport.index().total_height();
        self.window.observe_rows(total);
        let position = self.presented(at);
        let rows = self
            .viewport
            .edge_rows(&position, self.geometry.height, &NoSourceMap);
        for issued in self.window.poll_prefetch(rows, self.geometry.height) {
            update.requests.push(HostRequest::Page(issued));
        }
        update
    }

    /// Run eviction against the presented position (W7). Returns the eviction's changes.
    fn evict(&mut self, at: Instant) -> Option<TranscriptChanges> {
        let position = self.presented(at);
        let viewport_top = self
            .viewport
            .resolve(&position, self.geometry.height, &NoSourceMap)
            .map_or(0, |resolved| resolved.top_offset);
        let anchor = match &self.nav.mode {
            NavMode::Reading { anchor } | NavMode::TailSpace { anchor } => {
                Some(anchor.unit.clone())
            }
            NavMode::Holding { item, .. } => Some(item.clone()),
            NavMode::Following => None,
        };
        let pinned = self
            .nav
            .motion
            .as_ref()
            .and_then(|motion| match &motion.target {
                ScrollTarget::ItemTop(unit) | ScrollTarget::ItemBottom(unit) => Some(unit.clone()),
                ScrollTarget::Anchor(anchor) => Some(anchor.unit.clone()),
                ScrollTarget::LatestBottom => None,
            })
            .into_iter()
            .collect::<Vec<_>>();
        let context = EvictionContext {
            viewport_top,
            viewport_height: self.geometry.height,
            anchor: anchor.as_ref(),
            pinned: &pinned,
            deferred: self.nav.motion.is_some(),
        };
        let viewport = &self.viewport;
        let height = |key: &UnitKey| {
            viewport
                .index()
                .item_by_key(key)
                .map_or(0, |item| item.height)
        };
        self.window
            .evict(&context, &self.units, &height, &mut self.session)
            .map(|outcome| outcome.changes)
    }

    /// Derive units for a document change and splice the measured index (U4, L1, W3).
    fn apply_changes(&mut self, changes: &TranscriptChanges) -> UnitChanges {
        let items = transcript_items(&self.session);
        let unit_changes = self.units.apply(
            changes,
            items,
            self.window.summary().mode,
            self.structure.as_ref(),
        );
        let width = self.geometry.width;
        let source = TranscriptItemSource::new(
            &self.units,
            items,
            self.window.summary(),
            self.producer.as_ref(),
        );
        let mut measure = |key: &UnitKey| source.measure(key, width);
        if self
            .viewport
            .apply_unit_changes(&unit_changes, &self.units, width, &mut measure)
            .is_err()
        {
            // D3: an inconsistent index is rebuilt in full.
            let rebuilt = self.viewport.rebuild(&self.units, width, &mut measure);
            debug_assert!(rebuilt.is_ok(), "derived units never repeat a key");
        }
        unit_changes
    }

    /// Re-measure every resident unit at the current width (L9).
    fn remeasure(&mut self) {
        let items = transcript_items(&self.session);
        let width = self.geometry.width;
        let source = TranscriptItemSource::new(
            &self.units,
            items,
            self.window.summary(),
            self.producer.as_ref(),
        );
        let mut measure = |key: &UnitKey| source.measure(key, width);
        let remeasured = self.viewport.remeasure(&self.units, width, &mut measure);
        debug_assert!(remeasured.is_ok(), "derived units never repeat a key");
    }

    fn first_content_unit(&self) -> Option<UnitKey> {
        self.units
            .keys()
            .iter()
            .find(|key| !key.is_boundary())
            .cloned()
    }

    fn last_content_unit(&self) -> Option<UnitKey> {
        self.units
            .keys()
            .iter()
            .rev()
            .find(|key| !key.is_boundary())
            .cloned()
    }

    /// Measured rows of units a page inserted on its side of the previous content edge (N8).
    fn inserted_rows(
        &self,
        changes: &UnitChanges,
        direction: PageDirection,
        old_edge: Option<&UnitKey>,
    ) -> u64 {
        let index = self.viewport.index();
        let edge = old_edge.and_then(|key| index.key_offset(key));
        changes
            .inserted()
            .filter(|key| !key.is_boundary())
            .filter_map(|key| Some((index.key_offset(key)?, index.item_by_key(key)?.height)))
            .filter(|(offset, _)| match (direction, edge) {
                (_, None) => true,
                (PageDirection::Older, Some(edge)) => *offset < edge,
                (PageDirection::Newer, Some(edge)) => *offset > edge,
            })
            .map(|(_, height)| height)
            .sum()
    }
}

fn transcript_items(session: &SessionView) -> &[TranscriptViewItem] {
    &session.snapshot().transcript.items
}

#[cfg(test)]
mod tests;
