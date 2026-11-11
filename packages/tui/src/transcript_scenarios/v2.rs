//! Scenario driver for the V2 transcript engine in the application (TUI V2 phase 4).
//!
//! Drives the real [`BcodeRuntimeModel`] through `Program::update` (terminal input, session stream
//! messages, effect completions, timers) and presents through the production root presenter,
//! then commits through `Program::presentation_committed`. Keys and pointer input therefore
//! take the application's routing: the keymap, the composer, the transcript focus model and the
//! V2 host seams in the root model, exactly as in the application.
//!
//! No BMUX runtime loop runs. Effects queued by an update or a commit are collected afterwards:
//! the V2 host's history effects (`LoadTranscriptV2History`) become pending scenario requests,
//! answered from the scripted session through the host's production history executor (against
//! a daemon double over the scripted response) and delivered as their effect results; every other
//! effect is dropped (there is no daemon). The attach `Tail` window is answered with the setup's resident seed.
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
    DamageObservation, HistoryRequest, HistoryResponse, LayoutWork, PageLimits,
    PendingHistoryRequest, RepaintObservation, RequestId, ResidentSeed, ScenarioEngine,
    ScenarioInput, ScenarioIntent, ScenarioScrollbar, ScenarioSetting, ScenarioSetup,
    SubmissionOutcome, TranscriptScenarioDriver, WheelDirection, WindowWork,
};
use super::frame::{CellRect, FocusObservation, ScenarioFrame};
use crate::effects::{TuiEffect, TuiEffectResult};
use crate::history_flow::SessionStreamUpdate;
use crate::root_program::{BcodeRuntimeMessage, BcodeRuntimeModel};
use crate::transcript_v2_host::TranscriptV2Host;
use crate::transcript_v2_host::effects::{HistoryLoad, HistoryLoaded};

/// V2-only scenarios of the host chrome around the V2 transcript (C2, C27, C37, X4, FO5, D13).
mod host_chrome;

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
    /// Effective damage of the last presentation, as the terminal committed it (BMUX 15b).
    last_damage: Option<DamageObservation>,
    /// Every presentation rule the user-state double stores (make-default, `m`).
    stored_rules: Vec<bcode_presentation_policy::models::PresentationRule>,
    /// First and newest canonical sequences of the session (setup plus live deliveries): the
    /// daemon double answers the host's single-event session-bound probes from them.
    session_bounds: Option<(u64, u64)>,
    /// Submissions and slash commands awaiting the scenario's answer.
    shell: super::shell::ShellEffects,
}

impl V2Driver {
    fn host(&self) -> &TranscriptV2Host {
        self.model.transcript_host_for_test()
    }

    fn host_mut(&mut self) -> &mut TranscriptV2Host {
        self.model.transcript_host_mut_for_test()
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

    /// Keep V2 history loads as pending requests and the newest submission and slash command
    /// for the scenario to answer; drop work that needs a daemon. A new load
    /// replaces the pending load of its lane, as the effect queue does. Per-tool default rules
    /// (make-default, `m`) are persisted to a user-state double, which succeeds and stores
    /// them, and the result is delivered as the persistence effect's result (A3).
    fn collect_effects(&mut self) {
        let (scheduled, _notes) = self.model.chat.pending_effects.drain_runtime();
        for (_schedule, effect) in scheduled {
            if let TuiEffect::LoadTranscriptV2History { load } = effect {
                self.pending
                    .retain(|(_, pending)| pending.lane() != load.lane());
                self.next_request += 1;
                self.pending.push((RequestId(self.next_request), load));
            } else {
                self.shell.observe(effect);
            }
        }
        if let Some(rules) = self.model.chat.app.take_pending_default_rules() {
            self.stored_rules.extend(rules.iter().cloned());
            let result = TuiEffectResult::PresentationRulesPersisted {
                rules,
                result: Ok((
                    std::path::PathBuf::from("scenario-user-state.toml"),
                    self.stored_rules.clone(),
                )),
            };
            self.update(RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
                Box::new(result),
            )));
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

    /// Answer the attach `Tail` window with the setup's resident window. A draft attaches
    /// nothing.
    fn seed(&mut self, setup: &ScenarioSetup) {
        self.sync();
        if setup.resident == ResidentSeed::Draft {
            return;
        }
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
            ResidentSeed::Draft => &[],
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

/// The scenario observation of committed terminal damage.
fn damage_observation(
    damage: &bmux_tui::damage::Damage,
    applied: Option<bmux_tui::terminal::AppliedScroll>,
) -> DamageObservation {
    let rects = |regions: &[Rect]| regions.iter().copied().map(cell_rect).collect();
    match damage {
        bmux_tui::damage::Damage::None => DamageObservation::None,
        bmux_tui::damage::Damage::Full => DamageObservation::Full,
        bmux_tui::damage::Damage::Regions(regions) => DamageObservation::Regions(rects(regions)),
        bmux_tui::damage::Damage::Scroll {
            region,
            rows,
            regions,
        } => DamageObservation::Scroll {
            region: cell_rect(*region),
            rows: *rows,
            regions: rects(regions),
            accelerated: applied
                .is_some_and(|applied| applied.region == *region && applied.rows == *rows),
        },
    }
}

/// Terminal output that fails every write and flush ([`V2Driver::abandon_presentation`]).
struct FailingOutput;

impl std::io::Write for FailingOutput {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("scenario presentation failure"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("scenario presentation failure"))
    }
}

/// Interactive surface double: prefers a fixed height and paints one text line per row.
struct ScenarioSurface {
    rows: u16,
    text: String,
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for ScenarioSurface {
    fn id(&self) -> &'static str {
        "scenario-interaction"
    }

    fn title(&self) -> &'static str {
        "scenario interaction"
    }

    fn preferred_height(&mut self, _width: u16) -> u16 {
        self.rows
    }

    fn render(&mut self, area: Rect, frame: &mut bmux_tui::paint::PaintCx<'_, '_>) {
        for row in 0..area.height {
            let line = Rect::new(area.x, area.y.saturating_add(row), area.width, 1);
            frame.write_line(
                bmux_tui::paint::LocalRect::terminal(line),
                &bmux_tui::text::Line::raw(format!("{}{row:02}", self.text)),
            );
        }
    }

    fn render_slice(
        &mut self,
        _logical_height: u16,
        logical_row_offset: u16,
        destination: Rect,
        frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
    ) {
        // Logical rows keep their numbers across slices, so a scenario sees which rows show.
        for row in 0..destination.height {
            let line = Rect::new(
                destination.x,
                destination.y.saturating_add(row),
                destination.width,
                1,
            );
            frame.write_line(
                bmux_tui::paint::LocalRect::terminal(line),
                &bmux_tui::text::Line::raw(format!(
                    "{}{:02}",
                    self.text,
                    logical_row_offset.saturating_add(row)
                )),
            );
        }
    }

    fn handle_event(
        &mut self,
        _event: &Event,
        _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiAction {
        bcode_plugin_sdk::tui::PluginTuiAction::None
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
        let mut model = super::model::scenario_model(setup);
        let origin = Instant::now();
        model
            .transcript_host_mut_for_test()
            .set_scripted_now(origin);
        super::model::install_scenario_clipboard(&mut model);
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
            last_damage: None,
            stored_rules: Vec::new(),
            session_bounds: setup
                .session
                .events()
                .first()
                .zip(setup.session.events().last())
                .map(|(first, last)| (first.sequence, last.sequence)),
            shell: super::shell::ShellEffects::default(),
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
        // The post-copy selection clear runs on the V2 host clock (C35); deliver its timer when
        // scenario time reaches it, as the runtime does.
        if self
            .model
            .selection_clear_at_for_test()
            .is_some_and(|deadline| deadline <= now)
        {
            let timer = self.model.selection_clear_timer_for_test();
            self.update(RuntimeEvent::Timer(timer));
        }
    }

    fn present(&mut self) -> ScenarioFrame {
        let mut output = Vec::new();
        let mut staged = self.terminal.fork(&mut output);
        let report = crate::root_program::bcode_runtime_presenter(&mut staged)
            .present(&mut self.model)
            .expect("V2 scenario presentation must succeed");
        self.terminal.acknowledge(staged);
        // The damage the terminal committed the frame with, after promotion (BMUX 15b).
        let damage = damage_observation(
            self.terminal.committed_damage(),
            self.terminal.applied_scroll(),
        );
        self.last_repaint = Some(RepaintObservation {
            full: damage.is_full() || report.full_repaint,
            changed_cells: report.changed_cells,
        });
        self.last_damage = Some(damage);
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

    /// Run the production root presenter against terminal output that fails, as a broken
    /// terminal would: the frame is prepared and painted, the presentation reports the error,
    /// and neither the terminal nor the model commits it (CK2, C27).
    fn abandon_presentation(&mut self) {
        let mut output = FailingOutput;
        let mut staged = self.terminal.fork(&mut output);
        let failed = crate::root_program::bcode_runtime_presenter(&mut staged)
            .present(&mut self.model)
            .is_err();
        drop(staged);
        assert!(failed, "the presentation must fail when its output fails");
        self.collect_effects();
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

    /// The host shell's copy text, which the V2 engine's selection exports (C34).
    fn selection_text(&self) -> Option<String> {
        self.model.selected_plain_text.clone()
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
                super::model::set_scenario_clipboard_fails(*fails);
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
        super::model::scenario_clipboard_writes()
    }

    fn last_repaint(&self) -> Option<RepaintObservation> {
        self.last_repaint
    }

    fn resolve_submission(
        &mut self,
        outcome: &SubmissionOutcome,
        session: &mut super::session::ScenarioSession,
    ) {
        let result = self.shell.submission_result(session, outcome);
        self.update(RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
            Box::new(result),
        )));
    }

    fn complete_slash_command(&mut self) {
        let result = self.shell.slash_result();
        self.update(RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
            Box::new(result),
        )));
    }

    fn last_damage(&self) -> Option<DamageObservation> {
        self.last_damage.clone()
    }

    /// The host shell's activity animation accepts one frame (its `activity-animation`
    /// timed invalidation), then the update flush hands it to the V2 host (F7, D7).
    fn accept_activity_frame(&mut self) {
        let key = crate::invalidation::InvalidationKey::new("activity-animation");
        let (_invalidation, _damage) = self
            .model
            .chat
            .app
            .handle_invalidations_with_damage(&[key], self.origin + self.elapsed);
        self.sync();
    }

    fn horizontal_wheel(&mut self, right: bool, at: super::driver::CellPoint) {
        let kind = if right {
            MouseEventKind::ScrollRight
        } else {
            MouseEventKind::ScrollLeft
        };
        self.mouse(kind, at);
    }

    fn run_command(&mut self, route: &str) {
        self.model.run_command_route_for_test(route);
        self.sync();
    }

    fn open_interaction(&mut self, interaction_id: &str, rows: u16, text: &str) {
        let keymap = self.model.settings.keymap().clone();
        self.model.loop_state.install_interactive_surface_for_test(
            crate::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                interaction_id,
                Box::new(ScenarioSurface {
                    rows,
                    text: text.to_owned(),
                }),
                &keymap,
            ),
        );
        self.model.request_full_redraw_for_test();
        self.sync();
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

    /// Load the plugins through the production static plugin host and the effective keymap
    /// adapters render and route with (as startup installs them).
    fn load_bundled_plugins(&mut self, plugins: &[super::driver::ScenarioPlugin]) {
        let bundled = plugins
            .iter()
            .map(|plugin| match plugin {
                super::driver::ScenarioPlugin::Filesystem => {
                    bcode_plugin::StaticBundledPlugin::new(
                        include_str!("../../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                        bcode_filesystem_plugin::static_plugin(),
                    )
                }
            })
            .collect::<Vec<_>>();
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("bundled plugins select");
        self.model.chat.app.set_plugin_host(std::sync::Arc::new(
            bcode_plugin::PluginHost::load_static_plugins(&selected).expect("bundled plugins load"),
        ));
        let keymap = self.model.settings.keymap().shared_registry();
        self.model.chat.app.set_effective_keymap(keymap);
        self.model.request_full_redraw_for_test();
        self.sync();
    }
}
