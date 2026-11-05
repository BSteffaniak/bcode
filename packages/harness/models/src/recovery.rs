//! Optional recovery evidence interface. Discovery and validation confer no authority.
//! Absence is unsupported; ordinary v3 Close acknowledgement is never release evidence.
use crate::{HarnessControlRequest, HarnessInstance, HarnessOpenRequest, HarnessSessionTarget};
use serde::{Deserialize, Serialize};

/// Independently negotiated optional recovery service; leaves harness v3 unchanged.
pub const HARNESS_RECOVERY_INTERFACE_ID: &str = "bcode.harness.recovery/v1";
/// Recovery envelope version. Unknown versions fail closed.
pub const HARNESS_RECOVERY_SCHEMA_VERSION: u16 = 1;

/// Host-authorized, exactly correlated recovery operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRecoveryRequest {
    /// Must equal `HARNESS_RECOVERY_SCHEMA_VERSION`.
    pub schema_version: u16,
    /// Bounded host correlation identity.
    pub request_id: String,
    /// Exact executable/configuration/authentication selection.
    pub instance: HarnessInstance,
    /// Operation-specific intent; service method must match.
    pub kind: HarnessRecoveryRequestKind,
}

/// Recovery methods; neither permits replay of uncertain native work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessRecoveryRequestKind {
    /// Query-only validation of explicit resume constraints, without native spawn.
    PrepareResume {
        /// Exact Open intent with a confirmed resume binding.
        request: HarnessOpenRequest,
    },
    /// Observe completed physical release of an already canonically Closed transport.
    ReleaseTransport {
        /// Exact retained Close mutation. Never an instruction to issue another Close.
        request: HarnessControlRequest,
    },
}

/// Successful recovery evidence. Failures use the existing sanitized service error channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRecoveryResponse {
    /// Must equal `HARNESS_RECOVERY_SCHEMA_VERSION`.
    pub schema_version: u16,
    /// Exact request envelope correlation.
    pub request_id: String,
    /// Evidence, not a grant of execution or takeover authority.
    pub kind: HarnessRecoveryResponseKind,
}

/// Explicit evidence; no acknowledgement-to-release conversion is allowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessRecoveryResponseKind {
    /// The selected adapter validated exact supported resume constraints without spawning.
    ResumeValidated {
        /// Must equal the complete requested Open intent, not a rewritten approximation.
        request: HarnessOpenRequest,
    },
    /// Physical supervisor completion for an exact retained Close; not proof of no effects.
    TransportReleased {
        /// Exact request target, including generation and native version.
        target: HarnessSessionTarget,
        /// Exact retained Close mutation identity.
        mutation_id: String,
        /// Stable bounded adapter-issued identity for this physical release observation.
        reap_id: String,
    },
}
