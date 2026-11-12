//! Plugin-owned, non-secret configured-instance discovery. Preparation grants no execution authority.
//!
//! Configuration is `{ enabled, default_instance?, instances: { reference: { enabled, instance } } }`.
//! Each strict `instance` contains the portable identity fields (`instance_id`, `plugin_id`,
//! `executable`, `config_namespace`, `auth`); `auth` contains only provider/profile/namespace/custody
//! references. Keys must equal instance IDs. Defaults must name enabled entries. Missing settings
//! are not defaults. Paths and native plugin availability remain host/native admission concerns.
//! The host must register this service only for the enabled generic plugin and must re-admit the
//! returned instance through the managed application boundary before any execution or auth effect.

mod registrations;

use std::collections::BTreeMap;

use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody, HarnessInstance};
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};

/// Independently versioned configured-instance service; not the native execution interface.
pub const INTERFACE_ID: &str = "bcode.harness.configuration/v1";
/// Enumerate enabled references and the explicitly configured default.
pub const CATALOG_OPERATION: &str = "catalog";
/// Resolve exactly one explicit reference or explicitly request the configured default.
pub const PREPARE_OPERATION: &str = "prepare";
/// Preview configuration for one explicitly selected native-owned candidate; never writes it.
pub const PREPARE_SETUP_OPERATION: &str = "prepare_setup";
const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_INSTANCES: usize = 128;
const MAX_REQUEST_BYTES: usize = 4096;

/// An absent reference never implicitly chooses a default or the first catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstanceSelection {
    /// Explicit consent to resolve the configured default; fails if none exists.
    Default {},
    /// Exact configured instance reference.
    Instance {
        /// Configuration map key, required to equal the retained instance identity.
        instance_id: String,
    },
}

/// Resolve one instance without inspecting credentials, native processes, or model providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    /// Explicit reference selection.
    pub selection: InstanceSelection,
}

/// Validated non-secret configuration, not proof of plugin availability or auth readiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedInstance {
    /// Original selection, retained so callers can distinguish default from exact reference.
    pub selection: InstanceSelection,
    /// Exact resolved configuration. Native admission must independently verify this instance.
    pub instance: HarnessInstance,
}

/// Bounded catalog of enabled instances. No implicit recommendation is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceCatalog {
    /// Explicit default reference, if configured.
    pub default_instance: Option<String>,
    /// Enabled entries in stable reference order.
    pub instances: BTreeMap<String, HarnessInstance>,
}

/// Explicit setup request. The candidate must come from native-owned discovery or user input.
/// No executable, account, configuration namespace, or default is inferred here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareSetupRequest {
    /// Exact nonsecret instance selected by the user.
    #[serde(deserialize_with = "deserialize_instance")]
    pub instance: HarnessInstance,
    /// Explicit consent to replace the configured default reference.
    pub make_default: bool,
}

/// Explicit candidate-backed setup, retaining discovery identity for commit-time revalidation.
/// The native adapter must supply `instance`; the generic plugin never invents auth references.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareCandidateSetupRequest {
    /// Exact selected native-owned observation; not authority to run its executable.
    pub candidate: bcode_harness_models::HarnessInstallationCandidate,
    /// Exact adapter-produced instance binding and explicit default choice.
    pub setup: PrepareSetupRequest,
}

impl PrepareCandidateSetupRequest {
    /// Verify candidate membership in fresh, exactly correlated native discovery before commit.
    ///
    /// Call again immediately before applying reviewed settings; do not silently substitute a
    /// new candidate when identity, namespace, custody, readiness, or executable metadata changes.
    /// This validates observations, not filesystem freshness or dispatch authorization itself.
    ///
    /// # Errors
    /// Rejects mismatched correlation, changed candidates, failed discovery, or binding mismatch.
    pub fn revalidate(
        &self,
        request: &bcode_harness_models::HarnessCandidatesRequest,
        response: &bcode_harness_models::HarnessCandidatesResponse,
    ) -> Result<(), &'static str> {
        response
            .validate_for(request, &self.candidate.plugin_id)
            .map_err(|_| "invalid candidate discovery correlation")?;
        let bcode_harness_models::HarnessCandidatesResponseKind::Candidates { candidates, .. } =
            &response.kind
        else {
            return Err("candidate discovery unavailable");
        };
        if !candidates.contains(&self.candidate) {
            return Err("selected candidate changed; review setup again");
        }
        self.validate()
    }

    /// Check exact instance/candidate agreement without interpreting native credential behavior.
    ///
    /// # Errors
    /// Rejects invalid observations, unsupported readiness, or inconsistent instance bindings.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.candidate
            .validate()
            .map_err(|_| "invalid setup candidate")?;
        self.setup
            .instance
            .validate()
            .map_err(|_| "invalid setup instance")?;
        let instance = &self.setup.instance;
        if instance.plugin_id != self.candidate.plugin_id
            || instance.executable != self.candidate.executable.invocation()
            || instance.config_namespace != self.candidate.config_namespace
            || instance.auth.custody != self.candidate.custody
            || !matches!(
                self.candidate.readiness,
                bcode_harness_models::HarnessCandidateReadiness::ExistingNativeLogin
                    | bcode_harness_models::HarnessCandidateReadiness::LoginRequired
            )
        {
            return Err("candidate and instance binding mismatch");
        }
        Ok(())
    }
}

/// Native-correlated setup input accepted by the existing `prepare_setup` operation.
/// Adapter settings are proposals only and retain native-plugin ownership.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareNativeSetupRequest {
    /// Exact native preparation request, including selected candidate and instance ID.
    pub request: bcode_harness_models::HarnessPrepareCandidateRequest,
    /// Correlated response from the selected native plugin's `prepare_candidate` operation.
    pub response: bcode_harness_models::HarnessPrepareCandidateResponse,
    /// Existing complete native plugin settings (nonsecret configuration only).
    pub adapter_configuration: serde_json::Value,
    /// Explicit consent to make this instance the generic catalog default.
    pub make_default: bool,
}

/// Both plugin proposals, suitable for one policy-fenced atomic config edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedNativeSetup {
    /// Native proposal retaining exact candidate, instance binding, and adapter entry.
    pub native: bcode_harness_models::HarnessPreparedCandidate,
    /// Full native plugin settings with only the selected instance inserted.
    pub adapter_configuration: serde_json::Value,
    /// Full generic catalog settings and exact prepared instance.
    pub harness: PreparedInstanceSetup,
}

impl PreparedNativeSetup {
    /// Require a freshly prepared response to equal the reviewed proposal before commit.
    ///
    /// # Errors
    /// Rejects native correlation failure, unavailable preparation, or any changed proposal.
    pub fn revalidate(
        &self,
        request: &bcode_harness_models::HarnessPrepareCandidateRequest,
        response: &bcode_harness_models::HarnessPrepareCandidateResponse,
    ) -> Result<(), &'static str> {
        response
            .validate_for(request, &self.native.instance.plugin_id)
            .map_err(|_| "invalid native setup correlation")?;
        match &response.kind {
            bcode_harness_models::HarnessPrepareCandidateResponseKind::Prepared(prepared)
                if prepared.as_ref() == &self.native =>
            {
                Ok(())
            }
            _ => Err("native setup changed; prepare and review again"),
        }
    }
}

/// Reviewable settings only, not a write receipt or evidence of native availability/readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedInstanceSetup {
    /// Exact instance preparation, suitable for existing managed admission after configuration.
    pub prepared: PreparedInstance,
    /// Full proposed nonsecret generic-plugin configuration; unrelated instances are retained.
    pub configuration: serde_json::Value,
}

fn deserialize_instance<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<HarnessInstance, D::Error> {
    InstanceDefinition::deserialize(deserializer).map(InstanceDefinition::into_instance)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogRequest {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    enabled: bool,
    #[serde(default)]
    default_instance: Option<String>,
    instances: BTreeMap<String, InstanceSettings>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceSettings {
    enabled: bool,
    instance: InstanceDefinition,
}

// Strict settings DTOs intentionally reject unknown fields, including inline credentials.
// Portable runtime contracts remain owned by harness models.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceDefinition {
    instance_id: String,
    plugin_id: String,
    executable: String,
    config_namespace: String,
    auth: AuthDefinition,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthDefinition {
    provider_id: String,
    profile: String,
    credential_namespace: String,
    custody: HarnessAuthCustody,
}

impl InstanceDefinition {
    fn into_instance(self) -> HarnessInstance {
        HarnessInstance {
            instance_id: self.instance_id,
            plugin_id: self.plugin_id,
            executable: self.executable,
            config_namespace: self.config_namespace,
            auth: HarnessAuthBinding {
                provider_id: self.auth.provider_id,
                profile: self.auth.profile,
                credential_namespace: self.auth.credential_namespace,
                custody: self.auth.custody,
            },
        }
    }
}

fn catalog(context: &NativeServiceContext) -> Result<InstanceCatalog, ServiceResponse> {
    let unconfigured = context.config.config.is_null()
        || context
            .config
            .config
            .as_object()
            .is_some_and(serde_json::Map::is_empty);
    let mut catalog = if unconfigured {
        InstanceCatalog {
            default_instance: None,
            instances: BTreeMap::new(),
        }
    } else {
        catalog_from_config(&context.config.config)?
    };
    merge_published(&mut catalog.instances, published(context)?)?;
    if unconfigured && catalog.instances.is_empty() {
        return Err(ServiceResponse::error(
            "configuration_disabled",
            "managed instance configuration is disabled",
        ));
    }
    Ok(catalog)
}

/// Resolve the enabled instance catalog from plugin configuration without native inspection.
pub(crate) fn catalog_value(config: &serde_json::Value) -> Result<InstanceCatalog, ()> {
    catalog_from_config(config).map_err(drop)
}

fn catalog_from_config(config: &serde_json::Value) -> Result<InstanceCatalog, ServiceResponse> {
    // Check the encoded budget without allocating an unbounded serialization buffer.
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("configuration budget exceeded"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let invalid = || {
        ServiceResponse::error(
            "invalid_configuration",
            "invalid managed instance configuration",
        )
    };
    let disabled = || {
        ServiceResponse::error(
            "configuration_disabled",
            "managed instance configuration is disabled",
        )
    };
    // An absent section means the feature is unconfigured, not a malformed configuration.
    let raw = config;
    if raw.is_null() || raw.as_object().is_some_and(serde_json::Map::is_empty) {
        return Err(disabled());
    }
    serde_json::to_writer(Budget(MAX_CONFIG_BYTES), raw).map_err(|_| invalid())?;
    let settings: Settings = serde_json::from_value(config.clone()).map_err(|_| invalid())?;
    if !settings.enabled {
        return Err(disabled());
    }
    if settings.instances.len() > MAX_INSTANCES {
        return Err(invalid());
    }
    let mut instances = BTreeMap::new();
    for (reference, entry) in settings.instances {
        let instance = entry.instance.into_instance();
        if reference != instance.instance_id || instance.validate().is_err() {
            return Err(invalid());
        }
        if entry.enabled {
            instances.insert(reference, instance);
        }
    }
    if settings
        .default_instance
        .as_ref()
        .is_some_and(|reference| !instances.contains_key(reference))
    {
        return Err(invalid());
    }
    Ok(InstanceCatalog {
        default_instance: settings.default_instance,
        instances,
    })
}

fn published(
    context: &NativeServiceContext,
) -> Result<Vec<bcode_harness_models::native_connections::NativeConnectionRecord>, ServiceResponse>
{
    use bcode_harness_models::native_connections::{
        NATIVE_CONNECTION_LOOKUP_INTERFACE, NativeConnectionLookupRequest,
        NativeConnectionLookupResponse,
    };
    if !context.bridge.is_available() {
        return Ok(Vec::new());
    }
    let unavailable = || {
        ServiceResponse::error(
            "native_connections_unavailable",
            "Canonical native connections are unavailable",
        )
    };
    let reply = context
        .bridge
        .request(&ServiceBridgeRequest::InvokeService(
            bcode_tool::ToolInvocationServiceRequest {
                invocation_id: "native-catalog".into(),
                request_id: "native-catalog".into(),
                route_id: None,
                interface_id: NATIVE_CONNECTION_LOOKUP_INTERFACE.into(),
                operation: "lookup".into(),
                payload: serde_json::to_value(NativeConnectionLookupRequest::Catalog {})
                    .map_err(|_| unavailable())?,
            },
        ))
        .map_err(|_| unavailable())?;
    match reply {
        ServiceBridgeResponse::Service(
            bcode_tool::ToolInvocationServiceResolution::Responded { payload },
        ) => {
            let response: NativeConnectionLookupResponse =
                serde_json::from_value(payload).map_err(|_| unavailable())?;
            Ok(response.records)
        }
        // Older bridges explicitly lacking this optional capability preserve configured catalogs.
        ServiceBridgeResponse::Service(
            bcode_tool::ToolInvocationServiceResolution::Unsupported,
        ) => Ok(Vec::new()),
        _ => Err(unavailable()),
    }
}

fn merge_published(
    instances: &mut BTreeMap<String, HarnessInstance>,
    records: Vec<bcode_harness_models::native_connections::NativeConnectionRecord>,
) -> Result<(), ServiceResponse> {
    let invalid = || {
        ServiceResponse::error(
            "invalid_native_connections",
            "Invalid or conflicting canonical native connections",
        )
    };
    if records.len() > MAX_INSTANCES {
        return Err(invalid());
    }
    for record in records {
        record.validate().map_err(|_| invalid())?;
        if !record.approved {
            return Err(invalid());
        }
        let prepared = record.prepared.ok_or_else(invalid)?;
        if instances
            .insert(prepared.instance.instance_id.clone(), prepared.instance)
            .is_some()
        {
            return Err(invalid());
        }
    }
    if instances.len() > MAX_INSTANCES {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    if context.plugin_id != crate::PLUGIN_ID || context.request.interface_id != INTERFACE_ID {
        return ServiceResponse::error("invalid_context", "invalid managed configuration context");
    }
    let request_budget = if matches!(
        context.request.operation.as_str(),
        PREPARE_SETUP_OPERATION | "registrations_resolve" | "registrations_commit"
    ) {
        MAX_CONFIG_BYTES
    } else {
        MAX_REQUEST_BYTES
    };
    if context.request.payload.len() > request_budget {
        return ServiceResponse::error("invalid_request", "invalid managed configuration request");
    }
    if context.request.operation.starts_with("registrations_") {
        return registrations::invoke(context);
    }
    match context.request.operation.as_str() {
        CATALOG_OPERATION => {
            if context.request.payload_json::<CatalogRequest>().is_err() {
                return ServiceResponse::error(
                    "invalid_request",
                    "invalid instance catalog request",
                );
            }
            match catalog(context) {
                Ok(value) => crate::json_response(&value),
                Err(error) => error,
            }
        }
        PREPARE_SETUP_OPERATION => prepare_setup(context),
        PREPARE_OPERATION => {
            let Ok(request) = context.request.payload_json::<PrepareRequest>() else {
                return ServiceResponse::error("invalid_request", "invalid instance selection");
            };
            let catalog = match catalog(context) {
                Ok(value) => value,
                Err(error) => return error,
            };
            let reference = match &request.selection {
                InstanceSelection::Default {} => catalog.default_instance.as_deref(),
                InstanceSelection::Instance { instance_id } => Some(instance_id.as_str()),
            };
            let Some(instance) = reference.and_then(|id| catalog.instances.get(id)) else {
                return ServiceResponse::error(
                    "instance_unavailable",
                    "selected managed instance is unavailable",
                );
            };
            crate::json_response(&PreparedInstance {
                selection: request.selection,
                instance: instance.clone(),
            })
        }
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported managed configuration operation",
        ),
    }
}

// Native portable contracts allow extension fields elsewhere; configuration preparation is a
// strict reviewed boundary. Opaque adapter objects round-trip unchanged and stay native-owned.
fn contains_unknown_fields(input: &serde_json::Value, typed: &serde_json::Value) -> bool {
    match (input, typed) {
        (serde_json::Value::Object(input), serde_json::Value::Object(typed)) => {
            input.iter().any(|(key, value)| {
                typed
                    .get(key)
                    .is_none_or(|known| contains_unknown_fields(value, known))
            })
        }
        (serde_json::Value::Array(input), serde_json::Value::Array(typed)) => {
            input.len() != typed.len()
                || input
                    .iter()
                    .zip(typed)
                    .any(|(value, known)| contains_unknown_fields(value, known))
        }
        _ => false,
    }
}

fn prepare_setup(context: &NativeServiceContext) -> ServiceResponse {
    // A native proposal must never fall back to an older setup shape when malformed.
    let Ok(value) = context.request.payload_json::<serde_json::Value>() else {
        return ServiceResponse::error("invalid_request", "invalid managed setup request");
    };
    if value.get("request").is_some() || value.get("response").is_some() {
        let Ok(request) = context.request.payload_json::<PrepareNativeSetupRequest>() else {
            return ServiceResponse::error("invalid_request", "invalid native setup proposal");
        };
        let Ok(typed) = serde_json::to_value(&request) else {
            return ServiceResponse::error("invalid_request", "invalid native setup proposal");
        };
        if contains_unknown_fields(&value, &typed) {
            return ServiceResponse::error("invalid_request", "unknown native setup fields");
        }
        return prepare_native_setup(context, request);
    }
    let request = if let Ok(request) = context.request.payload_json::<PrepareSetupRequest>() {
        request
    } else if let Ok(request) = context
        .request
        .payload_json::<PrepareCandidateSetupRequest>()
    {
        if request.validate().is_err() {
            return ServiceResponse::error(
                "invalid_request",
                "candidate and instance binding mismatch",
            );
        }
        request.setup
    } else {
        return ServiceResponse::error("invalid_request", "invalid managed setup request");
    };
    prepare_instance_setup(context, &request)
}

fn prepare_instance_setup(
    context: &NativeServiceContext,
    request: &PrepareSetupRequest,
) -> ServiceResponse {
    if request.instance.validate().is_err() {
        return ServiceResponse::error("invalid_request", "invalid managed instance reference");
    }
    // Only absent/empty settings permit first setup. Disabled or malformed existing settings
    // cannot be silently repaired or re-enabled by selecting a candidate.
    let mut configuration = if context.config.config.is_null()
        || context
            .config
            .config
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    {
        serde_json::json!({"enabled": true, "instances": {}})
    } else {
        if let Err(error) = catalog(context) {
            return error;
        }
        context.config.config.clone()
    };
    let reference = request.instance.instance_id.clone();
    let Some(instances) = configuration
        .get_mut("instances")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return ServiceResponse::error("invalid_configuration", "invalid managed configuration");
    };
    // Setup cannot overwrite/re-enable an existing instance. Reusing an identical enabled
    // entry is harmless; changed account/custody/namespace needs a separate reviewed editor.
    let entry = serde_json::json!({"enabled": true, "instance": request.instance});
    if instances
        .get(&reference)
        .is_some_and(|existing| existing != &entry)
    {
        return ServiceResponse::error(
            "instance_conflict",
            "instance reference already configured",
        );
    }
    instances.insert(reference.clone(), entry);
    if request.make_default {
        configuration["default_instance"] = serde_json::Value::String(reference.clone());
    }
    // Reuse the catalog validator and budgets, not a second resolver or native readiness probe.
    let catalog = match catalog_from_config(&configuration) {
        Ok(catalog) => catalog,
        Err(error) => return error,
    };
    let Some(instance) = catalog.instances.get(&reference) else {
        return ServiceResponse::error(
            "instance_unavailable",
            "selected managed instance unavailable",
        );
    };
    crate::json_response(&PreparedInstanceSetup {
        prepared: PreparedInstance {
            selection: InstanceSelection::Instance {
                instance_id: reference,
            },
            instance: instance.clone(),
        },
        configuration,
    })
}

fn prepare_native_setup(
    context: &NativeServiceContext,
    request: PrepareNativeSetupRequest,
) -> ServiceResponse {
    let invalid = || ServiceResponse::error("invalid_request", "invalid native setup proposal");
    if request
        .response
        .validate_for(&request.request, &request.request.candidate.plugin_id)
        .is_err()
    {
        return invalid();
    }
    let bcode_harness_models::HarnessPrepareCandidateResponseKind::Prepared(native) =
        request.response.kind
    else {
        return ServiceResponse::error(
            "candidate_unavailable",
            "native setup preparation unavailable",
        );
    };
    if native.instance.plugin_id == crate::PLUGIN_ID {
        return invalid();
    }
    let mut adapter = request.adapter_configuration;
    if adapter.is_null() {
        adapter = serde_json::json!({});
    }
    let Some(table) = adapter.as_object_mut() else {
        return invalid();
    };
    if table
        .get("enabled")
        .is_some_and(|value| value != &serde_json::Value::Bool(true))
    {
        return ServiceResponse::error(
            "configuration_disabled",
            "native configuration is disabled",
        );
    }
    let instances = table
        .entry("instances")
        .or_insert_with(|| serde_json::json!({}));
    let Some(instances) = instances.as_object_mut() else {
        return invalid();
    };
    if instances.len() >= MAX_INSTANCES && !instances.contains_key(&native.instance.instance_id) {
        return invalid();
    }
    if instances
        .get(&native.instance.instance_id)
        .is_some_and(|value| value != &native.adapter_settings)
    {
        return ServiceResponse::error(
            "instance_conflict",
            "native instance reference already configured",
        );
    }
    instances.insert(
        native.instance.instance_id.clone(),
        native.adapter_settings.clone(),
    );
    if serde_json::to_vec(&adapter).map_or(true, |bytes| bytes.len() > MAX_CONFIG_BYTES) {
        return invalid();
    }
    let response = prepare_instance_setup(
        context,
        &PrepareSetupRequest {
            instance: native.instance.clone(),
            make_default: request.make_default,
        },
    );
    if response.error.is_some() {
        return response;
    }
    let Ok(harness) = response.payload_json::<PreparedInstanceSetup>() else {
        return invalid();
    };
    crate::json_response(&PreparedNativeSetup {
        native: *native,
        adapter_configuration: adapter,
        harness,
    })
}

#[cfg(test)]
mod tests;
