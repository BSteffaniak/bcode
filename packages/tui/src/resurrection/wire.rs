//! Application-resurrection wire protocol: newline-delimited JSON frames.
//!
//! Compiled only where a host transport exists; BMUX hosts resurrection over Unix
//! sockets.

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(super) const PROTOCOL: &str = "application-resurrection/1";
pub(super) const MAX_FRAME: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Descriptor {
    pub(super) application: String,
    pub(super) version: u32,
    pub(super) session_store: PathBuf,
    pub(super) session_id: SessionId,
}

impl Descriptor {
    pub(super) fn session(&self, authority: &std::path::Path) -> std::io::Result<SessionId> {
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
pub(super) struct Welcome {
    pub(super) protocol: String,
    pub(super) restore: Option<Descriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rejected {
    pub(super) protocol: String,
    pub(super) error: HostRejection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HostRejection {
    pub(super) code: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum HandshakeResponse {
    Welcome(Welcome),
    Rejected(Rejected),
}

pub(super) fn read_welcome(reader: &mut impl std::io::BufRead) -> std::io::Result<Welcome> {
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

pub(super) fn handshake_error(error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!(
            "Session restoration failed during the host handshake: {error}. To launch without restoration, set BCODE_NO_SESSION_RESURRECTION=1."
        ),
    )
}

#[derive(Serialize)]
pub(super) struct Hello {
    pub(super) protocol: &'static str,
    pub(super) application: &'static str,
    pub(super) pid: u32,
    pub(super) accept_restore: bool,
}

#[derive(Serialize)]
pub(super) struct Selection {
    pub(super) operation: &'static str,
    pub(super) revision: u64,
    pub(super) descriptor: Option<Descriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Acknowledgement {
    pub(super) revision: u64,
}

pub(super) fn write_frame(
    writer: &mut impl std::io::Write,
    value: &impl Serialize,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() + 1 >= MAX_FRAME {
        return Err(std::io::Error::other("resurrection frame exceeds limit"));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub(super) fn read_frame<T: serde::de::DeserializeOwned>(
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
