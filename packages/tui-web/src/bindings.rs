//! Explicit browser keymap policy admitted through the shared native binding seam.
use bcode_tui_components::session_ui::{
    bindings::{SessionKeyBinding, SessionKeyBindings, apply_editor_binding},
    composer_policy,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui_components::text_input::{TextInputControl, TextInputState};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Submit,
    Newline,
    PageUp,
    PageDown,
    Latest,
    Copy,
    PermissionApprove,
    PermissionDeny,
    SelectUp,
    SelectDown,
    SelectConfirm,
    SelectCancel,
    Ignore,
}

pub struct Bindings {
    overrides: BTreeMap<String, Command>,
}
impl Bindings {
    pub const fn new() -> Self {
        Self {
            overrides: BTreeMap::new(),
        }
    }
    pub fn configure(&mut self, json: &str) -> Result<(), &'static str> {
        if json.len() > 16 * 1024 {
            return Err("keymap too large");
        }
        let overrides: BTreeMap<String, Command> =
            serde_json::from_str(json).map_err(|_| "invalid keymap")?;
        if overrides.len() > 128 || overrides.keys().any(|key| key.len() > 64) {
            return Err("keymap too large");
        }
        self.overrides = overrides;
        Ok(())
    }
    pub fn command(&self, key: KeyStroke) -> Option<Command> {
        let name = key_name(key);
        if let Some(command) = self.overrides.get(&name) {
            return Some(*command);
        }
        match key.key {
            KeyCode::Enter if key.modifiers.shift => Some(Command::Newline),
            KeyCode::Enter
                if !key.modifiers.ctrl && !key.modifiers.alt && !key.modifiers.super_key =>
            {
                Some(Command::Submit)
            }
            KeyCode::PageUp => Some(Command::PageUp),
            KeyCode::PageDown => Some(Command::PageDown),
            KeyCode::Char('c' | 'C') if key.modifiers.ctrl && key.modifiers.shift => {
                Some(Command::Copy)
            }
            _ => None,
        }
    }
    pub fn permission_action(
        &self,
        key: KeyStroke,
    ) -> bcode_tui_components::permission::decision::PermissionAction {
        use bcode_tui_components::permission::decision::PermissionAction as Action;
        match self.overrides.get(&key_name(key)) {
            Some(Command::PermissionApprove) => Action::PermissionApprove,
            Some(Command::PermissionDeny) => Action::PermissionDeny,
            Some(Command::SelectUp) => Action::SelectUp,
            Some(Command::SelectDown) => Action::SelectDown,
            Some(Command::SelectConfirm) => Action::SelectConfirm,
            Some(Command::SelectCancel) => Action::SelectCancel,
            _ => Action::Other,
        }
    }
    pub fn edit(&self, state: &mut TextInputState, key: KeyStroke) {
        let binding = self.resolve(key);
        match binding {
            SessionKeyBinding::Literal(key) => {
                TextInputControl::new(&composer_policy())
                    .handle_key_with_history_action(state, key, None);
            }
            binding => {
                apply_editor_binding(state, binding);
            }
        }
    }
}
impl SessionKeyBindings for Bindings {
    fn resolve(&self, key: KeyStroke) -> SessionKeyBinding {
        if self.command(key).is_some() {
            return SessionKeyBinding::Host;
        }
        // The real native editor handles visual movement/selection/history. Admit
        // only keys recognized by its effective policy, never arbitrary shortcuts.
        let policy = composer_policy();
        if policy.keyboard.keymap.command_for_key(key).is_some() || key.modifiers.shift {
            SessionKeyBinding::Literal(key)
        } else {
            SessionKeyBinding::Ignored
        }
    }
}
fn key_name(key: KeyStroke) -> String {
    let mut result = String::new();
    if key.modifiers.ctrl {
        result.push_str("ctrl+");
    }
    if key.modifiers.alt {
        result.push_str("alt+");
    }
    if key.modifiers.shift {
        result.push_str("shift+");
    }
    if key.modifiers.super_key {
        result.push_str("meta+");
    }
    result.push_str(&match key.key {
        KeyCode::Enter => "enter".into(),
        KeyCode::PageUp => "page_up".into(),
        KeyCode::PageDown => "page_down".into(),
        KeyCode::Char(value) => value.to_lowercase().collect(),
        _ => format!("{:?}", key.key).to_lowercase(),
    });
    result
}
