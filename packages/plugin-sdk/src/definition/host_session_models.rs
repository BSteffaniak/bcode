//! Product-owned retained host-session contract, independent of native ABI layout.
//!
//! IDs address records only through the owning retained port. They are not grants,
//! process handles, credentials, pointers or a way to reopen an invocation port.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Exact negotiated product contract. Unsupported versions never fall back.
pub const PRODUCT_HOST_SESSION_CONTRACT: &str = "bcode.product-host-session/v1";
/// Independent product schema version.
pub const PRODUCT_HOST_SESSION_VERSION: u32 = 1;
/// Maximum encoded request/result envelope accepted by this product client.
pub const MAX_SESSION_ENVELOPE_BYTES: usize = 1024 * 1024;
/// Maximum identifier size in UTF-8 bytes, not terminal columns.
pub const MAX_SESSION_IDENTIFIER_BYTES: usize = 256;
/// Maximum records returned by one recovery page.
pub const MAX_SESSION_PAGE_SIZE: u32 = 256;

/// Session-wide policy imposed by the actual selected owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBudget {
    /// Simultaneously retained job identities, including tombstones.
    pub max_jobs: u32,
    /// Simultaneously retained operation identities, including tombstones.
    pub max_operations: u32,
    /// Maximum retained request bytes reserved by a job.
    pub max_request_bytes: u32,
    /// Maximum retained outcome bytes reserved by a job.
    pub max_response_bytes: u32,
    /// Total retention including identity metadata, requests, outcome reservations,
    /// failure records and acknowledged/released request tombstones.
    pub retained_bytes: u64,
}
impl SessionBudget {
    /// Check nonzero policy and minimum payload capacity with checked arithmetic.
    ///
    /// # Errors
    /// Returns `InvalidBudget` for zero/insufficient capacity. The runtime must
    /// additionally reserve exact metadata/failure/tombstone costs before effects.
    pub fn validate(self) -> Result<(), SessionCode> {
        if self.max_jobs == 0
            || self.max_operations == 0
            || self.max_request_bytes == 0
            || self.max_response_bytes == 0
            || self.retained_bytes == 0
        {
            return Err(SessionCode::InvalidBudget);
        }
        let minimum = u64::from(self.max_request_bytes)
            .checked_add(u64::from(self.max_response_bytes))
            .ok_or(SessionCode::InvalidBudget)?;
        if minimum > self.retained_bytes {
            return Err(SessionCode::InvalidBudget);
        }
        Ok(())
    }
}

/// Job-wide reservation, consumed before producer dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobReservation {
    /// Maximum operations admitted within the job.
    pub operations: u32,
    /// Retained request capacity, not a count of transient observations.
    pub request_bytes: u32,
    /// Entire worst-case outcome capacity, including bounded failure metadata.
    pub response_bytes: u32,
}
impl JobReservation {
    /// Validate policy bounds without reserving capacity or conferring authority.
    ///
    /// # Errors
    /// Returns `InvalidBudget` for zero policy or `CapacityExceeded` when this
    /// reservation exceeds the session. Runtime admission also charges metadata.
    pub fn validate_for(self, budget: SessionBudget) -> Result<(), SessionCode> {
        budget.validate()?;
        if self.operations == 0 || self.request_bytes == 0 || self.response_bytes == 0 {
            return Err(SessionCode::InvalidBudget);
        }
        let bytes = u64::from(self.request_bytes)
            .checked_add(u64::from(self.response_bytes))
            .ok_or(SessionCode::InvalidBudget)?;
        if self.operations > budget.max_operations
            || self.request_bytes > budget.max_request_bytes
            || self.response_bytes > budget.max_response_bytes
            || bytes > budget.retained_bytes
        {
            return Err(SessionCode::CapacityExceeded);
        }
        Ok(())
    }
}

/// Generic BMUX operation classification, shared without an IPC envelope.
pub use bmux_plugin_sdk::ServiceKind as SessionServiceKind;

/// Secret-safe closed diagnostics. Effects are classified by the enclosing
/// terminal variant, never inferred from this code or an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCode {
    /// Missing or unsupported independently negotiated contract.
    UnsupportedContract,
    /// Invalid envelope, identity or operation fields.
    InvalidRequest,
    /// Zero, inconsistent or overflowing reservation policy.
    InvalidBudget,
    /// Admission exhausted before dispatch; no eviction/replay is authorized.
    CapacityExceeded,
    /// Same request identity supplied with different canonical operation facts.
    ConflictingRequest,
    /// Owning port is unavailable; this does not prove no effects occurred.
    Unavailable,
    /// Owner policy denied the requested operation.
    PermissionDenied,
    /// Admission is closed; observation and cleanup can remain available.
    AdmissionsClosed,
    /// Requested job is not owned by this session.
    UnknownJob,
    /// Requested operation is not owned by the job.
    UnknownOperation,
    /// Acknowledgement receipt does not match the retained terminal result.
    StaleReceipt,
    /// Producers/callbacks have not settled.
    NotJoined,
    /// Outcomes remain unacknowledged.
    UnacknowledgedOutcomes,
    /// Encoded request exceeded its declared reservation.
    RequestLimitExceeded,
    /// Producer response exceeded its declared retention bound; do not replay.
    ResponseLimitExceeded,
    /// Producer explicitly rejected cancellation before effects.
    CancelledBeforeEffects,
    /// Producer failed; this code alone says nothing about effects.
    ProducerFailed,
    /// Protocol/outcome cannot be verified; retain explicit uncertainty.
    InvalidResponse,
}

/// Exact session-local product operation. Start request IDs are idempotency
/// identities, not caller credentials. No session reference selects another port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HostSessionRequest {
    /// Reserve slots/bytes and record request identity before admitting a job.
    BeginJob {
        request_id: Uuid,
        reservation: JobReservation,
    },
    /// Identical retries resolve the existing operation; conflicting retries reject.
    Start {
        /// Host-minted job identity within this port.
        job: Uuid,
        /// Caller idempotency identity retained through session release.
        request_id: Uuid,
        /// Exact negotiated capability.
        capability: String,
        /// Generic operation classification.
        kind: SessionServiceKind,
        /// Exact negotiated interface.
        interface: String,
        /// Exact operation name, not diagnostic/presentation text.
        operation: String,
        /// Existing owning product operation bytes; no new broad payload schema.
        payload: Vec<u8>,
    },
    /// Non-consuming observation, valid after admission closure.
    Observe { job: Uuid, operation: Uuid },
    /// Close admissions only; not cancellation, settlement or release.
    CloseJob { job: Uuid },
    /// Cancellation-safe settlement observation; dropping it does not stop work.
    JoinJob { job: Uuid },
    /// Explicitly accept one exact retained outcome.
    Acknowledge {
        job: Uuid,
        operation: Uuid,
        receipt: Uuid,
    },
    /// Requires joined producers/callbacks and acknowledged outcomes.
    ReleaseJob { job: Uuid },
    /// Reject new effects while keeping observe/join/ack/recovery usable.
    CloseSession,
    /// Observe settlement of all owned producers/callbacks.
    JoinSession,
    /// Release only after joined jobs and acknowledgements; clear tombstones last.
    ReleaseSession,
    /// Bounded, non-consuming enumeration ordered by host record identity.
    ListJobs { after: Option<Uuid>, limit: u32 },
    /// Bounded, non-consuming enumeration including acknowledged tombstones.
    ListOperations {
        job: Uuid,
        after: Option<Uuid>,
        limit: u32,
    },
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_IDENTIFIER_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
impl HostSessionRequest {
    /// Validate operation facts before invoking the retained port.
    ///
    /// # Errors
    /// Returns `InvalidRequest` for malformed fields/page bounds, `InvalidBudget`
    /// for zero reservations, or `RequestLimitExceeded` for an oversized payload.
    /// Runtime ownership, negotiated operations and aggregate capacity remain host checks.
    pub fn validate(&self) -> Result<(), SessionCode> {
        match self {
            Self::BeginJob {
                request_id,
                reservation,
            } => {
                if request_id.is_nil() {
                    return Err(SessionCode::InvalidRequest);
                }
                if reservation.operations == 0
                    || reservation.request_bytes == 0
                    || reservation.response_bytes == 0
                {
                    return Err(SessionCode::InvalidBudget);
                }
            }
            Self::Start {
                job,
                request_id,
                capability,
                interface,
                operation,
                payload,
                ..
            } => {
                if job.is_nil()
                    || request_id.is_nil()
                    || !identifier(capability)
                    || !identifier(interface)
                    || !identifier(operation)
                {
                    return Err(SessionCode::InvalidRequest);
                }
                if payload.len() > MAX_SESSION_ENVELOPE_BYTES {
                    return Err(SessionCode::RequestLimitExceeded);
                }
            }
            Self::Observe { job, operation } => {
                if job.is_nil() || operation.is_nil() {
                    return Err(SessionCode::InvalidRequest);
                }
            }
            Self::Acknowledge {
                job,
                operation,
                receipt,
            } => {
                if job.is_nil() || operation.is_nil() || receipt.is_nil() {
                    return Err(SessionCode::InvalidRequest);
                }
            }
            Self::CloseJob { job } | Self::JoinJob { job } | Self::ReleaseJob { job } => {
                if job.is_nil() {
                    return Err(SessionCode::InvalidRequest);
                }
            }
            Self::ListJobs { limit, after } => validate_page(*after, *limit)?,
            Self::ListOperations { job, limit, after } => {
                if job.is_nil() {
                    return Err(SessionCode::InvalidRequest);
                }
                validate_page(*after, *limit)?;
            }
            Self::CloseSession | Self::JoinSession | Self::ReleaseSession => {}
        }
        Ok(())
    }
}
fn validate_page(after: Option<Uuid>, limit: u32) -> Result<(), SessionCode> {
    if limit == 0 || limit > MAX_SESSION_PAGE_SIZE || after.is_some_and(|id| id.is_nil()) {
        return Err(SessionCode::InvalidRequest);
    }
    Ok(())
}

/// Author-visible terminal classification; uncertainty never implies rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "outcome",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SessionOutcome {
    /// Existing product response bytes, bounded by the prior reservation.
    Completed { payload: Vec<u8> },
    /// Verified rejection before effects; never synthesized from a late cancel flag.
    RejectedBeforeEffects { code: SessionCode },
    /// Failed/oversized/unverifiable producer result; effects may have committed.
    EffectsUnresolved {
        code: SessionCode,
        observed_bytes: Option<u64>,
    },
}

/// Retained operation recovery record. Acknowledged records preserve identity
/// tombstones until session release, even after outcome bytes are reclaimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOperationRecord {
    /// Owning job within this port.
    pub job: Uuid,
    /// Actual operation identity.
    pub operation: Uuid,
    /// Original caller request identity for recovery after a lost start reply.
    pub request_id: Uuid,
    /// True only once the exact terminal receipt was accepted.
    pub acknowledged: bool,
}

/// Bounded recovery job identity; release retains its request tombstone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionJobRecord {
    /// Actual job identity.
    pub job: Uuid,
    /// Begin request identity for lost-reply recovery.
    pub request_id: Uuid,
    /// Retained reservation facts used for conflicting-duplicate detection.
    pub reservation: JobReservation,
    /// New admissions are forbidden; not evidence of producer settlement.
    pub admissions_closed: bool,
    /// All producers and callbacks actually settled.
    pub joined: bool,
    /// Explicit release acknowledged; tombstone remains until session release.
    pub released: bool,
}

/// Same typed response for static author dispatch and native retained-port codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "result",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HostSessionResponse {
    /// Existing or newly reserved job; no duplicate execution.
    JobBegun { job: Uuid },
    /// Recoverable retained operation identity, published before dispatch.
    Started { operation: Uuid },
    /// Work exists but has not reached authoritative terminal state.
    Pending,
    /// Non-consuming exact outcome observation.
    Terminal {
        receipt: Uuid,
        outcome: SessionOutcome,
    },
    /// Admissions closed, without claiming cancellation or settlement.
    AdmissionsClosed,
    /// Actual producers and callbacks settled.
    Joined,
    /// Exact terminal receipt accepted; request tombstone retained.
    Acknowledged,
    /// Required settlement/ack conditions met and release acknowledged.
    Released,
    /// One bounded recovery page; cursor is session-local observation only.
    Jobs {
        jobs: Vec<SessionJobRecord>,
        next: Option<Uuid>,
    },
    /// One bounded recovery page including acknowledged identities.
    Operations {
        operations: Vec<SessionOperationRecord>,
        next: Option<Uuid>,
    },
    /// Request rejected; for Start this must be before dispatch. Transport failure
    /// after admission is not represented as a verified rejected operation.
    Rejected { code: SessionCode },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn future_variants_and_authority_fields_fail_closed() {
        assert!(serde_json::from_str::<HostSessionRequest>(r#"{"operation":"future"}"#).is_err());
        assert!(
            serde_json::from_str::<HostSessionRequest>(
                r#"{"operation":"close_session","owner":"forged"}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<SessionCode>("\"private unbounded diagnostic\"").is_err());
    }
    #[test]
    fn outcome_roundtrip_preserves_effect_uncertainty() {
        let reply = HostSessionResponse::Terminal {
            receipt: Uuid::from_u128(1),
            outcome: SessionOutcome::EffectsUnresolved {
                code: SessionCode::ResponseLimitExceeded,
                observed_bytes: Some(u64::MAX),
            },
        };
        let encoded = serde_json::to_vec(&reply).unwrap();
        assert_eq!(
            serde_json::from_slice::<HostSessionResponse>(&encoded).unwrap(),
            reply
        );
        assert_ne!(
            reply,
            HostSessionResponse::Rejected {
                code: SessionCode::ResponseLimitExceeded
            }
        );
    }
    #[test]
    fn budgets_and_pages_validate_without_admitting_effects() {
        let budget = SessionBudget {
            max_jobs: 1,
            max_operations: 2,
            max_request_bytes: 32,
            max_response_bytes: 64,
            retained_bytes: 128,
        };
        assert!(budget.validate().is_ok());
        assert_eq!(
            SessionBudget {
                retained_bytes: 1,
                ..budget
            }
            .validate(),
            Err(SessionCode::InvalidBudget)
        );
        assert_eq!(
            JobReservation {
                operations: 3,
                request_bytes: 8,
                response_bytes: 8
            }
            .validate_for(budget),
            Err(SessionCode::CapacityExceeded)
        );
        assert_eq!(
            HostSessionRequest::ListJobs {
                after: None,
                limit: MAX_SESSION_PAGE_SIZE + 1
            }
            .validate(),
            Err(SessionCode::InvalidRequest)
        );
    }
    #[test]
    fn close_and_join_are_distinct_wire_operations() {
        let close = serde_json::to_value(HostSessionRequest::CloseSession).unwrap();
        let join = serde_json::to_value(HostSessionRequest::JoinSession).unwrap();
        assert_ne!(close, join);
        let request = HostSessionRequest::Acknowledge {
            job: Uuid::from_u128(1),
            operation: Uuid::from_u128(2),
            receipt: Uuid::from_u128(3),
        };
        assert_eq!(
            serde_json::from_value::<HostSessionRequest>(serde_json::to_value(&request).unwrap())
                .unwrap(),
            request
        );
    }
}
