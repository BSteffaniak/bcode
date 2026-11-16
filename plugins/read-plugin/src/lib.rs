#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Terminal Markdown reader plugin for Bcode.

#[cfg(feature = "tui")]
/// Plugin-owned CLI parsing and orchestration.
pub mod cli;
#[cfg(feature = "tui")]
/// Plugin-owned sequential terminal output.
pub mod output;
#[cfg(feature = "tui")]
/// Plugin-owned interactive pager state.
pub mod pager;
#[cfg(feature = "tui")]
/// Plugin-owned terminal lifecycle and event handling.
pub mod terminal;

use bcode_plugin_sdk::prelude::*;

/// Register standalone pager actions for effective registry discovery.
#[cfg(feature = "tui")]
#[must_use]
pub fn tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    for action in pager::keymap_actions() {
        registry.register_keymap_action(action);
    }
    registry
}

/// Terminal Markdown reader plugin.
#[derive(Default)]
pub struct ReadPlugin;

impl RustPlugin for ReadPlugin {
    fn component_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
        crate::product_registrations()
    }
}

// Return the statically linked plugin vtable.
// Identity and service declarations come from the embedded manifest only.
bcode_plugin_sdk::define_plugin!(serial ReadPlugin, include_str!("../bcode-plugin.toml"));

fn product_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
    let registrations = bcode_plugin_sdk::definition::ProductRegistrations::default();
    #[cfg(feature = "tui")]
    let registrations = bcode_plugin_sdk::definition::ProductRegistrations {
        frontend: Some(tui_registry),
        cli: Some(cli::registration()),
        ..registrations
    };
    registrations
}

/// Bind this implementation to the host's invocation-scoped capability resolver.
/// The resolver must retain live cancellation, caller provenance and authorized bridges.
#[must_use]
pub fn plugin_definition_with_host(
    host: impl Fn(
        bcode_plugin_sdk::definition::PluginCreateContext,
    ) -> std::sync::Arc<dyn bcode_plugin_sdk::definition::ProductInvocationHost>
    + Send
    + Sync
    + 'static,
) -> bcode_plugin_sdk::definition::PluginDefinition {
    bcode_plugin_sdk::definition::rust_plugin_definition_with_host::<ReadPlugin>(
        plugin_definition().metadata,
        host,
    )
}

#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let vtable =
        bcode_plugin_sdk::static_plugin_vtable!(ReadPlugin, include_str!("../bcode-plugin.toml"));
    #[cfg(feature = "tui")]
    let vtable = {
        let mut vtable = vtable;
        vtable.cli_registration = Some(cli::registration);
        vtable
    };
    vtable
}
