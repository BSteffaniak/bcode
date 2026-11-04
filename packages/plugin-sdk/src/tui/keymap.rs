//! Terminal action registration and effective keybindings.
//!
//! The registry is owned by `bcode_keymap` so standalone terminal tools can resolve the same
//! bindings without the plugin SDK. Plugin hosts implement [`controls::KeymapResolver`].

pub use bcode_keymap::{
    KeySequence, KeymapAction, KeymapBinding, KeymapError, KeymapRegistry, SequenceMatch,
    SequenceStep,
};

/// Effective semantic control routing at the terminal boundary.
pub mod controls {
    pub use bcode_keymap::controls::*;
}

impl controls::KeymapResolver for dyn super::PluginTuiHost + '_ {
    fn keymap_action(&self, scope: &str, stroke: bmux_keyboard::KeyStroke) -> Option<String> {
        super::PluginTuiHost::keymap_action(self, scope, stroke)
    }

    fn keymap_is_overridden(&self, scope: &str, stroke: bmux_keyboard::KeyStroke) -> bool {
        super::PluginTuiHost::keymap_is_overridden(self, scope, stroke)
    }
}
