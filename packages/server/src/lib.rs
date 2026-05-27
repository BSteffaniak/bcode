#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Local Bcode daemon runtime.

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
    IpcEndpoint, LocalIpcListener, LocalIpcStream, PermissionSummary, PluginServiceError,
    PluginServiceResponse, PluginServiceSummary, Request, Response, ResponsePayload, ServerStatus,
    ServerStopMode, SessionCatalogSourceStatus, SessionCatalogStatus, WorktreeCreateRequest,
    WorktreeListRequest, WorktreeRemoveRequest, decode, event_envelope, recv_envelope,
    response_envelope, send_envelope,
};
use bcode_model::{
    CancelTurnRequest, ContentBlock, FinishTurnRequest, ImageMetadata as ModelImageMetadata,
    ImageRefContent, MODEL_PROVIDER_INTERFACE_ID, MessageRole, ModelList, ModelMessage,
    ModelParameters, ModelTurnRequest, NativeWebSearchRequest, NativeWebSearchResponse,
    OP_CANCEL_TURN, OP_FINISH_TURN, OP_MODELS, OP_NATIVE_WEB_SEARCH, OP_POLL_TURN_EVENTS,
    OP_START_TURN, PollTurnEventsRequest, PollTurnEventsResponse, ProviderTurnEvent,
    ReasoningEffort, StartTurnResponse, TokenUsage,
};
use bcode_session::{CatalogLoadStatus, SessionManager};
use bcode_session_models::{
    ClientId, ModelTurnOutcome, ProviderStreamEvent, ProviderToolCallProgress, RuntimeWorkId,
    RuntimeWorkKind, RuntimeWorkStatus, SessionEventKind, SessionId, SessionTokenUsage,
    SessionTraceEvent, SessionTracePayload, SessionTracePhase, ToolInvocationStreamEvent,
    ToolOutputStream as SessionToolOutputStream, TraceBlobRef, TraceRedaction,
};
use bcode_skill::{SkillRegistry, SkillRegistryOptions, SkillSourceRoot};
use bcode_skill_models::{
    SkillActivationMode, SkillContextResponse, SkillId, SkillList, SkillSource, SkillSourceKind,
};
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID,
    ToolDefinition as ServiceToolDefinition, ToolInvocationRequest, ToolInvocationResponse,
    ToolInvocationStreamEvent as ServiceToolInvocationStreamEvent, ToolList, ToolOutputStream,
    ToolResultContent,
};
use runtime_work::{CancellationHandle, RuntimeWorkManager, RuntimeWorkSpec};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{WriteHalf, split};
use tokio::sync::{Mutex, Notify, broadcast, mpsc};

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
    /// Registry I/O error: {0}
    #[error("daemon lifecycle error: {0}")]
    DaemonLifecycle(#[from] bcode_daemon_lifecycle::DaemonLifecycleError),
    #[error("blocking task join error: {0}")]
    BlockingTask(#[from] tokio::task::JoinError),
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
    auto_compaction: bcode_config::CompactionConfig,
    skills: Option<SkillRegistry>,
    skill_context_bytes: usize,
    active_skills: Mutex<BTreeMap<SessionId, BTreeSet<SkillId>>>,
    turn_skills: Mutex<BTreeMap<(SessionId, u64), SkillTurnInvocation>>,
    session_runtimes: Mutex<BTreeMap<SessionId, SessionRuntimeHandle>>,
    active_session_turns: Mutex<BTreeMap<SessionId, ActiveSessionTurn>>,
    runtime_work: RuntimeWorkManager,
    active_turns: Mutex<BTreeMap<SessionId, ActiveModelTurn>>,
    session_model_selections: Mutex<BTreeMap<SessionId, SessionModelSelection>>,
    session_agent_selections: Mutex<BTreeMap<SessionId, String>>,
    pending_permissions: Mutex<BTreeMap<String, PendingPermission>>,
    next_permission_id: Mutex<u64>,
    clients: Mutex<BTreeSet<ClientId>>,
    client_runtime_contexts: Mutex<BTreeMap<ClientId, ClientRuntimeContext>>,
    client_session_namespaces: Mutex<BTreeMap<ClientId, String>>,
    active_session_namespaces: Mutex<BTreeMap<SessionId, String>>,
    message_accepted_clients: Mutex<BTreeSet<ClientId>>,
    event_clients: Mutex<Vec<SharedWriter>>,
    catalog_events_started: std::sync::atomic::AtomicBool,
    daemon_status: DaemonStatus,
    daemon_record_path: Option<PathBuf>,
    shutdown: broadcast::Sender<()>,
}

#[derive(Debug, Clone)]
struct SessionRuntimeHandle {
    commands: mpsc::Sender<SessionCommand>,
    queued_commands: Arc<AtomicUsize>,
    receiver: Arc<Mutex<Option<mpsc::Receiver<SessionCommand>>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MessageQueueStatus {
    queued: bool,
    queue_position: Option<u32>,
}

#[derive(Debug)]
enum SessionCommand {
    UserMessage {
        client_id: ClientId,
        runtime_context: Option<ClientRuntimeContext>,
        text: String,
    },
    SkillInvocation {
        client_id: ClientId,
        runtime_context: Option<ClientRuntimeContext>,
        skill_id: SkillId,
        arguments: String,
        source: Option<SkillSource>,
        display_text: String,
    },
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

#[derive(Debug, Clone)]
struct ActiveSessionTurn {
    turn_id: String,
    cancel_state: Arc<TurnCancelState>,
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
        })
    }
}

#[derive(Debug, Clone)]
struct PendingPermission {
    summary: PermissionSummary,
    decision: Arc<Mutex<Option<bool>>>,
    notify: Arc<Notify>,
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
    auto_compaction: bcode_config::CompactionConfig,
    skills: Option<SkillRegistry>,
    skill_context_bytes: usize,
    daemon_status: DaemonStatus,
    daemon_record_path: Option<PathBuf>,
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
            auto_compaction: init.auto_compaction,
            skills: init.skills,
            skill_context_bytes: init.skill_context_bytes,
            active_skills: Mutex::default(),
            turn_skills: Mutex::default(),
            session_runtimes: Mutex::default(),
            active_session_turns: Mutex::default(),
            runtime_work: RuntimeWorkManager::default(),
            active_turns: Mutex::default(),
            session_model_selections: Mutex::default(),
            session_agent_selections: Mutex::default(),
            pending_permissions: Mutex::default(),
            next_permission_id: Mutex::new(1),
            clients: Mutex::default(),
            client_runtime_contexts: Mutex::default(),
            client_session_namespaces: Mutex::default(),
            active_session_namespaces: Mutex::default(),
            message_accepted_clients: Mutex::default(),
            event_clients: Mutex::default(),
            catalog_events_started: std::sync::atomic::AtomicBool::new(false),
            daemon_status: init.daemon_status,
            daemon_record_path: init.daemon_record_path,
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
        let active_model_turns = self.active_turns.lock().await.len();
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
            .map(|handle| handle.queued_commands.load(Ordering::Acquire))
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
        }
    }

    async fn register_event_client(&self, writer: SharedWriter) {
        self.event_clients.lock().await.push(writer);
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
    }

    async fn event_client_writers(&self) -> Vec<SharedWriter> {
        self.event_clients.lock().await.clone()
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

fn static_bundled_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    vec![
        #[cfg(feature = "static-bundled-bedrock-provider-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/bedrock-provider-plugin/bcode-plugin.toml"),
            bcode_bedrock_provider_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-blims-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/blims-plugin/bcode-plugin.toml"),
            bcode_blims_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-default-agents-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
            bcode_default_agents_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-document-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/document-plugin/bcode-plugin.toml"),
            bcode_document_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-fake-provider-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
            bcode_fake_provider_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-filesystem-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
            bcode_filesystem_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-git-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/git-plugin/bcode-plugin.toml"),
            bcode_git_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-openai-compatible-provider-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/openai-compatible-provider-plugin/bcode-plugin.toml"),
            bcode_openai_compatible_provider_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-opencode-session-import-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/opencode-session-import-plugin/bcode-plugin.toml"),
            bcode_opencode_session_import_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-pi-session-import-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/pi-session-import-plugin/bcode-plugin.toml"),
            bcode_pi_session_import_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-shell-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-web-search-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/web-search-plugin/bcode-plugin.toml"),
            bcode_web_search_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-worktree-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/worktree-plugin/bcode-plugin.toml"),
            bcode_worktree_plugin::static_plugin(),
        ),
    ]
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
    let store = sshenv_vault::SshenvStore::new(sshenv_vault::SshenvStoreConfig::new(vault));
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
    tracing::debug!(target: "bcode_server::startup", "loading config");
    let config = bcode_config::load_config()?;
    tracing::debug!(target: "bcode_server::startup", "config loaded");
    let plugin_selection = bcode_plugin::PluginSelection::from(&config);
    tracing::debug!(
        target: "bcode_server::startup",
        enabled = ?plugin_selection.enabled,
        disabled = ?plugin_selection.disabled,
        "plugin selection resolved"
    );
    tracing::debug!(target: "bcode_server::startup", "loading plugins");
    let static_plugins = static_bundled_plugins();
    let plugin_configs = resolve_plugin_configs(&config, &static_plugins);
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled_and_config(
        &plugin_selection,
        &static_plugins,
        plugin_configs,
    )?;
    tracing::debug!(target: "bcode_server::startup", "plugins loaded");
    tracing::debug!(target: "bcode_server::startup", endpoint = ?endpoint, "binding IPC endpoint");
    let listener = LocalIpcListener::bind(&endpoint)?;
    let daemon_record = register_daemon(&endpoint)?;
    let daemon_status = daemon_status_from_record(&daemon_record);
    tracing::debug!(target: "bcode_server::startup", "IPC endpoint bound");
    tracing::debug!(target: "bcode_server::startup", "initializing lazy session services");
    let sessions = SessionManager::persistent_lazy(default_session_store_dir());
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
    let state = Arc::new(ServerState::new(
        sessions,
        plugins,
        ServerStateInit {
            selected_provider_plugin_id: resolved_model.provider_plugin_id,
            selected_model_id: resolved_model.model_id,
            selected_provider_context: bcode_model::ProviderRequestContext {
                model_profile: resolved_model.model_profile,
                auth_profile: resolved_model.auth_profile,
                settings: resolved_model.settings,
                auth: None,
                request: resolved_model.request,
                env: BTreeMap::new(),
            },
            prompt_cache_mode: config.model.prompt_cache.mode,
            conversation_reuse_mode: config.model.conversation_reuse.mode,
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
            auto_compaction: config.model.compaction,
            skill_context_bytes: config.skills.max_context_bytes,
            skills,
            daemon_status,
            daemon_record_path: Some(bcode_daemon_lifecycle::record_path(
                &bcode_config::default_state_dir(),
                &daemon_record.namespace,
            )),
        },
    ));
    state.start_catalog_event_forwarder();
    recover_abandoned_runtime_work(&state).await?;
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
        bcode_daemon_lifecycle::remove_record_path(path)?;
    }
    tracing::debug!(target: "bcode_server::startup", "shutdown complete");
    Ok(())
}

async fn recover_abandoned_runtime_work(state: &ServerState) -> Result<(), ServerError> {
    state.sessions.wait_catalog_loaded().await?;
    let summaries = state.sessions.all_session_summaries().await;
    for summary in summaries {
        recover_abandoned_session_runtime_work(state, summary.id).await?;
    }
    Ok(())
}

async fn recover_abandoned_session_runtime_work(
    state: &ServerState,
    session_id: SessionId,
) -> Result<(), ServerError> {
    let mut active = BTreeMap::<RuntimeWorkId, String>::new();
    for event in state.sessions.session_history(session_id).await? {
        match event.kind {
            SessionEventKind::RuntimeWorkStarted { work_id, label, .. } => {
                active.insert(work_id, label);
            }
            SessionEventKind::RuntimeWorkFinished { work_id, .. } => {
                active.remove(&work_id);
            }
            _ => {}
        }
    }
    for (work_id, label) in active {
        append_runtime_work_finished_event(
            state,
            session_id,
            work_id,
            RuntimeWorkStatus::Failed,
            Some(format!(
                "daemon stopped before runtime work finished: {label}"
            )),
        )
        .await;
    }
    Ok(())
}

async fn handle_client(stream: LocalIpcStream, state: Arc<ServerState>) -> Result<(), ServerError> {
    let client_id = ClientId::new();
    state.register_client(client_id).await;

    let result = handle_registered_client(stream, &state, client_id).await;
    state.unregister_client(client_id).await;
    result
}

async fn handle_registered_client(
    stream: LocalIpcStream,
    state: &Arc<ServerState>,
    client_id: ClientId,
) -> Result<(), ServerError> {
    let (mut reader, writer) = split(stream);
    let writer = Arc::new(Mutex::new(writer));
    let mut attached_session = None;

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

        let request = decode(&envelope.payload)?;
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

    if let Some(session_id) = attached_session
        && let Some(event) = state.sessions.detach_session(session_id, client_id).await?
    {
        publish_session_event(state, &event).await;
        state
            .deactivate_session_namespace_if_inactive(session_id)
            .await;
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
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
        Request::CreateSession {
            name,
            working_directory,
        } => handle_create_session(request_id, state, writer, name, working_directory).await,
        Request::ListSessions { working_directory } => {
            handle_list_sessions(request_id, state, writer, &working_directory).await
        }
        Request::SubscribeCatalogUpdates => {
            handle_subscribe_catalog_updates(request_id, state, writer).await
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
        Request::RenameSession { session_id, name } => {
            handle_rename_session(request_id, state, writer, session_id, name).await
        }
        Request::DeleteSession { session_id } => {
            handle_delete_session(request_id, state, writer, session_id).await
        }
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
            handle_user_message(request_id, client_id, state, writer, session_id, text).await
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
        } => handle_resolve_permission(request_id, state, writer, &permission_id, approved).await,
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
        Request::ListWorktrees(_) | Request::CreateWorktree(_) | Request::RemoveWorktree(_) => {
            unreachable!("worktree request routed to primary handler")
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
    if let Ok(history) = state.sessions.session_history(session.id).await
        && let Some(event) = history.last()
    {
        publish_session_event(state, event).await;
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
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    state.register_event_client(writer.clone()).await;
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
    if state.active_turns.lock().await.contains_key(&session_id) {
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
                    "worktree_list_failed",
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
        && state.active_turns.lock().await.contains_key(&session_id)
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
    let config = bcode_config::load_config()?;
    match bcode_worktree::create_worktree(&config, &request, &cwd) {
        Ok(mut response) => {
            if let Some(session_id) = request.attach_session_id {
                if let Some(event) = state
                    .sessions
                    .change_session_working_directory(session_id, response.path.clone())
                    .await?
                {
                    publish_session_event(state, &event).await;
                }
                response.session = Some(state.sessions.session_summary(session_id).await?);
            } else if request.new_session {
                let session = state
                    .sessions
                    .create_session(Some(request.name), response.path.clone())
                    .await?;
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
                    "worktree_create_failed",
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
                "worktree_remove_failed",
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
                    "worktree_remove_failed",
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
    if state.active_turns.lock().await.contains_key(&session_id) {
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
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
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
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
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

async fn handle_attach_session(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
    session_id: SessionId,
) -> Result<(), ServerError> {
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
            publish_session_event(state, &attachment.attached_event).await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::Attached {
                    session_id,
                    session: attachment.session,
                    history: compact_attach_history(attachment.history),
                    input_history: attachment.input_history,
                    import_warnings: Vec::new(),
                }),
            )
            .await?;
            forward_session_events(writer.clone(), attachment.events);
            Ok(())
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
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
    let client_namespace = state.client_session_namespace(client_id).await;
    if let Err(active_namespace) = state
        .try_activate_session_namespace(session_id, client_namespace)
        .await
    {
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    match state
        .sessions
        .attach_session_recent(session_id, client_id, limit)
        .await
    {
        Ok(attachment) => {
            restore_active_skills_from_history(&attachment.history, state, session_id).await;
            *attached_session = Some(session_id);
            publish_session_event(state, &attachment.attached_event).await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::Attached {
                    session_id,
                    session: attachment.session,
                    history: compact_attach_history(attachment.history),
                    input_history: attachment.input_history,
                    import_warnings: Vec::new(),
                }),
            )
            .await?;
            forward_session_events(writer.clone(), attachment.events);
            Ok(())
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
            )
            .await?;
            state
                .deactivate_session_namespace_if_inactive(session_id)
                .await;
            Ok(())
        }
    }
}

async fn enqueue_session_command(
    state: &Arc<ServerState>,
    session_id: SessionId,
    command: SessionCommand,
) -> Result<MessageQueueStatus, ServerError> {
    state.sessions.ensure_session_current(session_id).await?;
    state.sessions.session_summary(session_id).await?;
    let handle = session_runtime_handle(state, session_id).await;
    let pending_before = handle.queued_commands.fetch_add(1, Ordering::AcqRel);
    let queued = pending_before > 0 || state.active_turns.lock().await.contains_key(&session_id);
    let queue_position = queued.then(|| usize_to_u32_saturating(pending_before.saturating_add(1)));
    if handle.commands.send(command).await.is_ok() {
        return Ok(MessageQueueStatus {
            queued,
            queue_position,
        });
    }
    handle.queued_commands.fetch_sub(1, Ordering::AcqRel);

    state.session_runtimes.lock().await.remove(&session_id);
    Err(bcode_session::SessionError::NotFound(session_id).into())
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

async fn session_runtime_handle(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> SessionRuntimeHandle {
    let mut runtimes = state.session_runtimes.lock().await;
    if let Some(handle) = runtimes.get(&session_id) {
        return handle.clone();
    }

    let (commands, receiver) = mpsc::channel(128);
    let queued_commands = Arc::new(AtomicUsize::new(0));
    let receiver = Arc::new(Mutex::new(Some(receiver)));
    let handle = SessionRuntimeHandle {
        commands,
        queued_commands: Arc::clone(&queued_commands),
        receiver: Arc::clone(&receiver),
    };
    runtimes.insert(session_id, handle.clone());
    drop(runtimes);
    let state_for_runtime = Arc::clone(state);
    tokio::spawn(async move {
        run_session_runtime(state_for_runtime, session_id, receiver, queued_commands).await;
    });
    handle
}

async fn run_session_runtime(
    state: Arc<ServerState>,
    session_id: SessionId,
    commands: Arc<Mutex<Option<mpsc::Receiver<SessionCommand>>>>,
    queued_commands: Arc<AtomicUsize>,
) {
    let mut permit = SessionTurnPermit::new(session_id);
    let mut commands = commands
        .lock()
        .await
        .take()
        .expect("session runtime receiver should be present");
    while let Some(command) = commands.recv().await {
        queued_commands.fetch_sub(1, Ordering::AcqRel);
        match command {
            SessionCommand::UserMessage {
                client_id,
                runtime_context,
                text,
            } => {
                process_user_message_command(&state, &mut permit, client_id, runtime_context, text)
                    .await;
            }
            SessionCommand::SkillInvocation {
                client_id,
                runtime_context,
                skill_id,
                arguments,
                source,
                display_text,
            } => {
                process_skill_invocation_command(
                    &state,
                    &mut permit,
                    client_id,
                    runtime_context,
                    skill_id,
                    arguments,
                    source,
                    display_text,
                )
                .await;
            }
        }
    }
    state.session_runtimes.lock().await.remove(&session_id);
}

async fn process_user_message_command(
    state: &ServerState,
    permit: &mut SessionTurnPermit,
    client_id: ClientId,
    runtime_context: Option<ClientRuntimeContext>,
    text: String,
) {
    match append_turn_user_message(state, permit, client_id, text).await {
        Ok(Some(user_event)) => {
            suggest_skills_for_prompt(state, permit.session_id(), &user_event).await;
            run_model_turn(state, permit, &user_event, runtime_context).await;
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
                format!("failed to append user message: {error}"),
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_skill_invocation_command(
    state: &ServerState,
    permit: &mut SessionTurnPermit,
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

    match append_turn_user_message(state, permit, client_id, display_text).await {
        Ok(Some(user_event)) => {
            state.turn_skills.lock().await.insert(
                (permit.session_id(), user_event.sequence),
                SkillTurnInvocation {
                    skill_id,
                    arguments,
                },
            );
            run_model_turn(state, permit, &user_event, runtime_context).await;
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
        ResponsePayload::MessageAccepted {
            queued: status.queued,
            queue_position: status.queue_position,
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
    let command = SessionCommand::SkillInvocation {
        client_id,
        runtime_context: state.client_runtime_context(client_id).await,
        skill_id,
        arguments,
        source: Some(summary.source),
        display_text,
    };
    match enqueue_session_command(state, session_id, command).await {
        Ok(status) => {
            send_message_acceptance_response(state, writer, request_id, client_id, status).await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
            )
            .await
        }
    }
}

async fn handle_user_message(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    session_id: SessionId,
    text: String,
) -> Result<(), ServerError> {
    if let Some(active_namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return send_incompatible_active_session_response(writer, request_id, &active_namespace)
            .await;
    }
    match enqueue_session_command(
        state,
        session_id,
        SessionCommand::UserMessage {
            client_id,
            runtime_context: state.client_runtime_context(client_id).await,
            text,
        },
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
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
            )
            .await
        }
    }
}

fn reasoning_capabilities_from_config(
    reasoning: &bcode_config::ReasoningConfig,
) -> Option<bcode_model::ModelReasoningInfo> {
    (!reasoning.effort_values.is_empty()
        || !reasoning.summary_values.is_empty()
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
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
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
    {
        let mut selections = state.session_model_selections.lock().await;
        let selection = selections
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
        if effort.is_some() {
            selection.reasoning_effort = effort;
        }
        if summary.is_some() {
            selection.reasoning_summary = summary;
        }
    }
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionModelSet),
    )
    .await
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
    let models = invoke_model_provider_json_blocking::<_, ModelList>(
        state,
        selection.provider_plugin_id.clone(),
        OP_MODELS,
        serde_json::Value::Null,
    )
    .await
    .ok();
    let model = models
        .as_ref()
        .and_then(|models| select_model_info(&models.models, selection.model_id.as_deref()));
    let model_id = selection
        .model_id
        .clone()
        .or_else(|| model.as_ref().map(|model| model.model_id.clone()));
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionModelStatus {
            status: bcode_ipc::SessionModelStatus {
                provider_plugin_id: selection.provider_plugin_id,
                model_id,
                context_window: model.as_ref().and_then(|model| model.context_window),
                max_output_tokens: model.as_ref().and_then(|model| model.max_output_tokens),
                reasoning: selection
                    .reasoning_capabilities
                    .or_else(|| model.as_ref().and_then(|model| model.reasoning.clone())),
                reasoning_effort: selection.reasoning_effort,
                reasoning_summary: selection.reasoning_summary,
            },
        }),
    )
    .await
}

async fn handle_session_model_list(
    request_id: u64,
    client_id: ClientId,
    state: &ServerState,
    writer: &SharedWriter,
    provider_plugin_id: Option<String>,
) -> Result<(), ServerError> {
    let selected_provider_plugin_id = provider_plugin_id.or_else(|| {
        state
            .client_runtime_contexts
            .try_lock()
            .ok()
            .and_then(|contexts| contexts.get(&client_id).cloned())
            .and_then(|context| context.selected_provider_plugin_id)
    });
    match invoke_model_provider_json_blocking::<_, ModelList>(
        state,
        selected_provider_plugin_id.clone(),
        OP_MODELS,
        serde_json::Value::Null,
    )
    .await
    {
        Ok(models) => {
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
        .or_else(|| models.iter().find(|model| model.is_default))
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
        .append_agent_changed(session_id, resolved_agent_id.clone())
        .await
    {
        Ok(event) => {
            state
                .session_agent_selections
                .lock()
                .await
                .insert(session_id, resolved_agent_id);
            publish_session_event(state, &event).await;
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
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
            )
            .await
        }
    }
}

async fn handle_cancel_session_turn(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    client_id: ClientId,
    clear_queue: bool,
) -> Result<(), ServerError> {
    let Some(active_session_turn) = state
        .active_session_turns
        .lock()
        .await
        .get(&session_id)
        .cloned()
    else {
        return send_response(
            writer,
            request_id,
            Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled: false }),
        )
        .await;
    };

    active_session_turn.cancel_state.cancel();
    if clear_queue {
        clear_session_command_queue(state, session_id).await;
    }
    append_model_turn_cancel_requested_event(
        state,
        session_id,
        active_session_turn.turn_id.clone(),
        Some(client_id),
    )
    .await;
    cancel_registered_runtime_work(
        state,
        session_id,
        RuntimeWorkId::new(format!("model_{}", active_session_turn.turn_id)),
        Some(client_id),
    )
    .await;

    let active_turn = state.active_turns.lock().await.get(&session_id).cloned();
    let Some(active_turn) = active_turn else {
        return send_response(
            writer,
            request_id,
            Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled: true }),
        )
        .await;
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
    match cancel_result {
        Ok(_) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled: true }),
            )
            .await
        }
        Err(error) => {
            append_system_event(
                state,
                session_id,
                format!("provider turn cancellation failed: {error}"),
            )
            .await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled: true }),
            )
            .await
        }
    }
}

async fn clear_session_command_queue(state: &ServerState, session_id: SessionId) {
    let Some(handle) = state
        .session_runtimes
        .lock()
        .await
        .get(&session_id)
        .cloned()
    else {
        return;
    };
    let mut cleared = 0_usize;
    if let Some(receiver) = handle.receiver.lock().await.as_mut() {
        while receiver.try_recv().is_ok() {
            cleared = cleared.saturating_add(1);
        }
    }
    if cleared > 0 {
        handle.queued_commands.fetch_sub(cleared, Ordering::AcqRel);
    }
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
        .session_history(session_id)
        .await?
        .into_iter()
        .filter(|event| {
            matches!(
                event.kind,
                SessionEventKind::RuntimeWorkStarted { .. }
                    | SessionEventKind::RuntimeWorkCancelRequested { .. }
                    | SessionEventKind::RuntimeWorkProgress { .. }
                    | SessionEventKind::RuntimeWorkFinished { .. }
            )
        })
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

async fn handle_compact_session(
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
    let selection = session_model_selection_with_runtime_context(
        state,
        session_id,
        state.client_runtime_context(client_id).await,
    )
    .await;
    match compact_session_context(state, session_id, &selection).await {
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
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
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
) -> Result<(), ServerError> {
    let Some(permission) = state.pending_permissions.lock().await.remove(permission_id) else {
        return send_response(
            writer,
            request_id,
            Response::Ok(ResponsePayload::PermissionResolved { resolved: false }),
        )
        .await;
    };
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
}

#[derive(Debug, Default)]
struct ModelStreamProgress {
    active_tool_call: Option<ToolArgumentStreamProgress>,
}

impl ModelStreamProgress {
    fn start_tool_call(&mut self, call_id: String, name: String) {
        self.active_tool_call = Some(ToolArgumentStreamProgress {
            call_id,
            name,
            argument_bytes: 0,
        });
    }

    fn record_completed_tool_call(&mut self, call: &bcode_model::ToolCall) {
        if self
            .active_tool_call
            .as_ref()
            .is_none_or(|active| active.call_id != call.id)
        {
            self.start_tool_call(call.id.clone(), call.name.clone());
        }
        if let Some(active) = self.active_tool_call.as_mut() {
            active.argument_bytes = serialized_tool_argument_len(&call.arguments);
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

#[derive(Default)]
struct ModelTurnRecoveryState {
    retried_after_context_overflow: bool,
    retried_after_malformed_tool_arguments: bool,
    retry_instruction: Option<&'static str>,
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
    compact_session_context_with_limit(state, session_id, selection, None).await
}

async fn compact_session_context_before_sequence(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    first_kept_sequence: u64,
) -> Result<String, CompactionError> {
    compact_session_context_with_limit(state, session_id, selection, Some(first_kept_sequence))
        .await
}

async fn compact_session_context_with_limit(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    first_kept_sequence: Option<u64>,
) -> Result<String, CompactionError> {
    if state.active_turns.lock().await.contains_key(&session_id) {
        return Err(CompactionError::Busy);
    }

    let history = state.sessions.session_history(session_id).await?;
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

    let summary = collect_compaction_summary(state, session_id, selection, &transcript).await?;
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
    if state.active_turns.lock().await.contains_key(&session_id) {
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
    match collect_compaction_summary_once(state, session_id, selection, transcript, &prompt_text)
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
) -> Result<String, String> {
    let turn_id = format!(
        "{session_id}-compact-{}",
        transcript.compacted_through_sequence
    );
    let request = build_compaction_request(session_id, selection, prompt_text, turn_id.clone());
    let start = invoke_model_provider_json_blocking::<_, StartTurnResponse>(
        state,
        selection.provider_plugin_id.clone(),
        OP_START_TURN,
        request,
    )
    .await?;

    let provider_turn_id = start.provider_turn_id;
    let result = poll_compaction_summary(state, session_id, selection, &provider_turn_id, &turn_id)
        .await
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
        "Bcode compacted older session context locally because the provider compaction request could not be used: {reason}. The full canonical history remains in the session event log."
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
        let response = poll_model_turn(state, selection.provider_plugin_id.as_deref(), &poll)
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
            | ProviderTurnEvent::ProviderMetadata { .. } => {}
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
    runtime_context: Option<ClientRuntimeContext>,
) {
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
    state.active_session_turns.lock().await.insert(
        session_id,
        ActiveSessionTurn {
            turn_id: turn_id.clone(),
            cancel_state: Arc::clone(&cancel_state),
        },
    );
    append_model_turn_started_event(state, session_id, turn_id.clone()).await;
    let completion = run_model_turn_inner(
        state,
        session_id,
        trigger_event,
        runtime_context,
        Arc::clone(&cancel_state),
    )
    .await;
    state.active_session_turns.lock().await.remove(&session_id);
    state.active_turns.lock().await.remove(&session_id);
    append_model_turn_finished_event(
        state,
        session_id,
        turn_id,
        completion.outcome,
        completion.message.clone(),
    )
    .await;
    finish_registered_runtime_work(
        state,
        session_id,
        model_work_id,
        runtime_work_status_from_model_outcome(completion.outcome),
        completion.message,
    )
    .await;
}

#[allow(clippy::too_many_lines)]
async fn run_model_turn_inner(
    state: &ServerState,
    session_id: SessionId,
    trigger_event: &bcode_session_models::SessionEvent,
    runtime_context: Option<ClientRuntimeContext>,
    cancel_state: Arc<TurnCancelState>,
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
        match maybe_retry_after_provider_error(
            state,
            session_id,
            trigger_event.sequence,
            &request.turn_id,
            &outcome,
            &selection,
            &mut recovery,
        )
        .await
        {
            ModelTurnRetry::Continue => continue,
            ModelTurnRetry::Return(completion) => return completion,
            ModelTurnRetry::None => {}
        }
        if outcome.provider_error.is_none() {
            recovery.retry_instruction = None;
        }
        if let Some(completion) = outcome.completion.clone() {
            append_deferred_provider_error_if_needed(state, session_id, &outcome).await;
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
    trigger_event_sequence: u64,
    turn_id: &str,
    outcome: &ModelPollOutcome,
    selection: &SessionModelSelection,
    recovery: &mut ModelTurnRecoveryState,
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
            turn_id,
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
            selection,
            trigger_event_sequence,
            error,
        )
        .await
        {
            Ok(()) => ModelTurnRetry::Continue,
            Err(completion) => ModelTurnRetry::Return(completion),
        };
    }

    ModelTurnRetry::None
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
) {
    if let Some(error) = outcome.provider_error.as_ref()
        && should_defer_visible_provider_error(error)
    {
        append_system_event(state, session_id, provider_error_message(error)).await;
    }
}

fn should_defer_visible_provider_error(error: &bcode_model::ProviderError) -> bool {
    is_context_length_provider_error(error) || is_tool_arguments_decode_provider_error(error)
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

#[allow(clippy::too_many_lines)]
async fn run_model_turn_round(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    request: &ModelTurnRequest,
    cancel_state: Arc<TurnCancelState>,
) -> Result<ModelPollOutcome, ModelTurnCompletion> {
    let round_start = Instant::now();
    let provider_label = provider_plugin_id.unwrap_or("<auto>").to_string();
    if cancel_state.is_cancelled() {
        return Err(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Cancelled,
            "model turn cancelled",
        ));
    }
    let start = invoke_model_provider_json_blocking::<_, StartTurnResponse>(
        state,
        provider_plugin_id.map(ToString::to_string),
        OP_START_TURN,
        request.clone(),
    )
    .await;
    let start = match start {
        Ok(start) => start,
        Err(error) => {
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
    };

    state.active_turns.lock().await.insert(
        session_id,
        ActiveModelTurn {
            provider_plugin_id: provider_plugin_id.map(ToString::to_string),
            provider_turn_id: start.provider_turn_id.clone(),
            reuse_key: request.conversation_reuse.key.clone(),
            request_message_count: request.messages.len(),
        },
    );

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
    )
    .await;

    ensure_terminal_poll_outcome(state, session_id, &mut outcome).await;

    if !assistant_text.is_empty() {
        append_assistant_message_event(state, session_id, assistant_text).await;
    }

    let active_turn = state.active_turns.lock().await.remove(&session_id);
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
    let _ = invoke_model_provider_json_blocking::<_, bcode_model::AckResponse>(
        state,
        active_turn.and_then(|turn| turn.provider_plugin_id),
        OP_FINISH_TURN,
        finish,
    )
    .await;
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
        .active_session_turns
        .lock()
        .await
        .get(&session_id)
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

async fn poll_model_turn_events(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    provider_turn_id: &str,
    turn_id: &str,
    cancel_state: Arc<TurnCancelState>,
) -> (String, ModelPollOutcome) {
    let mut assistant_text = String::new();
    let mut outcome = ModelPollOutcome::default();
    let mut stream_progress = ModelStreamProgress::default();
    let mut idle_for = Duration::ZERO;
    let mut no_progress_warned = false;
    loop {
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
        let response = poll_model_turn(state, provider_plugin_id, &poll).await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let message = format!("model provider error: {error}");
                append_system_event(state, session_id, message.clone()).await;
                outcome.completion = Some(ModelTurnCompletion::with_message(
                    ModelTurnOutcome::Error,
                    message,
                ));
                break;
            }
        };
        if response.events.is_empty() {
            let Some(next_idle_for) = wait_for_model_progress_or_timeout(
                state,
                session_id,
                idle_for,
                &mut no_progress_warned,
                cancel_state.as_ref(),
                stream_progress.tool_progress_snapshot(),
                &mut outcome,
            )
            .await
            else {
                break;
            };
            idle_for = next_idle_for;
            continue;
        }
        let saw_progress = model_events_include_progress(&response.events);
        for event in response.events {
            handle_provider_turn_event(
                state,
                session_id,
                turn_id,
                event,
                &mut assistant_text,
                &mut outcome,
                &mut stream_progress,
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
            )
            .await
            else {
                break;
            };
            idle_for = next_idle_for;
        }
    }
    (assistant_text, outcome)
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
        ProviderTurnEvent::ToolCallDelta { .. }
        | ProviderTurnEvent::TurnStarted
        | ProviderTurnEvent::Usage { .. }
        | ProviderTurnEvent::Warning { .. }
        | ProviderTurnEvent::ProviderMetadata { .. }
        | ProviderTurnEvent::Error { .. }
        | ProviderTurnEvent::Cancelled
        | ProviderTurnEvent::TurnFinished { .. } => false,
    }
}

async fn wait_for_model_progress_or_timeout(
    state: &ServerState,
    session_id: SessionId,
    idle_for: Duration,
    warned: &mut bool,
    cancel_state: &TurnCancelState,
    active_tool_call: Option<ProviderToolCallProgress>,
    outcome: &mut ModelPollOutcome,
) -> Option<Duration> {
    let idle_for = idle_for.saturating_add(MODEL_POLL_INTERVAL);
    let warning_after = Duration::from_secs(state.model_streaming.no_progress_warning_secs);
    let timeout_after = Duration::from_secs(state.model_streaming.no_progress_timeout_secs);
    if !*warned && idle_for >= warning_after {
        append_provider_stream_event_trace(
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
    provider_plugin_id: Option<&str>,
    poll: &PollTurnEventsRequest,
) -> Result<PollTurnEventsResponse, String> {
    invoke_model_provider_json_blocking::<_, PollTurnEventsResponse>(
        state,
        provider_plugin_id.map(ToString::to_string),
        OP_POLL_TURN_EVENTS,
        poll.clone(),
    )
    .await
}

#[allow(clippy::too_many_lines)]
async fn handle_provider_turn_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    event: ProviderTurnEvent,
    assistant_text: &mut String,
    outcome: &mut ModelPollOutcome,
    stream_progress: &mut ModelStreamProgress,
) {
    if state
        .active_session_turns
        .lock()
        .await
        .get(&session_id)
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
            assistant_text.push_str(&text);
            append_assistant_delta_event(state, session_id, text).await;
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
                assistant_text,
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
        ProviderTurnEvent::Usage { usage } => {
            append_provider_event_trace(state, session_id, turn_id, "usage", None).await;
            update_provider_usage_state(state, session_id, &usage).await;
            append_model_usage_event(state, session_id, turn_id.to_string(), usage).await;
        }
        ProviderTurnEvent::ProviderMetadata { key, value } => {
            handle_provider_metadata_event(state, session_id, turn_id, key, value).await;
        }
        ProviderTurnEvent::TurnStarted => {
            append_provider_stream_event_trace(
                state,
                session_id,
                turn_id,
                ProviderStreamEvent::TurnStarted,
            )
            .await;
        }
        ProviderTurnEvent::ToolCallStarted { call_id, name } => {
            stream_progress.start_tool_call(call_id.clone(), name.clone());
            append_provider_stream_event_trace(
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
            let _ = state
                .sessions
                .append_event(
                    session_id,
                    SessionEventKind::AssistantReasoningDelta { text },
                )
                .await;
            append_provider_event_trace(state, session_id, turn_id, "reasoning_delta", None).await;
        }
        ProviderTurnEvent::ToolCallDelta { .. } => {}
    }
}

async fn handle_provider_error_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    error: bcode_model::ProviderError,
    outcome: &mut ModelPollOutcome,
) {
    let message = provider_error_message(&error);
    let defer_visible_message = should_defer_visible_provider_error(&error);
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
    assistant_text: &mut String,
) {
    append_provider_stream_event_trace(
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
    append_provider_stream_event_trace(
        state,
        session_id,
        turn_id,
        ProviderStreamEvent::ToolCallFinished {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
        },
    )
    .await;
    if !assistant_text.is_empty() {
        append_assistant_message_event(state, session_id, std::mem::take(assistant_text)).await;
    }
    let Some(cancel_state) = active_turn_cancel_state(state, session_id).await else {
        return;
    };
    if cancel_state.is_cancelled() {
        return;
    }
    execute_model_tool(state, session_id, call, Arc::clone(&cancel_state)).await;
}

async fn active_turn_cancel_state(
    state: &ServerState,
    session_id: SessionId,
) -> Option<Arc<TurnCancelState>> {
    state
        .active_session_turns
        .lock()
        .await
        .get(&session_id)
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

async fn append_provider_stream_event_trace(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    event: ProviderStreamEvent,
) {
    append_trace_event(
        state,
        session_id,
        Some(turn_id.to_string()),
        SessionTracePhase::ModelProviderEvent,
        SessionTracePayload::ProviderStreamEvent(event),
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
        .active_turns
        .lock()
        .await
        .get(&session_id)
        .and_then(|turn| turn.reuse_key.clone());
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
    if key != "provider_response_id" {
        return;
    }
    let reuse_key = state
        .active_turns
        .lock()
        .await
        .get(&session_id)
        .and_then(|turn| turn.reuse_key.clone());
    let Some(reuse_key) = reuse_key else {
        return;
    };
    let reusable_message_count = state
        .active_turns
        .lock()
        .await
        .get(&session_id)
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
    state
        .plugins
        .invoke_service_by_interface_json::<_, PolicyStatusResponse>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_POLICY_STATUS,
            &serde_json::json!({}),
        )
        .await
        .ok()
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
    if let Some(context) = runtime_context {
        let selection = SessionModelSelection {
            provider_plugin_id: context.selected_provider_plugin_id,
            model_id: context.selected_model_id,
            thinking_level: None,
            reasoning_effort: state.selected_reasoning.effort.clone(),
            reasoning_summary: state.selected_reasoning.summary.clone(),
            reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
            provider_context: context.provider_context,
        };
        return selection;
    }
    session_model_selection(state, session_id).await
}

async fn session_model_selection(
    state: &ServerState,
    session_id: SessionId,
) -> SessionModelSelection {
    if let Some(selection) = state.session_model_selections.lock().await.get(&session_id) {
        return selection.clone();
    }
    let selection = if let Ok(Some((provider, model))) =
        state.sessions.current_model_selection(session_id).await
    {
        SessionModelSelection {
            provider_plugin_id: provider_to_selection(&provider),
            model_id: model_to_selection(&model),
            thinking_level: None,
            reasoning_effort: state.selected_reasoning.effort.clone(),
            reasoning_summary: state.selected_reasoning.summary.clone(),
            reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
            provider_context: state.selected_provider_context.clone(),
        }
    } else {
        SessionModelSelection {
            provider_plugin_id: state.selected_provider_plugin_id.clone(),
            model_id: state.selected_model_id.clone(),
            thinking_level: None,
            reasoning_effort: state.selected_reasoning.effort.clone(),
            reasoning_summary: state.selected_reasoning.summary.clone(),
            reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
            provider_context: state.selected_provider_context.clone(),
        }
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
    match provider_plugin_id.as_deref() {
        Some(provider_plugin_id) => {
            state
                .plugins
                .invoke_service_json::<_, R>(
                    provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    operation,
                    &request,
                )
                .await
        }
        None => {
            state
                .plugins
                .invoke_service_by_interface_json::<_, R>(
                    MODEL_PROVIDER_INTERFACE_ID,
                    operation,
                    &request,
                )
                .await
        }
    }
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
    let history = state.sessions.model_context_events(session_id).await?;
    let mut messages =
        session_events_to_model_messages_with_limit(&history, state.tool_output_context_chars);
    let prompt_cache = plan_prompt_cache(&mut messages, state.prompt_cache_mode);
    let agent_id = session_agent_selection(state, session_id).await;
    let agent_context = agent_context(state, session_id, &agent_id).await;
    let working_directory = state.sessions.session_working_directory(session_id).await?;
    let (system_prompt, dynamic_system_context) = build_coding_system_prompt_parts(
        &working_directory,
        agent_context
            .as_ref()
            .and_then(|context| context.system_prompt_suffix.as_deref()),
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
    let skill_contexts = turn_skill_contexts(state, session_id, trigger_event.sequence).await;
    for skill_context in skill_contexts {
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
                },
            )
            .await;
    }
    let enabled_tools = agent_context
        .as_ref()
        .and_then(|context| context.enabled_tools.clone());
    let tools = collect_model_tools(state, enabled_tools).await;
    let parameters = {
        let mut p = ModelParameters::default();
        if let Some(level) = &selection.thinking_level {
            p.reasoning_effort = Some(*level);
        }
        if let Some(effort) = &selection.reasoning_effort {
            p.reasoning_effort_value = Some(effort.clone());
        }
        if let Some(summary) = &selection.reasoning_summary {
            p.reasoning_summary = Some(summary.clone());
        }
        p
    };
    let model_id = model_id_for_provider_request(selected_model_id);
    let projection = ConversationProjection::new(
        session_id,
        provider_plugin_id.unwrap_or("<auto>"),
        &model_id,
        &system_prompt,
        &tools,
        &parameters,
        &messages,
    );
    let conversation_reuse = plan_conversation_reuse(state, &projection, messages.len()).await;
    let mut metadata = projection.metadata();
    insert_reasoning_metadata(&mut metadata, &parameters);
    Ok(ModelTurnRequest {
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
    })
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
    let request_blob = (state.observability.persist_model_requests
        || state.observability.debug_enabled())
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
    .flatten();
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
            metadata: request.metadata.clone(),
            request: request_blob,
        },
    )
    .await;
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
    let previous = state
        .provider_state
        .lock()
        .await
        .records
        .get(&reuse_key)
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
* Before finishing a coding task, run the most relevant formatting, check, or test command when practical.
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
    agent_prompt_suffix: Option<&str>,
) -> (String, String) {
    let (stable_context, dynamic_context) = build_repository_context_parts(cwd);
    let mut stable = format!(
        "{DEFAULT_CODING_SYSTEM_PROMPT}\n\n{}",
        truncate_text(&stable_context, MAX_REPOSITORY_CONTEXT_CHARS)
    );
    if let Some(suffix) = agent_prompt_suffix
        && !suffix.trim().is_empty()
    {
        stable.push_str("\n\nAgent-specific instructions:\n");
        stable.push_str(suffix.trim());
    }

    (
        stable,
        truncate_text(&dynamic_context, MAX_REPOSITORY_CONTEXT_CHARS),
    )
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
    if let Some(branch) = run_command(context_root, "git", &["branch", "--show-current"])
        && !branch.is_empty()
    {
        dynamic_lines.push(format!("* Git branch: {branch}"));
    }
    if let Some(status) = run_command(context_root, "git", &["status", "--short"]) {
        dynamic_lines.push(format!(
            "* Git status:\n{}",
            format_block_or_placeholder(&status, "clean")
        ));
    }

    (stable_lines.join("\n"), dynamic_lines.join("\n"))
}

fn discover_git_root(cwd: &Path) -> Option<PathBuf> {
    run_command(cwd, "git", &["rev-parse", "--show-toplevel"])
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
    let marker = format!(
        "\n\n[tool output truncated for model context: original {char_count} chars / {} bytes. Full output saved at: {path}. Use filesystem.read on that path if more context is needed.]\n\n",
        result.len()
    );
    if max_context_chars == 0 {
        return marker.trim().to_string();
    }

    let marker_chars = marker.chars().count();
    if marker_chars >= max_context_chars {
        return marker.chars().take(max_context_chars).collect();
    }
    let head_chars = max_context_chars.saturating_sub(marker_chars);
    let mut output = result.chars().take(head_chars).collect::<String>();
    output.push_str(&marker);
    output
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
            Ok(list) => {
                tools.extend(
                    list.tools
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
                                bcode_tool::ToolSideEffect::ReadOnly => {
                                    bcode_model::ToolSideEffect::ReadOnly
                                }
                                bcode_tool::ToolSideEffect::WriteFiles => {
                                    bcode_model::ToolSideEffect::WriteFiles
                                }
                                bcode_tool::ToolSideEffect::ExecuteProcess => {
                                    bcode_model::ToolSideEffect::ExecuteProcess
                                }
                            },
                            requires_permission: tool.requires_permission,
                        }),
                );
            }
            Err(error) => eprintln!("failed to list tools from {plugin_id}: {error}"),
        }
    }
    tools
}

async fn invoke_model_native_web_search_tool(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
) -> Result<ToolInvocationResponse, String> {
    let selection = session_model_selection(state, session_id).await;
    let request = NativeWebSearchRequest {
        query: call
            .arguments
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        max_results: call
            .arguments
            .get("max_results")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok()),
        site: call
            .arguments
            .get("site")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        freshness: call
            .arguments
            .get("freshness")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        region: call
            .arguments
            .get("region")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        safe_search: call
            .arguments
            .get("safe_search")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        provider_context: selection.provider_context,
        metadata: BTreeMap::from([("tool_call_id".to_string(), call.id.clone())]),
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
    })
}

async fn execute_model_tool(
    state: &ServerState,
    session_id: SessionId,
    call: bcode_model::ToolCall,
    cancel_state: Arc<TurnCancelState>,
) {
    append_tool_request_event(
        state,
        session_id,
        call.id.clone(),
        call.name.clone(),
        serde_json::to_string(&call.arguments).unwrap_or_default(),
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
            call.id,
            "tool skipped because model turn was cancelled".to_string(),
            true,
            Vec::new(),
            None,
        )
        .await;
        return;
    }
    let tool_start = Instant::now();
    let result = invoke_model_tool(state, session_id, &call, cancel_state.as_ref())
        .await
        .unwrap_or_else(|error| ToolInvocationResponse {
            output: error,
            is_error: true,
            content: Vec::new(),
            full_output: None,
        });
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
        call.id,
        result.output,
        result.is_error,
        result.content,
        output_blob,
    )
    .await;
}

#[allow(clippy::too_many_lines)]
async fn invoke_model_tool(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
    cancel_state: &TurnCancelState,
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
        });
    }
    if call.name == "web.search"
        && call
            .arguments
            .get("provider")
            .and_then(serde_json::Value::as_str)
            == Some("model_native")
    {
        return invoke_model_native_web_search_tool(state, session_id, call).await;
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
    match agent_decision.decision {
        AgentDecision::Deny => {
            return Ok(ToolInvocationResponse {
                output: agent_decision
                    .reason
                    .unwrap_or_else(|| "tool denied by active agent policy".to_string()),
                is_error: true,
                content: Vec::new(),
                full_output: None,
            });
        }
        AgentDecision::Ask => {
            if !request_tool_permission(state, session_id, call, &definition, cancel_state).await {
                return Ok(ToolInvocationResponse {
                    output: "permission denied".to_string(),
                    is_error: true,
                    content: Vec::new(),
                    full_output: None,
                });
            }
        }
        AgentDecision::Allow => {}
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
    let mut invocation = state
        .plugins
        .invoke_service_with_events(
            &plugin_id,
            TOOL_SERVICE_INTERFACE_ID,
            OP_INVOKE_TOOL,
            payload,
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
    let response = loop {
        tokio::select! {
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
                });
            }
            Some(payload) = invocation.events.recv() => {
                if let Ok(event) = serde_json::from_slice::<ServiceToolInvocationStreamEvent>(&payload) {
                    append_tool_stream_event(state, session_id, convert_tool_stream_event(event)).await;
                }
            }
            response = &mut invocation.response => {
                break response
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?;
            }
        }
    };
    while let Ok(payload) = invocation.events.try_recv() {
        if let Ok(event) = serde_json::from_slice::<ServiceToolInvocationStreamEvent>(&payload) {
            append_tool_stream_event(state, session_id, convert_tool_stream_event(event)).await;
        }
    }
    bcode_plugin::decode_service_response(response).map_err(|error| error.to_string())
}

/// Append durable tool stream lifecycle events or publish ephemeral output deltas.
///
/// `OutputDelta` carries raw live tool output, including PTY bytes. These chunks
/// are intentionally transient: they are broadcast to currently attached clients
/// and must not be appended to the session event log. Durable history stores the
/// tool request, stream lifecycle metadata, final status, and final bounded tool
/// result instead.
async fn append_tool_stream_event(
    state: &ServerState,
    session_id: SessionId,
    event: ToolInvocationStreamEvent,
) {
    if matches!(event, ToolInvocationStreamEvent::OutputDelta { .. }) {
        let _ = state
            .sessions
            .publish_transient_event(session_id, SessionEventKind::ToolInvocationStream { event })
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
}

fn convert_tool_stream_event(event: ServiceToolInvocationStreamEvent) -> ToolInvocationStreamEvent {
    match event {
        ServiceToolInvocationStreamEvent::Started {
            tool_call_id,
            tool_name,
            terminal,
            columns,
            rows,
        } => ToolInvocationStreamEvent::Started {
            tool_call_id,
            tool_name,
            terminal,
            columns,
            rows,
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
        ServiceToolInvocationStreamEvent::Finished {
            tool_call_id,
            sequence,
            is_error,
        } => ToolInvocationStreamEvent::Finished {
            tool_call_id,
            sequence,
            is_error,
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

async fn request_tool_permission(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
    definition: &ServiceToolDefinition,
    cancel_state: &TurnCancelState,
) -> bool {
    let permission_id = next_permission_id(state).await;
    let arguments_json = serde_json::to_string(&call.arguments).unwrap_or_default();
    let agent_id = session_agent_selection(state, session_id).await;
    append_permission_requested_event(
        state,
        session_id,
        permission_id.clone(),
        call.id.clone(),
        definition.name.clone(),
        arguments_json.clone(),
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
        },
        decision: Arc::new(Mutex::new(None)),
        notify: Arc::new(Notify::new()),
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
    permission_id: String,
    tool_call_id: String,
    tool_name: String,
    arguments_json: String,
) {
    match state
        .sessions
        .append_permission_requested(
            session_id,
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        )
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

    let mut messages = Vec::new();
    if let Some((index, compacted_through_sequence)) = latest_compaction {
        if let Some(message) =
            session_event_to_model_message_with_limit(&history[index], tool_output_context_chars)
        {
            messages.push(message);
        }
        messages.extend(
            history
                .iter()
                .enumerate()
                .filter_map(|(event_index, event)| {
                    (event_index != index && event.sequence > compacted_through_sequence).then(
                        || {
                            session_event_to_model_message_with_limit(
                                event,
                                tool_output_context_chars,
                            )
                        },
                    )?
                }),
        );
    } else {
        messages.extend(history.iter().filter_map(|event| {
            session_event_to_model_message_with_limit(event, tool_output_context_chars)
        }));
    }
    complete_orphaned_tool_calls_for_model_context(messages)
}

fn complete_orphaned_tool_calls_for_model_context(
    messages: Vec<ModelMessage>,
) -> Vec<ModelMessage> {
    let mut completed = Vec::with_capacity(messages.len());
    let mut pending_tool_call_ids = Vec::<String>::new();

    for message in messages {
        if message.role != MessageRole::Tool && !pending_tool_call_ids.is_empty() {
            append_missing_tool_results(&mut completed, &mut pending_tool_call_ids);
        }

        collect_tool_results(&message, &mut pending_tool_call_ids);
        let tool_call_ids = assistant_tool_call_ids(&message);
        completed.push(message);
        pending_tool_call_ids.extend(tool_call_ids);
    }

    append_missing_tool_results(&mut completed, &mut pending_tool_call_ids);
    completed
}

fn assistant_tool_call_ids(message: &ModelMessage) -> Vec<String> {
    if message.role != MessageRole::Assistant {
        return Vec::new();
    }
    message
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::ToolCall { call } => Some(call.id.clone()),
            _ => None,
        })
        .collect()
}

fn collect_tool_results(message: &ModelMessage, pending_tool_call_ids: &mut Vec<String>) {
    if message.role != MessageRole::Tool {
        return;
    }
    for call_id in message.content.iter().filter_map(|content| match content {
        ContentBlock::ToolResult { result } => Some(&result.call_id),
        _ => None,
    }) {
        pending_tool_call_ids.retain(|pending_call_id| pending_call_id != call_id);
    }
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

fn session_event_to_model_message_with_limit(
    event: &bcode_session_models::SessionEvent,
    tool_output_context_chars: usize,
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
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
        } => Some(ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                call: bcode_model::ToolCall {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    arguments: serde_json::from_str(arguments_json).unwrap_or_default(),
                },
            }],
        }),
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
            output,
        } => Some(ModelMessage {
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
        }),
        SessionEventKind::SystemMessage { text } => Some(ModelMessage {
            role: MessageRole::System,
            content: vec![ContentBlock::Text { text: text.clone() }],
        }),
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

async fn append_assistant_delta_event(state: &ServerState, session_id: SessionId, text: String) {
    match state
        .sessions
        .append_assistant_delta(session_id, text)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append assistant delta: {error}"),
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
) {
    let runtime_work_id = RuntimeWorkId::new(format!("tool_{tool_call_id}"));
    let runtime_label = tool_name.clone();
    let runtime_tool_call_id = tool_call_id.clone();
    match state
        .sessions
        .append_tool_call_requested(session_id, tool_call_id, tool_name, arguments_json)
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
                .active_session_turns
                .lock()
                .await
                .get(&session_id)
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

async fn append_tool_finished_event(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: String,
    result: String,
    is_error: bool,
    content: Vec<ToolResultContent>,
    output: Option<TraceBlobRef>,
) {
    if let Err(error) = append_tool_finished_event_inner(
        state,
        session_id,
        tool_call_id,
        result,
        is_error,
        content,
        output,
    )
    .await
    {
        eprintln!("failed to append tool result: {error}");
    }
}

async fn append_tool_finished_event_inner(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: String,
    result: String,
    is_error: bool,
    content: Vec<ToolResultContent>,
    output: Option<TraceBlobRef>,
) -> Result<bcode_session_models::SessionEvent, bcode_session::SessionError> {
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

async fn broadcast_catalog_update(state: &ServerState, revision: u64) {
    let envelope = match event_envelope(&Event::SessionCatalogUpdated { revision }) {
        Ok(envelope) => envelope,
        Err(error) => {
            eprintln!("failed to encode catalog update event: {error}");
            return;
        }
    };
    for writer in state.event_client_writers().await {
        let mut writer = writer.lock().await;
        if let Err(error) = send_envelope(&mut *writer, &envelope).await {
            eprintln!("failed to send catalog update event: {error}");
        }
    }
}

fn forward_session_events(
    writer: SharedWriter,
    mut events: tokio::sync::broadcast::Receiver<bcode_session_models::SessionEvent>,
) {
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            let envelope = match event_envelope(&Event::Session(event)) {
                Ok(envelope) => envelope,
                Err(error) => {
                    eprintln!("failed to encode session event: {error}");
                    break;
                }
            };
            let mut writer = writer.lock().await;
            if let Err(error) = send_envelope(&mut *writer, &envelope).await {
                eprintln!("failed to send session event: {error}");
                break;
            }
        }
    });
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
    bcode_config::default_state_dir().join("provider-state.json")
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
            session_id,
            provenance: None,
            kind,
        }
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
        assert!(!model_event_is_progress(
            &ProviderTurnEvent::ToolCallDelta {
                call_id: "call-test".to_string(),
                delta: "{\"path\"".to_string(),
            }
        ));
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
                    result: format!("{}tail", "x".repeat(4_000)),
                    is_error: false,
                    output: None,
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
        assert!(!text.contains("tail"));
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
        };

        assert!(is_tool_arguments_decode_provider_error(&error));
        assert!(should_retry_after_malformed_tool_arguments(&error, false));
        assert!(!should_retry_after_malformed_tool_arguments(&error, true));
    }

    #[test]
    fn recoverable_provider_errors_are_deferred_until_retry_exhaustion() {
        let malformed_tool_error = bcode_model::ProviderError {
            code: TOOL_ARGUMENTS_DECODE_FAILED_CODE.to_string(),
            category: bcode_model::ProviderErrorCategory::ProviderInternal,
            message: "invalid JSON".to_string(),
            retryable: false,
            provider_message: None,
        };
        let invalid_request_error = bcode_model::ProviderError {
            code: "bad_request".to_string(),
            category: bcode_model::ProviderErrorCategory::InvalidRequest,
            message: "bad request".to_string(),
            retryable: false,
            provider_message: None,
        };

        assert!(should_defer_visible_provider_error(&malformed_tool_error));
        assert!(!should_defer_visible_provider_error(&invalid_request_error));
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
            },
            bcode_model::ModelInfo {
                model_id: "selected".to_string(),
                display_name: "Selected".to_string(),
                is_default: false,
                context_window: Some(16_000),
                max_output_tokens: Some(2_000),
                capabilities: BTreeSet::new(),
                reasoning: None,
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
        let (stable, dynamic) = build_coding_system_prompt_parts(&cwd, Some("agent suffix"));

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
        assert!(truncated.contains("/tmp/full-output.txt"));
        assert!(!truncated.ends_with('z'));
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
                auto_compaction: bcode_config::CompactionConfig::default(),
                skills: None,
                skill_context_bytes: 0,
                daemon_status: DaemonStatus::default(),
                daemon_record_path: None,
            },
        );
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
                .events
                .recv()
                .await
                .expect("subscriber should receive transient delta");
            if matches!(event.kind, SessionEventKind::ToolInvocationStream { .. }) {
                break event;
            }
        };
        assert_eq!(
            received.kind,
            SessionEventKind::ToolInvocationStream { event: delta }
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
                auto_compaction: bcode_config::CompactionConfig::default(),
                skills: None,
                skill_context_bytes: 0,
                daemon_status: DaemonStatus::default(),
                daemon_record_path: None,
            },
        );

        let event = append_tool_finished_event_inner(
            &state,
            session_id,
            "call-1".to_owned(),
            canonical_result.clone(),
            false,
            Vec::new(),
            None,
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
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_string(),
                result: output,
                is_error: false,
                output: None,
            },
        };

        let message =
            session_event_to_model_message_with_limit(&event, 1_000).expect("tool result message");
        let ContentBlock::ToolResult { result } = &message.content[0] else {
            panic!("expected tool result content block");
        };

        assert!(result.output.chars().count() <= 1_000);
        assert!(result.output.contains("tool output truncated"));
    }
}
