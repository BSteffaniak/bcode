#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[tokio::main]
async fn main() {
    #[cfg(feature = "static-bundled-plugins")]
    let result =
        bcode_cli::run_with_static_bundled(bcode_bundled_plugins::static_bundled_plugins()).await;
    #[cfg(not(feature = "static-bundled-plugins"))]
    let result = bcode_cli::run().await;

    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
