use super::*;
use clap::Parser;

#[test]
fn consumer_queue_policy_is_explicit_positive_bounded_and_absorbs_short_bursts() {
    let session = "00000000-0000-0000-0000-000000000001";
    let defaults = Command::try_parse_from(["xray", session]).unwrap();
    assert_eq!(defaults.args.consumer_queue_capacity, 256);
    let configured =
        Command::try_parse_from(["xray", session, "--consumer-queue-capacity", "32"]).unwrap();
    assert_eq!(configured.args.consumer_queue_capacity, 32);
    for invalid in ["0", "4097", "-1"] {
        assert!(
            Command::try_parse_from(["xray", session, "--consumer-queue-capacity", invalid,])
                .is_err()
        );
    }
    let (sender, mut receiver) =
        tokio::sync::mpsc::channel(usize::from(defaults.args.consumer_queue_capacity));
    // Intake can preserve a diagnostic burst beyond the old 16-frame limit
    // even before its exporter gets scheduled; no producer waits or silent drops.
    for sequence in 1..=81 {
        enqueue(
            &sender,
            pipeline_envelope(sequence, "event", serde_json::json!({})),
        )
        .unwrap();
    }
    for sequence in 1..=81 {
        match receiver.try_recv().unwrap() {
            ConsumerMessage::Envelope(envelope) => assert_eq!(envelope.sequence, sequence),
            ConsumerMessage::Finish(_) | ConsumerMessage::Interrupt => {
                panic!("unexpected boundary")
            }
        }
    }
    assert!(receiver.try_recv().is_err());
}

#[test]
fn consumer_queue_is_bounded_nonblocking_and_preserves_accepted_order() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(CONSUMER_QUEUE_CAPACITY);
    for sequence in 1..=CONSUMER_QUEUE_CAPACITY as u64 {
        enqueue(
            &sender,
            pipeline_envelope(sequence, "event", serde_json::json!({})),
        )
        .unwrap();
    }
    let error = enqueue(
        &sender,
        pipeline_envelope(99, "event", serde_json::json!({})),
    )
    .unwrap_err();
    assert!(error.to_string().contains("overflow"));
    for sequence in 1..=CONSUMER_QUEUE_CAPACITY as u64 {
        match receiver.try_recv().unwrap() {
            ConsumerMessage::Envelope(envelope) => assert_eq!(envelope.sequence, sequence),
            ConsumerMessage::Finish(_) | ConsumerMessage::Interrupt => {
                panic!("unexpected terminal boundary")
            }
        }
    }
    drop(receiver);
    assert!(
        enqueue(
            &sender,
            pipeline_envelope(100, "event", serde_json::json!({}))
        )
        .is_err()
    );
}

pub(super) fn pipeline_envelope(
    sequence: u64,
    name: &str,
    data: serde_json::Value,
) -> bcode_xray_models::XrayEnvelope {
    bcode_xray_models::XrayEnvelope {
        schema_version: bcode_xray_models::XRAY_SCHEMA_VERSION,
        observation_id: "observer".into(),
        sequence,
        timestamp_unix_ms: 0,
        event: bcode_xray_models::XrayEvent {
            domain: "xray".into(),
            name: name.into(),
            data,
            sensitive: false,
            capture: None,
            correlation: bcode_xray_models::XrayCorrelation {
                session_id: "session".into(),
                ..Default::default()
            },
        },
    }
}

#[cfg(unix)]
#[tokio::test]
async fn consumer_finalizes_only_after_ordered_terminal_drain() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().canonicalize().unwrap().join("bundle");
    let boundary = bcode_xray_models::XrayFinalization {
        observation_id: "observer".into(),
        final_sequence: 2,
        dropped_events: 0,
        hub_contention_drops: 0,
        evicted_payloads: 0,
    };
    let (sender, receiver) = tokio::sync::mpsc::channel(CONSUMER_QUEUE_CAPACITY);
    enqueue(
        &sender,
        pipeline_envelope(1, "ready", serde_json::json!({})),
    )
    .unwrap();
    enqueue(
        &sender,
        pipeline_envelope(2, "finalized", serde_json::to_value(&boundary).unwrap()),
    )
    .unwrap();
    sender
        .send(ConsumerMessage::Finish(boundary))
        .await
        .unwrap();
    drop(sender);
    let destination = path.clone();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let client = XrayClient::new("http://127.0.0.1:1", "test-token").unwrap();
        consume_segment(
            receiver,
            &client,
            Some(&destination),
            "observer",
            "session",
            1024,
            &runtime,
        )
    })
    .await
    .unwrap()
    .unwrap();
    let manifest =
        bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default()).unwrap();
    assert_eq!(manifest.events.envelope_count, 2);
    assert_eq!(
        manifest.state,
        bcode_xray::recording::RecordingState::Finalized
    );
}

#[cfg(unix)]
#[tokio::test]
async fn lost_intake_marks_bundle_interrupted_without_a_finish_receipt() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().canonicalize().unwrap().join("unfinished");
    let (sender, receiver) = tokio::sync::mpsc::channel(CONSUMER_QUEUE_CAPACITY);
    enqueue(
        &sender,
        pipeline_envelope(1, "ready", serde_json::json!({})),
    )
    .unwrap();
    drop(sender);
    let destination = path.clone();
    let runtime = tokio::runtime::Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        let client = XrayClient::new("http://127.0.0.1:1", "test-token").unwrap();
        consume_segment(
            receiver,
            &client,
            Some(&destination),
            "observer",
            "session",
            1024,
            &runtime,
        )
    })
    .await
    .unwrap();
    assert!(result.is_err());
    let manifest =
        bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default()).unwrap();
    assert_eq!(
        manifest.state,
        bcode_xray::recording::RecordingState::Interrupted
    );
    assert!(!manifest.evidence_complete);
}

#[cfg(unix)]
fn gated_payload_server(
    listener: std::net::TcpListener,
    reference: serde_json::Value,
    requested: tokio::sync::oneshot::Sender<()>,
    released: std::sync::mpsc::Receiver<()>,
) -> std::thread::JoinHandle<()> {
    use std::io::{Read as _, Write as _};
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        };
        // Accepted sockets can inherit nonblocking mode on macOS. This fixture
        // uses blocking IO with explicit timeouts after its bounded accept loop.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        assert!(request.starts_with(b"POST /v1/xray/payload HTTP/1.1\r\n"));
        let headers = String::from_utf8(request).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        assert!(length < 8192);
        socket.read_exact(&mut vec![0; length]).unwrap();
        requested.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
        let body = serde_json::json!({
            "schema_version": 1, "observation_id": "observer", "payload": reference,
            "offset": 0, "bytes": [97, 98, 99], "eof": true
        })
        .to_string();
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    })
}

#[cfg(unix)]
#[tokio::test]
async fn slow_exporter_drains_accepted_queue_after_overflow_without_claiming_completion() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().canonicalize().unwrap().join("slow-export");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (requested, request_received) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let reference = bcode_xray_models::XrayPayloadRef {
        schema_version: bcode_xray_models::XRAY_SCHEMA_VERSION,
        payload_id: "blob".into(),
        session_id: "session".into(),
        content_type: "text/plain".into(),
        byte_len: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
    };
    let response_reference = serde_json::to_value(&reference).unwrap();
    let server = gated_payload_server(listener, response_reference, requested, released);
    let (sender, receiver) = tokio::sync::mpsc::channel(CONSUMER_QUEUE_CAPACITY);
    enqueue(
        &sender,
        pipeline_envelope(1, "ready", serde_json::json!({})),
    )
    .unwrap();
    let mut payload = pipeline_envelope(2, "payload", serde_json::json!({"blob": reference}));
    payload.event.domain = "plugin".into();
    payload.event.capture = Some(
        serde_json::from_value(serde_json::json!({
            "schema_version": 1, "attachments": [{"kind": "payload_ref", "pointer": "/blob"}]
        }))
        .unwrap(),
    );
    assert_eq!(payload.payload_references().unwrap(), vec![reference]);
    enqueue(&sender, payload).unwrap();
    let destination = path.clone();
    let runtime = tokio::runtime::Handle::current();
    let client = XrayClient::new(&origin, "fixture-token").unwrap();
    let export_client = client.clone();
    let mut consumer = tokio::task::spawn_blocking(move || {
        consume_segment(
            receiver,
            &export_client,
            Some(&destination),
            "observer",
            "session",
            1024,
            &runtime,
        )
    });
    let progress = tokio::select! {
        result = &mut consumer => panic!("consumer stopped before payload request: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(5), request_received) => result,
    };
    progress
        .expect("payload request readiness deadline")
        .expect("payload server stopped before receiving a request");
    // Export is gated, so the FIFO fills deterministically without sleeps or producer waits.
    let last = CONSUMER_QUEUE_CAPACITY as u64 + 2;
    for sequence in 3..=last {
        let name = if sequence == 3 {
            "payload_omitted"
        } else {
            "event"
        };
        enqueue(
            &sender,
            pipeline_envelope(sequence, name, serde_json::json!({})),
        )
        .unwrap();
    }
    assert!(
        enqueue(
            &sender,
            pipeline_envelope(last + 1, "event", serde_json::json!({}))
        )
        .is_err()
    );
    client.begin_payload_drain();
    drop(sender); // Lost intake, not a delivered terminal receipt.
    release.send(()).unwrap();
    assert!(consumer.await.unwrap().is_err());
    server.join().unwrap();
    let manifest =
        bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default()).unwrap();
    assert_eq!(manifest.events.envelope_count, last);
    assert_eq!(manifest.events.last_sequence, last);
    assert_eq!(manifest.omitted_events, 1);
    assert_eq!(manifest.missing_sequences, 0); // Overflow is outside the accepted prefix.
    assert_eq!(
        manifest.state,
        bcode_xray::recording::RecordingState::Interrupted
    );
    assert!(!manifest.evidence_complete);
    assert!(manifest.finalization.is_none());
    assert!(manifest.payloads["blob"].relative_path.is_some());
}

#[test]
fn origin_grant_requires_explicit_pair_and_bootstrap() {
    let session = "00000000-0000-0000-0000-000000000001";
    let parsed = Command::try_parse_from([
        "xray",
        session,
        "--allow-origin",
        "http://localhost:8080",
        "--origin-lifetime-seconds",
        "60",
        "--browser-handoff",
    ])
    .unwrap();
    let grant = origin_grant(&parsed.args).unwrap().unwrap();
    assert_eq!(grant.origin, "http://localhost:8080");
    assert_eq!(grant.lifetime_seconds, 60);
    for extra in [
        vec!["--allow-origin", "http://localhost:8080"],
        vec!["--origin-lifetime-seconds", "60"],
        vec!["--browser-handoff"],
        vec![
            "--allow-origin",
            "http://localhost:8080",
            "--origin-lifetime-seconds",
            "0",
        ],
        vec![
            "--allow-origin",
            "http://localhost:8080",
            "--origin-lifetime-seconds",
            "60",
            "--endpoint",
            "http://127.0.0.1:1234",
            "--token-file",
            "/private/token",
        ],
    ] {
        let mut argv = vec!["xray", session];
        argv.extend(extra);
        assert!(Command::try_parse_from(argv).is_err());
    }
    let default = Command::try_parse_from(["xray", session]).unwrap();
    assert!(origin_grant(&default.args).unwrap().is_none());
}

#[test]
fn browser_origin_parser_rejects_nonserialized_or_nonloopback_grants() {
    for origin in [
        "http://localhost",
        "http://localhost:8080",
        "http://127.0.0.1:3000",
        "http://127.2.3.4",
        "http://[::1]:8080",
        "http://[::1]",
    ] {
        assert_eq!(parse_browser_origin(origin).unwrap(), origin);
    }
    for origin in [
        "null",
        "*",
        "https://localhost",
        "http://example.com",
        "http://0.0.0.0",
        "http://localhost/",
        "http://localhost:80",
        "http://localhost:0080",
        "http://LOCALHOST",
        "http://user@localhost",
        "http://localhost?x",
        "http://localhost#x",
        "http://localhost/path",
        "http://127.1",
        "http://[::ffff:127.0.0.1]",
        "http://localhost:65536",
        "http://localhost:",
    ] {
        assert!(parse_browser_origin(origin).is_err(), "accepted {origin}");
    }
}

#[test]
fn browser_handoff_exports_only_locator_not_credential_bytes() {
    let args = Command::try_parse_from([
        "xray",
        "00000000-0000-0000-0000-000000000001",
        "--allow-origin",
        "http://localhost:8080",
        "--origin-lifetime-seconds",
        "60",
        "--browser-handoff",
    ])
    .unwrap()
    .args;
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("token");
    std::fs::write(&path, "private-test-bearer-never-export").unwrap();
    let mut output = Vec::new();
    write_browser_handoff(&mut output, &args, "http://127.0.0.1:1234", &path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["endpoint"], "http://127.0.0.1:1234");
    assert_eq!(
        value["credential_file"],
        serde_json::to_value(&path).unwrap()
    );
    assert!(
        !String::from_utf8(output)
            .unwrap()
            .contains("private-test-bearer-never-export")
    );
}

#[test]
fn reconnect_is_explicit_with_positive_bounded_controls() {
    let session = "00000000-0000-0000-0000-000000000001";
    let default = Command::try_parse_from(["xray", session]).unwrap();
    assert_eq!(default.args.reconnect, ReconnectPolicy::Never);
    let configured = Command::try_parse_from([
        "xray",
        session,
        "--reconnect",
        "transport",
        "--reconnect-retries",
        "7",
        "--reconnect-delay-ms",
        "20",
    ])
    .unwrap();
    assert_eq!(configured.args.reconnect, ReconnectPolicy::Transport);
    assert_eq!(configured.args.reconnect_retries, 7);
    for extra in [
        ["--reconnect", "always"],
        ["--reconnect-retries", "0"],
        ["--reconnect-delay-ms", "0"],
        ["--reconnect-delay-ms", "60001"],
    ] {
        assert!(Command::try_parse_from(["xray", session, extra[0], extra[1]]).is_err());
    }
}

#[derive(Parser)]
struct Command {
    #[command(flatten)]
    args: XrayArgs,
}

#[test]
fn bootstrap_is_default_and_manual_overrides_must_be_paired() {
    let parsed = Command::try_parse_from(["xray", "00000000-0000-0000-0000-000000000001"]).unwrap();
    assert!(parsed.args.endpoint.is_none());
    assert!(parsed.args.token_file.is_none());
    for trailing in [
        ["--endpoint", "http://127.0.0.1:8080"],
        ["--token-file", "/private/token"],
    ] {
        assert!(
            Command::try_parse_from([
                "xray",
                "00000000-0000-0000-0000-000000000001",
                trailing[0],
                trailing[1],
            ])
            .is_err()
        );
    }
}

#[test]
fn deep_capture_is_explicit_and_duration_is_positive() {
    let parsed = Command::try_parse_from([
        "xray",
        "00000000-0000-0000-0000-000000000001",
        "--endpoint",
        "http://127.0.0.1:8080",
        "--token-file",
        "/private/token",
    ])
    .unwrap();
    assert!(!parsed.args.deep);
    assert!(parsed.args.record.is_none());
    assert!(
        Command::try_parse_from([
            "xray",
            "00000000-0000-0000-0000-000000000001",
            "--endpoint",
            "http://127.0.0.1:8080",
            "--token-file",
            "/private/token",
            "--duration-seconds",
            "0",
        ])
        .is_err()
    );
}

#[test]
fn jsonl_preserves_unknown_domain_data_and_writes_one_line() {
    let envelope = bcode_xray_models::XrayEnvelope {
        schema_version: bcode_xray_models::XRAY_SCHEMA_VERSION,
        observation_id: "observer".into(),
        sequence: 2,
        timestamp_unix_ms: 0,
        event: bcode_xray_models::XrayEvent {
            domain: "future".into(),
            name: "future_event".into(),
            correlation: bcode_xray_models::XrayCorrelation {
                session_id: "session".into(),
                ..Default::default()
            },
            data: serde_json::json!({"text": "line one\nline two 雪"}),
            sensitive: false,
            capture: None,
        },
    };
    let mut output = Vec::new();
    write_envelope(&mut output, None, &envelope).unwrap();
    let mut lines = output.split(|byte| *byte == b'\n');
    let line = lines.next().unwrap();
    assert_eq!(
        serde_json::from_slice::<bcode_xray_models::XrayEnvelope>(line).unwrap(),
        envelope
    );
    assert_eq!(lines.next(), Some(b"".as_slice()));
    assert!(lines.next().is_none());
    assert!(finish_recording(None, None).is_err());
}

#[cfg(unix)]
#[test]
fn credential_files_reject_symlinks_and_public_permissions() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("token");
    std::fs::write(&path, "test-token\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_token(&path).unwrap(), "test-token\n");
    let link = directory.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(read_token(&link).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_token(&path).is_err());
}

#[cfg(unix)]
#[test]
fn oversized_credentials_fail_without_echoing_contents() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("token");
    std::fs::write(&path, "private-test-value".repeat(4096)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let error = read_token(&path).unwrap_err().to_string();
    assert!(!error.contains("private-test-value"));
}
