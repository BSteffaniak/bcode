//! Checks the host applies to a plugin before loading its backend.
//!
//! [`check_manifest_before_load`] and [`check_registered_plugin_before_load`]
//! are the only pre-load admission checks: the native loader runs them before
//! opening a library, and manifest-only readers (such as
//! `bcode theme explain`) run them to exclude plugins the host would reject
//! without opening any library. A library that passes these checks can still
//! fail when it is opened (an unloadable file, a missing symbol, or an
//! exported manifest that disagrees with the file); only loading detects that.

use std::path::{Path, PathBuf};

use crate::{
    CURRENT_PLUGIN_ABI_VERSION, PluginLoadError, PluginManifest, PluginRuntime, RegisteredPlugin,
    resolve_library_path, validate_tool_presentation_declarations,
};

/// Check `manifest` as the host does before loading any plugin backend,
/// static or dynamic.
///
/// # Errors
///
/// * [`PluginLoadError::InvalidToolPresentation`] when a tool presentation
///   declaration is invalid.
/// * [`PluginLoadError::UnsupportedAbi`] when the runtime declares an ABI
///   version other than the host's.
pub fn check_manifest_before_load(manifest: &PluginManifest) -> Result<(), PluginLoadError> {
    validate_tool_presentation_declarations(std::iter::once(manifest))?;
    let PluginRuntime::Native(runtime) = &manifest.runtime;
    if !runtime.is_current_abi() {
        return Err(PluginLoadError::UnsupportedAbi {
            plugin_id: manifest.id.clone(),
            actual: runtime.abi_version,
            expected: CURRENT_PLUGIN_ABI_VERSION,
        });
    }
    Ok(())
}

/// Check a discovered plugin as the host does before opening its library,
/// returning the resolved library path.
///
/// A library path relative to the manifest resolves against the manifest's
/// directory. The file must exist when the resolved path names a directory;
/// a bare file name is left to the platform loader's search path.
///
/// # Errors
///
/// * Any error from [`check_manifest_before_load`].
/// * [`PluginLoadError::LibraryNotFound`] when the resolved library file
///   does not exist.
pub fn check_registered_plugin_before_load(
    plugin: &RegisteredPlugin,
) -> Result<PathBuf, PluginLoadError> {
    check_manifest_before_load(&plugin.manifest)?;
    let PluginRuntime::Native(runtime) = &plugin.manifest.runtime;
    let library_path = resolve_library_path(&plugin.manifest_path, &runtime.library);
    if names_directory(&library_path) && !library_path.is_file() {
        return Err(PluginLoadError::LibraryNotFound {
            plugin_id: plugin.manifest.id.clone(),
            path: library_path,
        });
    }
    Ok(library_path)
}

/// Whether `path` has a directory part, so the platform loader opens it
/// directly instead of searching for it.
fn names_directory(path: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(directory: &Path, abi_version: u16, library: &str) -> RegisteredPlugin {
        RegisteredPlugin {
            manifest_path: directory.join("bcode-plugin.toml"),
            manifest: toml::from_str(&format!(
                r#"
id = "test.preload"
name = "Preload"
version = "0.0.1"

[runtime]
type = "native"
abi_version = {abi_version}
library = "{library}"
"#
            ))
            .expect("manifest parses"),
        }
    }

    /// A directory that exists; its `Cargo.toml` stands in for a library
    /// file, since the checks never open it.
    fn crate_directory() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn present_library_with_current_abi_passes() {
        let plugin = registered(crate_directory(), CURRENT_PLUGIN_ABI_VERSION, "Cargo.toml");
        assert_eq!(
            check_registered_plugin_before_load(&plugin).expect("passes"),
            crate_directory().join("Cargo.toml")
        );
    }

    #[test]
    fn missing_library_is_rejected_before_opening() {
        let plugin = registered(
            crate_directory(),
            CURRENT_PLUGIN_ABI_VERSION,
            "libgone-on-purpose.dylib",
        );
        let error = check_registered_plugin_before_load(&plugin).expect_err("missing");
        assert!(
            matches!(&error, PluginLoadError::LibraryNotFound { plugin_id, path }
                if plugin_id == "test.preload"
                    && *path == crate_directory().join("libgone-on-purpose.dylib")),
            "{error}"
        );
        // The native loader applies the same check.
        assert!(matches!(
            crate::load_registered_plugin(&plugin),
            Err(PluginLoadError::LibraryNotFound { .. })
        ));
    }

    #[test]
    fn unsupported_abi_is_rejected_before_the_library_is_resolved() {
        let plugin = registered(
            crate_directory(),
            CURRENT_PLUGIN_ABI_VERSION + 1,
            "libgone-on-purpose.dylib",
        );
        assert!(matches!(
            check_registered_plugin_before_load(&plugin),
            Err(PluginLoadError::UnsupportedAbi { actual, .. })
                if actual == CURRENT_PLUGIN_ABI_VERSION + 1
        ));
    }

    #[test]
    fn bare_library_names_are_left_to_the_loader_search_path() {
        let plugin = registered(
            Path::new(""),
            CURRENT_PLUGIN_ABI_VERSION,
            "libsearched.dylib",
        );
        assert_eq!(
            check_registered_plugin_before_load(&plugin).expect("passes"),
            PathBuf::from("libsearched.dylib")
        );
    }
}
