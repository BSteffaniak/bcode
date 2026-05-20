//! BMUX backend input handling.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};

use super::app::BmuxApp;

/// Result of handling one key stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct KeyOutcome {
    /// Whether the caller should redraw the UI.
    pub(super) redraw: bool,
    /// Whether the composer was submitted.
    pub(super) submitted: bool,
}

/// Handle a key stroke.
pub(super) fn handle_key(app: &mut BmuxApp, stroke: KeyStroke) -> KeyOutcome {
    if should_exit(stroke) {
        app.request_exit();
        return KeyOutcome {
            redraw: true,
            submitted: false,
        };
    }

    if stroke.key == KeyCode::Up && stroke.modifiers.is_empty() {
        return KeyOutcome {
            redraw: app.previous_input_history(),
            submitted: false,
        };
    }
    if stroke.key == KeyCode::Down && stroke.modifiers.is_empty() {
        return KeyOutcome {
            redraw: app.next_input_history(),
            submitted: false,
        };
    }

    let outcome = TextInputKeyHandler::new(TextKeymap::default(), TextInputEnterBehavior::Submit)
        .handle_key(app.composer_mut(), stroke);
    match outcome {
        TextInputKeyOutcome::Submitted => {
            app.stage_submission();
            app.wake_cursor();
            KeyOutcome {
                redraw: true,
                submitted: true,
            }
        }
        TextInputKeyOutcome::Edited => {
            app.wake_cursor();
            KeyOutcome {
                redraw: true,
                submitted: false,
            }
        }
        TextInputKeyOutcome::Ignored => KeyOutcome::default(),
    }
}

fn should_exit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Escape
        || (matches!(stroke.key, KeyCode::Char('c' | 'C')) && stroke.modifiers.ctrl)
}
