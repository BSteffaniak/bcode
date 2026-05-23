//! Shared helpers for TUI flows.

use std::io::{self, Write};

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;
use crossterm::terminal::size;

use super::TuiError;
use super::app::BmuxApp;
use super::keymap::BmuxKeyMap;

/// Apply a key stroke to a text buffer using configured editor bindings first.
pub fn handle_text_buffer_key(
    buffer: &mut bmux_text_edit::TextEditBuffer,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
    enter_behavior: TextInputEnterBehavior,
) -> TextInputKeyOutcome {
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        buffer.apply_command(command);
        return TextInputKeyOutcome::Edited;
    }
    handle_default_text_key(buffer, stroke, enter_behavior)
}

/// Apply a key stroke to a text buffer using the default text-input bindings.
///
/// Shift-only character strokes are text input. Command-style modified
/// character strokes are left ignored so shortcuts like Cmd-C do not leak text.
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

fn shifted_text_character(stroke: KeyStroke) -> Option<char> {
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

/// Report a client error in status and transcript.
pub fn report_client_error(app: &mut BmuxApp, label: &str, error: &TuiError) {
    let message = format!("{label}: {error}");
    app.set_status(message.clone());
    app.push_system_note(message);
}

/// Resize a terminal from current crossterm dimensions.
pub fn resize_from_terminal<W: Write>(terminal: &mut Terminal<&mut W>) -> io::Result<bool> {
    let area = terminal_area()?;
    let resized = terminal.area() != area;
    terminal.resize(area);
    Ok(resized)
}

/// Return the current terminal area.
pub fn terminal_area() -> io::Result<Rect> {
    let (width, height) = size()?;
    Ok(Rect::new(0, 0, width, height))
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
    use bmux_text_edit::TextEditBuffer;
    use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};

    use super::handle_default_text_key;

    #[test]
    fn default_text_key_inserts_plain_character() {
        let mut buffer = TextEditBuffer::new();
        let outcome = handle_default_text_key(
            &mut buffer,
            KeyStroke::simple(KeyCode::Char('c')),
            TextInputEnterBehavior::Submit,
        );

        assert_eq!(outcome, TextInputKeyOutcome::Edited);
        assert_eq!(buffer.text(), "c");
    }

    #[test]
    fn default_text_key_inserts_shifted_uppercase_character() {
        let mut buffer = TextEditBuffer::new();
        let outcome = handle_default_text_key(
            &mut buffer,
            KeyStroke::with_modifiers(
                KeyCode::Char('a'),
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            ),
            TextInputEnterBehavior::Submit,
        );

        assert_eq!(outcome, TextInputKeyOutcome::Edited);
        assert_eq!(buffer.text(), "A");
    }

    #[test]
    fn default_text_key_ignores_super_modified_character() {
        let mut buffer = TextEditBuffer::new();
        let outcome = handle_default_text_key(
            &mut buffer,
            KeyStroke::with_modifiers(
                KeyCode::Char('c'),
                Modifiers {
                    super_key: true,
                    ..Modifiers::NONE
                },
            ),
            TextInputEnterBehavior::Submit,
        );

        assert_eq!(outcome, TextInputKeyOutcome::Ignored);
        assert_eq!(buffer.text(), "");
    }

    #[test]
    fn default_text_key_ignores_ctrl_modified_character() {
        let mut buffer = TextEditBuffer::new();
        let outcome = handle_default_text_key(
            &mut buffer,
            KeyStroke::with_modifiers(
                KeyCode::Char('c'),
                Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                },
            ),
            TextInputEnterBehavior::Submit,
        );

        assert_eq!(outcome, TextInputKeyOutcome::Ignored);
        assert_eq!(buffer.text(), "");
    }
}
