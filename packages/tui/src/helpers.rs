//! Shared helpers for TUI flows.

use std::io;

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use crossterm::terminal::size;

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
