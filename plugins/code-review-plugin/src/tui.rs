//! Review keymap actions contributed to bcode's terminal keymap.

use bcode_plugin_sdk::tui::PluginTuiRegistry;

/// Register the review app's keymap actions, so bcode knows the review scopes and validates
/// overrides of them.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    for action in bcode_code_review_ui::keymap::actions() {
        registry.register_keymap_action(action);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bcode learns exactly the actions the app binds, so an override bcode accepts is one the
    /// app applies.
    #[test]
    fn bcode_receives_the_actions_the_app_binds() {
        let registry = tui_registry();
        let contributed: Vec<_> = registry.keymap_actions().to_vec();
        assert_eq!(contributed, bcode_code_review_ui::keymap::actions());
        let mut keymap = bcode_plugin_sdk::tui::keymap::KeymapRegistry::default();
        keymap
            .register_all(contributed)
            .expect("contributions register without conflicts");
        for scope in [
            bcode_code_review_ui::keymap::REVIEW_SCOPE,
            bcode_code_review_ui::keymap::HOME_SCOPE,
            bcode_code_review_ui::keymap::COMPOSER_SCOPE,
        ] {
            assert!(
                keymap.actions().any(|action| action.scope == scope),
                "{scope}"
            );
        }
    }
}
