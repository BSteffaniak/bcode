//! Portable action declarations. Key syntax is interpreted only by terminal consumers.

use serde::{Deserialize, Serialize};

/// One contributor-owned action in an input scope.
///
/// Missing defaults decode as unbound for compatibility. Unknown fields are rejected rather
/// than guessed; terminal consumers validate strokes before using a declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeymapAction {
    /// Stable, namespaced action identity.
    pub id: String,
    /// Input context in which this action is available.
    pub scope: String,
    /// Human-readable description, independent of its bindings.
    pub description: String,
    /// Default strokes in the terminal consumer's syntax, not terminal input types.
    #[serde(default)]
    pub defaults: Vec<String>,
}

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
