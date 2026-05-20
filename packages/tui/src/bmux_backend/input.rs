//! BMUX backend input handling.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};

use super::app::BmuxApp;

/// Handle a key stroke.
pub(super) fn handle_key(app: &mut BmuxApp, stroke: KeyStroke) {
    if should_exit(stroke) {
        app.request_exit();
        return;
    }

    let outcome = TextInputKeyHandler::new(TextKeymap::default(), TextInputEnterBehavior::Submit)
        .handle_key(app.composer_mut(), stroke);
    if outcome == TextInputKeyOutcome::Submitted {
        app.composer_mut().clear();
    }
}

fn should_exit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Escape
        || (matches!(stroke.key, KeyCode::Char('c' | 'C')) && stroke.modifiers.ctrl)
}
