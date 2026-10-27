#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Standalone recap application. No daemon or global Bcode session is used.
mod bcode_adapter;
mod cli;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    cli::main().await
}
