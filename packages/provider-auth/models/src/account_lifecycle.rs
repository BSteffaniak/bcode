//! Portable, secret-free local account lifecycle contracts.

use serde::{Deserialize, Serialize};

/// Current portable local account lifecycle contract version.
pub const ACCOUNT_LIFECYCLE_SCHEMA_VERSION: u16 = 1;

/// Local account action. Remote revocation is a separate provider-owned operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountLifecycleAction {
    /// Delete owned local credentials, retaining profiles and membership.
    Logout,
    /// Also forget owned runtime metadata and operational state, never configuration.
    Remove,
}

/// Explicit local account selection; no default account or pool fallback is permitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountLifecycleRequest {
    /// Contract version; currently 1.
    pub schema_version: u16,
    /// Registered auth provider ID (not necessarily the plugin ID).
    pub provider_id: String,
    /// Explicit configuration-local profile name.
    pub profile: String,
    /// Local operation authorized by the caller.
    pub action: AccountLifecycleAction,
}

/// Side-effect-free preview. This is descriptive, not an execution capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLifecyclePreview {
    /// Validated selection.
    pub request: AccountLifecycleRequest,
    /// Owning plugin, bound from the registered contribution by the host.
    pub owner_plugin_id: String,
    /// Canonical credential IDs to delete; no values or storage paths are exposed.
    pub credential_ids: Vec<String>,
    /// Runtime pools whose owned membership will be removed.
    pub runtime_pools: Vec<String>,
    /// Whether declarative metadata remains after removal.
    pub retains_declarative_profile: bool,
}

/// Effect evidence. An uncertain publication must be reconciled, not blindly retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountLifecycleEffect {
    /// No mutation was attempted.
    NotAttempted,
    /// The requested postcondition was established (possibly already absent).
    Complete,
    /// Publication was attempted but its durable outcome could not be established.
    Uncertain,
}

/// Normalized local failure; never contains provider, vault, or filesystem error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountLifecycleError {
    /// Unsupported contract, contribution, selection, ownership, or credential mapping.
    InvalidSelection,
    /// Authoritative selection or metadata changed since planning.
    StalePlan,
    /// State could not be read or exclusively updated.
    StateUnavailable,
    /// Credential custody is unavailable, fenced, or requires maintenance.
    CredentialsUnavailable,
}

/// Independently reported effects; local success never claims remote revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLifecycleOutcome {
    /// Owned credential deletion.
    pub credentials: AccountLifecycleEffect,
    /// Owned runtime registrations, bindings, membership and pool preferences.
    pub runtime: AccountLifecycleEffect,
    /// Account labels, pauses, routing preferences, usage and owned pool observations.
    /// Disposable session credential-revision caches require coordinator invalidation.
    pub operational: AccountLifecycleEffect,
    /// Failure after the effects above, if any.
    pub error: Option<AccountLifecycleError>,
}
