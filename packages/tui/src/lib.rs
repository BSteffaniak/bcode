//! Terminal user interface for Bcode.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub(crate) mod activity;
pub(crate) mod app;
pub(crate) mod async_values;
pub(crate) mod chat_loop;
pub(crate) mod clipboard_image;
pub mod code_review;
pub(crate) mod code_review_display;
pub mod code_review_home;
pub(crate) mod code_review_render;
pub(crate) mod command_palette;
pub(crate) mod command_palette_render;
pub(crate) mod composer_flow;
pub(crate) mod cursor_blink;
pub(crate) mod diff_extract;
pub(crate) mod diff_panel;
pub(crate) mod exit_state;
pub(crate) mod filtered_list;
pub(crate) mod helpers;
pub(crate) mod history_flow;
pub(crate) mod input;
pub(crate) mod input_history;
pub(crate) mod invalidation;
pub(crate) mod keymap;
pub(crate) mod model_flow;
pub(crate) mod model_picker;
pub(crate) mod model_picker_render;
pub(crate) mod mouse_flow;
pub(crate) mod older_history;
pub(crate) mod palette_flow;
pub(crate) mod pending_submission;
pub(crate) mod pending_submissions;
pub(crate) mod permission_dialog;
pub(crate) mod permission_dialog_render;
pub(crate) mod permission_flow;
pub(crate) mod permission_present;
pub(crate) mod picker_mouse;
pub(crate) mod picker_render;
pub(crate) mod plugin_surface_host;
pub mod plugin_tui;
pub(crate) mod provider_picker;
pub(crate) mod provider_picker_render;
pub(crate) mod render;
pub(crate) mod runtime;
mod runtime_context;
pub(crate) mod runtime_work_view;
pub(crate) mod session_flow;
pub(crate) mod session_picker;
pub(crate) mod session_picker_render;
pub(crate) mod skill_flow;
pub(crate) mod skill_picker;
pub(crate) mod skill_picker_render;
pub(crate) mod slash_commands;
pub(crate) mod slash_flow;
pub(crate) mod slash_palette;
pub(crate) mod slash_palette_render;
pub(crate) mod temporal;
pub(crate) mod terminal_events;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod text_input_flow;
mod thinking_dialog;
pub(crate) mod thinking_dialog_render;
pub(crate) mod thinking_flow;
pub(crate) mod time_format;
pub(crate) mod tool_present;
pub(crate) mod transcript;
pub(crate) mod transcript_document;
pub(crate) mod transcript_layout;
pub(crate) mod transcript_projection;
pub(crate) mod transcript_viewport;
pub(crate) mod worktree_create_dialog;
pub(crate) mod worktree_create_dialog_render;
pub(crate) mod worktree_flow;
pub(crate) mod worktree_picker;
pub(crate) mod worktree_picker_render;

use std::io;

use bcode_session_models::SessionId;
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::terminal::Terminal;

const CURSOR_BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const OLDER_HISTORY_EVENT_LIMIT: usize = 256;

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
    /// Task join error.
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// Plugin service error.
    #[error("plugin service error {code}: {message}")]
    PluginService { code: String, message: String },
    /// JSON error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Session storage is unavailable for normal runtime access.
    #[error("session unavailable: {session_id}: {reason}")]
    SessionUnavailable {
        session_id: SessionId,
        reason: String,
    },
    /// Session selection was canceled.
    #[error("session selection canceled")]
    Canceled,
}

/// Run the terminal user interface.
///
/// # Errors
///
/// Returns I/O errors from terminal setup, event polling, drawing, or Bcode
/// client operations.
pub async fn run(session_id: Option<SessionId>) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        runtime::run_event_loop(&mut terminal, session_id).await
    };

    match result {
        Ok(()) => {
            let _writer = guard.leave()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Run the full-screen review home/picker.
///
/// # Errors
///
/// Returns I/O, client, or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_code_review_home(repo_path: std::path::PathBuf) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        code_review_home::run(&mut terminal, repo_path).await
    };

    match result {
        Ok(code_review_home::ReviewHomeOutcome::OpenWorkspace {
            workspace,
            build_mode,
        }) => {
            let _writer = guard.leave()?;
            run_code_review_workspace(workspace, build_mode).await
        }
        Ok(code_review_home::ReviewHomeOutcome::Exit) => {
            let _writer = guard.leave()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Run the full-screen local code review interface for an existing workspace.
///
/// # Errors
///
/// Returns I/O, client, or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_code_review_workspace(
    workspace: bcode_code_review_models::ReviewWorkspace,
    build_mode: bool,
) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        code_review::run_workspace(&mut terminal, workspace, build_mode).await
    };

    match result {
        Ok(session_id) => {
            let _writer = guard.leave()?;
            if let Some(session_id) = session_id {
                run(Some(session_id)).await
            } else {
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}

/// Run the full-screen local code review interface.
///
/// # Errors
///
/// Returns I/O, client, or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_code_review(
    repo_path: std::path::PathBuf,
    target: code_review::ReviewOpenTarget,
) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        code_review::run(&mut terminal, repo_path, target).await
    };

    match result {
        Ok(session_id) => {
            let _writer = guard.leave()?;
            if let Some(session_id) = session_id {
                run(Some(session_id)).await
            } else {
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}
