//! Shared text-input helpers for picker and modal flows.

use bmux_keyboard::KeyStroke;
use bmux_text_edit::SelectionMode;
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

/// Create text-input state initialized with text and optional select-all state.
#[must_use]
pub fn state_with_text(text: impl Into<String>, select_all: bool) -> TextInputState {
    let mut buffer = bmux_text_edit::TextEditBuffer::from_text(text);
    if select_all {
        buffer.select_all();
    }
    TextInputState::new(buffer)
}

/// Edit text after modal action routing, respecting explicit scope overrides.
pub fn handle_key_in_scope(
    state: &mut TextInputState,
    keymap: &BmuxKeyMap,
    scope: &str,
    stroke: KeyStroke,
) -> TextInputOutcome {
    if scope != "chat" {
        use bcode_plugin_sdk::tui::keymap::controls::{ControlAction, ControlInput};
        if keymap.action(scope, stroke).is_some() {
            let input = ControlInput::from_registry(
                keymap.registry(),
                scope,
                stroke,
                ControlAction::TEXT,
                false,
            );
            return input
                .native_stroke()
                .map_or(TextInputOutcome::Ignored, |native| {
                    TextInputControl::new(&single_line_policy())
                        .handle_key_with_history_action(state, native, None)
                });
        }
        if keymap.registry().is_overridden_key(scope, stroke) {
            return TextInputOutcome::Ignored;
        }
    }
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
    // Only literal text is admitted here. Calling BMUX's raw key handler would
    // resurrect its default editor bindings after an explicit remap/unbind.
    if keymap.action("chat", stroke).is_none()
        && !keymap.registry().is_overridden_key("chat", stroke)
        && let Some(bcode_plugin_sdk::tui::keymap::controls::LiteralText(character)) =
            bcode_plugin_sdk::tui::keymap::controls::LiteralText::from_stroke(stroke)
    {
        state
            .buffer_mut()
            .apply_command(bmux_text_edit::TextEditCommand::Insert(character));
        state.sync_scroll_to_cursor(&single_line_policy());
        return TextInputOutcome::Edited;
    }
    TextInputOutcome::Ignored
}

/// Handle pasted text for picker/modal text input.
pub fn handle_paste(state: &mut TextInputState, text: &str) -> TextInputOutcome {
    TextInputControl::new(&single_line_policy()).handle_paste(state, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chat_history_actions_do_not_edit_picker_history() {
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "command_palette".into(),
            std::collections::BTreeMap::from([
                ("ctrl+r".into(), "tui.editor.undo".into()),
                ("ctrl+t".into(), "tui.editor.redo".into()),
            ]),
        );
        let keymap = BmuxKeyMap::from_config(&config);
        let mut input = state_with_text("a", false);
        input.buffer_mut().insert_str("b");
        let before = input.buffer().edit_state();
        for key in ["ctrl+z", "super+z", "ctrl+y", "ctrl+r", "ctrl+t"] {
            assert_eq!(
                handle_key_in_scope(
                    &mut input,
                    &keymap,
                    "command_palette",
                    bmux_keyboard::parse_key_stroke(key).unwrap()
                ),
                TextInputOutcome::Ignored
            );
            assert_eq!(input.buffer().edit_state(), before);
            assert!(input.buffer().can_undo());
        }
    }

    #[test]
    fn scoped_literal_unbind_and_navigation_remap_are_respected() {
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "command_palette".into(),
            std::collections::BTreeMap::from([
                ("x".into(), String::new()),
                ("ctrl+b".into(), "tui.control.left".into()),
            ]),
        );
        let keymap = BmuxKeyMap::from_config(&config);
        let mut input = state_with_text("ab", false);
        assert_eq!(
            handle_key_in_scope(
                &mut input,
                &keymap,
                "command_palette",
                bmux_keyboard::parse_key_stroke("x").unwrap()
            ),
            TextInputOutcome::Ignored
        );
        assert_eq!(input.buffer().text(), "ab");
        assert_eq!(
            handle_key_in_scope(
                &mut input,
                &keymap,
                "command_palette",
                bmux_keyboard::parse_key_stroke("ctrl+b").unwrap()
            ),
            TextInputOutcome::Edited
        );
        handle_key_in_scope(
            &mut input,
            &keymap,
            "command_palette",
            KeyStroke::simple(bmux_keyboard::KeyCode::Char('λ')),
        );
        assert_eq!(input.buffer().text(), "aλb");
    }

    #[test]
    fn unbound_backspace_does_not_fall_back_to_bmux_defaults() {
        let mut config = bcode_config::TuiConfig::default();
        config
            .keybindings
            .chat
            .insert("backspace".into(), String::new());
        let keymap = BmuxKeyMap::from_config(&config);
        let mut input = state_with_text("hello", false);
        assert_eq!(
            handle_key_in_scope(
                &mut input,
                &keymap,
                "chat",
                bmux_keyboard::parse_key_stroke("backspace").unwrap()
            ),
            TextInputOutcome::Ignored
        );
        assert_eq!(input.buffer().text(), "hello");
        assert_eq!(
            handle_key_in_scope(
                &mut input,
                &keymap,
                "chat",
                KeyStroke::simple(bmux_keyboard::KeyCode::Char('λ'))
            ),
            TextInputOutcome::Edited
        );
        assert_eq!(input.buffer().text(), "helloλ");
    }
}
