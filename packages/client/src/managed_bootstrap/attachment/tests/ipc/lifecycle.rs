//! Attachment races exercised through framed application IPC, without native effects.
use super::*;

#[derive(Clone, Copy)]
enum BindingChange {
    None,
    Generation,
    Instance,
    Target,
    Owner,
    Confirmed,
}

fn changed_binding(owner: SessionId, change: BindingChange) -> ManagedSessionTargetSnapshot {
    let mut binding = snapshot(owner);
    let bcode_harness_models::HarnessSessionTarget::Prepared(target) = &mut binding.target else {
        unreachable!();
    };
    match change {
        BindingChange::None => {}
        BindingChange::Generation => target.generation += 1,
        BindingChange::Instance => binding.instance.config_namespace = "/other/config".into(),
        BindingChange::Target => target.open_id = "replacement-open".into(),
        BindingChange::Owner => {
            let replacement = SessionId::new();
            binding.session_id = replacement;
            target.session_id = replacement.to_string();
        }
        BindingChange::Confirmed => {
            binding.target = bcode_harness_models::HarnessSessionTarget::Confirmed(
                bcode_harness_models::HarnessSessionBinding {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    session_id: owner.to_string(),
                    instance_id: "primary".into(),
                    native_session_id: "native-confirmed".into(),
                    native_version: "verified-version".into(),
                    generation: 1,
                },
            );
        }
    }
    binding.validate().unwrap();
    binding
}

fn projection_request() -> ProjectionWindowRequest {
    ProjectionWindowRequest {
        projection: bcode_session_models::SessionProjectionKind::Transcript,
        anchor: bcode_session_models::ProjectionWindowAnchor::Latest,
        direction: bcode_session_models::ProjectionWindowDirection::Backward,
        target: bcode_session_models::ProjectionWindowTarget::default(),
        limits: bcode_session_models::ProjectionWindowLimits {
            max_items: 32,
            max_events_scanned: 128,
            max_bytes: 65_536,
        },
    }
}

#[tokio::test]
async fn attachment_and_refresh_reject_changed_exact_binding_without_native_calls() {
    for projection in [false, true] {
        for refresh in [false, true] {
            for change in [
                BindingChange::None,
                BindingChange::Generation,
                BindingChange::Instance,
                BindingChange::Target,
                BindingChange::Owner,
                BindingChange::Confirmed,
            ] {
                Box::pin(attachment_race(projection, refresh, change)).await;
            }
        }
    }
}

async fn attachment_race(projection: bool, refresh: bool, change: BindingChange) {
    let directory = tempfile::tempdir().unwrap();
    let socket_directory = directory.path().join("ipc");
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&socket_directory)
        .unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(
        socket_directory
            .canonicalize()
            .unwrap()
            .join("lifecycle.sock"),
    );
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let owner = SessionId::new();
    let context = super::super::super::super::configured_context(
        &bcode_config::BcodeConfig::default(),
        directory.path().to_path_buf(),
    )
    .unwrap();
    let server = tokio::spawn(serve_binding_race(
        listener,
        owner,
        context.clone(),
        refresh,
        change,
    ));
    let client = super::super::super::ManagedSessionClient {
        inner: crate::BcodeClient::new(endpoint).with_runtime_context(Some(context)),
    };
    let work = async {
        let attached = if projection {
            client
                .attach_session_projection_window(owner, projection_request())
                .await
        } else {
            client.attach_session_recent(owner, 32).await
        };
        let result = if refresh {
            let (mut connection, _) = attached.unwrap();
            connection
                .refresh_projection(projection_request())
                .await
                .map(|_| ())
        } else {
            match attached {
                Ok((connection, _)) => {
                    let commands = connection.command_handle();
                    let channel = Arc::clone(&commands.command_connection);
                    // Exercise both immediate teardown and a busy command channel.
                    let busy = if projection {
                        Some(channel.lock().await)
                    } else {
                        None
                    };
                    drop(connection);
                    drop(busy);
                    tokio::time::timeout(std::time::Duration::from_secs(1), async {
                        loop {
                            let state = channel.lock().await;
                            if state.detached {
                                assert!(state.connection.is_none());
                                break;
                            }
                            drop(state);
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    Ok(())
                }
                Err(error) => Err(error),
            }
        };
        assert_eq!(result.is_ok(), matches!(change, BindingChange::None));
        if let Err(error) = result {
            assert!(matches!(error, ClientError::Protocol(_)));
        }
        server.await.unwrap();
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), work)
        .await
        .unwrap();
}

async fn serve_binding_race(
    listener: bcode_ipc::LocalIpcListener,
    owner: SessionId,
    context: bcode_ipc::ClientRuntimeContext,
    refresh: bool,
    change: BindingChange,
) {
    let mut tasks = Vec::new();
    let bindings = if refresh { 4 } else { 2 };
    let mut binding_reads = 0;
    // One persistent attachment plus independent, configured binding queries.
    for _ in 0..=bindings {
        let mut stream = listener.accept().await.unwrap();
        let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
        let Request::Hello {
            runtime_context, ..
        } = bcode_ipc::decode_request(&hello.payload).unwrap()
        else {
            panic!("expected handshake");
        };
        assert_eq!(runtime_context, Some(context.clone()));
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
            Request::ManagedSession { request } => {
                assert_eq!(request.operation, ManagedSessionOperation::Binding);
                let value: serde_json::Value = serde_json::from_str(&request.request_json).unwrap();
                assert_eq!(value["session_id"], serde_json::json!(owner));
                binding_reads += 1;
                let binding = if binding_reads == bindings {
                    changed_binding(owner, change)
                } else {
                    snapshot(owner)
                };
                send(
                    &mut stream,
                    envelope.request_id,
                    ResponsePayload::ManagedSession {
                        response: bcode_ipc::ManagedSessionResponse {
                            schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                            response_json: serde_json::to_string(&Some(binding)).unwrap(),
                        },
                    },
                )
                .await;
            }
            request => {
                tasks.push(tokio::spawn(async move {
                    check_attach_request(&request, owner);
                    send_empty_attachment(&mut stream, envelope.request_id, owner).await;
                    if refresh {
                        let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
                        check_attach_request(
                            &bcode_ipc::decode_request(&envelope.payload).unwrap(),
                            owner,
                        );
                        send_empty_attachment(&mut stream, envelope.request_id, owner).await;
                    }
                    // Dropping the attachment is not a native Close or Interrupt.
                    assert!(bcode_ipc::recv_envelope(&mut stream).await.is_err());
                }));
            }
        }
    }
    for task in tasks {
        task.await.unwrap();
    }
}

fn check_attach_request(request: &Request, owner: SessionId) {
    match request {
        Request::AttachSessionRecent { session_id, .. }
        | Request::AttachSessionProjectionWindow { session_id, .. } => {
            assert_eq!(*session_id, owner);
        }
        _ => panic!("only bounded attachment is allowed"),
    }
}

async fn send_empty_attachment(stream: &mut bcode_ipc::LocalIpcStream, id: u64, owner: SessionId) {
    let session = serde_json::from_value(serde_json::json!({
        "id": owner, "name": null, "client_count": 1,
        "created_at_ms": 0, "updated_at_ms": 0, "working_directory": "/workspace"
    }))
    .unwrap();
    send(
        stream,
        id,
        ResponsePayload::Attached {
            session_id: owner,
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
