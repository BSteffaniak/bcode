//! BMUX-native TUI backend.

mod app;
mod chat_loop;
mod command_palette;
mod command_palette_render;
mod composer_flow;
mod diff_extract;
mod diff_panel;
mod filtered_list;
mod helpers;
mod history_flow;
mod input;
mod keymap;
mod model_flow;
mod model_picker;
mod model_picker_render;
mod mouse_flow;
mod palette_flow;
mod pending_submission;
mod permission_dialog;
mod permission_dialog_render;
mod permission_flow;
mod picker_mouse;
mod picker_render;
mod provider_picker;
mod provider_picker_render;
mod render;
mod runtime;
mod session_flow;
mod session_picker;
mod session_picker_render;
mod skill_flow;
mod skill_picker;
mod skill_picker_render;
mod slash_commands;
mod slash_flow;
mod slash_palette;
mod slash_palette_render;
#[cfg(test)]
mod tests;
mod transcript;

use std::io;
use std::time::Duration;

use super::TuiError;
use bcode_session_models::SessionId;
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::terminal::Terminal;

const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);
const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;
const OLDER_HISTORY_EVENT_LIMIT: usize = 500;
const MOUSE_WHEEL_ROWS: usize = 1;

/// Run the BMUX-native TUI backend.
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
            guard.writer_mut().expect("guard writer exists"),
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
