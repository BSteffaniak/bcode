//! Configured theme resolution and structural rule-layer assembly.
//!
//! The TUI and command-line tooling (`bcode theme explain`) must resolve the
//! same theme and stack the same structural presentation layers, so both call
//! these pure functions instead of assembling layers independently.

use bcode_presentation_policy_models::{LayerSource, PresentationRule, RuleLayer};

use crate::definition::{ResolvedThemeDefinition, ThemeCatalog, ThemeError, ThemeSelection};
use crate::discovery::{ThemeDiscoveryDiagnostic, ThemeDiscoveryRoot, discover_themes};

/// Result of discovering the theme catalog and resolving one selection.
#[derive(Debug, Clone)]
pub struct DiscoveredResolution {
    /// The resolved selection, or why it could not be resolved.
    pub resolved: Result<ResolvedThemeDefinition, ThemeError>,
    /// External candidates skipped during discovery.
    pub diagnostics: Vec<ThemeDiscoveryDiagnostic>,
    /// Number of accepted external theme sources.
    pub external_sources: usize,
}

/// Discover themes under `roots` and resolve `selection` against them.
///
/// Discovery failures (only possible when embedded themes are invalid) fall
/// back to the bundled catalog, matching how the TUI degrades.
#[must_use]
pub fn resolve_discovered(
    roots: &[ThemeDiscoveryRoot],
    selection: &ThemeSelection,
) -> DiscoveredResolution {
    let (catalog, diagnostics, external_sources) = match discover_themes(roots) {
        Ok(discovered) => (
            Ok(discovered.catalog),
            discovered.diagnostics,
            discovered.sources.len(),
        ),
        Err(_) => (ThemeCatalog::bundled(), Vec::new(), 0),
    };
    DiscoveredResolution {
        resolved: catalog.and_then(|catalog| catalog.resolve(selection)),
        diagnostics,
        external_sources,
    }
}

/// Structural rule layers in precedence order (lowest first).
///
/// The order is the specification's layer order:
/// 1. host layers: the [`LayerSource::Host`] layers among `theme_layers`
///    (for example the chrome density layer), in their given order;
/// 2. `plugin_layers`, the plugin manifest defaults, in their given order;
/// 3. the remaining `theme_layers` (one per contributing theme or profile);
/// 4. declarative configuration, `config_rules`;
/// 5. interactive user state, `user_rules`.
///
/// Empty configuration and user-state rule lists contribute no layer.
#[must_use]
pub fn structure_layers(
    theme_layers: &[RuleLayer],
    plugin_layers: &[RuleLayer],
    config_rules: &[PresentationRule],
    user_rules: &[PresentationRule],
) -> Vec<RuleLayer> {
    let (mut layers, themes): (Vec<RuleLayer>, Vec<RuleLayer>) = theme_layers
        .iter()
        .cloned()
        .partition(|layer| layer.source == LayerSource::Host);
    layers.extend(plugin_layers.iter().cloned());
    layers.extend(themes);
    if !config_rules.is_empty() {
        layers.push(RuleLayer {
            source: LayerSource::Config,
            rules: config_rules.to_vec(),
        });
    }
    if !user_rules.is_empty() {
        layers.push(RuleLayer {
            source: LayerSource::UserState,
            rules: user_rules.to_vec(),
        });
    }
    layers
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy_models::{PresentationRule, RuleLayer};

    use super::*;
    use crate::definition::ResolvedThemeVariant;

    fn rule(source: &str) -> PresentationRule {
        toml::from_str(source).expect("rule")
    }

    #[test]
    fn layers_stack_host_plugins_theme_config_then_user_state() {
        let layer = |source: LayerSource, detail: &str| RuleLayer {
            source,
            rules: vec![rule(&format!("set = {{ detail = \"{detail}\" }}"))],
        };
        let theme = vec![
            layer(LayerSource::Theme("bare".to_owned()), "minimal"),
            layer(LayerSource::Host, "normal"),
        ];
        let plugins = vec![layer(
            LayerSource::Plugin("bcode.shell".to_owned()),
            "verbose",
        )];
        let config = vec![rule("set = { detail = \"normal\" }")];
        let user = vec![rule("set = { detail = \"verbose\" }")];
        let layers = structure_layers(&theme, &plugins, &config, &user);
        let sources: Vec<_> = layers.iter().map(|layer| layer.source.clone()).collect();
        assert_eq!(
            sources,
            [
                LayerSource::Host,
                LayerSource::Plugin("bcode.shell".to_owned()),
                LayerSource::Theme("bare".to_owned()),
                LayerSource::Config,
                LayerSource::UserState
            ]
        );
        assert_eq!(layers[3].rules, config);
        assert_eq!(layers[4].rules, user);
    }

    #[test]
    fn empty_rule_lists_contribute_no_layer() {
        assert_eq!(
            structure_layers(&[], &[], &[], &[]),
            Vec::<RuleLayer>::new()
        );
        let user = vec![rule("set = { detail = \"verbose\" }")];
        let layers = structure_layers(&[], &[], &[], &user);
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].source, LayerSource::UserState);
    }

    #[test]
    fn resolves_bundled_profile_over_missing_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = crate::discovery::default_theme_roots(dir.path(), dir.path(), &[]);
        let resolution = resolve_discovered(
            &roots,
            &ThemeSelection::new("terminal-native")
                .overlays(vec!["compact".to_owned()])
                .variant(ResolvedThemeVariant::Dark),
        );
        assert_eq!(resolution.diagnostics, Vec::new());
        assert_eq!(resolution.external_sources, 0);
        let resolved = resolution.resolved.expect("resolved");
        assert!(
            resolved
                .presentation
                .iter()
                .any(|layer| layer.source == LayerSource::Theme("compact".to_owned()))
        );
    }

    #[test]
    fn unknown_overlay_is_reported_not_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = crate::discovery::default_theme_roots(dir.path(), dir.path(), &[]);
        let resolution = resolve_discovered(
            &roots,
            &ThemeSelection::new("terminal-native").overlays(vec!["no-such-profile".to_owned()]),
        );
        assert!(matches!(
            resolution.resolved,
            Err(ThemeError::MissingTheme { .. })
        ));
    }
}
