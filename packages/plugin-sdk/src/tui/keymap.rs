//! Terminal action registration and effective keybindings.
//!
//! Definitions belong to their contributors. Dispatch and shortcut presentation both query
//! the same resolved registry; consumers must not fall back to default shortcuts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bmux_keyboard::{KeyCode, KeyStroke, parse_key_stroke};

pub mod controls;

pub use crate::keymap::KeymapAction;

/// Invalid registration or configuration. Existing registrations are preserved on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapError(pub String);

impl fmt::Display for KeymapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for KeymapError {}

/// Effective binding, shared by input dispatch and shortcut displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapBinding {
    /// Parsed terminal stroke.
    pub stroke: KeyStroke,
    /// Configured spelling for presentation.
    pub key: String,
    /// Registered action identity.
    pub action: String,
}

/// Contributor-extensible terminal keymap. No process-global singleton is required: each
/// frontend owns one registry for its resolved configuration and enabled contributors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeymapRegistry {
    actions: BTreeMap<(String, String), KeymapAction>,
    overrides: BTreeMap<String, BTreeMap<String, String>>,
    bindings: BTreeMap<String, BTreeMap<KeyStroke, KeymapBinding>>,
    overridden_strokes: BTreeMap<String, BTreeSet<KeyStroke>>,
}

impl KeymapRegistry {
    /// Register an action and its defaults atomically. Repeating the identical definition is safe.
    ///
    /// # Errors
    /// Rejects empty identities/descriptions, invalid strokes, conflicting definitions or defaults.
    pub fn register(&mut self, action: KeymapAction) -> Result<(), KeymapError> {
        self.register_all([action])
    }

    /// Register a contributor batch with one rebuild. Identical definitions are idempotent.
    ///
    /// # Errors
    /// Rejects invalid or conflicting definitions/defaults; the whole batch remains unapplied.
    pub fn register_all(
        &mut self,
        actions: impl IntoIterator<Item = KeymapAction>,
    ) -> Result<(), KeymapError> {
        let mut candidate = self.clone();
        let mut changed = false;
        for action in actions {
            if action.id.trim().is_empty()
                || action.scope.trim().is_empty()
                || action.description.trim().is_empty()
            {
                return Err(KeymapError(
                    "action identity, scope and description must be nonempty".into(),
                ));
            }
            let identity = (action.scope.clone(), action.id.clone());
            if let Some(existing) = candidate.actions.get(&identity) {
                if existing != &action {
                    return Err(KeymapError(format!(
                        "conflicting action definition: {} in {}",
                        action.id, action.scope
                    )));
                }
                continue;
            }
            candidate.actions.insert(identity, action);
            changed = true;
        }
        if changed {
            candidate.rebuild()?;
            *self = candidate;
        }
        Ok(())
    }

    /// Replace the enabled contributor snapshot and configuration together, atomically.
    /// Use on runtime selection/configuration updates so disabled contributors disappear.
    ///
    /// # Errors
    /// Rejects invalid configuration or declarations while preserving the previous snapshot.
    pub fn replace(
        &mut self,
        actions: impl IntoIterator<Item = KeymapAction>,
        overrides: BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<(), KeymapError> {
        let mut candidate = Self::default();
        candidate.configure(overrides)?;
        candidate.register_all(actions)?;
        *self = candidate;
        Ok(())
    }

    /// Replace configured scope/key-to-action overrides atomically. Empty action IDs unbind keys.
    /// Overrides targeting an action replace all its defaults. Unregistered action IDs are retained
    /// for later plugin registration, but never dispatched or advertised.
    ///
    /// # Errors
    /// Rejects malformed strokes and conflicting aliases for the same stroke.
    pub fn configure(
        &mut self,
        overrides: BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<(), KeymapError> {
        let mut candidate = self.clone();
        candidate.overrides = overrides;
        candidate.rebuild()?;
        *self = candidate;
        Ok(())
    }

    /// Resolve a stroke only within the explicitly active scope.
    #[must_use]
    pub fn action_for_key(&self, scope: &str, stroke: KeyStroke) -> Option<&str> {
        self.bindings
            .get(scope)?
            .get(&normalize_stroke(stroke))
            .map(|binding| binding.action.as_str())
    }

    /// Effective shortcuts for an action. Unbound actions return an empty list.
    #[must_use]
    pub fn keys_for_action(&self, scope: &str, action: &str) -> Vec<&str> {
        self.bindings
            .get(scope)
            .into_iter()
            .flat_map(BTreeMap::values)
            .filter(|binding| binding.action == action)
            .map(|binding| binding.key.as_str())
            .collect()
    }

    /// Whether configuration explicitly owns this stroke, even when unbound or unresolved.
    /// Text insertion must not bypass an explicit override.
    #[must_use]
    pub fn is_overridden_key(&self, scope: &str, stroke: KeyStroke) -> bool {
        self.overridden_strokes
            .get(scope)
            .is_some_and(|strokes| strokes.contains(&normalize_stroke(stroke)))
    }

    /// Registered descriptions and identities for help/action discovery.
    pub fn actions(&self) -> impl Iterator<Item = &KeymapAction> {
        self.actions.values()
    }

    /// Action IDs awaiting a contributor, useful for configuration diagnostics.
    #[must_use]
    pub fn unresolved_actions(&self) -> Vec<(&str, &str)> {
        self.overrides
            .iter()
            .flat_map(|(scope, bindings)| {
                bindings.values().filter_map(|action| {
                    (!action.is_empty()
                        && !self.actions.contains_key(&(scope.clone(), action.clone())))
                    .then_some((scope.as_str(), action.as_str()))
                })
            })
            .collect()
    }

    fn rebuild(&mut self) -> Result<(), KeymapError> {
        let mut resolved: BTreeMap<String, BTreeMap<KeyStroke, KeymapBinding>> = BTreeMap::new();
        let mut overridden_strokes = BTreeMap::new();
        let mut overridden_actions = BTreeMap::new();
        for (scope, overrides) in &self.overrides {
            if scope.trim().is_empty() {
                return Err(KeymapError("configured scope must be nonempty".into()));
            }
            let mut strokes = BTreeSet::new();
            for key in overrides.keys() {
                if !strokes.insert(parse(key)?) {
                    return Err(KeymapError(format!(
                        "duplicate configured stroke {key} in {scope}"
                    )));
                }
            }
            overridden_strokes.insert(scope.clone(), strokes);
            overridden_actions.insert(scope.clone(), overrides.values().collect::<BTreeSet<_>>());
        }
        for action in self.actions.values() {
            let replaced = overridden_actions
                .get(&action.scope)
                .is_some_and(|ids| ids.contains(&action.id));
            for key in &action.defaults {
                let stroke = parse(key)?;
                if replaced
                    || overridden_strokes
                        .get(&action.scope)
                        .is_some_and(|strokes| strokes.contains(&stroke))
                {
                    continue;
                }
                let bindings = resolved.entry(action.scope.clone()).or_default();
                let binding = KeymapBinding {
                    stroke,
                    key: key.clone(),
                    action: action.id.clone(),
                };
                if let Some(previous) = bindings.insert(stroke, binding)
                    && previous.action != action.id
                {
                    return Err(KeymapError(format!(
                        "conflicting default {key} in {}",
                        action.scope
                    )));
                }
            }
        }
        for (scope, overrides) in &self.overrides {
            let bindings = resolved.entry(scope.clone()).or_default();
            for (key, action) in overrides {
                let stroke = parse(key)?;
                bindings.remove(&stroke);
                if self.actions.contains_key(&(scope.clone(), action.clone())) {
                    bindings.insert(
                        stroke,
                        KeymapBinding {
                            stroke,
                            key: key.clone(),
                            action: action.clone(),
                        },
                    );
                }
            }
        }
        self.bindings = resolved;
        self.overridden_strokes = overridden_strokes;
        Ok(())
    }
}

const fn normalize_stroke(mut stroke: KeyStroke) -> KeyStroke {
    if let KeyCode::Char(ch) = stroke.key
        && ch.is_ascii_uppercase()
    {
        stroke.key = KeyCode::Char(ch.to_ascii_lowercase());
        stroke.modifiers.shift = true;
    }
    stroke
}

fn parse(key: &str) -> Result<KeyStroke, KeymapError> {
    parse_key_stroke(key)
        .map(normalize_stroke)
        .map_err(|error| KeymapError(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(id: &str, scope: &str, keys: &[&str]) -> KeymapAction {
        KeymapAction {
            id: id.into(),
            scope: scope.into(),
            description: format!("Perform {id}"),
            defaults: keys.iter().map(|key| (*key).into()).collect(),
        }
    }

    #[test]
    fn uppercase_events_share_shifted_dispatch_hints_and_override_ownership() {
        use bmux_keyboard::Modifiers;
        let mut registry = KeymapRegistry::default();
        registry
            .register(action("plugin.upper", "viewer", &["shift+a"]))
            .unwrap();
        let upper = KeyStroke::simple(KeyCode::Char('A'));
        let shifted_upper = KeyStroke::with_modifiers(
            KeyCode::Char('A'),
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        );
        let shifted_lower = parse("shift+a").unwrap();
        for stroke in [upper, shifted_upper, shifted_lower] {
            assert_eq!(
                registry.action_for_key("viewer", stroke),
                Some("plugin.upper")
            );
        }
        assert_eq!(registry.action_for_key("viewer", parse("a").unwrap()), None);
        assert_eq!(
            registry.keys_for_action("viewer", "plugin.upper"),
            ["shift+a"]
        );
        registry
            .configure(BTreeMap::from([(
                "viewer".into(),
                BTreeMap::from([("ctrl+alt+shift+a".into(), "plugin.upper".into())]),
            )]))
            .unwrap();
        let modified_upper = KeyStroke::with_modifiers(
            KeyCode::Char('A'),
            Modifiers {
                ctrl: true,
                alt: true,
                ..Modifiers::NONE
            },
        );
        let modified_lower = parse("ctrl+alt+shift+a").unwrap();
        for stroke in [modified_upper, modified_lower] {
            assert_eq!(
                registry.action_for_key("viewer", stroke),
                Some("plugin.upper")
            );
            assert!(registry.is_overridden_key("viewer", stroke));
        }
        assert_eq!(
            registry.keys_for_action("viewer", "plugin.upper"),
            ["ctrl+alt+shift+a"]
        );
        registry
            .configure(BTreeMap::from([(
                "viewer".into(),
                BTreeMap::from([("shift+a".into(), String::new())]),
            )]))
            .unwrap();
        for stroke in [upper, shifted_upper, shifted_lower] {
            assert_eq!(registry.action_for_key("viewer", stroke), None);
            assert!(registry.is_overridden_key("viewer", stroke));
        }
        assert!(
            registry
                .keys_for_action("viewer", "plugin.upper")
                .is_empty()
        );
    }

    #[test]
    fn uppercase_normalization_preserves_other_modifiers_and_unicode_identity() {
        use bmux_keyboard::Modifiers;
        let modifiers = Modifiers {
            ctrl: true,
            alt: true,
            super_key: true,
            hyper: true,
            meta: true,
            shift: false,
        };
        let upper = KeyStroke::with_modifiers(KeyCode::Char('A'), modifiers);
        assert_eq!(
            normalize_stroke(upper),
            KeyStroke::with_modifiers(
                KeyCode::Char('a'),
                Modifiers {
                    shift: true,
                    ..modifiers
                }
            )
        );
        for ch in ['Ä', 'λ', '中'] {
            let stroke = KeyStroke::with_modifiers(KeyCode::Char(ch), modifiers);
            assert_eq!(normalize_stroke(stroke), stroke);
        }
    }

    #[test]
    fn replacement_removes_disabled_contributors_and_is_atomic() {
        let mut registry = KeymapRegistry::default();
        registry
            .register_all([
                action("one", "viewer", &["enter"]),
                action("two", "viewer", &["x"]),
            ])
            .unwrap();
        let before = registry.clone();
        assert!(
            registry
                .replace(
                    [action("three", "viewer", &["invalid-key"])],
                    BTreeMap::new()
                )
                .is_err()
        );
        assert_eq!(registry, before);
        registry
            .replace(
                [action("one", "viewer", &["enter"])],
                BTreeMap::from([(
                    "viewer".into(),
                    BTreeMap::from([("x".into(), "two".into())]),
                )]),
            )
            .unwrap();
        assert_eq!(registry.action_for_key("viewer", parse("x").unwrap()), None);
        assert_eq!(registry.unresolved_actions(), [("viewer", "two")]);
        assert_eq!(registry.actions().count(), 1);
    }

    #[test]
    fn configured_unbind_resolves_colliding_defaults() {
        let mut registry = KeymapRegistry::default();
        registry
            .configure(BTreeMap::from([(
                "viewer".into(),
                BTreeMap::from([("enter".into(), String::new())]),
            )]))
            .unwrap();
        registry
            .register_all([
                action("one", "viewer", &["enter"]),
                action("two", "viewer", &["enter"]),
            ])
            .unwrap();
        assert!(registry.is_overridden_key("viewer", parse("enter").unwrap()));
        assert_eq!(
            registry.action_for_key("viewer", parse("enter").unwrap()),
            None
        );
    }

    #[test]
    fn remapping_and_unbinding_share_dispatch_and_display() {
        let mut registry = KeymapRegistry::default();
        registry
            .register(action("plugin.open", "viewer", &["enter", "o"]))
            .unwrap();
        registry
            .configure(BTreeMap::from([(
                "viewer".into(),
                BTreeMap::from([("ctrl+x".into(), "plugin.open".into())]),
            )]))
            .unwrap();
        assert_eq!(
            registry.action_for_key("viewer", parse("enter").unwrap()),
            None
        );
        assert_eq!(
            registry.action_for_key("viewer", parse("ctrl+x").unwrap()),
            Some("plugin.open")
        );
        assert_eq!(
            registry.keys_for_action("viewer", "plugin.open"),
            ["ctrl+x"]
        );
        assert_eq!(
            registry.action_for_key("other", parse("ctrl+x").unwrap()),
            None
        );
        registry
            .configure(BTreeMap::from([(
                "viewer".into(),
                BTreeMap::from([("enter".into(), String::new()), ("o".into(), String::new())]),
            )]))
            .unwrap();
        assert!(registry.keys_for_action("viewer", "plugin.open").is_empty());
    }

    #[test]
    fn late_registration_resolves_retained_configuration() {
        let mut registry = KeymapRegistry::default();
        registry
            .configure(BTreeMap::from([(
                "viewer".into(),
                BTreeMap::from([("x".into(), "plugin.open".into())]),
            )]))
            .unwrap();
        assert_eq!(registry.unresolved_actions(), [("viewer", "plugin.open")]);
        assert_eq!(registry.action_for_key("viewer", parse("x").unwrap()), None);
        registry
            .register(action("plugin.open", "viewer", &["enter"]))
            .unwrap();
        assert!(registry.unresolved_actions().is_empty());
        assert_eq!(registry.keys_for_action("viewer", "plugin.open"), ["x"]);
    }

    #[test]
    fn invalid_configuration_and_conflicts_preserve_previous_registry() {
        let mut registry = KeymapRegistry::default();
        registry
            .register(action("one", "viewer", &["enter"]))
            .unwrap();
        let before = registry.clone();
        assert!(
            registry
                .register(action("two", "viewer", &["enter"]))
                .is_err()
        );
        assert_eq!(registry, before);
        assert!(
            registry
                .configure(BTreeMap::from([(
                    "viewer".into(),
                    BTreeMap::from([("not-a-key".into(), "one".into())])
                )]))
                .is_err()
        );
        assert_eq!(registry, before);
        registry
            .register(action("one", "viewer", &["enter"]))
            .unwrap();
        assert_eq!(registry, before);
    }
}
