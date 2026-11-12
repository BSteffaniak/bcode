//! Review keymap actions contributed to bcode's terminal keymap.

use bcode_plugin_sdk::tui::PluginTuiRegistry;

/// Register the review's keymap actions, so bcode knows the review scopes and validates
/// overrides of them.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    for action in crate::keybindings::actions() {
        registry.register_keymap_action(action);
    }
    registry
}
