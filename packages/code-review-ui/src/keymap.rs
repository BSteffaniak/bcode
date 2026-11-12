//! Keymap actions of the review and their resolution with user overrides.
//!
//! The actions are the ones the code review plugin manifest declares, so the review binds the
//! same keys whether it runs standalone or from bcode. Users rebind or unbind them under
//! `[tui.keybindings.scopes."code-review*"]`.

use std::collections::BTreeMap;

use bcode_keymap::{KeymapAction, KeymapError, KeymapRegistry};

/// Main review scope: navigation, files, annotations, and review actions.
pub const REVIEW_SCOPE: &str = "code-review";
/// Home screen scope: the list of reviews and suggestions.
pub const HOME_SCOPE: &str = "code-review-home";
/// Comment composer scope.
pub const COMPOSER_SCOPE: &str = "code-review-composer";
/// File panel scope.
pub const FILES_SCOPE: &str = "code-review-files";
/// Thread panel scope.
pub const THREADS_SCOPE: &str = "code-review-threads";
/// New-review wizard scope.
pub const WIZARD_SCOPE: &str = "code-review-wizard";

/// The plugin manifest that declares the review's keymap actions.
const MANIFEST: &str = include_str!("../../../plugins/code-review-plugin/bcode-plugin.toml");

#[derive(serde::Deserialize)]
struct Declarations {
    keymap_actions: Vec<KeymapAction>,
}

/// Every declared keymap action of the review.
///
/// # Panics
///
/// Panics when the bundled manifest declarations do not parse, which the crate's tests rule out.
#[must_use]
pub fn actions() -> Vec<KeymapAction> {
    static ACTIONS: std::sync::OnceLock<Vec<KeymapAction>> = std::sync::OnceLock::new();
    ACTIONS
        .get_or_init(|| {
            toml::from_str::<Declarations>(MANIFEST)
                .expect("bundled keymap declarations are valid")
                .keymap_actions
        })
        .clone()
}

/// Declared actions of one keymap scope.
#[must_use]
pub fn actions_for(scope: &str) -> Vec<KeymapAction> {
    actions()
        .into_iter()
        .filter(|action| action.scope == scope)
        .collect()
}

/// Resolve the declared actions with `overrides` (`scope -> key -> action`; an empty action
/// unbinds the key).
///
/// # Errors
///
/// * [`KeymapError`] when an override names an unknown key or the declarations conflict.
pub fn registry(
    overrides: BTreeMap<String, BTreeMap<String, String>>,
) -> Result<KeymapRegistry, KeymapError> {
    let overrides = overrides
        .into_iter()
        .map(|(scope, bindings)| {
            let bindings = bindings
                .into_iter()
                .map(|(key, action)| {
                    let action = if matches!(action.as_str(), "none" | "unbind") {
                        String::new()
                    } else {
                        action
                    };
                    (key, action)
                })
                .collect();
            (scope, bindings)
        })
        .collect();
    bcode_keymap::resolve_registry(actions(), overrides)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::parse_key_stroke;
    use std::collections::BTreeSet;

    #[test]
    fn every_review_scope_declares_actions_and_defaults_resolve() {
        let registry = registry(BTreeMap::new()).expect("declarations resolve");
        for scope in [
            REVIEW_SCOPE,
            HOME_SCOPE,
            COMPOSER_SCOPE,
            FILES_SCOPE,
            THREADS_SCOPE,
            WIZARD_SCOPE,
        ] {
            assert!(
                actions().iter().any(|action| action.scope == scope),
                "{scope} declares no actions"
            );
        }
        let j = parse_key_stroke("j").expect("key");
        assert_eq!(
            registry.action_for_key(HOME_SCOPE, j),
            Some("bcode.code_review.home_down")
        );
    }

    #[test]
    fn overrides_rebind_and_unbind_keys() {
        let registry = registry(BTreeMap::from([(
            HOME_SCOPE.to_owned(),
            BTreeMap::from([
                ("x".to_owned(), "bcode.code_review.home_down".to_owned()),
                ("j".to_owned(), "none".to_owned()),
            ]),
        )]))
        .expect("overrides resolve");
        let key = |text: &str| parse_key_stroke(text).expect("key");
        assert_eq!(
            registry.action_for_key(HOME_SCOPE, key("x")),
            Some("bcode.code_review.home_down")
        );
        assert_eq!(registry.action_for_key(HOME_SCOPE, key("j")), None);
    }

    #[test]
    fn declared_bindings_resolve_and_actions_without_defaults_stay_unbound() {
        let registry = registry(BTreeMap::new()).unwrap();
        for action in actions() {
            let keys = registry.keys_for_action(&action.scope, &action.id);
            // Rarely used actions are declared without a default key: they run from the palette
            // and help, and users can bind them.
            assert_eq!(
                keys.is_empty(),
                action.defaults.is_empty(),
                "{} {}",
                action.scope,
                action.id
            );
        }
    }

    /// Overrides of the `code-review` scope keep applying to declared action ids, and ones
    /// naming actions that no longer exist are reported, not fatal.
    #[test]
    fn overrides_of_removed_actions_are_reported_not_fatal() {
        let registry = bcode_keymap::resolve_registry(
            actions(),
            BTreeMap::from([(
                REVIEW_SCOPE.to_owned(),
                BTreeMap::from([
                    ("ctrl+j".to_owned(), "bcode.code_review.down".to_owned()),
                    (
                        "ctrl+b".to_owned(),
                        "bcode.code_review.add_source".to_owned(),
                    ),
                ]),
            )]),
        )
        .expect("overrides resolve");
        let stroke = |key| parse_key_stroke(key).unwrap();
        assert_eq!(
            registry.action_for_key(REVIEW_SCOPE, stroke("ctrl+j")),
            Some("bcode.code_review.down")
        );
        assert_eq!(
            registry.unresolved_actions(),
            [(REVIEW_SCOPE, "bcode.code_review.add_source")]
        );
    }

    #[test]
    fn every_declared_scope_is_read_by_a_review_route() {
        let routes = [
            crate::SCOPE,
            crate::home::HOME_SCOPE,
            crate::wizard::WIZARD_SCOPE,
            crate::surface::FILES_SCOPE,
            crate::surface::THREADS_SCOPE,
            crate::surface::COMPOSER_SCOPE,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        let declared = actions()
            .into_iter()
            .map(|action| action.scope)
            .collect::<BTreeSet<_>>();
        assert_eq!(declared, routes);
    }
}
