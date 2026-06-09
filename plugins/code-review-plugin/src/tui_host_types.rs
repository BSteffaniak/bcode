//! Minimal host type shims for the code review native TUI surface.

use std::io;

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;

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
    use super::{
        KeyCode, KeyStroke, Rect, Terminal, TextInputEnterBehavior, TextInputKeyHandler,
        TextInputKeyOutcome, TextKeymap,
    };
    use std::io::{self, Write};

    /// Apply a key stroke to a text buffer using the default text-input bindings.
    pub fn handle_default_text_key(
        buffer: &mut bmux_text_edit::TextEditBuffer,
        stroke: KeyStroke,
        enter_behavior: TextInputEnterBehavior,
    ) -> TextInputKeyOutcome {
        if let Some(ch) = shifted_text_character(stroke) {
            buffer.insert_char(ch);
            return TextInputKeyOutcome::Edited;
        }

        TextInputKeyHandler::new(TextKeymap::default(), enter_behavior).handle_key(buffer, stroke)
    }

    const fn shifted_text_character(stroke: KeyStroke) -> Option<char> {
        if !stroke.modifiers.shift
            || stroke.modifiers.ctrl
            || stroke.modifiers.alt
            || stroke.modifiers.super_key
            || stroke.modifiers.hyper
            || stroke.modifiers.meta
        {
            return None;
        }

        match stroke.key {
            KeyCode::Char(ch) if ch.is_ascii_lowercase() => Some(ch.to_ascii_uppercase()),
            KeyCode::Char(ch) => Some(ch),
            _ => None,
        }
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
