//! Terminal user interface for Bcode.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod bmux_backend;

/// Errors returned by the TUI.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// Client error.
    #[error("client error: {0}")]
    Client(#[from] bcode_client::ClientError),
    /// Config error.
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Session selection was canceled.
    #[error("session selection canceled")]
    Canceled,
}

/// Run the terminal user interface.
///
/// # Errors
///
/// Returns client, config, I/O, or cancellation errors encountered while
/// running the BMUX TUI.
pub async fn run(session_id: Option<bcode_session_models::SessionId>) -> Result<(), TuiError> {
    bmux_backend::run(session_id).await
}
