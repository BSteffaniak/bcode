use super::*;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::{TextEditBuffer, TextEditCommand};

#[test]
fn host_owned_or_disabled_enter_never_falls_through_to_submission() {
    let mut state = TextInputState::new(TextEditBuffer::new());
    state.buffer_mut().insert_str("draft");
    assert_eq!(
        apply_editor_binding(&mut state, SessionKeyBinding::Host),
        None
    );
    assert_eq!(
        apply_editor_binding(&mut state, SessionKeyBinding::Ignored),
        Some(TextInputOutcome::Ignored)
    );
    assert_eq!(state.buffer().text(), "draft");
}

#[test]
fn configured_editor_command_wins_over_the_physical_key() {
    struct InsertNewline;
    impl SessionKeyBindings for InsertNewline {
        fn resolve(&self, _: KeyStroke) -> SessionKeyBinding {
            SessionKeyBinding::Editor(TextEditCommand::Insert('\n'))
        }
    }
    let mut ui = crate::session_ui::SessionUi::new();
    ui.composer.buffer_mut().insert_str("draft");
    assert_eq!(
        ui.update_configured_key(KeyStroke::simple(KeyCode::Enter), &InsertNewline),
        Some(TextInputOutcome::Edited)
    );
    assert_eq!(ui.composer.buffer().text(), "draft\n");
}
