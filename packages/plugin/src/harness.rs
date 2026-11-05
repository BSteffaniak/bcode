//! Host plumbing for explicitly selected managed harness service providers.
//!
//! No native executable is invoked by discovery. Multiple providers are expected;
//! routing always uses the admitted instance's canonical plugin owner.

use crate::{PluginHost, PluginManifest, ServiceResponse};
use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HarnessContractError, HarnessRequest, HarnessServiceResponse,
    MAX_HARNESS_ID_BYTES, MAX_HARNESS_OBSERVATIONS, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_models::harness::{
    HarnessContribution, HarnessDiscoveryPage, PluginOwnedHarnessContribution,
};

/// Secret-safe host routing failure. Native/plugin errors are never copied to diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessRoutingError {
    Contract(HarnessContractError),
    Unavailable,
    Ambiguous,
    Invocation,
    SensitiveTransportRequired,
}
impl std::fmt::Display for HarnessRoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "harness routing failure: {self:?}")
    }
}
impl std::error::Error for HarnessRoutingError {}
impl From<HarnessContractError> for HarnessRoutingError {
    fn from(value: HarnessContractError) -> Self {
        Self::Contract(value)
    }
}

/// Validate exactly one versioned harness service declaration for a manifest.
/// Existing manifest service metadata is the registration authority, not plugin payloads.
/// # Errors
/// Rejects missing, duplicate or incompatible interface declarations and invalid metadata.
pub fn harness_contribution(
    manifest: &PluginManifest,
) -> Result<PluginOwnedHarnessContribution, HarnessRoutingError> {
    let mut services = manifest
        .services
        .iter()
        .filter(|service| service.interface_id == HARNESS_INTERFACE_ID);
    let service = services.next().ok_or(HarnessRoutingError::Unavailable)?;
    if services.next().is_some() {
        return Err(HarnessRoutingError::Ambiguous);
    }
    if manifest.id.is_empty()
        || manifest.id.len() > MAX_HARNESS_ID_BYTES
        || manifest.id.trim() != manifest.id
        || manifest.id.chars().any(char::is_control)
    {
        return Err(HarnessContractError::InvalidIdentity.into());
    }
    let contribution = HarnessContribution {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        harness_id: manifest.id.clone(),
        display_name: service.name.clone().unwrap_or_else(|| manifest.id.clone()),
        service_interface_id: service.interface_id.clone(),
    };
    contribution.validate()?;
    Ok(PluginOwnedHarnessContribution {
        plugin_id: manifest.id.clone(),
        contribution,
    })
}

/// Decode bounded service output without reflecting plugin errors or native payloads.
/// # Errors
/// Rejects service failure, excessive/invalid JSON and mismatched response evidence.
pub fn decode_harness_response(
    response: &ServiceResponse,
    request: &HarnessRequest,
) -> Result<HarnessServiceResponse, HarnessRoutingError> {
    if response.error.is_some() {
        return Err(HarnessRoutingError::Invocation);
    }
    if response.payload.len() > MAX_HARNESS_WIRE_BYTES {
        return Err(HarnessContractError::BoundsExceeded.into());
    }
    let decoded: HarnessServiceResponse = serde_json::from_slice(&response.payload)
        .map_err(|_| HarnessRoutingError::Contract(HarnessContractError::InvalidShape))?;
    decoded.validate_for(request)?;
    Ok(decoded)
}

impl PluginHost {
    /// Discover one lexicographic bounded page of enabled, successfully loaded harness plugins.
    ///
    /// Only loaded manifest metadata is consulted. Disabled/unavailable plugins contribute
    /// nothing; that does not affect durable session-history readability.
    /// # Errors
    /// Rejects invalid page bounds/cursors and ambiguous or invalid declarations.
    pub fn harnesses(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<HarnessDiscoveryPage, HarnessRoutingError> {
        if limit == 0 || limit > MAX_HARNESS_OBSERVATIONS {
            return Err(HarnessContractError::BoundsExceeded.into());
        }
        if after.is_some_and(|cursor| {
            cursor.is_empty()
                || cursor.len() > MAX_HARNESS_ID_BYTES
                || cursor.chars().any(char::is_control)
        }) {
            return Err(HarnessContractError::InvalidIdentity.into());
        }
        // Bound retained results while scanning the already-loaded plugin set. Discovery
        // never opens native state, scans sessions or rebuilds an execution index.
        let mut page = std::collections::BTreeMap::new();
        for plugin in self.loaded_plugins() {
            let manifest = plugin.manifest();
            if after.is_some_and(|cursor| manifest.id.as_str() <= cursor)
                || !manifest
                    .services
                    .iter()
                    .any(|service| service.interface_id == HARNESS_INTERFACE_ID)
            {
                continue;
            }
            let contribution = harness_contribution(manifest)?;
            if page.insert(manifest.id.clone(), contribution).is_some() {
                return Err(HarnessRoutingError::Ambiguous);
            }
            if page.len() > limit + 1 {
                page.pop_last();
            }
        }
        let has_more = page.len() > limit;
        if has_more {
            page.pop_last();
        }
        let next_after = if has_more {
            page.last_key_value().map(|(key, _)| key.clone())
        } else {
            None
        };
        Ok(HarnessDiscoveryPage {
            harnesses: page.into_values().collect(),
            next_after,
        })
    }

    /// Route one admitted non-sensitive request to its exact enabled plugin owner.
    ///
    /// This is contract plumbing, not admission: callers must already hold verified
    /// execution ownership and canonical mutation intent. There is no unique-provider
    /// fallback and no retry. Login payloads require the auth coordinator's dedicated
    /// trace-exempt transport and are deliberately refused on this generic path.
    /// # Errors
    /// Rejects invalid payloads, absent/ambiguous services, sensitive operations,
    /// invocation failures and inconsistent response evidence.
    pub fn invoke_harness(
        &self,
        request: &HarnessRequest,
    ) -> Result<HarnessServiceResponse, HarnessRoutingError> {
        request.validate()?;
        if request.kind.operation().is_sensitive() {
            return Err(HarnessRoutingError::SensitiveTransportRequired);
        }
        let mut providers = self
            .loaded_plugins()
            .iter()
            .filter(|plugin| plugin.manifest().id == request.instance.plugin_id);
        let plugin = providers.next().ok_or(HarnessRoutingError::Unavailable)?;
        if providers.next().is_some() {
            return Err(HarnessRoutingError::Ambiguous);
        }
        harness_contribution(plugin.manifest())?;
        let payload = serde_json::to_vec(request)
            .map_err(|_| HarnessRoutingError::Contract(HarnessContractError::InvalidShape))?;
        let response = self
            .invoke_service(
                &request.instance.plugin_id,
                HARNESS_INTERFACE_ID,
                request.kind.operation().as_str(),
                payload,
            )
            .map_err(|_| HarnessRoutingError::Invocation)?;
        decode_harness_response(&response, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_uses_manifest_owner_and_rejects_duplicate_services() {
        let mut manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "canonical-plugin", "name": "Native", "version": "1.0.0",
            "services": [{ "interface_id": HARNESS_INTERFACE_ID, "name": "Native Harness" }],
            "runtime": { "type": "native", "abi_version": crate::CURRENT_PLUGIN_ABI_VERSION, "library": "native.so" }
        })).unwrap();
        let registered = harness_contribution(&manifest).unwrap();
        assert_eq!(registered.plugin_id, "canonical-plugin");
        assert_eq!(registered.contribution.harness_id, "canonical-plugin");
        manifest.services.push(manifest.services[0].clone());
        assert_eq!(
            harness_contribution(&manifest),
            Err(HarnessRoutingError::Ambiguous)
        );
        manifest.services.clear();
        assert_eq!(
            harness_contribution(&manifest),
            Err(HarnessRoutingError::Unavailable)
        );
    }
    #[test]
    fn discovery_bounds_are_checked_even_when_no_plugins_loaded() {
        let host = PluginHost::default();
        assert_eq!(host.harnesses(None, 1).unwrap().harnesses, Vec::new());
        assert_eq!(
            host.harnesses(None, 0),
            Err(HarnessRoutingError::Contract(
                HarnessContractError::BoundsExceeded
            ))
        );
        assert_eq!(
            host.harnesses(Some(""), 1),
            Err(HarnessRoutingError::Contract(
                HarnessContractError::InvalidIdentity
            ))
        );
    }
}
