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
    /// Undo the most recent composer edit.
    EditorUndo,
    /// Redo the most recently undone composer edit.
    EditorRedo,
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
            Self::EditorUndo => "tui.editor.undo",
            Self::EditorRedo => "tui.editor.redo",
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
            Self::EditorUndo => "Editor Undo",
            Self::EditorRedo => "Editor Redo",
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
            "tui.editor.undo" => Self::EditorUndo,
            "tui.editor.redo" => Self::EditorRedo,
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

/// Register host-owned dialog actions. Defaults are declarations, never dispatch fallbacks.
fn register_host_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    register_control_actions(registry)?;
    register_model_actions(registry)?;
    register_picker_actions(registry)?;
    register_streaming_actions(registry)?;
    register_form_editor_actions(registry)?;
    register_onboarding_actions(registry)?;
    Ok(())
}
fn control_defaults<'a>(scope: &str, id: &str, defaults: Vec<&'a str>) -> Option<Vec<&'a str>> {
    if scope == "composer_interaction"
        && !matches!(
            id,
            "tui.control.cancel"
                | "tui.control.submit"
                | "tui.control.up"
                | "tui.control.down"
                | "tui.control.focus_next"
        )
    {
        return None;
    }
    if id == "tui.control.toggle"
        && matches!(
            scope,
            "setup_connection"
                | "setup_settings"
                | "setup_auth"
                | "setup_auth_terminal"
                | "provider_picker"
                | "model_picker"
                | "command_palette"
                | "working_directory"
                | "worktree_create"
                | "ralph_start"
                | "session_rename"
                | "session_search"
                | "slash_palette"
        )
    {
        return None;
    }
    if scope == "session_picker" && id == "tui.control.focus_next" {
        return None;
    }
    let mut defaults = defaults;
    if scope == "setup" && id == "tui.control.focus_next" {
        defaults.push("n");
    }
    if scope == "setup" && id == "tui.control.focus_previous" {
        defaults.push("p");
    }

    if matches!(scope, "onboarding" | "quick_onboarding") && id == "tui.control.cancel" {
        defaults.push("q");
    }
    if scope == "onboarding" && id == "tui.control.up" {
        defaults.push("k");
    }
    if scope == "onboarding" && id == "tui.control.down" {
        defaults.push("j");
    }

    if scope == "session_delete" && id == "tui.control.cancel" {
        defaults.extend(["n", "shift+n"]);
    }
    if scope == "session_delete" && id == "tui.control.submit" {
        defaults = vec!["y", "shift+y"];
    }

    if matches!(scope, "theme_picker" | "timeline") && id == "tui.control.up" {
        defaults.push("k");
    }
    if matches!(scope, "theme_picker" | "timeline") && id == "tui.control.down" {
        defaults.push("j");
    }
    Some(defaults)
}

fn register_control_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;

    for scope in [
        "composer_interaction",
        "provider_picker",
        "model_picker_actions",
        "model_picker",
        "auth_pool_picker",
        "worktree_picker",
        "theme_picker",
        "timeline",
        "slash_palette",
        "command_palette",
        "thinking_dialog",
        "setup",
        "onboarding",
        "quick_onboarding",
        "working_directory",
        "worktree_create",
        "ralph_start",
        "streaming_configurator",
        "monitor",
        "visual",
        "session_rename",
        "session_delete",
        "session_search",
        "setup_connection",
        "setup_settings",
        "setup_auth",
        "setup_auth_terminal",
        "setup_device_login",
    ] {
        for (id, description, defaults) in [
            ("tui.control.cancel", "Cancel", vec!["escape"]),
            ("tui.control.submit", "Confirm", vec!["enter"]),
            ("tui.control.up", "Previous item", vec!["up"]),
            ("tui.control.down", "Next item", vec!["down"]),
            ("tui.control.left", "Previous value", vec!["left"]),
            ("tui.control.right", "Next value", vec!["right"]),
            ("tui.control.focus_next", "Next field", vec!["tab"]),
            (
                "tui.control.focus_previous",
                "Previous field",
                vec!["shift+tab"],
            ),
            ("tui.control.toggle", "Toggle", vec!["space"]),
            ("tui.control.page_up", "Previous page", vec!["pageup"]),
            ("tui.control.page_down", "Next page", vec!["pagedown"]),
            ("tui.control.home", "First item", vec!["home"]),
            ("tui.control.end", "Last item", vec!["end"]),
        ] {
            let Some(defaults) = control_defaults(scope, id, defaults) else {
                continue;
            };
            registry.register(KeymapAction {
                id: id.into(),
                scope: scope.into(),
                description: description.into(),
                defaults: defaults.into_iter().map(str::to_owned).collect(),
            })?;
        }
    }

    for (id, description, key) in [
        ("tui.timeline.milestones", "Read milestones", "m"),
        ("tui.timeline.prompts", "Show user prompts", "p"),
        ("tui.timeline.nextMilestonePage", "Next milestone page", "n"),
    ] {
        registry.register(KeymapAction {
            id: id.into(),
            scope: "timeline".into(),
            description: description.into(),
            defaults: vec![key.into()],
        })?;
    }
    Ok(())
}
fn register_model_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;
    for (id, description, defaults) in [
        (
            "tui.model.check",
            "Check selected model (may consume quota/cost)",
            vec!["c"],
        ),
        ("tui.model.filter", "Filter models", vec!["/"]),
        (
            "tui.model.showIgnored",
            "Show ignored models",
            vec!["shift+i"],
        ),
        ("tui.model.sort", "Cycle model sort", vec!["s"]),
        (
            "tui.model.reverseSort",
            "Reverse model sort",
            vec!["shift+s"],
        ),
        ("tui.model.ignore", "Ignore model", vec!["i"]),
        ("tui.model.unignore", "Unignore model", vec!["u"]),
    ] {
        registry.register(KeymapAction {
            id: id.into(),
            scope: "model_picker_actions".into(),
            description: description.into(),
            defaults: defaults.into_iter().map(str::to_owned).collect(),
        })?;
    }

    Ok(())
}
fn register_monitor_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;
    for (scope, id, description, defaults) in [
        ("chat", "tui.session.search", "Session Search Open", vec![]),
        (
            "auth_pool_picker",
            "tui.auth.clear",
            "Follow configuration",
            vec!["c"],
        ),
        (
            "auth_pool_picker",
            "tui.auth.pause",
            "Pause or resume account",
            vec!["p"],
        ),
        (
            "auth_pool_picker",
            "tui.auth.rename",
            "Rename account",
            vec!["r"],
        ),
        (
            "auth_pool_picker",
            "tui.auth.details",
            "Account details",
            vec!["d"],
        ),
        (
            "chat",
            "tui.monitor.toggle",
            "Toggle child monitor",
            vec!["ctrl+g"],
        ),
        (
            "monitor",
            "tui.monitor.nextPage",
            "Load next child page",
            vec!["alt+n"],
        ),
    ] {
        registry.register(KeymapAction {
            id: id.into(),
            scope: scope.into(),
            description: description.into(),
            defaults: defaults.into_iter().map(str::to_owned).collect(),
        })?;
    }
    Ok(())
}

fn register_picker_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;
    register_monitor_actions(registry)?;
    for (scope, id, description, defaults) in [
        (
            "session_picker",
            "tui.control.left",
            "Select parent",
            vec!["left"],
        ),
        (
            "session_picker",
            "tui.control.right",
            "Expand children",
            vec!["right"],
        ),
        (
            "session_search",
            "tui.search.mode",
            "Cycle search mode",
            vec!["alt+m"],
        ),
        (
            "session_search",
            "tui.search.deep",
            "Toggle deep search",
            vec!["alt+d"],
        ),
        (
            "session_search",
            "tui.search.content",
            "Cycle search content",
            vec!["alt+s"],
        ),
        (
            "session_search",
            "tui.search.nextMatch",
            "Next match",
            vec!["alt+n"],
        ),
        (
            "session_search",
            "tui.search.index",
            "Index search",
            vec!["alt+i"],
        ),
        (
            "session_search",
            "tui.search.migrate",
            "Migrate session",
            vec!["alt+g"],
        ),
        (
            "session_search",
            "tui.search.backfill",
            "Backfill search",
            vec!["alt+b"],
        ),
        (
            "session_search",
            "tui.search.cancelMaintenance",
            "Cancel maintenance",
            vec!["alt+x"],
        ),
        (
            "session_search",
            "tui.search.help",
            "Search syntax",
            vec!["?"],
        ),
    ] {
        registry.register(KeymapAction {
            id: id.into(),
            scope: scope.into(),
            description: description.into(),
            defaults: defaults.into_iter().map(str::to_owned).collect(),
        })?;
    }

    Ok(())
}
fn register_streaming_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;
    for (id, description, key) in [
        ("tui.streaming.restart", "Restart preview", "r"),
        ("tui.streaming.pause", "Pause preview", "p"),
        (
            "tui.streaming.coarseLeft",
            "Decrease coarsely",
            "shift+left",
        ),
        (
            "tui.streaming.coarseRight",
            "Increase coarsely",
            "shift+right",
        ),
    ] {
        registry.register(KeymapAction {
            id: id.into(),
            scope: "streaming_configurator".into(),
            description: description.into(),
            defaults: vec![key.into()],
        })?;
    }

    Ok(())
}
fn register_form_editor_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;
    for scope in [
        "setup_connection",
        "setup_settings",
        "setup_auth",
        "provider_picker",
        "model_picker",
        "command_palette",
        "working_directory",
        "worktree_create",
        "ralph_start",
        "session_rename",
        "session_search",
    ] {
        use bcode_plugin_sdk::tui::keymap::controls::ControlAction;
        for action in ControlAction::TEXT {
            if !matches!(
                action,
                ControlAction::Up
                    | ControlAction::Down
                    | ControlAction::Left
                    | ControlAction::Right
                    | ControlAction::Home
                    | ControlAction::End
            ) {
                let mut definition = action.definition(scope);
                // Transcript maintenance owns Alt-B in the search form.
                if scope == "session_search" && *action == ControlAction::WordLeft {
                    definition.defaults.retain(|key| key != "alt+b");
                }
                registry.register(definition)?;
            }
        }
    }
    registry.register(KeymapAction {
        id: "tui.setup.advanced".into(),
        scope: "setup_connection".into(),
        description: "Toggle advanced fields".into(),
        defaults: vec!["f2".into()],
    })?;

    Ok(())
}
fn register_onboarding_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;
    for (id, description, defaults) in [
        ("tui.quick.connect", "Connect another account", vec!["c"]),
        ("tui.quick.advanced", "Open advanced setup", vec!["f2"]),
    ] {
        registry.register(KeymapAction {
            id: id.into(),
            scope: "quick_onboarding".into(),
            description: description.into(),
            defaults: defaults.into_iter().map(str::to_owned).collect(),
        })?;
    }
    for (action, description, key) in [
        ("provider", "Connect provider", "p"),
        ("auth", "Review authentication", "a"),
        ("model", "Select model", "m"),
        ("permissions", "Review permissions", "r"),
        ("import", "Review import", "i"),
        ("plugins", "Review plugins", "g"),
        ("apply", "Apply plan", "x"),
        ("confirm", "Confirm plan", "y"),
        ("reject", "Reject plan", "n"),
        ("complete", "Complete setup", "c"),
        ("skip", "Skip setup", "s"),
        ("launch", "Launch", "l"),
        ("accounts", "Review accounts", "shift+a"),
        ("newContext", "Create context", "shift+n"),
        ("contexts", "Select context", "o"),
        ("discover", "Discover models", "shift+m"),
    ] {
        registry.register(KeymapAction {
            id: format!("tui.onboarding.{action}"),
            scope: "onboarding".into(),
            description: description.into(),
            defaults: vec![key.into()],
        })?;
    }
    for (id, description, key) in [
        ("tui.session.scopeToggle", "Toggle directory scope", "tab"),
        ("tui.session.catalogNextPage", "Next catalog page", "alt+p"),
        ("tui.session.catalogRestart", "Restart catalog", "alt+r"),
    ] {
        registry.register(KeymapAction {
            id: id.into(),
            scope: "session_picker".into(),
            description: description.into(),
            defaults: vec![key.into()],
        })?;
    }
    registry.register(KeymapAction {
        id: "tui.session.childrenNextPage".into(),
        scope: "session_picker".into(),
        description: "Load next child page".into(),
        defaults: vec!["alt+n".into()],
    })?;
    for (id, key) in [("tui.setup.retry", "r"), ("tui.setup.back", "b")] {
        registry.register(KeymapAction {
            id: id.into(),
            scope: "setup_auth_terminal".into(),
            description: id.into(),
            defaults: vec![key.into()],
        })?;
    }
    registry.register(KeymapAction {
        id: "tui.session.search".into(),
        scope: "session_picker".into(),
        description: "Search transcripts".into(),
        defaults: vec!["ctrl+f".into()],
    })?;

    Ok(())
}

/// Compiled TUI keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxKeyMap {
    error: Option<String>,
    registry: bcode_plugin_sdk::tui::keymap::KeymapRegistry,
    activations: BTreeMap<(BmuxScope, BmuxAction), BmuxKeyActivation>,
}

impl BmuxKeyMap {
    /// Adapt an already effective registry for host rendering.
    #[must_use]
    pub const fn from_registry(registry: bcode_plugin_sdk::tui::keymap::KeymapRegistry) -> Self {
        Self {
            registry,
            error: None,
            activations: BTreeMap::new(),
        }
    }

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
        let overrides = config.keybindings.scoped_overrides_with(|id| {
            BmuxAction::from_id(id).map_or_else(|| id.to_owned(), |action| action.id().into())
        });
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
        registry.register(KeymapAction {
            id: BmuxAction::TranscriptClearSelection.id().into(),
            scope: "chat".into(),
            description: BmuxAction::TranscriptClearSelection.description().into(),
            defaults: Vec::new(),
        })?;
        register_host_actions(&mut registry)?;
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

    /// Resolve a contributor-extensible action in an arbitrary terminal scope.
    #[must_use]
    pub fn action(&self, scope: &str, stroke: KeyStroke) -> Option<&str> {
        self.registry.action_for_key(scope, stroke)
    }

    /// Effective shortcut presentation; explicitly identifies unbound actions.
    #[must_use]
    pub fn hint(&self, scope: &str, action: &str) -> String {
        let keys = self.registry.keys_for_action(scope, action);
        if keys.is_empty() {
            "unbound".into()
        } else {
            keys.join(" / ")
        }
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

    /// Atomically integrate enabled contributors; failures disable dispatch.
    pub fn integrate_contributors(
        &mut self,
        actions: Result<
            Vec<bcode_plugin_sdk::tui::keymap::KeymapAction>,
            bcode_plugin_sdk::tui::keymap::KeymapError,
        >,
    ) {
        if self.error.is_some() {
            return;
        }
        let mut candidate = self.registry.clone();
        let result = actions.and_then(|actions| candidate.register_all(actions));
        match result {
            Ok(()) => self.registry = candidate,
            Err(error) => {
                self.registry = bcode_plugin_sdk::tui::keymap::KeymapRegistry::default();
                self.activations.clear();
                self.error = Some(error.to_string());
            }
        }
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
            BmuxAction::EditorUndo
            | BmuxAction::EditorRedo
            | BmuxAction::TranscriptClearSelection
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
            | BmuxAction::EditorUndo
            | BmuxAction::EditorRedo
            | BmuxAction::SkillInvoke
            | BmuxAction::SkillActivate
            | BmuxAction::SkillDeactivate
            | BmuxAction::SkillHelp => return None,
        })
    }
}

fn default_chat_bindings() -> Vec<BmuxKeyBinding> {
    vec![
        bind("enter", BmuxAction::InputSubmitSteering),
        bind("ctrl+shift+enter", BmuxAction::InputSubmitFollowUp),
        bind("shift+enter", BmuxAction::InputNewLine),
        bind("up", BmuxAction::InputHistoryPrevious),
        bind("down", BmuxAction::InputHistoryNext),
        bind("left", BmuxAction::EditorMoveLeft),
        bind("right", BmuxAction::EditorMoveRight),
        bind("home", BmuxAction::EditorMoveStart),
        bind("ctrl+a", BmuxAction::EditorMoveStart),
        bind("end", BmuxAction::EditorMoveEnd),
        bind("ctrl+e", BmuxAction::EditorMoveEnd),
        bind("ctrl+left", BmuxAction::EditorMoveWordLeft),
        bind("alt+left", BmuxAction::EditorMoveWordLeft),
        bind("alt+b", BmuxAction::EditorMoveWordLeft),
        bind("ctrl+right", BmuxAction::EditorMoveWordRight),
        bind("alt+right", BmuxAction::EditorMoveWordRight),
        bind("alt+f", BmuxAction::EditorMoveWordRight),
        bind("backspace", BmuxAction::EditorDeleteBackward),
        bind("delete", BmuxAction::EditorDeleteForward),
        bind("ctrl+w", BmuxAction::EditorDeleteWordBackward),
        bind("ctrl+backspace", BmuxAction::EditorDeleteWordBackward),
        bind("alt+backspace", BmuxAction::EditorDeleteWordBackward),
        bind("ctrl+delete", BmuxAction::EditorDeleteWordForward),
        bind("alt+delete", BmuxAction::EditorDeleteWordForward),
        bind("ctrl+u", BmuxAction::EditorDeleteToStart),
        bind("ctrl+k", BmuxAction::EditorDeleteToEnd),
        bind("ctrl+z", BmuxAction::EditorUndo),
        bind("super+z", BmuxAction::EditorUndo),
        bind("ctrl+shift+z", BmuxAction::EditorRedo),
        bind("super+shift+z", BmuxAction::EditorRedo),
        bind("ctrl+y", BmuxAction::EditorRedo),
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
    ]
}

fn default_bindings() -> BTreeMap<BmuxScope, Vec<BmuxKeyBinding>> {
    BTreeMap::from([
        (BmuxScope::Chat, default_chat_bindings()),
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

impl bcode_tui_components::session_ui::bindings::SessionKeyBindings for BmuxKeyMap {
    fn resolve(
        &self,
        stroke: KeyStroke,
    ) -> bcode_tui_components::session_ui::bindings::SessionKeyBinding {
        use bcode_tui_components::session_ui::bindings::SessionKeyBinding;
        if let Some(motion) = self.editor_selection_motion_for_key(stroke) {
            return SessionKeyBinding::Selection(motion);
        }
        if let Some(command) = self.editor_command_for_key(stroke) {
            return SessionKeyBinding::Editor(command);
        }
        if self.action("chat", stroke).is_some()
            || self.registry().is_overridden_key("chat", stroke)
        {
            return SessionKeyBinding::Host;
        }
        bcode_plugin_sdk::tui::keymap::controls::LiteralText::from_stroke(stroke)
            .map_or(SessionKeyBinding::Ignored, |text| {
                SessionKeyBinding::Literal(text.native_stroke())
            })
    }
}

#[cfg(test)]
mod tests {
    use bcode_config::TuiConfig;
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

    use super::{BmuxAction, BmuxKeyMap, BmuxScope};
    use bmux_keyboard::parse_key_stroke;
    use std::collections::BTreeMap;

    #[test]
    fn conflicting_contributors_disable_all_dispatch_atomically() {
        use bcode_plugin_sdk::tui::keymap::KeymapAction;
        let mut keymap = BmuxKeyMap::from_config(&TuiConfig::default());
        keymap.integrate_contributors(Ok(vec![
            KeymapAction {
                id: "test.valid".into(),
                scope: "test".into(),
                description: "Valid".into(),
                defaults: vec!["x".into()],
            },
            KeymapAction {
                id: "test.conflict".into(),
                scope: "test".into(),
                description: "Conflict".into(),
                defaults: vec!["x".into()],
            },
        ]));
        assert!(keymap.error().is_some());
        assert_eq!(keymap.action("test", parse_key_stroke("x").unwrap()), None);
        assert_eq!(
            keymap.action("chat", parse_key_stroke("enter").unwrap()),
            None
        );
    }

    #[test]
    fn host_remap_and_unbind_dispatch_and_hint_agree() {
        let mut config = TuiConfig::default();
        config.keybindings.scopes.insert(
            "theme_picker".into(),
            BTreeMap::from([
                ("ctrl+n".into(), "tui.control.down".into()),
                ("escape".into(), String::new()),
            ]),
        );
        let keymap = BmuxKeyMap::try_from_config(&config).expect("valid effective registry");
        assert_eq!(
            keymap.action("theme_picker", parse_key_stroke("down").unwrap()),
            None
        );
        assert_eq!(
            keymap.action("theme_picker", parse_key_stroke("j").unwrap()),
            None
        );
        assert_eq!(
            keymap.action("theme_picker", parse_key_stroke("ctrl+n").unwrap()),
            Some("tui.control.down")
        );
        assert_eq!(
            keymap.action("theme_picker", parse_key_stroke("escape").unwrap()),
            None
        );
        assert_eq!(keymap.hint("theme_picker", "tui.control.down"), "ctrl+n");
        assert_eq!(keymap.hint("theme_picker", "tui.control.cancel"), "unbound");
    }

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
            keymap.action("chat", default_stroke),
            Some("tui.monitor.toggle")
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
            configured.action("chat", default_stroke),
            Some("tui.monitor.toggle")
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
    fn catalog_scope_actions_have_effective_bindings() {
        let keymap = default_keymap();
        for (key, action) in [
            ("tab", "tui.session.scopeToggle"),
            ("alt+p", "tui.session.catalogNextPage"),
            ("alt+r", "tui.session.catalogRestart"),
        ] {
            assert_eq!(
                keymap.action("session_picker", parse_key_stroke(key).unwrap()),
                Some(action)
            );
            assert_eq!(
                keymap.registry().keys_for_action("session_picker", action),
                [key]
            );
        }
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
