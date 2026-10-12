//! Compatibility facade for BMUX Markdown rendering.
//!
//! All parsing, terminal projection, source provenance, and selection behavior is
//! implemented by `bmux_markdown`. Reexports preserve existing Bcode imports and
//! the identity of types shared with syntax and Mermaid rendering.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub use bmux_markdown::*;
