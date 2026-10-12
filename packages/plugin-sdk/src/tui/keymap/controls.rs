//! Effective semantic control routing at the terminal boundary.
//!
//! Native strokes below are BMUX adapter commands, never fallback user shortcuts. Only
//! resolved actions may produce them; literal text is classified independently.

use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

use super::{KeymapAction, KeymapError, KeymapRegistry};
use crate::tui::PluginTuiHost;

/// Shared semantic operations understood by BMUX terminal controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    /// Previous row or visual line.
    Up,
    /// Next row or visual line.
    Down,
    /// Previous character or collapse tree node.
    Left,
    /// Next character or expand tree node.
    Right,
    /// Beginning of the buffer/list.
    Home,
    /// End of the buffer/list.
    End,
    /// Previous page.
    PageUp,
    /// Next page.
    PageDown,
    /// Activate or submit.
    Submit,
    /// Insert a newline rather than submit.
    Newline,
    /// Cancel or close.
    Cancel,
    /// Move to the next focus target.
    FocusNext,
    /// Move to the previous focus target.
    FocusPrevious,
    /// Toggle the selected item.
    Toggle,
    /// Delete previous character.
    Backspace,
    /// Delete next character.
    Delete,
    /// Move to previous word.
    WordLeft,
    /// Move to next word.
    WordRight,
    /// Delete previous word.
    DeleteWordBackward,
    /// Delete next word.
    DeleteWordForward,
    /// Move to beginning of current boundary.
    LineStart,
    /// Move to end of current boundary.
    LineEnd,
    /// Delete to beginning of current boundary.
    DeleteToStart,
    /// Delete to end of current boundary.
    DeleteToEnd,
    /// Extend selection one character left.
    SelectLeft,
    /// Extend selection one character right.
    SelectRight,
    /// Extend selection one visual line up.
    SelectUp,
    /// Extend selection one visual line down.
    SelectDown,
    /// Extend selection one word left.
    SelectWordLeft,
    /// Extend selection one word right.
    SelectWordRight,
}

impl ControlAction {
    /// Shared text editor operations. Callers choose only operations their control supports.
    pub const TEXT: &'static [Self] = &[
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
        Self::Home,
        Self::End,
        Self::Backspace,
        Self::Delete,
        Self::WordLeft,
        Self::WordRight,
        Self::DeleteWordBackward,
        Self::DeleteWordForward,
        Self::LineStart,
        Self::LineEnd,
        Self::DeleteToStart,
        Self::DeleteToEnd,
        Self::SelectLeft,
        Self::SelectRight,
        Self::SelectUp,
        Self::SelectDown,
        Self::SelectWordLeft,
        Self::SelectWordRight,
    ];

    /// List navigation operations, excluding activation and dismissal policy.
    pub const NAVIGATION: &'static [Self] = &[
        Self::Up,
        Self::Down,
        Self::Home,
        Self::End,
        Self::PageUp,
        Self::PageDown,
    ];

    const fn metadata(self) -> (&'static str, &'static str, &'static [&'static str]) {
        match self {
            Self::Up => ("up", "Move up", &["up"]),
            Self::Down => ("down", "Move down", &["down"]),
            Self::Left => ("left", "Move left", &["left"]),
            Self::Right => ("right", "Move right", &["right"]),
            Self::Home => ("home", "Move to beginning", &["home"]),
            Self::End => ("end", "Move to end", &["end"]),
            Self::PageUp => ("page_up", "Move one page up", &["pageup"]),
            Self::PageDown => ("page_down", "Move one page down", &["pagedown"]),
            Self::Submit => ("submit", "Activate or submit", &["enter"]),
            Self::Newline => ("newline", "Insert newline", &["shift+enter"]),
            Self::Cancel => ("cancel", "Cancel or close", &["escape"]),
            Self::FocusNext => ("focus_next", "Focus next control", &["tab"]),
            Self::FocusPrevious => ("focus_previous", "Focus previous control", &["shift+tab"]),
            Self::Toggle => ("toggle", "Toggle selection", &["space"]),
            Self::Backspace => ("backspace", "Delete previous character", &["backspace"]),
            Self::Delete => ("delete", "Delete next character", &["delete"]),
            Self::WordLeft => (
                "word_left",
                "Move to previous word",
                &["ctrl+left", "alt+left", "alt+b"],
            ),
            Self::WordRight => (
                "word_right",
                "Move to next word",
                &["ctrl+right", "alt+right", "alt+f"],
            ),
            Self::DeleteWordBackward => (
                "delete_word_backward",
                "Delete previous word",
                &["ctrl+w", "ctrl+backspace", "alt+backspace"],
            ),
            Self::DeleteWordForward => (
                "delete_word_forward",
                "Delete next word",
                &["ctrl+delete", "alt+delete"],
            ),
            Self::LineStart => ("line_start", "Move to boundary start", &["ctrl+a"]),
            Self::LineEnd => ("line_end", "Move to boundary end", &["ctrl+e"]),
            Self::DeleteToStart => ("delete_to_start", "Delete to boundary start", &["ctrl+u"]),
            Self::DeleteToEnd => ("delete_to_end", "Delete to boundary end", &["ctrl+k"]),
            Self::SelectLeft => ("select_left", "Select left", &["shift+left"]),
            Self::SelectRight => ("select_right", "Select right", &["shift+right"]),
            Self::SelectUp => ("select_up", "Select up", &["shift+up"]),
            Self::SelectDown => ("select_down", "Select down", &["shift+down"]),
            Self::SelectWordLeft => (
                "select_word_left",
                "Select previous word",
                &["ctrl+shift+left", "alt+shift+left"],
            ),
            Self::SelectWordRight => (
                "select_word_right",
                "Select next word",
                &["ctrl+shift+right", "alt+shift+right"],
            ),
        }
    }

    /// Stable action identity, shared across scopes.
    #[must_use]
    pub fn id(self) -> String {
        format!("tui.control.{}", self.metadata().0)
    }

    /// Definition for the requested scope; action owners may replace its default aliases.
    #[must_use]
    pub fn definition(self, scope: &str) -> KeymapAction {
        let (_, description, defaults) = self.metadata();
        KeymapAction {
            id: self.id(),
            scope: scope.into(),
            description: description.into(),
            defaults: defaults.iter().map(|key| (*key).into()).collect(),
        }
    }

    /// BMUX-native adapter event for this semantic operation, not a user binding.
    #[must_use]
    pub const fn native_stroke(self) -> KeyStroke {
        let (key, ctrl, alt, shift) = match self {
            Self::Up => (KeyCode::Up, false, false, false),
            Self::Down => (KeyCode::Down, false, false, false),
            Self::Left => (KeyCode::Left, false, false, false),
            Self::Right => (KeyCode::Right, false, false, false),
            Self::Home => (KeyCode::Home, false, false, false),
            Self::End => (KeyCode::End, false, false, false),
            Self::PageUp => (KeyCode::PageUp, false, false, false),
            Self::PageDown => (KeyCode::PageDown, false, false, false),
            Self::Submit => (KeyCode::Enter, false, false, false),
            Self::Newline => (KeyCode::Enter, false, false, true),
            Self::Cancel => (KeyCode::Escape, false, false, false),
            Self::FocusNext => (KeyCode::Tab, false, false, false),
            Self::FocusPrevious => (KeyCode::Tab, false, false, true),
            Self::Toggle => (KeyCode::Char(' '), false, false, false),
            Self::Backspace => (KeyCode::Backspace, false, false, false),
            Self::Delete => (KeyCode::Delete, false, false, false),
            Self::WordLeft => (KeyCode::Left, true, false, false),
            Self::WordRight => (KeyCode::Right, true, false, false),
            Self::DeleteWordBackward => (KeyCode::Char('w'), true, false, false),
            Self::DeleteWordForward => (KeyCode::Delete, true, false, false),
            Self::LineStart => (KeyCode::Char('a'), true, false, false),
            Self::LineEnd => (KeyCode::Char('e'), true, false, false),
            Self::DeleteToStart => (KeyCode::Char('u'), true, false, false),
            Self::DeleteToEnd => (KeyCode::Char('k'), true, false, false),
            Self::SelectLeft => (KeyCode::Left, false, false, true),
            Self::SelectRight => (KeyCode::Right, false, false, true),
            Self::SelectUp => (KeyCode::Up, false, false, true),
            Self::SelectDown => (KeyCode::Down, false, false, true),
            Self::SelectWordLeft => (KeyCode::Left, true, false, true),
            Self::SelectWordRight => (KeyCode::Right, true, false, true),
        };
        KeyStroke::with_modifiers(
            key,
            Modifiers {
                ctrl,
                alt,
                shift,
                ..Modifiers::NONE
            },
        )
    }

    /// Recognize only semantic actions explicitly supported by this control.
    #[must_use]
    pub fn from_id(id: &str, supported: &[Self]) -> Option<Self> {
        let suffix = id.strip_prefix("tui.control.")?;
        supported
            .iter()
            .copied()
            .find(|action| action.metadata().0 == suffix)
    }
}

/// Literal printable text, separated from action routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiteralText(pub char);

impl LiteralText {
    /// Classify a printable stroke without control, alt, super, hyper or meta modifiers.
    #[must_use]
    pub const fn from_stroke(stroke: KeyStroke) -> Option<Self> {
        let mods = stroke.modifiers;
        if mods.ctrl || mods.alt || mods.super_key || mods.hyper || mods.meta {
            return None;
        }
        let ch = match stroke.key {
            KeyCode::Char(ch) if !ch.is_control() => {
                if mods.shift && ch.is_ascii_lowercase() {
                    ch.to_ascii_uppercase()
                } else {
                    ch
                }
            }
            KeyCode::Space => ' ',
            _ => return None,
        };
        Some(Self(ch))
    }

    /// Literal insertion event; no shortcut or navigation key can be produced.
    #[must_use]
    pub const fn native_stroke(self) -> KeyStroke {
        KeyStroke::simple(KeyCode::Char(self.0))
    }
}

/// Input admitted to a generic control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlInput {
    /// A registered and supported semantic operation.
    Action(ControlAction),
    /// A literal text character, admitted only when requested.
    Text(LiteralText),
    /// Unknown, unsupported or explicitly disabled input.
    Ignored,
}

impl ControlInput {
    /// Resolve using a plugin host. An action belonging to another handler never becomes text.
    #[must_use]
    pub fn from_host(
        host: &dyn PluginTuiHost,
        scope: &str,
        stroke: KeyStroke,
        supported: &[ControlAction],
        allow_text: bool,
    ) -> Self {
        let action = host.keymap_action(scope, stroke);
        if action.is_none() && host.keymap_is_overridden(scope, stroke) {
            return Self::Ignored;
        }
        Self::resolve(action.as_deref(), stroke, supported, allow_text)
    }

    /// Resolve using a standalone effective registry.
    #[must_use]
    pub fn from_registry(
        registry: &KeymapRegistry,
        scope: &str,
        stroke: KeyStroke,
        supported: &[ControlAction],
        allow_text: bool,
    ) -> Self {
        if registry.is_overridden_key(scope, stroke)
            && registry.action_for_key(scope, stroke).is_none()
        {
            return Self::Ignored;
        }
        Self::resolve(
            registry.action_for_key(scope, stroke),
            stroke,
            supported,
            allow_text,
        )
    }

    fn resolve(
        action: Option<&str>,
        stroke: KeyStroke,
        supported: &[ControlAction],
        allow_text: bool,
    ) -> Self {
        if let Some(id) = action {
            return ControlAction::from_id(id, supported).map_or(Self::Ignored, Self::Action);
        }
        if allow_text {
            LiteralText::from_stroke(stroke).map_or(Self::Ignored, Self::Text)
        } else {
            Self::Ignored
        }
    }

    /// BMUX event only for resolved semantics or literal insertion. Never forward raw input.
    #[must_use]
    pub const fn native_stroke(self) -> Option<KeyStroke> {
        match self {
            Self::Action(action) => Some(action.native_stroke()),
            Self::Text(text) => Some(text.native_stroke()),
            Self::Ignored => None,
        }
    }
}

/// Resolve standalone contributor definitions and configured overrides atomically.
/// Supply the same effective registry to input routing and shortcut presentation.
///
/// # Errors
/// Returns registration/conflict/stroke errors without silently using defaults.
pub fn resolve_registry(
    actions: impl IntoIterator<Item = KeymapAction>,
    overrides: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
) -> Result<KeymapRegistry, KeymapError> {
    let mut registry = KeymapRegistry::default();
    registry.configure(overrides)?;
    registry.register_all(actions)?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn remapped_navigation_never_forwards_raw_navigation() {
        let registry = resolve_registry(
            [ControlAction::Up.definition("list")],
            BTreeMap::from([(
                "list".into(),
                BTreeMap::from([("ctrl+p".into(), ControlAction::Up.id())]),
            )]),
        )
        .unwrap();
        let parse = |key| bmux_keyboard::parse_key_stroke(key).unwrap();
        assert_eq!(
            ControlInput::from_registry(
                &registry,
                "list",
                parse("up"),
                ControlAction::NAVIGATION,
                false
            ),
            ControlInput::Ignored
        );
        assert_eq!(
            ControlInput::from_registry(
                &registry,
                "list",
                parse("ctrl+p"),
                ControlAction::NAVIGATION,
                false
            )
            .native_stroke(),
            Some(KeyStroke::simple(KeyCode::Up))
        );
        assert_eq!(
            registry.keys_for_action("list", &ControlAction::Up.id()),
            ["ctrl+p"]
        );
    }

    #[test]
    fn explicit_printable_unbind_and_unknown_action_block_text_fallback() {
        let registry = resolve_registry(
            [],
            BTreeMap::from([(
                "input".into(),
                BTreeMap::from([
                    ("x".into(), String::new()),
                    ("y".into(), "plugin.missing".into()),
                ]),
            )]),
        )
        .unwrap();
        for ch in ['x', 'y'] {
            assert_eq!(
                ControlInput::from_registry(
                    &registry,
                    "input",
                    KeyStroke::simple(KeyCode::Char(ch)),
                    ControlAction::TEXT,
                    true
                ),
                ControlInput::Ignored
            );
        }
    }

    #[test]
    fn text_is_separate_from_registered_and_unknown_control_keys() {
        let registry =
            resolve_registry([ControlAction::Toggle.definition("input")], BTreeMap::new()).unwrap();
        assert_eq!(
            ControlInput::from_registry(
                &registry,
                "input",
                KeyStroke::simple(KeyCode::Char('λ')),
                ControlAction::TEXT,
                true
            ),
            ControlInput::Text(LiteralText('λ'))
        );
        assert_eq!(
            ControlInput::from_registry(
                &registry,
                "input",
                KeyStroke::simple(KeyCode::Space),
                ControlAction::TEXT,
                true
            ),
            ControlInput::Ignored
        );
        assert_eq!(
            ControlInput::from_registry(
                &registry,
                "input",
                KeyStroke::simple(KeyCode::Up),
                ControlAction::TEXT,
                true
            ),
            ControlInput::Ignored
        );
    }
}
