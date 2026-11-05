//! Ephemeral host/adapter managed-tool queue. This ordinary non-secret service never carries
//! credentials, ownership claims or workflow grant payloads. Only daemon-selected dispatch may
//! bind it; it is not reachable through the native tool/service router.
use crate::{HarnessContractError, ManagedToolRequest, ManagedToolResponse, id, wire};
use serde::{Deserialize, Serialize};

/// Exact additive queue service; old adapters simply do not declare it.
pub const MANAGED_TOOL_QUEUE_INTERFACE_ID: &str = "bcode.harness.tool-queue/v1";
/// Maximum requests drained per host poll and maximum outstanding native callbacks.
pub const MAX_MANAGED_TOOL_QUEUE: usize = 16;

/// Exact owner correlation on every queue operation. This data is not authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedToolQueueScope {
    pub session_id: String,
    pub generation: u64,
    pub bridge_id: String,
}
impl ManagedToolQueueScope {
    /// Validate bounded correlation only.
    /// # Errors
    /// Rejects malformed identities or zero generation.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.session_id)?;
        id(&self.bridge_id)?;
        if self.generation == 0 {
            return Err(HarnessContractError::StaleGeneration);
        }
        Ok(())
    }
}

/// Host-only service operations, never accepted as native callback operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedToolQueueRequest {
    Bind {
        scope: ManagedToolQueueScope,
    },
    Drain {
        scope: ManagedToolQueueScope,
    },
    Complete {
        scope: ManagedToolQueueScope,
        response: ManagedToolResponse,
    },
    Revoke {
        scope: ManagedToolQueueScope,
    },
}
impl ManagedToolQueueRequest {
    /// Validate bounded service payload.
    /// # Errors
    /// Rejects malformed or oversized messages.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        self.scope().validate()?;
        wire(self, crate::MAX_MANAGED_TOOL_MESSAGE_BYTES)
    }
    /// Exact bound correlation, not authority.
    #[must_use]
    pub const fn scope(&self) -> &ManagedToolQueueScope {
        match self {
            Self::Bind { scope }
            | Self::Drain { scope }
            | Self::Complete { scope, .. }
            | Self::Revoke { scope } => scope,
        }
    }
    /// Exact service operation name.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::Bind { .. } => "bind",
            Self::Drain { .. } => "drain",
            Self::Complete { .. } => "complete",
            Self::Revoke { .. } => "revoke",
        }
    }
}

/// No journal or delivery retry: drained requests remain pending until exact completion/revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedToolQueueResponse {
    Bound,
    Requests { requests: Vec<ManagedToolRequest> },
    Completed,
    Revoked,
    Rejected,
}
