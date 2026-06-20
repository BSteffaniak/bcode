#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output_dir = static_output_dir_from_args();
    if let Some(output_dir) = output_dir.as_deref() {
        bcode_models_site::build_catalog_artifacts(output_dir)?;
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

fn static_output_dir_from_args() -> Option<std::path::PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--output" {
            return args.next().map(Into::into);
        }
        if let Some(output) = arg.strip_prefix("--output=") {
            return Some(output.into());
        }
    }
    None
}
