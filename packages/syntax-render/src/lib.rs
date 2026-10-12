//! Compatibility facade for BMUX syntax rendering.
//!
//! Highlighting, semantic tokens, palettes, and terminal adaptation are owned by
//! `bmux_syntax_render`; these reexports retain existing Bcode imports.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub use bmux_syntax_render::*;
