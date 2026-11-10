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
        take!(icons, density, speaker, surfaces, motion);
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
}
