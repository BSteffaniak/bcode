//! Contributor-owned review keymap actions, declared in the plugin manifest.

use bcode_plugin_sdk::tui::keymap::KeymapAction;

#[derive(serde::Deserialize)]
struct Declarations {
    keymap_actions: Vec<KeymapAction>,
}

/// Declared actions of one keymap scope.
pub fn actions_for(scope: &str) -> Vec<KeymapAction> {
    actions()
        .into_iter()
        .filter(|action| action.scope == scope)
        .collect()
}

/// Every keymap action the manifest declares.
pub fn actions() -> Vec<KeymapAction> {
    static ACTIONS: std::sync::OnceLock<Vec<KeymapAction>> = std::sync::OnceLock::new();
    ACTIONS
        .get_or_init(|| {
            toml::from_str::<Declarations>(include_str!("../bcode-plugin.toml"))
                .expect("bundled keymap declarations are valid")
                .keymap_actions
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::tui::{
        SCOPE,
        home::HOME_SCOPE,
        surface::{COMPOSER_SCOPE, FILES_SCOPE, THREADS_SCOPE},
        wizard::WIZARD_SCOPE,
    };
    use bcode_plugin_sdk::tui::keymap::controls::resolve_registry;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn declared_bindings_resolve_and_actions_without_defaults_stay_unbound() {
        let registry = resolve_registry(actions(), BTreeMap::new()).unwrap();
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

    /// Overrides users wrote for the first review interface's `code-review` scope keep applying
    /// to shared action ids, and ones naming its removed actions are reported, not fatal.
    #[test]
    fn overrides_written_for_the_first_interface_still_resolve() {
        let registry = resolve_registry(
            actions(),
            BTreeMap::from([(
                SCOPE.to_owned(),
                BTreeMap::from([
                    ("ctrl+j".to_owned(), "bcode.code_review.down".to_owned()),
                    (
                        "ctrl+b".to_owned(),
                        "bcode.code_review.add_source".to_owned(),
                    ),
                ]),
            )]),
        )
        .expect("legacy overrides resolve");
        let stroke = |key| bmux_keyboard::parse_key_stroke(key).unwrap();
        assert_eq!(
            registry.action_for_key(SCOPE, stroke("ctrl+j")),
            Some("bcode.code_review.down")
        );
        assert_eq!(
            registry.unresolved_actions(),
            [(SCOPE, "bcode.code_review.add_source")]
        );
    }

    #[test]
    fn every_declared_scope_is_read_by_a_review_route() {
        let routes = [
            SCOPE,
            HOME_SCOPE,
            WIZARD_SCOPE,
            FILES_SCOPE,
            THREADS_SCOPE,
            COMPOSER_SCOPE,
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
