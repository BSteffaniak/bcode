//! Real framed IPC with a fake peer. No native harness or inference is launched.
mod lifecycle;
use super::*;
use bcode_ipc::{Request, Response, ResponsePayload};
use tokio::io::AsyncReadExt as _;

#[tokio::test]
async fn configured_catalog_and_open_transport_preserve_missing_model_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let socket_directory = directory.path().join("ipc");
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&socket_directory)
        .unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(
        socket_directory
            .canonicalize()
            .unwrap()
            .join("configured.sock"),
    );
    let config = bcode_config::decode_effective_config(
        r#"
[model]
provider_plugin_id = "example.model"
auth_profile = "unrelated"
[auth.profiles.unrelated]
backend = "env"
provider_id = "example"
owner_plugin_id = "example.model"
[auth.profiles.unrelated.map.api_key]
env = "BCODE_MANAGED_CATALOG_OPEN_UNSET_CREDENTIAL_714B"
"#,
    )
    .unwrap();
    let context =
        super::super::super::configured_context(&config, directory.path().to_path_buf()).unwrap();
    let owner = SessionId::new();
    let call = bcode_ipc::harness::ManagedSessionCall {
        session_id: Some(owner),
        harness: HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "retained-open".into(),
            instance: snapshot(owner).instance,
            kind: HarnessRequestKind::Open(bcode_harness_models::HarnessOpenRequest {
                open_id: "retained-open".into(),
                session_id: owner.to_string(),
                generation: 1,
                working_directory: directory.path().to_str().unwrap().into(),
                resume: None,
                parent_assistance: None,
            }),
        },
        parent_session_id: None,
        assistance_destination_session_id: None,
    };
    let request = bcode_ipc::harness::ManagedSessionRequest {
        schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
        operation: ManagedSessionOperation::Start,
        request_json: serde_json::to_string(&call).unwrap(),
    };
    let requests = vec![
        (
            "bcode.harness.configuration/v1".to_owned(),
            "catalog".to_owned(),
            b"{}".to_vec(),
        ),
        (
            "bcode.harness.configuration/v1".to_owned(),
            "prepare".to_owned(),
            br#"{"selection":{"kind":"instance","instance_id":"primary"}}"#.to_vec(),
        ),
        (
            bcode_ipc::harness::MANAGED_SESSION_INTERFACE_ID.to_owned(),
            "start".to_owned(),
            serde_json::to_vec(&request).unwrap(),
        ),
    ];
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let server = tokio::spawn(serve_configured_requests(
        listener,
        context.clone(),
        requests.clone(),
    ));
    let client = super::super::ManagedSessionClient {
        inner: crate::BcodeClient::new(endpoint).with_runtime_context(Some(context)),
    };
    for (interface, operation, payload) in requests {
        let response = client
            .call_plugin_service(interface, operation, payload)
            .await
            .unwrap();
        assert!(response.error.is_none());
        assert_eq!(response.payload, b"{}");
    }
    server.await.unwrap();
}

async fn serve_configured_requests(
    listener: bcode_ipc::LocalIpcListener,
    expected_context: bcode_ipc::ClientRuntimeContext,
    requests: Vec<(String, String, Vec<u8>)>,
) {
    for (expected_interface, expected_operation, expected_payload) in requests {
        let mut stream = listener.accept().await.unwrap();
        let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
        let Request::Hello {
            runtime_context: Some(context),
            ..
        } = bcode_ipc::decode_request(&hello.payload).unwrap()
        else {
            panic!("config-bound hello required")
        };
        assert_eq!(context, expected_context);
        let config = bcode_config::decode_effective_config(
            context.effective_config_toml.as_deref().unwrap(),
        )
        .unwrap();
        assert!(
            config.auth.profiles["unrelated"]
                .map
                .contains_key("api_key")
        );
        assert!(context.provider_context.auth.is_none());
        assert_eq!(context.provider_context.auth_candidates, Vec::new());
        send(
            &mut stream,
            hello.request_id,
            ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon: fixture_daemon(),
            },
        )
        .await;
        let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
        let Request::CallPluginService {
            interface_id,
            operation,
            payload,
        } = bcode_ipc::decode_request(&envelope.payload).unwrap()
        else {
            panic!("only sealed catalog/prepare/start routes allowed")
        };
        assert_eq!(
            (interface_id, operation, payload),
            (expected_interface, expected_operation, expected_payload)
        );
        send(
            &mut stream,
            envelope.request_id,
            ResponsePayload::PluginServiceResult {
                response: bcode_ipc::PluginServiceResponse {
                    payload: b"{}".to_vec(),
                    error: None,
                },
            },
        )
        .await;
    }
}

#[tokio::test]
async fn pending_event_receive_does_not_block_bound_input_and_interrupt() {
    let directory = tempfile::tempdir().unwrap();
    let socket_directory = directory.path().join("ipc");
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&socket_directory)
        .unwrap();
    let socket_directory = socket_directory.canonicalize().unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&socket_directory).unwrap().permissions()
        ) & 0o777,
        0o700
    );
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_directory.join("managed.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let owner = SessionId::new();
    let config = bcode_config::BcodeConfig::default();
    let context =
        super::super::super::configured_context(&config, directory.path().to_path_buf()).unwrap();
    let (release, release_event) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve_commands(
        listener,
        owner,
        context.clone(),
        release_event,
    ));
    let client = super::super::ManagedSessionClient {
        inner: crate::BcodeClient::new(endpoint).with_runtime_context(Some(context)),
    };
    let (mut connection, history) = client.attach_session_recent(owner, 32).await.unwrap();
    assert_eq!(history.draft.as_deref(), Some("draft"));
    let commands = connection.command_handle();
    check_command_history_boundaries(&commands, owner).await;
    let other_commands = commands.clone();
    let receive = connection.recv_event_without_reconnect();
    tokio::pin!(receive);
    let command_work = async {
        let retained = commands
            .prepare_managed_composer_submission(
                ("submission".into(), "attempt".into()),
                "input while receive pending".into(),
                false,
                None,
            )
            .await
            .unwrap();
        let retry_after_detach = retained.clone();
        commands
            .submit_managed_composer_request(retained)
            .await
            .unwrap();
        assert!(
            commands
                .command_connection
                .lock()
                .await
                .connection
                .is_some()
        );
        assert!(std::sync::Arc::ptr_eq(
            &commands.command_connection,
            &other_commands.command_connection
        ));
        other_commands.interrupt(1, "stop".into()).await.unwrap();
        other_commands.detach_commands().await;
        assert!(
            commands
                .submit_managed_composer_request(retry_after_detach)
                .await
                .is_err()
        );
    };
    tokio::pin!(command_work);
    tokio::select! {
        event = &mut receive => panic!("event arrived before commands: {event:?}"),
        () = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("command facade blocked behind event receive"),
        () = &mut command_work => {}
    }
    release.send(true).unwrap();
    assert!(
        matches!(receive.await.unwrap(), Event::SessionViewResyncRequired { session_id } if session_id == owner)
    );
    server.await.unwrap();
}

async fn check_command_history_boundaries(
    commands: &super::super::ManagedSessionCommandHandle,
    owner: SessionId,
) {
    assert_eq!(
        commands.submissions_for_history(&[]).await.unwrap(),
        Vec::new()
    );
    let history_event = |session_id, submission_id: &str| bcode_session_models::SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 0,
        session_id,
        provenance: None,
        kind: bcode_session_models::SessionEventKind::SubmissionCancelled {
            submission_id: bcode_session_models::SubmissionId(submission_id.to_owned()),
        },
    };
    // A foreign page and an oversized identity window fail before opening any transport.
    assert!(
        commands
            .submissions_for_history(&[history_event(SessionId::new(), "foreign")])
            .await
            .is_err()
    );
    let oversized: Vec<_> = (0..257)
        .map(|index| history_event(owner, &format!("submission-{index}")))
        .collect();
    assert!(commands.submissions_for_history(&oversized).await.is_err());
    // Missing canonical records must not become an apparently successful empty history page.
    assert!(
        commands
            .submissions_for_history(&[history_event(owner, "missing")])
            .await
            .is_err()
    );
}

fn fixture_daemon() -> bcode_ipc::DaemonStatus {
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

async fn serve_commands(
    listener: bcode_ipc::LocalIpcListener,
    owner: SessionId,
    expected_context: bcode_ipc::ClientRuntimeContext,
    release_event: tokio::sync::watch::Receiver<bool>,
) {
    let mut tasks = Vec::new();
    // Initial binding, attach, post-attach binding, missing canonical hydration lookup,
    // composer binding, submission, interrupt binding and interruption.
    // Each command retains the same config handshake.
    for _ in 0..8 {
        let stream = listener.accept().await.unwrap();
        tasks.push(tokio::spawn(serve_connection(
            stream,
            owner,
            expected_context.clone(),
            release_event.clone(),
        )));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

async fn serve_connection(
    mut stream: bcode_ipc::LocalIpcStream,
    owner: SessionId,
    expected_context: bcode_ipc::ClientRuntimeContext,
    release_event: tokio::sync::watch::Receiver<bool>,
) {
    let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
    let Request::Hello {
        runtime_context: Some(context),
        ..
    } = bcode_ipc::decode_request(&hello.payload).unwrap()
    else {
        panic!("expected config-bound hello");
    };
    assert_eq!(context, expected_context);
    send(
        &mut stream,
        hello.request_id,
        ResponsePayload::Hello {
            protocol_version: bcode_ipc::ProtocolVersion::current(),
            client_id: bcode_session_models::ClientId::new(),
            daemon: fixture_daemon(),
        },
    )
    .await;
    let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
    match bcode_ipc::decode_request(&envelope.payload).unwrap() {
        Request::GetSubmission {
            session_id,
            submission_id,
        } => {
            assert_eq!(session_id, owner);
            assert_eq!(submission_id.0, "missing");
            send(
                &mut stream,
                envelope.request_id,
                ResponsePayload::Submission { record: None },
            )
            .await;
        }
        Request::AttachSessionRecent { session_id, .. } => {
            assert_eq!(session_id, owner);
            serve_attachment(&mut stream, envelope.request_id, owner, release_event).await;
        }
        Request::ManagedSession { request } => {
            let submission = request.operation == ManagedSessionOperation::Followup;
            let json = managed_response(owner, &request);
            send(
                &mut stream,
                envelope.request_id,
                ResponsePayload::ManagedSession {
                    response: bcode_ipc::ManagedSessionResponse {
                        schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                        response_json: json,
                    },
                },
            )
            .await;
            if submission {
                // A completed Submit must not close its authenticated command principal.
                // Explicit detach below, not the response boundary, closes this socket.
                let mut byte = [0_u8; 1];
                assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
            }
        }
        _ => panic!("unexpected non-managed request"),
    }
}

async fn serve_attachment(
    stream: &mut bcode_ipc::LocalIpcStream,
    request_id: u64,
    owner: SessionId,
    mut release_event: tokio::sync::watch::Receiver<bool>,
) {
    let session = serde_json::from_value(serde_json::json!({
        "id": owner, "name": null, "client_count": 1,
        "created_at_ms": 0, "updated_at_ms": 0, "working_directory": "/workspace"
    }))
    .unwrap();
    send(
        stream,
        request_id,
        ResponsePayload::Attached {
            session_id: owner,
            session,
            history: Vec::new(),
            input_history: Vec::new(),
            usage_summary: None,
            import_warnings: Vec::new(),
            draft: Some("draft".into()),
            runtime_selection: bcode_session_models::SessionRuntimeSelection::default(),
            projection_window: None,
        },
    )
    .await;
    while !*release_event.borrow_and_update() {
        release_event.changed().await.unwrap();
    }
    let event =
        bcode_ipc::event_envelope(&Event::SessionViewResyncRequired { session_id: owner }).unwrap();
    bcode_ipc::send_envelope(stream, &event).await.unwrap();
}

fn managed_response(
    owner: SessionId,
    request: &bcode_ipc::harness::ManagedSessionRequest,
) -> String {
    match request.operation {
        ManagedSessionOperation::Binding => {
            let value: serde_json::Value = serde_json::from_str(&request.request_json).unwrap();
            assert_eq!(value["session_id"], serde_json::json!(owner));
            serde_json::to_string(&Some(snapshot(owner))).unwrap()
        }
        operation @ (ManagedSessionOperation::Followup | ManagedSessionOperation::Interrupt) => {
            let call = request.decode_call().unwrap();
            assert_eq!(call.session_id, Some(owner));
            assert_eq!(call.harness.instance.instance_id, "primary");
            let kind = match &call.harness.kind {
                HarnessRequestKind::Submit { submission, .. } => {
                    assert_eq!(operation, ManagedSessionOperation::Followup);
                    assert_eq!(submission.session_id, owner.to_string());
                    assert_eq!(submission.text, "input while receive pending");
                    bcode_harness_models::HarnessServiceResponseKind::Receipt(
                        bcode_harness_models::HarnessReceipt {
                            submission_id: submission.submission_id.clone(),
                            attempt_id: submission.attempt_id.clone(),
                            session_id: submission.session_id.clone(),
                            generation: submission.generation,
                            state: bcode_harness_models::HarnessDeliveryState::Accepted,
                            native_request_id: None,
                        },
                    )
                }
                HarnessRequestKind::Interrupt(control) => {
                    assert_eq!(operation, ManagedSessionOperation::Interrupt);
                    bcode_harness_models::HarnessServiceResponseKind::Acknowledged {
                        mutation_id: control.mutation_id.clone(),
                    }
                }
                _ => panic!("not a managed input/control operation"),
            };
            serde_json::to_string(&HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: call.harness.request_id,
                operation: call.harness.kind.operation(),
                kind,
            })
            .unwrap()
        }
        _ => panic!("unexpected operation; no model fallback is allowed"),
    }
}

async fn send(stream: &mut bcode_ipc::LocalIpcStream, id: u64, payload: ResponsePayload) {
    let response = bcode_ipc::response_envelope(id, &Response::Ok(payload)).unwrap();
    bcode_ipc::send_envelope(stream, &response).await.unwrap();
}
