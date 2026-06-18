//! TUI startup flow.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;

use super::app::BmuxApp;
use super::startup_action::StartupTuiAction;
use super::terminal_events::TuiInput;
use super::{TuiError, chat_loop, session_flow};

/// Attach to a session and run the active chat loop.
pub async fn run_event_loop<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
) -> Result<(), TuiError> {
    run_event_loop_with_startup(terminal, session_id, StartupTuiAction::None).await
}

/// Attach to a session, run an optional startup action, and run the active chat loop.
pub async fn run_event_loop_with_startup<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
    startup_action: StartupTuiAction,
) -> Result<(), TuiError> {
    let client = BcodeClient::default_endpoint();
    let mut terminal_events = TuiInput::start();
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let (async_event_sender, async_event_receiver) = mpsc::unbounded_channel();
    let mut app = BmuxApp::new_with_history(session_id, &[], &[], false);
    let agents = session_flow::AgentCatalog::default();
    agents.refresh_app_agent_metadata(&mut app);
    let launch_working_directory = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut settings = chat_loop::TuiRuntimeSettings::bootstrap(launch_working_directory.clone());
    let mut chat = session_flow::ActiveChat {
        app,
        agents,
        session_id: None,
        event_sender,
        event_receiver,
        event_task: None,
        async_event_sender,
        async_event_receiver,
        session_open_task: None,
        status_hydration_task: None,
        opening_session_id: None,
    };
    session_flow::start_config_hydration(&chat);
    session_flow::start_agent_catalog_hydration(&client, &chat);
    if let Some(session_id) = session_id {
        let initial_window_request = session_flow::initial_transcript_window_request(
            super::render::transcript_area_for_frame(&chat.app, terminal.area()),
        );
        session_flow::start_switch_session(&client, &mut chat, session_id, initial_window_request);
    } else {
        chat.app.set_status("New draft".to_owned());
    }
    let result = {
        chat_loop::run_with_client(
            terminal,
            &mut terminal_events,
            &client,
            &mut settings,
            &mut chat,
            startup_action,
        )
        .await
    };
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    if let Some(open_task) = chat.session_open_task.take() {
        open_task.abort();
    }
    if let Some(hydration_task) = chat.status_hydration_task.take() {
        hydration_task.abort();
    }
    result
}
