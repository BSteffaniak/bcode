#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Filesystem domain and producer-owned BMUX presentation.
#[cfg(feature = "presentation")]
mod file_change_tui;
#[cfg(feature = "presentation")]
mod filesystem_portable;
#[cfg(feature = "presentation")]
mod filesystem_tui;
#[cfg(feature = "presentation")]
pub use filesystem_portable::portable_visual_registrations;
#[cfg(feature = "native")]
mod batch_request;
#[cfg(all(feature = "native", unix))]
mod confined;
#[cfg(all(feature = "native", unix))]
mod coordination;
#[cfg(feature = "native")]
pub mod path_search;
#[cfg(feature = "native")]
mod replacement;
#[cfg(feature = "native")]
mod write_receipt;
// Include preserves the native implementation's private root module paths.
#[cfg(feature = "native")]
include!("native.rs");
