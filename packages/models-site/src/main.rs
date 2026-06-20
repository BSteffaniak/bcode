#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (output_dir, live_dir) = static_paths_from_args();
    if let Some(output_dir) = output_dir.as_deref() {
        bcode_models_site::build_catalog_artifacts(output_dir, live_dir.as_deref())?;
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

fn static_paths_from_args() -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let mut output = None;
    let mut live = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--output" {
            output = args.next().map(Into::into);
        } else if let Some(value) = arg.strip_prefix("--output=") {
            output = Some(value.into());
        } else if arg == "--live" {
            live = args.next().map(Into::into);
        } else if let Some(value) = arg.strip_prefix("--live=") {
            live = Some(value.into());
        }
    }
    (output, live)
}
