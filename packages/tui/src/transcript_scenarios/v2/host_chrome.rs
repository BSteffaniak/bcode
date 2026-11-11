//! V2-only scenarios for the host chrome and host-shell paths around the V2 transcript: the
//! status row's reading-position label (C2), presenter failure (C27), docked and off-screen
//! interaction surfaces (C37), theme changes reaching chrome (X4), transcript keys under an
//! interactive surface (FO5, EN5), user-bound tool-disclosure keys (FO6) and paced streaming
//! (D13).
//!
//! They run the application with the V2 engine through [`V2Driver`] and observe committed
//! frames, so every check reads what the terminal shows.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bcode_session_models::{SessionEventKind, SessionLiveEvent, SessionLiveEventKind};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui_runtime::RuntimeEvent;

use super::V2Driver;
use crate::history_flow::SessionStreamUpdate;
use crate::root_program::BcodeRuntimeMessage;
use crate::transcript_scenarios::Scenario;
use crate::transcript_scenarios::driver::{
    ResidentSeed, ScenarioSetup, TranscriptScenarioDriver, WheelDirection,
};
use crate::transcript_scenarios::frame::ScenarioFrame;
use crate::transcript_scenarios::interaction::{attach_all, tool_session, transcript_shows};
use crate::transcript_scenarios::session::ScenarioSession;

const WIDTH: u16 = 80;
const HEIGHT: u16 = 20;
/// Longer than any motion and the manual-scroll grace.
const SETTLE: Duration = Duration::from_secs(2);
/// Exchange id of the question [`push_question`] appends.
const EXCHANGE_ID: &str = "exchange-question";
/// Text every row of the scenario interaction surface starts with.
const SURFACE_TEXT: &str = "SURFACEROW";

/// Attach `session` fully resident and present a settled frame.
fn attached(session: ScenarioSession) -> Scenario<V2Driver> {
    let mut scenario = Scenario::<V2Driver>::start(ScenarioSetup {
        session,
        resident: ResidentSeed::Everything,
        width: WIDTH,
        height: HEIGHT,
    });
    settle(&mut scenario);
    scenario
}

/// Present, let motion and grace settle, and present again.
fn settle(scenario: &mut Scenario<V2Driver>) -> ScenarioFrame {
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present()
}

/// Rows below the transcript viewport (latest bar, status row, composer).
fn chrome_below(frame: &ScenarioFrame) -> Vec<&str> {
    frame
        .rows
        .iter()
        .skip(usize::from(frame.transcript.bottom()))
        .map(String::as_str)
        .collect()
}

/// Terminal rows whose text contains `needle`.
fn rows_with(frame: &ScenarioFrame, needle: &str) -> Vec<u16> {
    frame
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains(needle))
        .filter_map(|(y, _)| u16::try_from(y).ok())
        .collect()
}

/// Append a running `ask` call with an unresolved, required question exchange.
fn push_question(session: &mut ScenarioSession) {
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: "call-ask".to_owned(),
        producer_plugin_id: None,
        tool_name: "ask".to_owned(),
        arguments_json: "{}".to_owned(),
        working_directory: None,
    });
    session.push_kind(SessionEventKind::ToolExchangeRequested {
        request: bcode_session_models::ToolExchangeRequest {
            invocation_id: "call-ask".to_owned(),
            exchange_id: EXCHANGE_ID.to_owned(),
            producer_id: "scenario.producer".to_owned(),
            schema: "scenario.question/schema".to_owned(),
            schema_version: 1,
            payload: serde_json::json!({ "question": "QUESTIONTEXT" }),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        },
    });
}

/// Apply `edit` to the TUI configuration through the reload path (both owners).
fn configure_tui(
    scenario: &mut Scenario<V2Driver>,
    edit: impl FnOnce(&mut bcode_config::TuiConfig),
) {
    {
        let model = &mut scenario.driver.model;
        let mut config = model.chat.app.tui_config().clone();
        edit(&mut config);
        model.settings.apply_tui_config(&config);
        model.chat.app.apply_tui_config(config);
        model.request_full_redraw_for_test();
    }
    scenario.driver.sync();
}

// ---------------------------------------------------------------------------------------------
// C2: the status row reports the V2 reading position
// ---------------------------------------------------------------------------------------------

/// C2: the status row reports "N rows below latest" in tail space and "N rows from bottom"
/// while reading, from the engine's presented position, below the transcript; the label follows
/// every tick and disappears when following again.
#[tokio::test]
async fn c2_status_row_reports_the_v2_reading_position() {
    let mut scenario = attached(ScenarioSession::with_turns(20, 3));
    let following = scenario.last_frame().clone();
    scenario.check(
        !following.contains("rows below latest") && !following.contains("rows from bottom"),
        || "a following transcript reports a reading position".to_owned(),
    );
    scenario.wheel(WheelDirection::Down, 1);
    let tail = settle(&mut scenario);
    let below = scenario.driver.host().status_position().rows_below_latest;
    scenario.check(below > 0, || {
        "wheel down at the bottom entered no tail space".to_owned()
    });
    let label = format!("{below} rows below latest");
    scenario.check(
        chrome_below(&tail).iter().any(|row| row.contains(&label)),
        || format!("the status row does not show {label:?}"),
    );
    scenario.check(
        !tail
            .transcript_rows()
            .iter()
            .any(|row| row.contains("rows below latest")),
        || "the label is painted inside the transcript".to_owned(),
    );
    let tick = u64::from(scenario.driver.wheel_rows());
    scenario.wheel(WheelDirection::Up, 4);
    let reading = settle(&mut scenario);
    let from_bottom = scenario.driver.host().status_position().rows_from_bottom;
    scenario.check(from_bottom > 0, || {
        "wheel up did not leave the bottom".to_owned()
    });
    let label = format!("{from_bottom} rows from bottom");
    scenario.check(
        chrome_below(&reading)
            .iter()
            .any(|row| row.contains(&label)),
        || format!("the status row does not show {label:?}"),
    );
    scenario.check(!reading.contains("rows below latest"), || {
        "the tail-space label stayed after leaving tail space".to_owned()
    });
    // The status row repaints with each tick (regional damage includes it).
    scenario.wheel(WheelDirection::Up, 1);
    let further = settle(&mut scenario);
    let label = format!("{} rows from bottom", from_bottom + tick);
    scenario.check(
        chrome_below(&further)
            .iter()
            .any(|row| row.contains(&label)),
        || format!("the status row did not follow the tick to {label:?}"),
    );
    scenario.key("ctrl+end");
    let latest = settle(&mut scenario);
    scenario.check(
        !latest.contains("rows from bottom") && !latest.contains("rows below latest"),
        || "the label stayed after returning to the latest content".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// FO5/EN5: transcript keys under an interactive surface move the V2 transcript
// ---------------------------------------------------------------------------------------------

/// FO5/EN5: with a permission or question surface open, Page Up/Down, ctrl+up/down and
/// ctrl+home/end move the V2 transcript the reader sees.
#[tokio::test]
async fn fo5_transcript_keys_under_an_interactive_surface_move_the_v2_transcript() {
    let mut session = ScenarioSession::with_turns(20, 3);
    push_question(&mut session);
    let mut scenario = attached(session);
    scenario
        .driver
        .open_interaction(EXCHANGE_ID, 2, SURFACE_TEXT);
    let open = settle(&mut scenario);
    scenario.check(open.following_bottom, || {
        "the attach does not follow".to_owned()
    });
    scenario.key("pageUp");
    let paged = settle(&mut scenario);
    scenario.check(
        !paged.following_bottom && paged.shift_from(&open).is_some_and(|shift| shift > 0),
        || "Page Up under the surface did not scroll the transcript up".to_owned(),
    );
    scenario.key("ctrl+down");
    let line = settle(&mut scenario);
    scenario.check(line.shift_from(&paged) == Some(-3), || {
        format!(
            "ctrl+down moved {:?} rows, expected -3",
            line.shift_from(&paged)
        )
    });
    scenario.key("pageDown");
    scenario.key("pageDown");
    scenario.key("ctrl+home");
    let start = settle(&mut scenario);
    let first = scenario
        .session()
        .events()
        .first()
        .map(|event| event.sequence);
    scenario.check(
        start
            .top_marker()
            .is_some_and(|at| Some(at.marker.sequence) == first),
        || "ctrl+home under the surface did not show the session start".to_owned(),
    );
    scenario.key("ctrl+end");
    let latest = settle(&mut scenario);
    scenario.check(latest.following_bottom, || {
        "ctrl+end under the surface did not return to the latest content".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// FO6: user-bound tool-disclosure keys act on the V2 transcript
// ---------------------------------------------------------------------------------------------

/// FO6: `tui.tools.collapseAll`, `tui.tools.expandAll` and `tui.tools.resetDisclosure`, bound
/// by the user (they have no default keys), do what their palette commands do on V2.
#[tokio::test]
async fn fo6_user_bound_tool_keys_act_on_the_v2_transcript() {
    let mut scenario = attach_all::<V2Driver>(tool_session(), WIDTH, 40);
    configure_tui(&mut scenario, |config| {
        let chat = &mut config.keybindings.chat;
        chat.insert("alt+x".to_owned(), "tui.tools.collapseAll".to_owned());
        chat.insert("alt+y".to_owned(), "tui.tools.expandAll".to_owned());
        chat.insert("alt+z".to_owned(), "tui.tools.resetDisclosure".to_owned());
    });
    let shown = scenario.present();
    scenario.check(
        transcript_shows(&shown, "ALPHAOUT1") && transcript_shows(&shown, "GAMMAOUT1"),
        || "the tool outputs are not shown by default".to_owned(),
    );
    scenario.key("alt+x");
    let collapsed = scenario.present();
    scenario.check(
        !transcript_shows(&collapsed, "ALPHAOUT1") && !transcript_shows(&collapsed, "GAMMAOUT1"),
        || "the collapse-all key did not collapse every unit".to_owned(),
    );
    scenario.key("alt+y");
    let expanded = scenario.present();
    scenario.check(
        transcript_shows(&expanded, "ALPHAOUT1") && transcript_shows(&expanded, "GAMMAOUT1"),
        || "the expand-all key did not expand every unit".to_owned(),
    );
    scenario.key("alt+x");
    scenario.present();
    scenario.key("alt+z");
    let reset = scenario.present();
    scenario.check(
        transcript_shows(&reset, "ALPHAOUT1") && transcript_shows(&reset, "GAMMAOUT1"),
        || "the reset key did not restore the configured disclosure".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// X4: theme and profile changes after startup reach dialog and composer chrome
// ---------------------------------------------------------------------------------------------

/// The top-left glyph of the composer frame: the first frame glyph on the composer rows, the
/// bottom-most rows of the terminal.
fn composer_corner(frame: &ScenarioFrame) -> Option<char> {
    let corners = ['┌', '╭', '┏', '╔', '+'];
    frame
        .rows
        .iter()
        .skip(usize::from(frame.transcript.bottom()))
        .find_map(|row| row.chars().find(|glyph| corners.contains(glyph)))
}

/// X4: a profile or theme selected after startup reaches the composer frame and the chrome
/// dialogs and bordered components read.
#[tokio::test]
async fn x4_theme_changes_after_startup_reach_composer_and_dialog_chrome() {
    let mut scenario = attached(ScenarioSession::with_turns(3, 1));
    let before = scenario.last_frame().clone();
    let initial = composer_corner(&before);
    for (profile, corner) in [("retro", '╔'), ("brutalist", '┏'), ("focus", '╭')] {
        assert!(
            scenario.driver.model.chat.app.apply_profile(Some(profile)),
            "{profile} is bundled"
        );
        scenario.driver.model.request_full_redraw_for_test();
        scenario.driver.sync();
        let frame = scenario.present();
        scenario.check(composer_corner(&frame) == Some(corner), || {
            format!(
                "with the {profile} profile the composer frame starts with {:?} (startup {initial:?}), expected {corner:?}",
                composer_corner(&frame)
            )
        });
        let presented = scenario.driver.model.chat.app.presented_theme();
        let chrome = crate::render::semantic_state_theme();
        scenario.check(
            chrome.dialog_chrome == presented.dialog_chrome
                && chrome.unit_chrome == presented.unit_chrome,
            || format!("dialog chrome does not follow the {profile} profile"),
        );
    }
    assert!(scenario.driver.model.chat.app.apply_profile(None));
    assert!(scenario.driver.model.chat.app.apply_theme("high-contrast"));
    scenario.driver.model.request_full_redraw_for_test();
    scenario.driver.sync();
    scenario.present();
    let presented = scenario.driver.model.chat.app.presented_theme();
    let chrome = crate::render::semantic_state_theme();
    scenario.check(
        chrome.dialog_chrome == presented.dialog_chrome
            && chrome.unit_chrome == presented.unit_chrome
            && chrome.border == presented.border
            && chrome.text == presented.text,
        || "chrome does not follow a theme selected after startup".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// C37: docked placement and off-screen focus
// ---------------------------------------------------------------------------------------------

/// C37: with `interactions.placement = pinned` the surface is docked below the V2 transcript,
/// which reserves no rows for it and scrolls on its own under it.
#[tokio::test]
async fn c37_pinned_surface_docks_below_the_v2_transcript() {
    let mut session = ScenarioSession::with_turns(12, 2);
    push_question(&mut session);
    let mut scenario = attached(session);
    configure_tui(&mut scenario, |config| {
        config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
    });
    scenario
        .driver
        .open_interaction(EXCHANGE_ID, 3, SURFACE_TEXT);
    let docked = settle(&mut scenario);
    let rows = rows_with(&docked, SURFACE_TEXT);
    scenario.check(rows.len() == 3, || {
        format!("the docked surface does not show its three rows: {rows:?}")
    });
    scenario.check(
        rows.iter().all(|row| *row >= docked.transcript.bottom()),
        || {
            format!(
                "the docked surface overlaps the transcript {:?}: {rows:?}",
                docked.transcript
            )
        },
    );
    scenario.check(
        scenario.driver.host().inline_interaction_area().is_none(),
        || "the V2 transcript placed a docked surface inline".to_owned(),
    );
    scenario.wheel(WheelDirection::Up, 2);
    let scrolled = settle(&mut scenario);
    scenario.check(
        !scrolled.following_bottom && scrolled.transcript_rows() != docked.transcript_rows(),
        || "the transcript did not scroll under the docked surface".to_owned(),
    );
    scenario.check(rows_with(&scrolled, SURFACE_TEXT) == rows, || {
        "the docked surface moved with the transcript".to_owned()
    });
}

/// Interactive surface double that counts the key events it receives.
struct CountingSurface {
    keys: Arc<Mutex<usize>>,
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for CountingSurface {
    fn id(&self) -> &'static str {
        "counting-interaction"
    }

    fn title(&self) -> &'static str {
        "counting interaction"
    }

    fn preferred_height(&mut self, _width: u16) -> u16 {
        2
    }

    fn render(&mut self, area: Rect, frame: &mut bmux_tui::paint::PaintCx<'_, '_>) {
        for row in 0..area.height {
            let line = Rect::new(area.x, area.y.saturating_add(row), area.width, 1);
            frame.write_line(
                bmux_tui::paint::LocalRect::terminal(line),
                &bmux_tui::text::Line::raw(SURFACE_TEXT),
            );
        }
    }

    fn handle_event(
        &mut self,
        event: &Event,
        _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiAction {
        if matches!(event, Event::Key(_)) {
            *self.keys.lock().expect("key count") += 1;
        }
        bcode_plugin_sdk::tui::PluginTuiAction::None
    }
}

/// Open a [`CountingSurface`] for the question and return its key count.
fn open_counting_surface(scenario: &mut Scenario<V2Driver>) -> Arc<Mutex<usize>> {
    let keys = Arc::new(Mutex::new(0));
    {
        let model = &mut scenario.driver.model;
        let keymap = model.settings.keymap().clone();
        model.loop_state.install_interactive_surface_for_test(
            crate::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                EXCHANGE_ID,
                Box::new(CountingSurface {
                    keys: Arc::clone(&keys),
                }),
                &keymap,
            ),
        );
        model.request_full_redraw_for_test();
    }
    scenario.driver.sync();
    keys
}

/// C37: an inline surface scrolled out of the V2 viewport keeps receiving keys under
/// `offscreen_focus = retain` and stops under `suspend`; Ctrl-I brings it back and keys reach
/// it again.
#[tokio::test]
async fn c37_offscreen_surface_retains_or_suspends_focus_from_the_v2_geometry() {
    for suspend in [false, true] {
        let mut session = ScenarioSession::with_turns(12, 2);
        push_question(&mut session);
        let mut scenario = attached(session);
        configure_tui(&mut scenario, |config| {
            config.interactions.offscreen_focus = if suspend {
                bcode_config::TuiInteractionOffscreenFocus::Suspend
            } else {
                bcode_config::TuiInteractionOffscreenFocus::Retain
            };
        });
        let keys = open_counting_surface(&mut scenario);
        let visible = settle(&mut scenario);
        scenario.check(!rows_with(&visible, SURFACE_TEXT).is_empty(), || {
            "the inline surface is not visible".to_owned()
        });
        let count = || *keys.lock().expect("key count");
        scenario.key("q");
        scenario.check(count() == 1, || {
            format!("a visible surface did not receive the key ({})", count())
        });
        scenario.wheel(WheelDirection::Up, 20);
        let away = settle(&mut scenario);
        scenario.check(rows_with(&away, SURFACE_TEXT).is_empty(), || {
            "the surface stayed visible after its unit scrolled away".to_owned()
        });
        scenario.key("q");
        let expected = if suspend { 1 } else { 2 };
        scenario.check(count() == expected, || {
            format!(
                "off-screen with suspend={suspend} the surface saw {} keys, expected {expected}",
                count()
            )
        });
        scenario.key("ctrl+i");
        let restored = settle(&mut scenario);
        scenario.check(!rows_with(&restored, SURFACE_TEXT).is_empty(), || {
            "ctrl+i did not restore the surface into view".to_owned()
        });
        scenario.key("q");
        scenario.check(count() == expected + 1, || {
            "a restored surface did not receive the key".to_owned()
        });
    }
}

// ---------------------------------------------------------------------------------------------
// D13: paced streaming follows the host shell's streaming policy
// ---------------------------------------------------------------------------------------------

/// Long enough that the default 300 graphemes per second cannot show it at once.
const PACED_REPLY: &str = "PACEDREPLY paced streaming text that keeps going past the first step";

fn deliver_delta(scenario: &mut Scenario<V2Driver>, text: &str) {
    let session_id = scenario.session().id();
    scenario
        .driver
        .update(RuntimeEvent::Message(BcodeRuntimeMessage::SessionStream(
            Box::new(SessionStreamUpdate::Event(Box::new(
                bcode_ipc::Event::SessionLive(SessionLiveEvent {
                    session_id,
                    kind: SessionLiveEventKind::AssistantTextDelta {
                        turn_id: "turn-live".to_owned(),
                        segment_id: "segment-live".to_owned(),
                        segment_order: 0,
                        text: text.to_owned(),
                    },
                }),
            ))),
        )));
}

/// D13: with the host shell's paced policy (`[tui] streaming`), V2 shows live text
/// progressively on its own timer until all of it is visible; the streaming configurator's
/// reset to immediate presentation shows pending text at once.
#[tokio::test]
async fn d13_v2_paces_live_text_with_the_shell_streaming_policy() {
    let mut scenario = attached(ScenarioSession::with_turns(2, 1));
    let _changed = scenario
        .driver
        .model
        .chat
        .app
        .apply_streaming_presentation_policy(
            bcode_session_view_models::StreamingPresentationPolicy::default(),
        );
    scenario.driver.sync();
    deliver_delta(&mut scenario, PACED_REPLY);
    let first = scenario.present();
    scenario.check(!first.contains(PACED_REPLY), || {
        "paced live text was shown at once".to_owned()
    });
    scenario.check(scenario.driver.host().next_deadline().is_some(), || {
        "pending paced text schedules no V2 timer".to_owned()
    });
    scenario.advance(SETTLE);
    let shown = scenario.present();
    scenario.check(shown.contains(PACED_REPLY), || {
        "paced live text never became fully visible".to_owned()
    });
    drop(scenario);

    // The configurator's reset applies immediate presentation through the host shell.
    let mut scenario = attached(ScenarioSession::with_turns(2, 1));
    let _changed = scenario
        .driver
        .model
        .chat
        .app
        .apply_streaming_presentation_policy(
            bcode_session_view_models::StreamingPresentationPolicy::default(),
        );
    scenario.driver.sync();
    deliver_delta(&mut scenario, PACED_REPLY);
    let paced = scenario.present();
    scenario.check(!paced.contains(PACED_REPLY), || {
        "paced live text was shown at once".to_owned()
    });
    crate::chat_loop::ChatLoopState::request_clear_streaming_presentation_override(
        &mut scenario.driver.model.chat,
        bcode_session_view_models::StreamingPresentationPolicy::immediate(),
    );
    scenario.driver.sync();
    let flushed = scenario.present();
    scenario.check(flushed.contains(PACED_REPLY), || {
        "switching to immediate presentation did not show the pending text".to_owned()
    });
}
