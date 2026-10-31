use super::*;
use bcode_session_models::{SessionActivityWatchSnapshot, SessionApplication as _};
use tokio::sync::Semaphore;

struct Server {
    client: BcodeClient,
    handle: Handle,
    pulled: Arc<Semaphore>,
    disconnected: Arc<Semaphore>,
    replies: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
    directory: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    fn new(wrong_epoch: bool) -> Self {
        Self::with_open_gate(wrong_epoch, false)
    }

    fn with_open_gate(wrong_epoch: bool, block_open: bool) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::Builder::new()
            .prefix("bcaw-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("watch.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let handle = Handle {
            watch_id: SessionId::new(),
            epoch: SessionId::new(),
        };
        let pulled = Arc::new(Semaphore::new(0));
        let disconnected = Arc::new(Semaphore::new(0));
        let replies = Arc::new(Semaphore::new(0));
        let task = {
            let pulled = Arc::clone(&pulled);
            let disconnected = Arc::clone(&disconnected);
            let replies = Arc::clone(&replies);
            tokio::spawn(async move {
                let mut stream = listener.accept().await.unwrap();
                let hello = crate::recv_envelope(&mut stream).await.unwrap();
                respond(
                    &mut stream,
                    hello.request_id,
                    crate::ResponsePayload::Hello {
                        protocol_version: bcode_ipc::ProtocolVersion::current(),
                        client_id: bcode_session_models::ClientId::new(),
                        daemon: crate::client_timeout_tests::matching_daemon_status(),
                    },
                )
                .await;
                let mut sequence = 0;
                while let Ok(envelope) = crate::recv_envelope(&mut stream).await {
                    let payload = match bcode_ipc::decode_request(&envelope.payload).unwrap() {
                        crate::Request::OpenSessionActivityWatch { .. } => {
                            if block_open {
                                pulled.add_permits(1);
                                tokio::select! {
                                    permit = replies.acquire() => permit.unwrap().forget(),
                                    result = crate::recv_envelope(&mut stream) => {
                                        assert!(result.is_err());
                                        disconnected.add_permits(1);
                                        return;
                                    }
                                }
                            }
                            crate::ResponsePayload::SessionActivityWatchOpened { handle }
                        }
                        crate::Request::PullSessionActivityWatch { handle: requested } => {
                            assert_eq!(requested, handle);
                            pulled.add_permits(1);
                            // Detect cancellation/disconnect while a long pull is outstanding.
                            tokio::select! {
                                permit = replies.acquire() => permit.unwrap().forget(),
                                result = crate::recv_envelope(&mut stream) => {
                                    assert!(result.is_err(), "no second request may race an outstanding pull");
                                    disconnected.add_permits(1);
                                    return;
                                }
                            }
                            let payload = delivery_payload(handle, sequence, wrong_epoch);
                            sequence += 1;
                            payload
                        }
                        crate::Request::CloseSessionActivityWatch { handle: requested } => {
                            assert_eq!(requested, handle);
                            crate::ResponsePayload::SessionActivityWatchClosed { closed: true }
                        }
                        _ => panic!("watch connection must remain dedicated"),
                    };
                    respond(&mut stream, envelope.request_id, payload).await;
                }
                disconnected.add_permits(1);
            })
        };
        Self {
            client: BcodeClient::new(endpoint)
                .with_request_timeout(std::time::Duration::from_secs(10)),
            handle,
            pulled,
            disconnected,
            replies,
            task,
            directory,
        }
    }

    async fn open(&self) -> Handle {
        self.client
            .open_activity_watch(Scope {
                activity: Vec::new(),
                children: Vec::new(),
            })
            .await
            .unwrap()
    }

    async fn wait_pulled(&self) {
        // Counted stage signals cannot be lost if the server reaches the gate
        // before this waiter starts. The watchdog includes identity verification
        // under concurrent test load; it is not an application timeout assertion.
        tokio::time::timeout(std::time::Duration::from_secs(10), self.pulled.acquire())
            .await
            .expect("fixture did not receive the gated request")
            .unwrap()
            .forget();
    }

    async fn wait_disconnected(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.disconnected.acquire(),
        )
        .await
        .expect("fixture did not observe peer closure")
        .unwrap()
        .forget();
    }
}

fn delivery_payload(handle: Handle, sequence: u64, wrong_epoch: bool) -> crate::ResponsePayload {
    let delivered = if wrong_epoch {
        Handle {
            epoch: SessionId::new(),
            ..handle
        }
    } else {
        handle
    };
    let delivery = Delivery {
        handle: delivered,
        sequence,
        update: if sequence == 0 {
            Update::Initial
        } else {
            Update::Changed
        },
        snapshot: SessionActivityWatchSnapshot {
            activity: Vec::new(),
            children: Vec::new(),
        },
    };
    crate::ResponsePayload::SessionActivityWatchUpdated {
        delivery: Box::new(delivery),
    }
}

async fn respond(stream: &mut bcode_ipc::LocalIpcStream, id: u64, payload: crate::ResponsePayload) {
    crate::send_envelope(
        stream,
        &bcode_ipc::response_envelope(id, &crate::Response::Ok(payload)).unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn open_pull_and_close_retain_exact_connection_without_client_cycles() {
    let server = Server::new(false);
    let handle = server.open().await;
    assert_eq!(handle, server.handle);
    let weak = Arc::downgrade(&server.client.activity_watches);
    server.replies.add_permits(2);
    for sequence in 0..2 {
        let delivery = server
            .client
            .clone()
            .pull_activity_watch(handle)
            .await
            .unwrap();
        assert_eq!(delivery.sequence, sequence);
        assert_eq!(delivery.handle, handle);
    }
    assert!(server.client.close_activity_watch(handle).await.unwrap());
    assert!(!server.client.close_activity_watch(handle).await.unwrap());
    server.wait_disconnected().await;
    assert!(
        server
            .client
            .activity_watches
            .entries
            .lock()
            .unwrap()
            .is_empty()
    );
    drop(server);
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn cancelling_pull_removes_handle_and_disconnects_without_replay() {
    let server = Server::new(false);
    let handle = server.open().await;
    let client = server.client.clone();
    let pull = tokio::spawn(async move { client.pull_activity_watch(handle).await });
    server.wait_pulled().await;
    pull.abort();
    assert!(pull.await.unwrap_err().is_cancelled());
    server.wait_disconnected().await;
    assert_eq!(
        server
            .client
            .pull_activity_watch(handle)
            .await
            .unwrap_err()
            .code,
        "activity_watch_closed"
    );
    assert!(!server.client.close_activity_watch(handle).await.unwrap());
}

#[tokio::test]
async fn close_interrupts_outstanding_pull_and_concurrent_pull_is_rejected() {
    let server = Server::new(false);
    let handle = server.open().await;
    let client = server.client.clone();
    let pull = tokio::spawn(async move { client.pull_activity_watch(handle).await });
    server.wait_pulled().await;
    assert_eq!(
        server
            .client
            .pull_activity_watch(handle)
            .await
            .unwrap_err()
            .code,
        "activity_watch_busy"
    );
    assert!(server.client.close_activity_watch(handle).await.unwrap());
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), pull)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err().code, "activity_watch_closed");
    server.wait_disconnected().await;
}

#[tokio::test]
async fn foreign_context_and_epoch_cannot_use_or_close_handle() {
    let server = Server::new(false);
    let handle = server.open().await;
    let foreign = server.client.clone().with_runtime_context(None);
    assert_eq!(
        foreign.pull_activity_watch(handle).await.unwrap_err().code,
        "activity_watch_closed"
    );
    assert!(!foreign.close_activity_watch(handle).await.unwrap());
    let wrong_epoch = Handle {
        epoch: SessionId::new(),
        ..handle
    };
    assert!(
        !server
            .client
            .close_activity_watch(wrong_epoch)
            .await
            .unwrap()
    );
    server.replies.add_permits(1);
    assert!(server.client.pull_activity_watch(handle).await.is_ok());
    assert!(server.client.close_activity_watch(handle).await.unwrap());
}

#[tokio::test]
async fn invalid_delivery_discards_connection_and_handle() {
    let server = Server::new(true);
    let handle = server.open().await;
    server.replies.add_permits(1);
    assert_eq!(
        server
            .client
            .pull_activity_watch(handle)
            .await
            .unwrap_err()
            .code,
        "activity_watch_invalid_delivery"
    );
    server.wait_disconnected().await;
    assert!(!server.client.close_activity_watch(handle).await.unwrap());
}

#[test]
fn pending_open_reservations_are_bounded_and_cancelled_by_drop() {
    let registry = Arc::new(ActivityWatches::default());
    let leases: Vec<_> = (0..MAX_WATCHES)
        .map(|_| registry.reserve().unwrap())
        .collect();
    assert_eq!(
        registry.reserve().err().unwrap().code,
        "activity_watch_capacity"
    );
    drop(leases);
    assert!(registry.entries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dropping_last_client_disconnects_idle_watch() {
    let mut server = Server::new(false);
    let handle = server.open().await;
    let original = std::mem::replace(
        &mut server.client,
        BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(
            server.directory.path().join("unused.sock"),
        )),
    );
    let weak = Arc::downgrade(&original.activity_watches);
    drop(original);
    server.wait_disconnected().await;
    assert!(weak.upgrade().is_none());
    assert!(!server.client.close_activity_watch(handle).await.unwrap());
}

#[tokio::test]
async fn cancelled_open_discards_connection_and_pending_reservation() {
    let server = Server::with_open_gate(false, true);
    let client = server.client.clone();
    let open = tokio::spawn(async move {
        client
            .open_activity_watch(Scope {
                activity: Vec::new(),
                children: Vec::new(),
            })
            .await
    });
    server.wait_pulled().await;
    open.abort();
    assert!(open.await.unwrap_err().is_cancelled());
    server.wait_disconnected().await;
    assert!(
        server
            .client
            .activity_watches
            .entries
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn daemon_disconnect_ends_watch_without_automatic_reopen() {
    let server = Server::new(false);
    let handle = server.open().await;
    let client = server.client.clone();
    let pull = tokio::spawn(async move { client.pull_activity_watch(handle).await });
    server.wait_pulled().await;
    server.task.abort();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), pull)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert!(
        server
            .client
            .activity_watches
            .entries
            .lock()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        server
            .client
            .pull_activity_watch(handle)
            .await
            .unwrap_err()
            .code,
        "activity_watch_closed"
    );
}

#[tokio::test]
async fn unchanged_watch_long_pull_does_not_use_application_response_timeout() {
    let server = Server::new(false);
    let handle = server.open().await;
    {
        let mut entries = server.client.activity_watches.entries.lock().unwrap();
        let entry = entries.values_mut().next().unwrap();
        entry.connection.as_mut().unwrap().request_timeout = std::time::Duration::from_millis(5);
        drop(entries);
    }
    let client = server.client.clone();
    let pull = tokio::spawn(async move { client.pull_activity_watch(handle).await });
    server.wait_pulled().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!pull.is_finished());
    server.replies.add_permits(1);
    assert!(pull.await.unwrap().is_ok());
    assert!(server.client.close_activity_watch(handle).await.unwrap());
}
