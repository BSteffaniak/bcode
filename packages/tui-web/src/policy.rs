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
    /// Native interaction placement and offscreen focus policy.
    #[serde(default)]
    pub interaction: BrowserInteraction,
    /// Actual loaded producer set and native adapter selection policy.
    #[serde(default)]
    pub visual_policy: BrowserVisualPolicy,
}

/// Bounded selection data; registrations remain producer-owned Rust implementations.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserVisualPolicy {
    /// Enabled backend producers, never inferred from available WASM code.
    pub enabled_plugins: std::collections::BTreeSet<String>,
    /// Descending configured adapter preference.
    pub preferred: Vec<String>,
    /// Explicitly disabled adapter references.
    pub disabled: std::collections::BTreeSet<String>,
}

/// Renderer-owned policy, independent of producer workflow state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserInteraction {
    /// Inline transcript placement or pinned presentation.
    pub placement: InteractionPlacement,
    /// Input behavior while an inline interaction is hidden.
    pub offscreen_focus: OffscreenFocus,
}

/// Placement resolved by the native configuration owner.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionPlacement {
    /// Render at the canonical interaction's transcript position.
    #[default]
    Transcript,
    /// Keep the interaction visible above the composer.
    Pinned,
}

/// Focus behavior resolved by the native configuration owner.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OffscreenFocus {
    /// Continue routing input to a hidden active interaction.
    #[default]
    Retain,
    /// Return input to the composer until the interaction is visible.
    Suspend,
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
    /// Effective native multi-tap activation, absent for immediate bindings.
    #[serde(default)]
    pub multi_tap: Option<BrowserMultiTap>,
}

/// Native activation policy; event time comes only from the platform clock port.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserMultiTap {
    /// Consecutive taps required before admission.
    pub taps: u8,
    /// Maximum time between taps in milliseconds.
    pub window_ms: u64,
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
    event_time_ms: std::cell::Cell<Option<u64>>,
    taps: std::cell::RefCell<BTreeMap<String, (KeyStroke, u8, u64)>>,
}
impl PolicyBindings {
    pub fn replace(&mut self, policy: BrowserPolicy) {
        self.taps.get_mut().clear();
        self.policy = Some(policy);
        self.pending.get_mut().clear();
        self.resolved.get_mut().clear();
    }
    pub fn cancel_chord(&self) {
        self.taps.borrow_mut().clear();
        self.pending.borrow_mut().clear();
        self.resolved.borrow_mut().clear();
    }
    pub fn set_event_time(&self, millis: u64) {
        if self
            .event_time_ms
            .get()
            .is_some_and(|previous| millis < previous)
        {
            self.cancel_chord();
        }
        self.event_time_ms.set(Some(millis));
    }

    fn activate(
        &self,
        scope: &str,
        key: KeyStroke,
        binding: &BrowserBinding,
    ) -> BrowserResolvedBinding {
        let Some(policy) = &binding.multi_tap else {
            self.taps.borrow_mut().remove(scope);
            return binding.binding.clone();
        };
        let Some(now) = self.event_time_ms.get() else {
            return BrowserResolvedBinding::Ignored;
        };
        let mut taps = self.taps.borrow_mut();
        let previous = taps.get(scope).copied();
        let count = previous
            .filter(|(stroke, _, time)| {
                *stroke == key && now.saturating_sub(*time) <= policy.window_ms
            })
            .map_or(1, |(_, count, _)| count.saturating_add(1));
        if count >= policy.taps {
            taps.remove(scope);
            binding.binding.clone()
        } else {
            taps.insert(scope.to_owned(), (key, count, now));
            BrowserResolvedBinding::Ignored
        }
    }

    pub fn begin_event(&self) {
        self.resolved.borrow_mut().clear();
    }
    pub fn resolve(&self, scope: &str, key: KeyStroke) -> Option<BrowserResolvedBinding> {
        self.resolve_text(scope, key).or_else(|| {
            self.policy
                .as_ref()
                .map(|_| BrowserResolvedBinding::Ignored)
        })
    }

    /// Preserve unbound printable input for producer-owned text editing.
    /// Explicit ignored bindings and pending chords still consume the stroke.
    pub fn resolve_text(&self, scope: &str, key: KeyStroke) -> Option<BrowserResolvedBinding> {
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
            let binding = self.activate(scope, key, binding);
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
                let binding = self.activate(scope, key, binding);
                sequence.clear();
                Some(binding)
            } else {
                sequence.clear();
                self.taps.borrow_mut().remove(scope);
                None
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
        if json.len() > 1024 * 1024 {
            return Err("presentation policy too large");
        }
        let policy: Self = serde_json::from_str(json).map_err(|_| "invalid presentation policy")?;
        if policy.schema_version != 1 || policy.revision.is_empty() || policy.revision.len() > 128
            || policy.styles.len() > 64 || policy.bindings.len() > 4096
            || policy.styles.values().any(|style| style.foreground.is_some() && style.foreground_palette.is_some() || (style.background.is_some() && style.background_palette.is_some()))
            || policy.styles.keys().any(|key| key.len() > 128)
            || policy.bindings.iter().any(|binding| binding.scope.len() > 128 || binding.chord.is_empty() || binding.chord.len() > 4
                || binding.chord.iter().any(|key| key.key.is_empty() || key.key.len() > 128)
                || binding.multi_tap.as_ref().is_some_and(|tap| tap.taps < 2 || tap.taps > 8 || tap.window_ms == 0 || tap.window_ms > 60_000)
                || matches!(&binding.binding, BrowserResolvedBinding::Host { action } if action.len() > 128)) {
            return Err("unsupported presentation policy");
        }
        if policy.visual_policy.enabled_plugins.len() > 128
            || policy.visual_policy.preferred.len() > 128
            || policy.visual_policy.disabled.len() > 128
            || policy
                .visual_policy
                .enabled_plugins
                .iter()
                .chain(&policy.visual_policy.preferred)
                .chain(&policy.visual_policy.disabled)
                .any(|id| id.is_empty() || id.len() > 256)
        {
            return Err("visual policy exceeds bounds");
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
                raised: self.style("component.raised"),
                overlay: self.style("component.overlay"),
                scrim: self
                    .styles
                    .contains_key("component.scrim")
                    .then(|| self.style("component.scrim")),
            },
            text: self.style("text"),
            focused: self.style("focused"),
            selected: self.style("selection"),
            disabled: self.style("component.disabled"),
            muted: self.style("muted"),
            info: self.style("component.info"),
            success: self.style("component.success"),
            warning: self.style("component.warning"),
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
    fn multi_tap_uses_monotonic_window_and_never_repeats_within_one_event() {
        let json = serde_json::json!([{
            "scope":"chat", "chord":[BrowserStroke::from_key(key("escape"))],
            "binding":{"type":"host","action":"app.interrupt"},
            "multi_tap":{"taps":2,"window_ms":1500}
        }]);
        let mut bindings = PolicyBindings::default();
        bindings.replace(BrowserPolicy::parse(&policy(&json)).unwrap());
        assert!(matches!(
            bindings.resolve("chat", key("escape")),
            Some(BrowserResolvedBinding::Ignored)
        ));
        bindings.set_event_time(100);
        bindings.begin_event();
        assert!(matches!(
            bindings.resolve("chat", key("escape")),
            Some(BrowserResolvedBinding::Ignored)
        ));
        assert!(matches!(
            bindings.resolve("chat", key("escape")),
            Some(BrowserResolvedBinding::Ignored)
        ));
        bindings.set_event_time(1700);
        bindings.begin_event();
        assert!(matches!(
            bindings.resolve("chat", key("escape")),
            Some(BrowserResolvedBinding::Ignored)
        ));
        bindings.set_event_time(1800);
        bindings.begin_event();
        assert!(matches!(
            bindings.resolve("chat", key("escape")),
            Some(BrowserResolvedBinding::Host { .. })
        ));
        bindings.cancel_chord();
        bindings.set_event_time(1900);
        bindings.begin_event();
        assert!(matches!(
            bindings.resolve("chat", key("escape")),
            Some(BrowserResolvedBinding::Ignored)
        ));
        bindings.set_event_time(5);
        bindings.begin_event();
        assert!(matches!(
            bindings.resolve("chat", key("escape")),
            Some(BrowserResolvedBinding::Ignored)
        ));
    }

    #[test]
    fn component_roles_are_exact_and_style_count_is_bounded() {
        let mut styles = BTreeMap::new();
        let roles = [
            "surface",
            "text",
            "focused",
            "selection",
            "muted",
            "error",
            "border",
            "component.raised",
            "component.overlay",
            "component.disabled",
            "component.info",
            "component.success",
            "component.warning",
        ];
        for (index, role) in roles.iter().enumerate() {
            styles.insert(
                (*role).to_owned(),
                BrowserStyle {
                    foreground_palette: Some(u8::try_from(index).unwrap()),
                    ..BrowserStyle::default()
                },
            );
        }
        let mut policy = BrowserPolicy {
            schema_version: 1,
            revision: "roles".into(),
            styles,
            ..BrowserPolicy::default()
        };
        let theme = policy.component_theme();
        for (actual, role) in [
            (theme.canvas, "surface"),
            (theme.surfaces.normal, "surface"),
            (theme.surfaces.raised, "component.raised"),
            (theme.surfaces.overlay, "component.overlay"),
            (theme.text, "text"),
            (theme.focused, "focused"),
            (theme.selected, "selection"),
            (theme.disabled, "component.disabled"),
            (theme.muted, "muted"),
            (theme.info, "component.info"),
            (theme.success, "component.success"),
            (theme.warning, "component.warning"),
            (theme.error, "error"),
            (theme.border, "border"),
        ] {
            assert_eq!(actual, policy.style(role), "{role}");
        }
        assert!(theme.surfaces.scrim.is_none());
        policy
            .styles
            .insert("component.scrim".into(), BrowserStyle::default());
        assert_eq!(policy.component_theme().surfaces.scrim, Some(Style::new()));
        while policy.styles.len() < 64 {
            policy.styles.insert(
                format!("role{}", policy.styles.len()),
                BrowserStyle::default(),
            );
        }
        assert!(BrowserPolicy::parse(&serde_json::to_string(&policy).unwrap()).is_ok());
        policy
            .styles
            .insert("excess".into(), BrowserStyle::default());
        assert!(BrowserPolicy::parse(&serde_json::to_string(&policy).unwrap()).is_err());
    }

    #[test]
    fn policy_binding_and_serialized_limits_roundtrip_without_truncation() {
        let bindings = (0..4096)
            .map(|index| {
                serde_json::json!({
                    "scope":format!("plugin.{index}"),
                    "chord":[BrowserStroke::from_key(key("enter"))],
                    "binding":{"type":"host","action":format!("plugin.{index}.activate")}
                })
            })
            .collect::<Vec<_>>();
        let mut json = policy(&serde_json::json!(bindings));
        assert!(json.len() > 65536);
        let parsed = BrowserPolicy::parse(&json).unwrap();
        assert_eq!(parsed.bindings.len(), 4096);
        for (actual, expected) in parsed.bindings.iter().zip(&bindings) {
            let expected: BrowserBinding = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        json.push_str(&" ".repeat(1024 * 1024 - json.len()));
        assert_eq!(json.len(), 1024 * 1024);
        assert_eq!(BrowserPolicy::parse(&json).unwrap().bindings.len(), 4096);
        json.push(' ');
        assert_eq!(
            BrowserPolicy::parse(&json).unwrap_err(),
            "presentation policy too large"
        );
        let mut oversized = bindings.clone();
        oversized.push(serde_json::json!({"scope":"plugin.extra","chord":[BrowserStroke::from_key(key("enter"))],"binding":{"type":"ignored"}}));
        assert!(BrowserPolicy::parse(&policy(&serde_json::json!(oversized))).is_err());
        for (field, value) in [
            ("scope", serde_json::json!("x".repeat(129))),
            (
                "chord",
                serde_json::json!(vec![BrowserStroke::from_key(key("enter")); 5]),
            ),
        ] {
            let mut invalid = bindings[0].clone();
            invalid[field] = value;
            assert!(BrowserPolicy::parse(&policy(&serde_json::json!([invalid]))).is_err());
        }
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
