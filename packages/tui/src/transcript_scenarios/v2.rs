//! Scenario driver for the V2 transcript engine in the application (TUI V2 phase 4).
//!
//! Drives the real [`BcodeRuntimeModel`] with the V2 engine selected
//! ([`bcode_config::TuiEngine::V2`]) through `Program::update` (terminal input, session stream
//! messages, effect completions, timers) and presents through the production root presenter,
//! then commits through `Program::presentation_committed`. Keys and pointer input therefore
//! take the application's routing: the keymap, the composer, the transcript focus model and the
//! V2 host seams in the root model, exactly as in the application.
//!
//! No BMUX runtime loop runs. Effects queued by an update or a commit are collected afterwards:
//! the V2 host's history effects (`LoadTranscriptV2History`) become pending scenario requests,
//! answered from the scripted session through the host's production history executor (against
//! a daemon double over the scripted response) and delivered as their effect results; every other
//! effect, V1 history loads of the hidden host-shell transcript included, is dropped (there is
//! no daemon). The attach `Tail` window is answered with the setup's resident seed.
//!
//! Time is scenario time: the host clock is scripted ([`TranscriptV2Host::set_scripted_now`]),
//! so every input, timer and frame the V2 host reduces uses it, and `advance_clock` delivers the
//! production V2 timer (`bcode.transcript_v2`) through `update`.
//!
//! Observations come from the committed frame: the rows of the presented terminal buffer, the
//! regions the V2 host painted (viewport and latest bar) and the prepared snapshot that frame
//! painted (following, motion, focus).

use std::time::{Duration, Instant};

use std::future::Future as _;

use bcode_session_models::{
    SessionEvent, SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery,
};
use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::layout::LayoutWorkCounters;
use bcode_tui_transcript::window::{PageCaps, PageRequest, WindowRequest};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui_runtime::{Presenter as _, Program as _, RuntimeEvent, TimerId};

use super::driver::{
    HistoryRequest, HistoryResponse, LayoutWork, PageLimits, PendingHistoryRequest,
    RepaintObservation, RequestId, ResidentSeed, ScenarioEngine, ScenarioInput, ScenarioIntent,
    ScenarioScrollbar, ScenarioSetting, ScenarioSetup, TranscriptScenarioDriver, WheelDirection,
    WindowWork,
};
use super::frame::{CellRect, FocusObservation, ScenarioFrame};
use crate::effects::{TuiEffect, TuiEffectResult};
use crate::history_flow::SessionStreamUpdate;
use crate::root_program::{BcodeRuntimeMessage, BcodeRuntimeModel};
use crate::transcript_v2_host::TranscriptV2Host;
use crate::transcript_v2_host::effects::{HistoryLoad, HistoryLoaded};

/// Page sizing V2 asks for: filled to the per-request caps (W6, 11-P1).
const V2_PAGE_LIMITS: PageLimits = PageLimits {
    target_items: PageCaps::PAGE.max_items,
    target_rows: 0,
    max_items: PageCaps::PAGE.max_items,
    max_events: PageCaps::PAGE.max_events_scanned,
};

/// Events the host reads on each side of a timeline target before alignment (W8): half the
/// window cap at or before the target, the rest after it.
const V2_AROUND_HALF_WINDOW: usize = PageCaps::WINDOW.max_events_scanned / 2;

/// The root model's V2 transcript timer (motion frames, grace expiry, glow frames).
const TRANSCRIPT_V2_TIMER: &str = "bcode.transcript_v2";

/// V2 engine in the application under scenario control.
pub struct V2Driver {
    model: BcodeRuntimeModel,
    terminal: bmux_tui::terminal::Terminal<std::io::Sink>,
    /// V2 history effects not yet completed, in issue order.
    pending: Vec<(RequestId, HistoryLoad)>,
    next_request: u64,
    /// Origin of scenario time; only differences from it matter.
    origin: Instant,
    /// Scenario time advanced so far.
    elapsed: Duration,
    /// Repaint scope reported by the last presentation.
    last_repaint: Option<RepaintObservation>,
    /// First and newest canonical sequences of the session (setup plus live deliveries): the
    /// daemon double answers the host's single-event session-bound probes from them.
    session_bounds: Option<(u64, u64)>,
}

impl V2Driver {
    fn host(&self) -> &TranscriptV2Host {
        self.model
            .transcript_v2_for_test()
            .expect("the V2 scenario driver selects the V2 engine")
    }

    fn host_mut(&mut self) -> &mut TranscriptV2Host {
        self.model
            .transcript_v2_mut_for_test()
            .expect("the V2 scenario driver selects the V2 engine")
    }

    fn update(&mut self, event: RuntimeEvent<BcodeRuntimeMessage>) {
        let _update = self
            .model
            .update(event)
            .expect("V2 scenario update must succeed");
        self.collect_effects();
    }

    fn terminal_event(&mut self, event: Event) {
        self.update(RuntimeEvent::Terminal(event));
    }

    /// Deliver the V2 timer: due engine timers fire at the current scenario time, and the
    /// end-of-update flush follows the session, applies configuration and starts effects.
    fn sync(&mut self) {
        self.update(RuntimeEvent::Timer(TimerId::new(TRANSCRIPT_V2_TIMER)));
    }

    /// Keep V2 history loads as pending requests; drop work that needs a daemon. A new load
    /// replaces the pending load of its lane, as the effect queue does.
    fn collect_effects(&mut self) {
        let (scheduled, _notes) = self.model.chat.pending_effects.drain_runtime();
        for (_schedule, effect) in scheduled {
            if let TuiEffect::LoadTranscriptV2History { load } = effect {
                self.pending
                    .retain(|(_, pending)| pending.lane() != load.lane());
                self.next_request += 1;
                self.pending.push((RequestId(self.next_request), load));
            }
        }
    }

    /// Deliver the result of `load` through the root model's effect-completion path.
    fn deliver(&mut self, load: HistoryLoad, loaded: HistoryLoaded) {
        let result = TuiEffectResult::TranscriptV2HistoryLoaded {
            load,
            result: Ok(loaded),
        };
        self.update(RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
            Box::new(result),
        )));
    }

    /// Answer the attach `Tail` window with the setup's resident window.
    fn seed(&mut self, setup: &ScenarioSetup) {
        self.sync();
        let position = self
            .pending
            .iter()
            .position(|(_, load)| {
                matches!(load.request, HostRequest::Window(issued)
                    if issued.request == WindowRequest::Tail)
            })
            .expect("V2 attach must request the tail window");
        let (_, load) = self.pending.remove(position);
        let events = setup.session.events();
        let resident = match setup.resident {
            ResidentSeed::Latest { events: count } => setup.session.latest(count),
            ResidentSeed::Everything => events,
        };
        let loaded = execute(
            load,
            &HistoryResponse {
                events: resident.to_vec(),
                has_older: resident.len() < events.len(),
                has_newer: false,
            },
            self.session_bounds,
        );
        self.deliver(load, loaded);
    }

    fn history_request(load: &HistoryLoad) -> HistoryRequest {
        match load.request {
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
                    after_events: PageCaps::WINDOW.max_events_scanned - V2_AROUND_HALF_WINDOW,
                },
                WindowRequest::Head => HistoryRequest::Head {
                    max_events: PageCaps::WINDOW.max_events_scanned,
                },
                WindowRequest::Tail => HistoryRequest::Tail {
                    max_events: PageCaps::WINDOW.max_events_scanned,
                },
                WindowRequest::LiveState => HistoryRequest::LiveState {
                    after_sequence: load.resident_last,
                    max_events: PageCaps::PAGE.max_events_scanned,
                },
            },
        }
    }

    fn mouse(&mut self, kind: MouseEventKind, at: super::driver::CellPoint) {
        self.terminal_event(Event::Mouse(MouseEvent::new(kind, Point::new(at.x, at.y))));
    }
}

/// Execute `load` with the host's production history executor
/// ([`crate::transcript_v2_host::effects::load_history`]: bounded reads, the `Around` split,
/// page validation, session bounds) against a daemon double holding exactly the scripted
/// `response`: its events, with more history beyond them where the response says so. The
/// host's single-event session-bound probes are answered from `bounds` (the canonical session's
/// first and newest sequences), so window results place the reader within the whole session.
fn execute(
    load: HistoryLoad,
    response: &HistoryResponse,
    bounds: Option<(u64, u64)>,
) -> HistoryLoaded {
    let read = |query: SessionHistoryQuery| {
        std::future::ready(Ok::<_, bcode_client::ClientError>(
            bound_probe(load, response, &query, bounds)
                .unwrap_or_else(|| daemon_page(load, response, query)),
        ))
    };
    let future = std::pin::pin!(crate::transcript_v2_host::effects::load_history(load, read));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.poll(&mut context) {
        std::task::Poll::Ready(result) => {
            result.expect("scripted history responses are consistent daemon pages")
        }
        std::task::Poll::Pending => unreachable!("the daemon double answers immediately"),
    }
}

/// Answer a single-event cursorless read (the host's session-bound probe) at a session end the
/// scripted `response` does not reach, with that end's canonical event sequence.
fn bound_probe(
    load: HistoryLoad,
    response: &HistoryResponse,
    query: &SessionHistoryQuery,
    bounds: Option<(u64, u64)>,
) -> Option<SessionHistoryPage> {
    let (first, last) = bounds?;
    if query.cursor.is_some() || query.limit != 1 {
        return None;
    }
    let (beyond_edge, sequence, template) = match query.direction {
        SessionHistoryDirection::Forward => (response.has_older, first, response.events.first()?),
        SessionHistoryDirection::Backward => (response.has_newer, last, response.events.last()?),
    };
    if !beyond_edge {
        return None;
    }
    // Only the sequence of a bound probe is read; the event body is the response's edge event.
    let mut event = template.clone();
    event.sequence = sequence;
    Some(SessionHistoryPage {
        session_id: load.session_id,
        events: vec![event],
        compatibility_issues: Vec::new(),
        next_cursor: None,
        has_more: first != last,
    })
}

/// One daemon read over the scripted `response`: the events at or past the cursor in the
/// query direction, bounded by its limit, ascending; more remains past the limit or past the
/// response's own edge in that direction.
fn daemon_page(
    load: HistoryLoad,
    response: &HistoryResponse,
    query: SessionHistoryQuery,
) -> SessionHistoryPage {
    let cursor = query.cursor.map(|cursor| cursor.sequence);
    let (mut events, beyond_edge): (Vec<SessionEvent>, bool) = match query.direction {
        SessionHistoryDirection::Forward => (
            response
                .events
                .iter()
                .filter(|event| cursor.is_none_or(|cursor| event.sequence >= cursor))
                .cloned()
                .collect(),
            response.has_newer,
        ),
        SessionHistoryDirection::Backward => (
            response
                .events
                .iter()
                .rev()
                .filter(|event| cursor.is_none_or(|cursor| event.sequence <= cursor))
                .cloned()
                .collect(),
            response.has_older,
        ),
    };
    let has_more = events.len() > query.limit || (beyond_edge && !events.is_empty());
    events.truncate(query.limit);
    if query.direction == SessionHistoryDirection::Backward {
        events.reverse();
    }
    SessionHistoryPage {
        session_id: load.session_id,
        events,
        compatibility_issues: Vec::new(),
        next_cursor: None,
        has_more,
    }
}

const fn cell_rect(rect: Rect) -> CellRect {
    CellRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

impl TranscriptScenarioDriver for V2Driver {
    const ENGINE: ScenarioEngine = ScenarioEngine::V2;

    fn start(setup: &ScenarioSetup) -> Self {
        let mut model = super::v1::scenario_model(setup, bcode_config::TuiEngine::V2);
        let origin = Instant::now();
        model
            .transcript_v2_mut_for_test()
            .expect("V2 engine selected at startup")
            .set_scripted_now(origin);
        super::v1::install_scenario_clipboard(&mut model);
        let terminal = bmux_tui::terminal::Terminal::new(
            std::io::sink(),
            Rect::new(0, 0, setup.width, setup.height),
        );
        let mut driver = Self {
            model,
            terminal,
            pending: Vec::new(),
            next_request: 0,
            origin,
            elapsed: Duration::ZERO,
            last_repaint: None,
            session_bounds: setup
                .session
                .events()
                .first()
                .zip(setup.session.events().last())
                .map(|(first, last)| (first.sequence, last.sequence)),
        };
        driver.seed(setup);
        driver
    }

    fn wheel_rows(&self) -> u16 {
        u16::try_from(self.model.settings.mouse_scroll_rows()).unwrap_or(u16::MAX)
    }

    fn deliver_live(&mut self, event: SessionEvent) {
        let sequence = event.sequence;
        self.session_bounds = Some(
            self.session_bounds
                .map_or((sequence, sequence), |(first, last)| {
                    (first.min(sequence), last.max(sequence))
                }),
        );
        self.update(RuntimeEvent::Message(BcodeRuntimeMessage::SessionStream(
            Box::new(SessionStreamUpdate::Event(Box::new(
                bcode_ipc::Event::Session(event),
            ))),
        )));
    }

    fn pending_requests(&self) -> Vec<PendingHistoryRequest> {
        let engine = self.host().engine();
        self.pending
            .iter()
            .filter(|(_, load)| engine.is_awaiting(&load.request))
            .map(|(id, load)| PendingHistoryRequest {
                id: *id,
                request: Self::history_request(load),
            })
            .collect()
    }

    fn complete_request(&mut self, id: RequestId, response: HistoryResponse) {
        let position = self
            .pending
            .iter()
            .position(|(pending, _)| *pending == id)
            .expect("completed request must be pending");
        let (_, load) = self.pending.remove(position);
        let loaded = execute(load, &response, self.session_bounds);
        self.deliver(load, loaded);
    }

    fn input(&mut self, input: ScenarioInput) {
        match input {
            ScenarioInput::Key(name) => {
                let stroke = bmux_keyboard::parse_key_stroke(&name)
                    .unwrap_or_else(|_| panic!("invalid scenario key {name:?}"));
                self.terminal_event(Event::Key(stroke));
            }
            ScenarioInput::Wheel { direction, at } => {
                let kind = match direction {
                    WheelDirection::Up => MouseEventKind::ScrollUp,
                    WheelDirection::Down => MouseEventKind::ScrollDown,
                };
                self.mouse(kind, at);
            }
            ScenarioInput::Click { at } => {
                self.mouse(MouseEventKind::Down(MouseButton::Left), at);
                self.mouse(MouseEventKind::Up(MouseButton::Left), at);
            }
            ScenarioInput::Press { at } => self.mouse(MouseEventKind::Down(MouseButton::Left), at),
            ScenarioInput::DragTo { at } => {
                self.mouse(MouseEventKind::Drag(MouseButton::Left), at);
            }
            ScenarioInput::Release { at } => self.mouse(MouseEventKind::Up(MouseButton::Left), at),
            ScenarioInput::Resize { width, height } => {
                self.terminal.resize(Rect::new(0, 0, width, height));
                self.terminal_event(Event::Resize(Size { width, height }));
            }
        }
    }

    fn navigate(&mut self, intent: ScenarioIntent) {
        match intent {
            ScenarioIntent::JumpToSequence(sequence) => {
                // The timeline dialog's jump path, which the root model routes to the V2 host.
                self.model.timeline_jump_for_test(sequence);
                self.sync();
            }
            // The jump-to-latest binding the latest bar names.
            ScenarioIntent::JumpToLatest => self.input(ScenarioInput::Key("ctrl+end".to_owned())),
        }
    }

    fn advance_clock(&mut self, by: Duration) {
        self.elapsed = self.elapsed.saturating_add(by);
        let now = self.origin + self.elapsed;
        self.host_mut().set_scripted_now(now);
        self.sync();
    }

    fn present(&mut self) -> ScenarioFrame {
        let mut output = Vec::new();
        let mut staged = self.terminal.fork(&mut output);
        let full_damage = self.model.presentation_damage_is_full_for_test()
            || self.terminal.retained_buffer().is_none();
        let report = crate::root_program::bcode_runtime_presenter(&mut staged)
            .present(&mut self.model)
            .expect("V2 scenario presentation must succeed");
        self.terminal.acknowledge(staged);
        self.last_repaint = Some(RepaintObservation {
            full: full_damage || report.full_repaint,
            changed_cells: report.changed_cells,
        });
        let _update = self.model.presentation_committed(report);
        self.collect_effects();
        let buffer = self
            .terminal
            .retained_buffer()
            .expect("committed frame retains its buffer");
        let rows = (0..buffer.area().height)
            .map(|row| buffer.row_symbols(row).unwrap_or_default())
            .collect();
        let host = self.host();
        let regions = host.regions();
        let transcript = regions
            .viewport
            .or(regions.transcript)
            .map_or_else(CellRect::default, cell_rect);
        let frame = host.last_frame();
        let target = host.focus().transcript_target();
        ScenarioFrame {
            index: 0,
            rows,
            transcript,
            latest_bar: regions.latest_bar.map(cell_rect),
            following_bottom: frame.is_some_and(|frame| frame.following),
            motion_running: frame.is_some_and(|frame| frame.motion_running),
            focus: if target.is_some() {
                FocusObservation::Transcript { units: true }
            } else {
                FocusObservation::Composer
            },
            focused_target: target.map(|target| format!("{target:?}")),
            scrollbar: regions.scrollbar.map(cell_rect),
        }
    }

    fn window_work(&self) -> Option<WindowWork> {
        Some(super::v2_headless::window_work(
            self.host().engine().window_work(),
        ))
    }

    fn resident_event_count(&self) -> Option<u64> {
        Some(self.host().engine().window().event_count)
    }

    fn resident_sequences(&self) -> Option<(u64, u64)> {
        let window = self.host().engine().window();
        Some((window.first_sequence?, window.last_sequence?))
    }

    /// Transcript selection is phase 6 in V2; the host keeps none.
    fn selection_text(&self) -> Option<String> {
        None
    }

    fn configure(&mut self, setting: &ScenarioSetting) {
        let mut config = self.model.chat.app.tui_config().clone();
        match setting {
            ScenarioSetting::PresentationRules(rules) => {
                let presentation = toml::from_str::<bcode_config::PresentationConfig>(rules)
                    .unwrap_or_else(|error| panic!("invalid scenario rules: {error}"));
                self.model
                    .chat
                    .app
                    .apply_configured_structure_rules(presentation.rules);
            }
            ScenarioSetting::ClipboardFails(fails) => {
                super::v1::set_scenario_clipboard_fails(*fails);
                return;
            }
            ScenarioSetting::Selection {
                auto_copy_on_release,
                clear_after_copy,
                clear_delay_ms,
            } => {
                config.selection.auto_copy_on_mouse_release = *auto_copy_on_release;
                config.selection.clear_after_copy = *clear_after_copy;
                config.selection.clear_delay_ms = *clear_delay_ms;
                self.model.settings.apply_tui_config(&config);
                self.model.chat.app.apply_tui_config(config);
            }
            ScenarioSetting::TranscriptScrollbar(mode) => {
                config.transcript_scrollbar = match mode {
                    ScenarioScrollbar::Off => bcode_config::TuiTranscriptScrollbar::Off,
                    ScenarioScrollbar::Overlay => bcode_config::TuiTranscriptScrollbar::Overlay,
                    ScenarioScrollbar::Gutter => bcode_config::TuiTranscriptScrollbar::Gutter,
                };
                self.model.settings.apply_tui_config(&config);
                self.model.chat.app.apply_tui_config(config);
            }
            ScenarioSetting::ReducedMotion(reduce) => {
                config.reduce_motion = *reduce;
                // The configuration reload path applies TUI settings to both owners.
                self.model.settings.apply_tui_config(&config);
                self.model.chat.app.apply_tui_config(config);
            }
        }
        self.model.request_full_redraw_for_test();
        // The V2 host picks configuration up at the end of the next update.
        self.sync();
    }

    fn clipboard_writes(&self) -> Vec<String> {
        super::v1::scenario_clipboard_writes()
    }

    fn last_repaint(&self) -> Option<RepaintObservation> {
        self.last_repaint
    }

    fn layout_work(&mut self) -> Option<LayoutWork> {
        let LayoutWorkCounters {
            markdown_parses,
            highlights,
            projections,
            measurements,
            producer_builds,
        } = self.host_mut().take_layout_work();
        Some(LayoutWork {
            markdown_parses,
            highlights,
            projections,
            measurements,
            producer_builds,
        })
    }

    fn focused_extent(&self) -> Option<std::ops::Range<i64>> {
        let host = self.host();
        let engine = host.engine();
        let target = engine.focus().transcript_target()?;
        let unit = host
            .last_frame()?
            .viewport
            .as_ref()?
            .units
            .iter()
            .find(|unit| unit.key == target.unit)?;
        // The rows the target governs, which focus traversal reveals (C32).
        let rows = engine.focus_view().extent(target)?;
        let offset = |row: u64| {
            unit.top
                .saturating_add(i64::try_from(row).unwrap_or(i64::MAX))
        };
        Some(offset(rows.start)..offset(rows.end))
    }
}
