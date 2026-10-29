#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Standalone recap application. No daemon or global Bcode session is used.
mod application;
mod bcode_adapter;
mod cli;
mod connections;
#[cfg(feature = "tui")]
mod tui;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    Box::pin(cli::main()).await
}
