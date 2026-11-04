#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

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
    ///
    /// Whitespace-separated strokes (for example `g g`) declare a key sequence.
    #[serde(default)]
    pub defaults: Vec<String>,
}
