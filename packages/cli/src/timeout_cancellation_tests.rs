//! IPC regression coverage for scripted timeout cancellation.

use super::*;

async fn hello_peer(listener: &bcode_ipc::LocalIpcListener) -> bcode_ipc::LocalIpcStream {
    let mut stream = listener.accept().await.unwrap();
    let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
    let daemon = bcode_ipc::DaemonStatus {
        namespace: bcode_ipc::daemon_namespace(),
        protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
        artifact_id: Some(bcode_ipc::ArtifactId::current()),
        executable_digest: Some(
            bcode_daemon_lifecycle::current_executable_identity()
                .unwrap()
                .1,
        ),
        build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
        storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
        session_event_schema_version: Some(
            bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        ),
        state_location_id: Some(bcode_ipc::state_location_id()),
        ..Default::default()
    };
    reply(
        &mut stream,
        hello.request_id,
        bcode_ipc::ResponsePayload::Hello {
            protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
            client_id: bcode_session_models::ClientId::new(),
            daemon,
        },
    )
    .await;
    stream
}

async fn reply(
    stream: &mut bcode_ipc::LocalIpcStream,
    id: u64,
    payload: bcode_ipc::ResponsePayload,
) {
    bcode_ipc::send_envelope(
        stream,
        &bcode_ipc::response_envelope(id, &bcode_ipc::Response::Ok(payload)).unwrap(),
    )
    .await
    .unwrap();
}

async fn attach_peer(watch: &mut bcode_ipc::LocalIpcStream, session_id: SessionId) {
    let attach = bcode_ipc::recv_envelope(watch).await.unwrap();
    assert!(
        matches!(bcode_ipc::decode_request(&attach.payload).unwrap(),
        bcode_ipc::Request::AttachSessionRecent { session_id: id, .. } if id == session_id)
    );
    let session = serde_json::from_value(serde_json::json!({
        "id": session_id, "name": null, "client_count": 0,
        "created_at_ms": 0, "updated_at_ms": 0, "working_directory": "/tmp",
    }))
    .unwrap();
    reply(
        watch,
        attach.request_id,
        bcode_ipc::ResponsePayload::Attached {
            session_id,
            session,
            history: Vec::new(),
            input_history: Vec::new(),
            usage_summary: None,
            import_warnings: Vec::new(),
            draft: None,
            runtime_selection: bcode_session_models::SessionRuntimeSelection::default(),
            projection_window: None,
        },
    )
    .await;
}

#[tokio::test]
async fn observation_failure_without_timeout_authority_performs_no_transport_work() {
    for timeout_error in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("n.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let session_id = SessionId::new();
        let server = tokio::spawn(async move {
            let mut watch = hello_peer(&listener).await;
            attach_peer(&mut watch, session_id).await;
            (listener, watch)
        });
        let client = BcodeClient::new(endpoint);
        let mut watcher = client.watch_session(session_id, 1).await.unwrap();
        let (listener, mut peer) = server.await.unwrap();
        let receipt = bcode_session_models::TurnReceipt::from_accepted_event(session_id, 17);
        let timeout = std::time::Duration::from_millis(20);
        let options = TurnObservationOptions {
            timeout,
            cancellation_grace: (!timeout_error).then_some(timeout),
        };
        let error = if timeout_error {
            ClientError::RequestTimeout { timeout }
        } else {
            ClientError::UnexpectedResponse
        };
        let result = report_observation_failure(
            &client,
            session_id,
            &receipt,
            Some(&options),
            error,
            &mut watcher,
        )
        .await;
        assert!(result.is_err());
        assert!(
            tokio::time::timeout(timeout, listener.accept())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(timeout, bcode_ipc::recv_envelope(&mut peer))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn cancellation_recovery_deadline_preserves_original_timeout_without_retry() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("d.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut watch = hello_peer(&listener).await;
        attach_peer(&mut watch, session_id).await;
        let mut cancel = hello_peer(&listener).await;
        let request = bcode_ipc::recv_envelope(&mut cancel).await.unwrap();
        reply(
            &mut cancel,
            request.request_id,
            bcode_ipc::ResponsePayload::TurnCancellationRequested { cancelled: true },
        )
        .await;
        let history = bcode_ipc::recv_envelope(&mut watch).await.unwrap();
        assert!(matches!(
            bcode_ipc::decode_request(&history.payload).unwrap(),
            bcode_ipc::Request::SessionHistoryPage { .. }
        ));
        // Keep the response unanswered without closing the original subscription.
        (listener, watch)
    });
    let client =
        BcodeClient::new(endpoint).with_request_timeout(std::time::Duration::from_secs(30));
    let mut watcher = client.watch_session(session_id, 1).await.unwrap();
    let receipt = bcode_session_models::TurnReceipt::from_accepted_event(session_id, 17);
    let original = std::time::Duration::from_secs(7);
    let options = TurnObservationOptions {
        timeout: original,
        // Include fresh cancellation-connection executable verification under load.
        // This remains shorter than the transport timeout, and history stays unanswered.
        cancellation_grace: Some(std::time::Duration::from_secs(5)),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        report_observation_failure(
            &client,
            session_id,
            &receipt,
            Some(&options),
            ClientError::RequestTimeout { timeout: original },
            &mut watcher,
        ),
    )
    .await
    .expect("recovery grace must expire before the 30-second transport timeout");
    assert!(
        matches!(result, Err(CliError::Client(ClientError::RequestTimeout { timeout })) if timeout == original)
    );
    let (listener, mut peer) = server.await.unwrap();
    let bound = std::time::Duration::from_millis(50);
    assert!(
        tokio::time::timeout(bound, listener.accept())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(bound, bcode_ipc::recv_envelope(&mut peer))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unanswered_cancellation_is_bounded_by_recovery_grace() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("grace.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut watch = hello_peer(&listener).await;
        attach_peer(&mut watch, session_id).await;
        let mut cancel = hello_peer(&listener).await;
        let request = bcode_ipc::recv_envelope(&mut cancel).await.unwrap();
        assert!(matches!(
            bcode_ipc::decode_request(&request.payload).unwrap(),
            bcode_ipc::Request::CancelSessionTurn {
                clear_queue: false,
                ..
            }
        ));
        (listener, watch, cancel)
    });
    let client =
        BcodeClient::new(endpoint).with_request_timeout(std::time::Duration::from_secs(30));
    let mut watcher = client.watch_session(session_id, 1).await.unwrap();
    let receipt = bcode_session_models::TurnReceipt::from_accepted_event(session_id, 17);
    let original = std::time::Duration::from_secs(7);
    let options = TurnObservationOptions {
        timeout: original,
        cancellation_grace: Some(std::time::Duration::from_millis(100)),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        report_observation_failure(
            &client,
            session_id,
            &receipt,
            Some(&options),
            ClientError::RequestTimeout { timeout: original },
            &mut watcher,
        ),
    )
    .await
    .expect("cancellation must not wait for the 30-second transport timeout");
    assert!(
        matches!(result, Err(CliError::Client(ClientError::RequestTimeout { timeout })) if timeout == original)
    );
    let (listener, mut watch, mut cancel) = server.await.unwrap();
    assert!(bcode_ipc::recv_envelope(&mut cancel).await.is_err());
    let bound = std::time::Duration::from_millis(50);
    assert!(
        tokio::time::timeout(bound, listener.accept())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(bound, bcode_ipc::recv_envelope(&mut watch))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn delayed_acknowledgement_does_not_restart_recovery_grace() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("shared.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut watch = hello_peer(&listener).await;
        attach_peer(&mut watch, session_id).await;
        let mut cancel = hello_peer(&listener).await;
        let request = bcode_ipc::recv_envelope(&mut cancel).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        reply(
            &mut cancel,
            request.request_id,
            bcode_ipc::ResponsePayload::TurnCancellationRequested { cancelled: true },
        )
        .await;
        let history = bcode_ipc::recv_envelope(&mut watch).await.unwrap();
        assert!(matches!(
            bcode_ipc::decode_request(&history.payload).unwrap(),
            bcode_ipc::Request::SessionHistoryPage { .. }
        ));
        (listener, watch)
    });
    let client =
        BcodeClient::new(endpoint).with_request_timeout(std::time::Duration::from_secs(30));
    let mut watcher = client.watch_session(session_id, 1).await.unwrap();
    let receipt = bcode_session_models::TurnReceipt::from_accepted_event(session_id, 17);
    let original = std::time::Duration::from_secs(7);
    let options = TurnObservationOptions {
        timeout: original,
        cancellation_grace: Some(std::time::Duration::from_secs(1)),
    };
    // A fresh recovery budget after the delayed acknowledgement would take at
    // least 1.6 seconds. The shared budget must finish before that, allowing
    // scheduler slack without treating acknowledgement as terminal success.
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(1400),
        report_observation_failure(
            &client,
            session_id,
            &receipt,
            Some(&options),
            ClientError::RequestTimeout { timeout: original },
            &mut watcher,
        ),
    )
    .await
    .expect("acknowledgement must consume the recovery budget");
    assert!(
        matches!(result, Err(CliError::Client(ClientError::RequestTimeout { timeout })) if timeout == original)
    );
    let (listener, mut watch) = server.await.unwrap();
    let bound = std::time::Duration::from_millis(50);
    assert!(
        tokio::time::timeout(bound, listener.accept())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(bound, bcode_ipc::recv_envelope(&mut watch))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn headless_permission_wait_fails_without_answering_it() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("permission.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut watch = hello_peer(&listener).await;
        attach_peer(&mut watch, session_id).await;
        let history = bcode_ipc::recv_envelope(&mut watch).await.unwrap();
        reply(
            &mut watch,
            history.request_id,
            bcode_ipc::ResponsePayload::SessionHistoryPage {
                page: bcode_session_models::SessionHistoryPage {
                    session_id,
                    events: Vec::new(),
                    compatibility_issues: Vec::new(),
                    next_cursor: None,
                    has_more: false,
                },
            },
        )
        .await;
        let mut permissions = hello_peer(&listener).await;
        let request = bcode_ipc::recv_envelope(&mut permissions).await.unwrap();
        assert!(matches!(
            bcode_ipc::decode_request(&request.payload).unwrap(),
            bcode_ipc::Request::ListPermissions
        ));
        let permission = serde_json::from_value(serde_json::json!({
            "permission_id": "checkpoint", "session_id": session_id,
            "tool_call_id": "tool", "tool_name": "test", "arguments_json": "{}", "agent_id": "build",
        })).unwrap();
        reply(
            &mut permissions,
            request.request_id,
            bcode_ipc::ResponsePayload::PermissionList {
                permissions: vec![permission],
            },
        )
        .await;
        (listener, watch)
    });
    let client = BcodeClient::new(endpoint);
    let mut watcher = client.watch_session(session_id, 1).await.unwrap();
    let receipt = bcode_session_models::TurnReceipt::from_accepted_event(session_id, 17);
    let result = headless::observe(
        &client,
        &mut watcher,
        session_id,
        &receipt,
        std::time::Duration::from_secs(1),
    )
    .await;
    assert!(
        matches!(result, Err(ClientError::Protocol(message)) if message.contains("requires permission"))
    );
    let (listener, _watch) = server.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn headless_observes_authoritative_success_and_failure() {
    use bcode_session_models::{ModelTurnOutcome, SessionEvent, SessionEventKind, TurnReceipt};
    for outcome in [ModelTurnOutcome::Completed, ModelTurnOutcome::Cancelled] {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("terminal.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let session_id = SessionId::new();
        let receipt = TurnReceipt::from_accepted_event(session_id, 17);
        let expected = receipt.clone();
        let server = tokio::spawn(async move {
            let mut watch = hello_peer(&listener).await;
            attach_peer(&mut watch, session_id).await;
            let history = bcode_ipc::recv_envelope(&mut watch).await.unwrap();
            assert!(matches!(
                bcode_ipc::decode_request(&history.payload).unwrap(),
                bcode_ipc::Request::SessionHistoryPage { .. }
            ));
            reply(
                &mut watch,
                history.request_id,
                bcode_ipc::ResponsePayload::SessionHistoryPage {
                    page: bcode_session_models::SessionHistoryPage {
                        session_id,
                        compatibility_issues: Vec::new(),
                        next_cursor: None,
                        has_more: false,
                        events: vec![SessionEvent {
                            schema_version: 1,
                            session_id,
                            sequence: 18,
                            timestamp_ms: 0,
                            provenance: None,
                            kind: SessionEventKind::ModelTurnFinished {
                                turn_id: expected.turn_id.to_string(),
                                outcome,
                                message: None,
                            },
                        }],
                    },
                },
            )
            .await;
            // Retain transport until the observer settles the recovered terminal event.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });
        let client = BcodeClient::new(endpoint);
        let mut watcher = client.watch_session(session_id, 1).await.unwrap();
        let observed = headless::observe(
            &client,
            &mut watcher,
            session_id,
            &receipt,
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(observed, outcome);
        assert_eq!(
            write_terminal_turn_result(session_id, &receipt, observed).is_ok(),
            outcome == ModelTurnOutcome::Completed
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn headless_lost_admission_response_is_unknown_and_never_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("lost.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut watch = hello_peer(&listener).await;
        attach_peer(&mut watch, session_id).await;
        let mut admission = hello_peer(&listener).await;
        let request = bcode_ipc::recv_envelope(&mut admission).await.unwrap();
        assert!(
            matches!(bcode_ipc::decode_request(&request.payload).unwrap(),
            bcode_ipc::Request::SubmitTurn { session_id: id, .. } if id == session_id)
        );
        // The peer may have committed admission. Losing the response is not rejection.
        drop(admission);
        (listener, watch)
    });
    // Compatibility verification hashes the real test executable. Leave setup slack
    // under the parallel library suite; the lost response, not elapsed time, drives failure.
    let client = BcodeClient::new(endpoint);
    let result = send_message_observed(
        &client,
        session_id,
        scripted_send_options(),
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(30)),
    )
    .await;
    assert!(result.is_err());
    let (listener, _watch) = server.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn headless_subscription_uses_total_budget_not_transport_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("subscribe.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut watch = hello_peer(&listener).await;
        let _request = bcode_ipc::recv_envelope(&mut watch).await.unwrap();
        (listener, watch)
    });
    let client =
        BcodeClient::new(endpoint).with_request_timeout(std::time::Duration::from_secs(30));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send_message_observed(
            &client,
            session_id,
            scripted_send_options(),
            Some(tokio::time::Instant::now() + std::time::Duration::from_millis(100)),
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    let (listener, _watch) = server.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_prompt_never_connects_or_submits() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("input.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let client = BcodeClient::new(endpoint);
    let invalid_utf8 = directory.path().join("invalid.txt");
    std::fs::write(&invalid_utf8, [0xff]).unwrap();
    let inputs = [
        PromptInput {
            message: Some(String::new()),
            file: None,
            stdin: false,
        },
        PromptInput {
            message: Some("x".repeat(MAX_CLI_PROMPT_BYTES + 1)),
            file: None,
            stdin: false,
        },
        PromptInput {
            message: None,
            file: Some(invalid_utf8),
            stdin: false,
        },
        PromptInput {
            message: None,
            file: Some(directory.path().join("missing")),
            stdin: false,
        },
        PromptInput {
            message: None,
            file: Some(directory.path().to_path_buf()),
            stdin: false,
        },
        PromptInput {
            message: None,
            file: None,
            stdin: false,
        },
        PromptInput {
            message: Some("conflicting sources".into()),
            file: None,
            stdin: true,
        },
    ];
    for input in inputs {
        let mut options = scripted_send_options();
        options.input = input;
        let result = send_message_with_client(&client, SessionId::new(), options).await;
        assert!(matches!(
            result,
            Err(CliError::InvalidArguments(_) | CliError::Signal(_))
        ));
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

async fn admit_prompt(
    listener: &bcode_ipc::LocalIpcListener,
    session_id: SessionId,
    receipt: &bcode_session_models::TurnReceipt,
) {
    let mut stream = hello_peer(listener).await;
    let request = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
    let bcode_ipc::Request::SubmitTurn {
        session_id: id,
        text,
        admission,
    } = bcode_ipc::decode_request(&request.payload).unwrap()
    else {
        panic!("expected admission")
    };
    assert_eq!(id, session_id);
    assert_eq!(text, "$(touch never)\n具体");
    assert_eq!(admission.idempotency_key.as_deref(), Some("job-1"));
    assert_eq!(admission.origin.unwrap().producer, "ci.test");
    reply(
        &mut stream,
        request.request_id,
        bcode_ipc::ResponsePayload::TurnAdmission {
            admission: bcode_session_models::TurnAdmission::Accepted(receipt.clone()),
        },
    )
    .await;
}

fn scripted_send_options() -> SendOptions {
    SendOptions {
        input: PromptInput {
            message: Some("$(touch never)\n具体".into()),
            file: None,
            stdin: false,
        },
        placement: None,
        producer: "ci.test".into(),
        idempotency_key: Some("job-1".into()),
        background: false,
        json: true,
        observation: Some(TurnObservationOptions {
            timeout: std::time::Duration::from_millis(50),
            cancellation_grace: Some(std::time::Duration::from_secs(1)),
        }),
        launch_options: bcode_tui::TuiLaunchOptions::default(),
    }
}

#[tokio::test]
async fn send_timeout_recovers_terminal_outcome_after_false_or_lost_ack() {
    use bcode_session_models::{ModelTurnOutcome, SessionEvent, SessionEventKind, TurnReceipt};
    for acknowledged in [Some(false), Some(true), None] {
        for outcome in [ModelTurnOutcome::Completed, ModelTurnOutcome::Cancelled] {
            let directory = tempfile::tempdir().unwrap();
            let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("r.sock"));
            let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
            let session_id = SessionId::new();
            let receipt = TurnReceipt::from_accepted_event(session_id, 17);
            let expected = receipt.clone();
            let server = tokio::spawn(async move {
                let mut watch = hello_peer(&listener).await;
                attach_peer(&mut watch, session_id).await;
                admit_prompt(&listener, session_id, &expected).await;
                let initial = bcode_ipc::recv_envelope(&mut watch).await.unwrap();
                assert!(matches!(
                    bcode_ipc::decode_request(&initial.payload).unwrap(),
                    bcode_ipc::Request::SessionHistoryPage { .. }
                ));
                reply(
                    &mut watch,
                    initial.request_id,
                    bcode_ipc::ResponsePayload::SessionHistoryPage {
                        page: bcode_session_models::SessionHistoryPage {
                            session_id,
                            events: Vec::new(),
                            compatibility_issues: Vec::new(),
                            next_cursor: None,
                            has_more: false,
                        },
                    },
                )
                .await;
                let mut cancel = hello_peer(&listener).await;
                let request = bcode_ipc::recv_envelope(&mut cancel).await.unwrap();
                assert_eq!(
                    bcode_ipc::decode_request(&request.payload).unwrap(),
                    bcode_ipc::Request::CancelSessionTurn {
                        session_id,
                        expected_turn_id: Some(expected.turn_id.to_string()),
                        clear_queue: false,
                    }
                );
                if let Some(cancelled) = acknowledged {
                    reply(
                        &mut cancel,
                        request.request_id,
                        bcode_ipc::ResponsePayload::TurnCancellationRequested { cancelled },
                    )
                    .await;
                }
                drop(cancel);
                let history = bcode_ipc::recv_envelope(&mut watch).await.unwrap();
                assert!(
                    matches!(bcode_ipc::decode_request(&history.payload).unwrap(),
                    bcode_ipc::Request::SessionHistoryPage { session_id: id, .. } if id == session_id)
                );
                reply(
                    &mut watch,
                    history.request_id,
                    bcode_ipc::ResponsePayload::SessionHistoryPage {
                        page: bcode_session_models::SessionHistoryPage {
                            session_id,
                            compatibility_issues: Vec::new(),
                            next_cursor: None,
                            has_more: false,
                            events: vec![SessionEvent {
                                schema_version: 1,
                                sequence: 18,
                                timestamp_ms: 0,
                                session_id,
                                provenance: None,
                                kind: SessionEventKind::ModelTurnFinished {
                                    turn_id: expected.turn_id.to_string(),
                                    outcome,
                                    message: None,
                                },
                            }],
                        },
                    },
                )
                .await;
                listener
            });
            let client =
                BcodeClient::new(endpoint).with_request_timeout(std::time::Duration::from_secs(1));
            let result =
                send_message_with_client(&client, session_id, scripted_send_options()).await;
            assert_eq!(result.is_ok(), outcome == ModelTurnOutcome::Completed);
            let listener = server.await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        }
    }
}

#[test]
fn cancellation_acknowledgement_never_claims_terminal_success() {
    let session_id = SessionId::new();
    let receipt = bcode_session_models::TurnReceipt::from_accepted_event(session_id, 17);
    for (requested, acknowledged) in [
        (false, None),
        (true, None),
        (true, Some(false)),
        (true, Some(true)),
    ] {
        let mut output = Vec::new();
        write_unverified_observation(&mut output, session_id, &receipt, requested, acknowledged)
            .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "schema_version": 1,
                "session_id": session_id,
                "receipt": receipt,
                "outcome": null,
                "observation": "unverified",
                "cancellation_requested": requested,
                "cancellation_acknowledged": acknowledged,
            })
        );
    }
}

#[test]
fn unverified_observation_propagates_output_and_flush_failures() {
    struct FailingOutput {
        fail_write: bool,
    }
    impl std::io::Write for FailingOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                Err(std::io::Error::other("write failed"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush failed"))
        }
    }
    let session_id = SessionId::new();
    let receipt = bcode_session_models::TurnReceipt::from_accepted_event(session_id, 17);
    for fail_write in [true, false] {
        assert!(
            write_unverified_observation(
                &mut FailingOutput { fail_write },
                session_id,
                &receipt,
                true,
                Some(true),
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn timeout_cancellation_preserves_exact_target_and_unknown_acknowledgement() {
    for acknowledged in [Some(false), Some(true), None] {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("c.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let session_id = SessionId::new();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            let daemon = bcode_ipc::DaemonStatus {
                namespace: bcode_ipc::daemon_namespace(),
                protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                artifact_id: Some(bcode_ipc::ArtifactId::current()),
                executable_digest: Some(
                    bcode_daemon_lifecycle::current_executable_identity()
                        .unwrap()
                        .1,
                ),
                build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
                storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
                session_event_schema_version: Some(
                    bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                ),
                state_location_id: Some(bcode_ipc::state_location_id()),
                ..Default::default()
            };
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            bcode_ipc::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(hello.request_id, &response).unwrap(),
            )
            .await
            .unwrap();
            let request = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            assert_eq!(
                bcode_ipc::decode_request(&request.payload).unwrap(),
                bcode_ipc::Request::CancelSessionTurn {
                    session_id,
                    expected_turn_id: Some("turn-具体".to_owned()),
                    clear_queue: false,
                }
            );
            if let Some(cancelled) = acknowledged {
                let response = bcode_ipc::Response::Ok(
                    bcode_ipc::ResponsePayload::TurnCancellationRequested { cancelled },
                );
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(request.request_id, &response).unwrap(),
                )
                .await
                .unwrap();
            }
            // EOF before acknowledgement models an uncertain transport outcome.
            drop(stream);
            listener
        });
        let client =
            BcodeClient::new(endpoint).with_request_timeout(std::time::Duration::from_secs(1));
        assert_eq!(
            request_observation_cancellation(&client, session_id, "turn-具体".to_owned(), true,)
                .await,
            acknowledged
        );
        let listener = server.await.unwrap();
        // No retry, session-wide cancellation, or resubmission follows an error.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept(),)
                .await
                .is_err()
        );
        assert_eq!(
            request_observation_cancellation(&client, session_id, "turn-具体".to_owned(), false,)
                .await,
            None
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept(),)
                .await
                .is_err()
        );
    }
}
