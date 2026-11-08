#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Terminal theme definitions, bounded discovery and bundled themes.
//!
//! This crate owns the versioned theme file format and semantic role resolution so that
//! standalone terminal tools can share bcode themes without depending on the full TUI.

pub mod bundled;
pub mod check;
pub mod chrome;
pub mod contrast;
pub mod definition;
pub mod discovery;
pub mod expr;
pub mod roles;
pub mod selection;
