//! Optional native installation discovery. Candidates are observations, not configuration writes.
use crate::{
    HARNESS_SCHEMA_VERSION, HarnessAuthCustody, HarnessContractError, HarnessFailure,
    MAX_HARNESS_WIRE_BYTES, id, text, wire,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Opt-in native-plugin discovery; the generic configuration plugin does not interpret vendors.
pub const HARNESS_CANDIDATES_INTERFACE_ID: &str = "bcode.harness.candidates/v1";
/// Non-mutating local inspection only; never login, installation, or automatic configuration.
pub const HARNESS_DISCOVER_CANDIDATES_OPERATION: &str = "discover_candidates";
/// Prepare a selected candidate without configuring or enabling it.
/// Older plugins may return unsupported-operation; discovery remains wire-compatible.
pub const HARNESS_PREPARE_CANDIDATE_OPERATION: &str = "prepare_candidate";
/// Upper bound; incompleteness must be reported rather than silently called complete.
pub const MAX_HARNESS_CANDIDATES: usize = 8;

/// Host admission must authorize inspection using the plugin's trusted launch environment.
/// No client-controlled executable paths, environment variables or native command arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCandidatesRequest {
    pub schema_version: u16,
    pub request_id: String,
}
impl HarnessCandidatesRequest {
    /// Check compatibility and correlation before inspection.
    /// # Errors
    /// Rejects unknown versions, malformed identities or excessive payloads.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.request_id)?;
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}

/// Filesystem identity at inspection time, not a cryptographic artifact digest or execution grant.
/// Selection/commit must revalidate; a path or metadata snapshot cannot fence later replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessCandidateExecutableIdentity {
    pub canonical_path: String,
    /// The path the user's search path resolves (for example a stable profile symlink), when
    /// it differs from `canonical_path`. Configured instances invoke it so a package-manager
    /// update that replaces the canonical target does not break them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_path: Option<String>,
    pub byte_len: u64,
    pub modified_unix_nanos: Option<u128>,
    /// Platform file identity when available (for example device and inode).
    pub file_key: Option<String>,
}

/// Sanitized local status; never a remote entitlement, model-access, or credential proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessCandidateReadiness {
    ExistingNativeLogin,
    LoginRequired,
    Unsupported,
    Unknown,
}

/// One installed native-account candidate, not a configured/enabled harness instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInstallationCandidate {
    pub plugin_id: String,
    pub executable: HarnessCandidateExecutableIdentity,
    /// Exact observed native version, not a capability attestation.
    pub native_version: String,
    pub config_namespace: String,
    pub custody: HarnessAuthCustody,
    pub readiness: HarnessCandidateReadiness,
}
impl HarnessCandidateExecutableIdentity {
    /// The path a configured instance invokes: the search-path entry when known, otherwise the
    /// canonical file.
    #[must_use]
    pub fn invocation(&self) -> &str {
        self.invocation_path
            .as_deref()
            .unwrap_or(&self.canonical_path)
    }
}

impl HarnessInstallationCandidate {
    /// Validate bounded nonsecret metadata; not filesystem or execution authority.
    /// # Errors
    /// Rejects malformed fields and non-native custody.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.plugin_id)?;
        id(&self.native_version)?;
        text(&self.executable.canonical_path)?;
        text(&self.config_namespace)?;
        if self.executable.canonical_path.trim().is_empty()
            || self.config_namespace.trim().is_empty()
            || self.custody != HarnessAuthCustody::NativeManaged
        {
            return Err(HarnessContractError::InvalidShape);
        }
        if let Some(key) = &self.executable.file_key {
            id(key)?;
        }
        if let Some(path) = &self.executable.invocation_path {
            text(path)?;
            if !std::path::Path::new(path).is_absolute() {
                return Err(HarnessContractError::InvalidShape);
            }
        }
        Ok(())
    }
}

/// Explicit incomplete/unavailable results never mean no installed executable or no account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "response", rename_all = "snake_case")]
pub enum HarnessCandidatesResponseKind {
    Candidates {
        candidates: Vec<HarnessInstallationCandidate>,
        complete: bool,
    },
    Failure(HarnessFailure),
}
/// Exact request and producer identity are required before displaying or selecting candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessCandidatesResponse {
    pub schema_version: u16,
    pub request_id: String,
    pub plugin_id: String,
    pub kind: HarnessCandidatesResponseKind,
}
impl HarnessCandidatesResponse {
    /// Validate exact request/producer correlation, bounds and duplicate executable identities.
    /// # Errors
    /// Rejects unknown versions, mismatches, malformed metadata and excessive payloads.
    pub fn validate_for(
        &self,
        request: &HarnessCandidatesRequest,
        plugin_id: &str,
    ) -> Result<(), HarnessContractError> {
        request.validate()?;
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.plugin_id)?;
        if self.request_id != request.request_id || self.plugin_id != plugin_id {
            return Err(HarnessContractError::WrongOwner);
        }
        if let HarnessCandidatesResponseKind::Candidates { candidates, .. } = &self.kind {
            if candidates.len() > MAX_HARNESS_CANDIDATES {
                return Err(HarnessContractError::BoundsExceeded);
            }
            let mut unique = BTreeSet::new();
            for candidate in candidates {
                candidate.validate()?;
                if candidate.plugin_id != self.plugin_id
                    || !unique.insert(&candidate.executable.canonical_path)
                {
                    return Err(HarnessContractError::WrongOwner);
                }
            }
        }
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}

/// Explicit user selection of a native candidate. Instance naming is host-owned;
/// auth binding and adapter configuration are native-plugin-owned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessPrepareCandidateRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub instance_id: String,
    pub candidate: HarnessInstallationCandidate,
}
impl HarnessPrepareCandidateRequest {
    /// Check compatibility, bounded identity and candidate metadata before reinspection.
    /// # Errors
    /// Unknown versions, invalid fields or excessive payloads.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.request_id)?;
        id(&self.instance_id)?;
        self.candidate.validate()?;
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}

/// Nonsecret proposal for one native plugin instance, not a whole config replacement.
///
/// The configuration lane merges `adapter_settings` at the native plugin's
/// `instances[instance.instance_id]` entry without interpreting vendor semantics.
/// No readiness, policy bypass, durable ownership or login authority is conferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessPreparedCandidate {
    pub candidate: HarnessInstallationCandidate,
    pub instance: crate::HarnessInstance,
    /// Opaque native-plugin-owned bounded nonsecret object; never credentials.
    pub adapter_settings: serde_json::Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "response", rename_all = "snake_case")]
pub enum HarnessPrepareCandidateResponseKind {
    Prepared(Box<HarnessPreparedCandidate>),
    Failure(HarnessFailure),
}
/// Correlated native proposal after fresh exact candidate equality revalidation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessPrepareCandidateResponse {
    pub schema_version: u16,
    pub request_id: String,
    pub plugin_id: String,
    pub kind: HarnessPrepareCandidateResponseKind,
}
impl HarnessPrepareCandidateResponse {
    /// Validate correlation and unchanged candidate/instance selection before presenting.
    /// # Errors
    /// Unknown versions, mismatched selection, malformed settings or oversized payloads.
    pub fn validate_for(
        &self,
        request: &HarnessPrepareCandidateRequest,
        plugin_id: &str,
    ) -> Result<(), HarnessContractError> {
        request.validate()?;
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        if self.request_id != request.request_id
            || self.plugin_id != plugin_id
            || request.candidate.plugin_id != plugin_id
        {
            return Err(HarnessContractError::WrongOwner);
        }
        if let HarnessPrepareCandidateResponseKind::Prepared(prepared) = &self.kind {
            prepared.instance.validate()?;
            if prepared.candidate != request.candidate
                || prepared.instance.instance_id != request.instance_id
                || prepared.instance.plugin_id != plugin_id
                || prepared.instance.executable != request.candidate.executable.invocation()
                || prepared.instance.config_namespace != request.candidate.config_namespace
                || prepared.instance.auth.custody != HarnessAuthCustody::NativeManaged
                || !prepared.adapter_settings.is_object()
            {
                return Err(HarnessContractError::WrongOwner);
            }
            wire(&prepared.adapter_settings, crate::MAX_HARNESS_TEXT_BYTES)?;
        }
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidates_require_exact_request_and_producer() {
        let request = HarnessCandidatesRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "inspect-1".into(),
        };
        let mut response = HarnessCandidatesResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            plugin_id: "native-plugin".into(),
            kind: HarnessCandidatesResponseKind::Candidates {
                candidates: Vec::new(),
                complete: false,
            },
        };
        assert!(response.validate_for(&request, "native-plugin").is_ok());
        assert!(response.validate_for(&request, "other-plugin").is_err());
        response.request_id = "other-inspection".into();
        assert!(response.validate_for(&request, "native-plugin").is_err());
    }
}
