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
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::PaintCx;
use bmux_tui_components::scrollbar::ScrollbarStyles;
use bmux_tui_components::scrollbar_layout::{
    ScrollbarAxisLayoutMode, ScrollbarLayoutPolicy, scrollbar_layout,
};
use bmux_tui_components::virtual_list::{PositionEstimate as ListEstimate, VirtualListComponent};

use crate::focus::{ChatFocus, FocusState};
use crate::frame::{FrameClock, FrameId, FrameSnapshot};
use crate::latest_bar::LatestBar;
use crate::layout::geometry::ItemAction;
use crate::layout::producers::{ProducerRegistry, UnitProducer};
use crate::layout::{ActivityPhase, ItemLayoutCache, LayoutInputs, RenderConfig};
use crate::navigation::{
    ListPosition, MotionConfig, NavEffect, NavInput, NavIntent, NavMode, NavState, NavTimer,
    RemovedUnits, ScrollTarget, presented_position, reduce,
};
use crate::units::facts::UnitFactsView;
use crate::units::structure::{DisclosureTarget, StructureConfig, StructureResolver};
use crate::units::{UnitChanges, UnitKey, Units};
use crate::viewport::source::TranscriptItemSource;
use crate::viewport::{
    NavViewContext, ScrollbarMode, TranscriptViewport, ViewportGeometry, ViewportOutput,
    transcript_chrome,
};
use crate::window::{
    EvictionContext, HistoryPageResult, HistoryWindowResult, IssuedRequest, LiveInput, LiveOutcome,
    PAGE_EVENTS, PageDirection, PageRequest, RESIDENT_CEILING_EVENTS, ResidentWindow, StaleResult,
    WindowManager, WindowRequest,
};
use bcode_presentation_policy::models::Disclosure;

/// Resident events above which selection endpoint pins no longer hold back the W7a ceiling
/// trim: one page above the ceiling. Past it the selection is dropped so residency stays
/// bounded (W7, W7a).
const SELECTION_PIN_LIMIT_EVENTS: u64 = RESIDENT_CEILING_EVENTS + PAGE_EVENTS;

/// Layout id of the transcript list component; also the id of the scroll hit region the
/// list registers over its content.
pub const LIST_ID: &str = "transcript";

/// Narrowest transcript region that presents a scrollbar. A narrower region presents none,
/// so the scrollbar never takes the only content column (VP4, X6).
pub const MIN_SCROLLBAR_REGION_WIDTH: u64 = 2;

/// The interactive region under a point of a prepared frame ([`TranscriptEngine::hit_test`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemHit {
    /// Unit the region belongs to.
    pub unit: UnitKey,
    /// What activating the region does.
    pub action: ItemAction,
}

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

/// Scrollbar layout policy of the transcript list for `mode` (vertical only), as the list
/// chrome lays it out.
const fn list_scrollbar_policy(mode: ScrollbarMode) -> ScrollbarLayoutPolicy {
    ScrollbarLayoutPolicy::new(
        transcript_chrome(mode).scrollbar,
        ScrollbarAxisLayoutMode::Hidden,
    )
}

/// The V2 transcript engine.
pub struct TranscriptEngine {
    session: SessionView,
    window: WindowManager,
    units: Units,
    viewport: TranscriptViewport,
    nav: NavState,
    /// Composer/transcript keyboard focus (section 6).
    focus: FocusState,
    /// Latest-bar decision, activity and the full region geometry (F6).
    latest: LatestBar,
    geometry: ViewportGeometry,
    motion: MotionConfig,
    /// Configured scrollbar of the transcript list (VP4, X6).
    scrollbar: ScrollbarMode,
    /// Scrollbar styles (paint only).
    scrollbar_styles: ScrollbarStyles,
    producer: Box<dyn UnitProducer>,
    /// Presentation-policy structure units are derived with (U2, U5).
    structure: StructureResolver,
    /// Render configuration item contexts are built from (L2).
    render: RenderConfig,
    /// Identity-keyed retained layouts; the only source of unit heights (L1, VP6).
    layouts: ItemLayoutCache,
    /// Spinner and elapsed phase, advanced on accepted invalidations (F7).
    activity: ActivityPhase,
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
    /// Transcript selection (C34, W7).
    selection: crate::selection::SelectionModel,
}

// The engine is `Send` by construction: its retained items are Send-capable BMUX elements
// and its producers are `Send` (no thread confinement is needed to own it in a `Send` model).
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<TranscriptEngine>();
};

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
    /// Create an engine with the standard producers ([`ProducerRegistry::standard`]).
    #[must_use]
    pub fn new(geometry: ViewportGeometry, motion: MotionConfig) -> Self {
        Self::with_producer(geometry, motion, Box::new(ProducerRegistry::standard()))
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
            focus: FocusState::default(),
            latest: LatestBar::new(false, geometry),
            geometry,
            motion,
            scrollbar: ScrollbarMode::Off,
            scrollbar_styles: ScrollbarStyles::new(),
            producer,
            structure: StructureResolver::default(),
            render: RenderConfig::default(),
            layouts: ItemLayoutCache::new(),
            activity: ActivityPhase::default(),
            timers: BTreeMap::new(),
            now: None,
            attach: None,
            last_frame: FrameId::default(),
            committed: None,
            selection: crate::selection::SelectionModel::default(),
        }
    }

    /// Return the engine with `config` for structure resolution (U5).
    #[must_use]
    pub fn with_structure_config(mut self, config: StructureConfig) -> Self {
        self.structure.set_config(config);
        self
    }

    /// Return the engine with the transcript scrollbar `mode` (VP4, X6); a gutter narrows the
    /// content geometry by its column.
    #[must_use]
    pub fn with_scrollbar(mut self, mode: ScrollbarMode) -> Self {
        self.scrollbar = mode;
        self.geometry = self.content_geometry();
        self
    }

    /// Return the engine with `config` for item rendering (L2).
    #[must_use]
    pub fn with_render_config(mut self, config: RenderConfig) -> Self {
        self.render = config;
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

    /// Resolved structure (placements, per-unit presentations, disclosure overrides).
    #[must_use]
    pub const fn structure(&self) -> &StructureResolver {
        &self.structure
    }

    /// Render configuration.
    #[must_use]
    pub const fn render_config(&self) -> &RenderConfig {
        &self.render
    }

    /// Retained item layouts (heights, nodes, geometry).
    #[must_use]
    pub const fn layouts(&self) -> &ItemLayoutCache {
        &self.layouts
    }

    /// Layout work since the last call, then reset (spec 9.2, D4).
    #[cfg(any(test, feature = "testing"))]
    pub const fn take_layout_work(&mut self) -> crate::layout::LayoutWorkCounters {
        self.layouts.take_counters()
    }

    /// Window-operation work since the engine was created: whole-window replacements, SD2
    /// fallbacks, prepends, appends, evictions and reduced events (phase 5: paging performs no
    /// full rebuild).
    #[must_use]
    pub const fn window_work(&self) -> crate::window::WindowWork {
        self.window.work()
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
        self.focus.chat()
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

    /// Configured transcript scrollbar (VP4, X6).
    #[must_use]
    pub const fn scrollbar(&self) -> ScrollbarMode {
        self.scrollbar
    }

    /// Scrollbar presented at the current region width: the configured one, or
    /// [`ScrollbarMode::Off`] while the region is narrower than [`MIN_SCROLLBAR_REGION_WIDTH`].
    #[must_use]
    pub const fn presented_scrollbar(&self) -> ScrollbarMode {
        if self.latest.region().width < MIN_SCROLLBAR_REGION_WIDTH {
            ScrollbarMode::Off
        } else {
            self.scrollbar
        }
    }

    /// Presentation-only position of the resident rows within the whole session (W12): exact
    /// resident rows, with rows before and after the window extrapolated from the session
    /// bounds and the rows-per-event estimate. The scrollbar is drawn from it and scrollbar
    /// fractions are mapped through it (the same estimate `JumpToFraction` inverts).
    #[must_use]
    pub fn position_estimate(&self) -> ListEstimate {
        let estimate = self
            .window
            .summary()
            .position_estimate(self.viewport.index().total_height());
        ListEstimate {
            before: estimate.rows_before_window,
            resident: estimate.resident_rows,
            after: estimate.rows_after_window,
        }
    }

    /// Scrollbar track of a prepared frame in viewport cells, from the same BMUX scrollbar
    /// layout the list is laid out with; `None` without a presented scrollbar. An overlay
    /// track lies over the last content column.
    #[must_use]
    pub fn scrollbar_track(&self, snapshot: &FrameSnapshot) -> Option<Rect> {
        let mode = self.presented_scrollbar();
        if mode == ScrollbarMode::Off {
            return None;
        }
        let size = self.list_size(snapshot.geometry.height);
        scrollbar_layout(
            Rect::new(0, 0, size.width, size.height),
            list_scrollbar_policy(mode),
        )
        .vertical_scrollbar
        .filter(|track| !track.is_empty())
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
            let source = TranscriptItemSource::new(
                &self.units,
                self.window.summary(),
                &self.layouts,
                self.activity,
            );
            let component = self.list_component(&source);
            let layout = component.layout(
                Constraints::tight(self.list_size(self.geometry.height)),
                &mut LayoutCx::with_clock(FrameClock { now: at }.presentation()),
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
            // A buffered or dropped live event may still advance the newest known sequence of a
            // detached window (W2): the latest bar observes it.
            return self.settle_interaction(at);
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
        // `geometry` is the full transcript region; the effective viewport excludes the
        // latest-bar row while the bar is shown (F6) and a scrollbar gutter (VP4).
        self.latest.set_region(geometry);
        let geometry = self.content_geometry();
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

    /// Set the transcript scrollbar (VP4, X6). A gutter scrollbar reserves a content column,
    /// so switching to or from one re-measures resident units at the new content width (L9);
    /// keyed anchors keep the reading position (P1).
    pub fn set_scrollbar(&mut self, mode: ScrollbarMode, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        if self.scrollbar == mode {
            return EngineUpdate::default();
        }
        self.scrollbar = mode;
        let mut update = self.apply_effective_geometry();
        update.invalidated = true;
        update.merge(self.settle(at));
        update
    }

    /// Set the scrollbar styles (paint only). Returns whether they changed.
    pub fn set_scrollbar_styles(&mut self, styles: ScrollbarStyles) -> bool {
        if self.scrollbar_styles == styles {
            return false;
        }
        self.scrollbar_styles = styles;
        true
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

    /// Replace the render configuration (theme, Markdown and diff options, plugin visuals).
    /// Units whose fingerprint changed are re-measured in this update; keyed anchors keep the
    /// reading position (P1). A theme change re-measures every unit (L3).
    pub fn set_render_config(&mut self, config: RenderConfig, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        self.render = config;
        self.remeasure();
        let effects = self.reduce_input(
            NavInput::Geometry {
                viewport: self.geometry,
            },
            &RemovedUnits::new(),
        );
        let mut update = self.run_effects(effects);
        update.invalidated = true;
        update.merge(self.settle(at));
        update
    }

    /// Replace the accepted Markdown visuals (L8) and re-measure only the resident units whose
    /// accepted visuals changed (L3), leaving every other unit's retained layout untouched and
    /// unvisited by layout. Accepting a visual never changes a unit's height, but the units are
    /// re-measured through the measured index like any revision. Invalidates the transcript
    /// only when a resident unit changed.
    pub fn set_markdown_visuals(
        &mut self,
        visuals: crate::layout::producers::markdown::MarkdownVisualState,
        at: Instant,
    ) -> EngineUpdate {
        self.observe_time(at);
        if self.render.markdown.visuals == visuals {
            return EngineUpdate::default();
        }
        let previous = std::mem::replace(&mut self.render.markdown.visuals, visuals);
        let current = &self.render.markdown.visuals;
        let revised = self
            .units
            .keys()
            .iter()
            .filter(|key| {
                !key.is_boundary()
                    && previous.document_differs(
                        current,
                        &crate::layout::context::markdown_document_id(key),
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        if revised.is_empty() {
            return EngineUpdate::default();
        }
        let changes = UnitChanges {
            revised,
            ..UnitChanges::default()
        };
        let items = transcript_items(&self.session);
        let width = self.geometry.width;
        let inputs = LayoutInputs {
            units: &self.units,
            items,
            structure: &self.structure,
            render: &self.render,
            producer: self.producer.as_ref(),
        };
        let layouts = &mut self.layouts;
        let mut measure = |key: &UnitKey| inputs.measure(layouts, key, width);
        if self
            .viewport
            .apply_unit_changes(&changes, &self.units, width, &mut measure)
            .is_err()
        {
            let rebuilt = self.viewport.rebuild(&self.units, width, &mut measure);
            debug_assert!(rebuilt.is_ok(), "derived units never repeat a key");
        }
        let effects = self.reduce_input(
            NavInput::Geometry {
                viewport: self.geometry,
            },
            &RemovedUnits::new(),
        );
        let mut update = self.run_effects(effects);
        update.invalidated = true;
        update.merge(self.settle(at));
        update
    }

    /// Replace the structure configuration (policy, plugin facts, declared targets). Every
    /// resident item is re-resolved; units whose structure changed are re-derived and
    /// re-measured in this update, and anchors on units that fold into a group follow the
    /// group header (U2).
    pub fn set_structure_config(&mut self, config: StructureConfig, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        self.structure.set_config(config);
        self.content(TranscriptChanges::default(), at)
    }

    /// Toggle a disclosure target between expanded and collapsed (X1, U2). Returns the update
    /// and the new disclosure; `None` when the target is unknown or may not collapse (A16).
    pub fn toggle_disclosure(
        &mut self,
        target: &DisclosureTarget,
        at: Instant,
    ) -> (EngineUpdate, Option<Disclosure>) {
        self.observe_time(at);
        let Some(disclosure) = self.structure.toggle(target) else {
            return (EngineUpdate::default(), None);
        };
        (
            self.content(TranscriptChanges::default(), at),
            Some(disclosure),
        )
    }

    /// Set a disclosure target (A16 floor applies). Returns the update and whether the
    /// override changed.
    pub fn set_disclosure(
        &mut self,
        target: &DisclosureTarget,
        disclosure: Disclosure,
        at: Instant,
    ) -> (EngineUpdate, bool) {
        self.observe_time(at);
        if !self.structure.set_disclosure(target, disclosure) {
            return (EngineUpdate::default(), false);
        }
        (self.content(TranscriptChanges::default(), at), true)
    }

    /// Accept a new spinner/elapsed phase (F7, D7): a paint-only change.
    pub const fn set_activity(&mut self, activity: ActivityPhase) -> EngineUpdate {
        self.activity = activity;
        EngineUpdate {
            requests: Vec::new(),
            invalidated: true,
        }
    }

    // ---- frames ---------------------------------------------------------------------------

    /// Prepare one frame at `clock` (spec 9.1 step 2). Writes only the derived list position
    /// (VP5); navigation, window and units are unchanged (CK1).
    pub fn prepare_frame(&mut self, clock: FrameClock) -> FrameSnapshot {
        let position = self.presented(clock.now);
        self.viewport.apply_position(&position);
        self.last_frame = self.last_frame.next();
        let mut viewport = self
            .viewport
            .frame(&position, self.geometry.height, &self.layouts);
        if let Some(frame) = viewport.as_mut() {
            for unit in &mut frame.units {
                unit.layout = self
                    .layouts
                    .current(&unit.key)
                    .map(|layout| layout.key.clone());
            }
        }
        let motion_running = self.nav.motion.is_some();
        let mut snapshot = FrameSnapshot {
            id: self.last_frame,
            clock,
            geometry: self.geometry,
            following: self.nav.mode == NavMode::Following && !motion_running,
            motion_running,
            position,
            viewport,
            latest_bar: self.latest.state(),
            scrollbar_track: None,
            focus: self.focus.target().cloned(),
            focus_rows: None,
            hit_regions: Vec::new(),
            selection: self
                .selection
                .selection()
                .filter(|selection| !selection.is_collapsed())
                .cloned(),
            activity: self.activity,
        };
        // Derived from the retained layouts the snapshot names, once per frame (F1, F4).
        snapshot.scrollbar_track = self.scrollbar_track(&snapshot);
        snapshot.focus_rows = self.focus_rows(&snapshot);
        snapshot.hit_regions = self.visible_hit_regions(&snapshot);
        snapshot
    }

    /// Paint a prepared frame into `cx` at the transcript region's origin (spec 9.1 step 3).
    /// Only visible, correctly positioned units are built and painted (VP3).
    ///
    /// The list is laid out with the snapshot's clock as the BMUX presentation clock (BMUX 8),
    /// which is the instant the host prepared the frame with; when `cx` carries a presentation
    /// clock it is the same sample (F2).
    pub fn paint_frame(&self, snapshot: &FrameSnapshot, cx: &mut PaintCx<'_, '_>) {
        debug_assert!(
            cx.clock()
                .is_none_or(|clock| FrameClock::from(clock) == snapshot.clock),
            "a frame is painted with the clock it was prepared with (F2)"
        );
        let source = TranscriptItemSource::new(
            &self.units,
            self.window.summary(),
            &self.layouts,
            self.activity,
        );
        let component = self.list_component(&source);
        let size = self.list_size(snapshot.geometry.height);
        let layout = component.layout(
            Constraints::tight(size),
            &mut LayoutCx::with_clock(snapshot.clock.presentation()),
        );
        component.paint(&layout, cx);
    }

    /// The interactive region of a prepared frame at transcript-region cell (`column`, `row`)
    /// (B3, B7, X1).
    ///
    /// Regions come from the retained geometry of the unit painted at that row, so hits use
    /// the same layout definition as the painted cells. Only units visible in `snapshot` are
    /// hit, only inside the viewport, and only while their retained layout is still the one
    /// the snapshot painted (the layout key recorded by [`Self::prepare_frame`]: unit,
    /// revision, fingerprint and width, plus the painted height); otherwise there is no hit.
    /// When regions nest (a section label inside its unit), the smallest one wins.
    #[must_use]
    pub fn hit_test(&self, snapshot: &FrameSnapshot, column: u64, row: u64) -> Option<ItemHit> {
        if column >= snapshot.geometry.width || row >= snapshot.geometry.height {
            return None;
        }
        let viewport = snapshot.viewport.as_ref()?;
        let row = i64::try_from(row).ok()?;
        let unit = viewport.units.iter().find(|unit| {
            unit.top <= row
                && row
                    < unit
                        .top
                        .saturating_add(i64::try_from(unit.height).unwrap_or(i64::MAX))
        })?;
        let layout = self.painted_layout(snapshot, unit)?;
        let local_row = u64::try_from(row - unit.top).ok()?;
        layout
            .geometry
            .hit_regions
            .iter()
            .filter(|region| {
                let rect = region.rect;
                column >= rect.x
                    && column - rect.x < rect.width
                    && local_row >= rect.y
                    && local_row - rect.y < rect.height
            })
            .min_by_key(|region| region.rect.width.saturating_mul(region.rect.height))
            .map(|region| ItemHit {
                unit: unit.key.clone(),
                action: region.action.clone(),
            })
    }

    /// Activate the region a click at (`column`, `row`) of `snapshot` hits (X1): a disclosure
    /// region toggles its target (the visibility floor still applies, A16). Returns the update
    /// and the hit; producer activations ([`ItemAction::Activate`]) are returned for the host
    /// to handle and change nothing here.
    ///
    /// Phase 6 routes pointer gestures through BMUX click activation (BMUX 14); this entry point
    /// is the activation that routing ends in.
    pub fn click(
        &mut self,
        snapshot: &FrameSnapshot,
        column: u64,
        row: u64,
        at: Instant,
    ) -> (EngineUpdate, Option<ItemHit>) {
        self.observe_time(at);
        let Some(hit) = self.hit_test(snapshot, column, row) else {
            return (EngineUpdate::default(), None);
        };
        let update = match &hit.action {
            ItemAction::ToggleDisclosure(target) => self.toggle_disclosure(target, at).0,
            ItemAction::Activate(_) => EngineUpdate::default(),
        };
        (update, Some(hit))
    }

    /// Record a presented frame (CK3). Navigation is unchanged.
    pub fn commit(&mut self, frame: FrameId) -> EngineUpdate {
        self.committed = Some(frame);
        let effects = self.reduce_input(
            NavInput::PresentationCommitted { frame },
            &RemovedUnits::new(),
        );
        let mut update = self.run_effects(effects);
        if let Some(now) = self.now {
            update.merge(self.settle_interaction(now));
        }
        update
    }

    // ---- internals ------------------------------------------------------------------------

    fn observe_time(&mut self, at: Instant) {
        self.now = Some(self.now.map_or(at, |now| now.max(at)));
    }

    /// Size the transcript list is laid out at: the full region width (content plus any
    /// scrollbar gutter) by `height` rows.
    fn list_size(&self, height: u64) -> Size {
        Size::new(
            u16::try_from(self.latest.region().width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
        )
    }

    /// The effective viewport (F6) minus the columns the presented scrollbar reserves (VP4).
    /// Units are measured at this width, which is the width the list lays its items out at.
    pub(super) fn content_geometry(&self) -> ViewportGeometry {
        let effective = self.latest.effective();
        let width = u16::try_from(self.latest.region().width).unwrap_or(u16::MAX);
        let reserved = width.saturating_sub(
            scrollbar_layout(
                Rect::new(0, 0, width, 1),
                list_scrollbar_policy(self.presented_scrollbar()),
            )
            .content
            .width,
        );
        ViewportGeometry::new(
            effective.width.saturating_sub(u64::from(reserved)),
            effective.height,
        )
    }

    /// The transcript list over `source` with the presented scrollbar, its styles and the
    /// position estimate (VP4, W12).
    fn list_component<'a>(
        &'a self,
        source: &'a TranscriptItemSource<'a>,
    ) -> VirtualListComponent<'a, UnitKey, TranscriptItemSource<'a>, crate::layout::SourcePosition>
    {
        let mode = self.presented_scrollbar();
        self.viewport
            .component(LIST_ID, source, &self.layouts, mode)
            .chrome(transcript_chrome(mode).scrollbar_styles(self.scrollbar_styles))
            .position_estimate(self.position_estimate())
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
            sources: &self.layouts,
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
            sources: &self.layouts,
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
        self.record_hidden_activity(&unit_changes, at);
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
        // A selection whose endpoint unit left the window (W7a ceiling, a removal) is dropped.
        let units = &self.units;
        update.invalidated |= self.selection.retain_resident(|key| units.contains(key));
        let total = self.viewport.index().total_height();
        self.window.observe_rows(total);
        let position = self.presented(at);
        let rows = self
            .viewport
            .edge_rows(&position, self.geometry.height, &self.layouts);
        for issued in self.window.poll_prefetch(rows, self.geometry.height) {
            update.requests.push(HostRequest::Page(issued));
        }
        update.merge(self.settle_interaction(at));
        update
    }

    /// Run eviction against the presented position (W7). Returns the eviction's changes.
    ///
    /// Units holding a selection endpoint are pinned, including against the W7a ceiling trim.
    /// When those pins alone keep the window more than one page above the ceiling (a retained
    /// selection at the trimmed edge while durable events keep arriving), the selection is
    /// dropped and the trim runs without it, so residency stays bounded.
    fn evict(&mut self, at: Instant) -> Option<TranscriptChanges> {
        let changes = self.evict_with(at, true);
        if changes.is_some()
            || self.selection.selection().is_none()
            || self.window.event_count() <= SELECTION_PIN_LIMIT_EVENTS
        {
            return changes;
        }
        let _ = self.selection.set(None, false);
        self.evict_with(at, false)
    }

    fn evict_with(&mut self, at: Instant, pin_selection: bool) -> Option<TranscriptChanges> {
        let position = self.presented(at);
        let viewport_top = self
            .viewport
            .resolve(&position, self.geometry.height, &self.layouts)
            .map_or(0, |resolved| resolved.top_offset);
        let anchor = match &self.nav.mode {
            NavMode::Reading { anchor } | NavMode::TailSpace { anchor } => {
                Some(anchor.unit.clone())
            }
            NavMode::Holding { item, .. } => Some(item.clone()),
            NavMode::Following => None,
        };
        // W7: the motion target and the units holding a selection endpoint are never evicted
        // (the BMUX plan keeps everything between pinned keys and the anchor, so the whole
        // selected range stays resident), including by the W7a ceiling trim during deferral.
        let selection = self
            .selection
            .selection()
            .filter(|_| pin_selection)
            .into_iter()
            .flat_map(|selection| {
                [
                    selection.anchor().key.unit.clone(),
                    selection.focus().key.unit.clone(),
                ]
            });
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
            .chain(selection)
            .collect::<Vec<_>>();
        let context = EvictionContext {
            viewport_top,
            viewport_height: self.geometry.height,
            anchor: anchor.as_ref(),
            pinned: &pinned,
            // W7: a running motion or a selection (gesture or result) defers eviction.
            deferred: self.nav.motion.is_some() || self.selection.state().active,
            resident_rows: self.viewport.index().total_height(),
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

    /// Resolve structure for a document change, derive units and splice the measured index
    /// with exact heights from the layout cache (U4, U5, L1, W3).
    fn apply_changes(&mut self, changes: &TranscriptChanges) -> UnitChanges {
        let items = transcript_items(&self.session);
        let structure_update = self.structure.update(changes, items);
        let units = &self.units;
        let augmented = structure_update.augment(changes, &|id| {
            units.item(items, id).map(|item| item.revision)
        });
        let unit_changes = self.units.apply(
            &augmented,
            items,
            self.window.summary().mode,
            &self.structure,
        );
        if unit_changes.is_structural() {
            let units = &self.units;
            self.layouts.retain(|key| units.contains(key));
        }
        let width = self.geometry.width;
        let inputs = LayoutInputs {
            units: &self.units,
            items,
            structure: &self.structure,
            render: &self.render,
            producer: self.producer.as_ref(),
        };
        let layouts = &mut self.layouts;
        let mut measure = |key: &UnitKey| inputs.measure(layouts, key, width);
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

    /// Re-measure every resident unit at the current width and render configuration (L9).
    /// Units whose retained layout still matches exactly are not laid out again (L1).
    fn remeasure(&mut self) {
        let items = transcript_items(&self.session);
        let width = self.geometry.width;
        let inputs = LayoutInputs {
            units: &self.units,
            items,
            structure: &self.structure,
            render: &self.render,
            producer: self.producer.as_ref(),
        };
        let layouts = &mut self.layouts;
        let mut measure = |key: &UnitKey| inputs.measure(layouts, key, width);
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

mod interaction;
mod selection;

mod tools;
pub use tools::{DefaultRuleRefusal, DetailCycle, InlineInteractionPlacement};

pub use interaction::VisibleHitRegion;
mod submission;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod window_tests;
