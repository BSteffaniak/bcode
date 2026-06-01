#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Client/server IPC protocol for bcode.

use bcode_agent_profile::{AgentInfo, PolicyStatusResponse};
use bcode_metrics::MetricsSnapshot;
use bcode_session_models::{
    ClientId, ProjectionWindowRequest, RuntimeWorkId, RuntimeWorkKind, RuntimeWorkStatus,
    SessionEvent, SessionHistoryPage, SessionHistoryQuery, SessionId, SessionInputHistoryEntry,
    SessionSummary,
};
use bcode_skill_models::{SkillContextResponse, SkillId, SkillList, SkillManifest};
pub use bcode_worktree_models::{
    WorktreeCreateRequest, WorktreeCreateResponse, WorktreeListRequest, WorktreeListResponse,
    WorktreeRemoveRequest, WorktreeRemoveResponse,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub use bmux_ipc::IpcEndpoint;
pub use bmux_ipc::transport::{IpcTransportError, LocalIpcStream};

/// Local IPC listener that avoids unlinking live Unix socket endpoints.
#[derive(Debug)]
pub struct LocalIpcListener {
    inner: bmux_ipc::transport::LocalIpcListener,
}

impl LocalIpcListener {
    /// Bind a local listener for the provided endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the endpoint is unsupported on this platform, the
    /// endpoint appears to already have a live listener, stale endpoint cleanup
    /// fails, or the listener cannot be created.
    pub fn bind(endpoint: &IpcEndpoint) -> Result<Self, IpcTransportError> {
        prepare_endpoint_for_bind(endpoint)?;
        Ok(Self {
            inner: bmux_ipc::transport::LocalIpcListener::bind(endpoint)?,
        })
    }

    /// Accept an incoming local connection.
    ///
    /// # Errors
    ///
    /// Returns an error when accepting fails.
    pub async fn accept(&self) -> Result<LocalIpcStream, IpcTransportError> {
        self.inner.accept().await
    }
}

const FRAME_LEN_BYTES: usize = 4;

/// Maximum accepted encoded envelope payload size.
pub const MAX_FRAME_PAYLOAD_SIZE: usize = 1_048_576;

const MAX_CHUNK_DATA_SIZE: usize = MAX_FRAME_PAYLOAD_SIZE / 2;

/// Current Bcode IPC protocol version.
pub const CURRENT_PROTOCOL_VERSION: u16 = 2;

/// Build-scoped daemon fingerprint generated at compile time.
pub const BUILD_FINGERPRINT: &str = env!("BCODE_BUILD_FINGERPRINT");

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
    /// Internal continuation frame for logical envelopes that exceed one IPC frame.
    Chunk,
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
        #[serde(default)]
        runtime_context: Option<ClientRuntimeContext>,
        #[serde(default)]
        daemon_namespace: String,
    },
    Ping,
    ServerStatus,
    ServerStop {
        #[serde(default)]
        mode: ServerStopMode,
    },
    CreateSession {
        name: Option<String>,
        working_directory: PathBuf,
    },
    ListSessions {
        working_directory: PathBuf,
    },
    RenameSession {
        session_id: SessionId,
        name: Option<String>,
    },
    DeleteSession {
        session_id: SessionId,
    },
    SessionHistory {
        session_id: SessionId,
    },
    SessionHistoryPage {
        session_id: SessionId,
        query: SessionHistoryQuery,
    },
    AttachSession {
        session_id: SessionId,
    },
    AttachSessionRecent {
        session_id: SessionId,
        limit: usize,
    },
    SendUserMessage {
        session_id: SessionId,
        text: String,
    },
    InvokeSkill {
        session_id: SessionId,
        skill_id: SkillId,
        arguments: String,
        display_text: String,
    },
    CancelSessionTurn {
        session_id: SessionId,
        #[serde(default)]
        clear_queue: bool,
    },
    CancelRuntimeWork {
        session_id: SessionId,
        work_id: RuntimeWorkId,
    },
    CompactSession {
        session_id: SessionId,
    },
    SetSessionModel {
        session_id: SessionId,
        provider_plugin_id: Option<String>,
        model_id: String,
    },
    SetSessionReasoning {
        session_id: SessionId,
        effort: Option<String>,
        summary: Option<String>,
    },
    SessionModelStatus {
        session_id: SessionId,
    },
    SessionModelList {
        provider_plugin_id: Option<String>,
    },
    ListAgents,
    ListSkills,
    DescribeSkill {
        skill_id: SkillId,
    },
    ActivateSkill {
        session_id: SessionId,
        skill_id: SkillId,
    },
    DeactivateSkill {
        session_id: SessionId,
        skill_id: SkillId,
    },
    ActiveSkills {
        session_id: SessionId,
    },
    AgentPolicyStatus,
    SetSessionAgent {
        session_id: SessionId,
        agent_id: String,
    },
    ListPermissions,
    ResolvePermission {
        permission_id: String,
        approved: bool,
    },
    AddPermissionRule {
        agent_id: String,
        category: String,
        pattern: String,
        action: String,
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
    UpdateClientRuntimeContext {
        #[serde(default)]
        runtime_context: Option<ClientRuntimeContext>,
    },
    ChangeSessionWorkingDirectory {
        session_id: SessionId,
        working_directory: PathBuf,
    },
    ListWorktrees(WorktreeListRequest),
    CreateWorktree(WorktreeCreateRequest),
    RemoveWorktree(WorktreeRemoveRequest),
    ImportExternalSession {
        source_id: String,
        external_session_id: String,
    },
    RefreshSessionCatalog {
        #[serde(default)]
        working_directory: Option<PathBuf>,
        #[serde(default)]
        sources: Option<Vec<String>>,
    },
    ListRuntimeWork {
        session_id: SessionId,
    },
    RuntimeWorkHistory {
        session_id: SessionId,
        limit: usize,
    },
    SubscribeRuntimeWork {
        session_id: SessionId,
    },
    SubscribeCatalogUpdates,
    AttachSessionProjectionWindow {
        session_id: SessionId,
        request: ProjectionWindowRequest,
    },
}

/// Server stop request policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerStopMode {
    /// Stop regardless of connected clients or active work.
    #[default]
    Force,
    /// Stop only when the daemon is idle.
    IfIdle,
}

/// Per-client model/provider/auth context supplied at connection time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRuntimeContext {
    #[serde(default)]
    pub selected_provider_plugin_id: Option<String>,
    #[serde(default)]
    pub selected_model_id: Option<String>,
    #[serde(default)]
    pub provider_context: bcode_model::ProviderRequestContext,
    /// Redacted names of transient environment variables included in `provider_context.env`.
    #[serde(default)]
    pub env_keys: BTreeMap<String, bool>,
}

/// Persistent session catalog discovery status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionCatalogStatus {
    #[default]
    NotStarted,
    Loading,
    Loaded,
    Degraded(String),
    Failed(String),
}

/// Per-source session catalog discovery status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCatalogSourceStatus {
    pub source_id: String,
    pub display_name: String,
    pub status: SessionCatalogStatus,
    #[serde(default)]
    pub updated_at_ms: u64,
}

/// Local server status summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub connected_client_count: usize,
    pub sessions: Vec<SessionSummary>,
    #[serde(default)]
    pub session_catalog_loaded: bool,
    #[serde(default)]
    pub session_catalog_status: SessionCatalogStatus,
    #[serde(default)]
    pub session_catalog_sources: Vec<SessionCatalogSourceStatus>,
    #[serde(default)]
    pub session_catalog_revision: u64,
    #[serde(default)]
    pub selected_provider_plugin_id: Option<String>,
    #[serde(default)]
    pub selected_model_id: Option<String>,
    #[serde(default)]
    pub plugin_runtime: Vec<bcode_plugin::PluginExecutorStatus>,
    /// Server process identity and lifecycle metadata.
    #[serde(default)]
    pub daemon: DaemonStatus,
    /// Lightweight runtime metrics snapshot.
    #[serde(default)]
    pub metrics: MetricsSnapshot,
    /// Rich dashboard-ready metrics report.
    #[serde(default = "default_metrics_report_box")]
    pub metrics_report: Box<bcode_metrics::MetricsReport>,
}

fn default_metrics_report_box() -> Box<bcode_metrics::MetricsReport> {
    Box::default()
}

/// Server process identity and lifecycle metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DaemonStatus {
    /// Daemon namespace.
    #[serde(default)]
    pub namespace: String,
    /// IPC protocol version.
    #[serde(default)]
    pub protocol_version: u32,
    /// Build fingerprint included in the namespace.
    #[serde(default)]
    pub build_fingerprint: String,
    /// Process identifier, when available.
    #[serde(default)]
    pub pid: Option<u32>,
    /// Random per-process identity token.
    #[serde(default)]
    pub instance_id: String,
    /// Daemon start time in Unix milliseconds.
    #[serde(default)]
    pub started_at_unix_ms: u64,
}

/// Active model metadata for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModelStatus {
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning: Option<bcode_model::ModelReasoningInfo>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_summary: Option<String>,
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
    pub agent_id: String,
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

/// Warning reported after importing an external session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImportWarning {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeWorkSnapshot {
    pub work_id: RuntimeWorkId,
    pub kind: RuntimeWorkKind,
    pub label: String,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    pub status: RuntimeWorkStatus,
    pub cancellable: bool,
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
        #[serde(default)]
        catalog_status: SessionCatalogStatus,
        #[serde(default)]
        catalog_sources: Vec<SessionCatalogSourceStatus>,
        #[serde(default)]
        catalog_revision: u64,
    },
    SessionRenamed {
        session: SessionSummary,
    },
    SessionDeleted {
        session: SessionSummary,
    },
    SessionHistory {
        session_id: SessionId,
        history: Vec<SessionEvent>,
    },
    SessionHistoryPage {
        page: SessionHistoryPage,
    },
    Attached {
        session_id: SessionId,
        session: SessionSummary,
        history: Vec<SessionEvent>,
        #[serde(default)]
        input_history: Vec<SessionInputHistoryEntry>,
        #[serde(default)]
        import_warnings: Vec<SessionImportWarning>,
    },
    MessageSent,
    TurnCancellationRequested {
        cancelled: bool,
    },
    SessionCompacted {
        compacted: bool,
        message: String,
    },
    SessionModelSet,
    SessionModelStatus {
        status: SessionModelStatus,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    SkillList {
        skills: Box<SkillList>,
    },
    SkillManifest {
        skill: Box<SkillManifest>,
    },
    ActiveSkills {
        skills: Vec<SkillContextResponse>,
    },
    AgentPolicyStatus {
        status: PolicyStatusResponse,
    },
    SessionAgentSet,
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
    MessageAccepted {
        queued: bool,
        queue_position: Option<u32>,
    },
    SessionModelList {
        provider_plugin_id: Option<String>,
        models: bcode_model::ModelList,
    },
    ClientRuntimeContextUpdated,
    SessionWorkingDirectoryChanged {
        session: SessionSummary,
        changed: bool,
    },
    WorktreeList(WorktreeListResponse),
    WorktreeCreated(WorktreeCreateResponse),
    WorktreeRemoved(WorktreeRemoveResponse),
    ExternalSessionImported {
        session: SessionSummary,
        warnings: Vec<SessionImportWarning>,
    },
    SessionCatalogRefreshed {
        sessions: Vec<SessionSummary>,
        #[serde(default)]
        catalog_status: SessionCatalogStatus,
        #[serde(default)]
        catalog_sources: Vec<SessionCatalogSourceStatus>,
        #[serde(default)]
        catalog_revision: u64,
    },
    CatalogUpdatesSubscribed,
    RuntimeWorkList {
        work: Vec<RuntimeWorkSnapshot>,
    },
    RuntimeWorkCancellationRequested {
        cancelled: bool,
    },
    RuntimeWorkHistory {
        events: Vec<SessionEvent>,
    },
    RuntimeWorkSubscribed,
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
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(ResponsePayload),
    Err(ErrorResponse),
}

/// Server-to-client event payloads.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Session(SessionEvent),
    RuntimeWork(SessionEvent),
    SessionCatalogUpdated {
        #[serde(default)]
        revision: u64,
    },
}

/// Errors returned by Bcode IPC encoding/decoding.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("frame payload exceeds max size ({actual} bytes > {max} bytes)")]
    PayloadTooLarge { actual: usize, max: usize },
    #[error("invalid IPC chunk: {0}")]
    InvalidChunk(String),
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
    bmux_codec::to_positional_vec(value).map_err(CodecError::Serialize)
}

/// Decode a deserializable value with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    bmux_codec::from_positional_bytes(bytes).map_err(CodecError::Deserialize)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChunkPayload {
    chunk_index: u32,
    chunk_count: u32,
    total_len: u64,
    data: Vec<u8>,
}

/// Send one logical envelope.
///
/// Logical envelopes larger than [`MAX_FRAME_PAYLOAD_SIZE`] are transparently
/// fragmented into multiple physical IPC frames and reassembled by
/// [`recv_envelope`].
///
/// # Errors
///
/// Returns an error when serialization or writing fails.
pub async fn send_envelope<W>(writer: &mut W, envelope: &Envelope) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
{
    let payload = encode(envelope)?;
    if payload.len() <= MAX_FRAME_PAYLOAD_SIZE {
        return write_envelope_frame(writer, envelope).await;
    }
    send_chunked_envelope(writer, envelope.request_id, &payload).await
}

async fn send_chunked_envelope<W>(
    writer: &mut W,
    request_id: u64,
    payload: &[u8],
) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
{
    let chunk_count = payload.len().div_ceil(MAX_CHUNK_DATA_SIZE);
    let chunk_count = u32::try_from(chunk_count).map_err(|_| CodecError::PayloadTooLarge {
        actual: payload.len(),
        max: MAX_FRAME_PAYLOAD_SIZE,
    })?;
    let total_len = u64::try_from(payload.len()).map_err(|_| CodecError::PayloadTooLarge {
        actual: payload.len(),
        max: MAX_FRAME_PAYLOAD_SIZE,
    })?;

    for (chunk_index, data) in payload.chunks(MAX_CHUNK_DATA_SIZE).enumerate() {
        let chunk_payload = ChunkPayload {
            chunk_index: u32::try_from(chunk_index).map_err(|_| CodecError::PayloadTooLarge {
                actual: payload.len(),
                max: MAX_FRAME_PAYLOAD_SIZE,
            })?,
            chunk_count,
            total_len,
            data: data.to_vec(),
        };
        let chunk_envelope =
            Envelope::new(request_id, EnvelopeKind::Chunk, encode(&chunk_payload)?);
        write_envelope_frame(writer, &chunk_envelope).await?;
    }
    Ok(())
}

async fn write_envelope_frame<W>(writer: &mut W, envelope: &Envelope) -> Result<(), CodecError>
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

/// Receive one logical envelope.
///
/// If the sender fragmented a large logical envelope into continuation frames,
/// this function reassembles those frames before returning.
///
/// # Errors
///
/// Returns an error when reading, reassembly, or deserialization fails.
pub async fn recv_envelope<R>(reader: &mut R) -> Result<Envelope, CodecError>
where
    R: AsyncRead + Unpin,
{
    let envelope = read_envelope_frame(reader).await?;
    if envelope.kind == EnvelopeKind::Chunk {
        recv_chunked_envelope(reader, envelope).await
    } else {
        Ok(envelope)
    }
}

async fn read_envelope_frame<R>(reader: &mut R) -> Result<Envelope, CodecError>
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

async fn recv_chunked_envelope<R>(
    reader: &mut R,
    first_envelope: Envelope,
) -> Result<Envelope, CodecError>
where
    R: AsyncRead + Unpin,
{
    let first = decode_chunk_payload(&first_envelope)?;
    validate_first_chunk(&first)?;

    let mut assembled = Vec::new();
    let chunk_count = first.chunk_count;
    let total_len = first.total_len;
    assembled.extend_from_slice(&first.data);

    for expected_index in 1..chunk_count {
        let envelope = read_envelope_frame(reader).await?;
        if envelope.kind != EnvelopeKind::Chunk {
            return Err(CodecError::InvalidChunk(format!(
                "expected chunk {expected_index}, got {:?}",
                envelope.kind
            )));
        }
        let chunk = decode_chunk_payload(&envelope)?;
        validate_next_chunk(&chunk, expected_index, chunk_count, total_len)?;
        assembled.extend_from_slice(&chunk.data);
    }

    let actual_len = u64::try_from(assembled.len()).map_err(|_| {
        CodecError::InvalidChunk("assembled payload length does not fit in u64".to_string())
    })?;
    if actual_len != total_len {
        return Err(CodecError::InvalidChunk(format!(
            "assembled payload length {actual_len} does not match expected {total_len}"
        )));
    }

    let envelope: Envelope = decode(&assembled)?;
    if envelope.kind == EnvelopeKind::Chunk {
        return Err(CodecError::InvalidChunk(
            "nested chunk envelope is not allowed".to_string(),
        ));
    }
    if envelope.version != ProtocolVersion::current() {
        return Err(CodecError::UnsupportedVersion {
            actual: envelope.version.0,
            expected: ProtocolVersion::current().0,
        });
    }
    Ok(envelope)
}

fn decode_chunk_payload(envelope: &Envelope) -> Result<ChunkPayload, CodecError> {
    decode(&envelope.payload)
}

fn validate_first_chunk(chunk: &ChunkPayload) -> Result<(), CodecError> {
    if chunk.chunk_count == 0 {
        return Err(CodecError::InvalidChunk(
            "chunk count must be greater than zero".to_string(),
        ));
    }
    if chunk.chunk_index != 0 {
        return Err(CodecError::InvalidChunk(format!(
            "first chunk index must be 0, got {}",
            chunk.chunk_index
        )));
    }
    validate_next_chunk(chunk, 0, chunk.chunk_count, chunk.total_len)
}

fn validate_next_chunk(
    chunk: &ChunkPayload,
    expected_index: u32,
    chunk_count: u32,
    total_len: u64,
) -> Result<(), CodecError> {
    if chunk.chunk_index != expected_index {
        return Err(CodecError::InvalidChunk(format!(
            "expected chunk index {expected_index}, got {}",
            chunk.chunk_index
        )));
    }
    if chunk.chunk_count != chunk_count {
        return Err(CodecError::InvalidChunk(format!(
            "chunk count changed from {chunk_count} to {}",
            chunk.chunk_count
        )));
    }
    if chunk.total_len != total_len {
        return Err(CodecError::InvalidChunk(format!(
            "total length changed from {total_len} to {}",
            chunk.total_len
        )));
    }
    if chunk.data.len() > MAX_CHUNK_DATA_SIZE {
        return Err(CodecError::InvalidChunk(format!(
            "chunk data exceeds max size ({} bytes > {MAX_CHUNK_DATA_SIZE} bytes)",
            chunk.data.len()
        )));
    }
    Ok(())
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

/// Return the normalized current working directory for session scoping.
#[must_use]
pub fn current_working_directory() -> PathBuf {
    env::current_dir().map_or_else(|_| PathBuf::from("."), |path| normalize_path(&path))
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn prepare_endpoint_for_bind(endpoint: &IpcEndpoint) -> Result<(), IpcTransportError> {
    #[cfg(unix)]
    if let Some(path) = unix_socket_path(endpoint) {
        prepare_unix_socket_path_for_bind(&path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_unix_socket_path_for_bind(path: &Path) -> Result<(), IpcTransportError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_stream) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!(
                "refusing to replace live IPC socket {}; another bcode daemon is listening",
                path.display()
            ),
        )
        .into()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn unix_socket_path(endpoint: &IpcEndpoint) -> Option<PathBuf> {
    let debug = format!("{endpoint:?}");
    let marker = "UnixSocket(";
    let start = debug.find(marker)? + marker.len();
    let rest = &debug[start..];
    let end = rest.rfind(')')?;
    let path = rest[..end].trim().trim_matches('"');
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Return the daemon namespace for this build and IPC protocol version.
#[must_use]
pub fn daemon_namespace() -> String {
    format!("ipc-v{CURRENT_PROTOCOL_VERSION}-{BUILD_FINGERPRINT}")
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
        IpcEndpoint::windows_named_pipe(format!(r"\\.\pipe\bcode-{user}-{}", daemon_namespace()))
    }
}

#[cfg(unix)]
fn default_socket_path() -> PathBuf {
    if let Ok(path) = env::var("BCODE_SOCKET") {
        return PathBuf::from(path);
    }
    let user = env::var("USER").unwrap_or_else(|_| "user".to_string());
    env::temp_dir().join(format!("bcode-{user}-{}.sock", daemon_namespace()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEventKind, SessionId, SessionSummary,
    };

    #[test]
    fn ipc_v1_golden_fixtures_decode_to_expected_payloads() {
        let message_sent = fixture_bytes("fixtures/ipc/v1/response_message_sent.hex");
        let decoded: Response = decode(&message_sent).expect("message_sent fixture should decode");
        assert_eq!(decoded, Response::Ok(ResponsePayload::MessageSent));

        let cancelled = fixture_bytes("fixtures/ipc/v1/response_turn_cancellation_requested.hex");
        let decoded: Response = decode(&cancelled).expect("cancel fixture should decode");
        assert_eq!(
            decoded,
            Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled: true })
        );

        let accepted = fixture_bytes("fixtures/ipc/v1/response_message_accepted.hex");
        let decoded: Response = decode(&accepted).expect("message_accepted fixture should decode");
        assert_eq!(
            decoded,
            Response::Ok(ResponsePayload::MessageAccepted {
                queued: true,
                queue_position: Some(2),
            })
        );

        let request = fixture_bytes("fixtures/ipc/v1/request_send_user_message.hex");
        let decoded: Request = decode(&request).expect("send request fixture should decode");
        assert_eq!(
            decoded,
            Request::SendUserMessage {
                session_id: "00000000-0000-0000-0000-000000000001"
                    .parse()
                    .expect("fixture session id should parse"),
                text: "hello".to_string(),
            }
        );
    }

    #[test]
    fn ipc_v1_golden_fixtures_remain_byte_stable() {
        let cases = [
            (
                "fixtures/ipc/v1/response_message_sent.hex",
                encode(&Response::Ok(ResponsePayload::MessageSent))
                    .expect("response should encode"),
            ),
            (
                "fixtures/ipc/v1/response_turn_cancellation_requested.hex",
                encode(&Response::Ok(ResponsePayload::TurnCancellationRequested {
                    cancelled: true,
                }))
                .expect("response should encode"),
            ),
            (
                "fixtures/ipc/v1/response_message_accepted.hex",
                encode(&Response::Ok(ResponsePayload::MessageAccepted {
                    queued: true,
                    queue_position: Some(2),
                }))
                .expect("response should encode"),
            ),
            (
                "fixtures/ipc/v1/request_send_user_message.hex",
                encode(&Request::SendUserMessage {
                    session_id: "00000000-0000-0000-0000-000000000001"
                        .parse()
                        .expect("fixture session id should parse"),
                    text: "hello".to_string(),
                })
                .expect("request should encode"),
            ),
        ];
        for (path, encoded) in cases {
            assert_eq!(encoded, fixture_bytes(path), "fixture changed: {path}");
        }
    }

    #[test]
    fn attached_response_carries_canonical_session_summary() {
        let session_id: SessionId = "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("session id should parse");
        let summary = SessionSummary {
            id: session_id,
            name: Some("Canonical title".to_owned()),
            explicit_name: Some("Canonical title".to_owned()),
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::Explicit,
            client_count: 1,
            created_at_ms: 10,
            updated_at_ms: 20,
            working_directory: "/tmp/bcode-ipc-test".into(),
            import: None,
        };
        let response = Response::Ok(ResponsePayload::Attached {
            session_id,
            session: summary.clone(),
            history: Vec::new(),
            input_history: Vec::new(),
            import_warnings: Vec::new(),
        });

        let encoded = encode(&response).expect("response should encode");
        let decoded: Response = decode(&encoded).expect("response should decode");

        assert_eq!(decoded, response);
        let Response::Ok(ResponsePayload::Attached { session, .. }) = decoded else {
            panic!("decoded response should be attached");
        };
        assert_eq!(session, summary);
    }

    #[test]
    fn response_envelope_uses_current_protocol_version() {
        let envelope = response_envelope(7, &Response::Ok(ResponsePayload::MessageSent))
            .expect("response envelope should encode");

        assert_eq!(envelope.version, ProtocolVersion::current());
        assert_eq!(ProtocolVersion::current().0, 2);
    }

    #[tokio::test]
    async fn unsupported_protocol_version_is_rejected() {
        let envelope = Envelope {
            version: ProtocolVersion(1),
            request_id: 1,
            kind: EnvelopeKind::Response,
            payload: encode(&Response::Ok(ResponsePayload::MessageSent))
                .expect("response should encode"),
        };
        let encoded = encode(&envelope).expect("envelope should encode");
        let mut frame = Vec::new();
        frame.extend_from_slice(
            &u32::try_from(encoded.len())
                .expect("encoded envelope should fit u32")
                .to_le_bytes(),
        );
        frame.extend_from_slice(&encoded);
        let mut cursor = std::io::Cursor::new(frame);

        let error = read_envelope_frame(&mut cursor)
            .await
            .expect_err("old protocol version should fail");

        assert!(matches!(
            error,
            CodecError::UnsupportedVersion {
                actual: 1,
                expected: 2
            }
        ));
    }

    fn fixture_bytes(path: &str) -> Vec<u8> {
        let hex = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(path),
        )
        .expect("fixture should be readable");
        decode_hex(hex.trim()).expect("fixture should contain hex")
    }

    fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
        if !hex.len().is_multiple_of(2) {
            return Err("hex fixture has odd length".to_string());
        }
        (0..hex.len())
            .step_by(2)
            .map(|index| {
                u8::from_str_radix(&hex[index..index + 2], 16).map_err(|error| error.to_string())
            })
            .collect()
    }

    #[test]
    fn runtime_context_with_semantic_auth_round_trips() {
        let request = Request::Hello {
            client_name: "test".to_string(),
            daemon_namespace: daemon_namespace(),
            runtime_context: Some(ClientRuntimeContext {
                selected_provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                selected_model_id: Some("model".to_string()),
                provider_context: bcode_model::ProviderRequestContext {
                    auth_profile: Some("openrouter".to_string()),
                    auth: Some(bcode_model::ProviderAuthContext {
                        profile: Some("openrouter".to_string()),
                        backend: Some("sshenv".to_string()),
                        scheme: Some("api_key".to_string()),
                        credentials: BTreeMap::from([(
                            "api_key".to_string(),
                            bcode_model::ProviderAuthCredential {
                                value: "secret".to_string(),
                                source: Some("OPENROUTER_API_KEY".to_string()),
                            },
                        )]),
                        attributes: BTreeMap::from([(
                            "base_url".to_string(),
                            "https://openrouter.ai/api/v1".to_string(),
                        )]),
                        storage: BTreeMap::from([(
                            "api_key".to_string(),
                            bcode_model::ProviderAuthStorageRef {
                                backend: "sshenv".to_string(),
                                profile: "openrouter".to_string(),
                                key: "OPENROUTER_API_KEY".to_string(),
                                vault: Some("/tmp/vault".to_string()),
                            },
                        )]),
                    }),
                    ..bcode_model::ProviderRequestContext::default()
                },
                env_keys: BTreeMap::from([("OPENROUTER_API_KEY".to_string(), true)]),
            }),
        };

        let encoded = encode(&request).expect("request should encode");
        let decoded: Request = decode(&encoded).expect("request should decode");

        assert_eq!(decoded, request);
    }

    #[tokio::test]
    async fn oversized_response_envelope_round_trips_across_chunked_frames() {
        let payload = vec![b'x'; MAX_FRAME_PAYLOAD_SIZE + 100_000];
        let response = Response::Ok(ResponsePayload::PluginServiceResult {
            response: PluginServiceResponse {
                payload,
                error: None,
            },
        });
        let envelope = response_envelope(42, &response).expect("response should encode");
        assert!(encode(&envelope).expect("envelope should encode").len() > MAX_FRAME_PAYLOAD_SIZE);

        let received = round_trip_envelope(envelope.clone()).await;

        assert_eq!(received, envelope);
        let decoded = decode::<Response>(&received.payload).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[tokio::test]
    async fn oversized_event_envelope_round_trips_across_chunked_frames() {
        let session_id = SessionId::new();
        let event = Event::Session(SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 7,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_string(),
                result: "z".repeat(MAX_FRAME_PAYLOAD_SIZE + 100_000),
                is_error: false,
                output: None,
            },
        });
        let envelope = event_envelope(&event).expect("event should encode");
        assert!(encode(&envelope).expect("envelope should encode").len() > MAX_FRAME_PAYLOAD_SIZE);

        let received = round_trip_envelope(envelope.clone()).await;

        assert_eq!(received, envelope);
        let decoded = decode::<Event>(&received.payload).expect("event should decode");
        assert_eq!(decoded, event);
    }

    async fn round_trip_envelope(envelope: Envelope) -> Envelope {
        let (mut sender, mut receiver) = tokio::io::duplex(64 * 1024);
        let send = send_envelope(&mut sender, &envelope);
        let receive = recv_envelope(&mut receiver);
        let (send_result, receive_result) = tokio::join!(send, receive);
        send_result.expect("send should succeed");
        receive_result.expect("receive should succeed")
    }
}
