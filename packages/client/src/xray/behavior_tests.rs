//! Behavioral protocol tests; no running daemon, providers or canonical storage needed.

use super::*;
use std::io::{Read as _, Write as _};

#[test]
fn cloned_payload_drain_is_nonrenewable_and_bounds_new_requests() {
    let client = XrayClient::new("http://127.0.0.1:1234", "fixture-token").unwrap();
    let queued_exporter = client.clone();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    client.close_capture_for_drain(deadline);
    queued_exporter.begin_payload_drain();
    assert_eq!(*client.drain_expiry.lock().unwrap(), Some(deadline));
    let request = queued_exporter
        .bounded_post("v1/xray/payload")
        .unwrap()
        .build()
        .unwrap();
    assert!(request.timeout().unwrap() <= &std::time::Duration::from_secs(2));
    client.close_capture_for_drain(tokio::time::Instant::now());
    assert!(matches!(
        queued_exporter.bounded_post("v1/xray/payload"),
        Err(XrayClientError::InvalidCredential)
    ));
    assert!(matches!(
        client.ensure_active(),
        Err(XrayClientError::InvalidCredential)
    ));
}

fn envelope(data: &serde_json::Value) -> XrayEnvelope {
    serde_json::from_value(serde_json::json!({
        "schema_version": 1, "observation_id": "observation", "sequence": 1,
        "timestamp_unix_ms": 0,
        "event": {"domain": "plugin", "name": "diagnostic", "data": data,
            "correlation": {"session_id": "session"}, "sensitive": false}
    }))
    .unwrap()
}

fn reference() -> serde_json::Value {
    serde_json::json!({"schema_version": 1, "payload_id": "payload", "session_id": "session",
        "content_type": "text/plain", "byte_len": 3,
        "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"})
}

fn declare(envelope: &mut XrayEnvelope, pointer: &str) {
    envelope.event.capture = Some(bcode_xray_models::XrayCaptureAttachments::new(vec![
        bcode_xray_models::XrayCaptureAttachment::PayloadRef {
            pointer: pointer.into(),
        },
    ]));
}

#[test]
fn only_declared_attachments_are_resolved_and_verified() {
    let mut event =
        envelope(&serde_json::json!({"tool": {"payload_id": "ordinary"}, "blob": reference()}));
    assert_eq!(
        event.payload_references().unwrap(),
        [] as [bcode_xray::XrayPayloadRef; 0]
    );
    declare(&mut event, "/blob");
    let refs = event.payload_references().unwrap();
    assert_eq!(refs.len(), 1);
    assert!(validate_payload_bytes(&refs[0], b"abc").is_ok());
    assert!(validate_payload_bytes(&refs[0], b"abd").is_err());
    declare(&mut event, "/tool");
    assert!(event.payload_references().is_err());
}

#[test]
fn forged_foreign_unknown_and_missing_declared_references_fail_closed() {
    let mut event = envelope(&serde_json::json!({"blob": reference()}));
    declare(&mut event, "/absent");
    assert!(event.payload_references().is_err());
    declare(&mut event, "/blob");
    event.event.data["blob"]["session_id"] = serde_json::json!("foreign");
    assert!(event.payload_references().is_err());
    event.event.data["blob"] = reference();
    event.event.data["blob"]["schema_version"] = serde_json::json!(999);
    assert!(event.payload_references().is_err());
    event.event.data["blob"] = reference();
    event.schema_version = 999;
    assert!(event.payload_references().is_err());
    event.schema_version = 1;
    event.event.capture.as_mut().unwrap().schema_version = 999;
    assert!(event.payload_references().is_err());
    let encoded = serde_json::to_value(&event).unwrap();
    assert!(serde_json::from_value::<XrayEnvelope>(encoded).is_err());
}

/// A single bounded local response; assertions must not print credentials on failure.
pub(super) fn server(status: &str, body: String) -> (String, std::thread::JoinHandle<()>) {
    server_responses(vec![(status.to_owned(), "text/event-stream".into(), body)])
}

pub(super) fn server_responses(
    responses: Vec<(String, String, String)>,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for (status, content_type, body) in responses {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "local test client did not connect"
                        );
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("local test accept failed: {error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 16 * 1024);
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            // Consume the bounded request body before closing to avoid a TCP reset.
            let headers = String::from_utf8(request).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            assert!(length < 16 * 1024);
            socket.read_exact(&mut vec![0_u8; length]).unwrap();
            write!(socket, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (origin, handle)
}

pub(super) fn ready(observation: &str, version: u16) -> String {
    let mut event = envelope(&serde_json::json!({
        "schema_version": 1, "deep_capture": false, "durable_replay": false,
        "queue_capacity": 8, "max_event_bytes": 4096, "max_payload_bytes": 4096,
        "max_retained_payload_bytes": 8192, "max_retained_payloads": 4,
        "max_payload_range_bytes": 1024
    }));
    event.schema_version = version;
    event.observation_id = observation.into();
    event.event.domain = "xray".into();
    event.event.name = "ready".into();
    format!("data: {}\n\n", serde_json::to_string(&event).unwrap())
}

pub(super) fn request() -> XrayObserveRequest {
    XrayObserveRequest {
        schema_version: 1,
        session_id: "session".into(),
        deep_capture: false,
    }
}

#[tokio::test]
async fn endpoint_restart_requires_fresh_ready_and_has_no_replay() {
    let (origin, handle) = server("200 OK", ready("first-process", 1));
    let client = XrayClient::new(&origin, "first-token").unwrap();
    let mut first = client.observe(request()).await.unwrap();
    assert_eq!(first.recv().await.unwrap().unwrap().sequence, 1);
    assert!(matches!(
        first.recv().await,
        Err(XrayClientError::Incomplete)
    ));
    handle.join().unwrap();
    drop(first);
    drop(client);
    let (origin, handle) = server("200 OK", ready("second-process", 1));
    let client = XrayClient::new(&origin, "renewed-token").unwrap();
    let mut second = client.observe(request()).await.unwrap();
    let barrier = second.recv().await.unwrap().unwrap();
    assert_eq!(barrier.observation_id, "second-process");
    assert_eq!(barrier.sequence, 1);
    assert!(matches!(
        second.recv().await,
        Err(XrayClientError::Incomplete)
    ));
    handle.join().unwrap();
}

#[tokio::test]
async fn authorization_failure_and_unknown_schema_never_establish_readiness() {
    for status in ["401 Unauthorized", "403 Forbidden"] {
        let (origin, handle) = server(status, String::new());
        let result = XrayClient::new(&origin, "revoked-token")
            .unwrap()
            .observe(request())
            .await;
        assert!(matches!(result, Err(XrayClientError::Http(401 | 403))));
        handle.join().unwrap();
    }
    let (origin, handle) = server("200 OK", ready("future", 999));
    let result = XrayClient::new(&origin, "token")
        .unwrap()
        .observe(request())
        .await;
    assert!(matches!(result, Err(XrayClientError::Protocol)));
    handle.join().unwrap();
}

#[cfg(unix)]
#[test]
fn credential_loading_is_private_bounded_and_never_follows_symlinks() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credential");
    std::fs::write(&path, "private-token").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let client = XrayClient::from_credential_file("http://127.0.0.1:1234", &path).unwrap();
    assert!(!format!("{client:?}").contains("private-token"));
    let link = directory.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(XrayClient::from_credential_file("http://127.0.0.1:1234", &link).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(XrayClient::from_credential_file("http://127.0.0.1:1234", &path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, vec![b'x'; 4097]).unwrap();
    assert!(XrayClient::from_credential_file("http://127.0.0.1:1234", &path).is_err());
}
