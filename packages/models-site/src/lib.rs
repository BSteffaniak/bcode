#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bcode models catalog website.

use std::sync::LazyLock;

use bcode_model_catalog::{OutputFormat, default_source_dir, load_catalog};
use hyperchad::app::{App, AppBuilder, renderer::DefaultRenderer};
use hyperchad::color::Color;
use hyperchad::router::Router;

static BACKGROUND_COLOR: LazyLock<Color> = LazyLock::new(|| Color::from_hex("#0d1117"));

/// Default viewport meta tag for responsive design.
pub static VIEWPORT: LazyLock<String> =
    LazyLock::new(|| "width=device-width, initial-scale=1".to_string());

/// Application router.
pub static ROUTER: LazyLock<Router> = LazyLock::new(|| {
    Router::new().with_static_route(&["/", "/home"], |_| async {
        let catalog = load_catalog(&default_source_dir()).unwrap_or_else(|_| {
            bcode_model_catalog_models::CatalogDocument::empty(
                option_env!("GIT_HASH").unwrap_or("unknown"),
                "1970-01-01T00:00:00Z",
            )
        });
        bcode_models_site_ui::pages::home::home(&catalog)
    })
});

/// Initialize the application builder with default configuration.
#[must_use]
pub fn init() -> AppBuilder {
    AppBuilder::new()
        .with_router(ROUTER.clone())
        .with_background(*BACKGROUND_COLOR)
        .with_title("models.bmux.dev".to_string())
        .with_description("Model catalog for Bcode and BMUX".to_string())
        .with_size(1100.0, 700.0)
}

/// Build the application from the provided builder.
///
/// # Errors
///
/// Returns an error if the application fails to build.
pub fn build_app(builder: AppBuilder) -> Result<App<DefaultRenderer>, hyperchad::app::Error> {
    Ok(builder.build_default()?)
}

/// Build catalog API artifacts next to the HyperChad-generated static site.
///
/// Optional live snapshots can be provided with `--live <dir>`.
///
/// # Errors
///
/// Returns an error if catalog artifacts cannot be generated.
pub fn build_catalog_artifacts(
    output_dir: &std::path::Path,
    live_dir: Option<&std::path::Path>,
) -> bcode_model_catalog::Result<()> {
    bcode_model_catalog::build_artifacts_with_live(
        &default_source_dir(),
        live_dir,
        &output_dir.join("v1"),
        OutputFormat::PrettyJson,
    )
}
