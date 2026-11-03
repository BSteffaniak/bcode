use super::*;
use bcode_xray_models::{XrayCorrelation, XrayEnvelope, XrayEvent};

fn event() -> ExecutionWatchEvent {
    ExecutionWatchEvent {
        run_id: "run".into(),
        session_id: "session".into(),
        envelope: XrayEnvelope {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: "observation".into(),
            sequence: 1,
            timestamp_unix_ms: 0,
            event: XrayEvent {
                domain: "xray".into(),
                name: "ready".into(),
                sensitive: false,
                capture: None,
                correlation: XrayCorrelation {
                    session_id: "session".into(),
                    ..Default::default()
                },
                data: serde_json::json!({}),
            },
        },
    }
}

#[test]
fn relative_recording_parent_resolves_to_absolute_sink_destination() {
    let directory = recording_directory(std::path::Path::new(".")).unwrap();
    assert!(directory.is_absolute());
    assert_eq!(
        directory,
        std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("xray-execution")
    );
    let absent = tempfile::tempdir().unwrap();
    assert_eq!(
        recording_directory(&absent.path().join("missing")),
        Err("watch_recording_parent_unavailable")
    );
}

#[test]
fn page_bound_is_total_envelope_bytes_not_a_per_item_allowance() {
    let event = event();
    let size = u64::try_from(serde_json::to_vec(&event.envelope).unwrap().len()).unwrap();
    assert!(envelopes_fit_page(std::slice::from_ref(&event), size));
    assert!(!envelopes_fit_page(std::slice::from_ref(&event), size - 1));
    let page = [event.clone(), event];
    assert!(!envelopes_fit_page(&page, size));
    assert!(envelopes_fit_page(&page, size * 2));
    assert!(envelopes_fit_page(&[], 0));
}

#[cfg(unix)]
fn test_daemon() -> bcode_ipc::DaemonStatus {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let mut file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 65536];
    loop {
        let length = file.read(&mut buffer).unwrap();
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    bcode_ipc::DaemonStatus {
        namespace: bcode_ipc::daemon_namespace(),
        protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
        artifact_id: Some(bcode_ipc::ArtifactId::current()),
        build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
        executable_digest: Some(hex::encode(digest.finalize())),
        storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
        session_event_schema_version: Some(
            bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        ),
        state_location_id: Some(bcode_ipc::state_location_id()),
        ..Default::default()
    }
}

#[cfg(unix)]
fn payload_event(mode: &str) -> (bcode_xray_models::XrayPayloadRef, XrayEnvelope) {
    use bcode_xray_models::XrayPayloadRef;
    use sha2::{Digest as _, Sha256};
    let reference = XrayPayloadRef {
        schema_version: XRAY_SCHEMA_VERSION,
        payload_id: "../../opaque-identity".into(),
        session_id: if mode == "foreign" {
            "foreign"
        } else {
            "session"
        }
        .into(),
        content_type: "application/octet-stream".into(),
        byte_len: 3,
        sha256: hex::encode(Sha256::digest(b"abc")),
    };
    let mut payload = event().envelope;
    payload.sequence = 2;
    payload.event.domain = "plugin.fixture".into();
    payload.event.name = "payload".into();
    payload.event.data = serde_json::json!({"blob": reference});
    if mode != "opaque" {
        let mut capture = bcode_xray_models::XrayCaptureAttachments::new(vec![
            bcode_xray_models::XrayCaptureAttachment::PayloadRef {
                pointer: "/blob".into(),
            },
        ]);
        if mode == "future" {
            // Exercise the consumer's own check without a wire decode.
            capture.schema_version = 2;
        }
        payload.event.capture = Some(capture);
    }
    (reference, payload)
}

/// The real execution-consumer export path, with a private IPC peer and no daemon.
/// A single absolute allowance bounds both peer and client; dropping it closes both.
#[cfg(unix)]
async fn export_fixture(mode: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    #[cfg(target_os = "macos")]
    let physical_tmp = PathBuf::from("/private/tmp");
    #[cfg(not(target_os = "macos"))]
    let physical_tmp = std::fs::canonicalize("/tmp").unwrap();
    let directory = tempfile::Builder::new()
        .prefix("xe-")
        .tempdir_in(physical_tmp)
        .unwrap();
    // Strict IPC intentionally does not repair existing directory permissions.
    // Set the private fixture root explicitly instead of depending on umask or
    // weakening the listener's ownership/mode/ancestor checks.
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let physical_directory = directory.path().canonicalize().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(physical_directory.join("peer.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let daemon = test_daemon();
    let (reference, payload) = payload_event(mode);
    let ranges = if matches!(mode, "opaque" | "foreign" | "future") {
        0
    } else {
        2
    };
    let server = serve_export(listener, daemon, &reference, mode, ranges);
    let consumer = async {
        let application = bcode_client::BcodeClient::new(endpoint);
        let mut connection = application.connect("eval-export-test").await.unwrap();
        let bundle = directory.path().canonicalize().unwrap().join("bundle");
        let mut sink = JsonlBundleRecorder::create(
            &bundle,
            "observation",
            "session",
            RecordingOptions::default(),
        )
        .unwrap();
        sink.append(&event().envelope).unwrap();
        sink.append(&payload).unwrap();
        let exported = export_payloads(&mut connection, "watch", "run", &payload, &mut sink).await;
        assert_eq!(exported.is_ok(), matches!(mode, "valid" | "opaque"));
        let cutoff = XrayFinalization {
            observation_id: "observation".into(),
            final_sequence: 3,
            dropped_events: 0,
            hub_contention_drops: 0,
            evicted_payloads: 0,
        };
        let mut finalized = event().envelope;
        finalized.sequence = 3;
        finalized.event.name = "finalized".into();
        finalized.event.data = serde_json::to_value(&cutoff).unwrap();
        sink.append(&finalized).unwrap();
        // Match ExecutionCapture's exporter gate; failed export cannot finalize.
        let manifest = if exported.is_ok() {
            sink.finalize(&cutoff).unwrap()
        } else {
            sink.interrupt().unwrap()
        };
        assert_eq!(
            manifest.evidence_complete,
            matches!(mode, "valid" | "opaque")
        );
        if mode == "valid" {
            let retained = manifest.payloads[&reference.payload_id]
                .relative_path
                .as_ref()
                .unwrap();
            assert_eq!(std::fs::read(bundle.join(retained)).unwrap(), b"abc");
        } else if mode == "opaque" {
            assert!(manifest.payloads.is_empty());
        }
    };
    Box::pin(tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, consumer);
    }))
    .await
    .expect("bounded private IPC fixture");
}

#[cfg(unix)]
async fn serve_export(
    listener: bcode_ipc::LocalIpcListener,
    daemon: bcode_ipc::DaemonStatus,
    reference: &bcode_xray_models::XrayPayloadRef,
    mode: &str,
    ranges: u64,
) {
    use bcode_ipc::{Request, Response, ResponsePayload};
    use bcode_xray_models::XrayPayloadRange;
    let mut stream = listener.accept().await.unwrap();
    let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
    assert!(matches!(
        bcode_ipc::decode_request(&hello.payload).unwrap(),
        Request::Hello { .. }
    ));
    bcode_ipc::send_envelope(
        &mut stream,
        &bcode_ipc::response_envelope(
            hello.request_id,
            &Response::Ok(ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            }),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    for index in 0..ranges {
        let message = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
        let Request::ExportXrayExecutionWatch { request } =
            bcode_ipc::decode_request(&message.payload).unwrap()
        else {
            panic!("export must not schedule, reconnect, or steer execution");
        };
        assert_eq!(request.watch_id, "watch");
        assert_eq!(request.run_id, "run");
        assert_eq!(request.payload.observation_id, "observation");
        assert_eq!(request.payload.session_id, "session");
        assert_eq!(request.payload.payload_id, reference.payload_id);
        assert_eq!(request.payload.offset, u64::from(index != 0));
        assert_eq!(request.payload.length, 64 * 1024);
        let chunk = XrayPayloadRange {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: "observation".into(),
            payload: reference.clone(),
            offset: u64::from(index != 0 && mode != "offset"),
            bytes: if index == 0 {
                vec![b'a']
            } else if mode == "digest" {
                b"bd".to_vec()
            } else {
                b"bc".to_vec()
            },
            eof: index == 1,
        };
        bcode_ipc::send_envelope(
            &mut stream,
            &bcode_ipc::response_envelope(
                message.request_id,
                &Response::Ok(ResponsePayload::ExportXrayExecutionWatch { chunk }),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    }
    // Any extra request (including for opaque reference-shaped JSON) fails.
    assert!(bcode_ipc::recv_envelope(&mut stream).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn typed_payload_export_retains_verified_multirange_bytes() {
    Box::pin(export_fixture("valid")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn opaque_reference_shape_never_requests_payloads() {
    Box::pin(export_fixture("opaque")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn foreign_and_future_declared_payloads_fail_closed() {
    Box::pin(export_fixture("foreign")).await;
    Box::pin(export_fixture("future")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn payload_digest_and_offset_failures_never_become_complete() {
    Box::pin(export_fixture("digest")).await;
    Box::pin(export_fixture("offset")).await;
}
