//! Diff layout policy of the review surface (spec section 6.1).
//!
//! The default follows the host's `[tui.diff_viewer]` settings (delivered generically through
//! `PluginTuiHost::diff_layout`), the code review plugin's own configuration overrides it, and
//! the in-review toggle overrides both for the session without writing configuration. The same
//! table selects the review's glyph set (`icons = "ascii"` for terminals without the Unicode
//! symbols).

use crate::v2::document::LayoutMode;
use bcode_plugin_sdk::tui::PluginTuiDiffLayout;
use serde::Deserialize;

/// Plugin id whose configuration table holds the review settings.
const PLUGIN_ID: &str = "bcode.code_review";

/// Side-by-side breakpoint when nobody configures one (spec section 12).
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
    pub icons: Option<super::theme::IconStyle>,
    /// Tab stop interval of diff text, in cells (default [`DEFAULT_TAB_WIDTH`]).
    #[serde(default)]
    pub tab_width: Option<u16>,
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
    /// Read the `review_v2` table of the plugin's configuration value.
    #[must_use]
    pub fn from_plugin_config(value: Option<&toml::Value>) -> Self {
        value
            .and_then(|value| value.get("review_v2"))
            .and_then(|table| table.clone().try_into().ok())
            .unwrap_or_default()
    }

    /// Load the plugin's configuration from the Bcode configuration file. Missing or invalid
    /// configuration yields the defaults.
    #[must_use]
    pub fn load() -> Self {
        bcode_config::load_config().map_or_else(
            |_| Self::default(),
            |config| Self::from_plugin_config(config.plugins.config.get(PLUGIN_ID)),
        )
    }

    /// Tab stop interval of diff text: the configured width clamped to `1..=16`, else
    /// [`DEFAULT_TAB_WIDTH`].
    #[must_use]
    pub fn tab_width(self) -> u16 {
        self.tab_width
            .map_or(DEFAULT_TAB_WIDTH, |width| width.clamp(1, MAX_TAB_WIDTH))
    }

    /// The effective policy: this configuration over the host's preference over the default.
    #[must_use]
    pub fn policy(self, host: Option<PluginTuiDiffLayout>) -> PluginTuiDiffLayout {
        let host_breakpoint = match host {
            Some(PluginTuiDiffLayout::Auto { breakpoint }) => breakpoint,
            _ => DEFAULT_BREAKPOINT,
        };
        let breakpoint = self.side_by_side_breakpoint.unwrap_or(host_breakpoint);
        match self.diff_layout {
            Some(ConfiguredLayout::Auto) => PluginTuiDiffLayout::Auto { breakpoint },
            Some(ConfiguredLayout::Unified) => PluginTuiDiffLayout::Unified,
            Some(ConfiguredLayout::SideBySide) => PluginTuiDiffLayout::SideBySide,
            None => match host {
                Some(PluginTuiDiffLayout::Auto { .. }) | None => {
                    PluginTuiDiffLayout::Auto { breakpoint }
                }
                Some(fixed) => fixed,
            },
        }
    }
}

/// Document layout for `policy` at a list width of `width` cells.
#[must_use]
pub const fn resolve(policy: PluginTuiDiffLayout, width: u16) -> LayoutMode {
    match policy {
        PluginTuiDiffLayout::Auto { breakpoint } if width >= breakpoint => LayoutMode::SideBySide,
        PluginTuiDiffLayout::Auto { .. } | PluginTuiDiffLayout::Unified => LayoutMode::Unified,
        PluginTuiDiffLayout::SideBySide => LayoutMode::SideBySide,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_configuration_overrides_the_host_preference() {
        let none = ReviewViewConfig::default();
        assert_eq!(
            none.policy(None),
            PluginTuiDiffLayout::Auto {
                breakpoint: DEFAULT_BREAKPOINT
            }
        );
        assert_eq!(
            none.policy(Some(PluginTuiDiffLayout::Unified)),
            PluginTuiDiffLayout::Unified
        );
        assert_eq!(
            none.policy(Some(PluginTuiDiffLayout::Auto { breakpoint: 90 })),
            PluginTuiDiffLayout::Auto { breakpoint: 90 }
        );
        let value: toml::Value = toml::from_str(
            "[review_v2]\ndiff_layout = \"side_by_side\"\nside_by_side_breakpoint = 150\n",
        )
        .unwrap();
        let configured = ReviewViewConfig::from_plugin_config(Some(&value));
        assert_eq!(
            configured.policy(Some(PluginTuiDiffLayout::Unified)),
            PluginTuiDiffLayout::SideBySide
        );
        let auto: toml::Value =
            toml::from_str("[review_v2]\nside_by_side_breakpoint = 150\n").unwrap();
        assert_eq!(
            ReviewViewConfig::from_plugin_config(Some(&auto))
                .policy(Some(PluginTuiDiffLayout::Auto { breakpoint: 90 })),
            PluginTuiDiffLayout::Auto { breakpoint: 150 }
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
            Some(super::super::theme::IconStyle::Ascii)
        );
        assert_eq!(ReviewViewConfig::default().icons, None);
    }

    #[test]
    fn auto_switches_at_the_breakpoint() {
        let auto = PluginTuiDiffLayout::Auto { breakpoint: 120 };
        assert_eq!(resolve(auto, 119), LayoutMode::Unified);
        assert_eq!(resolve(auto, 120), LayoutMode::SideBySide);
        assert_eq!(
            resolve(PluginTuiDiffLayout::SideBySide, 10),
            LayoutMode::SideBySide
        );
    }
}
