//! Scenario driver for the V1 transcript engine.
//!
//! Drives the real [`BcodeRuntimeModel`] through `Program::update` (terminal input, session
//! stream messages, effect completions) and presents through the production root presenter, so
//! frame preparation, paint and commit run exactly as in the application. No BMUX runtime loop
//! runs: effects queued by an update are collected afterwards, history loads become pending
//! scenario requests, and every other effect is dropped (there is no daemon).

use std::time::{Duration, Instant};

use bcode_session_models::{SessionEvent, SessionHistoryPage};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui_runtime::{Presenter as _, Program as _, RuntimeEvent};

use super::driver::{
    HistoryRequest, HistoryResponse, PageLimits, PendingHistoryRequest, RepaintObservation,
    RequestId, ResidentSeed, ScenarioEngine, ScenarioInput, ScenarioIntent, ScenarioSetting,
    ScenarioSetup, SubmissionOutcome, TranscriptScenarioDriver, WheelDirection,
};
use super::frame::{CellRect, FocusObservation, ScenarioFrame};
use crate::effects::{TuiEffect, TuiEffectResult};
use crate::history_flow::SessionStreamUpdate;
use crate::root_program::{BcodeRuntimeMessage, BcodeRuntimeModel};

/// Page sizing V1 asks the daemon for (`effects.rs` transcript page constants).
const V1_PAGE_LIMITS: PageLimits = PageLimits {
    target_items: 4,
    target_rows: 24,
    max_items: 16,
    max_events: 512,
};

/// Events V1 requests on each side of a timeline jump target (`history_flow.rs`).
const V1_TIMELINE_HALF_WINDOW: usize = 512;

/// How far ahead a held V1 deadline is pinned in wall time: far beyond any test's duration, so
/// only scenario time ends it.
const HELD_DEADLINE: Duration = Duration::from_secs(3600);

/// A V1 deadline that V1 compares with wall time (`Instant::now()`), driven by scenario time.
///
/// When V1 arms the deadline, the driver records when it is due in scenario time and pins V1's
/// copy far into the future, so real time spent running the test can never end it early.
/// `advance_clock` ends it once scenario time reaches the recorded due time.
#[derive(Debug, Default)]
struct ScenarioDeadline {
    /// The value the driver last wrote to V1; any other value means V1 re-armed it.
    pinned: Option<Instant>,
    /// Scenario time at which the deadline is due.
    due: Option<Duration>,
}

impl ScenarioDeadline {
    /// Observe V1's current deadline after an update. Returns the far-future deadline V1 must
    /// hold instead when V1 has just armed it.
    fn observe(&mut self, current: Option<Instant>, elapsed: Duration) -> Option<Instant> {
        if current == self.pinned {
            return None;
        }
        let Some(deadline) = current else {
            *self = Self::default();
            return None;
        };
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        // V1 armed it as `now + delay` moments ago; round the remainder up to whole milliseconds.
        let millis = u64::try_from(remaining.as_micros().div_ceil(1000)).unwrap_or(u64::MAX);
        self.due = Some(elapsed.saturating_add(Duration::from_millis(millis)));
        self.pinned = now.checked_add(HELD_DEADLINE);
        self.pinned
    }

    /// Whether scenario time has reached the deadline; resets it when it has.
    fn take_due(&mut self, elapsed: Duration) -> bool {
        let due = self.due.is_some_and(|due| due <= elapsed);
        if due {
            *self = Self::default();
        }
        due
    }
}

thread_local! {
    /// Clipboard writes recorded by [`scenario_clipboard`] on this test thread.
    static CLIPBOARD_WRITES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Whether [`scenario_clipboard`] fails instead of recording.
    static CLIPBOARD_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Scenario clipboard: records writes on the current test thread, or fails when scripted to.
fn scenario_clipboard(text: &str) -> Result<(), String> {
    if CLIPBOARD_FAILS.with(std::cell::Cell::get) {
        return Err("scenario clipboard failure".to_owned());
    }
    CLIPBOARD_WRITES.with(|writes| writes.borrow_mut().push(text.to_owned()));
    Ok(())
}

/// Reset the scenario clipboard of this test thread and make `model` copy into it.
pub(super) fn install_scenario_clipboard(model: &mut BcodeRuntimeModel) {
    CLIPBOARD_WRITES.with(|writes| writes.borrow_mut().clear());
    CLIPBOARD_FAILS.with(|fails| fails.set(false));
    model.set_transcript_clipboard_for_test(scenario_clipboard);
}

/// Script the scenario clipboard of this test thread to fail (`true`) or succeed.
pub(super) fn set_scenario_clipboard_fails(fails: bool) {
    CLIPBOARD_FAILS.with(|cell| cell.set(fails));
}

/// Clipboard writes recorded on this test thread, oldest first.
pub(super) fn scenario_clipboard_writes() -> Vec<String> {
    CLIPBOARD_WRITES.with(|writes| writes.borrow().clone())
}

/// The real root model for `setup` with `engine` selected: the session attached with the
/// setup's resident window installed in the host shell, the production keymap contributors and
/// no daemon (effects are collected by the driver).
pub(super) fn scenario_model(
    setup: &ScenarioSetup,
    engine: bcode_config::TuiEngine,
) -> BcodeRuntimeModel {
    let draft = setup.resident == ResidentSeed::Draft;
    let session_id = setup.session.id();
    let events = setup.session.events();
    let resident = match setup.resident {
        ResidentSeed::Latest { events: count } => setup.session.latest(count),
        ResidentSeed::Everything => events,
        ResidentSeed::Draft => &[],
    };
    let has_older = resident.len() < events.len();
    let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
    let mut app = crate::app::BmuxApp::new_with_history(
        (!draft).then_some(session_id),
        resident,
        &[],
        has_older,
    );
    // Same bounded latest-window install session attach performs.
    if !draft {
        app.replace_latest_transcript_window(resident, has_older);
    }
    app.freeze_transcript_scroll_animation_for_test();
    let chat = crate::session_flow::ActiveChat {
        app,
        agents: crate::session_flow::AgentCatalog::default(),
        attachment: if draft {
            crate::session_flow::ChatSessionAttachment::Draft
        } else {
            crate::session_flow::ChatSessionAttachment::Attached { session_id }
        },
        event_sender,
        event_receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: crate::effects::TuiEffectQueue::default(),
    };
    let mut settings =
        crate::chat_loop::TuiRuntimeSettings::bootstrap(std::path::PathBuf::from("."), &[])
            .with_transcript_engine(engine);
    let mut actions = bcode_question_plugin::question_tui_registry()
        .keymap_actions()
        .to_vec();
    actions.extend_from_slice(bcode_shell_plugin::shell_tui_registry().keymap_actions());
    settings.integrate_keymap_contributors(Ok(actions));
    let client = bcode_client::BcodeClient::default_endpoint();
    let passive_client = client
        .clone()
        .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
    let loop_state = crate::chat_loop::ChatLoopState::new(&client, &passive_client, false);
    BcodeRuntimeModel::new(chat, settings, loop_state)
}

/// V1 engine under scenario control.
pub struct V1Driver {
    model: BcodeRuntimeModel,
    terminal: bmux_tui::terminal::Terminal<std::io::Sink>,
    pending: Vec<(RequestId, TuiEffect)>,
    next_request: u64,
    /// V1's manual-scroll grace window, held open by scenario time.
    grace: ScenarioDeadline,
    /// V1's post-copy selection clear, held open by scenario time.
    selection_clear: ScenarioDeadline,
    /// Wall time up to which latest-bar activity timestamps already exclude real time.
    wall_mark: Instant,
    /// Wall-clock origin of scenario time for V1 paths that take an `Instant`.
    clock_origin: Instant,
    /// Scenario time advanced so far.
    elapsed: Duration,
    /// Repaint scope reported by the last presentation.
    last_repaint: Option<RepaintObservation>,
    /// Submissions and slash commands awaiting the scenario's answer.
    shell: super::shell::ShellEffects,
}

impl V1Driver {
    fn update(&mut self, event: RuntimeEvent<BcodeRuntimeMessage>) {
        self.exclude_wall_time();
        let _update = self
            .model
            .update(event)
            .expect("V1 scenario update must succeed");
        self.collect_effects();
        self.hold_deadlines();
    }

    /// Pin deadlines V1 armed during the last update so only scenario time can end them.
    fn hold_deadlines(&mut self) {
        let grace = self
            .model
            .chat
            .app
            .manual_transcript_scroll_until_for_test();
        if let Some(held) = self.grace.observe(grace, self.elapsed) {
            self.model
                .chat
                .app
                .set_manual_transcript_scroll_until_for_test(Some(held));
        }
        let clear = self.model.selection_clear_at_for_test();
        if let Some(held) = self.selection_clear.observe(clear, self.elapsed) {
            self.model.set_selection_clear_at_for_test(Some(held));
        }
    }

    /// Move V1's latest-bar activity timestamps forward by the wall time spent since the last
    /// call, so glow phases advance only with scenario time.
    fn exclude_wall_time(&mut self) {
        let now = Instant::now();
        let wall = now.saturating_duration_since(self.wall_mark);
        self.wall_mark = now;
        self.model
            .chat
            .app
            .shift_latest_activity_for_test(wall, Duration::ZERO);
    }

    fn terminal_event(&mut self, event: Event) {
        self.update(RuntimeEvent::Terminal(event));
    }

    /// Keep history loads as pending requests and the newest submission and slash command for
    /// the scenario to answer; drop work that needs a daemon.
    fn collect_effects(&mut self) {
        let (scheduled, _notes) = self.model.chat.pending_effects.drain_runtime();
        for (_schedule, effect) in scheduled {
            if matches!(
                effect,
                TuiEffect::LoadOlderHistory { .. }
                    | TuiEffect::LoadNewerHistory { .. }
                    | TuiEffect::LoadTimelineJump { .. }
            ) {
                self.next_request += 1;
                self.pending.push((RequestId(self.next_request), effect));
            } else {
                self.shell.observe(effect);
            }
        }
    }

    fn complete_effect(&mut self, result: TuiEffectResult) {
        self.update(RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
            Box::new(result),
        )));
    }

    /// Install synchronously what the production loop installs from background projection
    /// completions between frames, so every presented frame shows accepted Markdown.
    fn project_markdown(&mut self) {
        let width = self.terminal.area().width;
        let app = &mut self.model.chat.app;
        let items = app
            .transcript()
            .iter()
            .filter(|item| item.text_format() == bcode_session_view_models::TextFormat::Markdown)
            .cloned()
            .collect::<Vec<_>>();
        for item in &items {
            let options = crate::render::markdown_render_options(app, item, width);
            app.transcript_markdown_cache().project(item, options);
        }
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

/// Successful page payload in the shape V1's history effects deliver.
fn history_page(
    session_id: bcode_session_models::SessionId,
    events: Vec<SessionEvent>,
    has_more: bool,
) -> (
    SessionHistoryPage,
    Vec<bcode_session_models::SubmissionRecord>,
) {
    (
        SessionHistoryPage {
            session_id,
            events,
            compatibility_issues: Vec::new(),
            next_cursor: None,
            has_more,
        },
        Vec::new(),
    )
}

impl TranscriptScenarioDriver for V1Driver {
    const ENGINE: ScenarioEngine = ScenarioEngine::V1;

    fn start(setup: &ScenarioSetup) -> Self {
        let model = scenario_model(setup, bcode_config::TuiEngine::V1);
        let terminal = bmux_tui::terminal::Terminal::new(
            std::io::sink(),
            Rect::new(0, 0, setup.width, setup.height),
        );
        let mut driver = Self {
            model,
            terminal,
            pending: Vec::new(),
            next_request: 0,
            grace: ScenarioDeadline::default(),
            selection_clear: ScenarioDeadline::default(),
            wall_mark: Instant::now(),
            clock_origin: Instant::now(),
            elapsed: Duration::ZERO,
            last_repaint: None,
            shell: super::shell::ShellEffects::default(),
        };
        install_scenario_clipboard(&mut driver.model);
        driver.collect_effects();
        driver
    }

    fn wheel_rows(&self) -> u16 {
        u16::try_from(self.model.settings.mouse_scroll_rows()).unwrap_or(u16::MAX)
    }

    fn deliver_live(&mut self, event: SessionEvent) {
        self.update(RuntimeEvent::Message(BcodeRuntimeMessage::SessionStream(
            Box::new(SessionStreamUpdate::Event(Box::new(
                bcode_ipc::Event::Session(event),
            ))),
        )));
    }

    fn pending_requests(&self) -> Vec<PendingHistoryRequest> {
        self.pending
            .iter()
            .filter_map(|(id, effect)| {
                let request = match effect {
                    TuiEffect::LoadOlderHistory { cursor, .. } => HistoryRequest::Older {
                        before_sequence: cursor.sequence.saturating_add(1),
                        limits: V1_PAGE_LIMITS,
                    },
                    TuiEffect::LoadNewerHistory { cursor, .. } => HistoryRequest::Newer {
                        after_sequence: cursor.sequence.saturating_sub(1),
                        limits: V1_PAGE_LIMITS,
                    },
                    TuiEffect::LoadTimelineJump { sequence, .. } => HistoryRequest::Around {
                        sequence: *sequence,
                        before_events: V1_TIMELINE_HALF_WINDOW,
                        after_events: V1_TIMELINE_HALF_WINDOW,
                    },
                    _ => return None,
                };
                Some(PendingHistoryRequest { id: *id, request })
            })
            .collect()
    }

    fn complete_request(&mut self, id: RequestId, response: HistoryResponse) {
        let position = self
            .pending
            .iter()
            .position(|(pending, _)| *pending == id)
            .expect("completed request must be pending");
        let (_, effect) = self.pending.remove(position);
        let result = match effect {
            TuiEffect::LoadOlderHistory {
                session_id,
                cursor,
                generation,
                ..
            } => TuiEffectResult::OlderHistoryLoaded {
                session_id,
                cursor,
                generation,
                result: Ok(history_page(
                    session_id,
                    response.events,
                    response.has_older,
                )),
            },
            TuiEffect::LoadNewerHistory {
                session_id,
                cursor,
                generation,
                ..
            } => TuiEffectResult::NewerHistoryLoaded {
                session_id,
                cursor,
                generation,
                result: Ok(history_page(
                    session_id,
                    response.events,
                    response.has_newer,
                )),
            },
            TuiEffect::LoadTimelineJump {
                session_id,
                generation,
                sequence,
            } => TuiEffectResult::TimelineJumpLoaded {
                session_id,
                generation,
                sequence,
                result: Ok((
                    response.events,
                    response.has_older,
                    response.has_newer,
                    Vec::new(),
                )),
            },
            _ => unreachable!("only history effects are retained"),
        };
        self.update(RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
            Box::new(result),
        )));
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
                self.terminal_event(Event::Mouse(MouseEvent::new(kind, Point::new(at.x, at.y))));
            }
            ScenarioInput::Click { at } => {
                let point = Point::new(at.x, at.y);
                self.terminal_event(Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    point,
                )));
                self.terminal_event(Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    point,
                )));
            }
            ScenarioInput::Press { at } => self.terminal_event(Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(at.x, at.y),
            ))),
            ScenarioInput::DragTo { at } => self.terminal_event(Event::Mouse(MouseEvent::new(
                MouseEventKind::Drag(MouseButton::Left),
                Point::new(at.x, at.y),
            ))),
            ScenarioInput::Release { at } => self.terminal_event(Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(at.x, at.y),
            ))),
            ScenarioInput::Resize { width, height } => {
                self.terminal.resize(Rect::new(0, 0, width, height));
                self.terminal_event(Event::Resize(Size { width, height }));
            }
        }
    }

    fn navigate(&mut self, intent: ScenarioIntent) {
        match intent {
            ScenarioIntent::JumpToSequence(sequence) => {
                self.model.timeline_jump_for_test(sequence);
                self.collect_effects();
            }
            // V1's jump-to-latest binding (also named by the latest bar).
            ScenarioIntent::JumpToLatest => self.input(ScenarioInput::Key("ctrl+end".to_owned())),
        }
    }

    fn advance_clock(&mut self, by: Duration) {
        self.exclude_wall_time();
        self.elapsed = self.elapsed.saturating_add(by);
        let now = self.clock_origin + self.elapsed;
        self.model
            .chat
            .app
            .advance_transcript_scroll_animation_for_test(by);
        let _advanced = self.model.chat.app.advance_streaming_presentation(now);
        // V1's grace window, latest-bar glow and post-copy clear read wall time; scenario time
        // drives them instead (see `ScenarioDeadline` and `exclude_wall_time`).
        if self.grace.take_due(self.elapsed) {
            self.model
                .chat
                .app
                .set_manual_transcript_scroll_until_for_test(None);
        }
        self.model
            .chat
            .app
            .shift_latest_activity_for_test(Duration::ZERO, by);
        if self.selection_clear.take_due(self.elapsed) {
            self.model
                .set_selection_clear_at_for_test(Some(Instant::now()));
            let timer = self.model.selection_clear_timer_for_test();
            self.update(RuntimeEvent::Timer(timer));
        }
        // In production, time reaches the program as the timed-invalidation timer (scroll motion,
        // glow and streaming frames), whose damage selects the temporal presentation path.
        let timer = self.model.invalidations_timer_for_test();
        self.update(RuntimeEvent::Timer(timer));
        // State the driver advanced directly (the frozen scroll-motion clock, streaming) may have
        // no due invalidation in wall time; repaint at paint level, never structurally.
        if !self.model.presentation_redraw_requested_for_test() {
            self.model.request_temporal_redraw_for_test();
        }
    }

    fn present(&mut self) -> ScenarioFrame {
        self.exclude_wall_time();
        self.project_markdown();
        let mut output = Vec::new();
        let mut staged = self.terminal.fork(&mut output);
        let full_damage = self.model.presentation_damage_is_full_for_test()
            || self.terminal.retained_buffer().is_none();
        let report = crate::root_program::bcode_runtime_presenter(&mut staged)
            .present(&mut self.model)
            .expect("V1 scenario presentation must succeed");
        self.terminal.acknowledge(staged);
        self.hold_deadlines();
        self.last_repaint = Some(RepaintObservation {
            full: full_damage || report.full_repaint,
            changed_cells: report.changed_cells,
        });
        let buffer = self
            .terminal
            .retained_buffer()
            .expect("committed frame retains its buffer");
        let rows = (0..buffer.area().height)
            .map(|row| buffer.row_symbols(row).unwrap_or_default())
            .collect();
        let layout = self.model.committed_layout_for_test();
        let transcript = layout.map_or_else(CellRect::default, |layout| cell_rect(layout.body()));
        let app = &self.model.chat.app;
        let focus = if app.transcript_focus_active() {
            FocusObservation::Transcript { units: true }
        } else {
            FocusObservation::Composer
        };
        let scroll_mode = app.frame_observation().scroll_mode;
        ScenarioFrame {
            index: 0,
            rows,
            transcript,
            latest_bar: layout
                .and_then(crate::render::FrameLayout::latest_bar)
                .map(cell_rect),
            following_bottom: scroll_mode == "bottom_follow",
            motion_running: scroll_mode == "transition_to_entry",
            focus,
            focused_target: app
                .focused_target_for_test()
                .map(|target| format!("{target:?}")),
            scrollbar: None,
        }
    }

    fn resident_sequences(&self) -> Option<(u64, u64)> {
        let sequences = self
            .model
            .chat
            .app
            .transcript()
            .iter()
            .filter_map(crate::transcript::TranscriptItem::event_sequence)
            .collect::<Vec<_>>();
        Some((*sequences.iter().min()?, *sequences.iter().max()?))
    }

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
                self.model.request_full_redraw_for_test();
                return;
            }
            ScenarioSetting::ClipboardFails(fails) => {
                set_scenario_clipboard_fails(*fails);
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
            }
            ScenarioSetting::ReducedMotion(reduce) => config.reduce_motion = *reduce,
            ScenarioSetting::TranscriptScrollbar(_) => {
                panic!("V1 driver: the transcript scrollbar setting is V2-only (X6)")
            }
        }
        // The configuration reload path applies TUI settings to both owners.
        self.model.settings.apply_tui_config(&config);
        self.model.chat.app.apply_tui_config(config);
        self.model.request_full_redraw_for_test();
    }

    fn clipboard_writes(&self) -> Vec<String> {
        scenario_clipboard_writes()
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
        self.complete_effect(result);
    }

    fn complete_slash_command(&mut self) {
        let result = self.shell.slash_result();
        self.complete_effect(result);
    }

    fn focused_extent(&self) -> Option<std::ops::Range<i64>> {
        let (extent, top) = {
            let app = &self.model.chat.app;
            let target = app.focused_target_for_test()?;
            let extent =
                crate::transcript_focus::target_extent(&app.disclosure_anchors(), &target, None)?;
            (
                extent,
                app.transcript_top_row(app.transcript_viewport_height_for_test()),
            )
        };
        let relative = |row: usize| {
            i64::try_from(row).unwrap_or(i64::MAX) - i64::try_from(top).unwrap_or(i64::MAX)
        };
        Some(relative(extent.start)..relative(extent.end))
    }
}
