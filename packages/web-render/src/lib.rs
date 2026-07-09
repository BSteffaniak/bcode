#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `HyperChad` web renderer host for Bcode sessions.

use std::str::FromStr as _;
use std::sync::{Arc, LazyLock};

use bcode_client::{AttachedSessionHistory, BcodeClient, ClientError};
use bcode_session_models::SessionSummary;
use bcode_session_view::SessionView;
use bcode_session_view_models::SessionViewSnapshot;
use hyperchad::app::{App, AppBuilder, renderer::DefaultRenderer};
use hyperchad::color::Color;
use hyperchad::router::{RoutePath, RouteRequest, Router};

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
    Router::new()
        .with_route("/", move |_| {
            let state = root_state.clone();
            async move { state.render_initial().await }
        })
        .with_route(
            RoutePath::LiteralPrefix("/session/".to_string()),
            move |request| {
                let state = state.clone();
                async move { state.render_session_request(&request).await }
            },
        )
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
        match self.session_snapshot(session_id).await {
            Ok(snapshot) => bcode_web_render_ui::pages::home::home(&snapshot, &sessions),
            Err(error) => error_page(&error.to_string()),
        }
    }
}

fn session_id_from_path(path: &str) -> Option<bcode_session_models::SessionId> {
    path.strip_prefix("/session/")
        .and_then(|value| bcode_session_models::SessionId::from_str(value).ok())
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
