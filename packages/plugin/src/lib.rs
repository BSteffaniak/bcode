#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Host-side plugin loading and discovery for Bcode.

use bcode_plugin_sdk::{
    CURRENT_PLUGIN_ABI_VERSION, DEFAULT_NATIVE_ACTIVATE_SYMBOL, DEFAULT_NATIVE_DEACTIVATE_SYMBOL,
    DEFAULT_NATIVE_MANIFEST_SYMBOL, DEFAULT_NATIVE_SERVICE_SYMBOL, NativeServiceContext,
    SERVICE_STATUS_BUFFER_TOO_SMALL, SERVICE_STATUS_OK, ServiceRequest,
};
pub use bcode_plugin_sdk::{ServiceError, ServiceResponse};
use libloading::Library;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Default plugin manifest file name.
pub const DEFAULT_PLUGIN_MANIFEST_FILE: &str = "bcode-plugin.toml";

type ManifestFn = unsafe extern "C" fn() -> *const std::ffi::c_char;
type LifecycleFn = unsafe extern "C" fn() -> i32;
type ServiceFn = unsafe extern "C" fn(*const u8, usize, *mut u8, usize, *mut usize) -> i32;

/// Plugin manifest loaded from `bcode-plugin.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: Version,
    #[serde(default)]
    pub services: Vec<PluginService>,
    pub runtime: PluginRuntime,
}

/// Service interface declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginService {
    pub interface_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Runtime configuration for a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginRuntime {
    Native(NativePluginRuntime),
}

/// Native dynamic-library plugin configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePluginRuntime {
    pub abi_version: u16,
    pub library: PathBuf,
    #[serde(default = "default_manifest_symbol")]
    pub manifest_symbol: String,
    #[serde(default = "default_activate_symbol")]
    pub activate_symbol: String,
    #[serde(default = "default_deactivate_symbol")]
    pub deactivate_symbol: String,
    #[serde(default = "default_service_symbol")]
    pub service_symbol: String,
}

impl NativePluginRuntime {
    /// Return true when this runtime targets the current host ABI.
    #[must_use]
    pub const fn is_current_abi(&self) -> bool {
        self.abi_version == CURRENT_PLUGIN_ABI_VERSION
    }
}

/// Plugin enable/disable selection policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginSelection {
    pub enabled: BTreeSet<String>,
    pub disabled: BTreeSet<String>,
}

impl PluginSelection {
    /// Return a policy where all discovered plugins are enabled unless disabled.
    #[must_use]
    pub fn all_enabled() -> Self {
        Self::default()
    }

    /// Return true when the plugin ID is enabled by this selection policy.
    #[must_use]
    pub fn is_enabled(&self, plugin_id: &str) -> bool {
        if self.disabled.contains(plugin_id) {
            return false;
        }
        self.enabled.is_empty() || self.enabled.contains(plugin_id)
    }
}

/// Discovered plugin manifest with source path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredPlugin {
    pub manifest_path: PathBuf,
    pub manifest: PluginManifest,
}

/// Loaded native plugin.
#[derive(Debug)]
pub struct LoadedPlugin {
    manifest: PluginManifest,
    library: Library,
    activate: LifecycleFn,
    deactivate: LifecycleFn,
    invoke_service: ServiceFn,
}

impl LoadedPlugin {
    /// Return the loaded plugin manifest.
    #[must_use]
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Activate the plugin.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin activation hook returns a non-zero code.
    pub fn activate(&self) -> Result<(), PluginLoadError> {
        let code = unsafe { (self.activate)() };
        if code == 0 {
            Ok(())
        } else {
            Err(PluginLoadError::LifecycleFailed {
                plugin_id: self.manifest.id.clone(),
                hook: "activate",
                code,
            })
        }
    }

    /// Deactivate the plugin.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin deactivation hook returns a non-zero code.
    pub fn deactivate(&self) -> Result<(), PluginLoadError> {
        let code = unsafe { (self.deactivate)() };
        if code == 0 {
            Ok(())
        } else {
            Err(PluginLoadError::LifecycleFailed {
                plugin_id: self.manifest.id.clone(),
                hook: "deactivate",
                code,
            })
        }
    }

    /// Invoke a service operation on this plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when request encoding, FFI invocation, or response decoding fails.
    pub fn invoke_service(
        &self,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let context = NativeServiceContext {
            plugin_id: self.manifest.id.clone(),
            request: ServiceRequest {
                interface_id: interface_id.into(),
                operation: operation.into(),
                payload,
            },
        };
        let input = serde_json::to_vec(&context).map_err(PluginLoadError::ServiceEncode)?;
        let mut output_len = 0_usize;
        let status = unsafe {
            (self.invoke_service)(
                input.as_ptr(),
                input.len(),
                std::ptr::null_mut(),
                0,
                &raw mut output_len,
            )
        };
        if status != SERVICE_STATUS_BUFFER_TOO_SMALL && status != SERVICE_STATUS_OK {
            return Err(PluginLoadError::ServiceInvokeFailed {
                plugin_id: self.manifest.id.clone(),
                code: status,
            });
        }
        let mut output = vec![0_u8; output_len];
        let status = unsafe {
            (self.invoke_service)(
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
                &raw mut output_len,
            )
        };
        if status != SERVICE_STATUS_OK {
            return Err(PluginLoadError::ServiceInvokeFailed {
                plugin_id: self.manifest.id.clone(),
                code: status,
            });
        }
        output.truncate(output_len);
        serde_json::from_slice(&output).map_err(PluginLoadError::ServiceDecode)
    }

    /// Return true while the dynamic library is retained by this loaded plugin.
    #[must_use]
    pub const fn is_library_retained(&self) -> bool {
        let _ = &self.library;
        true
    }
}

/// Plugin discovery/loading errors.
#[derive(Debug, Error)]
pub enum PluginLoadError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse manifest {path}: {source}")]
    ManifestParse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("plugin '{plugin_id}' uses unsupported ABI version {actual}; expected {expected}")]
    UnsupportedAbi {
        plugin_id: String,
        actual: u16,
        expected: u16,
    },
    #[error("failed to load native library {path}: {source}")]
    LibraryLoad {
        path: PathBuf,
        source: libloading::Error,
    },
    #[error("failed to load symbol '{symbol}' from {library}: {source}")]
    SymbolLoad {
        library: PathBuf,
        symbol: String,
        source: libloading::Error,
    },
    #[error("plugin manifest export returned null for library {0}")]
    NullManifest(PathBuf),
    #[error("plugin manifest export from {library} was invalid UTF-8: {source}")]
    ManifestUtf8 {
        library: PathBuf,
        source: std::str::Utf8Error,
    },
    #[error("plugin manifest export from {library} did not parse: {source}")]
    ExportedManifestParse {
        library: PathBuf,
        source: toml::de::Error,
    },
    #[error("manifest ID mismatch: file declared '{file_id}', library exported '{library_id}'")]
    ManifestIdMismatch { file_id: String, library_id: String },
    #[error("plugin is not loaded: {0}")]
    PluginNotLoaded(String),
    #[error("no loaded plugin declares service interface '{0}'")]
    ServiceNotRegistered(String),
    #[error("multiple loaded plugins declare service interface '{interface_id}': {plugin_ids:?}")]
    AmbiguousService {
        interface_id: String,
        plugin_ids: Vec<String>,
    },
    #[error("failed to encode service request: {0}")]
    ServiceEncode(#[source] serde_json::Error),
    #[error("failed to decode service response: {0}")]
    ServiceDecode(#[source] serde_json::Error),
    #[error("plugin '{plugin_id}' service invocation failed with code {code}")]
    ServiceInvokeFailed { plugin_id: String, code: i32 },
    #[error("plugin '{plugin_id}' {hook} hook failed with code {code}")]
    LifecycleFailed {
        plugin_id: String,
        hook: &'static str,
        code: i32,
    },
}

/// Return default plugin discovery roots.
#[must_use]
pub fn default_plugin_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(current_dir) = env::current_dir() {
        roots.push(current_dir.join(".bcode").join("plugins"));
    }
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        roots.push(PathBuf::from(config_home).join("bcode").join("plugins"));
    } else if let Ok(home) = env::var("HOME") {
        roots.push(
            PathBuf::from(home)
                .join(".config")
                .join("bcode")
                .join("plugins"),
        );
    }
    if let Ok(exe) = env::current_exe()
        && let Some(parent) = exe.parent()
    {
        roots.push(parent.join("plugins"));
    }
    roots
}

/// Discover plugin manifests in the default plugin roots.
///
/// # Errors
///
/// Returns an error when a root or manifest cannot be read.
pub fn discover_plugins() -> Result<Vec<RegisteredPlugin>, PluginLoadError> {
    discover_plugins_in_roots(&default_plugin_roots())
}

/// Discover plugin manifests in a set of roots.
///
/// # Errors
///
/// Returns an error when a root or manifest cannot be read.
pub fn discover_plugins_in_roots(
    roots: &[PathBuf],
) -> Result<Vec<RegisteredPlugin>, PluginLoadError> {
    let mut plugins = Vec::new();
    for root in roots {
        discover_plugins_in_root(root, &mut plugins)?;
    }
    Ok(plugins)
}

/// Filter registered plugins according to an enable/disable policy.
#[must_use]
pub fn filter_selected_plugins(
    plugins: Vec<RegisteredPlugin>,
    selection: &PluginSelection,
) -> Vec<RegisteredPlugin> {
    plugins
        .into_iter()
        .filter(|plugin| selection.is_enabled(&plugin.manifest.id))
        .collect()
}

/// Registry of service interfaces declared by loaded plugins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginServiceRegistry {
    providers: BTreeMap<String, BTreeSet<String>>,
}

impl PluginServiceRegistry {
    /// Build a registry from loaded plugins.
    #[must_use]
    pub fn from_loaded_plugins(plugins: &[LoadedPlugin]) -> Self {
        let mut registry = Self::default();
        for plugin in plugins {
            for service in &plugin.manifest.services {
                registry
                    .providers
                    .entry(service.interface_id.clone())
                    .or_default()
                    .insert(plugin.manifest.id.clone());
            }
        }
        registry
    }

    /// Return all service interface providers.
    #[must_use]
    pub const fn providers(&self) -> &BTreeMap<String, BTreeSet<String>> {
        &self.providers
    }

    /// Return plugin IDs that provide a service interface.
    #[must_use]
    pub fn providers_for(&self, interface_id: &str) -> Option<&BTreeSet<String>> {
        self.providers.get(interface_id)
    }

    fn unique_provider(&self, interface_id: &str) -> Result<&str, PluginLoadError> {
        let Some(providers) = self.providers.get(interface_id) else {
            return Err(PluginLoadError::ServiceNotRegistered(
                interface_id.to_string(),
            ));
        };
        if providers.len() != 1 {
            return Err(PluginLoadError::AmbiguousService {
                interface_id: interface_id.to_string(),
                plugin_ids: providers.iter().cloned().collect(),
            });
        }
        providers
            .iter()
            .next()
            .map(String::as_str)
            .ok_or_else(|| PluginLoadError::ServiceNotRegistered(interface_id.to_string()))
    }
}

/// Loaded plugin host retaining activated plugins.
#[derive(Debug, Default)]
pub struct PluginHost {
    loaded: Vec<LoadedPlugin>,
}

impl PluginHost {
    /// Discover, load, and activate plugins from default roots.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, loading, or activation fails.
    pub fn load_defaults(selection: &PluginSelection) -> Result<Self, PluginLoadError> {
        let plugins = filter_selected_plugins(discover_plugins()?, selection);
        Self::load_registered_plugins(&plugins)
    }

    /// Load and activate registered plugins.
    ///
    /// # Errors
    ///
    /// Returns an error when loading or activation fails.
    pub fn load_registered_plugins(plugins: &[RegisteredPlugin]) -> Result<Self, PluginLoadError> {
        let mut host = Self::default();
        for plugin in plugins {
            let loaded = load_registered_plugin(plugin)?;
            loaded.activate()?;
            host.loaded.push(loaded);
        }
        Ok(host)
    }

    /// Return loaded plugins.
    #[must_use]
    pub fn loaded_plugins(&self) -> &[LoadedPlugin] {
        &self.loaded
    }

    /// Return the service registry for currently loaded plugins.
    #[must_use]
    pub fn service_registry(&self) -> PluginServiceRegistry {
        PluginServiceRegistry::from_loaded_plugins(&self.loaded)
    }

    /// Invoke a service operation on a loaded plugin by ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is not loaded or service invocation fails.
    pub fn invoke_service(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let plugin = self
            .loaded
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.to_string()))?;
        plugin.invoke_service(interface_id, operation, payload)
    }

    /// Invoke a service operation by service interface ID.
    ///
    /// # Errors
    ///
    /// Returns an error when no loaded plugin provides the interface, more than one loaded plugin
    /// provides the interface, or service invocation fails.
    pub fn invoke_service_by_interface(
        &self,
        interface_id: &str,
        operation: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let registry = self.service_registry();
        let plugin_id = registry.unique_provider(interface_id)?;
        self.invoke_service(plugin_id, interface_id, operation, payload)
    }

    /// Deactivate all loaded plugins in reverse load order.
    ///
    /// # Errors
    ///
    /// Returns the first deactivation error.
    pub fn deactivate_all(&mut self) -> Result<(), PluginLoadError> {
        for plugin in self.loaded.iter().rev() {
            plugin.deactivate()?;
        }
        self.loaded.clear();
        Ok(())
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let _ = self.deactivate_all();
    }
}

/// Load a registered plugin.
///
/// # Errors
///
/// Returns an error if the plugin cannot be loaded or exports invalid metadata.
pub fn load_registered_plugin(plugin: &RegisteredPlugin) -> Result<LoadedPlugin, PluginLoadError> {
    let PluginRuntime::Native(runtime) = &plugin.manifest.runtime;
    if !runtime.is_current_abi() {
        return Err(PluginLoadError::UnsupportedAbi {
            plugin_id: plugin.manifest.id.clone(),
            actual: runtime.abi_version,
            expected: CURRENT_PLUGIN_ABI_VERSION,
        });
    }

    let library_path = resolve_library_path(&plugin.manifest_path, &runtime.library);
    let library =
        unsafe { Library::new(library_path.to_string_lossy().as_ref()) }.map_err(|source| {
            PluginLoadError::LibraryLoad {
                path: library_path.clone(),
                source,
            }
        })?;

    let exported_manifest = load_exported_manifest(&library, &library_path, runtime)?;
    if exported_manifest.id != plugin.manifest.id {
        return Err(PluginLoadError::ManifestIdMismatch {
            file_id: plugin.manifest.id.clone(),
            library_id: exported_manifest.id,
        });
    }

    let activate = load_lifecycle_symbol(&library, &library_path, &runtime.activate_symbol)?;
    let deactivate = load_lifecycle_symbol(&library, &library_path, &runtime.deactivate_symbol)?;
    let invoke_service = load_service_symbol(&library, &library_path, &runtime.service_symbol)?;

    Ok(LoadedPlugin {
        manifest: plugin.manifest.clone(),
        library,
        activate,
        deactivate,
        invoke_service,
    })
}

fn discover_plugins_in_root(
    root: &Path,
    plugins: &mut Vec<RegisteredPlugin>,
) -> Result<(), PluginLoadError> {
    if !root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            let manifest_path = path.join(DEFAULT_PLUGIN_MANIFEST_FILE);
            if manifest_path.exists() {
                plugins.push(read_registered_plugin(&manifest_path)?);
            }
        } else if path.file_name().and_then(|name| name.to_str())
            == Some(DEFAULT_PLUGIN_MANIFEST_FILE)
        {
            plugins.push(read_registered_plugin(&path)?);
        }
    }
    Ok(())
}

fn read_registered_plugin(path: &Path) -> Result<RegisteredPlugin, PluginLoadError> {
    let manifest = read_manifest(path)?;
    Ok(RegisteredPlugin {
        manifest_path: path.to_path_buf(),
        manifest,
    })
}

fn read_manifest(path: &Path) -> Result<PluginManifest, PluginLoadError> {
    let contents = std::fs::read_to_string(path)?;
    toml::from_str(&contents).map_err(|source| PluginLoadError::ManifestParse {
        path: path.to_path_buf(),
        source,
    })
}

fn resolve_library_path(manifest_path: &Path, library_path: &Path) -> PathBuf {
    if library_path.is_absolute() {
        return library_path.to_path_buf();
    }
    manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(library_path)
}

fn load_exported_manifest(
    library: &Library,
    library_path: &Path,
    runtime: &NativePluginRuntime,
) -> Result<PluginManifest, PluginLoadError> {
    let manifest_fn = unsafe { library.get::<ManifestFn>(runtime.manifest_symbol.as_bytes()) }
        .map_err(|source| PluginLoadError::SymbolLoad {
            library: library_path.to_path_buf(),
            symbol: runtime.manifest_symbol.clone(),
            source,
        })?;
    let ptr = unsafe { manifest_fn() };
    if ptr.is_null() {
        return Err(PluginLoadError::NullManifest(library_path.to_path_buf()));
    }
    let manifest_toml = unsafe { CStr::from_ptr(ptr) }.to_str().map_err(|source| {
        PluginLoadError::ManifestUtf8 {
            library: library_path.to_path_buf(),
            source,
        }
    })?;
    toml::from_str(manifest_toml).map_err(|source| PluginLoadError::ExportedManifestParse {
        library: library_path.to_path_buf(),
        source,
    })
}

fn load_lifecycle_symbol(
    library: &Library,
    library_path: &Path,
    symbol: &str,
) -> Result<LifecycleFn, PluginLoadError> {
    let loaded = unsafe { library.get::<LifecycleFn>(symbol.as_bytes()) }.map_err(|source| {
        PluginLoadError::SymbolLoad {
            library: library_path.to_path_buf(),
            symbol: symbol.to_string(),
            source,
        }
    })?;
    Ok(*loaded)
}

fn load_service_symbol(
    library: &Library,
    library_path: &Path,
    symbol: &str,
) -> Result<ServiceFn, PluginLoadError> {
    let loaded = unsafe { library.get::<ServiceFn>(symbol.as_bytes()) }.map_err(|source| {
        PluginLoadError::SymbolLoad {
            library: library_path.to_path_buf(),
            symbol: symbol.to_string(),
            source,
        }
    })?;
    Ok(*loaded)
}

fn default_manifest_symbol() -> String {
    DEFAULT_NATIVE_MANIFEST_SYMBOL.to_string()
}

fn default_activate_symbol() -> String {
    DEFAULT_NATIVE_ACTIVATE_SYMBOL.to_string()
}

fn default_deactivate_symbol() -> String {
    DEFAULT_NATIVE_DEACTIVATE_SYMBOL.to_string()
}

fn default_service_symbol() -> String {
    DEFAULT_NATIVE_SERVICE_SYMBOL.to_string()
}

#[cfg(test)]
mod tests {
    use super::{CURRENT_PLUGIN_ABI_VERSION, PluginRuntime, discover_plugins_in_roots};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn discovers_plugin_manifest_in_child_directory() {
        let root = unique_temp_dir();
        let plugin_dir = root.join("example-plugin");
        std::fs::create_dir_all(&plugin_dir).expect("plugin dir should be created");
        std::fs::write(
            plugin_dir.join("bcode-plugin.toml"),
            format!(
                r#"
id = "example.plugin"
name = "Example Plugin"
version = "0.1.0"

[runtime]
type = "native"
abi_version = {CURRENT_PLUGIN_ABI_VERSION}
library = "libexample_plugin.dylib"
"#,
            ),
        )
        .expect("manifest should be written");

        let plugins =
            discover_plugins_in_roots(std::slice::from_ref(&root)).expect("discovery should work");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.id, "example.plugin");
        assert!(matches!(
            plugins[0].manifest.runtime,
            PluginRuntime::Native(_)
        ));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("bcode-plugin-test-{nanos}"))
    }
}
