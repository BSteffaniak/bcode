#![cfg(all(
    feature = "app",
    feature = "daemon-client",
    feature = "static-bundled-fake-provider-plugin",
    feature = "static-bundled-default-agents-plugin",
    feature = "static-bundled-question-plugin"
))]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Persistent semantic IPC consumer acceptance; no provider spending.
//! Run with `app,daemon-client,static-bundled-fake-provider-plugin,static-bundled-default-agents-plugin,static-bundled-question-plugin`.

use bcode_plugin_sdk::interaction_application::{
    InteractionApplication, InteractionControllerInput,
};
use bcode_session_models::{
    CreateSessionRequest, SessionApplication, SessionEventKind, SessionId, TurnAdmission,
    TurnAdmissionMetadata,
};
use std::time::{Duration, Instant};

#[test]
fn complete_persistent_ipc_question_path() {
    run_complete_case("complete_persistent_ipc_question_path", "full");
}

#[test]
fn disabled_question_ipc_cleanup() {
    run_complete_case("disabled_question_ipc_cleanup", "disabled");
}

#[test]
#[cfg(feature = "static-bundled-shell-plugin")]
fn denied_permission_ipc_cleanup() {
    run_complete_case("denied_permission_ipc_cleanup", "deny");
}

#[test]
fn incompatible_controller_ipc_cleanup() {
    run_complete_case("incompatible_controller_ipc_cleanup", "incompatible");
}

#[test]
fn independent_explicit_daemon_consumer_process() {
    const CHILD: &str = "BCODE_INDEPENDENT_CONSUMER_CHILD";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(75), independent_consumer())
                .await
                .expect("independent consumer inner deadline; fixture RAII cleanup completed");
        });
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("bci-")
        .tempdir_in("/tmp")
        .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "independent_explicit_daemon_consumer_process",
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, "1")
        .env("HOME", root.path())
        .env("BCODE_STATE_DIR", root.path().join("state"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .current_dir(root.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "independent consumer process failed: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("independent consumer process exceeded watchdog");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// Owns only the isolated fixture child. Panic/timeout never leaks a test daemon.
struct FixtureDaemon(std::process::Child);
impl Drop for FixtureDaemon {
    fn drop(&mut self) {
        eprintln!("independent phase: cleanup_kill_reap_begin");
        let _ = self.0.kill();
        let _ = self.0.wait();
        eprintln!("independent phase: cleanup_kill_reap_complete");
    }
}

async fn independent_consumer() {
    use bcode_daemon_lifecycle::DaemonTarget;
    let directory = std::env::current_dir().unwrap();
    let executable = directory.join("selected-bcode");
    std::fs::copy(env!("CARGO_BIN_EXE_bcode"), &executable).unwrap();
    let digest = bcode_daemon_lifecycle::executable_sha256(&executable).unwrap();
    eprintln!(
        "independent fixture executable={} sha256={digest}",
        executable.display()
    );
    std::fs::write(directory.join("selected-executable.sha256"), &digest).unwrap();
    assert_ne!(
        digest,
        bcode_daemon_lifecycle::current_executable_identity()
            .unwrap()
            .1,
        "consumer and daemon must be independently linked executables"
    );
    // DaemonTarget obtains artifact-id through the selected executable's supported interface.
    let config_directory = bcode_config::default_config_dir();
    std::fs::create_dir_all(&config_directory).unwrap();
    let (config, ids) = independent_fake_configuration();
    eprintln!("independent phase: daemon_spawn_begin");
    let (mut daemon, location) = start_independent_daemon(&directory, &executable, &config);
    eprintln!("independent phase: target_verification_begin");
    let target = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "fixture daemon exited; inspect isolated daemon.log"
            );
            if let Ok(target) = DaemonTarget::verify(
                executable.clone(),
                digest.clone(),
                location.clone(),
                config_directory.clone(),
            )
            .await
            {
                break target;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("selected fixture daemon did not become verifiable");
    eprintln!("independent phase: target_verification_complete");
    let artifact = target.artifact_id().clone();
    assert_ne!(
        artifact,
        bcode_ipc::ArtifactId::current(),
        "supported daemon identity must differ from consumer identity"
    );
    eprintln!("independent phase: target_rejections_begin");
    assert_independent_target_rejections(
        &executable,
        &digest,
        &location,
        &config_directory,
        &config,
        &directory,
    )
    .await;
    eprintln!("independent phase: target_rejections_complete");
    let client =
        bcode_client::BcodeClient::for_daemon(target.clone(), &config, directory.clone()).unwrap();
    eprintln!("independent phase: plugin_services_begin");
    let services = bcode_plugin_sdk::application::PluginApplication::list_plugin_services(&client)
        .await
        .unwrap();
    assert!(
        services
            .iter()
            .any(|service| service.plugin_id == "bcode.fake-provider"
                && service.interface_id == bcode_model::MODEL_PROVIDER_INTERFACE_ID),
        "standalone daemon did not register the selectively compiled fake provider; loaded plugin services={services:?}"
    );
    assert!(
        services
            .iter()
            .all(|service| ids.contains(&service.plugin_id)),
        "fake-only fixture loaded an unexpected plugin: {services:?}"
    );
    assert_eq!(
        config
            .resolved_model_selection()
            .provider_plugin_id
            .as_deref(),
        Some("bcode.fake-provider")
    );
    eprintln!("independent phase: server_status_begin");
    let status = client.verified_server_status().await.unwrap();
    let mut foreign_artifact = status.daemon;
    foreign_artifact.artifact_id = Some(bcode_ipc::ArtifactId::current());
    assert!(target.verify_status(&foreign_artifact).is_err());
    eprintln!("independent phase: session_path_begin");
    independent_session_path(&client, directory.clone()).await;
    eprintln!("independent phase: replacement_shutdown_begin");
    assert_replaced_target_and_shutdown(
        &client,
        &target,
        &executable,
        &digest,
        directory,
        &mut daemon,
    )
    .await;
}

async fn independent_session_path(
    client: &bcode_client::BcodeClient,
    directory: std::path::PathBuf,
) {
    use bcode_session_models::{SessionObservationApplication as _, SessionObservationEvent};
    eprintln!("independent phase: session_create_begin");
    let session = SessionApplication::create_session(
        client,
        CreateSessionRequest {
            name: Some("independent consumer".into()),
            working_directory: directory.clone(),
        },
    )
    .await
    .unwrap();
    eprintln!("independent phase: session_agent_begin");
    SessionApplication::set_session_agent(client, session.id, "build".into())
        .await
        .unwrap();
    eprintln!("independent phase: session_model_begin");
    SessionApplication::set_session_model(
        client,
        session.id,
        Some("bcode.fake-provider".into()),
        "fake-echo".into(),
    )
    .await
    .unwrap();
    // Both handles retain independently authenticated sockets targeting the selected artifact.
    eprintln!("independent phase: controller_connect_begin");
    let controller = client.interaction_controller_consumer().await.unwrap();
    let mut observer = client
        .observe_session(observation_request(session.id))
        .await
        .unwrap();
    let initial = tokio::time::timeout(Duration::from_secs(5), observer.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initial.sequence, 0);
    assert!(matches!(
        initial.event,
        SessionObservationEvent::Snapshot { initial: true, .. }
    ));
    let prompt = r#"tool-call question {"questions":[{"header":"Pick","question":"Pick one","required":true,"custom":false,"control":"radio","selection_mode":"single","options":[{"label":"One","value":"one","description":"First"},{"label":"Two","value":"two","description":"Second"}]}]}"#;
    let admission = SessionApplication::submit_turn(
        client,
        session.id,
        prompt.into(),
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    let TurnAdmission::Accepted(receipt) = admission else {
        panic!("question admission failed: {admission:?}");
    };
    eprintln!("independent phase: question_wait_begin");
    let exchange = wait_independent_question(client, &controller, session.id, &receipt).await;
    eprintln!("independent phase: question_answer_begin");
    answer_independent_question(&controller, exchange).await;
    eprintln!("independent phase: question_terminal_begin");
    terminal(
        client,
        session.id,
        &receipt.turn_id.to_string(),
        bcode_session_models::ModelTurnOutcome::Completed,
    )
    .await;
    let update = tokio::time::timeout(Duration::from_secs(5), observer.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(update.sequence > initial.sequence);
    assert!(matches!(
        update.event,
        SessionObservationEvent::Snapshot { initial: false, .. }
    ));
    observer.close();
    drop(controller);
    eprintln!("independent phase: cancel_spawn_begin");
    independent_cancel_and_spawn(client, session.id, directory).await;
    eprintln!("independent phase: session_path_complete");
}

fn run_complete_case(test_name: &str, mode: &str) {
    const CHILD: &str = "BCODE_COMPLETE_IPC_CHILD";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Runtime::new().unwrap().block_on(host());
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env_clear()
        .env(CHILD, "1")
        .env("BCODE_COMPLETE_CASE", mode)
        .env("HOME", root.path())
        .env("BCODE_STATE_DIR", root.path().join("state"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .current_dir(root.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "IPC acceptance child failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("IPC acceptance timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn history(
    client: &bcode_client::BcodeClient,
    id: SessionId,
) -> Vec<bcode_session_models::SessionEvent> {
    let page = SessionApplication::session_history_page(
        client,
        id,
        bcode_session_models::SessionHistoryQuery {
            cursor: None,
            limit: 256,
            direction: bcode_session_models::SessionHistoryDirection::Backward,
        },
    )
    .await
    .unwrap();
    assert!(!page.has_more, "fixture exceeded bounded canonical history");
    page.events
}

async fn terminal(
    client: &bcode_client::BcodeClient,
    id: SessionId,
    turn: &str,
    expected: bcode_session_models::ModelTurnOutcome,
) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            for event in history(client, id).await {
                if let SessionEventKind::ModelTurnFinished {
                    turn_id, outcome, ..
                } = event.kind
                    && turn_id == turn
                {
                    assert_eq!(outcome, expected);
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("canonical terminal outcome missing");
}

fn observation_request(id: SessionId) -> bcode_session_models::SessionObservationRequest {
    bcode_session_models::SessionObservationRequest {
        schema_version: bcode_session_models::SESSION_OBSERVATION_SCHEMA_VERSION,
        session_id: id,
        window: bcode_session_models::ProjectionWindowRequest {
            projection: bcode_session_models::SessionProjectionKind::Transcript,
            anchor: bcode_session_models::ProjectionWindowAnchor::Latest,
            direction: bcode_session_models::ProjectionWindowDirection::Backward,
            target: bcode_session_models::ProjectionWindowTarget {
                min_items: Some(8),
                ..Default::default()
            },
            limits: bcode_session_models::ProjectionWindowLimits {
                max_items: 16,
                max_events_scanned: 64,
                max_bytes: bcode_session_models::observation::MAX_HOSTED_OBSERVATION_BYTES,
            },
        },
        max_updates: 64,
        max_snapshot_bytes: bcode_session_models::observation::MAX_HOSTED_OBSERVATION_BYTES,
    }
}

fn codec_category(error: &bcode_ipc::CodecError) -> &'static str {
    match error {
        bcode_ipc::CodecError::PayloadTooLarge { .. } => "payload_too_large",
        bcode_ipc::CodecError::InvalidChunk(_) => "invalid_chunk",
        bcode_ipc::CodecError::Io(_) => "io",
        bcode_ipc::CodecError::Serialize(_) => "serialize",
        bcode_ipc::CodecError::Deserialize(_) => "deserialize",
        bcode_ipc::CodecError::EventConversion(_) => "event_conversion",
        bcode_ipc::CodecError::UnsupportedVersion { .. } => "unsupported_version",
    }
}

async fn ipc_response(
    stream: &mut tokio::net::UnixStream,
    sequence: u64,
    request: &bcode_ipc::Request,
) -> bcode_ipc::Response {
    let envelope = bcode_ipc::request_envelope(sequence, request).unwrap();
    bcode_ipc::send_envelope(stream, &envelope).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let envelope = bcode_ipc::recv_envelope(stream)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "raw IPC receive codec={}; request={sequence}",
                        codec_category(&error)
                    )
                });
            if envelope.request_id == sequence && envelope.kind == bcode_ipc::EnvelopeKind::Response
            {
                return bcode_ipc::decode_response(&envelope.payload).unwrap_or_else(|error| {
                    panic!(
                        "raw IPC response codec={}; request={sequence}",
                        codec_category(&error)
                    )
                });
            }
        }
    })
    .await
    .unwrap()
}

async fn observation_socket() -> tokio::net::UnixStream {
    framed_socket(None).await
}

async fn framed_socket(
    runtime_context: Option<bcode_ipc::ClientRuntimeContext>,
) -> tokio::net::UnixStream {
    let mut stream =
        tokio::net::UnixStream::connect(std::env::current_dir().unwrap().join("complete.sock"))
            .await
            .unwrap();
    let hello = bcode_ipc::Request::Hello {
        client_name: "observation-acceptance".into(),
        runtime_context,
        daemon_namespace: bcode_ipc::daemon_namespace(),
        artifact_id: Some(bcode_ipc::ArtifactId::current()),
        build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
        state_location_id: Some(bcode_ipc::state_location_id()),
    };
    assert!(matches!(
        ipc_response(&mut stream, 1, &hello).await,
        bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello { .. })
    ));
    stream
}

fn assert_ipc_error(response: bcode_ipc::Response, code: &str) {
    let bcode_ipc::Response::Err(error) = response else {
        panic!("expected {code}, got {response:?}");
    };
    assert_eq!(error.code, code);
}

async fn hosted_observation_ipc_errors(id: SessionId) {
    use bcode_ipc::{Request, Response, ResponsePayload};
    use bcode_session_models::observation::{
        MAX_CLIENT_SESSION_OBSERVATIONS, SessionObservationPullRequest,
    };
    let mut owner = observation_socket().await;
    let mut foreign = observation_socket().await;
    let mut invalid_open = observation_request(id);
    invalid_open.schema_version += 1;
    assert_ipc_error(
        ipc_response(
            &mut owner,
            2,
            &Request::OpenSessionObservation {
                request: invalid_open,
            },
        )
        .await,
        "session_observation_unsupported_schema",
    );
    let mut invalid_open = observation_request(id);
    invalid_open.max_snapshot_bytes += 1;
    assert_ipc_error(
        ipc_response(
            &mut owner,
            3,
            &Request::OpenSessionObservation {
                request: invalid_open,
            },
        )
        .await,
        "session_observation_invalid_request",
    );
    let mut handles = Vec::new();
    for index in 0..MAX_CLIENT_SESSION_OBSERVATIONS {
        let response = ipc_response(
            &mut owner,
            10 + index as u64,
            &Request::OpenSessionObservation {
                request: observation_request(id),
            },
        )
        .await;
        let Response::Ok(ResponsePayload::SessionObservationOpened { handle }) = response else {
            panic!("open failed: {response:?}");
        };
        handles.push(handle);
    }
    assert_ipc_error(
        ipc_response(
            &mut owner,
            30,
            &Request::OpenSessionObservation {
                request: observation_request(id),
            },
        )
        .await,
        "session_observation_limit_reached",
    );
    let handle = handles.remove(0);
    let pull = SessionObservationPullRequest {
        schema_version: 1,
        handle_id: handle.handle_id.clone(),
        expected_sequence: 0,
        wait_ms: 1,
    };
    assert_ipc_error(
        ipc_response(
            &mut foreign,
            31,
            &Request::PullSessionObservation {
                request: pull.clone(),
            },
        )
        .await,
        "session_observation_unauthorized",
    );
    assert_ipc_error(
        ipc_response(
            &mut foreign,
            32,
            &Request::CloseSessionObservation {
                handle: handle.clone(),
            },
        )
        .await,
        "session_observation_unauthorized",
    );
    let mut bad = pull.clone();
    bad.schema_version += 1;
    assert_ipc_error(
        ipc_response(
            &mut owner,
            33,
            &Request::PullSessionObservation { request: bad },
        )
        .await,
        "session_observation_unsupported_schema",
    );
    let mut bad = pull.clone();
    bad.expected_sequence = 8;
    assert_ipc_error(
        ipc_response(
            &mut owner,
            34,
            &Request::PullSessionObservation { request: bad },
        )
        .await,
        "session_observation_conflict",
    );
    let mut bad = pull.clone();
    bad.handle_id = "unissued-token".into();
    assert_ipc_error(
        ipc_response(
            &mut owner,
            35,
            &Request::PullSessionObservation { request: bad },
        )
        .await,
        "session_observation_not_found",
    );
    assert!(matches!(
        ipc_response(
            &mut owner,
            36,
            &Request::CloseSessionObservation {
                handle: handle.clone()
            }
        )
        .await,
        Response::Ok(ResponsePayload::SessionObservationClosed { closed: true })
    ));
    assert!(matches!(
        ipc_response(
            &mut owner,
            37,
            &Request::OpenSessionObservation {
                request: observation_request(id)
            }
        )
        .await,
        Response::Ok(ResponsePayload::SessionObservationOpened { .. })
    ));
    // Send a valid initial pull and intentionally abandon its reply. A new
    // connection cannot reuse this identity, whether cleanup won the race or not.
    let lost = handles.remove(0);
    let envelope = bcode_ipc::request_envelope(
        38,
        &Request::PullSessionObservation {
            request: SessionObservationPullRequest {
                handle_id: lost.handle_id.clone(),
                ..pull
            },
        },
    )
    .unwrap();
    bcode_ipc::send_envelope(&mut owner, &envelope)
        .await
        .unwrap();
    drop(owner);
    let response = ipc_response(
        &mut foreign,
        39,
        &Request::PullSessionObservation {
            request: SessionObservationPullRequest {
                schema_version: 1,
                handle_id: lost.handle_id,
                expected_sequence: 0,
                wait_ms: 1,
            },
        },
    )
    .await;
    let Response::Err(error) = response else {
        panic!("lost owner token remained usable: {response:?}");
    };
    assert!(matches!(
        error.code.as_str(),
        "session_observation_not_found" | "session_observation_unauthorized"
    ));
}

async fn observation_error_and_cleanup_paths(client: &bcode_client::BcodeClient, id: SessionId) {
    hosted_observation_ipc_errors(id).await;
    use bcode_session_models::{
        SessionObservationApplication as _, SessionObservationEnd, SessionObservationError,
        SessionObservationEvent,
    };
    let request = observation_request(id);
    let mut invalid = request.clone();
    invalid.schema_version += 1;
    assert_eq!(
        client.observe_session(invalid).await.unwrap_err(),
        SessionObservationError::UnsupportedSchema
    );
    let mut oversized = request.clone();
    oversized.max_snapshot_bytes += 1;
    assert_eq!(
        client.observe_session(oversized).await.unwrap_err(),
        SessionObservationError::InvalidRequest
    );
    let mut tiny = request.clone();
    tiny.max_snapshot_bytes = 1;
    let mut limited = client.observe_session(tiny).await.unwrap();
    let end = limited.recv().await.unwrap();
    assert!(matches!(
        end.event,
        SessionObservationEvent::Ended {
            reason: SessionObservationEnd::LimitReached
        }
    ));
    assert!(limited.recv().await.is_none());
    drop(limited);

    // Resync means a new bounded initial snapshot, not replay of the old stream.
    let mut fresh = client.observe_session(request.clone()).await.unwrap();
    let initial = fresh.recv().await.unwrap();
    assert_eq!(initial.sequence, 0);
    assert!(matches!(
        initial.event,
        SessionObservationEvent::Snapshot { initial: true, .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), fresh.recv())
            .await
            .is_err()
    );
    fresh.close();
    assert!(matches!(
        fresh.recv().await.unwrap().event,
        SessionObservationEvent::Ended {
            reason: SessionObservationEnd::Closed
        }
    ));
    assert!(fresh.recv().await.is_none());
    drop(fresh);

    let mut dropped = client.observe_session(request.clone()).await.unwrap();
    dropped.recv().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), dropped.recv())
            .await
            .is_err()
    );
    drop(dropped);
    // A fresh connection must continue to admit and project after the pending
    // connection was dropped. Owner-specific registry release is tested separately.
    let mut after_drop = client.observe_session(request).await.unwrap();
    assert!(matches!(
        after_drop.recv().await.unwrap().event,
        SessionObservationEvent::Snapshot { initial: true, .. }
    ));
    after_drop.close();
}

async fn interaction_cancellation_path(
    client: &bcode_client::BcodeClient,
    id: SessionId,
    prompt: &str,
) {
    let controller = client.interaction_controller_consumer().await.unwrap();
    let admission = SessionApplication::submit_turn(
        client,
        id,
        prompt.into(),
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    let TurnAdmission::Accepted(receipt) = admission else {
        panic!("cancel fixture admission failed: {admission:?}");
    };
    let exchange = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            for permission in controller.pending_permissions(id).await.unwrap() {
                controller
                    .resolve_permission(id, &permission.permission_id, true, false)
                    .await
                    .unwrap();
            }
            if let Some(pending) = controller
                .pending_exchanges(id)
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                break pending.request.exchange_id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let opened = controller.open(1, &exchange).await.unwrap();
    assert!(
        SessionApplication::cancel_session_turn(
            client,
            id,
            true,
            Some(receipt.turn_id.to_string())
        )
        .await
        .unwrap()
    );
    terminal(
        client,
        id,
        &receipt.turn_id.to_string(),
        bcode_session_models::ModelTurnOutcome::Cancelled,
    )
    .await;
    assert!(
        controller
            .input(InteractionControllerInput {
                version: 1,
                exchange_id: exchange.clone(),
                expected_revision: opened.revision,
                input: serde_json::from_value(serde_json::json!({"type":"submit"})).unwrap(),
            })
            .await
            .is_err(),
        "cancelled exchange accepted a stale effect"
    );
    assert!(controller.pending_exchanges(id).await.unwrap().is_empty());
    assert!(controller.open(1, &exchange).await.is_err());
}

async fn consumer(client: bcode_client::BcodeClient, effective_config: String) {
    let directory = std::env::current_dir().unwrap();
    let parent = SessionApplication::create_session(
        &client,
        CreateSessionRequest {
            name: Some("persistent IPC parent".into()),
            working_directory: directory.clone(),
        },
    )
    .await
    .unwrap();
    observation_error_and_cleanup_paths(&client, parent.id).await;
    SessionApplication::set_session_agent(&client, parent.id, "build".into())
        .await
        .unwrap();
    SessionApplication::set_session_model(
        &client,
        parent.id,
        Some("bcode.fake-provider".into()),
        "fake-echo".into(),
    )
    .await
    .unwrap();
    let request = bcode_session_models::SessionSpawnRequest {
        version: 1,
        creation: bcode_session_models::SessionChildCreateRequest {
            parent_id: parent.id,
            child_id: SessionId::new(),
            name: Some("IPC child".into()),
            working_directory: directory,
        },
        worktree_directory: None,
        input: "initial IPC echo".into(),
        admission: TurnAdmissionMetadata::default(),
        auth_profile: None,
    };
    let mut unavailable = request.clone();
    unavailable.creation.child_id = SessionId::new();
    unavailable.auth_profile = Some("unavailable-acceptance-auth".into());
    let failure = SessionApplication::spawn_session(&client, unavailable.clone()).await;
    assert_eq!(
        failure.failure.unwrap().stage,
        bcode_session_models::SessionSpawnStage::Preparation
    );
    assert!(failure.session.is_none());
    assert!(
        SessionApplication::session_summary(&client, unavailable.creation.child_id)
            .await
            .is_err()
    );
    let first = SessionApplication::spawn_session(&client, request.clone()).await;
    first.validate().unwrap();
    let id = first.session.as_ref().unwrap().id;
    let Some(TurnAdmission::Accepted(receipt)) = first.admission else {
        panic!("actual spawn admission required: {first:?}");
    };
    let duplicate = SessionApplication::spawn_session(&client, request.clone()).await;
    assert!(
        matches!(duplicate.admission, Some(TurnAdmission::Existing(ref other) | TurnAdmission::Deferred(ref other)) if other == &receipt)
    );
    let mut conflict = request;
    conflict.input = "conflicting immutable IPC input".into();
    let failure = SessionApplication::spawn_session(&client, conflict).await;
    assert_eq!(
        failure.failure.unwrap().stage,
        bcode_session_models::SessionSpawnStage::Creation
    );
    assert_eq!(failure.session.unwrap().id, id);
    terminal(
        &client,
        id,
        &receipt.turn_id.to_string(),
        bcode_session_models::ModelTurnOutcome::Completed,
    )
    .await;

    // Hold one authenticated controller socket for the entire live question.
    let controller = client.interaction_controller_consumer().await.unwrap();
    let prompt = r#"tool-call question {"questions":[{"header":"Pick","question":"Pick one","required":true,"custom":false,"control":"radio","selection_mode":"single","options":[{"label":"One","value":"one","description":"First"},{"label":"Two","value":"two","description":"Second"}]}]}"#;
    let admission = SessionApplication::submit_turn(
        &client,
        id,
        prompt.into(),
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    let TurnAdmission::Accepted(question_receipt) = admission else {
        panic!("question turn not accepted: {admission:?}");
    };
    let exchange = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            for permission in controller.pending_permissions(id).await.unwrap() {
                controller
                    .resolve_permission(id, &permission.permission_id, true, false)
                    .await
                    .unwrap();
            }
            if let Some(exchange) = controller
                .pending_exchanges(id)
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                return exchange.request.exchange_id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("real question exchange unavailable");
    assert!(
        controller.open(2, &exchange).await.is_err(),
        "future controller version was interpreted as current"
    );
    // An incompatible open must not consume the pending exchange or prevent a
    // subsequent supported open on the same authenticated connection.
    assert!(
        controller
            .pending_exchanges(id)
            .await
            .unwrap()
            .iter()
            .any(|pending| pending.request.exchange_id == exchange)
    );
    let opened = controller.open(1, &exchange).await.unwrap();
    let activate = InteractionControllerInput {
        version: 1,
        exchange_id: exchange.clone(),
        expected_revision: opened.revision,
        input: serde_json::from_value(
            serde_json::json!({"type":"activate","control_id":"question-0.option-1"}),
        )
        .unwrap(),
    };
    let selected = controller.input(activate.clone()).await.unwrap();
    assert_eq!(
        controller.input(activate.clone()).await.unwrap(),
        selected,
        "exact duplicate changed the committed receipt"
    );
    let mut conflicting = activate;
    conflicting.input = serde_json::from_value(
        serde_json::json!({"type":"activate","control_id":"question-0.option-0"}),
    )
    .unwrap();
    assert!(
        controller.input(conflicting).await.is_err(),
        "conflicting input with stale revision was reapplied"
    );
    let completed = controller
        .input(InteractionControllerInput {
            version: 1,
            exchange_id: exchange.clone(),
            expected_revision: selected.revision,
            input: serde_json::from_value(serde_json::json!({"type":"submit"})).unwrap(),
        })
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&completed.output)
            .unwrap()
            .contains("two")
    );
    terminal(
        &client,
        id,
        &question_receipt.turn_id.to_string(),
        bcode_session_models::ModelTurnOutcome::Completed,
    )
    .await;
    let events = history(&client, id).await;
    let resolutions: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let SessionEventKind::ToolExchangeResolved { event } = &event.kind
                && event.exchange_id == exchange
            {
                Some(event)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(resolutions.len(), 1);
    assert!(
        serde_json::to_string(&resolutions[0].resolution)
            .unwrap()
            .contains("two")
    );
    drop(controller);
    let fresh = client.interaction_controller_consumer().await.unwrap();
    assert!(
        fresh
            .input(InteractionControllerInput {
                version: 1,
                exchange_id: exchange,
                expected_revision: selected.revision,
                input: serde_json::from_value(serde_json::json!({"type":"submit"})).unwrap(),
            })
            .await
            .is_err(),
        "detached terminal controller accepted stale effects"
    );

    interaction_cancellation_path(&client, id, prompt).await;

    let mut observer = bcode_session_models::SessionObservationApplication::observe_session(
        &client,
        bcode_session_models::SessionObservationRequest {
            schema_version: bcode_session_models::SESSION_OBSERVATION_SCHEMA_VERSION,
            session_id: id,
            window: bcode_session_models::ProjectionWindowRequest {
                projection: bcode_session_models::SessionProjectionKind::Transcript,
                anchor: bcode_session_models::ProjectionWindowAnchor::Latest,
                direction: bcode_session_models::ProjectionWindowDirection::Backward,
                target: bcode_session_models::ProjectionWindowTarget {
                    min_items: Some(32),
                    ..Default::default()
                },
                limits: bcode_session_models::ProjectionWindowLimits {
                    max_items: 64,
                    max_events_scanned: 256,
                    max_bytes: 1024 * 1024,
                },
            },
            max_updates: 4096,
            max_snapshot_bytes: bcode_session_models::observation::MAX_HOSTED_OBSERVATION_BYTES,
        },
    )
    .await
    .unwrap();
    let mut raw = framed_socket(Some(bcode_ipc::ClientRuntimeContext {
        effective_config_toml: Some(Box::new(effective_config)),
        selected_provider_plugin_id: Some("bcode.fake-provider".into()),
        selected_model_id: Some("fake-echo".into()),
        interaction_controller_versions: std::collections::BTreeSet::from([1]),
        ..Default::default()
    }))
    .await;
    let mut raw_request = observation_request(id);
    raw_request.window.target.min_items = Some(32);
    raw_request.window.limits.max_items = 64;
    raw_request.window.limits.max_events_scanned = 256;
    raw_request.max_updates = 4096;
    let opened = ipc_response(
        &mut raw,
        70,
        &bcode_ipc::Request::OpenSessionObservation {
            request: raw_request,
        },
    )
    .await;
    let bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionObservationOpened { handle }) =
        opened
    else {
        match opened {
            bcode_ipc::Response::Err(error) => {
                panic!("raw observation open error code={}", error.code)
            }
            _ => panic!("raw observation open unexpected response"),
        }
    };
    let pulled = ipc_response(
        &mut raw,
        71,
        &bcode_ipc::Request::PullSessionObservation {
            request: bcode_session_models::SessionObservationPullRequest {
                schema_version: 1,
                handle_id: handle.handle_id.clone(),
                expected_sequence: 0,
                wait_ms: 1000,
            },
        },
    )
    .await;
    match pulled {
        bcode_ipc::Response::Err(error) => panic!("raw observation pull error code={}", error.code),
        bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionObservationPulled {
            result,
        }) => {
            let delivery = result
                .delivery
                .expect("raw observation initial delivery absent");
            match delivery.event {
                bcode_session_models::SessionObservationEvent::Snapshot { snapshot, initial } => {
                    assert!(initial);
                    let snapshot_json =
                        serde_json::to_vec(&snapshot).expect("local snapshot encode failed");
                    let reconstructed = bcode_ipc::Response::Ok(
                        bcode_ipc::ResponsePayload::SessionObservationPulled {
                            result: Box::new(bcode_session_models::SessionObservationPullResult {
                                schema_version: 1,
                                handle_id: handle.handle_id.clone(),
                                finished: false,
                                delivery: Some(bcode_session_models::SessionObservationEnvelope {
                                    schema_version: 1,
                                    session_id: id,
                                    sequence: 0,
                                    source_sequence: delivery.source_sequence,
                                    event:
                                        bcode_session_models::SessionObservationEvent::Snapshot {
                                            initial: true,
                                            snapshot: serde_json::from_slice(&snapshot_json)
                                                .expect("local snapshot JSON decode failed"),
                                        },
                                }),
                            }),
                        },
                    );
                    let encoded = bcode_ipc::encode_response(&reconstructed)
                        .expect("local snapshot wire encode failed");
                    let decoded = bcode_ipc::decode_response(&encoded)
                        .unwrap_or_else(|_| panic!("local snapshot wire codec roundtrip failed"));
                    assert_eq!(decoded, reconstructed);
                }
                bcode_session_models::SessionObservationEvent::Ended { reason } => {
                    panic!("raw initial terminal={reason:?}")
                }
            }
        }
        _ => panic!("raw observation pull unexpected response"),
    }
    ipc_response(
        &mut raw,
        72,
        &bcode_ipc::Request::CloseSessionObservation { handle },
    )
    .await;
    drop(raw);
    let initial = observer.recv().await.expect("initial IPC observation");
    match initial.event {
        bcode_session_models::SessionObservationEvent::Snapshot { initial: true, .. } => {}
        bcode_session_models::SessionObservationEvent::Ended { reason } => {
            panic!(
                "initial IPC observation ended: {reason:?}; sequence={}; source={:?}",
                initial.sequence, initial.source_sequence
            );
        }
        _ => panic!("initial IPC observation was not an initial snapshot"),
    }
    let admission = SessionApplication::submit_turn(
        &client,
        id,
        "followup after real question".into(),
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    let TurnAdmission::Accepted(receipt) = admission else {
        panic!("followup not accepted: {admission:?}");
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let delivery = observer.recv().await.expect("live IPC observation");
            if let bcode_session_models::SessionObservationEvent::Ended { reason } = &delivery.event
            {
                panic!(
                    "live IPC observation ended: {reason:?}; sequence={}; source={:?}",
                    delivery.sequence, delivery.source_sequence
                );
            }
            if let bcode_session_models::SessionObservationEvent::Snapshot { snapshot, .. } =
                delivery.event
                && snapshot.runtime.active_turn_id.as_deref()
                    == Some(receipt.turn_id.to_string().as_str())
            {
                break;
            }
        }
    })
    .await
    .expect("followup was not observed active");
    drop(observer); // Observer teardown does not cancel the admitted provider work.
    terminal(
        &client,
        id,
        &receipt.turn_id.to_string(),
        bcode_session_models::ModelTurnOutcome::Completed,
    )
    .await;
    let admission = SessionApplication::submit_turn(
        &client,
        id,
        "cancel exact IPC turn".into(),
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    let TurnAdmission::Accepted(receipt) = admission else {
        panic!("cancellation turn not accepted: {admission:?}");
    };
    assert!(
        SessionApplication::cancel_session_turn(
            &client,
            id,
            false,
            Some(receipt.turn_id.to_string())
        )
        .await
        .unwrap()
    );
    terminal(
        &client,
        id,
        &receipt.turn_id.to_string(),
        bcode_session_models::ModelTurnOutcome::Cancelled,
    )
    .await;
}

async fn restricted_controller_consumer(
    client: bcode_client::BcodeClient,
    mode: &str,
    effective_config: String,
) {
    let session = SessionApplication::create_session(
        &client,
        CreateSessionRequest {
            name: Some(format!("{mode} controller fixture")),
            working_directory: std::env::current_dir().unwrap(),
        },
    )
    .await
    .unwrap();
    SessionApplication::set_session_agent(&client, session.id, "build".into())
        .await
        .unwrap();
    if mode == "deny" {
        let controller = client.interaction_controller_consumer().await.unwrap();
        let admission = SessionApplication::submit_turn(
            &client,
            session.id,
            r#"tool-call shell.run {"command":"printf forbidden-effect > denied-effect"}"#.into(),
            TurnAdmissionMetadata::default(),
        )
        .await
        .unwrap();
        let TurnAdmission::Accepted(receipt) = admission else {
            panic!("deny admission failed: {admission:?}");
        };
        let permission = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(permission) = controller.pending_permissions(session.id).await.unwrap().into_iter().next() { break permission; }
                let events = history(&client, session.id).await;
                assert!(!events.iter().any(|event| matches!(&event.kind,
                    SessionEventKind::ModelTurnFinished { turn_id, .. } if turn_id == &receipt.turn_id.to_string())),
                    "permission fixture terminalized before requesting approval; recent bounded kinds={:?}",
                    events.iter().rev().take(8).map(|event| match &event.kind {
                        SessionEventKind::ModelTurnFinished { outcome, .. } => format!("terminal:{outcome:?}"),
                        kind => format!("{:?}", std::mem::discriminant(kind)),
                    }).collect::<Vec<_>>());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap_or_else(|error| panic!("permission fixture timed out: {error}; marker_exists={}", std::env::current_dir().unwrap().join("denied-effect").exists()));
        assert!(
            !std::env::current_dir()
                .unwrap()
                .join("denied-effect")
                .exists()
        );
        controller
            .resolve_permission(session.id, &permission.permission_id, false, false)
            .await
            .unwrap();
        assert!(
            !controller
                .resolve_permission(session.id, &permission.permission_id, true, false)
                .await
                .unwrap(),
            "stale permission approval was accepted"
        );
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if history(&client, session.id).await.iter().any(|event| matches!(&event.kind,
                    SessionEventKind::ModelTurnFinished { turn_id, .. } if turn_id == &receipt.turn_id.to_string())) { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let decisions: Vec<_> = history(&client, session.id)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::PermissionResolved {
                    permission_id,
                    approved,
                } if permission_id == permission.permission_id => Some(approved),
                _ => None,
            })
            .collect();
        assert_eq!(decisions, vec![false]);
        assert!(
            !std::env::current_dir()
                .unwrap()
                .join("denied-effect")
                .exists()
        );
        assert!(
            controller
                .pending_permissions(session.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            controller
                .pending_exchanges(session.id)
                .await
                .unwrap()
                .is_empty()
        );
        return;
    }
    let controller = client.interaction_controller_consumer().await.unwrap();
    let admission = SessionApplication::submit_turn(&client, session.id,
        r#"tool-call question {"questions":[{"header":"Pick","question":"Pick one","required":true,"custom":false,"control":"radio","selection_mode":"single","options":[{"label":"One","value":"one","description":"First"}]}]}"#.into(),
        TurnAdmissionMetadata::default()).await.unwrap();
    let TurnAdmission::Accepted(receipt) = admission else {
        panic!("restricted admission failed: {admission:?}");
    };
    if mode == "incompatible" {
        let exchange = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                for permission in controller.pending_permissions(session.id).await.unwrap() {
                    controller
                        .resolve_permission(session.id, &permission.permission_id, true, false)
                        .await
                        .unwrap();
                }
                if let Some(pending) = controller
                    .pending_exchanges(session.id)
                    .await
                    .unwrap()
                    .into_iter()
                    .next()
                {
                    break pending.request.exchange_id;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let mut incompatible = framed_socket(Some(bcode_ipc::ClientRuntimeContext {
            effective_config_toml: Some(Box::new(effective_config)),
            selected_provider_plugin_id: Some("bcode.fake-provider".into()),
            selected_model_id: Some("fake-echo".into()),
            interaction_controller_versions: std::collections::BTreeSet::from([2]),
            ..Default::default()
        }))
        .await;
        let response = ipc_response(
            &mut incompatible,
            50,
            &bcode_ipc::Request::OpenInteractionController {
                version: 1,
                exchange_id: exchange.clone(),
            },
        )
        .await;
        assert!(
            matches!(response, bcode_ipc::Response::Err(_)),
            "unsupported negotiated controller opened: {response:?}"
        );
        assert!(
            SessionApplication::cancel_session_turn(
                &client,
                session.id,
                true,
                Some(receipt.turn_id.to_string())
            )
            .await
            .unwrap()
        );
        terminal(
            &client,
            session.id,
            &receipt.turn_id.to_string(),
            bcode_session_models::ModelTurnOutcome::Cancelled,
        )
        .await;
        assert!(
            controller
                .pending_exchanges(session.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(controller.open(1, &exchange).await.is_err());
    } else {
        // An unavailable optional tool must not leave an interaction resident.
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if history(&client, session.id).await.iter().any(|event| matches!(&event.kind,
                    SessionEventKind::ModelTurnFinished { turn_id, .. } if turn_id == &receipt.turn_id.to_string())) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        assert!(
            controller
                .pending_exchanges(session.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(controller.open(1, "unavailable-question").await.is_err());
    }
    // Independent core session operations remain usable after either rejection.
    SessionApplication::rename_session(&client, session.id, Some("core still available".into()))
        .await
        .unwrap();
}

async fn host() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();
    let directory = std::env::current_dir().unwrap();
    let mode = std::env::var("BCODE_COMPLETE_CASE").unwrap_or_else(|_| "full".into());
    let mut ids = vec![
        "bcode.fake-provider".to_owned(),
        "bcode.default-agents".to_owned(),
        "bcode.question".to_owned(),
    ];
    if mode == "deny" {
        ids.push("bcode.shell".into());
    }
    if mode == "disabled" {
        ids.retain(|id| id != "bcode.question");
    }
    let mut config = bcode_config::BcodeConfig::default();
    config.model.provider_plugin_id = Some("bcode.fake-provider".into());
    config.model.model_id = Some("fake-echo".into());
    config.plugins.default = bcode_config::PluginDefaultMode::None;
    config.plugins.enabled = ids.iter().cloned().collect();
    // Explicitly disable implicit provider defaults that this offline fixture
    // does not load. Resolve exactly the same selection for host and handshake.
    let implicit = bcode_config::plugin_selection_with_default_plugin_ids(&config, &ids);
    config.plugins.disabled = implicit
        .enabled
        .difference(&config.plugins.enabled)
        .cloned()
        .collect();
    let selection = bcode_config::plugin_selection_with_default_plugin_ids(&config, &ids);
    assert_eq!(selection.enabled, config.plugins.enabled);
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &selection,
        &bcode_bundled_plugins::static_bundled_plugins(),
    )
    .unwrap();
    assert_eq!(plugins.selection(), &selection);
    config
        .agent
        .entry("build".into())
        .or_default()
        .permission
        .command
        .insert(
            "*".into(),
            serde_json::from_value(serde_json::json!("ask")).unwrap(),
        );
    if mode == "deny" {
        config
            .agent
            .entry("build".into())
            .or_default()
            .permission
            .write
            .insert(
                "denied-effect".into(),
                serde_json::from_value(serde_json::json!("ask")).unwrap(),
            );
    }
    config
        .model
        .settings
        .insert("fake_prompt_tool_directives".into(), "true".into());
    config.model.prompt_cache.mode = serde_json::from_value(serde_json::json!("off")).unwrap();
    config
        .model
        .settings
        .insert("fake_turn_delay_ms".into(), "2000".into());
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.join("complete.sock"));
    let client = bcode_client::BcodeClient::new(endpoint.clone()).with_runtime_context(Some(
        bcode_ipc::ClientRuntimeContext {
            effective_config_toml: Some(Box::new(
                bcode_config::encode_effective_config(&config).unwrap(),
            )),
            selected_provider_plugin_id: Some("bcode.fake-provider".into()),
            selected_model_id: Some("fake-echo".into()),
            interaction_controller_versions: if mode == "incompatible" {
                std::collections::BTreeSet::from([2])
            } else {
                std::collections::BTreeSet::from([1])
            },
            ..Default::default()
        },
    ));
    let effective_config = bcode_config::encode_effective_config(&config).unwrap();
    let shutdown = bcode_agent_runtime::CancellationToken::new();
    let signal = shutdown.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let ready: bcode_server::EmbeddedWorkflowReady = Box::new(move |_| {
        Box::pin(async move {
            let task = tokio::spawn(async move {
                if mode == "full" {
                    consumer(client, effective_config).await;
                } else {
                    restricted_controller_consumer(client, &mode, effective_config).await;
                }
            });
            tokio::spawn(async move {
                let result = task.await;
                signal.cancel();
                let _ = sender.send(result);
            });
        })
    });
    bcode_server::run_embedded_with_workflow_application(
        endpoint,
        config,
        plugins,
        bcode_model_catalog::ModelCatalogResolver::embedded(),
        ids,
        shutdown,
        ready,
    )
    .await
    .unwrap();
    receiver.await.unwrap().unwrap();
}

fn independent_fake_configuration() -> (bcode_config::BcodeConfig, Vec<String>) {
    let ids = vec![
        "bcode.fake-provider".to_owned(),
        "bcode.default-agents".to_owned(),
        "bcode.question".to_owned(),
    ];
    let mut config = bcode_config::BcodeConfig::default();
    config.model.provider_plugin_id = Some("bcode.fake-provider".into());
    config.model.model_id = Some("fake-echo".into());
    config.plugins.default = bcode_config::PluginDefaultMode::None;
    config.plugins.enabled = ids.iter().cloned().collect();
    let implicit = bcode_config::plugin_selection_with_default_plugin_ids(&config, &ids);
    config.plugins.disabled = implicit
        .enabled
        .difference(&config.plugins.enabled)
        .cloned()
        .collect();
    config.model.profile = Some("independent-fake".into());
    config.model.profiles.insert(
        "independent-fake".into(),
        bcode_config::ModelProfileConfig {
            provider_plugin_id: "bcode.fake-provider".into(),
            model_id: Some("fake-echo".into()),
            ..Default::default()
        },
    );
    config
        .model
        .settings
        .insert("fake_prompt_tool_directives".into(), "true".into());
    config
        .model
        .settings
        .insert("fake_turn_delay_ms".into(), "1000".into());
    config.model.prompt_cache.mode = serde_json::from_value(serde_json::json!("off")).unwrap();
    (config, ids)
}

async fn assert_independent_target_rejections(
    executable: &std::path::Path,
    digest: &str,
    location: &bcode_config::StateLocation,
    config_directory: &std::path::Path,
    config: &bcode_config::BcodeConfig,
    directory: &std::path::Path,
) {
    use bcode_daemon_lifecycle::{DaemonTarget, DaemonTargetError};
    eprintln!("independent phase: reject_wrong_digest_begin");
    assert!(matches!(
        DaemonTarget::verify(
            executable.to_path_buf(),
            "0".repeat(64),
            location.clone(),
            config_directory.to_path_buf()
        )
        .await,
        Err(DaemonTargetError::InvalidSelection)
    ));
    eprintln!("independent phase: reject_wrong_state_begin");
    let other_location = bcode_config::resolve_state_location_set(
        &config.state,
        &bcode_config::StateLocationSelection {
            root: Some(directory.join("other-state")),
            profile: None,
        },
    )
    .unwrap()
    .primary()
    .clone();
    assert!(
        DaemonTarget::verify(
            executable.to_path_buf(),
            digest.to_owned(),
            other_location,
            config_directory.to_path_buf()
        )
        .await
        .is_err()
    );
    eprintln!("independent phase: reject_wrong_config_begin");
    let other_config = directory.join("other-config");
    std::fs::create_dir(&other_config).unwrap();
    assert!(
        DaemonTarget::verify(
            executable.to_path_buf(),
            digest.to_owned(),
            location.clone(),
            other_config
        )
        .await
        .is_err()
    );
    // The independent consumer test executable is a real different artifact, not a forged identity.
    eprintln!("independent phase: reject_consumer_artifact_begin");
    let consumer_executable = std::env::current_exe().unwrap();
    let consumer_digest = bcode_daemon_lifecycle::executable_sha256(&consumer_executable).unwrap();
    assert!(
        DaemonTarget::verify(
            consumer_executable,
            consumer_digest,
            location.clone(),
            config_directory.to_path_buf()
        )
        .await
        .is_err()
    );
}

async fn assert_replaced_target_and_shutdown(
    client: &bcode_client::BcodeClient,
    target: &bcode_daemon_lifecycle::DaemonTarget,
    executable: &std::path::Path,
    digest: &str,
    directory: std::path::PathBuf,
    daemon: &mut FixtureDaemon,
) {
    use bcode_session_models::SessionObservationApplication as _;
    // Replace only the copied test image, never Cargo's binary or an active product image.
    let replacement = directory.join("replacement");
    std::fs::write(&replacement, b"changed selected executable").unwrap();
    std::fs::rename(replacement, executable).unwrap();
    assert!(
        SessionApplication::create_session(
            client,
            CreateSessionRequest {
                name: Some("must not be admitted".into()),
                working_directory: directory,
            }
        )
        .await
        .is_err()
    );
    assert!(target.verify_executable().is_err());
    assert!(matches!(
        client.interaction_controller_consumer().await,
        Err(bcode_client::ClientError::IncompatibleDaemon { .. })
    ));
    assert!(
        client
            .observe_session(observation_request(SessionId::new()))
            .await
            .is_err()
    );
    // Restore the byte-identical selected test image solely to request graceful shutdown.
    std::fs::copy(env!("CARGO_BIN_EXE_bcode"), executable).unwrap();
    assert_eq!(
        bcode_daemon_lifecycle::executable_sha256(executable).unwrap(),
        digest
    );
    eprintln!("independent phase: graceful_stop_begin");
    client.server_stop_if_idle().await.unwrap();
    eprintln!("independent phase: graceful_exit_wait_begin");
    tokio::time::timeout(Duration::from_secs(5), async {
        while daemon.0.try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("fixture graceful shutdown did not complete; RAII kills/reaps only this child");
}

async fn wait_independent_question(
    client: &bcode_client::BcodeClient,
    controller: &bcode_client::InteractionControllerConsumer,
    session_id: SessionId,
    receipt: &bcode_session_models::TurnReceipt,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        // Match the complete consumer fixture: a permission checkpoint precedes the
        // question exchange. Approve only this expected fixture operation, once, and
        // never persist a policy rule or bypass normal permission admission.
        for permission in controller.pending_permissions(session_id).await.unwrap() {
            assert_eq!(permission.session_id, session_id);
            assert_eq!(
                permission.tool_name, "question",
                "unexpected fixture permission; refusing approval"
            );
            assert!(
                controller
                    .resolve_permission(session_id, &permission.permission_id, true, false)
                    .await
                    .unwrap()
            );
        }
        if let Some(exchange) = controller
            .pending_exchanges(session_id)
            .await
            .unwrap()
            .into_iter()
            .next()
        {
            break exchange.request.exchange_id;
        }
        // A bounded canonical page distinguishes a terminal provider/tool failure
        // from a live permission/controller wait. Do not hide it behind elapsed time.
        let events = history(client, session_id).await;
        if let Some((outcome, message)) = events.iter().find_map(|event| {
            if let SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            } = &event.kind
                && *turn_id == receipt.turn_id.to_string()
            {
                Some((outcome, message))
            } else {
                None
            }
        }) {
            panic!(
                "question turn {} terminalized before exchange: {outcome:?} message={message:?}; recent bounded event kinds={:?}",
                receipt.turn_id,
                events
                    .iter()
                    .rev()
                    .take(16)
                    .map(
                        |event| bcode_session_models::persisted_session_event_kind_name(
                            &event.kind
                        )
                    )
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            Instant::now() < deadline,
            "question exchange deadline; turn={} pending_permissions={:?}; recent bounded event kinds={:?}",
            receipt.turn_id,
            controller
                .pending_permissions(session_id)
                .await
                .unwrap()
                .iter()
                .map(|permission| (
                    &permission.permission_id,
                    &permission.tool_name,
                    &permission.policy_source
                ))
                .collect::<Vec<_>>(),
            events
                .iter()
                .rev()
                .take(16)
                .map(|event| bcode_session_models::persisted_session_event_kind_name(&event.kind))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn independent_cancel_and_spawn(
    client: &bcode_client::BcodeClient,
    session_id: SessionId,
    directory: std::path::PathBuf,
) {
    // Hold this turn at an unanswered question exchange rather than racing a
    // finite provider delay against IPC/history polling. The existing fake
    // directive and normal permission/controller path provide the gate.
    let controller = client.interaction_controller_consumer().await.unwrap();
    let admission = SessionApplication::submit_turn(
        client,
        session_id,
        r#"tool-call question {"questions":[{"header":"Cancel","question":"Wait for cancellation","required":true,"custom":false,"control":"radio","selection_mode":"single","options":[{"label":"Wait","value":"wait","description":"Leave unanswered"}]}]}"#.into(),
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    let TurnAdmission::Accepted(receipt) = admission else {
        panic!("cancellation admission failed: {admission:?}");
    };
    eprintln!("independent phase: cancellation_question_gate_begin");
    let exchange = wait_independent_question(client, &controller, session_id, &receipt).await;
    // Opening is read-only: deliberately do not submit or resolve this exchange.
    let opened = controller.open(1, &exchange).await.unwrap();
    assert_eq!(opened.exchange_id, exchange);
    eprintln!("independent phase: cancellation_question_gate_ready");
    assert!(
        SessionApplication::cancel_session_turn(
            client,
            session_id,
            false,
            Some(receipt.turn_id.to_string())
        )
        .await
        .unwrap()
    );
    terminal(
        client,
        session_id,
        &receipt.turn_id.to_string(),
        bcode_session_models::ModelTurnOutcome::Cancelled,
    )
    .await;
    drop(controller);
    eprintln!("independent phase: cancellation_terminal_complete_spawn_begin");
    let child = SessionApplication::spawn_session(
        client,
        bcode_session_models::SessionSpawnRequest {
            version: 1,
            creation: bcode_session_models::SessionChildCreateRequest {
                parent_id: session_id,
                child_id: SessionId::new(),
                name: Some("independent child".into()),
                working_directory: directory,
            },
            worktree_directory: None,
            input: "child fake echo".into(),
            admission: TurnAdmissionMetadata::default(),
            auth_profile: None,
        },
    )
    .await;
    child.validate().unwrap();
    assert!(child.failure.is_none(), "{child:?}");
    let child_session = child.session.unwrap();
    let Some(TurnAdmission::Accepted(receipt)) = child.admission else {
        panic!("spawn not admitted");
    };
    terminal(
        client,
        child_session.id,
        &receipt.turn_id.to_string(),
        bcode_session_models::ModelTurnOutcome::Completed,
    )
    .await;
}

fn start_independent_daemon(
    directory: &std::path::Path,
    executable: &std::path::Path,
    config: &bcode_config::BcodeConfig,
) -> (FixtureDaemon, bcode_config::StateLocation) {
    let config_file = directory.join("fake-only.toml");
    std::fs::write(
        &config_file,
        bcode_config::encode_effective_config(config).unwrap(),
    )
    .unwrap();
    let location = bcode_config::resolve_state_location_set(
        &config.state,
        &bcode_config::StateLocationSelection {
            root: Some(directory.join("state")),
            profile: None,
        },
    )
    .unwrap()
    .primary()
    .clone();
    let log = std::fs::File::create(directory.join("daemon.log")).unwrap();
    let mut command = std::process::Command::new(executable);
    command
        .env_clear()
        .env("HOME", directory)
        .env("BCODE_STATE_DIR", location.root())
        .env("XDG_CONFIG_HOME", directory.join("config"))
        .env("XDG_DATA_HOME", directory.join("data"));
    let child = command
        .arg("--config-only")
        .arg(&config_file)
        .args(["--profile", "independent-fake"])
        .arg("--state-root")
        .arg(location.root())
        .args(["server", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let daemon = FixtureDaemon(child);
    (daemon, location)
}

async fn answer_independent_question(
    controller: &bcode_client::InteractionControllerConsumer,
    exchange: String,
) {
    let opened = controller.open(1, &exchange).await.unwrap();
    let selected = controller
        .input(InteractionControllerInput {
            version: 1,
            exchange_id: exchange.clone(),
            expected_revision: opened.revision,
            input: serde_json::from_value(
                serde_json::json!({"type":"activate","control_id":"question-0.option-1"}),
            )
            .unwrap(),
        })
        .await
        .unwrap();
    controller
        .input(InteractionControllerInput {
            version: 1,
            exchange_id: exchange,
            expected_revision: selected.revision,
            input: serde_json::from_value(serde_json::json!({"type":"submit"})).unwrap(),
        })
        .await
        .unwrap();
}
