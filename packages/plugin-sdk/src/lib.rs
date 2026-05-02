#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin author SDK for Bcode native plugins.

use serde::{Deserialize, Serialize};
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

/// Default service invocation export symbol for native plugins.
pub const DEFAULT_NATIVE_SERVICE_SYMBOL: &str = "bcode_plugin_invoke_service_v1";

/// Lifecycle hook completed successfully.
pub const EXIT_OK: i32 = 0;

/// Lifecycle hook failed.
pub const EXIT_ERROR: i32 = 1;

/// Plugin instance is unavailable.
pub const EXIT_UNAVAILABLE: i32 = 70;

/// Native service invocation completed successfully.
pub const SERVICE_STATUS_OK: i32 = 0;

/// Native service invocation received invalid arguments.
pub const SERVICE_STATUS_INVALID_ARGUMENT: i32 = 2;

/// Native service invocation failed to decode the request.
pub const SERVICE_STATUS_DECODE_FAILED: i32 = 3;

/// Native service invocation output buffer was too small.
pub const SERVICE_STATUS_BUFFER_TOO_SMALL: i32 = 4;

/// Native service invocation failed to encode the response.
pub const SERVICE_STATUS_ENCODE_FAILED: i32 = 5;

/// Native service invocation could not access the plugin instance.
pub const SERVICE_STATUS_PLUGIN_UNAVAILABLE: i32 = 70;

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

/// Host request delivered to a native plugin service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeServiceContext {
    pub plugin_id: String,
    pub request: ServiceRequest,
}

/// Versioned service request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRequest {
    pub interface_id: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

/// Service response returned by plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceResponse {
    pub payload: Vec<u8>,
    pub error: Option<ServiceError>,
}

impl ServiceResponse {
    /// Create a successful service response.
    #[must_use]
    pub const fn ok(payload: Vec<u8>) -> Self {
        Self {
            payload,
            error: None,
        }
    }

    /// Create an empty successful service response.
    #[must_use]
    pub const fn empty() -> Self {
        Self::ok(Vec::new())
    }

    /// Create an error service response.
    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            payload: Vec::new(),
            error: Some(ServiceError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// Structured service error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
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

    /// Invoke a plugin-provided service operation.
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        ServiceResponse::error(
            "unsupported_service",
            format!(
                "plugin '{}' does not support service '{}:{}'",
                context.plugin_id, context.request.interface_id, context.request.operation
            ),
        )
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

#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn invoke_service_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let Ok(context) = serde_json::from_slice::<NativeServiceContext>(input) else {
        return SERVICE_STATUS_DECODE_FAILED;
    };
    let response = match instance.lock() {
        Ok(mut plugin) => plugin.invoke_service(context),
        Err(_) => return SERVICE_STATUS_PLUGIN_UNAVAILABLE,
    };
    let Ok(encoded) = serde_json::to_vec(&response) else {
        return SERVICE_STATUS_ENCODE_FAILED;
    };

    unsafe {
        *output_len = encoded.len();
    }
    if output_ptr.is_null() || output_capacity < encoded.len() {
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }
    SERVICE_STATUS_OK
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

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_invoke_service_v1(
            input_ptr: *const u8,
            input_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
        ) -> i32 {
            let instance = $crate::plugin_instance::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::invoke_service_export(
                instance,
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
            )
        }
    };
}

/// Common imports for plugin authors.
pub mod prelude {
    pub use crate::{
        CURRENT_PLUGIN_ABI_VERSION, EXIT_ERROR, EXIT_OK, EXIT_UNAVAILABLE, NativeServiceContext,
        PluginError, RustPlugin, SERVICE_STATUS_BUFFER_TOO_SMALL, SERVICE_STATUS_DECODE_FAILED,
        SERVICE_STATUS_ENCODE_FAILED, SERVICE_STATUS_INVALID_ARGUMENT, SERVICE_STATUS_OK,
        SERVICE_STATUS_PLUGIN_UNAVAILABLE, ServiceError, ServiceRequest, ServiceResponse,
        export_plugin,
    };
}
