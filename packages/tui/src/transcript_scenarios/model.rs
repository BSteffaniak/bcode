//! The real root model under scenario control, and the scenario clipboard.

use super::driver::{ResidentSeed, ScenarioSetup};
use crate::root_program::BcodeRuntimeModel;

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

/// The real root model for `setup`: the session attached with the setup's resident window
/// installed in the host shell, the production keymap contributors and no daemon (effects are
/// collected by the driver).
pub(super) fn scenario_model(setup: &ScenarioSetup) -> BcodeRuntimeModel {
    let draft = setup.resident == ResidentSeed::Draft;
    let session_id = setup.session.id();
    let events = setup.session.events();
    let resident = match setup.resident {
        ResidentSeed::Latest { events: count } => setup.session.latest(count),
        ResidentSeed::Everything => events,
        ResidentSeed::Draft => &[],
    };
    let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
    let mut app =
        crate::app::BmuxApp::new_with_history((!draft).then_some(session_id), resident, &[]);
    // Same bounded latest-window install session attach performs.
    if !draft {
        app.replace_session_history(resident);
    }
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
        crate::chat_loop::TuiRuntimeSettings::bootstrap(std::path::PathBuf::from("."), &[]);
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
