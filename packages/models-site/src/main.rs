#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(output_dir) = static_output_dir_from_args().as_deref() {
        bcode_models_site::build_catalog_artifacts(output_dir, live_dir_from_env().as_deref())?;
    }

    let runtime = switchy::unsync::runtime::Builder::new().build()?;
    let runtime = Arc::new(runtime);
    let app = bcode_models_site::init()
        .with_viewport(bcode_models_site::VIEWPORT.clone())
        .with_router(bcode_models_site::ROUTER.clone())
        .with_runtime_handle(runtime.handle());

    bcode_models_site::build_app(app)?.run()?;

    Ok(())
}

/// Mirror `HyperChad`'s `gen --output <dir>` argument so catalog artifacts land beside the site.
///
/// Only flags `HyperChad` itself accepts may appear on the command line; its `clap` parser runs
/// afterwards and rejects anything else.
fn static_output_dir_from_args() -> Option<std::path::PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--output" || arg == "-o" {
            return args.next().map(Into::into);
        }
        if let Some(value) = arg.strip_prefix("--output=") {
            return Some(value.into());
        }
    }
    None
}

fn live_dir_from_env() -> Option<std::path::PathBuf> {
    std::env::var_os("BCODE_MODEL_CATALOG_LIVE_DIR")
        .filter(|value| !value.is_empty())
        .map(Into::into)
}
