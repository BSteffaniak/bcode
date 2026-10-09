//! Secret-safe model support observations. Provider integrations classify outcomes.

use crate::ModelSupportTarget;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Independently versioned evidence document contract.
pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;

/// Exact local serving identity.
///
/// Fingerprints are lowercase SHA-256 hex digests, computed by the caller with
/// distinct domain separators; never raw endpoints, account identifiers,
/// access tokens, or configuration/profile names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceScope {
    /// Provider, authentication mode, and API surface.
    pub target: ModelSupportTarget,
    /// Digest of normalized serving endpoint (including routing path).
    pub endpoint_fingerprint: String,
    /// Digest of stable authenticated account identity, not rotating credentials.
    pub account_fingerprint: String,
}

/// Only capabilities actually exercised by a completed request may be asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCapability {
    /// Completed text generation.
    TextGeneration,
    /// Completed tool invocation round trip.
    Tools,
    /// Accepted image input with a completed response.
    ImageInput,
    /// Provider-reported cache usage was observed.
    PromptCache,
    /// Structured output was successfully exercised.
    StructuredOutput,
}

/// Normalized outcome, never a provider error string or HTTP status alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOutcome {
    /// Request completed successfully.
    Success,
    /// Definitive provider classification: unavailable on this serving surface.
    ProviderSurfaceUnsupported,
    /// Account authorization/entitlement denial; not a global model fact.
    AccountDenied,
}

/// One normalized observation. No payload/error/prompt/credential fields exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSupportEvidence {
    /// Exact serving scope.
    pub scope: EvidenceScope,
    /// Exact provider-native model id (not an alias).
    pub model_id: String,
    /// Normalized provider outcome.
    pub outcome: EvidenceOutcome,
    /// Capabilities exercised on success or definitively rejected on failure.
    pub capabilities: BTreeSet<EvidenceCapability>,
    /// Request admission time; reconciliation orders attempts, not response arrival.
    pub attempt_started_at_ms: u64,
    /// Monotonic admission ordinal, breaking same-millisecond attempt ties.
    pub attempt_ordinal: u64,
    /// Unix epoch milliseconds of the observation.
    pub observed_at_ms: u64,
    /// Exclusive expiry in Unix epoch milliseconds.
    pub expires_at_ms: u64,
}

/// Versioned, bounded local evidence document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDocument {
    /// Must equal `EVIDENCE_SCHEMA_VERSION`.
    pub schema_version: u32,
    /// Retained observations, including newer superseding observations.
    pub observations: Vec<ModelSupportEvidence>,
}

/// Evidence-only result: absence is unverified, never unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceResolution {
    /// Exercised successfully in this account/surface scope.
    Supported,
    /// Definitively rejected on this provider surface.
    Unsupported,
    /// This account was denied.
    AccountDenied,
    /// No current definitive evidence.
    Unverified,
}
