#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Local Bcode daemon runtime.

use bcode_ipc::{
    CodecError, EnvelopeKind, ErrorResponse, Event, IpcEndpoint, LocalIpcListener, LocalIpcStream,
    PermissionSummary, PluginServiceError, PluginServiceResponse, PluginServiceSummary, Request,
    Response, ResponsePayload, ServerStatus, decode, event_envelope, recv_envelope,
    response_envelope, send_envelope,
};
use bcode_model::{
    CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID, MessageRole,
    ModelMessage, ModelParameters, ModelTurnRequest, OP_CANCEL_TURN, OP_FINISH_TURN,
    OP_POLL_TURN_EVENTS, OP_START_TURN, PollTurnEventsRequest, PollTurnEventsResponse,
    ProviderTurnEvent, StartTurnResponse,
};
use bcode_session::SessionManager;
use bcode_session_models::{ClientId, SessionEventKind, SessionId};
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID,
    ToolDefinition as ServiceToolDefinition, ToolInvocationRequest, ToolInvocationResponse,
    ToolList,
};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
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
}

#[derive(Debug)]
struct ServerState {
    sessions: SessionManager,
    plugins: Mutex<bcode_plugin::PluginHost>,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    permission_policy: Mutex<PermissionPolicy>,
    active_turns: Mutex<BTreeMap<SessionId, ActiveModelTurn>>,
    pending_permissions: Mutex<BTreeMap<String, PendingPermission>>,
    next_permission_id: Mutex<u64>,
    clients: Mutex<BTreeSet<ClientId>>,
    shutdown: broadcast::Sender<()>,
}

#[derive(Debug, Clone)]
struct ActiveModelTurn {
    provider_plugin_id: Option<String>,
    provider_turn_id: String,
}

#[derive(Debug, Clone)]
struct PendingPermission {
    summary: PermissionSummary,
    decision: Arc<Mutex<Option<bool>>>,
    notify: Arc<Notify>,
}

#[derive(Debug, Clone, Default)]
struct PermissionPolicy {
    allow_tools: BTreeSet<String>,
    deny_tools: BTreeSet<String>,
    allow_shell_command_prefixes: BTreeSet<String>,
    deny_shell_command_prefixes: BTreeSet<String>,
    allow_path_prefixes: BTreeSet<String>,
    deny_path_prefixes: BTreeSet<String>,
}

impl PermissionPolicy {
    fn add_rule(&mut self, kind: &str, value: String) -> Result<(), String> {
        match kind {
            "allow_tool" => self.allow_tools.insert(value),
            "deny_tool" => self.deny_tools.insert(value),
            "allow_shell_command_prefix" => self.allow_shell_command_prefixes.insert(value),
            "deny_shell_command_prefix" => self.deny_shell_command_prefixes.insert(value),
            "allow_path_prefix" => self.allow_path_prefixes.insert(value),
            "deny_path_prefix" => self.deny_path_prefixes.insert(value),
            _ => return Err(format!("unknown permission rule kind: {kind}")),
        };
        Ok(())
    }

    fn decision_for_call(&self, tool_name: &str, arguments: &serde_json::Value) -> Option<bool> {
        if self.deny_tools.contains(tool_name)
            || self.denies_shell_command(tool_name, arguments)
            || self.denies_path(tool_name, arguments)
        {
            Some(false)
        } else if self.allow_tools.contains(tool_name)
            || self.allows_shell_command(tool_name, arguments)
            || self.allows_path(tool_name, arguments)
        {
            Some(true)
        } else {
            None
        }
    }

    fn denies_shell_command(&self, tool_name: &str, arguments: &serde_json::Value) -> bool {
        tool_name == "shell.run"
            && string_argument(arguments, "command")
                .is_some_and(|command| has_prefix(command, &self.deny_shell_command_prefixes))
    }

    fn allows_shell_command(&self, tool_name: &str, arguments: &serde_json::Value) -> bool {
        tool_name == "shell.run"
            && string_argument(arguments, "command")
                .is_some_and(|command| has_prefix(command, &self.allow_shell_command_prefixes))
    }

    fn denies_path(&self, tool_name: &str, arguments: &serde_json::Value) -> bool {
        tool_name.starts_with("filesystem.")
            && string_argument(arguments, "path")
                .is_some_and(|path| has_prefix(path, &self.deny_path_prefixes))
    }

    fn allows_path(&self, tool_name: &str, arguments: &serde_json::Value) -> bool {
        tool_name.starts_with("filesystem.")
            && string_argument(arguments, "path")
                .is_some_and(|path| has_prefix(path, &self.allow_path_prefixes))
    }
}

impl From<&bcode_config::PermissionConfig> for PermissionPolicy {
    fn from(value: &bcode_config::PermissionConfig) -> Self {
        Self {
            allow_tools: value.allow_tools.clone(),
            deny_tools: value.deny_tools.clone(),
            allow_shell_command_prefixes: value.allow_shell_command_prefixes.clone(),
            deny_shell_command_prefixes: value.deny_shell_command_prefixes.clone(),
            allow_path_prefixes: value.allow_path_prefixes.clone(),
            deny_path_prefixes: value.deny_path_prefixes.clone(),
        }
    }
}

fn string_argument<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(serde_json::Value::as_str)
}

fn has_prefix(value: &str, prefixes: &BTreeSet<String>) -> bool {
    prefixes.iter().any(|prefix| value.starts_with(prefix))
}

impl ServerState {
    fn new(
        sessions: SessionManager,
        plugins: bcode_plugin::PluginHost,
        selected_provider_plugin_id: Option<String>,
        selected_model_id: Option<String>,
        permission_policy: PermissionPolicy,
    ) -> Self {
        let (shutdown, _) = broadcast::channel(1);
        Self {
            sessions,
            plugins: Mutex::new(plugins),
            selected_provider_plugin_id,
            selected_model_id,
            permission_policy: Mutex::new(permission_policy),
            active_turns: Mutex::default(),
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
    let config = bcode_config::load_config()?;
    let plugin_selection = bcode_plugin::PluginSelection::from(&config);
    let plugins = bcode_plugin::PluginHost::load_defaults(&plugin_selection)?;
    let listener = LocalIpcListener::bind(&endpoint).await?;
    let sessions = SessionManager::persistent(default_session_store_dir())?;
    let state = Arc::new(ServerState::new(
        sessions,
        plugins,
        config.model.provider_plugin_id.clone(),
        config.model.model_id.clone(),
        PermissionPolicy::from(&config.permissions),
    ));
    let mut shutdown = state.subscribe_shutdown();
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
    state.plugins.lock().await.deactivate_all()?;
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
        Request::Ping => {
            send_response(writer, request_id, Response::Ok(ResponsePayload::Pong)).await
        }
        Request::ServerStatus => handle_server_status(request_id, state, writer).await,
        Request::ServerStop => handle_server_stop(request_id, state, writer).await,
        Request::CreateSession { name } => {
            handle_create_session(request_id, state, writer, name).await
        }
        Request::ListSessions => handle_list_sessions(request_id, state, writer).await,
        Request::SessionHistory { session_id } => {
            handle_session_history(request_id, state, writer, session_id).await
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
        Request::SendUserMessage { session_id, text } => {
            handle_user_message(request_id, client_id, state, writer, session_id, text).await
        }
        Request::CancelSessionTurn { session_id } => {
            handle_cancel_session_turn(request_id, state, writer, session_id).await
        }
        Request::ListPermissions => handle_list_permissions(request_id, state, writer).await,
        Request::ResolvePermission {
            permission_id,
            approved,
        } => handle_resolve_permission(request_id, state, writer, &permission_id, approved).await,
        Request::AddPermissionRule { kind, value } => {
            handle_add_permission_rule(request_id, state, writer, &kind, value).await
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
    }
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
                    history: attachment.history,
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
        Ok(event) => {
            publish_session_event(state, &event).await;
            let state_for_turn = Arc::clone(state);
            tokio::spawn(async move {
                run_model_turn(&state_for_turn, session_id, &event).await;
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
    let cancel_result = {
        let plugins = state.plugins.lock().await;
        invoke_model_provider_json::<_, bcode_model::AckResponse>(
            &plugins,
            active_turn.provider_plugin_id.as_deref(),
            OP_CANCEL_TURN,
            &request,
        )
    };
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
                Response::Err(ErrorResponse::new("plugin_error", error.to_string())),
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
    state: &ServerState,
    writer: &SharedWriter,
    kind: &str,
    value: String,
) -> Result<(), ServerError> {
    match bcode_config::add_permission_rule(kind, value.clone()) {
        Ok(path) => {
            let add_result = state.permission_policy.lock().await.add_rule(kind, value);
            if let Err(error) = add_result {
                return send_response(
                    writer,
                    request_id,
                    Response::Err(ErrorResponse::new("invalid_permission_rule", error)),
                )
                .await;
            }
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

const MAX_MODEL_TOOL_ROUNDS: u8 = 8;

#[derive(Debug, Clone, Copy, Default)]
struct ModelPollOutcome {
    stop_reason: Option<bcode_model::StopReason>,
    should_continue: bool,
}

async fn run_model_turn(
    state: &ServerState,
    session_id: SessionId,
    trigger_event: &bcode_session_models::SessionEvent,
) {
    if !has_model_provider(state).await {
        return;
    }

    let provider_plugin_id = state.selected_provider_plugin_id.clone();
    for round in 0..=MAX_MODEL_TOOL_ROUNDS {
        let request = match build_model_turn_request(state, session_id, trigger_event, round).await
        {
            Ok(request) => request,
            Err(error) => {
                append_system_event(state, session_id, format!("model request error: {error}"))
                    .await;
                return;
            }
        };
        let Some(outcome) =
            run_model_turn_round(state, session_id, provider_plugin_id.as_deref(), &request).await
        else {
            return;
        };
        if !outcome.should_continue {
            return;
        }
        if round == MAX_MODEL_TOOL_ROUNDS {
            append_system_event(
                state,
                session_id,
                "model tool-call round limit reached".to_string(),
            )
            .await;
            return;
        }
    }
}

async fn run_model_turn_round(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    request: &ModelTurnRequest,
) -> Option<ModelPollOutcome> {
    let start = {
        let plugins = state.plugins.lock().await;
        invoke_model_provider_json::<_, StartTurnResponse>(
            &plugins,
            provider_plugin_id,
            OP_START_TURN,
            request,
        )
    };
    let start = match start {
        Ok(start) => start,
        Err(error) => {
            append_system_event(state, session_id, format!("model provider error: {error}")).await;
            return None;
        }
    };

    state.active_turns.lock().await.insert(
        session_id,
        ActiveModelTurn {
            provider_plugin_id: provider_plugin_id.map(ToString::to_string),
            provider_turn_id: start.provider_turn_id.clone(),
        },
    );

    let (assistant_text, outcome) = poll_model_turn_events(
        state,
        session_id,
        provider_plugin_id,
        &start.provider_turn_id,
    )
    .await;

    if !assistant_text.is_empty() {
        append_assistant_message_event(state, session_id, assistant_text).await;
    }

    let active_turn = state.active_turns.lock().await.remove(&session_id);
    let finish = FinishTurnRequest {
        provider_turn_id: start.provider_turn_id,
    };
    let _ = {
        let plugins = state.plugins.lock().await;
        invoke_model_provider_json::<_, bcode_model::AckResponse>(
            &plugins,
            active_turn
                .as_ref()
                .and_then(|turn| turn.provider_plugin_id.as_deref()),
            OP_FINISH_TURN,
            &finish,
        )
    };
    Some(outcome)
}

async fn poll_model_turn_events(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    provider_turn_id: &str,
) -> (String, ModelPollOutcome) {
    let mut assistant_text = String::new();
    let mut outcome = ModelPollOutcome::default();
    let mut empty_polls = 0_u16;
    for _ in 0..1_200 {
        let poll = PollTurnEventsRequest {
            provider_turn_id: provider_turn_id.to_string(),
        };
        let response = poll_model_turn(state, provider_plugin_id, &poll).await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                append_system_event(state, session_id, format!("model provider error: {error}"))
                    .await;
                break;
            }
        };
        if response.events.is_empty() {
            empty_polls += 1;
            if empty_polls > 50 {
                append_system_event(
                    state,
                    session_id,
                    "model provider produced no events before timeout".to_string(),
                )
                .await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        empty_polls = 0;
        for event in response.events {
            handle_provider_turn_event(state, session_id, event, &mut assistant_text, &mut outcome)
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
) -> Result<PollTurnEventsResponse, bcode_plugin::PluginServiceCallError> {
    let plugins = state.plugins.lock().await;
    invoke_model_provider_json::<_, PollTurnEventsResponse>(
        &plugins,
        provider_plugin_id,
        OP_POLL_TURN_EVENTS,
        poll,
    )
}

async fn handle_provider_turn_event(
    state: &ServerState,
    session_id: SessionId,
    event: ProviderTurnEvent,
    assistant_text: &mut String,
    outcome: &mut ModelPollOutcome,
) {
    match event {
        ProviderTurnEvent::TextDelta { text } => {
            assistant_text.push_str(&text);
            append_assistant_delta_event(state, session_id, text).await;
        }
        ProviderTurnEvent::Error { error } => {
            append_system_event(
                state,
                session_id,
                format!("model error {}: {}", error.code, error.message),
            )
            .await;
            outcome.stop_reason = Some(bcode_model::StopReason::Error);
        }
        ProviderTurnEvent::TurnFinished { stop_reason } => {
            outcome.should_continue = stop_reason == bcode_model::StopReason::ToolCall;
            outcome.stop_reason = Some(stop_reason);
        }
        ProviderTurnEvent::Cancelled => {
            append_system_event(state, session_id, "model turn cancelled".to_string()).await;
            outcome.stop_reason = Some(bcode_model::StopReason::Cancelled);
        }
        ProviderTurnEvent::ToolCallFinished { call } => {
            if !assistant_text.is_empty() {
                append_assistant_message_event(state, session_id, std::mem::take(assistant_text))
                    .await;
            }
            execute_model_tool(state, session_id, call).await;
        }
        ProviderTurnEvent::Warning { message } => {
            append_system_event(state, session_id, format!("model warning: {message}")).await;
        }
        ProviderTurnEvent::TurnStarted
        | ProviderTurnEvent::ToolCallStarted { .. }
        | ProviderTurnEvent::ReasoningDelta { .. }
        | ProviderTurnEvent::ToolCallDelta { .. }
        | ProviderTurnEvent::Usage { .. } => {}
    }
}

async fn has_model_provider(state: &ServerState) -> bool {
    let plugins = state.plugins.lock().await;
    if let Some(provider_plugin_id) = &state.selected_provider_plugin_id {
        return plugins.loaded_plugins().iter().any(|plugin| {
            plugin.manifest().id == *provider_plugin_id
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

async fn build_model_turn_request(
    state: &ServerState,
    session_id: SessionId,
    trigger_event: &bcode_session_models::SessionEvent,
    round: u8,
) -> Result<ModelTurnRequest, bcode_session::SessionError> {
    let history = state.sessions.session_history(session_id).await?;
    let messages = history
        .iter()
        .filter_map(session_event_to_model_message)
        .collect();
    Ok(ModelTurnRequest {
        session_id,
        turn_id: format!("{}-{}-{round}", session_id, trigger_event.sequence),
        model_id: state
            .selected_model_id
            .clone()
            .unwrap_or_else(|| "fake-echo".to_string()),
        system_prompt: None,
        messages,
        tools: collect_model_tools(state).await,
        parameters: ModelParameters::default(),
        metadata: std::collections::BTreeMap::new(),
    })
}

#[allow(clippy::significant_drop_tightening)]
async fn collect_model_tools(state: &ServerState) -> Vec<bcode_model::ToolDefinition> {
    let plugins = state.plugins.lock().await;
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
    let result = invoke_model_tool(state, session_id, &call)
        .await
        .unwrap_or_else(|error| ToolInvocationResponse {
            output: error,
            is_error: true,
        });
    append_tool_finished_event(state, session_id, call.id, result.output, result.is_error).await;
}

async fn invoke_model_tool(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
) -> Result<ToolInvocationResponse, String> {
    let (plugin_id, definition) = find_tool_provider(state, &call.name)
        .await?
        .ok_or_else(|| format!("tool not found: {}", call.name))?;
    if definition.requires_permission
        && !request_tool_permission(state, session_id, call, &definition).await
    {
        return Ok(ToolInvocationResponse {
            output: "permission denied".to_string(),
            is_error: true,
        });
    }
    let plugins = state.plugins.lock().await;
    plugins
        .invoke_service_json::<_, ToolInvocationResponse>(
            &plugin_id,
            TOOL_SERVICE_INTERFACE_ID,
            OP_INVOKE_TOOL,
            &ToolInvocationRequest {
                tool_call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        )
        .map_err(|error| error.to_string())
}

#[allow(clippy::significant_drop_tightening)]
async fn find_tool_provider(
    state: &ServerState,
    tool_name: &str,
) -> Result<Option<(String, ServiceToolDefinition)>, String> {
    let plugins = state.plugins.lock().await;
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
}

async fn request_tool_permission(
    state: &ServerState,
    session_id: SessionId,
    call: &bcode_model::ToolCall,
    definition: &ServiceToolDefinition,
) -> bool {
    let permission_id = next_permission_id(state).await;
    let arguments_json = serde_json::to_string(&call.arguments).unwrap_or_default();
    append_permission_requested_event(
        state,
        session_id,
        permission_id.clone(),
        call.id.clone(),
        definition.name.clone(),
        arguments_json.clone(),
    )
    .await;
    let policy_decision = state
        .permission_policy
        .lock()
        .await
        .decision_for_call(&definition.name, &call.arguments);
    if let Some(decision) = policy_decision {
        append_permission_resolved_event(state, session_id, permission_id, decision).await;
        return decision;
    }
    let pending = PendingPermission {
        summary: PermissionSummary {
            permission_id: permission_id.clone(),
            session_id,
            tool_call_id: call.id.clone(),
            tool_name: definition.name.clone(),
            arguments_json,
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

fn session_event_to_model_message(
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
                    output: result.clone(),
                    is_error: *is_error,
                },
            }],
        }),
        SessionEventKind::SystemMessage { text } => Some(ModelMessage {
            role: MessageRole::System,
            content: vec![ContentBlock::Text { text: text.clone() }],
        }),
        _ => None,
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

async fn handle_list_plugin_services(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let services = {
        let plugins = state.plugins.lock().await;
        plugin_service_summaries(&plugins)
    };
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
    let response = {
        let plugins = state.plugins.lock().await;
        plugins.invoke_service(plugin_id, interface_id, operation, payload)
    };
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
    let response = {
        let plugins = state.plugins.lock().await;
        plugins.invoke_service_by_interface(interface_id, operation, payload)
    };
    send_plugin_service_response(writer, request_id, response).await
}

async fn handle_publish_plugin_event(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    topic: &str,
    payload: &[u8],
) -> Result<(), ServerError> {
    let response = {
        let plugins = state.plugins.lock().await;
        plugins.publish_event(topic, payload)
    };
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
    let plugins = state.plugins.lock().await;
    if let Err(error) = plugins.publish_event(SESSION_EVENT_PLUGIN_TOPIC, &payload) {
        eprintln!("failed to publish plugin session event: {error}");
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
    if let Ok(path) = env::var("BCODE_STATE_DIR") {
        return PathBuf::from(path).join("sessions");
    }
    if let Ok(path) = env::var("XDG_STATE_HOME") {
        return PathBuf::from(path).join("bcode").join("sessions");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("bcode")
            .join("sessions");
    }
    env::temp_dir().join("bcode").join("sessions")
}
