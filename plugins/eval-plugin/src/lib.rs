#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Eval run viewer plugin for Bcode.

pub mod eval_data;
pub mod eval_viewer;
pub mod tui;

use bcode_plugin_sdk::prelude::*;

/// Eval plugin.
#[derive(Default)]
pub struct EvalPlugin;

impl RustPlugin for EvalPlugin {}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable =
        bcode_plugin_sdk::static_plugin_vtable!(EvalPlugin, include_str!("../bcode-plugin.toml"));
    vtable.tui_registry = Some(tui::tui_registry);
    vtable
}

bcode_plugin_sdk::export_plugin!(EvalPlugin, include_str!("../bcode-plugin.toml"));
