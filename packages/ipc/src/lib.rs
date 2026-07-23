#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Client/server IPC protocol for bcode.

use bcode_agent_profile::{AgentInfo, PolicyStatusResponse};
use bcode_metrics::MetricsSnapshot;
use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_models::{
    ClientId, ProjectionWindowRequest, RuntimeWorkKind, RuntimeWorkStatus, SessionEvent,
    SessionHistoryPage, SessionHistoryQuery, SessionId, SessionInputHistoryEntry, SessionLiveEvent,
    SessionSummary, WorkId,
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
///
/// Same-build client/server compatibility is expected. Bump this when IPC DTO
/// enum layouts or envelope payload shapes change incompatibly so stale
/// client/daemon pairs fail explicitly during envelope decode instead of
/// interpreting payloads with mismatched positional layouts.
pub const CURRENT_PROTOCOL_VERSION: u16 = 12;

/// Durable session-storage writer epoch expected by this IPC build.
pub const CURRENT_SESSION_STORAGE_WRITER_EPOCH: u32 = 4;

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
    /// Insert the prompt at the next safe conversation boundary.
    ///
    /// When a model request is already streaming, the next safe boundary is a queued follow-up
    /// turn after the active response finishes.
    #[default]
    Steering,
    /// Queue the prompt to run as a follow-up turn after the active turn finishes.
    FollowUp,
}

/// Server-side disposition for an accepted user prompt or skill invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAcceptanceDisposition {
    /// Accepted as a new turn that can start immediately.
    #[default]
    StartedTurn,
    /// Accepted as steering for the active turn.
    AppliedSteering,
    /// Accepted as a follow-up requested for after the active turn.
    QueuedFollowUp,
    /// Accepted behind already queued session work.
    QueuedTurn,
}

impl MessageAcceptanceDisposition {
    /// Return whether the disposition is the wire-compatible default.
    #[must_use]
    pub const fn is_default(disposition: &Self) -> bool {
        matches!(disposition, Self::StartedTurn)
    }
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
    /// Internal compressed frame for large logical envelopes.
    Compressed,
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

/// Scope for durable composer draft persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerDraftScope {
    /// Draft belongs to a persisted session.
    Session { session_id: SessionId },
    /// Draft belongs to the unsaved draft session for the launch working directory.
    DraftSession { launch_working_directory: PathBuf },
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
    ServerStatus {
        /// Client working directory used to scope repository-local status.
        #[serde(default)]
        working_directory: Option<PathBuf>,
    },
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
    /// Submit an ordinary turn through generic admission metadata.
    SubmitTurn {
        session_id: SessionId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
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
        work_id: WorkId,
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
    DefaultModelStatus,
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
        #[serde(default)]
        remember: bool,
    },
    AddPermissionRule {
        agent_id: String,
        category: String,
        pattern: String,
        action: String,
    },
    ListPluginServices,
    ListPluginContributions,
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
    RalphStatus(RalphStatusRequest),
    RunRalphLoop(RalphRunRequest),
    CancelRalphLoop(RalphCancelRequest),
    ListRalphRuns(Box<RalphListRunsRequest>),
    ListRalphIterations(Box<RalphListIterationsRequest>),
    ResumeRalphRun(RalphResumeRequest),
    ApproveRalphRun(RalphApproveRequest),
    RalphRunStatus(RalphRunStatusRequest),
    RecordRalphLifecycle(RalphLifecycleRequest),
    ImportExternalSession {
        source_id: String,
        external_session_id: String,
        /// Client working directory used when the imported source has no cwd metadata.
        #[serde(default)]
        working_directory: Option<PathBuf>,
    },
    ForkSession {
        source_session_id: SessionId,
        prompt_sequence: u64,
        name: Option<String>,
    },
    CloneSession {
        source_session_id: SessionId,
        name: Option<String>,
        /// Require the cloned history snapshot to end at this generation.
        #[serde(default)]
        expected_generation: Option<u64>,
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
    SetComposerDraft {
        scope: ComposerDraftScope,
        text: String,
    },
    ComposerDraft {
        scope: ComposerDraftScope,
    },
    ListPendingToolExchanges,
    ResolveToolExchange {
        exchange_id: String,
        resolution_json: serde_json::Value,
    },
    /// Inspect effective model catalog and refresh state.
    ModelCatalogDiagnostics,
    /// Read a bounded byte range from a generic session artifact reference.
    ReadSessionArtifact {
        session_id: SessionId,
        artifact_id: String,
        reference_key: String,
        #[serde(default)]
        offset: u64,
        length: u32,
    },
    /// Deliver opaque schema-versioned input to an active invocation.
    InvocationInput {
        session_id: SessionId,
        input: bcode_tool::ToolInvocationInput,
    },
    /// Resolve every currently pending checkpoint in one exact authorization batch.
    ResolvePermissionBatch {
        batch_id: String,
        approved: bool,
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
    /// Canonical working directory of the client process.
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub selected_provider_plugin_id: Option<String>,
    #[serde(default)]
    pub selected_model_id: Option<String>,
    /// User-facing model id before alias resolution.
    #[serde(default)]
    pub requested_model_id: Option<String>,
    #[serde(default)]
    pub provider_context: bcode_model::ProviderRequestContext,
    /// Renderer-owned adapters available on this client connection.
    #[serde(default)]
    pub interaction_adapters:
        Vec<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability>,
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
    /// SHA-256 digest of the executable bytes running the daemon.
    #[serde(default)]
    pub executable_digest: Option<String>,
    /// Durable session-storage writer epoch supported by this daemon.
    #[serde(default)]
    pub storage_writer_epoch: Option<u32>,
    /// Highest persisted session-event schema version understood by this daemon.
    #[serde(default)]
    pub session_event_schema_version: Option<u16>,
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

/// Runtime selections restored from a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeSelection {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    /// User-facing requested model id.
    #[serde(default)]
    pub requested_model_id: Option<String>,
    /// Concrete effective model id when known.
    #[serde(default)]
    pub effective_model_id: Option<String>,
    /// Legacy model selection field.
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_summary: Option<String>,
}

/// Active model metadata for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModelStatus {
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    /// User-facing requested model id before alias/default resolution.
    #[serde(default)]
    pub requested_model_id: Option<String>,
    /// Concrete effective model id used for metadata and provider requests.
    #[serde(default)]
    pub effective_model_id: Option<String>,
    /// Legacy display model field retained for wire compatibility.
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Authoritative active context occupancy.
    #[serde(default)]
    pub context_occupancy: Option<Box<bcode_session_models::RequestContextOccupancy>>,
    /// Projection error preventing a trustworthy occupancy value.
    #[serde(default)]
    pub request_context_error: Option<String>,
    #[serde(default)]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub context_format_version: Option<u16>,
    #[serde(default)]
    pub compatibility_key: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning: Option<bcode_model::ModelReasoningInfo>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_summary: Option<String>,
    #[serde(default)]
    pub prompt_cache_mode: Option<String>,
    #[serde(default)]
    pub conversation_reuse_mode: Option<String>,
    #[serde(default)]
    pub compaction_mode: Option<String>,
    #[serde(default)]
    pub compaction_backend: Option<String>,
    #[serde(default)]
    pub proactive_compaction_threshold_percent: Option<u8>,
    #[serde(default)]
    pub cache: Option<bcode_model::ModelCacheInfo>,
    #[serde(default)]
    pub metadata_source: Option<bcode_model::ModelMetadataSource>,
    #[serde(default)]
    pub pricing: Option<bcode_model::ModelPricingInfo>,
}

/// Manifest-declared plugin contributions available without executing plugin code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginContributions {
    #[serde(default)]
    pub commands: Vec<bcode_plugin::PluginOwnedCommandContribution>,
    #[serde(default)]
    pub command_contributions: Vec<bcode_command::CommandContribution>,
    #[serde(default)]
    pub config_extensions: Vec<bcode_plugin::PluginConfigExtension>,
}

/// Service interface provided by a loaded plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginServiceSummary {
    pub plugin_id: String,
    pub interface_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

/// Correlation metadata for one permission checkpoint in a complete tool-call batch.
pub use bcode_session_models::PermissionBatchCorrelation;

/// Pending permission checkpoint summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSummary {
    pub permission_id: String,
    pub session_id: SessionId,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments_json: String,
    /// Complete-batch correlation for grouped permission consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<PermissionBatchCorrelation>,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_reason: Option<String>,
    #[serde(default)]
    pub can_remember_policy: bool,
}

/// Pending renderer-neutral invocation exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingToolExchangeSummary {
    pub session_id: SessionId,
    pub request: bcode_session_models::ToolExchangeRequest,
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

/// Ralph lifecycle session-history append request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphLifecycleRequest {
    /// Session that should receive the durable lifecycle marker.
    pub session_id: SessionId,
    /// User-facing loop name.
    pub loop_name: String,
    /// Ralph loop state directory.
    pub state_dir: PathBuf,
    /// Lifecycle kind.
    pub kind: String,
    /// Human-readable lifecycle message.
    pub message: String,
    /// Lifecycle time in Unix epoch milliseconds.
    pub occurred_at_ms: u64,
}

/// Ralph loop status request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphStatusRequest {
    /// Repository root used to discover the active/latest Ralph loop.
    pub repo_root: PathBuf,
}

/// Ralph loop status summary for IPC clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphStatusSummary {
    /// User-facing loop name.
    pub loop_name: String,
    /// Current lifecycle status.
    pub status: String,
    /// Loop state directory.
    pub state_dir: PathBuf,
    /// Canonical progress document path.
    pub progress_doc_path: PathBuf,
    /// Isolated work area path, when created.
    #[serde(default)]
    pub work_area_path: Option<PathBuf>,
    /// Session ID rooted at the isolated work area, when created.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Completed iteration count.
    pub iteration_count: u64,
    /// Suggested next action.
    pub next_action: String,
    /// Checked progress-doc checklist items.
    pub checked_count: usize,
    /// Unchecked progress-doc checklist items.
    pub unchecked_count: usize,
    /// Validation commands configured for the loop.
    #[serde(default)]
    pub validation_commands: Vec<String>,
}

/// Ralph loop status response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphStatusResponse {
    /// Latest Ralph loop summary for the repository, when one exists.
    #[serde(default)]
    pub loop_summary: Option<RalphStatusSummary>,
}

/// Request to start a bounded Ralph autonomous run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to run, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
    /// Requested max iteration override.
    #[serde(default)]
    pub max_iterations: Option<u64>,
    /// Requested no-progress limit override.
    #[serde(default)]
    pub no_progress_limit: Option<u64>,
    /// Whether this run should begin in an approval-gated state.
    #[serde(default)]
    pub require_approval: bool,
}

/// Request to cancel an active Ralph run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphCancelRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific run ID to cancel. Defaults to the active run for the loop.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Specific Ralph loop state directory to cancel, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
}

/// Request to list recent Ralph runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphListRunsRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
}

/// Request to list recent Ralph iterations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphListIterationsRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
    /// Specific run ID to inspect, when not using the latest run.
    #[serde(default)]
    pub run_id: Option<String>,
}

/// Request to prepare resuming an interrupted Ralph run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphResumeRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
    /// Specific interrupted run ID to resume, when not using the latest interrupted run.
    #[serde(default)]
    pub interrupted_run_id: Option<String>,
}

/// Request to approve and start an approval-gated Ralph run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphApproveRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
    /// Specific run ID to approve, when not using the active approval-gated run.
    #[serde(default)]
    pub run_id: Option<String>,
}

/// Request to inspect Ralph autonomous run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunStatusRequest {
    /// Repository root used to discover the selected Ralph loop.
    pub repo_root: PathBuf,
    /// Specific Ralph loop state directory to inspect, when not using latest.
    #[serde(default)]
    pub loop_state_dir: Option<PathBuf>,
}

/// Ralph autonomous run summary for IPC clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunSummary {
    /// Run ID.
    pub run_id: String,
    /// Loop state directory this run belongs to.
    pub state_dir: PathBuf,
    /// Work-area session used by the runner, when known.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Parent runtime-work ID emitted for this run.
    #[serde(default)]
    pub runtime_work_id: Option<String>,
    /// Current run status.
    pub status: String,
    /// Requested max iteration override.
    #[serde(default)]
    pub requested_max_iterations: Option<u64>,
    /// Requested no-progress limit override.
    #[serde(default)]
    pub requested_no_progress_limit: Option<u64>,
    /// Whether cancellation was requested.
    pub cancel_requested: bool,
    /// Run start time in Unix epoch milliseconds.
    pub started_at_ms: u64,
    /// Last update time in Unix epoch milliseconds.
    pub updated_at_ms: u64,
    /// Run finish time in Unix epoch milliseconds.
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
    /// Terminal stop reason, when known.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Terminal error message, when known.
    #[serde(default)]
    pub error_message: Option<String>,
}

/// Ralph iteration summary for IPC clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphIterationSummary {
    /// Iteration ID.
    pub iteration_id: String,
    /// Run ID this iteration belongs to.
    pub run_id: String,
    /// Iteration number.
    pub iteration_number: u64,
    /// Iteration status.
    pub status: String,
    /// Stop reason, when known.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Error message, when known.
    #[serde(default)]
    pub error_message: Option<String>,
    /// Finish time in Unix epoch milliseconds.
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
}

/// Ralph validation summary for IPC clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphValidationSummary {
    /// Validation ID.
    pub validation_id: String,
    /// Parent iteration ID.
    pub iteration_id: String,
    /// Validation command.
    pub command: String,
    /// Validation status.
    pub status: String,
    /// Process exit code, when available.
    #[serde(default)]
    pub exit_code: Option<i64>,
    /// Bounded output reference, when retained.
    #[serde(default)]
    pub output_ref: Option<String>,
    /// Validation finish time in Unix epoch milliseconds.
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
    /// Error message, when validation failed to run.
    #[serde(default)]
    pub error_message: Option<String>,
}

/// Response after starting a Ralph run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunResponse {
    /// Persisted run summary.
    pub run: RalphRunSummary,
}

/// Response after requesting Ralph run cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphCancelResponse {
    /// Run summary after cancellation was requested.
    pub run: RalphRunSummary,
    /// Whether the cancel flag was requested by this call.
    pub cancel_requested: bool,
}

/// Response listing recent Ralph runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphListRunsResponse {
    /// Latest or selected Ralph loop summary for the repository, when one exists.
    #[serde(default)]
    pub loop_summary: Option<RalphStatusSummary>,
    /// Recent runs for the loop.
    #[serde(default)]
    pub runs: Vec<RalphRunSummary>,
}

/// Response listing recent Ralph iterations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphListIterationsResponse {
    /// Latest or selected Ralph loop summary for the repository, when one exists.
    #[serde(default)]
    pub loop_summary: Option<RalphStatusSummary>,
    /// Run whose iterations were listed, when one exists.
    #[serde(default)]
    pub run: Option<RalphRunSummary>,
    /// Iterations for the run.
    #[serde(default)]
    pub iterations: Vec<RalphIterationSummary>,
    /// Validation records grouped with the listed iterations.
    #[serde(default)]
    pub validations: Vec<RalphValidationSummary>,
}

/// Response after preparing a Ralph resume run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphResumeResponse {
    /// Interrupted run selected for resume.
    pub interrupted_run: RalphRunSummary,
    /// Newly created approval-gated run.
    pub resumed_run: RalphRunSummary,
}

/// Response describing Ralph autonomous run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphRunStatusResponse {
    /// Latest or selected Ralph loop summary for the repository, when one exists.
    #[serde(default)]
    pub loop_summary: Option<RalphStatusSummary>,
    /// Active run for the loop, when one exists.
    #[serde(default)]
    pub active_run: Option<RalphRunSummary>,
    /// Interrupted runs for the loop.
    #[serde(default)]
    pub interrupted_runs: Vec<RalphRunSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeWorkSnapshot {
    pub work_id: WorkId,
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
        daemon: DaemonStatus,
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
        #[serde(default)]
        draft: Option<String>,
        #[serde(default)]
        runtime_selection: SessionRuntimeSelection,
        #[serde(default)]
        projection_window: Option<bcode_session_models::ProjectionWindow>,
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
    MessageAcceptedWithDisposition {
        queued: bool,
        queue_position: Option<u32>,
        disposition: MessageAcceptanceDisposition,
    },
    TurnAdmission {
        admission: bcode_session_models::TurnAdmission,
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
    RalphStatus(RalphStatusResponse),
    RalphRunStarted(RalphRunResponse),
    RalphRunCancelled(RalphCancelResponse),
    RalphRunsListed(RalphListRunsResponse),
    RalphIterationsListed(RalphListIterationsResponse),
    RalphRunResumed(RalphResumeResponse),
    RalphRunApproved(RalphRunResponse),
    RalphRunStatus(RalphRunStatusResponse),
    RalphLifecycleRecorded {
        event: SessionEvent,
    },
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
    ComposerDraft {
        draft: Option<String>,
    },
    ComposerDraftSet,
    PluginContributions {
        contributions: PluginContributions,
    },
    PendingToolExchangeList {
        exchanges: Vec<PendingToolExchangeSummary>,
    },
    ToolExchangeResolved {
        resolved: bool,
    },
    /// Effective model catalog diagnostics.
    ModelCatalogDiagnostics {
        embedded_revision: String,
        remote_revision: Option<String>,
        remote_enabled: bool,
        cache_state: String,
        cache_age_seconds: Option<u64>,
        refresh_in_progress: bool,
        last_refresh_attempt_ms: Option<u64>,
        last_refresh_success_ms: Option<u64>,
        last_refresh_error: Option<String>,
    },
    SessionArtifactRange {
        artifact_id: String,
        reference_key: String,
        content_type: Option<String>,
        offset: u64,
        total_bytes: u64,
        reference_bytes: Option<u64>,
        reference_revision: u64,
        finalized: bool,
        finalized_event_seq: Option<u64>,
        availability: Option<String>,
        complete: Option<bool>,
        checksum_sha256: Option<String>,
        bytes: Vec<u8>,
    },
    InvocationInputAccepted,
    PermissionBatchResolved {
        resolved: usize,
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
    /// Local/client signal that a reattached session view needs one bounded snapshot refresh.
    SessionViewResyncRequired {
        session_id: SessionId,
    },
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
    #[error("event conversion failed: {0}")]
    EventConversion(String),
    #[error("unsupported protocol version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u16, expected: u16 },
}

/// Encode a value with the positional Bcode wire codec.
///
/// Positional encoding is intended for fixed, hot-path IPC framing structures.
/// Prefer [`encode_typed_stable`] for logical IPC payloads with evolving schemas.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_positional<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    bmux_codec::to_positional_vec(value).map_err(CodecError::Serialize)
}

/// Decode a value with the positional Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode_positional<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    bmux_codec::from_positional_bytes(bytes).map_err(CodecError::Deserialize)
}

/// Encode a value with the typed-stable Bcode wire codec.
///
/// Typed-stable encoding is intended for logical IPC payloads whose schemas
/// evolve more frequently than the outer frame protocol.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_typed_stable<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    bmux_codec::to_typed_vec(value).map_err(CodecError::Serialize)
}

/// Decode a value with the typed-stable Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode_typed_stable<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    bmux_codec::from_typed_bytes(bytes).map_err(CodecError::Deserialize)
}

/// Encode a serializable value with the fixed-frame Bcode wire codec.
///
/// This compatibility helper remains positional for envelope and chunk framing.
/// New logical IPC payload code should use the explicit typed-stable helpers.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    encode_positional(value)
}

/// Encode a request with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_request(request: &Request) -> Result<Vec<u8>, CodecError> {
    encode_typed_stable(request)
}

/// Decode a request with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode_request(bytes: &[u8]) -> Result<Request, CodecError> {
    decode_typed_stable(bytes)
}

/// Encode a server event with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_event(event: &Event) -> Result<Vec<u8>, CodecError> {
    encode_typed_stable(event)
}

/// Decode a deserializable value with the fixed-frame Bcode wire codec.
///
/// This compatibility helper remains positional for envelope and chunk framing.
/// New logical IPC payload code should use the explicit typed-stable helpers.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    decode_positional(bytes)
}

/// Encode a response with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_response(response: &Response) -> Result<Vec<u8>, CodecError> {
    encode_typed_stable(response)
}

/// Decode a response with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization or domain conversion fails.
pub fn decode_response(bytes: &[u8]) -> Result<Response, CodecError> {
    decode_typed_stable(bytes)
}

/// Decode a server event with the Bcode wire codec.
///
/// # Errors
///
/// Returns an error when deserialization or domain conversion fails.
pub fn decode_event(bytes: &[u8]) -> Result<Event, CodecError> {
    decode_typed_stable(bytes)
}

const COMPRESSION_MIN_BYTES: usize = 256 * 1024;
const COMPRESSION_LEVEL: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompressedEnvelopePayload {
    kind: EnvelopeKind,
    request_id: u64,
    algorithm_wire_id: u8,
    uncompressed_len: u64,
    data: Vec<u8>,
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
    let frames = encode_envelope_frames(envelope)?;
    write_encoded_envelope_frames(writer, &frames).await
}

/// Encode one logical envelope into physical wire frames.
///
/// Pre-encoding lets callers perform expensive serialization/chunk construction outside writer
/// critical sections while preserving the exact same wire protocol.
///
/// # Errors
///
/// Returns an error when serialization fails or the logical envelope is too large to chunk.
pub fn encode_envelope_frames(envelope: &Envelope) -> Result<Vec<Vec<u8>>, CodecError> {
    let physical_envelope = maybe_compress_envelope(envelope)?;
    let payload = encode(&physical_envelope)?;
    if payload.len() <= MAX_FRAME_PAYLOAD_SIZE {
        return Ok(vec![encode_envelope_frame(&physical_envelope)?]);
    }
    encode_chunked_envelope_frames(physical_envelope.request_id, &payload)
}

fn maybe_compress_envelope(envelope: &Envelope) -> Result<Envelope, CodecError> {
    if !matches!(envelope.kind, EnvelopeKind::Response | EnvelopeKind::Event) {
        return Ok(envelope.clone());
    }
    let policy = bmux_codec::compression::CompressionPolicy::new(
        bmux_codec::compression::CompressionAlgorithm::Lz4,
    )
    .min_bytes(COMPRESSION_MIN_BYTES)
    .level(COMPRESSION_LEVEL);
    match bmux_codec::compression::maybe_compress_bytes(envelope.payload.clone(), policy)
        .map_err(CodecError::Serialize)?
    {
        bmux_codec::compression::CompressionDecision::Plain { .. } => Ok(envelope.clone()),
        bmux_codec::compression::CompressionDecision::Compressed {
            algorithm,
            uncompressed_len,
            bytes,
        } => Ok(Envelope::new(
            envelope.request_id,
            EnvelopeKind::Compressed,
            encode(&CompressedEnvelopePayload {
                kind: envelope.kind,
                request_id: envelope.request_id,
                algorithm_wire_id: algorithm.wire_id(),
                uncompressed_len: u64::try_from(uncompressed_len).map_err(|_| {
                    CodecError::PayloadTooLarge {
                        actual: uncompressed_len,
                        max: usize::MAX,
                    }
                })?,
                data: bytes,
            })?,
        )),
    }
}

fn encode_chunked_envelope_frames(
    request_id: u64,
    payload: &[u8],
) -> Result<Vec<Vec<u8>>, CodecError> {
    let chunk_count = payload.len().div_ceil(MAX_CHUNK_DATA_SIZE);
    let chunk_count = u32::try_from(chunk_count).map_err(|_| CodecError::PayloadTooLarge {
        actual: payload.len(),
        max: MAX_FRAME_PAYLOAD_SIZE,
    })?;
    let total_len = u64::try_from(payload.len()).map_err(|_| CodecError::PayloadTooLarge {
        actual: payload.len(),
        max: MAX_FRAME_PAYLOAD_SIZE,
    })?;

    payload
        .chunks(MAX_CHUNK_DATA_SIZE)
        .enumerate()
        .map(|(chunk_index, data)| {
            let chunk_payload = ChunkPayload {
                chunk_index: u32::try_from(chunk_index).map_err(|_| {
                    CodecError::PayloadTooLarge {
                        actual: payload.len(),
                        max: MAX_FRAME_PAYLOAD_SIZE,
                    }
                })?,
                chunk_count,
                total_len,
                data: data.to_vec(),
            };
            let chunk_envelope =
                Envelope::new(request_id, EnvelopeKind::Chunk, encode(&chunk_payload)?);
            encode_envelope_frame(&chunk_envelope)
        })
        .collect()
}

/// Write pre-encoded physical envelope frames.
///
/// # Errors
///
/// Returns an error when the underlying writer fails.
pub async fn write_encoded_envelope_frames<W>(
    writer: &mut W,
    frames: &[Vec<u8>],
) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
{
    for frame in frames {
        writer.write_all(frame).await?;
    }
    writer.flush().await?;
    Ok(())
}

/// Return how many physical frames would be written for a logical envelope.
///
/// # Errors
///
/// Returns an error when envelope serialization fails or the frame count exceeds wire limits.
pub fn envelope_frame_count(envelope: &Envelope) -> Result<u32, CodecError> {
    u32::try_from(encode_envelope_frames(envelope)?.len()).map_err(|_| {
        CodecError::PayloadTooLarge {
            actual: usize::MAX,
            max: MAX_FRAME_PAYLOAD_SIZE,
        }
    })
}

fn encode_envelope_frame(envelope: &Envelope) -> Result<Vec<u8>, CodecError> {
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
    let mut frame = Vec::with_capacity(FRAME_LEN_BYTES.saturating_add(payload.len()));
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
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
        unwrap_compressed_envelope(envelope)
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
    unwrap_compressed_envelope(envelope)
}

fn unwrap_compressed_envelope(envelope: Envelope) -> Result<Envelope, CodecError> {
    if envelope.kind != EnvelopeKind::Compressed {
        return Ok(envelope);
    }
    let compressed: CompressedEnvelopePayload = decode(&envelope.payload)?;
    if matches!(
        compressed.kind,
        EnvelopeKind::Chunk | EnvelopeKind::Compressed
    ) {
        return Err(CodecError::InvalidChunk(
            "compressed envelope cannot unwrap to chunk or compressed envelope".to_string(),
        ));
    }
    let algorithm =
        bmux_codec::compression::CompressionAlgorithm::from_wire_id(compressed.algorithm_wire_id)
            .map_err(CodecError::Deserialize)?;
    let expected_len =
        usize::try_from(compressed.uncompressed_len).map_err(|_| CodecError::PayloadTooLarge {
            actual: usize::MAX,
            max: MAX_FRAME_PAYLOAD_SIZE,
        })?;
    let payload =
        bmux_codec::compression::decompress_bytes(algorithm, &compressed.data, expected_len)
            .map_err(CodecError::Deserialize)?;
    Ok(Envelope::new(
        compressed.request_id,
        compressed.kind,
        payload,
    ))
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
        encode_request(request)?,
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
    if let Some(path) = endpoint.as_unix_socket() {
        prepare_unix_socket_path_for_bind(path)?;
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
                display_from_current_dir(path)
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

/// Environment variable carrying an encoded local IPC endpoint for child processes.
pub const BCODE_IPC_ENDPOINT_ENV: &str = "BCODE_IPC_ENDPOINT";

/// Environment variable identifying the build namespace that supplied an IPC endpoint override.
pub const BCODE_IPC_ENDPOINT_NAMESPACE_ENV: &str = "BCODE_IPC_ENDPOINT_NAMESPACE";

/// Serialize an IPC endpoint for process environment propagation.
///
/// # Errors
///
/// Returns an error when endpoint serialization fails.
pub fn endpoint_env_value(endpoint: &IpcEndpoint) -> Result<String, serde_json::Error> {
    serde_json::to_string(endpoint)
}

/// Return the environment pair used to propagate an exact IPC endpoint.
///
/// # Errors
///
/// Returns an error when endpoint serialization fails.
pub fn endpoint_env_pair(
    endpoint: &IpcEndpoint,
) -> Result<(&'static str, String), serde_json::Error> {
    Ok((BCODE_IPC_ENDPOINT_ENV, endpoint_env_value(endpoint)?))
}

/// Parse an IPC endpoint previously produced by [`endpoint_env_value`].
///
/// # Errors
///
/// Returns an error when the encoded endpoint is invalid.
pub fn endpoint_from_env_value(value: &str) -> Result<IpcEndpoint, serde_json::Error> {
    serde_json::from_str(value)
}

/// Return the daemon namespace for this build and IPC protocol version.
#[must_use]
pub fn daemon_namespace() -> String {
    format!("ipc-v{CURRENT_PROTOCOL_VERSION}-{BUILD_FINGERPRINT}")
}

/// Return the default local IPC endpoint.
#[must_use]
pub fn default_endpoint() -> IpcEndpoint {
    let endpoint_override_allowed = endpoint_override_allowed_for_current_process();
    if endpoint_override_allowed
        && let Ok(value) = env::var(BCODE_IPC_ENDPOINT_ENV)
        && let Ok(endpoint) = endpoint_from_env_value(&value)
    {
        return endpoint;
    }
    #[cfg(unix)]
    {
        IpcEndpoint::unix_socket(default_socket_path(endpoint_override_allowed))
    }
    #[cfg(windows)]
    {
        let user = env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
        IpcEndpoint::windows_named_pipe(format!(r"\\.\pipe\bcode-{user}-{}", daemon_namespace()))
    }
}

fn endpoint_override_allowed_for_current_process() -> bool {
    let current_namespace = daemon_namespace();
    let inherited_namespace = env::var(BCODE_IPC_ENDPOINT_NAMESPACE_ENV).ok();
    endpoint_override_allowed(
        inherited_namespace.as_deref(),
        env::var_os("BCODE_DAEMON_LOG").is_some(),
        &current_namespace,
    )
}

fn endpoint_override_allowed(
    inherited_namespace: Option<&str>,
    daemon_context: bool,
    current_namespace: &str,
) -> bool {
    inherited_namespace.map_or(!daemon_context, |namespace| namespace == current_namespace)
}

#[cfg(unix)]
fn default_socket_path(endpoint_override_allowed: bool) -> PathBuf {
    if endpoint_override_allowed && let Ok(path) = env::var("BCODE_SOCKET") {
        return PathBuf::from(path);
    }
    let user = env::var("USER").unwrap_or_else(|_| "user".to_string());
    env::temp_dir().join(format!("bcode-{user}-{}.sock", daemon_namespace()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, LegacyToolPresentationEvent,
        LegacyToolPresentationLevel, LegacyToolPresentationTarget, LegacyToolStatusPresentation,
        ModelTurnOutcome, SessionEventKind, SessionForkKind, SessionForkResult, SessionId,
        SessionSummary, SessionTraceEvent, ToolInvocationResult, ToolInvocationStreamEvent,
    };
    use bcode_skill_models::SkillActivationMode;
    use std::collections::BTreeSet;

    #[test]
    fn permission_batch_correlation_round_trips_and_defaults_when_absent() {
        let summary = PermissionSummary {
            permission_id: "perm-1".to_string(),
            session_id: SessionId::new(),
            tool_call_id: "call-2".to_string(),
            tool_name: "example.tool".to_string(),
            arguments_json: "{}".to_string(),
            batch: Some(PermissionBatchCorrelation {
                batch_id: "permission-batch-7".to_string(),
                call_index: 1,
                call_count: 3,
            }),
            agent_id: "build".to_string(),
            policy_source: None,
            policy_reason: None,
            can_remember_policy: false,
        };
        let value = serde_json::to_value(&summary).expect("permission summary should encode");
        let decoded: PermissionSummary =
            serde_json::from_value(value).expect("permission summary should decode");
        assert_eq!(decoded, summary);

        let mut value = serde_json::to_value(summary).expect("permission summary should encode");
        value
            .as_object_mut()
            .expect("permission summary JSON object")
            .remove("batch");
        let decoded: PermissionSummary =
            serde_json::from_value(value).expect("summary without batch should decode");
        assert_eq!(decoded.batch, None);
    }

    #[test]
    fn permission_batch_resolution_request_round_trips() {
        let request = Request::ResolvePermissionBatch {
            batch_id: "permission-batch-4".to_string(),
            approved: true,
        };
        let encoded = encode(&request).expect("batch resolution request should encode");
        let decoded: Request = decode(&encoded).expect("batch resolution request should decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn invocation_input_request_round_trips_with_opaque_payload() {
        let request = Request::InvocationInput {
            session_id: SessionId::new(),
            input: bcode_tool::ToolInvocationInput {
                invocation_id: "call-2".to_owned(),
                input_id: "resize-132x40".to_owned(),
                producer_id: "bcode.shell".to_owned(),
                schema: "bcode.shell.invocation-input".to_owned(),
                schema_version: 1,
                payload: serde_json::json!({
                    "unknown": {"nested": [1, 2, 3]},
                }),
            },
        };
        let encoded = encode_request(&request).expect("invocation input request should encode");
        let decoded = decode_request(&encoded).expect("invocation input request should decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn endpoint_override_rejects_stale_daemon_context() {
        assert!(!endpoint_override_allowed(None, true, "current"));
        assert!(!endpoint_override_allowed(
            Some("previous"),
            true,
            "current"
        ));
        assert!(endpoint_override_allowed(Some("current"), true, "current"));
        assert!(endpoint_override_allowed(None, false, "current"));
    }

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
    fn generic_turn_admission_request_and_response_round_trip() {
        let session_id: SessionId = "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("session id should parse");
        let admission = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: Some("operation-1".to_string()),
                display_label: Some("Background pass 1".to_string()),
            }),
            idempotency_key: Some("operation-1".to_string()),
            ..bcode_session_models::TurnAdmissionMetadata::default()
        };
        let request = Request::SubmitTurn {
            session_id,
            text: "continue".to_string(),
            admission,
        };
        let encoded = encode(&request).expect("request should encode");
        let decoded: Request = decode(&encoded).expect("request should decode");
        assert_eq!(decoded, request);

        let response = Response::Ok(ResponsePayload::TurnAdmission {
            admission: bcode_session_models::TurnAdmission::Accepted(
                bcode_session_models::TurnReceipt::from_accepted_event(session_id, 42),
            ),
        });
        let encoded = encode(&response).expect("response should encode");
        let decoded: Response = decode(&encoded).expect("response should decode");
        assert_eq!(decoded, response);
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
            expected_generation: Some(42),
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
    fn attached_projection_window_round_trips() {
        let session_id = SessionId::new();
        let response = Response::Ok(ResponsePayload::Attached {
            session_id,
            session: SessionSummary {
                id: session_id,
                name: None,
                explicit_name: None,
                derived_title: None,
                title_source: bcode_session_models::SessionTitleSource::EmptyDraft,
                client_count: 1,
                created_at_ms: 10,
                updated_at_ms: 20,
                working_directory: "/tmp/bcode-window-test".into(),
                import: None,
                fork: None,
            },
            history: Vec::new(),
            input_history: Vec::new(),
            import_warnings: Vec::new(),
            draft: None,
            runtime_selection: SessionRuntimeSelection::default(),
            projection_window: Some(bcode_session_models::ProjectionWindow {
                projection: bcode_session_models::SessionProjectionKind::Transcript,
                transcript_items: Vec::new(),
                source_range: None,
                has_older: true,
                has_newer: false,
                scanned_events: 4,
            }),
        });

        let encoded = encode(&response).expect("response should encode");
        let decoded: Response = decode(&encoded).expect("response should decode");

        assert_eq!(decoded, response);
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
            draft: Some("draft text".to_owned()),
            runtime_selection: SessionRuntimeSelection::default(),
            projection_window: None,
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
    fn session_model_list_with_pricing_round_trips() {
        let response = Response::Ok(ResponsePayload::SessionModelList {
            provider_plugin_id: Some("provider".to_string()),
            models: bcode_model::ModelList {
                models: vec![bcode_model::ModelInfo {
                    model_id: "model".to_string(),
                    display_name: "Model".to_string(),
                    is_default: true,
                    context_window: Some(128_000),
                    max_output_tokens: Some(16_000),
                    capabilities: BTreeSet::new(),
                    feature_support: bcode_model::ModelFeatureSupport::default(),
                    reasoning: None,
                    cache: bcode_model::ModelCacheInfo::default(),
                    metadata_source: None,
                    pricing: Some(bcode_model::ModelPricingInfo {
                        currency: "USD".to_string(),
                        unit: bcode_model::ModelPricingUnit::PerMillionTokens,
                        input: Some(bcode_model::ModelTokenPrice::from_micros(1_250_000)),
                        cached_input: Some(bcode_model::ModelTokenPrice::from_micros(125_000)),
                        cache_write_input: None,
                        output: Some(bcode_model::ModelTokenPrice::from_micros(10_000_000)),
                        source: bcode_model::ModelPricingSource::PatternMatch,
                    }),
                    visibility: bcode_model::ModelVisibility::Visible,
                }],
                catalog: bcode_model::ModelCatalogHints {
                    policy: bcode_model::ModelCatalogPolicy::ExpandSupported {
                        provider_id: "openai".to_string(),
                        target: bcode_model::ModelCatalogSupportHint {
                            provider: "openai".to_string(),
                            auth_mode: "api_key".to_string(),
                            api_surface: "responses".to_string(),
                            integration: None,
                        },
                        authority: bcode_model::ModelListAuthority::Partial,
                    },
                },
            },
        });

        let encoded = encode_response(&response).expect("response should encode");
        let decoded = decode_response(&encoded).expect("response should decode");

        assert_eq!(decoded, response);
    }

    #[test]
    fn response_envelope_uses_current_protocol_version() {
        let envelope = response_envelope(7, &Response::Ok(ResponsePayload::MessageSent))
            .expect("response envelope should encode");

        assert_eq!(envelope.version, ProtocolVersion::current());
        assert_eq!(ProtocolVersion::current().0, CURRENT_PROTOCOL_VERSION);
    }

    #[test]
    fn request_envelope_payload_decodes_with_request_codec() {
        let request = Request::Ping;
        let envelope = request_envelope(7, &request).expect("request envelope should encode");

        assert_eq!(envelope.kind, EnvelopeKind::Request);
        assert_eq!(
            decode_request(&envelope.payload).expect("request should decode"),
            request
        );
    }

    #[test]
    fn response_envelope_payload_decodes_with_response_codec() {
        let response = Response::Ok(ResponsePayload::Pong);
        let envelope = response_envelope(7, &response).expect("response envelope should encode");

        assert_eq!(envelope.kind, EnvelopeKind::Response);
        assert_eq!(
            decode_response(&envelope.payload).expect("response should decode"),
            response
        );
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
                expected: CURRENT_PROTOCOL_VERSION
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
    fn session_runtime_selection_round_trips_agent_identity() {
        let selection = SessionRuntimeSelection {
            agent_id: Some("build".to_owned()),
            provider_plugin_id: Some("provider".to_owned()),
            requested_model_id: Some("requested".to_owned()),
            effective_model_id: Some("effective".to_owned()),
            model_id: Some("requested".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            reasoning_summary: Some("detailed".to_owned()),
        };

        let encoded = encode(&selection).expect("selection should encode");
        let decoded: SessionRuntimeSelection = decode(&encoded).expect("selection should decode");

        assert_eq!(decoded, selection);
    }

    #[test]
    fn runtime_context_with_semantic_auth_round_trips() {
        let request = Request::Hello {
            client_name: "test".to_string(),
            daemon_namespace: daemon_namespace(),
            runtime_context: Some(ClientRuntimeContext {
                working_directory: Some(PathBuf::from("/tmp/client")),
                selected_provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                selected_model_id: Some("model".to_string()),
                requested_model_id: None,
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
                interaction_adapters: vec![
                    bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability {
                        producer_id: "example.plugin".to_owned(),
                        exchange_schema: "example.request".to_owned(),
                        min_schema_version: 2,
                        max_schema_version: 4,
                        platform_id: "tui".to_owned(),
                        priority: 50,
                        interaction_kind: "example.interaction".to_owned(),
                        tui_surface_kind: None,
                    },
                ],
                env_keys: BTreeMap::from([("OPENROUTER_API_KEY".to_string(), true)]),
            }),
        };

        let encoded = encode(&request).expect("request should encode");
        let decoded: Request = decode(&encoded).expect("request should decode");

        assert_eq!(decoded, request);
    }

    #[test]
    fn representative_non_history_responses_round_trip_through_typed_stable_ipc() {
        let responses = vec![
            Response::Ok(ResponsePayload::Hello {
                protocol_version: ProtocolVersion::current(),
                client_id: ClientId::new(),
                daemon: DaemonStatus {
                    namespace: daemon_namespace(),
                    protocol_version: u32::from(ProtocolVersion::current().0),
                    build_fingerprint: BUILD_FINGERPRINT.to_string(),
                    executable_digest: Some("digest".to_string()),
                    ..DaemonStatus::default()
                },
            }),
            Response::Ok(ResponsePayload::ServerStatus {
                status: ServerStatus {
                    connected_client_count: 1,
                    sessions: vec![test_session_summary("status")],
                    session_catalog_loaded: true,
                    session_catalog_status: SessionCatalogStatus::Loaded,
                    session_catalog_sources: Vec::new(),
                    session_catalog_revision: 7,
                    selected_provider_plugin_id: Some("provider".to_string()),
                    selected_model_id: Some("model".to_string()),
                    plugin_runtime: Vec::new(),
                    daemon: DaemonStatus {
                        namespace: daemon_namespace(),
                        protocol_version: u32::from(ProtocolVersion::current().0),
                        build_fingerprint: "test-build".to_string(),
                        executable_digest: Some("digest".to_string()),
                        storage_writer_epoch: Some(2),
                        session_event_schema_version: Some(38),
                        pid: Some(123),
                        instance_id: "instance".to_string(),
                        started_at_unix_ms: 456,
                    },
                    metrics: MetricsSnapshot::default(),
                    metrics_report: Box::default(),
                },
            }),
            Response::Ok(ResponsePayload::SessionList {
                sessions: vec![test_session_summary("listed")],
                catalog_status: SessionCatalogStatus::Loaded,
                catalog_sources: Vec::new(),
                catalog_revision: 7,
            }),
            Response::Ok(ResponsePayload::PluginServiceResult {
                response: PluginServiceResponse {
                    payload: b"payload".to_vec(),
                    error: None,
                },
            }),
            Response::Ok(ResponsePayload::WorktreeList(WorktreeListResponse {
                repo_root: "/tmp/repo".into(),
                current_worktree: "/tmp/repo".into(),
                worktrees: Vec::new(),
            })),
            Response::Ok(ResponsePayload::SessionCatalogRefreshed {
                sessions: vec![test_session_summary("refreshed")],
                catalog_status: SessionCatalogStatus::Loaded,
                catalog_sources: Vec::new(),
                catalog_revision: 8,
            }),
            Response::Ok(ResponsePayload::RuntimeWorkCancellationRequested { cancelled: true }),
            Response::Err(ErrorResponse::new("test_error", "something failed")),
        ];

        for response in responses {
            let encoded = encode_response(&response).expect("response should encode");
            let decoded = decode_response(&encoded).expect("response should decode");

            assert_eq!(decoded, response);
        }
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
        let decoded = decode_response(&received.payload).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[tokio::test]
    async fn oversized_event_envelope_round_trips_across_chunked_frames() {
        let session_id = SessionId::new();
        let event = Event::Session(SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 7,
            timestamp_ms: 1,
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
        let decoded = decode_event(&received.payload).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[allow(clippy::too_many_lines)]
    fn sample_session_event_kinds(session_id: SessionId) -> Vec<SessionEventKind> {
        vec![
            SessionEventKind::SessionCreated {
                name: Some("session".to_string()),
                working_directory: "/tmp/bcode".into(),
            },
            SessionEventKind::ClientAttached {
                client_id: ClientId::new(),
            },
            SessionEventKind::ClientDetached {
                client_id: ClientId::new(),
            },
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "hello".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
            SessionEventKind::AssistantDelta {
                text: "delta".to_string(),
            },
            SessionEventKind::AssistantMessage {
                text: "message".to_string(),
            },
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_string(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_string(),
                arguments_json: "{}".to_string(),
                working_directory: None,
                request_visual: None,
                legacy_request_presentation: None,
            },
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_string(),
                result: "done".to_string(),
                is_error: false,
                output: None,
                semantic_result: Some(ToolInvocationResult::Text {
                    text: "done".to_string(),
                }),
            },
            SessionEventKind::PermissionRequested {
                permission_id: "perm-1".to_string(),
                tool_call_id: "call-1".to_string(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_string(),
                arguments_json: "{}".to_string(),
                legacy_request_presentation: None,
                batch: None,
                policy_source: None,
                policy_reason: None,
            },
            SessionEventKind::PermissionResolved {
                permission_id: "perm-1".to_string(),
                approved: true,
            },
            SessionEventKind::ModelChanged {
                provider: "provider".to_string(),
                model: "model".to_string(),
            },
            SessionEventKind::SystemMessage {
                text: "system".to_string(),
            },
            SessionEventKind::AgentChanged {
                agent_id: "agent".to_string(),
            },
            SessionEventKind::ModelTurnStarted {
                turn_id: "turn-1".to_string(),
            },
            SessionEventKind::ModelTurnFinished {
                turn_id: "turn-1".to_string(),
                outcome: ModelTurnOutcome::Completed,
                message: Some("ok".to_string()),
            },
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_string(),
                usage: bcode_session_models::SessionTokenUsage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    total_tokens: Some(3),
                    cached_input_tokens: None,
                    cache_write_input_tokens: None,
                    reasoning_tokens: None,
                },
            },
            SessionEventKind::ContextCompacted {
                summary: "summary".to_string(),
                compacted_through_sequence: 10,
            },
            SessionEventKind::SessionRenamed {
                name: Some("renamed".to_string()),
            },
            SessionEventKind::TraceEvent {
                trace: Box::new(SessionTraceEvent {
                    timestamp_ms: 1,
                    turn_id: Some("turn-1".to_string()),
                    phase: bcode_session_models::SessionTracePhase::ModelRequestBuilt,
                    payload: bcode_session_models::SessionTracePayload::ModelRequestBuilt {
                        provider: "provider".to_string(),
                        model: "model".to_string(),
                        agent_id: "agent".to_string(),
                        message_count: 1,
                        tool_count: 0,
                        system_prompt_chars: 10,
                        prompt_cache_mode: "off".to_string(),
                        conversation_reuse_mode: "none".to_string(),
                        uses_previous_provider_response: false,
                        metadata: BTreeMap::new(),
                        request: None,
                    },
                }),
            },
            SessionEventKind::SkillInvoked {
                skill_id: SkillId::new("skill"),
                arguments: "{}".to_string(),
                source: None,
                invoked_at_ms: 1,
            },
            SessionEventKind::SkillSuggested {
                skill_id: SkillId::new("skill"),
                reason: Some("reason".to_string()),
                suggested_at_ms: 1,
            },
            SessionEventKind::SkillActivated {
                skill_id: SkillId::new("skill"),
                source: None,
                mode: SkillActivationMode::Explicit,
                activated_at_ms: 1,
            },
            SessionEventKind::SkillDeactivated {
                skill_id: SkillId::new("skill"),
                deactivated_at_ms: 1,
            },
            SessionEventKind::SkillContextLoaded {
                skill_id: SkillId::new("skill"),
                bytes_loaded: 42,
                truncated: false,
                loaded_at_ms: 1,
                source: None,
                preview: None,
            },
            SessionEventKind::SkillInvocationFailed {
                skill_id: SkillId::new("skill"),
                error: "nope".to_string(),
                failed_at_ms: 1,
            },
            SessionEventKind::AssistantReasoningDelta {
                text: "thinking".to_string(),
            },
            SessionEventKind::AssistantReasoningMessage {
                text: "thought".to_string(),
            },
            SessionEventKind::RuntimeWorkStarted {
                work_id: WorkId::new("work-1"),
                kind: RuntimeWorkKind::Tool,
                label: "work".to_string(),
                tool_call_id: Some("call-1".to_string()),
                plugin_id: Some("plugin".to_string()),
                service_interface: Some("service".to_string()),
                operation: Some("op".to_string()),
                parent_work_id: None,
                started_at_ms: Some(1),
                cancellable: true,
            },
            SessionEventKind::RuntimeWorkCancelRequested {
                work_id: WorkId::new("work-1"),
                requested_at_ms: Some(2),
                client_id: Some(ClientId::new()),
            },
            SessionEventKind::RuntimeWorkFinished {
                work_id: WorkId::new("work-1"),
                status: RuntimeWorkStatus::Completed,
                finished_at_ms: Some(3),
                message: Some("done".to_string()),
            },
            SessionEventKind::RuntimeWorkProgress {
                work_id: WorkId::new("work-1"),
                message: "progress".to_string(),
                progress_at_ms: Some(2),
                completed_units: Some(1),
                total_units: Some(2),
            },
            SessionEventKind::ModelTurnCancelRequested {
                turn_id: "turn-1".to_string(),
                requested_at_ms: Some(2),
                client_id: Some(ClientId::new()),
            },
            SessionEventKind::ToolContribution {
                event: bcode_session_models::ToolContributionEvent {
                    invocation_id: "call-1".to_string(),
                    contribution_id: "surface".to_string(),
                    sequence: 4,
                    producer_id: "future.producer".to_string(),
                    schema: "future.unknown/schema".to_string(),
                    schema_version: 77,
                    operation: bcode_session_models::ToolContributionOperation::Append,
                    persistence: bcode_session_models::ToolContributionPersistence::Durable,
                    artifact: None,
                    payload: serde_json::json!({"opaque": [1, {"future": true}]}),
                },
            },
            SessionEventKind::ToolExchangeRequested {
                request: bcode_session_models::ToolExchangeRequest {
                    invocation_id: "call-1".to_string(),
                    exchange_id: "question".to_string(),
                    producer_id: "future.producer".to_string(),
                    schema: "future.question/schema".to_string(),
                    schema_version: 77,
                    payload: serde_json::json!({"opaque": "request"}),
                    response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                },
            },
            SessionEventKind::ToolExchangeResolved {
                event: bcode_session_models::ToolExchangeResolutionEvent {
                    invocation_id: "call-1".to_string(),
                    exchange_id: "question".to_string(),
                    resolution: bcode_session_models::ToolExchangeResolution::Responded {
                        payload: serde_json::json!({"opaque": "response"}),
                    },
                },
            },
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "shell.run".to_string(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(80),
                    rows: Some(24),
                    started_at_ms: Some(1),
                },
            },
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-1".to_string(),
                    sequence: 1,
                    presentation: LegacyToolPresentationEvent::Status(
                        LegacyToolStatusPresentation {
                            target: LegacyToolPresentationTarget::Activity,
                            text: "running".to_string(),
                            level: LegacyToolPresentationLevel::Info,
                        },
                    ),
                },
            },
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory: "/tmp/old".into(),
                new_working_directory: "/tmp/new".into(),
            },
            SessionEventKind::SessionImported {
                source_id: "source".to_string(),
                source_display_name: "Source".to_string(),
                external_session_id: "external".to_string(),
                imported_at_ms: 1,
            },
            SessionEventKind::SessionForked {
                source_session_id: session_id,
                source_title: Some("source".to_string()),
                source_cutoff_sequence: Some(1),
                source_prompt_sequence: Some(1),
                forked_at_ms: 1,
                kind: SessionForkKind::Fork,
            },
        ]
    }

    #[test]
    fn direct_domain_session_events_round_trip_through_typed_stable() {
        let session_id = SessionId::new();
        for (index, kind) in sample_session_event_kinds(session_id)
            .into_iter()
            .enumerate()
        {
            let event = Event::Session(SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: u64::try_from(index).expect("index should fit u64"),
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind,
            });

            let encoded = encode_typed_stable(&event).expect("event should encode directly");
            let decoded: Event = decode_typed_stable(&encoded).unwrap_or_else(|error| {
                panic!("event {index} {event:?} should decode directly: {error}")
            });

            assert_eq!(decoded, event);
        }
    }

    #[tokio::test]
    async fn all_session_event_kinds_round_trip_across_ipc_frames() {
        let session_id = SessionId::new();
        for (index, kind) in sample_session_event_kinds(session_id)
            .into_iter()
            .enumerate()
        {
            let event = Event::Session(SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: u64::try_from(index).expect("index should fit u64"),
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind,
            });
            let envelope = event_envelope(&event).expect("event should encode");

            let received = round_trip_envelope(envelope).await;

            let decoded = decode_event(&received.payload).expect("event should decode");
            assert_eq!(decoded, event);
        }
    }

    #[tokio::test]
    async fn semantic_tool_result_events_round_trip_across_ipc_frames() {
        let session_id = SessionId::new();
        for semantic_result in semantic_tool_results() {
            let event = Event::Session(semantic_tool_result_event(session_id, semantic_result));
            let envelope = event_envelope(&event).expect("event should encode");

            let received = round_trip_envelope(envelope).await;

            let decoded = decode_event(&received.payload).expect("event should decode");
            assert_eq!(decoded, event);
        }
    }

    #[tokio::test]
    async fn semantic_tool_result_response_histories_round_trip_across_ipc_frames() {
        let session_id = SessionId::new();
        let session = test_session_summary("semantic history");

        for semantic_result in semantic_tool_results() {
            let event = semantic_tool_result_event(session_id, semantic_result);
            for response in [
                Response::Ok(ResponsePayload::Attached {
                    session_id,
                    session: session.clone(),
                    history: vec![event.clone()],
                    input_history: Vec::new(),
                    import_warnings: Vec::new(),
                    draft: Some("draft text".to_owned()),
                    runtime_selection: SessionRuntimeSelection::default(),
                    projection_window: None,
                }),
                Response::Ok(ResponsePayload::SessionHistory {
                    session_id,
                    history: vec![event.clone()],
                }),
                Response::Ok(ResponsePayload::SessionHistoryPage {
                    page: bcode_session_models::SessionHistoryPage {
                        session_id,
                        events: vec![event.clone()],
                        compatibility_issues: vec![
                            bcode_session_models::SessionEventCompatibilityIssue {
                                sequence: event.sequence,
                                event_kind: "future_event_kind".to_owned(),
                                schema_version: event.schema_version,
                                compatibility: bcode_session_models::SessionEventCompatibilityKind::UnknownEventKind,
                                remediation: "upgrade Bcode".to_owned(),
                            },
                        ],
                        next_cursor: None,
                        has_more: false,
                    },
                }),
                Response::Ok(ResponsePayload::RuntimeWorkHistory {
                    events: vec![event.clone()],
                }),
            ] {
                let envelope = response_envelope(42, &response).expect("response should encode");

                let received = round_trip_envelope(envelope).await;

                let decoded = decode_response(&received.payload).expect("response should decode");
                assert_eq!(decoded, response);
            }
        }
    }

    #[tokio::test]
    async fn presentation_event_response_histories_round_trip_across_ipc_frames() {
        let session_id = SessionId::new();
        let session = test_session_summary("presentation history");
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-1".to_string(),
                    sequence: 1,
                    presentation: LegacyToolPresentationEvent::Status(
                        LegacyToolStatusPresentation {
                            target: LegacyToolPresentationTarget::Result,
                            text: "completed".to_string(),
                            level: LegacyToolPresentationLevel::Success,
                        },
                    ),
                },
            },
        };

        for response in [
            Response::Ok(ResponsePayload::Attached {
                session_id,
                session: session.clone(),
                history: vec![event.clone()],
                input_history: Vec::new(),
                import_warnings: Vec::new(),
                draft: None,
                runtime_selection: SessionRuntimeSelection::default(),
                projection_window: None,
            }),
            Response::Ok(ResponsePayload::SessionHistory {
                session_id,
                history: vec![event.clone()],
            }),
            Response::Ok(ResponsePayload::SessionHistoryPage {
                page: bcode_session_models::SessionHistoryPage {
                    session_id,
                    events: vec![event.clone()],
                    compatibility_issues: Vec::new(),
                    next_cursor: None,
                    has_more: false,
                },
            }),
            Response::Ok(ResponsePayload::RuntimeWorkHistory {
                events: vec![event.clone()],
            }),
        ] {
            let envelope = response_envelope(42, &response).expect("response should encode");

            let received = round_trip_envelope(envelope).await;

            let decoded = decode_response(&received.payload).expect("response should decode");
            assert_eq!(decoded, response);
        }
    }

    fn semantic_tool_result_event(
        session_id: SessionId,
        semantic_result: ToolInvocationResult,
    ) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 77,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_string(),
                result: "tool result".to_string(),
                is_error: false,
                output: None,
                semantic_result: Some(semantic_result),
            },
        }
    }

    fn semantic_tool_results() -> Vec<ToolInvocationResult> {
        vec![
            ToolInvocationResult::Artifact {
                artifact: Box::new(bcode_session_models::ToolArtifact {
                    artifact_id: "artifact-1".to_string(),
                    producer_plugin_id: "bcode.test".to_string(),
                    schema: "bcode.test.artifact".to_string(),
                    schema_version: 1,
                    tool_call_id: Some("call-1".to_string()),
                    title: Some("Test artifact".to_string()),
                    metadata: serde_json::json!({"ok": true}),
                    refs: vec![bcode_session_models::ToolArtifactRef {
                        key: "data".to_string(),
                        content_type: Some("application/json".to_string()),
                        storage_uri: None,
                        byte_len: Some(11),
                        metadata: None,
                    }],
                }),
            },
            ToolInvocationResult::Text {
                text: "plain text".to_string(),
            },
            ToolInvocationResult::Json {
                value: r#"{"ok":true}"#.to_string(),
            },
        ]
    }

    #[test]
    fn ralph_runner_requests_round_trip() {
        let requests = [
            Request::RunRalphLoop(RalphRunRequest {
                repo_root: PathBuf::from("/repo"),
                loop_state_dir: Some(PathBuf::from("/repo/.bcode/ralph/state")),
                max_iterations: Some(5),
                no_progress_limit: Some(2),
                require_approval: true,
            }),
            Request::ApproveRalphRun(RalphApproveRequest {
                repo_root: PathBuf::from("/repo"),
                loop_state_dir: None,
                run_id: Some("run-1".to_owned()),
            }),
            Request::CancelRalphLoop(RalphCancelRequest {
                repo_root: PathBuf::from("/repo"),
                run_id: Some("run-1".to_owned()),
                loop_state_dir: None,
            }),
            Request::RalphRunStatus(RalphRunStatusRequest {
                repo_root: PathBuf::from("/repo"),
                loop_state_dir: None,
            }),
            Request::ListRalphRuns(Box::new(RalphListRunsRequest {
                repo_root: PathBuf::from("/repo"),
                loop_state_dir: None,
            })),
            Request::ListRalphIterations(Box::new(RalphListIterationsRequest {
                repo_root: PathBuf::from("/repo"),
                loop_state_dir: None,
                run_id: Some("run-1".to_owned()),
            })),
            Request::ResumeRalphRun(RalphResumeRequest {
                repo_root: PathBuf::from("/repo"),
                loop_state_dir: None,
                interrupted_run_id: Some("run-1".to_owned()),
            }),
        ];
        for request in requests {
            let encoded = encode(&request).expect("request should encode");
            let decoded: Request = decode(&encoded).expect("request should decode");
            assert_eq!(decoded, request);
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
