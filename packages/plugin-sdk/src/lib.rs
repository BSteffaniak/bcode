#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin author SDK for Bcode native plugins.

pub mod interaction;
pub mod tui;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::ffi::{CString, c_char, c_void};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

/// ABI-safe callback used by plugins to register command contributions during activation.
pub type CommandRegistrationCallback = extern "C" fn(*const u8, usize, *mut c_void);

/// ABI-safe callback used by plugins to emit incremental service events.
pub type ServiceEventCallback = extern "C" fn(*const u8, usize, *mut c_void);

/// Private marker prefix for transparent service response chunks emitted over the streaming
/// callback channel.
#[doc(hidden)]
pub const SERVICE_RESPONSE_CHUNK_PREFIX: &[u8] = b"bcode.internal.service_response_chunk.v1\0";

pub type StreamingServiceFn = fn(
    *const c_void,
    *const u8,
    usize,
    *mut u8,
    usize,
    *mut usize,
    Option<ServiceEventCallback>,
    *mut c_void,
) -> i32;

/// Cloneable cancellation state scoped to one service invocation.
#[derive(Debug, Clone, Default)]
pub struct ServiceCancellation {
    cancelled: Option<Arc<AtomicBool>>,
}

impl ServiceCancellation {
    /// Create cancellation state from a shared flag.
    #[must_use]
    pub const fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled: Some(cancelled),
        }
    }

    /// Return whether host cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
            .as_ref()
            .is_some_and(|cancelled| cancelled.load(Ordering::SeqCst))
    }
}

/// Cloneable command registrar scoped to plugin activation.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandRegistrar {
    callback: Option<CommandRegistrationCallback>,
    user_data: usize,
}

impl CommandRegistrar {
    /// Create a registrar from raw ABI callback parts.
    #[must_use]
    pub fn new(callback: Option<CommandRegistrationCallback>, user_data: *mut c_void) -> Self {
        Self {
            callback,
            user_data: user_data as usize,
        }
    }

    /// Return whether command registration is available.
    #[must_use]
    pub const fn is_available(self) -> bool {
        self.callback.is_some()
    }

    /// Register a command contribution with the host.
    ///
    /// # Errors
    ///
    /// Returns an error if the contribution cannot be serialized.
    pub fn register(
        self,
        contribution: &bcode_command::CommandContribution,
    ) -> Result<(), serde_json::Error> {
        let payload = serde_json::to_vec(contribution)?;
        if let Some(callback) = self.callback {
            callback(
                payload.as_ptr(),
                payload.len(),
                self.user_data as *mut c_void,
            );
        }
        Ok(())
    }
}

unsafe impl Send for CommandRegistrar {}
unsafe impl Sync for CommandRegistrar {}

/// Cloneable event emitter scoped to one service invocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServiceEventEmitter {
    callback: Option<ServiceEventCallback>,
    user_data: usize,
}

impl ServiceEventEmitter {
    /// Create an emitter from raw ABI callback parts.
    #[must_use]
    pub fn new(callback: Option<ServiceEventCallback>, user_data: *mut c_void) -> Self {
        Self {
            callback,
            user_data: user_data as usize,
        }
    }

    /// Return whether this invocation supports incremental events.
    #[must_use]
    pub const fn is_available(self) -> bool {
        self.callback.is_some()
    }

    /// Emit an incremental service event payload.
    pub fn emit(self, payload: &[u8]) {
        if let Some(callback) = self.callback {
            callback(
                payload.as_ptr(),
                payload.len(),
                self.user_data as *mut c_void,
            );
        }
    }
}

unsafe impl Send for ServiceEventEmitter {}
unsafe impl Sync for ServiceEventEmitter {}

/// Current stable native plugin ABI version.
pub const CURRENT_PLUGIN_ABI_VERSION: u16 = 1;

/// Default manifest export symbol for native plugins.
pub const DEFAULT_NATIVE_MANIFEST_SYMBOL: &str = "bcode_plugin_manifest_v1";

/// Default activation hook export symbol for native plugins.
pub const DEFAULT_NATIVE_ACTIVATE_SYMBOL: &str = "bcode_plugin_activate_v1";

/// Default activation-time command registration export symbol for native plugins.
pub const DEFAULT_NATIVE_REGISTER_COMMANDS_SYMBOL: &str = "bcode_plugin_register_commands_v1";

/// Default deactivation hook export symbol for native plugins.
pub const DEFAULT_NATIVE_DEACTIVATE_SYMBOL: &str = "bcode_plugin_deactivate_v1";

/// Default service invocation export symbol for native plugins.
pub const DEFAULT_NATIVE_SERVICE_SYMBOL: &str = "bcode_plugin_invoke_service_v1";

/// Default streaming service invocation export symbol for native plugins.
pub const DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL: &str =
    "bcode_plugin_invoke_service_streaming_v1";

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeServiceContext {
    pub plugin_id: String,
    pub request: ServiceRequest,
    #[serde(default)]
    pub config: PluginConfigContext,
    #[serde(skip)]
    pub events: ServiceEventEmitter,
    #[serde(skip)]
    pub cancellation: ServiceCancellation,
}

/// Resolved plugin configuration delivered by the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigContext {
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub redacted_config: serde_json::Value,
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

impl PluginConfigContext {
    /// Decode the resolved plugin config into a typed value.
    ///
    /// # Errors
    ///
    /// Returns an error when the config cannot deserialize into `T`.
    pub fn typed<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.config.clone())
    }

    /// Decode the resolved plugin config into a typed value, falling back to
    /// `T::default()` when the host did not provide config.
    ///
    /// # Errors
    ///
    /// Returns an error when non-empty config cannot deserialize into `T`.
    pub fn typed_or_default<T>(&self) -> Result<T, serde_json::Error>
    where
        T: Default + DeserializeOwned,
    {
        if self.config.is_null() {
            Ok(T::default())
        } else {
            self.typed()
        }
    }
}

impl NativeServiceContext {
    /// Decode the resolved plugin config into a typed value.
    ///
    /// # Errors
    ///
    /// Returns an error when the config cannot deserialize into `T`.
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        self.config.typed()
    }

    /// Decode the resolved plugin config into a typed value, falling back to
    /// `T::default()` when the host did not provide config.
    ///
    /// # Errors
    ///
    /// Returns an error when non-empty config cannot deserialize into `T`.
    pub fn config_or_default<T>(&self) -> Result<T, serde_json::Error>
    where
        T: Default + DeserializeOwned,
    {
        self.config.typed_or_default()
    }
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

    /// Called when the host provides activation-time command registration.
    ///
    /// # Errors
    ///
    /// Returns an error when command registration fails.
    fn register_commands(&mut self, _registrar: CommandRegistrar) -> Result<(), PluginError> {
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
pub fn register_commands_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    callback: Option<CommandRegistrationCallback>,
    user_data: *mut c_void,
) -> i32 {
    instance.lock().map_or(EXIT_UNAVAILABLE, |mut plugin| {
        result_to_exit_code(plugin.register_commands(CommandRegistrar::new(callback, user_data)))
    })
}

#[doc(hidden)]
pub fn deactivate_export<P: RustPlugin>(instance: &'static Mutex<P>) -> i32 {
    instance.lock().map_or(EXIT_UNAVAILABLE, |mut plugin| {
        result_to_exit_code(plugin.deactivate())
    })
}

/// Trait implemented by native Rust plugins that can handle service calls without holding the
/// SDK-managed plugin instance mutex for the duration of the call.
pub trait ConcurrentRustPlugin: RustPlugin + Sync {
    /// Called when the host activates the plugin using shared plugin state.
    ///
    /// # Errors
    ///
    /// Returns an error when activation fails.
    fn activate_concurrent(&self) -> Result<(), PluginError> {
        Ok(())
    }

    /// Called when the host deactivates the plugin using shared plugin state.
    ///
    /// # Errors
    ///
    /// Returns an error when deactivation fails.
    fn deactivate_concurrent(&self) -> Result<(), PluginError> {
        Ok(())
    }

    /// Invoke a plugin-provided service operation with shared plugin state.
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
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
pub fn plugin_instance_arc<P: ConcurrentRustPlugin>(
    instance: &'static OnceLock<Arc<P>>,
) -> &'static Arc<P> {
    instance.get_or_init(|| Arc::new(P::default()))
}

/// Encode and write a service response to ABI output buffers.
#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn write_service_response(
    response: &ServiceResponse,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    events: ServiceEventEmitter,
) -> i32 {
    let Ok(encoded) = serde_json::to_vec(response) else {
        return SERVICE_STATUS_ENCODE_FAILED;
    };

    unsafe {
        *output_len = encoded.len();
    }
    if output_ptr.is_null() || output_capacity < encoded.len() {
        if events.is_available() {
            emit_service_response_chunks(events, &encoded);
            unsafe {
                *output_len = 0;
            }
            return SERVICE_STATUS_OK;
        }
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }
    SERVICE_STATUS_OK
}

/// Decode and invoke a service with an explicit invocation-scoped event emitter.
#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn invoke_service_with_emitter_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    events: ServiceEventEmitter,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let Ok(mut context) = serde_json::from_slice::<NativeServiceContext>(input) else {
        return SERVICE_STATUS_DECODE_FAILED;
    };
    context.events = events;
    let response = match instance.lock() {
        Ok(mut plugin) => plugin.invoke_service(context),
        Err(_) => return SERVICE_STATUS_PLUGIN_UNAVAILABLE,
    };
    write_service_response(&response, output_ptr, output_capacity, output_len, events)
}

#[doc(hidden)]
#[must_use]
pub fn activate_concurrent_export<P: ConcurrentRustPlugin>(instance: &'static Arc<P>) -> i32 {
    result_to_exit_code(instance.activate_concurrent())
}

#[doc(hidden)]
#[must_use]
pub fn deactivate_concurrent_export<P: ConcurrentRustPlugin>(instance: &'static Arc<P>) -> i32 {
    result_to_exit_code(instance.deactivate_concurrent())
}

/// Decode and invoke a concurrent service with an explicit invocation-scoped event emitter.
#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn invoke_concurrent_service_with_emitter_export<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    events: ServiceEventEmitter,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let Ok(mut context) = serde_json::from_slice::<NativeServiceContext>(input) else {
        return SERVICE_STATUS_DECODE_FAILED;
    };
    context.events = events;
    let response = instance.invoke_service_concurrent(context);
    write_service_response(&response, output_ptr, output_capacity, output_len, events)
}

#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn invoke_concurrent_service_export<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    invoke_concurrent_service_with_emitter_export(
        instance,
        input_ptr,
        input_len,
        output_ptr,
        output_capacity,
        output_len,
        ServiceEventEmitter::default(),
    )
}

#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::too_many_arguments)]
pub fn invoke_concurrent_service_streaming_export<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    event_callback: Option<ServiceEventCallback>,
    event_user_data: *mut c_void,
) -> i32 {
    invoke_concurrent_service_with_emitter_export(
        instance,
        input_ptr,
        input_len,
        output_ptr,
        output_capacity,
        output_len,
        ServiceEventEmitter::new(event_callback, event_user_data),
    )
}

const SERVICE_RESPONSE_CHUNK_DATA_SIZE: usize = 256 * 1024;

fn emit_service_response_chunks(events: ServiceEventEmitter, encoded: &[u8]) {
    for chunk in encoded.chunks(SERVICE_RESPONSE_CHUNK_DATA_SIZE) {
        let mut payload = Vec::with_capacity(SERVICE_RESPONSE_CHUNK_PREFIX.len() + chunk.len());
        payload.extend_from_slice(SERVICE_RESPONSE_CHUNK_PREFIX);
        payload.extend_from_slice(chunk);
        events.emit(&payload);
    }
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
    let Ok(mut context) = serde_json::from_slice::<NativeServiceContext>(input) else {
        return SERVICE_STATUS_DECODE_FAILED;
    };
    context.events = ServiceEventEmitter::default();
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
#[allow(clippy::too_many_arguments)]
pub fn invoke_service_streaming_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    event_callback: Option<ServiceEventCallback>,
    event_user_data: *mut c_void,
) -> i32 {
    invoke_service_with_emitter_export(
        instance,
        input_ptr,
        input_len,
        output_ptr,
        output_capacity,
        output_len,
        ServiceEventEmitter::new(event_callback, event_user_data),
    )
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

use std::future::Future;
use std::pin::Pin;

/// Host action requested after a statically linked plugin CLI handler completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticCliHostAction {
    /// Launch the host TUI and open a plugin-owned surface.
    OpenTuiSurface {
        /// Plugin surface kind.
        surface_kind: String,
        /// Repository path used as surface context.
        repo_path: Option<std::path::PathBuf>,
        /// Plugin-defined string options for the surface.
        options: std::collections::BTreeMap<String, String>,
    },
}

/// Result of a statically linked plugin CLI invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaticCliOutcome {
    /// Optional action for the CLI host to perform.
    pub host_action: Option<StaticCliHostAction>,
}

/// Future returned by a statically linked plugin CLI handler.
pub type StaticCliFuture =
    Pin<Box<dyn Future<Output = Result<StaticCliOutcome, String>> + Send + 'static>>;

/// Rust-native CLI contribution from a statically linked plugin.
///
/// This API deliberately uses Clap directly. It is not part of the dynamic-library ABI.
#[derive(Clone, Copy)]
pub struct StaticCliRegistration {
    /// Build the plugin-owned top-level command.
    pub command: fn() -> clap::Command,
    /// Invoke the plugin command using matches from the composed root parser.
    pub invoke: fn(clap::ArgMatches) -> StaticCliFuture,
}

impl std::fmt::Debug for StaticCliRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticCliRegistration")
            .finish_non_exhaustive()
    }
}

pub type StaticCommandRegistrationFn =
    fn(*const c_void, Option<CommandRegistrationCallback>, *mut c_void) -> i32;

/// Statically linked native plugin ABI vtable.
#[derive(Clone, Copy)]
pub struct StaticPluginVtable {
    /// Opaque pointer to the plugin instance holder.
    pub instance: *const c_void,
    /// Manifest TOML provider.
    pub manifest: fn(&'static OnceLock<Option<CString>>) -> *const c_char,
    /// Activation hook.
    pub activate: fn(*const c_void) -> i32,
    /// Activation-time command registration hook.
    pub register_commands: Option<StaticCommandRegistrationFn>,
    /// Deactivation hook.
    pub deactivate: fn(*const c_void) -> i32,
    /// Service invocation hook.
    pub invoke_service: fn(*const c_void, *const u8, usize, *mut u8, usize, *mut usize) -> i32,
    /// Streaming service invocation hook.
    pub invoke_service_streaming: StreamingServiceFn,
    /// Event handling hook.
    pub handle_event: fn(*const c_void, *const u8, usize) -> i32,
    /// Native TUI registry provider, when statically linked.
    pub tui_registry: Option<fn() -> crate::tui::PluginTuiRegistry>,
    /// Renderer-neutral interaction registry provider, when statically linked.
    pub interaction_registry: Option<fn() -> crate::interaction::PluginInteractionRegistry>,
    /// Rust-native CLI contribution provider, when statically linked.
    pub cli_registration: Option<fn() -> StaticCliRegistration>,
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
pub fn static_register_commands_export<P: RustPlugin>(
    instance: *const c_void,
    callback: Option<CommandRegistrationCallback>,
    user_data: *mut c_void,
) -> i32 {
    let instance = unsafe { &*(instance.cast::<OnceLock<Mutex<P>>>()) };
    let instance = plugin_instance::<P>(instance);
    register_commands_export(instance, callback, user_data)
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
#[allow(clippy::too_many_arguments)]
pub fn static_invoke_service_streaming_export<P: RustPlugin>(
    instance: *const c_void,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    event_callback: Option<ServiceEventCallback>,
    event_user_data: *mut c_void,
) -> i32 {
    let instance = unsafe { &*(instance.cast::<OnceLock<Mutex<P>>>()) };
    let instance = plugin_instance::<P>(instance);
    invoke_service_streaming_export(
        instance,
        input_ptr,
        input_len,
        output_ptr,
        output_capacity,
        output_len,
        event_callback,
        event_user_data,
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
        pub extern "C" fn bcode_plugin_register_commands_v1(
            callback: Option<$crate::CommandRegistrationCallback>,
            user_data: *mut std::ffi::c_void,
        ) -> i32 {
            let instance = $crate::plugin_instance::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::register_commands_export(instance, callback, user_data)
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
        pub extern "C" fn bcode_plugin_invoke_service_streaming_v1(
            input_ptr: *const u8,
            input_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
            event_callback: Option<$crate::ServiceEventCallback>,
            event_user_data: *mut std::ffi::c_void,
        ) -> i32 {
            let instance = $crate::plugin_instance::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::invoke_service_streaming_export(
                instance,
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
                event_callback,
                event_user_data,
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

#[macro_export]
macro_rules! export_concurrent_plugin {
    ($plugin:ty, $manifest_toml:expr) => {
        static BCODE_PLUGIN_INSTANCE: std::sync::OnceLock<std::sync::Arc<$plugin>> =
            std::sync::OnceLock::new();
        static BCODE_PLUGIN_MANIFEST: std::sync::OnceLock<Option<std::ffi::CString>> =
            std::sync::OnceLock::new();

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_manifest_v1() -> *const std::ffi::c_char {
            $crate::manifest_toml_ptr($manifest_toml, &BCODE_PLUGIN_MANIFEST)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_activate_v1() -> i32 {
            let instance = $crate::plugin_instance_arc::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::activate_concurrent_export(instance)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_deactivate_v1() -> i32 {
            let instance = $crate::plugin_instance_arc::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::deactivate_concurrent_export(instance)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_invoke_service_v1(
            input_ptr: *const u8,
            input_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
        ) -> i32 {
            let instance = $crate::plugin_instance_arc::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::invoke_concurrent_service_export(
                instance,
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
            )
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_invoke_service_streaming_v1(
            input_ptr: *const u8,
            input_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
            event_callback: Option<$crate::ServiceEventCallback>,
            event_user_data: *mut std::ffi::c_void,
        ) -> i32 {
            let instance = $crate::plugin_instance_arc::<$plugin>(&BCODE_PLUGIN_INSTANCE);
            $crate::invoke_concurrent_service_streaming_export(
                instance,
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
                event_callback,
                event_user_data,
            )
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bcode_plugin_handle_event_v1(
            _input_ptr: *const u8,
            _input_len: usize,
        ) -> i32 {
            $crate::EVENT_STATUS_OK
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
            register_commands: Some($crate::static_register_commands_export::<$plugin>),
            deactivate: $crate::static_deactivate_export::<$plugin>,
            invoke_service: $crate::static_invoke_service_export::<$plugin>,
            invoke_service_streaming: $crate::static_invoke_service_streaming_export::<$plugin>,
            handle_event: $crate::static_handle_event_export::<$plugin>,
            tui_registry: None,
            interaction_registry: None,
            cli_registration: None,
        }
    }};
}

#[macro_export]
macro_rules! static_concurrent_plugin_vtable {
    ($plugin:ty, $manifest_toml:expr) => {{
        static BCODE_STATIC_PLUGIN_INSTANCE: std::sync::OnceLock<std::sync::Arc<$plugin>> =
            std::sync::OnceLock::new();
        fn manifest(
            cached: &'static std::sync::OnceLock<Option<std::ffi::CString>>,
        ) -> *const std::ffi::c_char {
            $crate::static_manifest_export($manifest_toml, cached)
        }
        fn invoke_service(
            instance: *const std::ffi::c_void,
            input_ptr: *const u8,
            input_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
        ) -> i32 {
            let instance =
                unsafe { &*(instance.cast::<std::sync::OnceLock<std::sync::Arc<$plugin>>>()) };
            let instance = $crate::plugin_instance_arc::<$plugin>(instance);
            $crate::invoke_concurrent_service_export(
                instance,
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
            )
        }
        #[allow(clippy::too_many_arguments)]
        fn invoke_service_streaming(
            instance: *const std::ffi::c_void,
            input_ptr: *const u8,
            input_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
            event_callback: Option<$crate::ServiceEventCallback>,
            event_user_data: *mut std::ffi::c_void,
        ) -> i32 {
            let instance =
                unsafe { &*(instance.cast::<std::sync::OnceLock<std::sync::Arc<$plugin>>>()) };
            let instance = $crate::plugin_instance_arc::<$plugin>(instance);
            $crate::invoke_concurrent_service_streaming_export(
                instance,
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
                event_callback,
                event_user_data,
            )
        }
        fn handle_event(_: *const std::ffi::c_void, _: *const u8, _: usize) -> i32 {
            $crate::EVENT_STATUS_OK
        }
        fn activate(instance: *const std::ffi::c_void) -> i32 {
            let instance =
                unsafe { &*(instance.cast::<std::sync::OnceLock<std::sync::Arc<$plugin>>>()) };
            let instance = $crate::plugin_instance_arc::<$plugin>(instance);
            $crate::activate_concurrent_export(instance)
        }
        fn deactivate(instance: *const std::ffi::c_void) -> i32 {
            let instance =
                unsafe { &*(instance.cast::<std::sync::OnceLock<std::sync::Arc<$plugin>>>()) };
            let instance = $crate::plugin_instance_arc::<$plugin>(instance);
            $crate::deactivate_concurrent_export(instance)
        }
        $crate::StaticPluginVtable {
            instance: (&BCODE_STATIC_PLUGIN_INSTANCE as *const _) as *const std::ffi::c_void,
            manifest,
            activate,
            register_commands: None,
            deactivate,
            invoke_service,
            invoke_service_streaming,
            handle_event,
            tui_registry: None,
            interaction_registry: None,
            cli_registration: None,
        }
    }};
}

/// Common imports for plugin authors.
pub mod prelude {
    pub use crate::{
        CURRENT_PLUGIN_ABI_VERSION, CommandRegistrar, ConcurrentRustPlugin,
        DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL, EVENT_STATUS_DECODE_FAILED,
        EVENT_STATUS_INVALID_ARGUMENT, EVENT_STATUS_OK, EVENT_STATUS_PLUGIN_UNAVAILABLE,
        EXIT_ERROR, EXIT_OK, EXIT_UNAVAILABLE, NativeEventContext, NativeServiceContext,
        PluginError, PluginEvent, RustPlugin, SERVICE_STATUS_BUFFER_TOO_SMALL,
        SERVICE_STATUS_DECODE_FAILED, SERVICE_STATUS_ENCODE_FAILED,
        SERVICE_STATUS_INVALID_ARGUMENT, SERVICE_STATUS_OK, SERVICE_STATUS_PLUGIN_UNAVAILABLE,
        ServiceError, ServiceEventCallback, ServiceEventEmitter, ServiceRequest, ServiceResponse,
        StaticPluginVtable, StreamingServiceFn, export_concurrent_plugin, export_plugin,
        static_concurrent_plugin_vtable, static_plugin_vtable,
        tui::{
            PluginSessionEvent, PluginSessionEventReplay, PluginSessionEventSubscription,
            PluginSessionEventSubscriptionRequest, PluginTuiAction, PluginTuiHost,
            PluginTuiHostError, PluginTuiRegistry, PluginTuiSurface, PluginTuiSurfaceFactory,
            PluginTuiSurfaceOpenRequest, TokioPluginTuiHost,
        },
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
