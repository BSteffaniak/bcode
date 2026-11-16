//! Optional correlated response settlement, independent of frontend-v2 payload semantics.
//!
//! This codec does not grant callback or recovery authority. Admission additionally requires
//! an actual negotiated generic lookup-V2 port. Delivery is never consumer commitment.
use super::Request as FrontendRequest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use uuid::Uuid;

/// Local failure at the opaque responsibility boundary. No variant grants execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionResponsibilityError {
    /// Opt-in completion authority was absent; commitment/effect transfer is forbidden.
    Missing,
    /// Existing scope/response ownership or local disposition cannot be verified.
    RecoveryRequired,
    /// Capacity was not reserved; caller must retain the original response uncommitted.
    Capacity,
}

/// Renderer-neutral local disposition, independent of callback receipt settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionDisposition {
    /// Reserved before effect publication.
    Reserved,
    /// Transferred into the existing consumer completion owner.
    Retained,
    /// Actual effect dispatch started.
    Dispatched,
    /// Replaced before verified dispatch.
    Displaced,
    /// Verified never dispatched.
    NotDispatched,
    /// Actual terminal result applied by its consumer.
    TerminalApplied,
    /// Responsibility is uncertain; original owner must remain retained.
    RecoveryRequired,
}

/// Rust-local, move-only responsibility over an already retained original completion.
///
/// Implementations must retain the actual response/session owner before publication; Drop
/// marks unresolved responsibility recovery-required and never acknowledges or releases it.
/// A serialized ID is correlation only and cannot construct this capability. The boxed handle
/// is intentionally neither Clone nor Serialize. The contract has no product-host dependency.
pub trait FrontendCompletionResponsibility: Send + std::fmt::Debug {
    /// Original typed correlation, not an authority grant.
    #[must_use]
    fn response_id(&self) -> ResponseId;
    /// Stable effect ordinal in that response's bounded record.
    #[must_use]
    fn ordinal(&self) -> usize;
    /// Observe local disposition, never infer callback/job settlement.
    #[must_use]
    fn disposition(&self) -> CompletionDisposition;
    /// Reserve a follow-up on the same record before its dispatch.
    ///
    /// # Errors
    /// Returns capacity or recovery-required without authorizing execution.
    fn follow_up(
        &self,
    ) -> Result<Box<dyn FrontendCompletionResponsibility>, CompletionResponsibilityError>;
    /// Consumer owner has reserved and retained this handle; not delivery acknowledgment.
    ///
    /// # Errors
    /// Rejects invalid local transitions.
    fn retained(&mut self) -> Result<(), CompletionResponsibilityError>;
    /// Mark immediately before actual execution, never at queue insertion.
    ///
    /// # Errors
    /// Rejects invalid local transitions.
    fn dispatched(&mut self) -> Result<(), CompletionResponsibilityError>;
    /// Preserve displaced responsibility, never borrow the replacement's outcome.
    ///
    /// # Errors
    /// Dispatched/uncertain work cannot be declared undispatched.
    fn displaced(&mut self) -> Result<(), CompletionResponsibilityError>;
    /// Verified removal before dispatch; not a cancellation acknowledgment.
    ///
    /// # Errors
    /// Rejects dispatched or unverifiable work.
    fn not_dispatched(&mut self) -> Result<(), CompletionResponsibilityError>;
    /// Consumer actually applied the terminal result; does not join or release author jobs.
    ///
    /// # Errors
    /// Rejects invalid/unverified transitions.
    fn terminal_applied(&mut self) -> Result<(), CompletionResponsibilityError>;
    /// Commit only after every local effect has transferred to its existing consumer owner.
    /// Implementations verify the original bounded slots; invocation/delivery alone is insufficient.
    ///
    /// # Errors
    /// Returns recovery-required on untransferred slots or uncertain transport. Never blindly retries.
    fn commit_transferred(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), CompletionResponsibilityError>> + Send + '_,
        >,
    >;
    /// Record uncertainty without dropping original ownership or acknowledging effects.
    fn recovery_required(&mut self);
}

/// Explicit response lineage captured by author tasks and callback service closures.
/// It confers no host authority and is never recovered from TLS or an invocation handle.
pub struct ResponseContext {
    id: ResponseId,
    active_tasks: AtomicUsize,
    sealed: AtomicBool,
    uncertain: AtomicBool,
}
impl ResponseContext {
    /// Create lineage only after its response has reserved admission capacity.
    #[must_use]
    pub fn new(id: ResponseId) -> Arc<Self> {
        Arc::new(Self {
            id,
            active_tasks: AtomicUsize::new(0),
            sealed: AtomicBool::new(false),
            uncertain: AtomicBool::new(false),
        })
    }
    /// Original correlation, not a replacement execution credential.
    #[must_use]
    pub const fn id(&self) -> ResponseId {
        self.id
    }
    /// Reserve a descendant before dispatch; closed/overflowing lineage fails closed.
    ///
    /// # Errors
    /// Returns unsettled after sealing and capacity when the bounded task count is exhausted.
    pub fn reserve_task(self: &Arc<Self>) -> Result<ResponseTask, LedgerError> {
        if self.sealed.load(Ordering::Acquire) {
            return Err(LedgerError::Unsettled);
        }
        self.active_tasks
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 32).then(|| count + 1)
            })
            .map_err(|_| LedgerError::Capacity)?;
        if self.sealed.load(Ordering::Acquire) {
            self.active_tasks.fetch_sub(1, Ordering::AcqRel);
            return Err(LedgerError::Unsettled);
        }
        Ok(ResponseTask {
            context: Arc::clone(self),
            finished: false,
        })
    }
    /// Seal only after the response's author/effect publication path has been explicitly closed.
    /// Does not assert task joining, callback settlement, or consumer commitment.
    pub fn seal(&self) {
        self.sealed.store(true, Ordering::Release);
    }
    /// Whether descendant publication has sealed without lost/failed task evidence.
    /// Actual `JoinHandle` results must still be joined before receipt release.
    #[must_use]
    pub fn publication_finished(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
            && self.active_tasks.load(Ordering::Acquire) == 0
            && !self.uncertain.load(Ordering::Acquire)
    }
}

/// Task-local explicit lineage guard; cancellation/drop is uncertainty, not settlement.
pub struct ResponseTask {
    context: Arc<ResponseContext>,
    finished: bool,
}
impl ResponseTask {
    /// Mark normal publication completion. The task owner must separately join its task.
    pub fn finish(mut self) {
        self.finished = true;
    }
}
impl Drop for ResponseTask {
    fn drop(&mut self) {
        if !self.finished {
            self.context.uncertain.store(true, Ordering::Release);
        }
        self.context.active_tasks.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Independently advertised optional frontend settlement capability and interface.
pub const CONTRACT: &str = "bcode.tui.frontend-settlement/v1";
/// Schema of the optional settlement envelope; frontend payloads remain schema 2.
pub const VERSION: u32 = 1;
/// Maximum number of responses not yet contiguously retired.
pub const MAX_IN_FLIGHT: usize = 32;

/// Stable response identity scoped to one actual selected activation session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseId {
    /// Host-bound negotiated session, never a resource credential.
    pub session_id: Uuid,
    /// Strictly increasing, nonzero sequence allocated before dispatch.
    pub sequence: u64,
}

/// Optional operations. These must not be decoded as ordinary frontend-v2 requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Reserve correlation and capacity before executing the unchanged frontend request.
    Execute {
        id: ResponseId,
        // Heap indirection changes only the Rust representation; Serde remains transparent.
        request: Box<FrontendRequest>,
    },
    /// Observe only; never redispatch an uncertain mutation.
    ObserveResponse { id: ResponseId },
    /// Consumer has committed state and retained responsibility for every effect.
    CommitResponse { id: ResponseId },
    /// Perform bounded recovery/settlement work, including after admission closes.
    Settle { session_id: Uuid, limit: u32 },
    /// Stop new execution, not observation or recovery.
    CloseAdmission { session_id: Uuid },
}

/// Versioned envelope for the optional interface only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    /// Must equal `VERSION`; unknown versions never reach dispatch.
    pub schema: u32,
    /// Correlated operation, not an ordinary frontend-v2 payload.
    pub request: Request,
}
impl RequestEnvelope {
    /// Decode within the existing request byte budget, before any admission or side effect.
    ///
    /// # Errors
    /// Rejects oversize, malformed, unknown-version and invalid bounded-control requests.
    pub fn decode(bytes: &[u8]) -> Result<Self, LedgerError> {
        if bytes.len() > crate::SERVICE_BRIDGE_MAX_REQUEST_BYTES {
            return Err(LedgerError::Capacity);
        }
        let envelope: Self =
            serde_json::from_slice(bytes).map_err(|_| LedgerError::InvalidRequest)?;
        if envelope.schema != VERSION {
            return Err(LedgerError::UnsupportedVersion);
        }
        match &envelope.request {
            Request::Execute { id, .. }
            | Request::ObserveResponse { id }
            | Request::CommitResponse { id } => {
                if id.session_id.is_nil() || id.sequence == 0 {
                    return Err(LedgerError::InvalidIdentity);
                }
            }
            Request::Settle { session_id, limit } => {
                if session_id.is_nil() || *limit == 0 || *limit > 32 {
                    return Err(LedgerError::InvalidRequest);
                }
            }
            Request::CloseAdmission { session_id } => {
                if session_id.is_nil() {
                    return Err(LedgerError::InvalidIdentity);
                }
            }
        }
        Ok(envelope)
    }
}

/// A normalized terminal frontend result; callback/provider-private errors never enter this wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FrontendResult {
    /// The existing schema-2 response, without reinterpretation.
    Completed(Box<super::Response>),
    /// A normalized failure, retained just like successful responses.
    Failed(LedgerError),
}

/// Versioned response to an optional settlement operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    /// Always `VERSION`; independent of the nested frontend response schema.
    pub schema: u32,
    /// Exact identity or session of the requested operation.
    pub response: Response,
}

/// Responses never imply commitment merely because the bytes were delivered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    /// Original work is still unresolved. The consumer may observe, never redispatch it.
    Pending { id: ResponseId },
    /// Exact retained result with its original identity.
    Result {
        id: ResponseId,
        result: FrontendResult,
        committed: bool,
    },
    /// Contiguous tombstone; never permission to repeat the original operation.
    Retired { id: ResponseId },
    /// Consumer commitment recorded; author jobs may still be unsettled.
    Committed {
        id: ResponseId,
        already_committed: bool,
    },
    /// Bounded settlement progress for this session, not a claim of effect replay safety.
    Settlement {
        session_id: Uuid,
        state: SettlementState,
    },
    /// Only new admission is closed; observation and recovery remain valid.
    AdmissionClosed { session_id: Uuid },
    /// Fail-closed rejection. Session and sequence are preserved in the enclosing call record.
    Rejected { error: LedgerError },
}

/// Settlement remains pending until actual task joining and confirmed job release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementState {
    /// More bounded observation or consumer commitment is required.
    Pending,
    /// All admitted records have verified settlement and contiguous retirement.
    Settled,
    /// Authority or outcome is uncertain; retain the selected owner and report it.
    RecoveryRequired,
}

/// Local admission/observation result. Retired identities never authorize execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// Original execution is admitted but no verified response is available.
    Pending,
    /// Exact retained encoded response; commitment does not erase it.
    Response { bytes: Vec<u8>, committed: bool },
    /// Contiguously retired identity. Its mutation must never be repeated.
    Retired,
}

/// Fail-closed correlation failures; none authorize fallback execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerError {
    /// Optional schema version is unsupported; never reinterpret as frontend-v2.
    UnsupportedVersion,
    /// Wrong session, nil identity, zero, out-of-order, or exhausted sequence.
    InvalidIdentity,
    /// Existing identity supplied with a different normalized request or response.
    ConflictingDuplicate,
    /// Slot or byte reservation unavailable before producer dispatch.
    Capacity,
    /// Admission is closed; recovery operations remain available.
    AdmissionClosed,
    /// Identity has not been admitted. Absence is not release evidence.
    UnknownResponse,
    /// No verified result or consumer commitment permits the requested transition.
    Unsettled,
    /// Encoding failed before admission.
    InvalidRequest,
}

struct Record {
    request: Vec<u8>,
    reservation: usize,
    response: Option<Vec<u8>>,
    committed: bool,
    released: bool,
}

/// Bounded author response records, not an independent product effect authority.
///
/// The existing product completion owner must retain effect responsibility before `commit`.
/// A record may retire only after its correlated callback jobs have confirmed release.
/// This in-memory ledger does not promise process-crash recovery.
pub struct ResponseLedger {
    session: Uuid,
    admitted: u64,
    retired: u64,
    closed: bool,
    byte_limit: usize,
    reserved: usize,
    records: BTreeMap<u64, Record>,
}
impl ResponseLedger {
    /// Construct a ledger for an already negotiated, actual session.
    ///
    /// # Errors
    /// Rejects a nil session or zero byte budget. Does not negotiate or grant authority.
    pub const fn new(session: Uuid, byte_limit: usize) -> Result<Self, LedgerError> {
        if session.is_nil() || byte_limit == 0 {
            return Err(LedgerError::InvalidIdentity);
        }
        Ok(Self {
            session,
            admitted: 0,
            retired: 0,
            closed: false,
            byte_limit,
            reserved: 0,
            records: BTreeMap::new(),
        })
    }

    fn validate(&self, id: ResponseId) -> Result<(), LedgerError> {
        if id.session_id != self.session || id.sequence == 0 {
            return Err(LedgerError::InvalidIdentity);
        }
        Ok(())
    }

    /// Reserve the exact request plus worst-case response capacity before dispatch.
    /// Returns `true` only for a new admission; duplicates must use `observe`.
    ///
    /// # Errors
    /// Rejects conflicting duplicates, invalid IDs, closed admission, or exhausted budgets.
    /// Retired IDs return `false`, never permission to execute again.
    pub fn admit(
        &mut self,
        id: ResponseId,
        request: &FrontendRequest,
        response_limit: usize,
    ) -> Result<bool, LedgerError> {
        self.validate(id)?;
        if id.sequence <= self.retired {
            return Ok(false);
        }
        // Normalize through the typed request; object field ordering is not identity.
        let value = serde_json::to_value(request).map_err(|_| LedgerError::InvalidRequest)?;
        let bytes = serde_json::to_vec(&value).map_err(|_| LedgerError::InvalidRequest)?;
        if let Some(record) = self.records.get(&id.sequence) {
            return if record.request == bytes {
                Ok(false)
            } else {
                Err(LedgerError::ConflictingDuplicate)
            };
        }
        if self.closed {
            return Err(LedgerError::AdmissionClosed);
        }
        if self.admitted.checked_add(1) != Some(id.sequence) {
            return Err(LedgerError::InvalidIdentity);
        }
        let reservation = bytes
            .len()
            .checked_add(response_limit)
            .and_then(|n| n.checked_add(256))
            .ok_or(LedgerError::Capacity)?;
        let reserved = self
            .reserved
            .checked_add(reservation)
            .ok_or(LedgerError::Capacity)?;
        if response_limit == 0 || self.records.len() >= MAX_IN_FLIGHT || reserved > self.byte_limit
        {
            return Err(LedgerError::Capacity);
        }
        self.records.insert(
            id.sequence,
            Record {
                request: bytes,
                reservation,
                response: None,
                committed: false,
                released: false,
            },
        );
        self.reserved = reserved;
        self.admitted = id.sequence;
        Ok(true)
    }

    /// Retain a verified encoded result without acknowledging any callback receipt.
    ///
    /// # Errors
    /// Rejects unknown/retired IDs, conflicting results, or reservation overflow.
    pub fn complete(&mut self, id: ResponseId, bytes: Vec<u8>) -> Result<(), LedgerError> {
        self.validate(id)?;
        let record = self
            .records
            .get_mut(&id.sequence)
            .ok_or(LedgerError::UnknownResponse)?;
        if let Some(original) = &record.response {
            return if *original == bytes {
                Ok(())
            } else {
                Err(LedgerError::ConflictingDuplicate)
            };
        }
        if bytes.len() > record.reservation - record.request.len() - 256 {
            return Err(LedgerError::Capacity);
        }
        record.response = Some(bytes);
        Ok(())
    }

    /// Observe exact retained bytes without dispatch, acknowledgment, or reclamation.
    ///
    /// # Errors
    /// Rejects foreign or unknown IDs. Missing state is never interpreted as release.
    pub fn observe(&self, id: ResponseId) -> Result<Observation, LedgerError> {
        self.validate(id)?;
        if id.sequence <= self.retired {
            return Ok(Observation::Retired);
        }
        let record = self
            .records
            .get(&id.sequence)
            .ok_or(LedgerError::UnknownResponse)?;
        Ok(record
            .response
            .as_ref()
            .map_or(Observation::Pending, |bytes| Observation::Response {
                bytes: bytes.clone(),
                committed: record.committed,
            }))
    }

    /// Record commitment asserted by the bound product completion/effect owner.
    /// This does not acknowledge receipts or imply author-task settlement.
    ///
    /// # Errors
    /// Rejects foreign/unknown IDs and responses without a verified result.
    pub fn commit(&mut self, id: ResponseId) -> Result<(), LedgerError> {
        self.validate(id)?;
        if id.sequence <= self.retired {
            return Ok(());
        }
        let record = self
            .records
            .get_mut(&id.sequence)
            .ok_or(LedgerError::UnknownResponse)?;
        if record.response.is_none() {
            return Err(LedgerError::Unsettled);
        }
        record.committed = true;
        Ok(())
    }

    /// Mark verified callback-job release after consumer commitment and actual task joining.
    /// Callers must supply that evidence through the negotiated settlement path, never delivery.
    /// Retires only a contiguous prefix; gaps continue consuming bounded slots.
    ///
    /// # Errors
    /// Rejects unknown IDs or records lacking consumer commitment.
    pub fn confirm_released(&mut self, id: ResponseId) -> Result<(), LedgerError> {
        self.validate(id)?;
        if id.sequence <= self.retired {
            return Ok(());
        }
        let record = self
            .records
            .get_mut(&id.sequence)
            .ok_or(LedgerError::UnknownResponse)?;
        if !record.committed {
            return Err(LedgerError::Unsettled);
        }
        record.released = true;
        while let Some(next) = self.retired.checked_add(1) {
            if !self
                .records
                .get(&next)
                .is_some_and(|record| record.released)
            {
                break;
            }
            let Some(record) = self.records.remove(&next) else {
                break;
            };
            self.reserved -= record.reservation;
            self.retired = next;
        }
        Ok(())
    }

    /// Check consumer commitment without treating result delivery or retired state as evidence.
    ///
    /// # Errors
    /// Rejects foreign/unknown identities; retired records require no further mutation.
    pub fn is_committed(&self, id: ResponseId) -> Result<bool, LedgerError> {
        self.validate(id)?;
        let record = self
            .records
            .get(&id.sequence)
            .ok_or(LedgerError::UnknownResponse)?;
        Ok(record.committed)
    }

    /// Stop new work while preserving duplicate observation, commitment and recovery.
    pub const fn close_admission(&mut self) {
        self.closed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(session_id: Uuid, sequence: u64) -> ResponseId {
        ResponseId {
            session_id,
            sequence,
        }
    }

    #[test]
    fn explicit_descendants_preserve_origin_and_drop_is_not_settlement() {
        let origin = id(Uuid::new_v4(), 1);
        let context = ResponseContext::new(origin);
        let task = context.reserve_task().unwrap();
        assert_eq!(context.id(), origin);
        context.seal();
        assert!(!context.publication_finished());
        assert!(context.reserve_task().is_err());
        task.finish();
        assert!(context.publication_finished());

        let uncertain = ResponseContext::new(origin);
        let lost = uncertain.reserve_task().unwrap();
        uncertain.seal();
        drop(lost);
        assert!(!uncertain.publication_finished());
    }

    #[test]
    fn descendant_capacity_is_reserved_before_dispatch() {
        let context = ResponseContext::new(id(Uuid::new_v4(), 1));
        let tasks: Vec<_> = (0..32).map(|_| context.reserve_task().unwrap()).collect();
        assert!(matches!(context.reserve_task(), Err(LedgerError::Capacity)));
        for task in tasks {
            task.finish();
        }
        context.seal();
        assert!(context.publication_finished());
    }

    #[test]
    fn boxed_execute_request_preserves_optional_wire_representation() {
        let session = Uuid::new_v4();
        let request = Request::Execute {
            id: id(session, 1),
            request: Box::new(FrontendRequest::Describe),
        };
        let expected = serde_json::json!({
            "operation": "execute",
            "id": { "session_id": session, "sequence": 1 },
            "request": { "operation": "describe" },
        });
        assert_eq!(serde_json::to_value(&request).unwrap(), expected);
        let decoded: Request = serde_json::from_value(expected).unwrap();
        assert!(matches!(decoded, Request::Execute { id: key, request }
            if key == id(session, 1) && matches!(*request, FrontendRequest::Describe)));
    }

    #[test]
    fn optional_codec_rejects_future_schema_without_changing_frontend_v2() {
        let ordinary = FrontendRequest::Describe;
        assert_eq!(
            serde_json::to_vec(&ordinary).unwrap(),
            br#"{"operation":"describe"}"#
        );
        assert!(serde_json::from_slice::<FrontendRequest>(
            br#"{"schema":1,"request":{"operation":"close_admission","session_id":"00000000-0000-0000-0000-000000000001"}}"#
        ).is_err());
        let envelope = RequestEnvelope {
            schema: 2,
            request: Request::CloseAdmission {
                session_id: Uuid::new_v4(),
            },
        };
        assert_eq!(
            RequestEnvelope::decode(&serde_json::to_vec(&envelope).unwrap()).unwrap_err(),
            LedgerError::UnsupportedVersion
        );
        let result = super::super::Response::Applied;
        assert_eq!(
            serde_json::to_vec(&result).unwrap(),
            br#"{"response":"applied"}"#
        );
    }

    #[test]
    fn delivery_without_commit_cannot_retire_or_reclaim() {
        let session = Uuid::new_v4();
        let key = id(session, 1);
        let mut ledger = ResponseLedger::new(session, 4096).unwrap();
        ledger.admit(key, &FrontendRequest::Describe, 8).unwrap();
        ledger.complete(key, vec![1]).unwrap();
        for _ in 0..3 {
            assert_eq!(
                ledger.observe(key),
                Ok(Observation::Response {
                    bytes: vec![1],
                    committed: false,
                })
            );
            assert_eq!(ledger.confirm_released(key), Err(LedgerError::Unsettled));
        }
        assert_eq!(
            ledger.complete(key, vec![2]),
            Err(LedgerError::ConflictingDuplicate)
        );
        assert_eq!(
            ledger.observe(id(Uuid::new_v4(), 1)),
            Err(LedgerError::InvalidIdentity)
        );
        assert_eq!(
            ledger.observe(id(session, 2)),
            Err(LedgerError::UnknownResponse)
        );
    }

    #[test]
    fn duplicates_conflicts_and_retired_ids_never_dispatch_again() {
        let session = Uuid::new_v4();
        let mut ledger = ResponseLedger::new(session, 4096).unwrap();
        let key = id(session, 1);
        assert!(ledger.admit(key, &FrontendRequest::Describe, 128).unwrap());
        assert!(!ledger.admit(key, &FrontendRequest::Describe, 128).unwrap());
        assert_eq!(
            ledger.admit(key, &FrontendRequest::DrainText, 128),
            Err(LedgerError::ConflictingDuplicate)
        );
        assert_eq!(ledger.commit(key), Err(LedgerError::Unsettled));
        ledger.complete(key, b"response".to_vec()).unwrap();
        assert_eq!(ledger.confirm_released(key), Err(LedgerError::Unsettled));
        ledger.commit(key).unwrap();
        ledger.confirm_released(key).unwrap();
        assert_eq!(ledger.observe(key), Ok(Observation::Retired));
        assert!(!ledger.admit(key, &FrontendRequest::DrainText, 128).unwrap());
    }

    #[test]
    fn slots_and_bytes_are_reserved_before_dispatch() {
        let session = Uuid::new_v4();
        let mut small = ResponseLedger::new(session, 1).unwrap();
        assert_eq!(
            small.admit(id(session, 1), &FrontendRequest::Describe, 128),
            Err(LedgerError::Capacity)
        );
        let mut ledger = ResponseLedger::new(session, 32 * 4096).unwrap();
        for sequence in 1..=32 {
            assert!(
                ledger
                    .admit(id(session, sequence), &FrontendRequest::Describe, 128)
                    .unwrap()
            );
        }
        assert_eq!(
            ledger.admit(id(session, 33), &FrontendRequest::Describe, 128),
            Err(LedgerError::Capacity)
        );
        let second = id(session, 2);
        ledger.complete(second, vec![]).unwrap();
        ledger.commit(second).unwrap();
        ledger.confirm_released(second).unwrap();
        assert_eq!(
            ledger.admit(id(session, 33), &FrontendRequest::Describe, 128),
            Err(LedgerError::Capacity)
        );
        let first = id(session, 1);
        ledger.complete(first, vec![]).unwrap();
        ledger.commit(first).unwrap();
        ledger.confirm_released(first).unwrap();
        assert!(
            ledger
                .admit(id(session, 33), &FrontendRequest::Describe, 128)
                .unwrap()
        );
    }

    #[test]
    fn closed_admission_keeps_observation_and_commitment_available() {
        let session = Uuid::new_v4();
        let key = id(session, 1);
        let mut ledger = ResponseLedger::new(session, 4096).unwrap();
        ledger.admit(key, &FrontendRequest::Describe, 128).unwrap();
        ledger.close_admission();
        assert_eq!(ledger.observe(key), Ok(Observation::Pending));
        assert_eq!(
            ledger.admit(id(session, 2), &FrontendRequest::Describe, 128),
            Err(LedgerError::AdmissionClosed)
        );
        ledger.complete(key, vec![1]).unwrap();
        assert_eq!(
            ledger.observe(key),
            Ok(Observation::Response {
                bytes: vec![1],
                committed: false
            })
        );
        ledger.commit(key).unwrap();
        ledger.confirm_released(key).unwrap();
    }
}
