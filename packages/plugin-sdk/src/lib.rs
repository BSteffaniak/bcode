#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin author SDK for Bcode native plugins.

use std::ffi::{CString, c_char};
use std::sync::{Mutex, OnceLock};

/// Current stable native plugin ABI version.
pub const CURRENT_PLUGIN_ABI_VERSION: u16 = 1;

/// Default manifest export symbol for native plugins.
pub const DEFAULT_NATIVE_MANIFEST_SYMBOL: &str = "bcode_plugin_manifest_v1";

/// Default activation hook export symbol for native plugins.
pub const DEFAULT_NATIVE_ACTIVATE_SYMBOL: &str = "bcode_plugin_activate_v1";

/// Default deactivation hook export symbol for native plugins.
pub const DEFAULT_NATIVE_DEACTIVATE_SYMBOL: &str = "bcode_plugin_deactivate_v1";

/// Lifecycle hook completed successfully.
pub const EXIT_OK: i32 = 0;

/// Lifecycle hook failed.
pub const EXIT_ERROR: i32 = 1;

/// Plugin instance is unavailable.
pub const EXIT_UNAVAILABLE: i32 = 70;

/// Error type returned by native plugin lifecycle hooks.
#[derive(Debug, Clone)]
pub struct PluginError {
    code: i32,
    message: String,
}

impl PluginError {
    /// Create an error with a specific exit code and message.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Create a generic plugin failure.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(EXIT_ERROR, message)
    }

    /// Return the process-style exit code associated with this error.
    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }

    /// Return the human-readable error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PluginError {}

impl From<String> for PluginError {
    fn from(message: String) -> Self {
        Self::failed(message)
    }
}

impl From<&str> for PluginError {
    fn from(message: &str) -> Self {
        Self::failed(message)
    }
}

/// Trait implemented by native Rust plugins.
pub trait RustPlugin: Default + Send + 'static {
    /// Called when the host activates the plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when activation fails.
    fn activate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    /// Called when the host deactivates the plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when deactivation fails.
    fn deactivate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

#[doc(hidden)]
pub fn plugin_instance<P: RustPlugin>(instance: &'static OnceLock<Mutex<P>>) -> &'static Mutex<P> {
    instance.get_or_init(|| Mutex::new(P::default()))
}

#[doc(hidden)]
pub fn manifest_toml_ptr(
    manifest_toml: &'static str,
    cached: &'static OnceLock<Option<CString>>,
) -> *const c_char {
    let cached = cached.get_or_init(|| CString::new(manifest_toml).ok());
    cached
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr())
}

#[doc(hidden)]
#[must_use]
pub fn result_to_exit_code(result: Result<(), PluginError>) -> i32 {
    match result {
        Ok(()) => EXIT_OK,
        Err(error) => {
            eprintln!("{}", error.message());
            error.code()
        }
    }
}

#[doc(hidden)]
pub fn activate_export<P: RustPlugin>(instance: &'static Mutex<P>) -> i32 {
    instance.lock().map_or(EXIT_UNAVAILABLE, |mut plugin| {
        result_to_exit_code(plugin.activate())
    })
}

#[doc(hidden)]
pub fn deactivate_export<P: RustPlugin>(instance: &'static Mutex<P>) -> i32 {
    instance.lock().map_or(EXIT_UNAVAILABLE, |mut plugin| {
        result_to_exit_code(plugin.deactivate())
    })
}

/// Export native plugin ABI symbols for a [`RustPlugin`] implementation.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty, $manifest_toml:expr) => {
        static BCODE_PLUGIN_INSTANCE: std::sync::OnceLock<std::sync::Mutex<$plugin>> =
            std::sync::OnceLock::new();
        static BCODE_PLUGIN_MANIFEST: std::sync::OnceLock<Option<std::ffi::CString>> =
            std::sync::OnceLock::new();

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_manifest_v1() -> *const std::ffi::c_char {
            $crate::manifest_toml_ptr($manifest_toml, &BCODE_PLUGIN_MANIFEST)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_activate_v1() -> i32 {
            let instance = $crate::plugin_instance::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::activate_export(instance)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_deactivate_v1() -> i32 {
            let instance = $crate::plugin_instance::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::deactivate_export(instance)
        }
    };
}

/// Common imports for plugin authors.
pub mod prelude {
    pub use crate::{
        CURRENT_PLUGIN_ABI_VERSION, EXIT_ERROR, EXIT_OK, EXIT_UNAVAILABLE, PluginError, RustPlugin,
        export_plugin,
    };
}
