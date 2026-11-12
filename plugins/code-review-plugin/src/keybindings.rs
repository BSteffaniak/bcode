//! Review keymap actions, declared in the plugin manifest and registered with bcode's keymap
//! so review scopes and overrides are known wherever bcode resolves keybindings.

use bcode_plugin_sdk::tui::keymap::KeymapAction;

#[derive(serde::Deserialize)]
struct Declarations {
    keymap_actions: Vec<KeymapAction>,
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
