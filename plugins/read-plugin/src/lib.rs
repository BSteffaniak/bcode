#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Terminal Markdown reader plugin for Bcode.

#[cfg(feature = "static-bundled")]
/// Plugin-owned CLI parsing and orchestration.
pub mod cli;
#[cfg(feature = "static-bundled")]
/// Plugin-owned sequential terminal output.
pub mod output;
#[cfg(feature = "static-bundled")]
/// Plugin-owned interactive pager state.
pub mod pager;
#[cfg(feature = "static-bundled")]
/// Plugin-owned terminal lifecycle and event handling.
pub mod terminal;

use bcode_plugin_sdk::prelude::*;

/// Terminal Markdown reader plugin.
#[derive(Default)]
pub struct ReadPlugin;

impl RustPlugin for ReadPlugin {}

/// Return the statically linked plugin vtable.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable =
        bcode_plugin_sdk::static_plugin_vtable!(ReadPlugin, include_str!("../bcode-plugin.toml"));
    vtable.cli_registration = Some(cli::registration);
    vtable
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(ReadPlugin, include_str!("../bcode-plugin.toml"));
