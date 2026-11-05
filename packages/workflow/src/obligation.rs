//! Same-run receipt adoption contracts. These identities grant no execution authority.
use serde::{Deserialize, Serialize};

/// Admission policy carried by an owner-issued preparation descriptor.
/// It reserves scheduling behavior only; it grants no policy or execution authority.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPreparedAdmission {
    /// Ordinary canonical activation admission.
    #[default]
    Immediate,
    /// Only an explicit authority-fenced obligation adoption may prepare this activation.
    DeferredUntilAdopted,
}

impl WorkflowPreparedAdmission {
    /// Decode the optional descriptor extension, rejecting unknown admission policies.
    /// # Errors
    /// Returns a serialization error for unsupported or malformed policy values.
    pub fn from_descriptor(descriptor: &serde_json::Value) -> Result<Self, serde_json::Error> {
        descriptor
            .get("admission")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map(Option::unwrap_or_default)
    }
}

/// Exact canonical attempt participating in a responsibility transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowObligationAttempt {
    /// Canonical graph node.
    pub node_id: String,
    /// Canonical activation.
    pub activation_id: String,
    /// Durable attempt ordinal.
    pub attempt: u32,
    /// Exact dispatch identity.
    pub dispatch_identity: String,
}

/// Explicit host-approved same-run adoption request, pinned before any transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowObligationAdoption {
    /// Exact source dispatch identity, reused as the obligation's unique retry identity.
    pub obligation_id: String,
    /// One canonical run; cross-run transfer is not supported by this contract.
    pub run_id: String,
    /// Exact graph revision before adoption.
    pub expected_revision: u64,
    /// Ownership generation at approval. Fencing token is checked separately by the store.
    pub ownership_generation: u64,
    /// Receipt-backed source.
    pub source: WorkflowObligationAttempt,
    /// Explicit pending destination graph activation. Attempt must be the next admitted attempt.
    pub destination: WorkflowObligationAttempt,
    /// Canonical checksum of the original source receipt, before adoption metadata.
    pub source_receipt_sha256: String,
    /// Canonical subject identity, including session, generation and submission/attempt.
    pub evidence_subject: String,
    /// Exact canonical evidence revision, not a native turn identifier.
    pub evidence_revision: u64,
    /// Checksum of verified bounded canonical evidence at that revision.
    pub evidence_sha256: String,
    /// Complete expected active source lease identities; no partial transfer is permitted.
    pub resource_lease_ids: Vec<String>,
    /// Exact existing canonical mutation grant approving the destination input/operation.
    pub destination_grant_id: String,
    /// Canonical checksum of that grant's operation-specific scope.
    pub destination_grant_scope_sha256: String,
}

/// Durable acknowledgment: destination receipt publication and resource transfer committed together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowObligationAdoptionProof {
    /// Exact retained request, including original expected revision.
    pub request: WorkflowObligationAdoption,
    /// Graph revision atomically advanced by the adoption transaction.
    pub adopted_revision: u64,
    /// Canonical checksum of the destination owner receipt before adoption metadata.
    pub destination_receipt_sha256: String,
}
