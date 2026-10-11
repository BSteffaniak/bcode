//! Transport-level coverage for exact-turn observation.

use super::*;
use bcode_session_models::{ModelTurnOutcome, SessionEventKind};

async fn watcher() -> (SessionWatcher, LocalIpcStream, std::path::PathBuf) {
    let directory = std::path::PathBuf::from(format!("/tmp/bcw-{}", SessionId::new()));
    std::fs::create_dir_all(&directory).expect("socket directory");
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.join("client.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
    let stream = LocalIpcStream::connect(&endpoint).await.expect("connect");
    let peer = listener.accept().await.expect("accept");
    let connection = ClientConnection {
        stream,
        next_request_id: 1,
        client_id: None,
        pending_events: VecDeque::new(),
        request_timeout: Duration::from_secs(1),
        reconnect_client: None,
        reconnect_name: std::sync::Arc::from(""),
        restore_state: ConnectionRestoreState::default(),
    };
    (
        SessionWatcher {
            connection,
            session_id: SessionId::new(),
            initial: None,
        },
        peer,
        directory,
    )
}

fn finished(session_id: SessionId, turn_id: &str, outcome: ModelTurnOutcome) -> SessionEvent {
    SessionEvent {
        schema_version: 1,
        sequence: 1,
        timestamp_ms: 0,
        session_id,
        provenance: None,
        kind: SessionEventKind::ModelTurnFinished {
            turn_id: turn_id.to_owned(),
            outcome,
            message: None,
        },
    }
}

async fn send(peer: &mut LocalIpcStream, event: Event) {
    let envelope = bcode_ipc::event_envelope(&event).expect("encode event");
    bcode_ipc::send_envelope(peer, &envelope)
        .await
        .expect("send event");
}

#[tokio::test]
async fn recovery_after_timeout_ignores_late_response_and_preserves_terminal_event() {
    let (mut watcher, mut peer, directory) = watcher().await;
    let session_id = watcher.session_id;
    let (read_sent, read_received) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let first = bcode_ipc::recv_envelope(&mut peer).await.unwrap();
        read_sent.send(()).unwrap();
        resumed.await.unwrap();
        let second = bcode_ipc::recv_envelope(&mut peer).await.unwrap();
        assert_ne!(first.request_id, second.request_id);
        for request in [&first, &second] {
            assert!(
                matches!(bcode_ipc::decode_request(&request.payload).unwrap(),
                Request::SessionHistoryPage { session_id: id, .. } if id == session_id)
            );
        }
        // A stale response must not satisfy the new request. A terminal event
        // arriving between responses must survive response correlation.
        let stale =
            bcode_ipc::Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled: true });
        bcode_ipc::send_envelope(
            &mut peer,
            &bcode_ipc::response_envelope(first.request_id, &stale).unwrap(),
        )
        .await
        .unwrap();
        send(
            &mut peer,
            Event::Session(finished(session_id, "wanted", ModelTurnOutcome::Cancelled)),
        )
        .await;
        let response = bcode_ipc::Response::Ok(ResponsePayload::SessionHistoryPage {
            page: bcode_session_models::SessionHistoryPage {
                session_id,
                events: Vec::new(),
                compatibility_issues: Vec::new(),
                next_cursor: None,
                has_more: false,
            },
        });
        bcode_ipc::send_envelope(
            &mut peer,
            &bcode_ipc::response_envelope(second.request_id, &response).unwrap(),
        )
        .await
        .unwrap();
    });
    let timeout = Duration::from_millis(50);
    let result = watcher
        .recover_and_wait_for_turn_outcome("wanted", timeout)
        .await;
    read_received.await.unwrap();
    assert!(
        matches!(result, Err(ClientError::RequestTimeout { timeout: actual }) if actual == timeout)
    );
    resume.send(()).unwrap();
    assert_eq!(
        watcher
            .recover_and_wait_for_turn_outcome("wanted", Duration::from_secs(1))
            .await
            .unwrap(),
        ModelTurnOutcome::Cancelled
    );
    server.await.unwrap();
    drop(watcher);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn canonical_recovery_is_bounded_and_checks_session_identity() {
    for wrong_session in [false, true] {
        let (mut watcher, mut peer, directory) = watcher().await;
        let session_id = watcher.session_id;
        let server = tokio::spawn(async move {
            let request = bcode_ipc::recv_envelope(&mut peer).await.unwrap();
            assert_eq!(
                bcode_ipc::decode_request(&request.payload).unwrap(),
                Request::SessionHistoryPage {
                    session_id,
                    query: SessionHistoryQuery {
                        cursor: None,
                        limit: bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS,
                        direction: bcode_session_models::SessionHistoryDirection::Backward,
                    },
                }
            );
            let response = bcode_ipc::Response::Ok(ResponsePayload::SessionHistoryPage {
                page: bcode_session_models::SessionHistoryPage {
                    session_id: if wrong_session {
                        SessionId::new()
                    } else {
                        session_id
                    },
                    events: vec![
                        finished(SessionId::new(), "wanted", ModelTurnOutcome::Completed),
                        finished(session_id, "other", ModelTurnOutcome::Completed),
                        finished(session_id, "wanted", ModelTurnOutcome::Cancelled),
                    ],
                    compatibility_issues: Vec::new(),
                    next_cursor: None,
                    has_more: false,
                },
            });
            bcode_ipc::send_envelope(
                &mut peer,
                &bcode_ipc::response_envelope(request.request_id, &response).unwrap(),
            )
            .await
            .unwrap();
        });
        let outcome = watcher
            .recover_and_wait_for_turn_outcome("wanted", Duration::from_secs(1))
            .await;
        if wrong_session {
            assert!(matches!(outcome, Err(ClientError::Protocol(_))));
        } else {
            assert_eq!(outcome.unwrap(), ModelTurnOutcome::Cancelled);
        }
        server.await.unwrap();
        drop(watcher);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn canonical_recovery_preserves_live_events_arriving_before_history_response() {
    let (mut watcher, mut peer, directory) = watcher().await;
    let session_id = watcher.session_id;
    let server = tokio::spawn(async move {
        let request = bcode_ipc::recv_envelope(&mut peer).await.unwrap();
        send(
            &mut peer,
            Event::Session(finished(session_id, "wanted", ModelTurnOutcome::Completed)),
        )
        .await;
        let response = bcode_ipc::Response::Ok(ResponsePayload::SessionHistoryPage {
            page: bcode_session_models::SessionHistoryPage {
                session_id,
                events: Vec::new(),
                compatibility_issues: Vec::new(),
                next_cursor: Some(bcode_session_models::SessionHistoryCursor { sequence: 3 }),
                has_more: true,
            },
        });
        bcode_ipc::send_envelope(
            &mut peer,
            &bcode_ipc::response_envelope(request.request_id, &response).unwrap(),
        )
        .await
        .unwrap();
    });
    assert_eq!(
        watcher
            .recover_and_wait_for_turn_outcome("wanted", Duration::from_secs(1))
            .await
            .unwrap(),
        ModelTurnOutcome::Completed
    );
    server.await.unwrap();
    drop(watcher);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn canonical_recovery_read_failure_and_deadline_do_not_request_effects() {
    for close_peer in [false, true] {
        let (mut watcher, mut peer, directory) = watcher().await;
        let server = tokio::spawn(async move {
            let request = bcode_ipc::recv_envelope(&mut peer).await.unwrap();
            assert!(matches!(
                bcode_ipc::decode_request(&request.payload).unwrap(),
                Request::SessionHistoryPage { .. }
            ));
            if close_peer {
                return;
            }
            // Keep the read unanswered until the observer drops its connection.
            assert!(bcode_ipc::recv_envelope(&mut peer).await.is_err());
        });
        let result = watcher
            .recover_and_wait_for_turn_outcome("wanted", Duration::from_millis(50))
            .await;
        if close_peer {
            assert!(result.is_err());
        } else {
            assert!(
                matches!(result, Err(ClientError::RequestTimeout { timeout }) if timeout == Duration::from_millis(50))
            );
        }
        drop(watcher);
        server.await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn bounded_initial_history_returns_terminal_without_transport_work() {
    let (mut watcher, mut peer, directory) = watcher().await;
    let session_id = watcher.session_id;
    watcher.initial = Some(AttachedSessionHistory {
        session: SessionSummary {
            id: session_id,
            parent_id: None,
            name: None,
            explicit_name: None,
            automatic_title: None,
            automatic_title_source_sequence: None,
            derived_title: None,
            automatic_title: None,
            automatic_title_source_sequence: None,
            title_source: bcode_session_models::SessionTitleSource::Explicit,
            client_count: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            working_directory: directory.clone(),
            import: None,
            execution: None,
            location: None,
        },
        history: vec![
            finished(SessionId::new(), "wanted", ModelTurnOutcome::Completed),
            finished(session_id, "other", ModelTurnOutcome::Completed),
            finished(session_id, "wanted", ModelTurnOutcome::Cancelled),
        ],
        input_history: Vec::new(),
        submissions: Vec::new(),
        usage_summary: bcode_session_models::SessionUsageSummary::default(),
        import_warnings: Vec::new(),
        draft: None,
        runtime_selection: bcode_session_models::SessionRuntimeSelection::default(),
        projection_window: None,
    });
    assert_eq!(
        watcher
            .wait_for_turn_outcome("wanted", Duration::from_secs(1))
            .await
            .expect("initial outcome"),
        ModelTurnOutcome::Cancelled
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            bcode_ipc::recv_envelope(&mut peer)
        )
        .await
        .is_err()
    );
    std::fs::remove_dir_all(directory).expect("cleanup");
}

#[tokio::test]
async fn live_terminal_outcomes_require_the_requested_turn() {
    let (mut watcher, mut peer, directory) = watcher().await;
    for outcome in [
        ModelTurnOutcome::Completed,
        ModelTurnOutcome::Cancelled,
        ModelTurnOutcome::Error,
        ModelTurnOutcome::IdleTimeout,
        ModelTurnOutcome::ToolRoundLimitReached,
        ModelTurnOutcome::ProviderUnavailable,
    ] {
        send(
            &mut peer,
            Event::RuntimeWork(finished(
                SessionId::new(),
                "wanted",
                ModelTurnOutcome::Completed,
            )),
        )
        .await;
        send(
            &mut peer,
            Event::Session(finished(
                watcher.session_id,
                "other",
                ModelTurnOutcome::Completed,
            )),
        )
        .await;
        send(
            &mut peer,
            Event::Session(finished(watcher.session_id, "wanted", outcome)),
        )
        .await;
        assert_eq!(
            watcher
                .wait_for_turn_outcome("wanted", Duration::from_secs(1))
                .await
                .expect("terminal outcome"),
            outcome
        );
    }
    std::fs::remove_dir_all(directory).expect("cleanup");
}

#[tokio::test]
async fn timeout_does_not_submit_cancel_or_prevent_later_observation() {
    let (mut watcher, mut peer, directory) = watcher().await;
    assert!(matches!(
        watcher
            .wait_for_turn_outcome("wanted", Duration::from_millis(10))
            .await,
        Err(ClientError::RequestTimeout { .. })
    ));
    // Observation must never send cancellation or replay requests to the daemon.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            bcode_ipc::recv_envelope(&mut peer)
        )
        .await
        .is_err()
    );
    send(
        &mut peer,
        Event::Session(finished(
            watcher.session_id,
            "wanted",
            ModelTurnOutcome::Completed,
        )),
    )
    .await;
    assert_eq!(
        watcher
            .wait_for_turn_outcome("wanted", Duration::from_secs(1))
            .await
            .expect("late outcome"),
        ModelTurnOutcome::Completed
    );
    std::fs::remove_dir_all(directory).expect("cleanup");
}

#[tokio::test]
async fn resync_fails_closed_only_for_the_observed_session() {
    let (mut watcher, mut peer, directory) = watcher().await;
    send(
        &mut peer,
        Event::SessionViewResyncRequired {
            session_id: SessionId::new(),
        },
    )
    .await;
    send(
        &mut peer,
        Event::SessionViewResyncRequired {
            session_id: watcher.session_id,
        },
    )
    .await;
    assert!(matches!(
        watcher
            .wait_for_turn_outcome("wanted", Duration::from_secs(1))
            .await,
        Err(ClientError::Protocol(_))
    ));
    std::fs::remove_dir_all(directory).expect("cleanup");
}
