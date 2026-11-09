//! Scenario driver for the V2 transcript engine (`bcode_tui_transcript`).
//!
//! Phase 2 drives the engine façade headlessly: no host wiring exists yet (spec section 12).
//! History requests the engine issues become pending scenario requests; inputs reach the
//! engine as BMUX terminal events routed to the transcript list (wheel) or as the navigation
//! intents the host keymap will produce for the default transcript keys; frames are prepared,
//! painted through BMUX into a buffer and committed, and observations are read from the painted
//! rows exactly as for V1 (marker tokens per row).
//!
//! The transcript region is the whole terminal: there is no composer, status row or latest bar
//! before phase 4. Inputs that need the host (composer typing, focus mode, pointer gestures,
//! presentation rules, clipboard) panic with the missing capability, so scenarios that need
//! them fail loudly instead of passing vacuously.

use std::time::{Duration, Instant};

use bcode_session_models::SessionEvent;
use bcode_tui_transcript::engine::{EngineUpdate, HostRequest, TranscriptEngine};
use bcode_tui_transcript::frame::FrameClock;
use bcode_tui_transcript::navigation::{InputSource, MotionConfig, NavIntent, Placement};
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
    HistoryRequest, HistoryResponse, PageLimits, PendingHistoryRequest, RequestId, ResidentSeed,
    ScenarioEngine, ScenarioInput, ScenarioIntent, ScenarioSetting, ScenarioSetup,
    TranscriptScenarioDriver, WheelDirection,
};
use super::frame::{CellRect, FocusObservation, ScenarioFrame};

/// Page sizing V2 asks for: filled to the per-request caps (W6, 11-P1).
const V2_PAGE_LIMITS: PageLimits = PageLimits {
    target_items: PageCaps::PAGE.max_items,
    target_rows: 0,
    max_items: PageCaps::PAGE.max_items,
    max_events: PageCaps::PAGE.max_events_scanned,
};

/// Events on each side of a timeline target before alignment (W8).
const V2_AROUND_HALF_WINDOW: usize = 256;

/// V2 engine under scenario control.
pub struct V2Driver {
    engine: TranscriptEngine,
    width: u16,
    height: u16,
    pending: Vec<(RequestId, HostRequest)>,
    next_request: u64,
    /// Origin of scenario time; only differences from it matter.
    origin: Instant,
    /// Scenario time advanced so far.
    elapsed: Duration,
    /// First canonical sequence of the session (session starts never change).
    first_sequence: Option<u64>,
    /// Newest canonical sequence known to exist (setup plus live deliveries).
    last_sequence: Option<u64>,
}

impl V2Driver {
    fn now(&self) -> Instant {
        self.origin + self.elapsed
    }

    fn record(&mut self, update: EngineUpdate) {
        for request in update.requests {
            self.next_request += 1;
            self.pending.push((RequestId(self.next_request), request));
        }
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
    /// Keys that need the host (composer, focus mode) are not expressible before phase 4.
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
                "V2 driver: key {name:?} needs the phase-4 host (composer, focus mode and keymap)"
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

    fn history_request(request: &HostRequest) -> HistoryRequest {
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
                WindowRequest::LiveState => {
                    panic!("V2 driver: live-state rehydration needs the phase-5 host")
                }
            },
        }
    }
}

impl TranscriptScenarioDriver for V2Driver {
    const ENGINE: ScenarioEngine = ScenarioEngine::V2;

    fn start(setup: &ScenarioSetup) -> Self {
        let engine = TranscriptEngine::new(
            ViewportGeometry::new(u64::from(setup.width), u64::from(setup.height)),
            MotionConfig::default(),
        );
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
            .filter(|(_, request)| self.engine.is_awaiting(request))
            .map(|(id, request)| PendingHistoryRequest {
                id: *id,
                request: Self::history_request(request),
            })
            .collect()
    }

    fn complete_request(&mut self, id: RequestId, response: HistoryResponse) {
        let position = self
            .pending
            .iter()
            .position(|(pending, _)| *pending == id)
            .expect("completed request must be pending");
        let (_, request) = self.pending.remove(position);
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
            ScenarioInput::Click { .. }
            | ScenarioInput::Press { .. }
            | ScenarioInput::DragTo { .. }
            | ScenarioInput::Release { .. } => panic!(
                "V2 driver: pointer gestures need the phase-4 host (latest bar) and phase-6 \
                 selection and click routing"
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
        let snapshot = self.engine.prepare_frame(FrameClock { now: self.now() });
        let area = Rect::new(0, 0, self.width, self.height);
        let mut buffer = Buffer::empty(area);
        let rows = {
            let mut frame = Frame::new(&mut buffer);
            self.engine
                .paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
            (0..self.height)
                .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
                .collect()
        };
        let update = self.engine.commit(snapshot.id);
        self.record(update);
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
            following_bottom: snapshot.following,
            motion_running: snapshot.motion_running,
            focus: FocusObservation::Composer,
            focused_target: None,
        }
    }

    fn resident_sequences(&self) -> Option<(u64, u64)> {
        let window = self.engine.window();
        Some((window.first_sequence?, window.last_sequence?))
    }

    fn selection_text(&self) -> Option<String> {
        None
    }

    fn configure(&mut self, setting: &ScenarioSetting) {
        match setting {
            ScenarioSetting::ReducedMotion(reduce) => {
                let motion = if *reduce {
                    MotionConfig::INSTANT
                } else {
                    MotionConfig::default()
                };
                let at = self.now();
                let update = self.engine.set_motion(motion, at);
                self.record(update);
            }
            ScenarioSetting::PresentationRules(_) => {
                panic!("V2 driver: presentation rules need phase-3 structure resolution")
            }
            ScenarioSetting::Selection { .. } | ScenarioSetting::ClipboardFails(_) => {
                panic!("V2 driver: selection and clipboard need phase 6")
            }
        }
    }
}
