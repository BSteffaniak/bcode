//! Public sealed attach/composer fixture. Isolated process config, fake IPC only.
use bcode_ipc::{Request, Response, ResponsePayload};

#[test]
fn managed_attach_and_composer_do_not_materialize_unrelated_credentials() {
    const PROBE: &str = "BCODE_TUI_MANAGED_ATTACH_PROBE";
    if std::env::var_os(PROBE).is_some() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    // Socket custody must be private from creation, independent of tempfile's
    // parent mode and the process umask. Never repair permissions on an existing path.
    let socket_directory = root.join("ipc");
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&socket_directory)
        .unwrap();
    let socket_directory = socket_directory.canonicalize().unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&socket_directory).unwrap().permissions()
        ) & 0o777,
        0o700,
    );
    let config_path = directory.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[model]
provider_plugin_id = "example.unrelated"
model_id = "unrelated-model"
auth_profile = "unrelated"
[auth.profiles.unrelated]
backend = "env"
provider_id = "example"
owner_plugin_id = "example.unrelated"
[auth.profiles.unrelated.map.api_key]
env = "BCODE_MANAGED_UI_MISSING_KEY_71E9"
"#,
    )
    .unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::managed_chat_tests::managed_attach_probe",
            "--nocapture",
        ])
        .env(PROBE, "1")
        .env("BCODE_CONFIG", &config_path)
        .env("BCODE_MANAGED_UI_ROOT", &root)
        .env(
            "BCODE_IPC_ENDPOINT_NAMESPACE",
            bcode_ipc::daemon_namespace(),
        )
        .env("BCODE_STATE_DIR", directory.path().join("state"))
        .env("XDG_CONFIG_HOME", directory.path().join("config-home"))
        .env("BCODE_SOCKET", socket_directory.join("s.sock"))
        .env_remove("BCODE_IPC_ENDPOINT")
        .env_remove("BCODE_DAEMON_LOG")
        .env_remove("BCODE_MANAGED_UI_MISSING_KEY_71E9")
        .env_remove("BCODE_CONTEXT")
        .env_remove("BCODE_PROFILE")
        .env_remove("BCODE_MODEL_PROVIDER")
        .env_remove("BCODE_PROVIDER")
        .env_remove("BCODE_MODEL")
        .env_remove("BCODE_MODEL_PROFILE")
        .env_remove("BCODE_AUTH_PROFILE")
        .env_remove("BCODE_AUTH_POOL")
        .env_remove("BCODE_CLI_OVERRIDES_JSON")
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(root.join("probe-complete")).unwrap(),
        "sealed attach and composer completed"
    );
}

#[tokio::test]
async fn managed_attach_probe() {
    if std::env::var_os("BCODE_TUI_MANAGED_ATTACH_PROBE").is_none() {
        return;
    }
    let owner = bcode_session_models::SessionId::new();
    let socket = std::env::var_os("BCODE_SOCKET").unwrap();
    let listener = bcode_ipc::LocalIpcListener::bind(&bcode_ipc::IpcEndpoint::unix_socket(
        std::path::PathBuf::from(socket),
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let mut tasks = Vec::new();
        // binding + attach + binding recheck + prepared input binding + submit
        for _ in 0..5 {
            let stream = listener.accept().await.unwrap();
            tasks.push(tokio::spawn(peer(stream, owner)));
        }
        for task in tasks {
            task.await.unwrap();
        }
    });
    let (connection, attached) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        bcode_client::ManagedSessionClient::default_endpoint().attach_session_recent(owner, 32),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(attached.session.id, owner);
    assert_eq!(attached.draft.as_deref(), Some("canonical draft"));
    assert_eq!(attached.input_history[0].text, "canonical previous input");
    let commands = connection.command_handle();
    let retained = commands
        .prepare_managed_composer_submission(
            ("submission".into(), "attempt".into()),
            "managed composer".into(),
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(retained.target().0, owner.to_string());
    assert_eq!(retained.target().1, 1);
    let response = commands
        .submit_managed_composer_request(retained)
        .await
        .unwrap();
    assert!(matches!(
        response.kind,
        bcode_session_models::ExternalHarnessResponseKind::Receipt(_)
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let root = std::path::PathBuf::from(std::env::var_os("BCODE_MANAGED_UI_ROOT").unwrap());
    std::fs::write(
        root.join("probe-complete"),
        "sealed attach and composer completed",
    )
    .unwrap();
}

async fn peer(mut stream: bcode_ipc::LocalIpcStream, owner: bcode_session_models::SessionId) {
    let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
    let Request::Hello {
        runtime_context: Some(context),
        ..
    } = bcode_ipc::decode_request(&hello.payload).unwrap()
    else {
        panic!("caller configuration required")
    };
    assert!(context.provider_context.auth.is_none());
    assert_eq!(
        context.provider_context.auth_candidates,
        [] as [bcode_model::ProviderAuthCandidate; 0]
    );
    assert!(context.process_env.is_empty());
    let config =
        bcode_config::decode_effective_config(context.effective_config_toml.as_deref().unwrap())
            .unwrap();
    assert_eq!(config.model.auth_profile.as_deref(), Some("unrelated"));
    assert_eq!(
        config.model.provider_plugin_id.as_deref(),
        Some("example.unrelated")
    );
    assert!(
        config.auth.profiles["unrelated"]
            .map
            .contains_key("api_key")
    );
    assert_eq!(
        context.selected_provider_plugin_id.as_deref(),
        Some("example.unrelated")
    );
    send_hello(&mut stream, hello.request_id).await;
    let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
    let request = bcode_ipc::decode_request(&envelope.payload).unwrap();
    match request {
        Request::AttachSessionRecent { session_id, .. } => {
            assert_eq!(session_id, owner);
            send(&mut stream, envelope.request_id, ResponsePayload::Attached {
                session_id: owner,
                session: serde_json::from_value(serde_json::json!({"id":owner,"name":null,"client_count":1,"created_at_ms":0,"updated_at_ms":0,"working_directory":"/workspace"})).unwrap(),
                history: Vec::new(), input_history: vec![bcode_session_models::SessionInputHistoryEntry { sequence: 1, timestamp_ms: 0, text: "canonical previous input".into() }],
                usage_summary: None, import_warnings: Vec::new(), draft: Some("canonical draft".into()),
                runtime_selection: bcode_session_models::SessionRuntimeSelection::default(), projection_window: None,
            }).await;
        }
        Request::ManagedSession { request } => {
            use bcode_ipc::harness::ManagedSessionOperation;
            let json = match request.operation {
                ManagedSessionOperation::Binding => {
                    serde_json::to_string(&Some(binding(owner))).unwrap()
                }
                ManagedSessionOperation::Followup => {
                    let call = request.decode_call().unwrap();
                    assert_eq!(call.session_id, Some(owner));
                    let bcode_harness_models::HarnessRequestKind::Submit { submission, .. } =
                        &call.harness.kind
                    else {
                        panic!("managed submission required")
                    };
                    assert_eq!(submission.text, "managed composer");
                    serde_json::to_string(&bcode_harness_models::HarnessServiceResponse {
                        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                        request_id: call.harness.request_id.clone(),
                        operation: bcode_harness_models::HarnessOperation::Submit,
                        kind: bcode_harness_models::HarnessServiceResponseKind::Receipt(
                            bcode_harness_models::HarnessReceipt {
                                submission_id: submission.submission_id.clone(),
                                attempt_id: submission.attempt_id.clone(),
                                session_id: owner.to_string(),
                                generation: 1,
                                state: bcode_harness_models::HarnessDeliveryState::Accepted,
                                native_request_id: None,
                            },
                        ),
                    })
                    .unwrap()
                }
                _ => panic!("unexpected operation: no model or credential fallback permitted"),
            };
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
        }
        _ => panic!("ordinary model/provider operation escaped managed facade"),
    }
}

fn binding(owner: bcode_session_models::SessionId) -> bcode_ipc::ManagedSessionTargetSnapshot {
    serde_json::from_value(serde_json::json!({
        "session_id":owner,
        "instance":{"instance_id":"primary","plugin_id":"example.native","executable":"/native","config_namespace":"/native/config",
            "auth":{"provider_id":"example.native","profile":"native","credential_namespace":"/native/config","custody":"native_managed"}},
        "target":{"state":"prepared","session":{"open_id":"open","session_id":owner,"instance_id":"primary","generation":1,"native_version":"fixture","requested_native_session_id":null}}
    })).unwrap()
}

async fn send_hello(stream: &mut bcode_ipc::LocalIpcStream, request_id: u64) {
    let (_, digest) = bcode_daemon_lifecycle::current_executable_identity().unwrap();
    send(
        stream,
        request_id,
        ResponsePayload::Hello {
            protocol_version: bcode_ipc::ProtocolVersion::current(),
            client_id: bcode_session_models::ClientId::new(),
            daemon: bcode_ipc::DaemonStatus {
                namespace: bcode_ipc::daemon_namespace(),
                protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                artifact_id: Some(bcode_ipc::ArtifactId::current()),
                build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
                executable_digest: Some(digest),
                storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
                session_event_schema_version: Some(
                    bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                ),
                state_location_id: Some(bcode_ipc::state_location_id()),
                ..Default::default()
            },
        },
    )
    .await;
}

async fn send(stream: &mut bcode_ipc::LocalIpcStream, id: u64, payload: ResponsePayload) {
    let envelope = bcode_ipc::response_envelope(id, &Response::Ok(payload)).unwrap();
    bcode_ipc::send_envelope(stream, &envelope).await.unwrap();
}
