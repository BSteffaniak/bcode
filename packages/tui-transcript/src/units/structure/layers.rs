//! Rule-layer assembly and policy compilation for structure resolution (U5, X3, X4).
//!
//! The host and headless drivers build a [`StructureConfig`] from the same inputs the
//! presentation specification layers: the theme's chrome density as a host-default layer, plugin
//! manifest defaults, the theme and profile `[presentation]` layers, declarative configuration
//! (including the reasoning-visibility rule) and interactive user state. Layer order comes from
//! `bcode_tui_theme::selection::structure_layers`, so the engine stacks exactly the layers the theme
//! tooling explains.

use std::sync::Arc;

use bcode_presentation_policy::models::{
    Disclosure, FactMatcher, LayerSource, OneOrMany, PresentationRule, PropertyPatch, RuleLayer,
    Spacing, UnitKind,
};
use bcode_presentation_policy::{CompiledPolicy, PolicyDiagnostic, PolicyRegistry};
use bcode_tui_theme::chrome::Density;
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bcode_tui_theme::selection::structure_layers;

use super::resolver::StructureConfig;

/// Inputs of one policy compilation, lowest precedence first.
#[derive(Debug, Clone, Copy)]
pub struct PolicyLayers<'a> {
    /// Resolved theme: its chrome density becomes a host-default layer and its `[presentation]`
    /// layers follow the plugin layers. `None` contributes neither.
    pub theme: Option<&'a ResolvedThemeDefinition>,
    /// Plugin manifest default layers, in load order.
    pub plugin_layers: &'a [RuleLayer],
    /// Declarative `[presentation]` configuration rules.
    pub config_rules: &'a [PresentationRule],
    /// Interactive user-state rules.
    pub user_rules: &'a [PresentationRule],
    /// Effective reasoning visibility (`tui.thinking`): when `false`, reasoning is hidden by a
    /// configuration rule and renders nothing at all (an explicit user choice applied before
    /// the visibility floor).
    pub reasoning_visible: bool,
}

impl Default for PolicyLayers<'_> {
    fn default() -> Self {
        Self {
            theme: None,
            plugin_layers: &[],
            config_rules: &[],
            user_rules: &[],
            reasoning_visible: true,
        }
    }
}

impl PolicyLayers<'_> {
    /// Rule layers in precedence order (lowest first).
    #[must_use]
    pub fn layers(&self) -> Vec<RuleLayer> {
        let mut theme_layers: Vec<RuleLayer> = self
            .theme
            .and_then(|theme| density_layer(theme.chrome.density))
            .into_iter()
            .collect();
        if let Some(theme) = self.theme {
            theme_layers.extend(theme.presentation.iter().cloned());
        }
        let mut config_rules = reasoning_rules(self.reasoning_visible);
        config_rules.extend(self.config_rules.iter().cloned());
        structure_layers(
            &theme_layers,
            self.plugin_layers,
            &config_rules,
            self.user_rules,
        )
    }

    /// Compile the layers against `registry` into a structure configuration with no plugin
    /// facts and no declared target fields (the host adds those). Invalid rules are dropped
    /// and reported as diagnostics, as by [`CompiledPolicy::compile`].
    #[must_use]
    pub fn compile(&self, registry: &PolicyRegistry) -> (StructureConfig, Vec<PolicyDiagnostic>) {
        let layers = self.layers();
        let (policy, diagnostics) = CompiledPolicy::compile(&layers, registry);
        let uses_last_in_turn = layers
            .iter()
            .flat_map(|layer| &layer.rules)
            .any(|rule| rule.matcher.last_in_turn.is_some());
        let config = StructureConfig {
            policy: Arc::new(policy),
            uses_last_in_turn,
            reasoning_suppressed: !self.reasoning_visible,
            ..StructureConfig::default()
        };
        (config, diagnostics)
    }

    /// [`Self::compile`] against the host part registry
    /// ([`PolicyRegistry::with_host_parts`]).
    #[must_use]
    pub fn compile_with_host_parts(&self) -> (StructureConfig, Vec<PolicyDiagnostic>) {
        self.compile(&PolicyRegistry::with_host_parts())
    }
}

/// Host-default rule layer carrying a chrome density's tool and group spacing.
///
/// Returns `None` for `cozy`, the built-in default. The layer is lowest in precedence, so
/// plugin, theme, configuration and user rules override it; message units keep their natural
/// rows.
#[must_use]
pub fn density_layer(density: Density) -> Option<RuleLayer> {
    let spacing = match density {
        Density::Compact => Spacing {
            before: 0,
            after: 0,
        },
        Density::Cozy => return None,
        Density::Comfortable => Spacing {
            before: 1,
            after: 1,
        },
    };
    Some(RuleLayer {
        source: LayerSource::Host,
        rules: vec![PresentationRule {
            matcher: FactMatcher {
                kind: OneOrMany(vec![UnitKind::Tool, UnitKind::Group]),
                ..FactMatcher::default()
            },
            set: PropertyPatch {
                spacing: Some(spacing),
                ..PropertyPatch::default()
            },
        }],
    })
}

/// Configuration rules for an effective reasoning visibility; empty when reasoning is shown.
fn reasoning_rules(visible: bool) -> Vec<PresentationRule> {
    if visible {
        return Vec::new();
    }
    vec![PresentationRule {
        matcher: FactMatcher {
            kind: OneOrMany(vec![UnitKind::Reasoning]),
            ..FactMatcher::default()
        },
        set: PropertyPatch {
            disclosure: Some(Disclosure::Hidden),
            ..PropertyPatch::default()
        },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(source: &str) -> PresentationRule {
        toml::from_str(source).expect("rule")
    }

    #[test]
    fn layers_stack_density_theme_config_then_user_state() {
        let mut theme = ResolvedThemeDefinition::empty("t", "T");
        theme.chrome.density = Density::Compact;
        theme.presentation = vec![RuleLayer {
            source: LayerSource::Theme("t".to_owned()),
            rules: vec![rule(
                "match = { kind = \"tool\" }\nset = { detail = \"verbose\" }",
            )],
        }];
        let config = [rule(
            "match = { kind = \"tool\" }\nset = { disclosure = \"collapsed\" }",
        )];
        let user = [rule(
            "match = { kind = \"tool\" }\nset = { disclosure = \"expanded\" }",
        )];
        let layers = PolicyLayers {
            theme: Some(&theme),
            config_rules: &config,
            user_rules: &user,
            reasoning_visible: false,
            ..PolicyLayers::default()
        }
        .layers();
        let sources: Vec<&LayerSource> = layers.iter().map(|layer| &layer.source).collect();
        assert_eq!(
            sources,
            [
                &LayerSource::Host,
                &LayerSource::Theme("t".to_owned()),
                &LayerSource::Config,
                &LayerSource::UserState,
            ]
        );
        assert_eq!(
            layers[2].rules.len(),
            2,
            "the reasoning rule precedes configured rules"
        );
    }

    #[test]
    fn cozy_density_adds_no_layer_and_compile_reports_turn_rules() {
        assert!(density_layer(Density::Cozy).is_none());
        assert!(density_layer(Density::Comfortable).is_some());
        let config = [rule(
            "match = { kind = \"tool\", last_in_turn = true }\nset = { disclosure = \"expanded\" }",
        )];
        let (compiled, diagnostics) = PolicyLayers {
            config_rules: &config,
            ..PolicyLayers::default()
        }
        .compile_with_host_parts();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(compiled.uses_last_in_turn);
        assert!(!compiled.reasoning_suppressed);
        assert_eq!(compiled.policy.len(), 1);
    }
}
