#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Terminal action registration and effective keybindings.
//!
//! Definitions belong to their contributors. Dispatch and shortcut presentation both query
//! the same resolved registry; consumers must not fall back to default shortcuts.
//!
//! Bindings may be single strokes (`ctrl+x`) or whitespace-separated sequences (`g g`).
//! [`KeySequence`] tracks pending sequence input for one handler.

pub mod controls;
mod registry;
mod sequence;

pub use bcode_keymap_models::KeymapAction;
pub use controls::resolve_registry;
pub use registry::{KeymapBinding, KeymapError, KeymapRegistry, SequenceMatch};
pub use sequence::{KeySequence, SequenceStep};
