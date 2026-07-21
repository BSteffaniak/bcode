#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `HyperChad` web renderer host for Bcode sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::str::FromStr as _;
use std::sync::{Arc, LazyLock, Mutex};

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
    ComposerDraftViewScope, InteractionViewSummary, PromptPlacementView, SessionViewAction,
    SessionViewSnapshot,
};
use hyperchad::app::{App, AppBuilder, renderer::DefaultRenderer};
use hyperchad::color::Color;
use hyperchad::renderer::Renderer as _;
use hyperchad::router::{RoutePath, RouteRequest, Router};
use serde::Deserialize;

static BACKGROUND_COLOR: LazyLock<Color> = LazyLock::new(|| Color::from_hex("#0d1117"));

/// Default loopback address for the local web renderer.
pub const DEFAULT_BIND_ADDRESS: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

/// Validate a requested web renderer bind address.
///
/// # Errors
///
/// Returns an error when a non-loopback address is requested without explicit opt-in.
pub const fn validate_bind_address(
    address: IpAddr,
    allow_non_loopback: bool,
) -> Result<IpAddr, &'static str> {
    if address.is_loopback() || allow_non_loopback {
        Ok(address)
    } else {
        Err("non-loopback web binds require explicit opt-in")
    }
}

/// Number of recent history events projected into the first web-render snapshot.
pub const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;

/// Delay between failed daemon watcher reconnection attempts.
const WATCH_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// Default viewport meta tag for responsive web rendering.
pub static VIEWPORT: LazyLock<String> =
    LazyLock::new(|| "width=device-width, initial-scale=1".to_string());

/// Web renderer runtime state shared by `HyperChad` route handlers.
#[derive(Debug, Clone)]
pub struct WebRenderState {
    client: BcodeClient,
    access_token: Arc<str>,
    watched_sessions: Arc<Mutex<BTreeSet<SessionId>>>,
    history_windows: Arc<Mutex<BTreeMap<SessionId, ProjectionWindowRequest>>>,
    renderer_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<ScopedSnapshotUpdate>>>>,
}

#[derive(Debug)]
struct ScopedSnapshotUpdate {
    scope: String,
    snapshot: SessionViewSnapshot,
    sessions: Vec<SessionSummary>,
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

impl WebRenderState {
    /// Create web renderer state from a daemon client and per-launch access token.
    #[must_use]
    pub fn new(client: BcodeClient, access_token: impl Into<Arc<str>>) -> Self {
        Self {
            client,
            access_token: access_token.into(),
            watched_sessions: Arc::new(Mutex::new(BTreeSet::new())),
            history_windows: Arc::new(Mutex::new(BTreeMap::new())),
            renderer_tx: Arc::new(Mutex::new(None)),
        }
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
        let access_token = Arc::clone(&self.access_token);
        let renderer_tx = Arc::clone(&self.renderer_tx);
        let history_windows = Arc::clone(&self.history_windows);
        let watched_sessions = Arc::clone(&self.watched_sessions);
        tokio::spawn(async move {
            if let Err(error) = Box::pin(watch_session_updates(
                client,
                access_token,
                session_id,
                Arc::clone(&renderer_tx),
                history_windows,
            ))
            .await
            {
                tracing::error!("web session watcher failed for {session_id}: {error}");
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

    /// Return the daemon client used by this web renderer.
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
        let sessions = self.client.list_sessions().await?;
        let snapshot = self
            .latest_session_snapshot(&sessions)
            .await?
            .unwrap_or_else(SessionViewSnapshot::empty);
        Ok((snapshot, sessions))
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
            .unwrap_or_else(web_projection_window_request);
        let mut connection = self.client.connect("bcode-web-render").await?;
        let attached =
            attach_web_projection_window_with_request(&mut connection, session_id, request).await?;
        session_view_from_attached_history(&self.client, attached).await
    }
}

async fn session_view_from_attached_history(
    client: &BcodeClient,
    attached: AttachedSessionHistory,
) -> Result<SessionViewSnapshot, ClientError> {
    let mut view = view_from_attached_history(&attached);
    hydrate_session_model_status(client, attached.session.id, &mut view).await?;
    hydrate_pending_permissions(client, attached.session.id, &mut view).await?;
    hydrate_pending_interactions(client, attached.session.id, &mut view).await?;
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

async fn hydrate_pending_interactions(
    client: &BcodeClient,
    session_id: SessionId,
    view: &mut SessionView,
) -> Result<(), ClientError> {
    let requests = client.list_interactive_tool_requests().await?;
    for request in requests
        .into_iter()
        .filter(|request| request.session_id == session_id)
    {
        let interaction_id = request.interaction_id.clone();
        let snapshot = client
            .interaction_snapshot(interaction_id.clone())
            .await?
            .map_or(request.request, |snapshot| snapshot.snapshot);
        let surface_kind = request.surface_kind.clone();
        view.upsert_interaction(InteractionViewSummary {
            interaction_id,
            kind: request
                .interaction_kind
                .unwrap_or_else(|| surface_kind.clone()),
            surface_kind,
            tool_call_id: Some(request.tool_call_id),
            title: Some(request.tool_name),
            required: request.required,
            snapshot: Some(snapshot),
            resolved: false,
            resolution: None,
            render_target: request.render_target,
            turn_behavior: request.turn_behavior,
        });
    }
    Ok(())
}

const fn web_projection_window_request() -> ProjectionWindowRequest {
    web_projection_window_request_for_anchor(
        ProjectionWindowAnchor::Latest,
        ProjectionWindowDirection::Backward,
    )
}

const fn web_projection_window_request_for_anchor(
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

async fn attach_web_projection_window_with_request(
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

fn snapshot_from_view(
    view: &SessionView,
    attached: &AttachedSessionHistory,
) -> SessionViewSnapshot {
    let mut snapshot = view.snapshot().clone();
    apply_projection_window_metadata(&mut snapshot, attached.projection_window.as_ref());
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

/// Build a state-backed application router.
#[must_use]
pub fn router_from_state(state: WebRenderState) -> Router {
    let root_state = state.clone();
    let session_state = state.clone();
    let submit_state = state.clone();
    let cancel_state = state.clone();
    let draft_state = state.clone();
    let permission_state = state.clone();
    let history_state = state.clone();
    let interaction_state = state;
    Router::new()
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
        .with_route("/actions/history-window", move |request| {
            let state = history_state.clone();
            async move { state.handle_history_window(request).await }
        })
        .with_route("/actions/interaction", move |request| {
            let state = interaction_state.clone();
            async move { state.handle_interaction(request).await }
        })
}

impl WebRenderState {
    async fn render_initial(&self) -> hyperchad::template::Containers {
        match self.initial_state().await {
            Ok((snapshot, sessions)) => {
                if let Some(session_id) = snapshot.session_id {
                    self.ensure_session_watcher(session_id);
                }
                bcode_web_render_ui::pages::home::home(&snapshot, &sessions, self.access_token())
            }
            Err(error) => error_page(&error.to_string()),
        }
    }

    async fn render_session_request(
        &self,
        request: &RouteRequest,
    ) -> hyperchad::template::Containers {
        let sessions = match self.client.list_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => return error_page(&error.to_string()),
        };
        let Some(session_id) = session_id_from_path(&request.path) else {
            return error_page("invalid session path");
        };
        self.render_session(session_id, &sessions).await
    }

    async fn handle_submit_message(
        &self,
        request: RouteRequest,
    ) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let form = match request.parse_form::<PromptForm>() {
            Ok(form) => form,
            Err(error) => return error_page(&error.to_string()),
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
                Err(error) => return error_page(&error.to_string()),
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
                ..
            }) => {
                self.render_session_or_initial(Some(session_id), "message accepted")
                    .await
            }
            Ok(_) => {
                self.render_session_or_initial(session_id, "message accepted")
                    .await
            }
            Err(error) => {
                self.render_session_or_initial(session_id, &error.to_string())
                    .await
            }
        }
    }

    async fn handle_cancel_turn(&self, request: RouteRequest) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let form = match request.parse_form::<CancelTurnForm>() {
            Ok(form) => form,
            Err(error) => return error_page(&error.to_string()),
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
                self.render_session_or_initial(Some(session_id), &error.to_string())
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
        let form = match request.parse_form::<UpdateDraftForm>() {
            Ok(form) => form,
            Err(error) => return error_page(&error.to_string()),
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
                self.render_session_or_initial(Some(session_id), &error.to_string())
                    .await
            }
        }
    }

    async fn handle_permission(&self, request: RouteRequest) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let form = match request.parse_form::<PermissionForm>() {
            Ok(form) => form,
            Err(error) => return error_page(&error.to_string()),
        };
        let Some(session_id) = parse_session_id(&form.session_id) else {
            return error_page("invalid session id");
        };
        let action = SessionViewAction::ResolvePermission {
            permission_id: form.permission_id,
            approved: form.approved,
            remember: form.remember,
        };
        match execute_session_view_action(&self.client, action).await {
            Ok(_) => {
                self.render_session_or_initial(Some(session_id), "permission resolved")
                    .await
            }
            Err(error) => {
                self.render_session_or_initial(Some(session_id), &error.to_string())
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
        let form = match request.parse_form::<HistoryWindowForm>() {
            Ok(form) => form,
            Err(error) => return error_page(&error.to_string()),
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
        let request = web_projection_window_request_for_anchor(anchor, direction);
        let mut connection = match self.client.connect("bcode-web-render-history").await {
            Ok(connection) => connection,
            Err(error) => return error_page(&error.to_string()),
        };
        let attached = match attach_web_projection_window_with_request(
            &mut connection,
            session_id,
            request.clone(),
        )
        .await
        {
            Ok(attached) => attached,
            Err(error) => return error_page(&error.to_string()),
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
        let snapshot = match session_view_from_attached_history(&self.client, attached).await {
            Ok(snapshot) => snapshot,
            Err(error) => return error_page(&error.to_string()),
        };
        let sessions = match self.client.list_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => return error_page(&error.to_string()),
        };
        bcode_web_render_ui::pages::home::home(&snapshot, &sessions, self.access_token())
    }

    async fn handle_interaction(&self, request: RouteRequest) -> hyperchad::template::Containers {
        if !self.authorizes(&request) {
            return unauthorized_page();
        }
        let form = match request.parse_form::<InteractionForm>() {
            Ok(form) => form,
            Err(error) => return error_page(&error.to_string()),
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
        let action = SessionViewAction::SubmitInteractionInput {
            interaction_id: form.interaction_id,
            input,
        };
        match execute_session_view_action(&self.client, action).await {
            Ok(_) => {
                self.render_session_or_initial(Some(session_id), "interaction input accepted")
                    .await
            }
            Err(error) => {
                self.render_session_or_initial(Some(session_id), &error.to_string())
                    .await
            }
        }
    }

    async fn render_session_or_initial(
        &self,
        session_id: Option<SessionId>,
        status: &str,
    ) -> hyperchad::template::Containers {
        let sessions = match self.client.list_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => return error_page(&error.to_string()),
        };
        match session_id {
            Some(session_id) => {
                self.render_session_with_status(session_id, &sessions, status)
                    .await
            }
            None => match self.initial_state().await {
                Ok((mut snapshot, sessions)) => {
                    snapshot.composer.disabled_reason = Some(status.to_owned());
                    bcode_web_render_ui::pages::home::home(
                        &snapshot,
                        &sessions,
                        self.access_token(),
                    )
                }
                Err(error) => error_page(&error.to_string()),
            },
        }
    }

    async fn render_session(
        &self,
        session_id: SessionId,
        sessions: &[SessionSummary],
    ) -> hyperchad::template::Containers {
        self.ensure_session_watcher(session_id);
        self.render_session_with_status(session_id, sessions, "connected")
            .await
    }

    async fn render_session_with_status(
        &self,
        session_id: SessionId,
        sessions: &[SessionSummary],
        status: &str,
    ) -> hyperchad::template::Containers {
        match self.session_snapshot(session_id).await {
            Ok(mut snapshot) => {
                snapshot.composer.disabled_reason = Some(status.to_owned());
                bcode_web_render_ui::pages::home::home(&snapshot, sessions, self.access_token())
            }
            Err(error) => error_page(&error.to_string()),
        }
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

fn session_id_from_path(path: &str) -> Option<SessionId> {
    path.strip_prefix("/session/").and_then(parse_session_id)
}

fn parse_session_id(value: &str) -> Option<SessionId> {
    SessionId::from_str(value).ok()
}

fn unauthorized_page() -> hyperchad::template::Containers {
    error_page("missing or invalid web renderer access token")
}

fn error_page(message: &str) -> hyperchad::template::Containers {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.title = Some("Web renderer error".to_owned());
    snapshot.composer.disabled_reason = Some(message.to_owned());
    bcode_web_render_ui::pages::home::home(&snapshot, &[], "")
}

/// Build the application router for the current snapshot and session list.
#[must_use]
pub fn router(snapshot: SessionViewSnapshot, sessions: Vec<SessionSummary>) -> Router {
    let snapshot = Arc::new(snapshot);
    let sessions = Arc::new(sessions);
    Router::new().with_static_route(&["/", "/session"], move |_| {
        let snapshot = Arc::clone(&snapshot);
        let sessions = Arc::clone(&sessions);
        async move { bcode_web_render_ui::pages::home::home(&snapshot, &sessions, "") }
    })
}

fn with_browser_runtime(builder: AppBuilder) -> AppBuilder {
    builder.with_static_asset_route(hyperchad::renderer::assets::StaticAssetRoute {
        route: format!(
            "js/{}",
            hyperchad::renderer_vanilla_js::SCRIPT_NAME_HASHED.as_str()
        ),
        target: hyperchad::renderer::assets::AssetPathTarget::FileContents(
            hyperchad::renderer_vanilla_js::SCRIPT.as_bytes().into(),
        ),
        not_found_behavior: None,
    })
}

/// Initialize the web renderer application builder with a static initial snapshot.
#[must_use]
pub fn init_with_snapshot(
    snapshot: SessionViewSnapshot,
    sessions: Vec<SessionSummary>,
) -> AppBuilder {
    with_browser_runtime(
        AppBuilder::new()
            .with_actix_bind_address(DEFAULT_BIND_ADDRESS.to_string())
            .with_router(router(snapshot, sessions))
            .with_background(*BACKGROUND_COLOR)
            .with_title("bcode web".to_string())
            .with_description("HyperChad web renderer for Bcode sessions".to_string())
            .with_viewport(VIEWPORT.clone())
            .with_size(1200.0, 800.0),
    )
}

/// Initialize the web renderer application builder from daemon state.
///
/// # Errors
///
/// Returns an error when initial daemon state cannot be loaded.
pub async fn init(state: &WebRenderState) -> Result<AppBuilder, ClientError> {
    state.client().ensure_daemon_available().await?;
    Ok(with_browser_runtime(
        AppBuilder::new()
            .with_actix_bind_address(DEFAULT_BIND_ADDRESS.to_string())
            .with_router(router_from_state(state.clone()))
            .with_background(*BACKGROUND_COLOR)
            .with_title("bcode web".to_string())
            .with_description("HyperChad web renderer for Bcode sessions".to_string())
            .with_viewport(VIEWPORT.clone())
            .with_size(1200.0, 800.0),
    ))
}

async fn attach_watched_session(
    client: &BcodeClient,
    session_id: SessionId,
) -> Result<(SessionWatcher, AttachedSessionHistory, SessionView), ClientError> {
    let mut watcher = client
        .watch_session_projection_window(session_id, web_projection_window_request())
        .await?;
    let attached = watcher
        .take_initial()
        .ok_or(ClientError::UnexpectedResponse)?;
    let mut view = view_from_attached_history(&attached);
    hydrate_session_model_status(client, session_id, &mut view).await?;
    hydrate_pending_permissions(client, session_id, &mut view).await?;
    hydrate_pending_interactions(client, session_id, &mut view).await?;
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
) -> Result<SessionViewSnapshot, ClientError> {
    let history_request = history_windows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&session_id)
        .cloned();
    if let Some(request) = history_request {
        let mut connection = client.connect("bcode-web-render-history-refresh").await?;
        let historical =
            attach_web_projection_window_with_request(&mut connection, session_id, request).await?;
        session_view_from_attached_history(client, historical).await
    } else {
        Ok(snapshot_from_view(view, attached))
    }
}

async fn watch_session_updates(
    client: BcodeClient,
    access_token: Arc<str>,
    session_id: SessionId,
    renderer_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<ScopedSnapshotUpdate>>>>,
    history_windows: Arc<Mutex<BTreeMap<SessionId, ProjectionWindowRequest>>>,
) -> Result<(), ClientError> {
    let (mut watcher, mut attached, mut view) = loop {
        match attach_watched_session(&client, session_id).await {
            Ok(state) => break state,
            Err(error) => {
                if browser_update_sender(&renderer_tx).is_none() {
                    return Ok(());
                }
                tracing::warn!("web session watcher attach failed for {session_id}: {error}");
                tokio::time::sleep(WATCH_RECONNECT_DELAY).await;
            }
        }
    };

    loop {
        let event = match watcher.next_event().await {
            Ok(event) => event,
            Err(error) => {
                if browser_update_sender(&renderer_tx).is_none() {
                    return Ok(());
                }
                tracing::warn!("web session watcher disconnected for {session_id}: {error}");
                tokio::time::sleep(WATCH_RECONNECT_DELAY).await;
                match attach_watched_session(&client, session_id).await {
                    Ok((new_watcher, new_attached, new_view)) => {
                        watcher = new_watcher;
                        attached = new_attached;
                        view = new_view;
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(
                            "web session watcher reconnect failed for {session_id}: {error}"
                        );
                        continue;
                    }
                }
            }
        };

        let mut resync = false;
        match event {
            SessionWatchEvent::Durable(event) => {
                match durable_event_disposition(view.snapshot().latest_sequence, event.sequence) {
                    DurableEventDisposition::Apply => view.apply_event(&event),
                    DurableEventDisposition::IgnoreDuplicate => {}
                    DurableEventDisposition::ResyncGap => {
                        tracing::warn!(
                            "web session watcher detected event gap for {session_id}: latest={:?}, received={}",
                            view.snapshot().latest_sequence,
                            event.sequence
                        );
                        resync = true;
                    }
                }
            }
            SessionWatchEvent::Live(event) => view.apply_live_event(&event),
            SessionWatchEvent::ResyncRequired => resync = true,
        }

        if resync {
            let state = loop {
                match attach_watched_session(&client, session_id).await {
                    Ok(state) => break state,
                    Err(error) => {
                        if browser_update_sender(&renderer_tx).is_none() {
                            return Ok(());
                        }
                        tracing::warn!(
                            "web session watcher resync failed for {session_id}: {error}"
                        );
                        tokio::time::sleep(WATCH_RECONNECT_DELAY).await;
                    }
                }
            };
            (watcher, attached, view) = state;
        } else {
            hydrate_session_model_status(&client, session_id, &mut view).await?;
            hydrate_pending_permissions(&client, session_id, &mut view).await?;
            hydrate_pending_interactions(&client, session_id, &mut view).await?;
        }

        let sessions = client.list_sessions().await?;
        if let Some(summary) = sessions.iter().find(|summary| summary.id == session_id) {
            attached.session.clone_from(summary);
        }
        let snapshot =
            watched_session_snapshot(&client, session_id, &attached, &view, &history_windows)
                .await?;
        let update = ScopedSnapshotUpdate {
            scope: format!("{access_token}:{session_id}"),
            snapshot,
            sessions,
        };
        let Some(sender) = browser_update_sender(&renderer_tx) else {
            return Ok(());
        };
        if sender.send(update).await.is_err() {
            return Ok(());
        }
    }
}

/// Configure scoped live snapshot rendering on a built application.
pub fn configure_live_updates(app: &mut App<DefaultRenderer>, state: &WebRenderState) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ScopedSnapshotUpdate>(1);
    *state
        .renderer_tx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    let renderer = app.renderer.clone();
    let access_token = Arc::clone(&state.access_token);
    tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            let containers = bcode_web_render_ui::pages::home::home(
                &update.snapshot,
                &update.sessions,
                &access_token,
            );
            if let Err(error) = renderer
                .render_scoped(update.scope, containers.into())
                .await
            {
                tracing::error!("failed to render scoped web snapshot: {error}");
            }
        }
    });
}

/// Build the application from the provided builder.
///
/// # Errors
///
/// Returns an error if the application fails to build.
pub fn build_app(builder: AppBuilder) -> Result<App<DefaultRenderer>, hyperchad::app::Error> {
    Ok(builder.build_default()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_projection_closes_resolved_permission_but_preserves_transcript_record() {
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
    fn web_renders_generic_active_invocation_without_tool_specific_branch() {
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
            bcode_web_render_ui::pages::home::home(view.snapshot(), &[], "token")
        );
        assert!(rendered.contains("active invocations"));
        assert!(rendered.contains("opaque-call"));
        assert!(rendered.contains("waiting generically"));
    }

    #[test]
    fn web_projection_keeps_active_sibling_and_does_not_revive_terminal_work() {
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
            bcode_web_render_ui::pages::home::home(snapshot, &[], "token")
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
        let older = web_projection_window_request_for_anchor(
            ProjectionWindowAnchor::BeforeSequence(10),
            ProjectionWindowDirection::Backward,
        );
        assert_eq!(older.anchor, ProjectionWindowAnchor::BeforeSequence(10));
        assert_eq!(older.direction, ProjectionWindowDirection::Backward);

        let newer = web_projection_window_request_for_anchor(
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

    #[test]
    fn web_renderer_init_smoke_test() {
        let builder = init_with_snapshot(SessionViewSnapshot::empty(), Vec::new());
        drop(builder);
    }

    #[test]
    fn bind_address_policy_requires_non_loopback_opt_in() {
        let loopback = "127.0.0.1".parse().expect("loopback should parse");
        let external = "0.0.0.0".parse().expect("external address should parse");

        assert_eq!(validate_bind_address(loopback, false), Ok(loopback));
        assert!(validate_bind_address(external, false).is_err());
        assert_eq!(validate_bind_address(external, true), Ok(external));
    }

    #[test]
    fn access_token_authorization_requires_exact_query_value() {
        let state = WebRenderState::new(BcodeClient::default_endpoint(), "secret-token");
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

    #[tokio::test]
    async fn web_renderer_app_build_smoke_test() {
        tokio::task::yield_now().await;
        let builder = init_with_snapshot(SessionViewSnapshot::empty(), Vec::new());
        assert!(build_app(builder).is_ok());
    }

    #[test]
    fn web_renderer_router_smoke_test() {
        let app_router = router(SessionViewSnapshot::empty(), Vec::new());
        drop(app_router);
    }
}
