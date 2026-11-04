#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Interactive terminal git client built on BMUX.
//!
//! The UI is a lazygit-style panel layout (status, files, branches, commits, stash and a main
//! view) with language-aware diff highlighting, hunk and line staging, interactive history
//! editing and a fully rebindable keymap. It runs standalone (`bcode-git`) or inside bcode
//! (`bcode git`) and reads the same configuration in both.

mod actions;
pub mod app;
pub mod config;
pub mod data;
pub mod diffview;
pub mod graph;
pub mod host;
pub mod keymap;
pub mod ops;
pub mod popup;
mod run;
pub mod theme;
mod view;

pub use run::{RunError, RunOptions, build_app, find_repo_root, render_text, run};
