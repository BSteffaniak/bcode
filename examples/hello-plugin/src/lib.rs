#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Example native Bcode plugin.

use bcode_plugin_sdk::prelude::*;

/// Example plugin used by smoke tests.
#[derive(Default)]
pub struct HelloPlugin;

impl RustPlugin for HelloPlugin {
    fn activate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn deactivate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

bcode_plugin_sdk::export_plugin!(HelloPlugin, include_str!("../bcode-plugin.toml"));
