//! Explicit authorization to supersede one failed, unpublished compaction operation.
//!
//! This is not summary reconstruction, checkpoint repair, or permission to reinterpret accepted
//! bytes. Admission and terminal completion are separate. Unknown outcomes require inspection,
//! not resubmission with another identity. Implementations must retain canonical old evidence.

use crate::{CompactionSynthesisCheckpoint, SessionId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Exact canonical prior marker identity, using the session domain's canonical event digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactionRestartMarker {
    /// Canonical marker event sequence (not its compacted-through source boundary).
    pub sequence: u64,
    /// Lowercase SHA-256 from `bcode_session::compaction_event_identity`.
    pub sha256: String,
}

/// Explicit new-operation allowances; never added to or substituted into the old checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactionRestartBudget {
    /// Maximum provider calls including repair, consolidation, and ambiguous reserved calls.
    pub max_provider_requests: u32,
    /// Maximum elapsed operation time in milliseconds.
    pub timeout_ms: u32,
}

impl SessionCompactionRestartBudget {
    /// Whether the explicitly supplied allowances fit the supported hard ceilings.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.max_provider_requests > 0
            && self.max_provider_requests <= 2_048
            && self.timeout_ms > 0
            && self.timeout_ms <= 600_000
    }
}

/// Compare-and-supersede authorization for one exact checkpoint and prior marker.
///
/// Version 2 explicitly authorizes abandoning unresolved summary response results, retaining
/// spent accounting and fencing late results by atomic supersession. It never certifies settlement
/// or permits replay of tools or other committed side effects.
///
/// Version 1 supports current-format, failed unpublished operations only. Unsupported versions,
/// stale identities, unknown payloads, unresolved provider outcomes, or non-quiescent ownership
/// must fail closed without retirement. `None` explicitly asserts that no prior marker exists.
/// New identity and allowances must be durably bound to the supersession before provider work;
/// retrying the same identity must never grant another allowance. No historical rebuild is implied.
/// The terminal identity must resolve to canonical completed-rejection evidence for this exact
/// operation and checkpoint. Reserved request slots, cancellation, a matching phase name, or
/// arbitrary error text alone never prove that provider effects have ended. Unknown historical
/// terminal representations require migration-owned handling, not runtime reinterpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactionRestartRequest {
    /// Version 1 requires settlement; version 2 explicitly abandons unresolved summary results.
    pub version: u32,
    /// Canonical session in the selected state location.
    pub session_id: SessionId,
    /// Caller-selected distinct operation identity, retained unchanged across observation retries.
    pub operation_id: Uuid,
    /// Exact old checkpoint namespace; policy changes must not bypass this comparison.
    pub expected_operation_key: String,
    /// Exact latest old checkpoint event sequence, not chunk number or source boundary.
    pub expected_checkpoint_sequence: u64,
    /// Exact canonical completed-rejection event for this operation, after the checkpoint.
    pub expected_terminal_sequence: u64,
    /// Lowercase canonical event SHA-256; identity alone does not establish terminal authority.
    pub expected_terminal_sha256: String,
    /// Exact canonical tail selected during preparation, separate from rejection evidence.
    /// Omission retains the original rejection-as-tail contract; it never selects a new tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_tail_marker: Option<SessionCompactionRestartMarker>,
    /// Exact prior marker, or an explicit assertion of no prior marker.
    #[serde(deserialize_with = "Option::deserialize")]
    pub expected_prior_marker: Option<SessionCompactionRestartMarker>,
    /// Explicitly authorized allowances for the distinct replacement operation only.
    pub budget: SessionCompactionRestartBudget,
}

impl SessionCompactionRestartRequest {
    /// Validate only portable shape and hard bounds, not ownership or restart eligibility.
    ///
    /// # Errors
    /// Rejects unsupported versions, invalid identities, keys, digests, or allowances without
    /// echoing potentially private checkpoint keys. The server must also validate canonical state.
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.version, 1 | 2) || self.operation_id.is_nil() || !self.budget.is_valid() {
            return Err("invalid or unsupported compaction restart request".into());
        }
        CompactionSynthesisCheckpoint::validate_key(&self.expected_operation_key)?;
        if self.expected_terminal_sequence <= self.expected_checkpoint_sequence
            || !valid_digest(&self.expected_terminal_sha256)
        {
            return Err("invalid compaction restart terminal event identity".into());
        }
        if self.expected_tail_marker.as_ref().is_some_and(|tail| {
            tail.sequence < self.expected_terminal_sequence
                || !valid_digest(&tail.sha256)
                || (tail.sequence == self.expected_terminal_sequence
                    && tail.sha256 != self.expected_terminal_sha256)
        }) {
            return Err("invalid compaction restart tail identity".into());
        }
        if self.expected_prior_marker.as_ref().is_some_and(|marker| {
            marker.sequence >= self.expected_checkpoint_sequence || !valid_digest(&marker.sha256)
        }) {
            return Err("invalid compaction restart prior marker identity".into());
        }
        Ok(())
    }
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Version 1 requires proven settlement; version 2 explicitly requests response abandonment.
///
/// Preparation neither admits work nor authorizes replay. Unsupported or incomplete evidence
/// returns a blocker; callers must not silently choose another checkpoint or increase bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactionRecoveryPrepareRequest {
    /// Request compatibility version; only 1 is supported.
    pub version: u32,
    /// Canonical session in the selected state location.
    pub session_id: SessionId,
    /// Distinct replacement identity, retained if the preparation is retried.
    pub operation_id: Uuid,
    /// Exact indexed old checkpoint namespace.
    pub expected_operation_key: String,
    /// Exact latest checkpoint event sequence.
    pub expected_checkpoint_sequence: u64,
    /// Exact final failed-attempt event; its digest is obtained from canonical evidence.
    pub expected_terminal_sequence: u64,
    /// Explicit new-operation allowances, never applied to the old operation.
    pub budget: SessionCompactionRestartBudget,
}

impl SessionCompactionRecoveryPrepareRequest {
    /// Validate portable shape, not canonical evidence, ownership, or dispatch eligibility.
    ///
    /// # Errors
    /// Rejects unsupported versions, empty/oversized identities, invalid order or allowances.
    pub fn validate(&self) -> Result<(), String> {
        CompactionSynthesisCheckpoint::validate_key(&self.expected_operation_key)?;
        if !matches!(self.version, 1 | 2)
            || self.operation_id.is_nil()
            || !self.budget.is_valid()
            || self.expected_checkpoint_sequence == 0
            || self.expected_terminal_sequence <= self.expected_checkpoint_sequence
        {
            return Err("invalid compaction recovery preparation request".into());
        }
        Ok(())
    }
}

/// Payload-free explanation of why read-only preparation cannot offer a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCompactionRecoveryBlocker {
    /// The selected checkpoint is no longer the exact latest indexed checkpoint.
    CheckpointChanged,
    /// Current host checkpoint validation does not recognize the representation.
    UnsupportedCheckpoint,
    /// Required canonical evidence is absent or exceeds the fixed read bound.
    EvidenceUnavailable,
    /// Reserved calls are not all proven terminal; counter exhaustion is not settlement.
    UnsettledRequests,
    /// The final attempt does not establish a supported unpublished failure.
    UnsupportedFailure,
    /// An existing supersession pointer must be inspected instead of admitting new work.
    AlreadySuperseded,
}

/// Bounded, payload-free observation, not an ownership grant or publication receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactionRecoveryPreparation {
    /// Response compatibility version; only 1 is produced.
    pub version: u32,
    /// Canonical session selected by the caller.
    pub session_id: SessionId,
    /// Exact canonical checkpoint identity when available; never includes accepted carry.
    pub checkpoint: Option<SessionCompactionRestartMarker>,
    /// Exact canonical final-attempt identity when available.
    pub terminal: Option<SessionCompactionRestartMarker>,
    /// Why no candidate can be offered. A candidate always has no blocker.
    pub blocker: Option<SessionCompactionRecoveryBlocker>,
    /// Exact request for explicit review; admission must revalidate state and fenced ownership.
    pub restart: Option<SessionCompactionRestartRequest>,
}

/// Portable application boundary for bounded, non-mutating preparation.
pub trait SessionCompactionRecoveryPreparationApplication: Sync {
    /// Observe canonical identities without provider calls, mutation, or ownership acquisition.
    ///
    /// # Errors
    /// Rejects invalid requests and unavailable canonical storage. Ineligible evidence is
    /// returned as a typed blocker, without exposing checkpoint payloads or summary text.
    fn prepare_session_compaction_recovery(
        &self,
        request: SessionCompactionRecoveryPrepareRequest,
    ) -> impl std::future::Future<
        Output = Result<SessionCompactionRecoveryPreparation, crate::SessionApplicationFailure>,
    > + Send;
}

/// Portable application admission boundary, separate from history reconstruction.
pub trait SessionCompactionRestartApplication: Sync {
    /// Observation handle returned only after serialized restart admission succeeds.
    type CompactionOperation: crate::SessionCompactionOperation;

    /// Admit an explicitly authorized, exact failed-operation restart.
    ///
    /// Dropping the returned handle does not cancel admitted work. A lost admission response
    /// does not prove failure; inspect canonical evidence before requesting a new identity.
    ///
    /// # Errors
    /// Rejects invalid requests, unavailable authority, stale or ambiguous evidence, unknown
    /// formats, active work, or failed admission. Rejection must not reset old allowances.
    fn begin_session_compaction_restart(
        &self,
        request: SessionCompactionRestartRequest,
    ) -> impl std::future::Future<
        Output = Result<Self::CompactionOperation, crate::SessionApplicationFailure>,
    > + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SessionCompactionRestartRequest {
        SessionCompactionRestartRequest {
            version: 1,
            session_id: SessionId::new(),
            operation_id: Uuid::new_v4(),
            expected_operation_key: "local-compaction-v1:old".into(),
            expected_checkpoint_sequence: 42,
            expected_terminal_sequence: 46,
            expected_terminal_sha256: "cd".repeat(32),
            expected_tail_marker: None,
            expected_prior_marker: Some(SessionCompactionRestartMarker {
                sequence: 12,
                sha256: "ab".repeat(32),
            }),
            budget: SessionCompactionRestartBudget {
                max_provider_requests: 16,
                timeout_ms: 30_000,
            },
        }
    }

    #[test]
    fn restart_round_trips_exact_authorization_and_explicit_absence() {
        let mut value = request();
        for marker in [value.expected_prior_marker.clone(), None] {
            value.expected_prior_marker = marker;
            value.validate().unwrap();
            let encoded = serde_json::to_string(&value).unwrap();
            assert_eq!(
                serde_json::from_str::<SessionCompactionRestartRequest>(&encoded).unwrap(),
                value
            );
        }
        for field in [
            "version",
            "session_id",
            "operation_id",
            "expected_operation_key",
            "expected_checkpoint_sequence",
            "expected_terminal_sequence",
            "expected_terminal_sha256",
            "expected_prior_marker",
            "budget",
        ] {
            let mut encoded = serde_json::to_value(&value).unwrap();
            encoded.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<SessionCompactionRestartRequest>(encoded).is_err(),
                "missing authorization field {field}"
            );
        }
        let mut encoded = serde_json::to_value(&value).unwrap();
        encoded["unexpected"] = true.into();
        assert!(serde_json::from_value::<SessionCompactionRestartRequest>(encoded).is_err());
    }

    #[test]
    fn preparation_validates_exact_selection_and_serializes_without_payload() {
        let restart = request();
        let mut prepare = SessionCompactionRecoveryPrepareRequest {
            version: 1,
            session_id: restart.session_id,
            operation_id: restart.operation_id,
            expected_operation_key: restart.expected_operation_key.clone(),
            expected_checkpoint_sequence: restart.expected_checkpoint_sequence,
            expected_terminal_sequence: restart.expected_terminal_sequence,
            budget: restart.budget,
        };
        prepare.validate().unwrap();
        let encoded = serde_json::to_value(&prepare).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionCompactionRecoveryPrepareRequest>(encoded.clone())
                .unwrap(),
            prepare
        );
        let mut future = encoded;
        future["version"] = 3.into();
        assert!(
            serde_json::from_value::<SessionCompactionRecoveryPrepareRequest>(future)
                .unwrap()
                .validate()
                .is_err()
        );
        prepare.expected_checkpoint_sequence = 0;
        assert!(prepare.validate().is_err());
        let response = SessionCompactionRecoveryPreparation {
            version: 1,
            session_id: restart.session_id,
            checkpoint: None,
            terminal: None,
            blocker: Some(SessionCompactionRecoveryBlocker::UnsettledRequests),
            restart: None,
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["blocker"], "unsettled_requests");
        assert_eq!(
            serde_json::from_value::<SessionCompactionRecoveryPreparation>(value).unwrap(),
            response
        );
    }

    #[test]
    fn restart_accepts_explicit_ceiling_without_changing_authorization() {
        let mut value = request();
        value.budget = SessionCompactionRestartBudget {
            max_provider_requests: 2_048,
            timeout_ms: 600_000,
        };
        value.validate().unwrap();
        let before = value.clone();
        value.validate().unwrap();
        assert_eq!(value, before);
    }

    #[test]
    fn restart_rejects_unknown_versions_identities_and_unbounded_allowances() {
        let original = request();
        let mut value = original.clone();
        value.version = 2;
        assert!(value.validate().is_ok()); // Explicit response abandonment is a different contract.
        value.version = 3;
        assert!(value.validate().is_err());
        value = original.clone();
        value.operation_id = Uuid::nil();
        assert!(value.validate().is_err());
        value = original.clone();
        value.expected_operation_key.clear();
        assert!(value.validate().is_err());
        for requests in [0, 2_049] {
            value = original.clone();
            value.budget.max_provider_requests = requests;
            assert!(value.validate().is_err());
        }
        for timeout in [0, 600_001] {
            value = original.clone();
            value.budget.timeout_ms = timeout;
            assert!(value.validate().is_err());
        }
        for digest in ["AB".repeat(32), "gg".repeat(32), "ab".repeat(31)] {
            value = original.clone();
            value.expected_prior_marker.as_mut().unwrap().sha256 = digest;
            assert!(value.validate().is_err());
        }
        for sequence in [0, 41, 42] {
            value = original.clone();
            value.expected_terminal_sequence = sequence;
            assert!(value.validate().is_err());
        }
        for digest in ["CD".repeat(32), "zz".repeat(32), "cd".repeat(31)] {
            value = original.clone();
            value.expected_terminal_sha256 = digest;
            assert!(value.validate().is_err());
        }
        value = original;
        value.expected_prior_marker.as_mut().unwrap().sequence = 42;
        assert!(value.validate().is_err());
    }
}
