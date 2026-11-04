//! Portable action declarations. Key syntax is interpreted only by terminal consumers.

use serde::{Deserialize, Serialize};

pub use bcode_keymap_models::KeymapAction;

/// Action declaration with its enabled plugin owner attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginOwnedKeymapAction {
    /// Loaded plugin identity.
    pub plugin_id: String,
    /// Contributor-owned declaration.
    pub action: KeymapAction,
}

/// Bounded loaded-plugin action discovery. A cursor is an exclusive `(plugin_id, scope, id)`.
/// Callers must restart discovery when their loaded-plugin snapshot changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapActionPage {
    /// Actions ordered by plugin identity, scope and action identity.
    pub actions: Vec<PluginOwnedKeymapAction>,
    /// Exclusive identity for the next page, if more declarations exist.
    pub next: Option<(String, String, String)>,
}
