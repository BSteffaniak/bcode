//! Native installation registrations; credentials remain native-owned.
use crate::HarnessInstance;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Reviewed complete native installation, never an authentication readiness assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    /// Exact installation/account identity.
    pub instance: HarnessInstance,
    /// Native-plugin-owned, non-secret preparation settings.
    pub adapter_settings: serde_json::Value,
}
/// Bounded local state snapshot with optimistic concurrency identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationSnapshot {
    /// Digest of the exact current state representation.
    pub revision: String,
    /// Locally registered installations.
    pub registrations: BTreeMap<String, Registration>,
}
/// Reviewed state mutation; does not grant native execution authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationCommit {
    /// Previously inspected snapshot revision.
    pub expected_revision: String,
    /// Complete installation to add.
    pub registration: Registration,
}
/// Host-admitted native adapter declarations for plugin-owned resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationResolveRequest {
    /// Enabled adapter declarations, not credentials.
    pub adapter_configurations: BTreeMap<String, serde_json::Value>,
}
/// Effective registration projection; state never re-enables disabled declarations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationResolution {
    /// Local snapshot identity.
    pub revision: String,
    /// Complete generic and adapter configuration projections.
    pub configurations: BTreeMap<String, serde_json::Value>,
    /// Instance source: configured or local.
    pub origins: BTreeMap<String, String>,
    /// Differing local IDs shadowed by declarations.
    pub shadowed: Vec<String>,
}
