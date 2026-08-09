#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Reusable Bcode TUI components and compatibility adapters.

#[cfg(feature = "compact")]
pub mod compact;
#[cfg(feature = "diff-viewer")]
pub mod diff_viewer;
#[cfg(feature = "source-preview")]
pub mod source_preview;
#[cfg(feature = "source-viewer")]
pub mod source_viewer;
#[cfg(feature = "terminal-viewer")]
pub mod terminal_viewer;
