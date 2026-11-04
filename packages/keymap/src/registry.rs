//! Effective keybinding resolution for contributor actions and configured overrides.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bmux_keyboard::{KeyCode, KeyStroke, parse_key_stroke};

use crate::KeymapAction;

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
    /// Parsed terminal strokes. Single-key bindings contain exactly one stroke.
    pub strokes: Vec<KeyStroke>,
    /// Configured spelling for presentation.
    pub key: String,
    /// Registered action identity.
    pub action: String,
}

/// Result of resolving a (possibly partial) key sequence within one scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceMatch<'a> {
    /// The strokes complete a binding for this action.
    Complete(&'a str),
    /// The strokes are a strict prefix of at least one binding.
    Pending,
    /// No binding starts with these strokes.
    NoMatch,
}

/// Contributor-extensible terminal keymap. No process-global singleton is required: each
/// frontend owns one registry for its resolved configuration and enabled contributors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeymapRegistry {
    actions: BTreeMap<(String, String), KeymapAction>,
    overrides: BTreeMap<String, BTreeMap<String, String>>,
    bindings: BTreeMap<String, BTreeMap<Vec<KeyStroke>, KeymapBinding>>,
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

    /// Resolve a single stroke only within the explicitly active scope.
    #[must_use]
    pub fn action_for_key(&self, scope: &str, stroke: KeyStroke) -> Option<&str> {
        self.bindings
            .get(scope)?
            .get(&[normalize_stroke(stroke)][..])
            .map(|binding| binding.action.as_str())
    }

    /// Resolve a key sequence typed so far within the explicitly active scope.
    #[must_use]
    pub fn resolve_sequence(&self, scope: &str, strokes: &[KeyStroke]) -> SequenceMatch<'_> {
        let Some(bindings) = self.bindings.get(scope) else {
            return SequenceMatch::NoMatch;
        };
        let prefix: Vec<KeyStroke> = strokes.iter().copied().map(normalize_stroke).collect();
        if let Some(binding) = bindings.get(&prefix) {
            return SequenceMatch::Complete(binding.action.as_str());
        }
        if bindings
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(candidate, _)| candidate.starts_with(&prefix))
        {
            SequenceMatch::Pending
        } else {
            SequenceMatch::NoMatch
        }
    }

    /// Bindings that extend `prefix`, for "which key" presentation of pending sequences.
    #[must_use]
    pub fn continuations(&self, scope: &str, prefix: &[KeyStroke]) -> Vec<&KeymapBinding> {
        let prefix: Vec<KeyStroke> = prefix.iter().copied().map(normalize_stroke).collect();
        self.bindings
            .get(scope)
            .into_iter()
            .flat_map(|bindings| bindings.range(prefix.clone()..))
            .take_while(|(strokes, _)| strokes.starts_with(&prefix))
            .filter(|(strokes, _)| strokes.len() > prefix.len())
            .map(|(_, binding)| binding)
            .collect()
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
    /// The first stroke of a configured sequence is owned too. Text insertion must not bypass an
    /// explicit override.
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
        let mut resolved: BTreeMap<String, BTreeMap<Vec<KeyStroke>, KeymapBinding>> =
            BTreeMap::new();
        let mut overridden_sequences: BTreeMap<String, BTreeSet<Vec<KeyStroke>>> = BTreeMap::new();
        let mut overridden_actions = BTreeMap::new();
        for (scope, overrides) in &self.overrides {
            if scope.trim().is_empty() {
                return Err(KeymapError("configured scope must be nonempty".into()));
            }
            let mut sequences = BTreeSet::new();
            for key in overrides.keys() {
                if !sequences.insert(parse_sequence(key)?) {
                    return Err(KeymapError(format!(
                        "duplicate configured stroke {key} in {scope}"
                    )));
                }
            }
            reject_prefix_conflicts(scope, sequences.iter())?;
            overridden_sequences.insert(scope.clone(), sequences);
            overridden_actions.insert(scope.clone(), overrides.values().collect::<BTreeSet<_>>());
        }
        for action in self.actions.values() {
            let replaced = overridden_actions
                .get(&action.scope)
                .is_some_and(|ids| ids.contains(&action.id));
            for key in &action.defaults {
                let strokes = parse_sequence(key)?;
                // Configured sequences own their exact strokes and any prefix-related defaults.
                if replaced
                    || overridden_sequences
                        .get(&action.scope)
                        .is_some_and(|owned| owned.iter().any(|seq| prefix_related(seq, &strokes)))
                {
                    continue;
                }
                let bindings = resolved.entry(action.scope.clone()).or_default();
                let binding = KeymapBinding {
                    strokes: strokes.clone(),
                    key: key.clone(),
                    action: action.id.clone(),
                };
                if let Some(previous) = bindings.insert(strokes, binding)
                    && previous.action != action.id
                {
                    return Err(KeymapError(format!(
                        "conflicting default {key} in {}",
                        action.scope
                    )));
                }
            }
        }
        for (scope, bindings) in &resolved {
            reject_prefix_conflicts(scope, bindings.keys())?;
        }
        for (scope, overrides) in &self.overrides {
            let bindings = resolved.entry(scope.clone()).or_default();
            for (key, action) in overrides {
                let strokes = parse_sequence(key)?;
                bindings.remove(&strokes);
                if self.actions.contains_key(&(scope.clone(), action.clone())) {
                    bindings.insert(
                        strokes.clone(),
                        KeymapBinding {
                            strokes,
                            key: key.clone(),
                            action: action.clone(),
                        },
                    );
                }
            }
        }
        self.bindings = resolved;
        self.overridden_strokes = overridden_sequences
            .into_iter()
            .map(|(scope, sequences)| {
                let firsts = sequences
                    .into_iter()
                    .filter_map(|sequence| sequence.first().copied())
                    .collect();
                (scope, firsts)
            })
            .collect();
        Ok(())
    }
}

fn prefix_related(left: &[KeyStroke], right: &[KeyStroke]) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn reject_prefix_conflicts<'a>(
    scope: &str,
    sequences: impl Iterator<Item = &'a Vec<KeyStroke>>,
) -> Result<(), KeymapError> {
    let mut previous: Option<&Vec<KeyStroke>> = None;
    for sequence in sequences {
        if let Some(prior) = previous
            && sequence.starts_with(prior)
        {
            return Err(KeymapError(format!(
                "key sequence {} in {scope} is shadowed by its prefix {}",
                display_sequence(sequence),
                display_sequence(prior)
            )));
        }
        previous = Some(sequence);
    }
    Ok(())
}

fn display_sequence(strokes: &[KeyStroke]) -> String {
    strokes
        .iter()
        .map(|stroke| format!("{stroke:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub const fn normalize_stroke(mut stroke: KeyStroke) -> KeyStroke {
    if let KeyCode::Char(ch) = stroke.key
        && ch.is_ascii_uppercase()
    {
        stroke.key = KeyCode::Char(ch.to_ascii_lowercase());
        stroke.modifiers.shift = true;
    }
    stroke
}

pub fn parse(key: &str) -> Result<KeyStroke, KeymapError> {
    parse_key_stroke(key)
        .map(normalize_stroke)
        .map_err(|error| KeymapError(error.to_string()))
}

fn parse_sequence(key: &str) -> Result<Vec<KeyStroke>, KeymapError> {
    let strokes = key
        .split_whitespace()
        .map(parse)
        .collect::<Result<Vec<_>, _>>()?;
    if strokes.is_empty() {
        return Err(KeymapError("key binding must contain a stroke".into()));
    }
    Ok(strokes)
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
        assert_eq!(
            registry.keys_for_action("viewer", "plugin.upper"),
            [] as [&str; 0]
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
        assert_eq!(
            registry.keys_for_action("viewer", "plugin.open"),
            [] as [&str; 0]
        );
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
        assert_eq!(registry.unresolved_actions(), [] as [(&str, &str); 0]);
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

    #[test]
    fn sequences_resolve_pending_complete_and_continuations() {
        let mut registry = KeymapRegistry::default();
        registry
            .register_all([
                action("top", "list", &["g g", "home"]),
                action("bottom", "list", &["shift+g"]),
                action("goto-ref", "list", &["g r"]),
            ])
            .unwrap();
        let g = parse("g").unwrap();
        assert_eq!(
            registry.resolve_sequence("list", &[g]),
            SequenceMatch::Pending
        );
        assert_eq!(
            registry.resolve_sequence("list", &[g, g]),
            SequenceMatch::Complete("top")
        );
        assert_eq!(
            registry.resolve_sequence("list", &[g, parse("x").unwrap()]),
            SequenceMatch::NoMatch
        );
        assert_eq!(registry.action_for_key("list", g), None);
        let mut continuations: Vec<_> = registry
            .continuations("list", &[g])
            .into_iter()
            .map(|binding| (binding.key.as_str(), binding.action.as_str()))
            .collect();
        continuations.sort_unstable();
        assert_eq!(continuations, [("g g", "top"), ("g r", "goto-ref")]);
        assert_eq!(registry.keys_for_action("list", "top").len(), 2);
    }

    #[test]
    fn default_prefix_conflicts_are_rejected() {
        let mut registry = KeymapRegistry::default();
        assert!(
            registry
                .register_all([
                    action("one", "list", &["g"]),
                    action("two", "list", &["g g"])
                ])
                .is_err()
        );
        assert_eq!(registry, KeymapRegistry::default());
    }

    #[test]
    fn configured_single_key_displaces_prefix_related_default_sequences() {
        let mut registry = KeymapRegistry::default();
        registry
            .configure(BTreeMap::from([(
                "list".into(),
                BTreeMap::from([("g".into(), "bottom".into())]),
            )]))
            .unwrap();
        registry
            .register_all([
                action("top", "list", &["g g"]),
                action("bottom", "list", &["shift+g"]),
            ])
            .unwrap();
        let g = parse("g").unwrap();
        assert_eq!(registry.action_for_key("list", g), Some("bottom"));
        assert_eq!(registry.keys_for_action("list", "top"), [] as [&str; 0]);
        assert!(registry.is_overridden_key("list", g));
    }

    #[test]
    fn configured_sequence_owns_its_first_stroke() {
        let mut registry = KeymapRegistry::default();
        registry
            .configure(BTreeMap::from([(
                "input".into(),
                BTreeMap::from([("z z".into(), "center".into())]),
            )]))
            .unwrap();
        registry.register(action("center", "input", &[])).unwrap();
        assert!(registry.is_overridden_key("input", parse("z").unwrap()));
        assert_eq!(registry.keys_for_action("input", "center"), ["z z"]);
    }
}
