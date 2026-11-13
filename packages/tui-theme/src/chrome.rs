//! Renderer chrome: the shape of the interface, independent of color.
//!
//! `[chrome.tui]` selects border glyphs, focus effects, icon sets, density,
//! separators, speaker-label treatment, surface layering, and motion. Every
//! value is a closed enumeration mapped onto BMUX primitives by the terminal
//! renderer; themes never supply layout code. Sections for other renderers
//! (`[chrome.<renderer>]`) are preserved as opaque data for those renderers.

use std::collections::BTreeMap;

use bmux_tui::style::Color;
use serde::{Deserialize, Serialize};

/// Border glyph family.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BorderStyle {
    /// No border.
    None,
    /// ASCII-safe `+-|`.
    Ascii,
    /// Single line.
    #[default]
    Single,
    /// Rounded corners.
    Rounded,
    /// Heavy lines.
    Thick,
    /// Double lines.
    Double,
    /// A single leading bar only (transcript containers).
    Left,
    /// Full-width horizontal rules above and below, no sides (the composer).
    Rules,
}

/// Focus effect for focused borders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusEffect {
    /// Focus uses the focused border style only.
    #[default]
    None,
    /// Static gradient across the focused border.
    Gradient,
    /// Gradient that sweeps around the border (respects reduced motion).
    Sweep,
}

/// Icon set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IconSet {
    /// No icons; text labels only.
    None,
    /// Portable Unicode symbols.
    #[default]
    Unicode,
    /// Nerd Font glyphs (requires a patched font; falls back to Unicode width rules).
    Nerd,
}

/// Spacing density.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// No padding; for small terminals.
    Compact,
    /// Balanced spacing.
    #[default]
    Cozy,
    /// Generous padding.
    Comfortable,
}

/// Separator glyph style between status segments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeparatorStyle {
    /// Middle dot / spaces.
    #[default]
    Plain,
    /// Vertical bar.
    Bar,
    /// Slanted wedges.
    Slant,
    /// Powerline arrows (requires a powerline-capable font).
    Powerline,
}

/// Speaker-label treatment in the transcript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerStyle {
    /// Plain text prefix.
    #[default]
    Prefix,
    /// Filled badge.
    Badge,
    /// Colored bar along the message.
    Bar,
    /// Label in a left gutter.
    Gutter,
    /// No heading row: the speaker's marker glyph (`markers`) precedes the first body row and
    /// the body hangs beside it.
    Marker,
}

/// Surface layering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceStyle {
    /// Raised and overlay surfaces share the canvas.
    #[default]
    Flat,
    /// Raised and overlay surfaces use their own fills.
    Layered,
}

/// Motion level for animated chrome.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Motion {
    /// No animation.
    None,
    /// Subtle, low-frequency animation.
    #[default]
    Subtle,
    /// Full animation.
    Full,
}

/// Whether a piece of screen chrome is drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Drawn.
    #[default]
    Shown,
    /// Not drawn.
    Hidden,
}

impl Visibility {
    /// Whether the chrome is drawn.
    #[must_use]
    pub const fn is_shown(self) -> bool {
        matches!(self, Self::Shown)
    }
}

/// Where the status line sits relative to the composer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusPlacement {
    /// Between the transcript and the composer.
    #[default]
    AboveComposer,
    /// Below the composer, as a footer.
    BelowComposer,
}

/// Status line placement and indentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StatusChrome {
    /// Placement relative to the composer.
    pub placement: StatusPlacement,
    /// Leading blank columns, at most [`MAX_STATUS_INDENT`].
    pub indent: u8,
}

/// Widest status line indent.
pub const MAX_STATUS_INDENT: u8 = 8;

/// Longest [`ChromeText`], in bytes.
pub const MAX_CHROME_TEXT_BYTES: usize = 48;

/// Short single-line chrome text: a glyph, prompt, or placeholder.
///
/// At most [`MAX_CHROME_TEXT_BYTES`] bytes and no control characters. Stored inline so resolved
/// chrome stays `Copy`; renderers measure it with their text-layout primitives.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChromeText {
    len: u8,
    bytes: [u8; MAX_CHROME_TEXT_BYTES],
}

impl ChromeText {
    /// `text` when it fits and contains no control characters.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        if text.len() > MAX_CHROME_TEXT_BYTES || text.chars().any(char::is_control) {
            return None;
        }
        let mut bytes = [0; MAX_CHROME_TEXT_BYTES];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Some(Self {
            len: u8::try_from(text.len()).ok()?,
            bytes,
        })
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

impl std::fmt::Debug for ChromeText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

impl Serialize for ChromeText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ChromeText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::new(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "chrome text must be at most {MAX_CHROME_TEXT_BYTES} bytes without control characters"
            ))
        })
    }
}

/// Glyphs of the `marker` speaker treatment; `None` uses the renderer's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageMarkers {
    /// Before user messages.
    pub user: Option<ChromeText>,
    /// Before assistant messages.
    pub assistant: Option<ChromeText>,
}

/// The input composer's chrome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComposerChrome {
    /// Glyph drawn before the first input row; `None` draws none.
    pub prompt: Option<ChromeText>,
    /// The frame title, drawn by full frames only.
    pub title: Visibility,
    /// Placeholder of an empty input; `None` keeps the host's.
    pub placeholder: Option<ChromeText>,
    /// The send and send-options controls.
    pub controls: Visibility,
}

/// Border style per surface; `None` keeps each component's own default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TuiBorders {
    /// Panels and panes.
    pub panel: Option<BorderStyle>,
    /// Dialogs and modals.
    pub dialog: Option<BorderStyle>,
    /// Tool cards.
    pub tool: Option<BorderStyle>,
    /// The composer.
    pub composer: Option<BorderStyle>,
}

/// Resolved terminal chrome.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct TuiChrome {
    /// Border style per surface.
    pub borders: TuiBorders,
    /// Focus effect.
    pub focus: FocusEffect,
    /// Gradient colors for focus effects; empty uses the focused border color.
    pub focus_colors: Vec<Color>,
    /// Icon set.
    pub icons: IconSet,
    /// Spacing density.
    pub density: Density,
    /// Status separators.
    pub separators: SeparatorStyle,
    /// Speaker-label treatment.
    pub speaker: SpeakerStyle,
    /// Surface layering.
    pub surfaces: SurfaceStyle,
    /// Motion level.
    pub motion: Motion,
    /// The header row at the top of the screen.
    pub header: Visibility,
    /// The marker at the start of a session's transcript.
    pub session_start: Visibility,
    /// Status line placement.
    pub status: StatusChrome,
    /// The input composer.
    pub composer: ComposerChrome,
    /// Speaker marker glyphs.
    pub markers: MessageMarkers,
}

/// Maximum focus gradient colors.
pub const MAX_FOCUS_COLORS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBorders {
    #[serde(default)]
    pub panel: Option<BorderStyle>,
    #[serde(default)]
    pub dialog: Option<BorderStyle>,
    #[serde(default)]
    pub tool: Option<BorderStyle>,
    #[serde(default)]
    pub composer: Option<BorderStyle>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawFocus {
    #[serde(default)]
    pub effect: Option<FocusEffect>,
    #[serde(default)]
    pub colors: Option<Vec<crate::definition::RawColor>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSeparators {
    #[serde(default)]
    pub status: Option<SeparatorStyle>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawStatus {
    #[serde(default)]
    pub placement: Option<StatusPlacement>,
    #[serde(default)]
    pub indent: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawComposer {
    #[serde(default)]
    pub prompt: Option<ChromeText>,
    #[serde(default)]
    pub title: Option<Visibility>,
    #[serde(default)]
    pub placeholder: Option<ChromeText>,
    #[serde(default)]
    pub controls: Option<Visibility>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMarkers {
    #[serde(default)]
    pub user: Option<ChromeText>,
    #[serde(default)]
    pub assistant: Option<ChromeText>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawTuiChrome {
    #[serde(default)]
    pub borders: Option<RawBorders>,
    #[serde(default)]
    pub focus: Option<RawFocus>,
    #[serde(default)]
    pub icons: Option<IconSet>,
    #[serde(default)]
    pub density: Option<Density>,
    #[serde(default)]
    pub separators: Option<RawSeparators>,
    #[serde(default)]
    pub speaker: Option<SpeakerStyle>,
    #[serde(default)]
    pub surfaces: Option<SurfaceStyle>,
    #[serde(default)]
    pub motion: Option<Motion>,
    #[serde(default)]
    pub header: Option<Visibility>,
    #[serde(default)]
    pub session_start: Option<Visibility>,
    #[serde(default)]
    pub status: Option<RawStatus>,
    #[serde(default)]
    pub composer: Option<RawComposer>,
    #[serde(default)]
    pub markers: Option<RawMarkers>,
}

/// `[chrome]` table: the terminal section plus opaque sections for other renderers.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct RawChrome {
    #[serde(default)]
    pub tui: Option<RawTuiChrome>,
    #[serde(flatten)]
    pub other: BTreeMap<String, toml::Value>,
}

impl RawTuiChrome {
    /// Overlay `other`; set fields in `other` win, nested tables merge per field.
    pub(crate) fn merge(&mut self, other: &Self) {
        if let Some(borders) = &other.borders {
            let target = self.borders.get_or_insert_with(RawBorders::default);
            for (slot, value) in [
                (&mut target.panel, borders.panel),
                (&mut target.dialog, borders.dialog),
                (&mut target.tool, borders.tool),
                (&mut target.composer, borders.composer),
            ] {
                if value.is_some() {
                    *slot = value;
                }
            }
        }
        if let Some(focus) = &other.focus {
            let target = self.focus.get_or_insert_with(RawFocus::default);
            if focus.effect.is_some() {
                target.effect = focus.effect;
            }
            if focus.colors.is_some() {
                target.colors.clone_from(&focus.colors);
            }
        }
        if let Some(separators) = &other.separators
            && separators.status.is_some()
        {
            self.separators
                .get_or_insert_with(RawSeparators::default)
                .status = separators.status;
        }
        macro_rules! take {
            ($($field:ident),*) => {$(
                if other.$field.is_some() {
                    self.$field = other.$field;
                }
            )*};
        }
        take!(
            icons,
            density,
            speaker,
            surfaces,
            motion,
            header,
            session_start
        );
        if let Some(status) = &other.status {
            let target = self.status.get_or_insert_with(RawStatus::default);
            if status.placement.is_some() {
                target.placement = status.placement;
            }
            if status.indent.is_some() {
                target.indent = status.indent;
            }
        }
        if let Some(markers) = &other.markers {
            let target = self.markers.get_or_insert_with(RawMarkers::default);
            if markers.user.is_some() {
                target.user = markers.user;
            }
            if markers.assistant.is_some() {
                target.assistant = markers.assistant;
            }
        }
        if let Some(composer) = &other.composer {
            let target = self.composer.get_or_insert_with(RawComposer::default);
            if composer.prompt.is_some() {
                target.prompt = composer.prompt;
            }
            if composer.title.is_some() {
                target.title = composer.title;
            }
            if composer.placeholder.is_some() {
                target.placeholder = composer.placeholder;
            }
            if composer.controls.is_some() {
                target.controls = composer.controls;
            }
        }
    }
}

impl RawChrome {
    pub(crate) fn merge(&mut self, other: &Self) {
        if let Some(tui) = &other.tui {
            self.tui
                .get_or_insert_with(RawTuiChrome::default)
                .merge(tui);
        }
        crate::definition::deep_merge_extensions(&mut self.other, &other.other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_merges_field_by_field() {
        let mut base: RawChrome = toml::from_str(
            r#"
            [tui]
            borders = { panel = "rounded", dialog = "thick" }
            icons = "nerd"
            "#,
        )
        .unwrap();
        let overlay: RawChrome = toml::from_str(
            r#"
            [tui]
            borders = { panel = "ascii" }
            motion = "none"

            [web]
            radius = 6
            "#,
        )
        .unwrap();
        base.merge(&overlay);
        let tui = base.tui.unwrap();
        let borders = tui.borders.unwrap();
        assert_eq!(borders.panel, Some(BorderStyle::Ascii));
        assert_eq!(borders.dialog, Some(BorderStyle::Thick));
        assert_eq!(tui.icons, Some(IconSet::Nerd));
        assert_eq!(tui.motion, Some(Motion::None));
        assert!(base.other.contains_key("web"));
    }

    #[test]
    fn unknown_tui_chrome_keys_are_rejected() {
        assert!(toml::from_str::<RawChrome>("[tui]\nborder = \"rounded\"").is_err());
        assert!(toml::from_str::<RawChrome>("[tui]\nicons = \"emoji\"").is_err());
    }

    #[test]
    fn screen_chrome_merges_per_field_and_validates_text() {
        let mut base: RawChrome = toml::from_str(
            r#"
            [tui]
            header = "hidden"
            status = { placement = "below_composer", indent = 2 }
            composer = { prompt = "> ", controls = "hidden" }
            "#,
        )
        .unwrap();
        let overlay: RawChrome = toml::from_str(
            r#"
            [tui]
            status = { indent = 4 }
            composer = { prompt = "❯ ", title = "hidden" }
            "#,
        )
        .unwrap();
        base.merge(&overlay);
        let tui = base.tui.unwrap();
        assert_eq!(tui.header, Some(Visibility::Hidden));
        let status = tui.status.unwrap();
        assert_eq!(status.placement, Some(StatusPlacement::BelowComposer));
        assert_eq!(status.indent, Some(4));
        let composer = tui.composer.unwrap();
        assert_eq!(
            composer.prompt.map(|text| text.as_str().to_owned()),
            Some("❯ ".to_owned())
        );
        assert_eq!(composer.title, Some(Visibility::Hidden));
        assert_eq!(composer.controls, Some(Visibility::Hidden));
        // Chrome text is one bounded line.
        assert!(toml::from_str::<RawChrome>("[tui]\ncomposer = { prompt = \"a\\nb\" }").is_err());
        let long = "x".repeat(MAX_CHROME_TEXT_BYTES + 1);
        assert!(
            toml::from_str::<RawChrome>(&format!("[tui]\ncomposer = {{ prompt = \"{long}\" }}"))
                .is_err()
        );
        assert!(toml::from_str::<RawChrome>("[tui]\nheader = \"faded\"").is_err());
    }
}
