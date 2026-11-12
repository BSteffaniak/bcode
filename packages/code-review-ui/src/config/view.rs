//! Diff layout policy and glyph set of the review screen.
//!
//! The default follows bcode's `[tui.diff_viewer]` settings, the review's own configuration
//! (`[plugins.config."bcode.code_review".review_v2]`) overrides it, and the in-review toggle
//! overrides both for the session without writing configuration. The same table selects the
//! review's glyph set (`icons = "ascii"` for terminals without the Unicode symbols).
//!
//! `viewed_reset` in the plugin table itself (`[plugins.config."bcode.code_review"]`) chooses
//! when a viewed file stops counting as viewed: `"diff"` (the default) while its changes stay
//! the same, `"file"` when either side of the file changes.

use bcode_code_review::document::LayoutMode;
use serde::Deserialize;

/// Diff layout policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLayout {
    /// Unified below `breakpoint` columns, side-by-side at or above it.
    Auto {
        /// Width at which the layout becomes side-by-side.
        breakpoint: u16,
    },
    /// Always unified.
    Unified,
    /// Always side-by-side.
    SideBySide,
}

/// Side-by-side breakpoint when nobody configures one.
pub const DEFAULT_BREAKPOINT: u16 = 120;

/// Tab stop interval of diff text when the configuration sets none.
pub const DEFAULT_TAB_WIDTH: u16 = 4;

/// Largest configurable tab stop interval.
const MAX_TAB_WIDTH: u16 = 16;

/// Layout configured in the code review plugin's section
/// (`[plugins.config."bcode.code_review".review_v2]`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct ReviewViewConfig {
    /// `auto`, `unified`, or `side_by_side`.
    #[serde(default)]
    pub diff_layout: Option<ConfiguredLayout>,
    /// Width at which `auto` switches to side-by-side.
    #[serde(default)]
    pub side_by_side_breakpoint: Option<u16>,
    /// Glyph set: `unicode` (default) or `ascii`.
    #[serde(default)]
    pub icons: Option<crate::theme::IconStyle>,
    /// Tab stop interval of diff text, in cells (default [`DEFAULT_TAB_WIDTH`]).
    #[serde(default)]
    pub tab_width: Option<u16>,
    /// What makes a viewed file stale (`viewed_reset` in the plugin table, not `review_v2`).
    #[serde(skip)]
    pub viewed_reset: bcode_code_review::viewed::ViewedReset,
}

/// Configured layout choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfiguredLayout {
    /// Unified below the breakpoint, side-by-side at or above it.
    Auto,
    /// Always unified.
    Unified,
    /// Always side-by-side.
    SideBySide,
}

impl ReviewViewConfig {
    /// Read the `review_v2` table and `viewed_reset` of the plugin's configuration value.
    #[must_use]
    pub fn from_plugin_config(value: Option<&toml::Value>) -> Self {
        value
            .and_then(toml::Value::as_table)
            .map(Self::from_plugin_table)
            .unwrap_or_default()
    }

    /// Read the `review_v2` table and `viewed_reset` of the review's plugin configuration
    /// table. Unreadable values fall back to their defaults.
    #[must_use]
    pub fn from_plugin_table(table: &toml::Table) -> Self {
        let mut config: Self = table
            .get("review_v2")
            .and_then(|table| table.clone().try_into().ok())
            .unwrap_or_default();
        config.viewed_reset = table
            .get("viewed_reset")
            .and_then(|value| value.clone().try_into().ok())
            .unwrap_or_default();
        config
    }

    /// Tab stop interval of diff text: the configured width clamped to `1..=16`, else
    /// [`DEFAULT_TAB_WIDTH`].
    #[must_use]
    pub fn tab_width(self) -> u16 {
        self.tab_width
            .map_or(DEFAULT_TAB_WIDTH, |width| width.clamp(1, MAX_TAB_WIDTH))
    }

    /// The effective policy: this configuration over bcode's preference over the default.
    #[must_use]
    pub fn policy(self, preference: Option<DiffLayout>) -> DiffLayout {
        let preferred_breakpoint = match preference {
            Some(DiffLayout::Auto { breakpoint }) => breakpoint,
            _ => DEFAULT_BREAKPOINT,
        };
        let breakpoint = self.side_by_side_breakpoint.unwrap_or(preferred_breakpoint);
        match self.diff_layout {
            Some(ConfiguredLayout::Auto) => DiffLayout::Auto { breakpoint },
            Some(ConfiguredLayout::Unified) => DiffLayout::Unified,
            Some(ConfiguredLayout::SideBySide) => DiffLayout::SideBySide,
            None => match preference {
                Some(DiffLayout::Auto { .. }) | None => DiffLayout::Auto { breakpoint },
                Some(fixed) => fixed,
            },
        }
    }
}

/// Document layout for `policy` at a list width of `width` cells.
#[must_use]
pub const fn resolve(policy: DiffLayout, width: u16) -> LayoutMode {
    match policy {
        DiffLayout::Auto { breakpoint } if width >= breakpoint => LayoutMode::SideBySide,
        DiffLayout::Auto { .. } | DiffLayout::Unified => LayoutMode::Unified,
        DiffLayout::SideBySide => LayoutMode::SideBySide,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_configuration_overrides_the_bcode_preference() {
        let none = ReviewViewConfig::default();
        assert_eq!(
            none.policy(None),
            DiffLayout::Auto {
                breakpoint: DEFAULT_BREAKPOINT
            }
        );
        assert_eq!(none.policy(Some(DiffLayout::Unified)), DiffLayout::Unified);
        assert_eq!(
            none.policy(Some(DiffLayout::Auto { breakpoint: 90 })),
            DiffLayout::Auto { breakpoint: 90 }
        );
        let value: toml::Value = toml::from_str(
            "[review_v2]\ndiff_layout = \"side_by_side\"\nside_by_side_breakpoint = 150\n",
        )
        .unwrap();
        let configured = ReviewViewConfig::from_plugin_config(Some(&value));
        assert_eq!(
            configured.policy(Some(DiffLayout::Unified)),
            DiffLayout::SideBySide
        );
        let auto: toml::Value =
            toml::from_str("[review_v2]\nside_by_side_breakpoint = 150\n").unwrap();
        assert_eq!(
            ReviewViewConfig::from_plugin_config(Some(&auto))
                .policy(Some(DiffLayout::Auto { breakpoint: 90 })),
            DiffLayout::Auto { breakpoint: 150 }
        );
        let tabs: toml::Value = toml::from_str("[review_v2]\ntab_width = 8\n").unwrap();
        assert_eq!(
            ReviewViewConfig::from_plugin_config(Some(&tabs)).tab_width(),
            8
        );
        assert_eq!(none.tab_width(), DEFAULT_TAB_WIDTH);
        let zero: toml::Value = toml::from_str("[review_v2]\ntab_width = 0\n").unwrap();
        assert_eq!(
            ReviewViewConfig::from_plugin_config(Some(&zero)).tab_width(),
            1
        );
        let invalid: toml::Value = toml::from_str("[review_v2]\ndiff_layout = 3\n").unwrap();
        assert_eq!(
            ReviewViewConfig::from_plugin_config(Some(&invalid)),
            ReviewViewConfig::default()
        );
    }

    #[test]
    fn icon_style_is_read_from_the_plugin_configuration() {
        let ascii: toml::Value = toml::from_str("[review_v2]\nicons = \"ascii\"\n").unwrap();
        assert_eq!(
            ReviewViewConfig::from_plugin_config(Some(&ascii)).icons,
            Some(crate::theme::IconStyle::Ascii)
        );
        assert_eq!(ReviewViewConfig::default().icons, None);
    }

    #[test]
    fn auto_switches_at_the_breakpoint() {
        let auto = DiffLayout::Auto { breakpoint: 120 };
        assert_eq!(resolve(auto, 119), LayoutMode::Unified);
        assert_eq!(resolve(auto, 120), LayoutMode::SideBySide);
        assert_eq!(resolve(DiffLayout::SideBySide, 10), LayoutMode::SideBySide);
    }
}
