use super::*;
use crate::{ClientRuntimeContext, Response, ResponsePayload};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy)]
enum Reply {
    Pong,
    CloseFirst,
    PartialFirst,
    WrongIdFirst,
    EventFirst,
}

struct Server {
    client: BcodeClient,
    handshakes: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    admitted: Arc<Notify>,
    contexts: Arc<Mutex<Vec<Option<ClientRuntimeContext>>>>,
    replies: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
    directory: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        // The server owns its JoinSet, so abort also drops all accepted sockets.
        self.task.abort();
    }
}

impl Server {
    fn new(reply: Reply) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::Builder::new()
            .prefix("bcl-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("lane.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let handshakes = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let admitted = Arc::new(Notify::new());
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Semaphore::new(0));
        let task = {
            let handshakes = Arc::clone(&handshakes);
            let requests = Arc::clone(&requests);
            let admitted = Arc::clone(&admitted);
            let contexts = Arc::clone(&contexts);
            let replies = Arc::clone(&replies);
            tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        stream = listener.accept() => {
                            let mut stream = stream.unwrap();
                            let handshakes = Arc::clone(&handshakes);
                            let requests = Arc::clone(&requests);
                            let admitted = Arc::clone(&admitted);
                            let contexts = Arc::clone(&contexts);
                            let replies = Arc::clone(&replies);
                            connections.spawn(async move {
                                let hello = crate::recv_envelope(&mut stream).await.unwrap();
                                let Request::Hello { runtime_context, .. } = bcode_ipc::decode_request(&hello.payload).unwrap() else {
                                    panic!("first frame must be a handshake");
                                };
                                contexts.lock().unwrap().push(runtime_context);
                                let response = Response::Ok(ResponsePayload::Hello {
                                    protocol_version: bcode_ipc::ProtocolVersion::current(),
                                    client_id: bcode_session_models::ClientId::new(),
                                    daemon: crate::client_timeout_tests::matching_daemon_status(),
                                });
                                crate::send_envelope(
                                    &mut stream,
                                    &bcode_ipc::response_envelope(hello.request_id, &response).unwrap(),
                                ).await.unwrap();
                                handshakes.fetch_add(1, Ordering::SeqCst);
                                while let Ok(request) = crate::recv_envelope(&mut stream).await {
                                    let index = requests.fetch_add(1, Ordering::SeqCst);
                                    admitted.notify_one();
                                    if index == 0 {
                                        match reply {
                                            Reply::CloseFirst => break,
                                            Reply::PartialFirst => {
                                                use tokio::io::AsyncWriteExt as _;
                                                // A prefix cannot be resumed by a later request.
                                                // A deadline may close the peer before this task runs.
                                                if stream.write_all(&[0, 0]).await.is_err() {
                                                    break;
                                                }
                                            }
                                            Reply::Pong | Reply::WrongIdFirst | Reply::EventFirst => {}
                                        }
                                    }
                                    let Ok(permit) = replies.acquire().await else { break };
                                    permit.forget();
                                    let id = request.request_id
                                        + u64::from(index == 0 && matches!(reply, Reply::WrongIdFirst));
                                    let payload = match bcode_ipc::decode_request(&request.payload).unwrap() {
                                        Request::IngestClientMetrics { .. } => ResponsePayload::ClientMetricsIngested { accepted: 0 },
                                        _ => ResponsePayload::Pong,
                                    };
                                    let mut response = bcode_ipc::response_envelope(
                                        id, &Response::Ok(payload),
                                    ).unwrap();
                                    if index == 0 && matches!(reply, Reply::EventFirst) {
                                        response.kind = crate::EnvelopeKind::Event;
                                    }
                                    if crate::send_envelope(&mut stream, &response).await.is_err() {
                                        break;
                                    }
                                }
                            });
                        }
                        Some(result) = connections.join_next(), if !connections.is_empty() => {
                            result.unwrap();
                        }
                    }
                }
            })
        };
        Self {
            client: BcodeClient::new(endpoint).with_request_timeout(Duration::from_secs(2)),
            handshakes,
            requests,
            admitted,
            contexts,
            replies,
            task,
            directory,
        }
    }

    async fn wait_requests(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.requests.load(Ordering::SeqCst) < count {
                self.admitted.notified().await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn sequential_requests_reuse_handshake_and_clone_ownership_is_acyclic() {
    let server = Server::new(Reply::Pong);
    server.replies.add_permits(3);
    let sibling = server.client.clone();
    let weak = Arc::downgrade(&sibling.request_lanes);
    for client in [&server.client, &sibling, &server.client] {
        assert!(matches!(
            client.send_request(Request::Ping).await,
            Ok(ResponsePayload::Pong)
        ));
    }
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        sibling.request_lanes.interactive.idle.lock().unwrap().len(),
        1
    );
    drop(sibling);
    drop(server);
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn releasing_idle_connections_from_clone_preserves_inflight_request() {
    let server = Server::new(Reply::Pong);
    server.replies.add_permits(1);
    server.client.send_request(Request::Ping).await.unwrap();
    let sibling = server.client.clone();
    sibling.release_idle_connections();
    let request = server.client.send_request(Request::Ping);
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut request)
            .await
            .is_err()
    );
    sibling.release_idle_connections();
    server.replies.add_permits(1);
    assert!(matches!(request.await, Ok(ResponsePayload::Pong)));
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 2);
    sibling.release_idle_connections();
    server.replies.add_permits(1);
    assert!(matches!(
        server.client.send_request(Request::Ping).await,
        Ok(ResponsePayload::Pong)
    ));
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn simultaneous_requests_are_bounded_and_background_cannot_take_interactive_capacity() {
    let server = Server::new(Reply::Pong);
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..(INTERACTIVE_CAPACITY * 2) {
        let client = server.client.clone();
        requests.spawn(async move { client.send_request(Request::Ping).await });
    }
    server.wait_requests(INTERACTIVE_CAPACITY).await;
    assert_eq!(
        server.handshakes.load(Ordering::SeqCst),
        INTERACTIVE_CAPACITY
    );
    // Occupy background independently: no interactive permit is available.
    let background = &server.client.request_lanes.background;
    let background_permit = background.capacity.acquire().await.unwrap();
    assert_eq!(background.capacity.available_permits(), 0);
    assert_eq!(
        server
            .client
            .request_lanes
            .interactive
            .capacity
            .available_permits(),
        0
    );
    drop(background_permit);
    server.replies.add_permits(INTERACTIVE_CAPACITY * 2);
    while let Some(result) = requests.join_next().await {
        assert!(result.unwrap().is_ok());
    }
    assert_eq!(
        server.handshakes.load(Ordering::SeqCst),
        INTERACTIVE_CAPACITY
    );
    assert_eq!(
        server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .len(),
        INTERACTIVE_CAPACITY
    );
}

#[tokio::test]
async fn context_builders_do_not_contaminate_siblings() {
    let server = Server::new(Reply::Pong);
    server.replies.add_permits(3);
    server.client.send_request(Request::Ping).await.unwrap();
    let context = ClientRuntimeContext {
        selected_model_id: Some("lane-test-model".into()),
        selected_provider_plugin_id: Some("lane-test-provider".into()),
        effective_config_toml: Some(Box::new("test = true".into())),
        working_directory: Some(server.directory.path().to_owned()),
        ..ClientRuntimeContext::default()
    };
    let changed = server
        .client
        .clone()
        .with_runtime_context(Some(context.clone()));
    changed.send_request(Request::Ping).await.unwrap();
    server.client.send_request(Request::Ping).await.unwrap();
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 2);
    assert!(!Arc::ptr_eq(
        &changed.request_lanes,
        &server.client.request_lanes
    ));
    assert!(server.client.runtime_context.is_none());
    assert!(changed.runtime_context.is_some());
    let contexts = server.contexts.lock().unwrap().clone();
    assert_eq!(contexts, vec![None, Some(context)]);
    for changed in [
        server
            .client
            .clone()
            .with_interaction_controller_versions(std::collections::BTreeSet::default()),
        server.client.clone().with_interaction_adapters(Vec::new()),
    ] {
        assert!(!Arc::ptr_eq(
            &changed.request_lanes,
            &server.client.request_lanes
        ));
    }
}

#[tokio::test]
async fn cancelled_request_discards_connection_and_does_not_replay() {
    let server = Server::new(Reply::Pong);
    let client = server.client.clone();
    let request = tokio::spawn(async move { client.send_request(Request::Ping).await });
    server.wait_requests(1).await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(
        server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .is_empty()
    );
    server.replies.add_permits(2);
    server.client.send_request(Request::Ping).await.unwrap();
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 2);
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn timeout_in_partial_frame_discards_connection() {
    let server = Server::new(Reply::PartialFirst);
    // Complete the verified handshake outside the intentionally short response
    // deadline; parallel test load must not turn this into a handshake timeout.
    let mut connection = server
        .client
        .connect("partial-response-fixture")
        .await
        .unwrap();
    connection.reconnect_client = None;
    server
        .client
        .request_lanes
        .interactive
        .idle
        .lock()
        .unwrap()
        .push(IdleConnection {
            connection,
            returned: tokio::time::Instant::now(),
        });
    let result = server
        .client
        .send_request_with_timeout(Request::Ping, Duration::from_millis(50))
        .await;
    assert!(matches!(result, Err(ClientError::RequestTimeout { .. })));
    assert!(
        server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .is_empty()
    );
    server.replies.add_permits(2);
    server.client.send_request(Request::Ping).await.unwrap();
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 2);
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn disconnect_and_wrong_response_never_replay_and_next_call_reconnects() {
    for reply in [Reply::CloseFirst, Reply::WrongIdFirst, Reply::EventFirst] {
        let server = Server::new(reply);
        server.replies.add_permits(2);
        assert!(server.client.send_request(Request::Ping).await.is_err());
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        server.client.send_request(Request::Ping).await.unwrap();
        assert_eq!(server.handshakes.load(Ordering::SeqCst), 2);
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn idle_expiration_rehandshakes_without_maintenance_tasks() {
    let server = Server::new(Reply::Pong);
    server.replies.add_permits(2);
    server.client.send_request(Request::Ping).await.unwrap();
    // Age an idle connection without advancing transport/request deadlines.
    server.client.request_lanes.interactive.idle.lock().unwrap()[0].returned =
        tokio::time::Instant::now() - MAX_IDLE;
    server.client.send_request(Request::Ping).await.unwrap();
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 2);
    assert_eq!(
        server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn dedicated_requests_do_not_inherit_pool_client_identity() {
    let server = Server::new(Reply::Pong);
    server.replies.add_permits(4);
    server.client.send_request(Request::Ping).await.unwrap();
    // Mutating lifecycle requests deliberately remain dedicated even on a warm pool.
    for _ in 0..2 {
        server
            .client
            .send_request(Request::DeleteSession {
                session_id: bcode_session_models::SessionId::new(),
            })
            .await
            .unwrap();
    }
    server.client.send_request(Request::Ping).await.unwrap();
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn metrics_ingest_reuses_background_connection_while_interactive_is_saturated() {
    let server = Server::new(Reply::Pong);
    let _interactive = server
        .client
        .request_lanes
        .interactive
        .capacity
        .acquire_many(u32::try_from(INTERACTIVE_CAPACITY).unwrap())
        .await
        .unwrap();
    server.replies.add_permits(2);
    for _ in 0..2 {
        assert_eq!(
            server
                .client
                .ingest_client_metrics(bcode_metrics::ClientMetricBatch::default())
                .await
                .unwrap(),
            0
        );
    }
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        server
            .client
            .request_lanes
            .background
            .idle
            .lock()
            .unwrap()
            .len(),
        1
    );
    assert!(
        server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn waiting_for_capacity_obeys_deadline_without_sending_or_discarding_idle_state() {
    let server = Server::new(Reply::Pong);
    server.replies.add_permits(1);
    server.client.send_request(Request::Ping).await.unwrap();
    let _capacity = server
        .client
        .request_lanes
        .interactive
        .capacity
        .acquire_many(u32::try_from(INTERACTIVE_CAPACITY).unwrap())
        .await
        .unwrap();
    let result = server
        .client
        .send_request_with_timeout(Request::Ping, Duration::from_millis(10))
        .await;
    assert!(matches!(result, Err(ClientError::RequestTimeout { .. })));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn lost_idle_transport_is_not_replayed_but_subsequent_call_reconnects() {
    let server = Server::new(Reply::Pong);
    server.replies.add_permits(1);
    server.client.send_request(Request::Ping).await.unwrap();
    // Simulate a dead daemon transport while retaining the verified idle connection.
    {
        use tokio::io::AsyncWriteExt as _;
        let mut idle = server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .pop()
            .unwrap();
        idle.connection.stream.shutdown().await.unwrap();
        server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .push(idle);
    }
    assert!(server.client.send_request(Request::Ping).await.is_err());
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 1);
    server.replies.add_permits(1);
    server.client.send_request(Request::Ping).await.unwrap();
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn ambiguous_dedicated_mutation_is_not_replayed() {
    let server = Server::new(Reply::CloseFirst);
    assert!(
        server
            .client
            .send_request(Request::DeleteSession {
                session_id: bcode_session_models::SessionId::new(),
            })
            .await
            .is_err()
    );
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(server.handshakes.load(Ordering::SeqCst), 1);
    assert!(
        server
            .client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reused_connection_rechecks_state_and_artifact_identity_before_sending() {
    for change_artifact in [false, true] {
        let server = Server::new(Reply::Pong);
        server.replies.add_permits(1);
        server.client.send_request(Request::Ping).await.unwrap();
        {
            let mut idle = server.client.request_lanes.interactive.idle.lock().unwrap();
            let daemon = idle[0].connection.verified_daemon.as_mut().unwrap();
            if change_artifact {
                daemon.artifact_id = None;
            } else {
                daemon.state_location_id = Some("different-state".into());
            }
            drop(idle);
        }
        assert!(matches!(
            server.client.send_request(Request::Ping).await,
            Err(ClientError::IncompatibleDaemon { .. })
        ));
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        assert_eq!(server.handshakes.load(Ordering::SeqCst), 1);
        assert!(
            server
                .client
                .request_lanes
                .interactive
                .idle
                .lock()
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn verified_status_disconnects_before_idle_only_stop() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::Builder::new()
        .prefix("bcstop-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("stop.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let client = BcodeClient::new(endpoint);
    let server = tokio::spawn(async move {
        for status_request in [true, false] {
            let mut stream = listener.accept().await.unwrap();
            let hello = crate::recv_envelope(&mut stream).await.unwrap();
            let daemon = crate::client_timeout_tests::matching_daemon_status();
            let response = Response::Ok(ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon: daemon.clone(),
            });
            crate::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(hello.request_id, &response).unwrap(),
            )
            .await
            .unwrap();
            let request = crate::recv_envelope(&mut stream).await.unwrap();
            let request_payload = bcode_ipc::decode_request(&request.payload).unwrap();
            let payload = if status_request {
                assert!(matches!(request_payload, Request::ServerStatus { .. }));
                let status = serde_json::from_value(serde_json::json!({
                    "connected_client_count": 1,
                    "sessions": [],
                    "daemon": daemon,
                }))
                .unwrap();
                ResponsePayload::ServerStatus { status }
            } else {
                assert!(matches!(
                    request_payload,
                    Request::ServerStop {
                        mode: crate::ServerStopMode::IfIdle
                    }
                ));
                ResponsePayload::ServerStopping
            };
            crate::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(request.request_id, &Response::Ok(payload)).unwrap(),
            )
            .await
            .unwrap();
            // Do not admit the stop connection until the status socket actually
            // disconnects. A retained status lane fails this behavioral check.
            assert!(
                tokio::time::timeout(Duration::from_secs(5), crate::recv_envelope(&mut stream))
                    .await
                    .expect("lifecycle request retained an idle connection")
                    .is_err()
            );
        }
    });
    client.verified_server_status().await.unwrap();
    assert!(
        client
            .request_lanes
            .interactive
            .idle
            .lock()
            .unwrap()
            .is_empty()
    );
    client.server_stop_if_idle().await.unwrap();
    server.await.unwrap();
}
