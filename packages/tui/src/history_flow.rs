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
                })
            },
        )
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
                })
            },
        )
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
    let mut pending_progress = BTreeMap::new();
    let progress_flush = tokio::time::sleep(SUPERSEDED_PROGRESS_FLUSH_INTERVAL);
    tokio::pin!(progress_flush);
    loop {
        let received = tokio::select! {
            event = connection.recv_event() => Some(event),
            () = &mut progress_flush, if !pending_progress.is_empty() => None,
        };
        let Some(received) = received else {
            if !flush_superseded_progress(&event_sender, &mut pending_progress).await {
                return;
            }
            progress_flush
                .as_mut()
                .reset(tokio::time::Instant::now() + SUPERSEDED_PROGRESS_FLUSH_INTERVAL);
            continue;
        };
        let needs_resync = match received {
            Ok(BcodeEvent::SessionViewResyncRequired {
                session_id: required,
            }) if required == session_id => true,
            Ok(event) => {
                reconnect_delay = std::time::Duration::from_millis(100);
                if let Some(key) = supersedable_event_key(&event) {
                    let was_empty = pending_progress.is_empty();
                    pending_progress.insert(key, event);
                    if pending_progress.len() >= MAX_PENDING_PROGRESS_KEYS
                        && !flush_superseded_progress(&event_sender, &mut pending_progress).await
                    {
                        return;
                    }
                    if was_empty {
                        progress_flush.as_mut().reset(
                            tokio::time::Instant::now() + SUPERSEDED_PROGRESS_FLUSH_INTERVAL,
                        );
                    }
                } else {
                    // Preserve canonical ordering at non-supersedable boundaries while collapsing
                    // high-frequency progress updates to their latest state.
                    if !flush_superseded_progress(&event_sender, &mut pending_progress).await
                        || event_sender
                            .send(SessionStreamUpdate::Event(Box::new(event)))
                            .await
                            .is_err()
                    {
                        return;
                    }
                }
                false
            }
            Err(_error) => true,
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
                        if event_sender
                            .send(SessionStreamUpdate::Resynchronized {
                                session_id,
                                attached: Box::new(attached),
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                        connection = next_connection;
                        reconnect_delay = std::time::Duration::from_millis(100);
                        break;
                    }
                }
                Err(_error) => {}
            }
            tokio::time::sleep(reconnect_delay).await;
            reconnect_delay = (reconnect_delay * 2).min(std::time::Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
