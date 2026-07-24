//! HTML/Actix backend construction for the Bcode `HyperChad` application.

use std::net::IpAddr;
use std::sync::LazyLock;

use bcode_client::ClientError;
use bcode_session_models::SessionSummary;
use bcode_session_view_models::SessionViewSnapshot;
use hyperchad::app::{App, AppBuilder, renderer::DefaultRenderer};
use hyperchad::color::Color;

use crate::{HyperChadAppState, router, router_from_state};

static BACKGROUND_COLOR: LazyLock<Color> = LazyLock::new(|| Color::from_hex("#0d1117"));

/// Default loopback address for the local HTML/Actix renderer.
pub const DEFAULT_BIND_ADDRESS: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

/// Default viewport meta tag for responsive HTML rendering.
pub static VIEWPORT: LazyLock<String> =
    LazyLock::new(|| "width=device-width, initial-scale=1".to_string());

/// Validate a requested HTML/Actix renderer bind address.
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

/// Initialize the HTML/Actix application builder with a static initial snapshot.
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
            .with_description("HyperChad application for Bcode sessions".to_string())
            .with_viewport(VIEWPORT.clone())
            .with_size(1200.0, 800.0),
    )
}

/// Initialize the HTML/Actix application builder from daemon state.
///
/// # Errors
///
/// Returns an error when initial daemon state cannot be loaded.
pub async fn init(state: &HyperChadAppState) -> Result<AppBuilder, ClientError> {
    state.client().ensure_daemon_available().await?;
    Ok(with_browser_runtime(
        AppBuilder::new()
            .with_actix_bind_address(DEFAULT_BIND_ADDRESS.to_string())
            .with_router(router_from_state(state.clone()))
            .with_background(*BACKGROUND_COLOR)
            .with_title("bcode web".to_string())
            .with_description("HyperChad application for Bcode sessions".to_string())
            .with_viewport(VIEWPORT.clone())
            .with_size(1200.0, 800.0),
    ))
}

/// Build the selected HTML/Actix application from the provided builder.
///
/// # Errors
///
/// Returns an error if the application fails to build.
pub fn build_app(builder: AppBuilder) -> Result<App<DefaultRenderer>, hyperchad::app::Error> {
    use hyperchad::renderer::Renderer as _;

    let mut app = builder.build_default()?;
    app.renderer.add_responsive_trigger(
        "narrow".to_owned(),
        hyperchad::renderer::transformer::ResponsiveTrigger::MaxWidth(
            hyperchad::renderer::transformer::Number::Integer(640),
        ),
    );
    app.renderer.add_responsive_trigger(
        "tablet".to_owned(),
        hyperchad::renderer::transformer::ResponsiveTrigger::MaxWidth(
            hyperchad::renderer::transformer::Number::Integer(960),
        ),
    );
    Ok(app)
}
