//! Host-independent, bounded application-resurrection control channel.
//!
//! The host authenticates the connecting process and scopes offers to its current
//! execution. Discovery is not authorization and a client-supplied PID is not proof.

use bcode_config::{SessionResurrectionConfig, SessionResurrectionMode};
use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const PROTOCOL: &str = "application-resurrection/1";
const MAX_FRAME: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    application: String,
    version: u32,
    session_store: PathBuf,
    session_id: SessionId,
}

impl Descriptor {
    fn session(&self, authority: &std::path::Path) -> std::io::Result<SessionId> {
        if self.application != "bcode" || self.version != 1 || self.session_store != authority {
            return Err(std::io::Error::other(
                "unsupported resurrection descriptor or session authority mismatch",
            ));
        }
        Ok(self.session_id)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Welcome {
    protocol: String,
    restore: Option<Descriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejected {
    protocol: String,
    error: HostRejection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostRejection {
    code: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum HandshakeResponse {
    Welcome(Welcome),
    Rejected(Rejected),
}

fn read_welcome(reader: &mut impl std::io::BufRead) -> std::io::Result<Welcome> {
    let response: HandshakeResponse = read_frame(reader)?;
    let protocol = match &response {
        HandshakeResponse::Welcome(value) => &value.protocol,
        HandshakeResponse::Rejected(value) => &value.protocol,
    };
    if protocol != PROTOCOL {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported resurrection host protocol",
        ));
    }
    match response {
        HandshakeResponse::Welcome(welcome) => Ok(welcome),
        HandshakeResponse::Rejected(rejected) => {
            // Host data is untrusted: render known reasons, never arbitrary host text.
            let (kind, reason) = match rejected.error.code.as_str() {
                "ownership_verification_unavailable" => (
                    std::io::ErrorKind::Other,
                    "the host could not verify foreground process ownership",
                ),
                "ownership_rejected" => (
                    std::io::ErrorKind::PermissionDenied,
                    "the connecting process does not own the current execution",
                ),
                "invalid_handshake" => (
                    std::io::ErrorKind::InvalidData,
                    "the host rejected the handshake or peer identity",
                ),
                "launch_unavailable" => (
                    std::io::ErrorKind::ConnectionAborted,
                    "the launch is no longer available for restoration",
                ),
                _ => (
                    std::io::ErrorKind::InvalidData,
                    "the host rejected the connection with an unknown error code",
                ),
            };
            Err(std::io::Error::new(kind, reason))
        }
    }
}

fn handshake_error(error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!(
            "Session restoration failed during the host handshake: {error}. To launch without restoration, set BCODE_NO_SESSION_RESURRECTION=1."
        ),
    )
}

#[derive(Serialize)]
struct Hello {
    protocol: &'static str,
    application: &'static str,
    pid: u32,
    accept_restore: bool,
}

#[derive(Serialize)]
struct Selection {
    operation: &'static str,
    revision: u64,
    descriptor: Option<Descriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledgement {
    revision: u64,
}

fn endpoint(
    config: &SessionResurrectionConfig,
    advertisement: Option<&str>,
    disabled: bool,
) -> std::io::Result<Option<PathBuf>> {
    if disabled || config.mode == SessionResurrectionMode::Disabled {
        return Ok(None);
    }
    let advertised = advertisement.and_then(|value| value.strip_prefix("v1:"));
    let selected = match config.mode {
        SessionResurrectionMode::Enabled => config.endpoint.as_deref().or(advertised),
        SessionResurrectionMode::Auto => advertised,
        SessionResurrectionMode::Disabled => None,
    };
    selected
        .map(|value| {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(std::io::Error::other(
                    "resurrection endpoint must be absolute",
                ));
            }
            Ok(path)
        })
        .transpose()
}

/// A bounded latest-selection mailbox; socket I/O runs outside the frame loop.
pub struct Publisher {
    sender: tokio::sync::watch::Sender<Option<SessionId>>,
    last: Option<SessionId>,
    published: bool,
}

impl Publisher {
    pub fn observe(&mut self, attachment: super::session_flow::ChatSessionAttachment) {
        use super::session_flow::ChatSessionAttachment;
        let selected = match attachment {
            ChatSessionAttachment::Opening { .. } => return,
            ChatSessionAttachment::Draft => None,
            ChatSessionAttachment::Attached { session_id }
            | ChatSessionAttachment::Detached { session_id } => Some(session_id),
        };
        if !self.published || self.last != selected {
            self.published = true;
            self.last = selected;
            self.sender.send_replace(selected);
        }
    }
}

pub async fn connect(
    config: &SessionResurrectionConfig,
    explicit_session: Option<SessionId>,
) -> std::io::Result<(Option<SessionId>, Option<Publisher>, Option<String>)> {
    let advertised = std::env::var("APPLICATION_RESURRECTION_HOST").ok();
    let disabled = std::env::var_os("BCODE_NO_SESSION_RESURRECTION").is_some();
    let Some(path) = endpoint(config, advertised.as_deref(), disabled)? else {
        return Ok((explicit_session, None, None));
    };
    connect_endpoint(
        path,
        explicit_session,
        bcode_config::default_session_store_dir(),
        config.mode,
    )
    .await
}

async fn connect_endpoint(
    path: PathBuf,
    explicit_session: Option<SessionId>,
    authority: PathBuf,
    mode: SessionResurrectionMode,
) -> std::io::Result<(Option<SessionId>, Option<Publisher>, Option<String>)> {
    #[cfg(unix)]
    {
        // Only transport discovery failures are optional. Never downgrade handshake
        // or authority validation failures to a successful ordinary launch.
        let connected = tokio::task::spawn_blocking(move || {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(stream) => Ok(Some(stream)),
                Err(error)
                    if mode == SessionResurrectionMode::Auto
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) =>
                {
                    Ok(None)
                }
                Err(error) => Err(std::io::Error::new(
                    error.kind(),
                    format!("connecting to session resurrection host: {error}"),
                )),
            }
        })
        .await
        .map_err(std::io::Error::other)??;
        let Some(stream) = connected else {
            return Ok((
                explicit_session,
                None,
                Some(
                    "Session resurrection host unavailable; continuing without restoration.".into(),
                ),
            ));
        };
        let (stream, restored, authority) = tokio::task::spawn_blocking(move || {
            stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
            stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
            let mut stream = std::io::BufReader::new(stream);
            write_frame(
                stream.get_mut(),
                &Hello {
                    protocol: PROTOCOL,
                    application: "bcode",
                    pid: std::process::id(),
                    accept_restore: explicit_session.is_none(),
                },
            )?;
            let welcome = read_welcome(&mut stream)?;
            let restored = if explicit_session.is_none() {
                welcome
                    .restore
                    .as_ref()
                    .map(|value| {
                        let canonical = authority.canonicalize().map_err(|error| {
                            std::io::Error::new(
                                error.kind(),
                                format!("validating resurrection session store: {error}"),
                            )
                        })?;
                        value.session(&canonical)
                    })
                    .transpose()?
            } else {
                None
            };
            Ok((stream, restored, authority))
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(std::convert::identity)
        .map_err(|error| handshake_error(&error))?;
        Ok((
            explicit_session.or(restored),
            Some(start_publisher(stream, authority)),
            None,
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "local resurrection transport is unavailable on this platform",
        ))
    }
}

#[cfg(unix)]
fn start_publisher(
    mut stream: std::io::BufReader<std::os::unix::net::UnixStream>,
    authority: PathBuf,
) -> Publisher {
    let (sender, mut receiver) = tokio::sync::watch::channel(None);
    tokio::spawn(async move {
        let mut revision = 0_u64;
        while receiver.changed().await.is_ok() {
            let selected = *receiver.borrow_and_update();
            let Some(next) = revision.checked_add(1) else {
                break;
            };
            revision = next;
            let descriptor = match selected {
                Some(session_id) => {
                    let Ok(session_store) = authority.canonicalize() else {
                        tracing::warn!(
                            "session resurrection publication stopped: session store unavailable"
                        );
                        break;
                    };
                    Some(Descriptor {
                        application: "bcode".to_owned(),
                        version: 1,
                        session_store,
                        session_id,
                    })
                }
                None => None,
            };
            let result = tokio::task::spawn_blocking(move || {
                write_frame(
                    stream.get_mut(),
                    &Selection {
                        operation: "selection",
                        revision,
                        descriptor,
                    },
                )?;
                let ack: Acknowledgement = read_frame(&mut stream)?;
                if ack.revision != revision {
                    return Err(std::io::Error::other(
                        "resurrection acknowledgement revision mismatch",
                    ));
                }
                Ok(stream)
            })
            .await;
            match result {
                Ok(Ok(next_stream)) => stream = next_stream,
                error => {
                    tracing::warn!(?error, "session resurrection publication stopped");
                    break;
                }
            }
        }
    });
    Publisher {
        sender,
        last: None,
        published: false,
    }
}

fn write_frame(writer: &mut impl std::io::Write, value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() + 1 >= MAX_FRAME {
        return Err(std::io::Error::other("resurrection frame exceeds limit"));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut impl std::io::BufRead,
) -> std::io::Result<T> {
    use std::io::BufRead;
    let mut bytes = Vec::new();
    std::io::Read::take(reader, MAX_FRAME as u64).read_until(b'\n', &mut bytes)?;
    if bytes.len() >= MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "resurrection response exceeds protocol limit",
        ));
    }
    if bytes.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "resurrection host closed the connection before replying",
        ));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated resurrection response",
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_does_not_enable_disabled_or_unadvertised_clients() {
        let mut config = SessionResurrectionConfig::default();
        assert_eq!(endpoint(&config, None, false).unwrap(), None);
        assert_eq!(
            endpoint(&config, Some("v2:/tmp/host"), false).unwrap(),
            None
        );
        assert_eq!(endpoint(&config, Some("v1:/tmp/host"), true).unwrap(), None);
        config.mode = SessionResurrectionMode::Disabled;
        assert_eq!(
            endpoint(&config, Some("v1:/tmp/host"), false).unwrap(),
            None
        );
        config.mode = SessionResurrectionMode::Enabled;
        config.endpoint = Some("/configured/host".into());
        assert_eq!(
            endpoint(&config, None, false).unwrap(),
            Some(PathBuf::from("/configured/host"))
        );
        config.endpoint = Some("relative".into());
        assert!(endpoint(&config, None, false).is_err());
    }

    #[test]
    fn frames_reject_truncation_size_and_unknown_fields() {
        assert!(read_frame::<Welcome>(&mut &b"{}"[..]).is_err());
        assert!(read_frame::<Welcome>(&mut &vec![b'x'; MAX_FRAME][..]).is_err());
        assert!(
            read_frame::<Welcome>(&mut &b"{\"protocol\":\"x\",\"restore\":null,\"extra\":1}\n"[..])
                .is_err()
        );
        let ack: Acknowledgement = read_frame(&mut &b"{\"revision\":2}\n"[..]).unwrap();
        assert_eq!(ack.revision, 2);
    }

    #[test]
    fn frames_distinguish_closed_truncated_oversized_and_malformed_responses() {
        for (bytes, kind, message) in [
            (
                Vec::new(),
                std::io::ErrorKind::UnexpectedEof,
                "closed the connection",
            ),
            (
                b"{\"revision\":1}".to_vec(),
                std::io::ErrorKind::UnexpectedEof,
                "truncated",
            ),
            (
                vec![b'x'; MAX_FRAME],
                std::io::ErrorKind::InvalidData,
                "exceeds protocol limit",
            ),
        ] {
            let error = read_frame::<Acknowledgement>(&mut bytes.as_slice())
                .err()
                .unwrap();
            assert_eq!(error.kind(), kind);
            assert!(error.to_string().contains(message));
        }
        assert!(read_frame::<Acknowledgement>(&mut &b"invalid json\n"[..]).is_err());
        let mut fragmented = std::io::BufReader::with_capacity(2, &b"{\"revision\":3}\n"[..]);
        assert_eq!(
            read_frame::<Acknowledgement>(&mut fragmented)
                .unwrap()
                .revision,
            3
        );
        let mut valid = b"{\"revision\":4}".to_vec();
        valid.resize(MAX_FRAME - 2, b' ');
        valid.push(b'\n');
        assert_eq!(
            read_frame::<Acknowledgement>(&mut valid.as_slice())
                .unwrap()
                .revision,
            4
        );
        valid.insert(0, b' ');
        assert!(read_frame::<Acknowledgement>(&mut valid.as_slice()).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_and_closed_handshakes_fail_with_explicit_recovery_guidance() {
        for mode in [
            SessionResurrectionMode::Auto,
            SessionResurrectionMode::Enabled,
        ] {
            for (response, expected) in [
                (None, "closed the connection before replying"),
                (
                    Some(
                        serde_json::json!({"protocol": PROTOCOL, "error": {"code": "ownership_verification_unavailable"}}),
                    ),
                    "could not verify foreground process ownership",
                ),
                (
                    Some(
                        serde_json::json!({"protocol": PROTOCOL, "error": {"code": "ownership_rejected"}}),
                    ),
                    "does not own the current execution",
                ),
                (
                    Some(
                        serde_json::json!({"protocol": PROTOCOL, "error": {"code": "unknown\u{001b}[31m"}}),
                    ),
                    "unknown error code",
                ),
                (
                    Some(
                        serde_json::json!({"protocol": "future", "error": {"code": "ownership_rejected"}}),
                    ),
                    "unsupported resurrection host protocol",
                ),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let socket = directory.path().join("host");
                let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                let host = std::thread::spawn(move || {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = std::io::BufReader::new(stream);
                    let _: serde_json::Value = read_frame(&mut reader).unwrap();
                    if let Some(response) = response {
                        write_frame(reader.get_mut(), &response).unwrap();
                    }
                });
                let error = connect_endpoint(socket, None, directory.path().to_owned(), mode)
                    .await
                    .err()
                    .unwrap();
                let message = error.to_string();
                assert!(message.contains("during the host handshake"));
                assert!(message.contains(expected), "{message}");
                assert!(message.contains("BCODE_NO_SESSION_RESURRECTION=1"));
                assert!(!message.contains('\u{001b}'));
                host.join().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn publication_tracks_selection_but_not_inflight_opens() {
        use crate::session_flow::ChatSessionAttachment;
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        let mut publisher = Publisher {
            sender,
            last: None,
            published: false,
        };
        let first = SessionId::new();
        let second = SessionId::new();
        publisher.observe(ChatSessionAttachment::Attached { session_id: first });
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(first));
        publisher.observe(ChatSessionAttachment::Opening {
            session_id: second,
            anchor_sequence: None,
        });
        assert!(!receiver.has_changed().unwrap());
        publisher.observe(ChatSessionAttachment::Detached { session_id: first });
        assert!(!receiver.has_changed().unwrap());
        publisher.observe(ChatSessionAttachment::Attached { session_id: second });
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(second));
        publisher.observe(ChatSessionAttachment::Draft);
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn independent_host_restores_only_when_offered_and_respects_explicit_selection() {
        for (offer, explicit) in [(false, false), (true, false), (true, true)] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("host.sock");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let authority = dir.path().canonicalize().unwrap();
            let restored = SessionId::new();
            let explicit_id = explicit.then(SessionId::new);
            let host_authority = authority.clone();
            let host = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut stream = std::io::BufReader::new(stream);
                let hello: serde_json::Value = read_frame(&mut stream).unwrap();
                assert_eq!(hello["protocol"], PROTOCOL);
                assert_eq!(hello["accept_restore"], !explicit);
                let descriptor = offer.then(|| Descriptor {
                    application: "bcode".into(),
                    version: 1,
                    session_store: host_authority,
                    session_id: restored,
                });
                write_frame(
                    stream.get_mut(),
                    &serde_json::json!({"protocol": PROTOCOL, "restore": descriptor}),
                )
                .unwrap();
                let selection: serde_json::Value = read_frame(&mut stream).unwrap();
                assert_eq!(selection["revision"], 1);
                assert_eq!(selection["operation"], "selection");
                write_frame(stream.get_mut(), &serde_json::json!({"revision": 1})).unwrap();
            });
            let (selected, publisher, warning) = connect_endpoint(
                socket,
                explicit_id,
                authority,
                SessionResurrectionMode::Auto,
            )
            .await
            .unwrap();
            assert!(warning.is_none());
            assert_eq!(selected, explicit_id.or_else(|| offer.then_some(restored)));
            let mut publisher = publisher.unwrap();
            publisher.observe(selected.map_or(
                crate::session_flow::ChatSessionAttachment::Draft,
                |session_id| crate::session_flow::ChatSessionAttachment::Attached { session_id },
            ));
            tokio::task::spawn_blocking(move || host.join().unwrap())
                .await
                .unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unavailable_hosts_are_optional_only_in_auto_mode() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("host");
        let missing_store = root.path().join("missing-store");
        let explicit = Some(SessionId::new());
        for refused in [false, true] {
            if refused {
                drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
            }
            let (selected, publisher, warning) = connect_endpoint(
                socket.clone(),
                explicit,
                missing_store.clone(),
                SessionResurrectionMode::Auto,
            )
            .await
            .unwrap();
            assert_eq!(selected, explicit);
            assert!(publisher.is_none());
            assert!(warning.is_some());
            assert!(
                connect_endpoint(
                    socket.clone(),
                    explicit,
                    missing_store.clone(),
                    SessionResurrectionMode::Enabled
                )
                .await
                .is_err()
            );
            assert!(!missing_store.exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handshake_and_restore_failures_are_not_optional() {
        for (protocol, offer, explicit, succeeds) in [
            (PROTOCOL, false, false, true),
            (PROTOCOL, true, false, false),
            ("future", false, false, false),
            (PROTOCOL, true, true, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("host");
            let authority = root.path().join("missing-store");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let host_authority = authority.clone();
            let host = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut stream = std::io::BufReader::new(stream);
                let _: serde_json::Value = read_frame(&mut stream).unwrap();
                let descriptor = offer.then(|| Descriptor {
                    application: "bcode".into(),
                    version: 1,
                    session_store: host_authority,
                    session_id: SessionId::new(),
                });
                write_frame(
                    stream.get_mut(),
                    &serde_json::json!({"protocol": protocol, "restore": descriptor}),
                )
                .unwrap();
            });
            let explicit_id = explicit.then(SessionId::new);
            let result = connect_endpoint(
                socket,
                explicit_id,
                authority.clone(),
                SessionResurrectionMode::Auto,
            )
            .await;
            assert_eq!(result.is_ok(), succeeds);
            if let Ok((selected, _, warning)) = result {
                assert_eq!(selected, explicit_id);
                assert!(warning.is_none());
            }
            assert!(!authority.exists());
            host.join().unwrap();
        }
    }

    #[test]
    fn descriptors_cannot_redirect_authority() {
        let descriptor = Descriptor {
            application: "bcode".into(),
            version: 1,
            session_store: PathBuf::from("/one"),
            session_id: SessionId::new(),
        };
        assert_eq!(
            descriptor.session(std::path::Path::new("/one")).unwrap(),
            descriptor.session_id
        );
        assert!(descriptor.session(std::path::Path::new("/two")).is_err());
    }
}
