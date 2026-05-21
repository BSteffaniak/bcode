#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin author SDK for Bcode native plugins.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::ffi::{CString, c_char, c_void};
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

/// Default event handler export symbol for native plugins.
pub const DEFAULT_NATIVE_EVENT_SYMBOL: &str = "bcode_plugin_handle_event_v1";

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

/// Native event handling completed successfully.
pub const EVENT_STATUS_OK: i32 = 0;

/// Native event handling received invalid arguments.
pub const EVENT_STATUS_INVALID_ARGUMENT: i32 = 2;

/// Native event handling failed to decode the event.
pub const EVENT_STATUS_DECODE_FAILED: i32 = 3;

/// Native event handler could not access the plugin instance.
pub const EVENT_STATUS_PLUGIN_UNAVAILABLE: i32 = 70;

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

/// Host event delivered to a native plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeEventContext {
    pub plugin_id: String,
    pub event: PluginEvent,
}

/// Plugin event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEvent {
    pub topic: String,
    pub payload: Vec<u8>,
}

impl PluginEvent {
    /// Decode the event payload from JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload is not valid JSON for the requested type.
    pub fn payload_json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    /// Return the event payload as UTF-8 text.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload is not valid UTF-8.
    pub fn payload_text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.payload)
    }
}

/// Versioned service request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRequest {
    pub interface_id: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

impl ServiceRequest {
    /// Decode the request payload from JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload is not valid JSON for the requested type.
    pub fn payload_json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    /// Return the request payload as UTF-8 text.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload is not valid UTF-8.
    pub fn payload_text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.payload)
    }
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

    /// Create a successful UTF-8 text service response.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::ok(text.into().into_bytes())
    }

    /// Create a successful JSON service response.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be encoded as JSON.
    pub fn json<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        serde_json::to_vec(value).map(Self::ok)
    }

    /// Create an empty successful service response.
    #[must_use]
    pub const fn empty() -> Self {
        Self::ok(Vec::new())
    }

    /// Decode a successful response payload from JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the response payload is not valid JSON for the requested type.
    pub fn payload_json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    /// Return a successful response payload as UTF-8 text.
    ///
    /// # Errors
    ///
    /// Returns an error when the response payload is not valid UTF-8.
    pub fn payload_text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.payload)
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

    /// Handle a subscribed host event.
    ///
    /// # Errors
    ///
    /// Returns an error when event handling fails.
    fn handle_event(&mut self, _context: NativeEventContext) -> Result<(), PluginError> {
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

#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn handle_event_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    input_ptr: *const u8,
    input_len: usize,
) -> i32 {
    if input_ptr.is_null() {
        return EVENT_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let Ok(context) = serde_json::from_slice::<NativeEventContext>(input) else {
        return EVENT_STATUS_DECODE_FAILED;
    };
    instance
        .lock()
        .map_or(EVENT_STATUS_PLUGIN_UNAVAILABLE, |mut plugin| {
            result_to_exit_code(plugin.handle_event(context))
        })
}

/// Statically linked native plugin ABI vtable.
#[derive(Clone, Copy)]
pub struct StaticPluginVtable {
    /// Opaque pointer to the plugin instance holder.
    pub instance: *const c_void,
    /// Manifest TOML provider.
    pub manifest: fn(&'static OnceLock<Option<CString>>) -> *const c_char,
    /// Activation hook.
    pub activate: fn(*const c_void) -> i32,
    /// Deactivation hook.
    pub deactivate: fn(*const c_void) -> i32,
    /// Service invocation hook.
    pub invoke_service: fn(*const c_void, *const u8, usize, *mut u8, usize, *mut usize) -> i32,
    /// Event handling hook.
    pub handle_event: fn(*const c_void, *const u8, usize) -> i32,
}

impl std::fmt::Debug for StaticPluginVtable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticPluginVtable").finish_non_exhaustive()
    }
}

unsafe impl Send for StaticPluginVtable {}
unsafe impl Sync for StaticPluginVtable {}

#[doc(hidden)]
pub fn static_manifest_export(
    manifest_toml: &'static str,
    cached: &'static OnceLock<Option<CString>>,
) -> *const c_char {
    manifest_toml_ptr(manifest_toml, cached)
}

#[doc(hidden)]
#[must_use]
pub fn static_activate_export<P: RustPlugin>(instance: *const c_void) -> i32 {
    let instance = unsafe { &*(instance.cast::<OnceLock<Mutex<P>>>()) };
    let instance = plugin_instance::<P>(instance);
    activate_export(instance)
}

#[doc(hidden)]
#[must_use]
pub fn static_deactivate_export<P: RustPlugin>(instance: *const c_void) -> i32 {
    let instance = unsafe { &*(instance.cast::<OnceLock<Mutex<P>>>()) };
    let instance = plugin_instance::<P>(instance);
    deactivate_export(instance)
}

#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn static_invoke_service_export<P: RustPlugin>(
    instance: *const c_void,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    let instance = unsafe { &*(instance.cast::<OnceLock<Mutex<P>>>()) };
    let instance = plugin_instance::<P>(instance);
    invoke_service_export(
        instance,
        input_ptr,
        input_len,
        output_ptr,
        output_capacity,
        output_len,
    )
}

#[doc(hidden)]
#[must_use]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn static_handle_event_export<P: RustPlugin>(
    instance: *const c_void,
    input_ptr: *const u8,
    input_len: usize,
) -> i32 {
    let instance = unsafe { &*(instance.cast::<OnceLock<Mutex<P>>>()) };
    let instance = plugin_instance::<P>(instance);
    handle_event_export(instance, input_ptr, input_len)
}

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

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_handle_event_v1(
            input_ptr: *const u8,
            input_len: usize,
        ) -> i32 {
            let instance = $crate::plugin_instance::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::handle_event_export(instance, input_ptr, input_len)
        }
    };
}

/// Build a static plugin vtable for a [`RustPlugin`] implementation.
#[macro_export]
macro_rules! static_plugin_vtable {
    ($plugin:ty, $manifest_toml:expr) => {{
        static BCODE_STATIC_PLUGIN_INSTANCE: std::sync::OnceLock<std::sync::Mutex<$plugin>> =
            std::sync::OnceLock::new();
        fn manifest(
            cached: &'static std::sync::OnceLock<Option<std::ffi::CString>>,
        ) -> *const std::ffi::c_char {
            $crate::static_manifest_export($manifest_toml, cached)
        }
        $crate::StaticPluginVtable {
            instance: (&BCODE_STATIC_PLUGIN_INSTANCE as *const _) as *const std::ffi::c_void,
            manifest,
            activate: $crate::static_activate_export::<$plugin>,
            deactivate: $crate::static_deactivate_export::<$plugin>,
            invoke_service: $crate::static_invoke_service_export::<$plugin>,
            handle_event: $crate::static_handle_event_export::<$plugin>,
        }
    }};
}

/// Common imports for plugin authors.
pub mod prelude {
    pub use crate::{
        CURRENT_PLUGIN_ABI_VERSION, EVENT_STATUS_DECODE_FAILED, EVENT_STATUS_INVALID_ARGUMENT,
        EVENT_STATUS_OK, EVENT_STATUS_PLUGIN_UNAVAILABLE, EXIT_ERROR, EXIT_OK, EXIT_UNAVAILABLE,
        NativeEventContext, NativeServiceContext, PluginError, PluginEvent, RustPlugin,
        SERVICE_STATUS_BUFFER_TOO_SMALL, SERVICE_STATUS_DECODE_FAILED,
        SERVICE_STATUS_ENCODE_FAILED, SERVICE_STATUS_INVALID_ARGUMENT, SERVICE_STATUS_OK,
        SERVICE_STATUS_PLUGIN_UNAVAILABLE, ServiceError, ServiceRequest, ServiceResponse,
        StaticPluginVtable, export_plugin, static_plugin_vtable,
    };
}

#[cfg(test)]
mod tests {
    use super::{ServiceRequest, ServiceResponse};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct ExamplePayload {
        value: String,
    }

    #[test]
    fn service_request_decodes_json_payload() {
        let request = ServiceRequest {
            interface_id: "example/v1".to_string(),
            operation: "read".to_string(),
            payload: br#"{"value":"hello"}"#.to_vec(),
        };

        let payload = request
            .payload_json::<ExamplePayload>()
            .expect("payload should decode");
        assert_eq!(
            payload,
            ExamplePayload {
                value: "hello".to_string()
            }
        );
    }

    #[test]
    fn service_response_encodes_json_payload() {
        let response = ServiceResponse::json(&ExamplePayload {
            value: "hello".to_string(),
        })
        .expect("payload should encode");

        let payload = response
            .payload_json::<ExamplePayload>()
            .expect("payload should decode");
        assert_eq!(
            payload,
            ExamplePayload {
                value: "hello".to_string()
            }
        );
    }

    #[test]
    fn service_response_round_trips_text_payload() {
        let response = ServiceResponse::text("hello");
        assert_eq!(
            response.payload_text().expect("text should decode"),
            "hello"
        );
    }
}
