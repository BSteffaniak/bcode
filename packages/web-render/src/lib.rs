#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `HyperChad` web renderer host for Bcode sessions.

use std::sync::{Arc, LazyLock};

use bcode_client::{BcodeClient, ClientError};
use bcode_session_models::SessionSummary;
use bcode_session_view::SessionView;
use bcode_session_view_models::SessionViewSnapshot;
use hyperchad::app::{App, AppBuilder, renderer::DefaultRenderer};
use hyperchad::color::Color;
use hyperchad::router::Router;

static BACKGROUND_COLOR: LazyLock<Color> = LazyLock::new(|| Color::from_hex("#0d1117"));

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
        let snapshot = SessionView::new().into_snapshot();
        Ok((snapshot, sessions))
    }
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
    let (snapshot, sessions) = state.initial_state().await?;
    Ok(init_with_snapshot(snapshot, sessions))
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
