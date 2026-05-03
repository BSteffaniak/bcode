#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Client/server IPC protocol for bcode.

use bcode_session_models::{ClientId, SessionEvent, SessionId, SessionSummary};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::env;
use std::path::PathBuf;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub use bmux_ipc::IpcEndpoint;
pub use bmux_ipc::transport::{IpcTransportError, LocalIpcListener, LocalIpcStream};

const FRAME_LEN_BYTES: usize = 4;

/// Maximum accepted encoded envelope payload size.
pub const MAX_FRAME_PAYLOAD_SIZE: usize = 1_048_576;

/// Current Bcode IPC protocol version.
pub const CURRENT_PROTOCOL_VERSION: u16 = 1;

/// Protocol version used in IPC envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProtocolVersion(pub u16);

impl ProtocolVersion {
    /// The currently supported protocol version.
    #[must_use]
    pub const fn current() -> Self {
        Self(CURRENT_PROTOCOL_VERSION)
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::current()
    }
}

/// Envelope discriminant for payload interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    Request,
    Response,
    Event,
}

/// Versioned IPC envelope with request correlation support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub version: ProtocolVersion,
    pub request_id: u64,
    pub kind: EnvelopeKind,
    pub payload: Vec<u8>,
}

impl Envelope {
    /// Build a new envelope.
    #[must_use]
    pub const fn new(request_id: u64, kind: EnvelopeKind, payload: Vec<u8>) -> Self {
        Self {
            version: ProtocolVersion::current(),
            request_id,
            kind,
            payload,
        }
    }
}

/// Request payload variants for Bcode client/server IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Hello {
        client_name: String,
    },
    Ping,
    ServerStatus,
    ServerStop,
    CreateSession {
        name: Option<String>,
    },
    ListSessions,
    SessionHistory {
        session_id: SessionId,
    },
    AttachSession {
        session_id: SessionId,
    },
    SendUserMessage {
        session_id: SessionId,
        text: String,
    },
    CancelSessionTurn {
        session_id: SessionId,
    },
    ListPermissions,
    ResolvePermission {
        permission_id: String,
        approved: bool,
    },
    AddPermissionRule {
        kind: String,
        value: String,
    },
    ListPluginServices,
    InvokePluginService {
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    },
    CallPluginService {
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    },
    PublishPluginEvent {
        topic: String,
        payload: Vec<u8>,
    },
}

/// Local server status summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub connected_client_count: usize,
    pub sessions: Vec<SessionSummary>,
}

/// Service interface provided by a loaded plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginServiceSummary {
    pub plugin_id: String,
    pub interface_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

/// Pending permission checkpoint summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSummary {
    pub permission_id: String,
    pub session_id: SessionId,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments_json: String,
}

/// Plugin service invocation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginServiceResponse {
    pub payload: Vec<u8>,
    pub error: Option<PluginServiceError>,
}

/// Plugin service invocation error payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginServiceError {
    pub code: String,
    pub message: String,
}

/// Successful response payload variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayload {
    Hello {
        protocol_version: ProtocolVersion,
        client_id: ClientId,
    },
    Pong,
    ServerStatus {
        status: ServerStatus,
    },
    ServerStopping,
    SessionCreated {
        session: SessionSummary,
    },
    SessionList {
        sessions: Vec<SessionSummary>,
    },
    SessionHistory {
        session_id: SessionId,
        history: Vec<SessionEvent>,
    },
    Attached {
        session_id: SessionId,
        history: Vec<SessionEvent>,
    },
    MessageSent,
    TurnCancellationRequested {
        cancelled: bool,
    },
    PermissionList {
        permissions: Vec<PermissionSummary>,
    },
    PermissionResolved {
        resolved: bool,
    },
    PermissionRuleAdded {
        config_path: String,
    },
    PluginServices {
        services: Vec<PluginServiceSummary>,
    },
    PluginServiceResult {
        response: PluginServiceResponse,
    },
    PluginEventPublished {
        delivered: usize,
    },
}

/// Structured error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
}

impl ErrorResponse {
    /// Create a new error response.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Top-level response message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(ResponsePayload),
    Err(ErrorResponse),
}

/// Server-to-client event payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Session(SessionEvent),
}

/// Errors returned by Bcode IPC encoding/decoding.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("frame payload exceeds max size ({actual} bytes > {max} bytes)")]
    PayloadTooLarge { actual: usize, max: usize },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization failed: {0}")]
    Serialize(#[source] bmux_codec::Error),
    #[error("deserialization failed: {0}")]
    Deserialize(#[source] bmux_codec::Error),
    #[error("unsupported protocol version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u16, expected: u16 },
}

/// Encode a serializable value with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    bmux_codec::to_vec(value).map_err(CodecError::Serialize)
}

/// Decode a deserializable value with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    bmux_codec::from_bytes(bytes).map_err(CodecError::Deserialize)
}

/// Send one framed envelope.
///
/// # Errors
///
/// Returns an error when serialization or writing fails.
pub async fn send_envelope<W>(writer: &mut W, envelope: &Envelope) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
{
    let payload = encode(envelope)?;
    if payload.len() > MAX_FRAME_PAYLOAD_SIZE {
        return Err(CodecError::PayloadTooLarge {
            actual: payload.len(),
            max: MAX_FRAME_PAYLOAD_SIZE,
        });
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| CodecError::PayloadTooLarge {
        actual: payload.len(),
        max: MAX_FRAME_PAYLOAD_SIZE,
    })?;
    writer.write_all(&payload_len.to_le_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Receive one framed envelope.
///
/// # Errors
///
/// Returns an error when reading or deserialization fails.
pub async fn recv_envelope<R>(reader: &mut R) -> Result<Envelope, CodecError>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0_u8; FRAME_LEN_BYTES];
    reader.read_exact(&mut len_bytes).await?;
    let payload_len = u32::from_le_bytes(len_bytes) as usize;
    if payload_len > MAX_FRAME_PAYLOAD_SIZE {
        return Err(CodecError::PayloadTooLarge {
            actual: payload_len,
            max: MAX_FRAME_PAYLOAD_SIZE,
        });
    }
    let mut payload = vec![0_u8; payload_len];
    reader.read_exact(&mut payload).await?;
    let envelope: Envelope = decode(&payload)?;
    if envelope.version != ProtocolVersion::current() {
        return Err(CodecError::UnsupportedVersion {
            actual: envelope.version.0,
            expected: ProtocolVersion::current().0,
        });
    }
    Ok(envelope)
}

/// Build a request envelope.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn request_envelope(request_id: u64, request: &Request) -> Result<Envelope, CodecError> {
    Ok(Envelope::new(
        request_id,
        EnvelopeKind::Request,
        encode(request)?,
    ))
}

/// Build a response envelope.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn response_envelope(request_id: u64, response: &Response) -> Result<Envelope, CodecError> {
    Ok(Envelope::new(
        request_id,
        EnvelopeKind::Response,
        encode(response)?,
    ))
}

/// Build an event envelope.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn event_envelope(event: &Event) -> Result<Envelope, CodecError> {
    Ok(Envelope::new(0, EnvelopeKind::Event, encode(event)?))
}

/// Return the default local IPC endpoint.
#[must_use]
pub fn default_endpoint() -> IpcEndpoint {
    #[cfg(unix)]
    {
        IpcEndpoint::unix_socket(default_socket_path())
    }
    #[cfg(windows)]
    {
        let user = env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
        IpcEndpoint::windows_named_pipe(format!(r"\\.\pipe\bcode-{user}"))
    }
}

#[cfg(unix)]
fn default_socket_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_SOCKET") {
        return PathBuf::from(path);
    }
    let user = env::var("USER").unwrap_or_else(|_| "user".to_string());
    env::temp_dir().join(format!("bcode-{user}.sock"))
}
