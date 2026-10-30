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
        let listener = TcpListener::bind("127.0.0.1:0").expect("isolated loopback listener");
        let address = listener.local_addr().expect("bound address");
        listener.set_nonblocking(true).expect("bounded accept");
        let worker = std::thread::spawn(move || {
            for (index, response) in responses.into_iter().enumerate() {
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
                let target = consume_request(&mut stream);
                let expected_path = if index == 0 { "observe" } else { "release" };
                assert_eq!(target, format!("POST /v1/xray/{expected_path} HTTP/1.1"));
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

fn consume_request(stream: &mut TcpStream) -> String {
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
    target.to_owned()
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

/// Executable-owned fixture: the daemon artifact and every CLI client are the same
/// identified file. No client from this test binary is used to cross artifact versions.
#[cfg(unix)]
mod production {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
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
        _socket_directory: tempfile::TempDir,
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
                _socket_directory: socket_directory,
                root,
                binary,
                environment,
                server: None,
                invocation: 0,
            };
            let child = app.spawn(&["server", "run"], "server");
            app.server = Some(child);
            // A successful application command is readiness, not socket existence.
            let deadline = std::time::Instant::now() + Duration::from_secs(45);
            loop {
                assert!(
                    app.server
                        .as_mut()
                        .expect("owned server")
                        .0
                        .try_wait()
                        .expect("server status")
                        .is_none(),
                    "foreground server exited during startup; private logs: {}",
                    app.root.display()
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
                    let result = app.run(&["server", "status"]);
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
            app
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

        fn stop(mut self) {
            let (success, _) = self.run(&["server", "stop-if-idle", "--wait"]);
            assert!(success, "only isolated idle server may be stopped");
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
