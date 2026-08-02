//! Host adapter for native plugin-owned TUI surfaces.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_plugin_sdk::tui::{
    PluginSessionViewSubscription, PluginSessionViewSubscriptionRequest, PluginSessionViewUpdate,
    PluginTask, PluginTuiAction, PluginTuiHost, PluginTuiHostError, PluginTuiSurface,
    PluginWorkflowControlAction, PluginWorkflowControlFuture, PluginWorkflowControlResult,
    PluginWorkflowInspection, PluginWorkflowInspectionFuture, PluginWorkflowLookup,
    PluginWorkflowLookupFuture, PluginWorkflowStartFuture, PluginWorkflowStartRequest,
    PluginWorkflowStartResponse, PluginWorkflowStatus, PluginWorkflowSummary,
};
use bcode_session_models::SessionId;
use bcode_session_view::SessionView;
use bcode_session_view_models::PermissionView;
use bcode_session_view_models::SessionConnectionViewStatus;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use bmux_tui_runtime::InvalidationSignal;
use tokio::sync::mpsc;

use super::terminal_events::TuiInput;
use super::{TuiError, helpers};

const DEFAULT_PLUGIN_SESSION_VIEW_BUFFER: usize = 32;
const MAX_PLUGIN_SESSION_VIEW_BUFFER: usize = 256;

/// Host services for plugin-owned TUI surfaces running inside Bcode's TUI.
#[derive(Debug, Clone)]
struct BcodePluginTuiHost {
    handle: tokio::runtime::Handle,
    redraw: InvalidationSignal,
    client: BcodeClient,
}

impl BcodePluginTuiHost {
    /// Create a plugin TUI host from the current Tokio runtime.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime.
    #[must_use]
    fn current(redraw: InvalidationSignal, client: BcodeClient) -> Self {
        Self {
            handle: tokio::runtime::Handle::current(),
            redraw,
            client,
        }
    }
}

fn workflow_start_request(
    request: PluginWorkflowStartRequest,
) -> Result<bcode_ipc::WorkflowStartRequest, PluginTuiHostError> {
    let parent_scope = request.parent_session_id.to_string();
    if request.binding.scope_key != parent_scope {
        return Err(PluginTuiHostError::InvalidRequest(
            "workflow binding scope must match the active parent session".to_string(),
        ));
    }
    Ok(bcode_ipc::WorkflowStartRequest {
        identity: request.identity,
        definition: request.definition,
        run_id: request.run_id,
        workspace_snapshot: None,
        parent_session_id: request.parent_session_id,
        input: request.input,
        binding: bcode_workflow_store::WorkflowRunBinding {
            owner_plugin_id: request.binding.owner_plugin_id,
            workflow_kind: request.binding.workflow_kind,
            scope_key: request.binding.scope_key,
            display_label: request.binding.display_label,
            single_active: request.binding.single_active,
        },
        limits: bcode_workflow_store::WorkflowRunLimits::default(),
    })
}

impl PluginTuiHost for BcodePluginTuiHost {
    fn spawn(&self, task: PluginTask) {
        let redraw = self.redraw.clone();
        drop(self.handle.spawn(async move {
            task.await;
            redraw.request();
        }));
    }

    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        let redraw = self.redraw.clone();
        drop(self.handle.spawn_blocking(move || {
            task();
            redraw.request();
        }));
    }

    fn request_redraw(&self) {
        self.redraw.request();
    }

    fn copy_text(&self, text: String) -> Result<(), PluginTuiHostError> {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
        clipboard
            .set_text(text)
            .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
    }

    fn start_workflow(&self, request: PluginWorkflowStartRequest) -> PluginWorkflowStartFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let started = client
                .start_workflow(workflow_start_request(request)?)
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            Ok(PluginWorkflowStartResponse {
                run_id: started.run.run_id,
                runtime_work_id: started.runtime_work_id.to_string(),
            })
        })
    }

    fn associated_workflow(&self, lookup: PluginWorkflowLookup) -> PluginWorkflowLookupFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .associated_workflow_run(workflow_lookup(lookup))
                .await
                .map(|run| run.map(workflow_summary))
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn inspect_associated_workflow(
        &self,
        lookup: PluginWorkflowLookup,
        limit: usize,
    ) -> PluginWorkflowInspectionFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .inspect_associated_workflow_run(workflow_lookup(lookup), limit)
                .await
                .and_then(|inspection| inspection.map(workflow_inspection).transpose())
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn control_associated_workflow(
        &self,
        lookup: PluginWorkflowLookup,
        action: PluginWorkflowControlAction,
    ) -> PluginWorkflowControlFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let (run, changed) = client
                .control_associated_workflow_run(
                    workflow_lookup(lookup),
                    match action {
                        PluginWorkflowControlAction::Pause => {
                            bcode_ipc::WorkflowRunControlAction::Pause
                        }
                        PluginWorkflowControlAction::Resume => {
                            bcode_ipc::WorkflowRunControlAction::Resume
                        }
                        PluginWorkflowControlAction::Cancel => {
                            bcode_ipc::WorkflowRunControlAction::Cancel
                        }
                    },
                )
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            Ok(PluginWorkflowControlResult {
                run: run.map(workflow_summary),
                changed,
            })
        })
    }

    fn subscribe_session_view(
        &self,
        request: PluginSessionViewSubscriptionRequest,
    ) -> Result<PluginSessionViewSubscription, PluginTuiHostError> {
        let buffer = request
            .buffer
            .clamp(1, MAX_PLUGIN_SESSION_VIEW_BUFFER)
            .max(DEFAULT_PLUGIN_SESSION_VIEW_BUFFER.min(MAX_PLUGIN_SESSION_VIEW_BUFFER));
        let (sender, receiver) = mpsc::channel(buffer);
        let client = self.client.clone();
        let redraw = self.redraw.clone();
        drop(self.handle.spawn(async move {
            Box::pin(stream_plugin_session_view(client, request, sender, redraw)).await;
        }));
        Ok(PluginSessionViewSubscription { receiver })
    }
}

fn workflow_lookup(lookup: PluginWorkflowLookup) -> bcode_ipc::WorkflowRunBindingLookup {
    bcode_ipc::WorkflowRunBindingLookup {
        owner_plugin_id: lookup.owner_plugin_id,
        workflow_kind: lookup.workflow_kind,
        scope_key: lookup.scope_key,
    }
}

const fn workflow_status(status: bcode_workflow_store::RunStatus) -> PluginWorkflowStatus {
    match status {
        bcode_workflow_store::RunStatus::Running => PluginWorkflowStatus::Running,
        bcode_workflow_store::RunStatus::Paused => PluginWorkflowStatus::Paused,
        bcode_workflow_store::RunStatus::Completed => PluginWorkflowStatus::Completed,
        bcode_workflow_store::RunStatus::Failed => PluginWorkflowStatus::Failed,
        bcode_workflow_store::RunStatus::Cancelled => PluginWorkflowStatus::Cancelled,
        bcode_workflow_store::RunStatus::RepairRequired => PluginWorkflowStatus::RepairRequired,
    }
}

fn workflow_summary(run: bcode_workflow_store::WorkflowRunSummary) -> PluginWorkflowSummary {
    PluginWorkflowSummary {
        run_id: run.run_id,
        definition_id: run.definition_id,
        definition_version: run.definition_version,
        status: workflow_status(run.status),
        cancellation_requested: run.cancellation_requested_at_ms.is_some(),
        created_at_ms: run.created_at_ms,
        updated_at_ms: run.updated_at_ms,
    }
}

fn workflow_inspection(
    inspection: bcode_ipc::WorkflowRunInspection,
) -> Result<PluginWorkflowInspection, bcode_client::ClientError> {
    Ok(PluginWorkflowInspection {
        run: workflow_summary(inspection.run),
        definition: serde_json::from_str(&inspection.definition.definition_json)
            .map_err(|_| bcode_client::ClientError::UnexpectedResponse)?,
        waits: inspection
            .waits
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        attempts: inspection
            .attempts
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        events: inspection
            .events
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        grants: inspection
            .grants
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        resource_leases: inspection
            .resource_leases
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        outputs: inspection
            .outputs
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        child_session_ids: inspection
            .child_sessions
            .into_iter()
            .map(|session| session.id)
            .collect(),
    })
}

async fn stream_plugin_session_view(
    client: BcodeClient,
    request: PluginSessionViewSubscriptionRequest,
    sender: mpsc::Sender<PluginSessionViewUpdate>,
    redraw: InvalidationSignal,
) {
    if let Err(error) =
        stream_plugin_session_view_inner(client, request, sender.clone(), redraw.clone()).await
    {
        let _ = sender
            .send(PluginSessionViewUpdate::Disconnected {
                message: error.to_string(),
            })
            .await;
        redraw.request();
    }
}

async fn attach_plugin_session_view(
    client: &BcodeClient,
    connection: &mut bcode_client::ClientConnection,
    request: &PluginSessionViewSubscriptionRequest,
) -> Result<SessionView, bcode_client::ClientError> {
    let attached = connection
        .attach_session_projection_window_with_input_history(
            request.session_id,
            request.projection.clone(),
        )
        .await?;
    let runtime = attached.runtime_selection;
    let mut view = SessionView::new();
    view.set_session_summary(attached.session);
    view.set_runtime_selection(
        runtime.provider_plugin_id,
        runtime.requested_model_id.or(runtime.model_id),
        runtime.effective_model_id,
        runtime.reasoning_effort,
        runtime.reasoning_summary,
        None,
    );
    view.set_agent_id(runtime.agent_id);
    view.set_reasoning_presentation_policy(request.reasoning_policy);
    if let Some(window) = attached.projection_window.as_ref() {
        view.set_history_window_metadata(
            window.source_range.map(|range| range.start_sequence),
            window.source_range.map(|range| range.end_sequence),
            window.has_older,
            window.has_newer,
        );
    }
    view.apply_history(&attached.history);
    let permissions = client_permission_views(
        client.list_permissions().await.unwrap_or_default(),
        request.session_id,
    );
    view.set_pending_permissions(permissions);
    if let Ok(runtime_work) = client.list_runtime_work(request.session_id).await {
        view.set_runtime_work_snapshots(&runtime_work);
    }
    if let Ok(interactions) =
        super::effects::load_pending_interactions(client, request.session_id).await
    {
        view.set_pending_interactions(interactions);
    }
    view.set_connection_status(SessionConnectionViewStatus::Attached);
    Ok(view)
}

fn client_permission_views(
    permissions: Vec<bcode_ipc::PermissionSummary>,
    session_id: SessionId,
) -> Vec<PermissionView> {
    permissions
        .into_iter()
        .filter(|permission| permission.session_id == session_id)
        .map(|permission| PermissionView {
            permission_id: permission.permission_id,
            session_id: Some(permission.session_id),
            tool_call_id: permission.tool_call_id,
            tool_name: permission.tool_name,
            arguments_json: permission.arguments_json,
            batch: permission
                .batch
                .map(|batch| bcode_session_view_models::PermissionBatchView {
                    batch_id: batch.batch_id,
                    call_index: batch.call_index,
                    call_count: batch.call_count,
                }),
            agent_id: permission.agent_id,
            title: Some("Permission requested".to_string()),
            policy_source: permission.policy_source,
            detail: permission.policy_reason,
            resolved: false,
            approved: None,
            can_remember: permission.can_remember_policy,
        })
        .collect()
}

async fn send_plugin_session_snapshot(
    sender: &mpsc::Sender<PluginSessionViewUpdate>,
    redraw: &InvalidationSignal,
    view: &SessionView,
) -> bool {
    if sender
        .send(PluginSessionViewUpdate::Snapshot(Box::new(
            view.snapshot().clone(),
        )))
        .await
        .is_err()
    {
        return false;
    }
    redraw.request();
    true
}

async fn stream_plugin_session_view_inner(
    client: BcodeClient,
    request: PluginSessionViewSubscriptionRequest,
    sender: mpsc::Sender<PluginSessionViewUpdate>,
    redraw: InvalidationSignal,
) -> Result<(), bcode_client::ClientError> {
    let session_id = request.session_id;
    let mut connection = client.connect("bcode-plugin-tui-session-view").await?;
    let mut view = attach_plugin_session_view(&client, &mut connection, &request).await?;
    if !send_plugin_session_snapshot(&sender, &redraw, &view).await {
        return Ok(());
    }

    let mut reconnect_delay = std::time::Duration::from_millis(100);
    loop {
        let needs_resync = match connection.recv_event().await {
            Ok(BcodeEvent::SessionViewResyncRequired {
                session_id: required,
            }) if required == session_id => true,
            Ok(event) => {
                let changed = match event {
                    BcodeEvent::Session(event) | BcodeEvent::RuntimeWork(event)
                        if event.session_id == session_id =>
                    {
                        view.apply_event(&event);
                        true
                    }
                    BcodeEvent::SessionLive(event) if event.session_id == session_id => {
                        view.apply_live_event(&event);
                        true
                    }
                    BcodeEvent::Session(_)
                    | BcodeEvent::SessionLive(_)
                    | BcodeEvent::RuntimeWork(_)
                    | BcodeEvent::SessionViewResyncRequired { .. }
                    | BcodeEvent::SessionCatalogUpdated { .. } => false,
                };
                if changed && !send_plugin_session_snapshot(&sender, &redraw, &view).await {
                    return Ok(());
                }
                false
            }
            Err(_error) => true,
        };
        if !needs_resync {
            continue;
        }

        view.set_connection_status(SessionConnectionViewStatus::Reconnecting);
        if !send_plugin_session_snapshot(&sender, &redraw, &view).await {
            return Ok(());
        }
        drop(connection);
        loop {
            if sender.is_closed() {
                return Ok(());
            }
            if let Ok(mut next_connection) = client.connect("bcode-plugin-tui-session-view").await
                && let Ok(next_view) =
                    attach_plugin_session_view(&client, &mut next_connection, &request).await
            {
                view = next_view;
                if !send_plugin_session_snapshot(&sender, &redraw, &view).await {
                    return Ok(());
                }
                connection = next_connection;
                reconnect_delay = std::time::Duration::from_millis(100);
                break;
            }
            tokio::time::sleep(reconnect_delay).await;
            reconnect_delay = (reconnect_delay * 2).min(std::time::Duration::from_secs(2));
        }
    }
}

/// Run one plugin-owned native TUI surface with a fresh terminal input stream and return its close outcome.
///
/// # Errors
///
/// Returns an error when terminal I/O or terminal input fails.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface<W: Write>(
    terminal: &mut Terminal<&mut W>,
    surface: &mut dyn PluginTuiSurface,
) -> Result<Option<serde_json::Value>, TuiError> {
    let mut input = TuiInput::start();
    run_plugin_surface_with_input(terminal, &mut input, surface).await
}

/// Run one plugin-owned native TUI surface with the caller-owned terminal input stream.
///
/// Use this when a plugin surface is nested inside the main TUI runtime so there is only one
/// terminal event reader.
///
/// # Errors
///
/// Returns an error when terminal I/O or terminal input fails.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface_with_input<W: Write>(
    terminal: &mut Terminal<&mut W>,
    input: &mut TuiInput,
    surface: &mut dyn PluginTuiSurface,
) -> Result<Option<serde_json::Value>, TuiError> {
    let client = BcodeClient::default_endpoint();
    run_plugin_surface_with_input_and_client(terminal, input, surface, client).await
}

/// Terminal outcome from running a plugin-owned surface until it closes or requests navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSurfaceRunOutcome {
    /// Surface closed normally.
    Closed(Option<serde_json::Value>),
    /// Surface requested temporary navigation to the ordinary native session viewer.
    OpenSession(SessionId),
}

/// Run one plugin-owned native TUI surface with an explicit Bcode client.
///
/// # Errors
///
/// Returns an error when terminal I/O or terminal input fails.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface_with_input_and_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    input: &mut TuiInput,
    surface: &mut dyn PluginTuiSurface,
    client: BcodeClient,
) -> Result<Option<serde_json::Value>, TuiError> {
    match run_plugin_surface_until_navigation_with_input_and_client(
        terminal, input, surface, client,
    )
    .await?
    {
        PluginSurfaceRunOutcome::Closed(outcome) => Ok(outcome),
        PluginSurfaceRunOutcome::OpenSession(session_id) => {
            Ok(Some(serde_json::json!({ "open_session": session_id })))
        }
    }
}

/// Run one plugin-owned surface until it closes or requests temporary native-session navigation.
///
/// The caller retains the surface and input stream across `OpenSession`, allowing it to run the
/// ordinary session viewer and then invoke this function again to resume exact surface state.
///
/// # Errors
///
/// Returns an error when terminal I/O or terminal input fails.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface_until_navigation_with_input_and_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    input: &mut TuiInput,
    surface: &mut dyn PluginTuiSurface,
    client: BcodeClient,
) -> Result<PluginSurfaceRunOutcome, TuiError> {
    let redraw = InvalidationSignal::new();
    let host = BcodePluginTuiHost::current(redraw.clone(), client);
    let mut needs_redraw = true;
    let mut close_outcome = None;
    let mut should_exit = false;

    while !should_exit {
        if !cfg!(test) && helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }
        let action = apply_plugin_surface_action(
            surface.poll(&host),
            &mut needs_redraw,
            &mut close_outcome,
            &mut should_exit,
        );
        if let PluginSurfaceHostAction::OpenSession(session_id) = action {
            return Ok(PluginSurfaceRunOutcome::OpenSession(session_id));
        }
        if should_exit {
            continue;
        }
        let action = apply_plugin_surface_action(
            surface.drain_effects(&host).await,
            &mut needs_redraw,
            &mut close_outcome,
            &mut should_exit,
        );
        if let PluginSurfaceHostAction::OpenSession(session_id) = action {
            return Ok(PluginSurfaceRunOutcome::OpenSession(session_id));
        }
        if should_exit {
            continue;
        }
        if redraw.take() {
            needs_redraw = true;
        }
        if needs_redraw {
            terminal.draw(|frame| {
                let area = frame.area();
                surface.render(area, frame);
            })?;
            needs_redraw = false;
        }

        tokio::select! {
            event = input.recv() => {
                let Some(event) = event? else {
                    continue;
                };
                if handle_host_event(terminal, &event) {
                    needs_redraw = true;
                }
                let action = apply_plugin_surface_action(
                    surface.handle_event(&event, &host),
                    &mut needs_redraw,
                    &mut close_outcome,
                    &mut should_exit,
                );
                if let PluginSurfaceHostAction::OpenSession(session_id) = action {
                    return Ok(PluginSurfaceRunOutcome::OpenSession(session_id));
                }
            }
            () = redraw.wait() => {
                needs_redraw |= redraw.take();
            }
        }
    }

    Ok(PluginSurfaceRunOutcome::Closed(close_outcome))
}

/// Host request produced by a plugin surface action.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PluginSurfaceHostAction {
    None,
    OpenSession(SessionId),
}

fn handle_host_event<W: Write>(terminal: &mut Terminal<&mut W>, event: &Event) -> bool {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            true
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => true,
        Event::Key(_) | Event::Mouse(_) | Event::Paste(_) | Event::User(_) => false,
    }
}

fn apply_plugin_surface_action(
    action: PluginTuiAction,
    needs_redraw: &mut bool,
    close_outcome: &mut Option<serde_json::Value>,
    should_exit: &mut bool,
) -> PluginSurfaceHostAction {
    match action {
        PluginTuiAction::None => PluginSurfaceHostAction::None,
        PluginTuiAction::Redraw | PluginTuiAction::OpenSurface { .. } => {
            *needs_redraw = true;
            PluginSurfaceHostAction::None
        }
        PluginTuiAction::OpenSession { session_id } => {
            PluginSurfaceHostAction::OpenSession(session_id)
        }
        PluginTuiAction::Close { outcome } => {
            *close_outcome = outcome;
            *should_exit = true;
            PluginSurfaceHostAction::None
        }
        PluginTuiAction::RunCommand { command } => {
            *close_outcome = Some(serde_json::json!({ "run_command": command }));
            *should_exit = true;
            PluginSurfaceHostAction::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginSurfaceHostAction, apply_plugin_surface_action, workflow_start_request};
    use bcode_plugin_sdk::tui::{
        PluginTuiAction, PluginWorkflowBinding, PluginWorkflowStartRequest,
    };

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn loop_surface_closes_after_live_host_and_daemon_admit_workflow() {
        use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
        use bmux_tui::event::Event;
        use bmux_tui::geometry::Rect;
        use bmux_tui::terminal::Terminal;

        let socket_dir = tempfile::tempdir().expect("socket dir");
        let state_dir = tempfile::tempdir().expect("state dir");
        let previous_state_dir = std::env::var_os("BCODE_STATE_DIR");
        unsafe {
            std::env::set_var("BCODE_STATE_DIR", state_dir.path());
        }
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.path().join("server.sock"));
        let server_endpoint = endpoint.clone();
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/loop-plugin/bcode-plugin.toml"),
            bcode_loop_plugin::static_plugin(),
        );
        let mut server = tokio::spawn(async move {
            loop {
                match bcode_server::run_embedded_with_static_bundled(
                    server_endpoint.clone(),
                    std::slice::from_ref(&plugin),
                )
                .await
                {
                    Err(bcode_server::ServerError::Config(_)) => {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    result => break result,
                }
            }
        });
        let client = bcode_client::BcodeClient::new(endpoint.clone());
        let ready = async {
            loop {
                if client.server_status().await.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        tokio::select! {
            result = &mut server => panic!("server exited before ready: {result:?}"),
            result = tokio::time::timeout(std::time::Duration::from_secs(30), ready) => {
                result.expect("server ready");
            }
        }
        let session = client
            .create_session_in_working_directory(
                Some("surface loop".to_string()),
                std::env::current_dir().expect("cwd"),
            )
            .await
            .expect("session");
        let mut surface = bcode_loop_plugin::tui_registry()
            .open(
                "loop.start",
                bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
                    instance_id: "loop-start".to_string(),
                    repo_path: None,
                    target: None,
                    options: serde_json::json!({"session_id": session.id}),
                },
            )
            .await
            .expect("surface");
        let key = |key| {
            Event::Key(KeyStroke {
                key,
                modifiers: Modifiers::NONE,
            })
        };
        let mut events = "implement"
            .chars()
            .map(|ch| key(KeyCode::Char(ch)))
            .collect::<Vec<_>>();
        events.push(key(KeyCode::Tab));
        events.extend("done".chars().map(|ch| key(KeyCode::Char(ch))));
        events.push(key(KeyCode::Tab));
        events.push(key(KeyCode::Enter));
        let mut input = crate::terminal_events::TuiInput::from_events(events);
        let mut output = Vec::new();
        let mut terminal = Terminal::new(&mut output, Rect::new(0, 0, 100, 30));
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            super::run_plugin_surface_with_input_and_client(
                &mut terminal,
                &mut input,
                surface.as_mut(),
                client.clone(),
            ),
        )
        .await
        .expect("surface close timeout")
        .expect("surface host");
        assert!(
            outcome
                .as_ref()
                .and_then(|value| value["run_id"].as_str())
                .is_some()
        );
        assert!(
            client
                .associated_workflow_run(bcode_ipc::WorkflowRunBindingLookup {
                    owner_plugin_id: "bcode.loop".to_string(),
                    workflow_kind: "bcode.loop".to_string(),
                    scope_key: session.id.to_string(),
                })
                .await
                .expect("associated run")
                .is_some()
        );
        client.server_stop().await.expect("stop server");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
        unsafe {
            if let Some(previous) = previous_state_dir {
                std::env::set_var("BCODE_STATE_DIR", previous);
            } else {
                std::env::remove_var("BCODE_STATE_DIR");
            }
        }
    }

    #[test]
    fn loop_surface_request_crosses_real_host_start_boundary() {
        let session_id = bcode_session_models::SessionId::new();
        let definition = bcode_workflow::WorkflowBuilder::new(
            "surface-loop",
            bcode_workflow::Step::map("step", |value: bool| Ok(value)),
        )
        .build()
        .expect("workflow");
        let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
            "bcode.loop",
            definition.definition(),
        )
        .expect("identity");
        let request = PluginWorkflowStartRequest {
            identity: identity.clone(),
            definition: definition.definition().clone(),
            run_id: Some("surface-loop-run".to_string()),
            parent_session_id: session_id,
            input: serde_json::json!(true),
            binding: PluginWorkflowBinding {
                owner_plugin_id: "bcode.loop".to_string(),
                workflow_kind: "bcode.loop".to_string(),
                scope_key: session_id.to_string(),
                display_label: Some("Loop".to_string()),
                single_active: true,
            },
        };
        let ipc = workflow_start_request(request).expect("host request");
        assert_eq!(ipc.identity, identity);
        assert_eq!(ipc.parent_session_id, session_id);
        assert_eq!(ipc.binding.owner_plugin_id, "bcode.loop");
        assert_eq!(ipc.binding.scope_key, session_id.to_string());

        let expected = serde_json::json!({"run_id": "surface-loop-run"});
        let mut needs_redraw = false;
        let mut outcome = None;
        let mut should_exit = false;
        apply_plugin_surface_action(
            PluginTuiAction::Close {
                outcome: Some(expected.clone()),
            },
            &mut needs_redraw,
            &mut outcome,
            &mut should_exit,
        );
        assert!(should_exit);
        assert_eq!(outcome, Some(expected));
    }

    #[test]
    fn repeated_open_session_actions_do_not_close_surface_host() {
        let first = bcode_session_models::SessionId::new();
        let second = bcode_session_models::SessionId::new();
        let mut needs_redraw = false;
        let mut outcome = None;
        let mut should_exit = false;
        for session_id in [first, second] {
            assert_eq!(
                apply_plugin_surface_action(
                    PluginTuiAction::OpenSession { session_id },
                    &mut needs_redraw,
                    &mut outcome,
                    &mut should_exit,
                ),
                PluginSurfaceHostAction::OpenSession(session_id)
            );
            assert!(!should_exit);
            assert!(outcome.is_none());
        }
    }

    #[test]
    fn open_session_suspends_surface_without_closing_it() {
        let session_id = bcode_session_models::SessionId::new();
        let mut needs_redraw = false;
        let mut outcome = None;
        let mut should_exit = false;

        let action = apply_plugin_surface_action(
            PluginTuiAction::OpenSession { session_id },
            &mut needs_redraw,
            &mut outcome,
            &mut should_exit,
        );

        assert_eq!(action, PluginSurfaceHostAction::OpenSession(session_id));
        assert!(!should_exit);
        assert!(outcome.is_none());
    }

    #[test]
    fn bmux_invalidation_signal_coalesces_plugin_redraw_requests() {
        let redraw = bmux_tui_runtime::InvalidationSignal::new();
        redraw.request();
        redraw.request();

        assert!(redraw.take());
        assert!(!redraw.take());
        assert_eq!(redraw.requests(), 2);
        assert_eq!(redraw.coalesced(), 1);
    }

    #[test]
    fn plugin_surface_host_source_uses_bmux_redraw_latch() {
        let source = include_str!("plugin_surface_host.rs");
        assert!(source.contains("InvalidationSignal"));
    }

    #[test]
    fn asynchronous_surface_close_is_not_reduced_to_redraw() {
        let expected = serde_json::json!({"status": "started"});
        let mut needs_redraw = false;
        let mut outcome = None;
        let mut should_exit = false;

        apply_plugin_surface_action(
            PluginTuiAction::Close {
                outcome: Some(expected.clone()),
            },
            &mut needs_redraw,
            &mut outcome,
            &mut should_exit,
        );

        assert!(should_exit);
        assert_eq!(outcome, Some(expected));
        assert!(!needs_redraw);
    }

    #[test]
    fn asynchronous_surface_actions_share_host_semantics() {
        let mut needs_redraw = false;
        let mut outcome = None;
        let mut should_exit = false;
        apply_plugin_surface_action(
            PluginTuiAction::OpenSurface {
                surface_id: "next".to_string(),
            },
            &mut needs_redraw,
            &mut outcome,
            &mut should_exit,
        );
        assert!(needs_redraw);
        assert!(!should_exit);

        needs_redraw = false;
        apply_plugin_surface_action(
            PluginTuiAction::RunCommand {
                command: "/loop status".to_string(),
            },
            &mut needs_redraw,
            &mut outcome,
            &mut should_exit,
        );
        assert!(should_exit);
        assert_eq!(
            outcome,
            Some(serde_json::json!({"run_command": "/loop status"}))
        );
        assert!(!needs_redraw);
    }
}
