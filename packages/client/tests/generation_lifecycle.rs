//! Behavioral coverage of the production structured-generation client over local IPC.
#![cfg(unix)]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_client::BcodeClient;
use bcode_ipc::{Request, Response, ResponsePayload};
use bcode_plugin_sdk::generation::{
    PluginStructuredGenerationControl, PluginStructuredGenerationRequest,
};
use bcode_session_models::SessionId;
use std::time::Duration;

struct Fixture {
    client: BcodeClient,
    history: tokio::sync::mpsc::UnboundedReceiver<()>,
    cancellations: tokio::sync::mpsc::UnboundedReceiver<SessionId>,
    server: tokio::task::JoinHandle<()>,
    directory: std::path::PathBuf,
    session_id: SessionId,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        // Only the socket and this fixture's uniquely named empty directory are removed.
        let _ = std::fs::remove_file(self.directory.join("test.sock"));
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn matching_daemon_status() -> bcode_ipc::DaemonStatus {
    let (_, digest) = bcode_daemon_lifecycle::current_executable_identity().unwrap();
    bcode_ipc::DaemonStatus {
        namespace: bcode_ipc::daemon_namespace(),
        protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
        artifact_id: Some(bcode_ipc::ArtifactId::current()),
        build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.to_owned(),
        executable_digest: Some(digest),
        storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
        session_event_schema_version: Some(
            bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        ),
        state_location_id: Some(bcode_ipc::state_location_id()),
        ..bcode_ipc::DaemonStatus::default()
    }
}

fn fixture(fail_history: bool) -> Fixture {
    fixture_with_pending(fail_history, |_| false)
}

fn fixture_with_pending(fail_history: bool, hold: fn(&Request) -> bool) -> Fixture {
    let session_id = SessionId::new();
    let directory = std::path::PathBuf::from(format!("/tmp/bcg-{session_id}"));
    std::fs::create_dir(&directory).unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.join("test.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let (history_tx, history) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, cancellations) = tokio::sync::mpsc::unbounded_channel();
    let daemon = matching_daemon_status();
    let server = tokio::spawn(async move {
        // Keep history connections unanswered while continuing to accept cancellation.
        // No fake provider or product daemon is involved in this deterministic IPC fixture.
        let mut held_history = None;
        let mut held_requests = Vec::new();
        loop {
            let mut stream = listener.accept().await.unwrap();
            let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            let response = Response::Ok(ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon: daemon.clone(),
            });
            bcode_ipc::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(hello.request_id, &response).unwrap(),
            )
            .await
            .unwrap();
            let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            let request = bcode_ipc::decode_request(&envelope.payload).unwrap();
            if hold(&request) {
                if let Request::CancelSessionTurn { session_id, .. } = request {
                    cancel_tx.send(session_id).unwrap();
                }
                assert!(
                    held_requests.len() < 2,
                    "mutating requests must not be retried"
                );
                held_requests.push(stream);
                continue;
            }
            let payload = match request {
                Request::CreateSession { .. } => ResponsePayload::SessionCreated {
                    session: serde_json::from_value(serde_json::json!({
                        "id": session_id, "name": "generation regression",
                        "client_count": 0, "created_at_ms": 0, "updated_at_ms": 0
                    }))
                    .unwrap(),
                },
                Request::SendUserMessageWithExecution {
                    session_id: actual,
                    execution,
                    ..
                } => {
                    assert_eq!(actual, session_id);
                    assert_eq!(
                        execution.tools,
                        bcode_session_models::TurnToolPolicy::Disabled
                    );
                    assert!(execution.structured_output.is_some());
                    ResponsePayload::MessageAccepted {
                        queued: false,
                        queue_position: None,
                    }
                }
                Request::SessionHistoryPage {
                    session_id: actual, ..
                } => {
                    assert_eq!(actual, session_id);
                    history_tx.send(()).unwrap();
                    if !fail_history {
                        assert!(
                            held_history.replace(stream).is_none(),
                            "only one history request may be pending"
                        );
                    }
                    // Dropping the stream simulates a history transport failure after admission.
                    continue;
                }
                Request::CancelSessionTurn {
                    session_id: actual, ..
                } => {
                    cancel_tx.send(actual).unwrap();
                    ResponsePayload::TurnCancellationRequested { cancelled: true }
                }
                other => panic!("unexpected generation request: {other:?}"),
            };
            bcode_ipc::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(envelope.request_id, &Response::Ok(payload)).unwrap(),
            )
            .await
            .unwrap();
        }
    });
    Fixture {
        client: BcodeClient::new(endpoint),
        history,
        cancellations,
        server,
        directory,
        session_id,
    }
}

#[tokio::test]
async fn deadline_bounds_preparation_and_ambiguous_submission() {
    for preparation in [true, false] {
        let mut fixture = fixture_with_pending(
            false,
            if preparation {
                |request| matches!(request, Request::CreateSession { .. })
            } else {
                |request| matches!(request, Request::SendUserMessageWithExecution { .. })
            },
        );
        let control = PluginStructuredGenerationControl::default();
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            fixture
                .client
                .generate_structured_output(request(100), control.clone()),
        )
        .await
        .expect("deadline covers preparation and submission")
        .expect_err("pending admission cannot produce output");
        assert!(error.to_string().contains("timed out"), "{error}");
        if preparation {
            assert!(control.session_id().is_none());
            assert!(fixture.cancellations.try_recv().is_err());
        } else {
            assert_eq!(control.session_id(), Some(fixture.session_id));
            assert_eq!(
                fixture.cancellations.try_recv().ok(),
                Some(fixture.session_id)
            );
            assert!(
                error
                    .to_string()
                    .contains("terminal acknowledgement unobserved")
            );
        }
    }
}

#[tokio::test]
async fn failed_history_cleanup_is_bounded_and_preserves_failure() {
    let mut fixture = fixture_with_pending(true, |request| {
        matches!(request, Request::CancelSessionTurn { .. })
    });
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.client.generate_structured_output(
            request(10_000),
            PluginStructuredGenerationControl::default(),
        ),
    )
    .await
    .expect("cleanup has a separate bounded allowance")
    .expect_err("history failure must not produce output");
    assert_eq!(
        fixture.cancellations.try_recv().ok(),
        Some(fixture.session_id)
    );
    assert!(
        error
            .to_string()
            .contains("cancellation request timed out; turn outcome unresolved"),
        "{error}"
    );
}

fn request(timeout_ms: u64) -> PluginStructuredGenerationRequest {
    PluginStructuredGenerationRequest {
        source_session_id: None,
        session_name: "generation regression".into(),
        system_prompt: "Return JSON".into(),
        prompt: "Generate goal instructions".into(),
        output_name: "goal".into(),
        output_schema: serde_json::json!({"type":"object"}),
        timeout_ms,
    }
}

#[tokio::test]
async fn history_failure_cancels_admitted_generation() {
    let mut fixture = fixture(true);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        fixture.client.generate_structured_output(
            request(2_000),
            PluginStructuredGenerationControl::default(),
        ),
    )
    .await
    .expect("history failure must return");
    assert!(result.is_err(), "failed history must not produce a goal");
    fixture
        .history
        .recv()
        .await
        .expect("history was requested after admission");
    let cancellation =
        tokio::time::timeout(Duration::from_millis(500), fixture.cancellations.recv()).await;
    assert_eq!(
        cancellation.ok().flatten(),
        Some(fixture.session_id),
        "history failure must cancel the admitted generation session before abandoning observation"
    );
}

#[tokio::test]
async fn deadline_interrupts_pending_history() {
    let mut fixture = fixture(false);
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        fixture
            .client
            .generate_structured_output(request(100), PluginStructuredGenerationControl::default()),
    )
    .await;
    fixture
        .history
        .recv()
        .await
        .expect("history was requested after admission");
    assert!(
        result.is_ok(),
        "100ms generation deadline must interrupt pending history within the 1s safety bound"
    );
    let error = result
        .unwrap()
        .expect_err("deadline must not generate a goal");
    assert!(error.to_string().contains("timed out"), "{error}");
    assert_eq!(
        fixture.cancellations.try_recv().ok(),
        Some(fixture.session_id),
        "timeout must request cancellation before returning"
    );
}

#[tokio::test]
async fn cancellation_interrupts_pending_history() {
    let mut fixture = fixture(false);
    let control = PluginStructuredGenerationControl::default();
    let generation = fixture
        .client
        .generate_structured_output(request(10_000), control.clone());
    tokio::pin!(generation);
    tokio::select! {
        result = &mut generation => panic!("generation ended before history: {result:?}"),
        signal = fixture.history.recv() => signal.expect("history requested"),
    }
    assert_eq!(control.session_id(), Some(fixture.session_id));
    control.cancel();
    let cancellation = tokio::time::timeout(Duration::from_millis(500), async {
        tokio::select! {
            result = &mut generation => {
                assert!(result.is_err(), "cancelled generation must not yield a goal");
                fixture.cancellations.recv().await
            }
            session = fixture.cancellations.recv() => session,
        }
    })
    .await;
    assert_eq!(
        cancellation.ok().flatten(),
        Some(fixture.session_id),
        "cancellation must reach the admitted session even while history is pending"
    );
}
