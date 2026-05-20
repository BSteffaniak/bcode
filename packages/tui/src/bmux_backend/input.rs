//! BMUX backend input handling.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};

use super::app::BmuxApp;

const TRANSCRIPT_SCROLL_ROWS: usize = 3;
const TRANSCRIPT_PAGE_ROWS: usize = 10;

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

    if let Some(redraw) = handle_transcript_navigation(app, stroke) {
        return KeyOutcome {
            redraw,
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

const fn handle_transcript_navigation(app: &mut BmuxApp, stroke: KeyStroke) -> Option<bool> {
    match stroke.key {
        KeyCode::PageUp if stroke.modifiers.is_empty() => {
            Some(app.scroll_transcript_up(TRANSCRIPT_PAGE_ROWS))
        }
        KeyCode::PageDown if stroke.modifiers.is_empty() => {
            Some(app.scroll_transcript_down(TRANSCRIPT_PAGE_ROWS))
        }
        KeyCode::Home if stroke.modifiers.ctrl => Some(app.scroll_transcript_up(usize::MAX / 2)),
        KeyCode::End if stroke.modifiers.ctrl => Some(app.scroll_transcript_to_bottom()),
        KeyCode::Up if stroke.modifiers.ctrl => {
            Some(app.scroll_transcript_up(TRANSCRIPT_SCROLL_ROWS))
        }
        KeyCode::Down if stroke.modifiers.ctrl => {
            Some(app.scroll_transcript_down(TRANSCRIPT_SCROLL_ROWS))
        }
        _ => None,
    }
}

fn should_exit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Escape
        || (matches!(stroke.key, KeyCode::Char('c' | 'C')) && stroke.modifiers.ctrl)
}
