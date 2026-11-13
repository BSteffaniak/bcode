//! The configured bcode theme, resolved into shared component styles.
//!
//! The theme is selected exactly as bcode selects it: `[tui.theme]` with the palette and profile
//! the appearance picker saved in user state, resolved by `bcode_tui_theme::appearance` and
//! mapped to the shared component styles by `bcode_tui_theme::components`, so the review looks
//! like the rest of bcode.

use bcode_tui_theme::appearance::{AppearanceResolution, AppearanceSources};
use bcode_tui_theme::components::{ComponentStyles, component_styles};
use bcode_tui_theme::definition::{ResolvedThemeDefinition, ThemeSelection};
use bmux_tui::capabilities::TerminalBackground;
use bmux_tui::style::Color;

pub use bcode_tui_theme::appearance::DEFAULT_THEME;

/// Accent used when the theme defines no `accent` color.
const DEFAULT_ACCENT: Color = Color::Cyan;

/// Resolved theme a host presents: the review derives its roles from [`Self::styles`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTheme {
    /// Id of the resolved theme; empty for built-in fallback colors.
    pub id: String,
    /// Shared component styles.
    pub styles: ComponentStyles,
    /// Fingerprint of the resolved definition; it changes when the theme does.
    pub fingerprint: u64,
}

impl HostTheme {
    /// Resolve the theme `sources` select for a terminal with `background`. Returns the theme
    /// and any diagnostics ([`Self::from_resolution`]).
    #[must_use]
    pub fn load(
        sources: &AppearanceSources,
        background: TerminalBackground,
    ) -> (Self, Vec<String>) {
        Self::from_resolution(&sources.resolve(background))
    }

    /// The theme of a resolved appearance. A selection that does not resolve falls back to
    /// [`DEFAULT_THEME`] and then to built-in colors; the diagnostics say why, along with saved
    /// choices that could not apply.
    #[must_use]
    pub fn from_resolution(resolution: &AppearanceResolution) -> (Self, Vec<String>) {
        let mut diagnostics: Vec<String> =
            resolution.notices.iter().map(ToString::to_string).collect();
        let theme = match &resolution.resolved {
            Ok(resolved) => Self::from_definition(Some(resolved)),
            Err(error) => {
                diagnostics.push(format!("theme {}: {error}", resolution.preferences.name));
                resolution
                    .catalog
                    .resolve(&ThemeSelection::new(DEFAULT_THEME).variant(resolution.variant))
                    .map_or_else(
                        |_| Self::fallback(),
                        |resolved| Self::from_definition(Some(&resolved)),
                    )
            }
        };
        (theme, diagnostics)
    }

    /// Built-in colors used when no theme can be resolved.
    #[must_use]
    pub fn fallback() -> Self {
        Self::from_definition(None)
    }

    /// Derive the review presentation from a resolved theme, or terminal defaults.
    #[must_use]
    pub fn from_definition(theme: Option<&ResolvedThemeDefinition>) -> Self {
        let accent = theme
            .and_then(|theme| theme.color("accent"))
            .unwrap_or(DEFAULT_ACCENT);
        Self {
            id: theme.map(|theme| theme.id.clone()).unwrap_or_default(),
            styles: component_styles(theme, accent),
            fingerprint: theme.map_or(0, |theme| theme.fingerprint),
        }
    }
}

#[cfg(test)]
mod tests {
    use bcode_tui_theme::appearance::{SavedAppearance, ThemePreferences};

    use super::*;

    fn sources(
        dir: &std::path::Path,
        configured: &str,
        saved: SavedAppearance,
    ) -> AppearanceSources {
        AppearanceSources {
            configured: ThemePreferences {
                name: configured.to_owned(),
                ..ThemePreferences::default()
            },
            saved,
            watched_files: Vec::new(),
            user_config_dir: dir.to_path_buf(),
            project_root: dir.to_path_buf(),
            ..AppearanceSources::default()
        }
    }

    #[test]
    fn configured_theme_resolves_and_unknown_themes_fall_back_with_a_diagnostic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (theme, diagnostics) = HostTheme::load(
            &sources(dir.path(), DEFAULT_THEME, SavedAppearance::default()),
            TerminalBackground::Unknown,
        );
        assert_eq!(theme.id, DEFAULT_THEME);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let (fallback, diagnostics) = HostTheme::load(
            &sources(dir.path(), "no-such-theme", SavedAppearance::default()),
            TerminalBackground::Unknown,
        );
        assert_eq!(fallback, theme);
        assert!(
            diagnostics
                .iter()
                .any(|message| message.contains("no-such-theme"))
        );
    }

    #[test]
    fn the_palette_saved_by_the_appearance_picker_is_presented() {
        let dir = tempfile::tempdir().expect("tempdir");
        let saved = SavedAppearance {
            theme: Some("catppuccin-mocha".to_owned()),
            profile: Some("structured".to_owned()),
        };
        let (theme, diagnostics) = HostTheme::load(
            &sources(dir.path(), "nord", saved),
            TerminalBackground::Unknown,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(theme.id, "catppuccin-mocha");
        let (configured, _) = HostTheme::load(
            &sources(dir.path(), "nord", SavedAppearance::default()),
            TerminalBackground::Unknown,
        );
        assert_eq!(configured.id, "nord");
        assert_ne!(theme.styles, configured.styles);
    }
}
