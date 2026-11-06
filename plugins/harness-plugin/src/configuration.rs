//! Plugin-owned, non-secret configured-instance discovery. Preparation grants no execution authority.
//!
//! Configuration is `{ enabled, default_instance?, instances: { reference: { enabled, instance } } }`.
//! Each strict `instance` contains the portable identity fields (`instance_id`, `plugin_id`,
//! `executable`, `config_namespace`, `auth`); `auth` contains only provider/profile/namespace/custody
//! references. Keys must equal instance IDs. Defaults must name enabled entries. Missing settings
//! are not defaults. Paths and native plugin availability remain host/native admission concerns.
//! The host must register this service only for the enabled generic plugin and must re-admit the
//! returned instance through the managed application boundary before any execution or auth effect.

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
    let raw = &context.config.config;
    if raw.is_null() || raw.as_object().is_some_and(serde_json::Map::is_empty) {
        return Err(disabled());
    }
    serde_json::to_writer(Budget(MAX_CONFIG_BYTES), raw).map_err(|_| invalid())?;
    let settings: Settings = context.config().map_err(|_| invalid())?;
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

pub(crate) fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    if context.plugin_id != crate::PLUGIN_ID || context.request.interface_id != INTERFACE_ID {
        return ServiceResponse::error("invalid_context", "invalid managed configuration context");
    }
    if context.request.payload.len() > MAX_REQUEST_BYTES {
        return ServiceResponse::error("invalid_request", "invalid managed configuration request");
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

#[cfg(test)]
mod tests;
