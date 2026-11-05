//! Canonical identity and state of host-admitted managed tool invocations.
//!
//! These records retain provenance, never bearer authorization or credential material.
use crate::{ExternalHarnessOwner, SessionEvent, SessionEventKind, ToolInvocationResultRecord};
use serde::{Deserialize, Serialize};

/// Reserved metadata key on ordinary tool lifecycle evidence.
pub const MANAGED_TOOL_CLAIM_KEY: &str = "bcode.managed-tool-claim.v1";

/// Exact immutable facts admitted by the host before tool execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedToolIdentity {
    /// Full durable managed execution owner at admission.
    pub owner: ExternalHarnessOwner,
    /// Host-retained grant identity; correlation only, not portable authority.
    pub grant_id: String,
    /// Ordinary invocation ID, unchanged by transport or adapter.
    pub invocation_id: String,
    /// Pinned registered tool producer.
    pub producer_plugin_id: String,
    /// Canonical registered tool name.
    pub tool_name: String,
    /// SHA-256 of canonical arguments.
    pub arguments_sha256: String,
    /// SHA-256 of the retained preparation descriptor and authorization facts.
    pub preparation_sha256: String,
    /// Canonical workspace selected by host admission.
    pub working_directory: String,
}
impl ManagedToolIdentity {
    /// Check non-secret shape and allocation limits; this grants no authority.
    /// # Errors
    /// Rejects invalid owner, identifiers, digests and excessive paths.
    pub fn validate(&self) -> Result<(), String> {
        self.owner.validate()?;
        for id in [
            &self.grant_id,
            &self.invocation_id,
            &self.producer_plugin_id,
            &self.tool_name,
        ] {
            if id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
                return Err("invalid managed tool identity".into());
            }
        }
        for digest in [&self.arguments_sha256, &self.preparation_sha256] {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("invalid managed tool digest".into());
            }
        }
        if self.working_directory.is_empty()
            || self.working_directory.len() > 4096
            || !std::path::Path::new(&self.working_directory).is_absolute()
        {
            return Err("invalid managed tool workspace".into());
        }
        Ok(())
    }
}

/// Typed claim transition carried in ordinary tool lifecycle metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transition", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedToolClaimEvent {
    /// Claim invocation identity; not approval or permission to execute.
    Claimed { identity: ManagedToolIdentity },
    /// Atomically consume the exact retained claim before transport dispatch.
    Started {
        identity: ManagedToolIdentity,
        claim_sequence: u64,
    },
}

/// Bounded canonical invocation state. Result bytes stay in their ordinary canonical event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedToolClaimRecord {
    /// Exact retained admission facts.
    pub identity: ManagedToolIdentity,
    /// Canonical claim event sequence.
    pub claim_sequence: u64,
    /// Canonical dispatch event; absence never authorizes an automatic retry.
    pub started_sequence: Option<u64>,
    /// Ordinary terminal result event sequence.
    pub result_sequence: Option<u64>,
}

/// Actor-serialized claim result. Only New may progress to a new dispatch.
#[derive(Debug, Clone)]
pub enum ManagedToolClaimOutcome {
    /// New identity was durably retained.
    New(ManagedToolClaimRecord),
    /// Existing claim cannot be automatically replayed, including after process loss.
    Uncertain(ManagedToolClaimRecord),
    /// Exact prior terminal evidence; never permission to rerun effects.
    Recorded {
        claim: ManagedToolClaimRecord,
        result: Box<ToolInvocationResultRecord>,
    },
}

impl ManagedToolClaimEvent {
    /// Decode only the reserved typed lifecycle envelope; absence is ordinary tool evidence.
    /// # Errors
    /// Rejects malformed, excessive, or unsupported managed claim metadata.
    pub fn from_event(event: &SessionEvent) -> Result<Option<Self>, String> {
        let SessionEventKind::ToolInvocationLifecycle { event: lifecycle } = &event.kind else {
            return Ok(None);
        };
        let Some(value) = lifecycle.metadata.get(MANAGED_TOOL_CLAIM_KEY) else {
            return Ok(None);
        };
        let bytes = serde_json::to_vec(value).map_err(|_| "invalid managed tool claim")?;
        if bytes.len() > 16_384 {
            return Err("managed tool claim exceeds bound".into());
        }
        let claim: Self =
            serde_json::from_slice(&bytes).map_err(|_| "unsupported managed tool claim")?;
        let identity = match &claim {
            Self::Claimed { identity } | Self::Started { identity, .. } => identity,
        };
        identity.validate()?;
        let expected_stage = match &claim {
            Self::Claimed { .. } => crate::ToolInvocationLifecycleStage::Waiting,
            Self::Started { .. } => crate::ToolInvocationLifecycleStage::Started,
        };
        if lifecycle.stage != expected_stage || lifecycle.sequence != 0 {
            return Err("managed tool claim lifecycle mismatch".into());
        }
        if identity.owner.session_id != event.session_id
            || lifecycle.invocation_id != identity.invocation_id
        {
            return Err("managed tool claim owner mismatch".into());
        }
        Ok(Some(claim))
    }
}
