#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `HyperChad` web renderer host for Bcode sessions.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::str::FromStr as _;
use std::sync::{Arc, LazyLock, Mutex};

use bcode_client::{AttachedSessionHistory, BcodeClient, ClientError, SessionWatchEvent};
use bcode_session_models::{SessionId, SessionSummary};
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

/// Default viewport meta tag for responsive web rendering.
pub static VIEWPORT: LazyLock<String> =
    LazyLock::new(|| "width=device-width, initial-scale=1".to_string());

/// Web renderer runtime state shared by `HyperChad` route handlers.
#[derive(Debug, Clone)]
pub struct WebRenderState {
    client: BcodeClient,
    access_token: Arc<str>,
    watched_sessions: Arc<Mutex<BTreeSet<SessionId>>>,
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
        let watched_sessions = Arc::clone(&self.watched_sessions);
        tokio::spawn(async move {
            if let Err(error) =
                watch_session_updates(client, access_token, session_id, Arc::clone(&renderer_tx))
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
        let mut connection = self.client.connect("bcode-web-render").await?;
        let attached = connection
            .attach_session_recent_with_input_history(session_id, INITIAL_HISTORY_EVENT_LIMIT)
            .await?;
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
        view.upsert_permission(bcode_session_view_models::PermissionView {
            permission_id: permission.permission_id,
            tool_call_id: permission.tool_call_id,
            title: Some(format!("Permission requested: {}", permission.tool_name)),
            detail: permission.policy_reason,
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
        view.upsert_interaction(InteractionViewSummary {
            interaction_id,
            kind: request
                .interaction_kind
                .unwrap_or_else(|| request.surface_kind.clone()),
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

fn snapshot_from_view(
    view: &SessionView,
    attached: &AttachedSessionHistory,
) -> SessionViewSnapshot {
    let mut snapshot = view.snapshot().clone();
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

async fn watch_session_updates(
    client: BcodeClient,
    access_token: Arc<str>,
    session_id: SessionId,
    renderer_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<ScopedSnapshotUpdate>>>>,
) -> Result<(), ClientError> {
    let mut watcher = client
        .watch_session(session_id, INITIAL_HISTORY_EVENT_LIMIT)
        .await?;
    let mut attached = watcher
        .take_initial()
        .ok_or(ClientError::UnexpectedResponse)?;
    let mut view = view_from_attached_history(&attached);
    hydrate_pending_interactions(&client, session_id, &mut view).await?;

    loop {
        match watcher.next_event().await? {
            SessionWatchEvent::Durable(event) => {
                if view
                    .snapshot()
                    .latest_sequence
                    .is_none_or(|sequence| event.sequence > sequence)
                {
                    view.apply_event(&event);
                }
            }
            SessionWatchEvent::Live(event) => view.apply_live_event(&event),
            SessionWatchEvent::ResyncRequired => {
                let mut connection = client.connect("bcode-web-render-resync").await?;
                attached = connection
                    .attach_session_recent_with_input_history(
                        session_id,
                        INITIAL_HISTORY_EVENT_LIMIT,
                    )
                    .await?;
                view = view_from_attached_history(&attached);
            }
        }

        hydrate_session_model_status(&client, session_id, &mut view).await?;
        hydrate_pending_permissions(&client, session_id, &mut view).await?;
        hydrate_pending_interactions(&client, session_id, &mut view).await?;
        let sessions = client.list_sessions().await?;
        if let Some(summary) = sessions.iter().find(|summary| summary.id == session_id) {
            attached.session.clone_from(summary);
        }
        let update = ScopedSnapshotUpdate {
            scope: format!("{access_token}:{session_id}"),
            snapshot: snapshot_from_view(&view, &attached),
            sessions,
        };
        let sender = renderer_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(sender) = sender
            && sender.send(update).await.is_err()
        {
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
