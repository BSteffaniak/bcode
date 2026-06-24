#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bcode documentation website.

use std::sync::LazyLock;

use hyperchad::app::{App, AppBuilder, renderer::DefaultRenderer};
use hyperchad_docs_site::{DocsSite, HeaderLink};
use serde_json::json;

/// Default viewport meta tag for responsive design.
pub static VIEWPORT: LazyLock<String> =
    LazyLock::new(|| "width=device-width, initial-scale=1".to_string());

#[cfg(feature = "assets")]
static CARGO_MANIFEST_DIR: LazyLock<Option<std::path::PathBuf>> =
    LazyLock::new(|| std::option_env!("CARGO_MANIFEST_DIR").map(Into::into));

#[cfg(feature = "assets")]
static ASSETS_DIR: LazyLock<std::path::PathBuf> = LazyLock::new(|| {
    CARGO_MANIFEST_DIR
        .as_ref()
        .expect("CARGO_MANIFEST_DIR must be available")
        .join("public")
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from("public"))
});

#[cfg(feature = "assets")]
static ASSETS: LazyLock<Vec<hyperchad::renderer::assets::StaticAssetRoute>> = LazyLock::new(|| {
    vec![
        #[cfg(feature = "vanilla-js")]
        hyperchad_docs_site::assets::vanilla_js_route(),
        hyperchad_docs_site::assets::public_dir_route("public", ASSETS_DIR.clone()),
    ]
});

/// Documentation site model with routes and navigation derived from the central
/// UI page registry.
pub static SITE: LazyLock<DocsSite> = LazyLock::new(|| {
    DocsSite::builder("bcode")
        .title("bcode docs")
        .description("Documentation for bcode — a terminal-native coding agent")
        .sections(bcode_docs_site_ui::doc_pages::DOC_SECTIONS)
        .pages(bcode_docs_site_ui::doc_pages::DOC_PAGES)
        .home(bcode_docs_site_ui::pages::home::home)
        .shell(bcode_docs_site_ui::home_layout::shell)
        .brand(">_ bcode", "/")
        .header_links([
            HeaderLink::new("docs", "/docs"),
            HeaderLink::external("github", "https://github.com/BSteffaniak/bcode"),
        ])
        .global_font(bcode_docs_site_ui::home_layout::MONO_FONT)
        .build()
});

/// Initialize the application builder with default configuration.
///
/// # Panics
///
/// Panics if the bundled docs-site static asset route cannot be registered.
#[must_use]
pub fn init() -> AppBuilder {
    let mut app = SITE.clone().init();

    #[cfg(feature = "assets")]
    for assets in ASSETS.iter().cloned() {
        app.static_asset_route_result(assets).unwrap();
    }

    app
}

/// Build the application from the provided builder.
///
/// # Errors
///
/// Returns an error if the application fails to build.
pub fn build_app(builder: AppBuilder) -> Result<App<DefaultRenderer>, hyperchad::app::Error> {
    hyperchad_docs_site::site::build_app(builder)
}

#[must_use]
pub fn viewport() -> serde_json::Value {
    json!({ "viewport": &*VIEWPORT })
}
