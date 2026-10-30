//! Bounded resolved native presentation policy. No configuration resolution here.
use bcode_tui_components::session_ui::bindings::SessionKeyBinding;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Resolved policy exported once by the native configuration owner.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserPolicy {
    /// Contract version, currently one.
    pub schema_version: u16,
    /// Effective theme and keymap fingerprint.
    pub revision: String,
    /// Resolved semantic styles.
    pub styles: BTreeMap<String, BrowserStyle>,
    /// Complete effective scoped bindings, including chords.
    pub bindings: Vec<BrowserBinding>,
}
/// One resolved style, with RGB colors rather than terminal palette guesses.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
// Wire-level style flags are independent modifier bits, not exclusive states.
#[allow(clippy::struct_excessive_bools)]
pub struct BrowserStyle {
    pub foreground: Option<[u8; 3]>,
    pub background: Option<[u8; 3]>,
    #[serde(default)]
    pub foreground_palette: Option<u8>,
    #[serde(default)]
    pub background_palette: Option<u8>,
    #[serde(default)]
    pub dim: bool,
    #[serde(default)]
    pub strikethrough: bool,
    #[serde(default)]
    pub slow_blink: bool,
    #[serde(default)]
    pub hidden: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reversed: bool,
}
/// One effective scoped binding.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserBinding {
    pub scope: String,
    pub chord: Vec<BrowserStroke>,
    pub binding: BrowserResolvedBinding,
}
/// Browser bridge key identity with explicit modifiers.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
// Wire-level keyboard modifiers are independent bits and may be combined.
#[allow(clippy::struct_excessive_bools)]
pub struct BrowserStroke {
    pub key: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}
/// Already-resolved editor, selection or application command.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserResolvedBinding {
    Editor { command: BrowserEditorCommand },
    Selection { motion: BrowserMotion },
    Host { action: String },
    Ignored,
}
/// Exact portable spelling of the BMUX editor command family.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum BrowserEditorCommand {
    Insert(char),
    InsertString(String),
    Paste(String),
    Move(BrowserMotion),
    Delete(BrowserDelete),
    Clear,
}
/// Exact portable spelling of BMUX source/visual motions.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum BrowserMotion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Start,
    End,
    LineStart,
    Absolute(usize),
    VisualUp,
    VisualDown,
    LineEnd,
}
/// Exact portable spelling of BMUX deletion commands.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum BrowserDelete {
    Backward,
    Forward,
    WordBackward,
    WordForward,
    ToLineStart,
    ToLineEnd,
    ToStart,
    ToEnd,
}

/// Caller-owned bounded chord state; prefixes never fall through to defaults.
#[derive(Default)]
pub struct PolicyBindings {
    pub policy: Option<BrowserPolicy>,
    pending: std::cell::RefCell<BTreeMap<String, Vec<BrowserStroke>>>,
    resolved: std::cell::RefCell<BTreeMap<String, (KeyStroke, Option<BrowserResolvedBinding>)>>,
}
impl PolicyBindings {
    pub fn replace(&mut self, policy: BrowserPolicy) {
        self.policy = Some(policy);
        self.pending.get_mut().clear();
        self.resolved.get_mut().clear();
    }
    pub fn cancel_chord(&self) {
        self.pending.borrow_mut().clear();
        self.resolved.borrow_mut().clear();
    }
    pub fn begin_event(&self) {
        self.resolved.borrow_mut().clear();
    }
    pub fn resolve(&self, scope: &str, key: KeyStroke) -> Option<BrowserResolvedBinding> {
        let policy = self.policy.as_ref()?;
        if let Some((previous, binding)) = self.resolved.borrow().get(scope)
            && *previous == key
        {
            return binding.clone();
        }
        let stroke = BrowserStroke::from_key(key);
        let mut pending = self.pending.borrow_mut();
        let sequence = pending.entry(scope.to_owned()).or_default();
        sequence.push(stroke.clone());
        let find = |sequence: &[BrowserStroke]| {
            policy
                .bindings
                .iter()
                .find(|binding| binding.scope == scope && binding.chord == sequence)
        };
        let binding = if let Some(binding) = find(sequence) {
            let binding = binding.binding.clone();
            sequence.clear();
            Some(binding)
        } else if policy
            .bindings
            .iter()
            .any(|binding| binding.scope == scope && binding.chord.starts_with(sequence))
        {
            Some(BrowserResolvedBinding::Ignored)
        } else {
            sequence.clear();
            sequence.push(stroke);
            if let Some(binding) = find(sequence) {
                let binding = binding.binding.clone();
                sequence.clear();
                Some(binding)
            } else {
                sequence.clear();
                Some(BrowserResolvedBinding::Ignored)
            }
        };
        self.resolved
            .borrow_mut()
            .insert(scope.to_owned(), (key, binding.clone()));
        binding
    }
}

impl BrowserPolicy {
    /// Validate an exact resolved native policy before installing any of it.
    ///
    /// # Errors
    /// Rejects malformed, unsupported, oversized or duplicate effective bindings.
    pub fn parse(json: &str) -> Result<Self, &'static str> {
        if json.len() > 65536 {
            return Err("presentation policy too large");
        }
        let policy: Self = serde_json::from_str(json).map_err(|_| "invalid presentation policy")?;
        if policy.schema_version != 1 || policy.revision.is_empty() || policy.revision.len() > 128
            || policy.styles.len() > 32 || policy.bindings.len() > 512
            || policy.styles.values().any(|style| style.foreground.is_some() && style.foreground_palette.is_some() || (style.background.is_some() && style.background_palette.is_some()))
            || policy.styles.keys().any(|key| key.len() > 128)
            || policy.bindings.iter().any(|binding| binding.scope.len() > 128 || binding.chord.is_empty() || binding.chord.len() > 4
                || binding.chord.iter().any(|key| key.key.is_empty() || key.key.len() > 128)
                || matches!(&binding.binding, BrowserResolvedBinding::Host { action } if action.len() > 128)) {
            return Err("unsupported presentation policy");
        }
        for (index, binding) in policy.bindings.iter().enumerate() {
            if policy.bindings[..index]
                .iter()
                .any(|other| other.scope == binding.scope && other.chord == binding.chord)
            {
                return Err("duplicate effective binding");
            }
        }
        Ok(policy)
    }
    pub(crate) fn transcript_presentation(
        &self,
    ) -> bcode_tui_components::transcript::accepted::TranscriptPresentation {
        bcode_tui_components::transcript::accepted::TranscriptPresentation {
            heading: self.style("title"),
            muted: self.style("muted"),
            markdown: bcode_markdown_render::MarkdownTheme {
                text: self.style("markdown.text"),
                heading: self.style("markdown.heading"),
                link: self.style("markdown.link"),
                strong: self.style("markdown.strong"),
                emphasis: self.style("markdown.emphasis"),
                strikethrough: self.style("markdown.strikethrough"),
                inline_code: self.style("markdown.inline_code"),
                code_block_text: self.style("markdown.code_block_text"),
                code_block_border: self.style("markdown.code_block_border"),
                blockquote_bar: self.style("markdown.blockquote_bar"),
                alert_note: self.style("markdown.alert_note"),
                alert_tip: self.style("markdown.alert_tip"),
                alert_important: self.style("markdown.alert_important"),
                alert_warning: self.style("markdown.alert_warning"),
                alert_caution: self.style("markdown.alert_caution"),
                list_marker: self.style("markdown.list_marker"),
                task_checked: self.style("markdown.task_checked"),
                task_unchecked: self.style("markdown.task_unchecked"),
                table_border: self.style("markdown.table_border"),
                horizontal_rule: self.style("markdown.horizontal_rule"),
            },
        }
    }
    pub(crate) fn component_theme(&self) -> bmux_tui_components::theme::ComponentTheme {
        use bmux_tui_components::theme::{ComponentSurfaces, ComponentTheme};
        ComponentTheme {
            canvas: self.style("surface"),
            surfaces: ComponentSurfaces {
                normal: self.style("surface"),
                raised: self.style("surface"),
                overlay: self.style("surface"),
                scrim: None,
            },
            text: self.style("text"),
            focused: self.style("focused"),
            selected: self.style("selection"),
            disabled: self.style("muted"),
            muted: self.style("muted"),
            info: self.style("title"),
            success: self.style("title"),
            warning: self.style("error"),
            error: self.style("error"),
            border: self.style("border"),
        }
    }
    pub(crate) fn style(&self, role: &str) -> Style {
        let Some(value) = self.styles.get(role) else {
            return Style::new();
        };
        let mut style = Style::new();
        if let Some(index) = value.foreground_palette {
            style = style.fg(Color::Indexed(index));
        }
        if let Some(index) = value.background_palette {
            style = style.bg(Color::Indexed(index));
        }
        if let Some([r, g, b]) = value.foreground {
            style = style.fg(Color::Rgb(r, g, b));
        }
        if let Some([r, g, b]) = value.background {
            style = style.bg(Color::Rgb(r, g, b));
        }
        for (enabled, modifier) in [
            (value.bold, Modifier::BOLD),
            (value.italic, Modifier::ITALIC),
            (value.underline, Modifier::UNDERLINE),
            (value.reversed, Modifier::REVERSED),
            (value.dim, Modifier::DIM),
            (value.strikethrough, Modifier::CROSSED_OUT),
            (value.slow_blink, Modifier::SLOW_BLINK),
            (value.hidden, Modifier::HIDDEN),
        ] {
            if enabled {
                style = style.add_modifier(modifier);
            }
        }
        style
    }
}
impl BrowserStroke {
    pub(crate) fn from_key(key: KeyStroke) -> Self {
        Self {
            key: match key.key {
                KeyCode::Char(' ') | KeyCode::Space => " ".into(),
                KeyCode::Char(c) => c.to_string(),
                KeyCode::Enter => "Enter".into(),
                KeyCode::Escape => "Escape".into(),
                KeyCode::Backspace => "Backspace".into(),
                KeyCode::Tab => "Tab".into(),
                KeyCode::Left => "ArrowLeft".into(),
                KeyCode::Right => "ArrowRight".into(),
                KeyCode::Up => "ArrowUp".into(),
                KeyCode::Down => "ArrowDown".into(),
                KeyCode::Home => "Home".into(),
                KeyCode::End => "End".into(),
                KeyCode::Delete => "Delete".into(),
                KeyCode::PageUp => "PageUp".into(),
                KeyCode::PageDown => "PageDown".into(),
                other => format!("{other:?}"),
            },
            ctrl: key.modifiers.ctrl,
            alt: key.modifiers.alt,
            shift: key.modifiers.shift,
            meta: key.modifiers.super_key,
        }
    }
}
impl From<BrowserMotion> for bmux_text_edit::TextMotion {
    fn from(value: BrowserMotion) -> Self {
        match value {
            BrowserMotion::Left => Self::Left,
            BrowserMotion::Right => Self::Right,
            BrowserMotion::WordLeft => Self::WordLeft,
            BrowserMotion::WordRight => Self::WordRight,
            BrowserMotion::Start => Self::Start,
            BrowserMotion::End => Self::End,
            BrowserMotion::LineStart => Self::LineStart,
            BrowserMotion::LineEnd => Self::LineEnd,
            BrowserMotion::Absolute(index) => Self::Absolute(index),
            BrowserMotion::VisualUp => Self::VisualUp,
            BrowserMotion::VisualDown => Self::VisualDown,
        }
    }
}
impl BrowserResolvedBinding {
    pub(crate) fn session(&self) -> SessionKeyBinding {
        use bmux_text_edit::{TextDelete as D, TextEditCommand as C};
        match self {
            Self::Editor { command } => SessionKeyBinding::Editor(match command {
                BrowserEditorCommand::Insert(c) => C::Insert(*c),
                BrowserEditorCommand::InsertString(s) => C::InsertString(s.clone()),
                BrowserEditorCommand::Paste(s) => C::Paste(s.clone()),
                BrowserEditorCommand::Move(m) => C::Move((*m).into()),
                BrowserEditorCommand::Clear => C::Clear,
                BrowserEditorCommand::Delete(d) => C::Delete(match d {
                    BrowserDelete::Backward => D::Backward,
                    BrowserDelete::Forward => D::Forward,
                    BrowserDelete::WordBackward => D::WordBackward,
                    BrowserDelete::WordForward => D::WordForward,
                    BrowserDelete::ToLineStart => D::ToLineStart,
                    BrowserDelete::ToLineEnd => D::ToLineEnd,
                    BrowserDelete::ToStart => D::ToStart,
                    BrowserDelete::ToEnd => D::ToEnd,
                }),
            }),
            Self::Selection { motion } => SessionKeyBinding::Selection((*motion).into()),
            Self::Host { .. } => SessionKeyBinding::Host,
            Self::Ignored => SessionKeyBinding::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(bindings: &serde_json::Value) -> String {
        serde_json::json!({"schema_version":1,"revision":"effective", "styles":{}, "bindings":bindings}).to_string()
    }
    fn key(text: &str) -> KeyStroke {
        bmux_keyboard::parse_key_stroke(text).unwrap()
    }
    #[test]
    fn typed_editor_selection_and_chord_never_fall_through_unknown_host_actions() {
        let parsed = BrowserPolicy::parse(&policy(&serde_json::json!([
            {"scope":"chat","chord":[BrowserStroke::from_key(key("ctrl+x")),BrowserStroke::from_key(key("ctrl+s"))],"binding":{"type":"host","action":"unsupported.action"}},
            {"scope":"chat","chord":[BrowserStroke::from_key(key("left"))],"binding":{"type":"editor","command":{"Move":"Left"}}},
            {"scope":"chat","chord":[BrowserStroke::from_key(key("shift+left"))],"binding":{"type":"selection","motion":"Left"}}
        ]))).unwrap();
        let mut state = PolicyBindings::default();
        state.replace(parsed);
        assert!(matches!(
            state.resolve("chat", key("ctrl+x")),
            Some(BrowserResolvedBinding::Ignored)
        ));
        state.begin_event();
        assert!(matches!(
            state.resolve("chat", key("ctrl+s")),
            Some(BrowserResolvedBinding::Host { .. })
        ));
        state.begin_event();
        assert!(matches!(
            state.resolve("chat", key("left")).unwrap().session(),
            SessionKeyBinding::Editor(bmux_text_edit::TextEditCommand::Move(
                bmux_text_edit::TextMotion::Left
            ))
        ));
        state.begin_event();
        assert!(matches!(
            state.resolve("chat", key("shift+left")).unwrap().session(),
            SessionKeyBinding::Selection(bmux_text_edit::TextMotion::Left)
        ));
        state.begin_event();
        assert!(matches!(
            state.resolve("chat", key("enter")),
            Some(BrowserResolvedBinding::Ignored)
        ));
    }
    #[test]
    fn palette_indices_and_all_supported_modifiers_are_not_rgb_guesses() {
        let json = serde_json::json!({"schema_version":1,"revision":"palette","bindings":[],"styles":{"text":{
            "foreground":null,"background":null,"foreground_palette":201,"background_palette":16,
            "bold":true,"italic":true,"underline":true,"reversed":true,"dim":true,"hidden":true,"strikethrough":true,"slow_blink":true
        }}}).to_string();
        let style = BrowserPolicy::parse(&json).unwrap().style("text");
        assert_eq!(style.fg, Some(Color::Indexed(201)));
        assert_eq!(style.bg, Some(Color::Indexed(16)));
        assert!(style.modifiers.contains(
            Modifier::HIDDEN | Modifier::CROSSED_OUT | Modifier::SLOW_BLINK | Modifier::DIM
        ));
        let duplicate = serde_json::json!([{"scope":"chat","chord":[BrowserStroke::from_key(key("enter"))],"binding":{"type":"ignored"}}]);
        let mut bindings = duplicate.as_array().unwrap().clone();
        bindings.push(bindings[0].clone());
        assert!(BrowserPolicy::parse(&policy(&serde_json::Value::Array(bindings))).is_err());
        assert!(
            BrowserPolicy::parse(&json.replace(
                "\"slow_blink\":true",
                "\"slow_blink\":true,\"rapid_blink\":true"
            ))
            .is_err()
        );
    }
}
