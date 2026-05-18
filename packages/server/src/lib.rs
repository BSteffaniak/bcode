#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Local Bcode daemon runtime.

use bcode_agent_profile::{
    AGENT_PROFILE_INTERFACE_ID, AgentContextRequest, AgentContextResponse, AgentDecision,
    AgentInfo, AgentList, EvaluateToolCallRequest, EvaluateToolCallResponse, OP_AGENT_CONTEXT,
    OP_EVALUATE_TOOL_CALL, OP_LIST_AGENTS, OP_POLICY_STATUS, PolicyStatusResponse,
};
use bcode_ipc::{
    CodecError, EnvelopeKind, ErrorResponse, Event, IpcEndpoint, LocalIpcListener, LocalIpcStream,
    PermissionSummary, PluginServiceError, PluginServiceResponse, PluginServiceSummary, Request,
    Response, ResponsePayload, ServerStatus, decode, event_envelope, recv_envelope,
    response_envelope, send_envelope,
};
use bcode_model::{
    CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID, MessageRole,
    ModelList, ModelMessage, ModelParameters, ModelTurnRequest, OP_CANCEL_TURN, OP_FINISH_TURN,
    OP_MODELS, OP_POLL_TURN_EVENTS, OP_START_TURN, PollTurnEventsRequest, PollTurnEventsResponse,
    ProviderTurnEvent, ReasoningEffort, StartTurnResponse, TokenUsage,
};
use bcode_session::SessionManager;
use bcode_session_models::{
    ClientId, ModelTurnOutcome, SessionEventKind, SessionId, SessionTokenUsage, SessionTraceEvent,
    SessionTracePayload, SessionTracePhase, TraceBlobRef, TraceRedaction,
};
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID,
    ToolDefinition as ServiceToolDefinition, ToolInvocationRequest, ToolInvocationResponse,
    ToolList,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{WriteHalf, split};
use tokio::sync::{Mutex, Notify, broadcast};

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
    #[error("blocking task join error: {0}")]
    BlockingTask(#[from] tokio::task::JoinError),
}

#[derive(Debug)]
struct ServerState {
    sessions: SessionManager,
    plugins: Arc<Mutex<bcode_plugin::PluginHost>>,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    selected_provider_context: bcode_model::ProviderRequestContext,
    prompt_cache_mode: bcode_model::PromptCacheMode,
    conversation_reuse_mode: bcode_model::ConversationReuseMode,
    provider_state: Mutex<ProviderStateStore>,
    observability: bcode_config::ObservabilityConfig,
    trace_store: TraceStore,
    max_tool_rounds: Option<u32>,
    tool_output_context_chars: usize,
    auto_compaction: bcode_config::CompactionConfig,
    active_turns: Mutex<BTreeMap<SessionId, ActiveModelTurn>>,
    session_model_selections: Mutex<BTreeMap<SessionId, SessionModelSelection>>,
    session_agent_selections: Mutex<BTreeMap<SessionId, String>>,
    pending_permissions: Mutex<BTreeMap<String, PendingPermission>>,
    next_permission_id: Mutex<u64>,
    clients: Mutex<BTreeSet<ClientId>>,
    shutdown: broadcast::Sender<()>,
}

#[derive(Debug, Clone)]
struct ActiveModelTurn {
    provider_plugin_id: Option<String>,
    provider_turn_id: String,
    reuse_key: Option<String>,
    request_message_count: usize,
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

    fn blob_path(&self, reference: &TraceBlobRef) -> PathBuf {
        self.root.join(&reference.path)
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
    provider_state: ProviderStateStore,
    observability: bcode_config::ObservabilityConfig,
    trace_store: TraceStore,
    max_tool_rounds: Option<u32>,
    tool_output_context_chars: usize,
    auto_compaction: bcode_config::CompactionConfig,
}

impl ServerState {
    fn new(
        sessions: SessionManager,
        plugins: bcode_plugin::PluginHost,
        init: ServerStateInit,
    ) -> Self {
        let (shutdown, _) = broadcast::channel(1);
        Self {
            sessions,
            plugins: Arc::new(Mutex::new(plugins)),
            selected_provider_plugin_id: init.selected_provider_plugin_id,
            selected_model_id: init.selected_model_id,
            selected_provider_context: init.selected_provider_context,
            prompt_cache_mode: init.prompt_cache_mode,
            conversation_reuse_mode: init.conversation_reuse_mode,
            provider_state: Mutex::new(init.provider_state),
            observability: init.observability,
            trace_store: init.trace_store,
            max_tool_rounds: init.max_tool_rounds,
            tool_output_context_chars: init.tool_output_context_chars,
            auto_compaction: init.auto_compaction,
            active_turns: Mutex::default(),
            session_model_selections: Mutex::default(),
            session_agent_selections: Mutex::default(),
            pending_permissions: Mutex::default(),
            next_permission_id: Mutex::new(1),
            clients: Mutex::default(),
            shutdown,
        }
    }

    async fn register_client(&self, client_id: ClientId) {
        self.clients.lock().await.insert(client_id);
    }

    async fn unregister_client(&self, client_id: ClientId) {
        self.clients.lock().await.remove(&client_id);
    }

    async fn status(&self) -> ServerStatus {
        ServerStatus {
            connected_client_count: self.clients.lock().await.len(),
            sessions: self.sessions.list_sessions().await,
            selected_provider_plugin_id: self.selected_provider_plugin_id.clone(),
            selected_model_id: self.selected_model_id.clone(),
        }
    }

    fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.shutdown.subscribe()
    }

    fn request_shutdown(&self) {
        let _ = self.shutdown.send(());
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
    let plugins = bcode_plugin::PluginHost::load_defaults(&plugin_selection)?;
    tracing::debug!(target: "bcode_server::startup", "plugins loaded");
    tracing::debug!(target: "bcode_server::startup", endpoint = ?endpoint, "binding IPC endpoint");
    let listener = LocalIpcListener::bind(&endpoint)?;
    tracing::debug!(target: "bcode_server::startup", "IPC endpoint bound");
    tracing::debug!(target: "bcode_server::startup", "opening session store");
    let sessions = SessionManager::persistent(default_session_store_dir())?;
    tracing::debug!(target: "bcode_server::startup", "session store ready");
    let resolved_model = config.resolved_model_selection();
    tracing::debug!(
        target: "bcode_server::startup",
        provider = ?resolved_model.provider_plugin_id,
        model = ?resolved_model.model_id,
        "model selection resolved"
    );
    let configured_agent_ids: Vec<String> = config.agent.keys().cloned().collect();
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
            },
            prompt_cache_mode: config.model.prompt_cache.mode,
            conversation_reuse_mode: config.model.conversation_reuse.mode,
            provider_state: ProviderStateStore::load(default_provider_state_path()),
            observability: config.observability,
            trace_store: TraceStore::new(default_trace_store_dir()),
            max_tool_rounds: config.model.effective_max_tool_rounds(),
            tool_output_context_chars: config.model.tool_output.context_chars,
            auto_compaction: config.model.compaction,
        },
    ));
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
    state.plugins.lock().await.deactivate_all()?;
    tracing::debug!(target: "bcode_server::startup", "shutdown complete");
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
    }

    Ok(())
}

async fn handle_request(
    request: Request,
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
) -> Result<(), ServerError> {
    match request {
        Request::Hello { .. } => handle_hello(request_id, client_id, writer).await,
        Request::Ping => handle_ping(request_id, writer).await,
        Request::ServerStatus => handle_server_status(request_id, state, writer).await,
        Request::ServerStop => handle_server_stop(request_id, state, writer).await,
        Request::CreateSession { name } => {
            handle_create_session(request_id, state, writer, name).await
        }
        Request::ListSessions => handle_list_sessions(request_id, state, writer).await,
        Request::RenameSession { session_id, name } => {
            handle_rename_session(request_id, state, writer, session_id, name).await
        }
        Request::DeleteSession { session_id } => {
            handle_delete_session(request_id, state, writer, session_id).await
        }
        Request::SessionHistory { session_id } => {
            handle_session_history(request_id, state, writer, session_id).await
        }
        Request::SessionHistoryPage { session_id, query } => {
            handle_session_history_page(request_id, state, writer, session_id, query).await
        }
        Request::AttachSession { session_id } => {
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
        Request::SendUserMessage { session_id, text } => {
            handle_user_message(request_id, client_id, state, writer, session_id, text).await
        }
        Request::CancelSessionTurn { session_id } => {
            handle_cancel_session_turn(request_id, state, writer, session_id).await
        }
        Request::CompactSession { session_id } => {
            handle_compact_session(request_id, state, writer, session_id).await
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
        Request::SessionModelStatus { session_id } => {
            handle_session_model_status(request_id, state, writer, session_id).await
        }
        request => handle_agent_permission_plugin_request(request, request_id, state, writer).await,
    }
}

async fn handle_agent_permission_plugin_request(
    request: Request,
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    match request {
        Request::ListAgents => handle_list_agents(request_id, state, writer).await,
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
        _ => unreachable!("primary request routed to agent/permission/plugin handler"),
    }
}

async fn handle_ping(request_id: u64, writer: &SharedWriter) -> Result<(), ServerError> {
    send_response(writer, request_id, Response::Ok(ResponsePayload::Pong)).await
}

async fn handle_hello(
    request_id: u64,
    client_id: ClientId,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
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
) -> Result<(), ServerError> {
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
) -> Result<(), ServerError> {
    let session = state.sessions.create_session(name).await?;
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
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let session_list = state.sessions.list_sessions().await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionList {
            sessions: session_list,
        }),
    )
    .await
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
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
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
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
    query: bcode_session_models::SessionHistoryQuery,
) -> Result<(), ServerError> {
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

async fn handle_attach_session(
    request_id: u64,
    client_id: ClientId,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
    session_id: SessionId,
) -> Result<(), ServerError> {
    match state.sessions.attach_session(session_id, client_id).await {
        Ok(attachment) => {
            *attached_session = Some(session_id);
            publish_session_event(state, &attachment.attached_event).await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::Attached {
                    session_id,
                    history: compact_attach_history(attachment.history),
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
            .await
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
    match state
        .sessions
        .attach_session_recent(session_id, client_id, limit)
        .await
    {
        Ok(attachment) => {
            *attached_session = Some(session_id);
            publish_session_event(state, &attachment.attached_event).await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::Attached {
                    session_id,
                    history: compact_attach_history(attachment.history),
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
    match state
        .sessions
        .append_user_message(session_id, client_id, text)
        .await
    {
        Ok(events) => {
            for event in &events {
                publish_session_event(state, event).await;
            }
            let Some(user_event) = events.last().cloned() else {
                return send_response(
                    writer,
                    request_id,
                    Response::Err(ErrorResponse::new(
                        "message_not_appended",
                        "no user message event was appended",
                    )),
                )
                .await;
            };
            let state_for_turn = Arc::clone(state);
            tokio::spawn(async move {
                run_model_turn(&state_for_turn, session_id, &user_event).await;
            });
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::MessageSent),
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

async fn handle_session_model_status(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    let selection = session_model_selection(state, session_id).await;
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
                model,
            },
        }),
    )
    .await
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
) -> Result<(), ServerError> {
    let Some(active_turn) = state.active_turns.lock().await.get(&session_id).cloned() else {
        return send_response(
            writer,
            request_id,
            Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled: false }),
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
            append_system_event(
                state,
                session_id,
                "model turn cancellation requested".to_string(),
            )
            .await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::TurnCancellationRequested { cancelled: true }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("plugin_error", error)),
            )
            .await
        }
    }
}

async fn handle_compact_session(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) -> Result<(), ServerError> {
    match compact_session_context(state, session_id).await {
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
const MODEL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
struct ModelPollOutcome {
    stop_reason: Option<bcode_model::StopReason>,
    should_continue: bool,
    completion: Option<ModelTurnCompletion>,
    provider_error: Option<bcode_model::ProviderError>,
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
    lines: Vec<String>,
    compacted_through_sequence: u64,
    event_count: usize,
}

const COMPACTION_SYSTEM_PROMPT: &str = "You compact coding-agent session history. Produce only a durable continuation summary for future model turns. Preserve all facts needed to continue the work, including user goals, decisions, constraints, files changed, commands run, validation results, current blockers, and next steps. Do not invent details. Do not include markdown fences.";
const COMPACTION_CHUNK_TARGET_CHARS: usize = 32_000;
const COMPACTION_MIN_CHUNK_CHARS: usize = 4_000;
const COMPACTION_MAX_CARRIED_SUMMARY_CHARS: usize = 24_000;
const COMPACTION_MAX_EVENT_CONTENT_CHARS: usize = 16_000;

async fn compact_session_context(
    state: &ServerState,
    session_id: SessionId,
) -> Result<String, CompactionError> {
    compact_session_context_with_limit(state, session_id, None).await
}

async fn compact_session_context_before_sequence(
    state: &ServerState,
    session_id: SessionId,
    first_kept_sequence: u64,
) -> Result<String, CompactionError> {
    compact_session_context_with_limit(state, session_id, Some(first_kept_sequence)).await
}

async fn compact_session_context_with_limit(
    state: &ServerState,
    session_id: SessionId,
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

    let selection = session_model_selection(state, session_id).await;
    if !has_model_provider(state, selection.provider_plugin_id.as_deref()).await {
        return Err(CompactionError::ProviderUnavailable);
    }

    let summary = collect_compaction_summary(state, session_id, &selection, &transcript).await?;
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
    let message = compact_session_context(state, session_id).await?;
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
            ContentBlock::ToolCall { call } => {
                call.name.chars().count() + call.arguments.to_string().chars().count()
            }
            ContentBlock::ToolResult { result } => result.output.chars().count(),
            ContentBlock::CachePoint { .. } => 0,
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
    let mut summary = transcript.previous_summary.clone().unwrap_or_default();
    let mut chunks = compaction_chunks(&transcript.lines, COMPACTION_CHUNK_TARGET_CHARS);
    let mut chunk_index = 0_usize;
    while chunk_index < chunks.len() {
        let prompt_text = compaction_prompt_text(
            summary.trim(),
            &chunks[chunk_index],
            chunk_index,
            chunks.len(),
        );
        match collect_compaction_summary_chunk(
            state,
            session_id,
            selection,
            transcript.compacted_through_sequence,
            chunk_index,
            &prompt_text,
        )
        .await
        {
            Ok(next_summary) => {
                summary = truncate_text(next_summary.trim(), COMPACTION_MAX_CARRIED_SUMMARY_CHARS);
                chunk_index = chunk_index.saturating_add(1);
            }
            Err(error)
                if is_context_length_compaction_error(&error)
                    && split_compaction_chunk_at(&mut chunks, chunk_index) => {}
            Err(error) => return Err(CompactionError::Provider(error)),
        }
    }
    Ok(summary)
}

async fn collect_compaction_summary_chunk(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    compacted_through_sequence: u64,
    chunk_index: usize,
    prompt_text: &str,
) -> Result<String, String> {
    let turn_id = format!(
        "{session_id}-compact-{compacted_through_sequence}-{}",
        chunk_index.saturating_add(1)
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
        .map_err(|error| error.to_string());
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

fn compaction_chunks(lines: &[String], target_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in lines {
        let separator_chars = usize::from(!current.is_empty()) * 2;
        if !current.is_empty()
            && current
                .chars()
                .count()
                .saturating_add(separator_chars)
                .saturating_add(line.chars().count())
                > target_chars
        {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn compaction_prompt_text(
    previous_summary: &str,
    chunk: &str,
    chunk_index: usize,
    total_chunks: usize,
) -> String {
    let previous_summary = previous_summary.trim();
    let carried_summary = truncate_text(previous_summary, COMPACTION_MAX_CARRIED_SUMMARY_CHARS);
    if carried_summary.is_empty() {
        return format!(
            "Compact this Bcode session transcript chunk for future continuation. This is chunk {}/{}. Return only the durable rolling summary.\n\nTranscript chunk:\n\n{chunk}",
            chunk_index.saturating_add(1),
            total_chunks
        );
    }
    format!(
        "Update the existing compacted Bcode session summary with the next transcript chunk. This is chunk {}/{}. Return only the updated durable summary.\n\nExisting summary:\n\n{carried_summary}\n\nNext transcript chunk:\n\n{chunk}",
        chunk_index.saturating_add(1),
        total_chunks
    )
}

fn split_compaction_chunk_at(chunks: &mut Vec<String>, index: usize) -> bool {
    let Some(chunk) = chunks.get(index) else {
        return false;
    };
    let char_count = chunk.chars().count();
    if char_count <= COMPACTION_MIN_CHUNK_CHARS {
        return false;
    }
    let midpoint = char_count / 2;
    let split_byte = chunk
        .char_indices()
        .nth(midpoint)
        .map_or_else(|| chunk.len(), |(byte_index, _)| byte_index);
    let second = chunk[split_byte..].trim_start().to_string();
    let first = chunk[..split_byte].trim_end().to_string();
    if first.is_empty() || second.is_empty() {
        return false;
    }
    chunks.splice(index..=index, [first, second]);
    true
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

async fn poll_compaction_summary(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    provider_turn_id: &str,
    turn_id: &str,
) -> Result<String, CompactionError> {
    let mut summary = String::new();
    let mut idle_for = Duration::ZERO;
    for _ in 0..1_200 {
        let poll = PollTurnEventsRequest {
            provider_turn_id: provider_turn_id.to_string(),
        };
        let response = poll_model_turn(state, selection.provider_plugin_id.as_deref(), &poll)
            .await
            .map_err(CompactionError::Provider)?;
        if response.events.is_empty() {
            idle_for = wait_for_compaction_event(idle_for).await?;
            continue;
        }
        idle_for = Duration::ZERO;
        match handle_compaction_events(state, session_id, turn_id, &mut summary, response.events)
            .await
        {
            CompactionPollStatus::Continue => {}
            CompactionPollStatus::Finished => return Ok(summary),
            CompactionPollStatus::Failed(error) => return Err(CompactionError::Provider(error)),
        }
    }
    Err(CompactionError::Provider(
        "model provider did not finish compaction turn".to_string(),
    ))
}

async fn wait_for_compaction_event(idle_for: Duration) -> Result<Duration, CompactionError> {
    let idle_for = idle_for.saturating_add(MODEL_POLL_INTERVAL);
    if idle_for > MODEL_IDLE_TIMEOUT {
        return Err(CompactionError::Provider(format!(
            "model provider was idle for {} seconds before timeout",
            MODEL_IDLE_TIMEOUT.as_secs()
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
            ProviderTurnEvent::ToolCallFinished { .. } => {
                return CompactionPollStatus::Failed(
                    "compaction summary unexpectedly requested a tool".to_string(),
                );
            }
            ProviderTurnEvent::TurnStarted
            | ProviderTurnEvent::ReasoningDelta { .. }
            | ProviderTurnEvent::ToolCallStarted { .. }
            | ProviderTurnEvent::ToolCallDelta { .. }
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
    let compacted_through_sequence = history.last()?.sequence;
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
    let mut lines = Vec::new();
    let mut event_count = 0_usize;
    for event in &history[start_index..] {
        if let Some(line) = session_event_compaction_line(event, tool_output_context_chars) {
            event_count = event_count.saturating_add(1);
            lines.push(line);
        }
    }

    if event_count == 0 {
        return None;
    }

    Some(CompactionTranscript {
        previous_summary,
        lines,
        compacted_through_sequence,
        event_count,
    })
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
        } => Some(format!(
            "#{} tool result {tool_call_id} (error={is_error}):\n{}",
            event.sequence,
            tool_result_for_model(
                result,
                None,
                tool_output_context_chars.min(COMPACTION_MAX_EVENT_CONTENT_CHARS),
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
    session_id: SessionId,
    trigger_event: &bcode_session_models::SessionEvent,
) {
    let turn_id = format!("{}-{}", session_id, trigger_event.sequence);
    append_model_turn_started_event(state, session_id, turn_id.clone()).await;
    let completion = run_model_turn_inner(state, session_id, trigger_event).await;
    append_model_turn_finished_event(
        state,
        session_id,
        turn_id,
        completion.outcome,
        completion.message,
    )
    .await;
}

async fn run_model_turn_inner(
    state: &ServerState,
    session_id: SessionId,
    trigger_event: &bcode_session_models::SessionEvent,
) -> ModelTurnCompletion {
    if let Err(error) = maybe_auto_compact_session_context(state, session_id).await {
        let message = format!("auto compaction failed: {error}");
        append_system_event(state, session_id, message).await;
    }

    let selection = session_model_selection(state, session_id).await;
    if !has_model_provider(state, selection.provider_plugin_id.as_deref()).await {
        return ModelTurnCompletion::with_message(
            ModelTurnOutcome::ProviderUnavailable,
            "model provider unavailable",
        );
    }

    let provider_plugin_id = selection.provider_plugin_id.clone();
    let mut round = 0_u32;
    let mut retried_after_context_overflow = false;
    loop {
        let request = match build_model_turn_request(
            state,
            session_id,
            trigger_event,
            round,
            provider_plugin_id.as_deref(),
            selection.model_id.as_deref(),
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
        let outcome =
            match run_model_turn_round(state, session_id, provider_plugin_id.as_deref(), &request)
                .await
            {
                Ok(outcome) => outcome,
                Err(completion) => return completion,
            };
        if let Some(error) = outcome.provider_error.as_ref()
            && should_retry_after_context_overflow(state, error, retried_after_context_overflow)
        {
            retried_after_context_overflow = true;
            match compact_session_after_context_overflow(
                state,
                session_id,
                trigger_event.sequence,
                error,
            )
            .await
            {
                Ok(()) => continue,
                Err(completion) => return completion,
            }
        }
        if let Some(completion) = outcome.completion.clone() {
            append_deferred_provider_error_if_needed(state, session_id, &outcome).await;
            return completion;
        }
        if !outcome.should_continue {
            return ModelTurnCompletion::completed();
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

fn should_retry_after_context_overflow(
    state: &ServerState,
    error: &bcode_model::ProviderError,
    already_retried: bool,
) -> bool {
    !already_retried
        && state.auto_compaction.mode.is_overflow_recovery_enabled()
        && is_context_length_provider_error(error)
}

async fn compact_session_after_context_overflow(
    state: &ServerState,
    session_id: SessionId,
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
    match compact_session_context_before_sequence(state, session_id, first_kept_sequence).await {
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
        && is_context_length_provider_error(error)
    {
        append_system_event(state, session_id, provider_error_message(error)).await;
    }
}

fn is_context_length_provider_error(error: &bcode_model::ProviderError) -> bool {
    error.category == bcode_model::ProviderErrorCategory::ContextLength
}

fn provider_error_message(error: &bcode_model::ProviderError) -> String {
    format!("model error {}: {}", error.code, error.message)
}

async fn run_model_turn_round(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    request: &ModelTurnRequest,
) -> Result<ModelPollOutcome, ModelTurnCompletion> {
    let round_start = Instant::now();
    let provider_label = provider_plugin_id.unwrap_or("<auto>").to_string();
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

    let (assistant_text, outcome) = poll_model_turn_events(
        state,
        session_id,
        provider_plugin_id,
        &start.provider_turn_id,
        &request.turn_id,
    )
    .await;

    if !assistant_text.is_empty() {
        append_assistant_message_event(state, session_id, assistant_text).await;
    }

    let active_turn = state.active_turns.lock().await.remove(&session_id);
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
) -> (String, ModelPollOutcome) {
    let mut assistant_text = String::new();
    let mut outcome = ModelPollOutcome::default();
    let mut idle_for = Duration::ZERO;
    for _ in 0..1_200 {
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
            idle_for += MODEL_POLL_INTERVAL;
            if idle_for > MODEL_IDLE_TIMEOUT {
                let message = format!(
                    "model provider was idle for {} seconds before timeout",
                    MODEL_IDLE_TIMEOUT.as_secs()
                );
                append_system_event(state, session_id, message.clone()).await;
                outcome.completion = Some(ModelTurnCompletion::with_message(
                    ModelTurnOutcome::IdleTimeout,
                    message,
                ));
                break;
            }
            tokio::time::sleep(MODEL_POLL_INTERVAL).await;
            continue;
        }
        idle_for = Duration::ZERO;
        for event in response.events {
            handle_provider_turn_event(
                state,
                session_id,
                turn_id,
                event,
                &mut assistant_text,
                &mut outcome,
            )
            .await;
        }
        if outcome.stop_reason.is_some() {
            break;
        }
    }
    (assistant_text, outcome)
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

async fn handle_provider_turn_event(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    event: ProviderTurnEvent,
    assistant_text: &mut String,
    outcome: &mut ModelPollOutcome,
) {
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
            handle_provider_tool_call_finished_event(
                state,
                session_id,
                turn_id,
                call,
                assistant_text,
            )
            .await;
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
            append_provider_event_trace(state, session_id, turn_id, "turn_started", None).await;
        }
        ProviderTurnEvent::ToolCallStarted { call_id, name } => {
            append_provider_event_trace(
                state,
                session_id,
                turn_id,
                "tool_call_started",
                Some(format!("{name} ({call_id})")),
            )
            .await;
        }
        ProviderTurnEvent::ReasoningDelta { .. } => {
            append_provider_event_trace(state, session_id, turn_id, "reasoning_delta", None).await;
        }
        ProviderTurnEvent::ToolCallDelta { call_id, .. } => {
            append_provider_event_trace(
                state,
                session_id,
                turn_id,
                "tool_call_delta",
                Some(call_id),
            )
            .await;
        }
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
    let defer_visible_message = is_context_length_provider_error(&error);
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
    outcome.should_continue = stop_reason == bcode_model::StopReason::ToolCall;
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
    append_provider_event_trace(
        state,
        session_id,
        turn_id,
        "tool_call_finished",
        Some(call.name.clone()),
    )
    .await;
    if !assistant_text.is_empty() {
        append_assistant_message_event(state, session_id, std::mem::take(assistant_text)).await;
    }
    execute_model_tool(state, session_id, call).await;
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
    with_plugins_blocking(state, |plugins| {
        plugins.invoke_service_by_interface_json::<_, PolicyStatusResponse>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_POLICY_STATUS,
            &serde_json::json!({}),
        )
    })
    .await
    .ok()
    .and_then(Result::ok)
}

async fn list_agent_profiles(state: &ServerState) -> Vec<AgentInfo> {
    with_plugins_blocking(state, |plugins| {
        plugins.invoke_service_by_interface_json::<_, AgentList>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_LIST_AGENTS,
            &serde_json::json!({}),
        )
    })
    .await
    .ok()
    .and_then(Result::ok)
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
    with_plugins_blocking(state, move |plugins| {
        plugins.invoke_service_by_interface_json::<_, AgentContextResponse>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_AGENT_CONTEXT,
            &request,
        )
    })
    .await
    .ok()
    .and_then(Result::ok)
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
            provider_context: state.selected_provider_context.clone(),
        }
    } else {
        SessionModelSelection {
            provider_plugin_id: state.selected_provider_plugin_id.clone(),
            model_id: state.selected_model_id.clone(),
            thinking_level: None,
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

async fn has_model_provider(state: &ServerState, provider_plugin_id: Option<&str>) -> bool {
    let provider_plugin_id = provider_plugin_id.map(ToString::to_string);
    with_plugins_blocking(state, move |plugins| {
        if let Some(provider_plugin_id) = provider_plugin_id {
            return plugins.loaded_plugins().iter().any(|plugin| {
                plugin.manifest().id == provider_plugin_id
                    && plugin
                        .manifest()
                        .services
                        .iter()
                        .any(|service| service.interface_id == MODEL_PROVIDER_INTERFACE_ID)
            });
        }
        plugins
            .service_registry()
            .providers_for(MODEL_PROVIDER_INTERFACE_ID)
            .is_some()
    })
    .await
    .unwrap_or(false)
}

fn invoke_model_provider_json<Q, R>(
    plugins: &bcode_plugin::PluginHost,
    provider_plugin_id: Option<&str>,
    operation: &str,
    request: &Q,
) -> Result<R, bcode_plugin::PluginServiceCallError>
where
    Q: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    provider_plugin_id.map_or_else(
        || {
            plugins.invoke_service_by_interface_json(
                MODEL_PROVIDER_INTERFACE_ID,
                operation,
                request,
            )
        },
        |provider_plugin_id| {
            plugins.invoke_service_json(
                provider_plugin_id,
                MODEL_PROVIDER_INTERFACE_ID,
                operation,
                request,
            )
        },
    )
}

async fn with_plugins_blocking<R>(
    state: &ServerState,
    invoke: impl FnOnce(&bcode_plugin::PluginHost) -> R + Send + 'static,
) -> Result<R, ServerError>
where
    R: Send + 'static,
{
    let plugins = Arc::clone(&state.plugins);
    tokio::task::spawn_blocking(move || {
        let plugins = plugins.blocking_lock();
        invoke(&plugins)
    })
    .await
    .map_err(ServerError::from)
}

async fn invoke_model_provider_json_blocking<Q, R>(
    state: &ServerState,
    provider_plugin_id: Option<String>,
    operation: &'static str,
    request: Q,
) -> Result<R, String>
where
    Q: serde::Serialize + Send + 'static,
    R: serde::de::DeserializeOwned + Send + 'static,
{
    with_plugins_blocking(state, move |plugins| {
        invoke_model_provider_json::<_, R>(
            plugins,
            provider_plugin_id.as_deref(),
            operation,
            &request,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

async fn build_model_turn_request(
    state: &ServerState,
    session_id: SessionId,
    trigger_event: &bcode_session_models::SessionEvent,
    round: u32,
    provider_plugin_id: Option<&str>,
    selected_model_id: Option<&str>,
) -> Result<ModelTurnRequest, bcode_session::SessionError> {
    let history = state.sessions.model_context_events(session_id).await?;
    let mut messages =
        session_events_to_model_messages_with_limit(&history, state.tool_output_context_chars);
    let prompt_cache = plan_prompt_cache(&mut messages, state.prompt_cache_mode);
    let selection = session_model_selection(state, session_id).await;
    let agent_id = session_agent_selection(state, session_id).await;
    let agent_context = agent_context(state, session_id, &agent_id).await;
    let (system_prompt, dynamic_system_context) = build_coding_system_prompt_parts(
        agent_context
            .as_ref()
            .and_then(|context| context.system_prompt_suffix.as_deref()),
    );
    if !dynamic_system_context.trim().is_empty() {
        messages.insert(
            0,
            ModelMessage {
                role: MessageRole::System,
                content: vec![ContentBlock::Text {
                    text: dynamic_system_context,
                }],
            },
        );
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
    let metadata = projection.metadata();
    Ok(ModelTurnRequest {
        session_id,
        turn_id: format!("{}-{}-{round}", session_id, trigger_event.sequence),
        model_id,
        provider_context: selection.provider_context,
        system_prompt: Some(system_prompt),
        messages,
        tools,
        parameters,
        prompt_cache,
        conversation_reuse,
        metadata,
    })
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
        state.trace_store.write_json_blob(
            session_id,
            &format!("model-request-round-{round}"),
            request,
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

fn build_coding_system_prompt_parts(agent_prompt_suffix: Option<&str>) -> (String, String) {
    let (stable_context, dynamic_context) = build_repository_context_parts();
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

fn build_repository_context_parts() -> (String, String) {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo_root = discover_git_root(&cwd);
    let context_root = repo_root.as_deref().unwrap_or(cwd.as_path());

    let mut stable_lines = vec!["Stable repository context:".to_string()];
    stable_lines.push(format!(
        "* Detected project files: {}",
        detected_project_files(context_root).join(", ")
    ));
    if let Some(instructions) = read_nearest_agent_instructions(&cwd, context_root) {
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

fn tool_result_for_model(
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
    with_plugins_blocking(state, move |plugins| {
        let mut tools = Vec::new();
        for plugin in plugins.loaded_plugins() {
            if !plugin
                .manifest()
                .services
                .iter()
                .any(|service| service.interface_id == TOOL_SERVICE_INTERFACE_ID)
            {
                continue;
            }
            let response = plugins.invoke_service_json::<_, ToolList>(
                &plugin.manifest().id,
                TOOL_SERVICE_INTERFACE_ID,
                OP_LIST_TOOLS,
                &ListToolsRequest::default(),
            );
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
                Err(error) => eprintln!(
                    "failed to list tools from {}: {error}",
                    plugin.manifest().id
                ),
            }
        }
        tools
    })
    .await
    .unwrap_or_else(|error| {
        eprintln!("failed to collect model tools: {error}");
        Vec::new()
    })
}

async fn execute_model_tool(
    state: &ServerState,
    session_id: SessionId,
    call: bcode_model::ToolCall,
) {
    append_tool_request_event(
        state,
        session_id,
        call.id.clone(),
        call.name.clone(),
        serde_json::to_string(&call.arguments).unwrap_or_default(),
    )
    .await;
    let tool_start = Instant::now();
    let result = invoke_model_tool(state, session_id, &call)
        .await
        .unwrap_or_else(|error| ToolInvocationResponse {
            output: error,
            is_error: true,
        });
    let output_blob = (state.observability.persist_tool_io || state.observability.debug_enabled())
        .then(|| {
            state.trace_store.write_text_blob(
                session_id,
                &format!("tool-output-{}", call.id),
                &result.output,
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
            output_bytes: result.output.len(),
            output: output_blob.clone(),
        },
    )
    .await;
    let model_result = tool_result_for_model(
        &result.output,
        output_blob
            .as_ref()
            .map(|blob| state.trace_store.blob_path(blob)),
        state.tool_output_context_chars,
    );
    append_tool_finished_event(state, session_id, call.id, model_result, result.is_error).await;
}

async fn invoke_model_tool(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
) -> Result<ToolInvocationResponse, String> {
    let (plugin_id, definition) = find_tool_provider(state, &call.name)
        .await?
        .ok_or_else(|| format!("tool not found: {}", call.name))?;
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
            });
        }
        AgentDecision::Ask => {
            if !request_tool_permission(state, session_id, call, &definition).await {
                return Ok(ToolInvocationResponse {
                    output: "permission denied".to_string(),
                    is_error: true,
                });
            }
        }
        AgentDecision::Allow => {}
    }
    let request = ToolInvocationRequest {
        tool_call_id: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
    };
    with_plugins_blocking(state, move |plugins| {
        plugins.invoke_service_json::<_, ToolInvocationResponse>(
            &plugin_id,
            TOOL_SERVICE_INTERFACE_ID,
            OP_INVOKE_TOOL,
            &request,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

async fn evaluate_agent_tool_policy(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
    definition: &ServiceToolDefinition,
) -> EvaluateToolCallResponse {
    let agent_id = session_agent_selection(state, session_id).await;
    let request = EvaluateToolCallRequest {
        session_id,
        agent_id,
        tool_name: definition.name.clone(),
        side_effect: definition.side_effect,
        arguments: call.arguments.clone(),
        cwd: env::current_dir()
            .ok()
            .map(|path| path.display().to_string()),
    };
    with_plugins_blocking(state, move |plugins| {
        plugins.invoke_service_by_interface_json::<_, EvaluateToolCallResponse>(
            AGENT_PROFILE_INTERFACE_ID,
            OP_EVALUATE_TOOL_CALL,
            &request,
        )
    })
    .await
    .ok()
    .and_then(Result::ok)
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
    let tool_name = tool_name.to_string();
    with_plugins_blocking(state, move |plugins| {
        for plugin in plugins.loaded_plugins() {
            if !plugin
                .manifest()
                .services
                .iter()
                .any(|service| service.interface_id == TOOL_SERVICE_INTERFACE_ID)
            {
                continue;
            }
            let list = plugins
                .invoke_service_json::<_, ToolList>(
                    &plugin.manifest().id,
                    TOOL_SERVICE_INTERFACE_ID,
                    OP_LIST_TOOLS,
                    &ListToolsRequest::default(),
                )
                .map_err(|error| error.to_string())?;
            if let Some(tool) = list.tools.into_iter().find(|tool| tool.name == tool_name) {
                return Ok(Some((plugin.manifest().id.clone(), tool)));
            }
        }
        Ok(None)
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn request_tool_permission(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
    definition: &ServiceToolDefinition,
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
        pending.notify.notified().await;
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
    messages
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
        } => Some(ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: tool_call_id.clone(),
                    output: tool_result_for_model(result, None, tool_output_context_chars),
                    is_error: *is_error,
                },
            }],
        }),
        SessionEventKind::SystemMessage { text } => Some(ModelMessage {
            role: MessageRole::System,
            content: vec![ContentBlock::Text { text: text.clone() }],
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

async fn append_tool_request_event(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: String,
    tool_name: String,
    arguments_json: String,
) {
    match state
        .sessions
        .append_tool_call_requested(session_id, tool_call_id, tool_name, arguments_json)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append tool request: {error}"),
    }
}

async fn append_tool_finished_event(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: String,
    result: String,
    is_error: bool,
) {
    match state
        .sessions
        .append_tool_call_finished(session_id, tool_call_id, result, is_error)
        .await
    {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append tool result: {error}"),
    }
}

async fn append_system_event(state: &ServerState, session_id: SessionId, text: String) {
    match state.sessions.append_system_message(session_id, text).await {
        Ok(event) => publish_session_event(state, &event).await,
        Err(error) => eprintln!("failed to append system message: {error}"),
    }
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
    let services = with_plugins_blocking(state, plugin_service_summaries).await?;
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
    let response = with_plugins_blocking(state, move |plugins| {
        plugins.invoke_service(&plugin_id, interface_id, operation, payload)
    })
    .await?;
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
    let response = with_plugins_blocking(state, move |plugins| {
        plugins.invoke_service_by_interface(&interface_id, operation, payload)
    })
    .await?;
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
    let response = with_plugins_blocking(state, move |plugins| {
        plugins.publish_event(&topic, &payload)
    })
    .await?;
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

fn plugin_service_summaries(plugins: &bcode_plugin::PluginHost) -> Vec<PluginServiceSummary> {
    let mut services = Vec::new();
    for plugin in plugins.loaded_plugins() {
        for service in &plugin.manifest().services {
            services.push(PluginServiceSummary {
                plugin_id: plugin.manifest().id.clone(),
                interface_id: service.interface_id.clone(),
                name: service.name.clone(),
                description: service.description.clone(),
            });
        }
    }
    services
}

async fn publish_session_event(state: &ServerState, event: &bcode_session_models::SessionEvent) {
    let payload = match serde_json::to_vec(event) {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("failed to encode plugin session event: {error}");
            return;
        }
    };
    let response = with_plugins_blocking(state, move |plugins| {
        plugins.publish_event(SESSION_EVENT_PLUGIN_TOPIC, &payload)
    })
    .await;
    match response {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => eprintln!("failed to publish plugin session event: {error}"),
        Err(error) => eprintln!("failed to publish plugin session event: {error}"),
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

async fn send_response(
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

fn default_session_store_dir() -> PathBuf {
    bcode_config::default_state_dir().join("sessions")
}

fn default_provider_state_path() -> PathBuf {
    bcode_config::default_state_dir().join("provider-state.json")
}

fn default_trace_store_dir() -> PathBuf {
    bcode_config::default_state_dir().join("traces")
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
            kind,
        }
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
                },
            )],
            1_000,
        )
        .expect("compaction transcript");

        let text = transcript.lines.join("\n\n");
        assert!(text.contains("tool output truncated"));
        assert!(!text.contains("tail"));
        assert!(text.chars().count() < 1_200);
    }

    #[test]
    fn compaction_chunks_stay_under_target_when_lines_fit() {
        let lines = vec!["a".repeat(10), "b".repeat(10), "c".repeat(10)];

        let chunks = compaction_chunks(&lines, 25);

        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 25));
    }

    #[test]
    fn compaction_chunk_split_halves_oversized_chunk() {
        let mut chunks = vec!["x".repeat(COMPACTION_MIN_CHUNK_CHARS + 2)];

        assert!(split_compaction_chunk_at(&mut chunks, 0));
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
    }

    #[test]
    fn compaction_prompt_truncates_carried_summary() {
        let prompt = compaction_prompt_text(
            &"s".repeat(COMPACTION_MAX_CARRIED_SUMMARY_CHARS + 100),
            "next chunk",
            0,
            1,
        );

        assert!(prompt.contains("[truncated]"));
        assert!(prompt.contains("next chunk"));
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
            },
            bcode_model::ModelInfo {
                model_id: "selected".to_string(),
                display_name: "Selected".to_string(),
                is_default: false,
                context_window: Some(16_000),
                max_output_tokens: Some(2_000),
                capabilities: BTreeSet::new(),
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
        let (stable, dynamic) = build_coding_system_prompt_parts(Some("agent suffix"));

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
    fn tool_result_for_model_preserves_small_output() {
        let output = "short tool output";

        assert_eq!(tool_result_for_model(output, None, 4_000), output);
    }

    #[test]
    fn tool_result_for_model_truncates_large_output_with_artifact_path() {
        let output = format!("{}middle{}", "a".repeat(4_000), "z".repeat(4_000));

        let truncated =
            tool_result_for_model(&output, Some(PathBuf::from("/tmp/full-output.txt")), 1_000);

        assert!(truncated.chars().count() <= 1_000);
        assert!(truncated.starts_with('a'));
        assert!(truncated.contains("tool output truncated"));
        assert!(truncated.contains("/tmp/full-output.txt"));
        assert!(!truncated.ends_with('z'));
    }

    #[test]
    fn tool_result_model_message_uses_truncated_output() {
        let session_id = SessionId::new();
        let output = "x".repeat(4_001);
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            session_id,
            kind: SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_string(),
                result: output,
                is_error: false,
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
