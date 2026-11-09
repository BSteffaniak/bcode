//! Engine selection at startup through the real root model (X7, X8, phase 4 exit criteria).
//!
//! These checks build the application's root model the way the scenario drivers do and observe
//! which transcript engine owns the presented transcript. The resolution rules themselves are
//! unit-tested in [`crate::engine_selection`]; here the resolved engine is followed into the
//! running application.

use std::time::Duration;

use bcode_config::TuiEngine;
use bmux_tui_runtime::{Program as _, RuntimeEvent, TimerId};

use super::Scenario;
use super::driver::{ResidentSeed, ScenarioSetup, TranscriptScenarioDriver};
use super::session::ScenarioSession;
use super::v1::{V1Driver, scenario_model};
use super::v2::V2Driver;
use crate::engine_selection::{ENGINE_ENV_VAR, EngineSelectionError, resolve_tui_engine};
use crate::root_program::BcodeRuntimeModel;

const WIDTH: u16 = 80;

/// `(--tui-engine, BCODE_TUI_ENGINE, [tui] engine, expected engine)`.
type SelectionCase = (
    Option<TuiEngine>,
    Option<&'static str>,
    Option<TuiEngine>,
    TuiEngine,
);
const HEIGHT: u16 = 20;

fn setup(session: ScenarioSession) -> ScenarioSetup {
    ScenarioSetup {
        session,
        resident: ResidentSeed::Latest { events: 12 },
        width: WIDTH,
        height: HEIGHT,
    }
}

/// Which engine owns the model's transcript.
fn selected_engine(model: &BcodeRuntimeModel) -> TuiEngine {
    if model.transcript_v2_for_test().is_some() {
        TuiEngine::V2
    } else {
        TuiEngine::V1
    }
}

/// Attach `session` with driver `D` and return the first committed frame's bottom marker and
/// the resident sequence range.
fn attach_with<D: TranscriptScenarioDriver>(
    session: ScenarioSession,
) -> (Option<super::session::Marker>, Option<(u64, u64)>) {
    let mut scenario = Scenario::<D>::attach_latest(session, 12, WIDTH, HEIGHT);
    scenario.advance(Duration::from_secs(2));
    let frame = scenario.present();
    scenario.check(frame.following_bottom, || {
        "attach must follow the latest bottom".to_owned()
    });
    (
        frame.bottom_marker().map(|at| at.marker),
        scenario.resident(),
    )
}

/// Both engines attach to the same scripted session and present its latest content (phase 4:
/// both engines selectable side by side).
#[tokio::test]
async fn both_engines_attach_to_the_same_scripted_session() {
    let session = ScenarioSession::with_turns(10, 3);
    let last = session
        .last_sequence()
        .expect("scripted session has events");
    let latest = session.last_marker(last).expect("last event is an item");

    let (v1_bottom, v1_resident) = attach_with::<V1Driver>(session.clone());
    let (v2_bottom, v2_resident) = attach_with::<V2Driver>(session);

    assert_eq!(v1_bottom, Some(latest), "V1 must present the latest marker");
    assert_eq!(v2_bottom, Some(latest), "V2 must present the latest marker");
    assert_eq!(
        v1_resident.map(|(_, last)| last),
        Some(last),
        "V1 must hold the session tail"
    );
    assert_eq!(
        v2_resident.map(|(_, last)| last),
        Some(last),
        "V2 must hold the session tail"
    );
}

/// X7: the engine the root model runs follows EN1 precedence (CLI flag > environment >
/// configuration > V1), and an invalid environment value is a startup error naming the
/// variable and the valid values even when the flag wins (EN2).
#[tokio::test]
async fn startup_engine_follows_precedence_and_rejects_invalid_environment() {
    let cases: [SelectionCase; 8] = [
        (None, None, None, TuiEngine::V1),
        (None, None, Some(TuiEngine::V2), TuiEngine::V2),
        (None, Some("v2"), Some(TuiEngine::V1), TuiEngine::V2),
        (None, Some("V1"), Some(TuiEngine::V2), TuiEngine::V1),
        (None, Some("  "), Some(TuiEngine::V2), TuiEngine::V2),
        (
            Some(TuiEngine::V1),
            Some("v2"),
            Some(TuiEngine::V2),
            TuiEngine::V1,
        ),
        (
            Some(TuiEngine::V2),
            None,
            Some(TuiEngine::V1),
            TuiEngine::V2,
        ),
        (Some(TuiEngine::V2), Some("v1"), None, TuiEngine::V2),
    ];
    let scenario_setup = setup(ScenarioSession::with_turns(4, 2));
    for (cli, env, config, expected) in cases {
        let engine = resolve_tui_engine(cli, env, config)
            .unwrap_or_else(|error| panic!("valid selection {cli:?}/{env:?}/{config:?}: {error}"));
        let model = scenario_model(&scenario_setup, engine);
        assert_eq!(
            selected_engine(&model),
            expected,
            "cli={cli:?} env={env:?} config={config:?}"
        );
    }

    for cli in [None, Some(TuiEngine::V1), Some(TuiEngine::V2)] {
        let error = resolve_tui_engine(cli, Some("v3"), Some(TuiEngine::V2))
            .expect_err("an invalid environment value is a startup error");
        assert_eq!(
            error,
            EngineSelectionError::InvalidEnvironment {
                value: "v3".to_owned()
            }
        );
        let message = error.to_string();
        assert!(
            message.contains(ENGINE_ENV_VAR) && message.contains("v1, v2"),
            "error must name the variable and the valid values: {message}"
        );
    }
}

/// X8: the engine is fixed at startup. A configuration reload that names the other engine
/// applies every other setting but never switches the transcript engine (EN3).
#[tokio::test]
async fn configuration_reload_never_switches_the_engine() {
    let scenario_setup = setup(ScenarioSession::with_turns(4, 2));
    for (started, configured) in [
        (TuiEngine::V1, TuiEngine::V2),
        (TuiEngine::V2, TuiEngine::V1),
    ] {
        let mut model = scenario_model(&scenario_setup, started);
        let mut config = model.chat.app.tui_config().clone();
        config.engine = Some(configured);
        config.reduce_motion = !config.reduce_motion;
        model.settings.apply_tui_config(&config);
        model.chat.app.apply_tui_config(config);
        let _update = model
            .update(RuntimeEvent::Timer(TimerId::new("bcode.transcript_v2")))
            .expect("update after a configuration reload succeeds");
        assert_eq!(
            model.chat.app.tui_config().engine,
            Some(configured),
            "the reloaded configuration is applied"
        );
        assert_eq!(model.settings.transcript_engine(), started);
        assert_eq!(
            selected_engine(&model),
            started,
            "started on {started:?}; a reload naming {configured:?} must not switch engines"
        );
    }
}
