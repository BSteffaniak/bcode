//! History and session event-stream plumbing for the TUI.

use std::collections::BTreeMap;

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

const SESSION_STREAM_CAPACITY: usize = 256;

/// Create the bounded reliable session-stream channel used by an active chat.
#[must_use]
pub fn session_stream_channel() -> (
    mpsc::Sender<SessionStreamUpdate>,
    mpsc::Receiver<SessionStreamUpdate>,
) {
    mpsc::channel(SESSION_STREAM_CAPACITY)
}

/// Update delivered by the resilient TUI session event stream.
#[derive(Debug)]
pub enum SessionStreamUpdate {
    /// Ordinary daemon event for the attached session.
    Event(Box<BcodeEvent>),
    /// Bounded lifecycle lookup resolved a missing acceptance payload.
    Submission {
        session_id: SessionId,
        record: Box<bcode_session_models::SubmissionRecord>,
    },
    /// Bounded authoritative queue status independent of history.
    QueueStatus {
        session_id: SessionId,
        status: Box<bcode_session_models::SubmissionQueueStatus>,
    },
    /// Bounded queue hydration failed; not an execution outcome.
    QueueUnavailable { session_id: SessionId },
    /// Event continuity was lost and the stream is reconnecting.
    ResyncStarted { session_id: SessionId },
    /// Fresh bounded state installed by a replacement attachment.
    Resynchronized {
        session_id: SessionId,
        attached: Box<bcode_client::AttachedSessionHistory>,
    },
}

const INITIAL_TRANSCRIPT_OVERSCAN_VIEWPORTS: usize = 2;
const SUPERSEDED_PROGRESS_FLUSH_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(50);
const MAX_PENDING_PROGRESS_KEYS: usize = 256;
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
) -> Result<
    (
        Vec<bcode_session_models::SessionEvent>,
        bool,
        bool,
        Vec<bcode_session_models::SubmissionRecord>,
    ),
    TuiError,
> {
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
    let (events, has_older, has_newer) =
        merge_timeline_history_pages(session_id, sequence, older, newer)?;
    let submissions = client.submissions_for_history(session_id, &events).await?;
    Ok((events, has_older, has_newer, submissions))
}

fn merge_timeline_history_pages(
    session_id: SessionId,
    sequence: u64,
    older: bcode_session_models::SessionHistoryPage,
    newer: bcode_session_models::SessionHistoryPage,
) -> Result<(Vec<bcode_session_models::SessionEvent>, bool, bool), TuiError> {
    let valid = |page: &bcode_session_models::SessionHistoryPage| {
        page.session_id == session_id
            && page.compatibility_issues.is_empty()
            && (!page.has_more || !page.events.is_empty())
            && page
                .events
                .iter()
                .all(|event| event.session_id == session_id)
            && page
                .events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
    };
    let older_boundary = older.events.first().map(|event| event.sequence);
    let newer_boundary = newer.events.last().map(|event| event.sequence);
    if !valid(&older)
        || !valid(&newer)
        || older.next_cursor.is_some_and(|cursor| {
            !older.has_more || older_boundary.is_none_or(|boundary| cursor.sequence >= boundary)
        })
        || newer.next_cursor.is_some_and(|cursor| {
            !newer.has_more || newer_boundary.is_none_or(|boundary| cursor.sequence <= boundary)
        })
        || older
            .events
            .last()
            .is_some_and(|event| event.sequence > sequence)
        || newer
            .events
            .first()
            .is_some_and(|event| event.sequence <= sequence)
        || older
            .events
            .last()
            .zip(newer.events.first())
            .is_some_and(|(before, after)| before.sequence >= after.sequence)
        || !older
            .events
            .iter()
            .chain(&newer.events)
            .any(|event| event.sequence == sequence)
    {
        return Err(TuiError::SessionUnavailable {
            session_id,
            reason: "timeline history cannot verify the selected source range; repair or retry the session".to_owned(),
        });
    }
    let has_older = older.has_more;
    let has_newer = newer.has_more;
    let mut events = older.events;
    events.extend(newer.events);
    Ok((events, has_older, has_newer))
}

/// Verify a completed timeline jump before replacing the active resident window.
///
/// The async result is untrusted even when the requester and window generation
/// match: it may have been produced by an incompatible or damaged history read.
#[must_use]
pub fn valid_timeline_window(
    session_id: SessionId,
    sequence: u64,
    events: &[bcode_session_models::SessionEvent],
    has_older: bool,
    has_newer: bool,
) -> bool {
    let Some(first) = events.first() else {
        return false;
    };
    let Some(last) = events.last() else {
        return false;
    };
    (!has_older || first.sequence > 0)
        && (!has_newer || last.sequence < u64::MAX)
        && events.iter().all(|event| event.session_id == session_id)
        && events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
        && events.iter().any(|event| event.sequence == sequence)
}

/// Attach to a session, but hold live event forwarding until the receiver is released.
pub async fn attach_paused_session_event_stream(
    client: &BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::Sender<SessionStreamUpdate>,
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
        Box::pin(reconnecting_event_stream(
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
                })
            },
        ))
        .await;
    });
    Ok((attached, event_task, release_sender))
}

/// Attach to a session with a bounded recent history limit and forward live events into the UI event channel.
#[allow(dead_code)]
pub async fn attach_session_event_stream_with_limit(
    client: &BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::Sender<SessionStreamUpdate>,
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
    event_sender: mpsc::Sender<SessionStreamUpdate>,
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
    event_sender: mpsc::Sender<SessionStreamUpdate>,
    limit: usize,
    connection: bcode_client::ClientConnection,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        Box::pin(reconnecting_event_stream(
            client,
            session_id,
            event_sender,
            connection,
            move |connection, session_id| {
                Box::pin(async move {
                    connection
                        .attach_session_recent_with_input_history(session_id, limit)
                        .await
                })
            },
        ))
        .await;
    })
}

fn spawn_reconnecting_window_event_stream(
    client: BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::Sender<SessionStreamUpdate>,
    request: ProjectionWindowRequest,
    connection: bcode_client::ClientConnection,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        Box::pin(reconnecting_event_stream(
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
                })
            },
        ))
        .await;
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SupersedableEventKey {
    Invocation(String),
    RuntimeWork(bcode_session_models::WorkId),
    ToolContribution {
        invocation_id: String,
        contribution_id: String,
    },
    ToolRequestDraft {
        tool_call_id: String,
        placement: &'static str,
    },
}

fn supersedable_event_key(event: &BcodeEvent) -> Option<SupersedableEventKey> {
    match event {
        BcodeEvent::Session(event) => match &event.kind {
            bcode_session_models::SessionEventKind::ToolInvocationLifecycle { event }
                if matches!(
                    event.stage,
                    bcode_session_models::ToolInvocationLifecycleStage::Progress
                        | bcode_session_models::ToolInvocationLifecycleStage::Waiting
                ) =>
            {
                Some(SupersedableEventKey::Invocation(
                    event.invocation_id.clone(),
                ))
            }
            bcode_session_models::SessionEventKind::RuntimeWorkProgress { work_id, .. } => {
                Some(SupersedableEventKey::RuntimeWork(work_id.clone()))
            }
            _ => None,
        },
        BcodeEvent::SessionLive(event) => match &event.kind {
            bcode_session_models::SessionLiveEventKind::ToolInvocationProgress { event } => Some(
                SupersedableEventKey::Invocation(event.invocation_id.clone()),
            ),
            bcode_session_models::SessionLiveEventKind::ToolContributionPlaced { envelope }
                if envelope.contribution.persistence
                    == bcode_session_models::ToolContributionPersistence::Transient
                    && envelope.placement
                        == bcode_session_models::ToolContributionPlacement::Progress
                    && envelope.contribution.operation
                        != bcode_session_models::ToolContributionOperation::Remove =>
            {
                Some(SupersedableEventKey::ToolContribution {
                    invocation_id: envelope.contribution.invocation_id.clone(),
                    contribution_id: envelope.contribution.contribution_id.clone(),
                })
            }
            bcode_session_models::SessionLiveEventKind::ToolRequestDraft { event }
                if !matches!(
                    &event.operation,
                    bcode_session_models::ToolRequestDraftOperation::Checkpoint { text, .. }
                        if event.revision == 1 && text.is_empty()
                ) && !matches!(
                    event.operation,
                    bcode_session_models::ToolRequestDraftOperation::Remove { .. }
                ) =>
            {
                Some(SupersedableEventKey::ToolRequestDraft {
                    tool_call_id: event.tool_call_id.clone(),
                    placement: match event.placement {
                        bcode_session_models::ToolContributionPlacement::Request => "request",
                        bcode_session_models::ToolContributionPlacement::Progress => "progress",
                        bcode_session_models::ToolContributionPlacement::Result => "result",
                        bcode_session_models::ToolContributionPlacement::Supplemental => {
                            "supplemental"
                        }
                        bcode_session_models::ToolContributionPlacement::Hidden => "hidden",
                    },
                })
            }
            _ => None,
        },
        BcodeEvent::RuntimeWork(_)
        | BcodeEvent::Workflow(_)
        | BcodeEvent::SessionViewResyncRequired { .. }
        | BcodeEvent::SessionCatalogUpdated { .. } => None,
    }
}

async fn flush_superseded_progress(
    event_sender: &mpsc::Sender<SessionStreamUpdate>,
    pending: &mut BTreeMap<SupersedableEventKey, BcodeEvent>,
) -> bool {
    let mut events = std::mem::take(pending).into_values().collect::<Vec<_>>();
    events.sort_by_key(|event| match event {
        BcodeEvent::Session(event) | BcodeEvent::RuntimeWork(event) => event.sequence,
        BcodeEvent::SessionLive(_)
        | BcodeEvent::Workflow(_)
        | BcodeEvent::SessionViewResyncRequired { .. }
        | BcodeEvent::SessionCatalogUpdated { .. } => 0,
    });
    for event in events {
        if event_sender
            .send(SessionStreamUpdate::Event(Box::new(event)))
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

// The receive future owns partial IPC framing state. Presentation timers must
// never cancel it; only closing the entire stream may abandon an in-flight read.
async fn receive_with_progress_flush<T>(
    receive: impl std::future::Future<Output = T>,
    sender: &mpsc::Sender<SessionStreamUpdate>,
    pending: &mut BTreeMap<SupersedableEventKey, BcodeEvent>,
    mut timer: std::pin::Pin<&mut tokio::time::Sleep>,
) -> Option<T> {
    tokio::pin!(receive);
    loop {
        tokio::select! {
            event = &mut receive => return Some(event),
            () = sender.closed() => return None,
            () = &mut timer, if !pending.is_empty() => {
                if !flush_superseded_progress(sender, pending).await { return None; }
                timer.as_mut().reset(tokio::time::Instant::now() + SUPERSEDED_PROGRESS_FLUSH_INTERVAL);
            }
        }
    }
}

async fn send_canonical_event(
    sender: &mpsc::Sender<SessionStreamUpdate>,
    pending: &mut BTreeMap<SupersedableEventKey, BcodeEvent>,
    event: BcodeEvent,
) -> bool {
    flush_superseded_progress(sender, pending).await
        && sender
            .send(SessionStreamUpdate::Event(Box::new(event)))
            .await
            .is_ok()
}

async fn hydrate_cancelled_submission(
    client: &BcodeClient,
    session_id: SessionId,
    sender: &mpsc::Sender<SessionStreamUpdate>,
    event: &BcodeEvent,
) -> bool {
    let BcodeEvent::Session(event) = event else {
        return true;
    };
    let (bcode_session_models::SessionEventKind::SubmissionCancelled { submission_id }
    | bcode_session_models::SessionEventKind::SubmissionPendingCancelled {
        submission_id, ..
    }
    | bcode_session_models::SessionEventKind::SubmissionRecoveryAuthorized {
        submission_id,
        ..
    }
    | bcode_session_models::SessionEventKind::SubmissionContextObserved {
        submission_id, ..
    }) = &event.kind
    else {
        return true;
    };
    if let Ok(Some(record)) = client
        .get_submission(session_id, submission_id.clone())
        .await
    {
        sender
            .send(SessionStreamUpdate::Submission {
                session_id,
                record: Box::new(record),
            })
            .await
            .is_ok()
    } else {
        false
    }
}

async fn reconnecting_event_stream<F>(
    client: BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::Sender<SessionStreamUpdate>,
    mut connection: bcode_client::ClientConnection,
    attach: F,
) where
    F: for<'a> Fn(
            &'a mut bcode_client::ClientConnection,
            SessionId,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            bcode_client::AttachedSessionHistory,
                            bcode_client::ClientError,
                        >,
                    > + Send
                    + 'a,
            >,
        > + Send
        + 'static,
{
    let mut reconnect_delay = std::time::Duration::from_millis(100);
    if !send_queue_status(&client, session_id, &event_sender).await {
        return;
    }
    let mut pending_progress = BTreeMap::new();
    let progress_flush = tokio::time::sleep(SUPERSEDED_PROGRESS_FLUSH_INTERVAL);
    tokio::pin!(progress_flush);
    let queue_refresh = tokio::time::sleep(std::time::Duration::from_secs(30));
    tokio::pin!(queue_refresh);
    loop {
        let Some(received) = receive_with_queue_refresh(
            &client,
            session_id,
            &event_sender,
            queue_refresh.as_mut(),
            receive_with_progress_flush(
                connection.recv_event_without_reconnect(),
                &event_sender,
                &mut pending_progress,
                progress_flush.as_mut(),
            ),
        )
        .await
        else {
            return;
        };
        let needs_resync = match received {
            Ok(BcodeEvent::SessionViewResyncRequired {
                session_id: required,
            }) if required == session_id => true,
            Ok(ref event)
                if !hydrate_cancelled_submission(&client, session_id, &event_sender, event)
                    .await =>
            {
                true
            }
            Ok(event) => {
                if matches!(&event, BcodeEvent::Session(event) if bcode_session_view::submission_queue_needs_refresh(&event.kind))
                    && !send_queue_status(&client, session_id, &event_sender).await
                {
                    return;
                }
                reconnect_delay = std::time::Duration::from_millis(100);
                let Some(reset_flush) =
                    forward_stream_event(&event_sender, &mut pending_progress, event).await
                else {
                    return;
                };
                if reset_flush {
                    progress_flush
                        .as_mut()
                        .reset(tokio::time::Instant::now() + SUPERSEDED_PROGRESS_FLUSH_INTERVAL);
                }
                false
            }
            Err(error) => {
                tracing::warn!(target: "bcode_tui::session_stream", %session_id,
                    transport_unavailable = error.is_daemon_unavailable(),
                    "session receive failed; starting bounded resynchronization");
                true
            }
        };
        if !needs_resync {
            continue;
        }
        pending_progress.clear();

        // Dropping the stale connection detaches its client before replacement attach. This keeps
        // session client accounting and idle database release semantics accurate.
        drop(connection);
        if event_sender
            .send(SessionStreamUpdate::ResyncStarted { session_id })
            .await
            .is_err()
        {
            return;
        }
        loop {
            if event_sender.is_closed() {
                return;
            }
            match client.connect("bcode-tui-bmux").await {
                Ok(mut next_connection) => {
                    if let Ok(attached) = attach(&mut next_connection, session_id).await {
                        if !send_resynchronized(&client, session_id, &event_sender, attached).await
                        {
                            return;
                        }
                        tracing::info!(target: "bcode_tui::session_stream", %session_id,
                            "session event stream resynchronized");
                        connection = next_connection;
                        reconnect_delay = std::time::Duration::from_millis(100);
                        break;
                    }
                }
                Err(_error) => {}
            }
            reconnect_delay = wait_before_reconnect(session_id, reconnect_delay).await;
        }
    }
}

async fn wait_before_reconnect(
    session_id: SessionId,
    delay: std::time::Duration,
) -> std::time::Duration {
    tracing::warn!(target: "bcode_tui::session_stream", %session_id,
        retry_delay_ms = delay.as_millis(),
        "session reconnect or attachment failed; retrying");
    tokio::time::sleep(delay).await;
    (delay * 2).min(std::time::Duration::from_secs(2))
}

async fn receive_with_queue_refresh<F, T>(
    client: &BcodeClient,
    session_id: SessionId,
    sender: &mpsc::Sender<SessionStreamUpdate>,
    mut deadline: std::pin::Pin<&mut tokio::time::Sleep>,
    receive: F,
) -> Option<T>
where
    F: std::future::Future<Output = Option<T>>,
{
    tokio::pin!(receive);
    loop {
        tokio::select! {
            result = &mut receive => return result,
            () = sender.closed() => return None,
            () = deadline.as_mut() => {
                if !send_queue_status(client, session_id, sender).await { return None; }
                deadline.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(30));
            }
        }
    }
}

async fn send_queue_status(
    client: &BcodeClient,
    session_id: SessionId,
    sender: &mpsc::Sender<SessionStreamUpdate>,
) -> bool {
    send_queue_status_with_retry(
        session_id,
        sender,
        || client.queue_status(session_id),
        std::time::Duration::from_secs(1),
    )
    .await
}

async fn send_queue_status_with_retry<F, Fut>(
    session_id: SessionId,
    sender: &mpsc::Sender<SessionStreamUpdate>,
    mut fetch: F,
    delay: std::time::Duration,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
            Output = Result<bcode_session_models::SubmissionQueueStatus, bcode_client::ClientError>,
        >,
{
    for attempt in 0..4 {
        if let Ok(status) = fetch().await {
            return sender
                .send(SessionStreamUpdate::QueueStatus {
                    session_id,
                    status: Box::new(status),
                })
                .await
                .is_ok();
        }
        if attempt == 0
            && sender
                .send(SessionStreamUpdate::QueueUnavailable { session_id })
                .await
                .is_err()
        {
            return false;
        }
        if attempt < 3 {
            tokio::select! {
                () = sender.closed() => return false,
                () = tokio::time::sleep(delay.saturating_mul(1 << attempt)) => {}
            }
        }
    }
    true
}

async fn forward_stream_event(
    sender: &mpsc::Sender<SessionStreamUpdate>,
    pending: &mut BTreeMap<SupersedableEventKey, BcodeEvent>,
    event: BcodeEvent,
) -> Option<bool> {
    if let Some(key) = supersedable_event_key(&event) {
        let was_empty = pending.is_empty();
        pending.insert(key, event);
        if pending.len() >= MAX_PENDING_PROGRESS_KEYS
            && !flush_superseded_progress(sender, pending).await
        {
            return None;
        }
        Some(was_empty)
    } else {
        send_canonical_event(sender, pending, event)
            .await
            .then_some(false)
    }
}

async fn send_resynchronized(
    client: &BcodeClient,
    session_id: SessionId,
    sender: &mpsc::Sender<SessionStreamUpdate>,
    attached: bcode_client::AttachedSessionHistory,
) -> bool {
    if sender
        .send(SessionStreamUpdate::Resynchronized {
            session_id,
            attached: Box::new(attached),
        })
        .await
        .is_err()
    {
        return false;
    }
    send_queue_status(client, session_id, sender).await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn queue_hydration_failure_is_visible_and_recovers_without_lifecycle_events() {
        let session_id = SessionId::new();
        let (sender, mut receiver) = mpsc::channel(8);
        let mut attempts = 0;
        let started = std::time::Instant::now();
        let result = send_queue_status_with_retry(
            session_id,
            &sender,
            || {
                attempts += 1;
                std::future::ready(if attempts == 1 {
                    Err(bcode_client::ClientError::UnexpectedResponse)
                } else {
                    Ok(bcode_session_models::SubmissionQueueStatus {
                        oldest: None,
                        blocker: None,
                    })
                })
            },
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(result);
        assert_eq!(attempts, 2);
        assert!(started.elapsed() >= std::time::Duration::from_millis(1));
        assert!(
            matches!(receiver.recv().await, Some(SessionStreamUpdate::QueueUnavailable { session_id: actual }) if actual == session_id)
        );
        assert!(
            matches!(receiver.recv().await, Some(SessionStreamUpdate::QueueStatus { status, .. }) if status.oldest.is_none())
        );
    }

    #[tokio::test]
    async fn queue_hydration_retry_budget_is_finite_and_preserves_unavailable_state() {
        let (sender, mut receiver) = mpsc::channel(8);
        let mut attempts = 0;
        assert!(
            send_queue_status_with_retry(
                SessionId::new(),
                &sender,
                || {
                    attempts += 1;
                    std::future::ready(Err(bcode_client::ClientError::UnexpectedResponse))
                },
                std::time::Duration::from_millis(1)
            )
            .await
        );
        assert_eq!(attempts, 4);
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionStreamUpdate::QueueUnavailable { .. })
        ));
        assert!(receiver.try_recv().is_err());
    }

    use super::*;

    #[test]
    fn timeline_pages_fail_closed_on_incompatible_or_unrelated_history() {
        use bcode_session_models::{
            CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventCompatibilityIssue,
            SessionEventCompatibilityKind, SessionEventKind, SessionHistoryPage,
        };
        let session_id = SessionId::new();
        let event = |sequence, session_id| SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: SessionEventKind::AssistantMessage {
                text: format!("message {sequence}"),
            },
        };
        let older = SessionHistoryPage {
            session_id,
            events: vec![event(9, session_id), event(10, session_id)],
            compatibility_issues: Vec::new(),
            next_cursor: Some(SessionHistoryCursor { sequence: 8 }),
            has_more: true,
        };
        let newer = SessionHistoryPage {
            session_id,
            events: vec![event(11, session_id)],
            compatibility_issues: Vec::new(),
            next_cursor: None,
            has_more: false,
        };
        let (events, has_older, has_newer) =
            merge_timeline_history_pages(session_id, 10, older.clone(), newer.clone())
                .expect("contiguous source");
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![9, 10, 11]
        );
        assert!(has_older);
        assert!(!has_newer);

        let mut damaged = older.clone();
        damaged
            .compatibility_issues
            .push(SessionEventCompatibilityIssue {
                sequence: 9,
                event_kind: "future".to_owned(),
                schema_version: u16::MAX,
                compatibility: SessionEventCompatibilityKind::FutureSchema,
                remediation: "upgrade".to_owned(),
            });
        assert!(merge_timeline_history_pages(session_id, 10, damaged, newer.clone()).is_err());
        let mut wrong_session = newer.clone();
        wrong_session.events[0].session_id = SessionId::new();
        assert!(
            merge_timeline_history_pages(session_id, 10, older.clone(), wrong_session).is_err()
        );
        let mut overlap = newer.clone();
        overlap.events[0].sequence = 10;
        assert!(merge_timeline_history_pages(session_id, 10, older.clone(), overlap).is_err());
        assert!(
            merge_timeline_history_pages(
                session_id,
                10,
                older.clone(),
                SessionHistoryPage {
                    events: Vec::new(),
                    has_more: true,
                    ..newer.clone()
                }
            )
            .is_err()
        );
        let mut invalid_cursor = older.clone();
        invalid_cursor.next_cursor = Some(SessionHistoryCursor { sequence: 9 });
        assert!(
            merge_timeline_history_pages(session_id, 10, invalid_cursor, newer.clone()).is_err()
        );
        let mut missing = older;
        missing.events.pop();
        assert!(merge_timeline_history_pages(session_id, 10, missing, newer).is_err());
    }

    #[tokio::test]
    async fn progress_flush_preserves_partial_ipc_frame() {
        use tokio::io::AsyncWriteExt as _;
        let session_id = SessionId::new();
        let event = lifecycle_event(
            session_id,
            1,
            bcode_session_models::ToolInvocationLifecycleStage::Progress,
        );
        let mut pending = BTreeMap::new();
        pending.insert(supersedable_event_key(&event).expect("progress key"), event);
        let envelope = bcode_ipc::request_envelope(1, &bcode_ipc::Request::Ping).expect("envelope");
        let bytes = bcode_ipc::encode_envelope_frames(&envelope)
            .expect("frames")
            .concat();
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        writer.write_all(&bytes[..4]).await.expect("header");
        let (sender, mut receiver) = session_stream_channel();
        let timer = tokio::time::sleep(std::time::Duration::from_millis(5));
        tokio::pin!(timer);
        let deliver = async {
            receiver
                .recv()
                .await
                .expect("progress flushed during partial read");
            writer
                .write_all(&bytes[4..])
                .await
                .expect("remaining frame");
        };
        let read = receive_with_progress_flush(
            bcode_ipc::recv_envelope(&mut reader),
            &sender,
            &mut pending,
            timer.as_mut(),
        );
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(read, deliver)
        })
        .await
        .expect("read completed");
        assert_eq!(
            result
                .expect("stream open")
                .expect("valid envelope")
                .payload,
            envelope.payload
        );
    }

    fn lifecycle_event(
        session_id: SessionId,
        sequence: u64,
        stage: bcode_session_models::ToolInvocationLifecycleStage,
    ) -> BcodeEvent {
        BcodeEvent::Session(bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence,
                    stage,
                    message: Some(sequence.to_string()),
                    metadata: serde_json::Value::Null,
                },
            },
        })
    }

    fn placed_progress_event(
        session_id: SessionId,
        sequence: u64,
        operation: bcode_session_models::ToolContributionOperation,
    ) -> BcodeEvent {
        BcodeEvent::SessionLive(bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Progress,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call-1".to_owned(),
                        contribution_id: "preview".to_owned(),
                        sequence,
                        producer_id: "test.plugin".to_owned(),
                        schema: "test.progress".to_owned(),
                        schema_version: 1,
                        operation,
                        persistence: bcode_session_models::ToolContributionPersistence::Transient,
                        artifact: None,
                        payload: serde_json::json!({"sequence": sequence}),
                    },
                ),
            },
        })
    }

    fn request_draft_event(
        session_id: SessionId,
        revision: u64,
        operation: bcode_session_models::ToolRequestDraftOperation,
    ) -> BcodeEvent {
        BcodeEvent::SessionLive(bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Request,
                    generation: 1,
                    revision,
                    operation,
                    argument_bytes: usize::try_from(revision).unwrap_or(usize::MAX),
                    truncated: false,
                },
            },
        })
    }

    #[test]
    fn progress_events_are_supersedable_but_boundaries_are_not() {
        let session_id = SessionId::new();
        assert_eq!(
            supersedable_event_key(&lifecycle_event(
                session_id,
                1,
                bcode_session_models::ToolInvocationLifecycleStage::Progress,
            )),
            Some(SupersedableEventKey::Invocation("call-1".to_owned()))
        );
        assert!(
            supersedable_event_key(&lifecycle_event(
                session_id,
                2,
                bcode_session_models::ToolInvocationLifecycleStage::Completed,
            ))
            .is_none()
        );
        assert!(
            supersedable_event_key(&placed_progress_event(
                session_id,
                1,
                bcode_session_models::ToolContributionOperation::Upsert,
            ))
            .is_some()
        );
        assert!(
            supersedable_event_key(&placed_progress_event(
                session_id,
                2,
                bcode_session_models::ToolContributionOperation::Remove,
            ))
            .is_none()
        );
        assert!(
            supersedable_event_key(&request_draft_event(
                session_id,
                1,
                bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                    start_offset: 0,
                    text: String::new(),
                },
            ))
            .is_none()
        );
        assert_eq!(
            supersedable_event_key(&request_draft_event(
                session_id,
                1,
                bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                    start_offset: 0,
                    text: "first payload".to_owned(),
                },
            )),
            Some(SupersedableEventKey::ToolRequestDraft {
                tool_call_id: "call-1".to_owned(),
                placement: "request",
            })
        );
        assert_eq!(
            supersedable_event_key(&request_draft_event(
                session_id,
                2,
                bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                    start_offset: 0,
                    text: "draft".to_owned(),
                },
            )),
            Some(SupersedableEventKey::ToolRequestDraft {
                tool_call_id: "call-1".to_owned(),
                placement: "request",
            })
        );
        assert!(
            supersedable_event_key(&request_draft_event(
                session_id,
                3,
                bcode_session_models::ToolRequestDraftOperation::Remove {
                    reason: bcode_session_models::ToolRequestDraftTerminalReason::Completed,
                },
            ))
            .is_none()
        );
    }

    #[tokio::test]
    async fn request_drafts_collapse_to_latest_checkpoint_per_placement_aware_key() {
        let session_id = SessionId::new();
        let (sender, receiver) = session_stream_channel();
        let mut pending = BTreeMap::new();
        for revision in 2..=10_000 {
            let mut event = request_draft_event(
                session_id,
                revision,
                bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                    start_offset: 0,
                    text: revision.to_string(),
                },
            );
            let BcodeEvent::SessionLive(live) = &mut event else {
                unreachable!("request draft is live");
            };
            let bcode_session_models::SessionLiveEventKind::ToolRequestDraft { event: draft } =
                &mut live.kind
            else {
                unreachable!("request draft event");
            };
            draft.tool_call_id = format!("call-{}", revision % 256);
            assert_eq!(
                supersedable_event_key(&event),
                Some(SupersedableEventKey::ToolRequestDraft {
                    tool_call_id: format!("call-{}", revision % 256),
                    placement: "request",
                })
            );
            pending.insert(
                supersedable_event_key(&event).expect("request draft key"),
                event,
            );
        }
        assert_eq!(pending.len(), 256);
        assert!(flush_superseded_progress(&sender, &mut pending).await);
        assert!(pending.is_empty());
        assert_eq!(receiver.len(), 256);
    }

    #[tokio::test]
    async fn placed_progress_flood_collapses_to_latest_update() {
        let session_id = SessionId::new();
        let (sender, mut receiver) = session_stream_channel();
        let mut pending = BTreeMap::new();
        for sequence in 1..=10_000 {
            let event = placed_progress_event(
                session_id,
                sequence,
                bcode_session_models::ToolContributionOperation::Upsert,
            );
            pending.insert(supersedable_event_key(&event).expect("progress key"), event);
        }

        assert!(flush_superseded_progress(&sender, &mut pending).await);
        assert!(pending.is_empty());
        let SessionStreamUpdate::Event(event) = receiver.try_recv().expect("latest progress")
        else {
            panic!("expected progress event");
        };
        assert!(matches!(
            *event,
            BcodeEvent::SessionLive(bcode_session_models::SessionLiveEvent {
                kind: bcode_session_models::SessionLiveEventKind::ToolContributionPlaced {
                    envelope: bcode_session_models::ToolContributionEnvelope {
                        contribution: bcode_session_models::ToolContributionEvent {
                            sequence: 10_000,
                            ..
                        },
                        ..
                    },
                },
                ..
            })
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn progress_flood_collapses_to_latest_update() {
        let session_id = SessionId::new();
        let (sender, mut receiver) = session_stream_channel();
        let mut pending = BTreeMap::new();
        for sequence in 1..=10_000 {
            let event = lifecycle_event(
                session_id,
                sequence,
                bcode_session_models::ToolInvocationLifecycleStage::Progress,
            );
            pending.insert(supersedable_event_key(&event).expect("progress key"), event);
        }

        assert!(flush_superseded_progress(&sender, &mut pending).await);
        assert!(pending.is_empty());
        let SessionStreamUpdate::Event(event) = receiver.try_recv().expect("latest progress")
        else {
            panic!("expected progress event");
        };
        assert!(matches!(
            *event,
            BcodeEvent::Session(bcode_session_models::SessionEvent {
                sequence: 10_000,
                ..
            })
        ));
        assert!(receiver.try_recv().is_err());
    }
}
