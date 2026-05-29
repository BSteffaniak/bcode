//! Shared text-input helpers for picker and modal flows.

use bmux_keyboard::KeyStroke;
use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui_components::text_input::{
    TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};

use super::keymap::BmuxKeyMap;

/// Return the standard single-line picker/modal text-input policy.
#[must_use]
pub const fn single_line_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

/// Create empty text-input state.
#[must_use]
pub fn empty_state() -> TextInputState {
    TextInputState::default()
}

/// Create text-input state from text, optionally selecting all text initially.
#[must_use]
pub fn state_with_text(text: &str, select_all: bool) -> TextInputState {
    let mut buffer = TextEditBuffer::from_text(text);
    if select_all {
        buffer.move_cursor_with_selection(TextMotion::Start, SelectionMode::Extend);
    }
    TextInputState::new(buffer)
}

/// Handle one key using configured editor bindings and reusable text-input behavior.
pub fn handle_key(
    state: &mut TextInputState,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> TextInputOutcome {
    if let Some(motion) = keymap.editor_selection_motion_for_key(stroke) {
        state
            .buffer_mut()
            .move_cursor_with_selection(motion, SelectionMode::Extend);
        state.sync_scroll_to_cursor(&single_line_policy());
        return TextInputOutcome::Edited;
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        state.buffer_mut().apply_command(command);
        state.sync_scroll_to_cursor(&single_line_policy());
        return TextInputOutcome::Edited;
    }
    TextInputControl::new(&single_line_policy()).handle_key(state, stroke)
}

/// Handle pasted text for picker/modal text input.
pub fn handle_paste(state: &mut TextInputState, text: &str) -> TextInputOutcome {
    TextInputControl::new(&single_line_policy()).handle_paste(state, text)
}
