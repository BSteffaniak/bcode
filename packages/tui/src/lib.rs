//! Terminal user interface for Bcode.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

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
    /// Requested TUI backend was not compiled into this binary.
    #[error("TUI backend '{0}' is not enabled at compile time")]
    BackendUnavailable(&'static str),
}

/// Compiled TUI backend choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiBackendKind {
    /// Existing ratatui/crossterm implementation.
    Ratatui,
    /// BMUX-native implementation.
    Bmux,
}

impl TuiBackendKind {
    /// Return the backend name used by environment/config selection.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ratatui => "ratatui",
            Self::Bmux => "bmux",
        }
    }
}

/// Options for launching the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TuiOptions {
    /// Requested backend. When unset, a compiled default is selected.
    pub backend: Option<TuiBackendKind>,
}

impl TuiOptions {
    /// Create options with a backend choice.
    #[must_use]
    pub const fn with_backend(backend: TuiBackendKind) -> Self {
        Self {
            backend: Some(backend),
        }
    }
}

#[cfg(feature = "tui-ratatui")]
mod ratatui_backend;

#[cfg(feature = "tui-bmux")]
mod bmux_backend;

/// Run the terminal user interface with default options.
///
/// # Errors
///
/// Returns client, config, I/O, cancellation, or backend availability errors
/// encountered while running the selected backend.
pub async fn run(session_id: Option<bcode_session_models::SessionId>) -> Result<(), TuiError> {
    run_with_options(session_id, TuiOptions::default()).await
}

/// Run the terminal user interface with explicit options.
///
/// # Errors
///
/// Returns client, config, I/O, cancellation, or backend availability errors
/// encountered while running the selected backend.
pub async fn run_with_options(
    session_id: Option<bcode_session_models::SessionId>,
    options: TuiOptions,
) -> Result<(), TuiError> {
    match selected_backend(options) {
        TuiBackendKind::Ratatui => run_ratatui(session_id).await,
        TuiBackendKind::Bmux => run_bmux(session_id).await,
    }
}

fn selected_backend(options: TuiOptions) -> TuiBackendKind {
    options
        .backend
        .or_else(backend_from_env)
        .unwrap_or_else(default_backend)
}

fn backend_from_env() -> Option<TuiBackendKind> {
    let value = std::env::var("BCODE_TUI").ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "ratatui" | "rata2i" => Some(TuiBackendKind::Ratatui),
        "bmux" | "bmux-tui" => Some(TuiBackendKind::Bmux),
        _ => None,
    }
}

const fn default_backend() -> TuiBackendKind {
    #[cfg(feature = "tui-ratatui")]
    {
        TuiBackendKind::Ratatui
    }
    #[cfg(all(not(feature = "tui-ratatui"), feature = "tui-bmux"))]
    {
        TuiBackendKind::Bmux
    }
    #[cfg(all(not(feature = "tui-ratatui"), not(feature = "tui-bmux")))]
    {
        TuiBackendKind::Ratatui
    }
}

async fn run_ratatui(session_id: Option<bcode_session_models::SessionId>) -> Result<(), TuiError> {
    #[cfg(feature = "tui-ratatui")]
    {
        ratatui_backend::run(session_id).await
    }
    #[cfg(not(feature = "tui-ratatui"))]
    {
        let _ = session_id;
        Err(TuiError::BackendUnavailable("ratatui"))
    }
}

async fn run_bmux(session_id: Option<bcode_session_models::SessionId>) -> Result<(), TuiError> {
    #[cfg(feature = "tui-bmux")]
    {
        tokio::task::yield_now().await;
        bmux_backend::run(session_id)
    }
    #[cfg(not(feature = "tui-bmux"))]
    {
        tokio::task::yield_now().await;
        let _ = session_id;
        Err(TuiError::BackendUnavailable("bmux"))
    }
}
