#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! User contrast preferences for bcode themes.
//!
//! Contrast is adjusted on top of a theme's exact colors after the theme is resolved. These
//! types are the configuration shape (`[tui.theme]` keys `contrast`, `minimum_contrast`,
//! `contrast_parts` and the per-theme `[tui.theme.overrides.<theme-id>]` tables) shared by
//! Bcode's typed configuration, the standalone review and git apps, and theme resolution.
//!
//! * [`ContrastFactor`] scales how far each color sits from the background it is painted on.
//! * [`MinimumContrast`] is an optional WCAG contrast-ratio floor.
//! * [`ContrastParts`] selects which groups of colors are adjusted.
//! * [`ContrastPreferences`] holds the global values and the per-theme overrides and answers
//!   the [`ContrastSettings`] of one theme.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Smallest accepted contrast factor.
pub const CONTRAST_FACTOR_MIN: f64 = 0.5;
/// Largest accepted contrast factor.
pub const CONTRAST_FACTOR_MAX: f64 = 2.0;
/// Step of the interactive contrast slider.
pub const CONTRAST_FACTOR_STEP: f64 = 0.05;
/// Largest WCAG contrast ratio.
pub const MAXIMUM_CONTRAST_RATIO: f64 = 21.0;
/// Floors the interactive minimum-contrast control cycles through after "off".
pub const MINIMUM_CONTRAST_PRESETS: [f64; 3] = [3.0, 4.5, 7.0];

/// Why a contrast value was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContrastValueError(String);

impl fmt::Display for ContrastValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ContrastValueError {}

/// How far each adjusted color sits from the background it is painted on, relative to the
/// theme.
///
/// `1.0` keeps the theme's exact colors, larger values push lightness away from the
/// background and smaller values pull it toward the background. Always finite and within [`CONTRAST_FACTOR_MIN`]..=[`CONTRAST_FACTOR_MAX`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct ContrastFactor(f64);

// Every value is finite (see `new`), so equality is reflexive.
impl Eq for ContrastFactor {}

impl ContrastFactor {
    /// The theme's exact colors.
    pub const EXACT: Self = Self(1.0);

    /// A factor of `value`.
    ///
    /// # Errors
    ///
    /// * When `value` is not finite or lies outside
    ///   [`CONTRAST_FACTOR_MIN`]..=[`CONTRAST_FACTOR_MAX`].
    pub fn new(value: f64) -> Result<Self, ContrastValueError> {
        if value.is_finite() && (CONTRAST_FACTOR_MIN..=CONTRAST_FACTOR_MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ContrastValueError(format!(
                "contrast must be a number from {CONTRAST_FACTOR_MIN} to {CONTRAST_FACTOR_MAX}, \
                 got {value}"
            )))
        }
    }

    /// The factor as a number.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    /// Whether this factor keeps the theme's exact colors.
    #[must_use]
    pub fn is_exact(self) -> bool {
        self == Self::EXACT
    }

    /// This factor moved by `steps` slider steps ([`CONTRAST_FACTOR_STEP`]), rounded to the
    /// step grid and clamped to the accepted range.
    #[must_use]
    pub fn stepped(self, steps: i32) -> Self {
        let grid = (self.0 / CONTRAST_FACTOR_STEP).round() + f64::from(steps);
        let value = (grid * CONTRAST_FACTOR_STEP).clamp(CONTRAST_FACTOR_MIN, CONTRAST_FACTOR_MAX);
        // Rounding to hundredths keeps repeated steps on exact decimal values.
        Self((value * 100.0).round() / 100.0)
    }
}

impl Default for ContrastFactor {
    fn default() -> Self {
        Self::EXACT
    }
}

impl TryFrom<f64> for ContrastFactor {
    type Error = ContrastValueError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ContrastFactor> for f64 {
    fn from(factor: ContrastFactor) -> Self {
        factor.0
    }
}

impl fmt::Display for ContrastFactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:.2}", self.0)
    }
}

/// An optional floor on the WCAG contrast ratio of every adjusted color against its
/// background. Written as a number from 1 to 21, or `"off"`.
///
/// When set, a color still below the floor after the [`ContrastFactor`] changes lightness by
/// the minimum needed to reach it; colors already at or above it stay as they are.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd)]
pub struct MinimumContrast(Option<f64>);

// Every ratio is finite (see `new`), so equality is reflexive.
impl Eq for MinimumContrast {}

impl MinimumContrast {
    /// No floor.
    pub const OFF: Self = Self(None);

    /// A floor of `ratio`.
    ///
    /// # Errors
    ///
    /// * When `ratio` is not finite or lies outside 1..=[`MAXIMUM_CONTRAST_RATIO`].
    pub fn new(ratio: f64) -> Result<Self, ContrastValueError> {
        if ratio.is_finite() && (1.0..=MAXIMUM_CONTRAST_RATIO).contains(&ratio) {
            Ok(Self(Some(ratio)))
        } else {
            Err(ContrastValueError(format!(
                "minimum_contrast must be \"off\" or a ratio from 1 to {MAXIMUM_CONTRAST_RATIO}, \
                 got {ratio}"
            )))
        }
    }

    /// The floor ratio, or `None` when off.
    #[must_use]
    pub const fn ratio(self) -> Option<f64> {
        self.0
    }

    /// The next value of the interactive cycle: off, then each of
    /// [`MINIMUM_CONTRAST_PRESETS`], then off again. A floor between presets moves to the next
    /// larger preset.
    #[must_use]
    pub fn next(self) -> Self {
        self.0
            .map_or(Self(Some(MINIMUM_CONTRAST_PRESETS[0])), |ratio| {
                MINIMUM_CONTRAST_PRESETS
                    .iter()
                    .find(|preset| **preset > ratio)
                    .map_or(Self::OFF, |preset| Self(Some(*preset)))
            })
    }

    /// The previous value of the interactive cycle ([`Self::next`] in reverse).
    #[must_use]
    pub fn previous(self) -> Self {
        let last = MINIMUM_CONTRAST_PRESETS[MINIMUM_CONTRAST_PRESETS.len() - 1];
        self.0.map_or(Self(Some(last)), |ratio| {
            MINIMUM_CONTRAST_PRESETS
                .iter()
                .rev()
                .find(|preset| **preset < ratio)
                .map_or(Self::OFF, |preset| Self(Some(*preset)))
        })
    }
}

impl fmt::Display for MinimumContrast {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => formatter.write_str("off"),
            Some(ratio) => write!(formatter, "{ratio:.1}"),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MinimumContrastWire {
    Ratio(f64),
    Word(String),
}

impl<'de> Deserialize<'de> for MinimumContrast {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match MinimumContrastWire::deserialize(deserializer)? {
            MinimumContrastWire::Ratio(ratio) => Self::new(ratio).map_err(serde::de::Error::custom),
            MinimumContrastWire::Word(word) if word == "off" => Ok(Self::OFF),
            MinimumContrastWire::Word(word) => Err(serde::de::Error::custom(format!(
                "minimum_contrast must be \"off\" or a ratio from 1 to {MAXIMUM_CONTRAST_RATIO}, \
                 got \"{word}\""
            ))),
        }
    }
}

impl Serialize for MinimumContrast {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            None => serializer.serialize_str("off"),
            Some(ratio) => serializer.serialize_f64(ratio),
        }
    }
}

const fn on() -> bool {
    true
}

/// Which groups of colors contrast adjustments change (`contrast_parts`). Every part is on
/// unless set to `false`.
// One switch per part is the configuration shape (`contrast_parts = { ansi = false }`).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContrastParts {
    /// Foregrounds of text, state, accent, link, Markdown, border and other non-code roles,
    /// against the background each is painted on.
    #[serde(default = "on")]
    pub text: bool,
    /// Syntax colors (`syntax.*` and `markdown.code.*`) against the code background.
    #[serde(default = "on")]
    pub syntax: bool,
    /// Fills (surfaces, selection, line highlight, diff rows, state surfaces), pushed away from
    /// the canvas; text on them is then measured against the changed fill.
    #[serde(default = "on")]
    pub fills: bool,
    /// The theme's ANSI palette for terminal output, against the terminal background.
    #[serde(default = "on")]
    pub ansi: bool,
}

impl ContrastParts {
    /// Every part.
    pub const ALL: Self = Self {
        text: true,
        syntax: true,
        fills: true,
        ansi: true,
    };
}

impl Default for ContrastParts {
    fn default() -> Self {
        Self::ALL
    }
}

/// Per-theme contrast values (`[tui.theme.overrides.<theme-id>]`); each set value replaces the
/// global one for that theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContrastOverride {
    /// Replaces the global `contrast`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<ContrastFactor>,
    /// Replaces the global `minimum_contrast`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_contrast: Option<MinimumContrast>,
    /// Replaces the global `contrast_parts` as a whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast_parts: Option<ContrastParts>,
}

impl ContrastOverride {
    /// Whether this override sets nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.contrast.is_none() && self.minimum_contrast.is_none() && self.contrast_parts.is_none()
    }

    /// This override with every value `over` sets replacing this one's.
    #[must_use]
    pub const fn overlaid_with(self, over: Self) -> Self {
        Self {
            contrast: match over.contrast {
                Some(contrast) => Some(contrast),
                None => self.contrast,
            },
            minimum_contrast: match over.minimum_contrast {
                Some(minimum) => Some(minimum),
                None => self.minimum_contrast,
            },
            contrast_parts: match over.contrast_parts {
                Some(parts) => Some(parts),
                None => self.contrast_parts,
            },
        }
    }
}

/// The contrast adjustments applied to one resolved theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContrastSettings {
    /// Lightness factor.
    pub factor: ContrastFactor,
    /// Contrast-ratio floor.
    pub minimum: MinimumContrast,
    /// Adjusted parts.
    pub parts: ContrastParts,
}

impl ContrastSettings {
    /// The theme's exact colors.
    pub const EXACT: Self = Self {
        factor: ContrastFactor::EXACT,
        minimum: MinimumContrast::OFF,
        parts: ContrastParts::ALL,
    };

    /// Whether these settings leave every color exact: factor `1.0`, no floor, or no part.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        let parts = self.parts;
        (self.factor.is_exact() && self.minimum.ratio().is_none())
            || !(parts.text || parts.syntax || parts.fills || parts.ansi)
    }
}

/// Global contrast values and per-theme overrides, as configured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContrastPreferences {
    /// Lightness factor for every theme without an override.
    pub contrast: ContrastFactor,
    /// Contrast-ratio floor for every theme without an override.
    pub minimum_contrast: MinimumContrast,
    /// Adjusted parts for every theme without an override.
    pub contrast_parts: ContrastParts,
    /// Overrides keyed by theme (palette) id.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, ContrastOverride>,
}

impl ContrastPreferences {
    /// The settings for the theme `theme_id`: its override's values where set, else the global
    /// ones.
    #[must_use]
    pub fn settings_for(&self, theme_id: &str) -> ContrastSettings {
        let theme = self.overrides.get(theme_id).copied().unwrap_or_default();
        ContrastSettings {
            factor: theme.contrast.unwrap_or(self.contrast),
            minimum: theme.minimum_contrast.unwrap_or(self.minimum_contrast),
            parts: theme.contrast_parts.unwrap_or(self.contrast_parts),
        }
    }

    /// Overlay saved per-theme values (from the appearance picker) on the configured
    /// overrides: each value a saved override sets replaces the configured one.
    pub fn apply_saved(&mut self, saved: &BTreeMap<String, ContrastOverride>) {
        for (theme, values) in saved {
            let merged = self
                .overrides
                .get(theme)
                .copied()
                .unwrap_or_default()
                .overlaid_with(*values);
            if merged.is_empty() {
                self.overrides.remove(theme);
            } else {
                self.overrides.insert(theme.clone(), merged);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preferences(source: &str) -> Result<ContrastPreferences, String> {
        toml::from_str(source).map_err(|error| error.to_string())
    }

    #[test]
    fn defaults_keep_exact_colors_with_every_part() {
        let defaults = preferences("").expect("empty");
        assert_eq!(defaults, ContrastPreferences::default());
        let settings = defaults.settings_for("nord");
        assert_eq!(settings, ContrastSettings::EXACT);
        assert!(settings.is_exact());
    }

    #[test]
    fn global_values_parse_and_apply_to_every_theme() {
        let parsed = preferences(
            "
            contrast = 1.2
            minimum_contrast = 4.5
            contrast_parts = { ansi = false }
            ",
        )
        .expect("parses");
        let settings = parsed.settings_for("solarized");
        assert!((settings.factor.get() - 1.2).abs() < f64::EPSILON);
        assert_eq!(settings.minimum.ratio(), Some(4.5));
        assert_eq!(
            settings.parts,
            ContrastParts {
                ansi: false,
                ..ContrastParts::ALL
            }
        );
        assert!(!settings.is_exact());
        // An integer is a factor too.
        assert_eq!(
            preferences("contrast = 2").expect("integer").contrast.get(),
            2.0
        );
    }

    #[test]
    fn a_theme_override_beats_the_global_values_it_sets() {
        let parsed = preferences(
            r#"
            contrast = 1.2
            minimum_contrast = 3

            [overrides.solarized-light]
            contrast = 1.5
            minimum_contrast = "off"
            "#,
        )
        .expect("parses");
        let solarized = parsed.settings_for("solarized-light");
        assert_eq!(solarized.factor.get(), 1.5);
        assert_eq!(solarized.minimum, MinimumContrast::OFF);
        let other = parsed.settings_for("nord");
        assert_eq!(other.factor.get(), 1.2);
        assert_eq!(other.minimum.ratio(), Some(3.0));
    }

    #[test]
    fn invalid_values_are_rejected_with_the_accepted_range() {
        for (source, expected) in [
            ("contrast = 0.2", "from 0.5 to 2"),
            ("contrast = 2.5", "from 0.5 to 2"),
            ("contrast = nan", "from 0.5 to 2"),
            ("contrast = \"high\"", "invalid type"),
            ("minimum_contrast = 0.5", "ratio from 1 to 21"),
            ("minimum_contrast = 22", "ratio from 1 to 21"),
            ("minimum_contrast = \"on\"", "ratio from 1 to 21"),
            ("contrast_parts = { colors = true }", "unknown field"),
            ("[overrides.nord]\nfactor = 1.2", "unknown field"),
        ] {
            let error = preferences(source).expect_err(source);
            assert!(error.contains(expected), "{source}: {error}");
        }
    }

    #[test]
    fn saved_values_replace_configured_override_values_one_by_one() {
        let mut configured = preferences(
            "
            [overrides.nord]
            contrast = 1.5
            minimum_contrast = 4.5
            ",
        )
        .expect("parses");
        configured.apply_saved(&BTreeMap::from([
            (
                "nord".to_owned(),
                ContrastOverride {
                    contrast: Some(ContrastFactor::new(0.8).expect("factor")),
                    ..ContrastOverride::default()
                },
            ),
            ("dracula".to_owned(), ContrastOverride::default()),
        ]));
        let nord = configured.settings_for("nord");
        assert_eq!(nord.factor.get(), 0.8);
        assert_eq!(nord.minimum.ratio(), Some(4.5));
        assert!(!configured.overrides.contains_key("dracula"));
    }

    #[test]
    fn slider_steps_stay_on_the_grid_and_in_range() {
        let mut factor = ContrastFactor::EXACT;
        for _ in 0..4 {
            factor = factor.stepped(1);
        }
        assert_eq!(factor.get(), 1.2);
        assert_eq!(factor.to_string(), "1.20");
        assert_eq!(
            ContrastFactor::EXACT.stepped(-100).get(),
            CONTRAST_FACTOR_MIN
        );
        assert_eq!(
            ContrastFactor::EXACT.stepped(100).get(),
            CONTRAST_FACTOR_MAX
        );
        // An off-grid configured value snaps to the grid on its first step.
        assert_eq!(
            ContrastFactor::new(1.23).expect("factor").stepped(1).get(),
            1.3
        );
    }

    #[test]
    fn minimum_contrast_cycles_off_and_the_presets() {
        let mut minimum = MinimumContrast::OFF;
        let mut seen = Vec::new();
        for _ in 0..4 {
            minimum = minimum.next();
            seen.push(minimum.to_string());
        }
        assert_eq!(seen, ["3.0", "4.5", "7.0", "off"]);
        assert_eq!(MinimumContrast::OFF.previous().ratio(), Some(7.0));
        assert_eq!(
            MinimumContrast::new(5.0).expect("floor").next().ratio(),
            Some(7.0)
        );
        assert_eq!(
            MinimumContrast::new(3.0).expect("floor").previous(),
            MinimumContrast::OFF
        );
    }

    #[test]
    fn preferences_round_trip_through_toml() {
        let parsed = preferences(
            r#"
            contrast = 1.25
            minimum_contrast = 7.0
            [overrides.nord]
            minimum_contrast = "off"
            contrast_parts = { text = true, syntax = false, fills = true, ansi = true }
            "#,
        )
        .expect("parses");
        let written = toml::to_string(&parsed).expect("serializes");
        assert_eq!(preferences(&written).expect("reparses"), parsed);
    }
}
