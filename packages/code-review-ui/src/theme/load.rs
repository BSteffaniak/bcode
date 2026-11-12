//! The configured bcode theme, resolved into shared component styles.
//!
//! The theme is selected by `[tui.theme]`, discovered with `bcode_tui_theme::discovery` like
//! every bcode program, and mapped to the shared component styles by
//! `bcode_tui_theme::components`, so the review looks like the rest of bcode.

use std::path::Path;

use bcode_tui_theme::components::{ComponentStyles, component_styles};
use bcode_tui_theme::definition::{ResolvedThemeDefinition, ThemeCatalog, ThemeSelection};
use bcode_tui_theme::discovery::{default_theme_roots, discover_themes};
use bmux_tui::style::Color;

use crate::config::ThemeChoice;

/// Theme used when configuration names none.
pub const DEFAULT_THEME: &str = "bcode-dark";

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
    /// Resolve the configured theme, falling back to [`DEFAULT_THEME`] and then to built-in
    /// colors. Returns the theme and any diagnostic.
    #[must_use]
    pub fn load(
        choice: &ThemeChoice,
        config_dir: &Path,
        project_root: &Path,
    ) -> (Self, Option<String>) {
        let name = if choice.name.is_empty() {
            DEFAULT_THEME
        } else {
            choice.name.as_str()
        };
        let roots = default_theme_roots(config_dir, project_root, &choice.paths);
        let catalog = discover_themes(&roots)
            .map(|discovered| discovered.catalog)
            .or_else(|_| ThemeCatalog::bundled());
        let catalog = match catalog {
            Ok(catalog) => catalog,
            Err(error) => return (Self::fallback(), Some(error.to_string())),
        };
        let selection = ThemeSelection::new(name).overlays(choice.overlays.clone());
        match catalog.resolve(&selection) {
            Ok(resolved) => (Self::from_definition(Some(&resolved)), None),
            Err(error) => {
                let fallback = catalog
                    .resolve(&ThemeSelection::new(DEFAULT_THEME))
                    .map_or_else(
                        |_| Self::fallback(),
                        |resolved| Self::from_definition(Some(&resolved)),
                    );
                (fallback, Some(format!("theme {name}: {error}")))
            }
        }
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
    use super::*;

    #[test]
    fn configured_theme_resolves_and_unknown_themes_fall_back_with_a_diagnostic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let choice = ThemeChoice {
            name: DEFAULT_THEME.to_owned(),
            ..ThemeChoice::default()
        };
        let (theme, diagnostic) = HostTheme::load(&choice, dir.path(), dir.path());
        assert_eq!(theme.id, DEFAULT_THEME);
        assert!(diagnostic.is_none(), "{diagnostic:?}");

        let missing = ThemeChoice {
            name: "no-such-theme".to_owned(),
            ..ThemeChoice::default()
        };
        let (fallback, diagnostic) = HostTheme::load(&missing, dir.path(), dir.path());
        assert_eq!(fallback, theme);
        assert!(diagnostic.is_some_and(|message| message.contains("no-such-theme")));
    }
}
