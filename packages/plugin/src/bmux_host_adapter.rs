#![allow(clippy::result_large_err)]

//! Bcode adapter for the shared BMUX plugin runtime.
//!
//! This module is the compatibility seam used while Bcode moves away from its
//! legacy in-repo runtime. It intentionally wraps `bmux_plugin`/`bmux_plugin_sdk`
//! types instead of re-declaring another plugin ABI.

use bmux_plugin::{LoadedPlugin, NativePluginLoader, PluginRegistry};
use bmux_plugin_sdk::{
    HostMetadata, HostScope, NativeServiceContext, NativeStreamingServiceContext, Result,
};
use std::collections::BTreeSet;
use std::path::Path;

/// Runtime implementation selected by Bcode during migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BcodePluginRuntimeMode {
    /// Existing Bcode runtime remains available behind a temporary switch.
    #[default]
    LegacyBcode,
    /// Shared BMUX host runtime.
    SharedBmux,
}

/// Generic capabilities Bcode exposes to BMUX-hosted plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcodeHostCapabilityMap {
    pub storage: HostScope,
    pub logging: HostScope,
    pub recording_events: HostScope,
    pub command_execution: HostScope,
    pub model_tool_session_services: HostScope,
}

impl Default for BcodeHostCapabilityMap {
    fn default() -> Self {
        Self {
            storage: HostScope::new("bcode.host.storage").expect("static scope should parse"),
            logging: HostScope::new("bcode.host.logging").expect("static scope should parse"),
            recording_events: HostScope::new("bcode.host.recording_events")
                .expect("static scope should parse"),
            command_execution: HostScope::new("bcode.host.command_execution")
                .expect("static scope should parse"),
            model_tool_session_services: HostScope::new("bcode.host.app_services")
                .expect("static scope should parse"),
        }
    }
}

impl BcodeHostCapabilityMap {
    #[must_use]
    pub fn as_set(&self) -> BTreeSet<HostScope> {
        [
            self.storage.clone(),
            self.logging.clone(),
            self.recording_events.clone(),
            self.command_execution.clone(),
            self.model_tool_session_services.clone(),
        ]
        .into_iter()
        .collect()
    }
}

/// Thin wrapper around BMUX's native plugin loader/discovery/service paths.
pub struct BmuxHostPluginAdapter {
    mode: BcodePluginRuntimeMode,
    capabilities: BcodeHostCapabilityMap,
    registry: PluginRegistry,
}

impl BmuxHostPluginAdapter {
    #[must_use]
    pub fn new(mode: BcodePluginRuntimeMode) -> Self {
        Self {
            mode,
            capabilities: BcodeHostCapabilityMap::default(),
            registry: PluginRegistry::new(),
        }
    }

    #[must_use]
    pub const fn mode(&self) -> BcodePluginRuntimeMode {
        self.mode
    }

    #[must_use]
    pub fn host_capabilities(&self) -> BTreeSet<HostScope> {
        self.capabilities.as_set()
    }

    /// Discover BMUX/Bcode-compatible plugin manifests through the shared runtime.
    ///
    /// # Errors
    ///
    /// Returns if shared runtime discovery fails.
    pub fn discover_with_shared_runtime(&mut self, root: &Path) -> Result<usize> {
        let discovered = bmux_plugin::discover_registered_plugins_in_roots(&[root.to_path_buf()])?;
        let count = discovered.iter().count();
        self.registry = discovered;
        Ok(count)
    }

    /// Load one native plugin through the shared BMUX loader.
    ///
    /// # Errors
    ///
    /// Returns if the plugin is not registered or fails BMUX validation/loading.
    pub fn load_native_with_shared_runtime(
        &self,
        plugin_id: &str,
        loader: &NativePluginLoader,
        host: &HostMetadata,
    ) -> Result<LoadedPlugin> {
        let registered = self.registry.get(plugin_id).ok_or_else(|| {
            bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: format!("plugin '{plugin_id}' is not registered"),
            }
        })?;
        let available_capabilities = self
            .host_capabilities()
            .into_iter()
            .map(|scope| {
                let key = scope.clone();
                (
                    key,
                    bmux_plugin::CapabilityProvider {
                        capability: scope,
                        provider: bmux_plugin_sdk::ProviderId::Host,
                    },
                )
            })
            .collect();
        loader.load_registered_plugin(registered, host, &available_capabilities)
    }

    /// Route a service invocation through the shared BMUX runtime.
    ///
    /// # Errors
    ///
    /// Returns if the plugin service invocation fails.
    pub fn invoke_service_with_shared_runtime(
        plugin: &LoadedPlugin,
        context: &NativeServiceContext,
    ) -> Result<bmux_plugin_sdk::ServiceResponse> {
        plugin.invoke_service(context)
    }

    /// Route a streaming service invocation through the shared BMUX runtime.
    ///
    /// # Errors
    ///
    /// Returns if the plugin streaming invocation fails.
    pub fn invoke_streaming_service_with_shared_runtime(
        plugin: &LoadedPlugin,
        context: &NativeStreamingServiceContext,
    ) -> Result<bmux_plugin_sdk::ServiceResponse> {
        plugin.invoke_streaming_service(context)
    }

    /// Route command contribution registration through BMUX's contribution registry.
    ///
    /// # Errors
    ///
    /// Returns if the plugin contribution hook or canonical duplicate checks fail.
    pub fn collect_command_contributions_with_shared_runtime(
        plugin: &LoadedPlugin,
    ) -> Result<Vec<bmux_plugin_sdk::PluginContribution>> {
        plugin.collect_contributions()
    }
}

#[cfg(test)]
mod tests {
    use super::{BcodePluginRuntimeMode, BmuxHostPluginAdapter};

    #[test]
    fn shared_runtime_adapter_maps_bcode_capabilities() {
        let adapter = BmuxHostPluginAdapter::new(BcodePluginRuntimeMode::SharedBmux);
        let capabilities = adapter.host_capabilities();
        assert!(
            capabilities
                .iter()
                .any(|scope| scope.as_str() == "bcode.host.storage")
        );
        assert!(
            capabilities
                .iter()
                .any(|scope| scope.as_str() == "bcode.host.logging")
        );
        assert!(
            capabilities
                .iter()
                .any(|scope| scope.as_str() == "bcode.host.recording_events")
        );
        assert!(
            capabilities
                .iter()
                .any(|scope| scope.as_str() == "bcode.host.command_execution")
        );
        assert!(
            capabilities
                .iter()
                .any(|scope| scope.as_str() == "bcode.host.app_services")
        );
    }

    #[test]
    fn old_runtime_remains_default_during_migration() {
        let adapter = BmuxHostPluginAdapter::new(BcodePluginRuntimeMode::default());
        assert_eq!(adapter.mode(), BcodePluginRuntimeMode::LegacyBcode);
    }
}
