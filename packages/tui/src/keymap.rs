//! TUI keymap adapter.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use bcode_config::TuiConfig;
use bmux_keyboard::{KeyStroke, parse_key_stroke};

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

impl BmuxScope {
    /// Registry input context.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Permission => "permission",
            Self::SessionPicker => "session_picker",
            Self::SkillPicker => "skill_picker",
        }
    }
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
    TranscriptCopySelection,
    /// Explicitly clear transcript selection without copying.
    TranscriptClearSelection,
    CommandPaletteOpen,
    SessionSearchOpen,
    AgentCycle,
    ThinkingEffortCycle,
    DiffViewerLayoutCycle,
    MarkdownFocusNext,
    MarkdownFocusPrevious,
    MarkdownActivate,
    MarkdownCopyDestination,
    TranscriptPageUp,
    TranscriptPageDown,
    TranscriptTop,
    TranscriptBottom,
    TranscriptLineUp,
    TranscriptLineDown,
    InteractionFocusActive,
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
    /// Stable registry identity for this built-in action.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::InputSubmitSteering => "tui.input.submitSteering",
            Self::InputSubmitFollowUp => "tui.input.submitFollowUp",
            Self::InputHistoryPrevious => "tui.input.historyPrevious",
            Self::InputHistoryNext => "tui.input.historyNext",
            Self::AppExit => "app.exit",
            Self::AppInterrupt => "app.interrupt",
            Self::ClipboardPasteImage => "app.clipboard.pasteImage",
            Self::TranscriptCopySelection => "tui.transcript.copySelection",
            Self::TranscriptClearSelection => "tui.transcript.clearSelection",
            Self::CommandPaletteOpen => "app.command_palette",
            Self::SessionSearchOpen => "tui.session.search",
            Self::AgentCycle => "tui.agent.cycle",
            Self::ThinkingEffortCycle => "tui.thinking.effort.cycle",
            Self::DiffViewerLayoutCycle => "tui.diff_viewer.layout_cycle",
            Self::MarkdownFocusNext => "tui.markdown.focusNext",
            Self::MarkdownFocusPrevious => "tui.markdown.focusPrevious",
            Self::MarkdownActivate => "tui.markdown.activate",
            Self::MarkdownCopyDestination => "tui.markdown.copyDestination",
            Self::TranscriptPageUp => "transcript.pageUp",
            Self::TranscriptPageDown => "transcript.pageDown",
            Self::TranscriptTop => "transcript.top",
            Self::TranscriptBottom => "transcript.bottom",
            Self::TranscriptLineUp => "transcript.lineUp",
            Self::TranscriptLineDown => "transcript.lineDown",
            Self::InteractionFocusActive => "tui.interaction.focusActive",
            Self::PermissionApprove => "app.permission.approve",
            Self::PermissionDeny => "app.permission.deny",
            Self::SelectUp => "tui.select.previous",
            Self::SelectDown => "tui.select.next",
            Self::SelectConfirm => "tui.select.confirm",
            Self::SelectCancel => "tui.select.cancel",
            Self::SessionNew => "tui.session.new",
            Self::SessionRename => "tui.session.rename",
            Self::SessionDelete => "tui.session.delete",
            Self::InputNewLine => "tui.input.newline",
            Self::EditorMoveLeft => "tui.editor.moveCursorLeft",
            Self::EditorMoveRight => "tui.editor.moveCursorRight",
            Self::EditorMoveWordLeft => "tui.editor.moveCursorWordLeft",
            Self::EditorMoveWordRight => "tui.editor.moveCursorWordRight",
            Self::EditorMoveStart => "tui.editor.moveCursorStart",
            Self::EditorMoveEnd => "tui.editor.moveCursorEnd",
            Self::EditorSelectLeft => "tui.editor.selectCursorLeft",
            Self::EditorSelectRight => "tui.editor.selectCursorRight",
            Self::EditorSelectWordLeft => "tui.editor.selectCursorWordLeft",
            Self::EditorSelectWordRight => "tui.editor.selectCursorWordRight",
            Self::EditorSelectUp => "tui.editor.selectCursorUp",
            Self::EditorSelectDown => "tui.editor.selectCursorDown",
            Self::EditorDeleteBackward => "tui.editor.deleteCharBackward",
            Self::EditorDeleteForward => "tui.editor.deleteCharForward",
            Self::EditorDeleteWordBackward => "tui.editor.deleteWordBackward",
            Self::EditorDeleteWordForward => "tui.editor.deleteWordForward",
            Self::EditorDeleteToStart => "tui.editor.deleteToStart",
            Self::EditorDeleteToEnd => "tui.editor.deleteToEnd",
            Self::SkillInvoke => "tui.skill.invoke",
            Self::SkillActivate => "tui.skill.activate",
            Self::SkillDeactivate => "tui.skill.deactivate",
            Self::SkillHelp => "tui.skill.help",
        }
    }

    /// User-facing action description, independent of its shortcut.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::InputSubmitSteering => "Input Submit Steering",
            Self::InputSubmitFollowUp => "Input Submit Follow Up",
            Self::InputHistoryPrevious => "Input History Previous",
            Self::InputHistoryNext => "Input History Next",
            Self::AppExit => "App Exit",
            Self::AppInterrupt => "App Interrupt",
            Self::ClipboardPasteImage => "Clipboard Paste Image",
            Self::TranscriptCopySelection => "Transcript Copy Selection",
            Self::TranscriptClearSelection => "Transcript Clear Selection",
            Self::CommandPaletteOpen => "Command Palette Open",
            Self::SessionSearchOpen => "Session Search Open",
            Self::AgentCycle => "Agent Cycle",
            Self::ThinkingEffortCycle => "Thinking Effort Cycle",
            Self::DiffViewerLayoutCycle => "Diff Viewer Layout Cycle",
            Self::MarkdownFocusNext => "Markdown Focus Next",
            Self::MarkdownFocusPrevious => "Markdown Focus Previous",
            Self::MarkdownActivate => "Markdown Activate",
            Self::MarkdownCopyDestination => "Markdown Copy Destination",
            Self::TranscriptPageUp => "Transcript Page Up",
            Self::TranscriptPageDown => "Transcript Page Down",
            Self::TranscriptTop => "Transcript Top",
            Self::TranscriptBottom => "Transcript Bottom",
            Self::TranscriptLineUp => "Transcript Line Up",
            Self::TranscriptLineDown => "Transcript Line Down",
            Self::InteractionFocusActive => "Interaction Focus Active",
            Self::PermissionApprove => "Permission Approve",
            Self::PermissionDeny => "Permission Deny",
            Self::SelectUp => "Select Up",
            Self::SelectDown => "Select Down",
            Self::SelectConfirm => "Select Confirm",
            Self::SelectCancel => "Select Cancel",
            Self::SessionNew => "Session New",
            Self::SessionRename => "Session Rename",
            Self::SessionDelete => "Session Delete",
            Self::InputNewLine => "Input New Line",
            Self::EditorMoveLeft => "Editor Move Left",
            Self::EditorMoveRight => "Editor Move Right",
            Self::EditorMoveWordLeft => "Editor Move Word Left",
            Self::EditorMoveWordRight => "Editor Move Word Right",
            Self::EditorMoveStart => "Editor Move Start",
            Self::EditorMoveEnd => "Editor Move End",
            Self::EditorSelectLeft => "Editor Select Left",
            Self::EditorSelectRight => "Editor Select Right",
            Self::EditorSelectWordLeft => "Editor Select Word Left",
            Self::EditorSelectWordRight => "Editor Select Word Right",
            Self::EditorSelectUp => "Editor Select Up",
            Self::EditorSelectDown => "Editor Select Down",
            Self::EditorDeleteBackward => "Editor Delete Backward",
            Self::EditorDeleteForward => "Editor Delete Forward",
            Self::EditorDeleteWordBackward => "Editor Delete Word Backward",
            Self::EditorDeleteWordForward => "Editor Delete Word Forward",
            Self::EditorDeleteToStart => "Editor Delete To Start",
            Self::EditorDeleteToEnd => "Editor Delete To End",
            Self::SkillInvoke => "Skill Invoke",
            Self::SkillActivate => "Skill Activate",
            Self::SkillDeactivate => "Skill Deactivate",
            Self::SkillHelp => "Skill Help",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "tui.input.submit" | "tui.input.submitSteering" => Self::InputSubmitSteering,
            "tui.input.submitFollowUp" => Self::InputSubmitFollowUp,
            "tui.input.historyPrevious" => Self::InputHistoryPrevious,
            "tui.input.historyNext" => Self::InputHistoryNext,
            "app.exit" => Self::AppExit,
            "app.interrupt" => Self::AppInterrupt,
            "app.clipboard.pasteImage" => Self::ClipboardPasteImage,
            "tui.transcript.copySelection" => Self::TranscriptCopySelection,
            "tui.transcript.clearSelection" => Self::TranscriptClearSelection,
            "app.command_palette" => Self::CommandPaletteOpen,
            "tui.session.search" => Self::SessionSearchOpen,
            "tui.agent.cycle" => Self::AgentCycle,
            "tui.thinking.effort.cycle" => Self::ThinkingEffortCycle,
            "tui.diff_viewer.layout_cycle" => Self::DiffViewerLayoutCycle,
            "tui.markdown.focusNext" => Self::MarkdownFocusNext,
            "tui.markdown.focusPrevious" => Self::MarkdownFocusPrevious,
            "tui.markdown.activate" => Self::MarkdownActivate,
            "tui.markdown.copyDestination" => Self::MarkdownCopyDestination,
            "transcript.pageUp" => Self::TranscriptPageUp,
            "transcript.pageDown" => Self::TranscriptPageDown,
            "transcript.top" => Self::TranscriptTop,
            "transcript.bottom" => Self::TranscriptBottom,
            "transcript.lineUp" => Self::TranscriptLineUp,
            "transcript.lineDown" => Self::TranscriptLineDown,
            "tui.interaction.focusActive" => Self::InteractionFocusActive,
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
    key: String,
    stroke: KeyStroke,
    action: BmuxAction,
    activation: BmuxKeyActivation,
}

impl BmuxKeyBinding {
    /// Create a key binding.
    #[must_use]
    pub const fn new(stroke: KeyStroke, action: BmuxAction, activation: BmuxKeyActivation) -> Self {
        Self {
            key: String::new(),
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
    error: Option<String>,
    registry: bcode_plugin_sdk::tui::keymap::KeymapRegistry,
    activations: BTreeMap<(BmuxScope, BmuxAction), BmuxKeyActivation>,
}

impl BmuxKeyMap {
    /// Build a keymap from TUI config.
    #[must_use]
    pub fn from_config(config: &TuiConfig) -> Self {
        Self::try_from_config(config).unwrap_or_else(|error| Self {
            registry: bcode_plugin_sdk::tui::keymap::KeymapRegistry::default(),
            activations: BTreeMap::new(),
            error: Some(error.to_string()),
        })
    }

    /// Compile contributor defaults and configuration without silently ignoring invalid keys.
    ///
    /// # Errors
    /// Returns invalid key configuration or conflicting action registration.
    pub fn try_from_config(
        config: &TuiConfig,
    ) -> Result<Self, bcode_plugin_sdk::tui::keymap::KeymapError> {
        use bcode_plugin_sdk::tui::keymap::{KeymapAction, KeymapRegistry};
        let mut registry = KeymapRegistry::default();
        let mut activations = BTreeMap::new();
        for (scope, bindings) in default_bindings() {
            let mut actions: BTreeMap<BmuxAction, Vec<String>> = BTreeMap::new();
            for binding in bindings {
                actions
                    .entry(binding.action)
                    .or_default()
                    .push(binding.key.clone());
                activations.insert((scope, binding.action), binding.activation);
            }
            for (action, defaults) in actions {
                registry.register(KeymapAction {
                    id: action.id().into(),
                    scope: scope.id().into(),
                    description: action.description().into(),
                    defaults,
                })?;
            }
        }
        let mut overrides = config.keybindings.scopes.clone();
        for (scope, configured) in [
            (BmuxScope::Chat, &config.keybindings.chat),
            (BmuxScope::Permission, &config.keybindings.permission),
            (BmuxScope::SessionPicker, &config.keybindings.session_picker),
            (BmuxScope::SkillPicker, &config.keybindings.skill_picker),
        ] {
            let entries = overrides.entry(scope.id().into()).or_default();
            entries.extend(configured.iter().map(|(key, id)| {
                (
                    key.clone(),
                    BmuxAction::from_id(id).map_or_else(|| id.clone(), |action| action.id().into()),
                )
            }));
        }
        for (scope, entries) in &overrides {
            for id in entries.values() {
                if let Some(action) = BmuxAction::from_id(id) {
                    for builtin_scope in [
                        BmuxScope::Chat,
                        BmuxScope::Permission,
                        BmuxScope::SessionPicker,
                        BmuxScope::SkillPicker,
                    ] {
                        if builtin_scope.id() == scope {
                            activations
                                .insert((builtin_scope, action), BmuxKeyActivation::Immediate);
                        }
                    }
                }
            }
        }
        registry.configure(overrides)?;
        Ok(Self {
            error: None,
            registry,
            activations,
        })
    }

    /// Configuration failures disable action dispatch rather than reverting to defaults.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Effective registry shared with plugin input and shortcut presentation.
    #[must_use]
    pub const fn registry(&self) -> &bcode_plugin_sdk::tui::keymap::KeymapRegistry {
        &self.registry
    }

    /// Register a contributor-owned action without extending the built-in action enum.
    ///
    /// # Errors
    /// Returns invalid or conflicting action definitions.
    pub fn register(
        &mut self,
        action: bcode_plugin_sdk::tui::keymap::KeymapAction,
    ) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
        self.registry.register(action)
    }

    /// Render help from registered descriptions and effective bindings, including unbound actions.
    #[must_use]
    pub fn help_markdown(&self) -> String {
        let mut help = String::from("# TUI keybindings\n\n");
        for action in self.registry.actions() {
            let keys = self.registry.keys_for_action(&action.scope, &action.id);
            let keys = if keys.is_empty() {
                "unbound".to_owned()
            } else {
                keys.join(", ")
            };
            let _ = writeln!(
                help,
                "* **{}** — {}: {} (`{}`)",
                action.scope, action.description, keys, action.id
            );
        }
        if let Some(error) = self.error() {
            let _ = writeln!(help, "\nKeymap unavailable: {error}");
        }
        help
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
        let action = BmuxAction::from_id(self.registry.action_for_key(scope.id(), stroke)?)?;
        let key = self
            .registry
            .keys_for_action(scope.id(), action.id())
            .into_iter()
            .find(|key| parse_key(key) == Some(stroke))?
            .to_owned();
        let mut activation = self
            .activations
            .get(&(scope, action))
            .cloned()
            .unwrap_or(BmuxKeyActivation::Immediate);
        if let BmuxKeyActivation::MultiTap { prompt, .. } = &mut activation {
            *prompt = prompt.replace("<binding>", &key);
        }
        Some(BmuxKeyBinding {
            key,
            stroke,
            action,
            activation,
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
            BmuxAction::TranscriptClearSelection
            | BmuxAction::InputSubmitSteering
            | BmuxAction::InputSubmitFollowUp
            | BmuxAction::InputHistoryPrevious
            | BmuxAction::InputHistoryNext
            | BmuxAction::AppExit
            | BmuxAction::AppInterrupt
            | BmuxAction::ClipboardPasteImage
            | BmuxAction::TranscriptCopySelection
            | BmuxAction::CommandPaletteOpen
            | BmuxAction::SessionSearchOpen
            | BmuxAction::AgentCycle
            | BmuxAction::ThinkingEffortCycle
            | BmuxAction::DiffViewerLayoutCycle
            | BmuxAction::MarkdownFocusNext
            | BmuxAction::MarkdownFocusPrevious
            | BmuxAction::MarkdownActivate
            | BmuxAction::MarkdownCopyDestination
            | BmuxAction::TranscriptPageUp
            | BmuxAction::TranscriptPageDown
            | BmuxAction::TranscriptTop
            | BmuxAction::TranscriptBottom
            | BmuxAction::TranscriptLineUp
            | BmuxAction::TranscriptLineDown
            | BmuxAction::InteractionFocusActive
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
            BmuxAction::TranscriptClearSelection
            | BmuxAction::InputSubmitSteering
            | BmuxAction::InputSubmitFollowUp
            | BmuxAction::InputHistoryPrevious
            | BmuxAction::InputHistoryNext
            | BmuxAction::AppExit
            | BmuxAction::AppInterrupt
            | BmuxAction::ClipboardPasteImage
            | BmuxAction::TranscriptCopySelection
            | BmuxAction::CommandPaletteOpen
            | BmuxAction::SessionSearchOpen
            | BmuxAction::AgentCycle
            | BmuxAction::ThinkingEffortCycle
            | BmuxAction::DiffViewerLayoutCycle
            | BmuxAction::MarkdownFocusNext
            | BmuxAction::MarkdownFocusPrevious
            | BmuxAction::MarkdownActivate
            | BmuxAction::MarkdownCopyDestination
            | BmuxAction::TranscriptPageUp
            | BmuxAction::TranscriptPageDown
            | BmuxAction::TranscriptTop
            | BmuxAction::TranscriptBottom
            | BmuxAction::TranscriptLineUp
            | BmuxAction::TranscriptLineDown
            | BmuxAction::InteractionFocusActive
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
                    "repeat <binding> to cancel",
                ),
                bind("ctrl+v", BmuxAction::ClipboardPasteImage),
                bind("ctrl+shift+c", BmuxAction::TranscriptCopySelection),
                bind("ctrl+p", BmuxAction::CommandPaletteOpen),
                bind("ctrl+g", BmuxAction::SessionSearchOpen),
                bind("tab", BmuxAction::AgentCycle),
                bind("shift+tab", BmuxAction::ThinkingEffortCycle),
                bind("ctrl+tab", BmuxAction::MarkdownFocusNext),
                bind("ctrl+shift+tab", BmuxAction::MarkdownFocusPrevious),
                bind("alt+enter", BmuxAction::MarkdownActivate),
                bind("ctrl+alt+c", BmuxAction::MarkdownCopyDestination),
                bind("pageUp", BmuxAction::TranscriptPageUp),
                bind("pageDown", BmuxAction::TranscriptPageDown),
                bind("ctrl+home", BmuxAction::TranscriptTop),
                bind("ctrl+end", BmuxAction::TranscriptBottom),
                bind("ctrl+up", BmuxAction::TranscriptLineUp),
                bind("ctrl+down", BmuxAction::TranscriptLineDown),
                bind("ctrl+i", BmuxAction::InteractionFocusActive),
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

fn bind(key: &str, action: BmuxAction) -> BmuxKeyBinding {
    let mut binding = binding(
        parse_key(key).expect("default BMUX key binding must parse"),
        action,
    );
    binding.key = key.into();
    binding
}

fn multi_tap_bind(
    key: &str,
    action: BmuxAction,
    required_taps: u8,
    window_ms: u64,
    prompt: &str,
) -> BmuxKeyBinding {
    let mut binding = BmuxKeyBinding::new(
        parse_key(key).expect("default BMUX key binding must parse"),
        action,
        BmuxKeyActivation::MultiTap {
            required_taps,
            window_ms,
            prompt: prompt.to_owned(),
        },
    );
    binding.key = key.into();
    binding
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
    fn global_session_search_binding_is_discoverable_and_configurable() {
        let keymap = default_keymap();
        let default_stroke = KeyStroke::with_modifiers(
            KeyCode::Char('g'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(
            keymap.action_for_key(BmuxScope::Chat, default_stroke),
            Some(BmuxAction::SessionSearchOpen)
        );

        let mut config = TuiConfig::default();
        config
            .keybindings
            .chat
            .insert("ctrl+s".to_owned(), "tui.session.search".to_owned());
        let configured = BmuxKeyMap::from_config(&config);
        assert_eq!(
            configured.action_for_key(
                BmuxScope::Chat,
                KeyStroke::with_modifiers(
                    KeyCode::Char('s'),
                    Modifiers {
                        ctrl: true,
                        ..Modifiers::NONE
                    }
                )
            ),
            Some(BmuxAction::SessionSearchOpen)
        );
        assert_eq!(
            configured.action_for_key(BmuxScope::Chat, default_stroke),
            None
        );
    }

    #[test]
    fn invalid_configuration_fails_closed() {
        let mut config = TuiConfig::default();
        config
            .keybindings
            .chat
            .insert("not-a-key".into(), "app.exit".into());
        assert!(BmuxKeyMap::try_from_config(&config).is_err());
        let keymap = BmuxKeyMap::from_config(&config);
        assert!(keymap.error().is_some());
        assert_eq!(
            keymap.action_for_key(BmuxScope::Chat, KeyStroke::simple(KeyCode::Enter)),
            None
        );
    }

    #[test]
    fn skill_overrides_are_independent_from_session_picker() {
        let mut config = TuiConfig::default();
        config
            .keybindings
            .skill_picker
            .insert("ctrl+i".into(), "tui.skill.invoke".into());
        let keymap = BmuxKeyMap::from_config(&config);
        assert_eq!(
            keymap.action_for_key(BmuxScope::SkillPicker, KeyStroke::simple(KeyCode::Enter)),
            None
        );
        assert_eq!(
            keymap.action_for_key(BmuxScope::SessionPicker, KeyStroke::simple(KeyCode::Enter)),
            Some(BmuxAction::SelectConfirm)
        );
        assert_eq!(
            keymap
                .registry()
                .keys_for_action("skill_picker", "tui.skill.invoke"),
            ["ctrl+i"]
        );
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
    fn markdown_actions_have_stable_default_bindings() {
        let keymap = default_keymap();
        for (stroke, action) in [
            (
                KeyStroke::with_modifiers(
                    KeyCode::Tab,
                    Modifiers {
                        ctrl: true,
                        ..Modifiers::NONE
                    },
                ),
                BmuxAction::MarkdownFocusNext,
            ),
            (
                KeyStroke::with_modifiers(
                    KeyCode::Tab,
                    Modifiers {
                        ctrl: true,
                        shift: true,
                        ..Modifiers::NONE
                    },
                ),
                BmuxAction::MarkdownFocusPrevious,
            ),
            (
                KeyStroke::with_modifiers(
                    KeyCode::Enter,
                    Modifiers {
                        alt: true,
                        ..Modifiers::NONE
                    },
                ),
                BmuxAction::MarkdownActivate,
            ),
            (
                KeyStroke::with_modifiers(
                    KeyCode::Char('c'),
                    Modifiers {
                        ctrl: true,
                        alt: true,
                        ..Modifiers::NONE
                    },
                ),
                BmuxAction::MarkdownCopyDestination,
            ),
        ] {
            assert_eq!(keymap.action_for_key(BmuxScope::Chat, stroke), Some(action));
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
