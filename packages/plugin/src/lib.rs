#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_plugin_sdk::tui::PluginTuiRegistry;
use bcode_plugin_sdk::{
    CURRENT_PLUGIN_ABI_VERSION, CommandRegistrationCallback, DEFAULT_NATIVE_ACTIVATE_SYMBOL,
    DEFAULT_NATIVE_DEACTIVATE_SYMBOL, DEFAULT_NATIVE_EVENT_SYMBOL, DEFAULT_NATIVE_MANIFEST_SYMBOL,
    DEFAULT_NATIVE_REGISTER_COMMANDS_SYMBOL, DEFAULT_NATIVE_SERVICE_SYMBOL,
    DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL, EVENT_STATUS_OK, NativeEventContext,
    NativeServiceContext, PluginConfigContext, PluginEvent, SERVICE_RESPONSE_CHUNK_PREFIX,
    SERVICE_STATUS_OK, ServiceEventCallback, ServiceRequest, StaticPluginVtable,
};
pub use bcode_plugin_sdk::{ServiceError, ServiceResponse};
use libloading::Library;
use semver::Version;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{CStr, CString};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::Instant;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

/// Default plugin manifest file name.
pub const DEFAULT_PLUGIN_MANIFEST_FILE: &str = "bcode-plugin.toml";

type ManifestFn = unsafe extern "C" fn() -> *const std::ffi::c_char;
type LifecycleFn = unsafe extern "C" fn() -> i32;
type RegisterCommandsFn =
    unsafe extern "C" fn(Option<CommandRegistrationCallback>, *mut std::ffi::c_void) -> i32;
type ServiceFn = unsafe extern "C" fn(*const u8, usize, *mut u8, usize, *mut usize) -> i32;
type StreamingServiceFn = unsafe extern "C" fn(
    *const u8,
    usize,
    *mut u8,
    usize,
    *mut usize,
    Option<ServiceEventCallback>,
    *mut std::ffi::c_void,
) -> i32;
type EventFn = unsafe extern "C" fn(*const u8, usize) -> i32;

struct ServiceCallbackState<'a> {
    on_event: &'a mut dyn FnMut(Vec<u8>),
    response_chunks: Vec<Vec<u8>>,
}

extern "C" fn service_event_callback(
    payload_ptr: *const u8,
    payload_len: usize,
    user_data: *mut std::ffi::c_void,
) {
    if payload_ptr.is_null() || user_data.is_null() {
        return;
    }
    let payload = unsafe { std::slice::from_raw_parts(payload_ptr, payload_len) }.to_vec();
    let state = unsafe { &mut *user_data.cast::<ServiceCallbackState<'_>>() };
    if let Some(chunk) = payload.strip_prefix(SERVICE_RESPONSE_CHUNK_PREFIX) {
        state.response_chunks.push(chunk.to_vec());
    } else {
        (state.on_event)(payload);
    }
}

/// Plugin manifest loaded from `bcode-plugin.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: Version,
    #[serde(default)]
    pub services: Vec<PluginService>,
    #[serde(default)]
    pub tui_surfaces: Vec<PluginTuiSurfaceDeclaration>,
    #[serde(default)]
    pub command_contributions: Vec<PluginCommandContribution>,
    #[serde(default)]
    pub event_subscriptions: Vec<PluginEventSubscription>,
    #[serde(default)]
    pub config: Option<PluginManifestConfig>,
    #[serde(default)]
    pub concurrency: PluginConcurrencyConfig,
    pub runtime: PluginRuntime,
}

/// Native TUI surface declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTuiSurfaceDeclaration {
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Command palette/action contribution declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandContribution {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub surface: Option<String>,
}

/// Service interface declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginService {
    pub interface_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub concurrency: Option<PluginConcurrencyConfig>,
    #[serde(default)]
    pub class: Option<PluginInvocationClass>,
}

/// Plugin config declaration from a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifestConfig {
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub schema_version: Option<u16>,
    #[serde(default)]
    pub schema_file: Option<PathBuf>,
    /// Additional top-level config sections that should be treated as aliases for this plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<PluginConfigAlias>,
    /// Lightweight ownership labels for plugin-owned config categories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
}

/// Plugin-owned config alias declaration from a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigAlias {
    /// User-facing top-level config section or dotted path.
    pub section: String,
    /// Optional reason, normally `legacy`, `compatibility`, or `short_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PluginManifestConfig {
    /// Return the primary config section plus manifest-declared aliases.
    #[must_use]
    pub fn sections(&self) -> Vec<&str> {
        self.section
            .iter()
            .map(String::as_str)
            .chain(self.aliases.iter().map(|alias| alias.section.as_str()))
            .collect()
    }

    /// Validate the manifest-declared config metadata without loading plugin code.
    #[must_use]
    pub fn validation_errors(&self) -> Vec<PluginConfigMetadataError> {
        let mut errors = Vec::new();
        let mut seen = BTreeSet::new();
        for section in self.sections() {
            if section.trim().is_empty() {
                errors.push(PluginConfigMetadataError::EmptySection);
            } else if !seen.insert(section.to_string()) {
                errors.push(PluginConfigMetadataError::DuplicateSection(
                    section.to_string(),
                ));
            }
        }
        for category in &self.categories {
            if category.trim().is_empty() {
                errors.push(PluginConfigMetadataError::EmptyCategory);
            }
        }
        errors
    }
}

/// Manifest-declared config metadata validation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginConfigMetadataError {
    /// A section or alias section was blank.
    EmptySection,
    /// A section was declared more than once.
    DuplicateSection(String),
    /// A config category was blank.
    EmptyCategory,
}

/// Event subscription declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEventSubscription {
    pub topic: String,
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
    #[serde(default = "default_streaming_service_symbol")]
    pub streaming_service_symbol: String,
    #[serde(default = "default_event_symbol")]
    pub event_symbol: String,
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

/// Resolved plugin config extension metadata with plugin ownership attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigExtension {
    pub plugin_id: String,
    pub section: Option<String>,
    pub aliases: Vec<PluginConfigAlias>,
    pub categories: Vec<String>,
    pub schema_version: Option<u16>,
    pub schema_file: Option<PathBuf>,
}

impl PluginConfigExtension {
    /// Return the primary config section plus manifest-declared aliases.
    #[must_use]
    pub fn sections(&self) -> Vec<&str> {
        self.section
            .iter()
            .map(String::as_str)
            .chain(self.aliases.iter().map(|alias| alias.section.as_str()))
            .collect()
    }
}

impl RegisteredPlugin {
    /// Return this plugin's manifest-declared config extension metadata, if any.
    #[must_use]
    pub fn config_extension(&self) -> Option<PluginConfigExtension> {
        let config = self.manifest.config.as_ref()?;
        Some(PluginConfigExtension {
            plugin_id: self.manifest.id.clone(),
            section: config.section.clone(),
            aliases: config.aliases.clone(),
            categories: config.categories.clone(),
            schema_version: config.schema_version,
            schema_file: config.schema_file.clone(),
        })
    }
}

/// Resolved per-plugin host configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPluginConfig {
    pub config: serde_json::Value,
    pub redacted_config: serde_json::Value,
}

impl ResolvedPluginConfig {
    /// Create a resolved plugin config from raw and redacted JSON values.
    #[must_use]
    pub const fn new(config: serde_json::Value, redacted_config: serde_json::Value) -> Self {
        Self {
            config,
            redacted_config,
        }
    }
}

/// Return manifest-declared plugin command contributions with plugin ownership.
#[must_use]
pub fn plugin_command_contributions(
    plugins: &[RegisteredPlugin],
) -> Vec<PluginOwnedCommandContribution> {
    plugins
        .iter()
        .flat_map(|plugin| {
            plugin
                .manifest
                .command_contributions
                .iter()
                .cloned()
                .map(|command| PluginOwnedCommandContribution {
                    plugin_id: plugin.manifest.id.clone(),
                    command,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Command contribution with plugin ownership attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginOwnedCommandContribution {
    pub plugin_id: String,
    pub command: PluginCommandContribution,
}

/// Return manifest-declared plugin config extension metadata for registered plugins.
#[must_use]
pub fn plugin_config_extensions(plugins: &[RegisteredPlugin]) -> Vec<PluginConfigExtension> {
    plugins
        .iter()
        .filter_map(RegisteredPlugin::config_extension)
        .collect()
}

/// Return manifest-declared plugin config metadata validation errors with plugin IDs attached.
#[must_use]
pub fn plugin_config_metadata_errors(
    plugins: &[RegisteredPlugin],
) -> Vec<PluginConfigMetadataDiagnostic> {
    plugins
        .iter()
        .filter_map(|plugin| {
            let config = plugin.manifest.config.as_ref()?;
            Some((plugin, config.validation_errors()))
        })
        .flat_map(|(plugin, errors)| {
            errors
                .into_iter()
                .map(|error| PluginConfigMetadataDiagnostic {
                    plugin_id: plugin.manifest.id.clone(),
                    manifest_path: plugin.manifest_path.clone(),
                    error,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Manifest-declared config metadata validation error with plugin ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfigMetadataDiagnostic {
    pub plugin_id: String,
    pub manifest_path: PathBuf,
    pub error: PluginConfigMetadataError,
}

/// Statically bundled plugin registration.
#[derive(Debug, Clone, Copy)]
pub struct StaticBundledPlugin {
    pub manifest_toml: &'static str,
    pub vtable: StaticPluginVtable,
}

impl StaticBundledPlugin {
    /// Create a statically bundled plugin registration.
    #[must_use]
    pub const fn new(manifest_toml: &'static str, vtable: StaticPluginVtable) -> Self {
        Self {
            manifest_toml,
            vtable,
        }
    }
}

#[derive(Debug)]
enum LoadedPluginBackend {
    Dynamic {
        _library: ManuallyDrop<Library>,
        activate: LifecycleFn,
        register_commands: Option<RegisterCommandsFn>,
        deactivate: LifecycleFn,
        invoke_service: ServiceFn,
        invoke_service_streaming: Option<StreamingServiceFn>,
        handle_event: EventFn,
    },
    Static {
        vtable: StaticPluginVtable,
    },
}

/// Loaded native plugin.
#[derive(Debug)]
pub struct LoadedPlugin {
    manifest: PluginManifest,
    backend: LoadedPluginBackend,
    config: ResolvedPluginConfig,
}

impl LoadedPlugin {
    /// Return the loaded plugin manifest.
    #[must_use]
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Return the resolved plugin config.
    #[must_use]
    pub const fn config(&self) -> &ResolvedPluginConfig {
        &self.config
    }

    /// Set the resolved host config for this loaded plugin.
    pub fn set_config(&mut self, config: ResolvedPluginConfig) {
        self.config = config;
    }

    /// Activate the plugin.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin activation hook returns a non-zero code.
    pub fn activate(&self) -> Result<(), PluginLoadError> {
        let code = match &self.backend {
            LoadedPluginBackend::Dynamic { activate, .. } => unsafe { activate() },
            LoadedPluginBackend::Static { vtable } => (vtable.activate)(vtable.instance),
        };
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

    /// Register plugin-owned commands through the plugin activation registration hook.
    ///
    /// # Errors
    ///
    /// Returns an error if the hook returns a non-zero code.
    pub fn register_commands(
        &self,
        registry: &mut bcode_command::CommandRegistry,
    ) -> Result<(), PluginLoadError> {
        extern "C" fn register_command_callback(
            payload: *const u8,
            payload_len: usize,
            user_data: *mut std::ffi::c_void,
        ) {
            if payload.is_null() || user_data.is_null() {
                return;
            }
            let bytes = unsafe { std::slice::from_raw_parts(payload, payload_len) };
            let Ok(contribution) =
                serde_json::from_slice::<bcode_command::CommandContribution>(bytes)
            else {
                return;
            };
            let registry = unsafe { &mut *(user_data.cast::<bcode_command::CommandRegistry>()) };
            registry.register(contribution);
        }

        let code = match &self.backend {
            LoadedPluginBackend::Dynamic {
                register_commands: Some(register_commands),
                ..
            } => unsafe {
                register_commands(
                    Some(register_command_callback),
                    std::ptr::from_mut(registry).cast::<std::ffi::c_void>(),
                )
            },
            LoadedPluginBackend::Dynamic {
                register_commands: None,
                ..
            } => 0,
            LoadedPluginBackend::Static { vtable } => {
                vtable.register_commands.map_or(0, |register_commands| {
                    register_commands(
                        vtable.instance,
                        Some(register_command_callback),
                        std::ptr::from_mut(registry).cast::<std::ffi::c_void>(),
                    )
                })
            }
        };
        if code == 0 {
            Ok(())
        } else {
            Err(PluginLoadError::LifecycleFailed {
                plugin_id: self.manifest.id.clone(),
                hook: "register_commands",
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
        let code = match &self.backend {
            LoadedPluginBackend::Dynamic { deactivate, .. } => unsafe { deactivate() },
            LoadedPluginBackend::Static { vtable } => (vtable.deactivate)(vtable.instance),
        };
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
        self.invoke_service_with_events(interface_id, operation, payload, |_| {})
    }

    /// Invoke a service operation on this plugin and receive incremental service events.
    ///
    /// # Errors
    ///
    /// Returns an error when request encoding, FFI invocation, or response decoding fails.
    pub fn invoke_service_with_events(
        &self,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        on_event: impl FnMut(Vec<u8>),
    ) -> Result<ServiceResponse, PluginLoadError> {
        self.invoke_service_with_events_and_cancellation(
            interface_id,
            operation,
            payload,
            on_event,
            bcode_plugin_sdk::ServiceCancellation::default(),
        )
    }

    fn invoke_service_with_events_and_cancellation(
        &self,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        mut on_event: impl FnMut(Vec<u8>),
        cancellation: bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let context = NativeServiceContext {
            plugin_id: self.manifest.id.clone(),
            request: ServiceRequest {
                interface_id: interface_id.into(),
                operation: operation.into(),
                payload,
            },
            config: PluginConfigContext {
                config: self.config.config.clone(),
                redacted_config: self.config.redacted_config.clone(),
                secrets: BTreeMap::new(),
            },
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            cancellation,
        };
        let input = serde_json::to_vec(&context).map_err(PluginLoadError::ServiceEncode)?;
        let output_capacity = 1024 * 1024;
        let mut output_len = 0_usize;
        let mut output = vec![0_u8; output_capacity];
        let mut callback_state = ServiceCallbackState {
            on_event: &mut on_event,
            response_chunks: Vec::new(),
        };
        let event_user_data = (&raw mut callback_state).cast::<std::ffi::c_void>();
        let status = self.invoke_service_raw(
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            output.len(),
            &raw mut output_len,
            Some(service_event_callback),
            event_user_data,
        );
        if output_len > output_capacity {
            return Err(PluginLoadError::ServiceResponseTooLarge {
                plugin_id: self.manifest.id.clone(),
                capacity: output_capacity,
                required: output_len,
            });
        }
        if status != SERVICE_STATUS_OK {
            return Err(PluginLoadError::ServiceInvokeFailed {
                plugin_id: self.manifest.id.clone(),
                code: status,
            });
        }
        if callback_state.response_chunks.is_empty() {
            output.truncate(output_len);
        } else {
            output = callback_state.response_chunks.concat();
        }
        serde_json::from_slice(&output).map_err(PluginLoadError::ServiceDecode)
    }

    /// Handle a host event for this plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when event encoding fails or the plugin handler returns a non-zero code.
    pub fn handle_event(
        &self,
        topic: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<(), PluginLoadError> {
        let context = NativeEventContext {
            plugin_id: self.manifest.id.clone(),
            event: PluginEvent {
                topic: topic.into(),
                payload,
            },
        };
        let input = serde_json::to_vec(&context).map_err(PluginLoadError::EventEncode)?;
        let status = match &self.backend {
            LoadedPluginBackend::Dynamic { handle_event, .. } => unsafe {
                handle_event(input.as_ptr(), input.len())
            },
            LoadedPluginBackend::Static { vtable } => {
                (vtable.handle_event)(vtable.instance, input.as_ptr(), input.len())
            }
        };
        if status == EVENT_STATUS_OK {
            Ok(())
        } else {
            Err(PluginLoadError::EventHandlerFailed {
                plugin_id: self.manifest.id.clone(),
                code: status,
            })
        }
    }

    /// Invoke a service operation on this plugin with JSON request and response payloads.
    ///
    /// # Errors
    ///
    /// Returns an error when the typed request cannot be encoded, invocation fails, the plugin
    /// returns a service error, or the typed response cannot be decoded.
    pub fn invoke_service_json<Q, R>(
        &self,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        request: &Q,
    ) -> Result<R, PluginServiceCallError>
    where
        Q: Serialize,
        R: DeserializeOwned,
    {
        let payload = serde_json::to_vec(request).map_err(PluginServiceCallError::RequestEncode)?;
        let response = self.invoke_service(interface_id, operation, payload)?;
        decode_service_response(response)
    }

    /// Return true while the dynamic library is retained by this loaded plugin.
    #[must_use]
    pub const fn is_library_retained(&self) -> bool {
        matches!(self.backend, LoadedPluginBackend::Dynamic { .. })
    }

    #[allow(clippy::too_many_arguments)]
    fn invoke_service_raw(
        &self,
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        event_callback: Option<ServiceEventCallback>,
        event_user_data: *mut std::ffi::c_void,
    ) -> i32 {
        match &self.backend {
            LoadedPluginBackend::Dynamic {
                invoke_service,
                invoke_service_streaming,
                ..
            } => unsafe {
                invoke_service_streaming.as_ref().map_or_else(
                    || {
                        invoke_service(
                            input_ptr,
                            input_len,
                            output_ptr,
                            output_capacity,
                            output_len,
                        )
                    },
                    |invoke_service_streaming| {
                        invoke_service_streaming(
                            input_ptr,
                            input_len,
                            output_ptr,
                            output_capacity,
                            output_len,
                            event_callback,
                            event_user_data,
                        )
                    },
                )
            },
            LoadedPluginBackend::Static { vtable } => (vtable.invoke_service_streaming)(
                vtable.instance,
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
                event_callback,
                event_user_data,
            ),
        }
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
    #[error(
        "plugin '{plugin_id}' service response exceeded {capacity} byte buffer ({required} bytes required)"
    )]
    ServiceResponseTooLarge {
        plugin_id: String,
        capacity: usize,
        required: usize,
    },
    #[error("plugin '{plugin_id}' service invocation failed with code {code}")]
    ServiceInvokeFailed { plugin_id: String, code: i32 },
    #[error("failed to encode plugin event: {0}")]
    EventEncode(#[source] serde_json::Error),
    #[error("plugin '{plugin_id}' event handler failed with code {code}")]
    EventHandlerFailed { plugin_id: String, code: i32 },
    #[error("plugin invocation {invocation_id:?} was cancelled before it started")]
    InvocationCancelled { invocation_id: PluginInvocationId },
    #[error("plugin '{plugin_id}' {hook} hook failed with code {code}")]
    LifecycleFailed {
        plugin_id: String,
        hook: &'static str,
        code: i32,
    },
    #[error("plugin '{plugin_id}' TUI surface open failed: {message}")]
    TuiSurfaceOpen { plugin_id: String, message: String },
}

/// Errors returned by typed plugin service calls.
#[derive(Debug, Error)]
pub enum PluginServiceCallError {
    #[error("plugin invocation failed: {0}")]
    Invoke(#[from] PluginLoadError),
    #[error("service returned error {code}: {message}")]
    Service { code: String, message: String },
    #[error("failed to encode typed service request: {0}")]
    RequestEncode(#[source] serde_json::Error),
    #[error("failed to decode typed service response: {0}")]
    ResponseDecode(#[source] serde_json::Error),
}

/// Decode a plugin service response as JSON.
///
/// # Errors
///
/// Returns an error when the service returned an error payload or response decoding fails.
pub fn decode_service_response<R: DeserializeOwned>(
    response: ServiceResponse,
) -> Result<R, PluginServiceCallError> {
    if let Some(error) = response.error {
        return Err(PluginServiceCallError::Service {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(PluginServiceCallError::ResponseDecode)
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

/// Return manifest IDs for statically bundled plugin registrations.
///
/// # Errors
///
/// Returns an error when a static plugin manifest cannot be parsed.
pub fn static_bundled_plugin_ids(
    plugins: &[StaticBundledPlugin],
) -> Result<Vec<String>, PluginLoadError> {
    plugins
        .iter()
        .map(|plugin| {
            let manifest: PluginManifest =
                toml::from_str(plugin.manifest_toml).map_err(|source| {
                    PluginLoadError::ExportedManifestParse {
                        library: PathBuf::from("<static>"),
                        source,
                    }
                })?;
            Ok(manifest.id)
        })
        .collect()
}

/// Filter static plugin registrations according to an enable/disable policy.
///
/// # Errors
///
/// Returns an error when a static plugin manifest cannot be parsed.
pub fn filter_selected_static_plugins(
    plugins: &[StaticBundledPlugin],
    selection: &PluginSelection,
) -> Result<Vec<(PluginManifest, StaticPluginVtable)>, PluginLoadError> {
    plugins
        .iter()
        .map(|plugin| {
            let manifest: PluginManifest =
                toml::from_str(plugin.manifest_toml).map_err(|source| {
                    PluginLoadError::ExportedManifestParse {
                        library: PathBuf::from("<static>"),
                        source,
                    }
                })?;
            Ok((manifest, plugin.vtable))
        })
        .filter(|plugin| match plugin {
            Ok((manifest, _)) => selection.is_enabled(&manifest.id),
            Err(_) => true,
        })
        .collect()
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
        let manifests = plugins
            .iter()
            .map(LoadedPlugin::manifest)
            .collect::<Vec<_>>();
        Self::from_manifests(manifests)
    }

    /// Build a registry from loaded plugin manifests.
    #[must_use]
    pub fn from_manifests<'a>(manifests: impl IntoIterator<Item = &'a PluginManifest>) -> Self {
        let mut registry = Self::default();
        for manifest in manifests {
            for service in &manifest.services {
                registry
                    .providers
                    .entry(service.interface_id.clone())
                    .or_default()
                    .insert(manifest.id.clone());
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

    /// Return the unique plugin ID that provides a service interface.
    ///
    /// # Errors
    ///
    /// Returns an error when the interface is not registered or has multiple providers.
    pub fn unique_provider(&self, interface_id: &str) -> Result<&str, PluginLoadError> {
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

/// Plugin manifest concurrency policy configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginConcurrencyConfig {
    /// Allow the runtime to execute invocations concurrently.
    #[default]
    Concurrent,
    /// Serialize invocations for this plugin or service.
    Exclusive,
    /// Allow up to `max` concurrent invocations.
    Limited { max: usize },
}

impl From<&PluginConcurrencyConfig> for PluginConcurrency {
    fn from(config: &PluginConcurrencyConfig) -> Self {
        match config {
            PluginConcurrencyConfig::Exclusive => Self::Exclusive,
            PluginConcurrencyConfig::Limited { max } => Self::Limited(*max),
            PluginConcurrencyConfig::Concurrent => Self::Concurrent,
        }
    }
}

/// Plugin service execution concurrency policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginConcurrency {
    /// Allow unconstrained concurrent plugin execution.
    #[default]
    Concurrent,
    /// Serialize invocations for this plugin on a dedicated worker.
    Exclusive,
    /// Reserve support for bounded concurrent plugin execution.
    Limited(usize),
}

/// Plugin invocation scheduling class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginInvocationClass {
    /// Control-plane requests that should remain responsive.
    Control,
    /// Metadata or discovery requests.
    Query,
    /// Long-running tool execution requests.
    ToolExecution,
    /// Model provider requests.
    ModelProvider,
    /// Event delivery requests.
    EventDelivery,
    /// Unclassified plugin request.
    #[default]
    Service,
}

/// Ownership scope for a plugin invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginInvocationScope {
    /// Daemon-owned invocation without a specific client/session owner.
    #[default]
    Global,
    /// Invocation owned by a client/session execution path.
    Session {
        /// Client that initiated or owns the invocation, when known.
        #[serde(default)]
        client_id: Option<String>,
        /// Session that owns the invocation.
        session_id: String,
        /// Model/provider turn that owns the invocation, when applicable.
        #[serde(default)]
        turn_id: Option<String>,
        /// Runtime work item represented by this invocation, when applicable.
        #[serde(default)]
        work_id: Option<String>,
    },
}

impl PluginInvocationScope {
    /// Construct a session-owned invocation scope.
    #[must_use]
    pub fn session(session_id: impl Into<String>) -> Self {
        Self::Session {
            client_id: None,
            session_id: session_id.into(),
            turn_id: None,
            work_id: None,
        }
    }

    /// Return this scope with a client owner attached.
    #[must_use]
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        if let Self::Session { client_id: id, .. } = &mut self {
            *id = Some(client_id.into());
        }
        self
    }

    /// Return this scope with a turn owner attached.
    #[must_use]
    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        if let Self::Session { turn_id: id, .. } = &mut self {
            *id = Some(turn_id.into());
        }
        self
    }

    /// Return this scope with a runtime work owner attached.
    #[must_use]
    pub fn with_work_id(mut self, work_id: impl Into<String>) -> Self {
        if let Self::Session { work_id: id, .. } = &mut self {
            *id = Some(work_id.into());
        }
        self
    }
}

/// Runtime plugin invocation identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginInvocationId(u64);

impl PluginInvocationId {
    /// Return the numeric invocation identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct PluginInvocationCancelHandle {
    id: PluginInvocationId,
    cancelled: Arc<AtomicBool>,
}

impl PluginInvocationCancelHandle {
    /// Return the plugin invocation identifier.
    #[must_use]
    pub const fn id(&self) -> PluginInvocationId {
        self.id
    }

    /// Request cancellation for this invocation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Return whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Plugin executor status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginExecutorStatus {
    pub plugin_id: String,
    pub concurrency: PluginConcurrency,
    pub running: usize,
    pub queued: usize,
    pub queued_control: usize,
    pub queued_query: usize,
    pub queued_tool_execution: usize,
    pub queued_model_provider: usize,
    pub queued_event_delivery: usize,
    pub queued_service: usize,
    pub completed: u64,
    pub failed: u64,
}

#[derive(Debug)]
struct PluginResourceLimiter {
    global: Arc<Semaphore>,
    per_session: Mutex<BTreeMap<String, Arc<Semaphore>>>,
    max_global: usize,
    max_per_session: usize,
}

#[derive(Debug)]
struct PluginResourcePermit {
    _global: OwnedSemaphorePermit,
    _session: Option<OwnedSemaphorePermit>,
    wait_ms: u128,
    active_global: usize,
    active_session: Option<usize>,
}

impl PluginResourceLimiter {
    fn new(max_global: usize, max_per_session: usize) -> Self {
        let max_global = max_global.max(1);
        Self {
            global: Arc::new(Semaphore::new(max_global)),
            per_session: Mutex::default(),
            max_global,
            max_per_session: max_per_session.max(1),
        }
    }

    async fn acquire(
        &self,
        scope: &PluginInvocationScope,
    ) -> Result<PluginResourcePermit, PluginLoadError> {
        let started_at = Instant::now();
        let session = match scope {
            PluginInvocationScope::Global => None,
            PluginInvocationScope::Session { session_id, .. } => {
                let semaphore = self.session_semaphore(session_id);
                Some(semaphore.acquire_owned().await.map_err(|_| {
                    PluginLoadError::PluginNotLoaded("plugin resource limiter".to_string())
                })?)
            }
        };
        let global =
            self.global.clone().acquire_owned().await.map_err(|_| {
                PluginLoadError::PluginNotLoaded("plugin resource limiter".to_string())
            })?;
        Ok(PluginResourcePermit {
            _global: global,
            _session: session,
            wait_ms: started_at.elapsed().as_millis(),
            active_global: self
                .max_global
                .saturating_sub(self.global.available_permits()),
            active_session: self.active_session_count(scope),
        })
    }

    fn active_session_count(&self, scope: &PluginInvocationScope) -> Option<usize> {
        match scope {
            PluginInvocationScope::Global => None,
            PluginInvocationScope::Session { session_id, .. } => self
                .per_session
                .lock()
                .expect("plugin resource limiter session map locks")
                .get(session_id)
                .map(|semaphore| {
                    self.max_per_session
                        .saturating_sub(semaphore.available_permits())
                }),
        }
    }

    fn session_semaphore(&self, session_id: &str) -> Arc<Semaphore> {
        self.per_session
            .lock()
            .expect("plugin resource limiter session map locks")
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.max_per_session)))
            .clone()
    }
}

impl Default for PluginResourceLimiter {
    fn default() -> Self {
        Self::new(64, 4)
    }
}

#[derive(Debug, Default)]
struct PluginExecutorMetrics {
    running: AtomicUsize,
    queued: AtomicUsize,
    queued_control: AtomicUsize,
    queued_query: AtomicUsize,
    queued_tool_execution: AtomicUsize,
    queued_model_provider: AtomicUsize,
    queued_event_delivery: AtomicUsize,
    queued_service: AtomicUsize,
    completed: AtomicU64,
    failed: AtomicU64,
}

impl PluginExecutorMetrics {
    fn snapshot(&self, plugin_id: String, concurrency: PluginConcurrency) -> PluginExecutorStatus {
        PluginExecutorStatus {
            plugin_id,
            concurrency,
            running: self.running.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            queued_control: self.queued_control.load(Ordering::Relaxed),
            queued_query: self.queued_query.load(Ordering::Relaxed),
            queued_tool_execution: self.queued_tool_execution.load(Ordering::Relaxed),
            queued_model_provider: self.queued_model_provider.load(Ordering::Relaxed),
            queued_event_delivery: self.queued_event_delivery.load(Ordering::Relaxed),
            queued_service: self.queued_service.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }

    fn enqueue(&self, class: PluginInvocationClass) {
        self.queued.fetch_add(1, Ordering::Relaxed);
        self.queue_for_class(class).fetch_add(1, Ordering::Relaxed);
    }

    fn dequeue(&self, class: PluginInvocationClass) {
        self.queued.fetch_sub(1, Ordering::Relaxed);
        self.queue_for_class(class).fetch_sub(1, Ordering::Relaxed);
    }

    const fn queue_for_class(&self, class: PluginInvocationClass) -> &AtomicUsize {
        match class {
            PluginInvocationClass::Control => &self.queued_control,
            PluginInvocationClass::Query => &self.queued_query,
            PluginInvocationClass::ToolExecution => &self.queued_tool_execution,
            PluginInvocationClass::ModelProvider => &self.queued_model_provider,
            PluginInvocationClass::EventDelivery => &self.queued_event_delivery,
            PluginInvocationClass::Service => &self.queued_service,
        }
    }
}

static NEXT_PLUGIN_INVOCATION_ID: AtomicU64 = AtomicU64::new(1);

fn next_plugin_invocation_id() -> PluginInvocationId {
    PluginInvocationId(NEXT_PLUGIN_INVOCATION_ID.fetch_add(1, Ordering::Relaxed))
}

#[derive(Debug)]
struct PluginInvocation {
    id: PluginInvocationId,
    class: PluginInvocationClass,
    enqueued_at: Instant,
    scope: PluginInvocationScope,
    interface_id: String,
    operation: String,
    payload: Vec<u8>,
    cancellation: PluginInvocationCancelHandle,
    response: oneshot::Sender<Result<ServiceResponse, PluginLoadError>>,
    event_sender: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

#[derive(Debug)]
struct PluginEventInvocation {
    id: PluginInvocationId,
    class: PluginInvocationClass,
    enqueued_at: Instant,
    topic: String,
    payload: Vec<u8>,
    response: oneshot::Sender<Result<(), PluginLoadError>>,
}

#[derive(Debug)]
enum PluginExecutorMessage {
    Service(PluginInvocation),
    Event(PluginEventInvocation),
    Deactivate(oneshot::Sender<Result<(), PluginLoadError>>),
}

/// Event yielded by a running streaming service invocation.
#[derive(Debug)]
pub enum StreamingServiceInvocationEvent {
    /// Plugin emitted an invocation event payload.
    Event(Vec<u8>),
    /// Plugin produced its final service response.
    Response(Result<ServiceResponse, PluginLoadError>),
}

/// Running streaming plugin service invocation.
#[derive(Debug)]
pub struct StreamingServiceInvocation {
    response: oneshot::Receiver<Result<ServiceResponse, PluginLoadError>>,
    events: mpsc::UnboundedReceiver<Vec<u8>>,
    pub cancel: PluginInvocationCancelHandle,
    resource_permit: Option<Arc<PluginResourcePermit>>,
}

impl StreamingServiceInvocation {
    /// Wait for the next invocation event or final response.
    ///
    /// # Errors
    ///
    /// Returns an error when the response channel closes before a plugin response is produced.
    pub async fn next_event(&mut self) -> Result<StreamingServiceInvocationEvent, PluginLoadError> {
        tokio::select! {
            event = self.events.recv() => {
                match event {
                    Some(payload) => Ok(StreamingServiceInvocationEvent::Event(payload)),
                    None => (&mut self.response).await.map_or_else(
                        |_| {
                            Err(PluginLoadError::ServiceInvokeFailed {
                                plugin_id: "streaming-service".to_owned(),
                                code: -1,
                            })
                        },
                        |response| Ok(StreamingServiceInvocationEvent::Response(response)),
                    ),
                }
            }
            response = &mut self.response => {
                match response {
                    Ok(response) => Ok(StreamingServiceInvocationEvent::Response(response)),
                    Err(_error) => Err(PluginLoadError::ServiceInvokeFailed {
                        plugin_id: "streaming-service".to_owned(),
                        code: -1,
                    }),
                }
            }
        }
    }

    /// Try to receive a queued invocation event without blocking.
    #[must_use]
    pub fn try_recv_event(&mut self) -> Option<Vec<u8>> {
        self.events.try_recv().ok()
    }
}

/// Handle to a plugin-local executor.
#[derive(Debug)]
pub struct PluginExecutorHandle {
    manifest: PluginManifest,
    concurrency: PluginConcurrency,
    executor: PluginExecutorKind,
    metrics: Arc<PluginExecutorMetrics>,
}

#[derive(Debug)]
enum PluginExecutorKind {
    Exclusive(mpsc::Sender<PluginExecutorMessage>),
    Concurrent(Arc<LoadedPlugin>, Option<Arc<Semaphore>>),
}

impl PluginExecutorHandle {
    #[must_use]
    const fn new(
        manifest: PluginManifest,
        concurrency: PluginConcurrency,
        executor: PluginExecutorKind,
        metrics: Arc<PluginExecutorMetrics>,
    ) -> Self {
        Self {
            manifest,
            concurrency,
            executor,
            metrics,
        }
    }

    /// Return the loaded plugin manifest.
    #[must_use]
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Return the plugin concurrency policy.
    #[must_use]
    pub const fn concurrency(&self) -> PluginConcurrency {
        self.concurrency
    }

    /// Return a point-in-time executor status snapshot.
    #[must_use]
    pub fn status(&self) -> PluginExecutorStatus {
        self.metrics
            .snapshot(self.manifest.id.clone(), self.concurrency)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_service_with_events_scoped(
        &self,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
        class: PluginInvocationClass,
        scope: PluginInvocationScope,
        invocation_id: PluginInvocationId,
        cancel: PluginInvocationCancelHandle,
        response: oneshot::Sender<Result<ServiceResponse, PluginLoadError>>,
        event_sender: mpsc::UnboundedSender<Vec<u8>>,
        response_receiver: oneshot::Receiver<Result<ServiceResponse, PluginLoadError>>,
        event_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Result<StreamingServiceInvocation, PluginLoadError> {
        match &self.executor {
            PluginExecutorKind::Exclusive(sender) => {
                let invocation = PluginInvocation {
                    id: invocation_id,
                    class,
                    enqueued_at: Instant::now(),
                    scope: scope.clone(),
                    interface_id,
                    operation,
                    payload,
                    cancellation: cancel.clone(),
                    response,
                    event_sender: Some(event_sender),
                };
                self.metrics.enqueue(class);
                sender
                    .send(PluginExecutorMessage::Service(invocation))
                    .await
                    .map_err(|_| {
                        self.metrics.dequeue(class);
                        PluginLoadError::PluginNotLoaded(self.manifest.id.clone())
                    })?;
            }
            PluginExecutorKind::Concurrent(plugin, semaphore) => {
                let permit = match semaphore {
                    Some(semaphore) => {
                        Some(semaphore.clone().acquire_owned().await.map_err(|_| {
                            PluginLoadError::PluginNotLoaded(self.manifest.id.clone())
                        })?)
                    }
                    None => None,
                };
                let (unused_response, _) = oneshot::channel();
                let invocation = PluginInvocation {
                    id: invocation_id,
                    class,
                    enqueued_at: Instant::now(),
                    scope,
                    interface_id,
                    operation,
                    payload,
                    cancellation: cancel.clone(),
                    response: unused_response,
                    event_sender: Some(event_sender),
                };
                let plugin = Arc::clone(plugin);
                let metrics = Arc::clone(&self.metrics);
                tokio::task::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        execute_plugin_service_invocation(&plugin, invocation, &metrics)
                    })
                    .await
                    .unwrap_or_else(|error| Err(PluginLoadError::Io(std::io::Error::other(error))));
                    let _ = response.send(result);
                });
            }
        }
        Ok(StreamingServiceInvocation {
            response: response_receiver,
            events: event_receiver,
            cancel,
            resource_permit: None,
        })
    }
    async fn invoke_service_scoped(
        &self,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
        class: PluginInvocationClass,
        scope: PluginInvocationScope,
        event_sender: Option<mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let invocation_id = next_plugin_invocation_id();
        let invocation = PluginInvocation {
            id: invocation_id,
            class,
            enqueued_at: Instant::now(),
            scope,
            interface_id,
            operation,
            payload,
            cancellation: PluginInvocationCancelHandle {
                id: invocation_id,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
            response: oneshot::channel().0,
            event_sender,
        };
        match &self.executor {
            PluginExecutorKind::Exclusive(sender) => {
                let (response, receiver) = oneshot::channel();
                let invocation = PluginInvocation {
                    response,
                    ..invocation
                };
                self.metrics.enqueue(class);
                sender
                    .send(PluginExecutorMessage::Service(invocation))
                    .await
                    .map_err(|_| {
                        self.metrics.dequeue(class);
                        PluginLoadError::PluginNotLoaded(self.manifest.id.clone())
                    })?;
                receiver
                    .await
                    .map_err(|_| PluginLoadError::PluginNotLoaded(self.manifest.id.clone()))?
            }
            PluginExecutorKind::Concurrent(plugin, semaphore) => {
                let permit = match semaphore {
                    Some(semaphore) => {
                        Some(semaphore.clone().acquire_owned().await.map_err(|_| {
                            PluginLoadError::PluginNotLoaded(self.manifest.id.clone())
                        })?)
                    }
                    None => None,
                };
                let plugin = Arc::clone(plugin);
                let metrics = Arc::clone(&self.metrics);
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    execute_plugin_service_invocation(&plugin, invocation, &metrics)
                })
                .await
                .map_err(|_| PluginLoadError::PluginNotLoaded(self.manifest.id.clone()))?
            }
        }
    }

    async fn handle_event(&self, topic: String, payload: Vec<u8>) -> Result<(), PluginLoadError> {
        match &self.executor {
            PluginExecutorKind::Exclusive(sender) => {
                let (response, receiver) = oneshot::channel();
                self.metrics.enqueue(PluginInvocationClass::EventDelivery);
                sender
                    .send(PluginExecutorMessage::Event(PluginEventInvocation {
                        id: next_plugin_invocation_id(),
                        class: PluginInvocationClass::EventDelivery,
                        enqueued_at: Instant::now(),
                        topic,
                        payload,
                        response,
                    }))
                    .await
                    .map_err(|_| {
                        self.metrics.dequeue(PluginInvocationClass::EventDelivery);
                        PluginLoadError::PluginNotLoaded(self.manifest.id.clone())
                    })?;
                receiver
                    .await
                    .map_err(|_| PluginLoadError::PluginNotLoaded(self.manifest.id.clone()))?
            }
            PluginExecutorKind::Concurrent(plugin, semaphore) => {
                let permit = match semaphore {
                    Some(semaphore) => {
                        Some(semaphore.clone().acquire_owned().await.map_err(|_| {
                            PluginLoadError::PluginNotLoaded(self.manifest.id.clone())
                        })?)
                    }
                    None => None,
                };
                let plugin = Arc::clone(plugin);
                let metrics = Arc::clone(&self.metrics);
                let invocation = PluginEventInvocation {
                    id: next_plugin_invocation_id(),
                    class: PluginInvocationClass::EventDelivery,
                    enqueued_at: Instant::now(),
                    topic,
                    payload,
                    response: oneshot::channel().0,
                };
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    execute_plugin_event_invocation(&plugin, invocation, &metrics)
                })
                .await
                .map_err(|_| PluginLoadError::PluginNotLoaded(self.manifest.id.clone()))?
            }
        }
    }

    async fn deactivate(&self) -> Result<(), PluginLoadError> {
        let (response, receiver) = oneshot::channel();
        match &self.executor {
            PluginExecutorKind::Exclusive(sender) => {
                sender
                    .send(PluginExecutorMessage::Deactivate(response))
                    .await
                    .map_err(|_| PluginLoadError::PluginNotLoaded(self.manifest.id.clone()))?;
                receiver
                    .await
                    .map_err(|_| PluginLoadError::PluginNotLoaded(self.manifest.id.clone()))?
            }
            PluginExecutorKind::Concurrent(plugin, _) => plugin.deactivate(),
        }
    }
}

/// Immutable plugin registry used for routing and metadata.
#[derive(Debug, Clone)]
pub struct PluginRegistry {
    manifests: BTreeMap<String, PluginManifest>,
    service_registry: PluginServiceRegistry,
    service_policies: BTreeMap<(String, String), ServiceRuntimePolicy>,
}

impl PluginRegistry {
    #[must_use]
    fn from_manifests(manifests: BTreeMap<String, PluginManifest>) -> Self {
        let service_registry = PluginServiceRegistry::from_manifests(manifests.values());
        let mut service_policies = BTreeMap::new();
        for manifest in manifests.values() {
            for service in &manifest.services {
                service_policies.insert(
                    (manifest.id.clone(), service.interface_id.clone()),
                    ServiceRuntimePolicy {
                        concurrency: service.concurrency.as_ref().map_or_else(
                            || PluginConcurrency::from(&manifest.concurrency),
                            PluginConcurrency::from,
                        ),
                        class: service.class,
                    },
                );
            }
        }
        Self {
            manifests,
            service_registry,
            service_policies,
        }
    }

    /// Return loaded plugin manifests keyed by plugin ID.
    #[must_use]
    pub const fn manifests(&self) -> &BTreeMap<String, PluginManifest> {
        &self.manifests
    }

    /// Return the service interface registry.
    #[must_use]
    pub const fn service_registry(&self) -> &PluginServiceRegistry {
        &self.service_registry
    }

    /// Return declared TUI surfaces for one plugin.
    #[must_use]
    pub fn tui_surfaces(&self, plugin_id: &str) -> Option<&[PluginTuiSurfaceDeclaration]> {
        self.manifests
            .get(plugin_id)
            .map(|manifest| manifest.tui_surfaces.as_slice())
    }

    /// Return declared TUI surface metadata by plugin and surface kind.
    #[must_use]
    pub fn tui_surface(
        &self,
        plugin_id: &str,
        surface_kind: &str,
    ) -> Option<&PluginTuiSurfaceDeclaration> {
        self.manifests
            .get(plugin_id)?
            .tui_surfaces
            .iter()
            .find(|surface| surface.kind == surface_kind)
    }

    /// Return runtime policy metadata for a plugin service interface.
    #[must_use]
    pub fn service_policy(
        &self,
        plugin_id: &str,
        interface_id: &str,
    ) -> Option<&ServiceRuntimePolicy> {
        self.service_policies
            .get(&(plugin_id.to_string(), interface_id.to_string()))
    }
}

/// Runtime policy metadata for a declared plugin service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceRuntimePolicy {
    pub concurrency: PluginConcurrency,
    pub class: Option<PluginInvocationClass>,
}

/// Concurrent plugin runtime with plugin-local execution isolation.
#[derive(Debug, Clone)]
pub struct PluginRuntimeHost {
    registry: Arc<PluginRegistry>,
    executors: Arc<BTreeMap<String, Arc<PluginExecutorHandle>>>,
    configs: Arc<BTreeMap<String, ResolvedPluginConfig>>,
    command_registry: Arc<bcode_command::CommandRegistry>,
    tui_registries: Arc<BTreeMap<String, PluginTuiRegistry>>,
    resources: Arc<PluginResourceLimiter>,
}

impl PluginRuntimeHost {
    /// Discover, load, activate, and start plugin executors from default roots.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, loading, activation, or executor startup fails.
    pub fn load_defaults(selection: &PluginSelection) -> Result<Self, PluginLoadError> {
        Self::load_defaults_with_static_bundled(selection, &[])
    }

    /// Discover, load, activate, and start plugin executors from default roots plus static bundled registrations.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, loading, activation, or executor startup fails.
    pub fn load_defaults_with_static_bundled(
        selection: &PluginSelection,
        static_plugins: &[StaticBundledPlugin],
    ) -> Result<Self, PluginLoadError> {
        PluginHost::load_defaults_with_static_bundled(selection, static_plugins).map(Self::from)
    }

    /// Discover, load, activate, and start plugin executors from default roots plus static bundled registrations and config.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, loading, activation, or executor startup fails.
    pub fn load_defaults_with_static_bundled_and_config(
        selection: &PluginSelection,
        static_plugins: &[StaticBundledPlugin],
        configs: BTreeMap<String, ResolvedPluginConfig>,
    ) -> Result<Self, PluginLoadError> {
        PluginHost::load_defaults_with_static_bundled_and_config(selection, static_plugins, configs)
            .map(Self::from)
    }

    /// Return the immutable plugin registry.
    #[must_use]
    pub fn registry(&self) -> &PluginRegistry {
        &self.registry
    }

    /// Return command contributions registered by loaded plugins.
    #[must_use]
    pub fn registered_command_contributions(
        &self,
        surface: &bcode_command::CommandSurface,
    ) -> Vec<bcode_command::CommandContribution> {
        self.command_registry.commands_for_surface(surface)
    }

    /// Return loaded plugin executor handles keyed by plugin ID.
    #[must_use]
    pub fn executors(&self) -> &BTreeMap<String, Arc<PluginExecutorHandle>> {
        &self.executors
    }

    /// Return resolved plugin configs keyed by plugin ID.
    #[must_use]
    pub fn configs(&self) -> &BTreeMap<String, ResolvedPluginConfig> {
        &self.configs
    }

    /// Return native TUI registries keyed by plugin ID.
    #[must_use]
    pub fn tui_registries(&self) -> &BTreeMap<String, PluginTuiRegistry> {
        &self.tui_registries
    }

    /// Return a native TUI registry for a loaded plugin.
    #[must_use]
    pub fn tui_registry(&self, plugin_id: &str) -> Option<&PluginTuiRegistry> {
        self.tui_registries.get(plugin_id)
    }

    /// Return plugin executor status snapshots.
    #[must_use]
    pub fn executor_statuses(&self) -> Vec<PluginExecutorStatus> {
        self.executors
            .values()
            .map(|executor| executor.status())
            .collect()
    }

    /// Return plugin service summaries without waiting for plugin execution.
    #[must_use]
    pub fn service_summaries(&self) -> Vec<(String, PluginService)> {
        self.registry
            .manifests
            .values()
            .flat_map(|manifest| {
                manifest
                    .services
                    .iter()
                    .cloned()
                    .map(|service| (manifest.id.clone(), service))
            })
            .collect()
    }

    /// Return plugin command contributions without waiting for plugin execution.
    #[must_use]
    pub fn command_contributions(&self) -> Vec<PluginOwnedCommandContribution> {
        self.registry
            .manifests
            .values()
            .flat_map(|manifest| {
                manifest
                    .command_contributions
                    .iter()
                    .cloned()
                    .map(|command| PluginOwnedCommandContribution {
                        plugin_id: manifest.id.clone(),
                        command,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Return plugin config extensions without waiting for plugin execution.
    #[must_use]
    pub fn config_extensions(&self) -> Vec<PluginConfigExtension> {
        self.registry
            .manifests
            .values()
            .filter_map(|manifest| {
                let config = manifest.config.as_ref()?;
                Some(PluginConfigExtension {
                    plugin_id: manifest.id.clone(),
                    section: config.section.clone(),
                    aliases: config.aliases.clone(),
                    categories: config.categories.clone(),
                    schema_version: config.schema_version,
                    schema_file: config.schema_file.clone(),
                })
            })
            .collect()
    }

    /// Invoke a service operation on a loaded plugin by ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is not loaded or service invocation fails.
    pub async fn invoke_service(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<ServiceResponse, PluginLoadError> {
        self.invoke_service_scoped(
            plugin_id,
            interface_id,
            operation,
            payload,
            PluginInvocationScope::Global,
        )
        .await
    }

    /// Invoke a service operation on a loaded plugin by ID with explicit ownership scope.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is not loaded or service invocation fails.
    pub async fn invoke_service_scoped(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        scope: PluginInvocationScope,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let interface_id = interface_id.into();
        let operation = operation.into();
        let executor = self
            .executors
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.to_string()))?;
        let class = self
            .registry
            .service_policy(plugin_id, &interface_id)
            .and_then(|policy| policy.class)
            .unwrap_or_else(|| classify_invocation(&interface_id, &operation));
        let resource_permit = self.resources.acquire(&scope).await?;
        tracing::debug!(
            target: "bcode_plugin::resources",
            plugin_id = %plugin_id,
            interface_id = %interface_id,
            operation = %operation,
            scope = ?scope,
            wait_ms = resource_permit.wait_ms,
            active_global = resource_permit.active_global,
            active_session = ?resource_permit.active_session,
            "plugin resource slot acquired"
        );
        executor
            .invoke_service_scoped(interface_id, operation, payload, class, scope, None)
            .await
    }

    /// Invoke a service operation on a loaded plugin by ID and collect incremental events.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is not loaded or service invocation fails.
    pub async fn invoke_service_with_events(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<StreamingServiceInvocation, PluginLoadError> {
        self.invoke_service_with_events_scoped(
            plugin_id,
            interface_id,
            operation,
            payload,
            PluginInvocationScope::Global,
        )
        .await
    }

    /// Invoke a service operation on a loaded plugin by ID with explicit ownership scope and events.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is not loaded or service invocation fails.
    pub async fn invoke_service_with_events_scoped(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        scope: PluginInvocationScope,
    ) -> Result<StreamingServiceInvocation, PluginLoadError> {
        let interface_id = interface_id.into();
        let operation = operation.into();
        let executor = self
            .executors
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.to_string()))?;
        let class = self
            .registry
            .service_policy(plugin_id, &interface_id)
            .and_then(|policy| policy.class)
            .unwrap_or_else(|| classify_invocation(&interface_id, &operation));
        let (response, response_receiver) = oneshot::channel();
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let invocation_id = next_plugin_invocation_id();
        let cancel = PluginInvocationCancelHandle {
            id: invocation_id,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let resource_permit = self.resources.acquire(&scope).await?;
        tracing::debug!(
            target: "bcode_plugin::resources",
            plugin_id = %plugin_id,
            interface_id = %interface_id,
            operation = %operation,
            scope = ?scope,
            wait_ms = resource_permit.wait_ms,
            active_global = resource_permit.active_global,
            active_session = ?resource_permit.active_session,
            "plugin resource slot acquired"
        );
        let mut invocation = executor
            .start_service_with_events_scoped(
                interface_id,
                operation,
                payload,
                class,
                scope,
                invocation_id,
                cancel,
                response,
                event_sender,
                response_receiver,
                event_receiver,
            )
            .await?;
        invocation.resource_permit = Some(Arc::new(resource_permit));
        Ok(invocation)
    }

    /// Invoke a service operation by service interface ID.
    ///
    /// # Errors
    ///
    /// Returns an error when no loaded plugin provides the interface, more than one loaded plugin
    /// provides the interface, or service invocation fails.
    pub async fn invoke_service_by_interface(
        &self,
        interface_id: &str,
        operation: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let plugin_id = self
            .registry
            .service_registry
            .unique_provider(interface_id)?;
        self.invoke_service_scoped(
            plugin_id,
            interface_id,
            operation,
            payload,
            PluginInvocationScope::Global,
        )
        .await
    }

    /// Invoke a service operation on a loaded plugin by ID with JSON payloads.
    ///
    /// # Errors
    ///
    /// Returns an error when the typed request cannot be encoded, invocation fails, the plugin
    /// returns a service error, or the typed response cannot be decoded.
    pub async fn invoke_service_json<Q, R>(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        request: &Q,
    ) -> Result<R, PluginServiceCallError>
    where
        Q: Serialize + Sync,
        R: DeserializeOwned,
    {
        let interface_id = interface_id.into();
        let operation = operation.into();
        let payload = serde_json::to_vec(request).map_err(PluginServiceCallError::RequestEncode)?;
        let response = self
            .invoke_service(plugin_id, interface_id, operation, payload)
            .await?;
        decode_service_response(response)
    }

    /// Invoke a service operation on a loaded plugin by ID with JSON payloads and explicit scope.
    ///
    /// # Errors
    ///
    /// Returns an error when the typed request cannot be encoded, invocation fails, the plugin
    /// returns a service error, or the typed response cannot be decoded.
    pub async fn invoke_service_json_scoped<Q, R>(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        request: &Q,
        scope: PluginInvocationScope,
    ) -> Result<R, PluginServiceCallError>
    where
        Q: Serialize + Sync,
        R: DeserializeOwned,
    {
        let interface_id = interface_id.into();
        let operation = operation.into();
        let payload = serde_json::to_vec(request).map_err(PluginServiceCallError::RequestEncode)?;
        let response = self
            .invoke_service_scoped(plugin_id, interface_id, operation, payload, scope)
            .await?;
        decode_service_response(response)
    }

    /// Invoke a service operation by service interface ID with JSON payloads.
    ///
    /// # Errors
    ///
    /// Returns an error when routing fails, the typed request cannot be encoded, invocation fails,
    /// the plugin returns a service error, or the typed response cannot be decoded.
    pub async fn invoke_service_by_interface_json<Q, R>(
        &self,
        interface_id: &str,
        operation: impl Into<String>,
        request: &Q,
    ) -> Result<R, PluginServiceCallError>
    where
        Q: Serialize + Sync,
        R: DeserializeOwned,
    {
        let operation = operation.into();
        let plugin_id = self
            .registry
            .service_registry
            .unique_provider(interface_id)?;
        self.invoke_service_json(plugin_id, interface_id, operation, request)
            .await
    }

    /// Publish an event to loaded plugins that subscribed to the event topic.
    ///
    /// # Errors
    ///
    /// Returns the first event handler error.
    pub async fn publish_event(
        &self,
        topic: impl Into<String>,
        payload: &[u8],
    ) -> Result<usize, PluginLoadError> {
        let topic = topic.into();
        let subscribers = self
            .registry
            .manifests
            .values()
            .filter(|manifest| manifest_subscribes_to(manifest, &topic))
            .map(|manifest| manifest.id.clone())
            .collect::<Vec<_>>();
        let mut deliveries = Vec::new();
        for plugin_id in subscribers {
            let executor = self
                .executors
                .get(&plugin_id)
                .cloned()
                .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.clone()))?;
            let topic = topic.clone();
            let payload = payload.to_vec();
            deliveries.push(tokio::spawn(async move {
                executor.handle_event(topic, payload).await
            }));
        }
        let mut delivered = 0;
        for delivery in deliveries {
            delivery
                .await
                .map_err(|_| PluginLoadError::PluginNotLoaded("event subscriber".to_string()))??;
            delivered += 1;
        }
        Ok(delivered)
    }

    /// Deactivate all loaded plugins through their plugin-local executors.
    ///
    /// # Errors
    ///
    /// Returns the first deactivation error.
    pub async fn deactivate_all(&self) -> Result<(), PluginLoadError> {
        for plugin_id in self.registry.manifests.keys().rev() {
            if let Some(executor) = self.executors.get(plugin_id) {
                executor.deactivate().await?;
            }
        }
        Ok(())
    }
}

impl From<PluginHost> for PluginRuntimeHost {
    fn from(mut host: PluginHost) -> Self {
        let loaded = std::mem::take(&mut host.loaded);
        let configs = std::mem::take(&mut host.configs);
        let mut manifests = BTreeMap::new();
        let mut executors = BTreeMap::new();
        let mut tui_registries = BTreeMap::new();
        for plugin in loaded {
            let manifest = plugin.manifest().clone();
            let plugin_id = manifest.id.clone();
            manifests.insert(plugin_id.clone(), manifest.clone());
            if let LoadedPluginBackend::Static { vtable } = &plugin.backend
                && let Some(tui_registry) = vtable.tui_registry
            {
                tui_registries.insert(plugin_id.clone(), tui_registry());
            }
            let metrics = Arc::new(PluginExecutorMetrics::default());
            let concurrency = PluginConcurrency::from(&manifest.concurrency);
            let executor = match concurrency {
                PluginConcurrency::Exclusive => {
                    let (sender, receiver) = mpsc::channel(32);
                    spawn_exclusive_plugin_executor(plugin, receiver, Arc::clone(&metrics));
                    PluginExecutorKind::Exclusive(sender)
                }
                PluginConcurrency::Limited(max) => PluginExecutorKind::Concurrent(
                    Arc::new(plugin),
                    Some(Arc::new(Semaphore::new(max.max(1)))),
                ),
                PluginConcurrency::Concurrent => {
                    PluginExecutorKind::Concurrent(Arc::new(plugin), None)
                }
            };
            executors.insert(
                plugin_id,
                Arc::new(PluginExecutorHandle::new(
                    manifest.clone(),
                    concurrency,
                    executor,
                    metrics,
                )),
            );
        }
        Self {
            registry: Arc::new(PluginRegistry::from_manifests(manifests)),
            executors: Arc::new(executors),
            configs: Arc::new(configs),
            tui_registries: Arc::new(tui_registries),
            command_registry: Arc::new(std::mem::take(&mut host.command_registry)),
            resources: Arc::default(),
        }
    }
}

fn execute_plugin_service_invocation(
    plugin: &LoadedPlugin,
    invocation: PluginInvocation,
    metrics: &PluginExecutorMetrics,
) -> Result<ServiceResponse, PluginLoadError> {
    if invocation.cancellation.is_cancelled() {
        metrics.failed.fetch_add(1, Ordering::Relaxed);
        return Err(PluginLoadError::InvocationCancelled {
            invocation_id: invocation.id,
        });
    }
    metrics.running.fetch_add(1, Ordering::Relaxed);
    let started_at = Instant::now();
    tracing::debug!(
        target: "bcode_plugin::runtime",
        plugin_id = %plugin.manifest.id,
        invocation_id = invocation.id.get(),
        class = ?invocation.class,
        scope = ?invocation.scope,
        queue_wait_ms = invocation.enqueued_at.elapsed().as_millis(),
        interface_id = %invocation.interface_id,
        operation = %invocation.operation,
        "plugin service invocation started"
    );
    let response = plugin.invoke_service_with_events_and_cancellation(
        invocation.interface_id,
        invocation.operation,
        invocation.payload,
        |event| {
            if let Some(sender) = &invocation.event_sender {
                let _ = sender.send(event);
            }
        },
        bcode_plugin_sdk::ServiceCancellation::new(Arc::clone(&invocation.cancellation.cancelled)),
    );
    metrics.running.fetch_sub(1, Ordering::Relaxed);
    if response.is_ok() {
        metrics.completed.fetch_add(1, Ordering::Relaxed);
    } else {
        metrics.failed.fetch_add(1, Ordering::Relaxed);
    }
    tracing::debug!(
        target: "bcode_plugin::runtime",
        plugin_id = %plugin.manifest.id,
        invocation_id = invocation.id.get(),
        duration_ms = started_at.elapsed().as_millis(),
        success = response.is_ok(),
        "plugin service invocation finished"
    );
    response
}

fn execute_plugin_event_invocation(
    plugin: &LoadedPlugin,
    invocation: PluginEventInvocation,
    metrics: &PluginExecutorMetrics,
) -> Result<(), PluginLoadError> {
    metrics.running.fetch_add(1, Ordering::Relaxed);
    let started_at = Instant::now();
    tracing::debug!(
        target: "bcode_plugin::runtime",
        plugin_id = %plugin.manifest.id,
        invocation_id = invocation.id.get(),
        class = ?invocation.class,
        queue_wait_ms = invocation.enqueued_at.elapsed().as_millis(),
        topic = %invocation.topic,
        "plugin event invocation started"
    );
    let response = plugin.handle_event(invocation.topic, invocation.payload);
    metrics.running.fetch_sub(1, Ordering::Relaxed);
    if response.is_ok() {
        metrics.completed.fetch_add(1, Ordering::Relaxed);
    } else {
        metrics.failed.fetch_add(1, Ordering::Relaxed);
    }
    tracing::debug!(
        target: "bcode_plugin::runtime",
        plugin_id = %plugin.manifest.id,
        invocation_id = invocation.id.get(),
        duration_ms = started_at.elapsed().as_millis(),
        success = response.is_ok(),
        "plugin event invocation finished"
    );
    response
}

fn spawn_exclusive_plugin_executor(
    plugin: LoadedPlugin,
    mut receiver: mpsc::Receiver<PluginExecutorMessage>,
    metrics: Arc<PluginExecutorMetrics>,
) {
    tokio::task::spawn_blocking(move || {
        let mut active = true;
        while let Some(message) = receiver.blocking_recv() {
            match message {
                PluginExecutorMessage::Service(invocation) => {
                    metrics.dequeue(invocation.class);
                    metrics.running.fetch_add(1, Ordering::Relaxed);
                    let started_at = Instant::now();
                    tracing::debug!(
                        target: "bcode_plugin::runtime",
                        plugin_id = %plugin.manifest.id,
                        invocation_id = invocation.id.get(),
                        class = ?invocation.class,
                        scope = ?invocation.scope,
                        queue_wait_ms = invocation.enqueued_at.elapsed().as_millis(),
                        interface_id = %invocation.interface_id,
                        operation = %invocation.operation,
                        "plugin service invocation started"
                    );
                    let response = if active {
                        plugin.invoke_service_with_events(
                            invocation.interface_id,
                            invocation.operation,
                            invocation.payload,
                            |event| {
                                if let Some(sender) = &invocation.event_sender {
                                    let _ = sender.send(event);
                                }
                            },
                        )
                    } else {
                        Err(PluginLoadError::PluginNotLoaded(plugin.manifest.id.clone()))
                    };
                    metrics.running.fetch_sub(1, Ordering::Relaxed);
                    if response.is_ok() {
                        metrics.completed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        metrics.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    tracing::debug!(
                        target: "bcode_plugin::runtime",
                        plugin_id = %plugin.manifest.id,
                        invocation_id = invocation.id.get(),
                        duration_ms = started_at.elapsed().as_millis(),
                        success = response.is_ok(),
                        "plugin service invocation finished"
                    );
                    let _ = invocation.response.send(response);
                }
                PluginExecutorMessage::Event(invocation) => {
                    metrics.dequeue(invocation.class);
                    metrics.running.fetch_add(1, Ordering::Relaxed);
                    let started_at = Instant::now();
                    tracing::debug!(
                        target: "bcode_plugin::runtime",
                        plugin_id = %plugin.manifest.id,
                        invocation_id = invocation.id.get(),
                        class = ?invocation.class,
                        queue_wait_ms = invocation.enqueued_at.elapsed().as_millis(),
                        topic = %invocation.topic,
                        "plugin event invocation started"
                    );
                    let response = if active {
                        plugin.handle_event(invocation.topic, invocation.payload)
                    } else {
                        Err(PluginLoadError::PluginNotLoaded(plugin.manifest.id.clone()))
                    };
                    metrics.running.fetch_sub(1, Ordering::Relaxed);
                    if response.is_ok() {
                        metrics.completed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        metrics.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    tracing::debug!(
                        target: "bcode_plugin::runtime",
                        plugin_id = %plugin.manifest.id,
                        invocation_id = invocation.id.get(),
                        duration_ms = started_at.elapsed().as_millis(),
                        success = response.is_ok(),
                        "plugin event invocation finished"
                    );
                    let _ = invocation.response.send(response);
                }
                PluginExecutorMessage::Deactivate(response) => {
                    let result = if active {
                        active = false;
                        plugin.deactivate()
                    } else {
                        Ok(())
                    };
                    let _ = response.send(result);
                    break;
                }
            }
        }
        if active {
            let _ = plugin.deactivate();
        }
    });
}

fn classify_invocation(interface_id: &str, operation: &str) -> PluginInvocationClass {
    match (interface_id, operation) {
        ("bcode.tool/v1", "invoke_tool") => PluginInvocationClass::ToolExecution,
        ("bcode.tool/v1", "list_tools") => PluginInvocationClass::Query,
        ("bcode.model-provider/v1", "capabilities" | "models" | "validate_config") => {
            PluginInvocationClass::Query
        }
        ("bcode.model-provider/v1", _) => PluginInvocationClass::ModelProvider,
        ("bcode.agent_profile", "policy_status" | "list_agents" | "agent_context") => {
            PluginInvocationClass::Control
        }
        ("bcode.agent_profile", "evaluate_tool_call") => PluginInvocationClass::Control,
        _ => PluginInvocationClass::Service,
    }
}

fn manifest_subscribes_to(manifest: &PluginManifest, topic: &str) -> bool {
    manifest
        .event_subscriptions
        .iter()
        .any(|subscription| subscription.topic == topic)
}

/// Loaded plugin host retaining activated plugins.
#[derive(Debug)]
pub struct PluginHost {
    loaded: Vec<LoadedPlugin>,
    configs: BTreeMap<String, ResolvedPluginConfig>,
    command_registry: bcode_command::CommandRegistry,
}

impl Default for PluginHost {
    fn default() -> Self {
        let mut command_registry = bcode_command::CommandRegistry::new();
        command_registry.extend(bcode_command::bundled_host_palette_commands());
        Self {
            loaded: Vec::new(),
            configs: BTreeMap::new(),
            command_registry,
        }
    }
}

impl PluginHost {
    /// Discover, load, and activate plugins from default roots.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, loading, or activation fails.
    pub fn load_defaults(selection: &PluginSelection) -> Result<Self, PluginLoadError> {
        Self::load_defaults_with_static_bundled(selection, &[])
    }

    /// Discover, load, and activate plugins from default roots plus static bundled registrations.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, loading, or activation fails.
    pub fn load_defaults_with_static_bundled(
        selection: &PluginSelection,
        static_plugins: &[StaticBundledPlugin],
    ) -> Result<Self, PluginLoadError> {
        Self::load_defaults_with_static_bundled_and_config(
            selection,
            static_plugins,
            BTreeMap::new(),
        )
    }

    /// Discover, load, and activate plugins from default roots plus static bundled registrations and config.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, loading, or activation fails.
    pub fn load_defaults_with_static_bundled_and_config(
        selection: &PluginSelection,
        static_plugins: &[StaticBundledPlugin],
        configs: BTreeMap<String, ResolvedPluginConfig>,
    ) -> Result<Self, PluginLoadError> {
        tracing::debug!(target: "bcode_plugin::startup", "discovering plugins");
        let static_plugins = filter_selected_static_plugins(static_plugins, selection)?;
        let static_ids = static_plugins
            .iter()
            .map(|plugin| plugin.0.id.clone())
            .collect::<BTreeSet<_>>();
        let plugins = filter_selected_plugins(discover_plugins()?, selection)
            .into_iter()
            .filter(|plugin| !static_ids.contains(&plugin.manifest.id))
            .collect::<Vec<_>>();
        tracing::debug!(
            target: "bcode_plugin::startup",
            static_plugins = ?static_plugins
                .iter()
                .map(|plugin| plugin.0.id.as_str())
                .collect::<Vec<_>>(),
            plugins = ?plugins
                .iter()
                .map(|plugin| plugin.manifest.id.as_str())
                .collect::<Vec<_>>(),
            "plugins selected"
        );
        let mut host = Self {
            loaded: Vec::new(),
            configs,
            command_registry: {
                let mut registry = bcode_command::CommandRegistry::new();
                registry.extend(bcode_command::bundled_host_palette_commands());
                registry
            },
        };
        host.load_static_plugins_into(&static_plugins)?;
        host.load_registered_plugins_into(&plugins)?;
        Ok(host)
    }

    /// Load and activate registered plugins.
    ///
    /// # Errors
    ///
    /// Returns an error when loading or activation fails.
    pub fn load_registered_plugins(plugins: &[RegisteredPlugin]) -> Result<Self, PluginLoadError> {
        let mut host = Self::default();
        host.load_registered_plugins_into(plugins)?;
        Ok(host)
    }

    /// Load and activate statically bundled plugins.
    ///
    /// # Errors
    ///
    /// Returns an error when loading or activation fails.
    pub fn load_static_plugins(
        plugins: &[(PluginManifest, StaticPluginVtable)],
    ) -> Result<Self, PluginLoadError> {
        let mut host = Self::default();
        host.load_static_plugins_into(plugins)?;
        Ok(host)
    }

    fn load_static_plugins_into(
        &mut self,
        plugins: &[(PluginManifest, StaticPluginVtable)],
    ) -> Result<(), PluginLoadError> {
        for (manifest, vtable) in plugins {
            tracing::debug!(target: "bcode_plugin::startup", plugin_id = %manifest.id, "loading static plugin");
            let mut loaded = load_static_plugin(manifest.clone(), *vtable)?;
            if let Some(config) = self.configs.get(&manifest.id).cloned() {
                loaded.set_config(config);
            }
            tracing::debug!(target: "bcode_plugin::startup", plugin_id = %loaded.manifest().id, "activating plugin");
            loaded.activate()?;
            loaded.register_commands(&mut self.command_registry)?;
            tracing::debug!(target: "bcode_plugin::startup", plugin_id = %loaded.manifest().id, "plugin activated");
            self.loaded.push(loaded);
        }
        Ok(())
    }

    fn load_registered_plugins_into(
        &mut self,
        plugins: &[RegisteredPlugin],
    ) -> Result<(), PluginLoadError> {
        for plugin in plugins {
            tracing::debug!(target: "bcode_plugin::startup", plugin_id = %plugin.manifest.id, "loading plugin");
            let mut loaded = load_registered_plugin(plugin)?;
            if let Some(config) = self.configs.get(&plugin.manifest.id).cloned() {
                loaded.set_config(config);
            }
            tracing::debug!(target: "bcode_plugin::startup", plugin_id = %loaded.manifest().id, "activating plugin");
            loaded.activate()?;
            loaded.register_commands(&mut self.command_registry)?;
            tracing::debug!(target: "bcode_plugin::startup", plugin_id = %loaded.manifest().id, "plugin activated");
            self.loaded.push(loaded);
        }
        Ok(())
    }

    /// Return loaded plugins.
    #[must_use]
    pub fn loaded_plugins(&self) -> &[LoadedPlugin] {
        &self.loaded
    }

    /// Return command contributions registered by the host and loaded plugins.
    #[must_use]
    pub fn registered_command_contributions(
        &self,
        surface: &bcode_command::CommandSurface,
    ) -> Vec<bcode_command::CommandContribution> {
        self.command_registry.commands_for_surface(surface)
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

    /// Invoke a service operation on a loaded plugin by ID with JSON payloads.
    ///
    /// # Errors
    ///
    /// Returns an error when the typed request cannot be encoded, invocation fails, the plugin
    /// returns a service error, or the typed response cannot be decoded.
    pub fn invoke_service_json<Q, R>(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        request: &Q,
    ) -> Result<R, PluginServiceCallError>
    where
        Q: Serialize,
        R: DeserializeOwned,
    {
        let plugin = self
            .loaded
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.to_string()))?;
        plugin.invoke_service_json(interface_id, operation, request)
    }

    /// Invoke a service operation by service interface ID with JSON payloads.
    ///
    /// # Errors
    ///
    /// Returns an error when routing fails, the typed request cannot be encoded, invocation fails,
    /// the plugin returns a service error, or the typed response cannot be decoded.
    pub fn invoke_service_by_interface_json<Q, R>(
        &self,
        interface_id: &str,
        operation: impl Into<String>,
        request: &Q,
    ) -> Result<R, PluginServiceCallError>
    where
        Q: Serialize,
        R: DeserializeOwned,
    {
        let registry = self.service_registry();
        let plugin_id = registry.unique_provider(interface_id)?;
        self.invoke_service_json(plugin_id, interface_id, operation, request)
    }

    /// Publish an event to loaded plugins that subscribed to the event topic.
    ///
    /// # Errors
    ///
    /// Returns the first event encoding or handler error.
    pub fn publish_event(
        &self,
        topic: impl Into<String>,
        payload: &[u8],
    ) -> Result<usize, PluginLoadError> {
        let topic = topic.into();
        let mut delivered = 0;
        for plugin in &self.loaded {
            if plugin_subscribes_to(plugin, &topic) {
                plugin.handle_event(topic.clone(), payload.to_vec())?;
                delivered += 1;
            }
        }
        Ok(delivered)
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
    tracing::debug!(
        target: "bcode_plugin::startup",
        plugin_id = %plugin.manifest.id,
        abi_version = runtime.abi_version,
        "validating plugin ABI"
    );
    if !runtime.is_current_abi() {
        return Err(PluginLoadError::UnsupportedAbi {
            plugin_id: plugin.manifest.id.clone(),
            actual: runtime.abi_version,
            expected: CURRENT_PLUGIN_ABI_VERSION,
        });
    }

    let library_path = resolve_library_path(&plugin.manifest_path, &runtime.library);
    tracing::debug!(
        target: "bcode_plugin::startup",
        plugin_id = %plugin.manifest.id,
        library = %library_path.display(),
        "loading native library"
    );
    let library =
        unsafe { Library::new(library_path.to_string_lossy().as_ref()) }.map_err(|source| {
            PluginLoadError::LibraryLoad {
                path: library_path.clone(),
                source,
            }
        })?;

    tracing::debug!(target: "bcode_plugin::startup", plugin_id = %plugin.manifest.id, "native library loaded");
    let exported_manifest = load_exported_manifest(&library, &library_path, runtime)?;
    tracing::debug!(target: "bcode_plugin::startup", plugin_id = %plugin.manifest.id, "exported manifest loaded");
    if exported_manifest.id != plugin.manifest.id {
        return Err(PluginLoadError::ManifestIdMismatch {
            file_id: plugin.manifest.id.clone(),
            library_id: exported_manifest.id,
        });
    }

    tracing::debug!(target: "bcode_plugin::startup", plugin_id = %plugin.manifest.id, "loading native symbols");
    let activate = load_lifecycle_symbol(&library, &library_path, &runtime.activate_symbol)?;
    let register_commands = load_register_commands_symbol(&library);
    let deactivate = load_lifecycle_symbol(&library, &library_path, &runtime.deactivate_symbol)?;
    let invoke_service = load_service_symbol(&library, &library_path, &runtime.service_symbol)?;
    let invoke_service_streaming =
        load_streaming_service_symbol(&library, &runtime.streaming_service_symbol);
    let handle_event = load_event_symbol(&library, &library_path, &runtime.event_symbol)?;
    tracing::debug!(target: "bcode_plugin::startup", plugin_id = %plugin.manifest.id, "native symbols loaded");

    Ok(LoadedPlugin {
        manifest: plugin.manifest.clone(),
        backend: LoadedPluginBackend::Dynamic {
            _library: ManuallyDrop::new(library),
            activate,
            register_commands,
            deactivate,
            invoke_service,
            invoke_service_streaming,
            handle_event,
        },
        config: ResolvedPluginConfig::default(),
    })
}

/// Load a statically linked plugin from its manifest and vtable.
///
/// # Errors
///
/// Returns an error when the manifest uses an unsupported ABI or the vtable manifest mismatches.
pub fn load_static_plugin(
    manifest: PluginManifest,
    vtable: StaticPluginVtable,
) -> Result<LoadedPlugin, PluginLoadError> {
    let PluginRuntime::Native(runtime) = &manifest.runtime;
    if !runtime.is_current_abi() {
        return Err(PluginLoadError::UnsupportedAbi {
            plugin_id: manifest.id.clone(),
            actual: runtime.abi_version,
            expected: CURRENT_PLUGIN_ABI_VERSION,
        });
    }

    let manifest_cache = Box::leak(Box::new(std::sync::OnceLock::new()));
    let exported_manifest = load_static_exported_manifest(vtable, manifest_cache)?;
    if exported_manifest.id != manifest.id {
        return Err(PluginLoadError::ManifestIdMismatch {
            file_id: manifest.id.clone(),
            library_id: exported_manifest.id,
        });
    }

    Ok(LoadedPlugin {
        manifest,
        backend: LoadedPluginBackend::Static { vtable },
        config: ResolvedPluginConfig::default(),
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

fn load_static_exported_manifest(
    vtable: StaticPluginVtable,
    manifest_cache: &'static std::sync::OnceLock<Option<CString>>,
) -> Result<PluginManifest, PluginLoadError> {
    let ptr = (vtable.manifest)(manifest_cache);
    if ptr.is_null() {
        return Err(PluginLoadError::NullManifest(PathBuf::from("<static>")));
    }
    let manifest_toml = unsafe { CStr::from_ptr(ptr) }.to_str().map_err(|source| {
        PluginLoadError::ManifestUtf8 {
            library: PathBuf::from("<static>"),
            source,
        }
    })?;
    toml::from_str(manifest_toml).map_err(|source| PluginLoadError::ExportedManifestParse {
        library: PathBuf::from("<static>"),
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

fn load_register_commands_symbol(library: &Library) -> Option<RegisterCommandsFn> {
    let mut symbol = DEFAULT_NATIVE_REGISTER_COMMANDS_SYMBOL.as_bytes().to_vec();
    symbol.push(0);
    unsafe { library.get::<RegisterCommandsFn>(&*symbol).ok().map(|s| *s) }
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

fn load_streaming_service_symbol(
    library: &Library,
    symbol_name: &str,
) -> Option<StreamingServiceFn> {
    let mut symbol = symbol_name.as_bytes().to_vec();
    symbol.push(0);
    unsafe { library.get::<StreamingServiceFn>(&*symbol).ok().map(|s| *s) }
}

fn load_event_symbol(
    library: &Library,
    library_path: &Path,
    symbol: &str,
) -> Result<EventFn, PluginLoadError> {
    let loaded = unsafe { library.get::<EventFn>(symbol.as_bytes()) }.map_err(|source| {
        PluginLoadError::SymbolLoad {
            library: library_path.to_path_buf(),
            symbol: symbol.to_string(),
            source,
        }
    })?;
    Ok(*loaded)
}

fn plugin_subscribes_to(plugin: &LoadedPlugin, topic: &str) -> bool {
    plugin
        .manifest
        .event_subscriptions
        .iter()
        .any(|subscription| subscription.topic == topic)
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

fn default_streaming_service_symbol() -> String {
    DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL.to_string()
}

fn default_event_symbol() -> String {
    DEFAULT_NATIVE_EVENT_SYMBOL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::{SERVICE_STATUS_BUFFER_TOO_SMALL, SERVICE_STATUS_OK};
    use semver::Version;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn manifest_config_supports_aliases_and_categories() {
        let manifest = toml::from_str::<PluginManifest>(
            r#"
id = "bcode.example"
name = "Example"
version = "0.0.1"

[config]
section = "example"
schema_version = 2
aliases = [
    { section = "legacy_example", reason = "legacy" },
    { section = "tools.example" },
]
categories = ["tool", "example"]

[runtime]
type = "native"
abi_version = 1
library = "libexample.dylib"
"#,
        )
        .expect("manifest should parse");
        let config = manifest.config.expect("config should be present");

        assert_eq!(
            config.sections(),
            vec!["example", "legacy_example", "tools.example"]
        );
        assert_eq!(config.categories, vec!["tool", "example"]);
        assert_eq!(config.aliases[0].reason.as_deref(), Some("legacy"));
        assert!(config.validation_errors().is_empty());
    }

    #[test]
    fn manifest_config_validation_reports_invalid_metadata() {
        let config = PluginManifestConfig {
            section: Some("example".to_string()),
            schema_version: None,
            schema_file: None,
            aliases: vec![
                PluginConfigAlias {
                    section: "example".to_string(),
                    reason: None,
                },
                PluginConfigAlias {
                    section: " ".to_string(),
                    reason: None,
                },
            ],
            categories: vec![String::new()],
        };

        assert_eq!(
            config.validation_errors(),
            vec![
                PluginConfigMetadataError::DuplicateSection("example".to_string()),
                PluginConfigMetadataError::EmptySection,
                PluginConfigMetadataError::EmptyCategory,
            ]
        );
    }

    #[test]
    fn static_bundled_plugins_can_be_disabled_by_selection() {
        fn manifest(
            _storage: &'static OnceLock<Option<std::ffi::CString>>,
        ) -> *const std::ffi::c_char {
            std::ptr::null()
        }
        fn lifecycle(_instance: *const std::ffi::c_void) -> i32 {
            SERVICE_STATUS_OK
        }
        fn service(
            _instance: *const std::ffi::c_void,
            _input: *const u8,
            _input_len: usize,
            _output: *mut u8,
            _cap: usize,
            _len: *mut usize,
        ) -> i32 {
            SERVICE_STATUS_OK
        }
        fn event(_instance: *const std::ffi::c_void, _input: *const u8, _input_len: usize) -> i32 {
            SERVICE_STATUS_OK
        }
        let static_plugins = [StaticBundledPlugin::new(
            r#"
id = "bcode.disabled"
name = "Disabled"
version = "0.0.1"

[[services]]
interface_id = "bcode.disabled/v1"

[runtime]
type = "native"
abi_version = 1
library = "libdisabled.dylib"
"#,
            StaticPluginVtable {
                instance: std::ptr::null(),
                manifest,
                activate: lifecycle,
                register_commands: None,
                deactivate: lifecycle,
                invoke_service: service,
                invoke_service_streaming: test_streaming_service,
                tui_registry: None,
                handle_event: event,
            },
        )];
        let selection = PluginSelection {
            enabled: BTreeSet::new(),
            disabled: BTreeSet::from(["bcode.disabled".to_string()]),
        };

        let selected = filter_selected_static_plugins(&static_plugins, &selection)
            .expect("static manifest should parse");

        assert!(selected.is_empty());
    }

    #[test]
    fn static_bundled_plugin_ids_are_derived_from_manifests() {
        fn manifest(
            _storage: &'static OnceLock<Option<std::ffi::CString>>,
        ) -> *const std::ffi::c_char {
            std::ptr::null()
        }
        fn lifecycle(_instance: *const std::ffi::c_void) -> i32 {
            SERVICE_STATUS_OK
        }
        fn service(
            _instance: *const std::ffi::c_void,
            _input: *const u8,
            _input_len: usize,
            _output: *mut u8,
            _cap: usize,
            _len: *mut usize,
        ) -> i32 {
            SERVICE_STATUS_OK
        }
        fn event(_instance: *const std::ffi::c_void, _input: *const u8, _input_len: usize) -> i32 {
            SERVICE_STATUS_OK
        }
        let static_plugins = [StaticBundledPlugin::new(
            r#"
id = "bcode.example-static"
name = "Example Static"
version = "0.0.1"

[runtime]
type = "native"
abi_version = 1
library = "libexample_static.dylib"
"#,
            StaticPluginVtable {
                instance: std::ptr::null(),
                manifest,
                activate: lifecycle,
                register_commands: None,
                deactivate: lifecycle,
                invoke_service: service,
                invoke_service_streaming: test_streaming_service,
                tui_registry: None,
                handle_event: event,
            },
        )];

        assert_eq!(
            static_bundled_plugin_ids(&static_plugins).expect("manifest should parse"),
            vec!["bcode.example-static".to_string()]
        );
    }

    #[test]
    fn registered_plugins_expose_command_contributions() {
        let manifest = toml::from_str::<PluginManifest>(
            r#"
id = "bcode.commands"
name = "Commands"
version = "0.0.1"

[[command_contributions]]
id = "example.run"
title = "Run Example"
description = "Run an example command"
category = "example"
surface = "palette"

[runtime]
type = "native"
abi_version = 1
library = "libcommands.dylib"
"#,
        )
        .expect("manifest should parse");
        let plugin = RegisteredPlugin {
            manifest_path: PathBuf::from("plugins/commands/bcode-plugin.toml"),
            manifest,
        };

        let commands = plugin_command_contributions(&[plugin]);

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].plugin_id, "bcode.commands");
        assert_eq!(commands[0].command.id, "example.run");
        assert_eq!(commands[0].command.surface.as_deref(), Some("palette"));
    }

    #[test]
    fn plugin_host_registers_plugin_commands_during_load() {
        fn register_commands(
            _instance: *const std::ffi::c_void,
            callback: Option<CommandRegistrationCallback>,
            user_data: *mut std::ffi::c_void,
        ) -> i32 {
            let contribution = bcode_command::CommandContribution {
                id: "example.run".to_string(),
                title: "Run Example".to_string(),
                description: Some("Run an example command".to_string()),
                category: Some("example".to_string()),
                surfaces: BTreeSet::from([bcode_command::CommandSurface::Palette]),
                owner: bcode_command::CommandOwner::Plugin {
                    plugin_id: "bcode.commands".to_string(),
                },
                action: bcode_command::CommandAction::Plugin {
                    plugin_id: "bcode.commands".to_string(),
                    command_id: "example.run".to_string(),
                },
            };
            let payload = serde_json::to_vec(&contribution).expect("contribution encodes");
            callback.expect("registration callback")(payload.as_ptr(), payload.len(), user_data);
            SERVICE_STATUS_OK
        }

        let manifest = toml::from_str::<PluginManifest>(
            r#"
id = "bcode.commands"
name = "Commands"
version = "0.0.1"

[[command_contributions]]
id = "example.run"
title = "Run Example"
description = "Run an example command"
category = "example"
surface = "palette"

[runtime]
type = "native"
abi_version = 1
library = "libcommands.dylib"
"#,
        )
        .expect("manifest should parse");
        let loaded = LoadedPlugin {
            config: ResolvedPluginConfig::default(),
            manifest,
            backend: LoadedPluginBackend::Static {
                vtable: StaticPluginVtable {
                    instance: std::ptr::null(),
                    manifest: |_: &'static std::sync::OnceLock<Option<std::ffi::CString>>| {
                        std::ptr::null()
                    },
                    activate: test_activate,
                    register_commands: Some(register_commands),
                    deactivate: test_deactivate,
                    invoke_service: test_service,
                    invoke_service_streaming: test_streaming_service,
                    tui_registry: None,
                    handle_event: test_handle_event,
                },
            },
        };
        let mut registry = bcode_command::CommandRegistry::new();
        loaded
            .register_commands(&mut registry)
            .expect("plugin registers commands");

        let commands = registry.commands_for_surface(&bcode_command::CommandSurface::Palette);

        assert!(commands.iter().any(|command| {
            command.id == "example.run"
                && command.action
                    == bcode_command::CommandAction::Plugin {
                        plugin_id: "bcode.commands".to_string(),
                        command_id: "example.run".to_string(),
                    }
        }));
    }

    #[test]
    fn registered_plugins_expose_config_extension_catalog() {
        let manifest = PluginManifest {
            id: "bcode.example".to_string(),
            name: "Example".to_string(),
            version: Version::new(0, 1, 0),
            services: Vec::new(),
            tui_surfaces: Vec::new(),
            command_contributions: Vec::new(),
            event_subscriptions: Vec::new(),
            config: Some(PluginManifestConfig {
                section: Some("example".to_string()),
                schema_version: Some(1),
                schema_file: Some(PathBuf::from("schema.toml")),
                aliases: vec![PluginConfigAlias {
                    section: "legacy_example".to_string(),
                    reason: Some("legacy".to_string()),
                }],
                categories: vec!["example".to_string()],
            }),
            concurrency: PluginConcurrencyConfig::default(),
            runtime: PluginRuntime::Native(NativePluginRuntime {
                abi_version: 1,
                library: PathBuf::from("libexample.dylib"),
                manifest_symbol: default_manifest_symbol(),
                activate_symbol: default_activate_symbol(),
                deactivate_symbol: default_deactivate_symbol(),
                service_symbol: default_service_symbol(),
                streaming_service_symbol: default_streaming_service_symbol(),
                event_symbol: default_event_symbol(),
            }),
        };
        let plugin = RegisteredPlugin {
            manifest_path: PathBuf::from("plugins/example/bcode-plugin.toml"),
            manifest,
        };

        let extensions = plugin_config_extensions(&[plugin]);

        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].plugin_id, "bcode.example");
        assert_eq!(extensions[0].sections(), vec!["example", "legacy_example"]);
        assert_eq!(extensions[0].schema_version, Some(1));
    }

    #[test]
    fn config_metadata_diagnostics_include_plugin_ownership() {
        let manifest = PluginManifest {
            id: "bcode.invalid".to_string(),
            name: "Invalid".to_string(),
            version: Version::new(0, 1, 0),
            services: Vec::new(),
            tui_surfaces: Vec::new(),
            command_contributions: Vec::new(),
            event_subscriptions: Vec::new(),
            config: Some(PluginManifestConfig {
                section: Some(" ".to_string()),
                schema_version: None,
                schema_file: None,
                aliases: Vec::new(),
                categories: Vec::new(),
            }),
            concurrency: PluginConcurrencyConfig::default(),
            runtime: PluginRuntime::Native(NativePluginRuntime {
                abi_version: 1,
                library: PathBuf::from("libinvalid.dylib"),
                manifest_symbol: default_manifest_symbol(),
                activate_symbol: default_activate_symbol(),
                deactivate_symbol: default_deactivate_symbol(),
                service_symbol: default_service_symbol(),
                streaming_service_symbol: default_streaming_service_symbol(),
                event_symbol: default_event_symbol(),
            }),
        };
        let plugin = RegisteredPlugin {
            manifest_path: PathBuf::from("plugins/invalid/bcode-plugin.toml"),
            manifest,
        };

        let diagnostics = plugin_config_metadata_errors(&[plugin]);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].plugin_id, "bcode.invalid");
        assert_eq!(
            diagnostics[0].error,
            PluginConfigMetadataError::EmptySection
        );
    }

    #[test]
    fn per_session_resource_limit_prevents_one_session_from_exhausting_global_slots() {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        tokio.block_on(async {
            let limiter = Arc::new(PluginResourceLimiter::new(2, 1));
            let session_a = PluginInvocationScope::session("session-a");
            let session_b = PluginInvocationScope::session("session-b");
            let first_a = limiter
                .acquire(&session_a)
                .await
                .expect("first session A permit should acquire");

            let (acquired_sender, mut acquired_receiver) = oneshot::channel();
            tokio::spawn({
                let limiter = Arc::clone(&limiter);
                let session_a = session_a.clone();
                async move {
                    let permit = limiter
                        .acquire(&session_a)
                        .await
                        .expect("second session A permit should acquire");
                    let _ = acquired_sender.send(());
                    drop(permit);
                }
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut acquired_receiver)
                    .await
                    .is_err(),
                "second session A permit should wait on per-session capacity"
            );

            let session_b_permit =
                tokio::time::timeout(Duration::from_millis(100), limiter.acquire(&session_b))
                    .await
                    .expect("session B should not wait behind session A")
                    .expect("session B permit should acquire");
            drop(session_b_permit);
            drop(first_a);

            tokio::time::timeout(Duration::from_millis(100), acquired_receiver)
                .await
                .expect("session A waiter should complete after first permit drops")
                .expect("session A waiter should signal acquisition");
        });
    }

    #[test]
    fn many_waiters_in_one_session_do_not_starve_other_sessions() {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        tokio.block_on(async {
            let limiter = Arc::new(PluginResourceLimiter::new(2, 1));
            let session_a = PluginInvocationScope::session("session-a");
            let session_b = PluginInvocationScope::session("session-b");
            let first_a = limiter
                .acquire(&session_a)
                .await
                .expect("first session A permit should acquire");
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let mut waiters = Vec::new();

            for _ in 0..8 {
                let limiter = Arc::clone(&limiter);
                let session_a = session_a.clone();
                let sender = sender.clone();
                waiters.push(tokio::spawn(async move {
                    let permit = limiter
                        .acquire(&session_a)
                        .await
                        .expect("session A waiter permit should acquire");
                    let _ = sender.send(());
                    drop(permit);
                }));
            }
            drop(sender);

            assert!(
                tokio::time::timeout(Duration::from_millis(10), receiver.recv())
                    .await
                    .is_err(),
                "queued session A waiters should not acquire while session A is at capacity"
            );

            let session_b_permit =
                tokio::time::timeout(Duration::from_millis(100), limiter.acquire(&session_b))
                    .await
                    .expect("session B should acquire despite queued session A waiters")
                    .expect("session B permit should acquire");
            drop(session_b_permit);

            for waiter in waiters {
                waiter.abort();
            }
            drop(first_a);
        });
    }

    #[test]
    fn dropped_resource_permit_releases_session_slot() {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        tokio.block_on(async {
            let limiter = PluginResourceLimiter::new(1, 1);
            let scope = PluginInvocationScope::session("session-a");
            let permit = limiter
                .acquire(&scope)
                .await
                .expect("first permit should acquire");

            assert!(
                tokio::time::timeout(Duration::from_millis(10), limiter.acquire(&scope))
                    .await
                    .is_err(),
                "second permit should wait while slot is held"
            );

            drop(permit);
            let permit = tokio::time::timeout(Duration::from_millis(100), limiter.acquire(&scope))
                .await
                .expect("permit should acquire after previous permit drops")
                .expect("permit should acquire");
            drop(permit);
        });
    }

    #[test]
    fn parses_invocation_class_names_as_manifest_snake_case() {
        let manifest: PluginManifest = toml::from_str(&format!(
            r#"
id = "example.plugin"
name = "Example Plugin"
version = "0.1.0"

[[services]]
interface_id = "bcode.tool/v1"
class = "tool_execution"

[runtime]
type = "native"
abi_version = {CURRENT_PLUGIN_ABI_VERSION}
library = "libexample_plugin.dylib"
"#,
        ))
        .expect("manifest should parse");

        assert_eq!(
            manifest.services[0].class,
            Some(PluginInvocationClass::ToolExecution)
        );

        let encoded = serde_json::to_value(PluginInvocationClass::ToolExecution)
            .expect("invocation class should encode");
        assert_eq!(encoded, serde_json::json!("tool_execution"));
    }

    #[test]
    fn classifies_versioned_tool_and_model_operations() {
        assert_eq!(
            classify_invocation("bcode.tool/v1", "invoke_tool"),
            PluginInvocationClass::ToolExecution
        );
        assert_eq!(
            classify_invocation("bcode.tool/v1", "list_tools"),
            PluginInvocationClass::Query
        );
        assert_eq!(
            classify_invocation("bcode.model-provider/v1", "start_turn"),
            PluginInvocationClass::ModelProvider
        );
        assert_eq!(
            classify_invocation("bcode.model-provider/v1", "models"),
            PluginInvocationClass::Query
        );
    }

    #[test]
    fn invocation_scope_builders_attach_session_ownership() {
        let scope = PluginInvocationScope::session("session-1")
            .with_client_id("client-1")
            .with_turn_id("turn-1")
            .with_work_id("work-1");

        assert_eq!(
            scope,
            PluginInvocationScope::Session {
                client_id: Some("client-1".to_string()),
                session_id: "session-1".to_string(),
                turn_id: Some("turn-1".to_string()),
                work_id: Some("work-1".to_string()),
            }
        );
    }

    #[test]
    fn bundled_plugin_manifests_parse() {
        for manifest_toml in [
            include_str!("../../../plugins/bedrock-provider-plugin/bcode-plugin.toml"),
            include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
            include_str!("../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
            include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
            include_str!("../../../plugins/openai-compatible-provider-plugin/bcode-plugin.toml"),
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
        ] {
            toml::from_str::<PluginManifest>(manifest_toml).expect("bundled manifest should parse");
        }
    }

    #[test]
    fn static_service_event_callback_delivers_stream_events() {
        let plugin = LoadedPlugin {
            config: ResolvedPluginConfig::default(),
            manifest: test_manifest("events"),
            backend: LoadedPluginBackend::Static {
                vtable: test_streaming_vtable(),
            },
        };
        let mut events = Vec::new();

        let response = plugin
            .invoke_service_with_events("events", "run", Vec::new(), |event| events.push(event))
            .expect("service should invoke");

        assert_eq!(response.payload, b"ok");
        assert_eq!(events, vec![b"event".to_vec(), b"thread-event".to_vec()]);
    }

    #[test]
    fn concurrent_streaming_service_sends_response_and_events() {
        let mut manifest = test_manifest("events");
        manifest.concurrency = PluginConcurrencyConfig::Limited { max: 1 };
        let runtime = PluginRuntimeHost::from(PluginHost {
            configs: BTreeMap::new(),
            command_registry: bcode_command::CommandRegistry::new(),
            loaded: vec![LoadedPlugin {
                config: ResolvedPluginConfig::default(),
                manifest,
                backend: LoadedPluginBackend::Static {
                    vtable: test_streaming_vtable(),
                },
            }],
        });
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        tokio.block_on(async {
            let StreamingServiceInvocation {
                response,
                mut events,
                cancel: _,
                resource_permit,
            } = runtime
                .invoke_service_with_events("events", "events", "run", Vec::new())
                .await
                .expect("service should start");
            let event = events.recv().await.expect("event should emit");
            let response = response
                .await
                .expect("response sender should stay alive")
                .expect("service should invoke");

            assert_eq!(event, b"event".to_vec());
            let thread_event = events.recv().await.expect("thread event should emit");
            assert_eq!(thread_event, b"thread-event".to_vec());
            drop(resource_permit);
            assert_eq!(response.payload, b"ok");
        });
    }

    #[test]
    fn chunked_service_response_reassembles_without_retry() {
        LARGE_CHUNKING_CALLS.store(0, Ordering::SeqCst);
        let plugin = LoadedPlugin {
            config: ResolvedPluginConfig::default(),
            manifest: test_manifest("large"),
            backend: LoadedPluginBackend::Static {
                vtable: test_large_chunking_vtable(),
            },
        };

        let response = plugin
            .invoke_service("large", "run", Vec::new())
            .expect("chunked response should invoke");

        assert_eq!(LARGE_CHUNKING_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(
            response.payload_text().expect("response should be text"),
            "x".repeat(1024 * 1024 + 1)
        );
    }

    #[test]
    fn oversized_service_response_is_not_retried() {
        LARGE_CALLS.store(0, Ordering::SeqCst);
        let plugin = LoadedPlugin {
            config: ResolvedPluginConfig::default(),
            manifest: test_manifest("large"),
            backend: LoadedPluginBackend::Static {
                vtable: test_large_vtable(),
            },
        };

        let error = plugin
            .invoke_service("large", "run", Vec::new())
            .expect_err("oversized response should fail without retry");

        assert_eq!(LARGE_CALLS.load(Ordering::SeqCst), 1);
        assert!(matches!(
            error,
            PluginLoadError::ServiceResponseTooLarge { .. }
        ));
    }

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

    #[test]
    fn omitted_manifest_concurrency_defaults_to_concurrent() {
        let manifest: PluginManifest = toml::from_str(&format!(
            r#"
id = "example.plugin"
name = "Example Plugin"
version = "0.1.0"

[runtime]
type = "native"
abi_version = {CURRENT_PLUGIN_ABI_VERSION}
library = "libexample_plugin.dylib"
"#,
        ))
        .expect("manifest should parse");

        assert_eq!(manifest.concurrency, PluginConcurrencyConfig::Concurrent);
        assert_eq!(
            PluginConcurrency::from(&manifest.concurrency),
            PluginConcurrency::Concurrent
        );
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn runtime_status_tracks_plugin_local_queueing() {
        use bcode_plugin_sdk::{
            SERVICE_STATUS_BUFFER_TOO_SMALL, SERVICE_STATUS_OK, StaticPluginVtable,
        };
        use std::ffi::c_void;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Mutex as StdMutex, OnceLock};
        use std::time::Duration;

        static SLOW_CALLS: AtomicUsize = AtomicUsize::new(0);
        static FAST_CALLS: AtomicUsize = AtomicUsize::new(0);
        static SLOW_GATE: OnceLock<StdMutex<()>> = OnceLock::new();

        fn activate(_: *const c_void) -> i32 {
            0
        }

        fn deactivate(_: *const c_void) -> i32 {
            0
        }

        fn handle_event(_: *const c_void, _: *const u8, _: usize) -> i32 {
            bcode_plugin_sdk::EVENT_STATUS_OK
        }

        fn write_response(
            response: &ServiceResponse,
            output: *mut u8,
            cap: usize,
            len: *mut usize,
        ) -> i32 {
            let encoded = serde_json::to_vec(response).expect("service response encodes");
            unsafe {
                *len = encoded.len();
            }
            if output.is_null() || cap < encoded.len() {
                return SERVICE_STATUS_BUFFER_TOO_SMALL;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
            }
            SERVICE_STATUS_OK
        }

        fn slow_service(
            _: *const c_void,
            _: *const u8,
            _: usize,
            output: *mut u8,
            cap: usize,
            len: *mut usize,
        ) -> i32 {
            SLOW_CALLS.fetch_add(1, Ordering::SeqCst);
            let _guard = SLOW_GATE
                .get_or_init(|| StdMutex::new(()))
                .lock()
                .expect("gate locks");
            std::thread::sleep(Duration::from_millis(150));
            write_response(&ServiceResponse::text("slow"), output, cap, len)
        }

        fn fast_service(
            _: *const c_void,
            _: *const u8,
            _: usize,
            output: *mut u8,
            cap: usize,
            len: *mut usize,
        ) -> i32 {
            FAST_CALLS.fetch_add(1, Ordering::SeqCst);
            write_response(&ServiceResponse::text("fast"), output, cap, len)
        }

        fn manifest(id: &str) -> PluginManifest {
            PluginManifest {
                config: None,
                id: id.to_string(),
                name: id.to_string(),
                version: Version::new(0, 0, 1),
                services: vec![PluginService {
                    interface_id: id.to_string(),
                    name: None,
                    description: None,
                    concurrency: None,
                    class: None,
                }],
                tui_surfaces: Vec::new(),
                command_contributions: Vec::new(),
                event_subscriptions: Vec::new(),
                concurrency: PluginConcurrencyConfig::Exclusive,
                runtime: PluginRuntime::Native(NativePluginRuntime {
                    abi_version: CURRENT_PLUGIN_ABI_VERSION,
                    library: PathBuf::from("test"),
                    manifest_symbol: DEFAULT_NATIVE_MANIFEST_SYMBOL.to_string(),
                    activate_symbol: DEFAULT_NATIVE_ACTIVATE_SYMBOL.to_string(),
                    deactivate_symbol: DEFAULT_NATIVE_DEACTIVATE_SYMBOL.to_string(),
                    service_symbol: DEFAULT_NATIVE_SERVICE_SYMBOL.to_string(),
                    streaming_service_symbol: DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL.to_string(),
                    event_symbol: DEFAULT_NATIVE_EVENT_SYMBOL.to_string(),
                }),
            }
        }

        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");
        tokio.block_on(async {
            let runtime = PluginRuntimeHost::from(PluginHost {
                configs: BTreeMap::new(),
                command_registry: bcode_command::CommandRegistry::new(),
                loaded: vec![
                    LoadedPlugin {
                        config: ResolvedPluginConfig::default(),
                        manifest: manifest("slow"),
                        backend: LoadedPluginBackend::Static {
                            vtable: StaticPluginVtable {
                                instance: std::ptr::null(),
                                manifest: |_: &'static OnceLock<Option<std::ffi::CString>>| {
                                    std::ptr::null()
                                },
                                activate,
                                register_commands: None,
                                deactivate,
                                invoke_service: slow_service,
                                invoke_service_streaming:
                                    |_,
                                     input_ptr,
                                     input_len,
                                     output_ptr,
                                     output_capacity,
                                     output_len,
                                     _,
                                     _| {
                                        slow_service(
                                            std::ptr::null(),
                                            input_ptr,
                                            input_len,
                                            output_ptr,
                                            output_capacity,
                                            output_len,
                                        )
                                    },
                                handle_event,
                                tui_registry: None,
                            },
                        },
                    },
                    LoadedPlugin {
                        config: ResolvedPluginConfig::default(),
                        manifest: manifest("fast"),
                        backend: LoadedPluginBackend::Static {
                            vtable: StaticPluginVtable {
                                instance: std::ptr::null(),
                                manifest: |_: &'static OnceLock<Option<std::ffi::CString>>| {
                                    std::ptr::null()
                                },
                                activate,
                                register_commands: None,
                                deactivate,
                                invoke_service: fast_service,
                                invoke_service_streaming:
                                    |_,
                                     input_ptr,
                                     input_len,
                                     output_ptr,
                                     output_capacity,
                                     output_len,
                                     _,
                                     _| {
                                        fast_service(
                                            std::ptr::null(),
                                            input_ptr,
                                            input_len,
                                            output_ptr,
                                            output_capacity,
                                            output_len,
                                        )
                                    },
                                handle_event,
                                tui_registry: None,
                            },
                        },
                    },
                ],
            });
            let slow = runtime.clone();
            let slow_task = tokio::spawn(async move {
                slow.invoke_service("slow", "slow", "run", Vec::new()).await
            });
            tokio::time::sleep(Duration::from_millis(25)).await;
            let fast_start = Instant::now();
            let fast = runtime
                .invoke_service("fast", "fast", "run", Vec::new())
                .await
                .expect("fast service returns");
            assert!(fast_start.elapsed() < Duration::from_millis(100));
            assert_eq!(fast.payload, b"fast");
            assert!(
                runtime
                    .executor_statuses()
                    .into_iter()
                    .any(|status| status.plugin_id == "slow" && status.running == 1)
            );
            let slow = slow_task
                .await
                .expect("slow task joins")
                .expect("slow returns");
            assert_eq!(slow.payload, b"slow");
        });
        assert_eq!(SLOW_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(FAST_CALLS.load(Ordering::SeqCst), 1);
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn concurrent_shell_invocations_do_not_block_other_sessions() {
        use bcode_plugin_sdk::StaticPluginVtable;
        use std::ffi::c_void;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        static SLOW_SHELL_CALLS: AtomicUsize = AtomicUsize::new(0);
        static FAST_SHELL_CALLS: AtomicUsize = AtomicUsize::new(0);

        fn service(
            _: *const c_void,
            input_ptr: *const u8,
            input_len: usize,
            output: *mut u8,
            cap: usize,
            len: *mut usize,
        ) -> i32 {
            let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
            let context = serde_json::from_slice::<bcode_plugin_sdk::NativeServiceContext>(input)
                .expect("service context should decode");
            if context.request.operation == "slow_shell" {
                SLOW_SHELL_CALLS.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(150));
                write_test_response(&ServiceResponse::text("slow"), output, cap, len)
            } else {
                FAST_SHELL_CALLS.fetch_add(1, Ordering::SeqCst);
                write_test_response(&ServiceResponse::text("fast"), output, cap, len)
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn service_streaming(
            _: *const c_void,
            input_ptr: *const u8,
            input_len: usize,
            output: *mut u8,
            cap: usize,
            len: *mut usize,
            _: Option<ServiceEventCallback>,
            _: *mut c_void,
        ) -> i32 {
            service(std::ptr::null(), input_ptr, input_len, output, cap, len)
        }

        fn manifest() -> PluginManifest {
            let mut manifest = test_manifest("shell");
            manifest.concurrency = PluginConcurrencyConfig::Concurrent;
            manifest.services = vec![PluginService {
                interface_id: "bcode.tool/v1".to_string(),
                name: Some("shell".to_string()),
                description: None,
                class: Some(PluginInvocationClass::ToolExecution),
                concurrency: None,
            }];
            manifest
        }

        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        tokio.block_on(async {
            let runtime = PluginRuntimeHost::from(PluginHost {
                configs: BTreeMap::new(),
                command_registry: bcode_command::CommandRegistry::new(),
                loaded: vec![LoadedPlugin {
                    config: ResolvedPluginConfig::default(),
                    manifest: manifest(),
                    backend: LoadedPluginBackend::Static {
                        vtable: StaticPluginVtable {
                            instance: std::ptr::null(),
                            manifest: |_: &'static OnceLock<Option<std::ffi::CString>>| {
                                std::ptr::null()
                            },
                            activate: test_activate,
                            register_commands: None,
                            deactivate: test_deactivate,
                            invoke_service: service,
                            invoke_service_streaming: service_streaming,
                            handle_event: test_handle_event,
                            tui_registry: None,
                        },
                    },
                }],
            });

            let slow_runtime = runtime.clone();
            let slow = tokio::spawn(async move {
                slow_runtime
                    .invoke_service_scoped(
                        "shell",
                        "bcode.tool/v1",
                        "slow_shell",
                        Vec::new(),
                        PluginInvocationScope::session("session-a"),
                    )
                    .await
            });
            tokio::time::sleep(Duration::from_millis(25)).await;

            let fast_start = Instant::now();
            let fast = runtime
                .invoke_service_scoped(
                    "shell",
                    "bcode.tool/v1",
                    "fast_shell",
                    Vec::new(),
                    PluginInvocationScope::session("session-b"),
                )
                .await
                .expect("fast shell invocation should complete");
            assert!(fast_start.elapsed() < Duration::from_millis(100));
            assert_eq!(fast.payload, b"fast");

            let slow = slow
                .await
                .expect("slow invocation task should join")
                .expect("slow shell invocation should complete");
            assert_eq!(slow.payload, b"slow");
        });

        assert_eq!(SLOW_SHELL_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(FAST_SHELL_CALLS.load(Ordering::SeqCst), 1);
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn concurrent_model_provider_invocations_do_not_block_other_sessions() {
        use bcode_plugin_sdk::StaticPluginVtable;
        use std::ffi::c_void;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        static SLOW_MODEL_CALLS: AtomicUsize = AtomicUsize::new(0);
        static FAST_MODEL_CALLS: AtomicUsize = AtomicUsize::new(0);

        fn service(
            _: *const c_void,
            input_ptr: *const u8,
            input_len: usize,
            output: *mut u8,
            cap: usize,
            len: *mut usize,
        ) -> i32 {
            let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
            let context = serde_json::from_slice::<bcode_plugin_sdk::NativeServiceContext>(input)
                .expect("service context should decode");
            if context.request.operation == "slow_start_turn" {
                SLOW_MODEL_CALLS.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(150));
                write_test_response(&ServiceResponse::text("slow"), output, cap, len)
            } else {
                FAST_MODEL_CALLS.fetch_add(1, Ordering::SeqCst);
                write_test_response(&ServiceResponse::text("fast"), output, cap, len)
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn service_streaming(
            _: *const c_void,
            input_ptr: *const u8,
            input_len: usize,
            output: *mut u8,
            cap: usize,
            len: *mut usize,
            _: Option<ServiceEventCallback>,
            _: *mut c_void,
        ) -> i32 {
            service(std::ptr::null(), input_ptr, input_len, output, cap, len)
        }

        fn manifest() -> PluginManifest {
            let mut manifest = test_manifest("model");
            manifest.concurrency = PluginConcurrencyConfig::Concurrent;
            manifest.services = vec![PluginService {
                interface_id: "bcode.model-provider/v1".to_string(),
                name: Some("model".to_string()),
                description: None,
                class: Some(PluginInvocationClass::ModelProvider),
                concurrency: None,
            }];
            manifest
        }

        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        tokio.block_on(async {
            let runtime = PluginRuntimeHost::from(PluginHost {
                configs: BTreeMap::new(),
                command_registry: bcode_command::CommandRegistry::new(),
                loaded: vec![LoadedPlugin {
                    config: ResolvedPluginConfig::default(),
                    manifest: manifest(),
                    backend: LoadedPluginBackend::Static {
                        vtable: StaticPluginVtable {
                            instance: std::ptr::null(),
                            manifest: |_: &'static OnceLock<Option<std::ffi::CString>>| {
                                std::ptr::null()
                            },
                            activate: test_activate,
                            register_commands: None,
                            deactivate: test_deactivate,
                            invoke_service: service,
                            invoke_service_streaming: service_streaming,
                            handle_event: test_handle_event,
                            tui_registry: None,
                        },
                    },
                }],
            });

            let slow_runtime = runtime.clone();
            let slow = tokio::spawn(async move {
                slow_runtime
                    .invoke_service_scoped(
                        "model",
                        "bcode.model-provider/v1",
                        "slow_start_turn",
                        Vec::new(),
                        PluginInvocationScope::session("session-a"),
                    )
                    .await
            });
            tokio::time::sleep(Duration::from_millis(25)).await;

            let fast_start = Instant::now();
            let fast = runtime
                .invoke_service_scoped(
                    "model",
                    "bcode.model-provider/v1",
                    "fast_start_turn",
                    Vec::new(),
                    PluginInvocationScope::session("session-b"),
                )
                .await
                .expect("fast model invocation should complete");
            assert!(fast_start.elapsed() < Duration::from_millis(100));
            assert_eq!(fast.payload, b"fast");

            let slow = slow
                .await
                .expect("slow invocation task should join")
                .expect("slow model invocation should complete");
            assert_eq!(slow.payload, b"slow");
        });

        assert_eq!(SLOW_MODEL_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(FAST_MODEL_CALLS.load(Ordering::SeqCst), 1);
    }

    fn test_activate(_: *const std::ffi::c_void) -> i32 {
        0
    }

    fn test_deactivate(_: *const std::ffi::c_void) -> i32 {
        0
    }

    fn test_handle_event(_: *const std::ffi::c_void, _: *const u8, _: usize) -> i32 {
        bcode_plugin_sdk::EVENT_STATUS_OK
    }

    fn test_service(
        _: *const std::ffi::c_void,
        _: *const u8,
        _: usize,
        output: *mut u8,
        cap: usize,
        len: *mut usize,
    ) -> i32 {
        write_test_response(&ServiceResponse::text("ok"), output, cap, len)
    }

    #[allow(clippy::too_many_arguments)]
    fn test_streaming_service(
        instance: *const std::ffi::c_void,
        input_ptr: *const u8,
        input_len: usize,
        output: *mut u8,
        cap: usize,
        len: *mut usize,
        callback: Option<ServiceEventCallback>,
        user_data: *mut std::ffi::c_void,
    ) -> i32 {
        if let Some(callback) = callback {
            callback(b"event".as_ptr(), b"event".len(), user_data);
            let user_data = user_data as usize;
            std::thread::spawn(move || {
                callback(
                    b"thread-event".as_ptr(),
                    b"thread-event".len(),
                    user_data as *mut std::ffi::c_void,
                );
            })
            .join()
            .expect("event thread should join");
        }
        test_service(instance, input_ptr, input_len, output, cap, len)
    }

    static LARGE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LARGE_CHUNKING_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn test_large_service(
        _: *const std::ffi::c_void,
        _: *const u8,
        _: usize,
        output: *mut u8,
        cap: usize,
        len: *mut usize,
    ) -> i32 {
        LARGE_CALLS.fetch_add(1, Ordering::SeqCst);
        let response = ServiceResponse::text("x".repeat(1024 * 1024 + 1));
        write_test_response(&response, output, cap, len)
    }

    #[allow(clippy::too_many_arguments)]
    fn test_large_chunking_service(
        _: *const std::ffi::c_void,
        _: *const u8,
        _: usize,
        output: *mut u8,
        cap: usize,
        len: *mut usize,
        callback: Option<ServiceEventCallback>,
        user_data: *mut std::ffi::c_void,
    ) -> i32 {
        LARGE_CHUNKING_CALLS.fetch_add(1, Ordering::SeqCst);
        let response = ServiceResponse::text("x".repeat(1024 * 1024 + 1));
        let encoded = serde_json::to_vec(&response).expect("service response encodes");
        unsafe {
            *len = encoded.len();
        }
        if output.is_null() || cap < encoded.len() {
            if let Some(callback) = callback {
                for chunk in encoded.chunks(256 * 1024) {
                    let mut payload =
                        Vec::with_capacity(SERVICE_RESPONSE_CHUNK_PREFIX.len() + chunk.len());
                    payload.extend_from_slice(SERVICE_RESPONSE_CHUNK_PREFIX);
                    payload.extend_from_slice(chunk);
                    callback(payload.as_ptr(), payload.len(), user_data);
                }
                unsafe {
                    *len = 0;
                }
                return SERVICE_STATUS_OK;
            }
            return SERVICE_STATUS_BUFFER_TOO_SMALL;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
        }
        SERVICE_STATUS_OK
    }

    fn test_large_chunking_vtable() -> StaticPluginVtable {
        StaticPluginVtable {
            instance: std::ptr::null(),
            manifest: |_: &'static std::sync::OnceLock<Option<std::ffi::CString>>| std::ptr::null(),
            activate: test_activate,
            register_commands: None,
            deactivate: test_deactivate,
            invoke_service: test_large_service,
            invoke_service_streaming: test_large_chunking_service,
            tui_registry: None,
            handle_event: test_handle_event,
        }
    }

    fn test_large_vtable() -> StaticPluginVtable {
        StaticPluginVtable {
            instance: std::ptr::null(),
            manifest: |_: &'static std::sync::OnceLock<Option<std::ffi::CString>>| std::ptr::null(),
            activate: test_activate,
            register_commands: None,
            deactivate: test_deactivate,
            invoke_service: test_large_service,
            invoke_service_streaming: |instance, input_ptr, input_len, output, cap, len, _, _| {
                test_large_service(instance, input_ptr, input_len, output, cap, len)
            },
            tui_registry: None,
            handle_event: test_handle_event,
        }
    }

    fn test_streaming_vtable() -> StaticPluginVtable {
        StaticPluginVtable {
            instance: std::ptr::null(),
            manifest: |_: &'static std::sync::OnceLock<Option<std::ffi::CString>>| std::ptr::null(),
            activate: test_activate,
            register_commands: None,
            deactivate: test_deactivate,
            invoke_service: test_service,
            invoke_service_streaming: test_streaming_service,
            tui_registry: None,
            handle_event: test_handle_event,
        }
    }

    fn test_manifest(id: &str) -> PluginManifest {
        PluginManifest {
            config: None,
            id: id.to_string(),
            name: id.to_string(),
            version: Version::new(0, 0, 1),
            services: vec![PluginService {
                interface_id: id.to_string(),
                name: None,
                description: None,
                concurrency: None,
                class: None,
            }],
            tui_surfaces: Vec::new(),
            command_contributions: Vec::new(),
            event_subscriptions: Vec::new(),
            concurrency: PluginConcurrencyConfig::Exclusive,
            runtime: PluginRuntime::Native(NativePluginRuntime {
                abi_version: CURRENT_PLUGIN_ABI_VERSION,
                library: PathBuf::from("test"),
                manifest_symbol: DEFAULT_NATIVE_MANIFEST_SYMBOL.to_string(),
                activate_symbol: DEFAULT_NATIVE_ACTIVATE_SYMBOL.to_string(),
                deactivate_symbol: DEFAULT_NATIVE_DEACTIVATE_SYMBOL.to_string(),
                service_symbol: DEFAULT_NATIVE_SERVICE_SYMBOL.to_string(),
                streaming_service_symbol: DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL.to_string(),
                event_symbol: DEFAULT_NATIVE_EVENT_SYMBOL.to_string(),
            }),
        }
    }

    fn write_test_response(
        response: &ServiceResponse,
        output: *mut u8,
        cap: usize,
        len: *mut usize,
    ) -> i32 {
        let encoded = serde_json::to_vec(response).expect("service response encodes");
        unsafe {
            *len = encoded.len();
        }
        if output.is_null() || cap < encoded.len() {
            return SERVICE_STATUS_BUFFER_TOO_SMALL;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
        }
        SERVICE_STATUS_OK
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("bcode-plugin-test-{nanos}"))
    }
}
