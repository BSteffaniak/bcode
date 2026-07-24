#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `HyperChad` application host for Bcode sessions.

#[cfg(feature = "renderer-html-actix")]
mod html_actix;

#[cfg(feature = "renderer-html-actix")]
pub use html_actix::{
    DEFAULT_BIND_ADDRESS, VIEWPORT, build_app, build_launch_url, init, init_with_snapshot,
    validate_bind_address,
};

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};

use bcode_client::{
    AttachedSessionHistory, BcodeClient, ClientError, SessionWatchEvent, SessionWatcher,
};
use bcode_session_models::{
    ProjectionWindow, ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
    ProjectionWindowRequest, ProjectionWindowTarget, SessionId, SessionProjectionKind,
    SessionSummary,
};
use bcode_session_view::{SessionView, execute_session_view_action};
use bcode_session_view_models::{
    ComposerDraftViewScope, InteractionViewSummary, MessageAcceptanceDispositionView,
    PromptPlacementView, SessionViewAction, SessionViewSnapshot,
};
use hyperchad::router::{RoutePath, RouteRequest, Router};
use serde::Deserialize;

/// Number of recent history events projected into the initial `HyperChad` snapshot.
pub const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;

/// Delay between failed daemon watcher reconnection attempts.
const WATCH_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// `HyperChad` application state shared by route and action handlers.
#[derive(Clone)]
pub struct HyperChadAppState {
    client: BcodeClient,
    access_token: Arc<str>,
    watched_sessions: Arc<Mutex<BTreeSet<SessionId>>>,
    history_windows: Arc<Mutex<BTreeMap<SessionId, ProjectionWindowRequest>>>,
    interaction_controllers: Arc<Mutex<LocalInteractionControllers>>,
    interaction_submissions: Arc<Mutex<BTreeSet<String>>>,
    renderer_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<ScopedSnapshotUpdate>>>>,
}

impl std::fmt::Debug for HyperChadAppState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HyperChadAppState")
            .field("client", &self.client)
            .field("access_token", &"[REDACTED]")
            .field("watched_sessions", &self.watched_sessions)
            .field("history_windows", &self.history_windows)
            .field("interaction_controllers", &self.interaction_controllers)
            .field("interaction_submissions", &self.interaction_submissions)
            .field(
                "renderer_configured",
                &self.renderer_tx.lock().map_or(true, |tx| tx.is_some()),
            )
            .finish()
    }
}

#[derive(Default)]
struct LocalInteractionControllers {
    entries: BTreeMap<String, bcode_plugin_sdk::interaction::BoxedPluginInteractionController>,
}

struct InteractionSubmissionGuard {
    interaction_id: String,
    submissions: Arc<Mutex<BTreeSet<String>>>,
}

impl InteractionSubmissionGuard {
    fn acquire(submissions: &Arc<Mutex<BTreeSet<String>>>, interaction_id: &str) -> Option<Self> {
        submissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(interaction_id.to_owned())
            .then(|| Self {
                interaction_id: interaction_id.to_owned(),
                submissions: Arc::clone(submissions),
            })
    }
}

impl Drop for InteractionSubmissionGuard {
    fn drop(&mut self) {
        self.submissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.interaction_id);
    }
}

impl std::fmt::Debug for LocalInteractionControllers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalInteractionControllers")
            .field("interaction_ids", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Opaque renderer-owned subscription scope.
///
/// Bcode application code stores and forwards this value without parsing or composing it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RenderSubscriptionScope(String);

impl std::fmt::Debug for RenderSubscriptionScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RenderSubscriptionScope([REDACTED])")
    }
}

#[derive(Debug)]
#[cfg_attr(not(feature = "renderer-html-actix"), allow(dead_code))]
struct ScopedSnapshotUpdate {
    scope: RenderSubscriptionScope,
    snapshot: SessionViewSnapshot,
    sessions: Vec<SessionSummary>,
}

struct SessionWatchContext {
    client: BcodeClient,
    render_scope: RenderSubscriptionScope,
    session_id: SessionId,
    renderer_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<ScopedSnapshotUpdate>>>>,
    history_windows: Arc<Mutex<BTreeMap<SessionId, ProjectionWindowRequest>>>,
    interaction_controllers: Arc<Mutex<LocalInteractionControllers>>,
}

#[derive(Debug, Deserialize)]
struct PromptForm {
    session_id: Option<String>,
    text: String,
    #[serde(default)]
    placement: PromptPlacementView,
}

#[derive(Debug, Deserialize)]
struct CancelTurnForm {
    session_id: String,
    #[serde(default)]
    clear_queue: bool,
}

#[derive(Debug, Deserialize)]
struct UpdateDraftForm {
    text: String,
}

#[derive(Debug, Deserialize)]
struct PermissionBatchForm {
    session_id: String,
    batch_id: String,
    approved: bool,
}

#[derive(Debug, Deserialize)]
struct PermissionForm {
    session_id: String,
    permission_id: String,
    approved: bool,
    #[serde(default)]
    remember: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InteractionInputKind {
    Activate,
    Change,
    Focus,
    Blur,
    Navigate,
    Submit,
    Cancel,
}

#[derive(Debug, Deserialize)]
struct HistoryWindowForm {
    session_id: String,
    direction: HistoryWindowDirection,
    anchor_sequence: u64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HistoryWindowDirection {
    Older,
    Newer,
}

#[derive(Debug, Deserialize)]
struct InteractionForm {
    session_id: String,
    interaction_id: String,
    kind: InteractionInputKind,
    control_id: Option<String>,
    value: Option<String>,
    #[serde(default)]
    value_is_json: bool,
    direction: Option<String>,
}

fn permission_action(form: PermissionForm) -> Result<(SessionId, SessionViewAction), &'static str> {
    let Some(session_id) = parse_session_id(&form.session_id) else {
        return Err("invalid session id");
    };
    Ok((
        session_id,
        SessionViewAction::ResolvePermission {
            permission_id: form.permission_id,
            approved: form.approved,
            remember: form.remember,
        },
    ))
}

fn permission_batch_action(
    form: PermissionBatchForm,
) -> Result<(SessionId, SessionViewAction), &'static str> {
    let Some(session_id) = parse_session_id(&form.session_id) else {
        return Err("invalid session id");
    };
    Ok((
        session_id,
        SessionViewAction::ResolvePermissionBatch {
            batch_id: form.batch_id,
            approved: form.approved,
        },
    ))
}

fn message_acceptance_status(
    disposition: MessageAcceptanceDispositionView,
    queue_position: Option<usize>,
) -> String {
    match disposition {
        MessageAcceptanceDispositionView::AppliedSteering => {
            "message applied to the active turn".to_owned()
        }
        MessageAcceptanceDispositionView::QueuedFollowUp => queue_position.map_or_else(
            || "message queued as a follow-up".to_owned(),
            |position| format!("message queued as follow-up {position}"),
        ),
        MessageAcceptanceDispositionView::QueuedTurn => queue_position.map_or_else(
            || "message queued for a future turn".to_owned(),
            |position| format!("message queued for future turn {position}"),
        ),
        MessageAcceptanceDispositionView::StartedTurn => "message started a new turn".to_owned(),
    }
}

impl HyperChadAppState {
    /// Create `HyperChad` application state from a daemon client and access capability.
    #[must_use]
    pub fn new(client: BcodeClient, access_token: impl Into<Arc<str>>) -> Self {
        let client = client.with_interaction_adapters(local_interaction_adapters());
        Self {
            client,
            access_token: access_token.into(),
            watched_sessions: Arc::new(Mutex::new(BTreeSet::new())),
            history_windows: Arc::new(Mutex::new(BTreeMap::new())),
            interaction_controllers: Arc::new(Mutex::new(LocalInteractionControllers::default())),
            interaction_submissions: Arc::new(Mutex::new(BTreeSet::new())),
            renderer_tx: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(feature = "renderer-html-actix")]
    fn render_home(
        &self,
        snapshot: &SessionViewSnapshot,
        sessions: &[SessionSummary],
    ) -> hyperchad::template::Containers {
        let context = html_actix::HtmlActixPresentationContext::new(Arc::clone(&self.access_token));
        bcode_hyperchad_ui::pages::home::home(snapshot, sessions, &context)
    }

    #[cfg(not(feature = "renderer-html-actix"))]
    fn render_home(
        &self,
        snapshot: &SessionViewSnapshot,
        sessions: &[SessionSummary],
    ) -> hyperchad::template::Containers {
        let _ = self;
        bcode_hyperchad_ui::pages::home::home(
            snapshot,
            sessions,
            &bcode_hyperchad_ui::context::StaticPresentationContext,
        )
    }

    fn ensure_session_watcher(&self, session_id: SessionId) {
        let mut watched = self
            .watched_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !watched.insert(session_id) {
            return;
        }
        drop(watched);

        let client = self.client.clone();
        #[cfg(feature = "renderer-html-actix")]
        let render_scope =
            html_actix::HtmlActixPresentationContext::new(Arc::clone(&self.access_token))
                .render_scope(session_id);
        #[cfg(not(feature = "renderer-html-actix"))]
        let render_scope = RenderSubscriptionScope(session_id.to_string());
        let renderer_tx = Arc::clone(&self.renderer_tx);
        let history_windows = Arc::clone(&self.history_windows);
        let interaction_controllers = Arc::clone(&self.interaction_controllers);
        let watched_sessions = Arc::clone(&self.watched_sessions);
        tokio::spawn(async move {
            if let Err(error) = Box::pin(watch_session_updates(SessionWatchContext {
                client,
                render_scope,
                session_id,
                renderer_tx: Arc::clone(&renderer_tx),
                history_windows,
                interaction_controllers,
            }))
            .await
            {
                tracing::error!("HyperChad session watcher failed for {session_id}: {error}");
            }
            watched_sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&session_id);
        });
    }

    /// Return the per-launch browser access token.
    #[must_use]
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    fn authorizes(&self, request: &RouteRequest) -> bool {
        request
            .query
            .get("token")
            .is_some_and(|token| token == self.access_token())
    }

    /// Return the daemon client used by this `HyperChad` application.
    #[must_use]
    pub const fn client(&self) -> &BcodeClient {
        &self.client
    }

    /// Load the initial renderer snapshot and catalog summaries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the catalog request.
    pub async fn initial_state(
        &self,
    ) -> Result<(SessionViewSnapshot, Vec<SessionSummary>), ClientError> {
        let session_list = self.client.list_sessions_with_status().await?;
        let mut snapshot = self
            .latest_session_snapshot(&session_list.sessions)
            .await?
            .unwrap_or_else(SessionViewSnapshot::empty);
        snapshot.catalog_status = catalog_view_status(session_list.catalog_status);
        Ok((snapshot, session_list.sessions))
    }

    async fn latest_session_snapshot(
        &self,
        sessions: &[SessionSummary],
    ) -> Result<Option<SessionViewSnapshot>, ClientError> {
        let Some(session) = sessions.iter().max_by_key(|session| session.updated_at_ms) else {
            return Ok(None);
        };
        self.session_snapshot(session.id).await.map(Some)
    }

    /// Load a bounded renderer-neutral snapshot for one session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the attach request.
    pub async fn session_snapshot(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> Result<SessionViewSnapshot, ClientError> {
        let request = self
            .history_windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned()
            .unwrap_or_else(hyperchad_projection_window_request);
        let mut connection = self.client.connect("bcode-hyperchad").await?;
        let attached =
            attach_hyperchad_projection_window_with_request(&mut connection, session_id, request)
                .await?;
        session_view_from_attached_history(&self.client, attached, &self.interaction_controllers)
            .await
    }
}

async fn session_view_from_attached_history(
    client: &BcodeClient,
    attached: AttachedSessionHistory,
    interaction_controllers: &Arc<Mutex<LocalInteractionControllers>>,
) -> Result<SessionViewSnapshot, ClientError> {
    let mut view = view_from_attached_history(&attached);
    hydrate_session_model_status(client, attached.session.id, &mut view).await?;
    hydrate_pending_permissions(client, attached.session.id, &mut view).await?;
    hydrate_pending_interactions(
        client,
        attached.session.id,
        &mut view,
        interaction_controllers,
    )
    .await?;
    Ok(snapshot_from_view(&view, &attached))
}

async fn hydrate_session_model_status(
    client: &BcodeClient,
    session_id: SessionId,
    view: &mut SessionView,
) -> Result<(), ClientError> {
    let status = client.session_model_status(session_id).await?;
    view.set_runtime_selection(
        status.provider_plugin_id,
        status.requested_model_id.or(status.model_id),
        status.effective_model_id,
        status.reasoning_effort,
        status.reasoning_summary,
        status.context_occupancy.map(|occupancy| *occupancy),
    );
    Ok(())
}

fn view_from_attached_history(attached: &AttachedSessionHistory) -> SessionView {
    let mut view = SessionView::new();
    view.apply_history(&attached.history);
    let runtime = &attached.runtime_selection;
    view.set_runtime_selection(
        runtime.provider_plugin_id.clone(),
        runtime
            .requested_model_id
            .clone()
            .or_else(|| runtime.model_id.clone()),
        runtime.effective_model_id.clone(),
        runtime.reasoning_effort.clone(),
        runtime.reasoning_summary.clone(),
        None,
    );
    if let Some(agent_id) = &runtime.agent_id {
        view.set_agent_id(Some(agent_id.clone()));
    }
    view
}

async fn hydrate_pending_permissions(
    client: &BcodeClient,
    session_id: SessionId,
    view: &mut SessionView,
) -> Result<(), ClientError> {
    for permission in client
        .list_permissions()
        .await?
        .into_iter()
        .filter(|permission| permission.session_id == session_id)
    {
        let title = Some(format!("Permission requested: {}", permission.tool_name));
        let detail = permission.policy_reason.clone();
        view.upsert_permission(bcode_session_view_models::PermissionView {
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
            title,
            policy_source: permission.policy_source,
            detail,
            resolved: false,
            approved: None,
            can_remember: permission.can_remember_policy,
        });
    }
    Ok(())
}

fn local_interaction_snapshot(
    exchange: &bcode_session_models::ToolExchangeRequest,
    interaction_controllers: &Arc<Mutex<LocalInteractionControllers>>,
) -> serde_json::Value {
    let interaction_id = &exchange.exchange_id;
    let mut controllers = interaction_controllers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !controllers.entries.contains_key(interaction_id)
        && let Some(controller) = local_interaction_controller(exchange)
    {
        controllers
            .entries
            .insert(interaction_id.clone(), controller);
    }
    controllers.entries.get(interaction_id).map_or_else(
        || exchange.payload.clone(),
        |controller| controller.snapshot_json(),
    )
}

async fn hydrate_pending_interactions(
    client: &BcodeClient,
    session_id: SessionId,
    view: &mut SessionView,
    interaction_controllers: &Arc<Mutex<LocalInteractionControllers>>,
) -> Result<(), ClientError> {
    let exchanges = client.list_pending_tool_exchanges().await?;
    let pending_ids = exchanges
        .iter()
        .map(|pending| pending.request.exchange_id.as_str())
        .collect::<BTreeSet<_>>();
    interaction_controllers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entries
        .retain(|interaction_id, _| pending_ids.contains(interaction_id.as_str()));
    for request in exchanges
        .into_iter()
        .filter(|request| request.session_id == session_id)
    {
        let exchange = request.request;
        let interaction_id = exchange.exchange_id.clone();
        let snapshot = local_interaction_snapshot(&exchange, interaction_controllers);
        let validation_error = snapshot
            .get("validation_error")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let adapter = local_interaction_adapter(&exchange);
        let kind = adapter.as_ref().map_or_else(
            || exchange.schema.clone(),
            |adapter| adapter.interaction_kind.clone(),
        );
        let surface_kind = adapter
            .and_then(|adapter| adapter.tui_surface_kind)
            .unwrap_or_else(|| exchange.schema.clone());
        let state = if validation_error.is_some() {
            bcode_session_view_models::InteractionViewState::ValidationError
        } else {
            bcode_session_view_models::InteractionViewState::Pending
        };
        let status_detail = validation_error;
        view.upsert_interaction(InteractionViewSummary {
            interaction_id,
            kind,
            surface_kind,
            tool_call_id: Some(exchange.invocation_id),
            title: Some(exchange.producer_id),
            required: exchange.response_policy
                == bcode_session_models::ToolExchangeResponsePolicy::Required,
            snapshot: Some(snapshot),
            state,
            status_detail,
            resolved: false,
            resolution: None,
        });
    }
    Ok(())
}

#[cfg(feature = "static-bundled-question-plugin")]
fn local_interaction_adapters()
-> Vec<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    bcode_bundled_plugins::interaction_adapters("web")
}

#[cfg(not(feature = "static-bundled-question-plugin"))]
const fn local_interaction_adapters()
-> Vec<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    Vec::new()
}

#[cfg(feature = "static-bundled-question-plugin")]
fn local_interaction_adapter(
    exchange: &bcode_session_models::ToolExchangeRequest,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    bcode_bundled_plugins::interaction_adapter(
        &exchange.producer_id,
        &exchange.schema,
        exchange.schema_version,
        "web",
    )
}

#[cfg(not(feature = "static-bundled-question-plugin"))]
const fn local_interaction_adapter(
    _exchange: &bcode_session_models::ToolExchangeRequest,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    None
}

#[cfg(feature = "static-bundled-question-plugin")]
fn local_interaction_controller(
    exchange: &bcode_session_models::ToolExchangeRequest,
) -> Option<bcode_plugin_sdk::interaction::BoxedPluginInteractionController> {
    let adapter = local_interaction_adapter(exchange)?;
    bcode_bundled_plugins::interaction_registry(&exchange.producer_id)?
        .open(&adapter.interaction_kind, exchange.payload.clone())
        .ok()
}

#[cfg(not(feature = "static-bundled-question-plugin"))]
const fn local_interaction_controller(
    _exchange: &bcode_session_models::ToolExchangeRequest,
) -> Option<bcode_plugin_sdk::interaction::BoxedPluginInteractionController> {
    None
}

const fn hyperchad_projection_window_request() -> ProjectionWindowRequest {
    hyperchad_projection_window_request_for_anchor(
        ProjectionWindowAnchor::Latest,
        ProjectionWindowDirection::Backward,
    )
}

const fn hyperchad_projection_window_request_for_anchor(
    anchor: ProjectionWindowAnchor,
    direction: ProjectionWindowDirection,
) -> ProjectionWindowRequest {
    ProjectionWindowRequest {
        projection: SessionProjectionKind::Transcript,
        anchor,
        direction,
        target: ProjectionWindowTarget {
            min_items: Some(64),
            min_estimated_rows: None,
            min_bytes: None,
            width_columns: None,
        },
        limits: ProjectionWindowLimits {
            max_items: INITIAL_HISTORY_EVENT_LIMIT,
            max_events_scanned: INITIAL_HISTORY_EVENT_LIMIT.saturating_mul(4),
            max_bytes: 2 * 1024 * 1024,
        },
    }
}

async fn attach_hyperchad_projection_window_with_request(
    connection: &mut bcode_client::ClientConnection,
    session_id: SessionId,
    request: ProjectionWindowRequest,
) -> Result<AttachedSessionHistory, ClientError> {
    connection
        .attach_session_projection_window_with_input_history(session_id, request)
        .await
}

fn apply_projection_window_metadata(
    snapshot: &mut SessionViewSnapshot,
    projection_window: Option<&ProjectionWindow>,
) {
    if let Some(window) = projection_window {
        snapshot.transcript.source_start_sequence =
            window.source_range.map(|range| range.start_sequence);
        snapshot.transcript.source_end_sequence =
            window.source_range.map(|range| range.end_sequence);
        snapshot.transcript.has_older_history = window.has_older;
        snapshot.transcript.has_newer_history = window.has_newer;
    }
}

fn catalog_view_status(
    status: bcode_ipc::SessionCatalogStatus,
) -> bcode_session_view_models::SessionCatalogViewStatus {
    match status {
        bcode_ipc::SessionCatalogStatus::NotStarted => {
            bcode_session_view_models::SessionCatalogViewStatus::NotStarted
        }
        bcode_ipc::SessionCatalogStatus::Loading => {
            bcode_session_view_models::SessionCatalogViewStatus::Loading
        }
        bcode_ipc::SessionCatalogStatus::Loaded => {
            bcode_session_view_models::SessionCatalogViewStatus::Loaded
        }
        bcode_ipc::SessionCatalogStatus::Degraded(message) => {
            bcode_session_view_models::SessionCatalogViewStatus::Degraded(message)
        }
        bcode_ipc::SessionCatalogStatus::Failed(message) => {
            bcode_session_view_models::SessionCatalogViewStatus::Failed(message)
        }
    }
}

fn snapshot_from_view(
    view: &SessionView,
    attached: &AttachedSessionHistory,
) -> SessionViewSnapshot {
    let mut snapshot = view.snapshot().clone();
    apply_projection_window_metadata(&mut snapshot, attached.projection_window.as_ref());
    snapshot.connection_status = bcode_session_view_models::SessionConnectionViewStatus::Attached;
    snapshot.session_id = Some(attached.session.id);
    snapshot.title = attached.session.title().map(ToOwned::to_owned);
    snapshot.working_directory = Some(attached.session.working_directory.clone());
    snapshot.composer.draft = attached.draft.clone().unwrap_or_default();
    snapshot.composer.can_submit = true;
    snapshot.session_summary = Some(attached.session.clone());
    snapshot
}

/// Build a renderer-neutral snapshot from bounded daemon attach history.
#[must_use]
pub fn snapshot_from_attached_history(attached: &AttachedSessionHistory) -> SessionViewSnapshot {
    let view = view_from_attached_history(attached);
    snapshot_from_view(&view, attached)
}

fn client_error_message(error: &ClientError) -> String {
    match error {
        ClientError::Transport(_) | ClientError::Codec(_) | ClientError::DaemonStart(_) => {
            "The local Bcode service is unavailable. Check that it is running, then try again."
                .to_owned()
        }
        ClientError::RequestTimeout { .. } => {
            "The local Bcode service did not respond in time. Try again.".to_owned()
        }
        ClientError::IncompatibleDaemon { .. } => {
            "The running Bcode service is incompatible with this application. Restart Bcode and try again."
                .to_owned()
        }
        ClientError::UnexpectedResponse | ClientError::UnexpectedEnvelope => {
            "The local Bcode service returned an unexpected response. Restart Bcode and try again."
                .to_owned()
        }
        ClientError::Server { code, .. } => match code.as_str() {
            "session_not_found" => "This session is no longer available.".to_owned(),
            "session_active_elsewhere" => {
                "This session is active in another client and cannot perform that action here."
                    .to_owned()
            }
            "session_repair_required" | "projection_stale" => {
                "This session needs repair before its full history is available.".to_owned()
            }
            "session_unavailable" | "session_writer_incompatible" => {
                "This session is temporarily unavailable.".to_owned()
            }
            "daemon_busy" => "Bcode is busy. Wait a moment, then try again.".to_owned(),
            _ => "The action could not be completed. Try again.".to_owned(),
        },
    }
}

#[cfg(feature = "renderer-html-actix")]
fn artifact_read_error_message(error: &ClientError) -> &'static str {
    match error {
        ClientError::Server { code, .. }
            if matches!(
                code.as_str(),
                "artifact_not_found" | "artifact_unavailable" | "artifact_read_failed"
            ) =>
        {
            "Image preview is unavailable. The session artifact may need to be regenerated."
        }
        _ => "Image preview is temporarily unavailable.",
    }
}

/// Build a state-backed application router.
#[must_use]
pub fn router_from_state(state: HyperChadAppState) -> Router {
    let root_state = state.clone();
    let session_state = state.clone();
    let submit_state = state.clone();
    let cancel_state = state.clone();
    let draft_state = state.clone();
    let permission_state = state.clone();
    let permission_batch_state = state.clone();
    let history_state = state.clone();
    #[cfg(feature = "renderer-html-actix")]
    let artifact_state = state.clone();
    let interaction_state = state;
    let router = Router::new()
        .with_route("/", move |request| {
            let state = root_state.clone();
            async move {
                if state.authorizes(&request) {
                    state.render_initial().await
                } else {
                    unauthorized_page()
                }
            }
        })
        .with_route(
            RoutePath::LiteralPrefix("/session/".to_string()),
            move |request| {
                let state = session_state.clone();
                async move {
                    if state.authorizes(&request) {
                        state.render_session_request(&request).await
                    } else {
                        unauthorized_page()
                    }
                }
            },
        )
        .with_route("/actions/submit-message", move |request| {
            let state = submit_state.clone();
            async move { state.handle_submit_message(request).await }
        })
        .with_route("/actions/cancel-turn", move |request| {
            let state = cancel_state.clone();
            async move { state.handle_cancel_turn(request).await }
        })
        .with_route(
            RoutePath::LiteralPrefix("/actions/update-draft/".to_string()),
            move |request| {
                let state = draft_state.clone();
                async move { state.handle_update_draft(request).await }
            },
        )
        .with_route("/actions/permission", move |request| {
            let state = permission_state.clone();
            async move { state.handle_permission(request).await }
        })
        .with_route("/actions/permission-batch", move |request| {
            let state = permission_batch_state.clone();
            async move { state.handle_permission_batch(request).await }
        })
        .with_route("/actions/history-window", move |request| {
            let state = history_state.clone();
            async move { state.handle_history_window(request).await }
        })
        .with_route("/actions/interaction", move |request| {
            let state = interaction_state.clone();
            async move { state.handle_interaction(request).await }
        });
    #[cfg(feature = "renderer-html-actix")]
    let router = router.with_route(
        RoutePath::LiteralPrefix("/artifacts/".to_owned()),
        move |request| {
            let state = artifact_state.clone();
            async move { state.handle_artifact(request).await }
        },
    );
    router
}

impl HyperChadAppState {
    async fn render_initial(&self) -> hyperchad::template::Containers {
        match self.initial_state().await {
            Ok((mut snapshot, sessions)) => {
                if let Some(session_id) = snapshot.session_id {
                    self.ensure_session_watcher(session_id);
                } else {
                    snapshot.connection_status =
                        bcode_session_view_models::SessionConnectionViewStatus::Connected;
                }
                self.render_home(&snapshot, &sessions)
            }
            Err(error) => error_page(&client_error_message(&error)),
        }
    }

    async fn render_session_request(
        &self,
        request: &RouteRequest,
    ) -> hyperchad::template::Containers {
        let session_list = match self.client.list_sessions_with_status().await {
            Ok(list) => list,
            Err(error) => return error_page(&client_error_message(&error)),
        };
        let catalog_status = catalog_view_status(session_list.catalog_status);
        let sessions = session_list.sessions;
        let Some(session_id) = session_id_from_path(&request.path) else {
            return error_page("invalid session path");
        };
        self.render_session(session_id, &sessions, catalog_status)
            .await
    }

    async fn handle_submit_message(
        &self,
        request: RouteRequest,
    ) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let Ok(form) = request.parse_form::<PromptForm>() else {
            return error_page(
                "The submitted form could not be read. Review the fields and try again.",
            );
        };
        let session_id = form.session_id.as_deref().and_then(parse_session_id);
        if form.text.trim().is_empty() {
            return self
                .render_session_or_initial(session_id, "prompt cannot be empty")
                .await;
        }
        let launch_working_directory = if session_id.is_none() {
            match std::env::current_dir() {
                Ok(working_directory) => Some(working_directory),
                Err(_) => {
                    return error_page(
                        "Bcode could not determine the working directory for a new session.",
                    );
                }
            }
        } else {
            None
        };
        let action = SessionViewAction::SubmitMessage {
            session_id,
            launch_working_directory,
            text: form.text,
            placement: form.placement,
        };
        match execute_session_view_action(&self.client, action).await {
            Ok(bcode_session_view_models::SessionViewActionOutcome::MessageAccepted {
                session_id,
                queue_position,
                disposition,
                ..
            }) => {
                let status = message_acceptance_status(disposition, queue_position);
                self.render_session_or_initial(Some(session_id), &status)
                    .await
            }
            Ok(_) => {
                self.render_session_or_initial(session_id, "message accepted")
                    .await
            }
            Err(error) => {
                self.render_session_or_initial(session_id, &client_error_message(&error))
                    .await
            }
        }
    }

    async fn handle_cancel_turn(&self, request: RouteRequest) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let Ok(form) = request.parse_form::<CancelTurnForm>() else {
            return error_page(
                "The submitted form could not be read. Review the fields and try again.",
            );
        };
        let Some(session_id) = parse_session_id(&form.session_id) else {
            return error_page("invalid session id");
        };
        let action = SessionViewAction::CancelTurn {
            session_id,
            clear_queue: form.clear_queue,
        };
        match execute_session_view_action(&self.client, action).await {
            Ok(_) => {
                self.render_session_or_initial(Some(session_id), "turn cancelled")
                    .await
            }
            Err(error) => {
                self.render_session_or_initial(Some(session_id), &client_error_message(&error))
                    .await
            }
        }
    }

    async fn handle_update_draft(&self, request: RouteRequest) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let Some(session_id) = request
            .path
            .strip_prefix("/actions/update-draft/")
            .and_then(parse_session_id)
        else {
            return error_page("invalid session path");
        };
        let Ok(form) = request.parse_form::<UpdateDraftForm>() else {
            return error_page(
                "The submitted form could not be read. Review the fields and try again.",
            );
        };
        let action = SessionViewAction::UpdateDraft {
            scope: ComposerDraftViewScope::Session { session_id },
            text: form.text,
        };
        match execute_session_view_action(&self.client, action).await {
            Ok(_) => {
                self.render_session_or_initial(Some(session_id), "draft saved")
                    .await
            }
            Err(error) => {
                self.render_session_or_initial(Some(session_id), &client_error_message(&error))
                    .await
            }
        }
    }

    async fn handle_permission(&self, request: RouteRequest) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let Ok(form) = request.parse_form::<PermissionForm>() else {
            return error_page(
                "The submitted form could not be read. Review the fields and try again.",
            );
        };
        let (session_id, action) = match permission_action(form) {
            Ok(value) => value,
            Err(message) => return error_page(message),
        };
        match execute_session_view_action(&self.client, action).await {
            Ok(_) => {
                self.render_session_or_initial(Some(session_id), "permission resolved")
                    .await
            }
            Err(error) => {
                self.render_session_or_initial(Some(session_id), &client_error_message(&error))
                    .await
            }
        }
    }

    async fn handle_permission_batch(
        &self,
        request: RouteRequest,
    ) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let Ok(form) = request.parse_form::<PermissionBatchForm>() else {
            return error_page(
                "The submitted form could not be read. Review the fields and try again.",
            );
        };
        let (session_id, action) = match permission_batch_action(form) {
            Ok(value) => value,
            Err(message) => return error_page(message),
        };
        match execute_session_view_action(&self.client, action).await {
            Ok(bcode_session_view_models::SessionViewActionOutcome::PermissionBatchResolved {
                resolved_count,
            }) => {
                self.render_session_or_initial(
                    Some(session_id),
                    &format!("resolved {resolved_count} batched permissions"),
                )
                .await
            }
            Ok(_) => {
                self.render_session_or_initial(Some(session_id), "permission batch resolved")
                    .await
            }
            Err(error) => {
                self.render_session_or_initial(Some(session_id), &client_error_message(&error))
                    .await
            }
        }
    }

    async fn handle_history_window(
        &self,
        request: RouteRequest,
    ) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let Ok(form) = request.parse_form::<HistoryWindowForm>() else {
            return error_page(
                "The submitted form could not be read. Review the fields and try again.",
            );
        };
        let Some(session_id) = parse_session_id(&form.session_id) else {
            return error_page("invalid session id");
        };
        let (anchor, direction) = match form.direction {
            HistoryWindowDirection::Older => (
                ProjectionWindowAnchor::BeforeSequence(form.anchor_sequence),
                ProjectionWindowDirection::Backward,
            ),
            HistoryWindowDirection::Newer => (
                ProjectionWindowAnchor::AfterSequence(form.anchor_sequence),
                ProjectionWindowDirection::Forward,
            ),
        };
        let request = hyperchad_projection_window_request_for_anchor(anchor, direction);
        let mut connection = match self.client.connect("bcode-hyperchad-history").await {
            Ok(connection) => connection,
            Err(error) => return error_page(&client_error_message(&error)),
        };
        let attached = match attach_hyperchad_projection_window_with_request(
            &mut connection,
            session_id,
            request.clone(),
        )
        .await
        {
            Ok(attached) => attached,
            Err(error) => return error_page(&client_error_message(&error)),
        };
        let at_tail = attached
            .projection_window
            .as_ref()
            .is_none_or(|window| !window.has_newer);
        if at_tail {
            self.history_windows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&session_id);
        } else {
            self.history_windows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(session_id, request);
        }
        let session_list = match self.client.list_sessions_with_status().await {
            Ok(list) => list,
            Err(error) => return error_page(&client_error_message(&error)),
        };
        let sessions = session_list.sessions.clone();
        let mut snapshot = match session_view_from_attached_history(
            &self.client,
            attached,
            &self.interaction_controllers,
        )
        .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => return error_page(&client_error_message(&error)),
        };
        snapshot.catalog_status = catalog_view_status(session_list.catalog_status);
        self.render_home(&snapshot, &sessions)
    }

    fn apply_local_interaction_input(
        &self,
        exchange: &bcode_session_models::ToolExchangeRequest,
        input: bcode_tool::InteractionInput,
    ) -> Option<bcode_tool::InteractionOutput> {
        let interaction_id = &exchange.exchange_id;
        let mut controllers = self
            .interaction_controllers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !controllers.entries.contains_key(interaction_id)
            && let Some(controller) = local_interaction_controller(exchange)
        {
            controllers
                .entries
                .insert(interaction_id.clone(), controller);
        }
        controllers
            .entries
            .get_mut(interaction_id)
            .map(|controller| controller.handle_input(input))
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_interaction(&self, request: RouteRequest) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let Ok(form) = request.parse_form::<InteractionForm>() else {
            return error_page(
                "The submitted form could not be read. Review the fields and try again.",
            );
        };
        let Some(session_id) = parse_session_id(&form.session_id) else {
            return error_page("invalid session id");
        };
        let input = match interaction_input_from_form(&form) {
            Ok(input) => input,
            Err(message) => {
                return self
                    .render_session_or_initial(Some(session_id), &message)
                    .await;
            }
        };
        let interaction_id = form.interaction_id.clone();
        let exchanges = match self.client.list_pending_tool_exchanges().await {
            Ok(exchanges) => exchanges,
            Err(error) => {
                return self
                    .render_session_or_initial(Some(session_id), &client_error_message(&error))
                    .await;
            }
        };
        let Some(exchange) = exchanges
            .into_iter()
            .find(|pending| {
                pending.session_id == session_id && pending.request.exchange_id == interaction_id
            })
            .map(|pending| pending.request)
        else {
            return self
                .render_session_or_initial(Some(session_id), "interaction is no longer pending")
                .await;
        };
        let controller_output = self.apply_local_interaction_input(&exchange, input);
        let resolution_and_status = if let Some(output) = controller_output {
            match output {
                bcode_tool::InteractionOutput::None | bcode_tool::InteractionOutput::Redraw => {
                    return self
                        .render_session_or_initial(Some(session_id), "interaction input accepted")
                        .await;
                }
                bcode_tool::InteractionOutput::Submitted { payload } => (
                    bcode_session_models::ToolExchangeResolution::Responded { payload },
                    "interaction submitted",
                ),
                bcode_tool::InteractionOutput::Cancelled => (
                    bcode_session_models::ToolExchangeResolution::Cancelled,
                    "interaction cancelled",
                ),
            }
        } else {
            match generic_interaction_resolution(&exchange, &form) {
                Ok(Some(resolution)) => resolution,
                Ok(None) => {
                    return self
                        .render_session_or_initial(
                            Some(session_id),
                            "This interaction supports only submit or cancel in the generic controls.",
                        )
                        .await;
                }
                Err(message) => {
                    return self
                        .render_session_or_initial(Some(session_id), &message)
                        .await;
                }
            }
        };
        let (resolution, status) = resolution_and_status;
        let Some(_submission) = InteractionSubmissionGuard::acquire(
            &self.interaction_submissions,
            &exchange.exchange_id,
        ) else {
            return self
                .render_interaction_status(
                    session_id,
                    &exchange.exchange_id,
                    bcode_session_view_models::InteractionViewState::Submitting,
                    "Interaction response is already being submitted.",
                )
                .await;
        };
        let client = if local_interaction_adapter(&exchange).is_none() {
            self.client
                .clone()
                .with_interaction_adapter(generic_interaction_adapter(&exchange))
        } else {
            self.client.clone()
        };
        let result = client
            .resolve_tool_exchange(exchange.exchange_id.clone(), resolution)
            .await;
        match result {
            Ok(true) => {
                self.interaction_controllers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entries
                    .remove(&exchange.exchange_id);
                self.render_session_or_initial(Some(session_id), status)
                    .await
            }
            Ok(false) => {
                self.render_session_or_initial(Some(session_id), "interaction is no longer pending")
                    .await
            }
            Err(error) => {
                self.render_interaction_status(
                    session_id,
                    &exchange.exchange_id,
                    bcode_session_view_models::InteractionViewState::ActionError,
                    client_error_message(&error),
                )
                .await
            }
        }
    }

    #[cfg(feature = "renderer-html-actix")]
    async fn handle_artifact(&self, request: RouteRequest) -> hyperchad::renderer::Content {
        const MAX_INLINE_IMAGE_BYTES: u32 = 1024 * 1024;

        let error = |message: &str| hyperchad::renderer::Content::Raw {
            data: message.as_bytes().to_vec().into(),
            content_type: "text/plain; charset=utf-8".to_owned(),
        };
        if !self.authorizes(&request) {
            return error("artifact access is not authorized");
        }
        let Some(session_id) = request
            .path
            .strip_prefix("/artifacts/")
            .and_then(parse_session_id)
        else {
            return error("invalid session artifact path");
        };
        let Some(artifact_id) = request.query.get("artifact_id") else {
            return error("missing artifact id");
        };
        let Some(reference_key) = request.query.get("reference_key") else {
            return error("missing artifact reference key");
        };
        let range = match self
            .client
            .session_artifact_range(
                session_id,
                artifact_id.clone(),
                reference_key.clone(),
                0,
                MAX_INLINE_IMAGE_BYTES,
            )
            .await
        {
            Ok(range) => range,
            Err(client_error) => return error(artifact_read_error_message(&client_error)),
        };
        let Some(content_type) = range.content_type.as_deref() else {
            return error("artifact has no declared content type");
        };
        if !is_safe_inline_image_content_type(content_type) {
            return error("artifact is not a supported image resource");
        }
        if !range.is_eof() || range.complete == Some(false) {
            return error("image artifact exceeds the safe inline preview limit");
        }
        hyperchad::renderer::Content::Raw {
            data: range.bytes.into(),
            content_type: content_type.to_owned(),
        }
    }

    async fn render_interaction_status(
        &self,
        session_id: SessionId,
        interaction_id: &str,
        state: bcode_session_view_models::InteractionViewState,
        detail: impl Into<String>,
    ) -> hyperchad::template::Containers {
        let session_list = match self.client.list_sessions_with_status().await {
            Ok(list) => list,
            Err(error) => return error_page(&client_error_message(&error)),
        };
        let sessions = session_list.sessions;
        match self.session_snapshot(session_id).await {
            Ok(mut snapshot) => {
                if let Some(interaction) = snapshot
                    .interactions
                    .iter_mut()
                    .find(|interaction| interaction.interaction_id == interaction_id)
                {
                    interaction.state = state;
                    interaction.status_detail = Some(detail.into());
                }
                self.render_home(&snapshot, &sessions)
            }
            Err(error) => error_page(&client_error_message(&error)),
        }
    }

    async fn render_session_or_initial(
        &self,
        session_id: Option<SessionId>,
        status: &str,
    ) -> hyperchad::template::Containers {
        let (notice_level, notice_message) = semantic_notice(status);
        let session_list = match self.client.list_sessions_with_status().await {
            Ok(list) => list,
            Err(error) => return error_page(&client_error_message(&error)),
        };
        let catalog_status = catalog_view_status(session_list.catalog_status);
        let sessions = session_list.sessions;
        match session_id {
            Some(session_id) => {
                self.render_session_with_status(session_id, &sessions, catalog_status, status)
                    .await
            }
            None => match self.initial_state().await {
                Ok((mut snapshot, sessions)) => {
                    snapshot.notice = Some(bcode_session_view_models::SessionViewNotice {
                        level: notice_level,
                        message: notice_message,
                    });
                    self.render_home(&snapshot, &sessions)
                }
                Err(error) => error_page(&client_error_message(&error)),
            },
        }
    }

    async fn render_session(
        &self,
        session_id: SessionId,
        sessions: &[SessionSummary],
        catalog_status: bcode_session_view_models::SessionCatalogViewStatus,
    ) -> hyperchad::template::Containers {
        self.ensure_session_watcher(session_id);
        self.render_session_with_status(session_id, sessions, catalog_status, "connected")
            .await
    }

    async fn render_session_with_status(
        &self,
        session_id: SessionId,
        sessions: &[SessionSummary],
        catalog_status: bcode_session_view_models::SessionCatalogViewStatus,
        status: &str,
    ) -> hyperchad::template::Containers {
        match self.session_snapshot(session_id).await {
            Ok(mut snapshot) => {
                snapshot.catalog_status = catalog_status;
                let (level, message) = semantic_notice(status);
                snapshot.notice =
                    Some(bcode_session_view_models::SessionViewNotice { level, message });
                self.render_home(&snapshot, sessions)
            }
            Err(error) => error_page(&client_error_message(&error)),
        }
    }
}

fn semantic_notice(status: &str) -> (bcode_session_view_models::SessionViewNoticeLevel, String) {
    let normalized = status.to_ascii_lowercase();
    let level = if [
        "error",
        "failed",
        "invalid",
        "cannot",
        "unavailable",
        "no longer",
    ]
    .iter()
    .any(|term| normalized.contains(term))
    {
        bcode_session_view_models::SessionViewNoticeLevel::Error
    } else if ["reconnect", "resync", "degraded", "repair", "pending"]
        .iter()
        .any(|term| normalized.contains(term))
    {
        bcode_session_view_models::SessionViewNoticeLevel::Warning
    } else {
        bcode_session_view_models::SessionViewNoticeLevel::Info
    };
    let message = match normalized.as_str() {
        "connected" => "Connected to the session.".to_owned(),
        "interaction is no longer pending" => {
            "This interaction was already resolved elsewhere.".to_owned()
        }
        _ => status.to_owned(),
    };
    (level, message)
}

fn generic_interaction_resolution(
    exchange: &bcode_session_models::ToolExchangeRequest,
    form: &InteractionForm,
) -> Result<Option<(bcode_session_models::ToolExchangeResolution, &'static str)>, String> {
    match form.kind {
        InteractionInputKind::Cancel => Ok(Some((
            bcode_session_models::ToolExchangeResolution::Cancelled,
            "interaction cancelled",
        ))),
        InteractionInputKind::Submit => {
            let payload = match form.value.as_deref() {
                Some(value) if form.value_is_json => serde_json::from_str(value)
                    .map_err(|error| format!("invalid interaction response JSON: {error}"))?,
                Some(value) => serde_json::Value::String(value.to_owned()),
                None => exchange.payload.clone(),
            };
            Ok(Some((
                bcode_session_models::ToolExchangeResolution::Responded { payload },
                "interaction submitted",
            )))
        }
        InteractionInputKind::Activate
        | InteractionInputKind::Change
        | InteractionInputKind::Focus
        | InteractionInputKind::Blur
        | InteractionInputKind::Navigate => Ok(None),
    }
}

fn generic_interaction_adapter(
    exchange: &bcode_session_models::ToolExchangeRequest,
) -> bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability {
    bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability {
        producer_id: exchange.producer_id.clone(),
        exchange_schema: exchange.schema.clone(),
        min_schema_version: exchange.schema_version,
        max_schema_version: exchange.schema_version,
        platform_id: "web-generic".to_owned(),
        priority: 0,
        interaction_kind: exchange.schema.clone(),
        tui_surface_kind: None,
    }
}

fn interaction_input_from_form(
    form: &InteractionForm,
) -> Result<bcode_tool::InteractionInput, String> {
    use bcode_tool::{
        InteractionControlId, InteractionInput, InteractionNavigation, InteractionValue,
    };

    let control_id = || {
        form.control_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(InteractionControlId::new)
            .ok_or_else(|| "interaction control id is required".to_owned())
    };

    match form.kind {
        InteractionInputKind::Activate => Ok(InteractionInput::Activate {
            control_id: control_id()?,
        }),
        InteractionInputKind::Change => {
            let value = form
                .value
                .as_deref()
                .ok_or_else(|| "interaction value is required".to_owned())?;
            let value = if form.value_is_json {
                serde_json::from_str::<InteractionValue>(value)
                    .map_err(|error| format!("invalid interaction value JSON: {error}"))?
            } else {
                InteractionValue::String(value.to_owned())
            };
            Ok(InteractionInput::Change {
                control_id: control_id()?,
                value,
            })
        }
        InteractionInputKind::Focus => Ok(InteractionInput::Focus {
            control_id: control_id()?,
        }),
        InteractionInputKind::Blur => Ok(InteractionInput::Blur {
            control_id: control_id()?,
        }),
        InteractionInputKind::Navigate => match form.direction.as_deref() {
            Some("next") => Ok(InteractionInput::Navigate {
                direction: InteractionNavigation::Next,
            }),
            Some("previous") => Ok(InteractionInput::Navigate {
                direction: InteractionNavigation::Previous,
            }),
            _ => Err("interaction navigation direction must be next or previous".to_owned()),
        },
        InteractionInputKind::Submit => Ok(InteractionInput::Submit),
        InteractionInputKind::Cancel => Ok(InteractionInput::Cancel),
    }
}

#[cfg(feature = "renderer-html-actix")]
fn is_safe_inline_image_content_type(content_type: &str) -> bool {
    matches!(
        content_type.split(';').next().map(str::trim),
        Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
    )
}

fn session_id_from_path(path: &str) -> Option<SessionId> {
    path.strip_prefix("/session/").and_then(parse_session_id)
}

fn parse_session_id(value: &str) -> Option<SessionId> {
    SessionId::from_str(value).ok()
}

fn unauthorized_page() -> hyperchad::template::Containers {
    error_page("missing or invalid HyperChad application access capability")
}

fn error_page(message: &str) -> hyperchad::template::Containers {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.title = Some("HyperChad application error".to_owned());
    snapshot.connection_status = bcode_session_view_models::SessionConnectionViewStatus::Error(
        "The application could not load session data.".to_owned(),
    );
    snapshot.notice = Some(bcode_session_view_models::SessionViewNotice {
        level: bcode_session_view_models::SessionViewNoticeLevel::Error,
        message: message.to_owned(),
    });
    bcode_hyperchad_ui::pages::home::home(
        &snapshot,
        &[],
        &bcode_hyperchad_ui::context::StaticPresentationContext,
    )
}

/// Build the application router for the current snapshot and session list.
#[must_use]
pub fn router(snapshot: SessionViewSnapshot, sessions: Vec<SessionSummary>) -> Router {
    let snapshot = Arc::new(snapshot);
    let sessions = Arc::new(sessions);
    Router::new().with_static_route(&["/", "/session"], move |_| {
        let snapshot = Arc::clone(&snapshot);
        let sessions = Arc::clone(&sessions);
        async move {
            bcode_hyperchad_ui::pages::home::home(
                &snapshot,
                &sessions,
                &bcode_hyperchad_ui::context::StaticPresentationContext,
            )
        }
    })
}

async fn attach_watched_session(
    client: &BcodeClient,
    session_id: SessionId,
    interaction_controllers: &Arc<Mutex<LocalInteractionControllers>>,
) -> Result<(SessionWatcher, AttachedSessionHistory, SessionView), ClientError> {
    let mut watcher = client
        .watch_session_projection_window(session_id, hyperchad_projection_window_request())
        .await?;
    let attached = watcher
        .take_initial()
        .ok_or(ClientError::UnexpectedResponse)?;
    let mut view = view_from_attached_history(&attached);
    hydrate_session_model_status(client, session_id, &mut view).await?;
    hydrate_pending_permissions(client, session_id, &mut view).await?;
    hydrate_pending_interactions(client, session_id, &mut view, interaction_controllers).await?;
    Ok((watcher, attached, view))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DurableEventDisposition {
    Apply,
    IgnoreDuplicate,
    ResyncGap,
}

const fn durable_event_disposition(
    latest_sequence: Option<u64>,
    sequence: u64,
) -> DurableEventDisposition {
    match latest_sequence {
        Some(latest) if sequence <= latest => DurableEventDisposition::IgnoreDuplicate,
        Some(latest) if sequence > latest.saturating_add(1) => DurableEventDisposition::ResyncGap,
        _ => DurableEventDisposition::Apply,
    }
}

fn browser_update_sender(
    renderer_tx: &Arc<Mutex<Option<tokio::sync::mpsc::Sender<ScopedSnapshotUpdate>>>>,
) -> Option<tokio::sync::mpsc::Sender<ScopedSnapshotUpdate>> {
    renderer_tx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

async fn watched_session_snapshot(
    client: &BcodeClient,
    session_id: SessionId,
    attached: &AttachedSessionHistory,
    view: &SessionView,
    history_windows: &Arc<Mutex<BTreeMap<SessionId, ProjectionWindowRequest>>>,
    interaction_controllers: &Arc<Mutex<LocalInteractionControllers>>,
) -> Result<SessionViewSnapshot, ClientError> {
    let history_request = history_windows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&session_id)
        .cloned();
    if let Some(request) = history_request {
        let mut connection = client.connect("bcode-hyperchad-history-refresh").await?;
        let historical =
            attach_hyperchad_projection_window_with_request(&mut connection, session_id, request)
                .await?;
        session_view_from_attached_history(client, historical, interaction_controllers).await
    } else {
        Ok(snapshot_from_view(view, attached))
    }
}

fn apply_watched_event(
    view: &mut SessionView,
    session_id: SessionId,
    event: SessionWatchEvent,
) -> bool {
    match event {
        SessionWatchEvent::Durable(event) => {
            match durable_event_disposition(view.snapshot().latest_sequence, event.sequence) {
                DurableEventDisposition::Apply => view.apply_event(&event),
                DurableEventDisposition::IgnoreDuplicate => {}
                DurableEventDisposition::ResyncGap => {
                    tracing::warn!(
                        "HyperChad session watcher detected event gap for {session_id}: latest={:?}, received={}",
                        view.snapshot().latest_sequence,
                        event.sequence
                    );
                    return true;
                }
            }
            false
        }
        SessionWatchEvent::Live(event) => {
            view.apply_live_event(&event);
            false
        }
        SessionWatchEvent::ResyncRequired => true,
    }
}

type AttachedWatchState = (SessionWatcher, AttachedSessionHistory, SessionView);

async fn attach_watch_with_retry(
    context: &SessionWatchContext,
    operation: &str,
) -> Result<Option<AttachedWatchState>, ClientError> {
    loop {
        match attach_watched_session(
            &context.client,
            context.session_id,
            &context.interaction_controllers,
        )
        .await
        {
            Ok(state) => return Ok(Some(state)),
            Err(error) => {
                if browser_update_sender(&context.renderer_tx).is_none() {
                    return Ok(None);
                }
                tracing::warn!(
                    "HyperChad session watcher {operation} failed for {}: {error}",
                    context.session_id
                );
                tokio::time::sleep(WATCH_RECONNECT_DELAY).await;
            }
        }
    }
}

async fn send_connection_update(
    context: &SessionWatchContext,
    attached: &AttachedSessionHistory,
    view: &SessionView,
    status: bcode_session_view_models::SessionConnectionViewStatus,
) -> Result<bool, ClientError> {
    let Some(sender) = browser_update_sender(&context.renderer_tx) else {
        return Ok(false);
    };
    let session_list = context.client.list_sessions_with_status().await;
    let (sessions, catalog_status) = match session_list {
        Ok(list) => (list.sessions, catalog_view_status(list.catalog_status)),
        Err(_) => (
            vec![attached.session.clone()],
            bcode_session_view_models::SessionCatalogViewStatus::Degraded(
                "Session navigation is temporarily unavailable while reconnecting.".to_owned(),
            ),
        ),
    };
    let mut snapshot = watched_session_snapshot(
        &context.client,
        context.session_id,
        attached,
        view,
        &context.history_windows,
        &context.interaction_controllers,
    )
    .await
    .unwrap_or_else(|_| snapshot_from_view(view, attached));
    snapshot.connection_status = status;
    snapshot.catalog_status = catalog_status;
    Ok(sender
        .send(ScopedSnapshotUpdate {
            scope: context.render_scope.clone(),
            snapshot,
            sessions,
        })
        .await
        .is_ok())
}

async fn send_watched_snapshot(
    context: &SessionWatchContext,
    attached: &mut AttachedSessionHistory,
    view: &SessionView,
) -> Result<bool, ClientError> {
    let session_list = context.client.list_sessions_with_status().await?;
    if let Some(summary) = session_list
        .sessions
        .iter()
        .find(|summary| summary.id == context.session_id)
    {
        attached.session.clone_from(summary);
    }
    let mut snapshot = watched_session_snapshot(
        &context.client,
        context.session_id,
        attached,
        view,
        &context.history_windows,
        &context.interaction_controllers,
    )
    .await?;
    snapshot.catalog_status = catalog_view_status(session_list.catalog_status);
    let Some(sender) = browser_update_sender(&context.renderer_tx) else {
        return Ok(false);
    };
    Ok(sender
        .send(ScopedSnapshotUpdate {
            scope: context.render_scope.clone(),
            snapshot,
            sessions: session_list.sessions,
        })
        .await
        .is_ok())
}

async fn watch_session_updates(context: SessionWatchContext) -> Result<(), ClientError> {
    let SessionWatchContext {
        client,
        session_id,
        renderer_tx,
        interaction_controllers,
        ..
    } = &context;
    let Some((mut watcher, mut attached, mut view)) =
        attach_watch_with_retry(&context, "attach").await?
    else {
        return Ok(());
    };

    loop {
        let event = match watcher.next_event().await {
            Ok(event) => event,
            Err(error) => {
                if browser_update_sender(renderer_tx).is_none() {
                    return Ok(());
                }
                tracing::warn!("HyperChad session watcher disconnected for {session_id}: {error}");
                if !send_connection_update(
                    &context,
                    &attached,
                    &view,
                    bcode_session_view_models::SessionConnectionViewStatus::Reconnecting,
                )
                .await?
                {
                    return Ok(());
                }
                tokio::time::sleep(WATCH_RECONNECT_DELAY).await;
                let Some((new_watcher, new_attached, new_view)) =
                    attach_watch_with_retry(&context, "reconnect").await?
                else {
                    return Ok(());
                };
                watcher = new_watcher;
                attached = new_attached;
                view = new_view;
                if !send_connection_update(
                    &context,
                    &attached,
                    &view,
                    bcode_session_view_models::SessionConnectionViewStatus::Attached,
                )
                .await?
                {
                    return Ok(());
                }
                continue;
            }
        };

        let resync = apply_watched_event(&mut view, *session_id, event);

        if resync {
            if !send_connection_update(
                &context,
                &attached,
                &view,
                bcode_session_view_models::SessionConnectionViewStatus::Resyncing,
            )
            .await?
            {
                return Ok(());
            }
            let Some(state) = attach_watch_with_retry(&context, "resync").await? else {
                return Ok(());
            };
            (watcher, attached, view) = state;
        } else {
            hydrate_session_model_status(client, *session_id, &mut view).await?;
            hydrate_pending_permissions(client, *session_id, &mut view).await?;
            hydrate_pending_interactions(client, *session_id, &mut view, interaction_controllers)
                .await?;
        }

        if !send_watched_snapshot(&context, &mut attached, &view).await? {
            return Ok(());
        }
    }
}

/// Configure scoped live snapshot rendering through a `HyperChad` renderer.
#[cfg(feature = "renderer-html-actix")]
pub fn configure_live_updates<R>(renderer: &R, state: &HyperChadAppState)
where
    R: hyperchad::renderer::Renderer + Clone + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ScopedSnapshotUpdate>(1);
    *state
        .renderer_tx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    let renderer = renderer.clone();
    let access_token = Arc::clone(&state.access_token);
    tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            let context = html_actix::HtmlActixPresentationContext::new(Arc::clone(&access_token));
            let containers =
                bcode_hyperchad_ui::pages::home::home(&update.snapshot, &update.sessions, &context);
            if let Err(error) = renderer
                .render_scoped(update.scope.0, containers.into())
                .await
            {
                tracing::error!("failed to render scoped HyperChad snapshot: {error}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "static-bundled-question-plugin")]
    #[test]
    fn question_controller_snapshot_survives_authoritative_rehydration() {
        let exchange = bcode_session_models::ToolExchangeRequest {
            invocation_id: "call-1".to_owned(),
            exchange_id: "exchange-1".to_owned(),
            producer_id: "bcode.question".to_owned(),
            schema: "bcode.question.request".to_owned(),
            schema_version: 1,
            payload: serde_json::json!({
                "questions": [{
                    "header": null,
                    "question": "Explain?",
                    "options": [{"label": "Keep", "value": "keep", "description": null}],
                    "control": "radio",
                    "selection_mode": "single",
                    "custom": true,
                    "custom_mode": "additional",
                    "required": true
                }]
            }),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let controllers = Arc::new(Mutex::new(LocalInteractionControllers::default()));
        let first = local_interaction_snapshot(&exchange, &controllers);
        assert_eq!(first["answers"][0]["custom"], serde_json::Value::Null);
        let output = {
            let mut controllers = controllers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            controllers
                .entries
                .get_mut(&exchange.exchange_id)
                .expect("question controller")
                .handle_input(bcode_tool::InteractionInput::Change {
                    control_id: bcode_tool::InteractionControlId::new("question-0.custom"),
                    value: bcode_tool::InteractionValue::String("preserved answer".to_owned()),
                })
        };
        assert_eq!(output, bcode_tool::InteractionOutput::Redraw);

        let rerendered = local_interaction_snapshot(&exchange, &controllers);
        let reconnected = local_interaction_snapshot(&exchange, &controllers);
        assert_eq!(rerendered["answers"][0]["custom"], "preserved answer");
        assert_eq!(reconnected, rerendered);
    }

    #[test]
    fn interaction_submission_guard_rejects_duplicates_and_releases_on_drop() {
        let submissions = Arc::new(Mutex::new(BTreeSet::new()));
        let guard = InteractionSubmissionGuard::acquire(&submissions, "interaction-1")
            .expect("first submission should acquire the guard");
        assert!(InteractionSubmissionGuard::acquire(&submissions, "interaction-1").is_none());
        assert!(InteractionSubmissionGuard::acquire(&submissions, "interaction-2").is_some());
        drop(guard);
        assert!(InteractionSubmissionGuard::acquire(&submissions, "interaction-1").is_some());
    }

    #[cfg(feature = "static-bundled-question-plugin")]
    #[test]
    fn hyperchad_runs_question_adapter_locally_from_opaque_exchange() {
        let exchange = bcode_session_models::ToolExchangeRequest {
            invocation_id: "call-1".to_owned(),
            exchange_id: "exchange-1".to_owned(),
            producer_id: "bcode.question".to_owned(),
            schema: "bcode.question.request".to_owned(),
            schema_version: 1,
            payload: serde_json::json!({
                "questions": [{
                    "header": null,
                    "question": "Proceed?",
                    "options": [{
                        "label": "Yes",
                        "value": "yes",
                        "description": null
                    }],
                    "control": "radio",
                    "selection_mode": "single",
                    "custom": false,
                    "custom_mode": "additional",
                    "required": false
                }]
            }),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let mut controller = local_interaction_controller(&exchange)
            .expect("question adapter should be available locally");
        assert_eq!(
            controller.handle_input(bcode_tool::InteractionInput::Activate {
                control_id: bcode_tool::InteractionControlId::new("question-0.option-0"),
            }),
            bcode_tool::InteractionOutput::Redraw
        );
        let snapshot = controller.snapshot_json();

        assert_eq!(snapshot["answers"][0]["selected"][0], "yes");
        assert_eq!(
            controller.handle_input(bcode_tool::InteractionInput::Submit),
            bcode_tool::InteractionOutput::Submitted {
                payload: serde_json::json!({
                    "status": "answered",
                    "questions": [{
                        "question_index": 0,
                        "selected": ["yes"],
                        "custom": null
                    }]
                }),
            }
        );
        let adapter = local_interaction_adapter(&exchange).expect("question adapter capability");
        assert_eq!(adapter.interaction_kind, "bcode.question");
        assert_eq!(adapter.platform_id, "web");
        assert_eq!(adapter.tui_surface_kind, None);
    }

    #[test]
    fn hyperchad_projection_closes_resolved_permission_but_preserves_transcript_record() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };
        view.apply_event(&event(
            1,
            bcode_session_models::SessionEventKind::PermissionRequested {
                permission_id: "permission-1".to_owned(),
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("example.plugin".to_owned()),
                tool_name: "example.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                legacy_request_presentation: None,
                batch: None,
                policy_source: None,
                policy_reason: None,
            },
        ));
        assert_eq!(view.snapshot().permissions.len(), 1);

        view.apply_event(&event(
            2,
            bcode_session_models::SessionEventKind::PermissionResolved {
                permission_id: "permission-1".to_owned(),
                approved: false,
            },
        ));

        assert!(view.snapshot().permissions.is_empty());
        assert!(view.snapshot().transcript.items.iter().any(|item| {
            matches!(
                &item.kind,
                bcode_session_view_models::TranscriptViewItemKind::Permission { permission }
                    if permission.resolved && permission.approved == Some(false)
            )
        }));
    }

    #[test]
    fn hyperchad_preserves_compact_single_tool_activity_until_terminal_event() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        view.apply_event(&bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "opaque-call".to_owned(),
                    sequence: 0,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Waiting,
                    message: Some("waiting generically".to_owned()),
                    metadata: serde_json::json!({"opaque": true}),
                },
            },
        });

        let rendered = format!(
            "{:?}",
            bcode_hyperchad_ui::pages::home::home(
                view.snapshot(),
                &[],
                &bcode_hyperchad_ui::context::StaticPresentationContext
            )
        );
        assert!(rendered.contains("active tool"));
        assert!(!rendered.contains("active invocations"));
        assert!(rendered.contains("opaque-call"));
        assert!(rendered.contains("waiting generically"));

        view.apply_event(&bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 2,
            timestamp_ms: 2,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "opaque-call".to_owned(),
                    sequence: 1,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Completed,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        });
        let completed = format!(
            "{:?}",
            bcode_hyperchad_ui::pages::home::home(
                view.snapshot(),
                &[],
                &bcode_hyperchad_ui::context::StaticPresentationContext
            )
        );
        assert!(!completed.contains("active tool"));
        assert!(!completed.contains("active invocations"));
    }

    #[test]
    fn hyperchad_uses_grouped_heading_only_for_multiple_active_invocations() {
        let session_id = SessionId::new();
        let mut view = SessionView::new();
        for (sequence, invocation_id) in [(1, "first"), (2, "second")] {
            view.apply_event(&bcode_session_models::SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence,
                timestamp_ms: sequence,
                session_id,
                provenance: None,
                kind: bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: invocation_id.to_owned(),
                        sequence: 0,
                        stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                        message: None,
                        metadata: serde_json::Value::Null,
                    },
                },
            });
        }

        let rendered = format!(
            "{:?}",
            bcode_hyperchad_ui::pages::home::home(
                view.snapshot(),
                &[],
                &bcode_hyperchad_ui::context::StaticPresentationContext
            )
        );
        assert!(rendered.contains("active invocations"));
        assert!(!rendered.contains("active tool"));
    }

    #[test]
    fn hyperchad_projection_keeps_active_sibling_and_does_not_revive_terminal_work() {
        let session_id = SessionId::new();
        let first = bcode_session_models::WorkId::new("work-first");
        let second = bcode_session_models::WorkId::new("work-second");
        let mut view = SessionView::new();
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };
        let started = |work_id: bcode_session_models::WorkId, label: &str| {
            bcode_session_models::SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind: bcode_session_models::RuntimeWorkKind::Tool,
                label: label.to_owned(),
                tool_call_id: None,
                plugin_id: None,
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(1),
                cancellable: true,
            }
        };
        view.apply_event(&event(1, started(first.clone(), "first")));
        view.apply_event(&event(2, started(second.clone(), "second")));
        view.apply_event(&event(
            3,
            bcode_session_models::SessionEventKind::RuntimeWorkFinished {
                work_id: first.clone(),
                status: bcode_session_models::RuntimeWorkStatus::Completed,
                finished_at_ms: Some(3),
                message: None,
            },
        ));
        view.apply_event(&event(4, started(first, "revived-marker-unique")));

        let snapshot = view.snapshot();
        assert_eq!(snapshot.runtime_work.len(), 1);
        assert_eq!(snapshot.runtime_work[0].work_id, second);
        let rendered = format!(
            "{:?}",
            bcode_hyperchad_ui::pages::home::home(
                snapshot,
                &[],
                &bcode_hyperchad_ui::context::StaticPresentationContext
            )
        );
        assert!(rendered.contains("work-second"));
        assert!(!rendered.contains("revived-marker-unique"));
    }

    #[test]
    fn projection_window_metadata_populates_history_availability() {
        let mut snapshot = SessionViewSnapshot::empty();
        let window = ProjectionWindow {
            projection: SessionProjectionKind::Transcript,
            transcript_items: Vec::new(),
            source_range: None,
            has_older: true,
            has_newer: false,
            scanned_events: 12,
        };

        apply_projection_window_metadata(&mut snapshot, Some(&window));

        assert_eq!(snapshot.transcript.source_start_sequence, None);
        assert_eq!(snapshot.transcript.source_end_sequence, None);
        assert!(snapshot.transcript.has_older_history);
        assert!(!snapshot.transcript.has_newer_history);
    }

    #[test]
    fn history_window_requests_use_strict_source_anchored_directions() {
        let older = hyperchad_projection_window_request_for_anchor(
            ProjectionWindowAnchor::BeforeSequence(10),
            ProjectionWindowDirection::Backward,
        );
        assert_eq!(older.anchor, ProjectionWindowAnchor::BeforeSequence(10));
        assert_eq!(older.direction, ProjectionWindowDirection::Backward);

        let newer = hyperchad_projection_window_request_for_anchor(
            ProjectionWindowAnchor::AfterSequence(20),
            ProjectionWindowDirection::Forward,
        );
        assert_eq!(newer.anchor, ProjectionWindowAnchor::AfterSequence(20));
        assert_eq!(newer.direction, ProjectionWindowDirection::Forward);
    }

    #[test]
    fn attached_runtime_selection_populates_authoritative_agent() {
        let session_id = SessionId::new();
        let mut attached = AttachedSessionHistory {
            session: SessionSummary {
                id: session_id,
                name: Some("session".to_owned()),
                explicit_name: Some("session".to_owned()),
                derived_title: None,
                title_source: bcode_session_models::SessionTitleSource::Explicit,
                client_count: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
                working_directory: std::path::PathBuf::from("/tmp"),
                import: None,
                fork: None,
                execution: None,
            },
            history: Vec::new(),
            input_history: Vec::new(),
            import_warnings: Vec::new(),
            draft: None,
            runtime_selection: bcode_ipc::SessionRuntimeSelection::default(),
            projection_window: None,
        };
        attached.runtime_selection.agent_id = Some("build".to_owned());

        let snapshot = snapshot_from_attached_history(&attached);

        assert_eq!(snapshot.runtime.agent_id.as_deref(), Some("build"));
    }

    #[test]
    fn permission_forms_preserve_individual_remember_and_batch_semantics() {
        let session_id = SessionId::new();
        for (approved, remember) in [(true, false), (false, false), (true, true), (false, true)] {
            assert_eq!(
                permission_action(PermissionForm {
                    session_id: session_id.to_string(),
                    permission_id: "permission-1".to_owned(),
                    approved,
                    remember,
                }),
                Ok((
                    session_id,
                    SessionViewAction::ResolvePermission {
                        permission_id: "permission-1".to_owned(),
                        approved,
                        remember,
                    },
                ))
            );
        }
        for approved in [true, false] {
            assert_eq!(
                permission_batch_action(PermissionBatchForm {
                    session_id: session_id.to_string(),
                    batch_id: "batch-1".to_owned(),
                    approved,
                }),
                Ok((
                    session_id,
                    SessionViewAction::ResolvePermissionBatch {
                        batch_id: "batch-1".to_owned(),
                        approved,
                    },
                ))
            );
        }
    }

    #[test]
    fn message_acceptance_status_preserves_every_authoritative_disposition() {
        assert_eq!(
            message_acceptance_status(MessageAcceptanceDispositionView::AppliedSteering, None),
            "message applied to the active turn"
        );
        assert_eq!(
            message_acceptance_status(MessageAcceptanceDispositionView::QueuedFollowUp, Some(2)),
            "message queued as follow-up 2"
        );
        assert_eq!(
            message_acceptance_status(MessageAcceptanceDispositionView::QueuedFollowUp, None),
            "message queued as a follow-up"
        );
        assert_eq!(
            message_acceptance_status(MessageAcceptanceDispositionView::QueuedTurn, Some(3)),
            "message queued for future turn 3"
        );
        assert_eq!(
            message_acceptance_status(MessageAcceptanceDispositionView::QueuedTurn, None),
            "message queued for a future turn"
        );
        assert_eq!(
            message_acceptance_status(MessageAcceptanceDispositionView::StartedTurn, None),
            "message started a new turn"
        );
    }

    #[test]
    fn watched_events_preserve_full_snapshot_resync_fallback() {
        let session_id = SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: text.to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        };
        let mut view = SessionView::new();
        assert!(!apply_watched_event(
            &mut view,
            session_id,
            SessionWatchEvent::Durable(Box::new(event(1, "first"))),
        ));
        let applied = view.snapshot().clone();
        assert_eq!(applied.latest_sequence, Some(1));

        for stale in [event(1, "duplicate"), event(0, "stale")] {
            assert!(!apply_watched_event(
                &mut view,
                session_id,
                SessionWatchEvent::Durable(Box::new(stale)),
            ));
            assert_eq!(view.snapshot(), &applied);
        }
        assert!(apply_watched_event(
            &mut view,
            session_id,
            SessionWatchEvent::Durable(Box::new(event(3, "gap"))),
        ));
        assert_eq!(view.snapshot(), &applied);
        assert!(apply_watched_event(
            &mut view,
            session_id,
            SessionWatchEvent::ResyncRequired,
        ));
    }

    #[test]
    fn durable_event_disposition_detects_duplicates_and_gaps() {
        assert_eq!(
            durable_event_disposition(None, 7),
            DurableEventDisposition::Apply
        );
        assert_eq!(
            durable_event_disposition(Some(7), 7),
            DurableEventDisposition::IgnoreDuplicate
        );
        assert_eq!(
            durable_event_disposition(Some(7), 6),
            DurableEventDisposition::IgnoreDuplicate
        );
        assert_eq!(
            durable_event_disposition(Some(7), 8),
            DurableEventDisposition::Apply
        );
        assert_eq!(
            durable_event_disposition(Some(7), 9),
            DurableEventDisposition::ResyncGap
        );
    }

    #[cfg(feature = "renderer-html-actix")]
    #[test]
    fn html_actix_accessibility_css_guarantees_focus_and_control_targets() {
        assert!(html_actix::accessibility_css().contains(":focus-visible"));
        assert!(html_actix::accessibility_css().contains("outline: 3px solid #58a6ff"));
        assert!(html_actix::accessibility_css().contains("min-height: 44px"));
        assert!(html_actix::accessibility_css().contains("overflow-x: auto"));
        assert!(html_actix::accessibility_css().contains("max-width: 100%"));
    }

    #[cfg(feature = "renderer-html-actix")]
    #[test]
    fn html_actix_renderer_init_smoke_test() {
        let builder = init_with_snapshot(SessionViewSnapshot::empty(), Vec::new());
        drop(builder);
    }

    #[cfg(feature = "renderer-html-actix")]
    #[test]
    fn html_actix_launch_url_preserves_guard_and_exact_session_scope() {
        let address = "127.0.0.1:4321".parse().expect("socket address");
        let session_id = SessionId::new();

        assert_eq!(
            build_launch_url(address, "secret-token", None),
            "http://127.0.0.1:4321/?token=secret-token"
        );
        assert_eq!(
            build_launch_url(address, "secret-token", Some(session_id)),
            format!(
                "http://127.0.0.1:4321/?token=secret-token&hyperchad-event-scope=secret-token:{session_id}"
            )
        );
    }

    #[cfg(feature = "renderer-html-actix")]
    #[test]
    fn html_actix_artifact_target_is_guarded_and_percent_encoded() {
        use bcode_hyperchad_ui::context::PresentationContext as _;

        let session_id = SessionId::new();
        let context = html_actix::HtmlActixPresentationContext::new(Arc::from("secret token"));
        let target = context
            .artifact_target(session_id, "artifact / one", "inline image")
            .expect("HTML backend exposes guarded artifact bytes");

        assert_eq!(
            target,
            format!(
                "/artifacts/{session_id}?token=secret+token&artifact_id=artifact+%2F+one&reference_key=inline+image"
            )
        );
        assert!(!target.contains("bcode-artifact://"));
    }

    #[cfg(feature = "renderer-html-actix")]
    #[test]
    fn bind_address_policy_requires_non_loopback_opt_in() {
        let loopback = "127.0.0.1".parse().expect("loopback should parse");
        let external = "0.0.0.0".parse().expect("external address should parse");

        assert_eq!(validate_bind_address(loopback, false), Ok(loopback));
        assert!(validate_bind_address(external, false).is_err());
        assert_eq!(validate_bind_address(external, true), Ok(external));
    }

    #[test]
    fn browser_capability_is_redacted_from_debug_output() {
        let token = "browser-capability-must-not-appear";
        let state = HyperChadAppState::new(BcodeClient::default_endpoint(), token);
        let state_debug = format!("{state:?}");
        assert!(state_debug.contains("[REDACTED]"));
        assert!(!state_debug.contains(token));

        let scope = RenderSubscriptionScope(format!("{token}:{}", SessionId::new()));
        assert!(!format!("{scope:?}").contains(token));

        #[cfg(feature = "renderer-html-actix")]
        {
            let context =
                html_actix::HtmlActixPresentationContext::new(Arc::from(token.to_owned()));
            let context_debug = format!("{context:?}");
            assert!(context_debug.contains("[REDACTED]"));
            assert!(!context_debug.contains(token));
        }
    }

    #[test]
    fn client_errors_use_stable_user_facing_language() {
        let cases = [
            (
                ClientError::RequestTimeout {
                    timeout: std::time::Duration::from_secs(15),
                },
                "The local Bcode service did not respond in time. Try again.",
            ),
            (
                ClientError::Server {
                    code: "session_repair_required".to_owned(),
                    message: "projection index tail mismatch at event 42".to_owned(),
                },
                "This session needs repair before its full history is available.",
            ),
            (
                ClientError::Server {
                    code: "permission_resolution_failed".to_owned(),
                    message: "internal permission provider detail".to_owned(),
                },
                "The action could not be completed. Try again.",
            ),
        ];

        for (error, expected) in cases {
            let message = client_error_message(&error);
            assert_eq!(message, expected);
            assert!(!message.contains("projection index"));
            assert!(!message.contains("provider detail"));
        }
    }

    #[test]
    fn access_token_authorization_requires_exact_query_value() {
        let state = HyperChadAppState::new(BcodeClient::default_endpoint(), "secret-token");
        let valid = RouteRequest::from_path(
            "/?token=secret-token",
            hyperchad::router::RequestInfo::default(),
        );
        let missing = RouteRequest::from_path("/", hyperchad::router::RequestInfo::default());
        let invalid = RouteRequest::from_path(
            "/?token=other-token",
            hyperchad::router::RequestInfo::default(),
        );

        assert!(state.authorizes(&valid));
        assert!(!state.authorizes(&missing));
        assert!(!state.authorizes(&invalid));
    }

    #[test]
    fn interaction_forms_cover_all_renderer_neutral_input_kinds() {
        use bcode_tool::{InteractionControlId, InteractionInput, InteractionNavigation};

        let form = |kind, control_id: Option<&str>, direction: Option<&str>| InteractionForm {
            session_id: SessionId::new().to_string(),
            interaction_id: "interaction-1".to_owned(),
            kind,
            control_id: control_id.map(str::to_owned),
            value: None,
            value_is_json: false,
            direction: direction.map(str::to_owned),
        };
        let cases = [
            (
                form(InteractionInputKind::Activate, Some("control"), None),
                InteractionInput::Activate {
                    control_id: InteractionControlId::new("control"),
                },
            ),
            (
                form(InteractionInputKind::Focus, Some("control"), None),
                InteractionInput::Focus {
                    control_id: InteractionControlId::new("control"),
                },
            ),
            (
                form(InteractionInputKind::Blur, Some("control"), None),
                InteractionInput::Blur {
                    control_id: InteractionControlId::new("control"),
                },
            ),
            (
                form(InteractionInputKind::Navigate, None, Some("next")),
                InteractionInput::Navigate {
                    direction: InteractionNavigation::Next,
                },
            ),
            (
                form(InteractionInputKind::Navigate, None, Some("previous")),
                InteractionInput::Navigate {
                    direction: InteractionNavigation::Previous,
                },
            ),
            (
                form(InteractionInputKind::Submit, None, None),
                InteractionInput::Submit,
            ),
            (
                form(InteractionInputKind::Cancel, None, None),
                InteractionInput::Cancel,
            ),
        ];

        for (form, expected) in cases {
            assert_eq!(interaction_input_from_form(&form), Ok(expected));
        }
    }

    #[test]
    fn interaction_change_form_preserves_plain_text_that_looks_like_json() {
        let form = InteractionForm {
            session_id: SessionId::new().to_string(),
            interaction_id: "interaction-1".to_owned(),
            kind: InteractionInputKind::Change,
            control_id: Some("answer".to_owned()),
            value: Some("true".to_owned()),
            value_is_json: false,
            direction: None,
        };

        assert_eq!(
            interaction_input_from_form(&form),
            Ok(bcode_tool::InteractionInput::Change {
                control_id: bcode_tool::InteractionControlId::new("answer"),
                value: bcode_tool::InteractionValue::String("true".to_owned()),
            })
        );
    }

    #[test]
    fn interaction_change_form_parses_explicit_json_values() {
        let form = InteractionForm {
            session_id: SessionId::new().to_string(),
            interaction_id: "interaction-1".to_owned(),
            kind: InteractionInputKind::Change,
            control_id: Some("toggle".to_owned()),
            value: Some("true".to_owned()),
            value_is_json: true,
            direction: None,
        };

        assert_eq!(
            interaction_input_from_form(&form),
            Ok(bcode_tool::InteractionInput::Change {
                control_id: bcode_tool::InteractionControlId::new("toggle"),
                value: bcode_tool::InteractionValue::Bool(true),
            })
        );
    }

    #[test]
    fn generic_unknown_interactions_support_submit_cancel_and_reject_controller_only_inputs() {
        let exchange = bcode_session_models::ToolExchangeRequest {
            invocation_id: "call-unknown".to_owned(),
            exchange_id: "exchange-unknown".to_owned(),
            producer_id: "unknown.plugin".to_owned(),
            schema: "unknown.request".to_owned(),
            schema_version: 7,
            payload: serde_json::json!({"original": true}),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let form = |kind, value: Option<&str>, value_is_json| InteractionForm {
            session_id: SessionId::new().to_string(),
            interaction_id: exchange.exchange_id.clone(),
            kind,
            control_id: None,
            value: value.map(str::to_owned),
            value_is_json,
            direction: None,
        };

        assert_eq!(
            generic_interaction_resolution(
                &exchange,
                &form(
                    InteractionInputKind::Submit,
                    Some("{\"accepted\":true}"),
                    true
                ),
            ),
            Ok(Some((
                bcode_session_models::ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"accepted": true}),
                },
                "interaction submitted",
            )))
        );
        assert_eq!(
            generic_interaction_resolution(
                &exchange,
                &form(InteractionInputKind::Submit, None, false),
            ),
            Ok(Some((
                bcode_session_models::ToolExchangeResolution::Responded {
                    payload: exchange.payload.clone(),
                },
                "interaction submitted",
            )))
        );
        assert_eq!(
            generic_interaction_resolution(
                &exchange,
                &form(InteractionInputKind::Cancel, None, false),
            ),
            Ok(Some((
                bcode_session_models::ToolExchangeResolution::Cancelled,
                "interaction cancelled",
            )))
        );
        assert_eq!(
            generic_interaction_resolution(
                &exchange,
                &form(InteractionInputKind::Focus, None, false),
            ),
            Ok(None)
        );
        assert!(
            generic_interaction_resolution(
                &exchange,
                &form(InteractionInputKind::Submit, Some("{"), true),
            )
            .is_err()
        );

        let adapter = generic_interaction_adapter(&exchange);
        assert_eq!(adapter.producer_id, exchange.producer_id);
        assert_eq!(adapter.exchange_schema, exchange.schema);
        assert_eq!(adapter.min_schema_version, 7);
        assert_eq!(adapter.max_schema_version, 7);
    }

    #[test]
    fn interaction_navigation_form_requires_valid_direction() {
        let form = InteractionForm {
            session_id: SessionId::new().to_string(),
            interaction_id: "interaction-1".to_owned(),
            kind: InteractionInputKind::Navigate,
            control_id: None,
            value: None,
            value_is_json: false,
            direction: Some("sideways".to_owned()),
        };

        assert_eq!(
            interaction_input_from_form(&form),
            Err("interaction navigation direction must be next or previous".to_owned())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_failure_route_renders_semantic_unavailable_state() {
        let socket_dir = tempfile::tempdir().expect("missing daemon socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.path().join("missing.sock"));
        let state = HyperChadAppState::new(BcodeClient::new(endpoint), "failure-token");
        let content = router_from_state(state)
            .navigate(hyperchad::router::RouteRequest::from_path(
                "/?token=failure-token",
                hyperchad::router::RequestInfo::default(),
            ))
            .await
            .expect("failure route")
            .expect("failure content");
        let rendered = format!("{content:?}");

        assert!(rendered.contains("The local Bcode service is unavailable."));
        assert!(rendered.contains("Session unavailable"));
        assert!(!rendered.contains("IPC transport"));
        assert!(!rendered.contains("missing.sock"));
    }

    #[cfg(feature = "renderer-html-actix")]
    #[test]
    fn representative_long_snapshot_render_measurement_stays_bounded() {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(SessionId::new());
        snapshot.connection_status =
            bcode_session_view_models::SessionConnectionViewStatus::Attached;
        snapshot.composer.can_submit = true;
        snapshot.transcript.items = (0..500)
            .map(|index| bcode_session_view_models::TranscriptViewItem {
                id: bcode_session_view_models::TranscriptViewItemId::new(format!(
                    "performance:{index}"
                )),
                revision: 1,
                sequence: Some(index + 1),
                timestamp_ms: Some(index + 1),
                streaming: index == 499,
                kind: bcode_session_view_models::TranscriptViewItemKind::AssistantMessage {
                    message: bcode_session_view_models::ChatMessageView::markdown(format!(
                        "## Streamed response {index}\n\n{}",
                        "representative bounded content ".repeat(20)
                    )),
                },
            })
            .collect();
        snapshot.transcript.revision = 1;
        snapshot.transcript.source_start_sequence = Some(1);
        snapshot.transcript.source_end_sequence = Some(500);

        let context = html_actix::HtmlActixPresentationContext::new(Arc::from("measure-token"));
        let started = std::time::Instant::now();
        let containers = bcode_hyperchad_ui::pages::home::home(&snapshot, &[], &context);
        let build_elapsed = started.elapsed();
        let root: hyperchad::renderer::transformer::Container = containers.into();
        let started = std::time::Instant::now();
        let html = hyperchad::renderer_html::html::container_to_html(
            &root,
            &hyperchad::renderer_html::DefaultHtmlTagRenderer::default(),
        )
        .expect("representative HTML");
        let html_elapsed = started.elapsed();

        eprintln!(
            "HyperChad representative scoped snapshot: build={build_elapsed:?} html={html_elapsed:?} bytes={}",
            html.len()
        );
        assert!(html.contains("Streamed response 499"));
        assert!(html.len() < 4 * 1024 * 1024);
        assert!(build_elapsed < std::time::Duration::from_secs(2));
        assert!(html_elapsed < std::time::Duration::from_secs(2));
    }

    #[cfg(feature = "renderer-html-actix")]
    #[tokio::test]
    async fn html_actix_app_build_smoke_test() {
        tokio::task::yield_now().await;
        let state = HyperChadAppState::new(BcodeClient::default_endpoint(), "scope-token");
        let builder = init_with_snapshot(SessionViewSnapshot::empty(), Vec::new());
        let app = build_app(builder).expect("HTML/Actix application should build");
        let renderer = app.renderer.clone();
        configure_live_updates(&renderer, &state);
        assert!(
            state
                .renderer_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        );
        drop(app);
    }

    #[test]
    fn hyperchad_router_smoke_test() {
        let app_router = router(SessionViewSnapshot::empty(), Vec::new());
        drop(app_router);
    }
}
