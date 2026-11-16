//! Adapter bookkeeping for the generic retained port; B4's product ledger remains
//! the sole authority for jobs, effects, idempotency and product acknowledgement.

use super::{
    HostSessionClientError, HostSessionFuture, ProductHostSessionClient, ProductHostSessionPort,
};
#[cfg(test)]
mod tests;
use crate::definition::host_session_models::{
    HostSessionRequest, HostSessionResponse, MAX_SESSION_ENVELOPE_BYTES,
    PRODUCT_HOST_SESSION_CONTRACT, PRODUCT_HOST_SESSION_VERSION, SessionBudget, SessionCode,
};
use crate::definition::{PluginCallContext, PluginCreateContext};
use bmux_plugin_sdk::abi::retained::{
    NativeRetainedReservationV1, RETAINED_HOST_RECORD_HEADER_BYTES, RETAINED_HOST_VERSION,
    RetainedHostObservation, RetainedHostOperation, RetainedHostOutcome, RetainedHostPort,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Executor-neutral timer. Implement with a real asynchronous runtime timer;
/// implementations must not block a poll or immediately self-wake in a spin loop.
pub trait RetainedSessionWait: Send + Sync {
    /// Suspend observation, without cancelling/acknowledging admitted producers.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Real asynchronous timer for the existing Tokio-backed frontend feature.
/// Construct only inside a runtime with its time driver enabled.
#[cfg(feature = "tui")]
pub struct TokioRetainedSessionWait;
#[cfg(feature = "tui")]
impl RetainedSessionWait for TokioRetainedSessionWait {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Exact, bounded negotiation reply returned by the host through the retained port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionNegotiation {
    /// Must equal the requested product contract, not an alias/fallback.
    pub contract: String,
    /// Exact independently negotiated schema version.
    pub version: u32,
    /// Host-selected policy, never reconstructed from caller authority claims.
    pub budget: SessionBudget,
}

/// Author-local capability captured while the invoking host port is still live.
///
/// Only the independent retained port and exact creation context survive capture.
/// There is no serde implementation and no retained invocation callback fallback.
#[derive(Clone)]
pub struct ProductAuthorContext {
    scope: PluginCreateContext,
    retained: RetainedHostPort,
    session: Arc<Mutex<Option<Arc<RetainedProductHostSession>>>>,
}
impl ProductAuthorContext {
    /// Open the independent capability synchronously while this call is valid.
    ///
    /// # Errors
    /// Returns unsupported when no exact retained opener is bound. No request-local
    /// service port is retained or resurrected after this function returns.
    pub fn capture(
        scope: PluginCreateContext,
        call: &PluginCallContext,
    ) -> Result<Self, HostSessionClientError> {
        let retained = call
            .host_services
            .as_ref()
            .ok_or(HostSessionClientError::Unsupported)?
            .open_retained_host()
            .map_err(|error| classify_open_error(&error))?;
        Ok(Self {
            scope,
            retained,
            session: Arc::new(Mutex::new(None)),
        })
    }
    /// Capture the explicitly bound recovery-V2 capability while this call is live.
    ///
    /// # Errors
    /// Missing V2 negotiation is unsupported. Denial/corruption stays fail-closed;
    /// this method never invokes the ordinary V1 opener after any failure.
    pub fn capture_recovery_v2(
        scope: PluginCreateContext,
        call: &PluginCallContext,
    ) -> Result<Self, HostSessionClientError> {
        let retained = call
            .host_services
            .as_ref()
            .ok_or(HostSessionClientError::Unsupported)?
            .open_retained_host_v2()
            .map_err(|error| classify_open_error(&error))?;
        Ok(Self {
            scope,
            retained,
            session: Arc::new(Mutex::new(None)),
        })
    }

    /// Capture an explicitly negotiated native recovery-V2 table, never V1 fallback.
    ///
    /// # Safety
    /// `ports` must be the currently valid authenticated invocation table. The
    /// generic SDK checks the exact V2 tail and independently retains its owner.
    /// # Errors
    /// Rejected/unsupported acquisition preserves failure without reopening V1.
    pub unsafe fn capture_native_v2(
        scope: PluginCreateContext,
        ports: *const bmux_plugin_sdk::abi::NativeInvocationPorts,
    ) -> Result<Self, HostSessionClientError> {
        let retained = unsafe { RetainedHostPort::open_native_v2(ports) }
            .map_err(|error| classify_open_error(&error))?;
        Ok(Self {
            scope,
            retained,
            session: Arc::new(Mutex::new(None)),
        })
    }
    /// Exact host-issued instance context; this observation alone grants nothing.
    #[must_use]
    pub const fn scope(&self) -> &PluginCreateContext {
        &self.scope
    }
    /// Negotiate over the independent capability, never the expired invocation.
    ///
    /// # Errors
    /// Returns unsupported, local bound failures or retained uncertainty. Keep the
    /// returned adapter on negotiation failure to observe/recover its transport ID.
    pub fn retained_session(
        &self,
        wait: Arc<dyn RetainedSessionWait>,
    ) -> Result<Arc<RetainedProductHostSession>, HostSessionClientError> {
        let mut stored = self.session.lock().map_err(|_| uncertain())?;
        let session = stored
            .get_or_insert_with(|| {
                Arc::new(RetainedProductHostSession::new(
                    self.retained.clone(),
                    self.scope.clone(),
                    wait,
                ))
            })
            .clone();
        drop(stored);
        Ok(session)
    }
}

struct Record {
    operation: Option<RetainedHostOperation>,
    publishing: bool,
    cached: Option<Vec<u8>>,
    // None for negotiation. Presence preserves full UUID facts unchanged.
    request: Option<HostSessionRequest>,
    receipt: Option<u64>,
    acknowledged: bool,
    reserved: u64,
    outcome_capacity: u64,
    facts: bmux_plugin_sdk::HostServiceOwnedRequest,
    reservation: NativeRetainedReservationV1,
}
struct State {
    next_id: u64,
    records: BTreeMap<u64, Record>,
    negotiation: Option<SessionNegotiation>,
    reserved: u64,
    released: bool,
    admissions_closed: bool,
}
/// One explicitly retained transport adapter, not a second product execution ledger.
///
/// Transport IDs are monotonic u64s; full product UUIDs stay in the encoded payload.
/// Records persist through uncertain reads and observer drops until explicit ack;
/// identity tombstones remain until the transport owner is released.
pub struct RetainedProductHostSession {
    port: RetainedHostPort,
    scope: PluginCreateContext,
    wait: Arc<dyn RetainedSessionWait>,
    state: Mutex<State>,
}
impl RetainedProductHostSession {
    /// Bind an actual M1 port obtained through selected host authority.
    #[must_use]
    pub fn new(
        port: RetainedHostPort,
        scope: PluginCreateContext,
        wait: Arc<dyn RetainedSessionWait>,
    ) -> Self {
        Self {
            port,
            scope,
            wait,
            state: Mutex::new(State {
                next_id: 1,
                records: BTreeMap::new(),
                negotiation: None,
                reserved: 0,
                released: false,
                admissions_closed: false,
            }),
        }
    }
    /// Observe the exact selected creation context, without serializing authority.
    #[must_use]
    pub const fn scope(&self) -> &PluginCreateContext {
        &self.scope
    }
    /// Negotiate once and bind the same typed author client used by static hosts.
    ///
    /// # Errors
    /// A failed/uncertain negotiation remains recoverable as transport ID 1. This
    /// method never starts a second negotiation with a fresh transport identity.
    pub async fn negotiate(
        self: &Arc<Self>,
    ) -> Result<ProductHostSessionClient, HostSessionClientError> {
        let existing = {
            let state = self.state.lock().map_err(|_| uncertain())?;
            if state.released {
                return Err(HostSessionClientError::Invalid(
                    SessionCode::AdmissionsClosed,
                ));
            }
            let negotiation = state.negotiation.clone();
            drop(state);
            negotiation
        };
        if existing.is_none() {
            let id = self.begin(None)?;
            let bytes = self.read(id).await?;
            let negotiation: SessionNegotiation =
                serde_json::from_slice(&bytes).map_err(|_| uncertain())?;
            if negotiation.contract != PRODUCT_HOST_SESSION_CONTRACT
                || negotiation.version != PRODUCT_HOST_SESSION_VERSION
            {
                return Err(HostSessionClientError::Unsupported);
            }
            negotiation
                .budget
                .validate()
                .map_err(HostSessionClientError::Invalid)?;
            let mut state = self.state.lock().map_err(|_| uncertain())?;
            if state
                .negotiation
                .as_ref()
                .is_some_and(|old| old != &negotiation)
            {
                return Err(uncertain());
            }
            state.negotiation = Some(negotiation);
        }
        // Negotiation is safely consumed/verified before transport-only ack. Failed
        // ack leaves the same operation+receipt for retry; no product ack is sent.
        self.acknowledge_transport(1)?;
        let state = self.state.lock().map_err(|_| uncertain())?;
        let budget = state.negotiation.as_ref().ok_or_else(uncertain)?.budget;
        // Negotiation keeps one 512-byte identity tombstone and one slot. A
        // session unable to accommodate it plus a request fails closed.
        if state.reserved > budget.retained_bytes || budget.max_operations < 2 {
            return Err(HostSessionClientError::Invalid(
                SessionCode::CapacityExceeded,
            ));
        }
        drop(state);
        ProductHostSessionClient::bind(self.clone())
    }
    fn begin(&self, request: Option<HostSessionRequest>) -> Result<u64, HostSessionClientError> {
        if let Some(request) = &request {
            request
                .validate()
                .map_err(HostSessionClientError::Invalid)?;
        }
        let mut state = self.state.lock().map_err(|_| uncertain())?;
        if state.released {
            return Err(HostSessionClientError::Invalid(
                SessionCode::AdmissionsClosed,
            ));
        }
        if request.is_none() && state.records.contains_key(&1) {
            return Ok(1);
        }
        if let Some(
            request @ (HostSessionRequest::BeginJob { .. } | HostSessionRequest::Start { .. }),
        ) = &request
            && let Some((id, _)) = state
                .records
                .iter()
                .find(|(_, record)| record.request.as_ref() == Some(request))
        {
            return Ok(*id);
        }
        if let Some(request) = &request
            && state.records.values().any(|record| {
                record
                    .request
                    .as_ref()
                    .is_some_and(|old| conflicts(old, request))
            })
        {
            return Err(HostSessionClientError::Invalid(
                SessionCode::ConflictingRequest,
            ));
        }
        if state.admissions_closed
            && matches!(
                request,
                Some(HostSessionRequest::BeginJob { .. } | HostSessionRequest::Start { .. })
            )
        {
            return Err(HostSessionClientError::Invalid(
                SessionCode::AdmissionsClosed,
            ));
        }
        // Retain this guard through reservation and serialization: duplicate
        // admission and aggregate accounting remain one atomic transition.
        let ReservedRequest { id, request_len } = reserve_request(&mut state, request)?;
        // All byte/slot capacity is charged now. Serializing into exactly counted
        // capacity cannot introduce an unreserved request buffer.
        let mut payload = Vec::with_capacity(request_len);
        if let Some(request) = state
            .records
            .get(&id)
            .and_then(|record| record.request.as_ref())
            && (serde_json::to_writer(&mut payload, request).is_err()
                || payload.len() != request_len)
        {
            state.records.get_mut(&id).ok_or_else(uncertain)?.publishing = false;
            return Err(uncertain());
        }
        state
            .records
            .get_mut(&id)
            .ok_or_else(uncertain)?
            .facts
            .payload = payload;
        drop(state);
        // Retain the published request identity even if begin's response is lost.
        // Never retry begin after an ambiguous result or under a different ID.
        let (facts, reservation) = {
            let state = self.state.lock().map_err(|_| uncertain())?;
            let record = state.records.get(&id).ok_or_else(uncertain)?;
            let values = (record.facts.clone(), record.reservation);
            drop(state);
            values
        };
        let operation = self.port.begin(id, &facts.as_request(), reservation);
        let mut state = self.state.lock().map_err(|_| uncertain())?;
        let record = state.records.get_mut(&id).ok_or_else(uncertain)?;
        record.publishing = false;
        match operation {
            Ok(operation) => record.operation = Some(operation),
            Err(_) => return Err(uncertain()),
        }
        drop(state);
        Ok(id)
    }
    async fn read(&self, id: u64) -> Result<Vec<u8>, HostSessionClientError> {
        let mut publish_delay = 1;
        let operation = loop {
            if let Some(bytes) = self.cached_bytes(id)? {
                return Ok(bytes);
            }
            if let Some(operation) = self.published_operation(id)? {
                break operation;
            }
            self.wait.sleep(Duration::from_millis(publish_delay)).await;
            publish_delay = (publish_delay * 2).min(32);
        };
        let mut delay = 1;
        loop {
            if let Some(bytes) = self.cached_bytes(id)? {
                return Ok(bytes);
            }
            let Ok(observation) = operation.observe() else {
                return self.cached_bytes(id)?.ok_or_else(uncertain);
            };
            match observation {
                RetainedHostObservation::Pending => {
                    self.wait.sleep(Duration::from_millis(delay)).await;
                    delay = (delay * 2).min(32);
                }
                RetainedHostObservation::Terminal { receipt, outcome } => {
                    let mut state = self.state.lock().map_err(|_| uncertain())?;
                    let record = state.records.get_mut(&id).ok_or_else(uncertain)?;
                    if record.receipt.is_some_and(|old| old != receipt) {
                        return Err(uncertain());
                    }
                    record.receipt = Some(receipt);
                    if let RetainedHostOutcome::Completed(bytes) = &outcome {
                        let observed = (bytes.len() as u64)
                            .checked_add(u64::from(RETAINED_HOST_RECORD_HEADER_BYTES))
                            .ok_or_else(uncertain)?;
                        if observed > record.outcome_capacity
                            || bytes.len() > MAX_SESSION_ENVELOPE_BYTES
                        {
                            return Err(uncertain());
                        }
                    }
                    if let RetainedHostOutcome::Completed(bytes) = &outcome {
                        if let Some(previous) = &record.cached {
                            if previous != bytes {
                                return Err(uncertain());
                            }
                        } else {
                            record.cached = Some(bytes.clone());
                        }
                    }
                    drop(state);
                    return match outcome {
                        RetainedHostOutcome::Completed(bytes)
                            if bytes.len() <= MAX_SESSION_ENVELOPE_BYTES =>
                        {
                            Ok(bytes)
                        }
                        _ => Err(uncertain()),
                    };
                }
            }
        }
    }
    fn published_operation(
        &self,
        id: u64,
    ) -> Result<Option<RetainedHostOperation>, HostSessionClientError> {
        let state = self.state.lock().map_err(|_| uncertain())?;
        let record = state.records.get(&id).ok_or_else(uncertain)?;
        let operation = record.operation.clone();
        let publishing = record.publishing;
        drop(state);
        if operation.is_none() && !publishing {
            return self.lookup_original(id).map(Some);
        }
        Ok(operation)
    }
    fn lookup_original(&self, id: u64) -> Result<RetainedHostOperation, HostSessionClientError> {
        let state = self.state.lock().map_err(|_| uncertain())?;
        let record = state.records.get(&id).ok_or_else(uncertain)?;
        let facts = record.facts.clone();
        let reservation = record.reservation;
        drop(state);
        // Strictly lookup; None/unsupported/conflict never invokes begin or
        // allocates a replacement request identity.
        let operation = self
            .port
            .lookup(id, &facts.as_request(), reservation)
            .map_err(|_| uncertain())?
            .ok_or_else(uncertain)?;
        let mut state = self.state.lock().map_err(|_| uncertain())?;
        let record = state.records.get_mut(&id).ok_or_else(uncertain)?;
        if record
            .operation
            .as_ref()
            .is_some_and(|old| old.id() != operation.id())
        {
            return Err(uncertain());
        }
        record.operation = Some(operation.clone());
        drop(state);
        Ok(operation)
    }
    fn cached_bytes(&self, id: u64) -> Result<Option<Vec<u8>>, HostSessionClientError> {
        let state = self.state.lock().map_err(|_| uncertain())?;
        if id == 1
            && let Some(negotiation) = state.negotiation.clone()
        {
            drop(state);
            return serde_json::to_vec(&negotiation)
                .map(Some)
                .map_err(|_| uncertain());
        }
        let bytes = state.records.get(&id).ok_or_else(uncertain)?.cached.clone();
        drop(state);
        Ok(bytes)
    }
    /// Admit one protocol operation and expose its transport identity before
    /// awaiting. On ambiguous begin failure, `transport_ids` still contains the
    /// published identity; never issue a replacement effect with a fresh ID.
    ///
    /// # Errors
    /// Returns pre-admission bounds errors or uncertainty, retaining all records.
    pub fn begin_transport_request(
        &self,
        request: HostSessionRequest,
    ) -> Result<u64, HostSessionClientError> {
        self.begin(Some(request))
    }
    /// Non-consuming recovery of the original transport call; no begin/replay.
    ///
    /// # Errors
    /// Returns uncertainty for unknown IDs, ambiguous begin or malformed outcomes.
    pub async fn recover(
        &self,
        transport_id: u64,
    ) -> Result<HostSessionResponse, HostSessionClientError> {
        let bytes = self.read(transport_id).await?;
        let response: HostSessionResponse =
            serde_json::from_slice(&bytes).map_err(|_| uncertain())?;
        let state = self.state.lock().map_err(|_| uncertain())?;
        let request = state
            .records
            .get(&transport_id)
            .and_then(|record| record.request.as_ref())
            .ok_or_else(uncertain)?;
        super::validate_response(request, &response).map_err(HostSessionClientError::Uncertain)?;
        drop(state);
        if matches!(response, HostSessionResponse::AdmissionsClosed)
            && matches!(
                self.transport_request(transport_id)?,
                Some(HostSessionRequest::CloseSession)
            )
        {
            self.state
                .lock()
                .map_err(|_| uncertain())?
                .admissions_closed = true;
        }
        Ok(response)
    }
    /// Bounded outstanding transport identities, including uncertainty/tombstones.
    ///
    /// # Errors
    /// Returns local state failure; IDs never confer authority outside this adapter.
    pub fn transport_ids(&self) -> Result<Vec<u64>, HostSessionClientError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| uncertain())?
            .records
            .keys()
            .copied()
            .collect())
    }
    /// Exact request/identity retained for one transport ID. Observation never
    /// allocates a new transport ID or calls begin again.
    ///
    /// # Errors
    /// Returns uncertainty for unknown IDs; payloads remain bounded/private.
    pub fn transport_request(
        &self,
        id: u64,
    ) -> Result<Option<HostSessionRequest>, HostSessionClientError> {
        let state = self.state.lock().map_err(|_| uncertain())?;
        let request = state
            .records
            .get(&id)
            .ok_or_else(uncertain)?
            .request
            .clone();
        drop(state);
        Ok(request)
    }
    /// Accept only the transport terminal receipt. Product Acknowledge is a
    /// separate serialized request; neither operation establishes producer join.
    ///
    /// # Errors
    /// Keeps the original operation/receipt on failure for exact acknowledgement retry.
    pub fn acknowledge_transport(&self, id: u64) -> Result<(), HostSessionClientError> {
        let state = self.state.lock().map_err(|_| uncertain())?;
        let record = state.records.get(&id).ok_or_else(uncertain)?;
        if record.acknowledged {
            return Ok(());
        }
        let operation = record.operation.clone().ok_or_else(uncertain)?;
        let receipt = record.receipt.ok_or_else(uncertain)?;
        drop(state);
        operation.acknowledge(receipt).map_err(|_| uncertain())?;
        let mut state = self.state.lock().map_err(|_| uncertain())?;
        let record = state.records.get_mut(&id).ok_or_else(uncertain)?;
        if record.acknowledged {
            return Ok(());
        }
        record.acknowledged = true;
        // Negotiation's verified data is held in State; reclaim its transport
        // outcome only. Other cached terminal replies stay charged/recoverable
        // even when the host reclaims its outcome after explicit transport ack.
        if id == 1 {
            record.cached = None;
            let reclaimed = record.reserved.saturating_sub(512);
            record.reserved = 512;
            state.reserved = state.reserved.saturating_sub(reclaimed);
        }
        drop(state);
        Ok(())
    }
    /// Explicit generic control (CANCEL/CLOSE/JOIN), never implicit on Drop.
    ///
    /// # Errors
    /// False is pending/busy and retains all ownership.
    pub fn control_transport(&self, control: u32) -> Result<bool, HostSessionClientError> {
        let completed = self.port.control(control).map_err(|_| uncertain())?;
        if completed && control == bmux_plugin_sdk::abi::retained::RETAINED_HOST_CONTROL_CLOSE {
            self.state
                .lock()
                .map_err(|_| uncertain())?
                .admissions_closed = true;
        }
        Ok(completed)
    }
    /// Release the generic owner only after host-verified closure/join/ack.
    ///
    /// # Errors
    /// Busy/uncertain release preserves authority and recovery records.
    pub fn release_transport(&self) -> Result<bool, HostSessionClientError> {
        let released = self.port.release().map_err(|_| uncertain())?;
        if released {
            let mut state = self.state.lock().map_err(|_| uncertain())?;
            state.records.clear();
            state.reserved = 0;
            state.released = true;
        }
        Ok(released)
    }
}
impl ProductHostSessionPort for RetainedProductHostSession {
    fn contract(&self) -> (&str, u32) {
        (PRODUCT_HOST_SESSION_CONTRACT, PRODUCT_HOST_SESSION_VERSION)
    }
    fn budget(&self) -> SessionBudget {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.negotiation.as_ref().map(|reply| reply.budget))
            .unwrap_or(SessionBudget {
                max_jobs: 0,
                max_operations: 0,
                max_request_bytes: 0,
                max_response_bytes: 0,
                retained_bytes: 0,
            })
    }
    fn request(&self, request: HostSessionRequest) -> HostSessionFuture<'_> {
        Box::pin(async move {
            let id = self.begin(Some(request))?;
            self.recover(id).await
        })
    }
}
struct ReservedRequest {
    id: u64,
    request_len: usize,
}

// Caller holds the same admission mutex before and after this helper. Count and
// reserve all bytes/slots, then publish the recoverable identity before allocation.
fn reserve_request(
    state: &mut State,
    request: Option<HostSessionRequest>,
) -> Result<ReservedRequest, HostSessionClientError> {
    let (max_request, max_response, slots, byte_limit) = match &state.negotiation {
        Some(reply) => (
            reply.budget.max_request_bytes as usize,
            (reply.budget.max_response_bytes as usize).min(
                MAX_SESSION_ENVELOPE_BYTES
                    .saturating_sub(RETAINED_HOST_RECORD_HEADER_BYTES as usize),
            ),
            u64::from(reply.budget.max_operations),
            reply.budget.retained_bytes,
        ),
        None if request.is_none() => (0, 4096, 1, 8192),
        None => return Err(HostSessionClientError::Unsupported),
    };
    // Count escaped identifiers, envelope tags and byte-array encoding, not
    // just original payload bytes, before allocation or budget admission.
    let request_len = request
        .as_ref()
        .map_or(Ok(0), |request| encoded_size(request, max_request))?;
    let capacity = (max_response as u64)
        .checked_add(u64::from(RETAINED_HOST_RECORD_HEADER_BYTES))
        .ok_or_else(uncertain)?;
    if capacity > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES as u64 {
        return Err(HostSessionClientError::Invalid(SessionCode::InvalidBudget));
    }
    let reserved = capacity
        .checked_add((request_len as u64).checked_mul(2).ok_or_else(uncertain)?)
        .and_then(|n| n.checked_add(512))
        .ok_or_else(uncertain)?;
    let total = state.reserved.checked_add(reserved).ok_or_else(uncertain)?;
    if state.records.len() as u64 >= slots || total > byte_limit {
        return Err(HostSessionClientError::Invalid(
            SessionCode::CapacityExceeded,
        ));
    }
    let id = state.next_id;
    state.next_id = id.checked_add(1).ok_or_else(uncertain)?;
    state.reserved = total;
    state.records.insert(
        id,
        Record {
            operation: None,
            publishing: true,
            cached: None,
            request,
            receipt: None,
            acknowledged: false,
            reserved,
            outcome_capacity: capacity,
            facts: bmux_plugin_sdk::HostServiceOwnedRequest {
                capability: PRODUCT_HOST_SESSION_CONTRACT.to_owned(),
                interface: PRODUCT_HOST_SESSION_CONTRACT.to_owned(),
                operation: if id == 1 { "negotiate" } else { "request" }.to_owned(),
                kind: bmux_plugin_sdk::ServiceKind::Query,
                payload: Vec::new(),
            },
            reservation: NativeRetainedReservationV1 {
                size: u32::try_from(std::mem::size_of::<NativeRetainedReservationV1>())
                    .map_err(|_| uncertain())?,
                version: RETAINED_HOST_VERSION,
                max_outcome_bytes: capacity,
                operation_slots: 1,
                reserved: 0,
            },
        },
    );
    Ok(ReservedRequest { id, request_len })
}

const fn uncertain() -> HostSessionClientError {
    HostSessionClientError::Uncertain(SessionCode::Unavailable)
}

fn classify_open_error(error: &bmux_plugin_sdk::PluginError) -> HostSessionClientError {
    match error {
        bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "open_retained_host" | "open_retained_host_v2",
        } => HostSessionClientError::Unsupported,
        bmux_plugin_sdk::PluginError::CapabilityAccessDenied { .. } => {
            HostSessionClientError::Uncertain(SessionCode::PermissionDenied)
        }
        _ => HostSessionClientError::Uncertain(SessionCode::Unavailable),
    }
}
fn encoded_size(
    value: &impl serde::Serialize,
    limit: usize,
) -> Result<usize, HostSessionClientError> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|total| *total <= self.limit)
                .ok_or_else(|| std::io::Error::other("request bound"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        bytes: 0,
        limit: limit.min(MAX_SESSION_ENVELOPE_BYTES),
    };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| HostSessionClientError::Invalid(SessionCode::RequestLimitExceeded))?;
    Ok(counter.bytes)
}

fn conflicts(left: &HostSessionRequest, right: &HostSessionRequest) -> bool {
    match (left, right) {
        (
            HostSessionRequest::BeginJob { request_id: a, .. },
            HostSessionRequest::BeginJob { request_id: b, .. },
        ) => a == b && left != right,
        (
            HostSessionRequest::Start {
                job: a,
                request_id: ar,
                ..
            },
            HostSessionRequest::Start {
                job: b,
                request_id: br,
                ..
            },
        ) => a == b && ar == br && left != right,
        _ => false,
    }
}
