//! Terminal action registration and effective keybindings.
//!
//! Definitions belong to their contributors. Dispatch and shortcut presentation both query
//! the same resolved registry; consumers must not fall back to default shortcuts.

use std::collections::BTreeMap;
use std::fmt;

use bmux_keyboard::{KeyStroke, parse_key_stroke};

/// One contributor-owned action in an input scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapAction {
    /// Stable, namespaced action identity.
    pub id: String,
    /// Input context in which this action is available.
    pub scope: String,
    /// Human-readable description, independent of its bindings.
    pub description: String,
    /// Default key strokes, using BMUX's key syntax.
    pub defaults: Vec<String>,
}

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
    bindings: BTreeMap<String, Vec<KeymapBinding>>,
}

impl KeymapRegistry {
    /// Register an action and its defaults atomically. Repeating the identical definition is safe.
    ///
    /// # Errors
    /// Rejects empty identities/descriptions, invalid strokes, conflicting definitions or defaults.
    pub fn register(&mut self, action: KeymapAction) -> Result<(), KeymapError> {
        if action.id.is_empty() || action.scope.is_empty() || action.description.is_empty() {
            return Err(KeymapError(
                "action identity, scope and description must be nonempty".into(),
            ));
        }
        let identity = (action.scope.clone(), action.id.clone());
        if let Some(existing) = self.actions.get(&identity) {
            return if existing == &action {
                Ok(())
            } else {
                Err(KeymapError(format!(
                    "conflicting action definition: {}",
                    action.id
                )))
            };
        }
        let mut candidate = self.clone();
        candidate.actions.insert(identity, action);
        candidate.rebuild()?;
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
            .iter()
            .find(|binding| binding.stroke == stroke)
            .map(|binding| binding.action.as_str())
    }

    /// Effective shortcuts for an action. Unbound actions return an empty list.
    #[must_use]
    pub fn keys_for_action(&self, scope: &str, action: &str) -> Vec<&str> {
        self.bindings
            .get(scope)
            .into_iter()
            .flatten()
            .filter(|binding| binding.action == action)
            .map(|binding| binding.key.as_str())
            .collect()
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
        for action in self.actions.values() {
            let configured = self.overrides.get(&action.scope);
            for key in &action.defaults {
                let stroke = parse(key)?;
                if configured.is_some_and(|bindings| bindings.values().any(|id| id == &action.id)) {
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
            let mut seen = BTreeMap::new();
            for (key, action) in overrides {
                let stroke = parse(key)?;
                if seen.insert(stroke, action).is_some() {
                    return Err(KeymapError(format!(
                        "duplicate configured stroke {key} in {scope}"
                    )));
                }
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
        self.bindings = resolved
            .into_iter()
            .map(|(scope, bindings)| (scope, bindings.into_values().collect()))
            .collect();
        Ok(())
    }
}

fn parse(key: &str) -> Result<KeyStroke, KeymapError> {
    parse_key_stroke(key).map_err(|error| KeymapError(error.to_string()))
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
