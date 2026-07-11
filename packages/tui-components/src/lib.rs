#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Reusable Bcode TUI components.

pub mod compact;
pub mod diff_viewer;
pub mod source_preview;
pub mod source_viewer;
pub mod terminal_viewer;
