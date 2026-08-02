#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

fn build_info() -> bcode_build_info::BuildInfo {
    let mode = match env!("BCODE_BUILD_MODE") {
        "developer" => bcode_build_info::BuildMode::Developer,
        "distribution" => bcode_build_info::BuildMode::Distribution,
        value => panic!("invalid embedded Bcode build mode: {value}"),
    };
    let commit = env!("BCODE_BUILD_GIT_COMMIT");
    let git = if commit.is_empty() {
        bcode_build_info::GitState::Unavailable
    } else {
        bcode_build_info::GitState::Revision {
            short_commit: commit.to_owned(),
            dirty: env!("BCODE_BUILD_GIT_DIRTY") == "1",
        }
    };
    bcode_build_info::BuildInfo::new(
        env!("CARGO_PKG_VERSION"),
        mode,
        git,
        env!("BCODE_BUILD_DIGEST"),
    )
    .expect("build script must embed valid Bcode build information")
}

#[tokio::main]
async fn main() {
    let build_info = build_info();
    #[cfg(feature = "static-bundled-plugins")]
    let result = bcode_cli::run_with_static_bundled(
        build_info,
        bcode_bundled_plugins::static_bundled_plugins(),
    )
    .await;
    #[cfg(not(feature = "static-bundled-plugins"))]
    let result = bcode_cli::run(build_info).await;

    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
