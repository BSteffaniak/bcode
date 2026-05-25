//! Terminal user interface for Bcode.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub(crate) mod activity;
pub(crate) mod app;
pub(crate) mod chat_loop;
pub(crate) mod clipboard_image;
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
pub(crate) mod provider_picker;
pub(crate) mod provider_picker_render;
pub(crate) mod render;
pub(crate) mod runtime;
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
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod thinking_dialog;
pub(crate) mod thinking_dialog_render;
pub(crate) mod thinking_flow;
pub(crate) mod tool_present;
pub(crate) mod transcript;
pub(crate) mod transcript_layout;
pub(crate) mod transcript_viewport;

use std::io;
use std::time::Duration;

use bcode_session_models::SessionId;
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::terminal::Terminal;

const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);
const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;
const OLDER_HISTORY_EVENT_LIMIT: usize = 500;

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
