//! TUI keymap adapter.

use std::collections::BTreeMap;

use bcode_config::TuiConfig;
use bmux_keyboard::{KeyCode, KeyStroke, parse_key_stroke};

/// Key handling scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BmuxScope {
    /// Chat view.
    Chat,
    /// Permission dialog.
    Permission,
    /// Session picker.
    SessionPicker,
    /// Skill picker.
    SkillPicker,
}

/// Actions used by the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BmuxAction {
    InputSubmitSteering,
    InputSubmitFollowUp,
    InputHistoryPrevious,
    InputHistoryNext,
    AppExit,
    AppInterrupt,
    ClipboardPasteImage,
    CommandPaletteOpen,
    AgentCycle,
    ThinkingEffortCycle,
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
    InputNewLine,
    EditorMoveLeft,
    EditorMoveRight,
    EditorMoveWordLeft,
    EditorMoveWordRight,
    EditorMoveStart,
    EditorMoveEnd,
    EditorSelectLeft,
    EditorSelectRight,
    EditorSelectWordLeft,
    EditorSelectWordRight,
    EditorSelectUp,
    EditorSelectDown,
    EditorDeleteBackward,
    EditorDeleteForward,
    EditorDeleteWordBackward,
    EditorDeleteWordForward,
    EditorDeleteToStart,
    EditorDeleteToEnd,
    SkillInvoke,
    SkillActivate,
    SkillDeactivate,
    SkillHelp,
}

impl BmuxAction {
    fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "tui.input.submit" | "tui.input.submitSteering" => Self::InputSubmitSteering,
            "tui.input.submitFollowUp" => Self::InputSubmitFollowUp,
            "tui.input.historyPrevious" => Self::InputHistoryPrevious,
            "tui.input.historyNext" => Self::InputHistoryNext,
            "app.exit" => Self::AppExit,
            "app.interrupt" => Self::AppInterrupt,
            "app.clipboard.pasteImage" => Self::ClipboardPasteImage,
            "app.command_palette" => Self::CommandPaletteOpen,
            "tui.agent.cycle" => Self::AgentCycle,
            "tui.thinking.effort.cycle" => Self::ThinkingEffortCycle,
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
            "tui.input.newLine" | "tui.input.newline" => Self::InputNewLine,
            "tui.editor.moveCursorLeft" => Self::EditorMoveLeft,
            "tui.editor.moveCursorRight" => Self::EditorMoveRight,
            "tui.editor.moveCursorWordLeft" => Self::EditorMoveWordLeft,
            "tui.editor.moveCursorWordRight" => Self::EditorMoveWordRight,
            "tui.editor.moveCursorStart" => Self::EditorMoveStart,
            "tui.editor.moveCursorEnd" => Self::EditorMoveEnd,
            "tui.editor.selectCursorLeft" => Self::EditorSelectLeft,
            "tui.editor.selectCursorRight" => Self::EditorSelectRight,
            "tui.editor.selectCursorWordLeft" => Self::EditorSelectWordLeft,
            "tui.editor.selectCursorWordRight" => Self::EditorSelectWordRight,
            "tui.editor.selectCursorUp" => Self::EditorSelectUp,
            "tui.editor.selectCursorDown" => Self::EditorSelectDown,
            "tui.editor.deleteCharBackward" => Self::EditorDeleteBackward,
            "tui.editor.deleteCharForward" => Self::EditorDeleteForward,
            "tui.editor.deleteWordBackward" => Self::EditorDeleteWordBackward,
            "tui.editor.deleteWordForward" => Self::EditorDeleteWordForward,
            "tui.editor.deleteToStart" => Self::EditorDeleteToStart,
            "tui.editor.deleteToEnd" => Self::EditorDeleteToEnd,
            "tui.skill.invoke" => Self::SkillInvoke,
            "tui.skill.activate" => Self::SkillActivate,
            "tui.skill.deactivate" => Self::SkillDeactivate,
            "tui.skill.help" => Self::SkillHelp,
            _ => return None,
        })
    }
}

/// Key binding activation behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BmuxKeyActivation {
    /// Run the action on the first matching key stroke.
    Immediate,
    /// Run the action only after repeated taps within a bounded window.
    MultiTap {
        /// Number of taps required before the action runs.
        required_taps: u8,
        /// Time window, in milliseconds, between taps.
        window_ms: u64,
        /// Status prompt to show while waiting for more taps.
        prompt: String,
    },
}

impl BmuxKeyActivation {
    const fn immediate() -> Self {
        Self::Immediate
    }
}

/// A resolved TUI key binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxKeyBinding {
    stroke: KeyStroke,
    action: BmuxAction,
    activation: BmuxKeyActivation,
}

impl BmuxKeyBinding {
    /// Create a key binding.
    #[must_use]
    pub const fn new(stroke: KeyStroke, action: BmuxAction, activation: BmuxKeyActivation) -> Self {
        Self {
            stroke,
            action,
            activation,
        }
    }

    /// Return the key stroke that triggers this binding.
    #[must_use]
    pub const fn stroke(&self) -> KeyStroke {
        self.stroke
    }

    /// Return the action this binding invokes.
    #[must_use]
    pub const fn action(&self) -> BmuxAction {
        self.action
    }

    /// Return the activation behavior for this binding.
    #[must_use]
    pub const fn activation(&self) -> &BmuxKeyActivation {
        &self.activation
    }
}

/// Compiled TUI keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxKeyMap {
    bindings: BTreeMap<BmuxScope, Vec<BmuxKeyBinding>>,
}

impl BmuxKeyMap {
    /// Build a keymap from TUI config.
    #[must_use]
    pub fn from_config(config: &TuiConfig) -> Self {
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
        apply_scope(
            &mut bindings,
            BmuxScope::SkillPicker,
            &config.keybindings.session_picker,
        );
        Self { bindings }
    }

    /// Return the action for a key in `scope`.
    #[must_use]
    pub fn action_for_key(&self, scope: BmuxScope, stroke: KeyStroke) -> Option<BmuxAction> {
        self.binding_for_key(scope, stroke)
            .map(|binding| binding.action())
    }

    /// Return the binding for a key in `scope`.
    #[must_use]
    pub fn binding_for_key(&self, scope: BmuxScope, stroke: KeyStroke) -> Option<BmuxKeyBinding> {
        self.bindings.get(&scope).and_then(|bindings| {
            bindings
                .iter()
                .find(|binding| binding.stroke == stroke)
                .cloned()
        })
    }

    /// Return compact chat key hints from configured bindings.
    #[must_use]
    pub fn chat_hints(&self) -> String {
        [
            (BmuxAction::InputSubmitSteering, "send"),
            (BmuxAction::AppInterrupt, "interrupt"),
            (BmuxAction::AppExit, "exit"),
            (BmuxAction::ClipboardPasteImage, "paste image"),
            (BmuxAction::CommandPaletteOpen, "palette"),
            (BmuxAction::AgentCycle, "agent"),
            (BmuxAction::ThinkingEffortCycle, "think"),
        ]
        .into_iter()
        .filter_map(|(action, label)| {
            self.key_for_action(BmuxScope::Chat, action)
                .map(|stroke| format!("{} {label}", key_label(stroke)))
        })
        .collect::<Vec<_>>()
        .join(" · ")
    }

    /// Return the compact key label for a chat action.
    #[must_use]
    pub fn chat_action_label(&self, action: BmuxAction) -> Option<String> {
        self.key_for_action(BmuxScope::Chat, action).map(key_label)
    }

    fn key_for_action(&self, scope: BmuxScope, action: BmuxAction) -> Option<KeyStroke> {
        self.bindings.get(&scope).and_then(|bindings| {
            bindings
                .iter()
                .find_map(|binding| (binding.action == action).then_some(binding.stroke))
        })
    }

    /// Return the configured editor command for `stroke`.
    #[must_use]
    pub fn editor_command_for_key(
        &self,
        stroke: KeyStroke,
    ) -> Option<bmux_text_edit::TextEditCommand> {
        use bmux_text_edit::{TextDelete, TextEditCommand, TextMotion};

        Some(match self.action_for_key(BmuxScope::Chat, stroke)? {
            BmuxAction::InputNewLine => TextEditCommand::Insert('\n'),
            BmuxAction::EditorMoveLeft => TextEditCommand::Move(TextMotion::Left),
            BmuxAction::EditorMoveRight => TextEditCommand::Move(TextMotion::Right),
            BmuxAction::EditorMoveWordLeft => TextEditCommand::Move(TextMotion::WordLeft),
            BmuxAction::EditorMoveWordRight => TextEditCommand::Move(TextMotion::WordRight),
            BmuxAction::EditorMoveStart => TextEditCommand::Move(TextMotion::Start),
            BmuxAction::EditorMoveEnd => TextEditCommand::Move(TextMotion::End),
            BmuxAction::EditorDeleteBackward => TextEditCommand::Delete(TextDelete::Backward),
            BmuxAction::EditorDeleteForward => TextEditCommand::Delete(TextDelete::Forward),
            BmuxAction::EditorDeleteWordBackward => {
                TextEditCommand::Delete(TextDelete::WordBackward)
            }
            BmuxAction::EditorDeleteWordForward => TextEditCommand::Delete(TextDelete::WordForward),
            BmuxAction::EditorDeleteToStart => TextEditCommand::Delete(TextDelete::ToStart),
            BmuxAction::EditorDeleteToEnd => TextEditCommand::Delete(TextDelete::ToEnd),
            BmuxAction::InputSubmitSteering
            | BmuxAction::InputSubmitFollowUp
            | BmuxAction::InputHistoryPrevious
            | BmuxAction::InputHistoryNext
            | BmuxAction::AppExit
            | BmuxAction::AppInterrupt
            | BmuxAction::ClipboardPasteImage
            | BmuxAction::CommandPaletteOpen
            | BmuxAction::AgentCycle
            | BmuxAction::ThinkingEffortCycle
            | BmuxAction::TranscriptPageUp
            | BmuxAction::TranscriptPageDown
            | BmuxAction::TranscriptTop
            | BmuxAction::TranscriptBottom
            | BmuxAction::TranscriptLineUp
            | BmuxAction::TranscriptLineDown
            | BmuxAction::PermissionApprove
            | BmuxAction::PermissionDeny
            | BmuxAction::SelectUp
            | BmuxAction::SelectDown
            | BmuxAction::SelectConfirm
            | BmuxAction::SelectCancel
            | BmuxAction::SessionNew
            | BmuxAction::SessionRename
            | BmuxAction::SessionDelete
            | BmuxAction::EditorSelectLeft
            | BmuxAction::EditorSelectRight
            | BmuxAction::EditorSelectWordLeft
            | BmuxAction::EditorSelectWordRight
            | BmuxAction::EditorSelectUp
            | BmuxAction::EditorSelectDown
            | BmuxAction::SkillInvoke
            | BmuxAction::SkillActivate
            | BmuxAction::SkillDeactivate
            | BmuxAction::SkillHelp => return None,
        })
    }

    /// Return the configured selection motion for `stroke`.
    #[must_use]
    pub fn editor_selection_motion_for_key(
        &self,
        stroke: KeyStroke,
    ) -> Option<bmux_text_edit::TextMotion> {
        use bmux_text_edit::TextMotion;

        Some(match self.action_for_key(BmuxScope::Chat, stroke)? {
            BmuxAction::EditorSelectLeft => TextMotion::Left,
            BmuxAction::EditorSelectRight => TextMotion::Right,
            BmuxAction::EditorSelectWordLeft => TextMotion::WordLeft,
            BmuxAction::EditorSelectWordRight => TextMotion::WordRight,
            BmuxAction::EditorSelectUp => TextMotion::VisualUp,
            BmuxAction::EditorSelectDown => TextMotion::VisualDown,
            BmuxAction::InputSubmitSteering
            | BmuxAction::InputSubmitFollowUp
            | BmuxAction::InputHistoryPrevious
            | BmuxAction::InputHistoryNext
            | BmuxAction::AppExit
            | BmuxAction::AppInterrupt
            | BmuxAction::ClipboardPasteImage
            | BmuxAction::CommandPaletteOpen
            | BmuxAction::AgentCycle
            | BmuxAction::ThinkingEffortCycle
            | BmuxAction::TranscriptPageUp
            | BmuxAction::TranscriptPageDown
            | BmuxAction::TranscriptTop
            | BmuxAction::TranscriptBottom
            | BmuxAction::TranscriptLineUp
            | BmuxAction::TranscriptLineDown
            | BmuxAction::PermissionApprove
            | BmuxAction::PermissionDeny
            | BmuxAction::SelectUp
            | BmuxAction::SelectDown
            | BmuxAction::SelectConfirm
            | BmuxAction::SelectCancel
            | BmuxAction::SessionNew
            | BmuxAction::SessionRename
            | BmuxAction::SessionDelete
            | BmuxAction::InputNewLine
            | BmuxAction::EditorMoveLeft
            | BmuxAction::EditorMoveRight
            | BmuxAction::EditorMoveWordLeft
            | BmuxAction::EditorMoveWordRight
            | BmuxAction::EditorMoveStart
            | BmuxAction::EditorMoveEnd
            | BmuxAction::EditorDeleteBackward
            | BmuxAction::EditorDeleteForward
            | BmuxAction::EditorDeleteWordBackward
            | BmuxAction::EditorDeleteWordForward
            | BmuxAction::EditorDeleteToStart
            | BmuxAction::EditorDeleteToEnd
            | BmuxAction::SkillInvoke
            | BmuxAction::SkillActivate
            | BmuxAction::SkillDeactivate
            | BmuxAction::SkillHelp => return None,
        })
    }
}

fn key_label(stroke: KeyStroke) -> String {
    let mut parts = Vec::new();
    if stroke.modifiers.ctrl {
        parts.push("ctrl".to_owned());
    }
    if stroke.modifiers.alt {
        parts.push("alt".to_owned());
    }
    if stroke.modifiers.shift {
        parts.push("shift".to_owned());
    }
    if stroke.modifiers.super_key {
        parts.push("super".to_owned());
    }
    if stroke.modifiers.hyper {
        parts.push("hyper".to_owned());
    }
    if stroke.modifiers.meta {
        parts.push("meta".to_owned());
    }
    parts.push(match stroke.key {
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Escape => "escape".to_owned(),
        KeyCode::Space => "space".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageUp".to_owned(),
        KeyCode::PageDown => "pageDown".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::F(index) => format!("f{index}"),
    });
    parts.join("+")
}

fn default_bindings() -> BTreeMap<BmuxScope, Vec<BmuxKeyBinding>> {
    BTreeMap::from([
        (
            BmuxScope::Chat,
            vec![
                bind("enter", BmuxAction::InputSubmitSteering),
                bind("ctrl+shift+enter", BmuxAction::InputSubmitFollowUp),
                bind("shift+enter", BmuxAction::InputNewLine),
                bind("up", BmuxAction::InputHistoryPrevious),
                bind("down", BmuxAction::InputHistoryNext),
                bind("shift+left", BmuxAction::EditorSelectLeft),
                bind("shift+right", BmuxAction::EditorSelectRight),
                bind("shift+alt+left", BmuxAction::EditorSelectWordLeft),
                bind("shift+alt+right", BmuxAction::EditorSelectWordRight),
                bind("shift+ctrl+left", BmuxAction::EditorSelectWordLeft),
                bind("shift+ctrl+right", BmuxAction::EditorSelectWordRight),
                bind("shift+up", BmuxAction::EditorSelectUp),
                bind("shift+down", BmuxAction::EditorSelectDown),
                bind("ctrl+d", BmuxAction::AppExit),
                multi_tap_bind(
                    "escape",
                    BmuxAction::AppInterrupt,
                    2,
                    1_500,
                    "hit esc twice to cancel",
                ),
                bind("ctrl+v", BmuxAction::ClipboardPasteImage),
                bind("ctrl+p", BmuxAction::CommandPaletteOpen),
                bind("tab", BmuxAction::AgentCycle),
                bind("shift+tab", BmuxAction::ThinkingEffortCycle),
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
                bind("down", BmuxAction::SelectDown),
                bind("enter", BmuxAction::SelectConfirm),
                bind("ctrl+n", BmuxAction::SessionNew),
                bind("ctrl+r", BmuxAction::SessionRename),
                bind("ctrl+d", BmuxAction::SessionDelete),
                bind("escape", BmuxAction::SelectCancel),
                bind("ctrl+c", BmuxAction::SelectCancel),
            ],
        ),
        (
            BmuxScope::SkillPicker,
            vec![
                bind("up", BmuxAction::SelectUp),
                bind("k", BmuxAction::SelectUp),
                bind("down", BmuxAction::SelectDown),
                bind("j", BmuxAction::SelectDown),
                bind("enter", BmuxAction::SkillInvoke),
                bind("a", BmuxAction::SkillActivate),
                bind("d", BmuxAction::SkillDeactivate),
                bind("?", BmuxAction::SkillHelp),
                bind("escape", BmuxAction::SelectCancel),
                bind("ctrl+c", BmuxAction::SelectCancel),
            ],
        ),
    ])
}

fn apply_scope(
    bindings: &mut BTreeMap<BmuxScope, Vec<BmuxKeyBinding>>,
    scope: BmuxScope,
    configured: &BTreeMap<String, String>,
) {
    let Some(scope_bindings) = bindings.get_mut(&scope) else {
        return;
    };
    let configured_bindings = configured
        .iter()
        .filter_map(|(key, action_id)| {
            BmuxAction::from_id(action_id)
                .and_then(|action| parse_key(key).map(|stroke| binding(stroke, action)))
        })
        .collect::<Vec<_>>();
    if configured_bindings.is_empty() {
        return;
    }

    scope_bindings.retain(|existing| {
        !configured_bindings.iter().any(|configured| {
            existing.action == configured.action || existing.stroke == configured.stroke
        })
    });
    scope_bindings.extend(configured_bindings);
}

fn bind(key: &str, action: BmuxAction) -> BmuxKeyBinding {
    binding(
        parse_key(key).expect("default BMUX key binding must parse"),
        action,
    )
}

fn multi_tap_bind(
    key: &str,
    action: BmuxAction,
    required_taps: u8,
    window_ms: u64,
    prompt: &str,
) -> BmuxKeyBinding {
    BmuxKeyBinding::new(
        parse_key(key).expect("default BMUX key binding must parse"),
        action,
        BmuxKeyActivation::MultiTap {
            required_taps,
            window_ms,
            prompt: prompt.to_owned(),
        },
    )
}

const fn binding(stroke: KeyStroke, action: BmuxAction) -> BmuxKeyBinding {
    BmuxKeyBinding::new(stroke, action, BmuxKeyActivation::immediate())
}

fn parse_key(input: &str) -> Option<KeyStroke> {
    parse_key_stroke(input).ok()
}

#[cfg(test)]
mod tests {
    use bcode_config::TuiConfig;
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

    use super::{BmuxAction, BmuxKeyMap, BmuxScope};

    fn default_keymap() -> BmuxKeyMap {
        BmuxKeyMap::from_config(&TuiConfig::default())
    }

    #[test]
    fn session_picker_plain_alphanumerics_are_text_not_actions() {
        let keymap = default_keymap();

        for key in '0'..='9' {
            assert_eq!(
                keymap.action_for_key(
                    BmuxScope::SessionPicker,
                    KeyStroke::simple(KeyCode::Char(key))
                ),
                None,
                "plain {key} should not be bound in the session picker"
            );
        }
        for key in 'a'..='z' {
            assert_eq!(
                keymap.action_for_key(
                    BmuxScope::SessionPicker,
                    KeyStroke::simple(KeyCode::Char(key))
                ),
                None,
                "plain {key} should not be bound in the session picker"
            );
        }
        for key in 'A'..='Z' {
            assert_eq!(
                keymap.action_for_key(
                    BmuxScope::SessionPicker,
                    KeyStroke::simple(KeyCode::Char(key))
                ),
                None,
                "plain {key} should not be bound in the session picker"
            );
        }
    }

    #[test]
    fn session_picker_ctrl_n_creates_new_session() {
        let keymap = default_keymap();
        let action = keymap.action_for_key(
            BmuxScope::SessionPicker,
            KeyStroke::with_modifiers(
                KeyCode::Char('n'),
                Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                },
            ),
        );

        assert_eq!(action, Some(BmuxAction::SessionNew));
    }
}
