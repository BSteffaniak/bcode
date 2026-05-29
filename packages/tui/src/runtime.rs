//! TUI startup flow.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;

use super::app::BmuxApp;
use super::keymap::BmuxKeyMap;
use super::terminal_events::TuiInput;
use super::{TuiError, chat_loop, session_flow};

/// Attach to a session and run the active chat loop.
pub async fn run_event_loop<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
) -> Result<(), TuiError> {
    let client = BcodeClient::default_endpoint();
    let config = bcode_config::load_config()?;
    let keymap = BmuxKeyMap::from_config(&config.tui);
    let mouse_scroll_rows = config.tui.mouse.effective_scroll_rows();
    let mut terminal_events = TuiInput::start();
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let mut app = BmuxApp::new_with_history(session_id, &[], &[], false);
    app.apply_tui_config(config.tui);
    let mut chat = session_flow::ActiveChat {
        app,
        session_id: None,
        event_sender,
        event_receiver,
        event_task: None,
        session_open_task: None,
        status_hydration_task: None,
        opening_session_id: None,
    };
    if let Some(session_id) = session_id {
        session_flow::start_switch_session(
            &client,
            &mut chat,
            session_id,
            session_flow::initial_history_event_limit(terminal.area()),
        );
    } else {
        chat.app
            .set_status("New draft session; send a message to save it".to_owned());
    }
    let result = {
        chat_loop::run_with_client(
            terminal,
            &mut terminal_events,
            &client,
            &keymap,
            &mut chat,
            mouse_scroll_rows,
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
