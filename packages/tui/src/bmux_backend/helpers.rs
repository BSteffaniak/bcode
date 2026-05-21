//! Shared helpers for BMUX backend flows.

use std::io::{self, Write};

use bmux_keyboard::KeyStroke;
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;
use crossterm::terminal::size;

use super::TuiError;
use super::app::BmuxApp;
use super::keymap::BmuxKeyMap;

/// Apply a key stroke to a text buffer using configured editor bindings first.
pub(super) fn handle_text_buffer_key(
    buffer: &mut bmux_text_edit::TextEditBuffer,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
    enter_behavior: TextInputEnterBehavior,
) -> TextInputKeyOutcome {
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        buffer.apply_command(command);
        return TextInputKeyOutcome::Edited;
    }
    TextInputKeyHandler::new(TextKeymap::default(), enter_behavior).handle_key(buffer, stroke)
}

/// Report a client error in status and transcript.
pub(super) fn report_client_error(app: &mut BmuxApp, label: &str, error: &TuiError) {
    let message = format!("{label}: {error}");
    app.set_status(message.clone());
    app.push_system_note(message);
}

/// Resize a terminal from current crossterm dimensions.
pub(super) fn resize_from_terminal<W: Write>(terminal: &mut Terminal<&mut W>) -> io::Result<bool> {
    let area = terminal_area()?;
    let resized = terminal.area() != area;
    terminal.resize(area);
    Ok(resized)
}

/// Return the current terminal area.
pub(super) fn terminal_area() -> io::Result<Rect> {
    let (width, height) = size()?;
    Ok(Rect::new(0, 0, width, height))
}
