//! Minimal host type shims for the code review native TUI surface.

use std::io;

use bmux_keyboard::KeyStroke;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use bmux_tui_components::text_input::{
    EnterBehavior as TextInputEnterBehavior, TextInputOutcome as TextInputKeyOutcome,
};

/// Errors returned by the code review TUI surface.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// Client error.
    #[error("client error: {0}")]
    Client(#[from] bcode_client::ClientError),
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// Task join error.
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// Plugin service error.
    #[error("plugin service error {code}: {message}")]
    PluginService { code: String, message: String },
    /// JSON error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Shared helper functions needed by the code review TUI.
pub mod helpers {
    use super::{KeyStroke, Rect, Terminal, TextInputEnterBehavior, TextInputKeyOutcome};
    use std::io::{self, Write};

    /// Apply a key stroke to a text buffer using the default text-input bindings.
    pub fn handle_default_text_key(
        buffer: &mut bmux_text_edit::TextEditBuffer,
        stroke: KeyStroke,
        enter_behavior: TextInputEnterBehavior,
    ) -> TextInputKeyOutcome {
        let mut policy = bmux_tui_components::text_input::TextInputPolicy::chat_composer();
        policy.keyboard.enter = enter_behavior;
        let mut state = bmux_tui_components::text_input::TextInputState::new(buffer.clone());
        let outcome = bmux_tui_components::text_input::TextInputControl::new(&policy)
            .handle_key(&mut state, stroke);
        *buffer = state.buffer().clone();
        outcome
    }

    /// Resize a terminal from current crossterm dimensions.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal size cannot be read.
    pub fn resize_from_terminal<W: Write>(terminal: &mut Terminal<&mut W>) -> io::Result<bool> {
        let area = terminal_area()?;
        let resized = terminal.area() != area;
        terminal.resize(area);
        Ok(resized)
    }

    fn terminal_area() -> io::Result<Rect> {
        let (width, height) = crossterm::terminal::size()?;
        Ok(Rect::new(0, 0, width, height))
    }
}
