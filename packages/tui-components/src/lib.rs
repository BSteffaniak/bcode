#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Reusable Bcode TUI components and compatibility adapters.

#[cfg(any(feature = "source-viewer", feature = "diff-viewer"))]
mod source_text;

#[cfg(feature = "activity")]
pub mod activity;
#[cfg(feature = "chrome")]
pub mod chrome;
#[cfg(feature = "compact")]
pub mod compact;
#[cfg(feature = "composer")]
pub mod composer;
#[cfg(feature = "diff-viewer")]
pub mod diff_viewer;
#[cfg(feature = "transcript-markdown")]
pub mod markdown_selection;
#[cfg(feature = "permission")]
pub mod permission;
#[cfg(feature = "session-ui")]
pub mod session_ui;
#[cfg(feature = "setup")]
pub mod setup;
#[cfg(feature = "source-preview")]
pub mod source_preview;
#[cfg(feature = "source-viewer")]
pub mod source_viewer;
#[cfg(feature = "terminal-viewer")]
pub mod terminal_viewer;
#[cfg(feature = "tool-card")]
pub mod tool_card;
#[cfg(feature = "transcript")]
pub mod transcript;
#[cfg(feature = "transcript")]
pub mod transcript_history;
#[cfg(feature = "transcript-markdown")]
pub mod transcript_markdown;
#[cfg(feature = "transcript")]
pub mod transcript_viewport;
