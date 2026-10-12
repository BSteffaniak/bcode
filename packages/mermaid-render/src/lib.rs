//! Compatibility facade for BMUX Mermaid rendering.
//!
//! Rendering, bounded caches, cancellation, worker isolation, and the versioned
//! worker protocol are implemented by `bmux_mermaid_render`. The packaged worker
//! is named `bmux-mermaid-worker`; its BCMW/BCMR protocol remains compatible.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub use bmux_mermaid_render::*;
