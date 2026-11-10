//! Headless scenario driver for the V2 transcript engine façade (`bcode_tui_transcript`).
//!
//! The phase 2–3 driver, kept for engine-level checks that isolate the engine from the host:
//! it drives the engine façade without the root model. The application path (root model,
//! V2 host, keymap and root presenter) is [`super::v2::V2Driver`].
//! History requests the engine issues become pending scenario requests; inputs reach the
//! engine as BMUX terminal events routed to the transcript list (wheel), as the navigation
//! intents the host keymap will produce for the default transcript keys, or as clicks hit-tested
//! against the committed frame's retained item geometry; frames are prepared, painted through
//! BMUX into a buffer and committed, and observations are read from the painted rows exactly as
//! for V1 (marker tokens per row).
//!
//! Units are rendered by the engine's standard producers (structure v2, chrome, Markdown, tools,
//! groups) with the bundled default theme, the theme's structural layers and its motion level,
//! as the host will configure them; the fake session embeds marker tokens in message and tool
//! text, so markers stay observable through item chrome.
//!
//! The transcript region is the whole terminal: there is no composer, status row or latest bar
//! (the host owns them). Inputs that need the host (composer typing, focus mode, drag gestures
//! and their click-versus-drag recognition, the copy policy and the clipboard) panic with the
//! missing capability, so scenarios that need them fail loudly instead of passing vacuously;
//! run those through [`super::v2::V2Driver`], which covers selection and copy end to end. The
//! selection text this driver reports is the engine's own selection export.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bcode_presentation_policy::models::PresentationRule;
use bcode_session_models::SessionEvent;
use bcode_tui_theme::definition::{ResolvedThemeDefinition, ThemeCatalog, ThemeSelection};
use bcode_tui_transcript::engine::{EngineUpdate, HostRequest, TranscriptEngine};
use bcode_tui_transcript::frame::{FrameClock, FrameSnapshot};
use bcode_tui_transcript::layout::RenderConfig;
use bcode_tui_transcript::navigation::{InputSource, MotionConfig, NavIntent, Placement};
use bcode_tui_transcript::scenario::LayoutWorkCounters;
use bcode_tui_transcript::units::structure::PolicyLayers;
use bcode_tui_transcript::viewport::ViewportGeometry;
use bcode_tui_transcript::window::{
    HistoryPageResult, HistoryWindowResult, LiveInput, PageCaps, PageDirection, PageRequest,
    WindowRequest,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::event::{Event, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::PaintCx;

use super::driver::{
    HistoryRequest, HistoryResponse, LayoutWork, PageLimits, PendingHistoryRequest, RequestId,
    ResidentSeed, ScenarioEngine, ScenarioInput, ScenarioIntent, ScenarioSetting, ScenarioSetup,
    TranscriptScenarioDriver, WheelDirection, WindowWork,
};
use super::frame::{CellRect, FocusObservation, ScenarioFrame};

/// The engine's window-operation counters as the engine-neutral observation.
pub(super) const fn window_work(work: bcode_tui_transcript::window::WindowWork) -> WindowWork {
    let bcode_tui_transcript::window::WindowWork {
        replacements,
        fallbacks,
        prepends,
        appends,
        evictions,
        events_reduced,
        eviction_plans: _,
    } = work;
    WindowWork {
        replacements,
        fallbacks,
        prepends,
        appends,
        evictions,
        events_reduced,
    }
}

/// Page sizing V2 asks for: filled to the per-request caps (W6, 11-P1).
const V2_PAGE_LIMITS: PageLimits = PageLimits {
    target_items: PageCaps::PAGE.max_items,
    target_rows: 0,
    max_items: PageCaps::PAGE.max_items,
    max_events: PageCaps::PAGE.max_events_scanned,
};

/// Events on each side of a timeline target before alignment (W8).
const V2_AROUND_HALF_WINDOW: usize = 256;

/// Theme the host selects by default (`[tui.theme] name`).
const DEFAULT_THEME: &str = "terminal-native";

/// The bundled default theme, resolved as the host resolves it without overlays.
fn default_theme() -> Arc<ResolvedThemeDefinition> {
    let resolved = ThemeCatalog::bundled()
        .and_then(|catalog| catalog.resolve(&ThemeSelection::new(DEFAULT_THEME)))
        .expect("the bundled default theme resolves");
    Arc::new(resolved)
}

/// V2 engine under scenario control.
pub struct V2HeadlessDriver {
    engine: TranscriptEngine,
    width: u16,
    height: u16,
    /// Requests not yet completed, with the newest resident sequence at issue (`LiveState`
    /// reads after it, as the host's executor does).
    pending: Vec<(RequestId, HostRequest, Option<u64>)>,
    next_request: u64,
    /// Origin of scenario time; only differences from it matter.
    origin: Instant,
    /// Scenario time advanced so far.
    elapsed: Duration,
    /// First canonical sequence of the session (session starts never change).
    first_sequence: Option<u64>,
    /// Newest canonical sequence known to exist (setup plus live deliveries).
    last_sequence: Option<u64>,
    /// Theme items render with; its structural layers and motion level apply too (X4).
    theme: Arc<ResolvedThemeDefinition>,
    /// Configured `[presentation]` rules (scenario settings).
    config_rules: Vec<PresentationRule>,
    /// `tui.reduce_motion`.
    reduce_motion: bool,
    /// Last committed frame; clicks are hit-tested against it.
    committed: Option<FrameSnapshot>,
}

impl V2HeadlessDriver {
    fn now(&self) -> Instant {
        self.origin + self.elapsed
    }

    fn record(&mut self, update: EngineUpdate) {
        let resident_last = self.engine.window().last_sequence;
        for request in update.requests {
            self.next_request += 1;
            self.pending
                .push((RequestId(self.next_request), request, resident_last));
        }
    }

    /// Recompile the structure policy from the theme layers and configured rules and apply it
    /// as a configuration reload would.
    fn apply_structure(&mut self) {
        let (config, diagnostics) = PolicyLayers {
            theme: Some(&self.theme),
            config_rules: &self.config_rules,
            ..PolicyLayers::default()
        }
        .compile_with_host_parts();
        assert!(
            diagnostics.is_empty(),
            "scenario rules must compile cleanly: {diagnostics:?}"
        );
        let at = self.now();
        let update = self.engine.set_structure_config(config, at);
        self.record(update);
    }

    fn intent(&mut self, intent: NavIntent) {
        let at = self.now();
        let update = self.engine.intent(intent, at);
        self.record(update);
    }

    fn list_event(&mut self, event: &Event) {
        let at = self.now();
        let (update, _handled) = self.engine.list_event(event, at);
        self.record(update);
    }

    /// Translate a key the host keymap binds to transcript navigation in every focus (FO5).
    /// Keys that need the host (composer, focus mode) are not expressible without it.
    fn key(&mut self, name: &str) {
        let intent = match name {
            "pageup" => NavIntent::PageBy { pages: -1 },
            "pagedown" => NavIntent::PageBy { pages: 1 },
            "ctrl+up" => NavIntent::ScrollBy {
                rows: -1,
                source: InputSource::Key,
            },
            "ctrl+down" => NavIntent::ScrollBy {
                rows: 1,
                source: InputSource::Key,
            },
            "ctrl+home" => NavIntent::JumpToSessionStart,
            "ctrl+end" => NavIntent::JumpToLatest,
            _ => panic!(
                "V2 headless driver: key {name:?} needs the host (composer, focus mode and keymap)"
            ),
        };
        self.intent(intent);
    }

    /// Answer the attach request with the setup's resident window.
    fn seed(&mut self, setup: &ScenarioSetup) {
        let at = self.now();
        let update = self.engine.attach(at);
        let Some(HostRequest::Window(issued)) = update.requests.first().copied() else {
            panic!("V2 attach must request the tail window");
        };
        let events = setup.session.events();
        let resident = match setup.resident {
            ResidentSeed::Latest { events: count } => setup.session.latest(count),
            ResidentSeed::Everything => events,
        };
        let result = HistoryWindowResult {
            generation: issued.generation,
            target: issued.request,
            events: resident.to_vec(),
            has_older: resident.len() < events.len(),
            has_newer: false,
            first_available_sequence: self.first_sequence,
            last_available_sequence: self.last_sequence,
        };
        let update = self
            .engine
            .apply_window(&result, at)
            .expect("V2 attach window applies");
        self.record(update);
    }

    fn history_request(request: &HostRequest, resident_last: Option<u64>) -> HistoryRequest {
        match *request {
            HostRequest::Page(issued) => match issued.request {
                PageRequest::Older { before_sequence } => HistoryRequest::Older {
                    before_sequence,
                    limits: V2_PAGE_LIMITS,
                },
                PageRequest::Newer { after_sequence } => HistoryRequest::Newer {
                    after_sequence,
                    limits: V2_PAGE_LIMITS,
                },
            },
            HostRequest::Window(issued) => match issued.request {
                WindowRequest::Around { sequence } => HistoryRequest::Around {
                    sequence,
                    before_events: V2_AROUND_HALF_WINDOW,
                    after_events: V2_AROUND_HALF_WINDOW,
                },
                WindowRequest::Head => HistoryRequest::Head {
                    max_events: PageCaps::WINDOW.max_events_scanned,
                },
                WindowRequest::Tail => HistoryRequest::Tail {
                    max_events: PageCaps::WINDOW.max_events_scanned,
                },
                WindowRequest::LiveState => HistoryRequest::LiveState {
                    after_sequence: resident_last,
                    max_events: PageCaps::PAGE.max_events_scanned,
                },
            },
        }
    }
}

impl TranscriptScenarioDriver for V2HeadlessDriver {
    const ENGINE: ScenarioEngine = ScenarioEngine::V2Headless;

    fn start(setup: &ScenarioSetup) -> Self {
        let theme = default_theme();
        let (structure, diagnostics) = PolicyLayers {
            theme: Some(&theme),
            ..PolicyLayers::default()
        }
        .compile_with_host_parts();
        assert!(
            diagnostics.is_empty(),
            "theme layers compile: {diagnostics:?}"
        );
        let engine = TranscriptEngine::new(
            ViewportGeometry::new(u64::from(setup.width), u64::from(setup.height)),
            MotionConfig::for_motion(theme.chrome.motion, false),
        )
        .with_structure_config(structure)
        .with_render_config(RenderConfig::for_theme(Arc::clone(&theme)));
        let mut driver = Self {
            engine,
            width: setup.width,
            height: setup.height,
            pending: Vec::new(),
            next_request: 0,
            origin: Instant::now(),
            elapsed: Duration::ZERO,
            first_sequence: setup.session.events().first().map(|event| event.sequence),
            last_sequence: setup.session.last_sequence(),
            theme,
            config_rules: Vec::new(),
            reduce_motion: false,
            committed: None,
        };
        driver.seed(setup);
        driver
    }

    fn wheel_rows(&self) -> u16 {
        self.engine.wheel_rows()
    }

    fn deliver_live(&mut self, event: SessionEvent) {
        self.last_sequence = Some(
            self.last_sequence
                .map_or(event.sequence, |last| last.max(event.sequence)),
        );
        let at = self.now();
        let update = self
            .engine
            .apply_live(LiveInput::Durable(Box::new(event)), at);
        self.record(update);
    }

    fn pending_requests(&self) -> Vec<PendingHistoryRequest> {
        self.pending
            .iter()
            .filter(|(_, request, _)| self.engine.is_awaiting(request))
            .map(|(id, request, resident_last)| PendingHistoryRequest {
                id: *id,
                request: Self::history_request(request, *resident_last),
            })
            .collect()
    }

    fn complete_request(&mut self, id: RequestId, response: HistoryResponse) {
        let position = self
            .pending
            .iter()
            .position(|(pending, _, _)| *pending == id)
            .expect("completed request must be pending");
        let (_, request, _) = self.pending.remove(position);
        let at = self.now();
        let update = match request {
            HostRequest::Page(issued) => {
                let direction = issued.request.direction();
                let has_more = match direction {
                    PageDirection::Older => response.has_older,
                    PageDirection::Newer => response.has_newer,
                };
                let result = HistoryPageResult {
                    generation: issued.generation,
                    direction,
                    events: response.events,
                    has_more,
                    unaligned: false,
                };
                self.engine.apply_page(&result, at)
            }
            HostRequest::Window(issued) if issued.request == WindowRequest::LiveState => {
                // As the host's executor: the events after the resident tail only.
                let result = HistoryWindowResult {
                    generation: issued.generation,
                    target: issued.request,
                    last_available_sequence: (!response.has_newer)
                        .then(|| response.events.last().map(|event| event.sequence))
                        .flatten(),
                    events: response.events,
                    has_older: false,
                    has_newer: response.has_newer,
                    first_available_sequence: None,
                };
                self.engine.apply_window(&result, at)
            }
            HostRequest::Window(issued) => {
                let result = HistoryWindowResult {
                    generation: issued.generation,
                    target: issued.request,
                    events: response.events,
                    has_older: response.has_older,
                    has_newer: response.has_newer,
                    first_available_sequence: self.first_sequence,
                    last_available_sequence: self.last_sequence,
                };
                self.engine.apply_window(&result, at)
            }
        };
        // A stale result (superseded generation) is dropped by the engine, as in the host.
        if let Ok(update) = update {
            self.record(update);
        }
    }

    fn input(&mut self, input: ScenarioInput) {
        match input {
            ScenarioInput::Key(name) => self.key(&name),
            ScenarioInput::Wheel { direction, at } => {
                let kind = match direction {
                    WheelDirection::Up => MouseEventKind::ScrollUp,
                    WheelDirection::Down => MouseEventKind::ScrollDown,
                };
                self.list_event(&Event::Mouse(MouseEvent::new(kind, Point::new(at.x, at.y))));
            }
            ScenarioInput::Resize { width, height } => {
                self.width = width;
                self.height = height;
                let at = self.now();
                let update = self.engine.resize(
                    ViewportGeometry::new(u64::from(width), u64::from(height)),
                    at,
                );
                self.record(update);
            }
            ScenarioInput::Click { at } => {
                // Press and release without movement activate the region under the pointer in
                // the committed frame (X1). Click-versus-drag recognition through the selection
                // (BMUX 14) is the host's; the V2 driver covers it.
                let snapshot = self
                    .committed
                    .clone()
                    .expect("V2 driver: a click needs a committed frame");
                let now = self.now();
                let (update, _hit) =
                    self.engine
                        .click(&snapshot, u64::from(at.x), u64::from(at.y), now);
                self.record(update);
            }
            ScenarioInput::Press { .. }
            | ScenarioInput::DragTo { .. }
            | ScenarioInput::Release { .. } => panic!(
                "V2 headless driver: drag gestures are recognized and routed by the host \
                 (BMUX 13, 14); run selection scenarios through the V2 driver"
            ),
        }
    }

    fn navigate(&mut self, intent: ScenarioIntent) {
        let intent = match intent {
            ScenarioIntent::JumpToSequence(sequence) => NavIntent::JumpToSequence {
                sequence,
                placement: Placement::ItemTopAtTop,
            },
            ScenarioIntent::JumpToLatest => NavIntent::JumpToLatest,
        };
        self.intent(intent);
    }

    fn advance_clock(&mut self, by: Duration) {
        self.elapsed = self.elapsed.saturating_add(by);
        let now = self.now();
        let update = self.engine.advance(now);
        self.record(update);
    }

    fn present(&mut self) -> ScenarioFrame {
        let clock = FrameClock { now: self.now() };
        let snapshot = self.engine.prepare_frame(clock);
        let area = Rect::new(0, 0, self.width, self.height);
        let mut buffer = Buffer::empty(area);
        let rows = {
            let mut frame = Frame::new(&mut buffer);
            // One presentation clock per frame (BMUX 8), as the runtime presenter supplies it.
            frame.set_clock(Some(clock.presentation()));
            self.engine
                .paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
            (0..self.height)
                .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
                .collect()
        };
        let update = self.engine.commit(snapshot.id);
        self.record(update);
        let following_bottom = snapshot.following;
        let motion_running = snapshot.motion_running;
        self.committed = Some(snapshot);
        ScenarioFrame {
            index: 0,
            rows,
            transcript: CellRect {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            },
            latest_bar: None,
            following_bottom,
            motion_running,
            focus: FocusObservation::Composer,
            focused_target: None,
            scrollbar: None,
        }
    }

    fn window_work(&self) -> Option<WindowWork> {
        Some(window_work(self.engine.window_work()))
    }

    fn resident_event_count(&self) -> Option<u64> {
        Some(self.engine.window().event_count)
    }

    fn resident_sequences(&self) -> Option<(u64, u64)> {
        let window = self.engine.window();
        Some((window.first_sequence?, window.last_sequence?))
    }

    /// The engine's selection export (C34). Only the host starts selections, so it stays
    /// empty here; clipboard writes are the host shell's and not observed (the default panics).
    fn selection_text(&self) -> Option<String> {
        self.engine.selected_text()
    }

    fn layout_work(&mut self) -> Option<LayoutWork> {
        let LayoutWorkCounters {
            markdown_parses,
            highlights,
            projections,
            measurements,
            producer_builds,
        } = self.engine.take_layout_work();
        Some(LayoutWork {
            markdown_parses,
            highlights,
            projections,
            measurements,
            producer_builds,
        })
    }

    fn configure(&mut self, setting: &ScenarioSetting) {
        match setting {
            ScenarioSetting::ReducedMotion(reduce) => {
                self.reduce_motion = *reduce;
                let motion = MotionConfig::for_motion(self.theme.chrome.motion, self.reduce_motion);
                let at = self.now();
                let update = self.engine.set_motion(motion, at);
                self.record(update);
            }
            ScenarioSetting::PresentationRules(rules) => {
                let presentation = toml::from_str::<bcode_config::PresentationConfig>(rules)
                    .unwrap_or_else(|error| panic!("invalid scenario rules: {error}"));
                self.config_rules = presentation.rules;
                self.apply_structure();
            }
            ScenarioSetting::Selection { .. } | ScenarioSetting::ClipboardFails(_) => {
                panic!(
                    "V2 headless driver: the copy policy and the clipboard belong to the host \
                     shell; run copy scenarios through the V2 driver"
                )
            }
            ScenarioSetting::TranscriptScrollbar(_) => {
                panic!("V2 headless driver: the scrollbar is painted and hit-tested by the host")
            }
        }
    }
}
