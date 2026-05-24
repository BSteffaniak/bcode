//! TUI startup flow.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;

use super::app::BmuxApp;
use super::keymap::BmuxKeyMap;
use super::{INITIAL_HISTORY_EVENT_LIMIT, TuiError, chat_loop, history_flow, session_flow};

/// Attach to a session and run the active chat loop.
pub async fn run_event_loop<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
) -> Result<(), TuiError> {
    let client = BcodeClient::default_endpoint();
    let config = bcode_config::load_config()?;
    let keymap = BmuxKeyMap::from_config(&config.tui);
    let mouse_scroll_rows = config.tui.mouse.effective_scroll_rows();
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => session_flow::pick_session(terminal, &client, &keymap).await?,
    };
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let (attached, event_task) =
        history_flow::attach_session_event_stream(&client, session_id, event_sender.clone())
            .await?;
    let mut app = BmuxApp::new_with_history(
        Some(session_id),
        &attached.history,
        &attached.input_history,
        attached.history.len() >= INITIAL_HISTORY_EVENT_LIMIT,
    );
    app.apply_session_summary(&attached.session);
    let mut chat = session_flow::ActiveChat {
        app,
        session_id,
        event_sender,
        event_receiver,
        event_task,
    };
    session_flow::hydrate_status(&client, &mut chat.app).await;
    let result =
        chat_loop::run_with_client(terminal, &client, &keymap, &mut chat, mouse_scroll_rows).await;
    chat.event_task.abort();
    result
}
