//! History and session event-stream plumbing for the TUI.

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::{SessionHistoryDirection, SessionHistoryQuery, SessionId};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{
    INITIAL_HISTORY_EVENT_LIMIT, OLDER_HISTORY_EVENT_LIMIT, TuiError, session_flow::ActiveChat,
};

/// Load the next older page of transcript history when available.
pub async fn load_older_history(
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(cursor) = chat.app.older_history_cursor() else {
        return Ok(());
    };
    chat.app.set_loading_older_history(true);
    match client
        .session_history_page(
            chat.session_id,
            SessionHistoryQuery {
                cursor: Some(cursor),
                limit: OLDER_HISTORY_EVENT_LIMIT,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
    {
        Ok(page) => {
            chat.app.prepend_older_history(&page.events, page.has_more);
        }
        Err(error) => {
            chat.app.set_loading_older_history(false);
            chat.app
                .set_status(format!("older history load failed: {error}"));
        }
    }
    Ok(())
}

/// Attach to a session and forward live events into the UI event channel.
pub async fn attach_session_event_stream(
    client: &BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
) -> Result<(bcode_client::AttachedSessionHistory, JoinHandle<()>), TuiError> {
    let mut connection = client.connect("bcode-tui-bmux").await?;
    let attached = connection
        .attach_session_recent_with_input_history(session_id, INITIAL_HISTORY_EVENT_LIMIT)
        .await?;
    let event_task = tokio::spawn(async move {
        loop {
            match connection.recv_event().await {
                Ok(event) => {
                    if event_sender.send(event).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("BMUX TUI event stream ended: {error}");
                    break;
                }
            }
        }
    });
    Ok((attached, event_task))
}
