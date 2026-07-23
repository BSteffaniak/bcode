//! History and session event-stream plumbing for the TUI.

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::{
    ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
    ProjectionWindowRequest, ProjectionWindowTarget, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery, SessionId, SessionProjectionKind,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::TuiError;

const INITIAL_TRANSCRIPT_OVERSCAN_VIEWPORTS: usize = 2;
const INITIAL_TRANSCRIPT_MIN_ITEMS: usize = 12;
const INITIAL_TRANSCRIPT_MAX_ITEMS: usize = 64;
const INITIAL_TRANSCRIPT_MAX_EVENTS_SCANNED: usize = 2_048;
const INITIAL_TRANSCRIPT_MAX_BYTES: usize = 512 * 1024;
const TIMELINE_JUMP_MAX_EVENTS_SCANNED: usize = 1_024;

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
) -> Result<(Vec<bcode_session_models::SessionEvent>, bool, bool), TuiError> {
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
    Ok((events, older.has_more, newer.has_more))
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
        |_| {},
    )
    .await
}

/// Attach to a session, but hold live event forwarding until the receiver is released.
pub async fn attach_paused_session_event_stream(
    client: &BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
) -> Result<
    (
        bcode_client::AttachedSessionHistory,
        JoinHandle<()>,
        oneshot::Sender<()>,
    ),
    TuiError,
> {
    let mut connection = client.connect("bcode-tui-bmux").await?;
    let request = initial_transcript_window_request(bmux_tui::geometry::Rect::new(0, 0, 80, 24));
    let attached = attach_projection_window(&mut connection, session_id, request.clone()).await?;
    let reconnect_client = client.clone();
    let (release_sender, release_receiver) = oneshot::channel();
    let event_task = tokio::spawn(async move {
        if release_receiver.await.is_err() {
            return;
        }
        reconnecting_event_stream(
            reconnect_client,
            session_id,
            event_sender,
            connection,
            move |connection, session_id| {
                let request = request.clone();
                Box::pin(async move {
                    connection
                        .attach_session_projection_window_with_input_history(session_id, request)
                        .await
                        .map(resynchronization_events)
                })
            },
        )
        .await;
    });
    Ok((attached, event_task, release_sender))
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
    let reconnect_client = client.clone();
    let event_task = spawn_reconnecting_recent_event_stream(
        reconnect_client,
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
    mut on_progress: impl FnMut(&bcode_session_models::SessionOpenOperationSnapshot),
) -> Result<(bcode_client::AttachedSessionHistory, JoinHandle<()>), TuiError> {
    let mut connection = client.connect("bcode-tui-bmux").await?;
    let attached = match connection
        .prepare_then_attach_session_projection_window(session_id, request.clone(), |snapshot| {
            on_progress(snapshot);
        })
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
    let reconnect_client = client.clone();
    let event_task = spawn_reconnecting_window_event_stream(
        reconnect_client,
        session_id,
        event_sender,
        request,
        connection,
    );
    Ok((attached, event_task))
}

async fn attach_projection_window(
    connection: &mut bcode_client::ClientConnection,
    session_id: SessionId,
    request: ProjectionWindowRequest,
) -> Result<bcode_client::AttachedSessionHistory, TuiError> {
    match connection
        .attach_session_projection_window_with_input_history(session_id, request)
        .await
    {
        Ok(attached) => Ok(attached),
        Err(bcode_client::ClientError::Server { code, message })
            if code == "projection_stale" || code == "session_repair_required" =>
        {
            Err(TuiError::SessionUnavailable {
                session_id,
                reason: message,
            })
        }
        Err(error) => Err(error.into()),
    }
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
                        .map(resynchronization_events)
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
                        .map(resynchronization_events)
                })
            },
        )
        .await;
    })
}

fn resynchronization_events(attached: bcode_client::AttachedSessionHistory) -> Vec<BcodeEvent> {
    attached
        .history
        .into_iter()
        .filter(|event| {
            matches!(
                event.kind,
                bcode_session_models::SessionEventKind::ModelChanged { .. }
                    | bcode_session_models::SessionEventKind::ReasoningChanged { .. }
                    | bcode_session_models::SessionEventKind::SkillActivated { .. }
                    | bcode_session_models::SessionEventKind::SkillDeactivated { .. }
                    | bcode_session_models::SessionEventKind::RequestContextObserved { .. }
                    | bcode_session_models::SessionEventKind::PermissionRequested { .. }
                    | bcode_session_models::SessionEventKind::PermissionResolved { .. }
                    | bcode_session_models::SessionEventKind::RuntimeWorkStarted { .. }
                    | bcode_session_models::SessionEventKind::RuntimeWorkCancelRequested { .. }
                    | bcode_session_models::SessionEventKind::RuntimeWorkProgress { .. }
                    | bcode_session_models::SessionEventKind::RuntimeWorkFinished { .. }
            )
        })
        .map(BcodeEvent::Session)
        .collect()
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
                dyn std::future::Future<Output = Result<Vec<BcodeEvent>, bcode_client::ClientError>>
                    + Send
                    + 'a,
            >,
        > + Send
        + 'static,
{
    let mut reconnect_delay = std::time::Duration::from_millis(100);
    loop {
        match connection.recv_event().await {
            Ok(event) => {
                reconnect_delay = std::time::Duration::from_millis(100);
                if event_sender.send(event).is_err() {
                    return;
                }
            }
            Err(_error) => loop {
                if event_sender.is_closed() {
                    return;
                }
                match client.connect("bcode-tui-bmux").await {
                    Ok(mut next_connection) => {
                        if let Ok(events) = attach(&mut next_connection, session_id).await {
                            if event_sender
                                .send(BcodeEvent::SessionViewResyncRequired { session_id })
                                .is_err()
                            {
                                return;
                            }
                            for event in events {
                                if event_sender.send(event).is_err() {
                                    return;
                                }
                            }
                            connection = next_connection;
                            break;
                        }
                    }
                    Err(_error) => {}
                }
                tokio::time::sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(std::time::Duration::from_secs(2));
            },
        }
    }
}
