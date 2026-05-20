//! BMUX backend keymap adapter.

use std::collections::BTreeMap;

use bcode_config::TuiConfig;
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

/// Key handling scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum BmuxScope {
    /// Chat view.
    Chat,
    /// Permission dialog.
    Permission,
    /// Session picker.
    SessionPicker,
}

/// Actions used by the BMUX backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum BmuxAction {
    InputSubmit,
    InputHistoryPrevious,
    InputHistoryNext,
    AppExit,
    AppInterrupt,
    CommandPaletteOpen,
    TranscriptPageUp,
    TranscriptPageDown,
    TranscriptTop,
    TranscriptBottom,
    TranscriptLineUp,
    TranscriptLineDown,
    PermissionApprove,
    PermissionDeny,
    SelectUp,
    SelectDown,
    SelectConfirm,
    SelectCancel,
    SessionNew,
    SessionRename,
    SessionDelete,
}

impl BmuxAction {
    fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "tui.input.submit" => Self::InputSubmit,
            "tui.input.historyPrevious" => Self::InputHistoryPrevious,
            "tui.input.historyNext" => Self::InputHistoryNext,
            "app.exit" => Self::AppExit,
            "app.interrupt" => Self::AppInterrupt,
            "app.command_palette" => Self::CommandPaletteOpen,
            "transcript.pageUp" => Self::TranscriptPageUp,
            "transcript.pageDown" => Self::TranscriptPageDown,
            "transcript.top" => Self::TranscriptTop,
            "transcript.bottom" => Self::TranscriptBottom,
            "transcript.lineUp" => Self::TranscriptLineUp,
            "transcript.lineDown" => Self::TranscriptLineDown,
            "app.permission.approve" => Self::PermissionApprove,
            "app.permission.deny" => Self::PermissionDeny,
            "tui.select.up" | "tui.select.previous" => Self::SelectUp,
            "tui.select.down" | "tui.select.next" => Self::SelectDown,
            "tui.select.confirm" => Self::SelectConfirm,
            "tui.select.cancel" => Self::SelectCancel,
            "tui.session.new" => Self::SessionNew,
            "tui.session.rename" => Self::SessionRename,
            "tui.session.delete" => Self::SessionDelete,
            _ => return None,
        })
    }
}

/// Compiled BMUX backend keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BmuxKeyMap {
    bindings: BTreeMap<BmuxScope, Vec<(KeyStroke, BmuxAction)>>,
}

impl BmuxKeyMap {
    /// Build a keymap from TUI config.
    #[must_use]
    pub(super) fn from_config(config: &TuiConfig) -> Self {
        let mut bindings = default_bindings();
        apply_scope(&mut bindings, BmuxScope::Chat, &config.keybindings.chat);
        apply_scope(
            &mut bindings,
            BmuxScope::Permission,
            &config.keybindings.permission,
        );
        apply_scope(
            &mut bindings,
            BmuxScope::SessionPicker,
            &config.keybindings.session_picker,
        );
        Self { bindings }
    }

    /// Return the action for a key in `scope`.
    #[must_use]
    pub(super) fn action_for_key(&self, scope: BmuxScope, stroke: KeyStroke) -> Option<BmuxAction> {
        self.bindings.get(&scope).and_then(|bindings| {
            bindings
                .iter()
                .find_map(|(binding, action)| (*binding == stroke).then_some(*action))
        })
    }
}

fn default_bindings() -> BTreeMap<BmuxScope, Vec<(KeyStroke, BmuxAction)>> {
    BTreeMap::from([
        (
            BmuxScope::Chat,
            vec![
                bind("enter", BmuxAction::InputSubmit),
                bind("up", BmuxAction::InputHistoryPrevious),
                bind("down", BmuxAction::InputHistoryNext),
                bind("ctrl+d", BmuxAction::AppExit),
                bind("escape", BmuxAction::AppInterrupt),
                bind("ctrl+p", BmuxAction::CommandPaletteOpen),
                bind("pageUp", BmuxAction::TranscriptPageUp),
                bind("pageDown", BmuxAction::TranscriptPageDown),
                bind("ctrl+home", BmuxAction::TranscriptTop),
                bind("ctrl+end", BmuxAction::TranscriptBottom),
                bind("ctrl+up", BmuxAction::TranscriptLineUp),
                bind("ctrl+down", BmuxAction::TranscriptLineDown),
            ],
        ),
        (
            BmuxScope::Permission,
            vec![
                bind("y", BmuxAction::PermissionApprove),
                bind("a", BmuxAction::PermissionApprove),
                bind("n", BmuxAction::PermissionDeny),
                bind("d", BmuxAction::PermissionDeny),
                bind("escape", BmuxAction::PermissionDeny),
                bind("left", BmuxAction::SelectUp),
                bind("up", BmuxAction::SelectUp),
                bind("right", BmuxAction::SelectDown),
                bind("down", BmuxAction::SelectDown),
                bind("tab", BmuxAction::SelectDown),
                bind("enter", BmuxAction::SelectConfirm),
            ],
        ),
        (
            BmuxScope::SessionPicker,
            vec![
                bind("up", BmuxAction::SelectUp),
                bind("k", BmuxAction::SelectUp),
                bind("down", BmuxAction::SelectDown),
                bind("j", BmuxAction::SelectDown),
                bind("enter", BmuxAction::SelectConfirm),
                bind("n", BmuxAction::SessionNew),
                bind("r", BmuxAction::SessionRename),
                bind("d", BmuxAction::SessionDelete),
                bind("escape", BmuxAction::SelectCancel),
                bind("ctrl+c", BmuxAction::SelectCancel),
            ],
        ),
    ])
}

fn apply_scope(
    bindings: &mut BTreeMap<BmuxScope, Vec<(KeyStroke, BmuxAction)>>,
    scope: BmuxScope,
    configured: &BTreeMap<String, String>,
) {
    let Some(scope_bindings) = bindings.get_mut(&scope) else {
        return;
    };
    for (key, action_id) in configured {
        let Some(action) = BmuxAction::from_id(action_id) else {
            continue;
        };
        let Some(stroke) = parse_key(key) else {
            continue;
        };
        scope_bindings.retain(|(_, existing)| *existing != action);
        scope_bindings.push((stroke, action));
    }
}

fn bind(key: &str, action: BmuxAction) -> (KeyStroke, BmuxAction) {
    (
        parse_key(key).expect("default BMUX key binding must parse"),
        action,
    )
}

fn parse_key(input: &str) -> Option<KeyStroke> {
    let mut modifiers = Modifiers::NONE;
    let mut key = None;
    for part in input.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" | "option" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "super" | "cmd" | "command" => modifiers.super_key = true,
            code => key = parse_key_code(code),
        }
    }
    key.map(|key| KeyStroke { key, modifiers })
}

fn parse_key_code(input: &str) -> Option<KeyCode> {
    Some(match input {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "escape" | "esc" => KeyCode::Escape,
        "space" => KeyCode::Space,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page_up" => KeyCode::PageUp,
        "pagedown" | "page_down" => KeyCode::PageDown,
        "insert" | "ins" => KeyCode::Insert,
        value if value.len() == 1 => KeyCode::Char(value.chars().next()?),
        _ => return None,
    })
}
