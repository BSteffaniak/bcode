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
) -> std::io::Result<(Option<SessionId>, Option<Publisher>)> {
    let advertised = std::env::var("APPLICATION_RESURRECTION_HOST").ok();
    let disabled = std::env::var_os("BCODE_NO_SESSION_RESURRECTION").is_some();
    let Some(path) = endpoint(config, advertised.as_deref(), disabled)? else {
        return Ok((explicit_session, None));
    };
    connect_endpoint(
        path,
        explicit_session,
        bcode_config::default_session_store_dir(),
    )
    .await
}

async fn connect_endpoint(
    path: PathBuf,
    explicit_session: Option<SessionId>,
    authority: PathBuf,
) -> std::io::Result<(Option<SessionId>, Option<Publisher>)> {
    #[cfg(unix)]
    {
        let authority = authority.canonicalize()?;
        let (mut stream, restored, authority) = tokio::task::spawn_blocking(move || {
            let stream = std::os::unix::net::UnixStream::connect(path)?;
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
            let welcome: Welcome = read_frame(&mut stream)?;
            if welcome.protocol != PROTOCOL {
                return Err(std::io::Error::other(
                    "unsupported resurrection host protocol",
                ));
            }
            let restored = if explicit_session.is_none() {
                welcome
                    .restore
                    .as_ref()
                    .map(|value| value.session(&authority))
                    .transpose()?
            } else {
                None
            };
            Ok((stream, restored, authority))
        })
        .await
        .map_err(std::io::Error::other)??;
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            let mut revision = 0_u64;
            while receiver.changed().await.is_ok() {
                let selected = *receiver.borrow_and_update();
                let Some(next) = revision.checked_add(1) else {
                    break;
                };
                revision = next;
                let descriptor = selected.map(|session_id| Descriptor {
                    application: "bcode".to_owned(),
                    version: 1,
                    session_store: authority.clone(),
                    session_id,
                });
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
        Ok((
            explicit_session.or(restored),
            Some(Publisher {
                sender,
                last: None,
                published: false,
            }),
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

fn write_frame(writer: &mut impl std::io::Write, value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() >= MAX_FRAME {
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
    if bytes.last() != Some(&b'\n') || bytes.len() >= MAX_FRAME {
        return Err(std::io::Error::other(
            "incomplete or oversized resurrection frame",
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
            let (selected, publisher) = connect_endpoint(socket, explicit_id, authority)
                .await
                .unwrap();
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
