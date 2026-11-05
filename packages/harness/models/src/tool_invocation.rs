//! Non-secret correlation for the host-managed ordinary-tool queue.
//!
//! These envelopes carry no authority. Only a host-issued, producer-bound queue endpoint
//! admits them; native arguments, parentage and copied grant identifiers cannot issue one.
use crate::{HarnessContractError, id, wire};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the asynchronous host queue protocol (not a plugin service).
pub const MANAGED_TOOL_BRIDGE_VERSION: u16 = 1;
/// Maximum encoded queue message.
pub const MAX_MANAGED_TOOL_MESSAGE_BYTES: usize = 1024 * 1024;

/// Request correlated with one response on a host-issued endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedToolRequest {
    /// Exact supported version.
    pub version: u16,
    /// Caller correlation, never authorization.
    pub request_id: String,
    /// Operation on this endpoint's retained grant.
    pub operation: ManagedToolOperation,
}

/// Ordinary tool operations; no shell, arbitrary service routing or grant issuance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedToolOperation {
    /// Bounded registered-tool discovery, ordered by canonical name.
    Catalog { after: Option<String>, limit: u16 },
    /// Pin producer, arguments, preparation descriptor and authorization facts.
    Prepare {
        invocation_id: String,
        tool_name: String,
        arguments: Value,
    },
    /// Invoke only the exact retained preparation.
    Invoke {
        invocation_id: String,
        preparation_sha256: String,
    },
    /// Inspect canonical state; never authorizes replay.
    Inspect { invocation_id: String },
    /// Request cooperative cancellation; not proof of physical settlement.
    Cancel { invocation_id: String },
}
impl ManagedToolRequest {
    /// Validate shape and allocation limits, without granting authority.
    /// # Errors
    /// Rejects unsupported versions, malformed identifiers and oversized requests.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.version != MANAGED_TOOL_BRIDGE_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.request_id)?;
        match &self.operation {
            ManagedToolOperation::Catalog { after, limit } => {
                if *limit == 0 || *limit > 64 {
                    return Err(HarnessContractError::BoundsExceeded);
                }
                if let Some(after) = after {
                    id(after)?;
                }
            }
            ManagedToolOperation::Prepare {
                invocation_id,
                tool_name,
                ..
            } => {
                id(invocation_id)?;
                id(tool_name)?;
            }
            ManagedToolOperation::Invoke {
                invocation_id,
                preparation_sha256,
            } => {
                id(invocation_id)?;
                if preparation_sha256.len() != 64
                    || !preparation_sha256.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(HarnessContractError::InvalidShape);
                }
            }
            ManagedToolOperation::Inspect { invocation_id }
            | ManagedToolOperation::Cancel { invocation_id } => id(invocation_id)?,
        }
        wire(self, MAX_MANAGED_TOOL_MESSAGE_BYTES)
    }
}

/// Correlated bounded host response. Canonical result payload is the ordinary session result
/// record encoded as JSON, not an adapter-invented result or native observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedToolResponse {
    pub version: u16,
    pub request_id: String,
    pub outcome: ManagedToolOutcome,
}

/// Secret-safe outcomes. Uncertainty never grants another execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedToolOutcome {
    Catalog {
        tools: Vec<Value>,
        next: Option<String>,
    },
    Prepared {
        invocation_id: String,
        preparation_sha256: String,
    },
    Recorded {
        result: Value,
    },
    Uncertain,
    Missing,
    CancelRequested,
    /// Policy/approval refused before dispatch, without a canonical execution claim.
    Denied,
    /// Invalid/stale/conflicting/unavailable requests disclose no internal error material.
    Rejected,
}
