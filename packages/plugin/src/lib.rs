#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Host-side plugin loading and discovery for Bcode.

use bcode_plugin_sdk::{
    CURRENT_PLUGIN_ABI_VERSION, DEFAULT_NATIVE_ACTIVATE_SYMBOL, DEFAULT_NATIVE_DEACTIVATE_SYMBOL,
    DEFAULT_NATIVE_MANIFEST_SYMBOL,
};
use libloading::Library;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Default plugin manifest file name.
pub const DEFAULT_PLUGIN_MANIFEST_FILE: &str = "bcode-plugin.toml";

type ManifestFn = unsafe extern "C" fn() -> *const std::ffi::c_char;
type LifecycleFn = unsafe extern "C" fn() -> i32;

/// Plugin manifest loaded from `bcode-plugin.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: Version,
    pub runtime: PluginRuntime,
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
}

impl NativePluginRuntime {
    /// Return true when this runtime targets the current host ABI.
    #[must_use]
    pub const fn is_current_abi(&self) -> bool {
        self.abi_version == CURRENT_PLUGIN_ABI_VERSION
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
    #[error("plugin '{plugin_id}' {hook} hook failed with code {code}")]
    LifecycleFailed {
        plugin_id: String,
        hook: &'static str,
        code: i32,
    },
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

    Ok(LoadedPlugin {
        manifest: plugin.manifest.clone(),
        library,
        activate,
        deactivate,
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

fn default_manifest_symbol() -> String {
    DEFAULT_NATIVE_MANIFEST_SYMBOL.to_string()
}

fn default_activate_symbol() -> String {
    DEFAULT_NATIVE_ACTIVATE_SYMBOL.to_string()
}

fn default_deactivate_symbol() -> String {
    DEFAULT_NATIVE_DEACTIVATE_SYMBOL.to_string()
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
