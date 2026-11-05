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
pub mod commit_message;
pub mod config;
pub mod data;
pub mod diffview;
pub mod graph;
pub mod host;
pub mod keymap;
pub mod layout;
pub mod lazygit;
pub mod ops;
pub mod panels;
pub mod popup;
mod run;
pub mod theme;
mod view;

pub use run::{
    COMMIT_MESSAGE_ENV, RunError, RunOptions, build_app, commit_message_from_env, find_repo_root,
    render_buffer, render_text, run,
};
