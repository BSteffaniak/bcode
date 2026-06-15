#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Client/server IPC protocol for bcode.

use bcode_agent_profile::{AgentInfo, PolicyStatusResponse};
use bcode_metrics::MetricsSnapshot;
use bcode_session_models::{
    ClientId, FileChangeResult, ModelTurnOutcome, ProjectionWindowRequest, RuntimeWorkId,
    RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind, SessionForkKind,
    SessionHistoryPage, SessionHistoryQuery, SessionId, SessionInputHistoryEntry, SessionLiveEvent,
    SessionSummary, SessionTokenUsage, SessionTraceEvent, ShellRunResult,
    ToolInvocationPresentation, ToolInvocationResult, ToolInvocationStreamEvent, TraceBlobRef,
};
use bcode_skill_models::{
    SkillActivationMode, SkillContextResponse, SkillId, SkillList, SkillManifest, SkillSource,
};
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
        match bmux_ipc::transport::LocalIpcListener::bind(endpoint) {
            Ok(inner) => Ok(Self { inner }),
            Err(IpcTransportError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                prepare_endpoint_for_bind(endpoint)?;
                Ok(Self {
                    inner: bmux_ipc::transport::LocalIpcListener::bind(endpoint)?,
                })
            }
            Err(error) => Err(error),
        }
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

/// Placement behavior for submitted user prompts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptPlacement {
    /// Inject the prompt into the active conversation at the next safe model boundary.
    #[default]
    Steering,
    /// Queue the prompt to run as a follow-up turn after the active turn finishes.
    FollowUp,
}

impl PromptPlacement {
    /// Return whether this placement is the wire-compatible default.
    #[must_use]
    pub const fn is_steering(placement: &Self) -> bool {
        matches!(placement, Self::Steering)
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
    /// Explicit complete-history request for export/debug/history commands only.
    ///
    /// This request may force the server to read every canonical event for the session.
    /// Normal runtime flows must use `SessionHistoryPage`, projection-window requests, or
    /// typed read-model endpoints instead.
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
    SendUserMessageWithPlacement {
        session_id: SessionId,
        text: String,
        placement: PromptPlacement,
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
    ForkSession {
        source_session_id: SessionId,
        prompt_sequence: u64,
        name: Option<String>,
    },
    CloneSession {
        source_session_id: SessionId,
        name: Option<String>,
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
    SessionForked {
        session: SessionSummary,
        draft: Option<String>,
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
    SessionLive(SessionLiveEvent),
    RuntimeWork(SessionEvent),
    SessionCatalogUpdated {
        #[serde(default)]
        revision: u64,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IpcResponse {
    Ok(IpcResponsePayload),
    Err(ErrorResponse),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IpcResponsePayload {
    Domain(Box<ResponsePayload>),
    SessionHistory {
        session_id: SessionId,
        history: Vec<IpcSessionEvent>,
    },
    SessionHistoryPage {
        page: IpcSessionHistoryPage,
    },
    Attached {
        session_id: SessionId,
        session: SessionSummary,
        history: Vec<IpcSessionEvent>,
        input_history: Vec<SessionInputHistoryEntry>,
        import_warnings: Vec<SessionImportWarning>,
    },
    RuntimeWorkHistory {
        events: Vec<IpcSessionEvent>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IpcSessionHistoryPage {
    session_id: SessionId,
    events: Vec<IpcSessionEvent>,
    next_cursor: Option<bcode_session_models::SessionHistoryCursor>,
    has_more: bool,
}

impl From<&Response> for IpcResponse {
    fn from(value: &Response) -> Self {
        match value {
            Response::Ok(payload) => Self::Ok(IpcResponsePayload::from(payload)),
            Response::Err(error) => Self::Err(error.clone()),
        }
    }
}

impl TryFrom<IpcResponse> for Response {
    type Error = CodecError;

    fn try_from(value: IpcResponse) -> Result<Self, Self::Error> {
        match value {
            IpcResponse::Ok(payload) => payload.try_into().map(Self::Ok),
            IpcResponse::Err(error) => Ok(Self::Err(error)),
        }
    }
}

impl From<&ResponsePayload> for IpcResponsePayload {
    fn from(value: &ResponsePayload) -> Self {
        match value {
            ResponsePayload::SessionHistory {
                session_id,
                history,
            } => Self::SessionHistory {
                session_id: *session_id,
                history: history.iter().map(IpcSessionEvent::from).collect(),
            },
            ResponsePayload::SessionHistoryPage { page } => Self::SessionHistoryPage {
                page: IpcSessionHistoryPage::from(page),
            },
            ResponsePayload::Attached {
                session_id,
                session,
                history,
                input_history,
                import_warnings,
            } => Self::Attached {
                session_id: *session_id,
                session: session.clone(),
                history: history.iter().map(IpcSessionEvent::from).collect(),
                input_history: input_history.clone(),
                import_warnings: import_warnings.clone(),
            },
            ResponsePayload::RuntimeWorkHistory { events } => Self::RuntimeWorkHistory {
                events: events.iter().map(IpcSessionEvent::from).collect(),
            },
            _ => Self::Domain(Box::new(value.clone())),
        }
    }
}

impl TryFrom<IpcResponsePayload> for ResponsePayload {
    type Error = CodecError;

    fn try_from(value: IpcResponsePayload) -> Result<Self, Self::Error> {
        match value {
            IpcResponsePayload::Domain(payload) => Ok(*payload),
            IpcResponsePayload::SessionHistory {
                session_id,
                history,
            } => Ok(Self::SessionHistory {
                session_id,
                history: ipc_events_to_session_events(history)?,
            }),
            IpcResponsePayload::SessionHistoryPage { page } => Ok(Self::SessionHistoryPage {
                page: page.try_into()?,
            }),
            IpcResponsePayload::Attached {
                session_id,
                session,
                history,
                input_history,
                import_warnings,
            } => Ok(Self::Attached {
                session_id,
                session,
                history: ipc_events_to_session_events(history)?,
                input_history,
                import_warnings,
            }),
            IpcResponsePayload::RuntimeWorkHistory { events } => Ok(Self::RuntimeWorkHistory {
                events: ipc_events_to_session_events(events)?,
            }),
        }
    }
}

impl From<&SessionHistoryPage> for IpcSessionHistoryPage {
    fn from(value: &SessionHistoryPage) -> Self {
        Self {
            session_id: value.session_id,
            events: value.events.iter().map(IpcSessionEvent::from).collect(),
            next_cursor: value.next_cursor,
            has_more: value.has_more,
        }
    }
}

impl TryFrom<IpcSessionHistoryPage> for SessionHistoryPage {
    type Error = CodecError;

    fn try_from(value: IpcSessionHistoryPage) -> Result<Self, Self::Error> {
        Ok(Self {
            session_id: value.session_id,
            events: ipc_events_to_session_events(value.events)?,
            next_cursor: value.next_cursor,
            has_more: value.has_more,
        })
    }
}

fn ipc_events_to_session_events(
    events: Vec<IpcSessionEvent>,
) -> Result<Vec<SessionEvent>, CodecError> {
    events.into_iter().map(TryInto::try_into).collect()
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IpcEvent {
    Session(IpcSessionEvent),
    SessionLive(SessionLiveEvent),
    RuntimeWork(IpcSessionEvent),
    SessionCatalogUpdated { revision: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IpcSessionEvent {
    schema_version: u16,
    sequence: u64,
    session_id: SessionId,
    provenance: Option<bcode_session_models::SessionEventProvenance>,
    kind: IpcSessionEventKind,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IpcSessionEventKind {
    SessionCreated {
        name: Option<String>,
        working_directory: PathBuf,
    },
    ClientAttached {
        client_id: ClientId,
    },
    ClientDetached {
        client_id: ClientId,
    },
    UserMessage {
        client_id: ClientId,
        text: String,
    },
    AssistantDelta {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCallRequested {
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    ToolCallFinished {
        tool_call_id: String,
        result: String,
        is_error: bool,
        output: Option<TraceBlobRef>,
        semantic_result: Option<IpcToolInvocationResult>,
    },
    PermissionRequested {
        permission_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    PermissionResolved {
        permission_id: String,
        approved: bool,
    },
    ModelChanged {
        provider: String,
        model: String,
    },
    SystemMessage {
        text: String,
    },
    AgentChanged {
        agent_id: String,
    },
    ModelTurnStarted {
        turn_id: String,
    },
    ModelTurnFinished {
        turn_id: String,
        outcome: ModelTurnOutcome,
        message: Option<String>,
    },
    ModelUsage {
        turn_id: String,
        usage: SessionTokenUsage,
    },
    ContextCompacted {
        summary: String,
        compacted_through_sequence: u64,
    },
    SessionRenamed {
        name: Option<String>,
    },
    TraceEvent {
        trace: Box<SessionTraceEvent>,
    },
    SkillInvoked {
        skill_id: SkillId,
        arguments: String,
        source: Option<SkillSource>,
        invoked_at_ms: u64,
    },
    SkillSuggested {
        skill_id: SkillId,
        reason: Option<String>,
        suggested_at_ms: u64,
    },
    SkillActivated {
        skill_id: SkillId,
        source: Option<SkillSource>,
        mode: SkillActivationMode,
        activated_at_ms: u64,
    },
    SkillDeactivated {
        skill_id: SkillId,
        deactivated_at_ms: u64,
    },
    SkillContextLoaded {
        skill_id: SkillId,
        bytes_loaded: usize,
        truncated: bool,
        loaded_at_ms: u64,
    },
    SkillInvocationFailed {
        skill_id: SkillId,
        error: String,
        failed_at_ms: u64,
    },
    AssistantReasoningDelta {
        text: String,
    },
    AssistantReasoningMessage {
        text: String,
    },
    RuntimeWorkStarted {
        work_id: RuntimeWorkId,
        kind: RuntimeWorkKind,
        label: String,
        tool_call_id: Option<String>,
        plugin_id: Option<String>,
        service_interface: Option<String>,
        operation: Option<String>,
        parent_work_id: Option<RuntimeWorkId>,
        started_at_ms: Option<u64>,
        cancellable: bool,
    },
    RuntimeWorkCancelRequested {
        work_id: RuntimeWorkId,
        requested_at_ms: Option<u64>,
        client_id: Option<ClientId>,
    },
    RuntimeWorkFinished {
        work_id: RuntimeWorkId,
        status: RuntimeWorkStatus,
        finished_at_ms: Option<u64>,
        message: Option<String>,
    },
    RuntimeWorkProgress {
        work_id: RuntimeWorkId,
        message: String,
        progress_at_ms: Option<u64>,
        completed_units: Option<u64>,
        total_units: Option<u64>,
    },
    ModelTurnCancelRequested {
        turn_id: String,
        requested_at_ms: Option<u64>,
        client_id: Option<ClientId>,
    },
    ToolInvocationStream {
        event: ToolInvocationStreamEvent,
    },
    WorkingDirectoryChanged {
        old_working_directory: PathBuf,
        new_working_directory: PathBuf,
    },
    SessionImported {
        source_id: String,
        source_display_name: String,
        external_session_id: String,
        imported_at_ms: u64,
    },
    ToolInvocationPresentation {
        tool_call_id: String,
        started_at_ms: Option<u64>,
        finished_at_ms: Option<u64>,
        is_error: bool,
        presentation: ToolInvocationPresentation,
    },
    SessionForked {
        source_session_id: SessionId,
        source_title: Option<String>,
        source_cutoff_sequence: Option<u64>,
        source_prompt_sequence: Option<u64>,
        forked_at_ms: u64,
        kind: SessionForkKind,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IpcToolInvocationResult {
    kind: IpcToolInvocationResultKind,
    text: Option<String>,
    json: Option<String>,
    shell_run: Option<IpcShellRunResult>,
    file_change: Option<FileChangeResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IpcToolInvocationResultKind {
    Text,
    Json,
    ShellRun,
    FileChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IpcShellRunResult {
    kind: IpcShellRunResultKind,
    terminal: Option<IpcShellRunTerminalResult>,
    captured: Option<IpcShellRunCapturedResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IpcShellRunResultKind {
    Terminal,
    Captured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IpcShellRunTerminalResult {
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    output_tail: String,
    output_truncated: bool,
    output_bytes: Option<u64>,
    retained_output_bytes: Option<u64>,
    columns: u16,
    rows: u16,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IpcShellRunCapturedResult {
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    stdout_bytes: Option<u64>,
    stderr_bytes: Option<u64>,
}

impl From<&Event> for IpcEvent {
    fn from(value: &Event) -> Self {
        match value {
            Event::Session(event) => Self::Session(IpcSessionEvent::from(event)),
            Event::SessionLive(event) => Self::SessionLive(event.clone()),
            Event::RuntimeWork(event) => Self::RuntimeWork(IpcSessionEvent::from(event)),
            Event::SessionCatalogUpdated { revision } => Self::SessionCatalogUpdated {
                revision: *revision,
            },
        }
    }
}

impl TryFrom<IpcEvent> for Event {
    type Error = CodecError;

    fn try_from(value: IpcEvent) -> Result<Self, Self::Error> {
        match value {
            IpcEvent::Session(event) => event.try_into().map(Self::Session),
            IpcEvent::SessionLive(event) => Ok(Self::SessionLive(event)),
            IpcEvent::RuntimeWork(event) => event.try_into().map(Self::RuntimeWork),
            IpcEvent::SessionCatalogUpdated { revision } => {
                Ok(Self::SessionCatalogUpdated { revision })
            }
        }
    }
}

impl From<&SessionEvent> for IpcSessionEvent {
    fn from(value: &SessionEvent) -> Self {
        Self {
            schema_version: value.schema_version,
            sequence: value.sequence,
            session_id: value.session_id,
            provenance: value.provenance.clone(),
            kind: IpcSessionEventKind::from(&value.kind),
        }
    }
}

impl TryFrom<IpcSessionEvent> for SessionEvent {
    type Error = CodecError;

    fn try_from(value: IpcSessionEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            schema_version: value.schema_version,
            sequence: value.sequence,
            session_id: value.session_id,
            provenance: value.provenance,
            kind: value.kind.try_into()?,
        })
    }
}

impl From<&SessionEventKind> for IpcSessionEventKind {
    #[allow(clippy::clone_on_copy, clippy::too_many_lines)]
    fn from(value: &SessionEventKind) -> Self {
        match value {
            SessionEventKind::SessionCreated {
                name,
                working_directory,
            } => Self::SessionCreated {
                name: name.clone(),
                working_directory: working_directory.clone(),
            },
            SessionEventKind::ClientAttached { client_id } => Self::ClientAttached {
                client_id: client_id.clone(),
            },
            SessionEventKind::ClientDetached { client_id } => Self::ClientDetached {
                client_id: client_id.clone(),
            },
            SessionEventKind::UserMessage { client_id, text } => Self::UserMessage {
                client_id: client_id.clone(),
                text: text.clone(),
            },
            SessionEventKind::AssistantDelta { text } => {
                Self::AssistantDelta { text: text.clone() }
            }
            SessionEventKind::AssistantMessage { text } => {
                Self::AssistantMessage { text: text.clone() }
            }
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            } => Self::ToolCallRequested {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
            },
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                semantic_result,
            } => Self::ToolCallFinished {
                tool_call_id: tool_call_id.clone(),
                result: result.clone(),
                is_error: is_error.clone(),
                output: output.clone(),
                semantic_result: semantic_result.as_ref().map(IpcToolInvocationResult::from),
            },
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            } => Self::PermissionRequested {
                permission_id: permission_id.clone(),
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
            },
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            } => Self::PermissionResolved {
                permission_id: permission_id.clone(),
                approved: approved.clone(),
            },
            SessionEventKind::ModelChanged { provider, model } => Self::ModelChanged {
                provider: provider.clone(),
                model: model.clone(),
            },
            SessionEventKind::SystemMessage { text } => Self::SystemMessage { text: text.clone() },
            SessionEventKind::AgentChanged { agent_id } => Self::AgentChanged {
                agent_id: agent_id.clone(),
            },
            SessionEventKind::ModelTurnStarted { turn_id } => Self::ModelTurnStarted {
                turn_id: turn_id.clone(),
            },
            SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            } => Self::ModelTurnFinished {
                turn_id: turn_id.clone(),
                outcome: outcome.clone(),
                message: message.clone(),
            },
            SessionEventKind::ModelUsage { turn_id, usage } => Self::ModelUsage {
                turn_id: turn_id.clone(),
                usage: usage.clone(),
            },
            SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            } => Self::ContextCompacted {
                summary: summary.clone(),
                compacted_through_sequence: compacted_through_sequence.clone(),
            },
            SessionEventKind::SessionRenamed { name } => {
                Self::SessionRenamed { name: name.clone() }
            }
            SessionEventKind::TraceEvent { trace } => Self::TraceEvent {
                trace: trace.clone(),
            },
            SessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            } => Self::SkillInvoked {
                skill_id: skill_id.clone(),
                arguments: arguments.clone(),
                source: source.clone(),
                invoked_at_ms: invoked_at_ms.clone(),
            },
            SessionEventKind::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            } => Self::SkillSuggested {
                skill_id: skill_id.clone(),
                reason: reason.clone(),
                suggested_at_ms: suggested_at_ms.clone(),
            },
            SessionEventKind::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            } => Self::SkillActivated {
                skill_id: skill_id.clone(),
                source: source.clone(),
                mode: mode.clone(),
                activated_at_ms: activated_at_ms.clone(),
            },
            SessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            } => Self::SkillDeactivated {
                skill_id: skill_id.clone(),
                deactivated_at_ms: deactivated_at_ms.clone(),
            },
            SessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
            } => Self::SkillContextLoaded {
                skill_id: skill_id.clone(),
                bytes_loaded: bytes_loaded.clone(),
                truncated: truncated.clone(),
                loaded_at_ms: loaded_at_ms.clone(),
            },
            SessionEventKind::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            } => Self::SkillInvocationFailed {
                skill_id: skill_id.clone(),
                error: error.clone(),
                failed_at_ms: failed_at_ms.clone(),
            },
            SessionEventKind::AssistantReasoningDelta { text } => {
                Self::AssistantReasoningDelta { text: text.clone() }
            }
            SessionEventKind::AssistantReasoningMessage { text } => {
                Self::AssistantReasoningMessage { text: text.clone() }
            }
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms,
                cancellable,
            } => Self::RuntimeWorkStarted {
                work_id: work_id.clone(),
                kind: kind.clone(),
                label: label.clone(),
                tool_call_id: tool_call_id.clone(),
                plugin_id: plugin_id.clone(),
                service_interface: service_interface.clone(),
                operation: operation.clone(),
                parent_work_id: parent_work_id.clone(),
                started_at_ms: started_at_ms.clone(),
                cancellable: cancellable.clone(),
            },
            SessionEventKind::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            } => Self::RuntimeWorkCancelRequested {
                work_id: work_id.clone(),
                requested_at_ms: requested_at_ms.clone(),
                client_id: client_id.clone(),
            },
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            } => Self::RuntimeWorkFinished {
                work_id: work_id.clone(),
                status: status.clone(),
                finished_at_ms: finished_at_ms.clone(),
                message: message.clone(),
            },
            SessionEventKind::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            } => Self::RuntimeWorkProgress {
                work_id: work_id.clone(),
                message: message.clone(),
                progress_at_ms: progress_at_ms.clone(),
                completed_units: completed_units.clone(),
                total_units: total_units.clone(),
            },
            SessionEventKind::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            } => Self::ModelTurnCancelRequested {
                turn_id: turn_id.clone(),
                requested_at_ms: requested_at_ms.clone(),
                client_id: client_id.clone(),
            },
            SessionEventKind::ToolInvocationStream { event } => Self::ToolInvocationStream {
                event: event.clone(),
            },
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            } => Self::WorkingDirectoryChanged {
                old_working_directory: old_working_directory.clone(),
                new_working_directory: new_working_directory.clone(),
            },
            SessionEventKind::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            } => Self::SessionImported {
                source_id: source_id.clone(),
                source_display_name: source_display_name.clone(),
                external_session_id: external_session_id.clone(),
                imported_at_ms: imported_at_ms.clone(),
            },
            SessionEventKind::ToolInvocationPresentation {
                tool_call_id,
                started_at_ms,
                finished_at_ms,
                is_error,
                presentation,
            } => Self::ToolInvocationPresentation {
                tool_call_id: tool_call_id.clone(),
                started_at_ms: started_at_ms.clone(),
                finished_at_ms: finished_at_ms.clone(),
                is_error: is_error.clone(),
                presentation: presentation.clone(),
            },
            SessionEventKind::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            } => Self::SessionForked {
                source_session_id: source_session_id.clone(),
                source_title: source_title.clone(),
                source_cutoff_sequence: source_cutoff_sequence.clone(),
                source_prompt_sequence: source_prompt_sequence.clone(),
                forked_at_ms: forked_at_ms.clone(),
                kind: kind.clone(),
            },
        }
    }
}

impl TryFrom<IpcSessionEventKind> for SessionEventKind {
    type Error = CodecError;

    #[allow(clippy::too_many_lines)]
    fn try_from(value: IpcSessionEventKind) -> Result<Self, Self::Error> {
        match value {
            IpcSessionEventKind::SessionCreated {
                name,
                working_directory,
            } => Ok(Self::SessionCreated {
                name,
                working_directory,
            }),
            IpcSessionEventKind::ClientAttached { client_id } => {
                Ok(Self::ClientAttached { client_id })
            }
            IpcSessionEventKind::ClientDetached { client_id } => {
                Ok(Self::ClientDetached { client_id })
            }
            IpcSessionEventKind::UserMessage { client_id, text } => {
                Ok(Self::UserMessage { client_id, text })
            }
            IpcSessionEventKind::AssistantDelta { text } => Ok(Self::AssistantDelta { text }),
            IpcSessionEventKind::AssistantMessage { text } => Ok(Self::AssistantMessage { text }),
            IpcSessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            } => Ok(Self::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            }),
            IpcSessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                semantic_result,
            } => Ok(Self::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                semantic_result: semantic_result.map(TryInto::try_into).transpose()?,
            }),
            IpcSessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            } => Ok(Self::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            }),
            IpcSessionEventKind::PermissionResolved {
                permission_id,
                approved,
            } => Ok(Self::PermissionResolved {
                permission_id,
                approved,
            }),
            IpcSessionEventKind::ModelChanged { provider, model } => {
                Ok(Self::ModelChanged { provider, model })
            }
            IpcSessionEventKind::SystemMessage { text } => Ok(Self::SystemMessage { text }),
            IpcSessionEventKind::AgentChanged { agent_id } => Ok(Self::AgentChanged { agent_id }),
            IpcSessionEventKind::ModelTurnStarted { turn_id } => {
                Ok(Self::ModelTurnStarted { turn_id })
            }
            IpcSessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            } => Ok(Self::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            }),
            IpcSessionEventKind::ModelUsage { turn_id, usage } => {
                Ok(Self::ModelUsage { turn_id, usage })
            }
            IpcSessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            } => Ok(Self::ContextCompacted {
                summary,
                compacted_through_sequence,
            }),
            IpcSessionEventKind::SessionRenamed { name } => Ok(Self::SessionRenamed { name }),
            IpcSessionEventKind::TraceEvent { trace } => Ok(Self::TraceEvent { trace }),
            IpcSessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            } => Ok(Self::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            }),
            IpcSessionEventKind::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            } => Ok(Self::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            }),
            IpcSessionEventKind::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            } => Ok(Self::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            }),
            IpcSessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            } => Ok(Self::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            }),
            IpcSessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
            } => Ok(Self::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
            }),
            IpcSessionEventKind::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            } => Ok(Self::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            }),
            IpcSessionEventKind::AssistantReasoningDelta { text } => {
                Ok(Self::AssistantReasoningDelta { text })
            }
            IpcSessionEventKind::AssistantReasoningMessage { text } => {
                Ok(Self::AssistantReasoningMessage { text })
            }
            IpcSessionEventKind::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms,
                cancellable,
            } => Ok(Self::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms,
                cancellable,
            }),
            IpcSessionEventKind::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            } => Ok(Self::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            }),
            IpcSessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            } => Ok(Self::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            }),
            IpcSessionEventKind::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            } => Ok(Self::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            }),
            IpcSessionEventKind::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            } => Ok(Self::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            }),
            IpcSessionEventKind::ToolInvocationStream { event } => {
                Ok(Self::ToolInvocationStream { event })
            }
            IpcSessionEventKind::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            } => Ok(Self::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            }),
            IpcSessionEventKind::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            } => Ok(Self::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            }),
            IpcSessionEventKind::ToolInvocationPresentation {
                tool_call_id,
                started_at_ms,
                finished_at_ms,
                is_error,
                presentation,
            } => Ok(Self::ToolInvocationPresentation {
                tool_call_id,
                started_at_ms,
                finished_at_ms,
                is_error,
                presentation,
            }),
            IpcSessionEventKind::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            } => Ok(Self::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            }),
        }
    }
}

impl From<&ToolInvocationResult> for IpcToolInvocationResult {
    fn from(value: &ToolInvocationResult) -> Self {
        match value {
            ToolInvocationResult::Text { text } => Self {
                kind: IpcToolInvocationResultKind::Text,
                text: Some(text.clone()),
                json: None,
                shell_run: None,
                file_change: None,
            },
            ToolInvocationResult::Json { value } => Self {
                kind: IpcToolInvocationResultKind::Json,
                text: None,
                json: Some(value.clone()),
                shell_run: None,
                file_change: None,
            },
            ToolInvocationResult::ShellRun { result } => Self {
                kind: IpcToolInvocationResultKind::ShellRun,
                text: None,
                json: None,
                shell_run: Some(IpcShellRunResult::from(result)),
                file_change: None,
            },
            ToolInvocationResult::FileChange { result } => Self {
                kind: IpcToolInvocationResultKind::FileChange,
                text: None,
                json: None,
                shell_run: None,
                file_change: Some(result.clone()),
            },
        }
    }
}

impl TryFrom<IpcToolInvocationResult> for ToolInvocationResult {
    type Error = CodecError;

    fn try_from(value: IpcToolInvocationResult) -> Result<Self, Self::Error> {
        match value.kind {
            IpcToolInvocationResultKind::Text => Ok(Self::Text {
                text: value.text.unwrap_or_default(),
            }),
            IpcToolInvocationResultKind::Json => Ok(Self::Json {
                value: value.json.unwrap_or_default(),
            }),
            IpcToolInvocationResultKind::ShellRun => Ok(Self::ShellRun {
                result: value
                    .shell_run
                    .ok_or_else(|| {
                        CodecError::EventConversion("missing shell_run payload".to_string())
                    })?
                    .try_into()?,
            }),
            IpcToolInvocationResultKind::FileChange => Ok(Self::FileChange {
                result: value.file_change.ok_or_else(|| {
                    CodecError::EventConversion("missing file_change payload".to_string())
                })?,
            }),
        }
    }
}

impl From<&ShellRunResult> for IpcShellRunResult {
    fn from(value: &ShellRunResult) -> Self {
        match value {
            ShellRunResult::Terminal {
                exit_code,
                timed_out,
                cancelled,
                output_tail,
                output_truncated,
                output_bytes,
                retained_output_bytes,
                columns,
                rows,
            } => Self {
                kind: IpcShellRunResultKind::Terminal,
                terminal: Some(IpcShellRunTerminalResult {
                    exit_code: *exit_code,
                    timed_out: *timed_out,
                    cancelled: *cancelled,
                    output_tail: output_tail.clone(),
                    output_truncated: *output_truncated,
                    output_bytes: *output_bytes,
                    retained_output_bytes: *retained_output_bytes,
                    columns: *columns,
                    rows: *rows,
                }),
                captured: None,
            },
            ShellRunResult::Captured {
                exit_code,
                timed_out,
                cancelled,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                stdout_bytes,
                stderr_bytes,
            } => Self {
                kind: IpcShellRunResultKind::Captured,
                terminal: None,
                captured: Some(IpcShellRunCapturedResult {
                    exit_code: *exit_code,
                    timed_out: *timed_out,
                    cancelled: *cancelled,
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                    stdout_truncated: *stdout_truncated,
                    stderr_truncated: *stderr_truncated,
                    stdout_bytes: *stdout_bytes,
                    stderr_bytes: *stderr_bytes,
                }),
            },
        }
    }
}

impl TryFrom<IpcShellRunResult> for ShellRunResult {
    type Error = CodecError;

    fn try_from(value: IpcShellRunResult) -> Result<Self, Self::Error> {
        match value.kind {
            IpcShellRunResultKind::Terminal => {
                let terminal = value.terminal.ok_or_else(|| {
                    CodecError::EventConversion("missing terminal shell payload".to_string())
                })?;
                Ok(Self::Terminal {
                    exit_code: terminal.exit_code,
                    timed_out: terminal.timed_out,
                    cancelled: terminal.cancelled,
                    output_tail: terminal.output_tail,
                    output_truncated: terminal.output_truncated,
                    output_bytes: terminal.output_bytes,
                    retained_output_bytes: terminal.retained_output_bytes,
                    columns: terminal.columns,
                    rows: terminal.rows,
                })
            }
            IpcShellRunResultKind::Captured => {
                let captured = value.captured.ok_or_else(|| {
                    CodecError::EventConversion("missing captured shell payload".to_string())
                })?;
                Ok(Self::Captured {
                    exit_code: captured.exit_code,
                    timed_out: captured.timed_out,
                    cancelled: captured.cancelled,
                    stdout: captured.stdout,
                    stderr: captured.stderr,
                    stdout_truncated: captured.stdout_truncated,
                    stderr_truncated: captured.stderr_truncated,
                    stdout_bytes: captured.stdout_bytes,
                    stderr_bytes: captured.stderr_bytes,
                })
            }
        }
    }
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
    #[error("event conversion failed: {0}")]
    EventConversion(String),
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

/// Encode a server event with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_event(event: &Event) -> Result<Vec<u8>, CodecError> {
    let event = IpcEvent::from(event);
    encode(&event)
}

/// Decode a deserializable value with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    bmux_codec::from_positional_bytes(bytes).map_err(CodecError::Deserialize)
}

/// Encode a response with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_response(response: &Response) -> Result<Vec<u8>, CodecError> {
    let response = IpcResponse::from(response);
    encode(&response)
}

/// Decode a response with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization or domain conversion fails.
pub fn decode_response(bytes: &[u8]) -> Result<Response, CodecError> {
    let response = decode::<IpcResponse>(bytes)?;
    response.try_into()
}

/// Decode a server event with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization or domain conversion fails.
pub fn decode_event(bytes: &[u8]) -> Result<Event, CodecError> {
    let event = decode::<IpcEvent>(bytes)?;
    event.try_into()
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
        encode_response(response)?,
    ))
}

/// Build an event envelope.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn event_envelope(event: &Event) -> Result<Envelope, CodecError> {
    Ok(Envelope::new(0, EnvelopeKind::Event, encode_event(event)?))
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
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, FileChangeResult, SessionEventKind,
        SessionForkResult, SessionId, SessionSummary, ShellRunResult, ToolInvocationResult,
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
    fn fork_session_request_and_response_round_trip() {
        let source_session_id: SessionId = "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("source session id should parse");
        let request = Request::ForkSession {
            source_session_id,
            prompt_sequence: 42,
            name: Some("[fork] source".to_owned()),
        };

        let encoded = encode(&request).expect("request should encode");
        let decoded: Request = decode(&encoded).expect("request should decode");

        assert_eq!(decoded, request);

        let session = test_session_summary("[fork] source");
        let response = Response::Ok(ResponsePayload::SessionForked {
            session,
            draft: Some("selected prompt".to_owned()),
        });

        let encoded = encode(&response).expect("response should encode");
        let decoded: Response = decode(&encoded).expect("response should decode");

        assert_eq!(decoded, response);
        let Response::Ok(ResponsePayload::SessionForked { session, draft }) = decoded else {
            panic!("decoded response should be session_forked");
        };
        assert_eq!(session.name.as_deref(), Some("[fork] source"));
        assert_eq!(draft.as_deref(), Some("selected prompt"));
    }

    #[test]
    fn clone_session_request_and_result_round_trip() {
        let source_session_id: SessionId = "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("source session id should parse");
        let request = Request::CloneSession {
            source_session_id,
            name: Some("[clone] source".to_owned()),
        };

        let encoded = encode(&request).expect("request should encode");
        let decoded: Request = decode(&encoded).expect("request should decode");

        assert_eq!(decoded, request);

        let result = SessionForkResult {
            session: test_session_summary("[clone] source"),
            draft: None,
        };
        let encoded = encode(&result).expect("result should encode");
        let decoded: SessionForkResult = decode(&encoded).expect("result should decode");

        assert_eq!(decoded, result);
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
            fork: None,
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

    fn test_session_summary(name: &str) -> SessionSummary {
        let session_id: SessionId = "00000000-0000-0000-0000-000000000002"
            .parse()
            .expect("session id should parse");
        SessionSummary {
            id: session_id,
            name: Some(name.to_owned()),
            explicit_name: Some(name.to_owned()),
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::Explicit,
            client_count: 0,
            created_at_ms: 10,
            updated_at_ms: 20,
            working_directory: "/tmp/bcode-ipc-test".into(),
            import: None,
            fork: None,
        }
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
                        diagnostics: Vec::new(),
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
                semantic_result: None,
            },
        });

        let envelope = event_envelope(&event).expect("event should encode");
        assert!(encode(&envelope).expect("envelope should encode").len() > MAX_FRAME_PAYLOAD_SIZE);

        let received = round_trip_envelope(envelope.clone()).await;

        assert_eq!(received, envelope);
        let decoded = decode::<Event>(&received.payload).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[tokio::test]
    async fn semantic_tool_result_events_round_trip_across_ipc_frames() {
        let session_id = SessionId::new();
        for semantic_result in [
            ToolInvocationResult::FileChange {
                result: FileChangeResult {
                    tool_name: "filesystem.write".to_string(),
                    summary: "wrote 171 bytes".to_string(),
                    path: Some("/tmp/hello_world.rs".to_string()),
                },
            },
            ToolInvocationResult::ShellRun {
                result: ShellRunResult::Terminal {
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    output_tail: "hello\n".to_string(),
                    output_truncated: false,
                    output_bytes: Some(6),
                    retained_output_bytes: Some(6),
                    columns: 120,
                    rows: 30,
                },
            },
            ToolInvocationResult::ShellRun {
                result: ShellRunResult::Captured {
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    stdout: "hello\n".to_string(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    stdout_bytes: Some(6),
                    stderr_bytes: Some(0),
                },
            },
        ] {
            let event = Event::Session(SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 77,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolCallFinished {
                    tool_call_id: "call-1".to_string(),
                    result: "tool result".to_string(),
                    is_error: false,
                    output: None,
                    semantic_result: Some(semantic_result),
                },
            });
            let envelope = event_envelope(&event).expect("event should encode");

            let received = round_trip_envelope(envelope).await;

            let decoded = decode_event(&received.payload).expect("event should decode");
            assert_eq!(decoded, event);
        }
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
