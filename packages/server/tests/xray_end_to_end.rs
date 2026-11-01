#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Production-path acceptance plus adversarial wire fixtures.
//! Whole-app cases use an explicitly identified Bcode executable, private foreground server,
//! typed IPC bootstrap, production HTTP/SSE, CLI, recorder, and real fake-provider turns.
//! Controlled peers below test protocol failures only; they do not establish app coverage.

use bcode_client::xray::{XrayClient, XrayClientError};
use bcode_xray::{XrayHub, XrayObserveRequest};
use serde_json::json;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

const SESSION: &str = "00000000-0000-0000-0000-000000000001";
const TOKEN: &str = "fixture-only-not-a-real-credential";

fn request() -> XrayObserveRequest {
    XrayObserveRequest {
        schema_version: 1,
        session_id: SESSION.to_owned(),
        deep_capture: false,
    }
}

struct Peer {
    address: SocketAddr,
    worker: Option<JoinHandle<()>>,
}

impl Peer {
    fn serve(responses: Vec<Vec<u8>>) -> Self {
        let responses = responses
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| (if index == 0 { "observe" } else { "release" }, bytes))
            .collect();
        Self::serve_paths(responses)
    }

    fn serve_paths(responses: Vec<(&'static str, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("isolated loopback listener");
        let address = listener.local_addr().expect("bound address");
        listener.set_nonblocking(true).expect("bounded accept");
        let worker = std::thread::spawn(move || {
            for (index, (expected_path, response)) in responses.into_iter().enumerate() {
                // The second connection follows CLI startup, stream drain, and local
                // recording flush; allow that work without weakening request checks.
                let admission_budget = if index == 0 { 10 } else { 30 };
                let deadline = std::time::Instant::now() + Duration::from_secs(admission_budget);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "HTTP fixture admission deadline"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                // macOS accepts inherit O_NONBLOCK from the listener. This peer uses
                // blocking read_exact/write_all with socket deadlines; normalize the
                // accepted stream before reading instead of racing request arrival.
                stream
                    .set_nonblocking(false)
                    .expect("blocking bounded fixture stream");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read bound");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("write bound");
                let (target, body) = consume_request(&mut stream);
                let wire_path = if expected_path == "payload_checked" {
                    "payload"
                } else {
                    expected_path
                };
                assert_eq!(target, format!("POST /v1/xray/{wire_path} HTTP/1.1"));
                if expected_path == "payload_checked" {
                    let outgoing = response
                        .windows(4)
                        .position(|part| part == b"\r\n\r\n")
                        .expect("response headers")
                        + 4;
                    let range: serde_json::Value =
                        serde_json::from_slice(&response[outgoing..]).expect("range JSON");
                    assert_eq!(
                        body["offset"], range["offset"],
                        "client must progress its requested offset"
                    );
                    assert_eq!(body["payload_id"], range["payload"]["payload_id"]);
                }
                // Fragment arbitrary UTF-8 and SSE boundaries, not just whole frames.
                for fragment in response.chunks(13) {
                    stream.write_all(fragment).expect("fixture response");
                }
            }
        });
        Self {
            address,
            worker: Some(worker),
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}/", self.address)
    }

    fn finish(mut self) {
        self.worker
            .take()
            .expect("worker")
            .join()
            .expect("HTTP peer completed");
    }
}

fn consume_request(stream: &mut TcpStream) -> (String, serde_json::Value) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).expect("bounded HTTP headers");
        bytes.push(byte[0]);
        assert!(bytes.len() <= 16 * 1024, "bounded fixture request");
        if bytes.ends_with(b"\r\n\r\n") {
            break bytes.len();
        }
    };
    let headers = String::from_utf8(bytes.clone()).expect("HTTP headers");
    let mut lines = headers.lines();
    let target = lines.next().expect("request line");
    assert!(!target.contains(TOKEN), "bearer must not enter URL");
    assert!(target.starts_with("POST /v1/xray/"));
    let authorization = format!("authorization: Bearer {TOKEN}");
    assert!(
        headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&authorization))
    );
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().expect("length"))
        })
        .unwrap_or(0);
    assert!(header_end + length <= 16 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("request body");
    assert!(!String::from_utf8_lossy(&body).contains(TOKEN));
    (
        target.to_owned(),
        serde_json::from_slice(&body).expect("typed JSON request"),
    )
}

fn response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn sse(envelopes: &[bcode_xray::XrayEnvelope]) -> Vec<u8> {
    let mut bytes = b": heartbeat\r\n\r\n".to_vec();
    for envelope in envelopes {
        bytes.extend_from_slice(
            format!(
                "event: xray\r\ndata: {}\r\n\r\n",
                serde_json::to_string(envelope).expect("envelope JSON")
            )
            .as_bytes(),
        );
    }
    response("200 OK", "text/event-stream", &bytes)
}

async fn engine_envelopes() -> Vec<bcode_xray::XrayEnvelope> {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request()).expect("idle observation");
    let context = hub.context(bcode_xray::XrayCorrelation {
        session_id: SESSION.to_owned(),
        turn_id: Some("turn-one".to_owned()),
        ..bcode_xray::XrayCorrelation::default()
    });
    context.emit("runtime", "turn_started", json!({}), false);
    context.emit(
        "runtime",
        "turn_completed",
        json!({"text": "CJK 中文 👩‍💻 e\u{301}"}),
        false,
    );
    observation.finalize().expect("cutoff");
    let mut envelopes = Vec::new();
    while let Some(envelope) = observation.recv().await {
        envelopes.push(envelope);
    }
    envelopes
}

#[tokio::test]
async fn real_http_client_preserves_engine_readiness_unicode_and_delivery_cutoff() {
    let expected = engine_envelopes().await;
    let peer = Peer::serve(vec![sse(&expected)]);
    let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("explicit local client");
    let mut stream = client.observe(request()).await.expect("ready barrier");
    assert_eq!(stream.observation_id(), expected[0].observation_id);
    for envelope in &expected {
        assert_eq!(
            stream.recv().await.expect("valid wire"),
            Some(envelope.clone())
        );
    }
    assert!(stream.finalization().is_some());
    assert!(stream.recv().await.expect("stable EOF").is_none());
    peer.finish();
}

#[tokio::test]
async fn production_loss_envelopes_cross_http_without_becoming_protocol_errors() {
    let hub = XrayHub::new(bcode_xray::XrayConfig {
        queue_capacity: 1,
        ..bcode_xray::XrayConfig::default()
    });
    let mut observation = hub.observe(request()).expect("observe");
    let context = hub.context(bcode_xray::XrayCorrelation {
        session_id: SESSION.to_owned(),
        ..bcode_xray::XrayCorrelation::default()
    });
    for _ in 0..4 {
        context.emit("runtime", "burst", json!({}), false);
    }
    let cutoff = observation.finalize().expect("cutoff");
    assert_eq!(cutoff.dropped_events, 3);
    let mut expected = Vec::new();
    while let Some(envelope) = observation.recv().await {
        expected.push(envelope);
    }
    assert!(expected.iter().any(|envelope| envelope.event.name == "gap"));
    let peer = Peer::serve(vec![sse(&expected)]);
    let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("client");
    let mut stream = client.observe(request()).await.expect("ready");
    for envelope in expected {
        assert_eq!(
            stream
                .recv()
                .await
                .expect("production gap protocol must agree"),
            Some(envelope)
        );
    }
    assert_eq!(stream.finalization(), Some(&cutoff));
    assert!(stream.recv().await.expect("finalized EOF").is_none());
    peer.finish();
}

#[tokio::test]
async fn ready_then_transport_eof_is_incomplete_not_success() {
    let expected = engine_envelopes().await;
    let peer = Peer::serve(vec![sse(&expected[..1])]);
    let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("client");
    let mut stream = client.observe(request()).await.expect("ready");
    assert!(stream.recv().await.expect("buffered ready").is_some());
    assert!(matches!(
        stream.recv().await,
        Err(XrayClientError::Incomplete)
    ));
    assert!(stream.finalization().is_none());
    peer.finish();
}

#[tokio::test]
async fn future_versions_and_cross_session_wire_data_fail_closed() {
    for change_version in [true, false] {
        let mut expected = engine_envelopes().await;
        if change_version {
            expected[1].schema_version = u16::MAX;
        } else {
            expected[1].event.correlation.session_id = "foreign-session".to_owned();
        }
        let peer = Peer::serve(vec![sse(&expected[..2])]);
        let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("client");
        let mut stream = client.observe(request()).await.expect("ready");
        stream.recv().await.expect("buffered ready");
        assert!(matches!(
            stream.recv().await,
            Err(XrayClientError::Protocol)
        ));
        assert!(stream.finalization().is_none());
        peer.finish();
    }
}

#[tokio::test]
async fn authentication_failures_exclude_server_body_and_bearer_from_errors() {
    let peer = Peer::serve(vec![response(
        "403 Forbidden",
        "text/plain",
        b"private-server-fixture",
    )]);
    let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("client");
    let Err(error) = client.observe(request()).await else {
        panic!("unauthorized observation admitted");
    };
    assert!(matches!(error, XrayClientError::Http(403)));
    let public = format!("{error:?} {error} {client:?}");
    assert!(!public.contains(TOKEN));
    assert!(!public.contains("private-server-fixture"));
    peer.finish();
}

#[test]
fn nonlocal_credential_bearing_and_ambiguous_origins_are_rejected() {
    for endpoint in [
        "https://127.0.0.1:80/",
        "http://example.com/",
        "http://localhost:80/",
        "http://user:secret@127.0.0.1/",
        "http://127.0.0.1/private",
        "http://127.0.0.1/?token=secret",
        "http://127.0.0.1/#secret",
    ] {
        assert!(matches!(
            XrayClient::new(endpoint, TOKEN),
            Err(XrayClientError::InvalidEndpoint)
        ));
    }
    for token in ["", "line\nbreak", "space token"] {
        assert!(matches!(
            XrayClient::new("http://127.0.0.1:1/", token),
            Err(XrayClientError::InvalidCredential)
        ));
    }
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires explicit BCODE_XRAY_TEST_BINARY; run via check-xray-e2e.sh"]
async fn identified_cli_writes_jsonl_through_finalization_and_records_without_deep_opt_in() {
    use std::os::unix::fs::PermissionsExt as _;
    use wait_timeout::ChildExt as _;

    let binary =
        std::env::var_os("BCODE_XRAY_TEST_BINARY").expect("explicit identified test binary");
    assert!(std::path::Path::new(&binary).is_absolute());
    let root = tempfile::Builder::new()
        .prefix("xray-cli-peer-")
        .tempdir()
        .expect("isolated CLI artifacts")
        .keep()
        .canonicalize()
        .expect("non-symlink fixture root");
    eprintln!("private CLI/peer diagnostics retained: {}", root.display());
    let token = root.join("token");
    std::fs::write(&token, TOKEN).expect("fixture credential");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600))
        .expect("private credential");
    let expected = engine_envelopes().await;
    let peer = Peer::serve(vec![
        sse(&expected),
        response("204 No Content", "application/json", b""),
    ]);
    let output = root.join("output.jsonl");
    let errors = root.join("stderr");
    let recording = root.join("recording");
    let mut child = std::process::Command::new(binary)
        .args([
            "session",
            "xray",
            SESSION,
            "--endpoint",
            &peer.endpoint(),
            "--token-file",
        ])
        .arg(&token)
        .arg("--record")
        .arg(&recording)
        .args(["--duration-seconds", "1"])
        .stdout(std::fs::File::create(&output).expect("stdout artifact"))
        .stderr(std::fs::File::create(&errors).expect("stderr artifact"))
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("identified CLI");
    let status = child
        .wait_timeout(Duration::from_secs(15))
        .expect("bounded CLI wait");
    if status.is_none() {
        child.kill().expect("kill only owned fixture child");
        child.wait().expect("reap fixture child");
        panic!("CLI failed to finish after authoritative finalization");
    }
    assert!(
        status.expect("CLI status").success(),
        "CLI failed; private artifacts remain in fixture directory"
    );
    let bytes = std::fs::read_to_string(&output).expect("JSONL output");
    let actual: Vec<bcode_xray::XrayEnvelope> = bytes
        .lines()
        .map(|line| serde_json::from_str(line).expect("one envelope per line"))
        .collect();
    assert_eq!(actual, expected);
    assert!(!bytes.contains(TOKEN));
    assert!(
        !std::fs::read_to_string(errors)
            .expect("stderr")
            .contains(TOKEN)
    );
    let manifest: bcode_xray::recording::RecordingManifest = serde_json::from_slice(
        &std::fs::read(recording.join("manifest.json")).expect("published recording manifest"),
    )
    .expect("typed bundle contract");
    assert_eq!(
        manifest.state,
        bcode_xray::recording::RecordingState::Finalized
    );
    assert!(manifest.evidence_complete);
    assert_eq!(manifest.session_id, SESSION);
    assert_eq!(
        manifest.events.envelope_count,
        u64::try_from(expected.len()).expect("bounded count")
    );
    assert_eq!(
        std::fs::read_to_string(recording.join(&manifest.events.relative_path))
            .expect("recorded JSONL"),
        bytes
    );
    peer.finish();
}

/// Payload fixtures exercise the public client and recorder, not production provider coverage.
fn payload_fixture(bytes: &[u8]) -> bcode_xray_models::XrayPayloadRef {
    use sha2::{Digest as _, Sha256};
    bcode_xray_models::XrayPayloadRef {
        schema_version: 1,
        payload_id: "opaque-payload".to_owned(),
        session_id: SESSION.to_owned(),
        content_type: "application/json".to_owned(),
        byte_len: u64::try_from(bytes.len()).expect("bounded fixture"),
        sha256: hex::encode(Sha256::digest(bytes)),
    }
}

fn declare_payload(
    envelope: &mut bcode_xray::XrayEnvelope,
    reference: &bcode_xray_models::XrayPayloadRef,
) {
    envelope.event.data = json!({"retained": reference});
    envelope.event.capture = Some(bcode_xray_models::XrayCaptureAttachments::new(vec![
        bcode_xray_models::XrayCaptureAttachment::PayloadRef {
            pointer: "/retained".to_owned(),
        },
    ]));
}

#[tokio::test]
async fn forged_reference_shaped_json_is_opaque_and_never_fetches_or_marks_payload_missing() {
    use bcode_xray::recording::{JsonlBundleRecorder, RecordingOptions, RecordingSink as _};
    let mut events = engine_envelopes().await;
    let forged = payload_fixture(b"not an attachment");
    // These names and values are user-controlled content, not attachment authority.
    events[1].event.data =
        json!({"payload_ref": forged, "nested": {"payload_id": "../../private"}});
    events[1].event.capture = None;
    let directory = tempfile::tempdir().expect("private recording root");
    let path = directory
        .path()
        .canonicalize()
        .expect("physical root")
        .join("opaque");
    let mut sink = JsonlBundleRecorder::create(
        &path,
        &events[0].observation_id,
        SESSION,
        RecordingOptions::default(),
    )
    .expect("new recorder");
    // No listener exists: any attempted fetch makes this test fail, rather than
    // satisfying it through a fixture that accidentally accepts forged access.
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserved local port");
    let endpoint = format!("http://{}/", listener.local_addr().expect("address"));
    drop(listener);
    let client = XrayClient::new(&endpoint, TOKEN).expect("local client");
    for event in &events {
        sink.append(event).expect("ordered envelope");
        client
            .record_payloads(event, &mut sink, 1024)
            .await
            .expect("ordinary JSON cannot trigger payload IO");
    }
    let boundary = serde_json::from_value(events.last().expect("cutoff").event.data.clone())
        .expect("typed cutoff");
    let manifest = sink
        .finalize(&boundary)
        .expect("complete opaque data recording");
    assert!(manifest.evidence_complete);
    assert_eq!(
        bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default())
            .expect("offline verified bundle"),
        manifest
    );
    let event_path = path.join("events.jsonl");
    let original = std::fs::read(&event_path).expect("events");
    let mut corrupt = original.clone();
    corrupt[0] ^= 1;
    std::fs::write(&event_path, &corrupt).expect("fixture corruption");
    assert!(
        bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default()).is_err(),
        "event corruption cannot be certified"
    );
    assert_eq!(
        std::fs::read(&event_path).expect("unchanged corrupt bytes"),
        corrupt,
        "inspection never repairs"
    );
    std::fs::write(&event_path, original).expect("restore fixture");
    let manifest_path = path.join("manifest.json");
    let mut future = serde_json::to_value(&manifest).expect("manifest JSON");
    future["schema_version"] = json!(u16::MAX);
    let future_bytes = serde_json::to_vec(&future).expect("future manifest");
    std::fs::write(&manifest_path, &future_bytes).expect("future fixture");
    assert!(
        bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default()).is_err(),
        "unknown recording schema rejected"
    );
    assert_eq!(
        std::fs::read(manifest_path).expect("preserved future manifest"),
        future_bytes
    );
    assert!(manifest.payloads.is_empty());
    let retained = std::fs::read_to_string(path.join("events.jsonl")).expect("retained data");
    assert!(
        retained.contains("../../private"),
        "ordinary data is preserved, not reinterpreted"
    );
}

#[tokio::test]
async fn payload_wire_corruption_future_schema_and_foreign_identity_never_complete_recording() {
    use bcode_xray::recording::{
        JsonlBundleRecorder, RecordingOptions, RecordingSink as _, RecordingState,
    };
    let bytes = b"complete sensitive fixture";
    let reference = payload_fixture(bytes);
    for fault in [
        "digest",
        "future_range",
        "future_reference",
        "foreign_observation",
        "foreign_session",
        "wrong_offset",
        "false_eof",
        "empty_progress",
    ] {
        let mut events = engine_envelopes().await;
        declare_payload(&mut events[1], &reference);
        let mut range = json!({
            "schema_version": 1, "observation_id": events[0].observation_id,
            "payload": reference, "offset": 0, "bytes": bytes, "eof": true,
        });
        match fault {
            "digest" => range["bytes"][0] = json!(0),
            "future_range" => range["schema_version"] = json!(2),
            "future_reference" => range["payload"]["schema_version"] = json!(2),
            "foreign_observation" => range["observation_id"] = json!("other-observer"),
            "foreign_session" => range["payload"]["session_id"] = json!("other-session"),
            "wrong_offset" => range["offset"] = json!(1),
            "false_eof" => range["eof"] = json!(false),
            "empty_progress" => {
                range["bytes"] = json!([]);
                range["eof"] = json!(false);
            }
            _ => unreachable!(),
        }
        let peer = Peer::serve_paths(vec![(
            "payload",
            response(
                "200 OK",
                "application/json",
                &serde_json::to_vec(&range).expect("range fixture"),
            ),
        )]);
        let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("client");
        let directory = tempfile::tempdir().expect("private root");
        let path = directory
            .path()
            .canonicalize()
            .expect("physical root")
            .join("corrupt");
        let mut sink = JsonlBundleRecorder::create(
            &path,
            &events[0].observation_id,
            SESSION,
            RecordingOptions::default(),
        )
        .expect("recorder");
        sink.append(&events[0]).expect("ready");
        sink.append(&events[1]).expect("declared capture");
        assert!(
            client
                .record_payloads(&events[1], &mut sink, 1024)
                .await
                .is_err(),
            "fault must fail closed: {fault}"
        );
        assert!(!sink.manifest().evidence_complete, "fault: {fault}");
        assert_ne!(
            sink.manifest().state,
            RecordingState::Finalized,
            "fault: {fault}"
        );
        assert!(
            sink.manifest()
                .payloads
                .values()
                .all(|payload| payload.relative_path.is_none()),
            "corrupt bytes cannot become retained evidence: {fault}"
        );
        peer.finish();
    }
}

#[tokio::test]
async fn multi_range_payload_transfer_records_exact_bytes_and_digest() {
    use bcode_xray::recording::{JsonlBundleRecorder, RecordingOptions, RecordingSink as _};
    let bytes = "dynamic-sized 中文 👩‍💻 e\u{301}\n".repeat(5000).into_bytes();
    assert!(bytes.len() > 128 * 1024);
    let reference = payload_fixture(&bytes);
    let mut events = engine_envelopes().await;
    declare_payload(&mut events[1], &reference);
    let responses = bytes
        .chunks(16 * 1024)
        .enumerate()
        .map(|(index, part)| {
            let offset = index * 16 * 1024;
            let range = json!({"schema_version": 1, "observation_id": events[0].observation_id,
            "payload": reference, "offset": offset, "bytes": part,
            "eof": offset + part.len() == bytes.len()});
            (
                "payload_checked",
                response(
                    "200 OK",
                    "application/json",
                    &serde_json::to_vec(&range).expect("range"),
                ),
            )
        })
        .collect();
    let peer = Peer::serve_paths(responses);
    let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("client");
    let directory = tempfile::tempdir().expect("root");
    let path = directory
        .path()
        .canonicalize()
        .expect("physical root")
        .join("ranges");
    let mut sink = JsonlBundleRecorder::create(
        &path,
        &events[0].observation_id,
        SESSION,
        RecordingOptions::default(),
    )
    .expect("recorder");
    for event in &events {
        sink.append(event).expect("ordered event");
        client
            .record_payloads(event, &mut sink, 16 * 1024)
            .await
            .expect("verified range export");
    }
    let cutoff = serde_json::from_value(events.last().expect("cutoff").event.data.clone())
        .expect("boundary");
    let manifest = sink.finalize(&cutoff).expect("finalized");
    assert!(manifest.evidence_complete);
    assert_eq!(manifest.payloads.len(), 1);
    let retained = manifest
        .payloads
        .get(&reference.payload_id)
        .expect("retained reference");
    assert_eq!(retained.reference, reference);
    let payload_path = path.join(retained.relative_path.as_ref().expect("retained bytes"));
    assert_eq!(std::fs::read(&payload_path).expect("payload"), bytes);
    bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default())
        .expect("independently verified complete bundle");
    let mut damaged = bytes.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 1;
    std::fs::write(&payload_path, &damaged).expect("same-length payload corruption");
    assert!(
        bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default()).is_err(),
        "same-length corruption must fail full digest verification"
    );
    assert_eq!(
        std::fs::read(payload_path).expect("unchanged corruption"),
        damaged,
        "reader must not repair payload"
    );
    peer.finish();
}

struct HostSink {
    hub: XrayHub,
    origin: bcode_xray::XrayCorrelation,
}
impl bcode_xray::XrayCaptureSink for HostSink {
    fn capture_active(&self) -> bool {
        self.hub.capture_active(&self.origin.session_id)
    }
    fn deep_capture_active(&self) -> bool {
        self.hub.deep_capture_active(&self.origin.session_id)
    }
    fn emit(&self, mut event: bcode_xray::XrayEvent) {
        event.correlation = self.origin.clone();
        self.hub.emit(event);
    }
    fn report_external_loss(&self, count: u64) -> bool {
        self.hub
            .context(self.origin.clone())
            .report_external_loss(count)
    }
}
fn load_dynamic_capture_provider() -> bcode_plugin::LoadedPlugin {
    let library = std::path::PathBuf::from(
        std::env::var_os("BCODE_XRAY_TEST_PROVIDER_LIBRARY")
            .expect("identified fake-provider dynamic library"),
    );
    assert!(library.is_absolute());
    let mut manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
        "../../../plugins/fake-provider-plugin/bcode-plugin.toml"
    ))
    .expect("real fake-provider manifest");
    let bcode_plugin::PluginRuntime::Native(runtime) = &mut manifest.runtime;
    runtime.library.clone_from(&library);
    let plugin = bcode_plugin::load_registered_plugin(&bcode_plugin::RegisteredPlugin {
        manifest,
        manifest_path: library.with_file_name("fixture-manifest.toml"),
    })
    .expect("real native plugin load");
    plugin.activate().expect("activate");
    plugin
}
fn invoke_dynamic_capture(plugin: &bcode_plugin::LoadedPlugin, hub: &XrayHub, large: &str) {
    let origin = bcode_xray::XrayCorrelation {
        session_id: SESSION.to_owned(),
        run_id: Some("large-native-fixture-run".to_owned()),
        activation_id: Some("large-native-fixture-activation".to_owned()),
        attempt_id: Some("real-dynamic-large".to_owned()),
        ..Default::default()
    };
    let external_hub = hub.clone();
    let authorized_origin = origin.clone();
    let capture = bcode_xray::CaptureContext::with_external_sink(
        origin.clone(),
        std::sync::Arc::new(HostSink {
            hub: hub.clone(),
            origin,
        }),
        move |_untrusted_origin, media_type, bytes| {
            external_hub.capture_payload(&authorized_origin, media_type, bytes)
        },
    );
    assert!(large.len() > 128 * 1024);
    let payload = serde_json::to_vec(&json!({
        "session_id": SESSION, "turn_id": "dynamic-large-turn", "model_id": "fake-echo",
        "system_prompt": large,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "tiny reply"}]}],
        "provider_context": {"settings": {"api_key": "synthetic-never-export-canary"}}
    }))
    .expect("provider request");
    let mut ordinary = Vec::new();
    let result = capture
        .scope_sync(|| {
            plugin.invoke_service_with_events(
                "bcode.model-provider/v3",
                "run_turn",
                payload,
                |event| ordinary.push(event),
            )
        })
        .expect("dynamic invocation");
    assert!(result.error.is_none());
    plugin.deactivate().expect("deactivate");
    assert!(
        !ordinary.is_empty(),
        "ordinary execution still produces provider output"
    );
    assert!(
        ordinary
            .iter()
            .all(|event| !String::from_utf8_lossy(event).contains("synthetic-never-export-canary")),
        "capture credentials never enter ordinary provider output"
    );
    drop(capture);
}
fn dynamic_payload_ranges(
    hub: &XrayHub,
    event: &bcode_xray_models::XrayEnvelope,
    reference: &bcode_xray_models::XrayPayloadRef,
    response_ranges: &mut Vec<(&'static str, Vec<u8>)>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let range = hub
            .read_payload(&bcode_xray_models::XrayReadPayloadRequest {
                schema_version: 1,
                observation_id: event.observation_id.clone(),
                session_id: SESSION.to_owned(),
                payload_id: reference.payload_id.clone(),
                offset: u64::try_from(bytes.len()).expect("offset"),
                length: 16 * 1024,
            })
            .expect("real retained SDK payload");
        bytes.extend_from_slice(&range.bytes);
        response_ranges.push((
            "payload_checked",
            response(
                "200 OK",
                "application/json",
                &serde_json::to_vec(&range).expect("range JSON"),
            ),
        ));
        if range.eof {
            break;
        }
    }
    bytes
}
fn verify_dynamic_capture(
    hub: &XrayHub,
    events: &[bcode_xray_models::XrayEnvelope],
    large: &str,
) -> Vec<(&'static str, Vec<u8>)> {
    let mut response_ranges = Vec::new();
    let mut large_requests = 0;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.domain == "provider.fake"
                && event.event.name == "adapted_request")
            .count(),
        1,
        "first and only real native request must be evidenced"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.event.name.ends_with("_omitted")),
        "large native transfer must not substitute an omission notice"
    );
    for event in events {
        assert!(
            !serde_json::to_string(event)
                .expect("JSON")
                .contains("synthetic-never-export-canary")
        );
        for reference in event
            .payload_references()
            .expect("explicit SDK attachments")
        {
            assert_eq!(
                event.event.correlation.run_id.as_deref(),
                Some("large-native-fixture-run")
            );
            assert_eq!(
                event.event.correlation.activation_id.as_deref(),
                Some("large-native-fixture-activation")
            );
            assert_eq!(
                event.event.correlation.attempt_id.as_deref(),
                Some("real-dynamic-large")
            );
            let bytes = dynamic_payload_ranges(hub, event, &reference, &mut response_ranges);
            assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-never-export-canary"));
            if bytes.len() > 128 * 1024 {
                let retained: serde_json::Value =
                    serde_json::from_slice(&bytes).expect("provider projection");
                assert_eq!(
                    retained["system_prompt"].as_str(),
                    Some(large),
                    "complete real request, not synthetic replacement or truncation"
                );
                large_requests += 1;
            }
        }
    }
    assert_eq!(
        large_requests, 1,
        "real dynamic adapted request must retain full payload through SDK callback"
    );
    response_ranges
}
async fn record_dynamic_capture(
    events: &[bcode_xray_models::XrayEnvelope],
    response_ranges: Vec<(&'static str, Vec<u8>)>,
) {
    use bcode_xray::recording::{JsonlBundleRecorder, RecordingOptions, RecordingSink as _};
    let peer = Peer::serve_paths(response_ranges);
    let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("client");
    let directory = tempfile::tempdir().expect("private root");
    let path = directory
        .path()
        .canonicalize()
        .expect("physical root")
        .join("dynamic");
    let mut sink = JsonlBundleRecorder::create(
        &path,
        &events[0].observation_id,
        SESSION,
        RecordingOptions::default(),
    )
    .expect("recorder");
    for event in events {
        sink.append(event).expect("record ordered real event");
        client
            .record_payloads(event, &mut sink, 16 * 1024)
            .await
            .expect("real dynamic bytes exported through client");
    }
    let cutoff = serde_json::from_value(events.last().expect("cutoff").event.data.clone())
        .expect("boundary");
    assert!(
        sink.finalize(&cutoff)
            .expect("recording cutoff")
            .evidence_complete
    );
    peer.finish();
}
#[tokio::test]
#[ignore = "requires explicitly identified BCODE_XRAY_TEST_PROVIDER_LIBRARY; no compilation or provider network calls"]
async fn real_dynamic_large_request_crosses_sdk_capture_and_verified_range_export() {
    let plugin = load_dynamic_capture_provider();
    let hub = XrayHub::default();
    let mut observe_request = request();
    observe_request.deep_capture = true;
    let mut observation = hub.observe(observe_request).expect("deep readiness");
    let large = "large-real-dynamic 中文 👩‍💻 e\u{301}\n".repeat(5000);
    invoke_dynamic_capture(&plugin, &hub, &large);
    drop(plugin);
    observation.finalize().expect("cutoff");
    let mut events = Vec::new();
    while let Some(event) = observation.recv().await {
        events.push(event);
    }
    let response_ranges = verify_dynamic_capture(&hub, &events, &large);
    record_dynamic_capture(&events, response_ranges).await;
}

#[tokio::test]
async fn stalled_observer_never_blocks_producer_and_loss_cannot_be_recorded_as_complete() {
    use bcode_xray::recording::{JsonlBundleRecorder, RecordingOptions, RecordingSink as _};
    let hub = XrayHub::new(bcode_xray::XrayConfig {
        queue_capacity: 4,
        ..Default::default()
    });
    let mut observation = hub.observe(request()).expect("slow observer");
    let producer_hub = hub.clone();
    let (done, completed) = std::sync::mpsc::channel();
    let producer = std::thread::spawn(move || {
        let context = producer_hub.context(bcode_xray::XrayCorrelation {
            session_id: SESSION.to_owned(),
            ..Default::default()
        });
        for index in 0..20_000 {
            context.emit("fixture", "burst", json!({"index": index}), false);
        }
        done.send(()).expect("producer completion");
    });
    // The receiver deliberately consumes nothing until producer completion. This
    // catches backpressure/deadlock without imposing a throughput microbenchmark.
    completed
        .recv_timeout(Duration::from_secs(10))
        .expect("stalled consumer must not hold execution hostage");
    producer.join().expect("producer");
    let cutoff = observation
        .finalize()
        .expect("cutoff independent of consumer");
    assert!(
        cutoff.dropped_events > 0,
        "bounded queue must report actual loss"
    );
    let directory = tempfile::tempdir().expect("root");
    let path = directory
        .path()
        .canonicalize()
        .expect("physical root")
        .join("slow");
    let mut sink = None;
    let mut gaps = 0;
    while let Some(event) = observation.recv().await {
        let recorder = sink.get_or_insert_with(|| {
            JsonlBundleRecorder::create(
                &path,
                &event.observation_id,
                SESSION,
                RecordingOptions::default(),
            )
            .expect("recorder")
        });
        if event.event.domain == "xray" && event.event.name == "gap" {
            gaps += 1;
        }
        recorder
            .append(&event)
            .expect("loss is valid diagnostic evidence");
    }
    assert!(gaps > 0);
    let manifest = sink
        .expect("ready cannot be crowded out")
        .finalize(&cutoff)
        .expect("local finalized evidence");
    assert!(!manifest.evidence_complete);
    assert!(manifest.missing_sequences > 0);
    bcode_xray::recording::inspect_bundle(&path, &RecordingOptions::default())
        .expect("loss-bearing recording remains verifiable");
}

#[tokio::test]
async fn dropping_one_observation_revokes_its_payloads_without_revoking_another_or_foreign_session()
{
    let hub = XrayHub::default();
    let mut deep = request();
    deep.deep_capture = true;
    let mut first = hub.observe(deep.clone()).expect("first deep grant");
    let mut second = hub.observe(deep).expect("independent deep grant");
    let mut foreign_request = request();
    foreign_request.session_id = "foreign-session".to_owned();
    foreign_request.deep_capture = true;
    let mut foreign = hub.observe(foreign_request).expect("foreign observation");
    let first_id = first.recv().await.expect("first ready").observation_id;
    let second_id = second.recv().await.expect("second ready").observation_id;
    let foreign_id = foreign.recv().await.expect("foreign ready").observation_id;
    let correlation = bcode_xray::XrayCorrelation {
        session_id: SESSION.to_owned(),
        ..Default::default()
    };
    let reference = hub
        .capture_payload(&correlation, "text/plain", b"observation-owned")
        .expect("capture")
        .expect("authorized active deep observer");
    let read =
        |observation_id: String, session_id: &str| bcode_xray_models::XrayReadPayloadRequest {
            schema_version: 1,
            observation_id,
            session_id: session_id.to_owned(),
            payload_id: reference.payload_id.clone(),
            offset: 0,
            length: 64,
        };
    assert!(hub.read_payload(&read(first_id.clone(), SESSION)).is_ok());
    assert!(
        hub.read_payload(&read(foreign_id.clone(), SESSION))
            .is_err()
    );
    assert!(
        hub.read_payload(&read(foreign_id, "foreign-session"))
            .is_err()
    );
    drop(first);
    assert!(
        hub.read_payload(&read(first_id, SESSION)).is_err(),
        "dropped scope no longer owns retained bytes"
    );
    assert_eq!(
        hub.read_payload(&read(second_id, SESSION))
            .expect("other observation unaffected")
            .bytes,
        b"observation-owned"
    );
    hub.context(correlation)
        .emit("fixture", "execution_continues", json!({}), false);
    let event = second.recv().await.expect("remaining live observation");
    assert_eq!(event.event.name, "execution_continues");
}

#[tokio::test]
async fn independently_future_capture_metadata_is_rejected_before_export() {
    let events = engine_envelopes().await;
    let mut wire = serde_json::to_value(&events[1]).expect("event JSON");
    wire["event"]["capture"] = json!({"schema_version": 2, "attachments": []});
    let mut body = format!(
        "event: xray\ndata: {}\n\n",
        serde_json::to_string(&events[0]).expect("ready")
    )
    .into_bytes();
    body.extend_from_slice(format!("event: xray\ndata: {wire}\n\n").as_bytes());
    let peer = Peer::serve(vec![response("200 OK", "text/event-stream", &body)]);
    let client = XrayClient::new(&peer.endpoint(), TOKEN).expect("client");
    let mut stream = client.observe(request()).await.expect("ready");
    stream.recv().await.expect("ready read");
    assert!(
        stream.recv().await.is_err(),
        "capture schema evolves independently of envelope version"
    );
    peer.finish();
}

/// Executable-owned fixture: the daemon artifact and every CLI client are the same
/// identified file. No client from this test binary is used to cross artifact versions.
#[cfg(unix)]
mod production {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;
    use wait_timeout::ChildExt as _;

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                // Only a directly spawned child is terminated. Never discover a PID,
                // stop-all, force a daemon, or connect to ambient application state.
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    struct App {
        _directory: PathBuf,
        socket_directory: tempfile::TempDir,
        root: PathBuf,
        binary: PathBuf,
        environment: BTreeMap<String, String>,
        server: Option<OwnedChild>,
        invocation: usize,
    }

    impl Drop for App {
        fn drop(&mut self) {
            // Reap owned foreground process before private roots are removed, even on panic.
            drop(self.server.take());
        }
    }

    impl App {
        fn start() -> Self {
            Self::start_with_settings("")
        }

        fn start_with_settings(settings: &str) -> Self {
            let binary = PathBuf::from(
                std::env::var_os("BCODE_XRAY_TEST_BINARY").expect("use identified-artifact runner"),
            );
            assert!(binary.is_absolute());
            let directory = tempfile::Builder::new()
                .prefix("xray-app-")
                .tempdir()
                .expect("private whole-app root")
                .keep();
            let root = directory.canonicalize().expect("physical root");
            eprintln!("private whole-app diagnostics retained: {}", root.display());
            for name in ["home", "xdg", "state", "tmp", "project"] {
                std::fs::create_dir(root.join(name)).expect("isolated directory");
            }
            // Keep Unix endpoint paths short, including on macOS's long TMPDIR.
            let socket_directory = tempfile::Builder::new()
                .prefix("xr-")
                .tempdir_in("/tmp")
                .expect("short private IPC root");
            let socket = socket_directory.path().join("s");
            let config = root.join("bcode.toml");
            let configuration = format!(
                r#"
[plugins]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents"]
[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
[model.prompt_cache]
mode = "off"
[model.settings]
fake_stream_delta_delay_ms = "10"
{settings}
[observability]
persist_model_requests = false
persist_tool_io = false
"#
            );
            std::fs::write(&config, configuration).expect("deliberate offline config");
            let environment = BTreeMap::from([
                (
                    "PATH".to_owned(),
                    "/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
                ),
                ("HOME".to_owned(), root.join("home").display().to_string()),
                ("TMPDIR".to_owned(), root.join("tmp").display().to_string()),
                (
                    "XDG_CONFIG_HOME".to_owned(),
                    root.join("xdg").display().to_string(),
                ),
                ("BCODE_CONFIG".to_owned(), config.display().to_string()),
                (
                    "BCODE_STATE_DIR".to_owned(),
                    root.join("state").display().to_string(),
                ),
                ("BCODE_SOCKET".to_owned(), socket.display().to_string()),
            ]);
            let mut app = Self {
                _directory: directory,
                socket_directory,
                root,
                binary,
                environment,
                server: None,
                invocation: 0,
            };
            app.start_server();
            app
        }

        fn start_server(&mut self) {
            let socket = self.socket_directory.path().join("s");
            let child = self.spawn(&["server", "run"], "server");
            self.server = Some(child);
            // A successful application command is readiness, not socket existence.
            let deadline = std::time::Instant::now() + Duration::from_secs(45);
            loop {
                assert!(
                    self.server
                        .as_mut()
                        .expect("owned server")
                        .0
                        .try_wait()
                        .expect("server status")
                        .is_none(),
                    "foreground server exited during startup; private logs: {}",
                    self.root.display()
                );
                let socket_bound = std::fs::symlink_metadata(&socket).is_ok_and(|metadata| {
                    use std::os::unix::fs::FileTypeExt as _;
                    metadata.file_type().is_socket()
                });
                // server status uses normal auto-start policy. Calling it while the
                // foreground server is still materializing its image starts a second
                // coordinator against the same artifact root. Wait for bind first,
                // then require an actual application handshake (bind alone is not ready).
                if socket_bound {
                    let result = self.run(&["server", "status"]);
                    if result.0 {
                        break;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "production startup deadline"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        fn command(&self) -> Command {
            let mut command = Command::new(&self.binary);
            command
                .env_clear()
                .envs(&self.environment)
                .current_dir(self.root.join("project"))
                .arg("--state-root")
                .arg(self.root.join("state"))
                .stdin(Stdio::null());
            command
        }

        fn spawn(&self, args: &[&str], label: &str) -> OwnedChild {
            let mut command = self.command();
            command
                .args(args)
                .stdout(
                    std::fs::File::create(self.root.join(format!("{label}.out"))).expect("stdout"),
                )
                .stderr(
                    std::fs::File::create(self.root.join(format!("{label}.err"))).expect("stderr"),
                );
            OwnedChild(command.spawn().expect("explicit test executable"))
        }

        fn run(&mut self, args: &[&str]) -> (bool, Vec<u8>) {
            self.invocation += 1;
            let label = format!("command-{}", self.invocation);
            let mut child = self.spawn(args, &label);
            let status = child
                .0
                .wait_timeout(Duration::from_secs(45))
                .expect("bounded command")
                .expect("application command deadline");
            (
                status.success(),
                std::fs::read(self.root.join(format!("{label}.out"))).expect("result"),
            )
        }

        fn create(&mut self) -> String {
            let (success, bytes) = self.run(&["session", "create", "xray-acceptance", "--json"]);
            assert!(success, "production session creation failed");
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("summary")["id"]
                .as_str()
                .expect("canonical session identity")
                .to_owned()
        }

        fn events(&self, label: &str) -> Vec<bcode_xray::XrayEnvelope> {
            let bytes = std::fs::read(self.root.join(format!("{label}.out"))).expect("live JSONL");
            // An in-flight final line is not yet an envelope; the CLI flushes complete lines.
            bytes
                .split_inclusive(|byte| *byte == b'\n')
                .filter(|line| line.ends_with(b"\n"))
                .map(|line| serde_json::from_slice(line).expect("production JSONL envelope"))
                .collect()
        }

        fn wait_event(&self, label: &str, predicate: impl Fn(&bcode_xray::XrayEnvelope) -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                if self.events(label).iter().any(&predicate) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "production observation event deadline; private logs: {} ({label}.err and server.err)",
                    self.root.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn stop(self) {
            self.stop_when_idle(false);
        }

        fn stop_after_observer_crash(self) {
            self.stop_when_idle(true);
        }

        fn stop_when_idle(mut self, await_reclamation: bool) {
            // A SIGKILL cannot close its capability. The server reclaims unused
            // endpoints after 60 seconds; allow bounded scheduling/CLI overhead.
            let deadline =
                Instant::now() + Duration::from_secs(if await_reclamation { 75 } else { 45 });
            loop {
                self.invocation += 1;
                let label = format!("command-{}", self.invocation);
                let mut child = self.spawn(&["server", "stop-if-idle", "--wait"], &label);
                let status = child
                    .0
                    .wait_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("bounded idle shutdown wait")
                    .expect("idle shutdown command deadline");
                if status.success() {
                    break;
                }
                let error = std::fs::read_to_string(self.root.join(format!("{label}.err")))
                    .expect("idle shutdown diagnostic");
                assert!(
                    await_reclamation
                        && error
                            .trim()
                            .starts_with("error: client error: server returned error daemon_busy:"),
                    "only isolated idle server may be stopped: {error}"
                );
                assert!(
                    Instant::now() < deadline,
                    "crashed observer capability was not reclaimed: {error}"
                );
                std::thread::sleep(
                    Duration::from_millis(250)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            let status = self
                .server
                .as_mut()
                .expect("server")
                .0
                .wait_timeout(Duration::from_secs(10))
                .expect("server shutdown wait");
            assert!(status.is_some(), "server did not release its lifetime");
        }
    }

    fn model(envelope: &bcode_xray::XrayEnvelope, name: &str) -> bool {
        envelope.event.domain == "model" && envelope.event.name == name
    }

    fn browser_request(
        endpoint: &str,
        token: &str,
        method: &str,
        route: &str,
        origin: &str,
        body: &serde_json::Value,
    ) -> std::io::Result<(u16, String)> {
        let address = endpoint
            .strip_prefix("http://")
            .expect("local endpoint")
            .trim_end_matches('/');
        let mut stream = TcpStream::connect_timeout(
            &address.parse().expect("loopback address"),
            Duration::from_secs(3),
        )?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let body = serde_json::to_vec(body).expect("request JSON");
        let preflight = if method == "OPTIONS" {
            "Access-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: authorization,content-type\r\n"
        } else {
            ""
        };
        let request = format!(
            "{method} /v1/xray/{route} HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\n{preflight}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(request.as_bytes())?;
        stream.write_all(&body)?;
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            headers.push(byte[0]);
            assert!(headers.len() <= 16 * 1024, "bounded HTTP response headers");
        }
        let headers = String::from_utf8(headers).expect("HTTP header UTF-8");
        let status = headers
            .lines()
            .next()
            .expect("status line")
            .split_whitespace()
            .nth(1)
            .expect("status code")
            .parse()
            .expect("numeric status");
        assert!(
            !headers.contains(token),
            "bearer bytes cannot enter response headers"
        );
        Ok((status, headers))
    }

    fn browser_handoff(app: &App) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let handoff = loop {
            let bytes =
                std::fs::read(app.root.join("browser.err")).expect("private CLI diagnostics");
            if let Some(value) = bytes
                .split(|byte| *byte == b'\n')
                .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                .find(|value| value["kind"] == "xray_browser_handoff")
            {
                break value;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "browser handoff deadline"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        app.wait_event("browser", |event| {
            event.event.domain == "xray" && event.event.name == "ready"
        });
        handoff
    }
    fn verify_browser_grant(
        app: &mut App,
        endpoint: &str,
        token: &str,
        origin: &str,
        session: &str,
    ) {
        let foreign_session = app.create();
        for route in ["observe", "payload", "finalize", "release"] {
            let (status, headers) =
                browser_request(endpoint, token, "OPTIONS", route, origin, &json!({}))
                    .expect("real preflight");
            assert_eq!(status, 204, "exact Origin preflight: {route}");
            let lower = headers.to_ascii_lowercase();
            assert!(lower.contains("access-control-allow-origin: http://localhost:5173\r\n"));
            assert!(!lower.contains("access-control-allow-credentials:"));
            for denied_origin in [
                "http://localhost:5174",
                "http://127.0.0.1:5173",
                "null",
                "*",
            ] {
                assert_eq!(
                    browser_request(endpoint, token, "OPTIONS", route, denied_origin, &json!({}))
                        .expect("denied preflight")
                        .0,
                    403
                );
                assert_eq!(
                    browser_request(endpoint, token, "POST", route, denied_origin, &json!({}))
                        .expect("denied actual request")
                        .0,
                    403,
                    "actual CORS enforcement: {route}"
                );
            }
        }
        assert_eq!(
            browser_request(
                endpoint,
                token,
                "POST",
                "observe",
                origin,
                &json!({"schema_version": 1, "session_id": foreign_session, "deep_capture": true})
            )
            .expect("foreign session denied")
            .0,
            403
        );
        // The granted Origin cannot turn diagnostic access into steering routes.
        assert_eq!(
            browser_request(
                endpoint,
                token,
                "POST",
                "cancel",
                origin,
                &json!({"session_id": session})
            )
            .expect("no execution control route")
            .0,
            404
        );
    }
    fn verify_browser_expiry(
        observer: &mut OwnedChild,
        endpoint: &str,
        token: &str,
        origin: &str,
        credential: &std::path::Path,
        observation_id: &str,
    ) {
        // Expiry must terminate existing deep capture, not only new preflights.
        let expired = observer
            .0
            .wait_timeout(Duration::from_secs(25))
            .expect("bounded expiry")
            .expect("expiry closes active observation");
        assert!(
            !expired.success(),
            "capability expiry is evidence interruption, not a successful requested cutoff"
        );
        assert!(
            !credential.exists(),
            "expired capability file must be revoked by owner"
        );
        for route in ["observe", "payload", "finalize", "release"] {
            match browser_request(
                endpoint,
                token,
                "POST",
                route,
                origin,
                &json!({"schema_version": 1, "observation_id": observation_id}),
            ) {
                Ok((status, _)) => {
                    assert_eq!(status, 403, "expired capabilities cannot invoke {route}");
                }
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::BrokenPipe
                    ),
                    "expiry must close transport or explicitly forbid, not hang: {error}"
                ),
            }
        }
    }
    #[test]
    #[ignore = "whole-app fixture requires identified bcode with static fake provider and browser Origin bootstrap"]
    fn production_browser_origin_is_exact_expiring_and_does_not_steer_execution() {
        let mut app = App::start();
        let session = app.create();
        let origin = "http://localhost:5173";
        let mut observer = app.spawn(
            &[
                "session",
                "xray",
                &session,
                "--deep",
                "--allow-origin",
                origin,
                "--origin-lifetime-seconds",
                "12",
                "--browser-handoff",
            ],
            "browser",
        );
        let handoff = browser_handoff(&app);
        let endpoint = handoff["endpoint"].as_str().expect("endpoint locator");
        let credential = PathBuf::from(
            handoff["credential_file"]
                .as_str()
                .expect("credential locator"),
        );
        assert!(
            credential.starts_with(app.root.join("state")),
            "daemon-owned private capability"
        );
        let token = std::fs::read_to_string(&credential)
            .expect("explicit out-of-band credential provisioning");
        let token = token.trim();
        assert!(
            !serde_json::to_string(&handoff)
                .expect("handoff JSON")
                .contains(token)
        );
        let observation_id = app.events("browser")[0].observation_id.clone();
        verify_browser_grant(&mut app, endpoint, token, origin, &session);
        verify_browser_expiry(
            &mut observer,
            endpoint,
            token,
            origin,
            &credential,
            &observation_id,
        );
        let (success, bytes) = app.run(&[
            "send",
            &session,
            "ordinary work after browser expiry",
            "--wait",
            "--json",
            "--timeout-secs",
            "20",
        ]);
        assert!(success, "expiry never cancels session execution");
        let result: serde_json::Value = serde_json::from_slice(&bytes).expect("terminal receipt");
        assert_eq!(result["outcome"], "completed");
        assert!(
            !std::fs::read_to_string(app.root.join("browser.out"))
                .expect("JSONL")
                .contains(token)
        );
        app.stop();
    }

    #[test]
    #[ignore = "whole-app fixture requires identified bcode with static fake provider"]
    fn production_observer_disconnection_is_isolated_and_never_cancels_session_work() {
        let mut app = App::start();
        let session = app.create();
        let foreign_session = app.create();
        let mut first = app.spawn(&["session", "xray", &session], "isolation-first");
        // This case tests disconnection isolation, not simultaneous bootstrap
        // admission against one session's bounded read/ownership gate.
        app.wait_event("isolation-first", |event| {
            event.event.domain == "xray" && event.event.name == "ready"
        });
        let mut second = app.spawn(
            &["session", "xray", &session, "--duration-seconds", "15"],
            "isolation-second",
        );
        app.wait_event("isolation-second", |event| {
            event.event.domain == "xray" && event.event.name == "ready"
        });
        let first_id = app.events("isolation-first")[0].observation_id.clone();
        let second_id = app.events("isolation-second")[0].observation_id.clone();
        assert_ne!(first_id, second_id);
        let (success, _) = app.run(&[
            "send",
            &foreign_session,
            "FOREIGN_SESSION_ISOLATION_CANARY",
            "--wait",
            "--json",
            "--timeout-secs",
            "20",
        ]);
        assert!(success);
        // Disconnect just one directly owned client, without any cancel command.
        first.0.kill().expect("disconnect owned observer");
        first.0.wait().expect("reap disconnected observer");
        let (success, bytes) = app.run(&[
            "send",
            &session,
            "work after observer disconnection",
            "--wait",
            "--json",
            "--timeout-secs",
            "20",
        ]);
        assert!(
            success,
            "observer disconnection must never cancel or block ordinary execution"
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&bytes).expect("terminal task receipt");
        assert_eq!(receipt["outcome"], "completed");
        let turn = receipt["receipt"]["turn_id"]
            .as_str()
            .expect("turn identity");
        app.wait_event("isolation-second", |event| {
            model(event, "turn_finished")
                && event.event.correlation.turn_id.as_deref() == Some(turn)
        });
        assert!(
            second
                .0
                .wait_timeout(Duration::from_secs(30))
                .expect("bounded drain")
                .expect("cutoff")
                .success()
        );
        let events = app.events("isolation-second");
        assert!(events.iter().all(|event| event.observation_id == second_id
            && event.event.correlation.session_id == session));
        assert!(
            !serde_json::to_string(&events)
                .expect("event JSON")
                .contains("FOREIGN_SESSION_ISOLATION_CANARY")
        );
        assert!(
            events
                .iter()
                .any(|event| event.event.domain == "xray" && event.event.name == "finalized")
        );
        app.stop_after_observer_crash();
    }

    fn interrupt_restart_segment(app: &mut App, session: &str) -> (PathBuf, String, Vec<u8>) {
        use bcode_xray::recording::{RecordingOptions, RecordingState};
        let first_bundle = app.root.join("before-restart");
        let mut old_observer = app.spawn(
            &[
                "session",
                "xray",
                session,
                "--record",
                first_bundle.to_str().expect("bundle"),
            ],
            "restart-old",
        );
        app.wait_event("restart-old", |event| {
            event.event.domain == "xray" && event.event.name == "ready"
        });
        let previous = app.events("restart-old")[0].observation_id.clone();
        // Only this fixture's directly owned foreground child is killed. This is
        // deliberate transport loss, not a supported observer cancellation API.
        let mut server = app.server.take().expect("owned foreground child");
        server.0.kill().expect("crash fixture daemon");
        server.0.wait().expect("reap fixture daemon");
        let status = old_observer
            .0
            .wait_timeout(Duration::from_secs(25))
            .expect("bounded observer exit")
            .expect("EOF must end old observer");
        assert!(
            !status.success(),
            "daemon death cannot become successful finalization"
        );
        let before =
            bcode_xray::recording::inspect_bundle(&first_bundle, &RecordingOptions::default())
                .expect("interrupted recording remains inspectable");
        assert_eq!(before.state, RecordingState::Interrupted);
        assert!(!before.evidence_complete);
        assert!(
            before.finalization.is_none(),
            "no synthesized terminal evidence after daemon death"
        );
        let immutable = std::fs::read(first_bundle.join("events.jsonl")).expect("first segment");
        (first_bundle, previous, immutable)
    }
    #[test]
    #[ignore = "whole-app fixture requires identified bcode with static fake provider"]
    fn production_daemon_restart_interrupts_old_bundle_and_new_segment_is_explicitly_gapful() {
        use bcode_xray::recording::{
            CaptureSegmentBoundary, JsonlBundleRecorder, RecordingOptions, RecordingSink as _,
            RecordingState,
        };
        let mut app = App::start();
        let session = app.create();
        let (first_bundle, previous, immutable) = interrupt_restart_segment(&mut app, &session);
        app.start_server();
        let mut new_observer = app.spawn(
            &["session", "xray", &session, "--duration-seconds", "10"],
            "restart-new",
        );
        app.wait_event("restart-new", |event| {
            event.event.domain == "xray" && event.event.name == "ready"
        });
        let fresh = app.events("restart-new")[0].observation_id.clone();
        assert_ne!(
            fresh, previous,
            "restart creates a new live observation, not a resume cursor"
        );
        let (success, _) = app.run(&[
            "send",
            &session,
            "after real daemon restart",
            "--wait",
            "--json",
            "--timeout-secs",
            "20",
        ]);
        assert!(success, "ordinary session work survives daemon replacement");
        assert!(
            new_observer
                .0
                .wait_timeout(Duration::from_secs(30))
                .expect("drain")
                .expect("new cutoff")
                .success()
        );
        let events = app.events("restart-new");
        assert_eq!(events[0].sequence, 1);
        assert!(events.iter().all(|event| event.observation_id == fresh));
        let destination = app.root.join("after-restart");
        let mut sink = JsonlBundleRecorder::create_segment(
            &destination,
            &fresh,
            &session,
            &previous,
            RecordingOptions::default(),
        )
        .expect("explicit new gapful segment");
        for event in &events {
            sink.append(event).expect("new segment event");
        }
        let cutoff =
            serde_json::from_value(events.last().expect("cutoff event").event.data.clone())
                .expect("boundary");
        let after = sink.finalize(&cutoff).expect("new segment finalized");
        assert_eq!(after.state, RecordingState::Finalized);
        assert_eq!(
            after.segment,
            CaptureSegmentBoundary::RestartAfterGap {
                previous_observation_id: previous
            }
        );
        assert!(
            !after.evidence_complete,
            "locally finalized restart segment cannot erase the interruption"
        );
        bcode_xray::recording::inspect_bundle(&destination, &RecordingOptions::default())
            .expect("verified gapful bundle");
        assert_eq!(
            std::fs::read(first_bundle.join("events.jsonl")).expect("old segment unchanged"),
            immutable
        );
        app.stop();
    }

    #[test]
    #[ignore = "whole-app fixture requires identified bcode with static fake-provider/filesystem/default-agents"]
    fn production_auto_bootstrap_observes_idle_two_real_turns_and_local_recording() {
        let mut app = App::start();
        let session = app.create();
        std::fs::write(app.root.join("project/fixture.txt"), "xray tool fixture")
            .expect("tool input");
        let bundle = app.root.join("recording");
        let mut observer = app.spawn(
            &[
                "session",
                "xray",
                &session,
                "--deep",
                "--record",
                bundle.to_str().expect("bundle path"),
                "--duration-seconds",
                "40",
            ],
            "observe",
        );
        app.wait_event("observe", |event| {
            event.event.domain == "xray" && event.event.name == "ready"
        });
        let initial = app.events("observe");
        assert_eq!(initial[0].sequence, 1);
        assert_eq!(initial[0].event.correlation.session_id, session);
        assert!(
            !initial.iter().any(|event| model(event, "turn_started")),
            "ready precedes first prompt"
        );

        let (success, first) = app.run(&[
            "send",
            &session,
            "xray first 中文 👩‍💻",
            "--wait",
            "--json",
            "--timeout-secs",
            "20",
        ]);
        assert!(success, "first real model turn failed");
        let first: serde_json::Value =
            serde_json::from_slice(&first).expect("first terminal receipt");
        assert_eq!(first["outcome"], "completed");
        let first_turn = first["receipt"]["turn_id"].as_str().expect("turn identity");
        app.wait_event("observe", |event| {
            model(event, "turn_finished")
                && event.event.correlation.turn_id.as_deref() == Some(first_turn)
        });
        // Terminal application receipt establishes idle, not a sleep or empty registry.
        assert!(
            observer.0.try_wait().expect("observer state").is_none(),
            "observer survives turn terminal"
        );
        let observation_id = initial[0].observation_id.clone();
        let (success, second) = app.run(&[
            "send",
            &session,
            "tool-read fixture.txt",
            "--wait",
            "--json",
            "--timeout-secs",
            "20",
        ]);
        assert!(success, "real filesystem tool turn failed");
        let second: serde_json::Value =
            serde_json::from_slice(&second).expect("second terminal receipt");
        assert_eq!(second["outcome"], "completed");
        assert_ne!(first["receipt"]["turn_id"], second["receipt"]["turn_id"]);
        let status = observer
            .0
            .wait_timeout(Duration::from_secs(55))
            .expect("observation drain")
            .expect("duration must finalize only the observation");
        assert!(status.success(), "production CLI finalization failed");
        let events = app.events("observe");
        assert_completed_tool_chain(&events, &second);
        assert_observation_identity(&events, &observation_id, &session);
        assert_recording_cutoff(&events, &bundle, &observation_id);
        // No observer is needed for continued execution; finalization never cancels work.
        assert!(
            app.run(&[
                "send",
                &session,
                "after observation",
                "--wait",
                "--json",
                "--timeout-secs",
                "20"
            ])
            .0
        );
        app.stop();
    }

    fn assert_observation_identity(
        events: &[bcode_xray::XrayEnvelope],
        observation_id: &str,
        session: &str,
    ) {
        assert!(
            events
                .iter()
                .all(|event| event.observation_id == observation_id
                    && event.event.correlation.session_id == session)
        );
        assert!(
            events
                .iter()
                .any(|event| model(event, "text_delta") || model(event, "output"))
        );
    }

    fn assert_completed_tool_chain(
        events: &[bcode_xray::XrayEnvelope],
        second: &serde_json::Value,
    ) {
        let tool_turn = second["receipt"]["turn_id"]
            .as_str()
            .expect("tool turn identity");
        let completed_call = events
            .iter()
            .find(|event| {
                event.event.domain == "session"
                    && event.event.name == "tool_invocation_lifecycle"
                    && event.event.correlation.turn_id.as_deref() == Some(tool_turn)
                    && event.event.data["stage"] == "completed"
            })
            .expect("canonical tool lifecycle must complete in the originating turn");
        let call_id = completed_call
            .event
            .correlation
            .tool_call_id
            .as_deref()
            .expect("tool identity");
        assert!(
            events.iter().any(|event| model(event, "output")
                && event.event.correlation.turn_id.as_deref() == Some(tool_turn)
                && event.event.correlation.tool_call_id.as_deref() == Some(call_id)
                && event.event.data["output"]["tool_call_id"] == call_id),
            "positioned model output must match the actually completed tool"
        );
        assert!(
            events.iter().any(|event| event.event.domain == "runtime"
                && event.event.correlation.tool_call_id.as_deref() == Some(call_id)
                && event.event.name == "tool_result"
                && event.event.data["is_error"] == false
                && event.event.data["output_bytes"]
                    .as_u64()
                    .is_some_and(|bytes| bytes > 0)),
            "actual tool execution must succeed, not only model tool-call generation"
        );
    }

    fn assert_recording_cutoff(
        events: &[bcode_xray::XrayEnvelope],
        bundle: &std::path::Path,
        observation_id: &str,
    ) {
        assert!(
            events
                .iter()
                .any(|event| event.event.domain == "provider.fake"
                    && event.event.name == "adapted_request"),
            "SDK/host provider diagnostic bridge is required"
        );
        let turn_ids: BTreeSet<_> = events
            .iter()
            .filter(|event| {
                event.event.domain == "session" && event.event.name == "model_turn_started"
            })
            .filter_map(|event| event.event.correlation.turn_id.as_deref())
            .collect();
        assert_eq!(
            turn_ids.len(),
            2,
            "one observer spans exactly two application turns"
        );
        assert_eq!(
            events.last().expect("final envelope").event.name,
            "finalized"
        );
        let manifest: bcode_xray::recording::RecordingManifest =
            serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).expect("manifest"))
                .expect("bundle contract");
        assert_eq!(
            manifest.state,
            bcode_xray::recording::RecordingState::Finalized
        );
        assert!(
            manifest.evidence_complete,
            "whole-app fixture requires complete evidence; cutoff: {:?}, missing sequences: {}, omitted events: {}; private bundle: {}",
            manifest.finalization,
            manifest.missing_sequences,
            manifest.omitted_events,
            bundle.display()
        );
        assert_eq!(manifest.observation_id, observation_id);
        assert_eq!(
            manifest.events.envelope_count,
            u64::try_from(events.len()).expect("bounded count")
        );
    }

    #[test]
    #[ignore = "whole-app fixture requires identified bcode with static fake provider"]
    fn production_shallow_observer_sees_exact_cancellation_without_deep_capture() {
        let mut app = App::start_with_settings("fake_turn_delay_ms = \"30000\"");
        let session = app.create();
        let mut observer = app.spawn(
            &["session", "xray", &session, "--duration-seconds", "90"],
            "cancel-observe",
        );
        app.wait_event("cancel-observe", |event| {
            event.event.name == "ready" && event.event.domain == "xray"
        });
        let mut submission = app.spawn(
            &[
                "send",
                &session,
                "cancel this real fake-provider turn",
                "--wait",
                "--json",
                "--timeout-secs",
                "60",
            ],
            "cancel-submission",
        );
        app.wait_event("cancel-observe", |event| model(event, "turn_started"));
        let started = app
            .events("cancel-observe")
            .into_iter()
            .find(|event| model(event, "turn_started"))
            .expect("originating turn");
        let turn = started
            .event
            .correlation
            .turn_id
            .as_deref()
            .expect("application turn identity");
        let (accepted, acknowledgement) =
            app.run(&["cancel", &session, "--turn-id", turn, "--json"]);
        assert!(accepted, "exact-turn cancellation command failed");
        let acknowledgement: serde_json::Value =
            serde_json::from_slice(&acknowledgement).expect("typed cancellation acknowledgement");
        assert_eq!(
            acknowledgement["cancellation_requested"], true,
            "fixture must cancel active work, not race an already completed turn"
        );
        app.wait_event("cancel-observe", |event| {
            event.event.domain == "session"
                && event.event.name == "model_turn_finished"
                && event.event.data["outcome"] == "cancelled"
                && event.event.correlation.turn_id.as_deref() == Some(turn)
        });
        let terminal = submission
            .0
            .wait_timeout(Duration::from_secs(25))
            .expect("terminal observation")
            .expect("cancelled turn must settle");
        assert!(
            !terminal.success(),
            "cancelled CLI task exits nonzero independently of evidence"
        );
        let receipt: serde_json::Value = serde_json::from_slice(
            &std::fs::read(app.root.join("cancel-submission.out")).expect("canonical outcome"),
        )
        .expect("terminal task receipt");
        assert_eq!(receipt["outcome"], "cancelled");
        assert_eq!(receipt["receipt"]["turn_id"], turn);
        assert!(
            observer.0.try_wait().expect("observer status").is_none(),
            "turn cancellation cannot close observer"
        );
        let status = observer
            .0
            .wait_timeout(Duration::from_secs(100))
            .expect("drain")
            .expect("finalization deadline");
        assert!(status.success());
        let events = app.events("cancel-observe");
        assert_eq!(events[0].event.data["deep_capture"], false);
        assert!(events.iter().all(|event| !event.event.sensitive));
        assert!(
            !events
                .iter()
                .any(|event| event.event.domain == "provider.fake")
        );
        assert_eq!(events.last().expect("cutoff").event.name, "finalized");
        app.stop();
    }

    #[test]
    #[ignore = "whole-app fixture requires identified bcode with static fake provider"]
    fn production_provider_error_is_evidence_not_an_observation_failure() {
        // The real fake adapter emits a one-time context overflow. Runtime recovery may
        // succeed or fail; neither outcome may turn missing diagnostic evidence into success.
        let mut app = App::start_with_settings("fake_context_overflow_once = \"true\"");
        let session = app.create();
        let mut observer = app.spawn(
            &["session", "xray", &session, "--duration-seconds", "20"],
            "error-observe",
        );
        app.wait_event("error-observe", |event| {
            event.event.domain == "xray" && event.event.name == "ready"
        });
        let (_, result) = app.run(&[
            "send",
            &session,
            "exercise provider error",
            "--wait",
            "--json",
            "--timeout-secs",
            "20",
        ]);
        let result: serde_json::Value =
            serde_json::from_slice(&result).expect("authoritative terminal task receipt");
        assert!(
            result["outcome"].is_string(),
            "task outcome is distinct from evidence outcome"
        );
        app.wait_event("error-observe", |event| model(event, "error"));
        let status = observer
            .0
            .wait_timeout(Duration::from_secs(35))
            .expect("drain")
            .expect("cutoff deadline");
        assert!(
            status.success(),
            "provider error cannot masquerade as transport failure"
        );
        let events = app.events("error-observe");
        let error = events
            .iter()
            .find(|event| model(event, "error"))
            .expect("normalized error observation");
        assert_eq!(
            error.event.data,
            json!({"outcome": "error", "category": "context_length"})
        );
        assert_eq!(events.last().expect("cutoff").event.name, "finalized");
        app.stop();
    }
}
