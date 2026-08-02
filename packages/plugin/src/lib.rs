#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod bmux_host_adapter;

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_plugin_sdk::{
    AuthRegistrationCallback, CURRENT_PLUGIN_ABI_VERSION, CommandRegistrationCallback,
    DEFAULT_NATIVE_ACTIVATE_SYMBOL, DEFAULT_NATIVE_DEACTIVATE_SYMBOL, DEFAULT_NATIVE_EVENT_SYMBOL,
    DEFAULT_NATIVE_MANIFEST_SYMBOL, DEFAULT_NATIVE_REGISTER_AUTH_PROVIDERS_SYMBOL,
    DEFAULT_NATIVE_REGISTER_COMMANDS_SYMBOL, DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL,
    EVENT_STATUS_OK, NativeEventContext, NativeServiceContext, PluginConfigContext, PluginEvent,
    SERVICE_BRIDGE_MAX_REQUEST_BYTES, SERVICE_BRIDGE_MAX_RESPONSE_BYTES,
    SERVICE_BRIDGE_STATUS_CANCELLED, SERVICE_BRIDGE_STATUS_FAILED,
    SERVICE_BRIDGE_STATUS_INVALID_ARGUMENT, SERVICE_BRIDGE_STATUS_OK,
    SERVICE_BRIDGE_STATUS_RESPONSE_TOO_LARGE, SERVICE_RESPONSE_CHUNK_PREFIX, SERVICE_STATUS_OK,
    ServiceBridgeCallback, ServiceBridgeRequest, ServiceBridgeResponse,
    ServiceCancellationWaitCallback, ServiceEventCallback, ServiceRequest, StaticPluginVtable,
};
pub use bcode_plugin_sdk::{ServiceError, ServiceResponse};
pub use bcode_provider_auth_models::{AuthContractError, AuthProviderContribution};

/// Authentication provider contribution with canonical host-attached plugin ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredAuthProvider {
    pub plugin_id: String,
    pub contribution: AuthProviderContribution,
}

/// Deterministic host registry of authentication providers from enabled plugins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthProviderRegistry {
    providers: BTreeMap<String, RegisteredAuthProvider>,
}

impl AuthProviderRegistry {
    /// Create an empty authentication provider registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            providers: BTreeMap::new(),
        }
    }

    /// Register a provider with host-attached plugin ownership.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed plugin IDs or contributions and duplicate provider IDs.
    pub fn register(
        &mut self,
        plugin_id: &str,
        contribution: AuthProviderContribution,
    ) -> Result<(), AuthProviderRegistryError> {
        validate_auth_plugin_id(plugin_id)?;
        contribution
            .validate()
            .map_err(AuthProviderRegistryError::InvalidContribution)?;
        let provider_id = contribution.provider_id.clone();
        if let Some(existing) = self.providers.get(&provider_id) {
            return Err(AuthProviderRegistryError::DuplicateProvider {
                provider_id,
                first_plugin_id: existing.plugin_id.clone(),
                second_plugin_id: plugin_id.to_owned(),
            });
        }
        self.providers.insert(
            provider_id,
            RegisteredAuthProvider {
                plugin_id: plugin_id.to_owned(),
                contribution,
            },
        );
        Ok(())
    }

    /// Return one provider by exact ID.
    #[must_use]
    pub fn get(&self, provider_id: &str) -> Option<&RegisteredAuthProvider> {
        self.providers.get(provider_id)
    }

    /// Return providers in stable provider-ID order.
    #[must_use]
    pub fn providers(&self) -> Vec<&RegisteredAuthProvider> {
        self.providers.values().collect()
    }

    /// Return the number of registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Return whether no providers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// Authentication provider registration failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthProviderRegistryError {
    #[error("invalid registering plugin ID")]
    InvalidPluginId,
    #[error("invalid authentication provider contribution: {0}")]
    InvalidContribution(AuthContractError),
    #[error(
        "authentication provider '{provider_id}' is contributed by both '{first_plugin_id}' and '{second_plugin_id}'"
    )]
    DuplicateProvider {
        provider_id: String,
        first_plugin_id: String,
        second_plugin_id: String,
    },
}

fn validate_auth_plugin_id(plugin_id: &str) -> Result<(), AuthProviderRegistryError> {
    if plugin_id.is_empty()
        || plugin_id.len() > bcode_provider_auth_models::MAX_AUTH_LABEL_BYTES
        || !plugin_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        Err(AuthProviderRegistryError::InvalidPluginId)
    } else {
        Ok(())
    }
}
pub use bmux_host_adapter::{
    BcodeHostCapabilityMap, BcodePluginRuntimeMode, BmuxHostPluginAdapter,
};
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
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

/// Default plugin manifest file name.
pub const DEFAULT_PLUGIN_MANIFEST_FILE: &str = "bcode-plugin.toml";

type ManifestFn = unsafe extern "C" fn() -> *const std::ffi::c_char;
type LifecycleFn = unsafe extern "C" fn() -> i32;
type RegisterCommandsFn =
    unsafe extern "C" fn(Option<CommandRegistrationCallback>, *mut std::ffi::c_void) -> i32;
type RegisterAuthProvidersFn =
    unsafe extern "C" fn(Option<AuthRegistrationCallback>, *mut std::ffi::c_void) -> i32;
type StreamingServiceFn = unsafe extern "C" fn(
    *const u8,
    usize,
    *mut u8,
    usize,
    *mut usize,
    Option<ServiceEventCallback>,
    *mut std::ffi::c_void,
    Option<ServiceBridgeCallback>,
    *mut std::ffi::c_void,
    Option<ServiceCancellationWaitCallback>,
    *mut std::ffi::c_void,
) -> i32;
type EventFn = unsafe extern "C" fn(*const u8, usize) -> i32;

struct ServiceCallbackState<'a> {
    on_event: &'a mut dyn FnMut(Vec<u8>),
    on_bridge: &'a mut dyn FnMut(
        ServiceBridgeRequest,
        bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<ServiceBridgeResponse, String>,
    response_chunks: Vec<Vec<u8>>,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
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

extern "C" fn service_bridge_callback(
    request_ptr: *const u8,
    request_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    if request_ptr.is_null()
        || output_len.is_null()
        || user_data.is_null()
        || request_len > SERVICE_BRIDGE_MAX_REQUEST_BYTES
    {
        return SERVICE_BRIDGE_STATUS_INVALID_ARGUMENT;
    }
    let state = unsafe { &mut *user_data.cast::<ServiceCallbackState<'_>>() };
    if state.cancellation.is_cancelled() {
        return SERVICE_BRIDGE_STATUS_CANCELLED;
    }
    let request = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
    let Ok(request) = serde_json::from_slice::<ServiceBridgeRequest>(request) else {
        return SERVICE_BRIDGE_STATUS_INVALID_ARGUMENT;
    };
    let response = match (state.on_bridge)(request, state.cancellation.clone()) {
        Ok(response) => response,
        Err(_) if state.cancellation.is_cancelled() => return SERVICE_BRIDGE_STATUS_CANCELLED,
        Err(_) => return SERVICE_BRIDGE_STATUS_FAILED,
    };
    if state.cancellation.is_cancelled() {
        return SERVICE_BRIDGE_STATUS_CANCELLED;
    }
    let Ok(encoded) = serde_json::to_vec(&response) else {
        return SERVICE_BRIDGE_STATUS_FAILED;
    };
    unsafe {
        *output_len = encoded.len();
    }
    if encoded.len() > SERVICE_BRIDGE_MAX_RESPONSE_BYTES
        || output_ptr.is_null()
        || output_capacity < encoded.len()
    {
        return SERVICE_BRIDGE_STATUS_RESPONSE_TOO_LARGE;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }
    SERVICE_BRIDGE_STATUS_OK
}

extern "C" fn service_cancellation_wait_callback(
    timeout_ms: u64,
    user_data: *mut std::ffi::c_void,
) -> bool {
    if user_data.is_null() {
        return false;
    }
    let cancellation = unsafe { &*user_data.cast::<bcode_plugin_sdk::ServiceCancellation>() };
    cancellation.wait_cancelled(Duration::from_millis(timeout_ms))
}

/// Stable plugin-owned workflow template contribution version.
pub const WORKFLOW_TEMPLATE_CONTRIBUTION_VERSION: u32 = 1;

/// Maximum serialized bytes accepted for one declarative workflow template definition.
pub const MAX_WORKFLOW_TEMPLATE_DEFINITION_BYTES: usize = 1_048_576;

/// One generic configuration binding applied to an exact template definition before start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTemplateCompilationBinding {
    /// Dotted path below the validated configuration value.
    pub configuration_path: String,
    /// Target agent node whose exact skill selection is replaced.
    pub node_id: String,
    /// Direct fallback edge compiled when the configured skill is absent/null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent_fallback_edge: Option<bcode_workflow::EdgeDefinition>,
    /// Activation mode assigned when the optional configured skill is present.
    pub skill_mode: bcode_workflow::AgentSkillActivationMode,
}

impl WorkflowTemplateCompilationBinding {
    fn validate(&self) -> Result<(), String> {
        if self.configuration_path.trim().is_empty()
            || self.configuration_path.len() > 512
            || self.node_id.trim().is_empty()
            || self.node_id.len() > 256
            || self.configuration_path.split('.').any(|part| {
                part.is_empty()
                    || part.len() > 128
                    || !part
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
            })
        {
            return Err("template compilation binding identity or path is invalid".to_string());
        }
        Ok(())
    }
}

/// One stable plugin-owned workflow template contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTemplateContribution {
    /// Contribution contract version.
    pub contribution_version: u32,
    /// Stable owner-local template identity.
    pub template_id: String,
    /// Owner-controlled template version.
    pub template_version: u32,
    /// User-facing title.
    pub title: String,
    /// User-facing summary.
    pub description: String,
    /// Typed configuration schema validated before definition compilation.
    pub configuration_schema: bcode_workflow::ValueSchema,
    /// Generic bounded bindings applied from validated configuration before exact identity is derived.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compilation_bindings: Vec<WorkflowTemplateCompilationBinding>,
    /// Exact declarative compiled definition for this template version.
    pub definition: bcode_workflow::WorkflowDefinition,
    /// Plugin IDs required by the compiled definition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_plugins: Vec<String>,
    /// Skill IDs required by the compiled definition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_skills: Vec<String>,
    /// Capability labels required from the production host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    /// Renderer-neutral bounded presentation metadata.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub presentation: BTreeMap<String, String>,
}

fn validate_template_compilation_bindings(
    template: &WorkflowTemplateContribution,
) -> Result<(), String> {
    if template.compilation_bindings.len() > 32 {
        return Err("template compilation binding count exceeds 32".to_string());
    }
    let mut binding_targets = BTreeSet::new();
    let mut fallback_edges = BTreeSet::new();
    for binding in &template.compilation_bindings {
        binding.validate()?;
        if !binding_targets.insert((
            binding.configuration_path.as_str(),
            binding.node_id.as_str(),
        )) {
            return Err("template compilation bindings must be unique".to_string());
        }
        let Some(node) = template.definition.nodes.get(&binding.node_id) else {
            return Err(format!(
                "template compilation binding references missing node '{}'",
                binding.node_id
            ));
        };
        if node.kind != bcode_workflow::NodeKind::Agent {
            return Err(format!(
                "template compilation binding target '{}' is not an agent node",
                binding.node_id
            ));
        }
        let Some(edge) = &binding.absent_fallback_edge else {
            continue;
        };
        if !fallback_edges.insert((edge.from.as_str(), edge.to.as_str())) {
            return Err("template compilation fallback edges must be unique".to_string());
        }
        if edge.from == binding.node_id
            || edge.to == binding.node_id
            || !template.definition.nodes.contains_key(&edge.from)
            || !template.definition.nodes.contains_key(&edge.to)
            || edge.kind != bcode_workflow::EdgeKind::Direct
        {
            return Err(format!(
                "template compilation fallback for '{}' must be a direct bypass edge between existing nodes",
                binding.node_id
            ));
        }
        if let Some(transform) = &edge.transform {
            transform.validate().map_err(|error| error.to_string())?;
            if transform.output != template.definition.nodes[&edge.to].input {
                return Err(format!(
                    "template compilation fallback output does not match target '{}' input",
                    edge.to
                ));
            }
        }
    }
    Ok(())
}

impl WorkflowTemplateContribution {
    /// Validate this contribution without starting or persisting a workflow run.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed or oversized metadata, duplicate
    /// requirements, invalid configuration schemas, or unsupported compiled definitions.
    pub fn validate(&self) -> Result<(), bcode_workflow::WorkflowError> {
        let invalid = |message: String| bcode_workflow::WorkflowError::Build {
            path: self.template_id.clone(),
            message,
        };
        if self.contribution_version != WORKFLOW_TEMPLATE_CONTRIBUTION_VERSION {
            return Err(invalid(format!(
                "unsupported workflow template contribution version {}",
                self.contribution_version
            )));
        }
        if self.template_id.trim().is_empty() || self.template_id.len() > 256 {
            return Err(invalid(
                "template_id must contain 1..=256 bytes".to_string(),
            ));
        }
        if self.template_version == 0 {
            return Err(invalid(
                "template_version must be greater than zero".to_string(),
            ));
        }
        if self.title.trim().is_empty() || self.title.len() > 256 || self.description.len() > 4096 {
            return Err(invalid(
                "template display metadata is empty or oversized".to_string(),
            ));
        }
        validate_template_compilation_bindings(self).map_err(&invalid)?;
        if self.configuration_schema.type_name.trim().is_empty()
            || self.configuration_schema.type_name.len() > 256
            || !self.configuration_schema.schema.is_object()
        {
            return Err(invalid(
                "configuration schema must have a bounded type name and object schema".to_string(),
            ));
        }
        validate_template_values("required_plugins", &self.required_plugins)?;
        validate_template_values("required_skills", &self.required_skills)?;
        validate_template_values("required_capabilities", &self.required_capabilities)?;
        if self.presentation.len() > 64
            || self
                .presentation
                .iter()
                .any(|(key, value)| key.trim().is_empty() || key.len() > 128 || value.len() > 4096)
        {
            return Err(invalid(
                "template presentation metadata is invalid or oversized".to_string(),
            ));
        }
        self.definition.validate()?;
        let definition_bytes = serde_json::to_vec(&self.definition).map_err(|error| {
            invalid(format!("template definition cannot be serialized: {error}"))
        })?;
        if definition_bytes.len() > MAX_WORKFLOW_TEMPLATE_DEFINITION_BYTES {
            return Err(invalid(format!(
                "template definition exceeds {MAX_WORKFLOW_TEMPLATE_DEFINITION_BYTES} bytes"
            )));
        }
        let admission = self
            .definition
            .production_admission(&bcode_workflow::WorkflowProductionCapabilities::current())?;
        if !admission.is_supported() {
            return Err(invalid(format!(
                "template definition is not production-supported: {}",
                admission
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(())
    }

    /// Derive the exact topology- and policy-sensitive compiled definition identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the contribution or definition is invalid.
    pub fn definition_identity(
        &self,
        owner_plugin_id: &str,
    ) -> Result<bcode_workflow::WorkflowDefinitionIdentity, bcode_workflow::WorkflowError> {
        self.validate()?;
        bcode_workflow::WorkflowDefinitionIdentity::for_definition(
            format!(
                "{owner_plugin_id}/{}@{}",
                self.template_id, self.template_version
            ),
            &self.definition,
        )
    }
}

fn validate_template_values(
    field: &str,
    values: &[String],
) -> Result<(), bcode_workflow::WorkflowError> {
    let mut seen = BTreeSet::new();
    if values.len() > 256
        || values
            .iter()
            .any(|value| value.trim().is_empty() || value.len() > 256 || !seen.insert(value))
    {
        return Err(bcode_workflow::WorkflowError::Build {
            path: field.to_string(),
            message: "template requirements must be bounded, non-empty, and unique".to_string(),
        });
    }
    Ok(())
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
    pub visual_adapters: Vec<PluginVisualAdapterDeclaration>,
    #[serde(default)]
    pub tool_presentations: Vec<PluginToolPresentationDeclaration>,
    #[serde(default)]
    pub command_contributions: Vec<PluginCommandContribution>,
    #[serde(default)]
    pub workflow_templates: Vec<WorkflowTemplateContribution>,
    #[serde(default)]
    pub event_subscriptions: Vec<PluginEventSubscription>,
    #[serde(default)]
    pub config: Option<PluginManifestConfig>,
    #[serde(default)]
    pub concurrency: PluginConcurrencyConfig,
    pub runtime: PluginRuntime,
}

fn default_tool_service_interface_id() -> String {
    bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_owned()
}

/// How a visual adapter's rows should be composed into host transcript chrome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginVisualAdapterRenderMode {
    #[default]
    Inline,
    TranscriptBlock,
    FullBlock,
}

/// Visual adapter capability declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginVisualAdapterDeclaration {
    pub id: String,
    #[serde(default, alias = "artifact_schema")]
    pub schema: String,
    #[serde(default)]
    pub min_schema_version: Option<u32>,
    #[serde(default)]
    pub max_schema_version: Option<u32>,
    #[serde(default = "default_tool_service_interface_id")]
    pub service_interface_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surfaces: Vec<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub producer_default: bool,
    #[serde(default)]
    pub render_mode: PluginVisualAdapterRenderMode,
}

/// Plugin-owned request-draft presentation routing for one model-callable tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginToolPresentationDeclaration {
    /// Exact model-callable tool name owned by this plugin.
    pub tool_name: String,
    /// Plugin-owned schema used to present a complete assembled request. Empty legacy values use
    /// `request_draft_schema`.
    #[serde(default)]
    pub request_schema: String,
    /// Version of `request_schema`. Zero legacy values use `request_draft_schema_version`.
    #[serde(default)]
    pub request_schema_version: u32,
    /// Plugin-owned schema used to present streamed request arguments.
    pub request_draft_schema: String,
    /// Version of `request_draft_schema`.
    pub request_draft_schema_version: u32,
}

impl PluginToolPresentationDeclaration {
    /// Return the effective complete-request schema with legacy draft-schema fallback.
    #[must_use]
    pub fn effective_request_schema(&self) -> &str {
        if self.request_schema.is_empty() {
            &self.request_draft_schema
        } else {
            &self.request_schema
        }
    }

    /// Return the effective complete-request schema version with legacy draft-version fallback.
    #[must_use]
    pub const fn effective_request_schema_version(&self) -> u32 {
        if self.request_schema_version == 0 {
            self.request_draft_schema_version
        } else {
            self.request_schema_version
        }
    }
}

fn validate_tool_presentation_declarations<'a>(
    manifests: impl IntoIterator<Item = &'a PluginManifest>,
) -> Result<(), PluginLoadError> {
    let mut owners = BTreeMap::<&str, Vec<&str>>::new();
    for manifest in manifests {
        let has_tool_service = manifest
            .services
            .iter()
            .any(|service| service.interface_id == bcode_tool::TOOL_SERVICE_INTERFACE_ID);
        let mut plugin_tools = BTreeSet::new();
        for presentation in &manifest.tool_presentations {
            let tool_name = presentation.tool_name.trim();
            let invalid = |reason: &str| PluginLoadError::InvalidToolPresentation {
                plugin_id: manifest.id.clone(),
                tool_name: presentation.tool_name.clone(),
                reason: reason.to_owned(),
            };
            if !has_tool_service {
                return Err(invalid(
                    "the declaring plugin does not provide bcode.tool/v1",
                ));
            }
            if tool_name.is_empty() {
                return Err(invalid("tool_name must not be empty"));
            }
            if !plugin_tools.insert(tool_name) {
                return Err(invalid(
                    "the tool is declared more than once by this plugin",
                ));
            }
            let request_schema = presentation.effective_request_schema();
            let request_schema_version = presentation.effective_request_schema_version();
            if request_schema.trim().is_empty() {
                return Err(invalid("request_schema must not be empty"));
            }
            if request_schema_version == 0 {
                return Err(invalid("request_schema_version must be greater than zero"));
            }
            if presentation.request_draft_schema.trim().is_empty() {
                return Err(invalid("request_draft_schema must not be empty"));
            }
            if presentation.request_draft_schema_version == 0 {
                return Err(invalid(
                    "request_draft_schema_version must be greater than zero",
                ));
            }
            let request_adapter_matches = manifest.visual_adapters.iter().any(|adapter| {
                adapter.schema == request_schema
                    && adapter
                        .min_schema_version
                        .is_none_or(|minimum| request_schema_version >= minimum)
                    && adapter
                        .max_schema_version
                        .is_none_or(|maximum| request_schema_version <= maximum)
            });
            if !request_adapter_matches {
                return Err(invalid(
                    "no visual adapter supports the declared request schema version",
                ));
            }
            let adapter_matches = manifest.visual_adapters.iter().any(|adapter| {
                adapter.schema == presentation.request_draft_schema
                    && adapter
                        .min_schema_version
                        .is_none_or(|minimum| presentation.request_draft_schema_version >= minimum)
                    && adapter
                        .max_schema_version
                        .is_none_or(|maximum| presentation.request_draft_schema_version <= maximum)
            });
            if !adapter_matches {
                return Err(invalid(
                    "no visual adapter supports the declared draft schema version",
                ));
            }
            owners.entry(tool_name).or_default().push(&manifest.id);
        }
    }
    if let Some((tool_name, plugin_ids)) = owners.into_iter().find(|(_, owners)| owners.len() > 1) {
        return Err(PluginLoadError::AmbiguousToolPresentation {
            tool_name: tool_name.to_owned(),
            plugin_ids: plugin_ids.into_iter().map(str::to_owned).collect(),
        });
    }
    Ok(())
}

impl PluginVisualAdapterDeclaration {
    /// Return whether this adapter can present the artifact schema/version on the requested surface.
    #[must_use]
    pub fn supports(&self, schema: &str, schema_version: u32, surface: &str) -> bool {
        self.schema == schema
            && self
                .min_schema_version
                .is_none_or(|minimum| schema_version >= minimum)
            && self
                .max_schema_version
                .is_none_or(|maximum| schema_version <= maximum)
            && (self.surfaces.is_empty() || self.surfaces.iter().any(|value| value == surface))
    }
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
    /// Workflow blocks exposed through this service.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_blocks: Vec<bcode_workflow::WorkflowBlockDefinition>,
    /// Operations this service explicitly exposes to active tool invocations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invocation_operations: Vec<String>,
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

/// Event delivery mode declared by a plugin manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginEventDelivery {
    /// Queue the event for plugin delivery without blocking the publisher.
    #[default]
    Async,
    /// Block the publisher until the plugin handles the event or times out.
    Barrier,
}

/// Event subscription declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEventSubscription {
    pub topic: String,
    #[serde(default)]
    pub delivery: PluginEventDelivery,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
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
    #[serde(default = "default_streaming_service_symbol")]
    pub streaming_service_symbol: String,
    #[serde(default = "default_register_auth_providers_symbol")]
    pub register_auth_providers_symbol: String,
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

/// Plugin default selection mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PluginSelectionMode {
    /// Enable all candidates unless disabled.
    All,
    /// Enable only explicitly selected plugin IDs.
    #[default]
    Explicit,
}

/// Plugin enable/disable selection policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSelection {
    pub mode: PluginSelectionMode,
    pub enabled: BTreeSet<String>,
    pub disabled: BTreeSet<String>,
}

impl Default for PluginSelection {
    fn default() -> Self {
        Self {
            mode: PluginSelectionMode::Explicit,
            enabled: BTreeSet::new(),
            disabled: BTreeSet::new(),
        }
    }
}

impl PluginSelection {
    /// Return a policy where all discovered plugins are enabled unless disabled.
    #[must_use]
    pub fn all_enabled() -> Self {
        Self {
            mode: PluginSelectionMode::All,
            ..Self::default()
        }
    }

    /// Return true when the plugin ID is enabled by this selection policy.
    #[must_use]
    pub fn is_enabled(&self, plugin_id: &str) -> bool {
        if self.disabled.contains(plugin_id) {
            return false;
        }
        match self.mode {
            PluginSelectionMode::All => true,
            PluginSelectionMode::Explicit => self.enabled.contains(plugin_id),
        }
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

/// Invocation-time secret resolver owned by the host application.
pub type PluginSecretResolver = Arc<
    dyn Fn(&str, &str, &serde_json::Value) -> Result<BTreeMap<String, String>, String>
        + Send
        + Sync,
>;

/// Resolved per-plugin host configuration.
#[derive(Clone)]
pub struct ResolvedPluginConfig {
    pub config: serde_json::Value,
    pub redacted_config: serde_json::Value,
    secret_resolver: Option<PluginSecretResolver>,
}

impl std::fmt::Debug for ResolvedPluginConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedPluginConfig")
            .field("config", &self.config)
            .field("redacted_config", &self.redacted_config)
            .field("has_secret_resolver", &self.secret_resolver.is_some())
            .finish()
    }
}

impl Default for ResolvedPluginConfig {
    fn default() -> Self {
        Self {
            config: serde_json::Value::Null,
            redacted_config: serde_json::Value::Null,
            secret_resolver: None,
        }
    }
}

impl ResolvedPluginConfig {
    /// Create resolved plugin config without an invocation-time secret resolver.
    #[must_use]
    pub const fn new(config: serde_json::Value, redacted_config: serde_json::Value) -> Self {
        Self {
            config,
            redacted_config,
            secret_resolver: None,
        }
    }

    /// Attach a host-owned invocation-time secret resolver.
    #[must_use]
    pub fn with_secret_resolver(mut self, resolver: PluginSecretResolver) -> Self {
        self.secret_resolver = Some(resolver);
        self
    }

    fn resolve_secrets(
        &self,
        interface_id: &str,
        operation: &str,
    ) -> Result<BTreeMap<String, String>, PluginLoadError> {
        self.secret_resolver.as_ref().map_or_else(
            || Ok(BTreeMap::new()),
            |resolver| {
                resolver(interface_id, operation, &self.config).map_err(|message| {
                    PluginLoadError::SecretResolution {
                        plugin_id: String::new(),
                        message,
                    }
                })
            },
        )
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

/// Host-owned policy describing whether an available plugin participates in
/// distribution defaults.
///
/// This policy is registration metadata supplied by a trusted host or
/// distribution. It is intentionally not part of [`PluginManifest`], because a
/// plugin must not be able to authorize its own activation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PluginDefaultActivation {
    /// Include the plugin when distribution defaults are selected.
    #[default]
    Enabled,
    /// Keep the plugin available but require another selection policy to enable it.
    Disabled,
}

/// Statically bundled plugin registration.
#[derive(Debug, Clone, Copy)]
pub struct StaticBundledPlugin {
    pub manifest_toml: &'static str,
    pub vtable: StaticPluginVtable,
    default_activation: PluginDefaultActivation,
}

impl StaticBundledPlugin {
    /// Create a statically bundled plugin registration included in distribution defaults.
    #[must_use]
    pub const fn new(manifest_toml: &'static str, vtable: StaticPluginVtable) -> Self {
        Self {
            manifest_toml,
            vtable,
            default_activation: PluginDefaultActivation::Enabled,
        }
    }

    /// Override whether this available plugin participates in distribution defaults.
    #[must_use]
    pub const fn with_default_activation(
        mut self,
        default_activation: PluginDefaultActivation,
    ) -> Self {
        self.default_activation = default_activation;
        self
    }

    /// Return this registration's host-owned distribution-default policy.
    #[must_use]
    pub const fn default_activation(self) -> PluginDefaultActivation {
        self.default_activation
    }

    /// Return this plugin's Rust-native CLI contribution, when registered.
    #[must_use]
    pub fn cli_registration(self) -> Option<bcode_plugin_sdk::StaticCliRegistration> {
        self.vtable
            .cli_registration
            .map(|registration| registration())
    }
}

#[derive(Debug)]
enum LoadedPluginBackend {
    Dynamic {
        _library: ManuallyDrop<Library>,
        activate: LifecycleFn,
        register_commands: Option<RegisterCommandsFn>,
        register_auth_providers: RegisterAuthProvidersFn,
        deactivate: LifecycleFn,
        invoke_service_streaming: StreamingServiceFn,
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

    /// Register plugin-owned authentication providers through the activation registration hook.
    ///
    /// # Errors
    ///
    /// Returns an error when the hook fails, emits malformed data, or contributes an invalid or
    /// duplicate provider.
    pub fn register_auth_providers(
        &self,
        registry: &mut AuthProviderRegistry,
    ) -> Result<(), PluginLoadError> {
        struct RegistrationContext<'a> {
            plugin_id: &'a str,
            registry: &'a mut AuthProviderRegistry,
            error: Option<AuthProviderRegistryError>,
        }

        extern "C" fn register_auth_provider_callback(
            payload: *const u8,
            payload_len: usize,
            user_data: *mut std::ffi::c_void,
        ) {
            if payload.is_null() || user_data.is_null() {
                return;
            }
            let context = unsafe { &mut *user_data.cast::<RegistrationContext<'_>>() };
            if context.error.is_some() {
                return;
            }
            let bytes = unsafe { std::slice::from_raw_parts(payload, payload_len) };
            let Ok(contribution) = serde_json::from_slice::<AuthProviderContribution>(bytes) else {
                context.error = Some(AuthProviderRegistryError::InvalidContribution(
                    AuthContractError::InvalidFlowShape(
                        "authentication registration payload did not decode",
                    ),
                ));
                return;
            };
            if let Err(error) = context.registry.register(context.plugin_id, contribution) {
                context.error = Some(error);
            }
        }

        let mut context = RegistrationContext {
            plugin_id: &self.manifest.id,
            registry,
            error: None,
        };
        let user_data = std::ptr::from_mut(&mut context).cast::<std::ffi::c_void>();
        let code = match &self.backend {
            LoadedPluginBackend::Dynamic {
                register_auth_providers,
                ..
            } => unsafe {
                register_auth_providers(Some(register_auth_provider_callback), user_data)
            },
            LoadedPluginBackend::Static { vtable } => {
                vtable
                    .register_auth_providers
                    .map_or(0, |register_auth_providers| {
                        register_auth_providers(
                            vtable.instance,
                            Some(register_auth_provider_callback),
                            user_data,
                        )
                    })
            }
        };
        if code != 0 {
            return Err(PluginLoadError::LifecycleFailed {
                plugin_id: self.manifest.id.clone(),
                hook: "register_auth_providers",
                code,
            });
        }
        if let Some(source) = context.error {
            return Err(PluginLoadError::AuthRegistration {
                plugin_id: self.manifest.id.clone(),
                source,
            });
        }
        Ok(())
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
        self.invoke_service_with_bridge(
            interface_id,
            operation,
            payload,
            on_event,
            |_, _| Err("invocation bridge is unavailable".to_string()),
            &bcode_plugin_sdk::ServiceCancellation::default(),
        )
    }

    /// Invoke a service operation with incremental events and a generic bounded bridge.
    ///
    /// # Errors
    ///
    /// Returns an error when request encoding, FFI invocation, bridge handling, or response
    /// decoding fails.
    pub fn invoke_service_with_bridge(
        &self,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        mut on_event: impl FnMut(Vec<u8>),
        mut on_bridge: impl FnMut(
            ServiceBridgeRequest,
            bcode_plugin_sdk::ServiceCancellation,
        ) -> Result<ServiceBridgeResponse, String>,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let interface_id = interface_id.into();
        let operation = operation.into();
        let secrets = self
            .config
            .resolve_secrets(&interface_id, &operation)
            .map_err(|error| match error {
                PluginLoadError::SecretResolution { message, .. } => {
                    PluginLoadError::SecretResolution {
                        plugin_id: self.manifest.id.clone(),
                        message,
                    }
                }
                other => other,
            })?;
        let context = NativeServiceContext {
            plugin_id: self.manifest.id.clone(),
            request: ServiceRequest {
                interface_id,
                operation,
                payload,
            },
            config: PluginConfigContext {
                config: self.config.config.clone(),
                redacted_config: self.config.redacted_config.clone(),
                secrets,
            },
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: bcode_plugin_sdk::ServiceBridge::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        };
        let input = serde_json::to_vec(&context).map_err(PluginLoadError::ServiceEncode)?;
        let output_capacity = 1024 * 1024;
        let mut output_len = 0_usize;
        let mut output = vec![0_u8; output_capacity];
        let mut callback_state = ServiceCallbackState {
            on_event: &mut on_event,
            on_bridge: &mut on_bridge,
            response_chunks: Vec::new(),
            cancellation: cancellation.clone(),
        };
        let event_user_data = (&raw mut callback_state).cast::<std::ffi::c_void>();
        let cancellation_user_data = std::ptr::from_ref(cancellation).cast_mut().cast();
        let status = self.invoke_service_raw(
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            output.len(),
            &raw mut output_len,
            Some(service_event_callback),
            event_user_data,
            Some(service_bridge_callback),
            event_user_data,
            Some(service_cancellation_wait_callback),
            cancellation_user_data,
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
        bridge_callback: Option<ServiceBridgeCallback>,
        bridge_user_data: *mut std::ffi::c_void,
        cancellation_callback: Option<ServiceCancellationWaitCallback>,
        cancellation_user_data: *mut std::ffi::c_void,
    ) -> i32 {
        match &self.backend {
            LoadedPluginBackend::Dynamic {
                invoke_service_streaming,
                ..
            } => unsafe {
                invoke_service_streaming(
                    input_ptr,
                    input_len,
                    output_ptr,
                    output_capacity,
                    output_len,
                    event_callback,
                    event_user_data,
                    bridge_callback,
                    bridge_user_data,
                    cancellation_callback,
                    cancellation_user_data,
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
                bridge_callback,
                bridge_user_data,
                cancellation_callback,
                cancellation_user_data,
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
    #[error("plugin '{plugin_id}' authentication registration failed: {source}")]
    AuthRegistration {
        plugin_id: String,
        source: AuthProviderRegistryError,
    },
    #[error("plugin '{plugin_id}' secret resolution failed: {message}")]
    SecretResolution { plugin_id: String, message: String },
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
    #[error("plugin '{plugin_id}' has invalid tool presentation for '{tool_name}': {reason}")]
    InvalidToolPresentation {
        plugin_id: String,
        tool_name: String,
        reason: String,
    },
    #[error("tool presentation for '{tool_name}' is declared by multiple plugins: {plugin_ids:?}")]
    AmbiguousToolPresentation {
        tool_name: String,
        plugin_ids: Vec<String>,
    },
    #[error("plugin '{plugin_id}' tool catalog validation failed: {message}")]
    ToolCatalogValidation { plugin_id: String, message: String },
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
    #[error("plugin '{plugin_id}' service invocation timed out after {timeout_ms} ms")]
    ServiceInvocationTimeout { plugin_id: String, timeout_ms: u64 },
    #[error("failed to encode plugin event: {0}")]
    EventEncode(#[source] serde_json::Error),
    #[error("plugin '{plugin_id}' event handler failed with code {code}")]
    EventHandlerFailed { plugin_id: String, code: i32 },
    #[error("plugin '{plugin_id}' event handler timed out after {timeout_ms} ms")]
    EventDeliveryTimeout { plugin_id: String, timeout_ms: u64 },
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
            let manifest = parse_static_bundled_manifest(plugin)?;
            Ok(manifest.id)
        })
        .collect()
}

/// Return manifest IDs for statically bundled registrations included in
/// distribution defaults.
///
/// The complete registration inventory remains available independently. This
/// projection represents trusted host policy and does not read activation policy
/// from plugin manifests.
///
/// # Errors
///
/// Returns an error when any static plugin manifest cannot be parsed, including
/// a registration that is not enabled by default.
pub fn static_bundled_default_plugin_ids(
    plugins: &[StaticBundledPlugin],
) -> Result<Vec<String>, PluginLoadError> {
    let mut plugin_ids = Vec::new();
    for plugin in plugins {
        let manifest = parse_static_bundled_manifest(plugin)?;
        if plugin.default_activation() == PluginDefaultActivation::Enabled {
            plugin_ids.push(manifest.id);
        }
    }
    Ok(plugin_ids)
}

fn parse_static_bundled_manifest(
    plugin: &StaticBundledPlugin,
) -> Result<PluginManifest, PluginLoadError> {
    toml::from_str(plugin.manifest_toml).map_err(|source| PluginLoadError::ExportedManifestParse {
        library: PathBuf::from("<static>"),
        source,
    })
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

const fn plugin_serialization_reason(concurrency: PluginConcurrency) -> Option<&'static str> {
    match concurrency {
        PluginConcurrency::Exclusive => Some("plugin_host_reentrancy"),
        PluginConcurrency::Concurrent | PluginConcurrency::Limited(_) => None,
    }
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
    cancellation: bcode_plugin_sdk::ServiceCancellation,
}

impl PluginInvocationCancelHandle {
    /// Return the plugin invocation identifier.
    #[must_use]
    pub const fn id(&self) -> PluginInvocationId {
        self.id
    }

    /// Request cancellation for this invocation.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Return whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
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
    registry: Mutex<bcode_metrics::MetricsRegistry>,
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
    fn set_registry(&self, registry: bcode_metrics::MetricsRegistry) {
        let Ok(mut metrics) = self.registry.lock() else {
            return;
        };
        *metrics = registry;
    }

    fn registry(&self) -> bcode_metrics::MetricsRegistry {
        self.registry.lock().map_or_else(
            |_| bcode_metrics::MetricsRegistry::disabled(),
            |metrics| metrics.clone(),
        )
    }

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

/// Thread-safe request/reply handler attached to one plugin invocation.
#[derive(Clone)]
pub struct PluginInvocationBridge {
    handler: Arc<
        dyn Fn(
                ServiceBridgeRequest,
                bcode_plugin_sdk::ServiceCancellation,
            ) -> Result<ServiceBridgeResponse, String>
            + Send
            + Sync,
    >,
}

impl PluginInvocationBridge {
    /// Create a bridge from a thread-safe request handler.
    #[must_use]
    pub fn new(
        handler: impl Fn(
            ServiceBridgeRequest,
            bcode_plugin_sdk::ServiceCancellation,
        ) -> Result<ServiceBridgeResponse, String>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }

    /// Execute one host bridge request.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured host handler rejects the request.
    pub fn request(
        &self,
        request: ServiceBridgeRequest,
        cancellation: bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<ServiceBridgeResponse, String> {
        (self.handler)(request, cancellation)
    }
}

impl std::fmt::Debug for PluginInvocationBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginInvocationBridge")
            .finish_non_exhaustive()
    }
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
    bridge: Option<PluginInvocationBridge>,
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
            biased;
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
        bridge: Option<PluginInvocationBridge>,
        response: oneshot::Sender<Result<ServiceResponse, PluginLoadError>>,
        event_sender: mpsc::UnboundedSender<Vec<u8>>,
        response_receiver: oneshot::Receiver<Result<ServiceResponse, PluginLoadError>>,
        event_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Result<StreamingServiceInvocation, PluginLoadError> {
        match &self.executor {
            PluginExecutorKind::Exclusive(sender) => {
                tracing::debug!(
                    target: "bcode_plugin::runtime",
                    plugin_id = %self.manifest.id,
                    class = ?class,
                    scope = ?scope,
                    interface_id = %interface_id,
                    operation = %operation,
                    serialization_reason = plugin_serialization_reason(self.concurrency),
                    "plugin service invocation serialized by host"
                );
                let invocation = PluginInvocation {
                    id: invocation_id,
                    class,
                    enqueued_at: Instant::now(),
                    scope: scope.clone(),
                    interface_id,
                    operation,
                    payload,
                    cancellation: cancel.clone(),
                    bridge: bridge.clone(),
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
                    bridge,
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
    #[allow(clippy::too_many_arguments)]
    async fn invoke_service_scoped(
        &self,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
        class: PluginInvocationClass,
        scope: PluginInvocationScope,
        event_sender: Option<mpsc::UnboundedSender<Vec<u8>>>,
        bridge: Option<PluginInvocationBridge>,
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
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            },
            bridge,
            response: oneshot::channel().0,
            event_sender,
        };
        match &self.executor {
            PluginExecutorKind::Exclusive(sender) => {
                tracing::debug!(
                    target: "bcode_plugin::runtime",
                    plugin_id = %self.manifest.id,
                    class = ?invocation.class,
                    scope = ?invocation.scope,
                    interface_id = %invocation.interface_id,
                    operation = %invocation.operation,
                    serialization_reason = plugin_serialization_reason(self.concurrency),
                    "plugin service invocation serialized by host"
                );
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
        validate_tool_presentation_declarations(manifests.values())
            .expect("loaded plugin tool presentation contracts must be valid");
        let mut template_identities = BTreeSet::new();
        for manifest in manifests.values() {
            for template in &manifest.workflow_templates {
                template
                    .validate()
                    .expect("manifest workflow template contract must be valid");
                assert!(
                    template_identities.insert((
                        manifest.id.clone(),
                        template.template_id.clone(),
                        template.template_version,
                    )),
                    "plugin workflow template identity/version must be unique"
                );
            }
            for service in &manifest.services {
                for block in &service.workflow_blocks {
                    assert_eq!(
                        service.interface_id,
                        bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID,
                        "plugin workflow blocks must use bcode.workflow-block/v1"
                    );
                    assert_eq!(
                        block.plugin_id, manifest.id,
                        "plugin workflow block owner must match manifest"
                    );
                    block
                        .validate()
                        .expect("manifest workflow block contract must be valid");
                }
            }
        }
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

    /// Return request-draft presentation metadata for an exact model-callable tool.
    #[must_use]
    pub fn tool_presentation(
        &self,
        tool_name: &str,
    ) -> Option<(&str, &PluginToolPresentationDeclaration)> {
        self.manifests.iter().find_map(|(plugin_id, manifest)| {
            manifest
                .tool_presentations
                .iter()
                .find(|presentation| presentation.tool_name == tool_name)
                .map(|presentation| (plugin_id.as_str(), presentation))
        })
    }

    /// Return the service interface registry.
    #[must_use]
    pub const fn service_registry(&self) -> &PluginServiceRegistry {
        &self.service_registry
    }

    /// Return all loaded, validated workflow block declarations in deterministic order.
    #[must_use]
    pub fn workflow_blocks(&self) -> Vec<bcode_workflow::WorkflowBlockDefinition> {
        self.manifests
            .values()
            .flat_map(|manifest| &manifest.services)
            .flat_map(|service| &service.workflow_blocks)
            .cloned()
            .collect()
    }

    /// Return all loaded, validated workflow template declarations in deterministic order.
    #[must_use]
    pub fn workflow_templates(&self) -> Vec<(&str, &WorkflowTemplateContribution)> {
        self.manifests
            .iter()
            .flat_map(|(plugin_id, manifest)| {
                manifest
                    .workflow_templates
                    .iter()
                    .map(move |template| (plugin_id.as_str(), template))
            })
            .collect()
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

    /// Return compatible visual adapter routes from highest to lowest default precedence.
    #[must_use]
    pub fn visual_adapters(
        &self,
        schema: &str,
        schema_version: u32,
        surface: &str,
        producer_plugin_id: Option<&str>,
    ) -> Vec<PluginVisualAdapterRoute> {
        select_visual_adapters(
            self.manifests.iter(),
            schema,
            schema_version,
            surface,
            producer_plugin_id,
        )
    }

    /// Return the highest-priority compatible visual adapter route for an artifact.
    #[must_use]
    pub fn visual_adapter(
        &self,
        schema: &str,
        schema_version: u32,
        surface: &str,
        producer_plugin_id: Option<&str>,
    ) -> Option<PluginVisualAdapterRoute> {
        self.visual_adapters(schema, schema_version, surface, producer_plugin_id)
            .into_iter()
            .next()
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

/// Loaded route for a manifest-declared visual adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginVisualAdapterRoute {
    pub plugin_id: String,
    pub adapter_id: String,
    pub schema: String,
    pub service_interface_id: String,
    pub surfaces: Vec<String>,
    pub priority: i32,
    pub producer_default: bool,
    pub render_mode: PluginVisualAdapterRenderMode,
}

impl PluginVisualAdapterRoute {
    /// Return this route's stable user-facing adapter reference.
    #[must_use]
    pub fn adapter_reference(&self) -> String {
        format!("{}/{}", self.plugin_id, self.adapter_id)
    }
}

fn select_visual_adapters<'a, I>(
    manifests: I,
    schema: &str,
    schema_version: u32,
    surface: &str,
    producer_plugin_id: Option<&str>,
) -> Vec<PluginVisualAdapterRoute>
where
    I: Iterator<Item = (&'a String, &'a PluginManifest)>,
{
    let mut routes = manifests
        .flat_map(|(plugin_id, manifest)| {
            manifest
                .visual_adapters
                .iter()
                .filter(move |adapter| adapter.supports(schema, schema_version, surface))
                .map(move |adapter| {
                    let producer_bonus = i32::from(
                        adapter.producer_default
                            && producer_plugin_id.is_some_and(|producer| producer == plugin_id),
                    );
                    (plugin_id, adapter, producer_bonus)
                })
        })
        .collect::<Vec<_>>();
    routes.sort_by(|left, right| {
        let (left_plugin, left_adapter, left_producer_bonus) = left;
        let (right_plugin, right_adapter, right_producer_bonus) = right;
        (
            right_adapter.priority,
            right_producer_bonus,
            right_plugin.as_str(),
            right_adapter.id.as_str(),
        )
            .cmp(&(
                left_adapter.priority,
                left_producer_bonus,
                left_plugin.as_str(),
                left_adapter.id.as_str(),
            ))
    });
    routes
        .into_iter()
        .map(|(plugin_id, adapter, _)| PluginVisualAdapterRoute {
            plugin_id: plugin_id.clone(),
            adapter_id: adapter.id.clone(),
            schema: adapter.schema.clone(),
            service_interface_id: adapter.service_interface_id.clone(),
            surfaces: adapter.surfaces.clone(),
            priority: adapter.priority,
            producer_default: adapter.producer_default,
            render_mode: adapter.render_mode,
        })
        .collect()
}

/// Runtime policy metadata for a declared plugin service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceRuntimePolicy {
    pub concurrency: PluginConcurrency,
    pub class: Option<PluginInvocationClass>,
}

#[derive(Debug)]
struct QueuedPluginEvent {
    topic: String,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct PluginEventDispatcher {
    plugin_id: String,
    executor: Arc<PluginExecutorHandle>,
    sender: mpsc::Sender<QueuedPluginEvent>,
    receiver: Mutex<Option<mpsc::Receiver<QueuedPluginEvent>>>,
    started: AtomicBool,
}

impl PluginEventDispatcher {
    fn new(plugin_id: String, executor: Arc<PluginExecutorHandle>) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        Self {
            plugin_id,
            executor,
            sender,
            receiver: Mutex::new(Some(receiver)),
            started: AtomicBool::new(false),
        }
    }

    fn start(&self) {
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let Some(receiver) = self
            .receiver
            .lock()
            .expect("plugin event dispatcher receiver lock")
            .take()
        else {
            return;
        };
        spawn_plugin_event_dispatcher(self.plugin_id.clone(), Arc::clone(&self.executor), receiver);
    }

    fn try_send(
        &self,
        event: QueuedPluginEvent,
    ) -> Result<(), mpsc::error::TrySendError<QueuedPluginEvent>> {
        self.start();
        self.sender.try_send(event)
    }
}

/// Concurrent plugin runtime with plugin-local execution isolation.
#[derive(Debug, Clone)]
pub struct PluginRuntimeHost {
    registry: Arc<PluginRegistry>,
    executors: Arc<BTreeMap<String, Arc<PluginExecutorHandle>>>,
    event_dispatchers: Arc<BTreeMap<String, Arc<PluginEventDispatcher>>>,
    configs: Arc<BTreeMap<String, ResolvedPluginConfig>>,
    selection: Arc<PluginSelection>,
    command_registry: Arc<bcode_command::CommandRegistry>,
    auth_provider_registry: Arc<AuthProviderRegistry>,
    resources: Arc<PluginResourceLimiter>,
    metrics: bcode_metrics::MetricsRegistry,
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
        PluginHost::load_defaults_with_static_bundled(selection, static_plugins)
            .map(Self::from)
            .map(|mut runtime| {
                runtime.selection = Arc::new(selection.clone());
                runtime
            })
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
            .map(|mut runtime| {
                runtime.selection = Arc::new(selection.clone());
                runtime
            })
    }

    /// Return a clone with the resolved selection inventory attached.
    #[must_use]
    pub fn with_selection(mut self, selection: PluginSelection) -> Self {
        self.selection = Arc::new(selection);
        self
    }

    /// Return the resolved plugin selection, including configured disabled and unavailable IDs.
    #[must_use]
    pub fn selection(&self) -> &PluginSelection {
        &self.selection
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

    /// Return the host-owned authentication provider registry.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn auth_provider_registry(&self) -> &AuthProviderRegistry {
        &self.auth_provider_registry
    }

    /// Return a registered authentication provider by exact ID.
    #[must_use]
    pub fn auth_provider(&self, provider_id: &str) -> Option<&RegisteredAuthProvider> {
        self.auth_provider_registry.get(provider_id)
    }

    /// Return loaded plugin ids.
    #[must_use]
    pub fn plugin_ids(&self) -> Vec<String> {
        self.registry.manifests.keys().cloned().collect()
    }

    /// Return request-draft presentation metadata for an exact model-callable tool.
    #[must_use]
    pub fn tool_presentation(
        &self,
        tool_name: &str,
    ) -> Option<(&str, &PluginToolPresentationDeclaration)> {
        self.registry.tool_presentation(tool_name)
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

    /// Return the highest-priority compatible visual adapter route.
    #[must_use]
    pub fn visual_adapter(
        &self,
        schema: &str,
        schema_version: u32,
        surface: &str,
        producer_plugin_id: Option<&str>,
    ) -> Option<PluginVisualAdapterRoute> {
        select_visual_adapters(
            self.registry.manifests().iter(),
            schema,
            schema_version,
            surface,
            producer_plugin_id,
        )
        .into_iter()
        .next()
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

    /// Return a clone of this runtime host that emits runtime metrics to `metrics`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: bcode_metrics::MetricsRegistry) -> Self {
        for executor in self.executors.values() {
            executor.metrics.set_registry(metrics.clone());
        }
        self.metrics = metrics;
        self
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
        self.invoke_service_with_bridge_scoped(
            plugin_id,
            interface_id,
            operation,
            payload,
            scope,
            None,
        )
        .await
    }

    /// Invoke a service operation with explicit scope and a cancellation-propagating timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is not loaded, invocation fails, or the deadline elapses.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn invoke_service_scoped_with_timeout(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        scope: PluginInvocationScope,
        timeout: std::time::Duration,
    ) -> Result<ServiceResponse, PluginLoadError> {
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        let mut invocation = self
            .invoke_service_with_events_scoped(plugin_id, interface_id, operation, payload, scope)
            .await?;
        let cancellation = invocation.cancel.clone();
        let timed = tokio::time::timeout(timeout, async {
            loop {
                if let StreamingServiceInvocationEvent::Response(response) =
                    invocation.next_event().await?
                {
                    return response;
                }
            }
        })
        .await;
        timed.unwrap_or_else(|_| {
            cancellation.cancel();
            Err(PluginLoadError::ServiceInvocationTimeout {
                plugin_id: plugin_id.to_owned(),
                timeout_ms,
            })
        })
    }

    /// Invoke a service operation with explicit ownership scope and a duplex bridge.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is not loaded or service invocation fails.
    pub async fn invoke_service_with_bridge_scoped(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        scope: PluginInvocationScope,
        bridge: Option<PluginInvocationBridge>,
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
        let metric_labels =
            plugin_runtime_metric_labels(plugin_id, &interface_id, &operation, class, &scope);
        self.metrics.record_histogram_with_labels(
            "plugin.resource_wait.duration_ms",
            u128_to_u64(resource_permit.wait_ms),
            metric_labels.clone(),
        );
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
        let result = executor
            .invoke_service_scoped(interface_id, operation, payload, class, scope, None, bridge)
            .await;
        self.metrics
            .span("plugin.invocation")
            .labels(metric_labels)
            .finish_result(&result);
        result
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
        self.invoke_service_with_events_and_bridge_scoped(
            plugin_id,
            interface_id,
            operation,
            payload,
            scope,
            None,
        )
        .await
    }

    /// Invoke a service with explicit scope, events, and a duplex bridge.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is not loaded or service invocation fails.
    pub async fn invoke_service_with_events_and_bridge_scoped(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        scope: PluginInvocationScope,
        bridge: Option<PluginInvocationBridge>,
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
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        };
        let resource_permit = self.resources.acquire(&scope).await?;
        let metric_labels =
            plugin_runtime_metric_labels(plugin_id, &interface_id, &operation, class, &scope);
        self.metrics.record_histogram_with_labels(
            "plugin.resource_wait.duration_ms",
            u128_to_u64(resource_permit.wait_ms),
            metric_labels.clone(),
        );
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
        let result = executor
            .start_service_with_events_scoped(
                interface_id,
                operation,
                payload,
                class,
                scope,
                invocation_id,
                cancel,
                bridge,
                response,
                event_sender,
                response_receiver,
                event_receiver,
            )
            .await;
        self.metrics
            .span("plugin.invocation.start")
            .labels(metric_labels)
            .finish_result(&result);
        let mut invocation = result?;
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

    /// Invoke a typed service operation with explicit scope and cancellation-propagating timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when encoding, invocation, timeout, service response, or decoding fails.
    pub async fn invoke_service_json_scoped_with_timeout<Q, R>(
        &self,
        plugin_id: &str,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        request: &Q,
        scope: PluginInvocationScope,
        timeout: std::time::Duration,
    ) -> Result<R, PluginServiceCallError>
    where
        Q: Serialize + Sync,
        R: DeserializeOwned,
    {
        let payload = serde_json::to_vec(request).map_err(PluginServiceCallError::RequestEncode)?;
        let response = self
            .invoke_service_scoped_with_timeout(
                plugin_id,
                interface_id,
                operation,
                payload,
                scope,
                timeout,
            )
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
            .filter_map(|manifest| {
                manifest_event_delivery_policy(manifest, &topic)
                    .map(|policy| (manifest.id.clone(), policy))
            })
            .collect::<Vec<_>>();
        let mut barrier_deliveries = Vec::new();
        let mut delivered = 0;
        for (plugin_id, policy) in subscribers {
            let topic = topic.clone();
            let payload = payload.to_vec();
            match policy.delivery {
                PluginEventDelivery::Async => {
                    let event = QueuedPluginEvent { topic, payload };
                    let Some(dispatcher) = self.event_dispatchers.get(&plugin_id) else {
                        return Err(PluginLoadError::PluginNotLoaded(plugin_id));
                    };
                    match dispatcher.try_send(event) {
                        Ok(()) => {
                            delivered += 1;
                        }
                        Err(error) => {
                            tracing::warn!(
                                target: "bcode_plugin::events",
                                plugin_id = %plugin_id,
                                %error,
                                "asynchronous plugin event queue rejected event"
                            );
                        }
                    }
                }
                PluginEventDelivery::Barrier => {
                    let executor = self
                        .executors
                        .get(&plugin_id)
                        .cloned()
                        .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.clone()))?;
                    barrier_deliveries.push(tokio::spawn(async move {
                        tokio::time::timeout(policy.timeout, executor.handle_event(topic, payload))
                            .await
                            .map_err(|_| PluginLoadError::EventDeliveryTimeout {
                                plugin_id: plugin_id.clone(),
                                timeout_ms: u64::try_from(policy.timeout.as_millis())
                                    .unwrap_or(u64::MAX),
                            })?
                    }));
                }
            }
        }
        for delivery in barrier_deliveries {
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
        let command_registry = std::mem::take(&mut host.command_registry);
        let auth_provider_registry = std::mem::take(&mut host.auth_provider_registry);
        let mut manifests = BTreeMap::new();
        let mut executors = BTreeMap::new();
        let mut event_dispatchers = BTreeMap::new();
        for plugin in loaded {
            let manifest = plugin.manifest().clone();
            let plugin_id = manifest.id.clone();
            manifests.insert(plugin_id.clone(), manifest.clone());
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
            let handle = Arc::new(PluginExecutorHandle::new(
                manifest.clone(),
                concurrency,
                executor,
                metrics,
            ));
            let dispatcher = Arc::new(PluginEventDispatcher::new(
                plugin_id.clone(),
                Arc::clone(&handle),
            ));
            event_dispatchers.insert(plugin_id.clone(), dispatcher);
            executors.insert(plugin_id, handle);
        }
        Self {
            registry: Arc::new(PluginRegistry::from_manifests(manifests)),
            executors: Arc::new(executors),
            event_dispatchers: Arc::new(event_dispatchers),
            configs: Arc::new(configs),
            selection: Arc::new(PluginSelection::default()),
            command_registry: Arc::new(command_registry),
            auth_provider_registry: Arc::new(auth_provider_registry),
            resources: Arc::default(),
            metrics: bcode_metrics::MetricsRegistry::disabled(),
        }
    }
}

fn plugin_runtime_metric_labels(
    plugin_id: &str,
    interface_id: &str,
    operation: &str,
    class: PluginInvocationClass,
    scope: &PluginInvocationScope,
) -> bcode_metrics::MetricLabels {
    let mut labels = bcode_metrics::MetricLabels::new();
    labels.insert("plugin_id".to_owned(), plugin_id.to_owned());
    labels.insert("interface_id".to_owned(), interface_id.to_owned());
    labels.insert("operation".to_owned(), operation.to_owned());
    labels.insert(
        "class".to_owned(),
        plugin_invocation_class_label(class).to_owned(),
    );
    labels.insert(
        "scope".to_owned(),
        plugin_invocation_scope_label(scope).to_owned(),
    );
    labels
}

const fn plugin_invocation_class_label(class: PluginInvocationClass) -> &'static str {
    match class {
        PluginInvocationClass::Control => "control",
        PluginInvocationClass::Query => "query",
        PluginInvocationClass::ToolExecution => "tool_execution",
        PluginInvocationClass::ModelProvider => "model_provider",
        PluginInvocationClass::EventDelivery => "event_delivery",
        PluginInvocationClass::Service => "service",
    }
}

const fn plugin_invocation_scope_label(scope: &PluginInvocationScope) -> &'static str {
    match scope {
        PluginInvocationScope::Global => "global",
        PluginInvocationScope::Session { .. } => "session",
    }
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn u128_to_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn spawn_plugin_event_dispatcher(
    plugin_id: String,
    executor: Arc<PluginExecutorHandle>,
    mut receiver: mpsc::Receiver<QueuedPluginEvent>,
) {
    tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            if let Err(error) = executor.handle_event(event.topic, event.payload).await {
                tracing::warn!(
                    target: "bcode_plugin::events",
                    plugin_id = %plugin_id,
                    %error,
                    "asynchronous plugin event delivery failed"
                );
            }
        }
    });
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
    let bridge = invocation.bridge.clone();
    let response = plugin.invoke_service_with_bridge(
        invocation.interface_id,
        invocation.operation,
        invocation.payload,
        |event| {
            if let Some(sender) = &invocation.event_sender {
                let _ = sender.send(event);
            }
        },
        move |request, cancellation| {
            bridge.as_ref().map_or_else(
                || Err("invocation bridge is unavailable".to_string()),
                |bridge| bridge.request(request, cancellation),
            )
        },
        &invocation.cancellation.cancellation,
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

#[allow(clippy::too_many_lines)]
fn spawn_exclusive_plugin_executor(
    plugin: LoadedPlugin,
    mut receiver: mpsc::Receiver<PluginExecutorMessage>,
    metrics: Arc<PluginExecutorMetrics>,
) {
    tokio::task::spawn_blocking(move || {
        let mut active = true;
        while let Some(message) = receiver.blocking_recv() {
            match message {
                PluginExecutorMessage::Service(mut invocation) => {
                    metrics.dequeue(invocation.class);
                    let queue_wait_ms = elapsed_ms(invocation.enqueued_at);
                    metrics.registry().record_histogram_with_labels(
                        "plugin.queue_wait.duration_ms",
                        queue_wait_ms,
                        plugin_runtime_metric_labels(
                            &plugin.manifest.id,
                            &invocation.interface_id,
                            &invocation.operation,
                            invocation.class,
                            &invocation.scope,
                        ),
                    );
                    let (unused_response, _) = oneshot::channel();
                    let response_sender =
                        std::mem::replace(&mut invocation.response, unused_response);
                    let response = if active {
                        execute_plugin_service_invocation(&plugin, invocation, &metrics)
                    } else {
                        metrics.failed.fetch_add(1, Ordering::Relaxed);
                        Err(PluginLoadError::PluginNotLoaded(plugin.manifest.id.clone()))
                    };
                    let _ = response_sender.send(response);
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

#[derive(Debug, Clone, Copy)]
struct EventDeliveryPolicy {
    delivery: PluginEventDelivery,
    timeout: Duration,
}

const DEFAULT_EVENT_BARRIER_TIMEOUT: Duration = Duration::from_secs(5);

fn manifest_event_delivery_policy(
    manifest: &PluginManifest,
    topic: &str,
) -> Option<EventDeliveryPolicy> {
    let mut subscriptions = manifest_event_subscriptions(manifest, topic).peekable();
    subscriptions.peek()?;
    let mut delivery = PluginEventDelivery::Async;
    let mut timeout = DEFAULT_EVENT_BARRIER_TIMEOUT;
    for subscription in subscriptions {
        if subscription.delivery == PluginEventDelivery::Barrier {
            delivery = PluginEventDelivery::Barrier;
        }
        if let Some(timeout_ms) = subscription.timeout_ms {
            timeout = timeout.min(Duration::from_millis(timeout_ms));
        }
    }
    Some(EventDeliveryPolicy { delivery, timeout })
}

fn manifest_event_subscriptions<'a>(
    manifest: &'a PluginManifest,
    topic: &str,
) -> impl Iterator<Item = &'a PluginEventSubscription> {
    manifest
        .event_subscriptions
        .iter()
        .filter(move |subscription| subscription.topic == topic)
}

/// Loaded plugin host retaining activated plugins.
#[derive(Debug)]
pub struct PluginHost {
    loaded: Vec<LoadedPlugin>,
    configs: BTreeMap<String, ResolvedPluginConfig>,
    command_registry: bcode_command::CommandRegistry,
    auth_provider_registry: AuthProviderRegistry,
}

impl Default for PluginHost {
    fn default() -> Self {
        let mut command_registry = bcode_command::CommandRegistry::new();
        command_registry.extend(bcode_command::bundled_host_palette_commands());
        Self {
            loaded: Vec::new(),
            configs: BTreeMap::new(),
            command_registry,
            auth_provider_registry: AuthProviderRegistry::new(),
        }
    }
}

impl PluginHost {
    /// Return whether one loaded plugin declares a service interface.
    #[must_use]
    pub fn has_service(&self, plugin_id: &str, interface_id: &str) -> bool {
        self.loaded.iter().any(|plugin| {
            plugin.manifest.id == plugin_id
                && plugin
                    .manifest
                    .services
                    .iter()
                    .any(|service| service.interface_id == interface_id)
        })
    }

    /// Return presentation metadata for an exact model-callable tool.
    #[must_use]
    pub fn tool_presentation(
        &self,
        tool_name: &str,
    ) -> Option<(&str, &PluginToolPresentationDeclaration)> {
        self.loaded.iter().find_map(|plugin| {
            plugin
                .manifest
                .tool_presentations
                .iter()
                .find(|presentation| presentation.tool_name == tool_name)
                .map(|presentation| (plugin.manifest.id.as_str(), presentation))
        })
    }

    /// Return compatible visual adapter routes for loaded plugins.
    #[must_use]
    pub fn visual_adapters(
        &self,
        schema: &str,
        schema_version: u32,
        surface: &str,
        producer_plugin_id: Option<&str>,
    ) -> Vec<PluginVisualAdapterRoute> {
        select_visual_adapters(
            self.loaded
                .iter()
                .map(|plugin| (&plugin.manifest.id, &plugin.manifest)),
            schema,
            schema_version,
            surface,
            producer_plugin_id,
        )
    }

    /// Return the highest-priority compatible visual adapter route for a loaded plugin.
    #[must_use]
    pub fn visual_adapter(
        &self,
        schema: &str,
        schema_version: u32,
        surface: &str,
        producer_plugin_id: Option<&str>,
    ) -> Option<PluginVisualAdapterRoute> {
        self.visual_adapters(schema, schema_version, surface, producer_plugin_id)
            .into_iter()
            .next()
    }

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
        let discovery_started_at = Instant::now();
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
            elapsed_ms = discovery_started_at.elapsed().as_millis(),
            static_plugin_count = static_plugins.len(),
            plugin_count = plugins.len(),
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
            auth_provider_registry: AuthProviderRegistry::new(),
        };
        host.load_static_plugins_into(&static_plugins)?;
        host.load_registered_plugins_into(&plugins)?;
        validate_tool_presentation_declarations(host.loaded.iter().map(LoadedPlugin::manifest))?;
        host.validate_tool_presentation_ownership()?;
        tracing::debug!(
            target: "bcode_plugin::startup",
            elapsed_ms = discovery_started_at.elapsed().as_millis(),
            loaded_plugin_count = host.loaded.len(),
            "plugin startup complete"
        );
        Ok(host)
    }

    /// Load and activate registered plugins.
    ///
    /// # Errors
    ///
    /// Returns an error when loading or activation fails.
    pub fn load_registered_plugins(plugins: &[RegisteredPlugin]) -> Result<Self, PluginLoadError> {
        validate_tool_presentation_declarations(plugins.iter().map(|plugin| &plugin.manifest))?;
        let mut host = Self::default();
        host.load_registered_plugins_into(plugins)?;
        host.validate_tool_presentation_ownership()?;
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
        validate_tool_presentation_declarations(plugins.iter().map(|(manifest, _)| manifest))?;
        let mut host = Self::default();
        host.load_static_plugins_into(plugins)?;
        host.validate_tool_presentation_ownership()?;
        Ok(host)
    }

    /// Return the number of manifest-declared visual adapters in loaded plugins.
    #[must_use]
    pub fn visual_adapter_count(&self) -> usize {
        self.loaded
            .iter()
            .map(|plugin| plugin.manifest.visual_adapters.len())
            .sum()
    }

    /// Load and activate statically bundled plugins best-effort.
    ///
    /// Plugins that fail to load or activate are skipped so independent client-side capabilities can
    /// remain available.
    #[must_use]
    pub fn load_static_plugins_best_effort(
        plugins: &[(PluginManifest, StaticPluginVtable)],
    ) -> Self {
        let mut host = Self::default();
        for plugin in plugins {
            let single = [plugin.clone()];
            let _ = host.load_static_plugins_into(&single);
        }
        host
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
            loaded.register_auth_providers(&mut self.auth_provider_registry)?;
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
            loaded.register_auth_providers(&mut self.auth_provider_registry)?;
            tracing::debug!(target: "bcode_plugin::startup", plugin_id = %loaded.manifest().id, "plugin activated");
            self.loaded.push(loaded);
        }
        Ok(())
    }

    fn validate_loaded_tool_presentation_ownership(
        &self,
        plugin_id: &str,
        tools: &bcode_tool::ToolList,
    ) -> Result<(), PluginLoadError> {
        let names = tools
            .tools
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<BTreeSet<_>>();
        let manifest = self
            .loaded
            .iter()
            .find(|plugin| plugin.manifest().id == plugin_id)
            .map(LoadedPlugin::manifest)
            .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.to_owned()))?;
        for presentation in &manifest.tool_presentations {
            if !names.contains(presentation.tool_name.as_str()) {
                return Err(PluginLoadError::InvalidToolPresentation {
                    plugin_id: plugin_id.to_owned(),
                    tool_name: presentation.tool_name.clone(),
                    reason: "the plugin does not expose this exact name from list_tools".to_owned(),
                });
            }
        }
        for tool_name in names {
            if let Some(owner) = self.loaded.iter().find(|plugin| {
                plugin.manifest().id != plugin_id
                    && plugin
                        .manifest()
                        .tool_presentations
                        .iter()
                        .any(|presentation| presentation.tool_name == tool_name)
            }) {
                return Err(PluginLoadError::AmbiguousToolPresentation {
                    tool_name: tool_name.to_owned(),
                    plugin_ids: vec![plugin_id.to_owned(), owner.manifest().id.clone()],
                });
            }
        }
        Ok(())
    }

    fn validate_tool_presentation_ownership(&self) -> Result<(), PluginLoadError> {
        for plugin in &self.loaded {
            if plugin.manifest().tool_presentations.is_empty() {
                continue;
            }
            let tools = plugin
                .invoke_service_json::<_, bcode_tool::ToolList>(
                    bcode_tool::TOOL_SERVICE_INTERFACE_ID,
                    bcode_tool::OP_LIST_TOOLS,
                    &bcode_tool::ListToolsRequest::default(),
                )
                .map_err(|error| PluginLoadError::ToolCatalogValidation {
                    plugin_id: plugin.manifest().id.clone(),
                    message: error.to_string(),
                })?;
            self.validate_loaded_tool_presentation_ownership(&plugin.manifest().id, &tools)?;
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

    /// Return the host-owned authentication provider registry.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn auth_provider_registry(&self) -> &AuthProviderRegistry {
        &self.auth_provider_registry
    }

    /// Return a registered authentication provider by exact ID.
    #[must_use]
    pub fn auth_provider(&self, provider_id: &str) -> Option<&RegisteredAuthProvider> {
        self.auth_provider_registry.get(provider_id)
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
    validate_tool_presentation_declarations(std::iter::once(&plugin.manifest))?;
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
        library = %display_from_current_dir(&library_path),
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
    let register_auth_providers = load_register_auth_providers_symbol(
        &library,
        &library_path,
        &runtime.register_auth_providers_symbol,
    )?;
    let deactivate = load_lifecycle_symbol(&library, &library_path, &runtime.deactivate_symbol)?;
    let invoke_service_streaming =
        load_streaming_service_symbol(&library, &library_path, &runtime.streaming_service_symbol)?;
    let handle_event = load_event_symbol(&library, &library_path, &runtime.event_symbol)?;
    tracing::debug!(target: "bcode_plugin::startup", plugin_id = %plugin.manifest.id, "native symbols loaded");

    Ok(LoadedPlugin {
        manifest: plugin.manifest.clone(),
        backend: LoadedPluginBackend::Dynamic {
            _library: ManuallyDrop::new(library),
            activate,
            register_commands,
            register_auth_providers,
            deactivate,
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
    validate_tool_presentation_declarations(std::iter::once(&manifest))?;
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

fn load_register_auth_providers_symbol(
    library: &Library,
    library_path: &Path,
    symbol: &str,
) -> Result<RegisterAuthProvidersFn, PluginLoadError> {
    let loaded =
        unsafe { library.get::<RegisterAuthProvidersFn>(symbol.as_bytes()) }.map_err(|source| {
            PluginLoadError::SymbolLoad {
                library: library_path.to_path_buf(),
                symbol: symbol.to_owned(),
                source,
            }
        })?;
    Ok(*loaded)
}

fn load_streaming_service_symbol(
    library: &Library,
    library_path: &Path,
    symbol: &str,
) -> Result<StreamingServiceFn, PluginLoadError> {
    let loaded =
        unsafe { library.get::<StreamingServiceFn>(symbol.as_bytes()) }.map_err(|source| {
            PluginLoadError::SymbolLoad {
                library: library_path.to_path_buf(),
                symbol: symbol.to_string(),
                source,
            }
        })?;
    Ok(*loaded)
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

fn default_streaming_service_symbol() -> String {
    DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL.to_string()
}

fn default_register_auth_providers_symbol() -> String {
    DEFAULT_NATIVE_REGISTER_AUTH_PROVIDERS_SYMBOL.to_string()
}

fn default_event_symbol() -> String {
    DEFAULT_NATIVE_EVENT_SYMBOL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    static BLOCKED_BRIDGE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use bcode_plugin_sdk::{SERVICE_STATUS_BUFFER_TOO_SMALL, SERVICE_STATUS_OK};
    use semver::Version;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn shell_manifest_declares_valid_workflow_block_contract() {
        let manifest: PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell manifest");
        let service = manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("workflow block service");
        assert_eq!(service.workflow_blocks.len(), 1);
        let block = &service.workflow_blocks[0];
        block.validate().expect("valid workflow block");
        assert_eq!(block.block_id, "shell.command-plan");
        assert_eq!(block.plugin_id, manifest.id);
        assert_eq!(block.effect, bcode_workflow::WorkflowBlockEffect::Mutating);
        assert!(block.authorization.explicit_grant_required);
        assert_eq!(
            block.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::RepairRequired
        );
    }

    #[test]
    fn code_review_manifest_declares_valid_workflow_block_contract() {
        let manifest: PluginManifest = toml::from_str(include_str!(
            "../../../plugins/code-review-plugin/bcode-plugin.toml"
        ))
        .expect("code review manifest");
        let service = manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("workflow block service");
        assert_eq!(service.workflow_blocks.len(), 1);
        let block = &service.workflow_blocks[0];
        block.validate().expect("valid workflow block");
        assert_eq!(block.plugin_id, manifest.id);
        assert_eq!(block.operation, "review.bundle.get");
        assert_eq!(
            block.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay
        );
    }

    #[test]
    fn plugin_serialization_reason_is_only_reentrancy_exclusivity() {
        assert_eq!(
            plugin_serialization_reason(PluginConcurrency::Exclusive),
            Some("plugin_host_reentrancy")
        );
        assert_eq!(
            plugin_serialization_reason(PluginConcurrency::Concurrent),
            None
        );
        assert_eq!(
            plugin_serialization_reason(PluginConcurrency::Limited(2)),
            None
        );
    }

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
                register_auth_providers: None,
                deactivate: lifecycle,
                invoke_service_streaming: test_streaming_service,
                cli_registration: None,
                handle_event: event,
            },
        )];
        let selection = PluginSelection {
            mode: PluginSelectionMode::All,
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
        fn event(_instance: *const std::ffi::c_void, _input: *const u8, _input_len: usize) -> i32 {
            SERVICE_STATUS_OK
        }
        let vtable = StaticPluginVtable {
            instance: std::ptr::null(),
            manifest,
            activate: lifecycle,
            register_commands: None,
            register_auth_providers: None,
            deactivate: lifecycle,
            invoke_service_streaming: test_streaming_service,
            cli_registration: None,
            handle_event: event,
        };
        let static_plugins = [
            StaticBundledPlugin::new(
                r#"
id = "bcode.example-static"
name = "Example Static"
version = "0.0.1"

[runtime]
type = "native"
abi_version = 1
library = "libexample_static.dylib"
"#,
                vtable,
            ),
            StaticBundledPlugin::new(
                r#"
id = "bcode.opt-in-static"
name = "Opt-in Static"
version = "0.0.1"

[runtime]
type = "native"
abi_version = 1
library = "libopt_in_static.dylib"
"#,
                vtable,
            )
            .with_default_activation(PluginDefaultActivation::Disabled),
        ];

        assert_eq!(
            static_bundled_plugin_ids(&static_plugins).expect("manifests should parse"),
            vec![
                "bcode.example-static".to_string(),
                "bcode.opt-in-static".to_string()
            ]
        );
        assert_eq!(
            static_bundled_default_plugin_ids(&static_plugins).expect("manifests should parse"),
            vec!["bcode.example-static".to_string()]
        );
        assert_eq!(
            static_plugins[0].default_activation(),
            PluginDefaultActivation::Enabled
        );
        assert_eq!(
            static_plugins[1].default_activation(),
            PluginDefaultActivation::Disabled
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
                execution: bcode_command::CommandExecution::Normal,
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
                    register_auth_providers: None,
                    deactivate: test_deactivate,
                    invoke_service_streaming: test_streaming_service,
                    cli_registration: None,
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
    fn plugin_host_registers_auth_providers_with_canonical_ownership() {
        fn register_auth_providers(
            _instance: *const std::ffi::c_void,
            callback: Option<AuthRegistrationCallback>,
            user_data: *mut std::ffi::c_void,
        ) -> i32 {
            let contribution = AuthProviderContribution {
                schema_version:
                    bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
                provider_id: "exa".to_owned(),
                display_name: "Exa".to_owned(),
                methods: vec![
                    bcode_provider_auth_models::AuthMethodContribution::SecretFields {
                        method_id: "api_key".to_owned(),
                        display_name: "API key".to_owned(),
                        fields: vec![bcode_provider_auth_models::AuthSecretField {
                            credential_id: "api_key".to_owned(),
                            storage_key: "TEST_PROVIDER_API_KEY".to_owned(),
                            prompt: "Exa API key".to_owned(),
                            optional: false,
                            validation: bcode_provider_auth_models::AuthSecretValidation::default(),
                        }],
                        supports_verification: false,
                        supports_revocation: false,
                    },
                ],
            };
            let payload = serde_json::to_vec(&contribution).expect("contribution encodes");
            callback.expect("registration callback")(payload.as_ptr(), payload.len(), user_data);
            SERVICE_STATUS_OK
        }

        let manifest = toml::from_str::<PluginManifest>(&format!(
            r#"
id = "bcode.web-search"
name = "Web Search"
version = "0.0.1"

[runtime]
type = "native"
abi_version = {CURRENT_PLUGIN_ABI_VERSION}
library = "libweb_search.dylib"
"#
        ))
        .expect("manifest should parse");
        let vtable = StaticPluginVtable {
            instance: std::ptr::null(),
            manifest: |_: &'static std::sync::OnceLock<Option<std::ffi::CString>>| std::ptr::null(),
            activate: test_activate,
            register_commands: None,
            register_auth_providers: Some(register_auth_providers),
            deactivate: test_deactivate,
            invoke_service_streaming: test_streaming_service,
            cli_registration: None,
            handle_event: test_handle_event,
        };
        let loaded = LoadedPlugin {
            config: ResolvedPluginConfig::default(),
            manifest,
            backend: LoadedPluginBackend::Static { vtable },
        };
        let mut registry = AuthProviderRegistry::new();

        loaded
            .register_auth_providers(&mut registry)
            .expect("plugin registers auth provider");

        let provider = registry.get("exa").expect("Exa registered");
        assert_eq!(provider.plugin_id, "bcode.web-search");
        assert_eq!(provider.contribution.display_name, "Exa");
    }

    #[test]
    fn auth_provider_registry_rejects_duplicates_and_invalid_contributions() {
        let contribution = AuthProviderContribution {
            schema_version: bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: "exa".to_owned(),
            display_name: "Exa".to_owned(),
            methods: vec![
                bcode_provider_auth_models::AuthMethodContribution::Interactive {
                    method_id: "browser".to_owned(),
                    display_name: "Browser".to_owned(),
                    operation: "login".to_owned(),
                    credentials: Vec::new(),
                    supports_revocation: false,
                },
            ],
        };
        let mut registry = AuthProviderRegistry::new();
        registry
            .register("bcode.first", contribution.clone())
            .expect("first provider");
        assert!(matches!(
            registry.register("bcode.second", contribution),
            Err(AuthProviderRegistryError::DuplicateProvider {
                first_plugin_id,
                second_plugin_id,
                ..
            }) if first_plugin_id == "bcode.first" && second_plugin_id == "bcode.second"
        ));

        let mut invalid = AuthProviderContribution {
            schema_version: bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: "valid".to_owned(),
            display_name: "Valid".to_owned(),
            methods: vec![
                bcode_provider_auth_models::AuthMethodContribution::Interactive {
                    method_id: "flow".to_owned(),
                    display_name: "Flow".to_owned(),
                    operation: "flow".to_owned(),
                    credentials: Vec::new(),
                    supports_revocation: false,
                },
            ],
        };
        invalid.schema_version += 1;
        assert!(matches!(
            registry.register("bcode.third", invalid),
            Err(AuthProviderRegistryError::InvalidContribution(
                AuthContractError::UnsupportedSchema { .. }
            ))
        ));
    }

    #[test]
    fn auth_registration_hook_and_activation_failures_are_reported() {
        fn loaded_with(
            activate: fn(*const std::ffi::c_void) -> i32,
            register_auth_providers: Option<bcode_plugin_sdk::StaticAuthRegistrationFn>,
        ) -> LoadedPlugin {
            let manifest = toml::from_str::<PluginManifest>(&format!(
                r#"
id = "bcode.auth-test"
name = "Auth Test"
version = "0.0.1"

[runtime]
type = "native"
abi_version = {CURRENT_PLUGIN_ABI_VERSION}
library = "libauth_test.dylib"
"#
            ))
            .expect("manifest");
            LoadedPlugin {
                config: ResolvedPluginConfig::default(),
                manifest,
                backend: LoadedPluginBackend::Static {
                    vtable: StaticPluginVtable {
                        instance: std::ptr::null(),
                        manifest: |_: &'static OnceLock<Option<std::ffi::CString>>| {
                            std::ptr::null()
                        },
                        activate,
                        register_commands: None,
                        register_auth_providers,
                        deactivate: test_deactivate,
                        invoke_service_streaming: test_streaming_service,
                        cli_registration: None,
                        handle_event: test_handle_event,
                    },
                },
            }
        }

        assert!(matches!(
            loaded_with(test_activate_failed, None).activate(),
            Err(PluginLoadError::LifecycleFailed {
                hook: "activate",
                code: 71,
                ..
            })
        ));
        assert!(matches!(
            loaded_with(test_activate, Some(test_register_auth_failed))
                .register_auth_providers(&mut AuthProviderRegistry::new()),
            Err(PluginLoadError::LifecycleFailed {
                hook: "register_auth_providers",
                code: 72,
                ..
            })
        ));
        assert!(matches!(
            loaded_with(test_activate, Some(test_register_auth_malformed))
                .register_auth_providers(&mut AuthProviderRegistry::new()),
            Err(PluginLoadError::AuthRegistration {
                source: AuthProviderRegistryError::InvalidContribution(_),
                ..
            })
        ));
    }

    #[test]
    fn static_plugins_with_incompatible_abi_fail_closed() {
        let mut manifest = toml::from_str::<PluginManifest>(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("hello manifest");
        let PluginRuntime::Native(runtime) = &mut manifest.runtime;
        runtime.abi_version = CURRENT_PLUGIN_ABI_VERSION - 1;

        assert!(matches!(
            load_static_plugin(manifest, bcode_hello_plugin::static_plugin()),
            Err(PluginLoadError::UnsupportedAbi { actual, expected, .. })
                if actual + 1 == expected
        ));
    }

    #[test]
    fn registered_plugins_expose_config_extension_catalog() {
        let manifest = PluginManifest {
            id: "bcode.example".to_string(),
            name: "Example".to_string(),
            version: Version::new(0, 1, 0),
            services: Vec::new(),
            tui_surfaces: Vec::new(),
            visual_adapters: Vec::new(),
            tool_presentations: Vec::new(),
            command_contributions: Vec::new(),
            workflow_templates: Vec::new(),
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
                streaming_service_symbol: default_streaming_service_symbol(),
                register_auth_providers_symbol: default_register_auth_providers_symbol(),
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
            visual_adapters: Vec::new(),
            tool_presentations: Vec::new(),
            command_contributions: Vec::new(),
            workflow_templates: Vec::new(),
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
                streaming_service_symbol: default_streaming_service_symbol(),
                register_auth_providers_symbol: default_register_auth_providers_symbol(),
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
    fn parses_tool_presentation_metadata() {
        let manifest: PluginManifest = toml::from_str(include_str!(
            "../../../plugins/filesystem-plugin/bcode-plugin.toml"
        ))
        .expect("filesystem manifest should parse");
        let write = manifest
            .tool_presentations
            .iter()
            .find(|presentation| presentation.tool_name == "filesystem.write")
            .expect("write presentation");
        assert_eq!(
            write.request_draft_schema,
            "bcode.filesystem.request-draft.write"
        );
        assert_eq!(write.request_draft_schema_version, 1);
    }

    #[test]
    fn rejects_invalid_tool_presentation_metadata() {
        let mut manifest = test_manifest("bcode.invalid-presentation");
        manifest.services[0].interface_id = bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_owned();
        manifest.tool_presentations = vec![PluginToolPresentationDeclaration {
            tool_name: "filesystem.write".to_owned(),
            request_schema: "bcode.filesystem.request-draft.write".to_owned(),
            request_schema_version: 1,
            request_draft_schema: String::new(),
            request_draft_schema_version: 1,
        }];

        let error = validate_tool_presentation_declarations([&manifest])
            .expect_err("empty schemas must be rejected");
        assert!(matches!(
            error,
            PluginLoadError::InvalidToolPresentation { reason, .. }
                if reason == "request_draft_schema must not be empty"
        ));
    }

    #[test]
    fn rejects_tool_presentation_without_matching_adapter() {
        let mut manifest = test_manifest("bcode.invalid-presentation");
        manifest.services[0].interface_id = bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_owned();
        manifest.tool_presentations = vec![PluginToolPresentationDeclaration {
            tool_name: "filesystem.write".to_owned(),
            request_schema: "bcode.filesystem.request-draft.write".to_owned(),
            request_schema_version: 1,
            request_draft_schema: "bcode.filesystem.request-draft.write".to_owned(),
            request_draft_schema_version: 1,
        }];

        let error = validate_tool_presentation_declarations([&manifest])
            .expect_err("unsupported schemas must be rejected");
        assert!(matches!(
            error,
            PluginLoadError::InvalidToolPresentation { reason, .. }
                if reason == "no visual adapter supports the declared request schema version"
        ));
    }

    #[test]
    fn rejects_ambiguous_tool_presentation_ownership() {
        let mut first = test_manifest("bcode.first");
        first.services[0].interface_id = bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_owned();
        first.visual_adapters.push(PluginVisualAdapterDeclaration {
            id: "draft".to_owned(),
            schema: "test.draft".to_owned(),
            min_schema_version: Some(1),
            max_schema_version: Some(1),
            service_interface_id: bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_owned(),
            surfaces: vec!["tui".to_owned()],
            priority: 0,
            producer_default: true,
            render_mode: PluginVisualAdapterRenderMode::TranscriptBlock,
        });
        first.tool_presentations = vec![PluginToolPresentationDeclaration {
            tool_name: "duplicate.tool".to_owned(),
            request_schema: "test.draft".to_owned(),
            request_schema_version: 1,
            request_draft_schema: "test.draft".to_owned(),
            request_draft_schema_version: 1,
        }];
        let mut second = first.clone();
        second.id = "bcode.second".to_owned();

        let error = validate_tool_presentation_declarations([&first, &second])
            .expect_err("ambiguous tool ownership must be rejected");
        assert!(matches!(
            error,
            PluginLoadError::AmbiguousToolPresentation { tool_name, .. }
                if tool_name == "duplicate.tool"
        ));
    }

    #[test]
    fn validates_tool_presentation_catalog_ownership() {
        let mut manifest = test_manifest("bcode.catalog-owner");
        manifest.tool_presentations = vec![PluginToolPresentationDeclaration {
            tool_name: "expected.tool".to_owned(),
            request_schema: "test.draft".to_owned(),
            request_schema_version: 1,
            request_draft_schema: "test.draft".to_owned(),
            request_draft_schema_version: 1,
        }];
        let loaded = LoadedPlugin {
            config: ResolvedPluginConfig::default(),
            manifest,
            backend: LoadedPluginBackend::Static {
                vtable: test_streaming_vtable(),
            },
        };
        let host = PluginHost {
            loaded: vec![loaded],
            configs: BTreeMap::new(),
            command_registry: bcode_command::CommandRegistry::new(),
            auth_provider_registry: AuthProviderRegistry::new(),
        };

        let error = host
            .validate_loaded_tool_presentation_ownership(
                "bcode.catalog-owner",
                &bcode_tool::ToolList {
                    tools: vec![bcode_tool::ToolDefinition {
                        name: "different.tool".to_owned(),
                        description: String::new(),
                        input_schema: serde_json::json!({}),
                    }],
                },
            )
            .expect_err("manifest tool names must match the runtime catalog");
        assert!(matches!(
            error,
            PluginLoadError::InvalidToolPresentation { reason, .. }
                if reason == "the plugin does not expose this exact name from list_tools"
        ));
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

    fn hello_dynamic_library_path() -> PathBuf {
        let executable = std::env::current_exe().expect("current test executable path");
        let directory = executable.parent().expect("test executable parent");
        let prefix = format!("{}bcode_hello_plugin", std::env::consts::DLL_PREFIX);
        std::fs::read_dir(directory)
            .expect("test dependency directory should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(&prefix) && name.ends_with(std::env::consts::DLL_SUFFIX)
                    })
            })
            .expect("hello plugin dynamic library should be built as a dev dependency")
    }

    fn load_dynamic_hello_plugin() -> LoadedPlugin {
        let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/hello-plugin/bcode-plugin.toml");
        let mut manifest = toml::from_str::<PluginManifest>(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("hello manifest should parse");
        let PluginRuntime::Native(runtime) = &mut manifest.runtime;
        runtime.library = hello_dynamic_library_path();
        load_registered_plugin(&RegisteredPlugin {
            manifest_path,
            manifest,
        })
        .expect("dynamic hello plugin should load")
    }

    fn load_static_hello_plugin() -> LoadedPlugin {
        let manifest = toml::from_str::<PluginManifest>(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("hello manifest should parse");
        load_static_plugin(manifest, bcode_hello_plugin::static_plugin())
            .expect("static hello plugin should load")
    }

    #[allow(clippy::unnecessary_wraps)]
    fn hello_bridge_response(
        request: ServiceBridgeRequest,
        _: bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<ServiceBridgeResponse, String> {
        Ok(match request {
            ServiceBridgeRequest::Exchange(_) => {
                ServiceBridgeResponse::Exchange(bcode_tool::ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"answer": true}),
                })
            }
            ServiceBridgeRequest::ReceiveInput { .. } => {
                ServiceBridgeResponse::Input(bcode_tool::ToolInvocationInputResolution::Closed)
            }
            ServiceBridgeRequest::InvokeService(_) => ServiceBridgeResponse::Service(
                bcode_tool::ToolInvocationServiceResolution::Responded {
                    payload: serde_json::json!({"nested": true}),
                },
            ),
            ServiceBridgeRequest::WriteArtifact(request) => {
                ServiceBridgeResponse::Artifact(bcode_tool::ToolArtifactWriteResolution::Written {
                    artifact_id: request.artifact_id,
                    byte_len: u64::try_from(request.bytes.len()).unwrap_or(u64::MAX),
                    reference: serde_json::json!({"stored": true}),
                })
            }
        })
    }

    fn assert_hello_plugin_bridge_and_cancellation(plugin: LoadedPlugin) {
        let plugin = Arc::new(plugin);
        let mut events = Vec::new();
        let event_response = plugin
            .invoke_service_with_bridge(
                "example-hello/v1",
                "emit-event",
                Vec::new(),
                |event| events.push(event),
                |_, _| Err("bridge is unused".to_string()),
                &bcode_plugin_sdk::ServiceCancellation::default(),
            )
            .expect("event invocation should complete");
        assert_eq!(event_response.payload, b"event-emitted");
        assert_eq!(events, vec![b"hello-event".to_vec()]);

        let response = plugin
            .invoke_service_with_bridge(
                "example-hello/v1",
                "bridge-all",
                Vec::new(),
                |_| {},
                hello_bridge_response,
                &bcode_plugin_sdk::ServiceCancellation::default(),
            )
            .expect("bridge invocation should complete");
        let responses = response
            .payload_json::<Vec<ServiceBridgeResponse>>()
            .expect("bridge responses should decode");
        assert!(matches!(responses[0], ServiceBridgeResponse::Exchange(_)));
        assert!(matches!(responses[1], ServiceBridgeResponse::Input(_)));
        assert!(matches!(responses[2], ServiceBridgeResponse::Service(_)));
        assert!(matches!(responses[3], ServiceBridgeResponse::Artifact(_)));

        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let task_plugin = Arc::clone(&plugin);
        let task_cancellation = cancellation.clone();
        let started = Instant::now();
        let task = std::thread::spawn(move || {
            task_plugin.invoke_service_with_bridge(
                "example-hello/v1",
                "wait-cancelled",
                Vec::new(),
                |_| {},
                |_, _| Err("bridge is unused".to_string()),
                &task_cancellation,
            )
        });
        std::thread::sleep(Duration::from_millis(25));
        cancellation.cancel();
        let response = task
            .join()
            .expect("service thread should join")
            .expect("cancellation invocation should complete");

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(response.payload, b"cancelled");
    }

    fn assert_blocked_plugin_bridge_wakes_on_cancellation(plugin: LoadedPlugin) {
        let _blocked_bridge_guard = BLOCKED_BRIDGE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let plugin = Arc::new(plugin);
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let task_plugin = Arc::clone(&plugin);
        let task_cancellation = cancellation.clone();
        let started = std::sync::mpsc::sync_channel(1);
        let (started_tx, started_rx) = started;
        let (woke_tx, woke_rx) = std::sync::mpsc::sync_channel(1);
        let task = std::thread::spawn(move || {
            task_plugin.invoke_service_with_bridge(
                "example-hello/v1",
                "bridge-exchange",
                Vec::new(),
                |_| {},
                |_, callback_cancellation| {
                    let _ = started_tx.send(());
                    if callback_cancellation.wait_cancelled(Duration::from_secs(5)) {
                        let _ = woke_tx.send(());
                        Err("cancelled".to_string())
                    } else {
                        panic!("ABI bridge callback did not wake on cancellation")
                    }
                },
                &task_cancellation,
            )
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("ABI bridge callback did not start");

        cancellation.cancel();
        woke_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("ABI bridge callback did not wake promptly on cancellation");
        let response = task
            .join()
            .expect("bridge invocation thread should join")
            .expect("bridge invocation should return a plugin response");

        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("bridge_failed")
        );
    }

    #[test]
    fn dynamic_and_static_hello_plugins_register_auth_providers() {
        for plugin in [load_dynamic_hello_plugin(), load_static_hello_plugin()] {
            let mut registry = AuthProviderRegistry::new();
            plugin
                .register_auth_providers(&mut registry)
                .expect("hello auth provider registration");
            let provider = registry
                .get("example-hello")
                .expect("example provider registered");
            assert_eq!(provider.plugin_id, "example.hello");
        }
    }

    #[tokio::test]
    async fn scoped_timeout_propagates_cancellation_to_static_plugin() {
        let manifest = toml::from_str::<PluginManifest>(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("hello manifest should parse");
        let host =
            PluginHost::load_static_plugins(&[(manifest, bcode_hello_plugin::static_plugin())])
                .expect("static hello host should load");
        let runtime = PluginRuntimeHost::from(host);
        let started = Instant::now();
        let error = runtime
            .invoke_service_scoped_with_timeout(
                "example.hello",
                "example-hello/v1",
                "wait-cancelled",
                Vec::new(),
                PluginInvocationScope::Global,
                Duration::from_millis(25),
            )
            .await
            .expect_err("wait operation should time out");
        assert!(matches!(
            error,
            PluginLoadError::ServiceInvocationTimeout {
                plugin_id,
                timeout_ms: 25
            } if plugin_id == "example.hello"
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn disabled_static_plugin_does_not_contribute_auth_provider() {
        let static_plugin = StaticBundledPlugin::new(
            include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
            bcode_hello_plugin::static_plugin(),
        );
        let mut selection = PluginSelection::all_enabled();
        selection.disabled.insert("example.hello".to_owned());
        let selected =
            filter_selected_static_plugins(&[static_plugin], &selection).expect("static selection");
        let host = PluginHost::load_static_plugins(&selected).expect("empty selected host");

        assert!(selected.is_empty());
        assert!(host.auth_provider_registry().is_empty());
    }

    #[test]
    fn dynamic_blocked_abi_bridge_wakes_on_cancellation() {
        assert_blocked_plugin_bridge_wakes_on_cancellation(load_dynamic_hello_plugin());
    }

    #[test]
    fn static_blocked_abi_bridge_wakes_on_cancellation() {
        assert_blocked_plugin_bridge_wakes_on_cancellation(load_static_hello_plugin());
    }
    fn question_dynamic_library_path() -> PathBuf {
        let workspace_library = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(format!(
                "{}bcode_question_plugin{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            ));
        if workspace_library.exists() {
            return workspace_library;
        }
        let executable = std::env::current_exe().expect("current test executable path");
        let directory = executable.parent().expect("test executable parent");
        let prefix = format!(
            "{}bcode_question_dynamic_plugin",
            std::env::consts::DLL_PREFIX
        );
        std::fs::read_dir(directory)
            .expect("test dependency directory should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(&prefix) && name.ends_with(std::env::consts::DLL_SUFFIX)
                    })
            })
            .expect("question plugin dynamic library should be built as a dev dependency")
    }

    fn load_dynamic_question_plugin() -> LoadedPlugin {
        let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/question-plugin/bcode-plugin.toml");
        let mut manifest = toml::from_str::<PluginManifest>(include_str!(
            "../../../plugins/question-plugin/bcode-plugin.toml"
        ))
        .expect("question manifest should parse");
        let PluginRuntime::Native(runtime) = &mut manifest.runtime;
        runtime.library = question_dynamic_library_path();
        load_registered_plugin(&RegisteredPlugin {
            manifest_path,
            manifest,
        })
        .expect("dynamic question plugin should load")
    }

    fn load_static_question_plugin() -> LoadedPlugin {
        let manifest = toml::from_str::<PluginManifest>(include_str!(
            "../../../plugins/question-plugin/bcode-plugin.toml"
        ))
        .expect("question manifest should parse");
        load_static_plugin(manifest, bcode_question_plugin::static_plugin())
            .expect("static question plugin should load")
    }

    fn assert_question_exchange_plugin(plugin: &LoadedPlugin) {
        let request = bcode_tool::ToolInvocationRequest {
            tool_call_id: "question-call".to_string(),
            name: "question".to_string(),
            arguments: serde_json::json!({
                "questions": [{
                    "question": "Proceed?",
                    "options": [{"label": "Yes", "value": "yes"}],
                    "required": true
                }]
            }),
            preparation_descriptor: serde_json::Value::Null,
        };
        let payload = serde_json::to_vec(&request).expect("question request encodes");
        let mut requests = Vec::new();
        let response = plugin
            .invoke_service_with_bridge(
                bcode_tool::TOOL_SERVICE_INTERFACE_ID,
                bcode_tool::OP_INVOKE_TOOL,
                payload,
                |_| {},
                |request, _| {
                    requests.push(request);
                    Ok(ServiceBridgeResponse::Exchange(
                        bcode_tool::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({
                                "status": "answered",
                                "questions": [{
                                    "question_index": 0,
                                    "selected": ["yes"]
                                }]
                            }),
                        },
                    ))
                },
                &bcode_plugin_sdk::ServiceCancellation::default(),
            )
            .expect("question service should invoke");
        let response: bcode_tool::ToolInvocationResponse =
            decode_service_response(response).expect("question response decodes");

        assert!(!response.is_error);
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0],
            ServiceBridgeRequest::Exchange(request)
                if request.invocation_id == "question-call"
                    && request.schema == "bcode.question.request"
        ));
    }

    fn assert_pending_question_does_not_block_plugin_services(plugin: LoadedPlugin) {
        let plugin = Arc::new(plugin);
        let request = bcode_tool::ToolInvocationRequest {
            tool_call_id: "blocking-question-call".to_string(),
            name: "question".to_string(),
            arguments: serde_json::json!({
                "questions": [{
                    "question": "Proceed?",
                    "options": [{"label": "Yes", "value": "yes"}],
                    "required": true
                }]
            }),
            preparation_descriptor: serde_json::Value::Null,
        };
        let payload = serde_json::to_vec(&request).expect("question request encodes");
        let (bridge_started_tx, bridge_started_rx) = std::sync::mpsc::sync_channel(1);
        let (answer_tx, answer_rx) = std::sync::mpsc::sync_channel(1);
        let question_plugin = Arc::clone(&plugin);
        let question = std::thread::spawn(move || {
            question_plugin.invoke_service_with_bridge(
                bcode_tool::TOOL_SERVICE_INTERFACE_ID,
                bcode_tool::OP_INVOKE_TOOL,
                payload,
                |_| {},
                |request, _| {
                    bridge_started_tx
                        .send(())
                        .expect("question bridge start should be observed");
                    answer_rx
                        .recv()
                        .expect("question answer should be delivered");
                    assert!(matches!(request, ServiceBridgeRequest::Exchange(_)));
                    Ok(ServiceBridgeResponse::Exchange(
                        bcode_tool::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({
                                "status": "answered",
                                "questions": [{
                                    "question_index": 0,
                                    "selected": ["yes"]
                                }]
                            }),
                        },
                    ))
                },
                &bcode_plugin_sdk::ServiceCancellation::default(),
            )
        });
        bridge_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("question bridge should start");

        let list_plugin = Arc::clone(&plugin);
        let (list_tx, list_rx) = std::sync::mpsc::sync_channel(1);
        let list = std::thread::spawn(move || {
            let payload = serde_json::to_vec(&bcode_tool::ListToolsRequest::default())
                .expect("list request encodes");
            let response = list_plugin.invoke_service(
                bcode_tool::TOOL_SERVICE_INTERFACE_ID,
                bcode_tool::OP_LIST_TOOLS,
                payload,
            );
            let _ = list_tx.send(response);
        });
        let list_response = list_rx.recv_timeout(Duration::from_secs(1));

        answer_tx.send(()).expect("question answer should send");
        let question_response = question
            .join()
            .expect("question invocation thread should join")
            .expect("question invocation should complete");
        let question_response: bcode_tool::ToolInvocationResponse =
            decode_service_response(question_response).expect("question response decodes");
        assert!(!question_response.is_error);
        list.join().expect("list invocation thread should join");

        let list_response = list_response
            .expect("list_tools must not wait for an unrelated pending question")
            .expect("list_tools invocation should complete");
        let tools: bcode_tool::ToolList =
            decode_service_response(list_response).expect("tool list response decodes");
        assert!(tools.tools.iter().any(|tool| tool.name == "question"));
    }

    #[test]
    fn dynamic_question_plugin_uses_same_invocation_exchange() {
        assert_question_exchange_plugin(&load_dynamic_question_plugin());
    }

    #[test]
    fn dynamic_pending_question_does_not_block_plugin_services() {
        assert_pending_question_does_not_block_plugin_services(load_dynamic_question_plugin());
    }

    #[test]
    fn static_question_plugin_uses_same_invocation_exchange() {
        assert_question_exchange_plugin(&load_static_question_plugin());
    }

    #[test]
    fn static_pending_question_does_not_block_plugin_services() {
        assert_pending_question_does_not_block_plugin_services(load_static_question_plugin());
    }
    #[test]
    fn dynamic_loader_supports_all_bridge_families_and_cancellation() {
        assert_hello_plugin_bridge_and_cancellation(load_dynamic_hello_plugin());
    }

    #[test]
    #[ignore = "subprocess helper for aborting_dynamic_library_terminates_only_fixture_process"]
    fn aborting_dynamic_library_subprocess_helper() {
        if std::env::var_os("BCODE_ABORT_DYNAMIC_PLUGIN_HELPER").is_none() {
            return;
        }
        let plugin = load_dynamic_hello_plugin();
        let _ = plugin.invoke_service_with_events(
            "example-hello/v1",
            "abort-process",
            Vec::new(),
            |_| {},
        );
        panic!("aborting dynamic plugin unexpectedly returned");
    }

    #[test]
    fn aborting_dynamic_library_terminates_only_fixture_process() {
        let executable = std::env::current_exe().expect("current test executable");
        let output = std::process::Command::new(executable)
            .arg("tests::aborting_dynamic_library_subprocess_helper")
            .arg("--ignored")
            .arg("--exact")
            .arg("--nocapture")
            .env("BCODE_ABORT_DYNAMIC_PLUGIN_HELPER", "1")
            .output()
            .expect("run aborting dynamic plugin helper");
        assert!(
            !output.status.success(),
            "aborting dynamic plugin helper must terminate unsuccessfully"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout)
                .contains("aborting dynamic plugin unexpectedly returned")
        );
    }

    #[test]
    fn static_loader_supports_all_bridge_families_and_cancellation() {
        assert_hello_plugin_bridge_and_cancellation(load_static_hello_plugin());
    }

    #[test]
    fn static_service_bridge_round_trips_neutral_request() {
        let plugin = LoadedPlugin {
            config: ResolvedPluginConfig::default(),
            manifest: test_manifest("bridge"),
            backend: LoadedPluginBackend::Static {
                vtable: test_bridge_vtable(),
            },
        };
        let mut requests = Vec::new();

        let response = plugin
            .invoke_service_with_bridge(
                "bridge",
                "run",
                Vec::new(),
                |_| {},
                |request, _| {
                    requests.push(request);
                    Ok(ServiceBridgeResponse::Exchange(
                        bcode_tool::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({"answer": true}),
                        },
                    ))
                },
                &bcode_plugin_sdk::ServiceCancellation::default(),
            )
            .expect("service should invoke");

        assert_eq!(response.payload, b"bridge-ok");
        assert_eq!(requests.len(), 1);
        assert!(matches!(requests[0], ServiceBridgeRequest::Exchange(_)));
    }

    #[test]
    fn blocked_static_bridge_call_wakes_on_cancellation() {
        let _blocked_bridge_guard = BLOCKED_BRIDGE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let plugin = Arc::new(LoadedPlugin {
            config: ResolvedPluginConfig::default(),
            manifest: test_manifest("bridge"),
            backend: LoadedPluginBackend::Static {
                vtable: test_bridge_vtable(),
            },
        });
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let task_plugin = Arc::clone(&plugin);
        let task_cancellation = cancellation.clone();
        let started = Arc::new(AtomicBool::new(false));
        let task_started = Arc::clone(&started);
        let task = std::thread::spawn(move || {
            task_plugin.invoke_service_with_bridge(
                "bridge",
                "run",
                Vec::new(),
                |_| {},
                |_, cancellation| {
                    task_started.store(true, Ordering::SeqCst);
                    if cancellation.wait_cancelled(Duration::from_secs(5)) {
                        Err("cancelled".to_string())
                    } else {
                        panic!("bridge handler did not wake on cancellation")
                    }
                },
                &task_cancellation,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "bridge handler did not start");
            std::thread::yield_now();
        }
        let cancel_started = Instant::now();
        cancellation.cancel();
        let response = task
            .join()
            .expect("service thread should join")
            .expect("service invocation should complete");

        assert!(cancel_started.elapsed() < Duration::from_secs(1));
        assert_eq!(response.payload, b"bridge-cancelled");
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
    fn streaming_events_cannot_be_overtaken_by_the_final_response() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");
        runtime.block_on(async {
            let (response_tx, response) = oneshot::channel();
            let (events_tx, events) = mpsc::unbounded_channel();
            events_tx.send(b"committed-prefix".to_vec()).expect("event");
            response_tx
                .send(Ok(ServiceResponse::text("complete")))
                .expect("response");
            drop(events_tx);
            let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
            let mut invocation = StreamingServiceInvocation {
                response,
                events,
                cancel: PluginInvocationCancelHandle {
                    id: PluginInvocationId(1),
                    cancellation,
                },
                resource_permit: None,
            };

            assert!(matches!(
                invocation.next_event().await.expect("first item"),
                StreamingServiceInvocationEvent::Event(payload)
                    if payload == b"committed-prefix"
            ));
            assert!(matches!(
                invocation.next_event().await.expect("second item"),
                StreamingServiceInvocationEvent::Response(Ok(response))
                    if response.payload_text().ok() == Some("complete")
            ));
            drop(invocation);
        });
    }

    #[test]
    fn concurrent_streaming_service_sends_response_and_events() {
        let mut manifest = test_manifest("events");
        manifest.concurrency = PluginConcurrencyConfig::Limited { max: 1 };
        let runtime = PluginRuntimeHost::from(PluginHost {
            configs: BTreeMap::new(),
            command_registry: bcode_command::CommandRegistry::new(),
            auth_provider_registry: AuthProviderRegistry::new(),
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
    fn discovers_and_loads_dynamic_plugin_when_fixture_is_configured() {
        let Some(root) = std::env::var_os("BCODE_DYNAMIC_PLUGIN_TEST_ROOT") else {
            return;
        };
        let plugins = discover_plugins_in_roots(&[PathBuf::from(root)]).expect("discovery");
        let plugin = plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "bcode.workflow")
            .expect("workflow manifest");
        let loaded = load_registered_plugin(plugin).expect("dynamic workflow plugin loads");
        assert_eq!(loaded.manifest().id, "bcode.workflow");
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
                    workflow_blocks: Vec::new(),
                    invocation_operations: Vec::new(),
                }],
                tui_surfaces: Vec::new(),
                visual_adapters: Vec::new(),
                tool_presentations: Vec::new(),
                command_contributions: Vec::new(),
                workflow_templates: Vec::new(),
                event_subscriptions: Vec::new(),
                concurrency: PluginConcurrencyConfig::Exclusive,
                runtime: PluginRuntime::Native(NativePluginRuntime {
                    abi_version: CURRENT_PLUGIN_ABI_VERSION,
                    library: PathBuf::from("test"),
                    manifest_symbol: DEFAULT_NATIVE_MANIFEST_SYMBOL.to_string(),
                    activate_symbol: DEFAULT_NATIVE_ACTIVATE_SYMBOL.to_string(),
                    deactivate_symbol: DEFAULT_NATIVE_DEACTIVATE_SYMBOL.to_string(),
                    streaming_service_symbol: DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL.to_string(),
                    register_auth_providers_symbol: DEFAULT_NATIVE_REGISTER_AUTH_PROVIDERS_SYMBOL
                        .to_string(),
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
                auth_provider_registry: AuthProviderRegistry::new(),
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
                                register_auth_providers: None,
                                deactivate,
                                invoke_service_streaming:
                                    |_,
                                     input_ptr,
                                     input_len,
                                     output_ptr,
                                     output_capacity,
                                     output_len,
                                     _,
                                     _,
                                     _,
                                     _,
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
                                cli_registration: None,
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
                                register_auth_providers: None,
                                deactivate,
                                invoke_service_streaming:
                                    |_,
                                     input_ptr,
                                     input_len,
                                     output_ptr,
                                     output_capacity,
                                     output_len,
                                     _,
                                     _,
                                     _,
                                     _,
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
                                cli_registration: None,
                            },
                        },
                    },
                ],
            });
            let slow = runtime.clone();
            let first_slow_task = tokio::spawn(async move {
                slow.invoke_service("slow", "slow", "run", Vec::new()).await
            });
            tokio::time::sleep(Duration::from_millis(25)).await;
            let slow = runtime.clone();
            let second_slow_task = tokio::spawn(async move {
                slow.invoke_service("slow", "slow", "run", Vec::new()).await
            });
            tokio::time::timeout(Duration::from_millis(100), async {
                loop {
                    let status = runtime
                        .executor_statuses()
                        .into_iter()
                        .find(|status| status.plugin_id == "slow")
                        .expect("slow executor status");
                    if status.running == 1 && status.queued == 1 {
                        assert_eq!(status.concurrency, PluginConcurrency::Exclusive);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("exclusive plugin should expose one queued invocation");
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
            let first_slow = first_slow_task
                .await
                .expect("first slow task joins")
                .expect("first slow returns");
            let second_slow = second_slow_task
                .await
                .expect("second slow task joins")
                .expect("second slow returns");
            assert_eq!(first_slow.payload, b"slow");
            assert_eq!(second_slow.payload, b"slow");
        });
        assert_eq!(SLOW_CALLS.load(Ordering::SeqCst), 2);
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
            _: Option<ServiceBridgeCallback>,
            _: *mut c_void,
            _: Option<ServiceCancellationWaitCallback>,
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
                workflow_blocks: Vec::new(),
                invocation_operations: Vec::new(),
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
                auth_provider_registry: AuthProviderRegistry::new(),
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
                            register_auth_providers: None,
                            deactivate: test_deactivate,
                            invoke_service_streaming: service_streaming,
                            handle_event: test_handle_event,
                            cli_registration: None,
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
            _: Option<ServiceBridgeCallback>,
            _: *mut c_void,
            _: Option<ServiceCancellationWaitCallback>,
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
                workflow_blocks: Vec::new(),
                invocation_operations: Vec::new(),
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
                auth_provider_registry: AuthProviderRegistry::new(),
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
                            register_auth_providers: None,
                            deactivate: test_deactivate,
                            invoke_service_streaming: service_streaming,
                            handle_event: test_handle_event,
                            cli_registration: None,
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

    fn test_activate_failed(_: *const std::ffi::c_void) -> i32 {
        71
    }

    fn test_register_auth_failed(
        _: *const std::ffi::c_void,
        _: Option<AuthRegistrationCallback>,
        _: *mut std::ffi::c_void,
    ) -> i32 {
        72
    }

    fn test_register_auth_malformed(
        _: *const std::ffi::c_void,
        callback: Option<AuthRegistrationCallback>,
        user_data: *mut std::ffi::c_void,
    ) -> i32 {
        let payload = b"not-json";
        callback.expect("registration callback")(payload.as_ptr(), payload.len(), user_data);
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
    fn test_bridge_service(
        _instance: *const std::ffi::c_void,
        _input_ptr: *const u8,
        _input_len: usize,
        output: *mut u8,
        cap: usize,
        len: *mut usize,
        _event_callback: Option<ServiceEventCallback>,
        _event_user_data: *mut std::ffi::c_void,
        bridge_callback: Option<ServiceBridgeCallback>,
        bridge_user_data: *mut std::ffi::c_void,
        _cancellation_callback: Option<ServiceCancellationWaitCallback>,
        _cancellation_user_data: *mut std::ffi::c_void,
    ) -> i32 {
        let callback = bridge_callback.expect("bridge callback should be provided");
        let request = ServiceBridgeRequest::Exchange(bcode_tool::ToolExchangeRequest {
            invocation_id: "invoke".to_string(),
            exchange_id: "exchange".to_string(),
            producer_id: "producer".to_string(),
            schema: "example.exchange".to_string(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
        });
        let request = serde_json::to_vec(&request).expect("bridge request encodes");
        let mut bridge_output = vec![0; SERVICE_BRIDGE_MAX_RESPONSE_BYTES];
        let mut bridge_output_len = 0;
        let status = callback(
            request.as_ptr(),
            request.len(),
            bridge_output.as_mut_ptr(),
            bridge_output.len(),
            &raw mut bridge_output_len,
            bridge_user_data,
        );
        if status == SERVICE_BRIDGE_STATUS_CANCELLED {
            return write_test_response(
                &ServiceResponse::text("bridge-cancelled"),
                output,
                cap,
                len,
            );
        }
        assert_eq!(status, SERVICE_BRIDGE_STATUS_OK);
        bridge_output.truncate(bridge_output_len);
        let response = serde_json::from_slice::<ServiceBridgeResponse>(&bridge_output)
            .expect("bridge response decodes");
        assert!(matches!(response, ServiceBridgeResponse::Exchange(_)));
        write_test_response(&ServiceResponse::text("bridge-ok"), output, cap, len)
    }

    fn test_bridge_vtable() -> StaticPluginVtable {
        let mut vtable = test_streaming_vtable();
        vtable.invoke_service_streaming = test_bridge_service;
        vtable
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
        _bridge_callback: Option<ServiceBridgeCallback>,
        _bridge_user_data: *mut std::ffi::c_void,
        _cancellation_callback: Option<ServiceCancellationWaitCallback>,
        _cancellation_user_data: *mut std::ffi::c_void,
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
        _bridge_callback: Option<ServiceBridgeCallback>,
        _bridge_user_data: *mut std::ffi::c_void,
        _cancellation_callback: Option<ServiceCancellationWaitCallback>,
        _cancellation_user_data: *mut std::ffi::c_void,
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
            register_auth_providers: None,
            deactivate: test_deactivate,
            invoke_service_streaming: test_large_chunking_service,
            cli_registration: None,
            handle_event: test_handle_event,
        }
    }

    fn test_large_vtable() -> StaticPluginVtable {
        StaticPluginVtable {
            instance: std::ptr::null(),
            manifest: |_: &'static std::sync::OnceLock<Option<std::ffi::CString>>| std::ptr::null(),
            activate: test_activate,
            register_commands: None,
            register_auth_providers: None,
            deactivate: test_deactivate,
            invoke_service_streaming: |instance,
                                       input_ptr,
                                       input_len,
                                       output,
                                       cap,
                                       len,
                                       _,
                                       _,
                                       _,
                                       _,
                                       _,
                                       _| {
                test_large_service(instance, input_ptr, input_len, output, cap, len)
            },
            cli_registration: None,
            handle_event: test_handle_event,
        }
    }

    fn test_streaming_vtable() -> StaticPluginVtable {
        StaticPluginVtable {
            instance: std::ptr::null(),
            manifest: |_: &'static std::sync::OnceLock<Option<std::ffi::CString>>| std::ptr::null(),
            activate: test_activate,
            register_commands: None,
            register_auth_providers: None,
            deactivate: test_deactivate,
            invoke_service_streaming: test_streaming_service,
            cli_registration: None,
            handle_event: test_handle_event,
        }
    }

    fn workflow_template() -> WorkflowTemplateContribution {
        let schema = bcode_workflow::ValueSchema {
            type_name: "bcode.example.config/v1".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        let definition = bcode_workflow::WorkflowDefinition {
            schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "example-template".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            nodes: BTreeMap::from([(
                "transform".to_string(),
                bcode_workflow::NodeDefinition {
                    id: "transform".to_string(),
                    name: "Transform".to_string(),
                    kind: bcode_workflow::NodeKind::Input,
                    dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema,
                    resources: Vec::new(),
                    configuration: serde_json::json!({"version": 1}),
                },
            )]),
            entries: vec!["transform".to_string()],
            exits: vec!["transform".to_string()],
            edges: Vec::new(),
        };
        WorkflowTemplateContribution {
            contribution_version: WORKFLOW_TEMPLATE_CONTRIBUTION_VERSION,
            template_id: "example".to_string(),
            template_version: 1,
            title: "Example".to_string(),
            description: "A declarative example workflow.".to_string(),
            configuration_schema: definition.input.clone(),
            compilation_bindings: Vec::new(),
            definition,
            required_plugins: Vec::new(),
            required_skills: Vec::new(),
            required_capabilities: vec!["transforms/v1".to_string()],
            presentation: BTreeMap::from([("category".to_string(), "examples".to_string())]),
        }
    }

    #[test]
    fn workflow_templates_validate_without_starting_and_have_exact_identity() {
        let template = workflow_template();
        template.validate().expect("template validates");
        let identity = template
            .definition_identity("bcode.example")
            .expect("identity");
        assert!(
            identity
                .definition_id
                .starts_with("bcode.example/example@1@")
        );

        let mut binding = WorkflowTemplateCompilationBinding {
            configuration_path: "commit_message_skill".to_string(),
            node_id: "transform".to_string(),
            skill_mode: bcode_workflow::AgentSkillActivationMode::Required,
            absent_fallback_edge: None,
        };
        let mut non_agent = template.clone();
        non_agent.compilation_bindings.push(binding.clone());
        assert!(non_agent.validate().is_err());

        binding.node_id = "missing".to_string();
        let mut missing = template.clone();
        missing.compilation_bindings.push(binding);
        assert!(missing.validate().is_err());

        let mut changed = template;
        changed.definition.name = "changed-topology-policy".to_string();
        assert_ne!(
            identity.definition_id,
            changed
                .definition_identity("bcode.example")
                .expect("changed identity")
                .definition_id
        );
    }

    #[test]
    fn registry_exposes_only_loaded_plugin_templates() {
        let mut enabled = test_manifest("bcode.enabled");
        enabled.workflow_templates.push(workflow_template());
        let registry =
            PluginRegistry::from_manifests(BTreeMap::from([(enabled.id.clone(), enabled)]));
        let templates = registry.workflow_templates();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].0, "bcode.enabled");
        assert_eq!(templates[0].1.template_id, "example");
    }

    #[test]
    fn visual_adapter_candidates_preserve_default_precedence() {
        let mut producer = test_manifest("bcode.producer");
        producer
            .visual_adapters
            .push(PluginVisualAdapterDeclaration {
                id: "producer".to_owned(),
                schema: "test.visual".to_owned(),
                min_schema_version: Some(1),
                max_schema_version: Some(1),
                service_interface_id: bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_owned(),
                surfaces: vec!["tui".to_owned()],
                priority: 10,
                producer_default: true,
                render_mode: PluginVisualAdapterRenderMode::TranscriptBlock,
            });
        let mut custom = test_manifest("user.custom");
        custom.visual_adapters.push(PluginVisualAdapterDeclaration {
            id: "custom".to_owned(),
            schema: "test.visual".to_owned(),
            min_schema_version: Some(1),
            max_schema_version: Some(1),
            service_interface_id: bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_owned(),
            surfaces: vec!["tui".to_owned()],
            priority: 20,
            producer_default: false,
            render_mode: PluginVisualAdapterRenderMode::FullBlock,
        });
        let manifests =
            BTreeMap::from([(producer.id.clone(), producer), (custom.id.clone(), custom)]);

        let routes = select_visual_adapters(
            manifests.iter(),
            "test.visual",
            1,
            "tui",
            Some("bcode.producer"),
        );

        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].adapter_reference(), "user.custom/custom");
        assert_eq!(routes[1].adapter_reference(), "bcode.producer/producer");
    }

    #[test]
    fn visual_adapter_producer_default_breaks_equal_priority_tie() {
        let adapter = |id: &str, producer_default| PluginVisualAdapterDeclaration {
            id: id.to_owned(),
            schema: "test.visual".to_owned(),
            min_schema_version: Some(1),
            max_schema_version: Some(1),
            service_interface_id: bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_owned(),
            surfaces: vec!["tui".to_owned()],
            priority: 10,
            producer_default,
            render_mode: PluginVisualAdapterRenderMode::TranscriptBlock,
        };
        let mut producer = test_manifest("bcode.producer");
        producer.visual_adapters.push(adapter("producer", true));
        let mut other = test_manifest("user.other");
        other.visual_adapters.push(adapter("other", false));
        let manifests =
            BTreeMap::from([(producer.id.clone(), producer), (other.id.clone(), other)]);

        let routes = select_visual_adapters(
            manifests.iter(),
            "test.visual",
            1,
            "tui",
            Some("bcode.producer"),
        );

        assert_eq!(routes[0].adapter_reference(), "bcode.producer/producer");
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
                workflow_blocks: Vec::new(),
                invocation_operations: Vec::new(),
            }],
            tui_surfaces: Vec::new(),
            visual_adapters: Vec::new(),
            tool_presentations: Vec::new(),
            command_contributions: Vec::new(),
            workflow_templates: Vec::new(),
            event_subscriptions: Vec::new(),
            concurrency: PluginConcurrencyConfig::Exclusive,
            runtime: PluginRuntime::Native(NativePluginRuntime {
                abi_version: CURRENT_PLUGIN_ABI_VERSION,
                library: PathBuf::from("test"),
                manifest_symbol: DEFAULT_NATIVE_MANIFEST_SYMBOL.to_string(),
                activate_symbol: DEFAULT_NATIVE_ACTIVATE_SYMBOL.to_string(),
                deactivate_symbol: DEFAULT_NATIVE_DEACTIVATE_SYMBOL.to_string(),
                streaming_service_symbol: DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL.to_string(),
                register_auth_providers_symbol: DEFAULT_NATIVE_REGISTER_AUTH_PROVIDERS_SYMBOL
                    .to_string(),
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
