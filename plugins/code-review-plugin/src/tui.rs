//! Native code review TUI surface contribution.

use bcode_plugin_sdk::tui::PluginTuiRegistry;

pub use crate::v2::tui::CODE_REVIEW_SURFACE_KIND;

/// Register the code review surface and its keymap actions.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    for action in crate::keybindings::actions() {
        registry.register_keymap_action(action);
    }
    registry.register_factory(Box::new(crate::v2::tui::ReviewSurfaceV2Factory));
    registry
}
