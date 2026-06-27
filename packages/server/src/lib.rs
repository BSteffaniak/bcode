#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Local Bcode daemon runtime.

mod model_ignores;
mod runtime_work;
pub mod session_catalog;
mod session_import;

use bcode_agent_profile::{
    AGENT_PROFILE_INTERFACE_ID, AgentContextRequest, AgentContextResponse, AgentDecision,
    AgentInfo, AgentList, EvaluateToolCallRequest, EvaluateToolCallResponse, OP_AGENT_CONTEXT,
    OP_EVALUATE_TOOL_CALL, OP_LIST_AGENTS, OP_POLICY_STATUS, PolicyStatusResponse,
};
use bcode_ipc::{
    ClientRuntimeContext, CodecError, DaemonStatus, EnvelopeKind, ErrorResponse, Event,
    IpcEndpoint, LocalIpcListener, LocalIpcStream, PermissionSummary, PluginContributions,
    PluginServiceError, PluginServiceResponse, PluginServiceSummary, RalphApproveRequest,
    RalphCancelRequest, RalphCancelResponse, RalphIterationSummary, RalphLifecycleRequest,
    RalphListIterationsRequest, RalphListIterationsResponse, RalphListRunsRequest,
    RalphListRunsResponse, RalphResumeRequest, RalphResumeResponse, RalphRunRequest,
    RalphRunResponse, RalphRunStatusRequest, RalphRunStatusResponse, RalphRunSummary,
    RalphStatusRequest, RalphStatusResponse, RalphStatusSummary, RalphValidationSummary, Request,
    Response, ResponsePayload, ServerStatus, ServerStopMode, SessionCatalogSourceStatus,
    SessionCatalogStatus, WorktreeCreateRequest, WorktreeListRequest, WorktreeRemoveRequest,
    decode_request, event_envelope, recv_envelope, response_envelope, send_envelope,
};
use bcode_metrics::{MetricLabels, MetricsRegistry};
use bcode_model::{
    CancelTurnRequest, ContentBlock, FinishTurnRequest, ImageMetadata as ModelImageMetadata,
    ImageRefContent, MODEL_PROVIDER_INTERFACE_ID, MessageRole, ModelList, ModelMessage,
    ModelParameters, ModelTurnRequest, NativeWebSearchRequest, NativeWebSearchResponse,
    OP_CANCEL_TURN, OP_FINISH_TURN, OP_MODELS, OP_NATIVE_WEB_SEARCH, OP_POLL_TURN_EVENTS,
    OP_START_TURN, PollTurnEventsRequest, PollTurnEventsResponse, ProviderTurnEvent,
    ReasoningEffort, StartTurnResponse, TokenUsage,
};
use bcode_plugin::{PluginInvocationScope, StreamingServiceInvocationEvent};
use bcode_session::{CatalogLoadStatus, SessionManager, lease::SessionLeaseOwnerContext};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, LiveFileEditPreview, LiveQueryPreview,
    LiveShellCommandPreview, LiveToolArgumentPreview, ModelTurnOutcome, ProviderStreamEvent,
    ProviderToolCallProgress, RuntimeWorkId, RuntimeWorkKind, RuntimeWorkStatus, SessionEventKind,
    SessionId, SessionLiveEventKind, SessionTokenUsage, SessionTraceEvent, SessionTracePayload,
    SessionTracePhase, ShellRunResult, ToolCardPresentation, ToolInvocationResult,
    ToolInvocationStreamEvent, ToolOutputStream as SessionToolOutputStream, ToolPresentationEvent,
    ToolPresentationField, ToolPresentationFieldKind, ToolPresentationFieldValue,
    ToolPresentationLevel, ToolPresentationSection, ToolPresentationTarget,
    ToolProgressPresentation, ToolRequestPresentationMetadata, TraceBlobRef, TraceRedaction,
};
use bcode_settings::SettingsStore;
use bcode_skill::{
    SkillPromptCatalogMode, SkillPromptCatalogOptions, SkillRegistry, SkillRegistryOptions,
    SkillSourceRoot, evaluate_skill_tool_call, format_skill_catalog_for_prompt,
    resolve_skill_permission_policy,
};
use bcode_skill_models::{
    SkillActivationMode, SkillContextResponse, SkillId, SkillList, SkillSource, SkillSourceKind,
    SkillToolDecision, SkillToolDecisionEntry, SkillToolDecisionKey, SkillToolDecisionScope,
    SkillToolPolicyOutcome, SkillToolPolicyRequest,
};
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, ShellRunResult as ServiceShellRunResult,
    TOOL_SERVICE_INTERFACE_ID, ToolDefinition as ServiceToolDefinition, ToolInvocationRequest,
    ToolInvocationResponse, ToolInvocationResult as ServiceToolInvocationResult,
    ToolInvocationStreamEvent as ServiceToolInvocationStreamEvent, ToolList, ToolOutputStream,
    ToolPresentationEvent as ServiceToolPresentationEvent,
    ToolPresentationFieldKind as ServiceToolPresentationFieldKind,
    ToolPresentationLevel as ServiceToolPresentationLevel,
    ToolPresentationSection as ServiceToolPresentationSection,
    ToolPresentationTarget as ServiceToolPresentationTarget,
    ToolRequestPresentationMetadata as ServiceToolRequestPresentationMetadata, ToolResultContent,
};
use runtime_work::{CancellationHandle, RuntimeWorkManager, RuntimeWorkSpec};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{WriteHalf, split};
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

const CLIENT_EVENT_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const CATALOG_EVENT_BROADCAST_BATCH_SIZE: usize = 16;

/// Shared client writer.
type SharedWriter = Arc<Mutex<WriteHalf<LocalIpcStream>>>;

/// Plugin event topic published for every session event appended by the server.
pub const SESSION_EVENT_PLUGIN_TOPIC: &str = "bcode.session.event";

/// Errors returned by the local server.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("IPC transport error: {0}")]
    Transport(#[from] bcode_ipc::IpcTransportError),
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("IPC codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("plugin error: {0}")]
    Plugin(#[from] bcode_plugin::PluginLoadError),
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("session error: {0}")]
    Session(#[from] bcode_session::SessionError),
    #[error("session event store error: {0}")]
    SessionStore(#[from] bcode_session::SessionStoreError),
    #[error("session database error: {0}")]
    SessionDb(#[from] bcode_session::db::SessionDbError),
    /// Registry I/O error: {0}
    #[error("daemon lifecycle error: {0}")]
    DaemonLifecycle(#[from] bcode_daemon_lifecycle::DaemonLifecycleError),
    #[error("blocking task join error: {0}")]
    BlockingTask(#[from] tokio::task::JoinError),
    #[error("model turn completion channel closed: {0}")]
    ModelTurnCompletionClosed(#[from] oneshot::error::RecvError),
}

#[derive(Debug)]
pub struct ServerState {
    pub sessions: SessionManager,
    pub session_catalog: Arc<session_catalog::SessionCatalog>,
    pub plugins: bcode_plugin::PluginRuntimeHost,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    selected_provider_context: bcode_model::ProviderRequestContext,
    prompt_cache_mode: bcode_model::PromptCacheMode,
    conversation_reuse_mode: bcode_model::ConversationReuseMode,
    selected_reasoning: bcode_config::ReasoningConfig,
    selected_reasoning_capabilities: Option<bcode_model::ModelReasoningInfo>,
    provider_state: Mutex<ProviderStateStore>,
    observability: bcode_config::ObservabilityConfig,
    trace_store: TraceStore,
    max_tool_rounds: Option<u32>,
    tool_output_context_chars: usize,
    model_streaming: bcode_config::StreamingConfig,
    model_retry: bcode_config::ModelRetryConfig,
    auto_compaction: bcode_config::CompactionConfig,
    system_prompt: bcode_config::SystemPromptConfig,
    skills: Option<SkillRegistry>,
    skill_context_bytes: usize,
    skill_prompt_options: SkillPromptCatalogOptions,
    active_skills: Mutex<BTreeMap<SessionId, BTreeSet<SkillId>>>,
    turn_skills: Mutex<BTreeMap<(SessionId, u64), SkillTurnInvocation>>,
    session_runtimes: Mutex<BTreeMap<SessionId, SessionRuntimeHandle>>,
    runtime_work: RuntimeWorkManager,
    ralph_store: bcode_ralph::RalphStateStore,
    active_ralph_runs: Mutex<BTreeMap<PathBuf, JoinHandle<()>>>,
    session_model_selections: Mutex<BTreeMap<SessionId, SessionModelSelection>>,
    session_agent_selections: Mutex<BTreeMap<SessionId, String>>,
    pending_permissions: Mutex<BTreeMap<String, PendingPermission>>,
    next_permission_id: Mutex<u64>,
    clients: Mutex<BTreeSet<ClientId>>,
    client_runtime_contexts: Mutex<BTreeMap<ClientId, ClientRuntimeContext>>,
    client_session_namespaces: Mutex<BTreeMap<ClientId, String>>,
    active_session_namespaces: Mutex<BTreeMap<SessionId, String>>,
    message_accepted_clients: Mutex<BTreeSet<ClientId>>,
    attached_client_sessions: Mutex<BTreeMap<ClientId, SessionId>>,
    client_forwarders: Mutex<BTreeMap<ClientId, Vec<JoinHandle<()>>>>,
    event_clients: Mutex<BTreeMap<ClientId, CatalogEventSubscription>>,
    catalog_events_started: std::sync::atomic::AtomicBool,
    idle_shutdown_started: std::sync::atomic::AtomicBool,
    daemon_status: DaemonStatus,
    daemon_record_path: Option<PathBuf>,
    metrics: MetricsRegistry,
    shutdown: broadcast::Sender<()>,
}

#[derive(Debug)]
struct CatalogEventSubscription {
    sink: ClientEventSink,
    last_sent_revision: Option<u64>,
}

impl CatalogEventSubscription {
    const fn new(sink: ClientEventSink) -> Self {
        Self {
            sink,
            last_sent_revision: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ClientEventSink {
    client_id: ClientId,
    writer: SharedWriter,
}

impl ClientEventSink {
    const fn new(client_id: ClientId, writer: SharedWriter) -> Self {
        Self { client_id, writer }
    }

    const fn client_id(&self) -> ClientId {
        self.client_id
    }

    async fn send(&self, event: Event) -> Result<(), CodecError> {
        let event_kind = client_event_kind(&event);
        let envelope = event_envelope(&event)?;
        let started_at = Instant::now();
        let result = {
            let mut writer = self.writer.lock().await;
            tokio::time::timeout(
                CLIENT_EVENT_SEND_TIMEOUT,
                send_envelope(&mut *writer, &envelope),
            )
            .await
            .unwrap_or_else(|_| {
                Err(CodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("timed out sending event to client {}", self.client_id),
                )))
            })
        };
        let elapsed = started_at.elapsed();
        if elapsed >= CLIENT_EVENT_SEND_TIMEOUT {
            tracing::warn!(
                target: "bcode_server::client_events",
                client_id = %self.client_id,
                event_kind,
                elapsed_ms = elapsed.as_millis(),
                "client event send reached timeout threshold"
            );
        } else {
            tracing::debug!(
                target: "bcode_server::client_events",
                client_id = %self.client_id,
                event_kind,
                elapsed_ms = elapsed.as_millis(),
                "client event sent"
            );
        }
        result
    }
}

const fn client_event_kind(event: &Event) -> &'static str {
    match event {
        Event::Session(_) => "session",
        Event::SessionLive(_) => "session_live",
        Event::RuntimeWork(_) => "runtime_work",
        Event::SessionCatalogUpdated { .. } => "session_catalog_updated",
    }
}

#[derive(Debug, Clone)]
struct SessionRuntimeHandle {
    followup_commands: mpsc::Sender<FollowupCommand>,
    steering_commands: mpsc::Sender<SteeringCommand>,
    cancel_commands: mpsc::Sender<CancelCommand>,
    queued_followups: Arc<AtomicUsize>,
    phase: Arc<Mutex<SessionRuntimePhase>>,
    current_turn: Arc<Mutex<Option<RuntimeCurrentTurn>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SessionRuntimePhase {
    #[default]
    Idle,
    AppendingUser,
    PreparingModelRequest,
    ProviderActive,
    Compacting,
    FinishingTurn,
}

impl SessionRuntimePhase {
    const fn accepts_inline_steering(self) -> bool {
        matches!(self, Self::AppendingUser | Self::PreparingModelRequest)
    }

    const fn has_active_work(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SteeringWindow {
    Idle,
    BeforeNextProviderRequest,
    ProviderInFlight,
    Finishing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MessageQueueStatus {
    queued: bool,
    queue_position: Option<u32>,
    disposition: bcode_ipc::MessageAcceptanceDisposition,
}

type ProviderCallFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

enum ProviderCallWait<T> {
    Completed(T),
    Cancelled,
}

struct RuntimeCommandContext<'a> {
    followup_commands: &'a mut mpsc::Receiver<FollowupCommand>,
    steering_commands: &'a mut mpsc::Receiver<SteeringCommand>,
    cancel_commands: &'a mut mpsc::Receiver<CancelCommand>,
    queued_followups: &'a AtomicUsize,
    current_turn: Arc<Mutex<Option<RuntimeCurrentTurn>>>,
}

impl<'a> RuntimeCommandContext<'a> {
    const fn new(
        followup_commands: &'a mut mpsc::Receiver<FollowupCommand>,
        steering_commands: &'a mut mpsc::Receiver<SteeringCommand>,
        cancel_commands: &'a mut mpsc::Receiver<CancelCommand>,
        queued_followups: &'a AtomicUsize,
        current_turn: Arc<Mutex<Option<RuntimeCurrentTurn>>>,
    ) -> Self {
        Self {
            followup_commands,
            steering_commands,
            cancel_commands,
            queued_followups,
            current_turn,
        }
    }
}

#[derive(Debug)]
enum FollowupCommand {
    UserMessage {
        client_id: ClientId,
        runtime_context: Option<ClientRuntimeContext>,
        text: String,
        placement: bcode_ipc::PromptPlacement,
        completion: Option<oneshot::Sender<ModelTurnCompletion>>,
    },
    ContinueFromUserEvent {
        client_id: ClientId,
        runtime_context: Option<ClientRuntimeContext>,
        user_event: Box<bcode_session_models::SessionEvent>,
        completion: Option<oneshot::Sender<ModelTurnCompletion>>,
    },
    SkillInvocation {
        client_id: ClientId,
        runtime_context: Option<ClientRuntimeContext>,
        skill_id: SkillId,
        arguments: String,
        source: Option<SkillSource>,
        display_text: String,
    },
    CompactSession {
        selection: SessionModelSelection,
        response: oneshot::Sender<Result<String, CompactionError>>,
    },
}

#[derive(Debug)]
struct SteeringCommand {
    client_id: ClientId,
    text: String,
    completion: Option<oneshot::Sender<ModelTurnCompletion>>,
}

#[derive(Debug)]
struct CancelCommand {
    clear_queue: bool,
    requested_by: Option<ClientId>,
    response: oneshot::Sender<bool>,
}

#[derive(Debug)]
struct SessionTurnPermit {
    session_id: SessionId,
    turn_entries: u64,
    _private: (),
}

impl SessionTurnPermit {
    #[must_use]
    const fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            turn_entries: 0,
            _private: (),
        }
    }

    #[must_use]
    const fn session_id(&self) -> SessionId {
        self.session_id
    }

    const fn enter_turn(&mut self) -> SessionId {
        self.turn_entries = self.turn_entries.saturating_add(1);
        self.session_id
    }
}

#[derive(Debug, Default)]
struct TurnCancelState {
    cancelled: std::sync::atomic::AtomicBool,
    notify: Notify,
}

impl TurnCancelState {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

#[derive(Debug, Clone)]
struct ActiveModelTurn {
    provider_plugin_id: Option<String>,
    provider_turn_id: String,
    reuse_key: Option<String>,
    request_message_count: usize,
}

#[derive(Debug, Clone)]
struct RuntimeCurrentTurn {
    client_id: ClientId,
    turn_id: String,
    cancel_state: Arc<TurnCancelState>,
    model: Option<ActiveModelTurn>,
}

impl RuntimeCurrentTurn {
    fn plugin_scope_for_model(&self, session_id: SessionId) -> PluginInvocationScope {
        PluginInvocationScope::session(session_id.to_string())
            .with_client_id(self.client_id.to_string())
            .with_turn_id(self.turn_id.clone())
            .with_work_id(format!("model_{}", self.turn_id))
    }

    fn plugin_scope_for_tool_call(
        &self,
        session_id: SessionId,
        tool_call_id: &str,
    ) -> PluginInvocationScope {
        PluginInvocationScope::session(session_id.to_string())
            .with_client_id(self.client_id.to_string())
            .with_turn_id(self.turn_id.clone())
            .with_work_id(format!("tool_{tool_call_id}"))
    }
}

#[derive(Debug, Clone)]
struct SkillTurnInvocation {
    skill_id: SkillId,
    arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct ProviderStateKey {
    session_id: SessionId,
    provider_plugin_id: String,
    model_id: String,
    stable_prompt_hash: String,
    tools_hash: String,
    parameters_hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProviderContinuationState {
    provider_response_id: String,
    reusable_message_count: usize,
    updated_sequence: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProviderTelemetryState {
    input: Option<u32>,
    cached: Option<u32>,
    cache_write: Option<u32>,
    uncached: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProviderStateRecord {
    #[serde(default)]
    continuation: Option<ProviderContinuationState>,
    #[serde(default)]
    telemetry: ProviderTelemetryState,
    #[serde(default)]
    provider_state: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct ProviderStateStore {
    path: PathBuf,
    records: BTreeMap<String, ProviderStateRecord>,
}

impl ProviderStateStore {
    fn load(path: PathBuf) -> Self {
        let records = fs::read_to_string(&path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default();
        Self { path, records }
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            eprintln!("failed to create provider state directory: {error}");
            return;
        }
        let Ok(contents) = serde_json::to_string_pretty(&self.records) else {
            eprintln!("failed to encode provider state");
            return;
        };
        if let Err(error) = fs::write(&self.path, contents) {
            eprintln!("failed to persist provider state: {error}");
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SessionModelSelection {
    provider_plugin_id: Option<String>,
    model_id: Option<String>,
    thinking_level: Option<ReasoningEffort>,
    reasoning_effort: Option<String>,
    reasoning_summary: Option<String>,
    reasoning_capabilities: Option<bcode_model::ModelReasoningInfo>,
    provider_context: bcode_model::ProviderRequestContext,
}

#[derive(Debug, Clone, Copy, Default)]
struct ModelMetadataOverride {
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
}

fn model_metadata_override(
    context: &bcode_model::ProviderRequestContext,
    model_id: &str,
) -> ModelMetadataOverride {
    let context_window = context
        .settings
        .get(&format!("model_metadata.{model_id}.context_window"))
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0);
    let max_output_tokens = context
        .settings
        .get(&format!("model_metadata.{model_id}.max_output_tokens"))
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0);
    ModelMetadataOverride {
        context_window,
        max_output_tokens,
    }
}

#[derive(Debug, Clone, Default)]
struct ModelReasoningOverride {
    effort_values: Option<Vec<String>>,
    default_effort: Option<String>,
    visible_summary_supported: Option<bool>,
    summary_values: Option<Vec<String>>,
    default_summary: Option<String>,
    raw_reasoning_supported: Option<bool>,
}

impl ModelReasoningOverride {
    const fn is_empty(&self) -> bool {
        self.effort_values.is_none()
            && self.default_effort.is_none()
            && self.visible_summary_supported.is_none()
            && self.summary_values.is_none()
            && self.default_summary.is_none()
            && self.raw_reasoning_supported.is_none()
    }
}

fn model_reasoning_override(
    context: &bcode_model::ProviderRequestContext,
    model_id: &str,
) -> Option<ModelReasoningOverride> {
    let prefix = format!("model_metadata.{model_id}.reasoning.");
    let override_ = ModelReasoningOverride {
        effort_values: context
            .settings
            .get(&format!("{prefix}effort_values"))
            .map(|value| split_config_values(value)),
        summary_values: context
            .settings
            .get(&format!("{prefix}summary_values"))
            .map(|value| split_config_values(value)),
        default_effort: non_empty_setting(context, &format!("{prefix}default_effort")),
        default_summary: non_empty_setting(context, &format!("{prefix}default_summary")),
        visible_summary_supported: bool_setting(
            context,
            &format!("{prefix}visible_summary_supported"),
        ),
        raw_reasoning_supported: bool_setting(context, &format!("{prefix}raw_reasoning_supported")),
    };
    (!override_.is_empty()).then_some(override_)
}

fn merge_reasoning_override(
    base: Option<bcode_model::ModelReasoningInfo>,
    override_: Option<ModelReasoningOverride>,
) -> Option<bcode_model::ModelReasoningInfo> {
    match (base, override_) {
        (Some(mut base), Some(override_)) => {
            if let Some(effort_values) = override_.effort_values {
                base.effort_values = effort_values;
            }
            if let Some(default_effort) = override_.default_effort {
                base.default_effort = Some(default_effort);
            }
            if let Some(visible_summary_supported) = override_.visible_summary_supported {
                base.visible_summary_supported = visible_summary_supported;
            }
            if let Some(summary_values) = override_.summary_values {
                base.summary_values = summary_values;
            }
            if let Some(default_summary) = override_.default_summary {
                base.default_summary = Some(default_summary);
            }
            if let Some(raw_reasoning_supported) = override_.raw_reasoning_supported {
                base.raw_reasoning_supported = raw_reasoning_supported;
            }
            base.source = bcode_model::ModelReasoningCapabilitySource::ConfigOverride;
            Some(base)
        }
        (Some(base), None) => Some(base),
        (None, Some(override_)) => Some(bcode_model::ModelReasoningInfo {
            effort_values: override_.effort_values.unwrap_or_default(),
            default_effort: override_.default_effort,
            visible_summary_supported: override_.visible_summary_supported.unwrap_or_default(),
            summary_values: override_.summary_values.unwrap_or_default(),
            default_summary: override_.default_summary,
            raw_reasoning_supported: override_.raw_reasoning_supported.unwrap_or_default(),
            source: bcode_model::ModelReasoningCapabilitySource::ConfigOverride,
        }),
        (None, None) => None,
    }
}

fn split_config_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn non_empty_setting(context: &bcode_model::ProviderRequestContext, key: &str) -> Option<String> {
    context
        .settings
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn bool_setting(context: &bcode_model::ProviderRequestContext, key: &str) -> Option<bool> {
    context
        .settings
        .get(key)
        .and_then(|value| value.parse::<bool>().ok())
}

#[derive(Debug, Clone)]
struct TraceStore {
    root: PathBuf,
}

impl TraceStore {
    const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn write_json_blob(
        &self,
        session_id: SessionId,
        name: &str,
        value: &impl serde::Serialize,
        max_bytes: usize,
    ) -> Option<TraceBlobRef> {
        let bytes = serde_json::to_vec_pretty(value).ok()?;
        self.write_blob(session_id, name, "application/json", &bytes, max_bytes)
    }

    fn write_text_blob(
        &self,
        session_id: SessionId,
        name: &str,
        text: &str,
        max_bytes: usize,
    ) -> Option<TraceBlobRef> {
        self.write_blob(session_id, name, "text/plain", text.as_bytes(), max_bytes)
    }

    fn write_blob(
        &self,
        session_id: SessionId,
        name: &str,
        content_type: &str,
        bytes: &[u8],
        max_bytes: usize,
    ) -> Option<TraceBlobRef> {
        use sha2::{Digest as _, Sha256};

        let bytes = if max_bytes == 0 {
            bytes
        } else if bytes.len() > max_bytes {
            &bytes[..max_bytes]
        } else {
            bytes
        };
        let hash = Sha256::digest(bytes);
        let mut sha256 = String::with_capacity(hash.len() * 2);
        for byte in hash {
            write!(sha256, "{byte:02x}").expect("writing to string should not fail");
        }
        let extension = if content_type == "application/json" {
            "json"
        } else {
            "txt"
        };
        let safe_name = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let relative = PathBuf::from(session_id.to_string())
            .join("blobs")
            .join(format!("{safe_name}-{sha256}.{extension}"));
        let path = self.root.join(&relative);
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return None;
        }
        let mut file = fs::File::create(&path).ok()?;
        file.write_all(bytes).ok()?;
        Some(TraceBlobRef {
            sha256,
            path: relative.display().to_string(),
            content_type: content_type.to_string(),
            byte_len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            redaction: TraceRedaction::Automatic,
            completeness: if max_bytes > 0 && bytes.len() >= max_bytes {
                bcode_session_models::TraceBlobCompleteness::Truncated
            } else {
                bcode_session_models::TraceBlobCompleteness::Complete
            },
        })
    }
}

#[derive(Debug, Clone)]
struct PendingPermission {
    summary: PermissionSummary,
    decision: Arc<Mutex<Option<bool>>>,
    notify: Arc<Notify>,
    skill_decision_key: Option<SkillToolDecisionKey>,
}

struct ServerStateInit {
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    selected_provider_context: bcode_model::ProviderRequestContext,
    prompt_cache_mode: bcode_model::PromptCacheMode,
    conversation_reuse_mode: bcode_model::ConversationReuseMode,
    selected_reasoning: bcode_config::ReasoningConfig,
    selected_reasoning_capabilities: Option<bcode_model::ModelReasoningInfo>,
    provider_state: ProviderStateStore,
    observability: bcode_config::ObservabilityConfig,
    trace_store: TraceStore,
    max_tool_rounds: Option<u32>,
    tool_output_context_chars: usize,
    model_streaming: bcode_config::StreamingConfig,
    model_retry: bcode_config::ModelRetryConfig,
    auto_compaction: bcode_config::CompactionConfig,
    system_prompt: bcode_config::SystemPromptConfig,
    skills: Option<SkillRegistry>,
    skill_context_bytes: usize,
    skill_prompt_options: SkillPromptCatalogOptions,
    daemon_status: DaemonStatus,
    daemon_record_path: Option<PathBuf>,
    metrics: MetricsRegistry,
    ralph_store: bcode_ralph::RalphStateStore,
}

impl ServerState {
    fn new(
        sessions: SessionManager,
        plugins: bcode_plugin::PluginRuntimeHost,
        init: ServerStateInit,
    ) -> Self {
        let (shutdown, _) = broadcast::channel(1);
        Self {
            sessions,
            session_catalog: Arc::new(session_catalog::SessionCatalog::default()),
            plugins,
            selected_provider_plugin_id: init.selected_provider_plugin_id,
            selected_model_id: init.selected_model_id,
            selected_provider_context: init.selected_provider_context,
            prompt_cache_mode: init.prompt_cache_mode,
            conversation_reuse_mode: init.conversation_reuse_mode,
            selected_reasoning: init.selected_reasoning,
            selected_reasoning_capabilities: init.selected_reasoning_capabilities,
            provider_state: Mutex::new(init.provider_state),
            observability: init.observability,
            trace_store: init.trace_store,
            max_tool_rounds: init.max_tool_rounds,
            tool_output_context_chars: init.tool_output_context_chars,
            model_streaming: init.model_streaming,
            model_retry: init.model_retry,
            auto_compaction: init.auto_compaction,
            system_prompt: init.system_prompt,
            skills: init.skills,
            skill_context_bytes: init.skill_context_bytes,
            skill_prompt_options: init.skill_prompt_options,
            active_skills: Mutex::default(),
            turn_skills: Mutex::default(),
            session_runtimes: Mutex::default(),
            runtime_work: RuntimeWorkManager::default(),
            ralph_store: init.ralph_store,
            active_ralph_runs: Mutex::default(),
            session_model_selections: Mutex::default(),
            session_agent_selections: Mutex::default(),
            pending_permissions: Mutex::default(),
            next_permission_id: Mutex::new(1),
            clients: Mutex::default(),
            client_runtime_contexts: Mutex::default(),
            client_session_namespaces: Mutex::default(),
            active_session_namespaces: Mutex::default(),
            message_accepted_clients: Mutex::default(),
            attached_client_sessions: Mutex::default(),
            client_forwarders: Mutex::default(),
            event_clients: Mutex::default(),
            catalog_events_started: std::sync::atomic::AtomicBool::new(false),
            idle_shutdown_started: std::sync::atomic::AtomicBool::new(false),
            daemon_status: init.daemon_status,
            daemon_record_path: init.daemon_record_path,
            metrics: init.metrics,
            shutdown,
        }
    }

    async fn register_client(&self, client_id: ClientId) {
        self.clients.lock().await.insert(client_id);
    }

    async fn unregister_client(&self, client_id: ClientId) {
        self.clients.lock().await.remove(&client_id);
        self.client_runtime_contexts.lock().await.remove(&client_id);
        self.client_session_namespaces
            .lock()
            .await
            .remove(&client_id);
        self.message_accepted_clients
            .lock()
            .await
            .remove(&client_id);
        self.unregister_catalog_event_client(client_id).await;
    }

    async fn attach_client_session(&self, client_id: ClientId, session_id: SessionId) {
        self.attached_client_sessions
            .lock()
            .await
            .insert(client_id, session_id);
    }

    async fn detach_client_session(
        &self,
        client_id: ClientId,
    ) -> Result<(), bcode_session::SessionError> {
        let session_id = self
            .attached_client_sessions
            .lock()
            .await
            .remove(&client_id);
        if let Some(session_id) = session_id {
            let _detached = self.sessions.detach_session(session_id, client_id).await?;
            self.deactivate_session_namespace_if_inactive(session_id)
                .await;
            self.release_session_resources_if_idle(session_id).await;
        }
        Ok(())
    }

    async fn session_current_turn(&self, session_id: SessionId) -> Option<RuntimeCurrentTurn> {
        let handle = self
            .session_runtimes
            .lock()
            .await
            .get(&session_id)
            .cloned()?;
        handle.current_turn.lock().await.clone()
    }

    async fn session_has_active_turn(&self, session_id: SessionId) -> bool {
        self.session_current_turn(session_id).await.is_some()
    }

    async fn active_model_turn_snapshot(&self, session_id: SessionId) -> Option<ActiveModelTurn> {
        self.session_current_turn(session_id)
            .await
            .and_then(|turn| turn.model)
    }

    async fn active_runtime_turn_count(&self) -> usize {
        let handles = self
            .session_runtimes
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut count = 0_usize;
        for handle in handles {
            if handle.current_turn.lock().await.is_some() {
                count = count.saturating_add(1);
            }
        }
        count
    }

    async fn release_session_resources_if_idle(&self, session_id: SessionId) {
        if self
            .attached_client_sessions
            .lock()
            .await
            .values()
            .any(|attached_session_id| *attached_session_id == session_id)
        {
            return;
        }
        if self.session_has_active_turn(session_id).await
            || !self
                .runtime_work
                .active_for_session(session_id)
                .await
                .is_empty()
        {
            return;
        }
        if let Err(error) = self
            .sessions
            .release_idle_session_resources(session_id)
            .await
        {
            eprintln!("failed to release idle session resources for {session_id}: {error}");
        }
    }

    async fn register_client_forwarder(&self, client_id: ClientId, handle: JoinHandle<()>) {
        self.client_forwarders
            .lock()
            .await
            .entry(client_id)
            .or_default()
            .push(handle);
    }

    async fn abort_client_forwarders(&self, client_id: ClientId) {
        let handles = self.client_forwarders.lock().await.remove(&client_id);
        if let Some(handles) = handles {
            for handle in handles {
                handle.abort();
            }
        }
    }

    async fn close_client(&self, client_id: ClientId) -> Result<(), ServerError> {
        self.abort_client_forwarders(client_id).await;
        self.detach_client_session(client_id).await?;
        self.unregister_client(client_id).await;
        Ok(())
    }

    async fn register_message_accepted_client(&self, client_id: ClientId) {
        self.message_accepted_clients.lock().await.insert(client_id);
    }

    async fn set_client_runtime_context(
        &self,
        client_id: ClientId,
        context: Option<ClientRuntimeContext>,
    ) {
        let mut contexts = self.client_runtime_contexts.lock().await;
        if let Some(context) = context {
            contexts.insert(client_id, context);
        } else {
            contexts.remove(&client_id);
        }
    }

    async fn client_runtime_context(&self, client_id: ClientId) -> Option<ClientRuntimeContext> {
        self.client_runtime_contexts
            .lock()
            .await
            .get(&client_id)
            .cloned()
    }

    async fn set_client_session_namespace(&self, client_id: ClientId, namespace: String) {
        self.client_session_namespaces
            .lock()
            .await
            .insert(client_id, namespace);
    }

    async fn client_session_namespace(&self, client_id: ClientId) -> String {
        self.client_session_namespaces
            .lock()
            .await
            .get(&client_id)
            .cloned()
            .unwrap_or_else(bcode_ipc::daemon_namespace)
    }

    async fn try_activate_session_namespace(
        &self,
        session_id: SessionId,
        namespace: String,
    ) -> Result<(), String> {
        let mut active_namespaces = self.active_session_namespaces.lock().await;
        match active_namespaces.get(&session_id) {
            Some(active_namespace) if active_namespace != &namespace => {
                Err(active_namespace.clone())
            }
            Some(_) => Ok(()),
            None => {
                active_namespaces.insert(session_id, namespace);
                drop(active_namespaces);
                Ok(())
            }
        }
    }

    async fn deactivate_session_namespace_if_inactive(&self, session_id: SessionId) {
        if let Ok(summary) = self.sessions.session_summary(session_id).await
            && summary.client_count == 0
        {
            self.active_session_namespaces
                .lock()
                .await
                .remove(&session_id);
        }
    }

    async fn active_session_namespace_mismatch(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Option<String> {
        let client_namespace = self.client_session_namespace(client_id).await;
        self.active_session_namespaces
            .lock()
            .await
            .get(&session_id)
            .filter(|active_namespace| *active_namespace != &client_namespace)
            .cloned()
    }

    async fn client_supports_message_accepted(&self, client_id: ClientId) -> bool {
        self.message_accepted_clients
            .lock()
            .await
            .contains(&client_id)
    }

    async fn idle_shutdown_blocker(&self) -> Option<String> {
        let connected_clients = self.clients.lock().await.len();
        if connected_clients > 1 {
            return Some(format!(
                "daemon has {connected_clients} connected clients; refusing idle-only stop"
            ));
        }
        let active_model_turns = self.active_runtime_turn_count().await;
        if active_model_turns > 0 {
            return Some(format!(
                "daemon has {active_model_turns} active model turns; refusing idle-only stop"
            ));
        }
        let queued_session_commands = self
            .session_runtimes
            .lock()
            .await
            .values()
            .map(|handle| handle.queued_followups.load(Ordering::Acquire))
            .sum::<usize>();
        if queued_session_commands > 0 {
            return Some(format!(
                "daemon has {queued_session_commands} queued session commands; refusing idle-only stop"
            ));
        }
        let plugin_running = self
            .plugins
            .executor_statuses()
            .into_iter()
            .map(|status| status.running)
            .sum::<usize>();
        if plugin_running > 0 {
            return Some(format!(
                "daemon has {plugin_running} running plugin tasks; refusing idle-only stop"
            ));
        }
        None
    }

    async fn status(&self) -> ServerStatus {
        let sessions = self
            .sessions
            .cached_sessions(&bcode_ipc::current_working_directory())
            .await;
        let status = catalog_status_to_ipc(self.sessions.catalog_status());
        ServerStatus {
            connected_client_count: self.clients.lock().await.len(),
            sessions,
            session_catalog_loaded: matches!(status, SessionCatalogStatus::Loaded),
            session_catalog_status: status.clone(),
            session_catalog_sources: vec![SessionCatalogSourceStatus {
                source_id: "native".to_owned(),
                display_name: "Native Bcode sessions".to_owned(),
                status,
                updated_at_ms: 0,
            }],
            session_catalog_revision: 0,
            selected_provider_plugin_id: self.selected_provider_plugin_id.clone(),
            selected_model_id: self.selected_model_id.clone(),
            plugin_runtime: self.plugins.executor_statuses(),
            daemon: self.daemon_status.clone(),
            metrics: self.metrics.snapshot(),
            metrics_report: Box::new(self.metrics.report()),
        }
    }

    async fn register_catalog_event_client(&self, client_id: ClientId, writer: SharedWriter) {
        self.event_clients.lock().await.insert(
            client_id,
            CatalogEventSubscription::new(ClientEventSink::new(client_id, writer)),
        );
    }

    async fn unregister_catalog_event_client(&self, client_id: ClientId) {
        self.event_clients.lock().await.remove(&client_id);
    }

    async fn unregister_catalog_event_clients(&self, client_ids: &[ClientId]) {
        if client_ids.is_empty() {
            return;
        }
        let mut event_clients = self.event_clients.lock().await;
        for client_id in client_ids {
            event_clients.remove(client_id);
        }
    }

    fn start_catalog_event_forwarder(self: &Arc<Self>) {
        if self
            .catalog_events_started
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let mut revisions = state.session_catalog.subscribe();
            loop {
                if revisions.changed().await.is_err() {
                    break;
                }
                let revision = *revisions.borrow_and_update();
                broadcast_catalog_update(&state, revision).await;
            }
        });

        let state = Arc::clone(self);
        tokio::spawn(async move {
            let mut mutations = state.sessions.subscribe_mutations();
            loop {
                match mutations.recv().await {
                    Ok(mutation) => {
                        state
                            .session_catalog
                            .upsert_native_session(mutation.summary)
                            .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            target: "bcode_server::session_catalog",
                            skipped,
                            "session mutation subscriber lagged; refreshing native catalog"
                        );
                        state.session_catalog.refresh_native_now(&state).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let state = Arc::clone(self);
        tokio::spawn(async move {
            match state.sessions.backfill_catalog().await {
                Ok(summaries) => {
                    for summary in summaries {
                        state.session_catalog.upsert_native_session(summary).await;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "bcode_server::session_catalog",
                        %error,
                        "session catalog backfill failed"
                    );
                }
            }
        });
    }

    fn start_idle_shutdown_watcher(self: &Arc<Self>, idle_after: Duration) {
        if idle_after.is_zero()
            || self
                .idle_shutdown_started
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let mut idle_since: Option<Instant> = None;
            let check_interval = idle_after.min(Duration::from_secs(30));
            let mut shutdown = state.subscribe_shutdown();
            loop {
                tokio::select! {
                    () = tokio::time::sleep(check_interval) => {}
                    _ = shutdown.recv() => break,
                }
                if let Some(blocker) = state.idle_shutdown_blocker().await {
                    if idle_since.take().is_some() {
                        tracing::debug!(target: "bcode_server::idle_shutdown", blocker, "daemon no longer idle; idle shutdown timer reset");
                    }
                    continue;
                }
                let now = Instant::now();
                let since = idle_since.get_or_insert_with(|| {
                    tracing::info!(target: "bcode_server::idle_shutdown", idle_after_secs = idle_after.as_secs(), "daemon idle; idle shutdown timer started");
                    now
                });
                if now.duration_since(*since) >= idle_after {
                    tracing::info!(target: "bcode_server::idle_shutdown", idle_after_secs = idle_after.as_secs(), "daemon idle timeout reached; shutting down");
                    state.request_shutdown();
                    break;
                }
            }
        });
    }

    async fn catalog_event_sinks(&self) -> Vec<ClientEventSink> {
        self.event_clients
            .lock()
            .await
            .values()
            .map(|subscription| subscription.sink.clone())
            .collect()
    }

    async fn mark_catalog_event_sent(&self, client_id: ClientId, revision: u64) {
        if let Some(subscription) = self.event_clients.lock().await.get_mut(&client_id) {
            subscription.last_sent_revision = Some(revision);
        }
    }

    fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.shutdown.subscribe()
    }

    fn request_shutdown(&self) {
        let _ = self.shutdown.send(());
    }
}

fn catalog_status_to_ipc(status: CatalogLoadStatus) -> SessionCatalogStatus {
    match status {
        CatalogLoadStatus::NotStarted => SessionCatalogStatus::NotStarted,
        CatalogLoadStatus::Loading => SessionCatalogStatus::Loading,
        CatalogLoadStatus::Loaded => SessionCatalogStatus::Loaded,
        CatalogLoadStatus::Failed(message) => SessionCatalogStatus::Failed(message),
    }
}

fn register_daemon(
    endpoint: &IpcEndpoint,
) -> Result<bcode_daemon_lifecycle::DaemonRecord, ServerError> {
    let instance_id = daemon_instance_id()?;
    let record = bcode_daemon_lifecycle::DaemonRecord::current(
        endpoint,
        daemon_log_path(),
        std::env::current_exe().ok(),
        instance_id,
    )?;
    bcode_daemon_lifecycle::write_record(&bcode_config::default_state_dir(), &record)?;
    Ok(record)
}

fn daemon_instance_id() -> Result<String, ServerError> {
    let started = bcode_daemon_lifecycle::unix_time_millis()?;
    Ok(format!("{}-{started}", std::process::id()))
}

fn daemon_status_from_record(record: &bcode_daemon_lifecycle::DaemonRecord) -> DaemonStatus {
    DaemonStatus {
        namespace: record.namespace.clone(),
        protocol_version: record.protocol_version,
        build_fingerprint: record.build_fingerprint.clone(),
        pid: record.pid,
        instance_id: record.instance_id.clone(),
        started_at_unix_ms: record.started_at_unix_ms,
    }
}

fn daemon_log_path() -> PathBuf {
    std::env::var_os("BCODE_DAEMON_LOG").map_or_else(
        || {
            bcode_config::default_state_dir()
                .join("logs")
                .join(format!("daemon-{}.log", bcode_ipc::daemon_namespace()))
        },
        PathBuf::from,
    )
}

fn resolve_plugin_configs(
    config: &bcode_config::BcodeConfig,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
) -> std::collections::BTreeMap<String, bcode_plugin::ResolvedPluginConfig> {
    let mut manifests = std::collections::BTreeMap::new();
    for plugin in static_plugins {
        if let Ok(manifest) = toml::from_str::<bcode_plugin::PluginManifest>(plugin.manifest_toml) {
            manifests.insert(manifest.id.clone(), manifest);
        }
    }
    manifests
        .values()
        .filter_map(|manifest| {
            let raw = resolved_plugin_config_value(config, manifest);
            if raw.is_null() {
                return None;
            }
            let redacted = redact_plugin_config_value(&raw);
            Some((
                manifest.id.clone(),
                bcode_plugin::ResolvedPluginConfig::new(raw, redacted),
            ))
        })
        .collect()
}

fn resolved_plugin_config_value(
    config: &bcode_config::BcodeConfig,
    manifest: &bcode_plugin::PluginManifest,
) -> serde_json::Value {
    let mut value = serde_json::Value::Object(serde_json::Map::new());
    if let Some(section) = manifest
        .config
        .as_ref()
        .and_then(|config| config.section.as_deref())
        && section == "web_search"
    {
        merge_json_values(&mut value, toml_value_to_json(&config.web_search));
    }
    if let Some(plugin_value) = config.plugins.config.get(&manifest.id) {
        merge_json_values(&mut value, toml_value_to_json(plugin_value));
    }
    resolve_plugin_config_secrets(value)
}

fn resolve_plugin_config_secrets(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(resolved) = resolve_secret_ref(&map) {
                return serde_json::Value::String(resolved);
            }
            serde_json::Value::Object(
                map.into_iter()
                    .map(|(key, value)| (key, resolve_plugin_config_secrets(value)))
                    .collect(),
            )
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(resolve_plugin_config_secrets)
                .collect(),
        ),
        value => value,
    }
}

fn resolve_secret_ref(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let backend = map.get("backend")?.as_str()?;
    match backend {
        "env" => map
            .get("name")
            .and_then(serde_json::Value::as_str)
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty()),
        "sshenv" => resolve_sshenv_secret_ref(map),
        _ => None,
    }
}

fn resolve_sshenv_secret_ref(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let profile = map.get("profile")?.as_str()?;
    let key = map
        .get("key")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(profile);
    let vault = map
        .get("vault")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            bcode_config::default_auth_vault_path,
            std::path::PathBuf::from,
        );
    let store = sshenv_vault::SshenvStore::new(
        sshenv_vault::SshenvStoreConfig::new(vault.clone()).with_private_key_paths(
            bcode_provider_auth::security::vault_private_key_paths(&vault),
        ),
    );
    store
        .get_profile(profile)
        .ok()
        .flatten()
        .and_then(|profile_env| profile_env.get(key).map(|value| value.as_str().to_string()))
}

fn toml_value_to_json(value: &toml::Value) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

fn merge_json_values(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge_json_values(base.entry(key).or_insert(serde_json::Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn redact_plugin_config_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| {
                    if key.to_ascii_lowercase().contains("key")
                        || key.to_ascii_lowercase().contains("secret")
                        || key.to_ascii_lowercase().contains("token")
                    {
                        (
                            key.clone(),
                            serde_json::Value::String("<redacted>".to_string()),
                        )
                    } else {
                        (key.clone(), redact_plugin_config_value(value))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_plugin_config_value).collect())
        }
        value => value.clone(),
    }
}

/// Run the local Bcode server until interrupted.
///
/// # Errors
///
/// Returns an error when the server cannot bind or accept local IPC connections.
pub async fn run(endpoint: IpcEndpoint) -> Result<(), ServerError> {
    run_with_static_bundled(endpoint, &[]).await
}

/// Run the local Bcode server with caller-provided static bundled plugins until interrupted.
///
/// # Errors
///
/// Returns an error when the server cannot bind or accept local IPC connections.
#[allow(clippy::too_many_lines)]
pub async fn run_with_static_bundled(
    endpoint: IpcEndpoint,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
) -> Result<(), ServerError> {
    tracing::debug!(target: "bcode_server::startup", "loading config");
    let config = bcode_config::load_config()?;
    tracing::debug!(target: "bcode_server::startup", "config loaded");
    let static_plugin_ids = bcode_plugin::static_bundled_plugin_ids(static_plugins)?;
    let plugin_selection =
        bcode_config::plugin_selection_with_default_plugin_ids(&config, &static_plugin_ids);
    tracing::debug!(
        target: "bcode_server::startup",
        enabled = ?plugin_selection.enabled,
        disabled = ?plugin_selection.disabled,
        "plugin selection resolved"
    );
    tracing::debug!(target: "bcode_server::startup", "loading plugins");
    let plugin_configs = resolve_plugin_configs(&config, static_plugins);
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled_and_config(
        &plugin_selection,
        static_plugins,
        plugin_configs,
    )?;
    tracing::debug!(target: "bcode_server::startup", "plugins loaded");
    tracing::debug!(target: "bcode_server::startup", endpoint = ?endpoint, "binding IPC endpoint");
    let listener = LocalIpcListener::bind(&endpoint)?;
    let daemon_record = register_daemon(&endpoint)?;
    let daemon_status = daemon_status_from_record(&daemon_record);
    tracing::debug!(target: "bcode_server::startup", "IPC endpoint bound");
    tracing::debug!(target: "bcode_server::startup", "initializing lazy session services");
    let metrics = MetricsRegistry::with_event_log(
        bcode_config::default_state_dir()
            .join("metrics")
            .join("events.jsonl"),
    );
    let sessions = SessionManager::persistent_lazy_with_metrics_and_lease_owner(
        default_session_store_dir(),
        metrics.clone(),
        SessionLeaseOwnerContext {
            daemon_namespace: Some(daemon_status.namespace.clone()),
            build_fingerprint: Some(daemon_status.build_fingerprint.clone()),
            protocol_version: Some(daemon_status.protocol_version),
            endpoint: Some(format!("{endpoint:?}")),
            executable_path: std::env::current_exe().ok(),
            daemon_instance_id: Some(daemon_status.instance_id.clone()),
        },
    );
    tracing::debug!(target: "bcode_server::startup", "lazy session services ready");
    let resolved_model = config.resolved_model_selection();
    tracing::debug!(
        target: "bcode_server::startup",
        provider = ?resolved_model.provider_plugin_id,
        model = ?resolved_model.model_id,
        "model selection resolved"
    );
    let configured_agent_ids: Vec<String> = config.agent.keys().cloned().collect();
    let skills = build_skill_registry(&config);
    let selected_provider_context = bcode_provider_auth::resolve_provider_request_context(
        bcode_provider_auth::ProviderRequestContextResolution {
            config: &config,
            selection: resolved_model.clone(),
        },
    );
    let state = Arc::new(ServerState::new(
        sessions,
        plugins,
        ServerStateInit {
            selected_provider_plugin_id: resolved_model.provider_plugin_id,
            selected_model_id: resolved_model.model_id,
            selected_provider_context,
            prompt_cache_mode: config.model.effective_prompt_cache_mode(),
            conversation_reuse_mode: config.model.effective_conversation_reuse_mode(),
            selected_reasoning: resolved_model.reasoning.clone(),
            selected_reasoning_capabilities: reasoning_capabilities_from_config(
                &resolved_model.reasoning,
            ),
            provider_state: ProviderStateStore::load(default_provider_state_path()),
            observability: config.observability,
            trace_store: TraceStore::new(default_trace_store_dir()),
            max_tool_rounds: config.model.effective_max_tool_rounds(),
            tool_output_context_chars: config.model.tool_output.context_chars,
            model_streaming: config.model.streaming,
            model_retry: config.model.retry,
            auto_compaction: config.model.compaction,
            system_prompt: config.system_prompt,
            skill_context_bytes: config.skills.max_context_bytes,
            skill_prompt_options: skill_prompt_options_from_config(&config.skills.prompt),
            skills,
            daemon_status,
            daemon_record_path: Some(bcode_daemon_lifecycle::record_path(
                &bcode_config::default_state_dir(),
                &daemon_record.namespace,
            )),
            metrics,
            ralph_store: bcode_ralph::RalphStateStore::default(),
        },
    ));
    state.start_catalog_event_forwarder();
    interrupt_stale_ralph_runs_best_effort(&state);
    if config.daemon.idle_shutdown {
        state.start_idle_shutdown_watcher(Duration::from_secs(
            config.daemon.idle_shutdown_after_secs,
        ));
    }
    warn_on_unregistered_agent_ids(&state, &configured_agent_ids).await;
    let mut shutdown = state.subscribe_shutdown();
    tracing::info!(target: "bcode_server::startup", "server ready; accepting clients");
    loop {
        tokio::select! {
            stream = listener.accept() => {
                let stream = stream?;
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, state).await {
                        eprintln!("client connection failed: {error}");
                    }
                });
            }
            _ = shutdown.recv() => break,
        }
    }
    tracing::debug!(target: "bcode_server::startup", "shutdown requested; deactivating plugins");
    state.plugins.deactivate_all().await?;
    if let Some(path) = &state.daemon_record_path {
        bcode_daemon_lifecycle::remove_record_if_instance(path, &state.daemon_status.instance_id)?;
    }
    tracing::debug!(target: "bcode_server::startup", "shutdown complete");
    Ok(())
}

fn interrupt_stale_ralph_runs_best_effort(state: &ServerState) {
    match state
        .ralph_store
        .mark_all_active_runs_interrupted("daemon restart")
    {
        Ok(marked) if marked > 0 => {
            state
                .metrics
                .increment_counter("server.ralph_runs.interrupted_on_startup_total");
            tracing::warn!(
                target: "bcode_server::ralph",
                marked,
                "marked stale active Ralph runs interrupted on daemon startup"
            );
        }
        Ok(_) => {}
        Err(error) => {
            state
                .metrics
                .increment_counter("server.ralph_runs.interrupt_startup_error_total");
            eprintln!("failed to interrupt stale Ralph runs on startup: {error}");
        }
    }
}

async fn recover_abandoned_session_runtime_work_best_effort(
    state: &ServerState,
    session_id: SessionId,
) {
    if let Err(error) = recover_abandoned_session_runtime_work(state, session_id).await {
        state
            .metrics
            .increment_counter("server.runtime_work.recovery_error_total");
        eprintln!("failed to recover abandoned runtime work for session {session_id}: {error}");
    }
}

async fn recover_abandoned_session_runtime_work(
    state: &ServerState,
    session_id: SessionId,
) -> Result<(), ServerError> {
    let active = state.sessions.active_runtime_work(session_id).await?;
    let work_ids = active
        .iter()
        .map(|work| work.work_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for work in active {
        let message = if work
            .parent_work_id
            .as_ref()
            .is_some_and(|parent| !work_ids.contains(parent))
        {
            format!("parent work ended before child finished: {}", work.label)
        } else {
            format!(
                "daemon stopped before runtime work finished: {}",
                work.label
            )
        };
        append_runtime_work_finished_event(
            state,
            session_id,
            work.work_id,
            RuntimeWorkStatus::Failed,
            Some(message),
        )
        .await;
    }
    Ok(())
}

async fn handle_client(stream: LocalIpcStream, state: Arc<ServerState>) -> Result<(), ServerError> {
    let client_id = ClientId::new();
    state.register_client(client_id).await;

    let result = handle_registered_client(stream, &state, client_id).await;
    let cleanup_result = state.close_client(client_id).await;
    match (result, cleanup_result) {
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn handle_registered_client(
    stream: LocalIpcStream,
    state: &Arc<ServerState>,
    client_id: ClientId,
) -> Result<(), ServerError> {
    let (mut reader, writer) = split(stream);
    let writer = Arc::new(Mutex::new(writer));
    let mut attached_session: Option<SessionId> = None;

    loop {
        let envelope = match recv_envelope(&mut reader).await {
            Ok(envelope) => envelope,
            Err(CodecError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(error) => return Err(error.into()),
        };

        if envelope.kind != EnvelopeKind::Request {
            continue;
        }

        let request = decode_request(&envelope.payload)?;
        handle_request(
            request,
            envelope.request_id,
            client_id,
            state,
            &writer,
            &mut attached_session,
        )
        .await?;
    }

    Ok(())
}

#[allow(clippy::too_many_lines, clippy::large_stack_frames)]
async fn handle_request(
    request: Request,
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
) -> Result<(), ServerError> {
    match request {
        Request::Hello {
            client_name,
            runtime_context,
            daemon_namespace,
        } => {
            handle_hello(
                request_id,
                client_id,
                state,
                writer,
                &client_name,
                runtime_context,
                daemon_namespace,
            )
            .await
        }
        Request::UpdateClientRuntimeContext { runtime_context } => {
            handle_update_client_runtime_context(
                request_id,
                client_id,
                state,
                writer,
                runtime_context,
            )
            .await
        }
        Request::Ping => handle_ping(request_id, writer).await,
        Request::ServerStatus => handle_server_status(request_id, state, writer).await,
        Request::ServerStop { mode } => handle_server_stop(request_id, state, writer, mode).await,
        Request::SetComposerDraft { scope, text } => {
            handle_set_composer_draft(request_id, state, writer, scope, text).await
        }
        Request::ComposerDraft { scope } => {
            handle_composer_draft(request_id, state, writer, scope).await
        }
        Request::CreateSession {
            name,
            working_directory,
        } => handle_create_session(request_id, state, writer, name, working_directory).await,
        Request::ListSessions { working_directory } => {
            handle_list_sessions(request_id, state, writer, &working_directory).await
        }
        Request::SubscribeCatalogUpdates => {
            handle_subscribe_catalog_updates(request_id, client_id, state, writer).await
        }
        Request::ChangeSessionWorkingDirectory {
            session_id,
            working_directory,
        } => {
            handle_change_session_working_directory(
                request_id,
                state,
                writer,
                session_id,
                working_directory,
            )
            .await
        }
        Request::ListWorktrees(request) => {
            handle_list_worktrees(request_id, state, writer, request).await
        }
        Request::CreateWorktree(request) => {
            handle_create_worktree(request_id, state, writer, request).await
        }
        Request::RemoveWorktree(request) => {
            handle_remove_worktree(request_id, state, writer, request).await
        }
        Request::RalphStatus(request) => {
            handle_ralph_status(request_id, state, writer, request).await
        }
        Request::RunRalphLoop(request) => {
            handle_run_ralph_loop(request_id, client_id, state, writer, request).await
        }
        Request::ApproveRalphRun(request) => {
            handle_approve_ralph_run(request_id, client_id, state, writer, request).await
        }
        Request::CancelRalphLoop(request) => {
            Box::pin(handle_cancel_ralph_loop(request_id, state, writer, request)).await
        }
        Request::ListRalphRuns(request) => {
            Box::pin(handle_list_ralph_runs(request_id, state, writer, *request)).await
        }
        Request::ListRalphIterations(request) => {
            Box::pin(handle_list_ralph_iterations(
                request_id, state, writer, *request,
            ))
            .await
        }
        Request::ResumeRalphRun(request) => {
            handle_resume_ralph_run(request_id, state, writer, request).await
        }
        Request::RalphRunStatus(request) => {
            handle_ralph_run_status(request_id, state, writer, request).await
        }
        Request::RecordRalphLifecycle(request) => {
            handle_record_ralph_lifecycle(request_id, state, writer, request).await
        }
        Request::RenameSession { session_id, name } => {
            handle_rename_session(request_id, state, writer, session_id, name).await
        }
        Request::DeleteSession { session_id } => {
            handle_delete_session(request_id, state, writer, session_id).await
        }
        Request::ForkSession {
            source_session_id,
            prompt_sequence,
            name,
        } => {
            handle_fork_session(
                request_id,
                state,
                writer,
                source_session_id,
                prompt_sequence,
                name,
            )
            .await
        }
        Request::CloneSession {
            source_session_id,
            name,
        } => handle_clone_session(request_id, state, writer, source_session_id, name).await,
        Request::SessionHistory { session_id } => {
            handle_session_history(request_id, client_id, state, writer, session_id).await
        }
        Request::SessionHistoryPage { session_id, query } => {
            handle_session_history_page(request_id, client_id, state, writer, session_id, query)
                .await
        }
        Request::AttachSession { session_id } => {
            let session_id = session_import::resolve_attach_session_id(state, session_id).await;
            handle_attach_session(
                request_id,
                client_id,
                state,
                writer,
                attached_session,
                session_id,
            )
            .await
        }
        Request::AttachSessionRecent { session_id, limit } => {
            let session_id = session_import::resolve_attach_session_id(state, session_id).await;
            handle_attach_session_recent(
                request_id,
                client_id,
                state,
                writer,
                attached_session,
                session_id,
                limit,
            )
            .await
        }
        Request::AttachSessionProjectionWindow {
            session_id,
            request,
        } => {
            let session_id = session_import::resolve_attach_session_id(state, session_id).await;
            handle_attach_session_projection_window(
                request_id,
                client_id,
                state,
                writer,
                attached_session,
                session_id,
                request,
            )
            .await
        }
        Request::ImportExternalSession {
            source_id,
            external_session_id,
        } => {
            session_import::handle_import_external_session(
                request_id,
                state,
                writer,
                &source_id,
                &external_session_id,
            )
            .await
        }
        Request::RefreshSessionCatalog {
            working_directory,
            sources,
        } => {
            let working_directory =
                working_directory.unwrap_or_else(bcode_ipc::current_working_directory);
            handle_refresh_session_catalog(
                request_id,
                state,
                writer,
                &working_directory,
                sources.as_deref(),
            )
            .await
        }
        Request::SendUserMessage { session_id, text } => {
            handle_user_message(
                request_id,
                client_id,
                state,
                writer,
                session_id,
                text,
                bcode_ipc::PromptPlacement::Steering,
            )
            .await
        }
        Request::SendUserMessageWithPlacement {
            session_id,
            text,
            placement,
        } => {
            handle_user_message(
                request_id, client_id, state, writer, session_id, text, placement,
            )
            .await
        }
        Request::InvokeSkill {
            session_id,
            skill_id,
            arguments,
            display_text,
        } => {
            handle_invoke_skill(
                request_id,
                client_id,
                state,
                writer,
                session_id,
                skill_id,
                arguments,
                display_text,
            )
            .await
        }
        Request::CancelSessionTurn {
            session_id,
            clear_queue,
        } => {
            handle_cancel_session_turn(
                request_id,
                state,
                writer,
                session_id,
                client_id,
                clear_queue,
            )
            .await
        }
        Request::CancelRuntimeWork {
            session_id,
            work_id,
        } => {
            handle_cancel_runtime_work(request_id, client_id, state, writer, session_id, work_id)
                .await
        }
        Request::ListRuntimeWork { session_id } => {
            handle_list_runtime_work(request_id, state, writer, session_id).await
        }
        Request::RuntimeWorkHistory { session_id, limit } => {
            handle_runtime_work_history(request_id, state, writer, session_id, limit).await
        }
        Request::SubscribeRuntimeWork { session_id } => {
            handle_subscribe_runtime_work(request_id, client_id, state, writer, session_id).await
        }
        Request::CompactSession { session_id } => {
            handle_compact_session(request_id, client_id, state, writer, session_id).await
        }
        Request::SetSessionModel {
            session_id,
            provider_plugin_id,
            model_id,
        } => {
            handle_set_session_model(
                request_id,
                state,
                writer,
                session_id,
                provider_plugin_id,
                model_id,
            )
            .await
        }
        Request::SetSessionReasoning {
            session_id,
            effort,
            summary,
        } => {
            handle_set_session_reasoning(request_id, state, writer, session_id, effort, summary)
                .await
        }
        Request::SessionModelStatus { session_id } => {
            handle_session_model_status(request_id, client_id, state, writer, session_id).await
        }
        Request::DefaultModelStatus => {
            handle_default_model_status(request_id, client_id, state, writer).await
        }
        Request::SessionModelList { provider_plugin_id } => {
            handle_session_model_list(request_id, client_id, state, writer, provider_plugin_id)
                .await
        }
        request => handle_remaining_request(request, request_id, state, writer).await,
    }
}

async fn handle_remaining_request(
    request: Request,
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    handle_agent_permission_plugin_request(request, request_id, state, writer).await
}

async fn handle_agent_permission_plugin_request(
    request: Request,
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    match request {
        Request::ListAgents => handle_list_agents(request_id, state, writer).await,
        Request::ListSkills => handle_list_skills(request_id, state, writer).await,
        Request::DescribeSkill { skill_id } => {
            handle_describe_skill(request_id, state, writer, &skill_id).await
        }
        Request::ActivateSkill {
            session_id,
            skill_id,
        } => handle_activate_skill(request_id, state, writer, session_id, skill_id).await,
        Request::DeactivateSkill {
            session_id,
            skill_id,
        } => handle_deactivate_skill(request_id, state, writer, session_id, skill_id).await,
        Request::ActiveSkills { session_id } => {
            handle_active_skills(request_id, state, writer, session_id).await
        }
        Request::AgentPolicyStatus => handle_agent_policy_status(request_id, state, writer).await,
        Request::SetSessionAgent {
            session_id,
            agent_id,
        } => handle_set_session_agent(request_id, state, writer, session_id, agent_id).await,
        Request::ListPermissions => handle_list_permissions(request_id, state, writer).await,
        Request::ResolvePermission {
            permission_id,
            approved,
            remember,
        } => {
            handle_resolve_permission(
                request_id,
                state,
                writer,
                &permission_id,
                approved,
                remember,
            )
            .await
        }
        Request::AddPermissionRule {
            agent_id,
            category,
            pattern,
            action,
        } => {
            handle_add_permission_rule(
                request_id, state, writer, &agent_id, &category, pattern, &action,
            )
            .await
        }
        Request::ListPluginServices => handle_list_plugin_services(request_id, state, writer).await,
        Request::ListPluginContributions => {
            handle_list_plugin_contributions(request_id, state, writer).await
        }
        Request::InvokePluginService {
            plugin_id,
            interface_id,
            operation,
            payload,
        } => {
            handle_invoke_plugin_service(
                request_id,
                state,
                writer,
                &plugin_id,
                &interface_id,
                operation,
                payload,
            )
            .await
        }
        Request::CallPluginService {
            interface_id,
            operation,
            payload,
        } => {
            handle_call_plugin_service(request_id, state, writer, &interface_id, operation, payload)
                .await
        }
        Request::PublishPluginEvent { topic, payload } => {
            handle_publish_plugin_event(request_id, state, writer, &topic, &payload).await
        }
        Request::ListWorktrees(_)
        | Request::CreateWorktree(_)
        | Request::RemoveWorktree(_)
        | Request::RalphStatus(_)
        | Request::RunRalphLoop(_)
        | Request::ApproveRalphRun(_)
        | Request::CancelRalphLoop(_)
        | Request::ListRalphRuns(_)
        | Request::ListRalphIterations(_)
        | Request::ResumeRalphRun(_)
        | Request::RalphRunStatus(_)
        | Request::RecordRalphLifecycle(_) => {
            unreachable!("primary request routed to primary handler")
        }
        _ => unreachable!("primary request routed to agent/permission/plugin handler"),
    }
}

async fn handle_update_client_runtime_context(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    runtime_context: Option<ClientRuntimeContext>,
) -> Result<(), ServerError> {
    state
        .set_client_runtime_context(client_id, runtime_context)
        .await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::ClientRuntimeContextUpdated),
    )
    .await
}

async fn handle_ping(request_id: u64, writer: &SharedWriter) -> Result<(), ServerError> {
    send_response(writer, request_id, Response::Ok(ResponsePayload::Pong)).await
}

fn client_name_supports_message_accepted(client_name: &str) -> bool {
    client_name
        .split(';')
        .any(|part| part.trim() == "cap=message_accepted")
}

async fn handle_hello(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    client_name: &str,
    runtime_context: Option<ClientRuntimeContext>,
    daemon_namespace: String,
) -> Result<(), ServerError> {
    if client_name_supports_message_accepted(client_name) {
        state.register_message_accepted_client(client_id).await;
    }
    state
        .set_client_runtime_context(client_id, runtime_context)
        .await;
    state
        .set_client_session_namespace(client_id, daemon_namespace)
        .await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::Hello {
            protocol_version: bcode_ipc::ProtocolVersion::current(),
            client_id,
        }),
    )
    .await
}

async fn handle_server_status(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let status = state.status().await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::ServerStatus { status }),
    )
    .await
}

async fn handle_server_stop(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    mode: ServerStopMode,
) -> Result<(), ServerError> {
    if mode == ServerStopMode::IfIdle
        && let Some(message) = state.idle_shutdown_blocker().await
    {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new("daemon_busy", message)),
        )
        .await;
    }
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::ServerStopping),
    )
    .await?;
    state.request_shutdown();
    Ok(())
}

async fn handle_set_composer_draft(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    scope: bcode_ipc::ComposerDraftScope,
    text: String,
) -> Result<(), ServerError> {
    match scope {
        bcode_ipc::ComposerDraftScope::Session { session_id } => {
            state
                .sessions
                .set_session_composer_draft(session_id, text)
                .await?;
        }
        bcode_ipc::ComposerDraftScope::DraftSession {
            launch_working_directory,
        } => {
            state
                .sessions
                .set_draft_session_composer_draft(launch_working_directory, text)
                .await?;
        }
    }
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::ComposerDraftSet),
    )
    .await
}

async fn handle_composer_draft(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    scope: bcode_ipc::ComposerDraftScope,
) -> Result<(), ServerError> {
    let draft = match scope {
        bcode_ipc::ComposerDraftScope::Session { session_id } => {
            state.sessions.session_composer_draft(session_id).await?
        }
        bcode_ipc::ComposerDraftScope::DraftSession {
            launch_working_directory,
        } => {
            state
                .sessions
                .draft_session_composer_draft(launch_working_directory)
                .await?
        }
    };
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::ComposerDraft { draft }),
    )
    .await
}

async fn handle_create_session(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    name: Option<String>,
    working_directory: PathBuf,
) -> Result<(), ServerError> {
    let session = state
        .sessions
        .create_session(name, working_directory)
        .await?;
    state
        .session_catalog
        .upsert_native_session(session.clone())
        .await;
    if let Ok(mut events) = state
        .sessions
        .session_events_range(session.id, 0, 0, 1)
        .await
        && let Some(event) = events.pop()
    {
        publish_session_event(state, &event).await;
    }
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionCreated { session }),
    )
    .await
}

async fn handle_list_sessions(
    request_id: u64,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    working_directory: &Path,
) -> Result<(), ServerError> {
    let snapshot = state
        .session_catalog
        .snapshot(state, working_directory)
        .await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionList {
            sessions: snapshot.sessions,
            catalog_status: snapshot.status,
            catalog_sources: snapshot.sources,
            catalog_revision: snapshot.revision,
        }),
    )
    .await
}

async fn handle_refresh_session_catalog(
    request_id: u64,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    working_directory: &Path,
    sources: Option<&[String]>,
) -> Result<(), ServerError> {
    let snapshot = state
        .session_catalog
        .refresh(state, working_directory, sources)
        .await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionCatalogRefreshed {
            sessions: snapshot.sessions,
            catalog_status: snapshot.status,
            catalog_sources: snapshot.sources,
            catalog_revision: snapshot.revision,
        }),
    )
    .await
}

async fn handle_subscribe_catalog_updates(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    state
        .register_catalog_event_client(client_id, writer.clone())
        .await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::CatalogUpdatesSubscribed),
    )
    .await
}

async fn handle_change_session_working_directory(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    working_directory: PathBuf,
) -> Result<(), ServerError> {
    if state.session_has_active_turn(session_id).await {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "session_busy",
                format!("session has an active model turn: {session_id}"),
            )),
        )
        .await;
    }
    match state
        .sessions
        .change_session_working_directory(session_id, working_directory)
        .await
    {
        Ok(event) => {
            let changed = event.is_some();
            if let Some(event) = event {
                publish_session_event(state, &event).await;
            }
            let session = state.sessions.session_summary(session_id).await?;
            if changed {
                state
                    .session_catalog
                    .upsert_native_session(session.clone())
                    .await;
            }
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionWorkingDirectoryChanged { session, changed }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "session_cwd_change_failed",
                    error.to_string(),
                )),
            )
            .await
        }
    }
}

async fn handle_list_worktrees(
    request_id: u64,
    _state: &ServerState,
    writer: &SharedWriter,
    request: WorktreeListRequest,
) -> Result<(), ServerError> {
    let cwd = request.cwd.unwrap_or_else(current_request_cwd);
    match bcode_worktree::list_worktrees(&cwd) {
        Ok(response) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::WorktreeList(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "worktree_list_command_failed",
                    error.to_string(),
                )),
            )
            .await
        }
    }
}

async fn handle_ralph_status(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    request: RalphStatusRequest,
) -> Result<(), ServerError> {
    match state.ralph_store.latest_loop(&request.repo_root) {
        Ok(loop_summary) => {
            let response = RalphStatusResponse {
                loop_summary: loop_summary
                    .map(|summary| ralph_status_summary(&state.ralph_store, summary)),
            };
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::RalphStatus(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("ralph_status_failed", error.to_string())),
            )
            .await
        }
    }
}

async fn handle_run_ralph_loop(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    request: RalphRunRequest,
) -> Result<(), ServerError> {
    match start_ralph_runner(
        state,
        request,
        state.client_runtime_context(client_id).await,
    )
    .await
    {
        Ok(response) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::RalphRunStarted(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("ralph_run_start_failed", error)),
            )
            .await
        }
    }
}

async fn handle_resume_ralph_run(
    request_id: u64,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    request: RalphResumeRequest,
) -> Result<(), ServerError> {
    match prepare_ralph_resume(state, request).await {
        Ok(response) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::RalphRunResumed(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("ralph_resume_failed", error)),
            )
            .await
        }
    }
}

async fn prepare_ralph_resume(
    state: &Arc<ServerState>,
    request: RalphResumeRequest,
) -> Result<RalphResumeResponse, String> {
    let summary = resolve_ralph_loop(
        &state.ralph_store,
        &request.repo_root,
        request.loop_state_dir.as_deref(),
    )?;
    if state
        .ralph_store
        .active_run_for_loop(&summary.state_dir)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err("Ralph loop already has an active run".to_owned());
    }
    let interrupted_runs = state
        .ralph_store
        .interrupted_runs_for_loop(&summary.state_dir)
        .map_err(|error| error.to_string())?;
    let interrupted_run = request
        .interrupted_run_id
        .as_deref()
        .and_then(|run_id| interrupted_runs.iter().find(|run| run.run_id == run_id))
        .or_else(|| interrupted_runs.first())
        .cloned()
        .ok_or_else(|| "Ralph loop has no interrupted runs to resume".to_owned())?;
    let resumed_run = state
        .ralph_store
        .create_run(bcode_ralph::RalphRunCreateRequest {
            state_dir: summary.state_dir.clone(),
            session_id: summary.session_id.clone(),
            status: "awaiting_approval".to_owned(),
            requested_max_iterations: interrupted_run.requested_max_iterations,
            requested_no_progress_limit: interrupted_run.requested_no_progress_limit,
        })
        .map_err(|error| error.to_string())?;
    let _ = state.ralph_store.append_lifecycle_event_for_summary(
        &summary,
        bcode_ralph::RalphLifecycleEventKind::RunStarted,
        "Prepared approval-gated Ralph resume run",
    );
    append_ralph_session_lifecycle(
        state,
        resumed_run.session_id.as_deref(),
        summary.loop_name,
        resumed_run.state_dir.clone(),
        "resume_prepared",
        "Prepared approval-gated Ralph resume run",
    )
    .await;
    Ok(RalphResumeResponse {
        interrupted_run: ralph_run_summary(interrupted_run),
        resumed_run: ralph_run_summary(resumed_run),
    })
}

async fn handle_approve_ralph_run(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    request: RalphApproveRequest,
) -> Result<(), ServerError> {
    match approve_ralph_run(
        state,
        request,
        state.client_runtime_context(client_id).await,
    )
    .await
    {
        Ok(response) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::RalphRunApproved(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("ralph_run_approval_failed", error)),
            )
            .await
        }
    }
}

async fn approve_ralph_run(
    state: &Arc<ServerState>,
    request: RalphApproveRequest,
    runtime_context: Option<ClientRuntimeContext>,
) -> Result<RalphRunResponse, String> {
    let summary = resolve_ralph_loop(
        &state.ralph_store,
        &request.repo_root,
        request.loop_state_dir.as_deref(),
    )?;
    let active_run = state
        .ralph_store
        .active_run_for_loop(&summary.state_dir)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Ralph loop has no approval-gated run".to_owned())?;
    if request
        .run_id
        .as_deref()
        .is_some_and(|run_id| run_id != active_run.run_id)
    {
        return Err("requested Ralph run is not active for this loop".to_owned());
    }
    if active_run.status != "awaiting_approval" {
        return Err("Ralph active run is not awaiting approval".to_owned());
    }
    state
        .ralph_store
        .update_run_status(&active_run.run_id, "running", None, None, None)
        .map_err(|error| error.to_string())?;
    let mut approved_run = active_run;
    approved_run.status.clear();
    approved_run.status.push_str("running");
    spawn_ralph_runner_task(state, approved_run.clone(), summary, runtime_context).await?;
    Ok(RalphRunResponse {
        run: ralph_run_summary(approved_run),
    })
}

async fn spawn_ralph_runner_task(
    state: &Arc<ServerState>,
    run: bcode_ralph::RalphRunRecord,
    summary: bcode_ralph::RalphLoopSummary,
    runtime_context: Option<ClientRuntimeContext>,
) -> Result<(), String> {
    let mut active_ralph_runs = state.active_ralph_runs.lock().await;
    if active_ralph_runs.contains_key(&run.state_dir) {
        return Err("Ralph loop already has an active runner task".to_owned());
    }
    let runner_state = Arc::clone(state);
    let state_dir = run.state_dir.clone();
    let handle = tokio::spawn(async move {
        run_ralph_runner_skeleton(runner_state, run, summary, runtime_context).await;
    });
    active_ralph_runs.insert(state_dir, handle);
    drop(active_ralph_runs);
    Ok(())
}

fn effective_ralph_run_limits(
    summary_max_iterations: u64,
    summary_no_progress_limit: u64,
    max_iterations: Option<u64>,
    no_progress_limit: Option<u64>,
) -> (Option<u64>, Option<u64>) {
    (
        max_iterations.or(Some(summary_max_iterations)),
        no_progress_limit.or(Some(summary_no_progress_limit)),
    )
}

async fn start_ralph_runner(
    state: &Arc<ServerState>,
    request: RalphRunRequest,
    runtime_context: Option<ClientRuntimeContext>,
) -> Result<RalphRunResponse, String> {
    let summary = resolve_ralph_loop(
        &state.ralph_store,
        &request.repo_root,
        request.loop_state_dir.as_deref(),
    )?;
    if state
        .ralph_store
        .active_run_for_loop(&summary.state_dir)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err("Ralph loop already has an active run".to_owned());
    }

    let active_ralph_runs = state.active_ralph_runs.lock().await;
    if active_ralph_runs.contains_key(&summary.state_dir) {
        return Err("Ralph loop already has an active runner task".to_owned());
    }

    let (requested_max_iterations, requested_no_progress_limit) = effective_ralph_run_limits(
        summary.max_iterations,
        summary.no_progress_limit,
        request.max_iterations,
        request.no_progress_limit,
    );
    let run = state
        .ralph_store
        .create_run(bcode_ralph::RalphRunCreateRequest {
            state_dir: summary.state_dir.clone(),
            session_id: summary.session_id.clone(),
            status: if request.require_approval {
                "awaiting_approval".to_owned()
            } else {
                "running".to_owned()
            },
            requested_max_iterations,
            requested_no_progress_limit,
        })
        .map_err(|error| error.to_string())?;
    let response = RalphRunResponse {
        run: ralph_run_summary(run.clone()),
    };
    let _ = state.ralph_store.append_lifecycle_event_for_summary(
        &summary,
        bcode_ralph::RalphLifecycleEventKind::RunStarted,
        "Started Ralph autonomous runner",
    );
    append_ralph_session_lifecycle(
        state,
        run.session_id.as_deref(),
        summary.loop_name.clone(),
        run.state_dir.clone(),
        "run_started",
        "Started Ralph autonomous runner",
    )
    .await;
    if request.require_approval {
        return Ok(response);
    }

    drop(active_ralph_runs);
    spawn_ralph_runner_task(state, run, summary, runtime_context).await?;
    Ok(response)
}

async fn append_ralph_session_lifecycle(
    state: &ServerState,
    session_id: Option<&str>,
    loop_name: String,
    state_dir: PathBuf,
    kind: &str,
    message: &str,
) {
    let Some(session_id) = session_id.and_then(|session_id| session_id.parse::<SessionId>().ok())
    else {
        return;
    };
    match state
        .sessions
        .append_event(
            session_id,
            SessionEventKind::RalphLifecycle {
                loop_name,
                state_dir,
                kind: kind.to_owned(),
                message: message.to_owned(),
                occurred_at_ms: current_time_ms(),
            },
        )
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append Ralph session lifecycle: {error}"),
    }
}

async fn register_ralph_runtime_work(
    state: &ServerState,
    session_id: Option<SessionId>,
    work_id: RuntimeWorkId,
    label: String,
    run_id: String,
    parent_work_id: Option<RuntimeWorkId>,
) {
    if let Some(session_id) = session_id {
        register_runtime_work(
            state,
            session_id,
            RuntimeWorkSpec::new(
                work_id,
                RuntimeWorkKind::EventDelivery,
                label,
                CancellationHandle::RalphRun {
                    store: state.ralph_store.clone(),
                    run_id,
                },
            )
            .with_parent_work_id(parent_work_id),
        )
        .await;
    }
}

async fn finish_ralph_runtime_work(
    state: &ServerState,
    session_id: Option<SessionId>,
    work_id: RuntimeWorkId,
    status: RuntimeWorkStatus,
    message: Option<String>,
) {
    if let Some(session_id) = session_id {
        finish_registered_runtime_work(state, session_id, work_id, status, message).await;
    }
}

async fn append_ralph_runner_progress(
    state: &ServerState,
    session_id: Option<SessionId>,
    runtime_work_id: &RuntimeWorkId,
    message: &str,
    completed_units: u64,
) {
    if let Some(session_id) = session_id {
        append_runtime_work_progress_event(
            state,
            session_id,
            runtime_work_id.clone(),
            message.to_owned(),
            Some(completed_units),
            Some(1),
        )
        .await;
    }
}

async fn submit_ralph_skeleton_work_turn(
    state: &Arc<ServerState>,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    run_id: &str,
    work_prompt: String,
    runtime_context: Option<ClientRuntimeContext>,
) -> Option<ModelTurnCompletion> {
    let session_id = runtime_session_id?;
    let work_turn_id = RuntimeWorkId::new(format!("ralph:{run_id}:work:1"));
    register_ralph_runtime_work(
        state,
        runtime_session_id,
        work_turn_id.clone(),
        "Ralph work turn 1".to_owned(),
        run_id.to_owned(),
        Some(parent_work_id.clone()),
    )
    .await;
    append_ralph_runner_progress(
        state,
        runtime_session_id,
        parent_work_id,
        "Ralph runner skeleton submitting work turn",
        1,
    )
    .await;
    let completion =
        submit_session_model_turn_and_wait(state, session_id, work_prompt, runtime_context)
            .await
            .unwrap_or_else(|error| {
                ModelTurnCompletion::with_message(ModelTurnOutcome::Error, error.to_string())
            });
    finish_ralph_runtime_work(
        state,
        runtime_session_id,
        work_turn_id,
        runtime_work_status_from_model_outcome(completion.outcome),
        completion.message.clone(),
    )
    .await;
    Some(completion)
}

async fn submit_ralph_skeleton_audit_turn(
    state: &Arc<ServerState>,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    run_id: &str,
    audit_prompt: String,
    runtime_context: Option<ClientRuntimeContext>,
) -> Option<ModelTurnCompletion> {
    let session_id = runtime_session_id?;
    let audit_turn_id = RuntimeWorkId::new(format!("ralph:{run_id}:audit:1"));
    register_ralph_runtime_work(
        state,
        runtime_session_id,
        audit_turn_id.clone(),
        "Ralph audit 1".to_owned(),
        run_id.to_owned(),
        Some(parent_work_id.clone()),
    )
    .await;
    append_ralph_runner_progress(
        state,
        runtime_session_id,
        parent_work_id,
        "Ralph runner skeleton submitting audit turn",
        1,
    )
    .await;
    let completion =
        submit_session_model_turn_and_wait(state, session_id, audit_prompt, runtime_context)
            .await
            .unwrap_or_else(|error| {
                ModelTurnCompletion::with_message(ModelTurnOutcome::Error, error.to_string())
            });
    finish_ralph_runtime_work(
        state,
        runtime_session_id,
        audit_turn_id,
        runtime_work_status_from_model_outcome(completion.outcome),
        completion.message.clone(),
    )
    .await;
    Some(completion)
}

async fn submit_ralph_skeleton_replan_turn(
    state: &Arc<ServerState>,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    run_id: &str,
    replan_prompt: String,
    runtime_context: Option<ClientRuntimeContext>,
) -> Option<ModelTurnCompletion> {
    let session_id = runtime_session_id?;
    let replan_turn_id = RuntimeWorkId::new(format!("ralph:{run_id}:replan:1"));
    register_ralph_runtime_work(
        state,
        runtime_session_id,
        replan_turn_id.clone(),
        "Ralph replan 1".to_owned(),
        run_id.to_owned(),
        Some(parent_work_id.clone()),
    )
    .await;
    append_ralph_runner_progress(
        state,
        runtime_session_id,
        parent_work_id,
        "Ralph runner skeleton submitting replan turn",
        1,
    )
    .await;
    let completion =
        submit_session_model_turn_and_wait(state, session_id, replan_prompt, runtime_context)
            .await
            .unwrap_or_else(|error| {
                ModelTurnCompletion::with_message(ModelTurnOutcome::Error, error.to_string())
            });
    finish_ralph_runtime_work(
        state,
        runtime_session_id,
        replan_turn_id,
        runtime_work_status_from_model_outcome(completion.outcome),
        completion.message.clone(),
    )
    .await;
    Some(completion)
}

fn ralph_run_failure_from_model_completion(
    phase: &'static str,
    completion: &ModelTurnCompletion,
) -> Option<(&'static str, &'static str, String)> {
    if completion.outcome == ModelTurnOutcome::Completed {
        return None;
    }
    let message = completion
        .message
        .clone()
        .unwrap_or_else(|| format!("Ralph {phase} turn ended with {:?}", completion.outcome));
    let lower = message.to_ascii_lowercase();
    if lower.contains("permission") && (lower.contains("denied") || lower.contains("rejected")) {
        return Some(("stopped", "permission_denied", message));
    }
    if lower.contains("question") || lower.contains("needs user") || lower.contains("ask the user")
    {
        return Some(("blocked", "user_question", message));
    }
    if lower.contains("rate") || lower.contains("429") {
        return Some(("blocked", "rate_limited", message));
    }
    if lower.contains("context") && (lower.contains("large") || lower.contains("length")) {
        return Some(("blocked", "context_too_large", message));
    }
    if lower.contains("provider") || lower.contains("unavailable") {
        return Some(("blocked", "provider_unavailable", message));
    }
    if lower.contains("tool") && (lower.contains("failed") || lower.contains("error")) {
        return Some(("blocked", "tool_error", message));
    }
    if lower.contains("session")
        && (lower.contains("missing") || lower.contains("invalid") || lower.contains("state"))
    {
        return Some(("blocked", "session_state_error", message));
    }
    match completion.outcome {
        ModelTurnOutcome::Cancelled => Some(("stopped", "cancelled", message)),
        ModelTurnOutcome::ProviderUnavailable => Some(("blocked", "provider_unavailable", message)),
        ModelTurnOutcome::IdleTimeout => Some(("blocked", "model_idle_timeout", message)),
        ModelTurnOutcome::ToolRoundLimitReached => Some(("blocked", "tool_round_limit", message)),
        ModelTurnOutcome::Error => Some(("blocked", "model_turn_failed", message)),
        ModelTurnOutcome::Completed => None,
    }
}

const fn ralph_iteration_status_from_model_outcome(
    outcome: Option<ModelTurnOutcome>,
) -> &'static str {
    match outcome {
        Some(ModelTurnOutcome::Completed) => "work_completed",
        Some(ModelTurnOutcome::Cancelled) => "work_cancelled",
        Some(ModelTurnOutcome::ProviderUnavailable) => "work_blocked",
        Some(ModelTurnOutcome::IdleTimeout) => "work_timed_out",
        Some(ModelTurnOutcome::ToolRoundLimitReached) => "work_tool_round_limit",
        Some(ModelTurnOutcome::Error) => "work_failed",
        None => "skipped",
    }
}

fn progress_doc_checklist_summary(
    path: &Path,
) -> Result<bcode_ralph::ProgressDocChecklistSummary, std::io::Error> {
    std::fs::read_to_string(path).map(|text| bcode_ralph::analyze_progress_doc_text(&text))
}

fn progress_doc_is_coherent_and_writable(path: &Path) -> bool {
    let Ok(summary) = progress_doc_checklist_summary(path) else {
        return false;
    };
    if summary.checked_count == 0 && summary.unchecked_count == 0 {
        return false;
    }
    std::fs::OpenOptions::new().write(true).open(path).is_ok()
}

fn validate_ralph_runner_inputs(
    summary: &bcode_ralph::RalphLoopSummary,
) -> Option<(&'static str, &'static str, &'static str)> {
    let Some(work_area_path) = summary.work_area_path.as_ref() else {
        return Some(("blocked", "work_area_missing", "Ralph work area is missing"));
    };
    if !work_area_path.is_dir() {
        return Some((
            "blocked",
            "work_area_invalid",
            "Ralph work area does not exist or is not a directory",
        ));
    }
    if !progress_doc_is_coherent_and_writable(&summary.progress_doc_path) {
        return Some((
            "blocked",
            "progress_doc_invalid",
            "Ralph progress doc is missing, corrupt, or not writable",
        ));
    }
    None
}

fn progress_doc_checklist_fingerprint(path: &Path) -> Option<String> {
    progress_doc_checklist_summary(path)
        .ok()
        .map(|summary| summary.checklist_fingerprint.to_string())
}

struct RalphRunnerIterationInput {
    iteration_number: u64,
    work_prompt: String,
    work_completion: Option<ModelTurnCompletion>,
}

async fn record_ralph_skeleton_noop_iteration(
    state: &ServerState,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    summary: &bcode_ralph::RalphLoopSummary,
    run: &bcode_ralph::RalphRunRecord,
    input: RalphRunnerIterationInput,
) -> Option<bcode_ralph::RalphIterationRecord> {
    let iteration_number = input.iteration_number;
    let iteration_work_id =
        RuntimeWorkId::new(format!("ralph:{}:iteration:{iteration_number}", run.run_id));
    register_ralph_runtime_work(
        state,
        runtime_session_id,
        iteration_work_id.clone(),
        format!("Ralph iteration {iteration_number}"),
        run.run_id.clone(),
        Some(parent_work_id.clone()),
    )
    .await;
    append_ralph_runner_progress(
        state,
        runtime_session_id,
        parent_work_id,
        &format!("Ralph runner recording iteration {iteration_number}"),
        1,
    )
    .await;
    let outcome = input
        .work_completion
        .as_ref()
        .map(|completion| completion.outcome);
    let iteration = state
        .ralph_store
        .create_iteration(bcode_ralph::RalphIterationCreateRequest {
            run_id: run.run_id.clone(),
            state_dir: run.state_dir.clone(),
            iteration_number,
            status: ralph_iteration_status_from_model_outcome(outcome).to_owned(),
            checklist_fingerprint_before: Some(
                summary.checklist_summary.checklist_fingerprint.to_string(),
            ),
            checklist_fingerprint_after: progress_doc_checklist_fingerprint(
                &summary.progress_doc_path,
            ),
            work_prompt: Some(input.work_prompt),
            finished_at_ms: Some(current_time_ms()),
            stop_reason: outcome.map(|outcome| format!("model_turn_{outcome:?}").to_lowercase()),
            error_message: input
                .work_completion
                .and_then(|completion| completion.message),
        });
    finish_ralph_runtime_work(
        state,
        runtime_session_id,
        iteration_work_id,
        RuntimeWorkStatus::Completed,
        Some(format!("Ralph iteration {iteration_number} recorded")),
    )
    .await;
    iteration.ok()
}
struct RalphValidationExecution {
    status: String,
    exit_code: Option<i64>,
    output_ref: Option<String>,
    error_message: Option<String>,
}

async fn execute_ralph_validation_command(
    work_area: PathBuf,
    command: String,
) -> RalphValidationExecution {
    tokio::task::spawn_blocking(move || {
        let output = Command::new("sh")
            .arg("-lc")
            .arg(&command)
            .current_dir(work_area)
            .output();
        match output {
            Ok(output) => {
                let exit_code = output.status.code().map(i64::from);
                let mut combined = String::new();
                combined.push_str(&String::from_utf8_lossy(&output.stdout));
                combined.push_str(&String::from_utf8_lossy(&output.stderr));
                if combined.len() > 8_192 {
                    combined.truncate(8_192);
                }
                RalphValidationExecution {
                    status: if output.status.success() {
                        "passed".to_owned()
                    } else {
                        "failed".to_owned()
                    },
                    exit_code,
                    output_ref: (!combined.trim().is_empty()).then_some(combined),
                    error_message: None,
                }
            }
            Err(error) => RalphValidationExecution {
                status: "error".to_owned(),
                exit_code: None,
                output_ref: None,
                error_message: Some(error.to_string()),
            },
        }
    })
    .await
    .unwrap_or_else(|error| RalphValidationExecution {
        status: "error".to_owned(),
        exit_code: None,
        output_ref: None,
        error_message: Some(error.to_string()),
    })
}

async fn run_ralph_iteration_validations(
    state: &ServerState,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    summary: &bcode_ralph::RalphLoopSummary,
    run: &bcode_ralph::RalphRunRecord,
    iteration: &bcode_ralph::RalphIterationRecord,
) -> bool {
    let Some(work_area) = summary.work_area_path.clone() else {
        return true;
    };
    let commands = state
        .ralph_store
        .list_validation_commands(&summary.state_dir)
        .unwrap_or_default();
    for (index, command) in commands.into_iter().enumerate() {
        let validation_work_id =
            RuntimeWorkId::new(format!("ralph:{}:validation:{}", run.run_id, index + 1));
        register_ralph_runtime_work(
            state,
            runtime_session_id,
            validation_work_id.clone(),
            format!("Ralph validation {}", index + 1),
            run.run_id.clone(),
            Some(parent_work_id.clone()),
        )
        .await;
        let execution =
            execute_ralph_validation_command(work_area.clone(), command.command.clone()).await;
        let passed = execution.status == "passed";
        let runtime_status = if passed {
            RuntimeWorkStatus::Completed
        } else {
            RuntimeWorkStatus::Failed
        };
        let validation_message = if passed {
            Some(format!("validation passed: {}", command.command))
        } else {
            Some(format!("validation failed: {}", command.command))
        };
        let _ = state
            .ralph_store
            .create_validation(bcode_ralph::RalphValidationCreateRequest {
                iteration_id: iteration.iteration_id.clone(),
                command: command.command,
                status: execution.status,
                exit_code: execution.exit_code,
                output_ref: execution.output_ref,
                finished_at_ms: Some(current_time_ms()),
                error_message: execution.error_message,
            });
        finish_ralph_runtime_work(
            state,
            runtime_session_id,
            validation_work_id,
            runtime_status,
            validation_message,
        )
        .await;
        if !passed {
            return false;
        }
    }
    true
}

async fn finish_ralph_runner_lifecycle(
    state: &ServerState,
    run: &bcode_ralph::RalphRunRecord,
    loop_name: String,
    final_status: RuntimeWorkStatus,
    final_message: Option<&str>,
) {
    let status_label = match final_status {
        RuntimeWorkStatus::Completed => "completed",
        RuntimeWorkStatus::Cancelled => "cancelled",
        RuntimeWorkStatus::Failed => "blocked",
        RuntimeWorkStatus::TimedOut => "timed out",
        RuntimeWorkStatus::Running | RuntimeWorkStatus::Queued | RuntimeWorkStatus::Cancelling => {
            "stopped"
        }
    };
    let message = final_message.map_or_else(
        || format!("Ralph autonomous runner {status_label}"),
        |message| format!("Ralph autonomous runner {status_label}: {message}"),
    );
    let _ = state.ralph_store.append_lifecycle_event_for_state_dir(
        &run.state_dir,
        bcode_ralph::RalphLifecycleEventKind::RunFinished,
        &message,
    );
    append_ralph_session_lifecycle(
        state,
        run.session_id.as_deref(),
        loop_name,
        run.state_dir.clone(),
        "run_finished",
        &message,
    )
    .await;
}

fn build_ralph_work_prompt(summary: &bcode_ralph::RalphLoopSummary) -> String {
    bcode_ralph::build_prompt(summary, bcode_ralph::RalphPromptKind::Work).unwrap_or_else(|error| {
        format!(
            "Ralph work prompt could not read the progress doc; report this blocker and stop safely. Error: {error}"
        )
    })
}

fn ralph_stop_decision_after_audit(
    store: &bcode_ralph::RalphStateStore,
    summary: &bcode_ralph::RalphLoopSummary,
    run: &bcode_ralph::RalphRunRecord,
    iteration: &bcode_ralph::RalphIterationRecord,
) -> Result<bcode_ralph::RalphStopDecision, std::io::Error> {
    let checklist_summary = progress_doc_checklist_summary(&summary.progress_doc_path)?;
    let iterations = store
        .list_iterations_for_run(&run.run_id)
        .unwrap_or_default();
    let iteration_count = u64::try_from(iterations.len()).unwrap_or(u64::MAX);
    let no_progress_count = consecutive_no_progress_iterations(&iterations, iteration);
    Ok(bcode_ralph::decide_stop(
        bcode_ralph::RalphStopDecisionInput {
            status: bcode_ralph::RalphLoopStatus::Running,
            iteration_count,
            max_iterations: run.requested_max_iterations.unwrap_or(0),
            no_progress_count,
            no_progress_limit: run.requested_no_progress_limit.unwrap_or(0),
            checklist_summary,
            permission_denied: false,
            validation_blocked: false,
            user_question: false,
        },
    ))
}

fn consecutive_no_progress_iterations(
    iterations: &[bcode_ralph::RalphIterationRecord],
    fallback_iteration: &bcode_ralph::RalphIterationRecord,
) -> u64 {
    let mut count = 0_u64;
    let source = if iterations.is_empty() {
        std::slice::from_ref(fallback_iteration)
    } else {
        iterations
    };
    for iteration in source.iter().rev() {
        if iteration.checklist_fingerprint_before.is_some()
            && iteration.checklist_fingerprint_before == iteration.checklist_fingerprint_after
        {
            count = count.saturating_add(1);
        } else {
            break;
        }
    }
    count
}

const fn ralph_run_terminal_from_decision(
    decision: bcode_ralph::RalphStopDecision,
) -> (&'static str, &'static str, &'static str) {
    match decision {
        bcode_ralph::RalphStopDecision::Continue => (
            "running",
            "continue",
            "Ralph iteration completed and loop will continue",
        ),
        bcode_ralph::RalphStopDecision::CompletionCandidate => (
            "done",
            "progress_doc_complete",
            "Ralph progress doc appears complete after audit",
        ),
        bcode_ralph::RalphStopDecision::MaxIterations => (
            "stopped",
            "max_iterations",
            "Ralph maximum iteration count reached",
        ),
        bcode_ralph::RalphStopDecision::RepeatedNoProgress => (
            "blocked",
            "no_progress",
            "Ralph repeated no-progress threshold reached",
        ),
        bcode_ralph::RalphStopDecision::PermissionDenied => {
            ("blocked", "permission_denied", "Ralph permission denied")
        }
        bcode_ralph::RalphStopDecision::ValidationBlocked => (
            "blocked",
            "validation_blocked",
            "Ralph validation blocked the loop",
        ),
        bcode_ralph::RalphStopDecision::UserQuestion => {
            ("blocked", "user_question", "Ralph needs a user answer")
        }
        bcode_ralph::RalphStopDecision::TerminalStatus => (
            "stopped",
            "terminal_status",
            "Ralph loop was already terminal",
        ),
    }
}

async fn submit_ralph_audit_after_validation(
    state: &Arc<ServerState>,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    summary: &bcode_ralph::RalphLoopSummary,
    run: &bcode_ralph::RalphRunRecord,
    iteration: Option<&bcode_ralph::RalphIterationRecord>,
    runtime_context: Option<ClientRuntimeContext>,
) -> Option<ModelTurnCompletion> {
    let prompt = bcode_ralph::build_prompt(summary, bcode_ralph::RalphPromptKind::Audit)
        .unwrap_or_else(|error| {
            format!(
                "Ralph audit prompt could not read the progress doc; report this blocker and stop safely. Error: {error}"
            )
        });
    if let Some(iteration) = iteration {
        let _ = state.ralph_store.update_iteration_prompts(
            &iteration.iteration_id,
            Some(prompt.clone()),
            None,
        );
    }
    submit_ralph_skeleton_audit_turn(
        state,
        runtime_session_id,
        parent_work_id,
        &run.run_id,
        prompt,
        runtime_context,
    )
    .await
}

async fn submit_ralph_replan_after_audit(
    state: &Arc<ServerState>,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    summary: &bcode_ralph::RalphLoopSummary,
    run: &bcode_ralph::RalphRunRecord,
    iteration: Option<&bcode_ralph::RalphIterationRecord>,
    runtime_context: Option<ClientRuntimeContext>,
) -> Option<ModelTurnCompletion> {
    let prompt = bcode_ralph::build_prompt(summary, bcode_ralph::RalphPromptKind::Replan)
        .unwrap_or_else(|error| {
            format!(
                "Ralph replan prompt could not read the progress doc; report this blocker and stop safely. Error: {error}"
            )
        });
    if let Some(iteration) = iteration {
        let _ = state.ralph_store.update_iteration_prompts(
            &iteration.iteration_id,
            None,
            Some(prompt.clone()),
        );
    }
    submit_ralph_skeleton_replan_turn(
        state,
        runtime_session_id,
        parent_work_id,
        &run.run_id,
        prompt,
        runtime_context,
    )
    .await
}

async fn apply_ralph_post_audit_decision(
    state: &Arc<ServerState>,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    summary: &bcode_ralph::RalphLoopSummary,
    run: &bcode_ralph::RalphRunRecord,
    iteration: Option<&bcode_ralph::RalphIterationRecord>,
    runtime_context: Option<ClientRuntimeContext>,
) -> (&'static str, &'static str, &'static str) {
    let Some(decision) = iteration.and_then(|iteration| {
        ralph_stop_decision_after_audit(&state.ralph_store, summary, run, iteration).ok()
    }) else {
        return (
            "blocked",
            "progress_doc_unreadable",
            "Ralph progress doc could not be read after audit",
        );
    };
    if decision != bcode_ralph::RalphStopDecision::Continue {
        return ralph_run_terminal_from_decision(decision);
    }
    if ralph_run_cancel_requested(&state.ralph_store, run) {
        return ("stopped", "cancelled", "Ralph run cancelled");
    }
    let replan_completion = submit_ralph_replan_after_audit(
        state,
        runtime_session_id,
        parent_work_id,
        summary,
        run,
        iteration,
        runtime_context,
    )
    .await;
    if replan_completion
        .as_ref()
        .map(|completion| completion.outcome)
        != Some(ModelTurnOutcome::Completed)
    {
        return (
            "blocked",
            "replan_failed",
            "Ralph replan turn did not complete successfully",
        );
    }
    if ralph_run_cancel_requested(&state.ralph_store, run) {
        return ("stopped", "cancelled", "Ralph run cancelled");
    }
    if !progress_doc_is_coherent_and_writable(&summary.progress_doc_path) {
        return (
            "blocked",
            "progress_doc_invalid",
            "Ralph progress doc was not coherent and writable after replan",
        );
    }
    (
        "running",
        "continue",
        "Ralph replan completed and loop will continue",
    )
}

struct RalphIterationCompletion {
    continue_loop: bool,
    runtime_status: RuntimeWorkStatus,
    message: String,
}

fn ralph_run_cancel_requested(
    store: &bcode_ralph::RalphStateStore,
    run: &bcode_ralph::RalphRunRecord,
) -> bool {
    store
        .active_run_for_loop(&run.state_dir)
        .ok()
        .flatten()
        .is_some_and(|active_run| active_run.cancel_requested)
}

fn ralph_cancelled_iteration_completion(
    store: &bcode_ralph::RalphStateStore,
    run: &bcode_ralph::RalphRunRecord,
) -> RalphIterationCompletion {
    let _ = store.update_run_status(
        &run.run_id,
        "stopped",
        Some(current_time_ms()),
        Some("cancelled"),
        None,
    );
    RalphIterationCompletion {
        continue_loop: false,
        runtime_status: RuntimeWorkStatus::Cancelled,
        message: "Ralph run cancelled".to_owned(),
    }
}

async fn complete_ralph_skeleton_after_iteration(
    state: &Arc<ServerState>,
    runtime_session_id: Option<SessionId>,
    parent_work_id: &RuntimeWorkId,
    summary: &bcode_ralph::RalphLoopSummary,
    run: &bcode_ralph::RalphRunRecord,
    iteration: Option<&bcode_ralph::RalphIterationRecord>,
    runtime_context: Option<ClientRuntimeContext>,
) -> RalphIterationCompletion {
    let validation_passed = if let Some(iteration) = iteration {
        run_ralph_iteration_validations(
            state,
            runtime_session_id,
            parent_work_id,
            summary,
            run,
            iteration,
        )
        .await
    } else {
        true
    };
    if !validation_passed {
        let _ = state.ralph_store.update_run_status(
            &run.run_id,
            "blocked",
            Some(current_time_ms()),
            Some("validation_failed"),
            Some("Ralph validation command failed"),
        );
        return RalphIterationCompletion {
            continue_loop: false,
            runtime_status: RuntimeWorkStatus::Failed,
            message: "Ralph validation command failed".to_owned(),
        };
    }
    if ralph_run_cancel_requested(&state.ralph_store, run) {
        return ralph_cancelled_iteration_completion(&state.ralph_store, run);
    }
    let audit_completion = submit_ralph_audit_after_validation(
        state,
        runtime_session_id,
        parent_work_id,
        summary,
        run,
        iteration,
        runtime_context.clone(),
    )
    .await;
    let audit_outcome = audit_completion
        .as_ref()
        .map(|completion| completion.outcome);
    if ralph_run_cancel_requested(&state.ralph_store, run) {
        return ralph_cancelled_iteration_completion(&state.ralph_store, run);
    }
    let (run_status, stop_reason, error_message): (&str, &str, String) =
        if audit_outcome == Some(ModelTurnOutcome::Completed) {
            let (run_status, stop_reason, error_message) = apply_ralph_post_audit_decision(
                state,
                runtime_session_id,
                parent_work_id,
                summary,
                run,
                iteration,
                runtime_context.clone(),
            )
            .await;
            (run_status, stop_reason, error_message.to_owned())
        } else if let Some((run_status, stop_reason, error_message)) = audit_completion
            .as_ref()
            .and_then(|completion| ralph_run_failure_from_model_completion("audit", completion))
        {
            (run_status, stop_reason, error_message)
        } else {
            (
                "blocked",
                "audit_failed",
                "Ralph audit turn did not complete successfully".to_owned(),
            )
        };
    let finished_at_ms = (run_status != "running").then(current_time_ms);
    let stop_reason_value = (run_status != "running").then_some(stop_reason);
    let error_message_value = (run_status != "running").then_some(error_message.as_str());
    let _ = state.ralph_store.update_run_status(
        &run.run_id,
        run_status,
        finished_at_ms,
        stop_reason_value,
        error_message_value,
    );
    let runtime_status = match run_status {
        "done" | "stopped" => RuntimeWorkStatus::Completed,
        "running" => RuntimeWorkStatus::Running,
        _ => RuntimeWorkStatus::Failed,
    };
    RalphIterationCompletion {
        continue_loop: run_status == "running",
        runtime_status,
        message: error_message.clone(),
    }
}

#[allow(clippy::too_many_lines)]
async fn run_ralph_runner_skeleton(
    state: Arc<ServerState>,
    run: bcode_ralph::RalphRunRecord,
    summary: bcode_ralph::RalphLoopSummary,
    runtime_context: Option<ClientRuntimeContext>,
) {
    let runtime_session_id = run
        .session_id
        .as_deref()
        .and_then(|session_id| session_id.parse::<SessionId>().ok());
    let runtime_work_id = RuntimeWorkId::new(format!("ralph:{}", run.run_id));
    register_ralph_runtime_work(
        &state,
        runtime_session_id,
        runtime_work_id.clone(),
        "Ralph autonomous runner".to_owned(),
        run.run_id.clone(),
        None,
    )
    .await;
    append_ralph_runner_progress(
        &state,
        runtime_session_id,
        &runtime_work_id,
        "Ralph runner skeleton started",
        0,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(250)).await;
    let (final_status, final_message) = loop {
        let next_iteration_number = state
            .ralph_store
            .list_iterations_for_run(&run.run_id)
            .map_or(1, |iterations| {
                u64::try_from(iterations.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1)
            });
        let active_run = state
            .ralph_store
            .active_run_for_loop(&run.state_dir)
            .ok()
            .flatten();
        append_ralph_runner_progress(
            &state,
            runtime_session_id,
            &runtime_work_id,
            &format!("Ralph runner checking cancellation before iteration {next_iteration_number}"),
            next_iteration_number.saturating_sub(1),
        )
        .await;
        if let Some((run_status, stop_reason, error_message)) =
            validate_ralph_runner_inputs(&summary)
        {
            let _ = state.ralph_store.update_run_status(
                &run.run_id,
                run_status,
                Some(current_time_ms()),
                Some(stop_reason),
                Some(error_message),
            );
            break (RuntimeWorkStatus::Failed, Some(error_message.to_owned()));
        }
        if active_run.as_ref().is_some_and(|run| run.cancel_requested) {
            append_ralph_runner_progress(
                &state,
                runtime_session_id,
                &runtime_work_id,
                "Ralph runner observed cancellation",
                next_iteration_number.saturating_sub(1),
            )
            .await;
            let _ = state.ralph_store.update_run_status(
                &run.run_id,
                "stopped",
                Some(current_time_ms()),
                Some("cancelled"),
                None,
            );
            break (
                RuntimeWorkStatus::Cancelled,
                Some("Ralph run cancelled".to_owned()),
            );
        }
        let work_prompt = build_ralph_work_prompt(&summary);
        let work_completion = submit_ralph_skeleton_work_turn(
            &state,
            runtime_session_id,
            &runtime_work_id,
            &run.run_id,
            work_prompt.clone(),
            runtime_context.clone(),
        )
        .await;
        let work_failure = work_completion
            .as_ref()
            .and_then(|completion| ralph_run_failure_from_model_completion("work", completion));
        let iteration = record_ralph_skeleton_noop_iteration(
            &state,
            runtime_session_id,
            &runtime_work_id,
            &summary,
            &run,
            RalphRunnerIterationInput {
                iteration_number: next_iteration_number,
                work_prompt,
                work_completion,
            },
        )
        .await;
        if let Some((run_status, stop_reason, error_message)) = work_failure {
            let _ = state.ralph_store.update_run_status(
                &run.run_id,
                run_status,
                Some(current_time_ms()),
                Some(stop_reason),
                Some(error_message.as_str()),
            );
            let runtime_status = if run_status == "stopped" {
                RuntimeWorkStatus::Completed
            } else {
                RuntimeWorkStatus::Failed
            };
            break (runtime_status, Some(error_message));
        }
        if ralph_run_cancel_requested(&state.ralph_store, &run) {
            let completion = ralph_cancelled_iteration_completion(&state.ralph_store, &run);
            break (completion.runtime_status, Some(completion.message));
        }
        let completion = complete_ralph_skeleton_after_iteration(
            &state,
            runtime_session_id,
            &runtime_work_id,
            &summary,
            &run,
            iteration.as_ref(),
            runtime_context.clone(),
        )
        .await;
        if completion.continue_loop {
            append_ralph_runner_progress(
                &state,
                runtime_session_id,
                &runtime_work_id,
                &format!("Ralph iteration {next_iteration_number} complete; continuing"),
                next_iteration_number,
            )
            .await;
            continue;
        }
        break (completion.runtime_status, Some(completion.message));
    };
    finish_ralph_runner_lifecycle(
        &state,
        &run,
        summary.loop_name,
        final_status,
        final_message.as_deref(),
    )
    .await;
    finish_ralph_runtime_work(
        &state,
        runtime_session_id,
        runtime_work_id,
        final_status,
        final_message,
    )
    .await;
    state.active_ralph_runs.lock().await.remove(&run.state_dir);
}

async fn handle_cancel_ralph_loop(
    request_id: u64,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    request: RalphCancelRequest,
) -> Result<(), ServerError> {
    match resolve_ralph_loop(
        &state.ralph_store,
        &request.repo_root,
        request.loop_state_dir.as_deref(),
    )
    .and_then(|summary| {
        resolve_ralph_cancel_target(&state.ralph_store, &summary.state_dir, request.run_id)
    }) {
        Ok(run) => match state.ralph_store.request_run_cancel(&run.run_id) {
            Ok(()) => {
                if let Some(session_id) = run
                    .session_id
                    .as_deref()
                    .and_then(|session_id| session_id.parse::<SessionId>().ok())
                {
                    let _cancelled =
                        enqueue_cancel_turn_command(state, session_id, true, None).await?;
                }
                let response = RalphCancelResponse {
                    run: RalphRunSummary {
                        cancel_requested: true,
                        updated_at_ms: current_time_ms(),
                        ..ralph_run_summary(run)
                    },
                    cancel_requested: true,
                };
                send_response(
                    writer,
                    request_id,
                    Response::Ok(ResponsePayload::RalphRunCancelled(response)),
                )
                .await
            }
            Err(error) => {
                send_response(
                    writer,
                    request_id,
                    Response::Err(ErrorResponse::new(
                        "ralph_run_cancel_failed",
                        error.to_string(),
                    )),
                )
                .await
            }
        },
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("ralph_run_cancel_failed", error)),
            )
            .await
        }
    }
}

async fn handle_list_ralph_runs(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    request: RalphListRunsRequest,
) -> Result<(), ServerError> {
    match resolve_ralph_loop(
        &state.ralph_store,
        &request.repo_root,
        request.loop_state_dir.as_deref(),
    )
    .and_then(|summary| {
        let runs = state
            .ralph_store
            .list_runs_for_loop(&summary.state_dir)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(ralph_run_summary)
            .collect();
        Ok(RalphListRunsResponse {
            loop_summary: Some(ralph_status_summary(&state.ralph_store, summary)),
            runs,
        })
    }) {
        Ok(response) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::RalphRunsListed(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("ralph_runs_list_failed", error)),
            )
            .await
        }
    }
}

async fn handle_list_ralph_iterations(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    request: RalphListIterationsRequest,
) -> Result<(), ServerError> {
    match resolve_ralph_loop(
        &state.ralph_store,
        &request.repo_root,
        request.loop_state_dir.as_deref(),
    )
    .and_then(|summary| {
        let runs = state
            .ralph_store
            .list_runs_for_loop(&summary.state_dir)
            .map_err(|error| error.to_string())?;
        let run = request
            .run_id
            .as_deref()
            .and_then(|run_id| runs.iter().find(|run| run.run_id == run_id))
            .or_else(|| runs.first());
        let (iterations, validations) = run
            .map_or_else(
                || Ok((Vec::new(), Vec::new())),
                |run| {
                    let iteration_records =
                        state.ralph_store.list_iterations_for_run(&run.run_id)?;
                    let mut validation_records = Vec::new();
                    for iteration in &iteration_records {
                        validation_records.extend(
                            state
                                .ralph_store
                                .list_validations_for_iteration(&iteration.iteration_id)?,
                        );
                    }
                    Ok((
                        iteration_records
                            .into_iter()
                            .map(ralph_iteration_summary)
                            .collect(),
                        validation_records
                            .into_iter()
                            .map(ralph_validation_summary)
                            .collect(),
                    ))
                },
            )
            .map_err(|error: bcode_ralph::RalphStateError| error.to_string())?;
        Ok(RalphListIterationsResponse {
            loop_summary: Some(ralph_status_summary(&state.ralph_store, summary)),
            run: run.cloned().map(ralph_run_summary),
            iterations,
            validations,
        })
    }) {
        Ok(response) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::RalphIterationsListed(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("ralph_iterations_list_failed", error)),
            )
            .await
        }
    }
}

async fn handle_ralph_run_status(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    request: RalphRunStatusRequest,
) -> Result<(), ServerError> {
    match resolve_ralph_loop(
        &state.ralph_store,
        &request.repo_root,
        request.loop_state_dir.as_deref(),
    )
    .and_then(|summary| {
        let active_run = state
            .ralph_store
            .active_run_for_loop(&summary.state_dir)
            .map_err(|error| error.to_string())?
            .map(ralph_run_summary);
        let interrupted_runs = state
            .ralph_store
            .interrupted_runs_for_loop(&summary.state_dir)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(ralph_run_summary)
            .collect();
        Ok(RalphRunStatusResponse {
            loop_summary: Some(ralph_status_summary(&state.ralph_store, summary)),
            active_run,
            interrupted_runs,
        })
    }) {
        Ok(response) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::RalphRunStatus(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("ralph_run_status_failed", error)),
            )
            .await
        }
    }
}

fn resolve_ralph_loop(
    store: &bcode_ralph::RalphStateStore,
    repo_root: &Path,
    loop_state_dir: Option<&Path>,
) -> Result<bcode_ralph::RalphLoopSummary, String> {
    let summary = store
        .latest_loop(repo_root)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no Ralph loop exists for repository".to_owned())?;
    if let Some(loop_state_dir) = loop_state_dir
        && summary.state_dir != loop_state_dir
    {
        return Err("requested Ralph loop is not the latest loop for repository".to_owned());
    }
    Ok(summary)
}

fn resolve_ralph_cancel_target(
    store: &bcode_ralph::RalphStateStore,
    state_dir: &Path,
    run_id: Option<String>,
) -> Result<bcode_ralph::RalphRunRecord, String> {
    let active_run = store
        .active_run_for_loop(state_dir)
        .map_err(|error| error.to_string())?;
    match (run_id, active_run) {
        (Some(run_id), Some(run)) if run.run_id == run_id => Ok(run),
        (Some(_), Some(_)) => Err("requested Ralph run is not active for loop".to_owned()),
        (Some(_), None) => Err("requested Ralph run is not active".to_owned()),
        (None, Some(run)) => Ok(run),
        (None, None) => Err("Ralph loop has no active run".to_owned()),
    }
}

fn ralph_validation_summary(
    validation: bcode_ralph::RalphValidationRecord,
) -> RalphValidationSummary {
    RalphValidationSummary {
        validation_id: validation.validation_id,
        iteration_id: validation.iteration_id,
        command: validation.command,
        status: validation.status,
        exit_code: validation.exit_code,
        output_ref: validation.output_ref,
        finished_at_ms: validation.finished_at_ms,
        error_message: validation.error_message,
    }
}

fn ralph_status_summary(
    store: &bcode_ralph::RalphStateStore,
    summary: bcode_ralph::RalphLoopSummary,
) -> RalphStatusSummary {
    let validation_commands = store
        .list_validation_commands(&summary.state_dir)
        .map(|commands| {
            commands
                .into_iter()
                .map(|command| command.command)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    RalphStatusSummary {
        loop_name: summary.loop_name,
        status: summary.status,
        state_dir: summary.state_dir,
        progress_doc_path: summary.progress_doc_path,
        work_area_path: summary.work_area_path,
        session_id: summary.session_id,
        iteration_count: summary.iteration_count,
        next_action: summary.next_action,
        checked_count: summary.checklist_summary.checked_count,
        unchecked_count: summary.checklist_summary.unchecked_count,
        validation_commands,
    }
}

fn ralph_run_summary(run: bcode_ralph::RalphRunRecord) -> RalphRunSummary {
    let runtime_work_id = Some(format!("ralph:{}", run.run_id));
    RalphRunSummary {
        run_id: run.run_id,
        state_dir: run.state_dir,
        session_id: run.session_id,
        runtime_work_id,
        status: run.status,
        requested_max_iterations: run.requested_max_iterations,
        requested_no_progress_limit: run.requested_no_progress_limit,
        cancel_requested: run.cancel_requested,
        started_at_ms: run.started_at_ms,
        updated_at_ms: run.updated_at_ms,
        finished_at_ms: run.finished_at_ms,
        stop_reason: run.stop_reason,
        error_message: run.error_message,
    }
}

fn ralph_iteration_summary(iteration: bcode_ralph::RalphIterationRecord) -> RalphIterationSummary {
    RalphIterationSummary {
        iteration_id: iteration.iteration_id,
        run_id: iteration.run_id,
        iteration_number: iteration.iteration_number,
        status: iteration.status,
        stop_reason: iteration.stop_reason,
        error_message: iteration.error_message,
        finished_at_ms: iteration.finished_at_ms,
    }
}

async fn handle_record_ralph_lifecycle(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    request: RalphLifecycleRequest,
) -> Result<(), ServerError> {
    match state
        .sessions
        .append_event(
            request.session_id,
            SessionEventKind::RalphLifecycle {
                loop_name: request.loop_name,
                state_dir: request.state_dir,
                kind: request.kind,
                message: request.message,
                occurred_at_ms: request.occurred_at_ms,
            },
        )
        .await
    {
        Ok(event) => {
            publish_session_event(state, &event).await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::RalphLifecycleRecorded { event }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "ralph_lifecycle_record_failed",
                    error.to_string(),
                )),
            )
            .await
        }
    }
}

async fn handle_create_worktree(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    request: WorktreeCreateRequest,
) -> Result<(), ServerError> {
    if let Some(session_id) = request.attach_session_id
        && state.session_has_active_turn(session_id).await
    {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "session_busy",
                format!("session has an active model turn: {session_id}"),
            )),
        )
        .await;
    }
    let cwd = request.cwd.clone().unwrap_or_else(current_request_cwd);
    let config_paths = bcode_config::default_config_paths_from(&cwd);
    let config = bcode_config::load_config_from_paths(&config_paths)?;
    match bcode_worktree::create_worktree(&config, &request, &cwd) {
        Ok(mut response) => {
            if let Some(session_id) = request.attach_session_id {
                let changed = if let Some(event) = state
                    .sessions
                    .change_session_working_directory(session_id, response.path.clone())
                    .await?
                {
                    publish_session_event(state, &event).await;
                    true
                } else {
                    false
                };
                let session = state.sessions.session_summary(session_id).await?;
                if changed {
                    state
                        .session_catalog
                        .upsert_native_session(session.clone())
                        .await;
                }
                response.session = Some(session);
            } else if request.new_session {
                let session = state
                    .sessions
                    .create_session(Some(request.name), response.path.clone())
                    .await?;
                state
                    .session_catalog
                    .upsert_native_session(session.clone())
                    .await;
                response.session = Some(session);
            }
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::WorktreeCreated(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "worktree_create_command_failed",
                    error.to_string(),
                )),
            )
            .await
        }
    }
}

async fn handle_remove_worktree(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    request: WorktreeRemoveRequest,
) -> Result<(), ServerError> {
    let cwd = request.cwd.clone().unwrap_or_else(current_request_cwd);
    let sessions = state.sessions.cached_sessions(&cwd).await;
    if let Some(session) = sessions
        .iter()
        .find(|session| path_is_inside(&session.working_directory, &request.path))
    {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "worktree_remove_command_failed",
                format!(
                    "session {} is rooted inside worktree {}; move or delete it before removal",
                    session.id,
                    request.path.display()
                ),
            )),
        )
        .await;
    }
    match bcode_worktree::remove_worktree(&cwd, &request.path, request.force) {
        Ok(response) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::WorktreeRemoved(response)),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "worktree_remove_command_failed",
                    error.to_string(),
                )),
            )
            .await
        }
    }
}

fn current_request_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn path_is_inside(path: &Path, root: &Path) -> bool {
    let normalized_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let normalized_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    normalized_path == normalized_root || normalized_path.starts_with(normalized_root)
}

async fn handle_rename_session(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    name: Option<String>,
) -> Result<(), ServerError> {
    match state.sessions.rename_session(session_id, name).await {
        Ok(event) => {
            publish_session_event(state, &event).await;
            let session = state.sessions.session_summary(session_id).await?;
            state
                .session_catalog
                .upsert_native_session(session.clone())
                .await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionRenamed { session }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "session_rename_failed",
                    error.to_string(),
                )),
            )
            .await
        }
    }
}

async fn handle_delete_session(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    if state.session_has_active_turn(session_id).await {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "session_busy",
                format!("session has an active model turn: {session_id}"),
            )),
        )
        .await;
    }
    match state.sessions.delete_session(session_id).await {
        Ok(session) => {
            state
                .session_model_selections
                .lock()
                .await
                .remove(&session_id);
            state
                .session_agent_selections
                .lock()
                .await
                .remove(&session_id);
            state
                .session_catalog
                .remove_native_session(session_id)
                .await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionDeleted { session }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "session_delete_failed",
                    error.to_string(),
                )),
            )
            .await
        }
    }
}

async fn handle_clone_session(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    source_session_id: SessionId,
    name: Option<String>,
) -> Result<(), ServerError> {
    match state.sessions.clone_session(source_session_id, name).await {
        Ok(result) => {
            state
                .session_catalog
                .upsert_native_session(result.session.clone())
                .await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionForked {
                    session: result.session,
                    draft: result.draft,
                }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "session_clone_failed",
                    error.to_string(),
                )),
            )
            .await
        }
    }
}

async fn handle_fork_session(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    source_session_id: SessionId,
    prompt_sequence: u64,
    name: Option<String>,
) -> Result<(), ServerError> {
    match state
        .sessions
        .fork_session_from_prompt(source_session_id, prompt_sequence, name)
        .await
    {
        Ok(result) => {
            state
                .session_catalog
                .upsert_native_session(result.session.clone())
                .await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionForked {
                    session: result.session,
                    draft: result.draft,
                }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("session_fork_failed", error.to_string())),
            )
            .await
        }
    }
}

/// Handle an explicit full-history request.
///
/// This endpoint performs a complete canonical event read and is intended only for
/// export/debug/history commands. Normal UI, attach, prompt/model-context, catalog,
/// and maintenance flows must use bounded pages, projection windows, or typed read models.
async fn handle_session_history(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    if let Some(active_namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    match state.sessions.session_history(session_id).await {
        Ok(history) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionHistory {
                    session_id,
                    history,
                }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await
        }
    }
}

async fn handle_session_history_page(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    query: bcode_session_models::SessionHistoryQuery,
) -> Result<(), ServerError> {
    if let Some(active_namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    match state.sessions.session_history_page(session_id, query).await {
        Ok(page) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionHistoryPage { page }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await
        }
    }
}

async fn send_incompatible_active_session_response(
    writer: &SharedWriter,
    request_id: u64,
    active_namespace: &str,
) -> Result<(), ServerError> {
    send_response(
        writer,
        request_id,
        Response::Err(ErrorResponse::new(
            "session_incompatible_active_client",
            format!(
                "session is active for daemon compatibility namespace {active_namespace}; reconnect with a matching client or wait until the session is inactive"
            ),
        )),
    )
    .await
}

fn server_session_error_response(error: &ServerError) -> ErrorResponse {
    match error {
        ServerError::Session(error) => session_error_response(error),
        ServerError::SessionDb(error) if database_error_requires_repair(error) => {
            ErrorResponse::new("session_repair_required", error.to_string())
        }
        _ => ErrorResponse::new("session_not_found", error.to_string()),
    }
}

fn session_error_response(error: &bcode_session::SessionError) -> ErrorResponse {
    match error {
        bcode_session::SessionError::Lease(
            bcode_session::lease::SessionLeaseError::OwnedByOtherDaemon { .. },
        ) => ErrorResponse::new("session_active_elsewhere", error.to_string()),
        bcode_session::SessionError::Db(error) if database_error_requires_repair(error) => {
            ErrorResponse::new("session_repair_required", error.to_string())
        }
        _ => ErrorResponse::new("session_not_found", error.to_string()),
    }
}

fn database_error_requires_repair(error: &bcode_session::db::SessionDbError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("short read on wal frame")
        || message.contains("database disk image is malformed")
        || message.contains("file is not a database")
}

async fn handle_attach_session(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
    session_id: SessionId,
) -> Result<(), ServerError> {
    recover_abandoned_session_runtime_work_best_effort(state, session_id).await;
    let client_namespace = state.client_session_namespace(client_id).await;
    if let Err(active_namespace) = state
        .try_activate_session_namespace(session_id, client_namespace)
        .await
    {
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    match state.sessions.attach_session(session_id, client_id).await {
        Ok(attachment) => {
            restore_active_skills_from_history(&attachment.history, state, session_id).await;
            *attached_session = Some(session_id);
            state.attach_client_session(client_id, session_id).await;
            let draft = state.sessions.session_composer_draft(session_id).await?;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::Attached {
                    session_id,
                    session: attachment.session,
                    history: compact_attach_history(attachment.history),
                    input_history: attachment.input_history,
                    import_warnings: Vec::new(),
                    draft,
                    runtime_selection: session_runtime_selection_payload(state, session_id).await,
                }),
            )
            .await?;
            let handle = forward_session_events(
                ClientEventSink::new(client_id, writer.clone()),
                attachment.events,
                attachment.live_events,
            );
            state.register_client_forwarder(client_id, handle).await;
            Ok(())
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await?;
            state
                .deactivate_session_namespace_if_inactive(session_id)
                .await;
            Ok(())
        }
    }
}

async fn handle_attach_session_recent(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
    session_id: SessionId,
    limit: usize,
) -> Result<(), ServerError> {
    let total_started_at = Instant::now();
    state
        .metrics
        .increment_counter("server.attach_recent.total");
    state
        .metrics
        .record_histogram("server.attach_recent.limit", usize_to_u64(limit));
    recover_abandoned_session_runtime_work_best_effort(state, session_id).await;
    let namespace_started_at = Instant::now();
    let client_namespace = state.client_session_namespace(client_id).await;
    if let Err(active_namespace) = state
        .try_activate_session_namespace(session_id, client_namespace)
        .await
    {
        state.metrics.record_histogram(
            "server.attach_recent.namespace_activation_duration_ms",
            elapsed_ms(namespace_started_at),
        );
        state.metrics.record_histogram(
            "server.attach_recent.total_duration_ms",
            elapsed_ms(total_started_at),
        );
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    state.metrics.record_histogram(
        "server.attach_recent.namespace_activation_duration_ms",
        elapsed_ms(namespace_started_at),
    );
    let attach_started_at = Instant::now();
    match state
        .sessions
        .attach_session_recent(session_id, client_id, limit)
        .await
    {
        Ok(attachment) => {
            finish_attach_session_recent_success(
                AttachRecentSuccessContext {
                    request_id,
                    writer,
                    client_id,
                    attached_session,
                },
                state,
                session_id,
                attachment,
                AttachRecentTimings {
                    total_started_at,
                    attach_started_at,
                },
            )
            .await
        }
        Err(error) => {
            state.metrics.record_histogram(
                "server.attach_recent.session_attach_duration_ms",
                elapsed_ms(attach_started_at),
            );
            state
                .metrics
                .increment_counter("server.attach_recent.error_total");
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await?;
            state
                .deactivate_session_namespace_if_inactive(session_id)
                .await;
            state.metrics.record_histogram(
                "server.attach_recent.total_duration_ms",
                elapsed_ms(total_started_at),
            );
            Ok(())
        }
    }
}

async fn handle_attach_session_projection_window(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
    session_id: SessionId,
    request: bcode_session_models::ProjectionWindowRequest,
) -> Result<(), ServerError> {
    let total_started_at = Instant::now();
    state
        .metrics
        .increment_counter("server.attach_projection_window.total");
    recover_abandoned_session_runtime_work_best_effort(state, session_id).await;
    let namespace_started_at = Instant::now();
    let client_namespace = state.client_session_namespace(client_id).await;
    if let Err(active_namespace) = state
        .try_activate_session_namespace(session_id, client_namespace)
        .await
    {
        state.metrics.record_histogram(
            "server.attach_projection_window.namespace_activation_duration_ms",
            elapsed_ms(namespace_started_at),
        );
        state.metrics.record_histogram(
            "server.attach_projection_window.total_duration_ms",
            elapsed_ms(total_started_at),
        );
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    state.metrics.record_histogram(
        "server.attach_projection_window.namespace_activation_duration_ms",
        elapsed_ms(namespace_started_at),
    );
    let attach_started_at = Instant::now();
    match state
        .sessions
        .attach_session_projection_window(session_id, client_id, request)
        .await
    {
        Ok(window_attachment) => {
            finish_attach_session_projection_window_success(
                AttachProjectionWindowSuccessContext {
                    request_id,
                    writer,
                    client_id,
                    attached_session,
                },
                state,
                session_id,
                window_attachment,
                AttachRecentTimings {
                    total_started_at,
                    attach_started_at,
                },
            )
            .await
        }
        Err(error) => {
            state.metrics.record_histogram(
                "server.attach_projection_window.session_attach_duration_ms",
                elapsed_ms(attach_started_at),
            );
            state
                .metrics
                .increment_counter("server.attach_projection_window.error_total");
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await?;
            state
                .deactivate_session_namespace_if_inactive(session_id)
                .await;
            state.metrics.record_histogram(
                "server.attach_projection_window.total_duration_ms",
                elapsed_ms(total_started_at),
            );
            Ok(())
        }
    }
}

struct AttachProjectionWindowSuccessContext<'a> {
    request_id: u64,
    writer: &'a SharedWriter,
    client_id: ClientId,
    attached_session: &'a mut Option<SessionId>,
}

async fn finish_attach_session_projection_window_success(
    context: AttachProjectionWindowSuccessContext<'_>,
    state: &Arc<ServerState>,
    session_id: SessionId,
    window_attachment: bcode_session::SessionProjectionWindowAttachment,
    timings: AttachRecentTimings,
) -> Result<(), ServerError> {
    let attachment = window_attachment.attachment;
    state.metrics.record_histogram(
        "server.attach_projection_window.session_attach_duration_ms",
        elapsed_ms(timings.attach_started_at),
    );
    state.metrics.record_histogram(
        "server.attach_projection_window.history_event_count",
        usize_to_u64(attachment.history.len()),
    );
    state.metrics.record_histogram(
        "server.attach_projection_window.input_history_entry_count",
        usize_to_u64(attachment.input_history.len()),
    );
    state.metrics.record_histogram(
        "server.attach_projection_window.projection_item_count",
        usize_to_u64(window_attachment.projection_window.transcript_items.len()),
    );
    let restore_started_at = Instant::now();
    restore_active_skills_from_history(&attachment.history, state, session_id).await;
    state.metrics.record_histogram(
        "server.attach_projection_window.restore_active_skills_duration_ms",
        elapsed_ms(restore_started_at),
    );
    *context.attached_session = Some(session_id);
    state
        .attach_client_session(context.client_id, session_id)
        .await;
    let compact_started_at = Instant::now();
    let compacted_history = compact_attach_history(attachment.history);
    state.metrics.record_histogram(
        "server.attach_projection_window.compact_history_duration_ms",
        elapsed_ms(compact_started_at),
    );
    state.metrics.record_histogram(
        "server.attach_projection_window.compacted_history_event_count",
        usize_to_u64(compacted_history.len()),
    );
    let send_started_at = Instant::now();
    let draft = state.sessions.session_composer_draft(session_id).await?;
    send_response(
        context.writer,
        context.request_id,
        Response::Ok(ResponsePayload::Attached {
            session_id,
            session: attachment.session,
            history: compacted_history,
            input_history: attachment.input_history,
            import_warnings: Vec::new(),
            draft,
            runtime_selection: session_runtime_selection_payload(state, session_id).await,
        }),
    )
    .await?;
    state.metrics.record_histogram(
        "server.attach_projection_window.response_send_duration_ms",
        elapsed_ms(send_started_at),
    );
    state.metrics.record_histogram(
        "server.attach_projection_window.total_duration_ms",
        elapsed_ms(timings.total_started_at),
    );
    let handle = forward_session_events(
        ClientEventSink::new(context.client_id, context.writer.clone()),
        attachment.events,
        attachment.live_events,
    );
    state
        .register_client_forwarder(context.client_id, handle)
        .await;
    Ok(())
}

struct AttachRecentTimings {
    total_started_at: Instant,
    attach_started_at: Instant,
}

struct AttachRecentSuccessContext<'a> {
    request_id: u64,
    writer: &'a SharedWriter,
    client_id: ClientId,
    attached_session: &'a mut Option<SessionId>,
}

async fn finish_attach_session_recent_success(
    context: AttachRecentSuccessContext<'_>,
    state: &Arc<ServerState>,
    session_id: SessionId,
    attachment: bcode_session::SessionAttachment,
    timings: AttachRecentTimings,
) -> Result<(), ServerError> {
    state.metrics.record_histogram(
        "server.attach_recent.session_attach_duration_ms",
        elapsed_ms(timings.attach_started_at),
    );
    state.metrics.record_histogram(
        "server.attach_recent.history_event_count",
        usize_to_u64(attachment.history.len()),
    );
    state.metrics.record_histogram(
        "server.attach_recent.input_history_entry_count",
        usize_to_u64(attachment.input_history.len()),
    );
    let restore_started_at = Instant::now();
    restore_active_skills_from_history(&attachment.history, state, session_id).await;
    state.metrics.record_histogram(
        "server.attach_recent.restore_active_skills_duration_ms",
        elapsed_ms(restore_started_at),
    );
    *context.attached_session = Some(session_id);
    state
        .attach_client_session(context.client_id, session_id)
        .await;
    let compact_started_at = Instant::now();
    let compacted_history = compact_attach_history(attachment.history);
    state.metrics.record_histogram(
        "server.attach_recent.compact_history_duration_ms",
        elapsed_ms(compact_started_at),
    );
    state.metrics.record_histogram(
        "server.attach_recent.compacted_history_event_count",
        usize_to_u64(compacted_history.len()),
    );
    let send_started_at = Instant::now();
    let draft = state.sessions.session_composer_draft(session_id).await?;
    send_response(
        context.writer,
        context.request_id,
        Response::Ok(ResponsePayload::Attached {
            session_id,
            session: attachment.session,
            history: compacted_history,
            input_history: attachment.input_history,
            import_warnings: Vec::new(),
            draft,
            runtime_selection: session_runtime_selection_payload(state, session_id).await,
        }),
    )
    .await?;
    state.metrics.record_histogram(
        "server.attach_recent.response_send_duration_ms",
        elapsed_ms(send_started_at),
    );
    state.metrics.record_histogram(
        "server.attach_recent.total_duration_ms",
        elapsed_ms(timings.total_started_at),
    );
    let handle = forward_session_events(
        ClientEventSink::new(context.client_id, context.writer.clone()),
        attachment.events,
        attachment.live_events,
    );
    state
        .register_client_forwarder(context.client_id, handle)
        .await;
    Ok(())
}

async fn enqueue_user_message_command(
    state: &Arc<ServerState>,
    session_id: SessionId,
    client_id: ClientId,
    runtime_context: Option<ClientRuntimeContext>,
    text: String,
    placement: bcode_ipc::PromptPlacement,
) -> Result<MessageQueueStatus, ServerError> {
    state.sessions.session_summary(session_id).await?;
    let handle = session_runtime_handle(state, session_id).await;
    let phase_snapshot = *handle.phase.lock().await;
    let steering_window = if placement == bcode_ipc::PromptPlacement::Steering {
        Some(steering_window(phase_snapshot, &handle.current_turn).await)
    } else {
        None
    };
    if let Some(window) = steering_window {
        return enqueue_steering_message_command(
            state,
            &handle,
            session_id,
            client_id,
            runtime_context,
            text,
            window,
        )
        .await;
    }

    let pending_before = handle.queued_followups.fetch_add(1, Ordering::AcqRel);
    let queued = pending_before > 0 || phase_snapshot.has_active_work();
    let queue_position = queued.then(|| usize_to_u32_saturating(pending_before.saturating_add(1)));
    let disposition = if queued && placement == bcode_ipc::PromptPlacement::FollowUp {
        bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp
    } else if queued {
        bcode_ipc::MessageAcceptanceDisposition::QueuedTurn
    } else {
        bcode_ipc::MessageAcceptanceDisposition::StartedTurn
    };
    let send_result = handle
        .followup_commands
        .send(FollowupCommand::UserMessage {
            client_id,
            runtime_context,
            text,
            placement,
            completion: None,
        })
        .await
        .map_err(|error| error.to_string());
    if send_result.is_ok() {
        return Ok(MessageQueueStatus {
            queued,
            queue_position,
            disposition,
        });
    }
    handle.queued_followups.fetch_sub(1, Ordering::AcqRel);
    *handle.phase.lock().await = SessionRuntimePhase::Idle;

    state.session_runtimes.lock().await.remove(&session_id);
    Err(bcode_session::SessionError::NotFound(session_id).into())
}

async fn enqueue_steering_message_command(
    state: &Arc<ServerState>,
    handle: &SessionRuntimeHandle,
    session_id: SessionId,
    client_id: ClientId,
    runtime_context: Option<ClientRuntimeContext>,
    text: String,
    window: SteeringWindow,
) -> Result<MessageQueueStatus, ServerError> {
    match window {
        SteeringWindow::Idle => {
            let pending_before = handle.queued_followups.fetch_add(1, Ordering::AcqRel);
            let send_result = handle
                .followup_commands
                .send(FollowupCommand::UserMessage {
                    client_id,
                    runtime_context,
                    text,
                    placement: bcode_ipc::PromptPlacement::Steering,
                    completion: None,
                })
                .await
                .map_err(|error| error.to_string());
            if send_result.is_ok() {
                return Ok(MessageQueueStatus {
                    queued: false,
                    queue_position: None,
                    disposition: bcode_ipc::MessageAcceptanceDisposition::StartedTurn,
                });
            }
            handle.queued_followups.fetch_sub(1, Ordering::AcqRel);
            debug_assert_eq!(pending_before, 0);
        }
        SteeringWindow::BeforeNextProviderRequest => {
            if handle
                .steering_commands
                .send(SteeringCommand {
                    client_id,
                    text,
                    completion: None,
                })
                .await
                .is_ok()
            {
                return Ok(MessageQueueStatus {
                    queued: false,
                    queue_position: None,
                    disposition: bcode_ipc::MessageAcceptanceDisposition::AppliedSteering,
                });
            }
        }
        SteeringWindow::ProviderInFlight | SteeringWindow::Finishing => {
            let user_event = append_steering_user_message(state, session_id, client_id, text)
                .await?
                .ok_or_else(|| bcode_session::SessionError::NotFound(session_id))?;
            let pending_before = handle.queued_followups.fetch_add(1, Ordering::AcqRel);
            let queue_position = Some(usize_to_u32_saturating(pending_before.saturating_add(1)));
            let send_result = handle
                .followup_commands
                .send(FollowupCommand::ContinueFromUserEvent {
                    client_id,
                    runtime_context,
                    user_event: Box::new(user_event),
                    completion: None,
                })
                .await
                .map_err(|error| error.to_string());
            if send_result.is_ok() {
                return Ok(MessageQueueStatus {
                    queued: true,
                    queue_position,
                    disposition: bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp,
                });
            }
            handle.queued_followups.fetch_sub(1, Ordering::AcqRel);
        }
    }

    *handle.phase.lock().await = SessionRuntimePhase::Idle;
    state.session_runtimes.lock().await.remove(&session_id);
    Err(bcode_session::SessionError::NotFound(session_id).into())
}

async fn steering_window(
    phase: SessionRuntimePhase,
    current_turn: &Arc<Mutex<Option<RuntimeCurrentTurn>>>,
) -> SteeringWindow {
    match phase {
        SessionRuntimePhase::Idle => SteeringWindow::Idle,
        SessionRuntimePhase::AppendingUser | SessionRuntimePhase::PreparingModelRequest => {
            SteeringWindow::BeforeNextProviderRequest
        }
        SessionRuntimePhase::ProviderActive => {
            if current_turn
                .lock()
                .await
                .as_ref()
                .is_some_and(|turn| turn.model.is_some())
            {
                SteeringWindow::ProviderInFlight
            } else {
                SteeringWindow::BeforeNextProviderRequest
            }
        }
        SessionRuntimePhase::Compacting | SessionRuntimePhase::FinishingTurn => {
            SteeringWindow::Finishing
        }
    }
}

async fn enqueue_cancel_turn_command(
    state: &Arc<ServerState>,
    session_id: SessionId,
    clear_queue: bool,
    requested_by: Option<ClientId>,
) -> Result<bool, ServerError> {
    state.sessions.session_summary(session_id).await?;
    let handle = session_runtime_handle(state, session_id).await;
    let (response, completion) = oneshot::channel();
    if handle
        .cancel_commands
        .send(CancelCommand {
            clear_queue,
            requested_by,
            response,
        })
        .await
        .is_err()
    {
        state.session_runtimes.lock().await.remove(&session_id);
        return Err(bcode_session::SessionError::NotFound(session_id).into());
    }
    completion.await.map_err(ServerError::from)
}

async fn enqueue_followup_command(
    state: &Arc<ServerState>,
    session_id: SessionId,
    command: FollowupCommand,
) -> Result<MessageQueueStatus, ServerError> {
    state.sessions.session_summary(session_id).await?;
    let handle = session_runtime_handle(state, session_id).await;
    let pending_before = handle.queued_followups.fetch_add(1, Ordering::AcqRel);
    let queued = pending_before > 0 || handle.phase.lock().await.has_active_work();
    let queue_position = queued.then(|| usize_to_u32_saturating(pending_before.saturating_add(1)));
    let disposition = if queued {
        bcode_ipc::MessageAcceptanceDisposition::QueuedTurn
    } else {
        bcode_ipc::MessageAcceptanceDisposition::StartedTurn
    };
    if handle.followup_commands.send(command).await.is_ok() {
        return Ok(MessageQueueStatus {
            queued,
            queue_position,
            disposition,
        });
    }
    handle.queued_followups.fetch_sub(1, Ordering::AcqRel);

    state.session_runtimes.lock().await.remove(&session_id);
    Err(bcode_session::SessionError::NotFound(session_id).into())
}

async fn enqueue_compact_session_command(
    state: &Arc<ServerState>,
    session_id: SessionId,
    selection: SessionModelSelection,
) -> Result<Result<String, CompactionError>, ServerError> {
    let (response, completion) = oneshot::channel();
    enqueue_followup_command(
        state,
        session_id,
        FollowupCommand::CompactSession {
            selection,
            response,
        },
    )
    .await?;
    completion.await.map_err(ServerError::from)
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

async fn session_runtime_handle(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> SessionRuntimeHandle {
    let mut runtimes = state.session_runtimes.lock().await;
    if let Some(handle) = runtimes.get(&session_id) {
        return handle.clone();
    }

    let (followup_commands, followup_receiver) = mpsc::channel(128);
    let (steering_commands, steering_receiver) = mpsc::channel(128);
    let (cancel_commands, cancel_receiver) = mpsc::channel(32);
    let queued_followups = Arc::new(AtomicUsize::new(0));
    let followup_receiver = Arc::new(Mutex::new(Some(followup_receiver)));
    let steering_receiver = Arc::new(Mutex::new(Some(steering_receiver)));
    let cancel_receiver = Arc::new(Mutex::new(Some(cancel_receiver)));
    let phase = Arc::new(Mutex::new(SessionRuntimePhase::Idle));
    let current_turn = Arc::new(Mutex::new(None));
    let handle = SessionRuntimeHandle {
        followup_commands,
        steering_commands,
        cancel_commands,
        queued_followups: Arc::clone(&queued_followups),
        phase: Arc::clone(&phase),
        current_turn: Arc::clone(&current_turn),
    };
    runtimes.insert(session_id, handle.clone());
    drop(runtimes);
    let state_for_runtime = Arc::clone(state);
    tokio::spawn(async move {
        Box::pin(run_session_runtime(
            state_for_runtime,
            session_id,
            followup_receiver,
            steering_receiver,
            cancel_receiver,
            queued_followups,
            phase,
            current_turn,
        ))
        .await;
    });
    handle
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_session_runtime(
    state: Arc<ServerState>,
    session_id: SessionId,
    followup_commands: Arc<Mutex<Option<mpsc::Receiver<FollowupCommand>>>>,
    steering_commands: Arc<Mutex<Option<mpsc::Receiver<SteeringCommand>>>>,
    cancel_commands: Arc<Mutex<Option<mpsc::Receiver<CancelCommand>>>>,
    queued_followups: Arc<AtomicUsize>,
    phase: Arc<Mutex<SessionRuntimePhase>>,
    current_turn: Arc<Mutex<Option<RuntimeCurrentTurn>>>,
) {
    let mut permit = SessionTurnPermit::new(session_id);
    let mut followup_commands = followup_commands
        .lock()
        .await
        .take()
        .expect("session runtime followup receiver should be present");
    let mut steering_commands = steering_commands
        .lock()
        .await
        .take()
        .expect("session runtime steering receiver should be present");
    let mut cancel_commands = cancel_commands
        .lock()
        .await
        .take()
        .expect("session runtime cancel receiver should be present");
    loop {
        service_cancel_commands(
            &state,
            session_id,
            &mut cancel_commands,
            &mut followup_commands,
            queued_followups.as_ref(),
        )
        .await;
        while let Ok(command) = steering_commands.try_recv() {
            process_steering_message_command(
                &state,
                permit.session_id(),
                command.client_id,
                command.text,
                command.completion,
            )
            .await;
        }
        let Some(command) =
            next_followup_command(&mut followup_commands, queued_followups.as_ref()).await
        else {
            break;
        };
        match command {
            FollowupCommand::UserMessage {
                client_id,
                runtime_context,
                text,
                placement,
                completion,
            } => {
                Box::pin(process_user_message_command(
                    &state,
                    &mut permit,
                    Arc::clone(&phase),
                    &mut followup_commands,
                    &mut steering_commands,
                    &mut cancel_commands,
                    queued_followups.as_ref(),
                    Arc::clone(&current_turn),
                    client_id,
                    runtime_context,
                    text,
                    placement,
                    completion,
                ))
                .await;
            }
            FollowupCommand::ContinueFromUserEvent {
                client_id,
                runtime_context,
                user_event,
                completion,
            } => {
                Box::pin(process_existing_user_event_command(
                    &state,
                    &mut permit,
                    Arc::clone(&phase),
                    &mut followup_commands,
                    &mut steering_commands,
                    &mut cancel_commands,
                    queued_followups.as_ref(),
                    Arc::clone(&current_turn),
                    client_id,
                    runtime_context,
                    *user_event,
                    completion,
                ))
                .await;
            }
            FollowupCommand::SkillInvocation {
                client_id,
                runtime_context,
                skill_id,
                arguments,
                source,
                display_text,
            } => {
                Box::pin(process_skill_invocation_command(
                    &state,
                    &mut permit,
                    Arc::clone(&phase),
                    &mut followup_commands,
                    &mut steering_commands,
                    &mut cancel_commands,
                    queued_followups.as_ref(),
                    Arc::clone(&current_turn),
                    client_id,
                    runtime_context,
                    skill_id,
                    arguments,
                    source,
                    display_text,
                ))
                .await;
            }
            FollowupCommand::CompactSession {
                selection,
                response,
            } => {
                let result = process_compact_session_command(
                    &state,
                    permit.session_id(),
                    Arc::clone(&phase),
                    &mut followup_commands,
                    &mut steering_commands,
                    &mut cancel_commands,
                    queued_followups.as_ref(),
                    Arc::clone(&current_turn),
                    selection,
                )
                .await;
                let _sent = response.send(result);
            }
        }
    }
    state.session_runtimes.lock().await.remove(&session_id);
}

async fn next_followup_command(
    followup_commands: &mut mpsc::Receiver<FollowupCommand>,
    queued_followups: &AtomicUsize,
) -> Option<FollowupCommand> {
    let command = followup_commands.recv().await?;
    queued_followups.fetch_sub(1, Ordering::AcqRel);
    Some(command)
}

fn drain_followup_commands(followup_commands: &mut mpsc::Receiver<FollowupCommand>) -> usize {
    let mut cleared = 0_usize;
    while followup_commands.try_recv().is_ok() {
        cleared = cleared.saturating_add(1);
    }
    cleared
}

async fn service_cancel_commands(
    state: &ServerState,
    session_id: SessionId,
    cancel_commands: &mut mpsc::Receiver<CancelCommand>,
    followup_commands: &mut mpsc::Receiver<FollowupCommand>,
    queued_followups: &AtomicUsize,
) {
    while let Ok(command) = cancel_commands.try_recv() {
        let cancelled = process_cancel_turn_command(
            state,
            session_id,
            followup_commands,
            queued_followups,
            command.clear_queue,
            command.requested_by,
        )
        .await;
        let _sent = command.response.send(cancelled);
    }
}

async fn process_cancel_turn_command(
    state: &ServerState,
    session_id: SessionId,
    followup_commands: &mut mpsc::Receiver<FollowupCommand>,
    queued_followups: &AtomicUsize,
    clear_queue: bool,
    requested_by: Option<ClientId>,
) -> bool {
    let cancelled = request_session_turn_cancellation(state, session_id, requested_by).await;
    if clear_queue {
        let cleared = drain_followup_commands(followup_commands);
        if cleared > 0 {
            queued_followups.fetch_sub(cleared, Ordering::AcqRel);
        }
    }
    cancelled
}

async fn process_steering_message_command(
    state: &ServerState,
    session_id: SessionId,
    client_id: ClientId,
    text: String,
    completion_sender: Option<oneshot::Sender<ModelTurnCompletion>>,
) {
    let completion = match append_steering_user_message(state, session_id, client_id, text).await {
        Ok(Some(_event)) => ModelTurnCompletion::completed(),
        Ok(None) => {
            let message = "no steering user message event was appended".to_string();
            append_system_event(state, session_id, message.clone()).await;
            ModelTurnCompletion::with_message(ModelTurnOutcome::Error, message)
        }
        Err(error) => {
            let message = format!("failed to append steering user message: {error}");
            append_system_event(state, session_id, message.clone()).await;
            ModelTurnCompletion::with_message(ModelTurnOutcome::Error, message)
        }
    };
    if let Some(sender) = completion_sender {
        let _sent = sender.send(completion);
    }
}

async fn wait_for_provider_call<'a, T>(
    state: &ServerState,
    session_id: SessionId,
    context: &mut RuntimeCommandContext<'_>,
    cancel_state: &TurnCancelState,
    mut provider_call: ProviderCallFuture<'a, T>,
) -> ProviderCallWait<T>
where
    T: Send + 'a,
{
    loop {
        tokio::select! {
            result = &mut provider_call => return ProviderCallWait::Completed(result),
            cancel_command = context.cancel_commands.recv() => {
                if let Some(command) = cancel_command {
                    let cancelled = process_cancel_turn_command(
                        state,
                        session_id,
                        context.followup_commands,
                        context.queued_followups,
                        command.clear_queue,
                        command.requested_by,
                    )
                    .await;
                    let _sent = command.response.send(cancelled);
                }
                if cancel_state.is_cancelled() {
                    return ProviderCallWait::Cancelled;
                }
            }
            steering_command = context.steering_commands.recv() => {
                if let Some(command) = steering_command {
                    process_steering_message_command(
                        state,
                        session_id,
                        command.client_id,
                        command.text,
                        command.completion,
                    )
                    .await;
                }
                if cancel_state.is_cancelled() {
                    return ProviderCallWait::Cancelled;
                }
            }
            () = cancel_state.cancelled() => return ProviderCallWait::Cancelled,
        }
    }
}

async fn service_runtime_priority_commands(
    state: &ServerState,
    session_id: SessionId,
    context: &mut RuntimeCommandContext<'_>,
) {
    service_cancel_commands(
        state,
        session_id,
        context.cancel_commands,
        context.followup_commands,
        context.queued_followups,
    )
    .await;
    while let Ok(command) = context.steering_commands.try_recv() {
        process_steering_message_command(
            state,
            session_id,
            command.client_id,
            command.text,
            command.completion,
        )
        .await;
    }
}

async fn runtime_accepts_inline_steering(phase: &Arc<Mutex<SessionRuntimePhase>>) -> bool {
    phase.lock().await.accepts_inline_steering()
}

async fn set_runtime_phase(
    phase: &Arc<Mutex<SessionRuntimePhase>>,
    next_phase: SessionRuntimePhase,
) {
    *phase.lock().await = next_phase;
}

async fn begin_current_turn(
    context: &RuntimeCommandContext<'_>,
    client_id: ClientId,
    turn_id: String,
    cancel_state: Arc<TurnCancelState>,
) {
    let mut current_turn = context.current_turn.lock().await;
    debug_assert!(
        current_turn.is_none(),
        "begin_current_turn requires no active current turn"
    );
    *current_turn = Some(RuntimeCurrentTurn {
        client_id,
        turn_id,
        cancel_state,
        model: None,
    });
}

async fn finish_current_turn(context: &RuntimeCommandContext<'_>) {
    let mut current_turn = context.current_turn.lock().await;
    debug_assert!(
        current_turn.is_some(),
        "finish_current_turn requires an active current turn"
    );
    *current_turn = None;
}

async fn begin_provider_round(context: &RuntimeCommandContext<'_>, model_turn: ActiveModelTurn) {
    let mut current_turn_guard = context.current_turn.lock().await;
    let Some(current_turn) = current_turn_guard.as_mut() else {
        debug_assert!(
            false,
            "begin_provider_round requires an active current turn"
        );
        return;
    };
    debug_assert!(
        current_turn.model.is_none(),
        "begin_provider_round requires no active provider round"
    );
    current_turn.model = Some(model_turn);
    drop(current_turn_guard);
}

async fn finish_provider_round(context: &RuntimeCommandContext<'_>) -> Option<ActiveModelTurn> {
    let mut current_turn_guard = context.current_turn.lock().await;
    let Some(current_turn) = current_turn_guard.as_mut() else {
        debug_assert!(
            false,
            "finish_provider_round requires an active current turn"
        );
        return None;
    };
    let active_turn = current_turn.model.take();
    debug_assert!(
        active_turn.is_some(),
        "finish_provider_round requires an active provider round"
    );
    drop(current_turn_guard);
    active_turn
}

#[allow(clippy::too_many_arguments)]
async fn process_user_message_command(
    state: &ServerState,
    permit: &mut SessionTurnPermit,
    phase: Arc<Mutex<SessionRuntimePhase>>,
    followup_commands: &mut mpsc::Receiver<FollowupCommand>,
    steering_commands: &mut mpsc::Receiver<SteeringCommand>,
    cancel_commands: &mut mpsc::Receiver<CancelCommand>,
    queued_followups: &AtomicUsize,
    current_turn: Arc<Mutex<Option<RuntimeCurrentTurn>>>,
    client_id: ClientId,
    runtime_context: Option<ClientRuntimeContext>,
    text: String,
    placement: bcode_ipc::PromptPlacement,
    completion_sender: Option<oneshot::Sender<ModelTurnCompletion>>,
) {
    if placement == bcode_ipc::PromptPlacement::Steering
        && runtime_accepts_inline_steering(&phase).await
    {
        process_steering_message_command(
            state,
            permit.session_id(),
            client_id,
            text,
            completion_sender,
        )
        .await;
        return;
    }

    set_runtime_phase(&phase, SessionRuntimePhase::AppendingUser).await;
    let completion = match append_turn_user_message(state, permit, client_id, text).await {
        Ok(Some(user_event)) => {
            suggest_skills_for_prompt(state, permit.session_id(), &user_event).await;
            set_runtime_phase(&phase, SessionRuntimePhase::PreparingModelRequest).await;
            let mut command_context = RuntimeCommandContext::new(
                followup_commands,
                steering_commands,
                cancel_commands,
                queued_followups,
                Arc::clone(&current_turn),
            );
            run_model_turn(
                state,
                permit,
                &user_event,
                client_id,
                runtime_context,
                &mut command_context,
                &phase,
            )
            .await
        }
        Ok(None) => {
            let message = "no user message event was appended".to_string();
            append_system_event(state, permit.session_id(), message.clone()).await;
            ModelTurnCompletion::with_message(ModelTurnOutcome::Error, message)
        }
        Err(error) => {
            let message = format!("failed to append user message: {error}");
            append_system_event(state, permit.session_id(), message.clone()).await;
            ModelTurnCompletion::with_message(ModelTurnOutcome::Error, message)
        }
    };
    set_runtime_phase(&phase, SessionRuntimePhase::Idle).await;
    if let Some(sender) = completion_sender {
        let _sent = sender.send(completion);
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_existing_user_event_command(
    state: &ServerState,
    permit: &mut SessionTurnPermit,
    phase: Arc<Mutex<SessionRuntimePhase>>,
    followup_commands: &mut mpsc::Receiver<FollowupCommand>,
    steering_commands: &mut mpsc::Receiver<SteeringCommand>,
    cancel_commands: &mut mpsc::Receiver<CancelCommand>,
    queued_followups: &AtomicUsize,
    current_turn: Arc<Mutex<Option<RuntimeCurrentTurn>>>,
    client_id: ClientId,
    runtime_context: Option<ClientRuntimeContext>,
    user_event: bcode_session_models::SessionEvent,
    completion_sender: Option<oneshot::Sender<ModelTurnCompletion>>,
) {
    set_runtime_phase(&phase, SessionRuntimePhase::PreparingModelRequest).await;
    suggest_skills_for_prompt(state, permit.session_id(), &user_event).await;
    let mut command_context = RuntimeCommandContext::new(
        followup_commands,
        steering_commands,
        cancel_commands,
        queued_followups,
        Arc::clone(&current_turn),
    );
    let completion = run_model_turn(
        state,
        permit,
        &user_event,
        client_id,
        runtime_context,
        &mut command_context,
        &phase,
    )
    .await;
    set_runtime_phase(&phase, SessionRuntimePhase::Idle).await;
    if let Some(sender) = completion_sender {
        let _sent = sender.send(completion);
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_skill_invocation_command(
    state: &ServerState,
    permit: &mut SessionTurnPermit,
    phase: Arc<Mutex<SessionRuntimePhase>>,
    followup_commands: &mut mpsc::Receiver<FollowupCommand>,
    steering_commands: &mut mpsc::Receiver<SteeringCommand>,
    cancel_commands: &mut mpsc::Receiver<CancelCommand>,
    queued_followups: &AtomicUsize,
    current_turn: Arc<Mutex<Option<RuntimeCurrentTurn>>>,
    client_id: ClientId,
    runtime_context: Option<ClientRuntimeContext>,
    skill_id: SkillId,
    arguments: String,
    source: Option<SkillSource>,
    display_text: String,
) {
    let invocation = state
        .sessions
        .append_event(
            permit.session_id(),
            SessionEventKind::SkillInvoked {
                skill_id: skill_id.clone(),
                arguments: arguments.clone(),
                source,
                invoked_at_ms: current_time_ms(),
            },
        )
        .await;
    match invocation {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => {
            append_system_event(
                state,
                permit.session_id(),
                format!("failed to append skill invocation: {error}"),
            )
            .await;
            return;
        }
    }

    set_runtime_phase(&phase, SessionRuntimePhase::AppendingUser).await;
    match append_turn_user_message(state, permit, client_id, display_text).await {
        Ok(Some(user_event)) => {
            state.turn_skills.lock().await.insert(
                (permit.session_id(), user_event.sequence),
                SkillTurnInvocation {
                    skill_id,
                    arguments,
                },
            );
            set_runtime_phase(&phase, SessionRuntimePhase::PreparingModelRequest).await;
            let mut command_context = RuntimeCommandContext::new(
                followup_commands,
                steering_commands,
                cancel_commands,
                queued_followups,
                Arc::clone(&current_turn),
            );
            run_model_turn(
                state,
                permit,
                &user_event,
                client_id,
                runtime_context,
                &mut command_context,
                &phase,
            )
            .await;
        }
        Ok(None) => {
            append_system_event(
                state,
                permit.session_id(),
                "no user message event was appended".to_string(),
            )
            .await;
        }
        Err(error) => {
            append_system_event(
                state,
                permit.session_id(),
                format!("failed to append skill user message: {error}"),
            )
            .await;
        }
    }
    set_runtime_phase(&phase, SessionRuntimePhase::Idle).await;
}

#[allow(clippy::too_many_arguments)]
async fn process_compact_session_command(
    state: &ServerState,
    session_id: SessionId,
    phase: Arc<Mutex<SessionRuntimePhase>>,
    followup_commands: &mut mpsc::Receiver<FollowupCommand>,
    steering_commands: &mut mpsc::Receiver<SteeringCommand>,
    cancel_commands: &mut mpsc::Receiver<CancelCommand>,
    queued_followups: &AtomicUsize,
    current_turn: Arc<Mutex<Option<RuntimeCurrentTurn>>>,
    selection: SessionModelSelection,
) -> Result<String, CompactionError> {
    set_runtime_phase(&phase, SessionRuntimePhase::Compacting).await;
    let mut command_context = RuntimeCommandContext::new(
        followup_commands,
        steering_commands,
        cancel_commands,
        queued_followups,
        current_turn,
    );
    let result = compact_session_context_with_limit(
        state,
        session_id,
        &selection,
        None,
        Some(&mut command_context),
    )
    .await;
    set_runtime_phase(&phase, SessionRuntimePhase::Idle).await;
    result
}

async fn append_turn_user_message(
    state: &ServerState,
    permit: &mut SessionTurnPermit,
    client_id: ClientId,
    text: String,
) -> Result<Option<bcode_session_models::SessionEvent>, bcode_session::SessionError> {
    let events = state
        .sessions
        .append_user_message(permit.enter_turn(), client_id, text)
        .await?;
    for event in &events {
        publish_session_event(state, event).await;
    }
    if !events.is_empty()
        && let Ok(session) = state.sessions.session_summary(permit.session_id()).await
    {
        state.session_catalog.upsert_native_session(session).await;
    }
    Ok(events.last().cloned())
}

#[allow(clippy::too_many_arguments)]
async fn send_message_acceptance_response(
    state: &ServerState,
    writer: &SharedWriter,
    request_id: u64,
    client_id: ClientId,
    status: MessageQueueStatus,
) -> Result<(), ServerError> {
    let payload = if state.client_supports_message_accepted(client_id).await {
        ResponsePayload::MessageAcceptedWithDisposition {
            queued: status.queued,
            queue_position: status.queue_position,
            disposition: status.disposition,
        }
    } else {
        ResponsePayload::MessageSent
    };
    send_response(writer, request_id, Response::Ok(payload)).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_invoke_skill(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    session_id: SessionId,
    skill_id: SkillId,
    arguments: String,
    display_text: String,
) -> Result<(), ServerError> {
    if let Some(active_namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    let Some(registry) = &state.skills else {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new("skills_disabled", "skills are disabled")),
        )
        .await;
    };
    let Some(summary) = registry.summary(&skill_id).cloned() else {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "unknown_skill",
                format!("unknown skill: {skill_id}"),
            )),
        )
        .await;
    };
    let command = FollowupCommand::SkillInvocation {
        client_id,
        runtime_context: state.client_runtime_context(client_id).await,
        skill_id,
        arguments,
        source: Some(summary.source),
        display_text,
    };
    match enqueue_followup_command(state, session_id, command).await {
        Ok(status) => {
            send_message_acceptance_response(state, writer, request_id, client_id, status).await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(server_session_error_response(&error)),
            )
            .await
        }
    }
}

async fn submit_session_model_turn_and_wait(
    state: &Arc<ServerState>,
    session_id: SessionId,
    text: String,
    runtime_context: Option<ClientRuntimeContext>,
) -> Result<ModelTurnCompletion, ServerError> {
    let (sender, receiver) = oneshot::channel();
    enqueue_followup_command(
        state,
        session_id,
        FollowupCommand::UserMessage {
            client_id: ClientId::new(),
            runtime_context,
            text,
            placement: bcode_ipc::PromptPlacement::FollowUp,
            completion: Some(sender),
        },
    )
    .await?;
    receiver.await.map_err(ServerError::from)
}

async fn handle_user_message(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    session_id: SessionId,
    text: String,
    placement: bcode_ipc::PromptPlacement,
) -> Result<(), ServerError> {
    if let Some(active_namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    match enqueue_user_message_command(
        state,
        session_id,
        client_id,
        state.client_runtime_context(client_id).await,
        text,
        placement,
    )
    .await
    {
        Ok(status) => {
            send_message_acceptance_response(state, writer, request_id, client_id, status).await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(server_session_error_response(&error)),
            )
            .await
        }
    }
}

async fn append_steering_user_message(
    state: &ServerState,
    session_id: SessionId,
    client_id: ClientId,
    text: String,
) -> Result<Option<bcode_session_models::SessionEvent>, bcode_session::SessionError> {
    let events = state
        .sessions
        .append_user_message(session_id, client_id, text)
        .await?;
    let user_event = events.last().cloned();
    for event in &events {
        publish_session_event(state, event).await;
    }
    if !events.is_empty()
        && let Ok(session) = state.sessions.session_summary(session_id).await
    {
        state.session_catalog.upsert_native_session(session).await;
    }
    Ok(user_event)
}

fn reasoning_capabilities_from_config(
    reasoning: &bcode_config::ReasoningConfig,
) -> Option<bcode_model::ModelReasoningInfo> {
    (!reasoning.effort_values.is_empty()
        || !reasoning.summary_values.is_empty()
        || reasoning.default_effort.is_some()
        || reasoning.default_summary.is_some()
        || reasoning.visible_summary_supported.is_some()
        || reasoning.raw_reasoning_supported.is_some())
    .then(|| bcode_model::ModelReasoningInfo {
        effort_values: reasoning.effort_values.clone(),
        default_effort: reasoning.default_effort.clone(),
        visible_summary_supported: reasoning.visible_summary_supported.unwrap_or_default(),
        summary_values: reasoning.summary_values.clone(),
        default_summary: reasoning.default_summary.clone(),
        raw_reasoning_supported: reasoning.raw_reasoning_supported.unwrap_or_default(),
        source: bcode_model::ModelReasoningCapabilitySource::ConfigOverride,
    })
}

async fn handle_set_session_model(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    provider_plugin_id: Option<String>,
    model_id: String,
) -> Result<(), ServerError> {
    let provider = provider_plugin_id.unwrap_or_else(|| "<auto>".to_string());
    match state
        .sessions
        .append_model_changed(session_id, provider.clone(), model_id.clone())
        .await
    {
        Ok(event) => {
            let selection = SessionModelSelection {
                provider_plugin_id: provider_to_selection(&provider),
                model_id: model_to_selection(&model_id),
                thinking_level: None,
                reasoning_effort: state.selected_reasoning.effort.clone(),
                reasoning_summary: state.selected_reasoning.summary.clone(),
                reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
                provider_context: state.selected_provider_context.clone(),
            };
            state
                .session_model_selections
                .lock()
                .await
                .insert(session_id, selection);
            publish_session_event(state, &event).await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionModelSet),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await
        }
    }
}

#[allow(clippy::significant_drop_tightening)]
async fn handle_set_session_reasoning(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    effort: Option<String>,
    summary: Option<String>,
) -> Result<(), ServerError> {
    match state
        .sessions
        .append_reasoning_changed(session_id, effort.clone(), summary.clone())
        .await
    {
        Ok(event) => {
            {
                let mut selections = state.session_model_selections.lock().await;
                let selection =
                    selections
                        .entry(session_id)
                        .or_insert_with(|| SessionModelSelection {
                            provider_plugin_id: state.selected_provider_plugin_id.clone(),
                            model_id: state.selected_model_id.clone(),
                            thinking_level: None,
                            reasoning_effort: state.selected_reasoning.effort.clone(),
                            reasoning_summary: state.selected_reasoning.summary.clone(),
                            reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
                            provider_context: state.selected_provider_context.clone(),
                        });
                selection.reasoning_effort = effort;
                selection.reasoning_summary = summary;
            }
            publish_session_event(state, &event).await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionModelSet),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await
        }
    }
}

async fn handle_session_model_status(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    let selection = session_model_selection_with_runtime_context(
        state,
        session_id,
        state.client_runtime_context(client_id).await,
    )
    .await;
    let status = model_status_for_selection(state, selection).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionModelStatus { status }),
    )
    .await
}

async fn handle_default_model_status(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let selection = default_model_selection_with_runtime_context(
        state,
        state.client_runtime_context(client_id).await,
    );
    let status = model_status_for_selection(state, selection).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionModelStatus { status }),
    )
    .await
}

async fn model_status_for_selection(
    state: &ServerState,
    selection: SessionModelSelection,
) -> bcode_ipc::SessionModelStatus {
    let mut models = invoke_model_provider_json_blocking::<_, ModelList>(
        state,
        selection.provider_plugin_id.clone(),
        OP_MODELS,
        bcode_model::ModelListRequest {
            provider_context: selection.provider_context.clone(),
            selected_model_id: selection.model_id.clone(),
        },
    )
    .await
    .ok();
    if let Some(models) = &mut models {
        let provider_for_ignores = selection
            .provider_plugin_id
            .as_deref()
            .unwrap_or("bcode.openai-compatible");
        if let Ok(rules) = bcode_config::effective_model_ignore_rules(provider_for_ignores) {
            model_ignores::apply_model_ignores(&mut models.models, &rules);
        }
    }
    let model = models
        .as_ref()
        .and_then(|models| select_model_info(&models.models, selection.model_id.as_deref()));
    let cache_info = model.as_ref().map(|model| model.cache.clone());
    let model_id = selection
        .model_id
        .clone()
        .or_else(|| model.as_ref().map(|model| model.model_id.clone()));
    let override_metadata = model_id
        .as_deref()
        .map(|model_id| model_metadata_override(&selection.provider_context, model_id));
    let context_window = override_metadata
        .as_ref()
        .and_then(|metadata| metadata.context_window)
        .or_else(|| model.as_ref().and_then(|model| model.context_window));
    let max_output_tokens = override_metadata
        .as_ref()
        .and_then(|metadata| metadata.max_output_tokens)
        .or_else(|| model.as_ref().and_then(|model| model.max_output_tokens));
    let metadata_source = if override_metadata.as_ref().is_some_and(|metadata| {
        metadata.context_window.is_some() || metadata.max_output_tokens.is_some()
    }) {
        Some(bcode_model::ModelMetadataSource::ConfigOverride)
    } else {
        model.as_ref().and_then(|model| model.metadata_source)
    };
    let reasoning_override = model_id
        .as_deref()
        .and_then(|model_id| model_reasoning_override(&selection.provider_context, model_id));
    let base_reasoning = selection
        .reasoning_capabilities
        .or_else(|| model.as_ref().and_then(|model| model.reasoning.clone()));
    bcode_ipc::SessionModelStatus {
        provider_plugin_id: selection.provider_plugin_id,
        model_id,
        context_window,
        max_output_tokens,
        reasoning: merge_reasoning_override(base_reasoning, reasoning_override),
        reasoning_effort: selection.reasoning_effort,
        reasoning_summary: selection.reasoning_summary,
        prompt_cache_mode: Some(prompt_cache_mode_name(state.prompt_cache_mode).to_string()),
        conversation_reuse_mode: Some(
            conversation_reuse_mode_name(state.conversation_reuse_mode).to_string(),
        ),
        compaction_mode: Some(compaction_mode_name(state.auto_compaction.mode).to_string()),
        cache: cache_info,
        metadata_source,
        pricing: model.as_ref().and_then(|model| model.pricing.clone()),
    }
}

async fn handle_session_model_list(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    provider_plugin_id: Option<String>,
) -> Result<(), ServerError> {
    let runtime_context = state
        .client_runtime_contexts
        .try_lock()
        .ok()
        .and_then(|contexts| contexts.get(&client_id).cloned());
    let selected_provider_plugin_id = provider_plugin_id.or_else(|| {
        runtime_context
            .as_ref()
            .and_then(|context| context.selected_provider_plugin_id.clone())
    });
    match invoke_model_provider_json_blocking::<_, ModelList>(
        state,
        selected_provider_plugin_id.clone(),
        OP_MODELS,
        bcode_model::ModelListRequest {
            provider_context: runtime_context
                .map_or_else(bcode_model::ProviderRequestContext::default, |context| {
                    context.provider_context
                }),
            selected_model_id: None,
        },
    )
    .await
    {
        Ok(mut models) => {
            let provider_for_ignores = selected_provider_plugin_id
                .as_deref()
                .unwrap_or("bcode.openai-compatible");
            if let Ok(rules) = bcode_config::effective_model_ignore_rules(provider_for_ignores) {
                model_ignores::apply_model_ignores(&mut models.models, &rules);
            }
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionModelList {
                    provider_plugin_id: selected_provider_plugin_id,
                    models,
                }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("model_list_failed", error.clone())),
            )
            .await
        }
    }
}

fn select_model_info(
    models: &[bcode_model::ModelInfo],
    selected_model_id: Option<&str>,
) -> Option<bcode_model::ModelInfo> {
    selected_model_id
        .and_then(|model_id| models.iter().find(|model| model.model_id == model_id))
        .or_else(|| {
            models
                .iter()
                .find(|model| model.is_default && !model_ignores::is_ignored(model))
        })
        .or_else(|| {
            models
                .iter()
                .find(|model| !model_ignores::is_ignored(model))
        })
        .or_else(|| models.first())
        .cloned()
}

async fn handle_list_agents(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let agents = list_agent_profiles(state).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::AgentList { agents }),
    )
    .await
}

async fn handle_list_skills(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let skills = state.skills.as_ref().map_or_else(
        || SkillList {
            skills: Vec::new(),
            diagnostics: Vec::new(),
        },
        SkillRegistry::list,
    );
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SkillList {
            skills: Box::new(skills),
        }),
    )
    .await
}

async fn handle_describe_skill(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    skill_id: &SkillId,
) -> Result<(), ServerError> {
    let Some(registry) = &state.skills else {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse {
                code: "skills_disabled".to_string(),
                message: "skills are disabled".to_string(),
            }),
        )
        .await;
    };
    match registry.describe(skill_id) {
        Ok(skill) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SkillManifest {
                    skill: Box::new(skill),
                }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse {
                    code: "skill_describe_failed".to_string(),
                    message: error.to_string(),
                }),
            )
            .await
        }
    }
}

async fn handle_activate_skill(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    skill_id: SkillId,
) -> Result<(), ServerError> {
    let Some(registry) = &state.skills else {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse {
                code: "skills_disabled".to_string(),
                message: "skills are disabled".to_string(),
            }),
        )
        .await;
    };
    let Some(summary) = registry.summary(&skill_id).cloned() else {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse {
                code: "unknown_skill".to_string(),
                message: format!("unknown skill: {skill_id}"),
            }),
        )
        .await;
    };
    state
        .active_skills
        .lock()
        .await
        .entry(session_id)
        .or_default()
        .insert(skill_id.clone());
    let event = state
        .sessions
        .append_event(
            session_id,
            SessionEventKind::SkillActivated {
                skill_id,
                source: Some(summary.source),
                mode: SkillActivationMode::Explicit,
                activated_at_ms: current_time_ms(),
            },
        )
        .await?;
    publish_session_event(state, &event).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionAgentSet),
    )
    .await
}

async fn handle_deactivate_skill(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    skill_id: SkillId,
) -> Result<(), ServerError> {
    if let Some(skills) = state.active_skills.lock().await.get_mut(&session_id) {
        skills.remove(&skill_id);
    }
    let event = state
        .sessions
        .append_event(
            session_id,
            SessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms: current_time_ms(),
            },
        )
        .await?;
    publish_session_event(state, &event).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionAgentSet),
    )
    .await
}

async fn handle_active_skills(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    let skills = active_skill_contexts(state, session_id).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::ActiveSkills { skills }),
    )
    .await
}

async fn handle_agent_policy_status(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let status = agent_policy_status(state)
        .await
        .unwrap_or_else(|| PolicyStatusResponse {
            source: "agent profile provider not loaded".to_string(),
            using_default: true,
            build_enabled_tools: Vec::new(),
            plan_enabled_tools: Vec::new(),
            diagnostics: vec!["agent profile provider not loaded".to_string()],
        });
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::AgentPolicyStatus { status }),
    )
    .await
}

async fn handle_set_session_agent(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    agent_id: String,
) -> Result<(), ServerError> {
    let Some(resolved_agent_id) = resolve_agent_id(state, &agent_id).await else {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "unknown_agent",
                format!("unknown agent profile: {agent_id}"),
            )),
        )
        .await;
    };
    match state
        .sessions
        .set_current_agent(session_id, resolved_agent_id.clone())
        .await
    {
        Ok(()) => {
            state
                .session_agent_selections
                .lock()
                .await
                .insert(session_id, resolved_agent_id);
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionAgentSet),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await
        }
    }
}

async fn request_session_turn_cancellation(
    state: &ServerState,
    session_id: SessionId,
    requested_by: Option<ClientId>,
) -> bool {
    let Some(current_turn) = state.session_current_turn(session_id).await else {
        return false;
    };

    current_turn.cancel_state.cancel();
    append_model_turn_cancel_requested_event(
        state,
        session_id,
        current_turn.turn_id.clone(),
        requested_by,
    )
    .await;
    cancel_registered_runtime_work(
        state,
        session_id,
        RuntimeWorkId::new(format!("model_{}", current_turn.turn_id)),
        requested_by,
    )
    .await;

    let active_turn = state.active_model_turn_snapshot(session_id).await;
    let Some(active_turn) = active_turn else {
        return true;
    };
    let request = CancelTurnRequest {
        provider_turn_id: active_turn.provider_turn_id,
    };
    let cancel_result = invoke_model_provider_json_blocking::<_, bcode_model::AckResponse>(
        state,
        active_turn.provider_plugin_id,
        OP_CANCEL_TURN,
        request,
    )
    .await;
    if let Err(error) = cancel_result {
        append_system_event(
            state,
            session_id,
            format!("provider turn cancellation failed: {error}"),
        )
        .await;
    }
    true
}

async fn handle_cancel_session_turn(
    request_id: u64,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    session_id: SessionId,
    client_id: ClientId,
    clear_queue: bool,
) -> Result<(), ServerError> {
    let cancelled =
        enqueue_cancel_turn_command(state, session_id, clear_queue, Some(client_id)).await?;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled }),
    )
    .await
}

async fn handle_cancel_runtime_work(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    work_id: RuntimeWorkId,
) -> Result<(), ServerError> {
    let cancelled =
        cancel_registered_runtime_work(state, session_id, work_id, Some(client_id)).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::RuntimeWorkCancellationRequested { cancelled }),
    )
    .await
}

async fn handle_list_runtime_work(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    let work = state.runtime_work.active_for_session(session_id).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::RuntimeWorkList { work }),
    )
    .await
}

async fn handle_runtime_work_history(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    limit: usize,
) -> Result<(), ServerError> {
    let mut events = state
        .sessions
        .runtime_work_history(session_id, limit)
        .await?
        .into_iter()
        .flat_map(|work| runtime_work_projection_to_events(session_id, work))
        .collect::<Vec<_>>();
    if limit > 0 && events.len() > limit {
        events.drain(0..events.len() - limit);
    }
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::RuntimeWorkHistory { events }),
    )
    .await
}

fn runtime_work_projection_to_events(
    session_id: SessionId,
    work: bcode_session::db::RuntimeWorkProjection,
) -> Vec<bcode_session_models::SessionEvent> {
    let start = bcode_session_models::SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: work.event_seq_start,
        timestamp_ms: work.started_at_ms.unwrap_or(0),
        session_id,
        provenance: None,
        kind: SessionEventKind::RuntimeWorkStarted {
            work_id: work.work_id.clone(),
            kind: work.kind,
            label: work.label,
            tool_call_id: None,
            plugin_id: None,
            service_interface: None,
            operation: None,
            parent_work_id: work.parent_work_id,
            started_at_ms: work.started_at_ms,
            cancellable: work.cancellable,
        },
    };
    if work.status == RuntimeWorkStatus::Running {
        return vec![start];
    }
    let finish = bcode_session_models::SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: work.event_seq_end.unwrap_or(work.event_seq_start),
        timestamp_ms: work.finished_at_ms.or(work.started_at_ms).unwrap_or(0),
        session_id,
        provenance: None,
        kind: SessionEventKind::RuntimeWorkFinished {
            work_id: work.work_id,
            status: work.status,
            finished_at_ms: work.finished_at_ms,
            message: work.message,
        },
    };
    vec![start, finish]
}

async fn handle_subscribe_runtime_work(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    let attachment = state
        .sessions
        .attach_session_recent(session_id, ClientId::new(), 1)
        .await?;
    let handle = forward_runtime_work_events(
        ClientEventSink::new(client_id, writer.clone()),
        attachment.events,
    );
    state.register_client_forwarder(client_id, handle).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::RuntimeWorkSubscribed),
    )
    .await
}

async fn handle_compact_session(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    if let Some(active_namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    let selection = session_model_selection_with_runtime_context(
        state,
        session_id,
        state.client_runtime_context(client_id).await,
    )
    .await;
    match enqueue_compact_session_command(state, session_id, selection).await? {
        Ok(message) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionCompacted {
                    compacted: true,
                    message,
                }),
            )
            .await
        }
        Err(CompactionError::NothingToCompact(message)) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionCompacted {
                    compacted: false,
                    message,
                }),
            )
            .await
        }
        Err(CompactionError::Session(error)) => {
            send_response(
                writer,
                request_id,
                Response::Err(session_error_response(&error)),
            )
            .await
        }
        Err(CompactionError::Busy) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "session_busy",
                    "cannot compact while a model turn is active for this session",
                )),
            )
            .await
        }
        Err(CompactionError::ProviderUnavailable) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new(
                    "provider_unavailable",
                    "model provider unavailable",
                )),
            )
            .await
        }
        Err(CompactionError::Provider(error)) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("plugin_error", error)),
            )
            .await
        }
    }
}

async fn handle_list_permissions(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let permissions = state
        .pending_permissions
        .lock()
        .await
        .values()
        .map(|permission| permission.summary.clone())
        .collect();
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::PermissionList { permissions }),
    )
    .await
}

async fn handle_add_permission_rule(
    request_id: u64,
    _state: &ServerState,
    writer: &SharedWriter,
    agent_id: &str,
    category: &str,
    pattern: String,
    action: &str,
) -> Result<(), ServerError> {
    match bcode_config::upsert_agent_permission_rule(agent_id, category, pattern, action) {
        Ok(path) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::PermissionRuleAdded {
                    config_path: path.display().to_string(),
                }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("config_error", error.to_string())),
            )
            .await
        }
    }
}

async fn handle_resolve_permission(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    permission_id: &str,
    approved: bool,
    remember: bool,
) -> Result<(), ServerError> {
    let Some(permission) = state.pending_permissions.lock().await.remove(permission_id) else {
        return send_response(
            writer,
            request_id,
            Response::Ok(ResponsePayload::PermissionResolved { resolved: false }),
        )
        .await;
    };
    if remember && let Some(key) = permission.skill_decision_key.clone() {
        remember_skill_tool_decision(
            key,
            if approved {
                SkillToolDecision::Allow
            } else {
                SkillToolDecision::Deny
            },
        );
    }
    *permission.decision.lock().await = Some(approved);
    permission.notify.notify_waiters();
    append_permission_resolved_event(
        state,
        permission.summary.session_id,
        permission.summary.permission_id,
        approved,
    )
    .await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::PermissionResolved { resolved: true }),
    )
    .await
}

const MODEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MODEL_STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
const MODEL_STREAM_FLUSH_BYTES: usize = 512;
const TOOL_OUTPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
const TOOL_OUTPUT_FLUSH_BYTES: usize = 4096;
const TOOL_ARGUMENTS_DECODE_FAILED_CODE: &str = "tool_arguments_decode_failed";
const MALFORMED_TOOL_ARGUMENTS_RETRY_INSTRUCTION: &str = "The previous model turn emitted malformed JSON for a tool call, so the tool did not run. Reissue the intended tool call with valid JSON arguments. Do not explain unless the user explicitly asked for an explanation.";

#[derive(Debug, Clone, Default)]
struct ModelPollOutcome {
    stop_reason: Option<bcode_model::StopReason>,
    completion: Option<ModelTurnCompletion>,
    provider_error: Option<bcode_model::ProviderError>,
}

#[derive(Debug, Clone)]
struct ToolArgumentStreamProgress {
    call_id: String,
    name: String,
    argument_bytes: usize,
    last_emitted_argument_bytes: usize,
    last_emitted_at: Option<Instant>,
    emitted_progress_events: usize,
    force_emit_final: bool,
    preview_fields: StreamingJsonStringFields,
    preview_metadata: Option<bcode_tool::ToolLiveArgumentPreviewMetadata>,
    last_emitted_preview: Option<LiveToolArgumentPreview>,
}

#[derive(Debug, Default)]
struct ModelStreamProgress {
    active_tool_call: Option<ToolArgumentStreamProgress>,
}

#[derive(Debug)]
enum ToolOutputLivePublisherEvent {
    FlushDue { generation: u64 },
}

#[derive(Debug)]
struct ToolOutputLivePublisher {
    pending_output: Option<ToolOutputStreamAccumulator>,
    flush_generation: u64,
    flush_tx: mpsc::UnboundedSender<ToolOutputLivePublisherEvent>,
    flush_rx: mpsc::UnboundedReceiver<ToolOutputLivePublisherEvent>,
}

impl ToolOutputLivePublisher {
    fn new() -> Self {
        let (flush_tx, flush_rx) = mpsc::unbounded_channel();
        Self {
            pending_output: None,
            flush_generation: 0,
            flush_tx,
            flush_rx,
        }
    }

    async fn next_event(&mut self) -> Option<ToolOutputLivePublisherEvent> {
        self.flush_rx.recv().await
    }

    async fn push_stream_event(
        &mut self,
        state: &ServerState,
        session_id: SessionId,
        event: ToolInvocationStreamEvent,
    ) {
        let ToolInvocationStreamEvent::OutputDelta {
            tool_call_id,
            stream,
            sequence,
            text,
            byte_len,
        } = event
        else {
            self.flush(state, session_id).await;
            append_tool_stream_event(state, session_id, event).await;
            return;
        };

        let can_absorb = self
            .pending_output
            .as_ref()
            .is_some_and(|output| output.can_absorb(&tool_call_id, stream));
        if !can_absorb {
            self.flush(state, session_id).await;
            self.pending_output = Some(ToolOutputStreamAccumulator::new(
                tool_call_id,
                stream,
                sequence,
                text,
                byte_len,
            ));
            self.schedule_flush();
        } else if let Some(output) = self.pending_output.as_mut() {
            output.push(&text, byte_len);
        }

        if self
            .pending_output
            .as_ref()
            .is_some_and(ToolOutputStreamAccumulator::should_flush)
        {
            self.flush(state, session_id).await;
        }
    }

    async fn handle_event(
        &mut self,
        state: &ServerState,
        session_id: SessionId,
        event: ToolOutputLivePublisherEvent,
    ) {
        match event {
            ToolOutputLivePublisherEvent::FlushDue { generation }
                if generation == self.flush_generation =>
            {
                self.flush(state, session_id).await;
            }
            ToolOutputLivePublisherEvent::FlushDue { .. } => {}
        }
    }

    async fn finish(&mut self, state: &ServerState, session_id: SessionId) {
        self.flush(state, session_id).await;
    }

    async fn flush(&mut self, state: &ServerState, session_id: SessionId) {
        self.flush_generation = self.flush_generation.wrapping_add(1);
        flush_tool_output_stream(state, session_id, &mut self.pending_output).await;
    }

    fn schedule_flush(&mut self) {
        self.flush_generation = self.flush_generation.wrapping_add(1);
        let generation = self.flush_generation;
        let flush_tx = self.flush_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(TOOL_OUTPUT_FLUSH_INTERVAL).await;
            let _ = flush_tx.send(ToolOutputLivePublisherEvent::FlushDue { generation });
        });
    }
}

#[derive(Debug)]
struct ToolOutputStreamAccumulator {
    tool_call_id: String,
    stream: SessionToolOutputStream,
    first_sequence: u64,
    text: String,
    byte_len: usize,
    last_flush: Instant,
}

impl ToolOutputStreamAccumulator {
    fn new(
        tool_call_id: String,
        stream: SessionToolOutputStream,
        sequence: u64,
        text: String,
        byte_len: usize,
    ) -> Self {
        Self {
            tool_call_id,
            stream,
            first_sequence: sequence,
            text,
            byte_len,
            last_flush: Instant::now(),
        }
    }

    fn can_absorb(&self, tool_call_id: &str, stream: SessionToolOutputStream) -> bool {
        self.tool_call_id == tool_call_id && self.stream == stream
    }

    fn push(&mut self, text: &str, byte_len: usize) {
        self.text.push_str(text);
        self.byte_len = self.byte_len.saturating_add(byte_len);
    }

    fn should_flush(&self) -> bool {
        self.byte_len >= TOOL_OUTPUT_FLUSH_BYTES
            || self.last_flush.elapsed() >= TOOL_OUTPUT_FLUSH_INTERVAL
    }

    fn into_event(self) -> ToolInvocationStreamEvent {
        ToolInvocationStreamEvent::OutputDelta {
            tool_call_id: self.tool_call_id,
            stream: self.stream,
            sequence: self.first_sequence,
            text: self.text,
            byte_len: self.byte_len,
        }
    }
}

async fn flush_tool_output_stream(
    state: &ServerState,
    session_id: SessionId,
    pending_output: &mut Option<ToolOutputStreamAccumulator>,
) {
    if let Some(output) = pending_output.take() {
        let _ = state
            .sessions
            .publish_live_event(
                session_id,
                SessionLiveEventKind::ToolOutputDelta {
                    event: output.into_event(),
                },
            )
            .await;
    }
}

#[cfg(test)]
async fn push_tool_output_stream(
    state: &ServerState,
    session_id: SessionId,
    pending_output: &mut Option<ToolOutputStreamAccumulator>,
    event: ToolInvocationStreamEvent,
) {
    let ToolInvocationStreamEvent::OutputDelta {
        tool_call_id,
        stream,
        sequence,
        text,
        byte_len,
    } = event
    else {
        append_tool_stream_event(state, session_id, event).await;
        return;
    };

    let can_absorb = pending_output
        .as_ref()
        .is_some_and(|output| output.can_absorb(&tool_call_id, stream));
    if !can_absorb {
        flush_tool_output_stream(state, session_id, pending_output).await;
        *pending_output = Some(ToolOutputStreamAccumulator::new(
            tool_call_id,
            stream,
            sequence,
            text,
            byte_len,
        ));
    } else if let Some(output) = pending_output.as_mut() {
        output.push(&text, byte_len);
    }

    if pending_output
        .as_ref()
        .is_some_and(ToolOutputStreamAccumulator::should_flush)
    {
        flush_tool_output_stream(state, session_id, pending_output).await;
    }
}

#[derive(Debug)]
struct ModelStreamAccumulator {
    session_id: SessionId,
    turn_id: String,
    assistant_text: String,
    pending_text: String,
    pending_reasoning: String,
    last_flush: Instant,
}

impl ModelStreamAccumulator {
    fn new(session_id: SessionId, turn_id: &str) -> Self {
        Self {
            session_id,
            turn_id: turn_id.to_owned(),
            assistant_text: String::new(),
            pending_text: String::new(),
            pending_reasoning: String::new(),
            last_flush: Instant::now(),
        }
    }

    fn push_text(&mut self, text: &str) {
        self.assistant_text.push_str(text);
        self.pending_text.push_str(text);
    }

    fn push_reasoning(&mut self, text: &str) {
        self.pending_reasoning.push_str(text);
    }

    fn should_flush(&self) -> bool {
        self.pending_text
            .len()
            .saturating_add(self.pending_reasoning.len())
            >= MODEL_STREAM_FLUSH_BYTES
            || self.last_flush.elapsed() >= MODEL_STREAM_FLUSH_INTERVAL
    }

    async fn flush_if_ready(&mut self, state: &ServerState) {
        if self.should_flush() {
            self.flush(state).await;
        }
    }

    async fn flush(&mut self, state: &ServerState) {
        let text = std::mem::take(&mut self.pending_text);
        if !text.is_empty() {
            let _ = state
                .sessions
                .publish_live_event(
                    self.session_id,
                    SessionLiveEventKind::AssistantTextDelta {
                        turn_id: self.turn_id.clone(),
                        text,
                    },
                )
                .await;
        }
        let reasoning = std::mem::take(&mut self.pending_reasoning);
        if !reasoning.is_empty() {
            let _ = state
                .sessions
                .publish_live_event(
                    self.session_id,
                    SessionLiveEventKind::AssistantReasoningDelta {
                        turn_id: self.turn_id.clone(),
                        text: reasoning,
                    },
                )
                .await;
        }
        self.last_flush = Instant::now();
    }

    fn take_assistant_text(&mut self) -> String {
        std::mem::take(&mut self.assistant_text)
    }

    fn finish(self) -> String {
        self.assistant_text
    }
}

impl ModelStreamProgress {
    const FIRST_TOOL_PROGRESS_BYTES: usize = 512;
    const TOOL_PROGRESS_MIN_BYTES: usize = 1024;
    const TOOL_PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);
    const MAX_TOOL_PROGRESS_EVENTS: usize = 512;
    const TOOL_ARGUMENT_FIELD_PREVIEW_MAX_CHARS: usize = 32 * 1024;

    fn start_tool_call(
        &mut self,
        call_id: String,
        name: String,
        preview_metadata: Option<bcode_tool::ToolLiveArgumentPreviewMetadata>,
    ) {
        self.active_tool_call = Some(ToolArgumentStreamProgress {
            call_id,
            name,
            argument_bytes: 0,
            last_emitted_argument_bytes: 0,
            last_emitted_at: None,
            emitted_progress_events: 0,
            force_emit_final: false,
            preview_fields: StreamingJsonStringFields::default(),
            preview_metadata,
            last_emitted_preview: None,
        });
    }

    fn record_completed_tool_call(&mut self, call: &bcode_model::ToolCall) {
        if self
            .active_tool_call
            .as_ref()
            .is_none_or(|active| active.call_id != call.id)
        {
            self.start_tool_call(call.id.clone(), call.name.clone(), None);
        }
        if let Some(active) = self.active_tool_call.as_mut() {
            active.argument_bytes = serialized_tool_argument_len(&call.arguments);
            active.force_emit_final = true;
            active.preview_fields = StreamingJsonStringFields::from_json_value(&call.arguments);
        }
    }

    fn finish_tool_call(&mut self, call_id: &str) {
        if self
            .active_tool_call
            .as_ref()
            .is_some_and(|active| active.call_id == call_id)
        {
            self.active_tool_call = None;
        }
    }

    fn record_tool_call_delta(&mut self, call_id: &str, delta: &str) {
        if let Some(active) = self.active_tool_call.as_mut()
            && active.call_id == call_id
        {
            active.argument_bytes = active.argument_bytes.saturating_add(delta.len());
            active.preview_fields.push(delta);
        }
    }

    fn take_tool_progress_event(&mut self) -> Option<ProviderToolCallProgress> {
        let active = self.active_tool_call.as_mut()?;
        if !active.force_emit_final {
            if active.emitted_progress_events >= Self::MAX_TOOL_PROGRESS_EVENTS {
                return None;
            }
            if active.emitted_progress_events == 0 {
                if active.argument_bytes < Self::FIRST_TOOL_PROGRESS_BYTES {
                    return None;
                }
            } else {
                let byte_delta = active
                    .argument_bytes
                    .saturating_sub(active.last_emitted_argument_bytes);
                if byte_delta < Self::TOOL_PROGRESS_MIN_BYTES {
                    return None;
                }
                if active.last_emitted_at.is_some_and(|emitted_at| {
                    emitted_at.elapsed() < Self::TOOL_PROGRESS_MIN_INTERVAL
                }) {
                    return None;
                }
            }
        } else if active.argument_bytes == active.last_emitted_argument_bytes {
            return None;
        }
        active.force_emit_final = false;
        active.emitted_progress_events = active.emitted_progress_events.saturating_add(1);
        active.last_emitted_argument_bytes = active.argument_bytes;
        active.last_emitted_at = Some(Instant::now());
        Some(ProviderToolCallProgress {
            tool_call_id: active.call_id.clone(),
            tool_name: active.name.clone(),
            argument_bytes: active.argument_bytes,
        })
    }

    fn take_tool_argument_preview(&mut self) -> Option<LiveToolArgumentPreview> {
        let active = self.active_tool_call.as_mut()?;
        let preview = live_tool_argument_preview_from_fields(
            active.preview_metadata.as_ref()?,
            &active.preview_fields,
        )?;
        if active
            .last_emitted_preview
            .as_ref()
            .is_some_and(|last| last == &preview)
        {
            return None;
        }
        active.last_emitted_preview = Some(preview.clone());
        Some(preview)
    }

    fn tool_progress_snapshot(&self) -> Option<ProviderToolCallProgress> {
        let active = self.active_tool_call.as_ref()?;
        Some(ProviderToolCallProgress {
            tool_call_id: active.call_id.clone(),
            tool_name: active.name.clone(),
            argument_bytes: active.argument_bytes,
        })
    }
}

fn serialized_tool_argument_len(arguments: &serde_json::Value) -> usize {
    serde_json::to_vec(arguments).map_or(0, |encoded| encoded.len())
}

fn live_tool_argument_preview_from_fields(
    metadata: &bcode_tool::ToolLiveArgumentPreviewMetadata,
    fields: &StreamingJsonStringFields,
) -> Option<LiveToolArgumentPreview> {
    match metadata {
        bcode_tool::ToolLiveArgumentPreviewMetadata::FileEdit {
            path_fields,
            old_text_fields,
            new_text_fields,
            preview_title,
            streaming_status,
        } => live_file_edit_preview_from_fields(
            fields,
            path_fields,
            old_text_fields,
            new_text_fields,
            preview_title.clone(),
            streaming_status.clone(),
        )
        .map(LiveToolArgumentPreview::FileEdit),
        bcode_tool::ToolLiveArgumentPreviewMetadata::ShellCommand {
            command_field,
            cwd_field,
            preview_title,
            streaming_status,
        } => live_shell_command_preview_from_fields(
            fields,
            command_field,
            cwd_field.as_deref(),
            preview_title.clone(),
            streaming_status.clone(),
        )
        .map(LiveToolArgumentPreview::ShellCommand),
        bcode_tool::ToolLiveArgumentPreviewMetadata::Query {
            fields: field_names,
            preview_title,
            streaming_status,
        } => live_query_preview_from_fields(
            fields,
            field_names,
            preview_title.clone(),
            streaming_status.clone(),
        )
        .map(LiveToolArgumentPreview::Query),
    }
}

fn live_file_edit_preview_from_fields(
    fields: &StreamingJsonStringFields,
    path_fields: &[String],
    old_text_fields: &[String],
    new_text_fields: &[String],
    preview_title: Option<String>,
    streaming_status: Option<String>,
) -> Option<LiveFileEditPreview> {
    let path = fields.field_owned(path_fields).map(|field| field.value);
    let old_text_prefix = fields.field_owned(old_text_fields).map(|field| field.value);
    let new_text = fields.field_owned(new_text_fields)?;
    Some(LiveFileEditPreview {
        preview_title,
        streaming_status,
        path,
        old_text_prefix,
        new_text_prefix: new_text.value,
        argument_bytes: fields.input_bytes,
        truncated: new_text.truncated,
    })
}

fn live_shell_command_preview_from_fields(
    fields: &StreamingJsonStringFields,
    command_field: &str,
    cwd_field: Option<&str>,
    preview_title: Option<String>,
    streaming_status: Option<String>,
) -> Option<LiveShellCommandPreview> {
    let command = fields.field(&[command_field])?;
    let cwd = cwd_field.and_then(|field| fields.field(&[field]).map(|value| value.value));
    Some(LiveShellCommandPreview {
        preview_title,
        streaming_status,
        command_prefix: command.value,
        cwd,
        argument_bytes: fields.input_bytes,
        truncated: command.truncated,
    })
}

fn live_query_preview_from_fields(
    fields: &StreamingJsonStringFields,
    field_names: &[String],
    preview_title: Option<String>,
    streaming_status: Option<String>,
) -> Option<LiveQueryPreview> {
    let mut preview_fields = BTreeMap::new();
    let mut truncated = false;
    for name in field_names {
        if let Some(field) = fields.field(&[name.as_str()]) {
            truncated |= field.truncated;
            preview_fields.insert(name.clone(), field.value);
        }
    }
    if preview_fields.is_empty() {
        return None;
    }
    Some(LiveQueryPreview {
        preview_title,
        streaming_status,
        fields: preview_fields,
        argument_bytes: fields.input_bytes,
        truncated,
    })
}

#[derive(Debug, Clone)]
struct PartialJsonStringField {
    value: String,
    truncated: bool,
}

#[derive(Debug, Clone, Default)]
struct StreamingJsonStringFields {
    input_bytes: usize,
    fields: BTreeMap<String, PartialJsonStringField>,
    parser: StreamingJsonStringFieldParser,
}

impl StreamingJsonStringFields {
    const FIELD_VALUE_MAX_CHARS: usize = ModelStreamProgress::TOOL_ARGUMENT_FIELD_PREVIEW_MAX_CHARS;

    fn from_json_value(value: &serde_json::Value) -> Self {
        let mut fields = Self {
            input_bytes: serialized_tool_argument_len(value),
            ..Self::default()
        };
        if let serde_json::Value::Object(object) = value {
            for (key, value) in object {
                if let Some(value) = value.as_str() {
                    fields.insert_field(key, value, false);
                }
            }
        }
        fields
    }

    fn push(&mut self, delta: &str) {
        self.input_bytes = self.input_bytes.saturating_add(delta.len());
        let updates = self.parser.push(delta);
        for update in updates {
            self.fields.insert(update.name, update.field);
        }
    }

    fn field(&self, names: &[&str]) -> Option<PartialJsonStringField> {
        names
            .iter()
            .find_map(|name| self.fields.get(*name).cloned())
    }

    fn field_owned(&self, names: &[String]) -> Option<PartialJsonStringField> {
        names.iter().find_map(|name| self.fields.get(name).cloned())
    }

    fn insert_field(&mut self, name: &str, value: &str, truncated: bool) {
        let mut field = decode_partial_json_string_from_value(value, Self::FIELD_VALUE_MAX_CHARS);
        field.truncated |= truncated;
        self.fields.insert(name.to_owned(), field);
    }
}

#[derive(Debug, Clone, Default)]
struct StreamingJsonStringFieldParser {
    state: JsonFieldParserState,
    key: String,
    value: String,
    value_chars: usize,
    value_truncated: bool,
    escape: JsonStringEscapeState,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum JsonFieldParserState {
    #[default]
    SeekingKey,
    InKey,
    AfterKey,
    BeforeValue,
    InStringValue,
    SkippingValue,
}

#[derive(Debug, Clone, Default)]
enum JsonStringEscapeState {
    #[default]
    None,
    Escape,
    Unicode {
        digits: String,
    },
}

#[derive(Debug, Clone)]
struct StreamingJsonStringFieldUpdate {
    name: String,
    field: PartialJsonStringField,
}

impl StreamingJsonStringFieldParser {
    fn push(&mut self, input: &str) -> Vec<StreamingJsonStringFieldUpdate> {
        let mut updates = Vec::new();
        for ch in input.chars() {
            self.push_char(ch, &mut updates);
        }
        if matches!(self.state, JsonFieldParserState::InStringValue) {
            updates.push(self.current_update(true));
        }
        updates
    }

    fn push_char(&mut self, ch: char, updates: &mut Vec<StreamingJsonStringFieldUpdate>) {
        match self.state {
            JsonFieldParserState::SeekingKey => {
                if ch == '"' {
                    self.key.clear();
                    self.escape = JsonStringEscapeState::None;
                    self.state = JsonFieldParserState::InKey;
                }
            }
            JsonFieldParserState::InKey => match self.decode_string_char(ch) {
                JsonStringChar::Char(decoded) => self.key.push(decoded),
                JsonStringChar::End => self.state = JsonFieldParserState::AfterKey,
                JsonStringChar::Pending => {}
            },
            JsonFieldParserState::AfterKey => {
                if ch == ':' {
                    self.state = JsonFieldParserState::BeforeValue;
                } else if !ch.is_whitespace() {
                    self.state = JsonFieldParserState::SeekingKey;
                }
            }
            JsonFieldParserState::BeforeValue => {
                if ch == '"' {
                    self.value.clear();
                    self.value_chars = 0;
                    self.value_truncated = false;
                    self.escape = JsonStringEscapeState::None;
                    self.state = JsonFieldParserState::InStringValue;
                } else if !ch.is_whitespace() {
                    self.state = JsonFieldParserState::SkippingValue;
                }
            }
            JsonFieldParserState::InStringValue => match self.decode_string_char(ch) {
                JsonStringChar::Char(decoded) => self.push_value_char(decoded),
                JsonStringChar::End => {
                    updates.push(self.current_update(false));
                    self.state = JsonFieldParserState::SeekingKey;
                }
                JsonStringChar::Pending => {}
            },
            JsonFieldParserState::SkippingValue => {
                if ch == ',' || ch == '}' {
                    self.state = JsonFieldParserState::SeekingKey;
                }
            }
        }
    }

    fn push_value_char(&mut self, ch: char) {
        if self.value_chars < StreamingJsonStringFields::FIELD_VALUE_MAX_CHARS {
            self.value.push(ch);
            self.value_chars = self.value_chars.saturating_add(1);
        } else {
            self.value_truncated = true;
        }
    }

    fn current_update(&self, partial: bool) -> StreamingJsonStringFieldUpdate {
        StreamingJsonStringFieldUpdate {
            name: self.key.clone(),
            field: PartialJsonStringField {
                value: self.value.clone(),
                truncated: self.value_truncated || partial,
            },
        }
    }

    fn decode_string_char(&mut self, ch: char) -> JsonStringChar {
        match &mut self.escape {
            JsonStringEscapeState::None => match ch {
                '"' => JsonStringChar::End,
                '\\' => {
                    self.escape = JsonStringEscapeState::Escape;
                    JsonStringChar::Pending
                }
                other => JsonStringChar::Char(other),
            },
            JsonStringEscapeState::Escape => match ch {
                'n' => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char('\n')
                }
                'r' => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char('\r')
                }
                't' => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char('\t')
                }
                '"' => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char('"')
                }
                '\\' => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char('\\')
                }
                '/' => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char('/')
                }
                'b' => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char('\u{0008}')
                }
                'f' => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char('\u{000c}')
                }
                'u' => {
                    self.escape = JsonStringEscapeState::Unicode {
                        digits: String::new(),
                    };
                    JsonStringChar::Pending
                }
                other => {
                    self.escape = JsonStringEscapeState::None;
                    JsonStringChar::Char(other)
                }
            },
            JsonStringEscapeState::Unicode { digits } => {
                digits.push(ch);
                if digits.len() < 4 {
                    return JsonStringChar::Pending;
                }
                let decoded = u32::from_str_radix(digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or('\u{FFFD}');
                self.escape = JsonStringEscapeState::None;
                JsonStringChar::Char(decoded)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonStringChar {
    Char(char),
    End,
    Pending,
}

fn decode_partial_json_string_from_value(value: &str, max_chars: usize) -> PartialJsonStringField {
    let mut truncated = false;
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            truncated = true;
            break;
        }
        output.push(ch);
    }
    PartialJsonStringField {
        value: output,
        truncated,
    }
}

#[derive(Default)]
struct ModelTurnRecoveryState {
    retried_after_context_overflow: bool,
    retried_after_malformed_tool_arguments: bool,
    retry_attempts: BTreeMap<String, u8>,
    retry_instruction: Option<&'static str>,
}

impl ModelTurnRecoveryState {
    fn record_successful_provider_round(&mut self) {
        self.retry_attempts.clear();
        self.retry_instruction = None;
    }
}

struct ProviderErrorRetryContext<'a> {
    trigger_event_sequence: u64,
    turn_id: &'a str,
    selection: &'a SessionModelSelection,
    provider_retry_rules: &'a [bcode_model::ProviderRetryRule],
    remote_catalog_retry_rules: &'a [bcode_model::ProviderRetryRule],
    cancel_state: &'a TurnCancelState,
}

enum ModelTurnRetry {
    None,
    Continue,
    Return(ModelTurnCompletion),
}

fn format_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if bytes >= MIB {
        let whole = bytes / MIB;
        let decimal = (bytes % MIB) * 10 / MIB;
        format!("{whole}.{decimal} MiB")
    } else if bytes >= KIB {
        let whole = bytes / KIB;
        let decimal = (bytes % KIB) * 10 / KIB;
        format!("{whole}.{decimal} KiB")
    } else {
        format!("{bytes} B")
    }
}

#[derive(Debug, Clone)]
struct ModelTurnCompletion {
    outcome: ModelTurnOutcome,
    message: Option<String>,
}

impl ModelTurnCompletion {
    const fn completed() -> Self {
        Self {
            outcome: ModelTurnOutcome::Completed,
            message: None,
        }
    }

    fn with_message(outcome: ModelTurnOutcome, message: impl Into<String>) -> Self {
        Self {
            outcome,
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, Error)]
enum CompactionError {
    #[error("nothing to compact: {0}")]
    NothingToCompact(String),
    #[error("session error: {0}")]
    Session(#[from] bcode_session::SessionError),
    #[error("model provider unavailable")]
    ProviderUnavailable,
    #[error("session has an active model turn")]
    Busy,
    #[error("provider error: {0}")]
    Provider(String),
}

struct CompactionTranscript {
    previous_summary: Option<String>,
    lines: Vec<CompactionLine>,
    compacted_through_sequence: u64,
    event_count: usize,
}

struct CompactionLine {
    sequence: u64,
    text: String,
    can_cut_after: bool,
}

const COMPACTION_SYSTEM_PROMPT: &str = "You compact coding-agent session history. Produce only a durable continuation summary for future model turns. Preserve all facts needed to continue the work, including user goals, decisions, constraints, files changed, commands run, validation results, current blockers, and next steps. Do not invent details. Do not include markdown fences.";
const COMPACTION_KEEP_RECENT_CHARS: usize = 8_000;
const COMPACTION_MAX_SUMMARY_INPUT_CHARS: usize = 16_000;
const COMPACTION_MAX_CARRIED_SUMMARY_CHARS: usize = 6_000;
const COMPACTION_MAX_EVENT_CONTENT_CHARS: usize = 4_000;
const COMPACTION_TOOL_RESULT_CHARS: usize = 2_000;

async fn compact_session_context(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
) -> Result<String, CompactionError> {
    compact_session_context_with_limit(state, session_id, selection, None, None).await
}

async fn compact_session_context_before_sequence(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    first_kept_sequence: u64,
) -> Result<String, CompactionError> {
    compact_session_context_with_limit(
        state,
        session_id,
        selection,
        Some(first_kept_sequence),
        None,
    )
    .await
}

async fn compact_session_context_with_limit(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    first_kept_sequence: Option<u64>,
    command_context: Option<&mut RuntimeCommandContext<'_>>,
) -> Result<String, CompactionError> {
    if state.session_has_active_turn(session_id).await {
        return Err(CompactionError::Busy);
    }

    let history = state.sessions.model_context_events(session_id).await?;
    let transcript_history = first_kept_sequence.map_or_else(
        || history.clone(),
        |first_kept_sequence| {
            history
                .iter()
                .filter(|event| event.sequence < first_kept_sequence)
                .cloned()
                .collect()
        },
    );
    let Some(transcript) =
        compaction_transcript(&transcript_history, state.tool_output_context_chars)
    else {
        return Err(CompactionError::NothingToCompact(
            "nothing new to compact".to_string(),
        ));
    };

    if !has_model_provider(state, selection.provider_plugin_id.as_deref()) {
        return Err(CompactionError::ProviderUnavailable);
    }

    let summary =
        collect_compaction_summary(state, session_id, selection, &transcript, command_context)
            .await?;
    let summary = summary.trim().to_string();
    if summary.is_empty() {
        return Err(CompactionError::Provider(
            "provider returned an empty compaction summary".to_string(),
        ));
    }

    let event = state
        .sessions
        .append_context_compacted(session_id, summary, transcript.compacted_through_sequence)
        .await?;
    publish_session_event(state, &event).await;

    Ok(format!(
        "compacted {} events through #{}",
        transcript.event_count, transcript.compacted_through_sequence
    ))
}

async fn maybe_auto_compact_session_context(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
) -> Result<(), CompactionError> {
    if !state.auto_compaction.mode.is_proactive_enabled()
        || state.auto_compaction.context_chars == 0
    {
        return Ok(());
    }
    if state.session_has_active_turn(session_id).await {
        append_context_compaction_trace(
            state,
            session_id,
            "active_turn",
            0,
            false,
            Some("skipping auto compaction while a turn is active".to_string()),
        )
        .await;
        return Ok(());
    }

    let history = state.sessions.model_context_events(session_id).await?;
    let projected_context_chars =
        projected_model_context_chars(&history, state.tool_output_context_chars);
    if projected_context_chars < state.auto_compaction.context_chars {
        append_context_compaction_trace(
            state,
            session_id,
            "below_threshold",
            projected_context_chars,
            false,
            None,
        )
        .await;
        return Ok(());
    }

    append_context_compaction_trace(
        state,
        session_id,
        "threshold_exceeded",
        projected_context_chars,
        false,
        Some(format!(
            "projected context {projected_context_chars} chars >= threshold {} chars",
            state.auto_compaction.context_chars
        )),
    )
    .await;
    let message = compact_session_context(state, session_id, selection).await?;
    append_context_compaction_trace(
        state,
        session_id,
        "threshold_exceeded",
        projected_context_chars,
        true,
        Some(message),
    )
    .await;
    Ok(())
}

async fn append_context_compaction_trace(
    state: &ServerState,
    session_id: SessionId,
    reason: &str,
    projected_context_chars: usize,
    compacted: bool,
    message: Option<String>,
) {
    let phase = if compacted {
        SessionTracePhase::ContextCompactionFinished
    } else if reason == "below_threshold" {
        SessionTracePhase::ContextCompactionSkipped
    } else {
        SessionTracePhase::ContextCompactionStarted
    };
    append_trace_event(
        state,
        session_id,
        None,
        phase,
        SessionTracePayload::ContextCompaction {
            reason: reason.to_string(),
            projected_context_chars,
            compacted,
            message,
        },
    )
    .await;
}

fn projected_model_context_chars(
    history: &[bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
) -> usize {
    session_events_to_model_messages_with_limit(history, tool_output_context_chars)
        .iter()
        .map(model_message_context_chars)
        .sum()
}

fn model_message_context_chars(message: &ModelMessage) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.chars().count(),
            ContentBlock::Image { .. } | ContentBlock::CachePoint { .. } => 0,
            ContentBlock::ToolCall { call } => {
                call.name.chars().count() + call.arguments.to_string().chars().count()
            }
            ContentBlock::ToolResult { result } => result.output.chars().count(),
            ContentBlock::ProviderExtension { value } => value.to_string().chars().count(),
        })
        .sum()
}

async fn collect_compaction_summary(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    transcript: &CompactionTranscript,
    command_context: Option<&mut RuntimeCommandContext<'_>>,
) -> Result<String, CompactionError> {
    append_context_compaction_trace(
        state,
        session_id,
        "summary_request",
        0,
        false,
        Some("compacting older context in one bounded request".to_string()),
    )
    .await;

    let prompt_text = compaction_prompt_text(transcript);
    match collect_compaction_summary_once(
        state,
        session_id,
        selection,
        transcript,
        &prompt_text,
        command_context,
    )
    .await
    {
        Ok(summary) if !summary.trim().is_empty() => Ok(truncate_text(
            summary.trim(),
            COMPACTION_MAX_CARRIED_SUMMARY_CHARS,
        )),
        Ok(_) => Ok(local_compaction_summary(
            transcript,
            "provider returned an empty summary",
        )),
        Err(error) if is_retriable_compaction_error(&error) => {
            append_context_compaction_trace(
                state,
                session_id,
                "local_fallback",
                0,
                true,
                Some(format!(
                    "compaction provider request failed ({error}); using bounded local summary"
                )),
            )
            .await;
            Ok(local_compaction_summary(transcript, &error))
        }
        Err(error) => Err(CompactionError::Provider(error)),
    }
}

async fn collect_compaction_summary_once(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    transcript: &CompactionTranscript,
    prompt_text: &str,
    mut command_context: Option<&mut RuntimeCommandContext<'_>>,
) -> Result<String, String> {
    let turn_id = format!(
        "{session_id}-compact-{}",
        transcript.compacted_through_sequence
    );
    let request = build_compaction_request(session_id, selection, prompt_text, turn_id.clone());
    let compaction_cancel_state = TurnCancelState::default();
    let provider_turn_id = if let Some(context) = &mut command_context {
        match wait_for_provider_call(
            state,
            session_id,
            context,
            &compaction_cancel_state,
            Box::pin(invoke_model_provider_json_blocking::<_, StartTurnResponse>(
                state,
                selection.provider_plugin_id.clone(),
                OP_START_TURN,
                request,
            )),
        )
        .await
        {
            ProviderCallWait::Completed(result) => result?.provider_turn_id,
            ProviderCallWait::Cancelled => return Err("compaction cancelled".to_string()),
        }
    } else {
        invoke_model_provider_json_blocking::<_, StartTurnResponse>(
            state,
            selection.provider_plugin_id.clone(),
            OP_START_TURN,
            request,
        )
        .await?
        .provider_turn_id
    };

    let result = if let Some(context) = command_context {
        poll_compaction_summary_actor_aware(
            state,
            session_id,
            selection,
            &provider_turn_id,
            &turn_id,
            context,
            &compaction_cancel_state,
        )
        .await
    } else {
        poll_compaction_summary(state, session_id, selection, &provider_turn_id, &turn_id).await
    }
    .map_err(compaction_error_detail);
    finish_provider_turn(
        state,
        selection.provider_plugin_id.clone(),
        provider_turn_id,
    )
    .await;
    result
}

fn build_compaction_request(
    session_id: SessionId,
    selection: &SessionModelSelection,
    prompt_text: &str,
    turn_id: String,
) -> ModelTurnRequest {
    ModelTurnRequest {
        session_id,
        turn_id,
        model_id: model_id_for_provider_request(selection.model_id.as_deref()),
        provider_context: selection.provider_context.clone(),
        system_prompt: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        messages: vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: prompt_text.to_string(),
            }],
        }],
        tools: Vec::new(),
        parameters: ModelParameters::default(),
        prompt_cache: bcode_model::PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: BTreeMap::from([("bcode_request_kind".to_string(), "compaction".to_string())]),
    }
}

fn compaction_prompt_text(transcript: &CompactionTranscript) -> String {
    let previous_summary = transcript
        .previous_summary
        .as_deref()
        .unwrap_or_default()
        .trim();
    let carried_summary = truncate_text(previous_summary, COMPACTION_MAX_CARRIED_SUMMARY_CHARS);
    let transcript_text =
        bounded_compaction_body(&transcript.lines, COMPACTION_MAX_SUMMARY_INPUT_CHARS);
    if carried_summary.is_empty() {
        return format!(
            "Compact this Bcode session transcript for future continuation. Return only the durable continuation summary.\n\nTranscript excerpt:\n\n{transcript_text}"
        );
    }
    format!(
        "Update the existing compacted Bcode session summary with the transcript excerpt. Return only the updated durable continuation summary.\n\nExisting summary:\n\n{carried_summary}\n\nTranscript excerpt:\n\n{transcript_text}"
    )
}

fn bounded_compaction_body(lines: &[CompactionLine], max_chars: usize) -> String {
    truncate_text(
        &lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        max_chars,
    )
}

fn local_compaction_summary(transcript: &CompactionTranscript, reason: &str) -> String {
    let mut parts = Vec::new();
    if let Some(previous) = transcript.previous_summary.as_deref()
        && !previous.trim().is_empty()
    {
        parts.push("## Previous Summary".to_string());
        parts.push(truncate_text(
            previous.trim(),
            COMPACTION_MAX_CARRIED_SUMMARY_CHARS / 2,
        ));
    }
    parts.push("## Local Compaction Fallback".to_string());
    parts.push(format!(
        "Bcode compacted older session context locally because the provider compaction request could not be used: {reason}. The full canonical history remains in durable session storage."
    ));
    parts.push("## Older Context Outline".to_string());
    parts.push(bounded_compaction_body(
        &transcript.lines,
        COMPACTION_MAX_CARRIED_SUMMARY_CHARS / 2,
    ));
    truncate_text(&parts.join("\n\n"), COMPACTION_MAX_CARRIED_SUMMARY_CHARS)
}

fn is_retriable_compaction_error(error: &str) -> bool {
    is_context_length_compaction_error(error) || is_timeout_compaction_error(error)
}

fn is_context_length_compaction_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("context_length")
        || error.contains("context length")
        || error.contains("context window")
        || error.contains("maximum context")
        || error.contains("input exceeds")
        || error.contains("prompt is too long")
        || error.contains("input is too long")
        || error.contains("too many tokens")
}

fn is_timeout_compaction_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("did not finish compaction turn")
        || error.contains("compaction turn timed out")
        || error.contains("provider was idle")
}

fn compaction_error_detail(error: CompactionError) -> String {
    match error {
        CompactionError::Provider(message) => message,
        error => error.to_string(),
    }
}

async fn poll_compaction_summary_actor_aware(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    provider_turn_id: &str,
    turn_id: &str,
    command_context: &mut RuntimeCommandContext<'_>,
    cancel_state: &TurnCancelState,
) -> Result<String, CompactionError> {
    let mut summary = String::new();
    let mut idle_for = Duration::ZERO;
    loop {
        let poll = PollTurnEventsRequest {
            provider_turn_id: provider_turn_id.to_string(),
        };
        let response = match wait_for_provider_call(
            state,
            session_id,
            command_context,
            cancel_state,
            Box::pin(poll_model_turn(
                state,
                session_id,
                selection.provider_plugin_id.as_deref(),
                &poll,
            )),
        )
        .await
        {
            ProviderCallWait::Completed(result) => result.map_err(CompactionError::Provider)?,
            ProviderCallWait::Cancelled => {
                return Err(CompactionError::Provider(
                    "compaction cancelled".to_string(),
                ));
            }
        };
        if response.events.is_empty() {
            idle_for = wait_for_compaction_progress_actor_aware(
                state,
                session_id,
                command_context,
                cancel_state,
                idle_for,
            )
            .await?;
            continue;
        }
        let saw_progress = compaction_events_include_progress(&response.events);
        match handle_compaction_events(state, session_id, turn_id, &mut summary, response.events)
            .await
        {
            CompactionPollStatus::Continue => {
                if saw_progress {
                    idle_for = Duration::ZERO;
                } else {
                    idle_for = wait_for_compaction_progress_actor_aware(
                        state,
                        session_id,
                        command_context,
                        cancel_state,
                        idle_for,
                    )
                    .await?;
                }
            }
            CompactionPollStatus::Finished => return Ok(summary),
            CompactionPollStatus::Failed(error) => return Err(CompactionError::Provider(error)),
        }
    }
}

async fn wait_for_compaction_progress_actor_aware(
    state: &ServerState,
    session_id: SessionId,
    command_context: &mut RuntimeCommandContext<'_>,
    cancel_state: &TurnCancelState,
    idle_for: Duration,
) -> Result<Duration, CompactionError> {
    let idle_for = idle_for.saturating_add(MODEL_POLL_INTERVAL);
    let timeout = Duration::from_secs(state.model_streaming.no_progress_timeout_secs);
    if idle_for > timeout {
        return Err(CompactionError::Provider(format!(
            "model provider made no compaction progress for {} seconds before timeout",
            timeout.as_secs()
        )));
    }
    tokio::select! {
        () = tokio::time::sleep(MODEL_POLL_INTERVAL) => Ok(idle_for),
        cancel_command = command_context.cancel_commands.recv() => {
            if let Some(command) = cancel_command {
                let cancelled = process_cancel_turn_command(
                    state,
                    session_id,
                    command_context.followup_commands,
                    command_context.queued_followups,
                    command.clear_queue,
                    command.requested_by,
                )
                .await;
                let _sent = command.response.send(cancelled);
            }
            if cancel_state.is_cancelled() {
                Err(CompactionError::Provider("compaction cancelled".to_string()))
            } else {
                Ok(idle_for)
            }
        }
        steering_command = command_context.steering_commands.recv() => {
            if let Some(command) = steering_command {
                process_steering_message_command(
                    state,
                    session_id,
                    command.client_id,
                    command.text,
                    command.completion,
                )
                .await;
            }
            Ok(idle_for)
        }
    }
}

async fn poll_compaction_summary(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    provider_turn_id: &str,
    turn_id: &str,
) -> Result<String, CompactionError> {
    let mut summary = String::new();
    let mut idle_for = Duration::ZERO;
    loop {
        let poll = PollTurnEventsRequest {
            provider_turn_id: provider_turn_id.to_string(),
        };
        let response = poll_model_turn(
            state,
            session_id,
            selection.provider_plugin_id.as_deref(),
            &poll,
        )
        .await
        .map_err(CompactionError::Provider)?;
        if response.events.is_empty() {
            idle_for = wait_for_compaction_progress(&state.model_streaming, idle_for).await?;
            continue;
        }
        let saw_progress = compaction_events_include_progress(&response.events);
        match handle_compaction_events(state, session_id, turn_id, &mut summary, response.events)
            .await
        {
            CompactionPollStatus::Continue => {
                if saw_progress {
                    idle_for = Duration::ZERO;
                } else {
                    idle_for =
                        wait_for_compaction_progress(&state.model_streaming, idle_for).await?;
                }
            }
            CompactionPollStatus::Finished => return Ok(summary),
            CompactionPollStatus::Failed(error) => return Err(CompactionError::Provider(error)),
        }
    }
}

fn compaction_events_include_progress(events: &[ProviderTurnEvent]) -> bool {
    events.iter().any(compaction_event_is_progress)
}

const fn compaction_event_is_progress(event: &ProviderTurnEvent) -> bool {
    match event {
        ProviderTurnEvent::TextDelta { text } | ProviderTurnEvent::ReasoningDelta { text } => {
            !text.is_empty()
        }
        _ => false,
    }
}

async fn wait_for_compaction_progress(
    streaming: &bcode_config::StreamingConfig,
    idle_for: Duration,
) -> Result<Duration, CompactionError> {
    let idle_for = idle_for.saturating_add(MODEL_POLL_INTERVAL);
    let timeout = Duration::from_secs(streaming.no_progress_timeout_secs);
    if idle_for > timeout {
        return Err(CompactionError::Provider(format!(
            "model provider made no compaction progress for {} seconds before timeout",
            timeout.as_secs()
        )));
    }
    tokio::time::sleep(MODEL_POLL_INTERVAL).await;
    Ok(idle_for)
}

enum CompactionPollStatus {
    Continue,
    Finished,
    Failed(String),
}

async fn handle_compaction_events(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    summary: &mut String,
    events: Vec<ProviderTurnEvent>,
) -> CompactionPollStatus {
    for event in events {
        match event {
            ProviderTurnEvent::TextDelta { text } => summary.push_str(&text),
            ProviderTurnEvent::Usage { usage } => {
                append_model_usage_event(state, session_id, turn_id.to_string(), usage).await;
            }
            ProviderTurnEvent::Warning { message } => {
                append_system_event(state, session_id, format!("model warning: {message}")).await;
            }
            ProviderTurnEvent::Error { error } => {
                return CompactionPollStatus::Failed(format!(
                    "model error {}: {}",
                    error.code, error.message
                ));
            }
            ProviderTurnEvent::Cancelled => {
                return CompactionPollStatus::Failed("model turn cancelled".to_string());
            }
            ProviderTurnEvent::TurnFinished { stop_reason } => match stop_reason {
                bcode_model::StopReason::Error => {
                    return CompactionPollStatus::Failed("model turn ended with error".to_string());
                }
                bcode_model::StopReason::Cancelled => {
                    return CompactionPollStatus::Failed("model turn cancelled".to_string());
                }
                _ => return CompactionPollStatus::Finished,
            },
            ProviderTurnEvent::ToolCallStarted { .. }
            | ProviderTurnEvent::ToolCallDelta { .. }
            | ProviderTurnEvent::ToolCallFinished { .. } => {
                return CompactionPollStatus::Failed(
                    "compaction summary unexpectedly requested a tool".to_string(),
                );
            }
            ProviderTurnEvent::TurnStarted
            | ProviderTurnEvent::ReasoningDelta { .. }
            | ProviderTurnEvent::RequestProjection { .. }
            | ProviderTurnEvent::ProviderMetadata { .. }
            | ProviderTurnEvent::RetryScheduled { .. } => {}
        }
    }
    CompactionPollStatus::Continue
}

async fn finish_provider_turn(
    state: &ServerState,
    provider_plugin_id: Option<String>,
    provider_turn_id: String,
) {
    let finish = FinishTurnRequest { provider_turn_id };
    let _ = invoke_model_provider_json_blocking::<_, bcode_model::AckResponse>(
        state,
        provider_plugin_id,
        OP_FINISH_TURN,
        finish,
    )
    .await;
}

fn compaction_transcript(
    history: &[bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
) -> Option<CompactionTranscript> {
    let history = compact_attach_history(history.to_vec());
    let latest_compaction =
        history
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, event)| match &event.kind {
                SessionEventKind::ContextCompacted { summary, .. } => Some((index, summary)),
                _ => None,
            });

    let previous_summary = latest_compaction.map(|(_, summary)| summary.clone());
    let start_index = latest_compaction.map_or(0, |(index, _)| index.saturating_add(1));
    let mut candidates = Vec::new();
    for event in &history[start_index..] {
        if let Some(text) = session_event_compaction_line(event, tool_output_context_chars) {
            candidates.push(CompactionLine {
                sequence: event.sequence,
                text,
                can_cut_after: session_event_is_safe_compaction_cut(event),
            });
        }
    }

    let lines = compaction_lines_to_summarize(candidates);
    let compacted_through_sequence = lines.last()?.sequence;
    let event_count = lines.len();

    Some(CompactionTranscript {
        previous_summary,
        lines,
        compacted_through_sequence,
        event_count,
    })
}

fn compaction_lines_to_summarize(mut candidates: Vec<CompactionLine>) -> Vec<CompactionLine> {
    if candidates.len() <= 1 {
        return candidates;
    }

    let mut kept_recent_chars = 0_usize;
    let mut keep_start = candidates.len();
    for (index, line) in candidates.iter().enumerate().rev() {
        kept_recent_chars = kept_recent_chars.saturating_add(line.text.chars().count());
        keep_start = index;
        if kept_recent_chars >= COMPACTION_KEEP_RECENT_CHARS {
            break;
        }
    }

    if keep_start == 0 {
        keep_start = candidates.len().saturating_sub(1);
    }
    candidates.truncate(keep_start);
    while candidates.last().is_some_and(|line| !line.can_cut_after) {
        candidates.pop();
    }
    candidates
}

const fn session_event_is_safe_compaction_cut(event: &bcode_session_models::SessionEvent) -> bool {
    matches!(
        &event.kind,
        SessionEventKind::UserMessage { .. }
            | SessionEventKind::AssistantReasoningDelta { .. }
            | SessionEventKind::AssistantReasoningMessage { .. }
            | SessionEventKind::AssistantMessage { .. }
            | SessionEventKind::ToolCallFinished { .. }
            | SessionEventKind::SystemMessage { .. }
    )
}

fn session_event_compaction_line(
    event: &bcode_session_models::SessionEvent,
    tool_output_context_chars: usize,
) -> Option<String> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => Some(format!(
            "#{} user:\n{}",
            event.sequence,
            truncate_text(text, COMPACTION_MAX_EVENT_CONTENT_CHARS)
        )),
        SessionEventKind::AssistantMessage { text } => Some(format!(
            "#{} assistant:\n{}",
            event.sequence,
            truncate_text(text, COMPACTION_MAX_EVENT_CONTENT_CHARS)
        )),
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
            ..
        } => Some(format!(
            "#{} assistant tool call {tool_call_id} ({tool_name}):\n{}",
            event.sequence,
            truncate_text(arguments_json, COMPACTION_MAX_EVENT_CONTENT_CHARS)
        )),
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
            output,
            ..
        } => Some(format!(
            "#{} tool result {tool_call_id} (error={is_error}):\n{}",
            event.sequence,
            project_tool_result_for_model_context(
                result,
                output.as_ref().map(trace_blob_read_path),
                tool_output_context_chars.min(COMPACTION_TOOL_RESULT_CHARS),
            )
        )),
        SessionEventKind::SystemMessage { text } => Some(format!(
            "#{} system:\n{}",
            event.sequence,
            truncate_text(text, COMPACTION_MAX_EVENT_CONTENT_CHARS)
        )),
        _ => None,
    }
}

async fn run_model_turn(
    state: &ServerState,
    permit: &mut SessionTurnPermit,
    trigger_event: &bcode_session_models::SessionEvent,
    client_id: ClientId,
    runtime_context: Option<ClientRuntimeContext>,
    command_context: &mut RuntimeCommandContext<'_>,
    phase: &Arc<Mutex<SessionRuntimePhase>>,
) -> ModelTurnCompletion {
    let session_id = permit.enter_turn();
    let turn_id = format!("{}-{}", session_id, trigger_event.sequence);
    let model_work_id = RuntimeWorkId::new(format!("model_{turn_id}"));
    let cancel_state = Arc::new(TurnCancelState::default());
    append_model_runtime_work_started_event(
        state,
        session_id,
        model_work_id.clone(),
        turn_id.clone(),
        Arc::clone(&cancel_state),
    )
    .await;
    begin_current_turn(
        command_context,
        client_id,
        turn_id.clone(),
        Arc::clone(&cancel_state),
    )
    .await;
    append_model_turn_started_event(state, session_id, turn_id.clone()).await;
    set_runtime_phase(phase, SessionRuntimePhase::ProviderActive).await;
    service_runtime_priority_commands(state, session_id, command_context).await;
    let completion = run_model_turn_inner(
        state,
        session_id,
        trigger_event,
        runtime_context,
        Arc::clone(&cancel_state),
        command_context,
    )
    .await;
    service_runtime_priority_commands(state, session_id, command_context).await;
    set_runtime_phase(phase, SessionRuntimePhase::FinishingTurn).await;
    finish_current_turn(command_context).await;
    append_model_turn_finished_event(
        state,
        session_id,
        turn_id,
        completion.outcome,
        completion.message.clone(),
    )
    .await;
    service_runtime_priority_commands(state, session_id, command_context).await;
    finish_registered_runtime_work(
        state,
        session_id,
        model_work_id,
        runtime_work_status_from_model_outcome(completion.outcome),
        completion.message.clone(),
    )
    .await;
    service_runtime_priority_commands(state, session_id, command_context).await;
    completion
}

#[allow(clippy::too_many_lines)]
async fn run_model_turn_inner(
    state: &ServerState,
    session_id: SessionId,
    trigger_event: &bcode_session_models::SessionEvent,
    runtime_context: Option<ClientRuntimeContext>,
    cancel_state: Arc<TurnCancelState>,
    command_context: &mut RuntimeCommandContext<'_>,
) -> ModelTurnCompletion {
    let selection =
        session_model_selection_with_runtime_context(state, session_id, runtime_context).await;

    if let Err(error) = maybe_auto_compact_session_context(state, session_id, &selection).await {
        let message = format!("auto compaction failed: {error}");
        append_system_event(state, session_id, message).await;
    }
    if !has_model_provider(state, selection.provider_plugin_id.as_deref()) {
        return ModelTurnCompletion::with_message(
            ModelTurnOutcome::ProviderUnavailable,
            "model provider unavailable",
        );
    }

    let provider_plugin_id = selection.provider_plugin_id.clone();
    let provider_retry_rules = provider_retry_rules(state, provider_plugin_id.as_deref()).await;
    let remote_catalog_retry_rules =
        remote_catalog_retry_rules(state, provider_plugin_id.as_deref()).await;
    let mut round = 0_u32;
    let mut recovery = ModelTurnRecoveryState::default();
    loop {
        if cancel_state.is_cancelled() {
            return ModelTurnCompletion::with_message(
                ModelTurnOutcome::Cancelled,
                "model turn cancelled",
            );
        }
        let request = match build_model_turn_request(
            state,
            session_id,
            trigger_event,
            round,
            provider_plugin_id.as_deref(),
            selection.model_id.as_deref(),
            recovery.retry_instruction,
            &selection,
        )
        .await
        {
            Ok(request) => request,
            Err(error) => {
                let message = format!("model request error: {error}");
                append_system_event(state, session_id, message.clone()).await;
                return ModelTurnCompletion::with_message(ModelTurnOutcome::Error, message);
            }
        };
        append_model_request_trace(
            state,
            session_id,
            &request,
            provider_plugin_id.as_deref(),
            round,
        )
        .await;
        let outcome = match run_model_turn_round(
            state,
            session_id,
            provider_plugin_id.as_deref(),
            &request,
            Arc::clone(&cancel_state),
            command_context,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(completion) => return completion,
        };
        if cancel_state.is_cancelled() {
            return ModelTurnCompletion::with_message(
                ModelTurnOutcome::Cancelled,
                "model turn cancelled",
            );
        }
        let retry_context = ProviderErrorRetryContext {
            trigger_event_sequence: trigger_event.sequence,
            turn_id: &request.turn_id,
            selection: &selection,
            provider_retry_rules: &provider_retry_rules,
            remote_catalog_retry_rules: &remote_catalog_retry_rules,
            cancel_state: cancel_state.as_ref(),
        };
        match maybe_retry_after_provider_error(
            state,
            session_id,
            &outcome,
            &mut recovery,
            retry_context,
        )
        .await
        {
            ModelTurnRetry::Continue => continue,
            ModelTurnRetry::Return(completion) => return completion,
            ModelTurnRetry::None => {}
        }
        if outcome.provider_error.is_none() {
            recovery.record_successful_provider_round();
        }
        if let Some(completion) = outcome.completion.clone() {
            append_deferred_provider_error_if_needed(state, session_id, &outcome, &selection).await;
            return completion;
        }
        match outcome.stop_reason {
            Some(bcode_model::StopReason::ToolCall) => {}
            Some(_) => return ModelTurnCompletion::completed(),
            None => {
                let message = "model provider polling ended without a terminal event".to_string();
                append_system_event(state, session_id, message.clone()).await;
                return ModelTurnCompletion::with_message(ModelTurnOutcome::Error, message);
            }
        }
        round = round.saturating_add(1);
        if state.max_tool_rounds.is_some_and(|max| round > max) {
            let max = state.max_tool_rounds.unwrap_or_default();
            let message = format!(
                "model tool-call round limit reached ({max}); remove [model].max_tool_rounds or set max_tool_rounds = 0 for unlimited rounds"
            );
            append_system_event(state, session_id, message.clone()).await;
            return ModelTurnCompletion::with_message(
                ModelTurnOutcome::ToolRoundLimitReached,
                message,
            );
        }
    }
}

async fn maybe_retry_after_provider_error(
    state: &ServerState,
    session_id: SessionId,
    outcome: &ModelPollOutcome,
    recovery: &mut ModelTurnRecoveryState,
    context: ProviderErrorRetryContext<'_>,
) -> ModelTurnRetry {
    let Some(error) = outcome.provider_error.as_ref() else {
        return ModelTurnRetry::None;
    };

    if should_retry_after_malformed_tool_arguments(
        error,
        recovery.retried_after_malformed_tool_arguments,
    ) {
        recovery.retried_after_malformed_tool_arguments = true;
        recovery.retry_instruction = Some(MALFORMED_TOOL_ARGUMENTS_RETRY_INSTRUCTION);
        append_provider_event_trace(
            state,
            session_id,
            context.turn_id,
            "recoverable_error_retry",
            Some(format!(
                "model emitted malformed tool arguments ({}: {}); retrying once",
                error.code, error.message
            )),
        )
        .await;
        return ModelTurnRetry::Continue;
    }

    if should_retry_after_context_overflow(state, error, recovery.retried_after_context_overflow) {
        recovery.retried_after_context_overflow = true;
        return match compact_session_after_context_overflow(
            state,
            session_id,
            context.selection,
            context.trigger_event_sequence,
            error,
        )
        .await
        {
            Ok(()) => ModelTurnRetry::Continue,
            Err(completion) => ModelTurnRetry::Return(completion),
        };
    }

    if let Some(policy) = matching_provider_retry_policy(
        state,
        error,
        context.selection,
        context.provider_retry_rules,
        context.remote_catalog_retry_rules,
    ) {
        let attempts = recovery
            .retry_attempts
            .entry(policy.id.clone())
            .or_default();
        if *attempts < policy.max_retries {
            *attempts = attempts.saturating_add(1);
            return retry_after_provider_error(
                state,
                session_id,
                context.turn_id,
                error,
                &policy,
                *attempts,
                context.cancel_state,
            )
            .await;
        }
    }

    ModelTurnRetry::None
}

#[derive(Debug, Clone)]
struct ProviderRetryPolicy {
    id: String,
    display_name: String,
    max_retries: u8,
    initial_delay_ms: u64,
    max_delay_ms: u64,
    use_provider_retry_hint: bool,
    kind: ProviderRetryPolicyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderRetryPolicyKind {
    Overload,
    Custom,
}

fn matching_provider_retry_policy(
    state: &ServerState,
    error: &bcode_model::ProviderError,
    selection: &SessionModelSelection,
    provider_rules: &[bcode_model::ProviderRetryRule],
    remote_catalog_rules: &[bcode_model::ProviderRetryRule],
) -> Option<ProviderRetryPolicy> {
    if !state.model_retry.enabled {
        return None;
    }
    if state.model_retry.overload_enabled && is_overloaded_provider_error(error) {
        return Some(ProviderRetryPolicy {
            id: "builtin.overload".to_string(),
            display_name: "overload".to_string(),
            max_retries: state.model_retry.max_overload_retries,
            initial_delay_ms: state.model_retry.overload_initial_delay_ms,
            max_delay_ms: state.model_retry.overload_max_delay_ms,
            use_provider_retry_hint: true,
            kind: ProviderRetryPolicyKind::Overload,
        });
    }

    let rules = effective_provider_retry_rules(
        provider_rules,
        remote_catalog_rules,
        &state.model_retry.rules,
    );
    rules
        .iter()
        .find(|rule| custom_retry_rule_matches(rule, error, selection))
        .map(ProviderRetryPolicy::from_rule)
}

fn effective_provider_retry_rules(
    provider_rules: &[bcode_model::ProviderRetryRule],
    remote_catalog_rules: &[bcode_model::ProviderRetryRule],
    user_rules: &[bcode_config::ModelRetryRuleConfig],
) -> Vec<bcode_model::ProviderRetryRule> {
    let mut rules = BTreeMap::new();
    for rule in provider_rules {
        rules.insert(rule.id.clone(), rule.clone());
    }
    for rule in remote_catalog_rules {
        rules
            .entry(rule.id.clone())
            .and_modify(|existing: &mut bcode_model::ProviderRetryRule| {
                existing.merge_override(rule.clone());
            })
            .or_insert_with(|| rule.clone());
    }
    for rule in user_rules {
        rules
            .entry(rule.id.clone())
            .and_modify(|existing: &mut bcode_model::ProviderRetryRule| {
                existing.merge_override(rule.clone());
            })
            .or_insert_with(|| rule.clone());
    }
    rules.into_values().collect()
}

fn custom_retry_rule_matches(
    rule: &bcode_model::ProviderRetryRule,
    error: &bcode_model::ProviderError,
    selection: &SessionModelSelection,
) -> bool {
    rule.enabled.unwrap_or(true)
        && rule
            .max_retries
            .unwrap_or(default_provider_retry_max_retries())
            > 0
        && rule.r#match.has_conditions()
        && retry_rule_scope_matches(rule, selection)
        && retry_rule_error_matches(&rule.r#match, error)
}

impl ProviderRetryPolicy {
    fn from_rule(rule: &bcode_model::ProviderRetryRule) -> Self {
        Self {
            id: format!("custom.{}", rule.id),
            display_name: rule.id.clone(),
            max_retries: rule
                .max_retries
                .unwrap_or(default_provider_retry_max_retries()),
            initial_delay_ms: rule
                .initial_delay_ms
                .unwrap_or(default_provider_retry_initial_delay_ms()),
            max_delay_ms: rule
                .max_delay_ms
                .unwrap_or(default_provider_retry_max_delay_ms()),
            use_provider_retry_hint: rule
                .use_provider_retry_hint
                .unwrap_or(default_provider_retry_use_provider_retry_hint()),
            kind: ProviderRetryPolicyKind::Custom,
        }
    }
}

const fn default_provider_retry_max_retries() -> u8 {
    3
}

const fn default_provider_retry_initial_delay_ms() -> u64 {
    1_000
}

const fn default_provider_retry_max_delay_ms() -> u64 {
    8_000
}

const fn default_provider_retry_use_provider_retry_hint() -> bool {
    true
}

fn retry_rule_scope_matches(
    rule: &bcode_model::ProviderRetryRule,
    selection: &SessionModelSelection,
) -> bool {
    optional_exact_matches(
        rule.provider_plugin_id.as_deref(),
        selection.provider_plugin_id.as_deref(),
    ) && optional_contains_matches(
        rule.provider_plugin_id_contains.as_deref(),
        selection.provider_plugin_id.as_deref(),
    ) && optional_exact_matches(rule.model_id.as_deref(), selection.model_id.as_deref())
        && optional_contains_matches(
            rule.model_id_contains.as_deref(),
            selection.model_id.as_deref(),
        )
}

fn retry_rule_error_matches(
    matcher: &bcode_model::ProviderRetryRuleMatch,
    error: &bcode_model::ProviderError,
) -> bool {
    matcher
        .category
        .is_none_or(|category| category == error.category)
        && optional_exact_matches(matcher.code.as_deref(), Some(error.code.as_str()))
        && optional_exact_matches(
            matcher.message_equals.as_deref(),
            Some(error.message.as_str()),
        )
        && optional_contains_matches(
            matcher.message_contains.as_deref(),
            Some(error.message.as_str()),
        )
        && optional_exact_matches(
            matcher.provider_message_equals.as_deref(),
            error.provider_message.as_deref(),
        )
        && optional_contains_matches(
            matcher.provider_message_contains.as_deref(),
            error.provider_message.as_deref(),
        )
}

fn optional_exact_matches(expected: Option<&str>, actual: Option<&str>) -> bool {
    expected.is_none_or(|expected| actual == Some(expected))
}

fn optional_contains_matches(expected: Option<&str>, actual: Option<&str>) -> bool {
    expected.is_none_or(|expected| actual.is_some_and(|actual| actual.contains(expected)))
}

fn is_overloaded_provider_error(error: &bcode_model::ProviderError) -> bool {
    error.category == bcode_model::ProviderErrorCategory::Overloaded
        || error.code == "server_is_overloaded"
}

async fn provider_retry_rules(
    state: &ServerState,
    provider_plugin_id: Option<&str>,
) -> Vec<bcode_model::ProviderRetryRule> {
    let Some(provider_plugin_id) = provider_plugin_id else {
        return Vec::new();
    };
    state
        .plugins
        .invoke_service_json::<(), bcode_model::ProviderCapabilities>(
            provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_CAPABILITIES,
            &(),
        )
        .await
        .map_or_else(|_| Vec::new(), |capabilities| capabilities.retry_rules)
}

async fn remote_catalog_retry_rules(
    state: &ServerState,
    provider_plugin_id: Option<&str>,
) -> Vec<bcode_model::ProviderRetryRule> {
    if !state.model_retry.remote_catalog_rules_enabled {
        return Vec::new();
    }
    let Some(provider_plugin_id) = provider_plugin_id else {
        return Vec::new();
    };
    let Ok(catalog) = bcode_model_catalog::ModelCatalog::load_bundled_with_remote_overlay().await
    else {
        return Vec::new();
    };
    catalog
        .document()
        .providers
        .values()
        .flat_map(remote_provider_retry_rules)
        .filter(|rule| remote_rule_scope_could_match(rule, provider_plugin_id))
        .collect()
}

fn remote_rule_scope_could_match(
    rule: &bcode_model::ProviderRetryRule,
    provider_plugin_id: &str,
) -> bool {
    optional_exact_matches(rule.provider_plugin_id.as_deref(), Some(provider_plugin_id))
        && optional_contains_matches(
            rule.provider_plugin_id_contains.as_deref(),
            Some(provider_plugin_id),
        )
}

fn remote_provider_retry_rules(
    provider: &bcode_model_catalog_models::ProviderCatalog,
) -> Vec<bcode_model::ProviderRetryRule> {
    provider
        .error_handling
        .recoverable_error_patterns
        .iter()
        .filter_map(remote_pattern_retry_rule)
        .collect()
}

fn remote_pattern_retry_rule(
    pattern: &bcode_model_catalog_models::RecoverableErrorPattern,
) -> Option<bcode_model::ProviderRetryRule> {
    if pattern.id.trim().is_empty() || !remote_pattern_has_scope(pattern) {
        return None;
    }
    Some(bcode_model::ProviderRetryRule {
        id: pattern.id.clone(),
        enabled: Some(pattern.enabled_by_default),
        provider_plugin_id: pattern.scope.provider_plugin_id.clone(),
        provider_plugin_id_contains: pattern.scope.provider_plugin_id_contains.clone(),
        model_id: pattern.scope.model_id.clone(),
        model_id_contains: pattern.scope.model_id_contains.clone(),
        r#match: remote_pattern_match(&pattern.r#match)?,
        ..bcode_model::ProviderRetryRule::default()
    })
}

const fn remote_pattern_has_scope(
    pattern: &bcode_model_catalog_models::RecoverableErrorPattern,
) -> bool {
    pattern.scope.provider_plugin_id.is_some()
        || pattern.scope.provider_plugin_id_contains.is_some()
        || pattern.scope.model_id.is_some()
        || pattern.scope.model_id_contains.is_some()
}

fn remote_pattern_match(
    matcher: &bcode_model_catalog_models::RecoverableErrorPatternMatch,
) -> Option<bcode_model::ProviderRetryRuleMatch> {
    let rule_match = bcode_model::ProviderRetryRuleMatch {
        category: matcher
            .category
            .as_deref()
            .and_then(provider_error_category),
        code: matcher.code.clone(),
        message_equals: matcher.message_equals.clone(),
        message_contains: matcher.message_contains.clone(),
        provider_message_equals: matcher.provider_message_equals.clone(),
        provider_message_contains: matcher.provider_message_contains.clone(),
    };
    rule_match.has_conditions().then_some(rule_match)
}

fn provider_error_category(category: &str) -> Option<bcode_model::ProviderErrorCategory> {
    match category {
        "config" => Some(bcode_model::ProviderErrorCategory::Config),
        "auth" => Some(bcode_model::ProviderErrorCategory::Auth),
        "rate_limit" => Some(bcode_model::ProviderErrorCategory::RateLimit),
        "network" => Some(bcode_model::ProviderErrorCategory::Network),
        "timeout" => Some(bcode_model::ProviderErrorCategory::Timeout),
        "model_not_found" => Some(bcode_model::ProviderErrorCategory::ModelNotFound),
        "context_length" => Some(bcode_model::ProviderErrorCategory::ContextLength),
        "invalid_request" => Some(bcode_model::ProviderErrorCategory::InvalidRequest),
        "unsupported_feature" => Some(bcode_model::ProviderErrorCategory::UnsupportedFeature),
        "provider_internal" => Some(bcode_model::ProviderErrorCategory::ProviderInternal),
        "overloaded" => Some(bcode_model::ProviderErrorCategory::Overloaded),
        "cancelled" => Some(bcode_model::ProviderErrorCategory::Cancelled),
        _ => None,
    }
}

async fn retry_after_provider_error(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    error: &bcode_model::ProviderError,
    policy: &ProviderRetryPolicy,
    attempt: u8,
    cancel_state: &TurnCancelState,
) -> ModelTurnRetry {
    let delay = provider_retry_delay(policy, error, attempt);
    append_provider_event_trace(
        state,
        session_id,
        turn_id,
        "recoverable_error_retry",
        Some(format!(
            "model provider error matched retry policy {} ({}: {}); retrying attempt {}/{} in {}ms",
            policy.id,
            error.code,
            error.message,
            attempt,
            policy.max_retries,
            delay.as_millis()
        )),
    )
    .await;
    append_system_event(
        state,
        session_id,
        provider_retry_message(policy, delay, attempt),
    )
    .await;

    tokio::select! {
        () = tokio::time::sleep(delay) => ModelTurnRetry::Continue,
        () = cancel_state.cancelled() => ModelTurnRetry::Return(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Cancelled,
            "model turn cancelled",
        )),
    }
}

fn provider_retry_message(policy: &ProviderRetryPolicy, delay: Duration, attempt: u8) -> String {
    if policy.kind == ProviderRetryPolicyKind::Overload {
        return format!(
            "Model provider is overloaded. Retrying automatically in {} (attempt {}/{}).",
            format_retry_delay(delay),
            attempt,
            policy.max_retries
        );
    }
    format!(
        "Model provider error matched retry rule {:?}. Retrying automatically in {} (attempt {}/{}).",
        policy.display_name,
        format_retry_delay(delay),
        attempt,
        policy.max_retries
    )
}

fn provider_retry_delay(
    policy: &ProviderRetryPolicy,
    error: &bcode_model::ProviderError,
    attempt: u8,
) -> Duration {
    let max_delay = Duration::from_millis(policy.max_delay_ms);
    if policy.use_provider_retry_hint
        && let Some(retry) = error.retry.as_deref()
    {
        if let Some(retry_after_ms) = retry.retry_after_ms {
            return Duration::from_millis(retry_after_ms).min(max_delay);
        }
        if let Some(retry_at_unix) = retry.retry_at_unix {
            let now = unix_timestamp();
            if retry_at_unix > now {
                return Duration::from_secs(retry_at_unix.saturating_sub(now)).min(max_delay);
            }
        }
    }

    let multiplier = 1_u64
        .checked_shl(u32::from(attempt.saturating_sub(1)))
        .unwrap_or(u64::MAX);
    Duration::from_millis(policy.initial_delay_ms.saturating_mul(multiplier)).min(max_delay)
}

#[cfg(test)]
fn overload_retry_delay(
    config: &bcode_config::ModelRetryConfig,
    error: &bcode_model::ProviderError,
    attempt: u8,
) -> Duration {
    let policy = ProviderRetryPolicy {
        id: "builtin.overload".to_string(),
        display_name: "overload".to_string(),
        max_retries: config.max_overload_retries,
        initial_delay_ms: config.overload_initial_delay_ms,
        max_delay_ms: config.overload_max_delay_ms,
        use_provider_retry_hint: true,
        kind: ProviderRetryPolicyKind::Overload,
    };
    provider_retry_delay(&policy, error, attempt)
}

#[cfg(test)]
fn should_retry_after_overload_error(
    state: &ServerState,
    error: &bcode_model::ProviderError,
    attempts: u8,
) -> bool {
    state.model_retry.enabled
        && state.model_retry.overload_enabled
        && attempts < state.model_retry.max_overload_retries
        && is_overloaded_provider_error(error)
}

fn format_retry_delay(delay: Duration) -> String {
    if delay.as_millis() < 1_000 {
        return format!("{}ms", delay.as_millis());
    }
    if delay.as_millis().is_multiple_of(1_000) {
        return format!("{}s", delay.as_secs());
    }
    format!("{:.1}s", delay.as_secs_f64())
}

fn should_retry_after_context_overflow(
    state: &ServerState,
    error: &bcode_model::ProviderError,
    already_retried: bool,
) -> bool {
    !already_retried
        && state.auto_compaction.mode.is_overflow_recovery_enabled()
        && is_context_length_provider_error(error)
}

fn should_retry_after_malformed_tool_arguments(
    error: &bcode_model::ProviderError,
    already_retried: bool,
) -> bool {
    !already_retried && is_tool_arguments_decode_provider_error(error)
}

async fn compact_session_after_context_overflow(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    first_kept_sequence: u64,
    error: &bcode_model::ProviderError,
) -> Result<(), ModelTurnCompletion> {
    append_context_compaction_trace(
        state,
        session_id,
        "overflow",
        0,
        false,
        Some(format!(
            "provider reported context overflow ({}: {})",
            error.code, error.message
        )),
    )
    .await;
    match compact_session_context_before_sequence(state, session_id, selection, first_kept_sequence)
        .await
    {
        Ok(message) => {
            append_context_compaction_trace(
                state,
                session_id,
                "overflow",
                0,
                true,
                Some(format!("{message}; retrying model turn")),
            )
            .await;
            Ok(())
        }
        Err(error) => {
            let message = format!("context overflow compaction failed: {error}");
            append_system_event(state, session_id, message.clone()).await;
            Err(ModelTurnCompletion::with_message(
                ModelTurnOutcome::Error,
                message,
            ))
        }
    }
}

async fn append_deferred_provider_error_if_needed(
    state: &ServerState,
    session_id: SessionId,
    outcome: &ModelPollOutcome,
    selection: &SessionModelSelection,
) {
    if let Some(error) = outcome.provider_error.as_ref()
        && should_defer_visible_provider_error(state, error, Some(selection))
    {
        append_system_event(state, session_id, provider_error_message(error)).await;
    }
}

fn should_defer_visible_provider_error(
    state: &ServerState,
    error: &bcode_model::ProviderError,
    selection: Option<&SessionModelSelection>,
) -> bool {
    is_context_length_provider_error(error)
        || is_tool_arguments_decode_provider_error(error)
        || is_overloaded_provider_error(error)
        || selection.is_some_and(|selection| {
            matching_provider_retry_policy(state, error, selection, &[], &[]).is_some()
        })
}

fn is_context_length_provider_error(error: &bcode_model::ProviderError) -> bool {
    error.category == bcode_model::ProviderErrorCategory::ContextLength
}

fn is_tool_arguments_decode_provider_error(error: &bcode_model::ProviderError) -> bool {
    error.code == TOOL_ARGUMENTS_DECODE_FAILED_CODE
}

fn provider_error_message(error: &bcode_model::ProviderError) -> String {
    format!("model error {}: {}", error.code, error.message)
}

async fn active_plugin_scope_for_session(
    state: &ServerState,
    session_id: SessionId,
) -> PluginInvocationScope {
    state
        .session_current_turn(session_id)
        .await
        .map_or_else(PluginInvocationScope::default, |turn| {
            turn.plugin_scope_for_model(session_id)
        })
}

async fn active_plugin_scope_for_tool_call(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: &str,
) -> PluginInvocationScope {
    state
        .session_current_turn(session_id)
        .await
        .map_or_else(PluginInvocationScope::default, |turn| {
            turn.plugin_scope_for_tool_call(session_id, tool_call_id)
        })
}

#[allow(clippy::too_many_lines)]
async fn run_model_turn_round(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    request: &ModelTurnRequest,
    cancel_state: Arc<TurnCancelState>,
    command_context: &mut RuntimeCommandContext<'_>,
) -> Result<ModelPollOutcome, ModelTurnCompletion> {
    let round_start = Instant::now();
    let provider_label = provider_plugin_id.unwrap_or("<auto>").to_string();
    if cancel_state.is_cancelled() {
        return Err(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Cancelled,
            "model turn cancelled",
        ));
    }
    service_runtime_priority_commands(state, session_id, command_context).await;
    let start_timer = state.metrics.timer();
    let scope = active_plugin_scope_for_session(state, session_id).await;
    let start = wait_for_provider_call(
        state,
        session_id,
        command_context,
        cancel_state.as_ref(),
        Box::pin(invoke_model_provider_json_blocking_scoped::<
            _,
            StartTurnResponse,
        >(
            state,
            provider_plugin_id.map(ToString::to_string),
            OP_START_TURN,
            request.clone(),
            scope,
        )),
    )
    .await;
    state.metrics.record_histogram(
        "model.provider.start_turn_duration_ms",
        start_timer.elapsed_ms(),
    );
    let start = match start {
        ProviderCallWait::Completed(Ok(start)) => start,
        ProviderCallWait::Completed(Err(error)) => {
            let message = format!("model provider error: {error}");
            append_trace_event(
                state,
                session_id,
                Some(request.turn_id.clone()),
                SessionTracePhase::ModelProviderRoundFinished,
                SessionTracePayload::ProviderRound {
                    provider_turn_id: None,
                    provider: provider_label,
                    round: model_round_from_turn_id(&request.turn_id),
                    stop_reason: None,
                    duration_ms: Some(elapsed_ms(round_start)),
                    error: Some(message.clone()),
                },
            )
            .await;
            append_system_event(state, session_id, message.clone()).await;
            return Err(ModelTurnCompletion::with_message(
                ModelTurnOutcome::Error,
                message,
            ));
        }
        ProviderCallWait::Cancelled => {
            return Err(ModelTurnCompletion::with_message(
                ModelTurnOutcome::Cancelled,
                "model turn cancelled",
            ));
        }
    };

    let active_model_turn = ActiveModelTurn {
        provider_plugin_id: provider_plugin_id.map(ToString::to_string),
        provider_turn_id: start.provider_turn_id.clone(),
        reuse_key: request.conversation_reuse.key.clone(),
        request_message_count: request.messages.len(),
    };
    begin_provider_round(command_context, active_model_turn).await;

    append_trace_event(
        state,
        session_id,
        Some(request.turn_id.clone()),
        SessionTracePhase::ModelProviderRoundStarted,
        SessionTracePayload::ProviderRound {
            provider_turn_id: Some(start.provider_turn_id.clone()),
            provider: provider_label.clone(),
            round: model_round_from_turn_id(&request.turn_id),
            stop_reason: None,
            duration_ms: None,
            error: None,
        },
    )
    .await;

    let (assistant_text, mut outcome) = poll_model_turn_events(
        state,
        session_id,
        provider_plugin_id,
        &start.provider_turn_id,
        &request.turn_id,
        Arc::clone(&cancel_state),
        command_context,
    )
    .await;

    service_runtime_priority_commands(state, session_id, command_context).await;
    ensure_terminal_poll_outcome(state, session_id, &mut outcome).await;

    if !assistant_text.is_empty() {
        append_assistant_message_event(state, session_id, assistant_text).await;
    }

    service_runtime_priority_commands(state, session_id, command_context).await;
    let active_turn = finish_provider_round(command_context).await;
    if cancel_state.is_cancelled() && outcome.completion.is_none() {
        outcome.stop_reason = Some(bcode_model::StopReason::Cancelled);
        outcome.completion = Some(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Cancelled,
            "model turn cancelled",
        ));
    }
    let finish = FinishTurnRequest {
        provider_turn_id: start.provider_turn_id,
    };
    append_model_provider_round_finished_trace(
        state,
        session_id,
        request,
        finish.provider_turn_id.clone(),
        provider_label,
        round_start,
        &outcome,
    )
    .await;
    let finish_result = wait_for_provider_call(
        state,
        session_id,
        command_context,
        cancel_state.as_ref(),
        Box::pin(invoke_model_provider_json_blocking::<
            _,
            bcode_model::AckResponse,
        >(
            state,
            active_turn.and_then(|turn| turn.provider_plugin_id),
            OP_FINISH_TURN,
            finish,
        )),
    )
    .await;
    if let ProviderCallWait::Completed(Err(error)) = finish_result {
        append_system_event(
            state,
            session_id,
            format!("model provider finish turn failed: {error}"),
        )
        .await;
    }
    Ok(outcome)
}

async fn ensure_terminal_poll_outcome(
    state: &ServerState,
    session_id: SessionId,
    outcome: &mut ModelPollOutcome,
) {
    if outcome.stop_reason.is_some() || outcome.completion.is_some() {
        return;
    }
    if state
        .session_current_turn(session_id)
        .await
        .is_some_and(|turn| turn.cancel_state.is_cancelled())
    {
        outcome.stop_reason = Some(bcode_model::StopReason::Cancelled);
        outcome.completion = Some(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Cancelled,
            "model turn cancelled",
        ));
        return;
    }
    let message = "model provider polling ended without a terminal event".to_string();
    append_system_event(state, session_id, message.clone()).await;
    outcome.completion = Some(ModelTurnCompletion::with_message(
        ModelTurnOutcome::Error,
        message,
    ));
}

async fn append_model_provider_round_finished_trace(
    state: &ServerState,
    session_id: SessionId,
    request: &ModelTurnRequest,
    provider_turn_id: String,
    provider: String,
    round_start: Instant,
    outcome: &ModelPollOutcome,
) {
    append_trace_event(
        state,
        session_id,
        Some(request.turn_id.clone()),
        SessionTracePhase::ModelProviderRoundFinished,
        SessionTracePayload::ProviderRound {
            provider_turn_id: Some(provider_turn_id),
            provider,
            round: model_round_from_turn_id(&request.turn_id),
            stop_reason: outcome.stop_reason.map(|reason| format!("{reason:?}")),
            duration_ms: Some(elapsed_ms(round_start)),
            error: outcome
                .completion
                .as_ref()
                .and_then(|completion| completion.message.clone()),
        },
    )
    .await;
}

#[allow(clippy::too_many_lines)]
async fn poll_model_turn_events(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    provider_turn_id: &str,
    turn_id: &str,
    cancel_state: Arc<TurnCancelState>,
    command_context: &mut RuntimeCommandContext<'_>,
) -> (String, ModelPollOutcome) {
    let mut stream = ModelStreamAccumulator::new(session_id, turn_id);
    let mut outcome = ModelPollOutcome::default();
    let mut stream_progress = ModelStreamProgress::default();
    let mut idle_for = Duration::ZERO;
    let mut no_progress_warned = false;
    loop {
        service_runtime_priority_commands(state, session_id, command_context).await;
        if cancel_state.is_cancelled() {
            outcome.stop_reason = Some(bcode_model::StopReason::Cancelled);
            outcome.completion = Some(ModelTurnCompletion::with_message(
                ModelTurnOutcome::Cancelled,
                "model turn cancelled",
            ));
            break;
        }
        let poll = PollTurnEventsRequest {
            provider_turn_id: provider_turn_id.to_string(),
        };
        let poll_timer = state.metrics.timer();
        let response = wait_for_provider_call(
            state,
            session_id,
            command_context,
            cancel_state.as_ref(),
            Box::pin(poll_model_turn(
                state,
                session_id,
                provider_plugin_id,
                &poll,
            )),
        )
        .await;
        state.metrics.record_histogram(
            "model.provider.poll_turn_events_duration_ms",
            poll_timer.elapsed_ms(),
        );
        let response = match response {
            ProviderCallWait::Completed(Ok(response)) => response,
            ProviderCallWait::Completed(Err(error)) => {
                let message = format!("model provider error: {error}");
                append_system_event(state, session_id, message.clone()).await;
                outcome.completion = Some(ModelTurnCompletion::with_message(
                    ModelTurnOutcome::Error,
                    message,
                ));
                break;
            }
            ProviderCallWait::Cancelled => {
                outcome.stop_reason = Some(bcode_model::StopReason::Cancelled);
                outcome.completion = Some(ModelTurnCompletion::with_message(
                    ModelTurnOutcome::Cancelled,
                    "model turn cancelled",
                ));
                break;
            }
        };
        if response.events.is_empty() {
            state
                .metrics
                .increment_counter("model.provider.poll_empty_total");
            let Some(next_idle_for) = wait_for_model_progress_or_timeout(
                state,
                session_id,
                idle_for,
                &mut no_progress_warned,
                cancel_state.as_ref(),
                stream_progress.tool_progress_snapshot(),
                &mut outcome,
                command_context,
            )
            .await
            else {
                break;
            };
            idle_for = next_idle_for;
            continue;
        }
        let saw_progress = model_events_include_progress(&response.events);
        state.metrics.record_histogram(
            "model.provider.poll_events_per_response",
            response.events.len() as u64,
        );
        for event in response.events {
            handle_provider_turn_event(
                state,
                session_id,
                turn_id,
                event,
                &mut stream,
                &mut outcome,
                &mut stream_progress,
                command_context,
            )
            .await;
        }
        if outcome.stop_reason.is_some() || outcome.completion.is_some() {
            break;
        }
        if saw_progress {
            idle_for = Duration::ZERO;
            no_progress_warned = false;
        } else {
            let Some(next_idle_for) = wait_for_model_progress_or_timeout(
                state,
                session_id,
                idle_for,
                &mut no_progress_warned,
                cancel_state.as_ref(),
                stream_progress.tool_progress_snapshot(),
                &mut outcome,
                command_context,
            )
            .await
            else {
                break;
            };
            idle_for = next_idle_for;
        }
    }
    stream.flush(state).await;
    (stream.finish(), outcome)
}

fn model_events_include_progress(events: &[ProviderTurnEvent]) -> bool {
    events.iter().any(model_event_is_progress)
}

const fn model_event_is_progress(event: &ProviderTurnEvent) -> bool {
    match event {
        ProviderTurnEvent::TextDelta { text } | ProviderTurnEvent::ReasoningDelta { text } => {
            !text.is_empty()
        }
        ProviderTurnEvent::ToolCallStarted { .. } | ProviderTurnEvent::ToolCallFinished { .. } => {
            true
        }
        ProviderTurnEvent::ToolCallDelta { delta, .. } => !delta.is_empty(),
        ProviderTurnEvent::RequestProjection { .. }
        | ProviderTurnEvent::TurnStarted
        | ProviderTurnEvent::Usage { .. }
        | ProviderTurnEvent::Warning { .. }
        | ProviderTurnEvent::RetryScheduled { .. }
        | ProviderTurnEvent::ProviderMetadata { .. }
        | ProviderTurnEvent::Error { .. }
        | ProviderTurnEvent::Cancelled
        | ProviderTurnEvent::TurnFinished { .. } => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_model_progress_or_timeout(
    state: &ServerState,
    session_id: SessionId,
    idle_for: Duration,
    warned: &mut bool,
    cancel_state: &TurnCancelState,
    active_tool_call: Option<ProviderToolCallProgress>,
    outcome: &mut ModelPollOutcome,
    command_context: &mut RuntimeCommandContext<'_>,
) -> Option<Duration> {
    let idle_for = idle_for.saturating_add(MODEL_POLL_INTERVAL);
    let warning_after = Duration::from_secs(state.model_streaming.no_progress_warning_secs);
    let timeout_after = Duration::from_secs(state.model_streaming.no_progress_timeout_secs);
    if !*warned && idle_for >= warning_after {
        publish_provider_stream_progress_live(
            state,
            session_id,
            "model-stream",
            ProviderStreamEvent::NoProgressWarning {
                idle_seconds: idle_for.as_secs(),
                active_tool_call: active_tool_call.clone(),
            },
        )
        .await;
        *warned = true;
    }
    if idle_for > timeout_after {
        let detail = active_tool_call
            .as_ref()
            .map_or_else(String::new, |progress| {
                format!(
                    " while assembling {} arguments · {} received",
                    progress.tool_name,
                    format_bytes(progress.argument_bytes)
                )
            });
        let message = format!(
            "model provider made no progress for {} seconds before timeout{detail}",
            timeout_after.as_secs()
        );
        append_system_event(state, session_id, message.clone()).await;
        outcome.completion = Some(ModelTurnCompletion::with_message(
            ModelTurnOutcome::IdleTimeout,
            message,
        ));
        return None;
    }
    tokio::select! {
        () = tokio::time::sleep(MODEL_POLL_INTERVAL) => Some(idle_for),
        cancel_command = command_context.cancel_commands.recv() => {
            if let Some(command) = cancel_command {
                let cancelled = process_cancel_turn_command(
                    state,
                    session_id,
                    command_context.followup_commands,
                    command_context.queued_followups,
                    command.clear_queue,
                    command.requested_by,
                )
                .await;
                let _sent = command.response.send(cancelled);
            }
            Some(idle_for)
        }
        steering_command = command_context.steering_commands.recv() => {
            if let Some(command) = steering_command {
                process_steering_message_command(
                    state,
                    session_id,
                    command.client_id,
                    command.text,
                    command.completion,
                )
                .await;
            }
            Some(idle_for)
        }
        () = cancel_state.cancelled() => {
            outcome.stop_reason = Some(bcode_model::StopReason::Cancelled);
            outcome.completion = Some(ModelTurnCompletion::with_message(
                ModelTurnOutcome::Cancelled,
                "model turn cancelled",
            ));
            None
        }
    }
}

async fn poll_model_turn(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    poll: &PollTurnEventsRequest,
) -> Result<PollTurnEventsResponse, String> {
    let scope = active_plugin_scope_for_session(state, session_id).await;
    invoke_model_provider_json_blocking_scoped::<_, PollTurnEventsResponse>(
        state,
        provider_plugin_id.map(ToString::to_string),
        OP_POLL_TURN_EVENTS,
        poll.clone(),
        scope,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_provider_turn_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    event: ProviderTurnEvent,
    stream: &mut ModelStreamAccumulator,
    outcome: &mut ModelPollOutcome,
    stream_progress: &mut ModelStreamProgress,
    command_context: &mut RuntimeCommandContext<'_>,
) {
    if state
        .session_current_turn(session_id)
        .await
        .is_some_and(|turn| turn.cancel_state.is_cancelled())
    {
        outcome.stop_reason = Some(bcode_model::StopReason::Cancelled);
        outcome.completion = Some(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Cancelled,
            "model turn cancelled",
        ));
        return;
    }
    match event {
        ProviderTurnEvent::TextDelta { text } => {
            append_provider_event_trace(state, session_id, turn_id, "text_delta", None).await;
            state.metrics.record_histogram(
                "model.provider.text_delta_chars",
                text.chars().count() as u64,
            );
            stream.push_text(&text);
            stream.flush_if_ready(state).await;
        }
        ProviderTurnEvent::Error { error } => {
            handle_provider_error_event(state, session_id, turn_id, error, outcome).await;
        }
        ProviderTurnEvent::TurnFinished { stop_reason } => {
            handle_provider_turn_finished_event(state, session_id, turn_id, stop_reason, outcome)
                .await;
        }
        ProviderTurnEvent::Cancelled => {
            handle_provider_cancelled_event(state, session_id, turn_id, outcome).await;
        }
        ProviderTurnEvent::ToolCallFinished { call } => {
            let call_id = call.id.clone();
            stream_progress.record_completed_tool_call(&call);
            handle_provider_tool_call_finished_event(
                state,
                session_id,
                turn_id,
                call,
                stream,
                command_context,
            )
            .await;
            stream_progress.finish_tool_call(&call_id);
        }
        ProviderTurnEvent::Warning { message } => {
            append_provider_event_trace(
                state,
                session_id,
                turn_id,
                "warning",
                Some(message.clone()),
            )
            .await;
            append_system_event(state, session_id, format!("model warning: {message}")).await;
        }
        ProviderTurnEvent::RetryScheduled {
            message,
            retry_at_unix,
        } => {
            append_provider_event_trace(
                state,
                session_id,
                turn_id,
                "retry_scheduled",
                Some(message.clone()),
            )
            .await;
            publish_provider_stream_progress_live(
                state,
                session_id,
                turn_id,
                ProviderStreamEvent::RetryScheduled {
                    message,
                    retry_at_unix,
                },
            )
            .await;
        }
        ProviderTurnEvent::Usage { usage } => {
            append_provider_event_trace(state, session_id, turn_id, "usage", None).await;
            update_provider_usage_state(state, session_id, &usage).await;
            append_model_usage_event(state, session_id, turn_id.to_string(), usage).await;
        }
        ProviderTurnEvent::RequestProjection { projection } => {
            handle_provider_request_projection_event(state, session_id, turn_id, projection).await;
        }
        ProviderTurnEvent::ProviderMetadata { key, value } => {
            handle_provider_metadata_event(state, session_id, turn_id, key, value).await;
        }
        ProviderTurnEvent::TurnStarted => {
            publish_provider_stream_progress_live(
                state,
                session_id,
                turn_id,
                ProviderStreamEvent::TurnStarted,
            )
            .await;
        }
        ProviderTurnEvent::ToolCallStarted { call_id, name } => {
            let preview_metadata = find_tool_provider(state, &name)
                .await
                .ok()
                .flatten()
                .and_then(|(_, definition)| definition.ui.live_argument_preview);
            stream_progress.start_tool_call(call_id.clone(), name.clone(), preview_metadata);
            publish_provider_stream_progress_live(
                state,
                session_id,
                turn_id,
                ProviderStreamEvent::ToolCallStarted {
                    tool_call_id: call_id,
                    tool_name: name,
                },
            )
            .await;
        }
        ProviderTurnEvent::ReasoningDelta { text } => {
            stream.push_reasoning(&text);
            stream.flush_if_ready(state).await;
            append_provider_event_trace(state, session_id, turn_id, "reasoning_delta", None).await;
        }
        ProviderTurnEvent::ToolCallDelta { call_id, delta } => {
            stream_progress.record_tool_call_delta(&call_id, &delta);
            if let Some(progress) = stream_progress.tool_progress_snapshot()
                && let Some(preview) = stream_progress.take_tool_argument_preview()
            {
                publish_tool_argument_preview_live(
                    state,
                    session_id,
                    turn_id,
                    progress.tool_call_id,
                    progress.tool_name,
                    progress.argument_bytes,
                    live_tool_argument_preview_with_bytes(preview, progress.argument_bytes),
                )
                .await;
            }
            if let Some(progress) = stream_progress.take_tool_progress_event() {
                publish_provider_stream_progress_live(
                    state,
                    session_id,
                    turn_id,
                    ProviderStreamEvent::ToolCallProgress {
                        tool_call_id: progress.tool_call_id,
                        tool_name: progress.tool_name,
                        argument_bytes: progress.argument_bytes,
                    },
                )
                .await;
            }
        }
    }
}

async fn handle_provider_request_projection_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    projection: bcode_model::ProviderRequestProjection,
) {
    append_provider_event_trace(
        state,
        session_id,
        turn_id,
        "request_projection",
        Some(provider_request_projection_detail(&projection)),
    )
    .await;
}

fn provider_request_projection_detail(
    projection: &bcode_model::ProviderRequestProjection,
) -> String {
    let mut parts = Vec::new();
    if let Some(provider) = &projection.provider {
        parts.push(format!("provider={provider}"));
    }
    if let Some(api_shape) = &projection.api_shape {
        parts.push(format!("api_shape={api_shape}"));
    }
    if let Some(sent) = projection.sent_message_count {
        parts.push(format!("sent_messages={sent}"));
    }
    if let Some(original) = projection.original_message_count {
        parts.push(format!("original_messages={original}"));
    }
    if let Some(omitted) = projection.omitted_message_count {
        parts.push(format!("omitted_messages={omitted}"));
    }
    if let Some(input_items) = projection.input_item_count {
        parts.push(format!("input_items={input_items}"));
    }
    if let Some(emitted) = projection.emitted_cache_point_count {
        parts.push(format!("emitted_cache_points={emitted}"));
    }
    if let Some(dropped) = projection.dropped_cache_point_count {
        parts.push(format!("dropped_cache_points={dropped}"));
    }
    if projection.used_previous_response_id {
        parts.push("used_previous_response_id=true".to_string());
    }
    if let Some(detail) = &projection.detail {
        parts.push(format!("detail={detail}"));
    }
    parts.join(",")
}

async fn handle_provider_error_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    error: bcode_model::ProviderError,
    outcome: &mut ModelPollOutcome,
) {
    let message = provider_error_message(&error);
    let selection = session_model_selection_with_runtime_context(state, session_id, None).await;
    let defer_visible_message =
        should_defer_visible_provider_error(state, &error, Some(&selection));
    append_provider_event_trace(state, session_id, turn_id, "error", Some(message.clone())).await;
    if !defer_visible_message {
        append_system_event(state, session_id, message.clone()).await;
    }
    outcome.stop_reason = Some(bcode_model::StopReason::Error);
    outcome.provider_error = Some(error);
    outcome.completion = Some(ModelTurnCompletion::with_message(
        ModelTurnOutcome::Error,
        message,
    ));
}

async fn handle_provider_turn_finished_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    stop_reason: bcode_model::StopReason,
    outcome: &mut ModelPollOutcome,
) {
    append_provider_event_trace(
        state,
        session_id,
        turn_id,
        "turn_finished",
        Some(format!("{stop_reason:?}")),
    )
    .await;
    outcome.stop_reason = Some(stop_reason);
    if stop_reason == bcode_model::StopReason::Cancelled {
        outcome.completion = Some(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Cancelled,
            "model turn cancelled",
        ));
    } else if stop_reason == bcode_model::StopReason::Error && outcome.completion.is_none() {
        outcome.completion = Some(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Error,
            "model turn ended with error",
        ));
    }
}

async fn handle_provider_cancelled_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    outcome: &mut ModelPollOutcome,
) {
    let message = "model turn cancelled".to_string();
    append_provider_event_trace(
        state,
        session_id,
        turn_id,
        "cancelled",
        Some(message.clone()),
    )
    .await;
    append_system_event(state, session_id, message.clone()).await;
    outcome.stop_reason = Some(bcode_model::StopReason::Cancelled);
    outcome.completion = Some(ModelTurnCompletion::with_message(
        ModelTurnOutcome::Cancelled,
        message,
    ));
}

async fn handle_provider_tool_call_finished_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    call: bcode_model::ToolCall,
    stream: &mut ModelStreamAccumulator,
    command_context: &mut RuntimeCommandContext<'_>,
) {
    publish_provider_stream_progress_live(
        state,
        session_id,
        turn_id,
        ProviderStreamEvent::ToolCallProgress {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            argument_bytes: serialized_tool_argument_len(&call.arguments),
        },
    )
    .await;
    let preview_fields = StreamingJsonStringFields::from_json_value(&call.arguments);
    let preview_metadata = find_tool_provider(state, &call.name)
        .await
        .ok()
        .flatten()
        .and_then(|(_, definition)| definition.ui.live_argument_preview);
    if let Some(preview) = preview_metadata
        .as_ref()
        .and_then(|metadata| live_tool_argument_preview_from_fields(metadata, &preview_fields))
    {
        publish_tool_argument_preview_live(
            state,
            session_id,
            turn_id,
            call.id.clone(),
            call.name.clone(),
            serialized_tool_argument_len(&call.arguments),
            live_tool_argument_preview_with_bytes(
                preview,
                serialized_tool_argument_len(&call.arguments),
            ),
        )
        .await;
    }
    publish_provider_stream_progress_live(
        state,
        session_id,
        turn_id,
        ProviderStreamEvent::ToolCallFinished {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
        },
    )
    .await;
    stream.flush(state).await;
    let assistant_text = stream.take_assistant_text();
    if !assistant_text.is_empty() {
        append_assistant_message_event(state, session_id, assistant_text).await;
    }
    let Some(cancel_state) = active_turn_cancel_state(state, session_id).await else {
        return;
    };
    if cancel_state.is_cancelled() {
        return;
    }
    execute_model_tool(
        state,
        session_id,
        call,
        Arc::clone(&cancel_state),
        command_context,
    )
    .await;
}

async fn active_turn_cancel_state(
    state: &ServerState,
    session_id: SessionId,
) -> Option<Arc<TurnCancelState>> {
    state
        .session_current_turn(session_id)
        .await
        .map(|turn| Arc::clone(&turn.cancel_state))
}

async fn handle_provider_metadata_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    key: String,
    value: String,
) {
    append_provider_event_trace(
        state,
        session_id,
        turn_id,
        "provider_metadata",
        Some(key.clone()),
    )
    .await;
    update_provider_metadata_state(state, session_id, &key, value).await;
}

async fn publish_provider_stream_progress_live(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    event: ProviderStreamEvent,
) {
    let _ = state
        .sessions
        .publish_live_event(
            session_id,
            SessionLiveEventKind::ProviderStreamProgress {
                turn_id: turn_id.to_string(),
                event,
            },
        )
        .await;
}

const fn live_tool_argument_preview_with_bytes(
    mut preview: LiveToolArgumentPreview,
    argument_bytes: usize,
) -> LiveToolArgumentPreview {
    match &mut preview {
        LiveToolArgumentPreview::FileEdit(file) => {
            file.argument_bytes = argument_bytes;
        }
        LiveToolArgumentPreview::ShellCommand(shell) => {
            shell.argument_bytes = argument_bytes;
        }
        LiveToolArgumentPreview::Query(query) => {
            query.argument_bytes = argument_bytes;
        }
    }
    preview
}

async fn publish_tool_argument_preview_live(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    tool_call_id: String,
    tool_name: String,
    argument_bytes: usize,
    preview: LiveToolArgumentPreview,
) {
    let _ = state
        .sessions
        .publish_live_event(
            session_id,
            SessionLiveEventKind::ToolArgumentPreview {
                turn_id: turn_id.to_string(),
                tool_call_id,
                tool_name,
                argument_bytes,
                preview,
            },
        )
        .await;
}

async fn append_provider_event_trace(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    event_type: &str,
    detail: Option<String>,
) {
    append_trace_event(
        state,
        session_id,
        Some(turn_id.to_string()),
        SessionTracePhase::ModelProviderEvent,
        SessionTracePayload::ProviderEvent {
            event_type: event_type.to_string(),
            detail,
        },
    )
    .await;
}

async fn update_provider_usage_state(
    state: &ServerState,
    session_id: SessionId,
    usage: &TokenUsage,
) {
    let reuse_key = state
        .active_model_turn_snapshot(session_id)
        .await
        .and_then(|turn| turn.reuse_key);
    let Some(reuse_key) = reuse_key else {
        return;
    };

    let mut provider_state = state.provider_state.lock().await;
    let record = provider_state.records.entry(reuse_key).or_default();
    record.telemetry = ProviderTelemetryState {
        input: usage.input_tokens,
        cached: usage.cached_input_tokens,
        cache_write: usage.cache_write_input_tokens,
        uncached: usage.uncached_input_tokens(),
    };
    provider_state.save();
}

async fn update_provider_metadata_state(
    state: &ServerState,
    session_id: SessionId,
    key: &str,
    value: String,
) {
    let reuse_key = state
        .active_model_turn_snapshot(session_id)
        .await
        .and_then(|turn| turn.reuse_key);
    let Some(reuse_key) = reuse_key else {
        return;
    };

    if key == "provider_state" {
        let Ok(provider_state_value) = serde_json::from_str::<serde_json::Value>(&value) else {
            return;
        };
        let mut provider_state = state.provider_state.lock().await;
        let record = provider_state.records.entry(reuse_key).or_default();
        record.provider_state = Some(provider_state_value);
        provider_state.save();
        drop(provider_state);
        return;
    }

    if key != "provider_response_id" {
        return;
    }
    let reusable_message_count = state
        .active_model_turn_snapshot(session_id)
        .await
        .map_or(0, |turn| turn.request_message_count.saturating_add(1));

    let mut provider_state = state.provider_state.lock().await;
    let record = provider_state.records.entry(reuse_key).or_default();
    record.continuation = Some(ProviderContinuationState {
        provider_response_id: value,
        reusable_message_count,
        updated_sequence: reusable_message_count.try_into().unwrap_or(u64::MAX),
    });
    provider_state.save();
}

async fn agent_policy_status(state: &ServerState) -> Option<PolicyStatusResponse> {
    let mut status = state
        .plugins
        .invoke_service_by_interface_json::<_, PolicyStatusResponse>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_POLICY_STATUS,
            &serde_json::json!({}),
        )
        .await
        .ok()?;
    let discovered_tools = collect_available_tool_names(state).await;
    append_missing_enabled_tool_diagnostics(&mut status, &discovered_tools);
    Some(status)
}

async fn collect_available_tool_names(state: &ServerState) -> BTreeSet<String> {
    collect_tool_definitions(state)
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

fn append_missing_enabled_tool_diagnostics(
    status: &mut PolicyStatusResponse,
    discovered_tools: &BTreeSet<String>,
) {
    for (agent_id, enabled_tools) in [
        ("build", &status.build_enabled_tools),
        ("plan", &status.plan_enabled_tools),
    ] {
        for tool_id in enabled_tools {
            if !discovered_tools.contains(tool_id) {
                status.diagnostics.push(format!(
                    "agent '{agent_id}' enables tool '{tool_id}' but no loaded tool plugin provides it"
                ));
            }
        }
    }
}

async fn list_agent_profiles(state: &ServerState) -> Vec<AgentInfo> {
    state
        .plugins
        .invoke_service_by_interface_json::<_, AgentList>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_LIST_AGENTS,
            &serde_json::json!({}),
        )
        .await
        .ok()
        .map_or_else(default_agent_profiles, |list| list.agents)
}

async fn warn_on_unregistered_agent_ids(state: &ServerState, configured_agent_ids: &[String]) {
    if configured_agent_ids.is_empty() {
        return;
    }
    let registered: BTreeSet<String> = list_agent_profiles(state)
        .await
        .into_iter()
        .flat_map(|agent| std::iter::once(agent.id).chain(agent.aliases))
        .collect();
    for agent_id in configured_agent_ids {
        if !registered.contains(agent_id) {
            tracing::warn!(
                target: "bcode_server::startup",
                agent_id = %agent_id,
                "agent defined in bcode.toml but not registered by any agent-profile plugin; it will be usable via /agent {agent_id} but won't appear in agent pickers"
            );
        }
    }
}

fn default_agent_profiles() -> Vec<AgentInfo> {
    vec![AgentInfo {
        id: "build".to_string(),
        name: "Build".to_string(),
        description: "Default implementation agent".to_string(),
        badge: Some("build".to_string()),
        accent: None,
        aliases: vec!["build".to_string()],
        is_default: true,
    }]
}

async fn resolve_agent_id(state: &ServerState, agent_id: &str) -> Option<String> {
    list_agent_profiles(state)
        .await
        .into_iter()
        .find_map(|agent| {
            (agent.id == agent_id || agent.aliases.iter().any(|alias| alias == agent_id))
                .then_some(agent.id)
        })
}

async fn session_agent_selection(state: &ServerState, session_id: SessionId) -> String {
    if let Some(agent_id) = state.session_agent_selections.lock().await.get(&session_id) {
        return agent_id.clone();
    }
    let selected =
        if let Ok(Some(agent_id)) = state.sessions.current_agent_selection(session_id).await {
            agent_id
        } else {
            default_agent_id(&list_agent_profiles(state).await)
        };
    state
        .session_agent_selections
        .lock()
        .await
        .insert(session_id, selected.clone());
    selected
}

fn default_agent_id(agents: &[AgentInfo]) -> String {
    agents
        .iter()
        .find(|agent| agent.is_default)
        .or_else(|| agents.first())
        .map_or_else(|| "build".to_string(), |agent| agent.id.clone())
}

async fn agent_context(
    state: &ServerState,
    session_id: SessionId,
    agent_id: &str,
) -> Option<AgentContextResponse> {
    let request = AgentContextRequest {
        session_id,
        agent_id: agent_id.to_string(),
        available_tools: collect_tool_definitions(state).await,
    };
    state
        .plugins
        .invoke_service_by_interface_json::<_, AgentContextResponse>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_AGENT_CONTEXT,
            &request,
        )
        .await
        .ok()
}

async fn session_model_selection_with_runtime_context(
    state: &ServerState,
    session_id: SessionId,
    runtime_context: Option<ClientRuntimeContext>,
) -> SessionModelSelection {
    let mut selection = session_model_selection(state, session_id).await;
    if let Some(context) = runtime_context {
        if selection.provider_plugin_id.is_none() {
            selection.provider_plugin_id = context.selected_provider_plugin_id;
        }
        if selection.model_id.is_none() {
            selection.model_id = context.selected_model_id;
        }
        selection.provider_context = context.provider_context;
    }
    selection
}

fn default_model_selection_with_runtime_context(
    state: &ServerState,
    runtime_context: Option<ClientRuntimeContext>,
) -> SessionModelSelection {
    if let Some(context) = runtime_context {
        return model_selection_from_runtime_context(state, context);
    }
    SessionModelSelection {
        provider_plugin_id: state.selected_provider_plugin_id.clone(),
        model_id: state.selected_model_id.clone(),
        thinking_level: None,
        reasoning_effort: state.selected_reasoning.effort.clone(),
        reasoning_summary: state.selected_reasoning.summary.clone(),
        reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
        provider_context: state.selected_provider_context.clone(),
    }
}

fn model_selection_from_runtime_context(
    state: &ServerState,
    context: ClientRuntimeContext,
) -> SessionModelSelection {
    SessionModelSelection {
        provider_plugin_id: context.selected_provider_plugin_id,
        model_id: context.selected_model_id,
        thinking_level: None,
        reasoning_effort: state.selected_reasoning.effort.clone(),
        reasoning_summary: state.selected_reasoning.summary.clone(),
        reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
        provider_context: context.provider_context,
    }
}

async fn session_model_selection(
    state: &ServerState,
    session_id: SessionId,
) -> SessionModelSelection {
    if let Some(selection) = state.session_model_selections.lock().await.get(&session_id) {
        return selection.clone();
    }
    let fallback_runtime_selection = || bcode_session::SessionRuntimeSelection {
        provider_plugin_id: state.selected_provider_plugin_id.clone(),
        model_id: state.selected_model_id.clone(),
        reasoning_effort: state.selected_reasoning.effort.clone(),
        reasoning_summary: state.selected_reasoning.summary.clone(),
    };
    let runtime_selection = state
        .sessions
        .current_runtime_selection(session_id)
        .await
        .unwrap_or_else(|_| fallback_runtime_selection());
    let selection = SessionModelSelection {
        provider_plugin_id: runtime_selection
            .provider_plugin_id
            .as_deref()
            .and_then(provider_to_selection)
            .or_else(|| state.selected_provider_plugin_id.clone()),
        model_id: runtime_selection
            .model_id
            .as_deref()
            .and_then(model_to_selection)
            .or_else(|| state.selected_model_id.clone()),
        thinking_level: None,
        reasoning_effort: runtime_selection
            .reasoning_effort
            .or_else(|| state.selected_reasoning.effort.clone()),
        reasoning_summary: runtime_selection
            .reasoning_summary
            .or_else(|| state.selected_reasoning.summary.clone()),
        reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
        provider_context: state.selected_provider_context.clone(),
    };
    state
        .session_model_selections
        .lock()
        .await
        .insert(session_id, selection.clone());
    selection
}

fn provider_to_selection(provider: &str) -> Option<String> {
    if provider == "<auto>" || provider.is_empty() {
        None
    } else {
        Some(provider.to_string())
    }
}

fn model_to_selection(model: &str) -> Option<String> {
    if model == "<default>" || model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

async fn session_runtime_selection_payload(
    state: &ServerState,
    session_id: SessionId,
) -> bcode_ipc::SessionRuntimeSelection {
    state
        .sessions
        .current_runtime_selection(session_id)
        .await
        .map(|selection| bcode_ipc::SessionRuntimeSelection {
            provider_plugin_id: selection
                .provider_plugin_id
                .as_deref()
                .and_then(provider_to_selection),
            model_id: selection.model_id.as_deref().and_then(model_to_selection),
            reasoning_effort: selection.reasoning_effort,
            reasoning_summary: selection.reasoning_summary,
        })
        .unwrap_or_default()
}

fn has_model_provider(state: &ServerState, provider_plugin_id: Option<&str>) -> bool {
    if let Some(provider_plugin_id) = provider_plugin_id {
        return state
            .plugins
            .registry()
            .manifests()
            .get(provider_plugin_id)
            .is_some_and(|manifest| {
                manifest
                    .services
                    .iter()
                    .any(|service| service.interface_id == MODEL_PROVIDER_INTERFACE_ID)
            });
    }
    state
        .plugins
        .registry()
        .service_registry()
        .providers_for(MODEL_PROVIDER_INTERFACE_ID)
        .is_some()
}

async fn invoke_model_provider_json_blocking<Q, R>(
    state: &ServerState,
    provider_plugin_id: Option<String>,
    operation: &'static str,
    request: Q,
) -> Result<R, String>
where
    Q: serde::Serialize + Send + Sync + 'static,
    R: serde::de::DeserializeOwned + Send + 'static,
{
    invoke_model_provider_json_blocking_scoped(
        state,
        provider_plugin_id,
        operation,
        request,
        PluginInvocationScope::Global,
    )
    .await
}

async fn invoke_model_provider_json_blocking_scoped<Q, R>(
    state: &ServerState,
    provider_plugin_id: Option<String>,
    operation: &'static str,
    request: Q,
    scope: PluginInvocationScope,
) -> Result<R, String>
where
    Q: serde::Serialize + Send + Sync + 'static,
    R: serde::de::DeserializeOwned + Send + 'static,
{
    let provider_plugin_id = if let Some(provider_plugin_id) = provider_plugin_id.as_deref() {
        provider_plugin_id
    } else {
        state
            .plugins
            .registry()
            .service_registry()
            .unique_provider(MODEL_PROVIDER_INTERFACE_ID)
            .map_err(|error| error.to_string())?
    };
    state
        .plugins
        .invoke_service_json_scoped::<_, R>(
            provider_plugin_id,
            MODEL_PROVIDER_INTERFACE_ID,
            operation,
            &request,
            scope,
        )
        .await
        .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn build_model_turn_request(
    state: &ServerState,
    session_id: SessionId,
    trigger_event: &bcode_session_models::SessionEvent,
    round: u32,
    provider_plugin_id: Option<&str>,
    selected_model_id: Option<&str>,
    retry_instruction: Option<&str>,
    selection: &SessionModelSelection,
) -> Result<ModelTurnRequest, bcode_session::SessionError> {
    let build_timer = state.metrics.timer();
    let history_timer = state.metrics.timer();
    let history = state.sessions.model_context_events(session_id).await?;
    state.metrics.record_histogram(
        "model.request_build.load_context_events_duration_ms",
        history_timer.elapsed_ms(),
    );
    state
        .metrics
        .record_histogram("model.context.event_count", history.len() as u64);
    let convert_timer = state.metrics.timer();
    let mut messages =
        session_events_to_model_messages_with_limit(&history, state.tool_output_context_chars);
    state.metrics.record_histogram(
        "model.request_build.convert_events_duration_ms",
        convert_timer.elapsed_ms(),
    );
    state
        .metrics
        .record_histogram("model.context.message_count", messages.len() as u64);
    state.metrics.record_histogram(
        "model.context.message_chars",
        messages
            .iter()
            .map(model_message_context_chars)
            .sum::<usize>() as u64,
    );
    let prompt_cache_timer = state.metrics.timer();
    let prompt_cache = plan_prompt_cache(&mut messages, state.prompt_cache_mode);
    state.metrics.record_histogram(
        "model.request_build.prompt_cache_plan_duration_ms",
        prompt_cache_timer.elapsed_ms(),
    );
    let agent_timer = state.metrics.timer();
    let agent_id = session_agent_selection(state, session_id).await;
    let agent_context = agent_context(state, session_id, &agent_id).await;
    state.metrics.record_histogram(
        "model.request_build.agent_context_duration_ms",
        agent_timer.elapsed_ms(),
    );
    let working_directory_timer = state.metrics.timer();
    let working_directory = state.sessions.session_working_directory(session_id).await?;
    state.metrics.record_histogram(
        "model.request_build.working_directory_duration_ms",
        working_directory_timer.elapsed_ms(),
    );
    let system_prompt_timer = state.metrics.timer();
    let skill_catalog = if state.system_prompt.sections.skill_catalog {
        state.skills.as_ref().map_or_else(String::new, |registry| {
            format_skill_catalog_for_prompt(&registry.list(), &state.skill_prompt_options)
        })
    } else {
        String::new()
    };
    let (system_prompt, dynamic_system_context) = build_coding_system_prompt_parts(
        &working_directory,
        &state.system_prompt,
        agent_context
            .as_ref()
            .and_then(|context| context.system_prompt_suffix.as_deref()),
        Some(&skill_catalog),
    );
    state.metrics.record_histogram(
        "model.request_build.system_prompt_duration_ms",
        system_prompt_timer.elapsed_ms(),
    );
    let mut system_prefix_len = 0;
    if !dynamic_system_context.trim().is_empty() {
        messages.insert(
            system_prefix_len,
            ModelMessage {
                role: MessageRole::System,
                content: vec![ContentBlock::Text {
                    text: dynamic_system_context,
                }],
            },
        );
        system_prefix_len += 1;
    }
    if let Some(instruction) = retry_instruction
        && !instruction.trim().is_empty()
    {
        messages.insert(
            system_prefix_len,
            ModelMessage {
                role: MessageRole::System,
                content: vec![ContentBlock::Text {
                    text: instruction.to_string(),
                }],
            },
        );
        system_prefix_len += 1;
    }
    let skill_context_timer = state.metrics.timer();
    let skill_contexts = turn_skill_contexts(state, session_id, trigger_event.sequence).await;
    state.metrics.record_histogram(
        "model.request_build.skill_context_duration_ms",
        skill_context_timer.elapsed_ms(),
    );
    state.metrics.record_histogram(
        "model.context.skill_context_count",
        skill_contexts.len() as u64,
    );
    for skill_context in skill_contexts {
        let preview = skill_context_preview(&skill_context.context);
        messages.insert(
            system_prefix_len,
            ModelMessage {
                role: MessageRole::System,
                content: vec![ContentBlock::Text {
                    text: skill_context.context,
                }],
            },
        );
        system_prefix_len += 1;
        let _ = state
            .sessions
            .append_event(
                session_id,
                SessionEventKind::SkillContextLoaded {
                    skill_id: skill_context.skill_id,
                    bytes_loaded: skill_context.bytes_loaded,
                    truncated: skill_context.truncated,
                    loaded_at_ms: current_time_ms(),
                    preview: Some(preview),
                    source: Some(skill_context.source),
                },
            )
            .await;
    }
    let enabled_tools = agent_context
        .as_ref()
        .and_then(|context| context.enabled_tools.clone());
    let tool_collection_timer = state.metrics.timer();
    let tools = collect_model_tools(state, enabled_tools).await;
    state.metrics.record_histogram(
        "model.request_build.tool_collection_duration_ms",
        tool_collection_timer.elapsed_ms(),
    );
    state
        .metrics
        .record_histogram("model.context.tool_count", tools.len() as u64);
    let model_id = model_id_for_provider_request(selected_model_id);
    let reasoning_capabilities = resolve_model_reasoning_info(
        state,
        provider_plugin_id,
        selected_model_id,
        &selection.provider_context,
    )
    .await;
    let parameters_timer = state.metrics.timer();
    let parameters = {
        let mut p = ModelParameters::default();
        if let Some(level) = &selection.thinking_level {
            p.reasoning_effort = Some(*level);
        }
        if let Some(reasoning) = reasoning_capabilities.as_ref() {
            if let Some(effort) = supported_reasoning_value(
                selection.reasoning_effort.as_deref(),
                &reasoning.effort_values,
            ) {
                p.reasoning_effort_value = Some(effort.to_owned());
            }
            if let Some(summary) = supported_reasoning_value(
                selection.reasoning_summary.as_deref(),
                &reasoning.summary_values,
            ) {
                p.reasoning_summary = Some(summary.to_owned());
            }
        }
        p
    };
    state.metrics.record_histogram(
        "model.request_build.parameters_duration_ms",
        parameters_timer.elapsed_ms(),
    );
    let projection_timer = state.metrics.timer();
    let projection = ConversationProjection::new(
        session_id,
        provider_plugin_id.unwrap_or("<auto>"),
        &model_id,
        &system_prompt,
        &tools,
        &parameters,
        &messages,
    );
    state.metrics.record_histogram(
        "model.request_build.conversation_projection_duration_ms",
        projection_timer.elapsed_ms(),
    );
    let conversation_reuse_timer = state.metrics.timer();
    let conversation_reuse = plan_conversation_reuse(state, &projection, messages.len()).await;
    state.metrics.record_histogram(
        "model.request_build.conversation_reuse_plan_duration_ms",
        conversation_reuse_timer.elapsed_ms(),
    );
    let metadata_timer = state.metrics.timer();
    let mut metadata = projection.metadata();
    insert_reasoning_metadata(&mut metadata, &parameters);
    if let Some(cache_info) =
        resolve_model_cache_info(state, provider_plugin_id, selected_model_id).await
    {
        insert_model_cache_metadata(&mut metadata, &cache_info);
    }
    state.metrics.record_histogram(
        "model.request_build.metadata_duration_ms",
        metadata_timer.elapsed_ms(),
    );
    let request_assembly_timer = state.metrics.timer();
    let request = ModelTurnRequest {
        session_id,
        turn_id: format!("{}-{}-{round}", session_id, trigger_event.sequence),
        model_id,
        provider_context: selection.provider_context.clone(),
        system_prompt: Some(system_prompt),
        messages,
        tools,
        parameters,
        prompt_cache,
        conversation_reuse,
        metadata,
    };
    state.metrics.record_histogram(
        "model.request_build.request_assembly_duration_ms",
        request_assembly_timer.elapsed_ms(),
    );
    state
        .metrics
        .record_histogram("model.request_build_duration_ms", build_timer.elapsed_ms());
    Ok(request)
}

fn insert_reasoning_metadata(
    metadata: &mut BTreeMap<String, String>,
    parameters: &ModelParameters,
) {
    if let Some(effort) = &parameters.reasoning_effort_value {
        metadata.insert("reasoning_effort".to_string(), effort.clone());
    } else if let Some(effort) = parameters.reasoning_effort {
        metadata.insert("reasoning_effort".to_string(), format!("{effort:?}"));
    }
    if let Some(summary) = &parameters.reasoning_summary {
        metadata.insert("reasoning_summary".to_string(), summary.clone());
    }
}

fn supported_reasoning_value<'a>(
    requested: Option<&'a str>,
    supported: &[String],
) -> Option<&'a str> {
    let requested = requested?.trim();
    if requested.is_empty() {
        return None;
    }
    (supported.is_empty() || supported.iter().any(|value| value == requested)).then_some(requested)
}

async fn resolve_model_reasoning_info(
    state: &ServerState,
    provider_plugin_id: Option<&str>,
    selected_model_id: Option<&str>,
    provider_context: &bcode_model::ProviderRequestContext,
) -> Option<bcode_model::ModelReasoningInfo> {
    let provider_reasoning = resolve_model_reasoning_info_from_provider(
        state,
        provider_plugin_id,
        selected_model_id,
        provider_context.clone(),
    )
    .await
    .or_else(|| state.selected_reasoning_capabilities.clone());
    let override_ =
        selected_model_id.and_then(|model_id| model_reasoning_override(provider_context, model_id));
    merge_reasoning_override(provider_reasoning, override_)
}

async fn resolve_model_reasoning_info_from_provider(
    state: &ServerState,
    provider_plugin_id: Option<&str>,
    selected_model_id: Option<&str>,
    provider_context: bcode_model::ProviderRequestContext,
) -> Option<bcode_model::ModelReasoningInfo> {
    let models = invoke_model_provider_json_blocking::<_, ModelList>(
        state,
        provider_plugin_id.map(ToOwned::to_owned),
        OP_MODELS,
        bcode_model::ModelListRequest {
            provider_context,
            selected_model_id: selected_model_id.map(ToOwned::to_owned),
        },
    )
    .await
    .ok()?;
    select_model_info(&models.models, selected_model_id).and_then(|model| model.reasoning)
}

fn insert_model_cache_metadata(
    metadata: &mut BTreeMap<String, String>,
    cache_info: &bcode_model::ModelCacheInfo,
) {
    let capabilities = cache_info
        .capabilities
        .iter()
        .map(|x| model_cache_capability_name(*x))
        .collect::<Vec<_>>()
        .join(",");
    metadata.insert("model_cache_capabilities".to_string(), capabilities);
}

const fn model_cache_capability_name(
    capability: bcode_model::ModelCacheCapability,
) -> &'static str {
    match capability {
        bcode_model::ModelCacheCapability::PromptCacheKey => "prompt_cache_key",
        bcode_model::ModelCacheCapability::AutomaticPrefixCache => "automatic_prefix_cache",
        bcode_model::ModelCacheCapability::ExplicitCachePoints => "explicit_cache_points",
        bcode_model::ModelCacheCapability::CacheUsageReporting => "cache_usage_reporting",
        bcode_model::ModelCacheCapability::PreviousResponseId => "previous_response_id",
    }
}

async fn resolve_model_cache_info(
    state: &ServerState,
    provider_plugin_id: Option<&str>,
    selected_model_id: Option<&str>,
) -> Option<bcode_model::ModelCacheInfo> {
    let models = invoke_model_provider_json_blocking::<_, ModelList>(
        state,
        provider_plugin_id.map(ToOwned::to_owned),
        OP_MODELS,
        bcode_model::ModelListRequest {
            provider_context: bcode_model::ProviderRequestContext::default(),
            selected_model_id: selected_model_id.map(ToOwned::to_owned),
        },
    )
    .await
    .ok()?;
    select_model_info(&models.models, selected_model_id).map(|model| model.cache)
}

async fn append_model_request_trace(
    state: &ServerState,
    session_id: SessionId,
    request: &ModelTurnRequest,
    provider_plugin_id: Option<&str>,
    round: u32,
) {
    if !state.observability.enabled() {
        return;
    }
    let request_blob = model_request_trace_blob(state, session_id, request, round);
    let metadata = model_request_trace_metadata(request, provider_plugin_id);
    append_trace_event(
        state,
        session_id,
        Some(request.turn_id.clone()),
        SessionTracePhase::ModelRequestBuilt,
        SessionTracePayload::ModelRequestBuilt {
            provider: provider_plugin_id.unwrap_or("<auto>").to_string(),
            model: if request.model_id.is_empty() {
                "<provider-default>".to_string()
            } else {
                request.model_id.clone()
            },
            agent_id: session_agent_selection(state, session_id).await,
            message_count: request.messages.len(),
            tool_count: request.tools.len(),
            system_prompt_chars: request.system_prompt.as_ref().map_or(0, String::len),
            prompt_cache_mode: prompt_cache_mode_name(request.prompt_cache.mode).to_string(),
            conversation_reuse_mode: conversation_reuse_mode_name(request.conversation_reuse.mode)
                .to_string(),
            uses_previous_provider_response: request
                .conversation_reuse
                .previous_provider_response_id
                .is_some(),
            metadata,
            request: request_blob,
        },
    )
    .await;
}

fn model_request_trace_blob(
    state: &ServerState,
    session_id: SessionId,
    request: &ModelTurnRequest,
    round: u32,
) -> Option<bcode_session_models::TraceBlobRef> {
    (state.observability.persist_model_requests || state.observability.debug_enabled())
        .then(|| {
            let mut redacted_request = request.clone();
            redacted_request.provider_context.env = redacted_request
                .provider_context
                .env
                .keys()
                .cloned()
                .map(|key| (key, "<redacted>".to_string()))
                .collect();
            if let Some(auth) = &mut redacted_request.provider_context.auth {
                auth.credentials = auth
                    .credentials
                    .keys()
                    .cloned()
                    .map(|key| {
                        (
                            key,
                            bcode_model::ProviderAuthCredential {
                                value: "<redacted>".to_string(),
                                source: None,
                            },
                        )
                    })
                    .collect();
            }
            state.trace_store.write_json_blob(
                session_id,
                &format!("model-request-round-{round}"),
                &redacted_request,
                state.observability.max_blob_bytes,
            )
        })
        .flatten()
}

fn model_request_trace_metadata(
    request: &ModelTurnRequest,
    provider_plugin_id: Option<&str>,
) -> BTreeMap<String, String> {
    let prompt_cache_points = prompt_cache_point_count(request);
    let mut metadata = request.metadata.clone();
    metadata.insert(
        "message_count".to_string(),
        request.messages.len().to_string(),
    );
    metadata.insert(
        "new_messages_start_index".to_string(),
        request
            .conversation_reuse
            .new_messages_start_index
            .map_or_else(|| "none".to_string(), |index| index.to_string()),
    );
    metadata.insert(
        "sent_message_count".to_string(),
        sent_message_count(request).to_string(),
    );
    metadata.insert(
        "prompt_cache_points".to_string(),
        prompt_cache_points.to_string(),
    );
    metadata.insert(
        "cache_system_prompt".to_string(),
        request.prompt_cache.cache_system_prompt.to_string(),
    );
    metadata.insert(
        "cache_tools".to_string(),
        request.prompt_cache.cache_tools.to_string(),
    );
    metadata.insert(
        "cache_conversation_prefix".to_string(),
        (prompt_cache_points > 0).to_string(),
    );
    let cache_capabilities = request
        .metadata
        .get("model_cache_capabilities")
        .cloned()
        .unwrap_or_else(|| cache_capability(provider_plugin_id));
    metadata.insert("cache_capability".to_string(), cache_capabilities.clone());
    metadata.insert(
        "cache_point_projection".to_string(),
        cache_point_projection(&cache_capabilities, provider_plugin_id, prompt_cache_points),
    );
    metadata.insert(
        "provider_reuse_capability".to_string(),
        provider_reuse_capability(&cache_capabilities, provider_plugin_id),
    );
    metadata
}

fn cache_capability(provider_plugin_id: Option<&str>) -> String {
    match provider_plugin_id {
        Some("bcode.openai-compatible") => "prompt_cache_key_automatic_prefix".to_string(),
        Some("bcode.bedrock") => "provider_specific_cache_control".to_string(),
        Some(_) => "unknown".to_string(),
        None => "auto_provider_unknown".to_string(),
    }
}

fn cache_point_projection(
    cache_capabilities: &str,
    provider_plugin_id: Option<&str>,
    prompt_cache_points: usize,
) -> String {
    if prompt_cache_points == 0 {
        return "none".to_string();
    }
    if cache_capabilities
        .split(',')
        .any(|capability| capability == "explicit_cache_points")
    {
        return "provider_declared_explicit_cache_points".to_string();
    }
    match provider_plugin_id {
        Some("bcode.openai-compatible") => "unsupported_dropped".to_string(),
        Some(_) => "unsupported_or_unknown".to_string(),
        None => "auto_provider_unknown".to_string(),
    }
}

fn provider_reuse_capability(cache_capabilities: &str, provider_plugin_id: Option<&str>) -> String {
    if cache_capabilities
        .split(',')
        .any(|capability| capability == "previous_response_id")
    {
        return "provider_declared_previous_response_id".to_string();
    }
    match provider_plugin_id {
        Some(_) => "unsupported_or_unknown".to_string(),
        None => "auto_provider_unknown".to_string(),
    }
}

fn sent_message_count(request: &ModelTurnRequest) -> usize {
    request
        .conversation_reuse
        .new_messages_start_index
        .filter(|_| {
            request
                .conversation_reuse
                .previous_provider_response_id
                .is_some()
        })
        .map_or(request.messages.len(), |start| {
            request
                .messages
                .len()
                .saturating_sub(start.min(request.messages.len()))
        })
}

fn prompt_cache_point_count(request: &ModelTurnRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::CachePoint { .. }))
        .count()
}

fn model_round_from_turn_id(turn_id: &str) -> Option<u32> {
    turn_id
        .rsplit('-')
        .next()
        .and_then(|round| round.parse().ok())
}

#[derive(Debug, Clone)]
struct ConversationProjection {
    key: ProviderStateKey,
    conversation_hash: String,
}

impl ConversationProjection {
    fn new(
        session_id: SessionId,
        provider_plugin_id: &str,
        model_id: &str,
        system_prompt: &str,
        tools: &[bcode_model::ToolDefinition],
        parameters: &ModelParameters,
        messages: &[ModelMessage],
    ) -> Self {
        let stable_prompt_hash = stable_hash(system_prompt);
        let tools_hash = stable_json_hash(tools);
        let parameters_hash = stable_json_hash(parameters);
        let conversation_hash = stable_json_hash(messages);
        Self {
            key: ProviderStateKey {
                session_id,
                provider_plugin_id: provider_plugin_id.to_string(),
                model_id: model_id.to_string(),
                stable_prompt_hash,
                tools_hash,
                parameters_hash,
            },
            conversation_hash,
        }
    }

    fn reuse_key(&self) -> String {
        stable_json_hash(&self.key)
    }

    fn metadata(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "stable_prompt_hash".to_string(),
                self.key.stable_prompt_hash.clone(),
            ),
            ("tools_hash".to_string(), self.key.tools_hash.clone()),
            (
                "parameters_hash".to_string(),
                self.key.parameters_hash.clone(),
            ),
            (
                "conversation_hash".to_string(),
                self.conversation_hash.clone(),
            ),
            ("conversation_reuse_key".to_string(), self.reuse_key()),
        ])
    }
}

async fn plan_conversation_reuse(
    state: &ServerState,
    projection: &ConversationProjection,
    message_count: usize,
) -> bcode_model::ConversationReuseHints {
    let mode = state.conversation_reuse_mode;
    if !mode.is_enabled() {
        return bcode_model::ConversationReuseHints::default();
    }

    let reuse_key = projection.reuse_key();
    let record = state
        .provider_state
        .lock()
        .await
        .records
        .get(&reuse_key)
        .cloned();
    let previous = record
        .as_ref()
        .and_then(|record| record.continuation.clone())
        .filter(|continuation| continuation.reusable_message_count <= message_count);

    bcode_model::ConversationReuseHints {
        mode,
        key: Some(reuse_key),
        previous_provider_response_id: previous
            .as_ref()
            .map(|continuation| continuation.provider_response_id.clone()),
        new_messages_start_index: previous
            .as_ref()
            .map(|continuation| continuation.reusable_message_count),
        provider_state: record.and_then(|record| record.provider_state),
    }
}

fn stable_json_hash(value: &(impl serde::Serialize + ?Sized)) -> String {
    serde_json::to_string(value).map_or_else(
        |_| stable_hash("<serialize-error>"),
        |json| stable_hash(&json),
    )
}

fn stable_hash(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn plan_prompt_cache(
    messages: &mut [ModelMessage],
    mode: bcode_model::PromptCacheMode,
) -> bcode_model::PromptCacheHints {
    if !mode.is_enabled() {
        return bcode_model::PromptCacheHints::default();
    }

    if mode.cache_conversation_prefix()
        && let Some(index) = conversation_cache_point_index(messages)
    {
        messages[index].content.push(ContentBlock::CachePoint {
            hint: bcode_model::PromptCachePoint {
                label: Some("conversation_prefix".to_string()),
                ttl_seconds: None,
            },
        });
    }

    bcode_model::PromptCacheHints {
        mode,
        cache_system_prompt: true,
        cache_tools: true,
    }
}

fn conversation_cache_point_index(messages: &[ModelMessage]) -> Option<usize> {
    const MIN_MESSAGES_FOR_CONVERSATION_CACHE: usize = 6;
    if messages.len() < MIN_MESSAGES_FOR_CONVERSATION_CACHE {
        return None;
    }
    messages
        .iter()
        .enumerate()
        .rev()
        .skip(2)
        .find_map(|(index, message)| {
            matches!(
                message.role,
                MessageRole::User | MessageRole::Assistant | MessageRole::Tool
            )
            .then_some(index)
        })
}

fn model_id_for_provider_request(selected_model_id: Option<&str>) -> String {
    selected_model_id.map_or_else(String::new, ToString::to_string)
}

const DEFAULT_CODING_SYSTEM_PROMPT: &str = r"You are Bcode, a terminal-native coding agent running on the user's machine.

Operate like a careful pair programmer:
* Understand the user's goal before changing files.
* Prefer inspecting relevant files before editing them.
* Use filesystem tools for file reads/writes/edits instead of guessing file contents.
* Use shell tools for focused validation, discovery, and tests when useful.
* Keep edits minimal, domain-driven, and consistent with existing project conventions.
* Do not create speculative crates, packages, or placeholder files.
* Respect project instructions from AGENTS.md or similar repository guidance when provided.
* Treat discovered project instructions as binding for workflow and validation requirements.
* Before finishing a coding task, run the validation required by project instructions when practical; otherwise run the most relevant formatting, check, or test command.
* If validation cannot be run, explain why.
* Summarize what changed and exactly what validation ran.

Tool and safety rules:
* File writes, edits, and shell commands may require user permission. Ask through tools normally; do not claim a side effect happened unless a tool result confirms it.
* Prefer small, reviewable tool calls over broad destructive commands.
* Never run destructive commands such as deleting broad directories unless explicitly requested and permissioned.
* Treat tool output as potentially partial or truncated.
";

const MAX_REPOSITORY_CONTEXT_CHARS: usize = 12_000;
const MAX_CONTEXT_FILE_CHARS: usize = 6_000;
const MAX_GIT_STATUS_CHARS: usize = 4_000;

fn build_coding_system_prompt_parts(
    cwd: &Path,
    config: &bcode_config::SystemPromptConfig,
    agent_prompt_suffix: Option<&str>,
    skill_catalog: Option<&str>,
) -> (String, String) {
    let (stable_context, dynamic_context) = build_repository_context_parts(cwd);
    let mut stable = match config.mode {
        bcode_config::SystemPromptMode::Default => DEFAULT_CODING_SYSTEM_PROMPT.to_string(),
        bcode_config::SystemPromptMode::Replace => config.text.clone().unwrap_or_default(),
    };
    if config.sections.repository_context {
        stable.push_str("\n\n");
        stable.push_str(&truncate_text(
            &stable_context,
            MAX_REPOSITORY_CONTEXT_CHARS,
        ));
    }
    if config.sections.agent_suffix
        && let Some(suffix) = agent_prompt_suffix
        && !suffix.trim().is_empty()
    {
        stable.push_str("\n\nAgent-specific instructions:\n");
        stable.push_str(suffix.trim());
    }

    let mut dynamic = if config.sections.dynamic_repository_context {
        truncate_text(&dynamic_context, MAX_REPOSITORY_CONTEXT_CHARS)
    } else {
        String::new()
    };
    if let Some(skill_catalog) = skill_catalog
        && !skill_catalog.trim().is_empty()
    {
        dynamic.push_str(skill_catalog);
    }

    (stable, dynamic)
}

fn build_repository_context_parts(cwd: &Path) -> (String, String) {
    let repo_root = discover_git_root(cwd);
    let context_root = repo_root.as_deref().unwrap_or(cwd);

    let mut stable_lines = vec!["Stable repository context:".to_string()];
    stable_lines.push(format!(
        "* Detected project files: {}",
        detected_project_files(context_root).join(", ")
    ));
    if let Some(instructions) = read_nearest_agent_instructions(cwd, context_root) {
        stable_lines.push(format!("* Project instructions excerpt:\n{instructions}"));
    }

    let mut dynamic_lines = vec![
        "Dynamic repository context:".to_string(),
        format!("* Current directory: {}", cwd.display()),
    ];
    if let Some(repo_root) = &repo_root {
        dynamic_lines.push(format!("* Git root: {}", repo_root.display()));
    }
    if let Some(branch) = run_command(context_root, "git", &["branch", "--show-current"][..])
        && !branch.is_empty()
    {
        dynamic_lines.push(format!("* Git branch: {branch}"));
    }
    if let Some(status) = run_command(context_root, "git", &["status", "--short"][..]) {
        dynamic_lines.push(format!(
            "* Git status:\n{}",
            format_block_or_placeholder(&status, "clean")
        ));
    }

    (stable_lines.join("\n"), dynamic_lines.join("\n"))
}

fn discover_git_root(cwd: &Path) -> Option<PathBuf> {
    run_command(cwd, "git", &["rev-parse", "--show-toplevel"][..])
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
}

fn run_command(cwd: &Path, program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| truncate_text(value.trim(), MAX_GIT_STATUS_CHARS))
}

fn detected_project_files(root: &Path) -> Vec<String> {
    let candidates = [
        "AGENTS.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Makefile",
        "justfile",
        "README.md",
    ];
    let detected = candidates
        .into_iter()
        .filter(|candidate| root.join(candidate).exists())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if detected.is_empty() {
        vec!["<none detected>".to_string()]
    } else {
        detected
    }
}

fn read_nearest_agent_instructions(cwd: &Path, stop_at: &Path) -> Option<String> {
    let mut current = Some(cwd);
    while let Some(directory) = current {
        let candidate = directory.join("AGENTS.md");
        if candidate.exists() {
            return read_file_excerpt(&candidate, MAX_CONTEXT_FILE_CHARS);
        }
        if directory == stop_at {
            break;
        }
        current = directory.parent();
    }
    None
}

fn read_file_excerpt(path: &Path, max_chars: usize) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| truncate_text(text.trim(), max_chars))
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        truncated.push_str("\n[truncated]");
    }
    truncated
}

fn project_tool_result_for_model_context(
    result: &str,
    full_output_path: Option<PathBuf>,
    max_context_chars: usize,
) -> String {
    let char_count = result.chars().count();
    if char_count <= max_context_chars {
        return result.to_string();
    }

    let path = full_output_path.map_or_else(
        || "the session trace blob store".to_string(),
        |path| path.display().to_string(),
    );
    let footer = format!(
        "\n\n[tool output truncated for model context: original {char_count} chars / {} bytes. Full retained output saved at: {path}. Bcode already includes the beginning and end of long tool output when possible; do not rerun the same shell command with sed/head/tail just to inspect omitted output. Prefer artifact.metadata, artifact.grep, or artifact.read with max_bytes/from_end for saved tool-output artifacts; filesystem.grep or filesystem.read with offset/limit also work for regular files. Avoid reading the whole file unless necessary.]\n\n",
        result.len()
    );
    if max_context_chars == 0 {
        return footer.trim().to_string();
    }

    let omitted_chars = char_count.saturating_sub(max_context_chars);
    let omission_marker =
        format!("\n\n[... omitted {omitted_chars} chars from tool output ...]\n\n");
    let fixed_chars = omission_marker.chars().count() + footer.chars().count();
    if fixed_chars >= max_context_chars {
        return footer.chars().take(max_context_chars).collect();
    }

    let budget = max_context_chars.saturating_sub(fixed_chars);
    let head_chars = budget / 2;
    let tail_chars = budget.saturating_sub(head_chars);
    let head = result.chars().take(head_chars).collect::<String>();
    let tail = result
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{head}{omission_marker}{tail}{footer}")
}

fn trace_blob_read_path(blob: &TraceBlobRef) -> PathBuf {
    let path = PathBuf::from(&blob.path);
    if path.is_absolute() {
        path
    } else {
        default_trace_store_dir().join(path)
    }
}

fn format_block_or_placeholder(value: &str, placeholder: &str) -> String {
    if value.is_empty() {
        format!("  {placeholder}")
    } else {
        value
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

async fn collect_model_tools(
    state: &ServerState,
    enabled_tools: Option<Vec<String>>,
) -> Vec<bcode_model::ToolDefinition> {
    let enabled_tools = enabled_tools.map(|tools| tools.into_iter().collect::<BTreeSet<_>>());
    collect_tool_definitions(state)
        .await
        .into_iter()
        .filter(|tool| {
            enabled_tools
                .as_ref()
                .is_none_or(|enabled| enabled.contains(&tool.name))
        })
        .map(|tool| bcode_model::ToolDefinition {
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
            side_effect: match tool.side_effect {
                bcode_tool::ToolSideEffect::ReadOnly => bcode_model::ToolSideEffect::ReadOnly,
                bcode_tool::ToolSideEffect::WriteFiles => bcode_model::ToolSideEffect::WriteFiles,
                bcode_tool::ToolSideEffect::ExecuteProcess => {
                    bcode_model::ToolSideEffect::ExecuteProcess
                }
            },
            requires_permission: tool.requires_permission,
        })
        .collect()
}

async fn collect_tool_definitions(state: &ServerState) -> Vec<ServiceToolDefinition> {
    let mut tools = Vec::new();
    for plugin_id in tool_provider_plugin_ids(state) {
        let response = state
            .plugins
            .invoke_service_json::<_, ToolList>(
                &plugin_id,
                TOOL_SERVICE_INTERFACE_ID,
                OP_LIST_TOOLS,
                &ListToolsRequest::default(),
            )
            .await;
        match response {
            Ok(list) => tools.extend(list.tools),
            Err(error) => eprintln!("failed to list tools from {plugin_id}: {error}"),
        }
    }
    tools
}

async fn invoke_host_provider_native_search(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: &str,
    bridge_request: bcode_tool::HostModelNativeWebSearchRequest,
) -> Result<ToolInvocationResponse, String> {
    let selection = session_model_selection(state, session_id).await;
    let request = NativeWebSearchRequest {
        query: bridge_request.query,
        max_results: bridge_request.max_results,
        site: bridge_request.site,
        freshness: bridge_request.freshness,
        region: bridge_request.region,
        safe_search: bridge_request.safe_search,
        provider_context: selection.provider_context,
        metadata: BTreeMap::from([("tool_call_id".to_string(), tool_call_id.to_string())]),
    };
    let response = invoke_model_provider_json_blocking::<_, NativeWebSearchResponse>(
        state,
        selection.provider_plugin_id,
        OP_NATIVE_WEB_SEARCH,
        request,
    )
    .await?;
    Ok(ToolInvocationResponse {
        output: serde_json::to_string_pretty(&response).map_err(|error| error.to_string())?,
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    })
}

#[allow(clippy::too_many_lines)]
async fn execute_model_tool(
    state: &ServerState,
    session_id: SessionId,
    call: bcode_model::ToolCall,
    cancel_state: Arc<TurnCancelState>,
    command_context: &mut RuntimeCommandContext<'_>,
) {
    let request_presentation = find_tool_provider(state, &call.name)
        .await
        .ok()
        .flatten()
        .and_then(|(_, definition)| definition.ui.request_presentation)
        .as_ref()
        .map(service_request_presentation_to_session);
    append_tool_request_event(
        state,
        session_id,
        call.id.clone(),
        call.name.clone(),
        serde_json::to_string(&call.arguments).unwrap_or_default(),
        request_presentation,
    )
    .await;
    if cancel_state.is_cancelled() {
        cancel_registered_runtime_work(
            state,
            session_id,
            RuntimeWorkId::new(format!("tool_{}", call.id)),
            None,
        )
        .await;
        append_tool_finished_event(
            state,
            session_id,
            ToolFinishedEventInput {
                tool_call_id: call.id,
                result: "tool skipped because model turn was cancelled".to_string(),
                is_error: true,
                content: Vec::new(),
                output: None,
                semantic_result: None,
            },
        )
        .await;
        return;
    }
    let tool_start = Instant::now();
    let result = invoke_model_tool(
        state,
        session_id,
        &call,
        cancel_state.as_ref(),
        command_context,
    )
    .await
    .unwrap_or_else(|error| ToolInvocationResponse {
        output: error,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    });
    let semantic_result = result.result.clone().map(service_tool_result_to_session);
    let artifact_output = result.full_output.as_deref().unwrap_or(&result.output);
    let output_blob = (state.observability.persist_tool_io || state.observability.debug_enabled())
        .then(|| {
            state.trace_store.write_text_blob(
                session_id,
                &format!("tool-output-{}", call.id),
                artifact_output,
                0,
            )
        })
        .flatten();
    append_trace_event(
        state,
        session_id,
        None,
        SessionTracePhase::ToolInvocationFinished,
        SessionTracePayload::ToolInvocationFinished {
            tool_call_id: call.id.clone(),
            duration_ms: elapsed_ms(tool_start),
            is_error: result.is_error,
            output_bytes: artifact_output.len(),
            output: output_blob.clone(),
        },
    )
    .await;
    append_tool_finished_event(
        state,
        session_id,
        ToolFinishedEventInput {
            tool_call_id: call.id,
            result: result.output,
            is_error: result.is_error,
            content: result.content,
            output: output_blob,
            semantic_result,
        },
    )
    .await;
}

#[allow(clippy::too_many_lines)]
async fn invoke_model_tool(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
    cancel_state: &TurnCancelState,
    command_context: &mut RuntimeCommandContext<'_>,
) -> Result<ToolInvocationResponse, String> {
    let (plugin_id, definition) = find_tool_provider(state, &call.name)
        .await?
        .ok_or_else(|| format!("tool not found: {}", call.name))?;
    if cancel_state.is_cancelled() {
        return Ok(ToolInvocationResponse {
            output: "tool cancelled before invocation".to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        });
    }
    let argument_blob = (state.observability.persist_tool_io
        || state.observability.debug_enabled())
    .then(|| {
        state.trace_store.write_json_blob(
            session_id,
            &format!("tool-arguments-{}", call.id),
            &call.arguments,
            state.observability.max_blob_bytes,
        )
    })
    .flatten();
    append_trace_event(
        state,
        session_id,
        None,
        SessionTracePhase::ToolInvocationStarted,
        SessionTracePayload::ToolInvocationStarted {
            tool_call_id: call.id.clone(),
            plugin_id: plugin_id.clone(),
            tool_name: definition.name.clone(),
            side_effect: side_effect_name(definition.side_effect).to_string(),
            requires_permission: definition.requires_permission,
            arguments: argument_blob,
        },
    )
    .await;
    let agent_decision = evaluate_agent_tool_policy(state, session_id, call, &definition).await;
    append_trace_event(
        state,
        session_id,
        None,
        SessionTracePhase::ToolPolicyEvaluated,
        SessionTracePayload::ToolPolicyEvaluated {
            tool_call_id: call.id.clone(),
            agent_id: session_agent_selection(state, session_id).await,
            decision: agent_decision_name(agent_decision.decision).to_string(),
            reason: agent_decision.reason.clone(),
        },
    )
    .await;
    let skill_decision = evaluate_active_skill_tool_policy(state, session_id, &definition).await;
    append_trace_event(
        state,
        session_id,
        None,
        SessionTracePhase::ToolPolicyEvaluated,
        SessionTracePayload::ToolPolicyEvaluated {
            tool_call_id: call.id.clone(),
            agent_id: session_agent_selection(state, session_id).await,
            decision: skill_tool_policy_decision_name(&skill_decision).to_string(),
            reason: skill_tool_policy_reason(&skill_decision),
        },
    )
    .await;
    if let SkillToolPolicyOutcome::Deny { reason } = skill_decision {
        return Ok(ToolInvocationResponse {
            output: reason,
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        });
    }
    let skill_requests_permission = matches!(skill_decision, SkillToolPolicyOutcome::Ask { .. });
    match agent_decision.decision {
        AgentDecision::Deny => {
            return Ok(ToolInvocationResponse {
                output: agent_decision
                    .reason
                    .unwrap_or_else(|| "tool denied by active agent policy".to_string()),
                is_error: true,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            });
        }
        AgentDecision::Ask => {
            if !request_tool_permission(
                state,
                session_id,
                call,
                &definition,
                cancel_state,
                PermissionPolicyContext {
                    source: None,
                    reason: agent_decision.reason.clone(),
                    skill_decision_key: None,
                },
            )
            .await
            {
                return Ok(ToolInvocationResponse {
                    output: "permission denied".to_string(),
                    is_error: true,
                    content: Vec::new(),
                    full_output: None,
                    host_action: None,
                    result: None,
                });
            }
        }
        AgentDecision::Allow => {
            if skill_requests_permission {
                let skill_decision_key =
                    skill_tool_decision_key(state, session_id, &definition.name).await;
                match skill_decision_key
                    .as_ref()
                    .and_then(remembered_skill_tool_decision)
                {
                    Some(SkillToolDecision::Allow) => {
                        append_trace_event(
                            state,
                            session_id,
                            None,
                            SessionTracePhase::ToolPolicyEvaluated,
                            SessionTracePayload::ToolPolicyEvaluated {
                                tool_call_id: call.id.clone(),
                                agent_id: session_agent_selection(state, session_id).await,
                                decision: "remembered_skill_allow".to_string(),
                                reason: Some(
                                    "remembered skill tool decision allowed prompt skip"
                                        .to_string(),
                                ),
                            },
                        )
                        .await;
                    }
                    Some(SkillToolDecision::Deny) => {
                        append_trace_event(
                            state,
                            session_id,
                            None,
                            SessionTracePhase::ToolPolicyEvaluated,
                            SessionTracePayload::ToolPolicyEvaluated {
                                tool_call_id: call.id.clone(),
                                agent_id: session_agent_selection(state, session_id).await,
                                decision: "remembered_skill_deny".to_string(),
                                reason: Some(
                                    "remembered skill tool decision denied tool call".to_string(),
                                ),
                            },
                        )
                        .await;
                        return Ok(ToolInvocationResponse {
                            output: "tool denied by remembered skill policy decision".to_string(),
                            is_error: true,
                            content: Vec::new(),
                            full_output: None,
                            host_action: None,
                            result: None,
                        });
                    }
                    None => {
                        if !request_tool_permission(
                            state,
                            session_id,
                            call,
                            &definition,
                            cancel_state,
                            PermissionPolicyContext {
                                source: Some("skill".to_string()),
                                reason: skill_tool_policy_reason(&skill_decision),
                                skill_decision_key,
                            },
                        )
                        .await
                        {
                            return Ok(ToolInvocationResponse {
                                output: "permission denied".to_string(),
                                is_error: true,
                                content: Vec::new(),
                                full_output: None,
                                host_action: None,
                                result: None,
                            });
                        }
                    }
                }
            }
        }
    }
    let working_directory = state
        .sessions
        .session_working_directory(session_id)
        .await
        .map_err(|error| error.to_string())?;
    let cancellation_path = default_session_artifact_dir(session_id).join(format!(
        "tool-cancel-{}",
        call.id
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>()
    ));
    let request = ToolInvocationRequest {
        tool_call_id: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
        cwd: Some(working_directory),
        artifact_dir: Some(default_session_artifact_dir(session_id)),
        cancellation_path: Some(cancellation_path.clone()),
    };
    let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    let scope = active_plugin_scope_for_tool_call(state, session_id, &call.id).await;
    let mut invocation = state
        .plugins
        .invoke_service_with_events_scoped(
            &plugin_id,
            TOOL_SERVICE_INTERFACE_ID,
            OP_INVOKE_TOOL,
            payload,
            scope,
        )
        .await
        .map_err(|error| error.to_string())?;
    let tool_work_id = RuntimeWorkId::new(format!("tool_{}", call.id));
    let _ = state
        .runtime_work
        .replace_cancellation(
            session_id,
            &tool_work_id,
            CancellationHandle::PluginInvocation(invocation.cancel.clone()),
        )
        .await;
    let mut tool_output_publisher = ToolOutputLivePublisher::new();
    let mut stream_sequences: BTreeMap<String, u64> = BTreeMap::new();
    let response = loop {
        tokio::select! {
            cancel_command = command_context.cancel_commands.recv() => {
                if let Some(command) = cancel_command {
                    let cancelled = process_cancel_turn_command(
                        state,
                        session_id,
                        command_context.followup_commands,
                        command_context.queued_followups,
                        command.clear_queue,
                        command.requested_by,
                    )
                    .await;
                    let _sent = command.response.send(cancelled);
                }
                if cancel_state.is_cancelled() {
                    invocation.cancel.cancel();
                    let _ = std::fs::write(&cancellation_path, b"cancelled\n");
                    cancel_registered_runtime_work(
                        state,
                        session_id,
                        RuntimeWorkId::new(format!("tool_{}", call.id)),
                        None,
                    )
                    .await;
                    return Ok(ToolInvocationResponse {
                        output: "tool invocation cancelled".to_string(),
                        is_error: true,
                        content: Vec::new(),
                        full_output: None,
                        host_action: None,
                        result: None,
                    });
                }
            }
            steering_command = command_context.steering_commands.recv() => {
                if let Some(command) = steering_command {
                    process_steering_message_command(
                        state,
                        session_id,
                        command.client_id,
                        command.text,
                        command.completion,
                    )
                    .await;
                }
                if cancel_state.is_cancelled() {
                    invocation.cancel.cancel();
                    let _ = std::fs::write(&cancellation_path, b"cancelled\n");
                    cancel_registered_runtime_work(
                        state,
                        session_id,
                        RuntimeWorkId::new(format!("tool_{}", call.id)),
                        None,
                    )
                    .await;
                    return Ok(ToolInvocationResponse {
                        output: "tool invocation cancelled".to_string(),
                        is_error: true,
                        content: Vec::new(),
                        full_output: None,
                        host_action: None,
                        result: None,
                    });
                }
            }
            () = cancel_state.cancelled() => {
                invocation.cancel.cancel();
                let _ = std::fs::write(&cancellation_path, b"cancelled\n");
                cancel_registered_runtime_work(
                    state,
                    session_id,
                    RuntimeWorkId::new(format!("tool_{}", call.id)),
                    None,
                )
                .await;
                return Ok(ToolInvocationResponse {
                    output: "tool invocation cancelled".to_string(),
                    is_error: true,
                    content: Vec::new(),
                    full_output: None,
            host_action: None,
            result: None,
                });
            }
            publisher_event = tool_output_publisher.next_event() => {
                if let Some(publisher_event) = publisher_event {
                    tool_output_publisher.handle_event(state, session_id, publisher_event).await;
                }
            }
            event = invocation.next_event() => {
                match event.map_err(|error| error.to_string())? {
                    StreamingServiceInvocationEvent::Event(payload) => {
                        if let Ok(event) = serde_json::from_slice::<ServiceToolInvocationStreamEvent>(&payload) {
                            tool_output_publisher.push_stream_event(
                                state,
                                session_id,
                                normalize_tool_stream_event_sequence(event, &mut stream_sequences),
                            )
                            .await;
                        }
                    }
                    StreamingServiceInvocationEvent::Response(response) => {
                        break response.map_err(|error| error.to_string())?;
                    }
                }
            }
        }
    };
    while let Some(payload) = invocation.try_recv_event() {
        if let Ok(event) = serde_json::from_slice::<ServiceToolInvocationStreamEvent>(&payload) {
            tool_output_publisher
                .push_stream_event(
                    state,
                    session_id,
                    normalize_tool_stream_event_sequence(event, &mut stream_sequences),
                )
                .await;
        }
    }
    tool_output_publisher.finish(state, session_id).await;
    let response: ToolInvocationResponse =
        bcode_plugin::decode_service_response(response).map_err(|error| error.to_string())?;
    if let Some(bcode_tool::ToolInvocationHostAction::HostModelNativeWebSearch(request)) =
        response.host_action
    {
        return invoke_host_provider_native_search(state, session_id, &call.id, request).await;
    }
    Ok(response)
}

/// Append durable tool stream lifecycle events or publish ephemeral output deltas.
///
/// `OutputDelta` carries raw live tool output, including PTY bytes. These chunks
/// are intentionally transient: they are broadcast to currently attached clients
/// and must not be appended to durable session history. Durable history stores the
/// tool request, stream lifecycle metadata, final status, and final bounded tool
/// result instead.
async fn append_tool_stream_event(
    state: &ServerState,
    session_id: SessionId,
    event: ToolInvocationStreamEvent,
) {
    let progress = runtime_work_progress_from_tool_stream_event(&event);
    if matches!(event, ToolInvocationStreamEvent::OutputDelta { .. }) {
        let _ = state
            .sessions
            .publish_live_event(session_id, SessionLiveEventKind::ToolOutputDelta { event })
            .await;
        return;
    }

    match state
        .sessions
        .append_event(session_id, SessionEventKind::ToolInvocationStream { event })
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append tool stream event: {error}"),
    }
    if let Some((work_id, message)) = progress {
        append_runtime_work_progress_event(state, session_id, work_id, message, None, None).await;
    }
}

fn runtime_work_progress_from_tool_stream_event(
    event: &ToolInvocationStreamEvent,
) -> Option<(RuntimeWorkId, String)> {
    match event {
        ToolInvocationStreamEvent::Status {
            tool_call_id,
            message,
            ..
        } => Some((
            RuntimeWorkId::new(format!("tool_{tool_call_id}")),
            message.clone(),
        )),
        ToolInvocationStreamEvent::Started {
            tool_call_id,
            tool_name,
            ..
        } => Some((
            RuntimeWorkId::new(format!("tool_{tool_call_id}")),
            format!("started {tool_name}"),
        )),
        ToolInvocationStreamEvent::Finished {
            tool_call_id,
            is_error,
            ..
        } => Some((
            RuntimeWorkId::new(format!("tool_{tool_call_id}")),
            if *is_error {
                "finished with error"
            } else {
                "finished"
            }
            .to_string(),
        )),
        ToolInvocationStreamEvent::OutputDelta { .. }
        | ToolInvocationStreamEvent::Presentation { .. } => None,
    }
}

fn normalize_tool_stream_event_sequence(
    event: ServiceToolInvocationStreamEvent,
    stream_sequences: &mut BTreeMap<String, u64>,
) -> ToolInvocationStreamEvent {
    let event = convert_tool_stream_event(event);
    let tool_call_id = match &event {
        ToolInvocationStreamEvent::Started { tool_call_id, .. }
        | ToolInvocationStreamEvent::OutputDelta { tool_call_id, .. }
        | ToolInvocationStreamEvent::Status { tool_call_id, .. }
        | ToolInvocationStreamEvent::Presentation { tool_call_id, .. }
        | ToolInvocationStreamEvent::Finished { tool_call_id, .. } => tool_call_id.clone(),
    };
    let next = stream_sequences
        .entry(tool_call_id)
        .and_modify(|sequence| *sequence = sequence.saturating_add(1))
        .or_insert(1);
    let sequence = *next;
    set_tool_stream_event_sequence(event, sequence)
}

fn set_tool_stream_event_sequence(
    event: ToolInvocationStreamEvent,
    sequence: u64,
) -> ToolInvocationStreamEvent {
    match event {
        ToolInvocationStreamEvent::Started {
            tool_call_id,
            tool_name,
            terminal,
            columns,
            rows,
            started_at_ms,
            ..
        } => ToolInvocationStreamEvent::Started {
            tool_call_id,
            tool_name,
            sequence,
            terminal,
            columns,
            rows,
            started_at_ms,
        },
        ToolInvocationStreamEvent::OutputDelta {
            tool_call_id,
            stream,
            text,
            byte_len,
            ..
        } => ToolInvocationStreamEvent::OutputDelta {
            tool_call_id,
            stream,
            sequence,
            text,
            byte_len,
        },
        ToolInvocationStreamEvent::Status {
            tool_call_id,
            message,
            ..
        } => ToolInvocationStreamEvent::Status {
            tool_call_id,
            sequence,
            message,
        },
        ToolInvocationStreamEvent::Presentation {
            tool_call_id,
            presentation,
            ..
        } => ToolInvocationStreamEvent::Presentation {
            tool_call_id,
            sequence,
            presentation,
        },
        ToolInvocationStreamEvent::Finished {
            tool_call_id,
            is_error,
            finished_at_ms,
            ..
        } => ToolInvocationStreamEvent::Finished {
            tool_call_id,
            sequence,
            is_error,
            finished_at_ms,
        },
    }
}

fn convert_tool_stream_event(event: ServiceToolInvocationStreamEvent) -> ToolInvocationStreamEvent {
    match event {
        ServiceToolInvocationStreamEvent::Started {
            tool_call_id,
            tool_name,
            sequence,
            terminal,
            columns,
            rows,
            started_at_ms,
        } => ToolInvocationStreamEvent::Started {
            tool_call_id,
            tool_name,
            sequence,
            terminal,
            columns,
            rows,
            started_at_ms: started_at_ms.or_else(|| Some(current_unix_millis())),
        },
        ServiceToolInvocationStreamEvent::OutputDelta {
            tool_call_id,
            stream,
            sequence,
            text,
            byte_len,
        } => ToolInvocationStreamEvent::OutputDelta {
            tool_call_id,
            stream: convert_tool_output_stream(stream),
            sequence,
            text,
            byte_len,
        },
        ServiceToolInvocationStreamEvent::Status {
            tool_call_id,
            sequence,
            message,
        } => ToolInvocationStreamEvent::Status {
            tool_call_id,
            sequence,
            message,
        },
        ServiceToolInvocationStreamEvent::Presentation {
            tool_call_id,
            sequence,
            presentation,
        } => ToolInvocationStreamEvent::Presentation {
            tool_call_id,
            sequence,
            presentation: convert_tool_presentation_event(presentation),
        },
        ServiceToolInvocationStreamEvent::Finished {
            tool_call_id,
            sequence,
            is_error,
            finished_at_ms,
        } => ToolInvocationStreamEvent::Finished {
            tool_call_id,
            sequence,
            is_error,
            finished_at_ms: finished_at_ms.or_else(|| Some(current_unix_millis())),
        },
    }
}

fn convert_tool_presentation_event(event: ServiceToolPresentationEvent) -> ToolPresentationEvent {
    match event {
        ServiceToolPresentationEvent::Status(status) => {
            ToolPresentationEvent::Status(bcode_session_models::ToolStatusPresentation {
                target: convert_tool_presentation_target(status.target),
                text: status.text,
                level: convert_tool_presentation_level(status.level),
            })
        }
        ServiceToolPresentationEvent::Card(card) => {
            ToolPresentationEvent::Card(ToolCardPresentation {
                target: convert_tool_presentation_target(card.target),
                title: card.title,
                subtitle: card.subtitle,
                sections: card
                    .sections
                    .into_iter()
                    .map(convert_tool_presentation_section)
                    .collect(),
            })
        }
        ServiceToolPresentationEvent::Progress(progress) => {
            ToolPresentationEvent::Progress(ToolProgressPresentation {
                target: convert_tool_presentation_target(progress.target),
                text: progress.text,
                percent: progress.percent,
                level: convert_tool_presentation_level(progress.level),
            })
        }
        ServiceToolPresentationEvent::Clear { target } => ToolPresentationEvent::Clear {
            target: convert_tool_presentation_target(target),
        },
    }
}

const fn convert_tool_presentation_target(
    target: ServiceToolPresentationTarget,
) -> ToolPresentationTarget {
    match target {
        ServiceToolPresentationTarget::Activity => ToolPresentationTarget::Activity,
        ServiceToolPresentationTarget::Preview => ToolPresentationTarget::Preview,
        ServiceToolPresentationTarget::Result => ToolPresentationTarget::Result,
    }
}

const fn convert_tool_presentation_level(
    level: ServiceToolPresentationLevel,
) -> ToolPresentationLevel {
    match level {
        ServiceToolPresentationLevel::Info => ToolPresentationLevel::Info,
        ServiceToolPresentationLevel::Success => ToolPresentationLevel::Success,
        ServiceToolPresentationLevel::Warning => ToolPresentationLevel::Warning,
        ServiceToolPresentationLevel::Error => ToolPresentationLevel::Error,
    }
}

fn convert_tool_presentation_section(
    section: ServiceToolPresentationSection,
) -> ToolPresentationSection {
    match section {
        ServiceToolPresentationSection::Text { label, text } => {
            ToolPresentationSection::Text { label, text }
        }
        ServiceToolPresentationSection::Fields { fields } => ToolPresentationSection::Fields {
            fields: fields
                .into_iter()
                .map(|field| ToolPresentationFieldValue {
                    label: field.label,
                    value: field.value,
                })
                .collect(),
        },
        ServiceToolPresentationSection::Diff {
            path,
            old_text,
            new_text,
        } => ToolPresentationSection::Diff {
            path,
            old_text,
            new_text,
        },
        ServiceToolPresentationSection::Terminal {
            output,
            columns,
            rows,
        } => ToolPresentationSection::Terminal {
            output,
            columns,
            rows,
        },
    }
}

const fn convert_tool_output_stream(stream: ToolOutputStream) -> SessionToolOutputStream {
    match stream {
        ToolOutputStream::Stdout => SessionToolOutputStream::Stdout,
        ToolOutputStream::Stderr => SessionToolOutputStream::Stderr,
        ToolOutputStream::Pty => SessionToolOutputStream::Pty,
    }
}

async fn evaluate_agent_tool_policy(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
    definition: &ServiceToolDefinition,
) -> EvaluateToolCallResponse {
    let agent_id = session_agent_selection(state, session_id).await;
    let cwd = state
        .sessions
        .session_working_directory(session_id)
        .await
        .ok()
        .map(|path| path.display().to_string());
    let request = EvaluateToolCallRequest {
        session_id,
        agent_id,
        tool_name: definition.name.clone(),
        side_effect: definition.side_effect,
        policy: definition.policy.clone(),
        arguments: call.arguments.clone(),
        cwd,
    };
    state
        .plugins
        .invoke_service_by_interface_json::<_, EvaluateToolCallResponse>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_EVALUATE_TOOL_CALL,
            &request,
        )
        .await
        .ok()
        .unwrap_or(EvaluateToolCallResponse {
            decision: if definition.requires_permission {
                AgentDecision::Ask
            } else {
                AgentDecision::Allow
            },
            reason: None,
        })
}

fn remembered_skill_tool_decision(key: &SkillToolDecisionKey) -> Option<SkillToolDecision> {
    SettingsStore::default()
        .skill_tool_decisions()
        .ok()
        .and_then(|state| state.decision_for(key).map(|entry| entry.decision))
}

fn remember_skill_tool_decision(key: SkillToolDecisionKey, decision: SkillToolDecision) {
    let store = SettingsStore::default();
    let Ok(mut state) = store.skill_tool_decisions() else {
        return;
    };
    state.upsert(SkillToolDecisionEntry {
        key,
        decision,
        remembered_at_ms: current_time_ms(),
        reason: Some("remembered from permission dialog".to_string()),
    });
    let _ = store.save_skill_tool_decisions(&state, current_time_ms());
}

async fn skill_tool_decision_key(
    state: &ServerState,
    session_id: SessionId,
    tool_name: &str,
) -> Option<SkillToolDecisionKey> {
    let skill_ids = state
        .active_skills
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .unwrap_or_default();
    if skill_ids.is_empty() {
        return None;
    }
    let working_directory = state
        .sessions
        .session_working_directory(session_id)
        .await
        .ok()?;
    Some(skill_tool_decision_key_for_working_directory(
        skill_ids,
        tool_name,
        &working_directory,
    ))
}

fn skill_tool_decision_key_for_working_directory(
    skill_ids: BTreeSet<SkillId>,
    tool_name: &str,
    working_directory: &Path,
) -> SkillToolDecisionKey {
    SkillToolDecisionKey {
        skill_ids,
        tool_name: tool_name.to_string(),
        scope: SkillToolDecisionScope::Workspace {
            workspace_id: workspace_identity(working_directory),
        },
    }
}

fn workspace_identity(working_directory: &Path) -> String {
    git_root_for_path(working_directory)
        .or_else(|| working_directory.canonicalize().ok())
        .unwrap_or_else(|| working_directory.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn git_root_for_path(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let root = stdout.trim();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

async fn list_service_tools(state: &ServerState) -> Vec<ServiceToolDefinition> {
    let mut tools = Vec::new();
    for plugin_id in tool_provider_plugin_ids(state) {
        match state
            .plugins
            .invoke_service_json::<_, ToolList>(
                &plugin_id,
                TOOL_SERVICE_INTERFACE_ID,
                OP_LIST_TOOLS,
                &ListToolsRequest::default(),
            )
            .await
        {
            Ok(list) => tools.extend(list.tools),
            Err(error) => eprintln!("failed to list tools from {plugin_id}: {error}"),
        }
    }
    tools
}

async fn evaluate_active_skill_tool_policy(
    state: &ServerState,
    session_id: SessionId,
    definition: &ServiceToolDefinition,
) -> SkillToolPolicyOutcome {
    let Some(registry) = &state.skills else {
        return SkillToolPolicyOutcome::NoOpinion;
    };
    let skill_ids = state
        .active_skills
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .unwrap_or_default();
    if skill_ids.is_empty() {
        return SkillToolPolicyOutcome::NoOpinion;
    }
    let available_tools = list_service_tools(state).await;
    let active_policies = skill_ids
        .into_iter()
        .filter_map(|skill_id| registry.describe(&skill_id).ok())
        .map(|manifest| {
            resolve_skill_permission_policy(&manifest.permission_policy, &available_tools)
        })
        .collect();
    evaluate_skill_tool_call(&SkillToolPolicyRequest {
        tool: definition.clone(),
        active_policies,
    })
}

const fn skill_tool_policy_decision_name(outcome: &SkillToolPolicyOutcome) -> &'static str {
    match outcome {
        SkillToolPolicyOutcome::NoOpinion => "skill_no_opinion",
        SkillToolPolicyOutcome::Allow { .. } => "skill_allow",
        SkillToolPolicyOutcome::Warn { .. } => "skill_warn",
        SkillToolPolicyOutcome::Ask { .. } => "skill_ask",
        SkillToolPolicyOutcome::Deny { .. } => "skill_deny",
    }
}

fn skill_tool_policy_reason(outcome: &SkillToolPolicyOutcome) -> Option<String> {
    match outcome {
        SkillToolPolicyOutcome::NoOpinion => None,
        SkillToolPolicyOutcome::Allow { reason }
        | SkillToolPolicyOutcome::Warn { reason }
        | SkillToolPolicyOutcome::Ask { reason }
        | SkillToolPolicyOutcome::Deny { reason } => Some(reason.clone()),
    }
}

async fn find_tool_provider(
    state: &ServerState,
    tool_name: &str,
) -> Result<Option<(String, ServiceToolDefinition)>, String> {
    for plugin_id in tool_provider_plugin_ids(state) {
        let list = state
            .plugins
            .invoke_service_json::<_, ToolList>(
                &plugin_id,
                TOOL_SERVICE_INTERFACE_ID,
                OP_LIST_TOOLS,
                &ListToolsRequest::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
        if let Some(tool) = list.tools.into_iter().find(|tool| tool.name == tool_name) {
            return Ok(Some((plugin_id, tool)));
        }
    }
    Ok(None)
}

fn service_request_presentation_to_session(
    value: &ServiceToolRequestPresentationMetadata,
) -> ToolRequestPresentationMetadata {
    ToolRequestPresentationMetadata {
        title: value.title.clone(),
        fields: value
            .fields
            .iter()
            .map(|field| ToolPresentationField {
                label: field.label.clone(),
                argument: field.argument.clone(),
                kind: service_presentation_field_kind_to_session(field.kind),
                optional: field.optional,
            })
            .collect(),
        preview: value
            .preview
            .as_ref()
            .map(service_request_preview_to_session),
    }
}

fn service_request_preview_to_session(
    value: &bcode_tool::ToolRequestPreviewMetadata,
) -> bcode_session_models::ToolRequestPreviewMetadata {
    match value {
        bcode_tool::ToolRequestPreviewMetadata::FileEdit {
            path_fields,
            old_text_fields,
            new_text_fields,
        } => bcode_session_models::ToolRequestPreviewMetadata::FileEdit {
            path_fields: path_fields.clone(),
            old_text_fields: old_text_fields.clone(),
            new_text_fields: new_text_fields.clone(),
        },
    }
}

const fn service_presentation_field_kind_to_session(
    value: ServiceToolPresentationFieldKind,
) -> ToolPresentationFieldKind {
    match value {
        ServiceToolPresentationFieldKind::Text => ToolPresentationFieldKind::Text,
        ServiceToolPresentationFieldKind::Path => ToolPresentationFieldKind::Path,
        ServiceToolPresentationFieldKind::Url => ToolPresentationFieldKind::Url,
        ServiceToolPresentationFieldKind::Command => ToolPresentationFieldKind::Command,
        ServiceToolPresentationFieldKind::Boolean => ToolPresentationFieldKind::Boolean,
        ServiceToolPresentationFieldKind::Count => ToolPresentationFieldKind::Count,
        ServiceToolPresentationFieldKind::DurationMs => ToolPresentationFieldKind::DurationMs,
        ServiceToolPresentationFieldKind::Json => ToolPresentationFieldKind::Json,
    }
}

fn tool_provider_plugin_ids(state: &ServerState) -> Vec<String> {
    state
        .plugins
        .registry()
        .manifests()
        .values()
        .filter(|manifest| {
            manifest
                .services
                .iter()
                .any(|service| service.interface_id == TOOL_SERVICE_INTERFACE_ID)
        })
        .map(|manifest| manifest.id.clone())
        .collect()
}

#[derive(Debug, Clone, Default)]
struct PermissionPolicyContext {
    source: Option<String>,
    reason: Option<String>,
    skill_decision_key: Option<SkillToolDecisionKey>,
}

#[allow(clippy::too_many_lines)]
async fn request_tool_permission(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
    definition: &ServiceToolDefinition,
    cancel_state: &TurnCancelState,
    policy_context: PermissionPolicyContext,
) -> bool {
    let permission_id = next_permission_id(state).await;
    let arguments_json = serde_json::to_string(&call.arguments).unwrap_or_default();
    let agent_id = session_agent_selection(state, session_id).await;
    append_permission_requested_event(
        state,
        session_id,
        SessionEventKind::PermissionRequested {
            permission_id: permission_id.clone(),
            tool_call_id: call.id.clone(),
            tool_name: definition.name.clone(),
            arguments_json: arguments_json.clone(),
            request_presentation: definition
                .ui
                .request_presentation
                .as_ref()
                .map(service_request_presentation_to_session),
            policy_source: policy_context.source.clone(),
            policy_reason: policy_context.reason.clone(),
        },
    )
    .await;
    append_trace_event(
        state,
        session_id,
        None,
        SessionTracePhase::ToolPermissionWaitStarted,
        SessionTracePayload::ToolPermissionWait {
            permission_id: permission_id.clone(),
            tool_call_id: call.id.clone(),
            approved: None,
            duration_ms: None,
        },
    )
    .await;
    let wait_start = Instant::now();
    let pending = PendingPermission {
        summary: PermissionSummary {
            permission_id: permission_id.clone(),
            session_id,
            tool_call_id: call.id.clone(),
            tool_name: definition.name.clone(),
            arguments_json,
            agent_id,
            request_presentation: definition
                .ui
                .request_presentation
                .as_ref()
                .map(service_request_presentation_to_session),
            policy_source: policy_context.source,
            policy_reason: policy_context.reason,
            can_remember_policy: policy_context.skill_decision_key.is_some(),
        },
        decision: Arc::new(Mutex::new(None)),
        notify: Arc::new(Notify::new()),
        skill_decision_key: policy_context.skill_decision_key,
    };
    state
        .pending_permissions
        .lock()
        .await
        .insert(permission_id, pending.clone());
    loop {
        let decision = *pending.decision.lock().await;
        if let Some(decision) = decision {
            append_trace_event(
                state,
                session_id,
                None,
                SessionTracePhase::ToolPermissionWaitFinished,
                SessionTracePayload::ToolPermissionWait {
                    permission_id: pending.summary.permission_id.clone(),
                    tool_call_id: pending.summary.tool_call_id.clone(),
                    approved: Some(decision),
                    duration_ms: Some(elapsed_ms(wait_start)),
                },
            )
            .await;
            return decision;
        }
        tokio::select! {
            () = pending.notify.notified() => {}
            () = cancel_state.cancelled() => {
                append_trace_event(
                    state,
                    session_id,
                    None,
                    SessionTracePhase::ToolPermissionWaitFinished,
                    SessionTracePayload::ToolPermissionWait {
                        permission_id: pending.summary.permission_id.clone(),
                        tool_call_id: pending.summary.tool_call_id.clone(),
                        approved: Some(false),
                        duration_ms: Some(elapsed_ms(wait_start)),
                    },
                )
                .await;
                state
                    .pending_permissions
                    .lock()
                    .await
                    .remove(&pending.summary.permission_id);
                append_permission_resolved_event(
                    state,
                    session_id,
                    pending.summary.permission_id.clone(),
                    false,
                )
                .await;
                return false;
            }
        }
    }
}

async fn next_permission_id(state: &ServerState) -> String {
    let mut next = state.next_permission_id.lock().await;
    let permission_id = format!("perm-{}", *next);
    *next += 1;
    permission_id
}

async fn append_permission_requested_event(
    state: &ServerState,
    session_id: SessionId,
    request: SessionEventKind,
) {
    match state
        .sessions
        .append_permission_requested(session_id, request)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append permission request: {error}"),
    }
}

async fn append_permission_resolved_event(
    state: &ServerState,
    session_id: SessionId,
    permission_id: String,
    approved: bool,
) {
    match state
        .sessions
        .append_permission_resolved(session_id, permission_id, approved)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append permission result: {error}"),
    }
}

#[cfg(test)]
fn session_events_to_model_messages(
    history: &[bcode_session_models::SessionEvent],
) -> Vec<ModelMessage> {
    session_events_to_model_messages_with_limit(history, usize::MAX)
}

fn session_events_to_model_messages_with_limit(
    history: &[bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
) -> Vec<ModelMessage> {
    let history = compact_attach_history(history.to_vec());
    let latest_compaction =
        history
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, event)| match &event.kind {
                SessionEventKind::ContextCompacted {
                    compacted_through_sequence,
                    ..
                } => Some((index, *compacted_through_sequence)),
                _ => None,
            });

    let selected_events = if let Some((index, compacted_through_sequence)) = latest_compaction {
        std::iter::once(&history[index])
            .chain(
                history
                    .iter()
                    .enumerate()
                    .filter_map(|(event_index, event)| {
                        (event_index != index && event.sequence > compacted_through_sequence)
                            .then_some(event)
                    }),
            )
            .collect::<Vec<_>>()
    } else {
        history.iter().collect::<Vec<_>>()
    };
    session_events_to_sanitized_model_messages(&selected_events, tool_output_context_chars)
}

fn session_events_to_sanitized_model_messages(
    events: &[&bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
) -> Vec<ModelMessage> {
    let mut messages = Vec::new();
    let mut seen_tool_call_ids = BTreeSet::new();
    let mut pending_tool_call_ids = Vec::<String>::new();

    for event in events {
        match &event.kind {
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
                ..
            } => {
                if seen_tool_call_ids.contains(tool_call_id) {
                    append_missing_tool_results(&mut messages, &mut pending_tool_call_ids);
                    messages.push(plain_context_message(format!(
                        "Historical assistant tool call omitted from structured tool protocol because its call id was duplicated. Call id: {tool_call_id}; tool: {tool_name}; arguments: {}",
                        truncate_text(arguments_json, MAX_CONTEXT_FILE_CHARS),
                    )));
                    continue;
                }
                let Ok(arguments) = serde_json::from_str(arguments_json) else {
                    append_missing_tool_results(&mut messages, &mut pending_tool_call_ids);
                    messages.push(plain_context_message(format!(
                        "Historical assistant tool call omitted from structured tool protocol because its arguments were malformed or truncated. Call id: {tool_call_id}; tool: {tool_name}; raw arguments: {}",
                        truncate_text(arguments_json, MAX_CONTEXT_FILE_CHARS),
                    )));
                    continue;
                };
                messages.push(ModelMessage {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolCall {
                        call: bcode_model::ToolCall {
                            id: tool_call_id.clone(),
                            name: tool_name.clone(),
                            arguments,
                        },
                    }],
                });
                seen_tool_call_ids.insert(tool_call_id.clone());
                pending_tool_call_ids.push(tool_call_id.clone());
            }
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                ..
            } => {
                if pending_tool_call_ids
                    .iter()
                    .any(|pending| pending == tool_call_id)
                {
                    pending_tool_call_ids.retain(|pending| pending != tool_call_id);
                    messages.push(ModelMessage {
                        role: MessageRole::Tool,
                        content: vec![ContentBlock::ToolResult {
                            result: bcode_model::ToolResult {
                                call_id: tool_call_id.clone(),
                                output: project_tool_result_for_model_context(
                                    result,
                                    output.as_ref().map(trace_blob_read_path),
                                    tool_output_context_chars,
                                ),
                                is_error: *is_error,
                                content: tool_result_content_from_output(result),
                            },
                        }],
                    });
                } else {
                    append_missing_tool_results(&mut messages, &mut pending_tool_call_ids);
                    messages.push(plain_context_message(format!(
                        "Historical tool result omitted from structured tool protocol because its matching assistant tool call is unavailable. Call id: {tool_call_id}; error={is_error}; result: {}",
                        project_tool_result_for_model_context(
                            result,
                            output.as_ref().map(trace_blob_read_path),
                            tool_output_context_chars,
                        ),
                    )));
                }
            }
            _ => {
                append_missing_tool_results(&mut messages, &mut pending_tool_call_ids);
                if let Some(message) = non_tool_session_event_to_model_message(event) {
                    messages.push(message);
                }
            }
        }
    }

    append_missing_tool_results(&mut messages, &mut pending_tool_call_ids);
    messages
}

fn append_missing_tool_results(
    messages: &mut Vec<ModelMessage>,
    pending_tool_call_ids: &mut Vec<String>,
) {
    messages.extend(pending_tool_call_ids.drain(..).map(|call_id| ModelMessage {
        role: MessageRole::Tool,
        content: vec![ContentBlock::ToolResult {
            result: bcode_model::ToolResult {
                call_id,
                output: "tool invocation was interrupted before Bcode could persist a result"
                    .to_string(),
                is_error: true,
                content: Vec::new(),
            },
        }],
    }));
}

fn plain_context_message(text: String) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ContentBlock::Text { text }],
    }
}

fn non_tool_session_event_to_model_message(
    event: &bcode_session_models::SessionEvent,
) -> Option<ModelMessage> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => Some(ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text: text.clone() }],
        }),
        SessionEventKind::AssistantMessage { text } => Some(ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text { text: text.clone() }],
        }),
        SessionEventKind::SystemMessage { text } if system_message_is_model_context(text) => {
            Some(ModelMessage {
                role: MessageRole::System,
                content: vec![ContentBlock::Text { text: text.clone() }],
            })
        }
        SessionEventKind::WorkingDirectoryChanged {
            old_working_directory,
            new_working_directory,
        } => Some(ModelMessage {
            role: MessageRole::System,
            content: vec![ContentBlock::Text {
                text: working_directory_changed_message(
                    old_working_directory,
                    new_working_directory,
                ),
            }],
        }),
        SessionEventKind::ContextCompacted { summary, .. } => Some(ModelMessage {
            role: MessageRole::System,
            content: vec![ContentBlock::Text {
                text: format!("Previous conversation summary:\n{summary}"),
            }],
        }),
        _ => None,
    }
}

fn system_message_is_model_context(text: &str) -> bool {
    !text.starts_with("model error ")
        && !text.starts_with("model warning ")
        && !text.starts_with("model turn cancelled")
        && !text.starts_with("model provider polling ended")
        && !text.starts_with("auto compaction failed")
}

fn working_directory_changed_message(
    old_working_directory: &Path,
    new_working_directory: &Path,
) -> String {
    format!(
        "Working directory changed from `{}` to `{}`. Treat prior file/path assumptions as possibly stale unless reconfirmed.",
        old_working_directory.display(),
        new_working_directory.display()
    )
}

fn tool_result_content_from_output(output: &str) -> Vec<bcode_model::ToolResultContent> {
    let Some(marker_index) = output.find("\n\n[structured tool content attached]") else {
        return Vec::new();
    };
    let note = &output[marker_index..];
    note.lines()
        .filter_map(parse_image_tool_content_note)
        .map(|image| bcode_model::ToolResultContent::ImageRef { image })
        .collect()
}

fn parse_image_tool_content_note(line: &str) -> Option<ImageRefContent> {
    let line = line.strip_prefix("image ")?;
    let (_number, fields) = line.split_once(": ")?;
    let mut mime_type = None;
    let mut source_path = None;
    let mut width = None;
    let mut height = None;
    for field in fields.split_whitespace() {
        if let Some(value) = field.strip_prefix("mime=") {
            mime_type = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("path=") {
            source_path = Some(value.to_string());
        } else if let Some((raw_width, raw_height)) = field.split_once('x') {
            width = raw_width.parse::<u32>().ok();
            height = raw_height.parse::<u32>().ok();
        }
    }
    let source_path = source_path?;
    Some(ImageRefContent {
        path: source_path.clone(),
        mime_type: mime_type.unwrap_or_else(|| "image/png".to_string()),
        metadata: ModelImageMetadata {
            width,
            height,
            byte_len: None,
            source_path: Some(source_path),
        },
    })
}

async fn append_trace_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: Option<String>,
    phase: SessionTracePhase,
    payload: SessionTracePayload,
) {
    if !state.observability.enabled() {
        return;
    }
    let trace = SessionTraceEvent {
        timestamp_ms: current_time_ms(),
        turn_id,
        phase,
        payload,
    };
    match state.sessions.append_trace_event(session_id, trace).await {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append trace event: {error}"),
    }
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const fn side_effect_name(side_effect: bcode_tool::ToolSideEffect) -> &'static str {
    match side_effect {
        bcode_tool::ToolSideEffect::ReadOnly => "read_only",
        bcode_tool::ToolSideEffect::WriteFiles => "write_files",
        bcode_tool::ToolSideEffect::ExecuteProcess => "execute_process",
    }
}

const fn agent_decision_name(decision: AgentDecision) -> &'static str {
    match decision {
        AgentDecision::Allow => "allow",
        AgentDecision::Ask => "ask",
        AgentDecision::Deny => "deny",
    }
}

const fn prompt_cache_mode_name(mode: bcode_model::PromptCacheMode) -> &'static str {
    match mode {
        bcode_model::PromptCacheMode::Off => "off",
        bcode_model::PromptCacheMode::Auto => "auto",
        bcode_model::PromptCacheMode::Aggressive => "aggressive",
    }
}

const fn conversation_reuse_mode_name(mode: bcode_model::ConversationReuseMode) -> &'static str {
    match mode {
        bcode_model::ConversationReuseMode::Off => "off",
        bcode_model::ConversationReuseMode::Auto => "auto",
    }
}

const fn compaction_mode_name(mode: bcode_config::CompactionMode) -> &'static str {
    match mode {
        bcode_config::CompactionMode::Off => "off",
        bcode_config::CompactionMode::OnOverflow => "on_overflow",
        bcode_config::CompactionMode::Proactive => "proactive",
        bcode_config::CompactionMode::ProactiveAndOverflow => "proactive_and_overflow",
        bcode_config::CompactionMode::Auto => "auto",
    }
}

async fn append_assistant_message_event(state: &ServerState, session_id: SessionId, text: String) {
    match state
        .sessions
        .append_assistant_message(session_id, text)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append assistant message: {error}"),
    }
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

async fn append_tool_request_event(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: String,
    tool_name: String,
    arguments_json: String,
    request_presentation: Option<ToolRequestPresentationMetadata>,
) {
    let runtime_work_id = RuntimeWorkId::new(format!("tool_{tool_call_id}"));
    let runtime_label = tool_name.clone();
    let runtime_tool_call_id = tool_call_id.clone();
    match state
        .sessions
        .append_tool_call_requested(
            session_id,
            tool_call_id,
            tool_name,
            arguments_json,
            request_presentation,
        )
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append tool request: {error}"),
    }
    register_runtime_work(
        state,
        session_id,
        RuntimeWorkSpec::new(
            runtime_work_id,
            RuntimeWorkKind::Tool,
            runtime_label,
            CancellationHandle::SessionTurn(
                active_turn_cancel_state(state, session_id)
                    .await
                    .unwrap_or_else(|| Arc::new(TurnCancelState::default())),
            ),
        )
        .with_tool_call_id(Some(runtime_tool_call_id))
        .with_parent_work_id(
            state
                .session_current_turn(session_id)
                .await
                .map(|turn| RuntimeWorkId::new(format!("model_{}", turn.turn_id))),
        ),
    )
    .await;
}

async fn append_runtime_work_cancel_requested_event(
    state: &ServerState,
    session_id: SessionId,
    work_id: RuntimeWorkId,
    client_id: Option<ClientId>,
) {
    match state
        .sessions
        .append_runtime_work_cancel_requested(
            session_id,
            work_id,
            Some(current_unix_millis()),
            client_id,
        )
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append runtime work cancel request: {error}"),
    }
}

fn service_tool_result_to_session(result: ServiceToolInvocationResult) -> ToolInvocationResult {
    match result {
        ServiceToolInvocationResult::Text { text } => ToolInvocationResult::Text { text },
        ServiceToolInvocationResult::Json { value } => ToolInvocationResult::Json { value },
        ServiceToolInvocationResult::ShellRun { result } => ToolInvocationResult::ShellRun {
            result: service_shell_result_to_session(result),
        },
        ServiceToolInvocationResult::FileChange { result } => ToolInvocationResult::FileChange {
            result: bcode_session_models::FileChangeResult {
                tool_name: result.tool_name,
                summary: result.summary,
                path: result.path,
            },
        },
    }
}

fn service_shell_result_to_session(result: ServiceShellRunResult) -> ShellRunResult {
    match result {
        ServiceShellRunResult::Terminal {
            exit_code,
            timed_out,
            cancelled,
            duration_ms,
            output_tail,
            output_truncated,
            output_bytes,
            retained_output_bytes,
            columns,
            rows,
        } => ShellRunResult::Terminal {
            exit_code,
            timed_out,
            cancelled,
            duration_ms,
            output_tail,
            output_truncated,
            output_bytes,
            retained_output_bytes,
            columns: columns.max(1),
            rows: rows.max(1),
        },
        ServiceShellRunResult::Captured {
            exit_code,
            timed_out,
            cancelled,
            duration_ms,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            stdout_bytes,
            stderr_bytes,
        } => ShellRunResult::Captured {
            exit_code,
            timed_out,
            cancelled,
            duration_ms,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            stdout_bytes,
            stderr_bytes,
        },
    }
}

struct ToolFinishedEventInput {
    tool_call_id: String,
    result: String,
    is_error: bool,
    content: Vec<ToolResultContent>,
    output: Option<TraceBlobRef>,
    semantic_result: Option<ToolInvocationResult>,
}

async fn append_tool_finished_event(
    state: &ServerState,
    session_id: SessionId,
    input: ToolFinishedEventInput,
) {
    if let Err(error) = append_tool_finished_event_inner(state, session_id, input).await {
        eprintln!("failed to append tool result: {error}");
    }
}

async fn append_tool_finished_event_inner(
    state: &ServerState,
    session_id: SessionId,
    input: ToolFinishedEventInput,
) -> Result<bcode_session_models::SessionEvent, bcode_session::SessionError> {
    let ToolFinishedEventInput {
        tool_call_id,
        result,
        is_error,
        content,
        output,
        semantic_result,
    } = input;
    let runtime_work_id = RuntimeWorkId::new(format!("tool_{tool_call_id}"));
    let runtime_status = runtime_work_status_from_tool_result(&result, is_error);
    state
        .runtime_work
        .finish(session_id, &runtime_work_id)
        .await;
    let content_note = tool_result_content_model_note(&tool_call_id, &content);
    let event = state
        .sessions
        .append_tool_call_finished(
            session_id,
            tool_call_id,
            format!("{result}{content_note}"),
            is_error,
            output,
            semantic_result,
        )
        .await?;
    publish_session_event(state, &event).await;
    if let Ok(runtime_event) = state
        .sessions
        .append_runtime_work_finished(
            session_id,
            runtime_work_id,
            runtime_status,
            Some(current_unix_millis()),
            None,
        )
        .await
    {
        publish_session_event(state, &runtime_event).await;
    }
    Ok(event)
}

fn tool_result_content_model_note(tool_call_id: &str, content: &[ToolResultContent]) -> String {
    let images = content
        .iter()
        .filter_map(|item| match item {
            ToolResultContent::Image { image } => Some((
                image.mime_type.as_str(),
                &image.metadata,
                image.metadata.source_path.as_deref(),
            )),
            ToolResultContent::ImageRef { image } => Some((
                image.mime_type.as_str(),
                &image.metadata,
                Some(image.path.as_str()),
            )),
            ToolResultContent::Text { .. } => None,
        })
        .collect::<Vec<_>>();
    if images.is_empty() {
        return String::new();
    }
    let mut note = String::from("\n\n[structured tool content attached]");
    for (index, (mime_type, metadata, path)) in images.iter().enumerate() {
        let image_number = index + 1;
        let dimensions = metadata
            .width
            .zip(metadata.height)
            .map_or_else(String::new, |(width, height)| format!(" {width}x{height}"));
        let path = path.map_or_else(String::new, |path| format!(" path={path}"));
        let _ = write!(
            note,
            "\nimage {image_number}: call_id={tool_call_id} mime={mime_type}{dimensions}{path}"
        );
    }
    note
}

fn runtime_work_status_from_tool_result(result: &str, is_error: bool) -> RuntimeWorkStatus {
    if !is_error {
        return RuntimeWorkStatus::Completed;
    }
    let lower = result.to_ascii_lowercase();
    if lower.contains("cancelled") {
        RuntimeWorkStatus::Cancelled
    } else if lower.contains("timed_out: true") || lower.contains("\"timed_out\":true") {
        RuntimeWorkStatus::TimedOut
    } else {
        RuntimeWorkStatus::Failed
    }
}

async fn append_system_event(state: &ServerState, session_id: SessionId, text: String) {
    match state.sessions.append_system_message(session_id, text).await {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append system message: {error}"),
    }
}

async fn register_runtime_work(state: &ServerState, session_id: SessionId, spec: RuntimeWorkSpec) {
    let work_id = spec.work_id.clone();
    let kind = spec.kind;
    let label = spec.label.clone();
    let tool_call_id = spec.tool_call_id.clone();
    let plugin_id = spec.plugin_id.clone();
    let service_interface = spec.service_interface.clone();
    let operation = spec.operation.clone();
    let parent_work_id = spec.parent_work_id.clone();
    let cancellable = state.runtime_work.start(session_id, spec).await;
    match state
        .sessions
        .append_runtime_work_started(
            session_id,
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms: Some(current_unix_millis()),
                cancellable,
            },
        )
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append runtime work start: {error}"),
    }
}

async fn cancel_registered_runtime_work(
    state: &ServerState,
    session_id: SessionId,
    work_id: RuntimeWorkId,
    client_id: Option<ClientId>,
) -> bool {
    let cancelled_work_ids = state
        .runtime_work
        .cancel_with_children(session_id, &work_id)
        .await;
    if cancelled_work_ids.is_empty() {
        return false;
    }
    for cancelled_work_id in cancelled_work_ids {
        append_runtime_work_cancel_requested_event(state, session_id, cancelled_work_id, client_id)
            .await;
    }
    true
}

async fn finish_registered_runtime_work(
    state: &ServerState,
    session_id: SessionId,
    work_id: RuntimeWorkId,
    status: RuntimeWorkStatus,
    message: Option<String>,
) {
    state.runtime_work.finish(session_id, &work_id).await;
    append_runtime_work_finished_event(state, session_id, work_id, status, message).await;
    state.release_session_resources_if_idle(session_id).await;
}

async fn append_model_runtime_work_started_event(
    state: &ServerState,
    session_id: SessionId,
    work_id: RuntimeWorkId,
    turn_id: String,
    cancel_state: Arc<TurnCancelState>,
) {
    register_runtime_work(
        state,
        session_id,
        RuntimeWorkSpec::new(
            work_id,
            RuntimeWorkKind::ModelTurn,
            format!("model turn {turn_id}"),
            CancellationHandle::SessionTurn(cancel_state),
        ),
    )
    .await;
}

async fn append_model_turn_started_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: String,
) {
    match state
        .sessions
        .append_model_turn_started(session_id, turn_id)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append model turn start: {error}"),
    }
}

async fn append_runtime_work_progress_event(
    state: &ServerState,
    session_id: SessionId,
    work_id: RuntimeWorkId,
    message: String,
    completed_units: Option<u64>,
    total_units: Option<u64>,
) {
    match state
        .sessions
        .append_event(
            session_id,
            SessionEventKind::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms: Some(current_unix_millis()),
                completed_units,
                total_units,
            },
        )
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append runtime work progress: {error}"),
    }
}

async fn append_runtime_work_finished_event(
    state: &ServerState,
    session_id: SessionId,
    work_id: RuntimeWorkId,
    status: RuntimeWorkStatus,
    message: Option<String>,
) {
    match state
        .sessions
        .append_runtime_work_finished(
            session_id,
            work_id,
            status,
            Some(current_unix_millis()),
            message,
        )
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append runtime work finish: {error}"),
    }
}

const fn runtime_work_status_from_model_outcome(outcome: ModelTurnOutcome) -> RuntimeWorkStatus {
    match outcome {
        ModelTurnOutcome::Completed => RuntimeWorkStatus::Completed,
        ModelTurnOutcome::Cancelled => RuntimeWorkStatus::Cancelled,
        ModelTurnOutcome::IdleTimeout => RuntimeWorkStatus::TimedOut,
        ModelTurnOutcome::Error
        | ModelTurnOutcome::ToolRoundLimitReached
        | ModelTurnOutcome::ProviderUnavailable => RuntimeWorkStatus::Failed,
    }
}

async fn append_model_turn_cancel_requested_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: String,
    client_id: Option<ClientId>,
) {
    match state
        .sessions
        .append_model_turn_cancel_requested(
            session_id,
            turn_id,
            Some(current_unix_millis()),
            client_id,
        )
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append model turn cancel request: {error}"),
    }
}

async fn append_model_turn_finished_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: String,
    outcome: ModelTurnOutcome,
    message: Option<String>,
) {
    match state
        .sessions
        .append_model_turn_finished(session_id, turn_id, outcome, message)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append model turn finish: {error}"),
    }
}

async fn append_model_usage_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: String,
    usage: TokenUsage,
) {
    match state
        .sessions
        .append_model_usage(session_id, turn_id, session_token_usage(&usage))
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append model usage: {error}"),
    }
}

const fn session_token_usage(usage: &TokenUsage) -> SessionTokenUsage {
    SessionTokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    }
}

async fn handle_list_plugin_services(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let services = plugin_service_summaries(&state.plugins);
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::PluginServices { services }),
    )
    .await
}

async fn handle_list_plugin_contributions(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let contributions = PluginContributions {
        command_contributions: state
            .plugins
            .registered_command_contributions(&bcode_command::CommandSurface::Palette),
        commands: state.plugins.command_contributions(),
        config_extensions: state.plugins.config_extensions(),
    };
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::PluginContributions { contributions }),
    )
    .await
}

async fn handle_invoke_plugin_service(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    plugin_id: &str,
    interface_id: &str,
    operation: String,
    payload: Vec<u8>,
) -> Result<(), ServerError> {
    let plugin_id = plugin_id.to_string();
    let interface_id = interface_id.to_string();
    let response = state
        .plugins
        .invoke_service(&plugin_id, interface_id, operation, payload)
        .await;
    send_plugin_service_response(writer, request_id, response).await
}

async fn handle_call_plugin_service(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    interface_id: &str,
    operation: String,
    payload: Vec<u8>,
) -> Result<(), ServerError> {
    let interface_id = interface_id.to_string();
    let response = state
        .plugins
        .invoke_service_by_interface(&interface_id, operation, payload)
        .await;
    send_plugin_service_response(writer, request_id, response).await
}

async fn handle_publish_plugin_event(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    topic: &str,
    payload: &[u8],
) -> Result<(), ServerError> {
    let topic = topic.to_string();
    let payload = payload.to_vec();
    let response = state.plugins.publish_event(&topic, &payload).await;
    match response {
        Ok(delivered) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::PluginEventPublished { delivered }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("plugin_error", error.to_string())),
            )
            .await
        }
    }
}

async fn send_plugin_service_response(
    writer: &SharedWriter,
    request_id: u64,
    response: Result<bcode_plugin::ServiceResponse, bcode_plugin::PluginLoadError>,
) -> Result<(), ServerError> {
    let response = match response {
        Ok(response) => Response::Ok(ResponsePayload::PluginServiceResult {
            response: PluginServiceResponse {
                payload: response.payload,
                error: response.error.map(|error| PluginServiceError {
                    code: error.code,
                    message: error.message,
                }),
            },
        }),
        Err(error) => Response::Err(ErrorResponse::new("plugin_error", error.to_string())),
    };
    send_response(writer, request_id, response).await
}

fn plugin_service_summaries(
    plugins: &bcode_plugin::PluginRuntimeHost,
) -> Vec<PluginServiceSummary> {
    plugins
        .service_summaries()
        .into_iter()
        .map(|(plugin_id, service)| PluginServiceSummary {
            plugin_id,
            interface_id: service.interface_id,
            name: service.name,
            description: service.description,
        })
        .collect()
}

async fn publish_session_event(state: &ServerState, event: &bcode_session_models::SessionEvent) {
    state
        .metrics
        .record_event("session.event", 1, session_event_metric_labels(event));
    let payload = match serde_json::to_vec(event) {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("failed to encode plugin session event: {error}");
            return;
        }
    };
    let response = state
        .plugins
        .publish_event(SESSION_EVENT_PLUGIN_TOPIC, &payload)
        .await;
    match response {
        Ok(_) => {}
        Err(error) => eprintln!("failed to publish plugin session event: {error}"),
    }
}

fn session_event_metric_labels(event: &bcode_session_models::SessionEvent) -> MetricLabels {
    let mut labels = MetricLabels::new();
    labels.insert("session_id".to_owned(), event.session_id.to_string());
    labels.insert(
        "event_type".to_owned(),
        session_event_kind_name(&event.kind).to_owned(),
    );
    labels.insert("sequence".to_owned(), event.sequence.to_string());
    labels
}

const fn session_event_kind_name(kind: &SessionEventKind) -> &'static str {
    match kind {
        SessionEventKind::SessionCreated { .. } => "session_created",
        SessionEventKind::ClientAttached { .. } => "client_attached",
        SessionEventKind::ClientDetached { .. } => "client_detached",
        SessionEventKind::UserMessage { .. } => "user_message",
        SessionEventKind::AssistantDelta { .. } => "assistant_delta",
        SessionEventKind::AssistantMessage { .. } => "assistant_message",
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
        SessionEventKind::ToolCallFinished { .. } => "tool_call_finished",
        SessionEventKind::PermissionRequested { .. } => "permission_requested",
        SessionEventKind::PermissionResolved { .. } => "permission_resolved",
        SessionEventKind::ModelChanged { .. } => "model_changed",
        SessionEventKind::ReasoningChanged { .. } => "reasoning_changed",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::AgentChanged { .. } => "agent_changed",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
        SessionEventKind::ModelTurnFinished { .. } => "model_turn_finished",
        SessionEventKind::ModelUsage { .. } => "model_usage",
        SessionEventKind::ContextCompacted { .. } => "context_compacted",
        SessionEventKind::SessionRenamed { .. } => "session_renamed",
        SessionEventKind::TraceEvent { .. } => "trace_event",
        SessionEventKind::SkillInvoked { .. } => "skill_invoked",
        SessionEventKind::SkillSuggested { .. } => "skill_suggested",
        SessionEventKind::SkillActivated { .. } => "skill_activated",
        SessionEventKind::SkillDeactivated { .. } => "skill_deactivated",
        SessionEventKind::SkillContextLoaded { .. } => "skill_context_loaded",
        SessionEventKind::SkillInvocationFailed { .. } => "skill_invocation_failed",
        SessionEventKind::AssistantReasoningDelta { .. } => "assistant_reasoning_delta",
        SessionEventKind::AssistantReasoningMessage { .. } => "assistant_reasoning_message",
        SessionEventKind::RuntimeWorkStarted { .. } => "runtime_work_started",
        SessionEventKind::RuntimeWorkCancelRequested { .. } => "runtime_work_cancel_requested",
        SessionEventKind::RuntimeWorkFinished { .. } => "runtime_work_finished",
        SessionEventKind::RuntimeWorkProgress { .. } => "runtime_work_progress",
        SessionEventKind::ModelTurnCancelRequested { .. } => "model_turn_cancel_requested",
        SessionEventKind::ToolInvocationStream { .. } => "tool_invocation_stream",
        SessionEventKind::WorkingDirectoryChanged { .. } => "working_directory_changed",
        SessionEventKind::SessionImported { .. } => "session_imported",
        SessionEventKind::SessionForked { .. } => "session_forked",
        SessionEventKind::RalphLifecycle { .. } => "ralph_lifecycle",
    }
}

async fn broadcast_catalog_update(state: &ServerState, revision: u64) {
    let event = Event::SessionCatalogUpdated { revision };
    let mut disconnected_clients = Vec::new();
    let mut send_tasks = JoinSet::new();
    for sink in state.catalog_event_sinks().await {
        let event = event.clone();
        send_tasks.spawn(async move {
            let client_id = sink.client_id();
            (client_id, sink.send(event).await)
        });
        if send_tasks.len() >= CATALOG_EVENT_BROADCAST_BATCH_SIZE {
            collect_catalog_send_result(
                state,
                revision,
                &mut send_tasks,
                &mut disconnected_clients,
            )
            .await;
        }
    }
    while !send_tasks.is_empty() {
        collect_catalog_send_result(state, revision, &mut send_tasks, &mut disconnected_clients)
            .await;
    }
    state
        .unregister_catalog_event_clients(&disconnected_clients)
        .await;
}

async fn collect_catalog_send_result(
    state: &ServerState,
    revision: u64,
    send_tasks: &mut JoinSet<(ClientId, Result<(), CodecError>)>,
    disconnected_clients: &mut Vec<ClientId>,
) {
    let Some(result) = send_tasks.join_next().await else {
        return;
    };
    let Ok((client_id, send_result)) = result else {
        return;
    };
    if let Err(error) = send_result {
        disconnected_clients.push(client_id);
        if !is_expected_disconnect(&error) {
            eprintln!("failed to send catalog update event to {client_id}: {error}");
        }
    } else {
        state.mark_catalog_event_sent(client_id, revision).await;
    }
}

fn is_expected_disconnect(error: &CodecError) -> bool {
    matches!(
        error,
        CodecError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::TimedOut
            )
    )
}

fn forward_session_events(
    sink: ClientEventSink,
    mut events: tokio::sync::broadcast::Receiver<bcode_session_models::SessionEvent>,
    mut live_events: tokio::sync::broadcast::Receiver<bcode_session_models::SessionLiveEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                durable = events.recv() => match durable {
                    Ok(event) => Event::Session(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                live = live_events.recv() => match live {
                    Ok(event) => Event::SessionLive(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            };
            if let Err(error) = sink.send(event).await {
                if !is_expected_disconnect(&error) {
                    eprintln!(
                        "failed to send session event to {}: {error}",
                        sink.client_id()
                    );
                }
                break;
            }
        }
    })
}

fn forward_runtime_work_events(
    sink: ClientEventSink,
    mut events: tokio::sync::broadcast::Receiver<bcode_session_models::SessionEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if !matches!(
                event.kind,
                SessionEventKind::RuntimeWorkStarted { .. }
                    | SessionEventKind::RuntimeWorkCancelRequested { .. }
                    | SessionEventKind::RuntimeWorkProgress { .. }
                    | SessionEventKind::RuntimeWorkFinished { .. }
            ) {
                continue;
            }
            if let Err(error) = sink.send(Event::RuntimeWork(event)).await {
                if !is_expected_disconnect(&error) {
                    eprintln!(
                        "failed to send runtime work event to {}: {error}",
                        sink.client_id()
                    );
                }
                break;
            }
        }
    })
}

fn compact_attach_history(
    history: Vec<bcode_session_models::SessionEvent>,
) -> Vec<bcode_session_models::SessionEvent> {
    let mut compacted = Vec::with_capacity(history.len());
    let mut pending_assistant_deltas = Vec::new();

    for event in history {
        match event.kind {
            SessionEventKind::AssistantReasoningDelta { .. } => {
                pending_assistant_deltas.push(event);
            }
            SessionEventKind::AssistantDelta { .. } => pending_assistant_deltas.push(event),
            SessionEventKind::AssistantMessage { .. } => {
                pending_assistant_deltas.clear();
                compacted.push(event);
            }
            _ => {
                compacted.append(&mut pending_assistant_deltas);
                compacted.push(event);
            }
        }
    }

    compacted.append(&mut pending_assistant_deltas);
    compacted
}

pub(crate) async fn send_response(
    writer: &SharedWriter,
    request_id: u64,
    response: Response,
) -> Result<(), ServerError> {
    let envelope = response_envelope(request_id, &response)?;
    let mut writer = writer.lock().await;
    send_envelope(&mut *writer, &envelope).await?;
    drop(writer);
    Ok(())
}

fn build_skill_registry(config: &bcode_config::BcodeConfig) -> Option<SkillRegistry> {
    let mut roots = Vec::new();
    if !config.skills.enabled {
        return None;
    }
    if config.skills.include_repo_skills {
        roots.push(SkillSourceRoot::new(
            PathBuf::from(".bcode/skills"),
            SkillSourceKind::Repository,
            "repo:.bcode/skills",
            10,
        ));
    }
    if config.skills.include_generic_repo_skills {
        roots.push(SkillSourceRoot::new(
            PathBuf::from("skills"),
            SkillSourceKind::Repository,
            "repo:skills",
            15,
        ));
    }
    if config.skills.include_compat_claude_skills {
        roots.push(SkillSourceRoot::new(
            PathBuf::from(".claude/skills"),
            SkillSourceKind::Compatibility,
            "repo:.claude/skills",
            20,
        ));
    }
    if config.skills.include_user_skills {
        roots.push(SkillSourceRoot::new(
            bcode_config::default_config_dir().join("skills"),
            SkillSourceKind::User,
            "user-config:skills",
            30,
        ));
        roots.push(SkillSourceRoot::new(
            bcode_config::default_state_dir().join("skills"),
            SkillSourceKind::User,
            "user-state:skills",
            35,
        ));
    }
    for (index, path) in config.skills.sources.paths.iter().enumerate() {
        roots.push(SkillSourceRoot::new(
            path.clone(),
            SkillSourceKind::Configured,
            format!("configured:{index}"),
            40 + u16::try_from(index).unwrap_or(u16::MAX - 40),
        ));
    }
    let options = SkillRegistryOptions {
        max_skill_file_bytes: config.skills.max_skill_file_bytes,
        max_context_bytes: config.skills.max_context_bytes,
        follow_symlinks: config.skills.follow_symlinks,
        disabled_ids: config.skills.disabled_skill_ids(),
    };
    match SkillRegistry::discover(&roots, options) {
        Ok(mut registry) => {
            add_plugin_skills(&mut registry);
            Some(registry)
        }
        Err(error) => {
            eprintln!("failed to build skill registry: {error}");
            None
        }
    }
}

const fn skill_prompt_options_from_config(
    config: &bcode_config::SkillPromptConfig,
) -> SkillPromptCatalogOptions {
    SkillPromptCatalogOptions {
        mode: match config.catalog {
            bcode_config::SkillPromptCatalogMode::Off => SkillPromptCatalogMode::Off,
            bcode_config::SkillPromptCatalogMode::NamesOnly => SkillPromptCatalogMode::NamesOnly,
            bcode_config::SkillPromptCatalogMode::Summary => SkillPromptCatalogMode::Summary,
        },
        max_bytes: config.max_bytes,
        max_description_chars: config.max_description_chars,
        include_sources: config.include_sources,
        include_keywords: config.include_keywords,
    }
}

fn rebuild_active_skills_from_history(
    events: &[bcode_session_models::SessionEvent],
) -> BTreeSet<SkillId> {
    let mut active = BTreeSet::new();
    for event in events {
        match &event.kind {
            SessionEventKind::SkillActivated { skill_id, .. } => {
                active.insert(skill_id.clone());
            }
            SessionEventKind::SkillDeactivated { skill_id, .. } => {
                active.remove(skill_id);
            }
            _ => {}
        }
    }
    active
}

async fn restore_active_skills_from_history(
    events: &[bcode_session_models::SessionEvent],
    state: &ServerState,
    session_id: SessionId,
) {
    let active = rebuild_active_skills_from_history(events);
    if !active.is_empty() {
        state.active_skills.lock().await.insert(session_id, active);
    }
}

const fn add_plugin_skills(_registry: &mut SkillRegistry) {
    // Plugin-provided skills use the same `bcode.skill/v1` contract types as folder skills.
    // Provider invocation routing is intentionally kept out of the folder registry; once a
    // bundled/external plugin declares the interface, server handlers can merge `list` results
    // here and delegate `describe`/`context` to that provider by source/plugin ID.
}

async fn suggest_skills_for_prompt(
    state: &ServerState,
    session_id: SessionId,
    user_event: &bcode_session_models::SessionEvent,
) {
    let SessionEventKind::UserMessage { text, .. } = &user_event.kind else {
        return;
    };
    let Some(registry) = &state.skills else {
        return;
    };
    let lower_text = text.to_lowercase();
    let active = state
        .active_skills
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .unwrap_or_default();
    for skill in registry.list().skills {
        if active.contains(&skill.id) {
            continue;
        }
        let Some(keyword) = skill
            .activation
            .keywords
            .iter()
            .find(|keyword| lower_text.contains(&keyword.to_lowercase()))
        else {
            continue;
        };
        let event = state
            .sessions
            .append_event(
                session_id,
                SessionEventKind::SkillSuggested {
                    skill_id: skill.id,
                    reason: Some(format!("matched keyword '{keyword}'")),
                    suggested_at_ms: current_time_ms(),
                },
            )
            .await;
        if let Ok(event) = event {
            publish_session_event(state, &event).await;
        }
    }
}

const MAX_SKILL_CONTEXT_PREVIEW_CHARS: usize = 2_000;

fn skill_context_preview(context: &str) -> String {
    truncate_text(context, MAX_SKILL_CONTEXT_PREVIEW_CHARS)
}

async fn turn_skill_contexts(
    state: &ServerState,
    session_id: SessionId,
    trigger_sequence: u64,
) -> Vec<SkillContextResponse> {
    let Some(registry) = &state.skills else {
        return Vec::new();
    };
    let Some(invocation) = state
        .turn_skills
        .lock()
        .await
        .remove(&(session_id, trigger_sequence))
    else {
        return Vec::new();
    };
    let skill_id = invocation.skill_id;
    let Some(summary) = registry.summary(&skill_id) else {
        return Vec::new();
    };
    let context = match registry.context(&skill_id, Some(state.skill_context_bytes)) {
        Ok(context) => context,
        Err(error) => {
            let _ = state
                .sessions
                .append_event(
                    session_id,
                    SessionEventKind::SkillInvocationFailed {
                        skill_id,
                        error: error.to_string(),
                        failed_at_ms: current_time_ms(),
                    },
                )
                .await;
            return Vec::new();
        }
    };
    let mut context = context;
    let arguments = if invocation.arguments.trim().is_empty() {
        "<empty>".to_string()
    } else {
        invocation.arguments
    };
    write!(context, "\n\nSkill invocation arguments:\n{arguments}").expect("write to string");
    let bytes_loaded = context.len();
    let truncated = bytes_loaded >= state.skill_context_bytes;
    vec![SkillContextResponse {
        skill_id,
        context,
        source: summary.source.clone(),
        bytes_loaded,
        truncated,
    }]
}

async fn active_skill_contexts(
    state: &ServerState,
    session_id: SessionId,
) -> Vec<SkillContextResponse> {
    let Some(registry) = &state.skills else {
        return Vec::new();
    };
    let skill_ids = state
        .active_skills
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .unwrap_or_default();
    let per_skill_budget = state
        .skill_context_bytes
        .checked_div(skill_ids.len().max(1));
    let mut contexts = Vec::new();
    for skill_id in skill_ids {
        let Some(summary) = registry.summary(&skill_id) else {
            continue;
        };
        let context = match registry.context(&skill_id, per_skill_budget) {
            Ok(context) => context,
            Err(error) => {
                let _ = state
                    .sessions
                    .append_event(
                        session_id,
                        SessionEventKind::SkillInvocationFailed {
                            skill_id,
                            error: error.to_string(),
                            failed_at_ms: current_time_ms(),
                        },
                    )
                    .await;
                continue;
            }
        };
        let bytes_loaded = context.len();
        let truncated = per_skill_budget.is_some_and(|budget| bytes_loaded >= budget);
        contexts.push(SkillContextResponse {
            skill_id,
            context,
            source: summary.source.clone(),
            bytes_loaded,
            truncated,
        });
    }
    contexts
}

fn default_session_store_dir() -> PathBuf {
    bcode_config::default_state_dir().join("sessions")
}

fn default_provider_state_path() -> PathBuf {
    bcode_config::default_state_dir()
        .join("provider-state")
        .join(format!(
            "{}.json",
            safe_state_namespace(bcode_ipc::BUILD_FINGERPRINT)
        ))
}

fn safe_state_namespace(value: &str) -> String {
    let namespace = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if namespace.is_empty() {
        "unknown".to_string()
    } else {
        namespace
    }
}

fn default_trace_store_dir() -> PathBuf {
    bcode_config::default_state_dir().join("traces")
}

fn default_session_artifact_dir(session_id: SessionId) -> PathBuf {
    bcode_config::default_state_dir()
        .join("artifacts")
        .join("sessions")
        .join(session_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent};

    fn session_event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind,
        }
    }

    #[test]
    fn skill_tool_decision_key_uses_sorted_skills_and_git_root_workspace() {
        let temp = tempfile::tempdir().expect("temp dir");
        shell_git(&["init"], temp.path());
        let nested = temp.path().join("src");
        std::fs::create_dir_all(&nested).expect("nested dir");
        let skill_ids = BTreeSet::from([SkillId::new("zeta"), SkillId::new("alpha")]);

        let key = skill_tool_decision_key_for_working_directory(
            skill_ids.clone(),
            "filesystem_read",
            &nested,
        );

        assert_eq!(key.skill_ids, skill_ids);
        assert_eq!(key.tool_name, "filesystem_read");
        let SkillToolDecisionScope::Workspace { workspace_id } = key.scope else {
            panic!("expected workspace-scoped decision");
        };
        assert_eq!(
            PathBuf::from(workspace_id)
                .canonicalize()
                .expect("workspace"),
            temp.path().canonicalize().expect("repo")
        );
    }

    fn shell_git(args: &[&str], cwd: &Path) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn aggressive_prompt_cache_adds_conversation_cache_point() {
        let mut messages = (0..6)
            .map(|index| ModelMessage {
                role: if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: vec![ContentBlock::Text {
                    text: format!("message {index}"),
                }],
            })
            .collect::<Vec<_>>();

        let hints = plan_prompt_cache(&mut messages, bcode_model::PromptCacheMode::Aggressive);

        assert_eq!(hints.mode, bcode_model::PromptCacheMode::Aggressive);
        assert_eq!(prompt_cache_point_count_in_messages(&messages), 1);
    }

    #[test]
    fn auto_prompt_cache_does_not_add_conversation_cache_point() {
        let mut messages = (0..6)
            .map(|index| ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: format!("message {index}"),
                }],
            })
            .collect::<Vec<_>>();

        let hints = plan_prompt_cache(&mut messages, bcode_model::PromptCacheMode::Auto);

        assert_eq!(hints.mode, bcode_model::PromptCacheMode::Auto);
        assert_eq!(prompt_cache_point_count_in_messages(&messages), 0);
    }

    #[test]
    fn sent_message_count_skips_only_when_reusing_provider_response() {
        let messages = vec![
            test_model_message(MessageRole::User, "one"),
            test_model_message(MessageRole::Assistant, "two"),
            test_model_message(MessageRole::User, "three"),
        ];
        let mut request = test_model_turn_request(messages);
        request.conversation_reuse.new_messages_start_index = Some(2);
        assert_eq!(sent_message_count(&request), 3);

        request.conversation_reuse.previous_provider_response_id = Some("resp_1".to_string());
        assert_eq!(sent_message_count(&request), 1);
    }

    fn test_model_message(role: MessageRole, text: &str) -> ModelMessage {
        ModelMessage {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn test_model_turn_request(messages: Vec<ModelMessage>) -> ModelTurnRequest {
        ModelTurnRequest {
            session_id: SessionId::new(),
            turn_id: "turn-test".to_string(),
            model_id: "model-test".to_string(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: None,
            messages,
            tools: Vec::new(),
            parameters: ModelParameters::default(),
            prompt_cache: bcode_model::PromptCacheHints::default(),
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: BTreeMap::new(),
        }
    }

    fn prompt_cache_point_count_in_messages(messages: &[ModelMessage]) -> usize {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|block| matches!(block, ContentBlock::CachePoint { .. }))
            .count()
    }

    #[test]
    fn semantic_tool_response_carries_only_semantic_result() {
        let response = ToolInvocationResponse {
            output: "legacy".to_owned(),
            is_error: false,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: Some(ServiceToolInvocationResult::ShellRun {
                result: ServiceShellRunResult::Terminal {
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    duration_ms: None,
                    output_tail: "terminal".to_owned(),
                    output_truncated: false,
                    output_bytes: Some(8),
                    retained_output_bytes: Some(8),
                    columns: 80,
                    rows: 24,
                },
            }),
        };

        assert!(matches!(
            response.result,
            Some(ServiceToolInvocationResult::ShellRun { .. })
        ));
    }

    fn test_working_directory() -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    fn test_tool_call() -> bcode_model::ToolCall {
        bcode_model::ToolCall {
            id: "call-test".to_string(),
            name: "filesystem.read".to_string(),
            arguments: serde_json::json!({ "path": "Cargo.toml" }),
        }
    }

    struct TestRalphStore {
        _temp: tempfile::TempDir,
        store: bcode_ralph::RalphStateStore,
    }

    fn test_ralph_store() -> TestRalphStore {
        let temp = tempfile::tempdir().expect("temp Ralph store should create");
        let store = bcode_ralph::RalphStateStore::from_ralph_state_root(temp.path().join("ralph"));
        TestRalphStore { _temp: temp, store }
    }

    fn unique_ralph_repo_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bcode-server-ralph-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("test repo root should be created");
        path
    }

    fn create_test_ralph_loop(
        store: &bcode_ralph::RalphStateStore,
        label: &str,
    ) -> (PathBuf, bcode_ralph::RalphLoopSummary) {
        let repo_root = unique_ralph_repo_root(label);
        store
            .create_initial_loop_state("test-loop", &repo_root, Some("test"))
            .expect("Ralph loop should be created");
        let summary = store
            .latest_loop(&repo_root)
            .expect("latest loop should query")
            .expect("latest loop should exist");
        (repo_root, summary)
    }

    #[tokio::test]
    async fn ralph_start_rejects_duplicate_active_run() {
        let ralph = test_ralph_store();
        let sessions = SessionManager::default();
        let state = Arc::new(test_server_state_with_ralph_store(
            sessions,
            ralph.store.clone(),
        ));
        let (repo_root, _summary) = create_test_ralph_loop(&ralph.store, "duplicate");
        let request = RalphRunRequest {
            repo_root: repo_root.clone(),
            loop_state_dir: None,
            max_iterations: Some(1),
            no_progress_limit: Some(1),
            require_approval: true,
        };

        start_ralph_runner(&state, request.clone(), None)
            .await
            .expect("first run should prepare");
        let error = start_ralph_runner(&state, request, None)
            .await
            .expect_err("second active run should be rejected");

        assert!(error.contains("already has an active run"));
    }

    #[tokio::test]
    async fn ralph_cancel_marks_active_run_cancel_requested() {
        let ralph = test_ralph_store();
        let sessions = SessionManager::default();
        let state = Arc::new(test_server_state_with_ralph_store(
            sessions,
            ralph.store.clone(),
        ));
        let (repo_root, summary) = create_test_ralph_loop(&ralph.store, "cancel");
        let response = start_ralph_runner(
            &state,
            RalphRunRequest {
                repo_root,
                loop_state_dir: None,
                max_iterations: Some(1),
                no_progress_limit: Some(1),
                require_approval: true,
            },
            None,
        )
        .await
        .expect("run should prepare");

        ralph
            .store
            .request_run_cancel(&response.run.run_id)
            .expect("cancel should persist");
        let active = ralph
            .store
            .active_run_for_loop(&summary.state_dir)
            .expect("active run query should work")
            .expect("run should remain active while awaiting runner observation");

        assert!(active.cancel_requested);
    }

    #[test]
    fn ralph_run_summary_reports_active_runtime_work_identity() {
        let ralph = test_ralph_store();
        let state_dir = PathBuf::from(format!(
            "/tmp/bcode-server-ralph-summary-{}",
            uuid::Uuid::new_v4()
        ));
        let run = ralph
            .store
            .create_run(bcode_ralph::RalphRunCreateRequest {
                state_dir,
                session_id: Some("session-test".to_owned()),
                status: "running".to_owned(),
                requested_max_iterations: Some(3),
                requested_no_progress_limit: Some(2),
            })
            .expect("run should persist");

        let summary = ralph_run_summary(run.clone());

        assert_eq!(summary.run_id, run.run_id);
        assert_eq!(summary.session_id.as_deref(), Some("session-test"));
        let expected_runtime_work_id = format!("ralph:{}", run.run_id);
        assert_eq!(
            summary.runtime_work_id.as_deref(),
            Some(expected_runtime_work_id.as_str())
        );
        assert_eq!(summary.status, "running");
    }

    #[test]
    fn ralph_validation_failure_completion_blocks_run() {
        let ralph = test_ralph_store();
        let state_dir = PathBuf::from(format!(
            "/tmp/bcode-server-ralph-validation-{}",
            uuid::Uuid::new_v4()
        ));
        let run = ralph
            .store
            .create_run(bcode_ralph::RalphRunCreateRequest {
                state_dir: state_dir.clone(),
                session_id: None,
                status: "running".to_owned(),
                requested_max_iterations: Some(1),
                requested_no_progress_limit: Some(1),
            })
            .expect("run should persist");

        let _ = ralph.store.update_run_status(
            &run.run_id,
            "blocked",
            Some(current_time_ms()),
            Some("validation_failed"),
            Some("Ralph validation command failed"),
        );
        let active = ralph
            .store
            .active_run_for_loop(&state_dir)
            .expect("active run query should work");

        assert!(active.is_none());
    }

    #[test]
    fn ralph_permission_denial_completion_stops_run() {
        let ralph = test_ralph_store();
        let state_dir = PathBuf::from(format!(
            "/tmp/bcode-server-ralph-permission-{}",
            uuid::Uuid::new_v4()
        ));
        let run = ralph
            .store
            .create_run(bcode_ralph::RalphRunCreateRequest {
                state_dir: state_dir.clone(),
                session_id: None,
                status: "running".to_owned(),
                requested_max_iterations: Some(1),
                requested_no_progress_limit: Some(1),
            })
            .expect("run should persist");
        let failure = ralph_run_failure_from_model_completion(
            "work",
            &ModelTurnCompletion::with_message(ModelTurnOutcome::Error, "permission denied"),
        )
        .expect("permission denial should classify");

        let _ = ralph.store.update_run_status(
            &run.run_id,
            failure.0,
            Some(current_time_ms()),
            Some(failure.1),
            Some(failure.2.as_str()),
        );
        let active = ralph
            .store
            .active_run_for_loop(&state_dir)
            .expect("active run query should work");

        assert!(active.is_none());
        assert_eq!(failure.0, "stopped");
        assert_eq!(failure.1, "permission_denied");
    }

    #[tokio::test]
    async fn ralph_runtime_work_events_are_appended_to_session_history() {
        let sessions = SessionManager::default();
        let summary = sessions
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        let session_id = summary.id;
        let _attachment = sessions
            .attach_session(session_id, ClientId::new())
            .await
            .expect("session should attach");
        let state = test_server_state(sessions);
        let work_id = RuntimeWorkId::new("ralph:test-run");

        register_ralph_runtime_work(
            &state,
            Some(session_id),
            work_id.clone(),
            "Ralph loop: test".to_owned(),
            "test-run".to_owned(),
            None,
        )
        .await;
        finish_ralph_runtime_work(
            &state,
            Some(session_id),
            work_id.clone(),
            RuntimeWorkStatus::Completed,
            Some("done".to_owned()),
        )
        .await;

        let history = state
            .sessions
            .session_history(session_id)
            .await
            .expect("history should read");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::RuntimeWorkStarted { work_id: id, label, .. }
                if id == &work_id && label == "Ralph loop: test"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::RuntimeWorkFinished { work_id: id, status, message, .. }
                if id == &work_id
                    && *status == RuntimeWorkStatus::Completed
                    && message.as_deref() == Some("done")
        )));
    }

    #[tokio::test]
    async fn ralph_session_lifecycle_markers_are_appended() {
        let sessions = SessionManager::default();
        let summary = sessions
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        let session_id = summary.id;
        let state = test_server_state(sessions);
        let state_dir = PathBuf::from("/tmp/bcode-server-ralph-lifecycle");
        let session_id_text = session_id.to_string();

        append_ralph_session_lifecycle(
            &state,
            Some(session_id_text.as_str()),
            "test-loop".to_owned(),
            state_dir.clone(),
            "run_finished",
            "Ralph autonomous runner completed",
        )
        .await;

        let history = state
            .sessions
            .session_history(session_id)
            .await
            .expect("history should read");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::RalphLifecycle {
                loop_name,
                state_dir: event_state_dir,
                kind,
                message,
                ..
            } if loop_name == "test-loop"
                && event_state_dir == &state_dir
                && kind == "run_finished"
                && message == "Ralph autonomous runner completed"
        )));
    }

    #[tokio::test]
    async fn ralph_successful_work_iteration_reaches_audit_prompt() {
        let ralph = test_ralph_store();
        let sessions = SessionManager::default();
        let state = Arc::new(test_server_state_with_ralph_store(
            sessions,
            ralph.store.clone(),
        ));
        let (repo_root, summary) = create_test_ralph_loop(&ralph.store, "audit");
        let run = ralph
            .store
            .create_run(bcode_ralph::RalphRunCreateRequest {
                state_dir: summary.state_dir.clone(),
                session_id: None,
                status: "running".to_owned(),
                requested_max_iterations: Some(1),
                requested_no_progress_limit: Some(1),
            })
            .expect("run should persist");
        let work_prompt = build_ralph_work_prompt(&summary);
        let iteration = ralph
            .store
            .create_iteration(bcode_ralph::RalphIterationCreateRequest {
                run_id: run.run_id.clone(),
                state_dir: summary.state_dir.clone(),
                iteration_number: 1,
                status: "work_completed".to_owned(),
                checklist_fingerprint_before: None,
                checklist_fingerprint_after: None,
                work_prompt: Some(work_prompt),
                finished_at_ms: Some(current_time_ms()),
                stop_reason: None,
                error_message: None,
            })
            .expect("iteration should persist");

        let completion = submit_ralph_audit_after_validation(
            &state,
            None,
            &RuntimeWorkId::new("ralph:test-run"),
            &summary,
            &run,
            Some(&iteration),
            None,
        )
        .await;
        let refreshed = ralph
            .store
            .list_iterations_for_run(&run.run_id)
            .expect("iterations should list")
            .into_iter()
            .find(|record| record.iteration_id == iteration.iteration_id)
            .expect("iteration should exist");

        assert!(completion.is_none());
        assert!(refreshed.audit_prompt.is_some());
        assert!(
            refreshed
                .audit_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("audit")),
            "audit prompt should be persisted for post-work review"
        );
        let _ = std::fs::remove_dir_all(repo_root);
    }

    fn ralph_iteration_with_fingerprints(
        iteration_number: u64,
        before: Option<String>,
        after: Option<String>,
    ) -> bcode_ralph::RalphIterationRecord {
        bcode_ralph::RalphIterationRecord {
            iteration_id: format!("iteration-{iteration_number}"),
            run_id: "run-test".to_owned(),
            state_dir: PathBuf::from("/tmp/bcode-ralph-server-test"),
            iteration_number,
            status: "work_completed".to_owned(),
            checklist_fingerprint_before: before,
            checklist_fingerprint_after: after,
            work_prompt: None,
            audit_prompt: None,
            replan_prompt: None,
            validation_status: None,
            validation_summary: None,
            started_at_ms: iteration_number,
            finished_at_ms: Some(iteration_number),
            stop_reason: None,
            error_message: None,
        }
    }

    #[test]
    fn ralph_run_limits_default_from_loop_summary() {
        assert_eq!(
            effective_ralph_run_limits(7, 3, None, None),
            (Some(7), Some(3))
        );
        assert_eq!(
            effective_ralph_run_limits(7, 3, Some(2), Some(1)),
            (Some(2), Some(1))
        );
    }

    #[test]
    fn ralph_terminal_mapping_keeps_continue_running() {
        assert_eq!(
            ralph_run_terminal_from_decision(bcode_ralph::RalphStopDecision::Continue),
            (
                "running",
                "continue",
                "Ralph iteration completed and loop will continue"
            )
        );
        assert_eq!(
            ralph_run_terminal_from_decision(bcode_ralph::RalphStopDecision::MaxIterations).0,
            "stopped"
        );
        assert_eq!(
            ralph_run_terminal_from_decision(bcode_ralph::RalphStopDecision::CompletionCandidate).0,
            "done"
        );
    }

    #[test]
    fn ralph_no_progress_counts_trailing_noop_iterations() {
        let iterations = vec![
            ralph_iteration_with_fingerprints(1, Some("a".to_owned()), Some("b".to_owned())),
            ralph_iteration_with_fingerprints(2, Some("b".to_owned()), Some("b".to_owned())),
            ralph_iteration_with_fingerprints(3, Some("b".to_owned()), Some("b".to_owned())),
        ];
        assert_eq!(
            consecutive_no_progress_iterations(&iterations, &iterations[2]),
            2
        );
    }

    #[test]
    fn ralph_no_progress_resets_after_progress() {
        let iterations = vec![
            ralph_iteration_with_fingerprints(1, Some("a".to_owned()), Some("a".to_owned())),
            ralph_iteration_with_fingerprints(2, Some("a".to_owned()), Some("b".to_owned())),
        ];
        assert_eq!(
            consecutive_no_progress_iterations(&iterations, &iterations[1]),
            0
        );
    }

    #[test]
    fn ralph_model_completion_failures_map_to_terminal_run_reasons() {
        assert_eq!(
            ralph_run_failure_from_model_completion(
                "work",
                &ModelTurnCompletion::with_message(ModelTurnOutcome::Cancelled, "cancelled")
            ),
            Some(("stopped", "cancelled", "cancelled".to_owned()))
        );
        assert_eq!(
            ralph_run_failure_from_model_completion(
                "work",
                &ModelTurnCompletion::with_message(
                    ModelTurnOutcome::ProviderUnavailable,
                    "provider unavailable"
                )
            ),
            Some((
                "blocked",
                "provider_unavailable",
                "provider unavailable".to_owned()
            ))
        );
        assert_eq!(
            ralph_run_failure_from_model_completion(
                "work",
                &ModelTurnCompletion::with_message(ModelTurnOutcome::Error, "permission denied")
            ),
            Some((
                "stopped",
                "permission_denied",
                "permission denied".to_owned()
            ))
        );
        assert_eq!(
            ralph_run_failure_from_model_completion(
                "work",
                &ModelTurnCompletion::with_message(ModelTurnOutcome::Error, "needs user question")
            ),
            Some(("blocked", "user_question", "needs user question".to_owned()))
        );
        assert_eq!(
            ralph_run_failure_from_model_completion("work", &ModelTurnCompletion::completed()),
            None
        );
    }

    #[test]
    fn ralph_cancelled_iteration_completion_persists_stopped_run() {
        let ralph = test_ralph_store();
        let state_dir = PathBuf::from(format!(
            "/tmp/bcode-server-ralph-cancel-{}",
            uuid::Uuid::new_v4()
        ));
        let run = ralph
            .store
            .create_run(bcode_ralph::RalphRunCreateRequest {
                state_dir: state_dir.clone(),
                session_id: None,
                status: "running".to_owned(),
                requested_max_iterations: Some(1),
                requested_no_progress_limit: Some(1),
            })
            .expect("run should persist");
        ralph
            .store
            .request_run_cancel(&run.run_id)
            .expect("cancel should persist");
        assert!(ralph_run_cancel_requested(&ralph.store, &run));

        let completion = ralph_cancelled_iteration_completion(&ralph.store, &run);

        assert!(!completion.continue_loop);
        assert_eq!(completion.runtime_status, RuntimeWorkStatus::Cancelled);
        assert!(
            ralph
                .store
                .active_run_for_loop(&state_dir)
                .expect("active run query should work")
                .is_none()
        );
    }

    #[test]
    fn ralph_iteration_status_tracks_model_outcomes() {
        assert_eq!(
            ralph_iteration_status_from_model_outcome(Some(ModelTurnOutcome::Completed)),
            "work_completed"
        );
        assert_eq!(
            ralph_iteration_status_from_model_outcome(Some(ModelTurnOutcome::Cancelled)),
            "work_cancelled"
        );
        assert_eq!(
            ralph_iteration_status_from_model_outcome(Some(ModelTurnOutcome::Error)),
            "work_failed"
        );
    }

    #[test]
    fn tool_stream_sequence_normalization_is_monotonic_per_call() {
        let mut sequences = BTreeMap::new();
        let first = normalize_tool_stream_event_sequence(
            ServiceToolInvocationStreamEvent::Started {
                tool_call_id: "call-a".to_string(),
                tool_name: "shell.run".to_string(),
                sequence: 99,
                terminal: true,
                columns: Some(80),
                rows: Some(24),
                started_at_ms: Some(1),
            },
            &mut sequences,
        );
        let second = normalize_tool_stream_event_sequence(
            ServiceToolInvocationStreamEvent::Status {
                tool_call_id: "call-a".to_string(),
                sequence: 99,
                message: "running".to_string(),
            },
            &mut sequences,
        );
        let other_call = normalize_tool_stream_event_sequence(
            ServiceToolInvocationStreamEvent::Finished {
                tool_call_id: "call-b".to_string(),
                sequence: 99,
                is_error: false,
                finished_at_ms: Some(2),
            },
            &mut sequences,
        );

        assert!(matches!(
            first,
            ToolInvocationStreamEvent::Started { sequence: 1, .. }
        ));
        assert!(matches!(
            second,
            ToolInvocationStreamEvent::Status { sequence: 2, .. }
        ));
        assert!(matches!(
            other_call,
            ToolInvocationStreamEvent::Finished { sequence: 1, .. }
        ));
    }

    #[test]
    fn model_poll_progress_requires_meaningful_stream_events() {
        assert!(model_event_is_progress(&ProviderTurnEvent::TextDelta {
            text: "hello".to_string(),
        }));
        assert!(model_event_is_progress(
            &ProviderTurnEvent::ReasoningDelta {
                text: "thinking".to_string(),
            }
        ));
        assert!(model_event_is_progress(&ProviderTurnEvent::ToolCallDelta {
            call_id: "call-test".to_string(),
            delta: "{\"path\"".to_string(),
        }));
        assert!(model_event_is_progress(
            &ProviderTurnEvent::ToolCallStarted {
                call_id: "call-test".to_string(),
                name: "filesystem.read".to_string(),
            }
        ));
        assert!(model_event_is_progress(
            &ProviderTurnEvent::ToolCallFinished {
                call: test_tool_call(),
            }
        ));

        assert!(!model_event_is_progress(&ProviderTurnEvent::TextDelta {
            text: String::new(),
        }));
        assert!(!model_event_is_progress(
            &ProviderTurnEvent::ToolCallDelta {
                call_id: "call-test".to_string(),
                delta: String::new(),
            }
        ));
        assert!(!model_event_is_progress(&ProviderTurnEvent::Usage {
            usage: bcode_model::TokenUsage::default(),
        }));
        assert!(!model_event_is_progress(
            &ProviderTurnEvent::ProviderMetadata {
                key: "response_id".to_string(),
                value: "resp-test".to_string(),
            }
        ));
        assert!(!model_event_is_progress(&ProviderTurnEvent::Warning {
            message: "warning".to_string(),
        }));
    }

    #[test]
    fn compaction_poll_progress_only_counts_summary_content() {
        assert!(compaction_event_is_progress(
            &ProviderTurnEvent::TextDelta {
                text: "summary".to_string(),
            }
        ));
        assert!(compaction_event_is_progress(
            &ProviderTurnEvent::ReasoningDelta {
                text: "thinking".to_string(),
            }
        ));
        assert!(!compaction_event_is_progress(&ProviderTurnEvent::Usage {
            usage: bcode_model::TokenUsage::default(),
        }));
        assert!(!compaction_event_is_progress(
            &ProviderTurnEvent::ToolCallStarted {
                call_id: "call-test".to_string(),
                name: "filesystem.read".to_string(),
            }
        ));
    }

    #[test]
    fn compact_attach_history_drops_completed_assistant_deltas() {
        let session_id = SessionId::new();
        let history = vec![
            session_event(
                session_id,
                0,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "hello".to_string(),
                },
            ),
            session_event(
                session_id,
                1,
                SessionEventKind::AssistantDelta {
                    text: "hel".to_string(),
                },
            ),
            session_event(
                session_id,
                2,
                SessionEventKind::AssistantDelta {
                    text: "lo".to_string(),
                },
            ),
            session_event(
                session_id,
                3,
                SessionEventKind::AssistantMessage {
                    text: "hello".to_string(),
                },
            ),
            session_event(
                session_id,
                4,
                SessionEventKind::SystemMessage {
                    text: "done".to_string(),
                },
            ),
        ];

        let compacted = compact_attach_history(history);

        assert_eq!(compacted.len(), 3);
        assert!(matches!(
            compacted[0].kind,
            SessionEventKind::UserMessage { .. }
        ));
        assert!(matches!(
            compacted[1].kind,
            SessionEventKind::AssistantMessage { .. }
        ));
        assert_eq!(compacted[1].sequence, 3);
        assert!(matches!(
            compacted[2].kind,
            SessionEventKind::SystemMessage { .. }
        ));
    }

    #[test]
    fn session_projection_uses_latest_context_compaction() {
        let session_id = SessionId::new();
        let history = vec![
            session_event(
                session_id,
                0,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "old request".to_string(),
                },
            ),
            session_event(
                session_id,
                1,
                SessionEventKind::ContextCompacted {
                    summary: "summary of old request".to_string(),
                    compacted_through_sequence: 0,
                },
            ),
            session_event(
                session_id,
                2,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "new request".to_string(),
                },
            ),
        ];

        let messages = session_events_to_model_messages(&history);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[1].role, MessageRole::User);
        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::Text { text } if text.contains("summary of old request")
        ));
        assert!(matches!(
            &messages[1].content[0],
            ContentBlock::Text { text } if text == "new request"
        ));
    }

    #[test]
    fn session_projection_keeps_events_after_compacted_sequence() {
        let session_id = SessionId::new();
        let client_id = ClientId::new();
        let history = vec![
            session_event(
                session_id,
                0,
                SessionEventKind::UserMessage {
                    client_id,
                    text: "old request".to_string(),
                },
            ),
            session_event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id,
                    text: "current request".to_string(),
                },
            ),
            session_event(
                session_id,
                2,
                SessionEventKind::ContextCompacted {
                    summary: "summary of old request".to_string(),
                    compacted_through_sequence: 0,
                },
            ),
        ];

        let messages = session_events_to_model_messages(&history);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[1].role, MessageRole::User);
        assert!(matches!(
            &messages[1].content[0],
            ContentBlock::Text { text } if text == "current request"
        ));
    }

    #[test]
    fn session_projection_synthesizes_missing_tool_result_before_next_user_message() {
        let session_id = SessionId::new();
        let client_id = ClientId::new();
        let history = vec![
            session_event(
                session_id,
                0,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "shell.run".to_string(),
                    arguments_json: r#"{"command":"true"}"#.to_string(),
                    request_presentation: None,
                },
            ),
            session_event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id,
                    text: "continue".to_string(),
                },
            ),
        ];

        let messages = session_events_to_model_messages(&history);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, MessageRole::Assistant);
        assert_eq!(messages[1].role, MessageRole::Tool);
        assert_eq!(messages[2].role, MessageRole::User);
        assert!(matches!(
            &messages[1].content[0],
            ContentBlock::ToolResult { result }
                if result.call_id == "call-1" && result.is_error
        ));
    }

    #[test]
    fn session_projection_keeps_existing_tool_result_for_tool_call() {
        let session_id = SessionId::new();
        let history = vec![
            session_event(
                session_id,
                0,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "shell.run".to_string(),
                    arguments_json: r#"{"command":"true"}"#.to_string(),
                    request_presentation: None,
                },
            ),
            session_event(
                session_id,
                1,
                SessionEventKind::ToolCallFinished {
                    tool_call_id: "call-1".to_string(),
                    result: "ok".to_string(),
                    is_error: false,
                    output: None,
                    semantic_result: None,
                },
            ),
        ];

        let messages = session_events_to_model_messages(&history);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::Assistant);
        assert_eq!(messages[1].role, MessageRole::Tool);
        assert!(matches!(
            &messages[1].content[0],
            ContentBlock::ToolResult { result }
                if result.call_id == "call-1" && result.output == "ok" && !result.is_error
        ));
    }

    #[test]
    fn session_projection_converts_orphan_tool_result_to_plain_context() {
        let session_id = SessionId::new();
        let history = vec![session_event(
            session_id,
            1,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_string(),
                result: "orphaned output".to_string(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        )];

        let messages = session_events_to_model_messages(&history);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, MessageRole::User);
        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::Text { text }
                if text.contains("matching assistant tool call is unavailable")
                    && text.contains("orphaned output")
        ));
    }

    #[test]
    fn session_projection_converts_malformed_tool_call_to_plain_context() {
        let session_id = SessionId::new();
        let history = vec![session_event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_string(),
                tool_name: "shell.run".to_string(),
                arguments_json: "{not-json".to_string(),
                request_presentation: None,
            },
        )];

        let messages = session_events_to_model_messages(&history);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, MessageRole::User);
        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::Text { text }
                if text.contains("arguments were malformed or truncated")
                    && text.contains("{not-json")
        ));
    }

    #[test]
    fn compaction_request_omits_optional_params_for_strict_providers() {
        let session_id = SessionId::new();
        let selection = SessionModelSelection {
            model_id: Some("model".to_string()),
            ..SessionModelSelection::default()
        };
        let prompt_text = "old transcript";

        let request = build_compaction_request(session_id, &selection, prompt_text, "turn".into());

        assert_eq!(request.parameters.temperature, None);
        assert_eq!(request.parameters.max_output_tokens, None);
        assert_eq!(request.parameters.top_p, None);
    }

    #[test]
    fn compaction_transcript_truncates_large_tool_results() {
        let session_id = SessionId::new();
        let transcript = compaction_transcript(
            &[session_event(
                session_id,
                1,
                SessionEventKind::ToolCallFinished {
                    tool_call_id: "call-1".to_string(),
                    result: format!("{}middle{}", "x".repeat(2_000), "y".repeat(2_000)),
                    is_error: false,
                    output: None,
                    semantic_result: None,
                },
            )],
            1_000,
        )
        .expect("compaction transcript");

        let text = transcript
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(text.contains("tool output truncated"));
        assert!(!text.contains("middle"));
        assert!(text.chars().count() < 1_200);
    }

    #[test]
    fn compaction_transcript_keeps_recent_tail_out_of_summary() {
        let session_id = SessionId::new();
        let mut history = Vec::new();
        for sequence in 0..6 {
            history.push(session_event(
                session_id,
                sequence,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "x".repeat(COMPACTION_KEEP_RECENT_CHARS / 2),
                },
            ));
        }

        let transcript = compaction_transcript(&history, 1_000).expect("compaction transcript");

        assert!(transcript.compacted_through_sequence < 5);
        assert!(transcript.event_count < history.len());
    }

    #[test]
    fn compaction_cut_never_leaves_orphan_tool_result() {
        let lines = compaction_lines_to_summarize(vec![
            CompactionLine {
                sequence: 1,
                text: "tool call".to_string(),
                can_cut_after: false,
            },
            CompactionLine {
                sequence: 2,
                text: "recent tail".repeat(COMPACTION_KEEP_RECENT_CHARS),
                can_cut_after: true,
            },
        ]);

        assert!(lines.is_empty());
    }

    #[test]
    fn compaction_prompt_truncates_carried_summary() {
        let transcript = CompactionTranscript {
            previous_summary: Some("s".repeat(COMPACTION_MAX_CARRIED_SUMMARY_CHARS + 100)),
            lines: vec![CompactionLine {
                sequence: 1,
                text: "next chunk".to_string(),
                can_cut_after: true,
            }],
            compacted_through_sequence: 1,
            event_count: 1,
        };

        let prompt = compaction_prompt_text(&transcript);

        assert!(prompt.contains("[truncated]"));
        assert!(prompt.contains("next chunk"));
    }

    #[test]
    fn compaction_timeout_errors_use_local_fallback_path() {
        assert!(is_retriable_compaction_error(
            "model provider did not finish compaction turn within 120 seconds"
        ));
    }

    #[test]
    fn compaction_provider_error_detail_avoids_nested_provider_prefix() {
        assert_eq!(
            compaction_error_detail(CompactionError::Provider("model failed".to_string())),
            "model failed"
        );
    }

    #[test]
    fn context_length_errors_are_retryable_by_compaction_policy() {
        let error = bcode_model::ProviderError {
            code: "context_length_exceeded".to_string(),
            category: bcode_model::ProviderErrorCategory::ContextLength,
            message: "too many tokens".to_string(),
            retryable: false,
            provider_message: None,
            retry: None,
        };

        assert!(is_context_length_provider_error(&error));
    }

    #[test]
    fn malformed_tool_arguments_are_retryable_once() {
        let error = bcode_model::ProviderError {
            code: TOOL_ARGUMENTS_DECODE_FAILED_CODE.to_string(),
            category: bcode_model::ProviderErrorCategory::ProviderInternal,
            message: "EOF while parsing a string".to_string(),
            retryable: false,
            provider_message: None,
            retry: None,
        };

        assert!(is_tool_arguments_decode_provider_error(&error));
        assert!(should_retry_after_malformed_tool_arguments(&error, false));
        assert!(!should_retry_after_malformed_tool_arguments(&error, true));
    }

    #[test]
    fn overloaded_errors_are_retryable_until_configured_limit() {
        let error = bcode_model::ProviderError {
            code: "server_is_overloaded".to_string(),
            category: bcode_model::ProviderErrorCategory::Overloaded,
            message: "try again later".to_string(),
            retryable: true,
            provider_message: None,
            retry: None,
        };
        let mut state = test_server_state(SessionManager::default());
        state.model_retry.max_overload_retries = 2;

        assert!(is_overloaded_provider_error(&error));
        assert!(should_retry_after_overload_error(&state, &error, 0));
        assert!(should_retry_after_overload_error(&state, &error, 1));
        assert!(!should_retry_after_overload_error(&state, &error, 2));
    }

    #[test]
    fn successful_provider_round_resets_retry_attempts() {
        let mut recovery = ModelTurnRecoveryState {
            retry_attempts: BTreeMap::from([("builtin.overload".to_string(), 3)]),
            retry_instruction: Some(MALFORMED_TOOL_ARGUMENTS_RETRY_INSTRUCTION),
            ..ModelTurnRecoveryState::default()
        };

        recovery.record_successful_provider_round();

        assert!(recovery.retry_attempts.is_empty());
        assert_eq!(recovery.retry_instruction, None);
    }

    #[test]
    fn remote_catalog_rule_merges_between_provider_and_user_rules() {
        let provider_rule = bcode_model::ProviderRetryRule {
            id: "provider.rule".to_string(),
            max_retries: Some(3),
            r#match: bcode_model::ProviderRetryRuleMatch {
                code: Some("provider_code".to_string()),
                ..bcode_model::ProviderRetryRuleMatch::default()
            },
            ..bcode_model::ProviderRetryRule::default()
        };
        let remote_rule = bcode_model::ProviderRetryRule {
            id: "provider.rule".to_string(),
            r#match: bcode_model::ProviderRetryRuleMatch {
                message_contains: Some("remote message".to_string()),
                ..bcode_model::ProviderRetryRuleMatch::default()
            },
            ..bcode_model::ProviderRetryRule::default()
        };
        let user_rule = bcode_config::ModelRetryRuleConfig {
            id: "provider.rule".to_string(),
            max_retries: Some(1),
            ..bcode_config::ModelRetryRuleConfig::default()
        };

        let effective =
            effective_provider_retry_rules(&[provider_rule], &[remote_rule], &[user_rule]);

        assert_eq!(effective.len(), 1);
        let rule = &effective[0];
        assert_eq!(rule.max_retries, Some(1));
        assert_eq!(rule.r#match.code.as_deref(), Some("provider_code"));
        assert_eq!(
            rule.r#match.message_contains.as_deref(),
            Some("remote message")
        );
    }

    #[test]
    fn remote_catalog_pattern_converts_to_sparse_retry_rule() {
        let pattern = bcode_model_catalog_models::RecoverableErrorPattern {
            id: "remote.pattern".to_string(),
            enabled_by_default: true,
            scope: bcode_model_catalog_models::RecoverableErrorPatternScope {
                provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                ..bcode_model_catalog_models::RecoverableErrorPatternScope::default()
            },
            r#match: bcode_model_catalog_models::RecoverableErrorPatternMatch {
                code: Some("http_400".to_string()),
                message_contains: Some("Unsupported content type".to_string()),
                ..bcode_model_catalog_models::RecoverableErrorPatternMatch::default()
            },
        };

        let rule = remote_pattern_retry_rule(&pattern).expect("pattern should convert");

        assert_eq!(rule.id, "remote.pattern");
        assert_eq!(rule.enabled, Some(true));
        assert_eq!(rule.max_retries, None);
        assert_eq!(
            rule.provider_plugin_id.as_deref(),
            Some("bcode.openai-compatible")
        );
        assert_eq!(rule.r#match.code.as_deref(), Some("http_400"));
    }

    #[test]
    fn user_retry_rule_deep_merges_over_provider_rule() {
        let provider_rule = bcode_model::ProviderRetryRule {
            id: "provider.rule".to_string(),
            enabled: Some(true),
            provider_plugin_id: Some("bcode.openai-compatible".to_string()),
            max_retries: Some(3),
            initial_delay_ms: Some(1_000),
            max_delay_ms: Some(8_000),
            use_provider_retry_hint: Some(true),
            r#match: bcode_model::ProviderRetryRuleMatch {
                code: Some("http_400".to_string()),
                message_contains: Some("Unsupported content type".to_string()),
                ..bcode_model::ProviderRetryRuleMatch::default()
            },
            ..bcode_model::ProviderRetryRule::default()
        };
        let user_rule = bcode_config::ModelRetryRuleConfig {
            id: "provider.rule".to_string(),
            max_retries: Some(1),
            ..bcode_config::ModelRetryRuleConfig::default()
        };

        let effective = effective_provider_retry_rules(&[provider_rule], &[], &[user_rule]);

        assert_eq!(effective.len(), 1);
        let rule = &effective[0];
        assert_eq!(rule.max_retries, Some(1));
        assert_eq!(rule.initial_delay_ms, Some(1_000));
        assert_eq!(rule.r#match.code.as_deref(), Some("http_400"));
    }

    #[test]
    fn user_retry_rule_can_disable_provider_rule_without_redefining_matcher() {
        let provider_rule = bcode_model::ProviderRetryRule {
            id: "provider.rule".to_string(),
            enabled: Some(true),
            r#match: bcode_model::ProviderRetryRuleMatch {
                code: Some("http_400".to_string()),
                ..bcode_model::ProviderRetryRuleMatch::default()
            },
            ..bcode_model::ProviderRetryRule::default()
        };
        let user_rule = bcode_config::ModelRetryRuleConfig {
            id: "provider.rule".to_string(),
            enabled: Some(false),
            ..bcode_config::ModelRetryRuleConfig::default()
        };
        let error = bcode_model::ProviderError {
            code: "http_400".to_string(),
            category: bcode_model::ProviderErrorCategory::InvalidRequest,
            message: "bad request".to_string(),
            retryable: false,
            provider_message: None,
            retry: None,
        };
        let selection = SessionModelSelection::default();

        let effective = effective_provider_retry_rules(&[provider_rule], &[], &[user_rule]);

        assert!(!custom_retry_rule_matches(
            &effective[0],
            &error,
            &selection
        ));
    }

    #[test]
    fn custom_retry_rule_matches_error_and_scope() {
        let mut state = test_server_state(SessionManager::default());
        state
            .model_retry
            .rules
            .push(bcode_config::ModelRetryRuleConfig {
                id: "unsupported-content-type".to_string(),
                provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                model_id_contains: Some("claude".to_string()),
                max_retries: Some(2),
                initial_delay_ms: Some(500),
                max_delay_ms: Some(4_000),
                r#match: bcode_config::ModelRetryRuleMatchConfig {
                    code: Some("http_400".to_string()),
                    message_contains: Some("Unsupported content type".to_string()),
                    ..bcode_config::ModelRetryRuleMatchConfig::default()
                },
                ..bcode_config::ModelRetryRuleConfig::default()
            });
        let error = bcode_model::ProviderError {
            code: "http_400".to_string(),
            category: bcode_model::ProviderErrorCategory::InvalidRequest,
            message: r#"{"detail":"Unsupported content type"}"#.to_string(),
            retryable: false,
            provider_message: None,
            retry: None,
        };
        let selection = SessionModelSelection {
            provider_plugin_id: Some("bcode.openai-compatible".to_string()),
            model_id: Some("anthropic.claude-test".to_string()),
            ..SessionModelSelection::default()
        };

        let policy = matching_provider_retry_policy(&state, &error, &selection, &[], &[])
            .expect("custom retry policy should match");

        assert_eq!(policy.id, "custom.unsupported-content-type");
        assert_eq!(policy.max_retries, 2);
        assert_eq!(policy.initial_delay_ms, 500);
    }

    #[test]
    fn custom_retry_rule_does_not_match_unscoped_provider() {
        let mut state = test_server_state(SessionManager::default());
        state
            .model_retry
            .rules
            .push(bcode_config::ModelRetryRuleConfig {
                id: "unsupported-content-type".to_string(),
                provider_plugin_id: Some("bcode.openai-compatible".to_string()),
                r#match: bcode_config::ModelRetryRuleMatchConfig {
                    code: Some("http_400".to_string()),
                    message_contains: Some("Unsupported content type".to_string()),
                    ..bcode_config::ModelRetryRuleMatchConfig::default()
                },
                ..bcode_config::ModelRetryRuleConfig::default()
            });
        let error = bcode_model::ProviderError {
            code: "http_400".to_string(),
            category: bcode_model::ProviderErrorCategory::InvalidRequest,
            message: r#"{"detail":"Unsupported content type"}"#.to_string(),
            retryable: false,
            provider_message: None,
            retry: None,
        };
        let selection = SessionModelSelection {
            provider_plugin_id: Some("bcode.bedrock".to_string()),
            ..SessionModelSelection::default()
        };

        assert!(matching_provider_retry_policy(&state, &error, &selection, &[], &[]).is_none());
    }

    #[test]
    fn custom_retry_delay_can_ignore_provider_hint() {
        let error = bcode_model::ProviderError {
            code: "http_400".to_string(),
            category: bcode_model::ProviderErrorCategory::InvalidRequest,
            message: "Unsupported content type".to_string(),
            retryable: false,
            provider_message: None,
            retry: Some(Box::new(bcode_model::ProviderRetryHint {
                retry_after_ms: Some(60_000),
                retry_at_unix: None,
                source: Some("header".to_string()),
            })),
        };
        let policy = ProviderRetryPolicy {
            id: "custom.unsupported-content-type".to_string(),
            display_name: "unsupported-content-type".to_string(),
            max_retries: 3,
            initial_delay_ms: 1_000,
            max_delay_ms: 8_000,
            use_provider_retry_hint: false,
            kind: ProviderRetryPolicyKind::Custom,
        };

        assert_eq!(
            provider_retry_delay(&policy, &error, 2),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn overload_retry_delay_uses_retry_hint_with_config_cap() {
        let error = bcode_model::ProviderError {
            code: "server_is_overloaded".to_string(),
            category: bcode_model::ProviderErrorCategory::Overloaded,
            message: "try again later".to_string(),
            retryable: true,
            provider_message: None,
            retry: Some(Box::new(bcode_model::ProviderRetryHint {
                retry_after_ms: Some(60_000),
                retry_at_unix: None,
                source: Some("header".to_string()),
            })),
        };
        let config = bcode_config::ModelRetryConfig {
            max_overload_retries: 5,
            overload_initial_delay_ms: 1_000,
            overload_max_delay_ms: 10_000,
            ..bcode_config::ModelRetryConfig::default()
        };

        assert_eq!(
            overload_retry_delay(&config, &error, 1),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn overload_retry_delay_uses_exponential_config_fallback() {
        let error = bcode_model::ProviderError {
            code: "server_is_overloaded".to_string(),
            category: bcode_model::ProviderErrorCategory::Overloaded,
            message: "try again later".to_string(),
            retryable: true,
            provider_message: None,
            retry: None,
        };
        let config = bcode_config::ModelRetryConfig {
            max_overload_retries: 5,
            overload_initial_delay_ms: 1_000,
            overload_max_delay_ms: 10_000,
            ..bcode_config::ModelRetryConfig::default()
        };

        assert_eq!(
            overload_retry_delay(&config, &error, 1),
            Duration::from_secs(1)
        );
        assert_eq!(
            overload_retry_delay(&config, &error, 4),
            Duration::from_secs(8)
        );
        assert_eq!(
            overload_retry_delay(&config, &error, 5),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn recoverable_provider_errors_are_deferred_until_retry_exhaustion() {
        let malformed_tool_error = bcode_model::ProviderError {
            code: TOOL_ARGUMENTS_DECODE_FAILED_CODE.to_string(),
            category: bcode_model::ProviderErrorCategory::ProviderInternal,
            message: "invalid JSON".to_string(),
            retryable: false,
            provider_message: None,
            retry: None,
        };
        let invalid_request_error = bcode_model::ProviderError {
            code: "bad_request".to_string(),
            category: bcode_model::ProviderErrorCategory::InvalidRequest,
            message: "bad request".to_string(),
            retryable: false,
            provider_message: None,
            retry: None,
        };

        let state = test_server_state(SessionManager::default());
        assert!(should_defer_visible_provider_error(
            &state,
            &malformed_tool_error,
            None
        ));
        let overload_error = bcode_model::ProviderError {
            code: "server_is_overloaded".to_string(),
            category: bcode_model::ProviderErrorCategory::Overloaded,
            message: "try again later".to_string(),
            retryable: true,
            provider_message: None,
            retry: None,
        };
        assert!(should_defer_visible_provider_error(
            &state,
            &overload_error,
            None
        ));
        assert!(!should_defer_visible_provider_error(
            &state,
            &invalid_request_error,
            None
        ));
    }

    #[test]
    fn compact_attach_history_preserves_incomplete_assistant_deltas() {
        let session_id = SessionId::new();
        let history = vec![
            session_event(
                session_id,
                0,
                SessionEventKind::AssistantDelta {
                    text: "still".to_string(),
                },
            ),
            session_event(
                session_id,
                1,
                SessionEventKind::AssistantDelta {
                    text: " streaming".to_string(),
                },
            ),
        ];

        let compacted = compact_attach_history(history);

        assert_eq!(compacted.len(), 2);
        assert!(matches!(
            compacted[0].kind,
            SessionEventKind::AssistantDelta { .. }
        ));
        assert!(matches!(
            compacted[1].kind,
            SessionEventKind::AssistantDelta { .. }
        ));
    }

    #[test]
    fn compact_attach_history_flushes_deltas_before_non_assistant_events() {
        let session_id = SessionId::new();
        let history = vec![
            session_event(
                session_id,
                0,
                SessionEventKind::AssistantDelta {
                    text: "partial".to_string(),
                },
            ),
            session_event(
                session_id,
                1,
                SessionEventKind::SystemMessage {
                    text: "interrupted".to_string(),
                },
            ),
            session_event(
                session_id,
                2,
                SessionEventKind::AssistantMessage {
                    text: "next turn".to_string(),
                },
            ),
        ];

        let compacted = compact_attach_history(history);

        assert_eq!(compacted.len(), 3);
        assert!(matches!(
            compacted[0].kind,
            SessionEventKind::AssistantDelta { .. }
        ));
        assert!(matches!(
            compacted[1].kind,
            SessionEventKind::SystemMessage { .. }
        ));
        assert!(matches!(
            compacted[2].kind,
            SessionEventKind::AssistantMessage { .. }
        ));
    }

    #[test]
    fn unspecified_model_selection_lets_provider_choose_default() {
        assert_eq!(model_id_for_provider_request(None), "");
    }

    #[test]
    fn explicit_model_selection_is_sent_to_provider() {
        assert_eq!(
            model_id_for_provider_request(Some("fake-echo")),
            "fake-echo"
        );
    }

    #[test]
    fn select_model_info_prefers_selected_model_then_default() {
        let models = vec![
            bcode_model::ModelInfo {
                model_id: "default".to_string(),
                display_name: "Default".to_string(),
                is_default: true,
                context_window: Some(8_000),
                max_output_tokens: Some(1_000),
                capabilities: BTreeSet::new(),
                reasoning: None,
                cache: bcode_model::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                visibility: bcode_model::ModelVisibility::Visible,
            },
            bcode_model::ModelInfo {
                model_id: "selected".to_string(),
                display_name: "Selected".to_string(),
                is_default: false,
                context_window: Some(16_000),
                max_output_tokens: Some(2_000),
                capabilities: BTreeSet::new(),
                reasoning: None,
                cache: bcode_model::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                visibility: bcode_model::ModelVisibility::Visible,
            },
        ];

        assert_eq!(
            select_model_info(&models, Some("selected")).map(|model| model.model_id),
            Some("selected".to_string())
        );
        assert_eq!(
            select_model_info(&models, None).map(|model| model.model_id),
            Some("default".to_string())
        );
    }

    #[test]
    fn prompt_cache_auto_marks_stable_sections_only() {
        let mut messages = Vec::new();

        let hints = plan_prompt_cache(&mut messages, bcode_model::PromptCacheMode::Auto);

        assert!(hints.cache_system_prompt);
        assert!(hints.cache_tools);
        assert_eq!(messages.len(), 0);
    }

    #[test]
    fn prompt_cache_aggressive_marks_conversation_prefix() {
        let mut messages = (0..6)
            .map(|index| ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: format!("message {index}"),
                }],
            })
            .collect::<Vec<_>>();

        let hints = plan_prompt_cache(&mut messages, bcode_model::PromptCacheMode::Aggressive);

        assert!(hints.cache_system_prompt);
        assert!(matches!(
            messages[3].content.last(),
            Some(ContentBlock::CachePoint { .. })
        ));
        assert!(!matches!(
            messages[5].content.last(),
            Some(ContentBlock::CachePoint { .. })
        ));
    }

    #[test]
    fn coding_system_prompt_splits_stable_and_dynamic_context() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let (stable, dynamic) = build_coding_system_prompt_parts(
            &cwd,
            &bcode_config::SystemPromptConfig::default(),
            Some("agent suffix"),
            Some("<skills />"),
        );

        assert!(stable.contains(DEFAULT_CODING_SYSTEM_PROMPT));
        assert!(stable.contains("Stable repository context:"));
        assert!(stable.contains("agent suffix"));
        assert!(dynamic.contains("Dynamic repository context:"));
        assert!(!stable.contains("Git status:"));
    }

    #[test]
    fn session_token_usage_preserves_normalized_fields() {
        let usage = session_token_usage(&TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cached_input_tokens: Some(3),
            cache_write_input_tokens: Some(4),
            reasoning_tokens: Some(2),
        });

        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.metered_total_tokens(), Some(15));
        assert_eq!(usage.cached_input_tokens, Some(3));
        assert_eq!(usage.cache_write_input_tokens, Some(4));
        assert_eq!(usage.reasoning_tokens, Some(2));
    }

    #[test]
    fn project_tool_result_for_model_context_preserves_small_output() {
        let output = "short tool output";

        assert_eq!(
            project_tool_result_for_model_context(output, None, 4_000),
            output
        );
    }

    #[test]
    fn project_tool_result_for_model_context_truncates_large_output_with_artifact_path() {
        let output = format!("{}middle{}", "a".repeat(4_000), "z".repeat(4_000));

        let truncated = project_tool_result_for_model_context(
            &output,
            Some(PathBuf::from("/tmp/full-output.txt")),
            1_000,
        );

        assert!(truncated.chars().count() <= 1_000);
        assert!(truncated.starts_with('a'));
        assert!(truncated.contains("tool output truncated"));
        assert!(truncated.contains("artifact.read"));
        assert!(truncated.contains("/tmp/full-output.txt"));
        assert!(truncated.ends_with("necessary.]\n\n") || truncated.contains('z'));
        assert!(truncated.contains('z'));
    }

    fn test_server_state(sessions: SessionManager) -> ServerState {
        test_server_state_with_ralph_store(sessions, bcode_ralph::RalphStateStore::default())
    }

    fn test_server_state_with_ralph_store(
        sessions: SessionManager,
        ralph_store: bcode_ralph::RalphStateStore,
    ) -> ServerState {
        ServerState::new(
            sessions,
            bcode_plugin::PluginHost::default().into(),
            ServerStateInit {
                selected_provider_plugin_id: None,
                selected_model_id: None,
                selected_provider_context: bcode_model::ProviderRequestContext::default(),
                prompt_cache_mode: bcode_model::PromptCacheMode::default(),
                conversation_reuse_mode: bcode_model::ConversationReuseMode::default(),
                selected_reasoning: bcode_config::ReasoningConfig::default(),
                selected_reasoning_capabilities: None,
                provider_state: ProviderStateStore::load(PathBuf::new()),
                observability: bcode_config::ObservabilityConfig::default(),
                trace_store: TraceStore::new(PathBuf::new()),
                max_tool_rounds: None,
                tool_output_context_chars: 1_000,
                model_streaming: bcode_config::StreamingConfig::default(),
                model_retry: bcode_config::ModelRetryConfig::default(),
                auto_compaction: bcode_config::CompactionConfig::default(),
                skills: None,
                skill_context_bytes: 0,
                skill_prompt_options: SkillPromptCatalogOptions::default(),
                system_prompt: bcode_config::SystemPromptConfig::default(),
                daemon_status: DaemonStatus::default(),
                daemon_record_path: None,
                metrics: MetricsRegistry::default(),
                ralph_store,
            },
        )
    }

    #[tokio::test]
    async fn tool_output_delta_is_transient_not_durable() {
        let sessions = SessionManager::default();
        let summary = sessions
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        let session_id = summary.id;
        let mut attachment = sessions
            .attach_session(session_id, ClientId::new())
            .await
            .expect("session should attach");
        let state = test_server_state(sessions);
        let delta = ToolInvocationStreamEvent::OutputDelta {
            tool_call_id: "call-1".to_owned(),
            stream: SessionToolOutputStream::Pty,
            sequence: 1,
            text: "live".to_owned(),
            byte_len: 4,
        };

        append_tool_stream_event(&state, session_id, delta.clone()).await;

        let received = loop {
            let event = attachment
                .live_events
                .recv()
                .await
                .expect("subscriber should receive live delta");
            if matches!(event.kind, SessionLiveEventKind::ToolOutputDelta { .. }) {
                break event;
            }
        };
        assert_eq!(
            received.kind,
            SessionLiveEventKind::ToolOutputDelta { event: delta }
        );
        let history = state
            .sessions
            .session_history(session_id)
            .await
            .expect("history should read");
        assert!(!history.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta { .. }
            }
        )));
    }

    #[tokio::test]
    async fn tool_output_accumulator_coalesces_adjacent_deltas() {
        let sessions = SessionManager::default();
        let summary = sessions
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        let session_id = summary.id;
        let mut attachment = sessions
            .attach_session(session_id, ClientId::new())
            .await
            .expect("session should attach");
        let state = test_server_state(sessions);
        let mut pending_output = None;

        push_tool_output_stream(
            &state,
            session_id,
            &mut pending_output,
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-1".to_owned(),
                stream: SessionToolOutputStream::Pty,
                sequence: 1,
                text: "hello ".to_owned(),
                byte_len: 6,
            },
        )
        .await;
        push_tool_output_stream(
            &state,
            session_id,
            &mut pending_output,
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-1".to_owned(),
                stream: SessionToolOutputStream::Pty,
                sequence: 2,
                text: "world".to_owned(),
                byte_len: 5,
            },
        )
        .await;
        flush_tool_output_stream(&state, session_id, &mut pending_output).await;

        let received = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive coalesced live delta");
        assert_eq!(
            received.kind,
            SessionLiveEventKind::ToolOutputDelta {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-1".to_owned(),
                    stream: SessionToolOutputStream::Pty,
                    sequence: 1,
                    text: "hello world".to_owned(),
                    byte_len: 11,
                },
            }
        );
        assert!(attachment.live_events.try_recv().is_err());
        let history = state
            .sessions
            .session_history(session_id)
            .await
            .expect("history should read");
        assert!(!history.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta { .. }
            }
        )));
    }

    #[tokio::test]
    async fn tool_output_accumulator_flushes_on_stream_change() {
        let sessions = SessionManager::default();
        let summary = sessions
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        let session_id = summary.id;
        let mut attachment = sessions
            .attach_session(session_id, ClientId::new())
            .await
            .expect("session should attach");
        let state = test_server_state(sessions);
        let mut pending_output = None;

        push_tool_output_stream(
            &state,
            session_id,
            &mut pending_output,
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-1".to_owned(),
                stream: SessionToolOutputStream::Stdout,
                sequence: 1,
                text: "out".to_owned(),
                byte_len: 3,
            },
        )
        .await;
        push_tool_output_stream(
            &state,
            session_id,
            &mut pending_output,
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-1".to_owned(),
                stream: SessionToolOutputStream::Stderr,
                sequence: 2,
                text: "err".to_owned(),
                byte_len: 3,
            },
        )
        .await;
        flush_tool_output_stream(&state, session_id, &mut pending_output).await;

        let first = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive stdout delta");
        let second = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive stderr delta");
        assert_eq!(
            first.kind,
            SessionLiveEventKind::ToolOutputDelta {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-1".to_owned(),
                    stream: SessionToolOutputStream::Stdout,
                    sequence: 1,
                    text: "out".to_owned(),
                    byte_len: 3,
                },
            }
        );
        assert_eq!(
            second.kind,
            SessionLiveEventKind::ToolOutputDelta {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-1".to_owned(),
                    stream: SessionToolOutputStream::Stderr,
                    sequence: 2,
                    text: "err".to_owned(),
                    byte_len: 3,
                },
            }
        );
    }

    #[tokio::test]
    async fn tool_output_accumulator_flushes_at_byte_threshold() {
        let sessions = SessionManager::default();
        let summary = sessions
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        let session_id = summary.id;
        let mut attachment = sessions
            .attach_session(session_id, ClientId::new())
            .await
            .expect("session should attach");
        let state = test_server_state(sessions);
        let mut pending_output = None;

        push_tool_output_stream(
            &state,
            session_id,
            &mut pending_output,
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-1".to_owned(),
                stream: SessionToolOutputStream::Pty,
                sequence: 1,
                text: "x".repeat(TOOL_OUTPUT_FLUSH_BYTES - 1),
                byte_len: TOOL_OUTPUT_FLUSH_BYTES - 1,
            },
        )
        .await;
        assert!(attachment.live_events.try_recv().is_err());

        push_tool_output_stream(
            &state,
            session_id,
            &mut pending_output,
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-1".to_owned(),
                stream: SessionToolOutputStream::Pty,
                sequence: 2,
                text: "y".to_owned(),
                byte_len: 1,
            },
        )
        .await;

        let received = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive threshold flush");
        let SessionLiveEventKind::ToolOutputDelta {
            event:
                ToolInvocationStreamEvent::OutputDelta {
                    sequence,
                    text,
                    byte_len,
                    ..
                },
        } = received.kind
        else {
            panic!("expected tool output delta");
        };
        assert_eq!(sequence, 1);
        assert_eq!(text.len(), TOOL_OUTPUT_FLUSH_BYTES);
        assert!(text.ends_with('y'));
        assert_eq!(byte_len, TOOL_OUTPUT_FLUSH_BYTES);
        assert!(pending_output.is_none());
    }

    #[tokio::test]
    async fn append_tool_finished_event_inner_preserves_canonical_result() {
        let sessions = SessionManager::default();
        let summary = sessions
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        let session_id = summary.id;
        let canonical_result = serde_json::json!({
            "mode": "terminal",
            "exit_code": 0,
            "timed_out": false,
            "output": "x".repeat(4_001),
            "columns": 80,
            "rows": 10,
        })
        .to_string();
        let state = ServerState::new(
            sessions,
            bcode_plugin::PluginHost::default().into(),
            ServerStateInit {
                selected_provider_plugin_id: None,
                selected_model_id: None,
                selected_provider_context: bcode_model::ProviderRequestContext::default(),
                prompt_cache_mode: bcode_model::PromptCacheMode::default(),
                conversation_reuse_mode: bcode_model::ConversationReuseMode::default(),
                selected_reasoning: bcode_config::ReasoningConfig::default(),
                selected_reasoning_capabilities: None,
                provider_state: ProviderStateStore::load(PathBuf::new()),
                observability: bcode_config::ObservabilityConfig::default(),
                trace_store: TraceStore::new(PathBuf::new()),
                max_tool_rounds: None,
                tool_output_context_chars: 1_000,
                model_streaming: bcode_config::StreamingConfig::default(),
                model_retry: bcode_config::ModelRetryConfig::default(),
                auto_compaction: bcode_config::CompactionConfig::default(),
                skills: None,
                skill_context_bytes: 0,
                skill_prompt_options: SkillPromptCatalogOptions::default(),
                system_prompt: bcode_config::SystemPromptConfig::default(),
                daemon_status: DaemonStatus::default(),
                daemon_record_path: None,
                metrics: MetricsRegistry::default(),
                ralph_store: bcode_ralph::RalphStateStore::default(),
            },
        );

        let event = append_tool_finished_event_inner(
            &state,
            session_id,
            ToolFinishedEventInput {
                tool_call_id: "call-1".to_owned(),
                result: canonical_result.clone(),
                is_error: false,
                content: Vec::new(),
                output: None,
                semantic_result: None,
            },
        )
        .await
        .expect("tool result event should append");

        let SessionEventKind::ToolCallFinished { result, .. } = event.kind else {
            panic!("expected tool result event");
        };
        assert_eq!(result, canonical_result);
        assert!(serde_json::from_str::<serde_json::Value>(&result).is_ok());
    }

    #[test]
    fn tool_result_model_message_uses_truncated_output() {
        let session_id = SessionId::new();
        let output = "x".repeat(4_001);
        let history = vec![
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 0,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "filesystem.read".to_string(),
                    arguments_json: r#"{"path":"Cargo.toml"}"#.to_string(),
                    request_presentation: None,
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolCallFinished {
                    tool_call_id: "call-1".to_string(),
                    result: output,
                    is_error: false,
                    output: None,
                    semantic_result: None,
                },
            },
        ];

        let messages = session_events_to_model_messages_with_limit(&history, 1_000);
        let ContentBlock::ToolResult { result } = &messages[1].content[0] else {
            panic!("expected tool result content block");
        };

        assert!(result.output.chars().count() <= 1_000);
        assert!(result.output.contains("tool output truncated"));
    }
    #[tokio::test]
    async fn append_tool_finished_event_inner_persists_semantic_result() {
        let sessions = SessionManager::default();
        let summary = sessions
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        let session_id = summary.id;
        let state = ServerState::new(
            sessions,
            bcode_plugin::PluginHost::default().into(),
            ServerStateInit {
                selected_provider_plugin_id: None,
                selected_model_id: None,
                selected_provider_context: bcode_model::ProviderRequestContext::default(),
                prompt_cache_mode: bcode_model::PromptCacheMode::default(),
                conversation_reuse_mode: bcode_model::ConversationReuseMode::default(),
                selected_reasoning: bcode_config::ReasoningConfig::default(),
                selected_reasoning_capabilities: None,
                provider_state: ProviderStateStore::load(PathBuf::new()),
                observability: bcode_config::ObservabilityConfig::default(),
                trace_store: TraceStore::new(PathBuf::new()),
                max_tool_rounds: None,
                tool_output_context_chars: 1_000,
                model_streaming: bcode_config::StreamingConfig::default(),
                model_retry: bcode_config::ModelRetryConfig::default(),
                auto_compaction: bcode_config::CompactionConfig::default(),
                skills: None,
                skill_context_bytes: 0,
                skill_prompt_options: SkillPromptCatalogOptions::default(),
                system_prompt: bcode_config::SystemPromptConfig::default(),
                daemon_status: DaemonStatus::default(),
                daemon_record_path: None,
                metrics: MetricsRegistry::default(),
                ralph_store: bcode_ralph::RalphStateStore::default(),
            },
        );
        let semantic_result = ToolInvocationResult::Text {
            text: "semantic text".to_owned(),
        };

        let event = append_tool_finished_event_inner(
            &state,
            session_id,
            ToolFinishedEventInput {
                tool_call_id: "call-semantic".to_owned(),
                result: "legacy text".to_owned(),
                is_error: false,
                content: Vec::new(),
                output: None,
                semantic_result: Some(semantic_result.clone()),
            },
        )
        .await
        .expect("tool result event should append");

        let SessionEventKind::ToolCallFinished {
            result,
            semantic_result: persisted_semantic_result,
            ..
        } = event.kind
        else {
            panic!("expected tool result event");
        };
        assert_eq!(result, "legacy text");
        assert_eq!(persisted_semantic_result, Some(semantic_result));
    }

    #[test]
    fn live_preview_extraction_supports_shell_file_and_query_tools() {
        let shell_metadata = bcode_tool::ToolLiveArgumentPreviewMetadata::ShellCommand {
            command_field: "command".to_owned(),
            cwd_field: Some("cwd".to_owned()),
            preview_title: None,
            streaming_status: None,
        };
        let mut shell_fields = StreamingJsonStringFields::default();
        shell_fields.push(r#"{"command":"cargo test","cwd":"/repo"}"#);
        let shell = live_tool_argument_preview_from_fields(&shell_metadata, &shell_fields)
            .expect("shell preview");
        assert!(matches!(shell, LiveToolArgumentPreview::ShellCommand(_)));

        let file_metadata = bcode_tool::ToolLiveArgumentPreviewMetadata::FileEdit {
            path_fields: vec!["path".to_owned()],
            old_text_fields: Vec::new(),
            new_text_fields: vec!["contents".to_owned()],
            preview_title: None,
            streaming_status: None,
        };
        let mut file_fields = StreamingJsonStringFields::default();
        file_fields.push(r#"{"path":"src/lib.rs","contents":"pub fn demo() {}"}"#);
        let file = live_tool_argument_preview_from_fields(&file_metadata, &file_fields)
            .expect("file preview");
        assert!(matches!(file, LiveToolArgumentPreview::FileEdit(_)));

        let query_metadata = bcode_tool::ToolLiveArgumentPreviewMetadata::Query {
            fields: vec!["query".to_owned(), "provider".to_owned()],
            preview_title: None,
            streaming_status: None,
        };
        let mut query_fields = StreamingJsonStringFields::default();
        query_fields.push(r#"{"query":"rust tui","provider":"brave"}"#);
        let query = live_tool_argument_preview_from_fields(&query_metadata, &query_fields)
            .expect("query preview");
        let LiveToolArgumentPreview::Query(query) = query else {
            panic!("expected query preview");
        };
        assert_eq!(query.fields.get("query"), Some(&"rust tui".to_owned()));
        assert_eq!(query.fields.get("provider"), Some(&"brave".to_owned()));
    }

    #[test]
    fn live_preview_suppresses_duplicate_preview_snapshots() {
        let mut progress = ModelStreamProgress::default();
        let metadata = bcode_tool::ToolLiveArgumentPreviewMetadata::ShellCommand {
            command_field: "command".to_owned(),
            cwd_field: None,
            preview_title: None,
            streaming_status: None,
        };
        progress.start_tool_call("call-1".to_owned(), "shell_run".to_owned(), Some(metadata));
        progress.record_tool_call_delta("call-1", r#"{"command":"cargo"}"#);
        assert!(progress.take_tool_argument_preview().is_some());
        assert!(progress.take_tool_argument_preview().is_none());
    }

    #[test]
    fn live_preview_is_independent_from_coarse_progress_threshold() {
        let mut progress = ModelStreamProgress::default();
        let metadata = bcode_tool::ToolLiveArgumentPreviewMetadata::ShellCommand {
            command_field: "command".to_owned(),
            cwd_field: None,
            preview_title: None,
            streaming_status: None,
        };
        progress.start_tool_call("call-1".to_owned(), "shell_run".to_owned(), Some(metadata));
        progress.record_tool_call_delta("call-1", r#"{"command":"x"}"#);
        assert!(progress.take_tool_progress_event().is_none());
        assert!(progress.take_tool_argument_preview().is_some());
    }

    #[test]
    fn streaming_json_fields_capture_path_after_large_contents() {
        let mut fields = StreamingJsonStringFields::default();
        fields.push("{\"contents\":\"");
        fields.push(&"pub fn hello() {}\n".repeat(40_000));
        fields.push("\",\"path\":\"src/hello.rs\"}");

        let contents = fields.field(&["contents"]).expect("contents field");
        assert!(contents.value.starts_with("pub fn hello"));
        assert!(contents.truncated);
        let path = fields.field(&["path"]).expect("path field");
        assert_eq!(path.value, "src/hello.rs");

        let metadata = bcode_tool::ToolLiveArgumentPreviewMetadata::FileEdit {
            path_fields: vec!["path".to_owned()],
            old_text_fields: Vec::new(),
            new_text_fields: vec!["contents".to_owned()],
            preview_title: None,
            streaming_status: None,
        };
        let preview =
            live_tool_argument_preview_from_fields(&metadata, &fields).expect("file preview");
        let LiveToolArgumentPreview::FileEdit(file) = preview else {
            panic!("expected file edit preview");
        };
        assert_eq!(file.path.as_deref(), Some("src/hello.rs"));
        assert!(file.new_text_prefix.starts_with("pub fn hello"));
        assert!(file.truncated);
    }

    #[test]
    fn streaming_json_fields_handle_split_names_and_escapes() {
        let mut fields = StreamingJsonStringFields::default();
        fields.push(r#"{"comm"#);
        fields.push(r#"and":"echo \"hi\" \u263A"}"#);
        let command = fields.field(&["command"]).expect("command field");
        assert_eq!(command.value, "echo \"hi\" ☺");
        assert!(!command.truncated);
    }

    #[test]
    fn streaming_json_fields_handle_partial_escape_and_unicode() {
        let mut fields = StreamingJsonStringFields::default();
        fields.push(r#"{"command":"echo \"hi\" \u263A"#);
        let command = fields.field(&["command"]).expect("partial command");
        assert_eq!(command.value, "echo \"hi\" ☺");
        assert!(command.truncated);

        let mut fields = StreamingJsonStringFields::default();
        fields.push(r#"{"command":"abcdef"}"#);
        let command = fields.field(&["command"]).expect("command");
        assert_eq!(command.value, "abcdef");
        assert!(!command.truncated);
    }

    #[tokio::test]
    async fn split_runtime_queues_clear_only_drains_followups() {
        let (followup_tx, mut followup_rx) = mpsc::channel(8);
        let (cancel_tx, mut cancel_rx) = mpsc::channel(8);
        let queued_followups = AtomicUsize::new(0);

        queued_followups.fetch_add(1, Ordering::AcqRel);
        followup_tx
            .send(FollowupCommand::UserMessage {
                client_id: ClientId::new(),
                runtime_context: None,
                text: "queued followup".to_owned(),
                placement: bcode_ipc::PromptPlacement::FollowUp,
                completion: None,
            })
            .await
            .expect("followup send should succeed");
        let (response, _completion) = oneshot::channel();
        cancel_tx
            .send(CancelCommand {
                clear_queue: true,
                requested_by: None,
                response,
            })
            .await
            .expect("cancel send should succeed");

        let cleared = drain_followup_commands(&mut followup_rx);
        queued_followups.fetch_sub(cleared, Ordering::AcqRel);

        assert_eq!(cleared, 1);
        assert_eq!(queued_followups.load(Ordering::Acquire), 0);
        assert!(followup_rx.try_recv().is_err());
        assert!(matches!(
            cancel_rx.try_recv(),
            Ok(CancelCommand {
                clear_queue: true,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn current_turn_transition_helpers_manage_provider_round_state() {
        let (_followup_tx, mut followup_rx) = mpsc::channel(1);
        let (_steering_tx, mut steering_rx) = mpsc::channel(1);
        let (_cancel_tx, mut cancel_rx) = mpsc::channel(1);
        let queued_followups = AtomicUsize::new(0);
        let current_turn = Arc::new(Mutex::new(None));
        let context = RuntimeCommandContext::new(
            &mut followup_rx,
            &mut steering_rx,
            &mut cancel_rx,
            &queued_followups,
            Arc::clone(&current_turn),
        );
        let cancel_state = Arc::new(TurnCancelState::default());

        begin_current_turn(
            &context,
            ClientId::new(),
            "turn-test".to_owned(),
            Arc::clone(&cancel_state),
        )
        .await;
        assert!(current_turn.lock().await.is_some());

        let model_turn = ActiveModelTurn {
            provider_plugin_id: Some("provider-test".to_owned()),
            provider_turn_id: "provider-turn-test".to_owned(),
            reuse_key: None,
            request_message_count: 1,
        };
        begin_provider_round(&context, model_turn.clone()).await;
        assert_eq!(
            current_turn
                .lock()
                .await
                .as_ref()
                .and_then(|turn| turn.model.as_ref())
                .map(|turn| turn.provider_turn_id.as_str()),
            Some("provider-turn-test")
        );

        let finished = finish_provider_round(&context)
            .await
            .expect("provider round should finish");
        assert_eq!(finished.provider_turn_id, model_turn.provider_turn_id);
        assert!(
            current_turn
                .lock()
                .await
                .as_ref()
                .is_some_and(|turn| turn.model.is_none())
        );

        finish_current_turn(&context).await;
        assert!(current_turn.lock().await.is_none());
    }
}
