#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `HyperChad` web renderer host for Bcode sessions.

use std::str::FromStr as _;
use std::sync::{Arc, LazyLock};

use bcode_client::{AttachedSessionHistory, BcodeClient, ClientError};
use bcode_session_models::{SessionId, SessionSummary};
use bcode_session_view::{SessionView, execute_session_view_action};
use bcode_session_view_models::{
    ComposerDraftViewScope, PromptPlacementView, SessionViewAction, SessionViewSnapshot,
};
use hyperchad::app::{App, AppBuilder, renderer::DefaultRenderer};
use hyperchad::color::Color;
use hyperchad::router::{RoutePath, RouteRequest, Router};
use serde::Deserialize;

static BACKGROUND_COLOR: LazyLock<Color> = LazyLock::new(|| Color::from_hex("#0d1117"));

/// Number of recent history events projected into the first web-render snapshot.
pub const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;

/// Default viewport meta tag for responsive web rendering.
pub static VIEWPORT: LazyLock<String> =
    LazyLock::new(|| "width=device-width, initial-scale=1".to_string());

/// Web renderer runtime state shared by `HyperChad` route handlers.
#[derive(Debug, Clone)]
pub struct WebRenderState {
    client: BcodeClient,
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
    session_id: String,
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

impl WebRenderState {
    /// Create web renderer state from a daemon client.
    #[must_use]
    pub const fn new(client: BcodeClient) -> Self {
        Self { client }
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
        Ok(snapshot_from_attached_history(attached))
    }
}

/// Build a renderer-neutral snapshot from bounded daemon attach history.
#[must_use]
pub fn snapshot_from_attached_history(attached: AttachedSessionHistory) -> SessionViewSnapshot {
    let mut view = SessionView::new();
    view.apply_history(&attached.history);
    let mut snapshot = view.into_snapshot();
    snapshot.session_id = Some(attached.session.id);
    snapshot.title = attached.session.title().map(ToOwned::to_owned);
    snapshot.working_directory = Some(attached.session.working_directory.clone());
    snapshot.composer.draft = attached.draft.unwrap_or_default();
    snapshot.composer.can_submit = true;
    snapshot.session_summary = Some(attached.session);
    snapshot
}

/// Build a state-backed application router.
#[must_use]
pub fn router_from_state(state: WebRenderState) -> Router {
    let root_state = state.clone();
    let session_state = state.clone();
    let submit_state = state.clone();
    let cancel_state = state.clone();
    let draft_state = state.clone();
    let permission_state = state;
    Router::new()
        .with_route("/", move |_| {
            let state = root_state.clone();
            async move { state.render_initial().await }
        })
        .with_route(
            RoutePath::LiteralPrefix("/session/".to_string()),
            move |request| {
                let state = session_state.clone();
                async move { state.render_session_request(&request).await }
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
        .with_route("/actions/update-draft", move |request| {
            let state = draft_state.clone();
            async move { state.handle_update_draft(request).await }
        })
        .with_route("/actions/permission", move |request| {
            let state = permission_state.clone();
            async move { state.handle_permission(request).await }
        })
}

impl WebRenderState {
    async fn render_initial(&self) -> hyperchad::template::Containers {
        match self.initial_state().await {
            Ok((snapshot, sessions)) => {
                bcode_web_render_ui::pages::home::home(&snapshot, &sessions)
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
        let form = match request.parse_form::<UpdateDraftForm>() {
            Ok(form) => form,
            Err(error) => return error_page(&error.to_string()),
        };
        let Some(session_id) = parse_session_id(&form.session_id) else {
            return error_page("invalid session id");
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
                    bcode_web_render_ui::pages::home::home(&snapshot, &sessions)
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
                bcode_web_render_ui::pages::home::home(&snapshot, sessions)
            }
            Err(error) => error_page(&error.to_string()),
        }
    }
}

fn session_id_from_path(path: &str) -> Option<SessionId> {
    path.strip_prefix("/session/").and_then(parse_session_id)
}

fn parse_session_id(value: &str) -> Option<SessionId> {
    SessionId::from_str(value).ok()
}

fn error_page(message: &str) -> hyperchad::template::Containers {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.title = Some("Web renderer error".to_owned());
    snapshot.composer.disabled_reason = Some(message.to_owned());
    bcode_web_render_ui::pages::home::home(&snapshot, &[])
}

/// Build the application router for the current snapshot and session list.
#[must_use]
pub fn router(snapshot: SessionViewSnapshot, sessions: Vec<SessionSummary>) -> Router {
    let snapshot = Arc::new(snapshot);
    let sessions = Arc::new(sessions);
    Router::new().with_static_route(&["/", "/session"], move |_| {
        let snapshot = Arc::clone(&snapshot);
        let sessions = Arc::clone(&sessions);
        async move { bcode_web_render_ui::pages::home::home(&snapshot, &sessions) }
    })
}

/// Initialize the web renderer application builder with a static initial snapshot.
#[must_use]
pub fn init_with_snapshot(
    snapshot: SessionViewSnapshot,
    sessions: Vec<SessionSummary>,
) -> AppBuilder {
    AppBuilder::new()
        .with_router(router(snapshot, sessions))
        .with_background(*BACKGROUND_COLOR)
        .with_title("bcode web".to_string())
        .with_description("HyperChad web renderer for Bcode sessions".to_string())
        .with_size(1200.0, 800.0)
}

/// Initialize the web renderer application builder from daemon state.
///
/// # Errors
///
/// Returns an error when initial daemon state cannot be loaded.
pub async fn init(state: &WebRenderState) -> Result<AppBuilder, ClientError> {
    state.client().ensure_daemon_available().await?;
    Ok(AppBuilder::new()
        .with_router(router_from_state(state.clone()))
        .with_background(*BACKGROUND_COLOR)
        .with_title("bcode web".to_string())
        .with_description("HyperChad web renderer for Bcode sessions".to_string())
        .with_size(1200.0, 800.0))
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
    fn web_renderer_router_smoke_test() {
        let app_router = router(SessionViewSnapshot::empty(), Vec::new());
        drop(app_router);
    }
}
