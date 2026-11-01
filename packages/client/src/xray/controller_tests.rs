//! Whole controller admission/rotation through public IPC and HTTP, not daemon management.
#![cfg(unix)]

use super::*;
use bcode_ipc::{Request, Response, ResponsePayload};
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

fn daemon_status() -> bcode_ipc::DaemonStatus {
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

struct Fixture {
    directory: tempfile::TempDir,
    endpoint: bcode_ipc::IpcEndpoint,
    session_id: bcode_session_models::SessionId,
    serving: tokio::task::JoinHandle<()>,
    first_http: std::thread::JoinHandle<()>,
    second_http: std::thread::JoinHandle<()>,
}

fn fixture() -> Fixture {
    // Use the short physical sticky-root temp directory, never ambient TMPDIR (which
    // can point through writable mount ancestors) or macOS's /tmp symlink.
    #[cfg(target_os = "macos")]
    let physical_tmp = std::path::PathBuf::from("/private/tmp");
    #[cfg(not(target_os = "macos"))]
    let physical_tmp = std::fs::canonicalize("/tmp").unwrap();
    let directory = tempfile::Builder::new()
        .prefix("xrc")
        .tempdir_in(physical_tmp)
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let physical_directory = directory.path().canonicalize().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(physical_directory.join("s"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let session_id = bcode_session_models::SessionId::new();
    let first_file = directory.path().join("first");
    let second_file = directory.path().join("second");
    for (path, token) in [(&first_file, "first-token"), (&second_file, "fresh-token")] {
        std::fs::write(path, token).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut first_ready = super::behavior_tests::ready("engine-one", 1);
    let mut second_ready = super::behavior_tests::ready("engine-two", 1);
    // Correlation is exact session metadata, not inferred from arbitrary payload text.
    first_ready = first_ready.replace("\"session\"", &format!("\"{session_id}\""));
    second_ready = second_ready.replace("\"session\"", &format!("\"{session_id}\""));
    let reference = serde_json::json!({"schema_version": 1, "payload_id": "queued",
        "session_id": session_id.to_string(), "content_type": "text/plain", "byte_len": 3,
        "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"});
    let diagnostic: XrayEnvelope = serde_json::from_value(serde_json::json!({
        "schema_version": 1, "observation_id": "engine-one", "sequence": 2, "timestamp_unix_ms": 0,
        "event": {"domain": "plugin", "name": "payload", "correlation": {"session_id": session_id.to_string()},
            "sensitive": false, "data": {"blob": reference},
            "capture": {"schema_version": 1, "attachments": [{"kind": "payload_ref", "pointer": "/blob"}]}}
    })).unwrap();
    writeln!(
        first_ready,
        "data: {}\n",
        serde_json::to_string(&diagnostic).unwrap()
    )
    .unwrap();
    let range = serde_json::json!({"schema_version": 1, "observation_id": "engine-one",
        "payload": reference, "offset": 0, "bytes": [97, 98, 99], "eof": true});
    let (first_origin, first_http) = super::behavior_tests::server_responses(vec![
        ("200 OK".into(), "text/event-stream".into(), first_ready),
        (
            "200 OK".into(),
            "application/json".into(),
            range.to_string(),
        ),
    ]);
    let (second_origin, second_http) = super::behavior_tests::server("200 OK", second_ready);
    let serving = serve_endpoints(
        listener,
        session_id,
        [
            (1, first_origin, first_file),
            (2, second_origin, second_file),
        ],
    );
    Fixture {
        directory,
        endpoint,
        session_id,
        serving,
        first_http,
        second_http,
    }
}

fn serve_endpoints(
    listener: bcode_ipc::LocalIpcListener,
    session_id: bcode_session_models::SessionId,
    endpoints: [(u8, String, std::path::PathBuf); 2],
) -> tokio::task::JoinHandle<()> {
    let daemon = daemon_status();
    tokio::spawn(async move {
        for (index, origin, path) in endpoints {
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
            let open = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            match bcode_ipc::decode_request(&open.payload).unwrap() {
                Request::OpenXrayEndpoint {
                    session_id: actual,
                    deep_capture,
                    origin_grant,
                } => {
                    assert_eq!(actual, session_id);
                    assert!(!deep_capture);
                    if index == 1 {
                        assert!(origin_grant.is_none());
                    } else {
                        let grant = origin_grant.expect("explicit second-segment origin grant");
                        assert_eq!(grant.origin, "https://inspector.example");
                        assert_eq!(grant.lifetime_seconds, 30);
                    }
                }
                _ => panic!("controller attempted a non-observation operation"),
            }
            let id = format!("endpoint-{index}");
            let response = Response::Ok(ResponsePayload::XrayEndpointOpened {
                endpoint: bcode_ipc::XrayEndpointInfo {
                    endpoint_id: id.clone(),
                    session_id,
                    endpoint: origin,
                    credential_file: path,
                    deep_capture: false,
                    schema_version: 1,
                },
            });
            bcode_ipc::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(open.request_id, &response).unwrap(),
            )
            .await
            .unwrap();
            let close = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            match bcode_ipc::decode_request(&close.payload).unwrap() {
                Request::CloseXrayEndpoint { endpoint_id } => assert_eq!(endpoint_id, id),
                _ => panic!("controller attempted execution steering instead of revocation"),
            }
            bcode_ipc::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(
                    close.request_id,
                    &Response::Ok(ResponsePayload::XrayEndpointClosed),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        }
    })
}

#[tokio::test]
async fn controller_rotates_endpoint_revokes_old_capability_and_surfaces_restart_gap() {
    let Fixture {
        directory,
        endpoint,
        session_id,
        serving,
        first_http,
        second_http,
    } = fixture();
    let application = crate::BcodeClient::new(endpoint);
    let mut controller = XrayObservationController::new(&application, session_id, false);
    assert!(controller.segment().is_none());
    let first = controller.rebootstrap().await.unwrap();
    assert!(!first.gap_before);
    assert_eq!(first.ordinal, 1);
    let stale_client = controller.client().unwrap().clone();
    let ready = controller.recv().await.unwrap().unwrap();
    let queued = controller.recv().await.unwrap().unwrap();
    assert!(matches!(
        controller.recv().await,
        Err(XrayClientError::Incomplete)
    ));
    assert!(matches!(
        controller.recv().await,
        Err(XrayClientError::Incomplete)
    ));
    assert!(controller.client().is_some());
    assert!(controller.last_result().is_none());
    // Export actual queued referenced bytes after EOF and before revocation.
    let mut sink = bcode_xray::recording::JsonlBundleRecorder::create(
        &directory.path().join("bundle"),
        &first.observation_id,
        &session_id.to_string(),
        bcode_xray::recording::RecordingOptions::default(),
    )
    .unwrap();
    bcode_xray::recording::RecordingSink::append(&mut sink, &ready).unwrap();
    bcode_xray::recording::RecordingSink::append(&mut sink, &queued).unwrap();
    // Model an exporter queued behind local IO; draining must survive a repeat
    // interruption notification without restarting intake or renewing its deadline.
    controller.interrupt();
    controller
        .record_payloads(&queued, &mut sink, 1024)
        .await
        .unwrap();
    bcode_xray::recording::RecordingSink::interrupt(&mut sink).unwrap();
    let verified = bcode_xray::recording::inspect_bundle(
        &directory.path().join("bundle"),
        &bcode_xray::recording::RecordingOptions::default(),
    )
    .unwrap();
    assert_eq!(verified.events.envelope_count, 2);
    assert_eq!(
        verified.state,
        bcode_xray::recording::RecordingState::Interrupted
    );
    assert!(!verified.evidence_complete);
    assert!(
        verified
            .payloads
            .values()
            .all(|payload| payload.relative_path.is_some())
    );
    assert!(matches!(
        stale_client.observe(super::behavior_tests::request()).await,
        Err(XrayClientError::InvalidCredential)
    ));
    controller.stop().await;
    let first_result = controller.last_result().unwrap().clone();
    assert_eq!(first_result.end, XraySegmentEnd::Interrupted);
    assert_eq!(first_result.delivered_through, 2);
    assert!(first_result.revocation_confirmed);
    assert!(controller.client().is_none());
    assert!(matches!(
        stale_client.observe(super::behavior_tests::request()).await,
        Err(XrayClientError::InvalidCredential)
    ));
    controller.set_origin_grant(Some(bcode_ipc::XrayOriginGrantRequest {
        origin: "https://inspector.example".into(),
        lifetime_seconds: 30,
    }));
    let second = controller.rebootstrap().await.unwrap();
    assert!(second.gap_before);
    assert_eq!(second.ordinal, 2);
    assert_ne!(first.observation_id, second.observation_id);
    assert_eq!(controller.recv().await.unwrap().unwrap().sequence, 1);
    controller.stop().await;
    assert!(controller.last_result().unwrap().revocation_confirmed);
    assert_eq!(
        controller.last_result().unwrap().end,
        XraySegmentEnd::Stopped
    );
    assert!(controller.client().is_none());
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .unwrap()
        .unwrap();
    first_http.join().unwrap();
    second_http.join().unwrap();
}
