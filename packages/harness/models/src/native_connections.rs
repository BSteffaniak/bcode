//! Native-account consent and publication. No credential material or vendor settings cross this boundary.
#[cfg(test)]
mod tests;
use crate::{HarnessAuthCustody, HarnessContractError, HarnessInstance, id, wire};
use serde::{Deserialize, Serialize};

/// Plugin-owned passive discovery and consent-gated preparation service.
pub const NATIVE_PREPARATION_INTERFACE: &str = "bcode.harness.native-connection/v1";
/// Host application service for consent and preparation, not a direct native launch service.
pub const NATIVE_CONNECTION_APPLICATION_INTERFACE: &str = "bcode.harness.native-connections/v1";
/// Host-owned read-only publication lookup, available only through an admitted bridge.
pub const NATIVE_CONNECTION_LOOKUP_INTERFACE: &str = "bcode.harness.native-connections-host/v1";
/// Maximum records/candidates in this bounded initial contract.
pub const MAX_NATIVE_CONNECTIONS: usize = 128;
/// Maximum encoded document at this boundary.
pub const MAX_NATIVE_CONNECTION_BYTES: usize = 256 * 1024;

/// Passive installation identity. Discovery is not permission or compatibility evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCandidate {
    /// Plugin-defined stable binding, invalidated by installation/namespace drift.
    pub candidate_id: String,
    /// Owning enabled native harness plugin.
    pub plugin_id: String,
    /// Canonical absolute executable path.
    pub executable: String,
    /// Canonical absolute native credential/config namespace.
    pub config_namespace: String,
    /// Plugin-defined exact installation provenance, not merely a version string.
    pub installation_id: String,
}
impl NativeCandidate {
    /// Validate portable bounds; plugins still verify filesystem provenance.
    /// # Errors
    /// Rejects malformed identities or non-absolute/control-bearing paths.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        for value in [&self.candidate_id, &self.plugin_id, &self.installation_id] {
            id(value)?;
        }
        for path in [&self.executable, &self.config_namespace] {
            if path.len() > 4096
                || path.chars().any(char::is_control)
                || !std::path::Path::new(path).is_absolute()
            {
                return Err(HarnessContractError::InvalidIdentity);
            }
        }
        Ok(())
    }
}

/// Plugin-produced preparation, checked against its supported protocol/isolation profiles at use.
/// Evidence identifiers reference plugin-owned compatibility evidence, never caller attestations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePreparedConnection {
    /// Exact consented candidate.
    pub candidate: NativeCandidate,
    /// Existing managed execution identity; credentials remain native-managed.
    pub instance: HarnessInstance,
    /// Exact implemented native protocol profile.
    pub protocol_profile: String,
    /// Plugin-owned evidence covering this installation and protocol.
    pub compatibility_evidence: String,
    /// Plugin-owned settings/environment isolation profile.
    pub isolation_profile: String,
}
impl NativePreparedConnection {
    /// Validate correlation, custody and bounds, not empirical compatibility.
    /// # Errors
    /// Rejects foreign instance identity, non-native custody or malformed profiles.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        self.candidate.validate()?;
        self.instance.validate()?;
        for value in [
            &self.protocol_profile,
            &self.compatibility_evidence,
            &self.isolation_profile,
        ] {
            id(value)?;
        }
        if self.instance.plugin_id != self.candidate.plugin_id
            || self.instance.executable != self.candidate.executable
            || self.instance.config_namespace != self.candidate.config_namespace
            || self.instance.auth.credential_namespace != self.candidate.config_namespace
            || self.instance.auth.custody != HarnessAuthCustody::NativeManaged
        {
            return Err(HarnessContractError::InvalidShape);
        }
        wire(self, MAX_NATIVE_CONNECTION_BYTES)
    }
}

/// Durable broad-local consent and optional publication. No app-scoped grants until caller identity exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConnectionRecord {
    /// Exact approved candidate; its ID also identifies the connection.
    pub candidate: NativeCandidate,
    /// Monotonic per-connection revision, including revocation/reapproval.
    pub revision: u64,
    /// Explicit all-local-Bcode approval; false is a retained revocation tombstone.
    pub approved: bool,
    /// Approval timestamp, not authentication freshness.
    pub approved_at_unix: u64,
    /// Published only after plugin preparation and consent revision revalidation.
    pub prepared: Option<NativePreparedConnection>,
}
impl NativeConnectionRecord {
    /// Validate durable shape without touching native files.
    /// # Errors
    /// Rejects inconsistent publication, invalid candidate or zero revision.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        self.candidate.validate()?;
        if self.revision == 0 {
            return Err(HarnessContractError::InvalidShape);
        }
        if let Some(prepared) = &self.prepared {
            prepared.validate()?;
            if !self.approved || prepared.candidate != self.candidate {
                return Err(HarnessContractError::InvalidShape);
            }
        }
        Ok(())
    }
}

/// Sanitized failure; never contains vendor output or credential values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeConnectionError {
    /// Malformed request.
    InvalidRequest,
    /// Missing/disabled plugin, source or host lookup.
    Unavailable,
    /// No current consent.
    NotApproved,
    /// Revision, installation or retry identity conflict.
    Conflict,
    /// Unsupported installed protocol or missing real compatibility evidence.
    CompatibilityUnavailable,
    /// Corrupt, future or unavailable canonical user state; preserve it.
    StateUnavailable,
    /// Document or record count exceeds the declared budget.
    BoundsExceeded,
}

/// Preparation is called only after durable consent and ordinary execution authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePrepareRequest {
    /// Must equal one.
    pub schema_version: u16,
    /// Correlation identity retained by the application.
    pub request_id: String,
    /// Exact approved candidate, revalidated by the plugin before probes.
    pub candidate: NativeCandidate,
}
impl NativePrepareRequest {
    /// Validate before any native process or status probe.
    /// # Errors
    /// Rejects unsupported schemas, malformed correlation or invalid candidate provenance shape.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.schema_version != 1 {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.request_id)?;
        self.candidate.validate()?;
        wire(self, MAX_NATIVE_CONNECTION_BYTES)
    }
}

/// Plugin service reply for `discover` (request `{}`) or `prepare`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativePreparationResponse {
    /// Passive candidates; not evidence of login or support.
    Candidates { candidates: Vec<NativeCandidate> },
    /// Correlated successful preparation.
    Prepared {
        request_id: String,
        connection: Box<NativePreparedConnection>,
    },
    /// No publication; no fallback may be inferred.
    Failed { error: NativeConnectionError },
}
/// Frontend-independent application operations. Mutation IDs must not change on retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeConnectionRequest {
    /// Query one enabled plugin without launching native processes.
    Discover { plugin_id: String },
    /// Explicit broad-local consent; expected revision zero means absent.
    Approve {
        candidate: NativeCandidate,
        expected_revision: u64,
        mutation_id: String,
    },
    /// Consent-gated compatibility/status preparation and publication, not generation.
    Prepare {
        candidate_id: String,
        expected_revision: u64,
        mutation_id: String,
    },
    /// Bounded non-mutating user-state read.
    List {},
    /// Block future use, without cancelling work or logging out the native application.
    Revoke {
        candidate_id: String,
        expected_revision: u64,
        mutation_id: String,
    },
}
impl NativeConnectionRequest {
    /// Validate before discovery, probes or durable mutations.
    /// # Errors
    /// Rejects malformed correlation/selection identities and oversized requests.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        match self {
            Self::Discover { plugin_id } => id(plugin_id)?,
            Self::Approve {
                candidate,
                mutation_id,
                ..
            } => {
                candidate.validate()?;
                id(mutation_id)?;
            }
            Self::Prepare {
                candidate_id,
                mutation_id,
                ..
            }
            | Self::Revoke {
                candidate_id,
                mutation_id,
                ..
            } => {
                id(candidate_id)?;
                id(mutation_id)?;
            }
            Self::List {} => {}
        }
        wire(self, MAX_NATIVE_CONNECTION_BYTES)
    }
}

/// Bounded application response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeConnectionResponse {
    /// Passive discovery result.
    Candidates { candidates: Vec<NativeCandidate> },
    /// Current durable records, including revoked entries.
    Records {
        records: Vec<NativeConnectionRecord>,
    },
    /// Exact mutation result.
    Record { record: Box<NativeConnectionRecord> },
    /// Sanitized failure.
    Failed { error: NativeConnectionError },
}
/// Read-only host bridge request. Host authenticates the invoking plugin separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeConnectionLookupRequest {
    /// Only the generic catalog plugin may enumerate published entries.
    Catalog {},
    /// A native plugin may resolve only its own exact instance.
    Resolve { instance_id: String },
}
/// Read-only lookup reply; records must be approved and prepared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConnectionLookupResponse {
    /// Published records in this state location, never caller-supplied configs.
    pub records: Vec<NativeConnectionRecord>,
}
