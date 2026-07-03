#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[tokio::main]
async fn main() {
    if let Err(error) = Box::pin(bcode_cli::run_with_static_bundled(
        bcode_bundled_plugins::static_bundled_plugins(),
    ))
    .await
    {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
