//! Entry point for the bcode documentation website.

use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = switchy::unsync::runtime::Builder::new().build()?;
    let runtime = Arc::new(runtime);

    let app = bcode_docs_site::init()
        .with_viewport(bcode_docs_site::VIEWPORT.clone())
        .with_runtime_handle(runtime.handle());

    bcode_docs_site::build_app(app)?.run()?;

    Ok(())
}
