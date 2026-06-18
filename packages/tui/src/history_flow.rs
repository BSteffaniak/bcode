//! History and session event-stream plumbing for the TUI.

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::{
    ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
    ProjectionWindowRequest, ProjectionWindowTarget, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery, SessionId, SessionProjectionKind,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

use super::{OLDER_HISTORY_EVENT_LIMIT, TuiError, session_flow::ActiveChat};

const INITIAL_TRANSCRIPT_OVERSCAN_VIEWPORTS: usize = 2;
const INITIAL_TRANSCRIPT_MIN_ITEMS: usize = 12;
const INITIAL_TRANSCRIPT_MAX_ITEMS: usize = 64;
const INITIAL_TRANSCRIPT_MAX_EVENTS_SCANNED: usize = 2_048;
const INITIAL_TRANSCRIPT_MAX_BYTES: usize = 512 * 1024;
const TIMELINE_JUMP_MAX_EVENTS_SCANNED: usize = 1_024;
const EVENT_STREAM_RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(100);
const EVENT_STREAM_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(2);

/// Build the projection-window request used for initial session attach.
#[must_use]
pub fn initial_transcript_window_request(
    transcript_area: bmux_tui::geometry::Rect,
) -> ProjectionWindowRequest {
    let viewport_rows = usize::from(transcript_area.height.max(1));
    ProjectionWindowRequest {
        projection: SessionProjectionKind::Transcript,
        anchor: ProjectionWindowAnchor::Latest,
        direction: ProjectionWindowDirection::Backward,
        target: ProjectionWindowTarget {
            min_items: Some(INITIAL_TRANSCRIPT_MIN_ITEMS),
            min_estimated_rows: Some(
                viewport_rows.saturating_mul(INITIAL_TRANSCRIPT_OVERSCAN_VIEWPORTS),
            ),
            min_bytes: None,
            width_columns: Some(transcript_area.width.max(1)),
        },
        limits: ProjectionWindowLimits {
            max_items: INITIAL_TRANSCRIPT_MAX_ITEMS,
            max_events_scanned: INITIAL_TRANSCRIPT_MAX_EVENTS_SCANNED,
            max_bytes: INITIAL_TRANSCRIPT_MAX_BYTES,
        },
    }
}

/// Load a bounded transcript event window around an event sequence.
pub async fn load_timeline_jump_events(
    client: &BcodeClient,
    session_id: SessionId,
    sequence: u64,
) -> Result<Vec<bcode_session_models::SessionEvent>, TuiError> {
    let half_limit = TIMELINE_JUMP_MAX_EVENTS_SCANNED / 2;
    let older = client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: Some(SessionHistoryCursor { sequence }),
                limit: half_limit.max(1),
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await?;
    let newer = client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: Some(SessionHistoryCursor {
                    sequence: sequence.saturating_add(1),
                }),
                limit: half_limit.max(1),
                direction: SessionHistoryDirection::Forward,
            },
        )
        .await?;
    let mut events = older.events;
    events.extend(
        newer
            .events
            .into_iter()
            .filter(|event| event.sequence != sequence),
    );
    events.sort_by_key(|event| event.sequence);
    events.dedup_by_key(|event| event.sequence);
    Ok(events)
}

/// Load the next older page of transcript history when available.
#[allow(dead_code)]
pub async fn load_older_history(
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(cursor) = chat.app.older_history_cursor() else {
        return Ok(());
    };
    chat.app.set_loading_older_history(true);
    let Some(session_id) = chat.session_id else {
        return Ok(());
    };
    match client
        .session_history_page(
            session_id,
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
    attach_session_event_stream_with_window_request(
        client,
        session_id,
        event_sender,
        initial_transcript_window_request(bmux_tui::geometry::Rect::new(0, 0, 80, 24)),
    )
    .await
}

/// Attach to a session with a bounded recent history limit and forward live events into the UI event channel.
#[allow(dead_code)]
pub async fn attach_session_event_stream_with_limit(
    client: &BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
    limit: usize,
) -> Result<(bcode_client::AttachedSessionHistory, JoinHandle<()>), TuiError> {
    let mut connection = client.connect("bcode-tui-bmux").await?;
    let attached = match connection
        .attach_session_recent_with_input_history(session_id, limit)
        .await
    {
        Ok(attached) => attached,
        Err(bcode_client::ClientError::Server { code, message })
            if code == "projection_stale" || code == "session_repair_required" =>
        {
            return Err(TuiError::SessionUnavailable {
                session_id,
                reason: message,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let event_task = spawn_reconnecting_recent_event_stream(
        client.clone(),
        session_id,
        event_sender,
        limit,
        connection,
    );
    Ok((attached, event_task))
}

/// Attach to a session with a semantic projection-window request and forward live events into the UI event channel.
pub async fn attach_session_event_stream_with_window_request(
    client: &BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
    request: ProjectionWindowRequest,
) -> Result<(bcode_client::AttachedSessionHistory, JoinHandle<()>), TuiError> {
    let mut connection = client.connect("bcode-tui-bmux").await?;
    let attached = match connection
        .attach_session_projection_window_with_input_history(session_id, request.clone())
        .await
    {
        Ok(attached) => attached,
        Err(bcode_client::ClientError::Server { code, message })
            if code == "projection_stale" || code == "session_repair_required" =>
        {
            return Err(TuiError::SessionUnavailable {
                session_id,
                reason: message,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let event_task = spawn_reconnecting_window_event_stream(
        client.clone(),
        session_id,
        event_sender,
        request,
        connection,
    );
    Ok((attached, event_task))
}

fn spawn_reconnecting_recent_event_stream(
    client: BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
    limit: usize,
    connection: bcode_client::ClientConnection,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        reconnecting_event_stream(
            client,
            session_id,
            event_sender,
            connection,
            move |connection, session_id| {
                Box::pin(async move {
                    connection
                        .attach_session_recent_with_input_history(session_id, limit)
                        .await
                        .map(|_| ())
                })
            },
        )
        .await;
    })
}

fn spawn_reconnecting_window_event_stream(
    client: BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
    request: ProjectionWindowRequest,
    connection: bcode_client::ClientConnection,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        reconnecting_event_stream(
            client,
            session_id,
            event_sender,
            connection,
            move |connection, session_id| {
                let request = request.clone();
                Box::pin(async move {
                    connection
                        .attach_session_projection_window_with_input_history(session_id, request)
                        .await
                        .map(|_| ())
                })
            },
        )
        .await;
    })
}

async fn reconnecting_event_stream<F>(
    client: BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
    mut connection: bcode_client::ClientConnection,
    attach: F,
) where
    F: for<'a> Fn(
            &'a mut bcode_client::ClientConnection,
            SessionId,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), bcode_client::ClientError>> + Send + 'a,
            >,
        > + Send
        + 'static,
{
    let mut reconnect_delay = EVENT_STREAM_RECONNECT_INITIAL_DELAY;
    let mut latest_sequence = 0;
    loop {
        while let Ok(event) = connection.recv_event().await {
            reconnect_delay = EVENT_STREAM_RECONNECT_INITIAL_DELAY;
            latest_sequence = latest_sequence.max(event_sequence(&event));
            if event_sender.send(event).is_err() {
                return;
            }
        }

        loop {
            if event_sender.is_closed() {
                return;
            }
            sleep(reconnect_delay).await;
            reconnect_delay =
                (reconnect_delay.saturating_mul(2)).min(EVENT_STREAM_RECONNECT_MAX_DELAY);

            let Ok(mut next_connection) = client.connect("bcode-tui-bmux").await else {
                continue;
            };
            if attach(&mut next_connection, session_id).await.is_ok() {
                if let Ok(replayed_latest_sequence) =
                    replay_gap(&client, session_id, latest_sequence, &event_sender).await
                {
                    latest_sequence = latest_sequence.max(replayed_latest_sequence);
                }
                connection = next_connection;
                reconnect_delay = EVENT_STREAM_RECONNECT_INITIAL_DELAY;
                break;
            }
        }
    }
}

const fn event_sequence(event: &BcodeEvent) -> u64 {
    match event {
        BcodeEvent::Session(event) | BcodeEvent::RuntimeWork(event) => event.sequence,
        BcodeEvent::SessionLive(_) | BcodeEvent::SessionCatalogUpdated { .. } => 0,
    }
}

async fn replay_gap(
    client: &BcodeClient,
    session_id: SessionId,
    latest_sequence: u64,
    event_sender: &mpsc::UnboundedSender<BcodeEvent>,
) -> Result<u64, bcode_client::ClientError> {
    let page = client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: Some(SessionHistoryCursor {
                    sequence: latest_sequence.saturating_add(1),
                }),
                limit: 512,
                direction: SessionHistoryDirection::Forward,
            },
        )
        .await?;
    let mut replayed_latest_sequence = latest_sequence;
    for event in page.events {
        if event.sequence > latest_sequence {
            replayed_latest_sequence = replayed_latest_sequence.max(event.sequence);
            if event_sender.send(BcodeEvent::Session(event)).is_err() {
                break;
            }
        }
    }
    Ok(replayed_latest_sequence)
}
