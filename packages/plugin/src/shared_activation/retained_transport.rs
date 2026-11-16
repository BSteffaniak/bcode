//! Generic retained transport records live in the selected product session ledger.
#[cfg(test)]
mod tests;

use super::{Arc, ProductHostSession, SessionBudget, host_session_error};
use bcode_plugin_sdk::definition::{
    host_session::{HostSessionClientError, ProductHostSessionPort},
    host_session_models::{
        HostSessionRequest, MAX_SESSION_ENVELOPE_BYTES, PRODUCT_HOST_SESSION_CONTRACT,
        PRODUCT_HOST_SESSION_VERSION,
    },
};
use bmux_plugin_sdk::abi::retained::{
    NativeRetainedReservationV1, RETAINED_HOST_CONTROL_CLOSE, RETAINED_HOST_CONTROL_JOIN,
    RetainedHostDispatch, RetainedHostObservation, RetainedHostOutcome,
};
use std::{collections::BTreeMap, sync::atomic::Ordering};

#[derive(Default)]
pub(super) struct TransportLedger {
    joined: bool,
    next: u64,
    bytes: u64,
    records: BTreeMap<u64, TransportRecord>,
}
struct TransportRecord {
    operation: u64,
    facts: bmux_plugin_sdk::HostServiceOwnedRequest,
    reservation: NativeRetainedReservationV1,
    outcome: Option<RetainedHostOutcome>,
    acknowledged: bool,
}
struct Completion {
    session: ProductHostSession,
    request: u64,
}
impl Completion {
    fn complete(&self, outcome: RetainedHostOutcome) {
        if let Ok(mut ledger) = self.session.inner.transport.lock()
            && let Some(record) = ledger.records.get_mut(&self.request)
            && record.outcome.is_none()
        {
            record.outcome = Some(outcome);
        }
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        self.complete(RetainedHostOutcome::EffectsUnresolved { observed_bytes: 0 });
    }
}

/// Count before allocating; the limit includes the generic 16-byte record header.
pub(super) fn validate_encoding(
    value: &impl serde::Serialize,
    limit: u64,
) -> bmux_plugin_sdk::Result<usize> {
    struct Counter {
        bytes: u64,
        limit: u64,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len() as u64)
                .filter(|bytes| *bytes <= self.limit)
                .ok_or_else(|| std::io::Error::other("response bound"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 16, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| host_session_error("retained response bound"))?;
    usize::try_from(counter.bytes.saturating_sub(16))
        .map_err(|_| host_session_error("response size overflow"))
}
fn encode(value: &impl serde::Serialize, limit: u64) -> bmux_plugin_sdk::Result<Vec<u8>> {
    let size = validate_encoding(value, limit)?;
    let mut bytes = Vec::with_capacity(size);
    serde_json::to_writer(&mut bytes, value)
        .map_err(|_| host_session_error("retained response encoding"))?;
    Ok(bytes)
}
impl ProductHostSession {
    /// Expose the same selected session ledger through the neutral retained table.
    #[must_use]
    pub fn retained_port(&self) -> bmux_plugin_sdk::abi::retained::RetainedHostPort {
        bmux_plugin_sdk::abi::retained::RetainedHostPort::new(Arc::new(self.clone()))
    }
    async fn transport_request(
        &self,
        operation: &str,
        payload: &[u8],
        limit: u64,
    ) -> RetainedHostOutcome {
        let response = if operation == "negotiate" {
            #[derive(serde::Serialize)]
            struct Negotiation<'a> {
                contract: &'a str,
                version: u32,
                budget: SessionBudget,
            }
            encode(
                &Negotiation {
                    contract: PRODUCT_HOST_SESSION_CONTRACT,
                    version: PRODUCT_HOST_SESSION_VERSION,
                    budget: self.budget(),
                },
                limit,
            )
        } else {
            let Ok(request) = serde_json::from_slice::<HostSessionRequest>(payload) else {
                return RetainedHostOutcome::RejectedBeforeEffects;
            };
            match self.request(request).await {
                Ok(response) => encode(&response, limit),
                Err(HostSessionClientError::Invalid(_) | HostSessionClientError::Unsupported) => {
                    return RetainedHostOutcome::RejectedBeforeEffects;
                }
                Err(HostSessionClientError::Uncertain(_)) => {
                    return RetainedHostOutcome::EffectsUnresolved { observed_bytes: 0 };
                }
            }
        };
        response.map_or_else(
            |_| RetainedHostOutcome::ResponseLimitExceeded { observed_bytes: 0 },
            RetainedHostOutcome::Completed,
        )
    }
}
impl RetainedHostDispatch for ProductHostSession {
    fn begin(
        &self,
        request_id: u64,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> bmux_plugin_sdk::Result<u64> {
        reservation
            .validate()
            .map_err(|_| host_session_error("invalid transport reservation"))?;
        if request_id == 0
            || request.kind != bmux_plugin_sdk::ServiceKind::Query
            || request.capability != PRODUCT_HOST_SESSION_CONTRACT
            || request.interface != PRODUCT_HOST_SESSION_CONTRACT
            || !matches!(request.operation, "negotiate" | "request")
            || (request.operation == "negotiate" && !request.payload.is_empty())
            || request.payload.len() > MAX_SESSION_ENVELOPE_BYTES
        {
            return Err(host_session_error("invalid retained product route"));
        }
        let runtime = self
            .inner
            .executor
            .clone()
            .ok_or_else(|| host_session_error("selected retained executor unavailable"))?;
        let budget = self.budget();
        let mut ledger = self
            .inner
            .transport
            .lock()
            .map_err(|_| host_session_error("transport ledger poisoned"))?;
        if let Some(record) = ledger.records.get(&request_id) {
            return if exact_transport_facts(record, request, reservation) {
                Ok(record.operation)
            } else {
                Err(host_session_error("conflicting transport request identity"))
            };
        }
        let charge = reservation
            .max_outcome_bytes
            .checked_add((request.payload.len() as u64).saturating_mul(2))
            .and_then(|bytes| bytes.checked_add(512))
            .ok_or_else(|| host_session_error("transport capacity overflow"))?;
        let bytes = ledger
            .bytes
            .checked_add(charge)
            .ok_or_else(|| host_session_error("transport capacity overflow"))?;
        if ledger.records.len() >= budget.max_operations as usize
            || bytes > budget.retained_bytes
            || self.is_released()
        {
            return Err(host_session_error("transport capacity exhausted"));
        }
        let operation = ledger
            .next
            .checked_add(1)
            .ok_or_else(|| host_session_error("transport identity exhausted"))?;
        let facts = bmux_plugin_sdk::HostServiceOwnedRequest {
            kind: request.kind,
            capability: request.capability.to_owned(),
            interface: request.interface.to_owned(),
            operation: request.operation.to_owned(),
            payload: request.payload.to_vec(),
        };
        ledger.records.insert(
            request_id,
            TransportRecord {
                operation,
                facts,
                reservation,
                outcome: None,
                acknowledged: false,
            },
        );
        ledger.joined = false;
        ledger.bytes = bytes;
        ledger.next = operation;
        drop(ledger);
        let completion = Completion {
            session: self.clone(),
            request: request_id,
        };
        let session = self.clone();
        let operation_name = request.operation.to_owned();
        let payload = request.payload.to_vec();
        let _task = runtime.spawn(async move {
            let outcome = session
                .transport_request(&operation_name, &payload, reservation.max_outcome_bytes)
                .await;
            completion.complete(outcome);
        });
        Ok(operation)
    }
    fn lookup(
        &self,
        request_id: u64,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> bmux_plugin_sdk::Result<Option<u64>> {
        reservation
            .validate()
            .map_err(|_| host_session_error("invalid lookup reservation"))?;
        if request_id == 0 {
            return Err(host_session_error("invalid lookup identity"));
        }
        let ledger = self
            .inner
            .transport
            .lock()
            .map_err(|_| host_session_error("transport ledger poisoned"))?;
        let operation = match ledger.records.get(&request_id) {
            Some(record) if exact_transport_facts(record, request, reservation) => {
                Some(record.operation)
            }
            Some(_) => return Err(host_session_error("conflicting transport lookup facts")),
            None => None,
        };
        drop(ledger);
        Ok(operation)
    }
    fn observe(
        &self,
        operation: u64,
        max_bytes: u64,
    ) -> bmux_plugin_sdk::Result<RetainedHostObservation> {
        let ledger = self
            .inner
            .transport
            .lock()
            .map_err(|_| host_session_error("transport ledger poisoned"))?;
        let record = ledger
            .records
            .values()
            .find(|record| record.operation == operation)
            .ok_or_else(|| host_session_error("unknown transport operation"))?;
        if record.acknowledged {
            return Err(host_session_error("transport outcome acknowledged"));
        }
        let Some(outcome) = &record.outcome else {
            return Ok(RetainedHostObservation::Pending);
        };
        let size = match outcome {
            RetainedHostOutcome::Completed(bytes) => bytes.len() as u64 + 16,
            _ => 16,
        };
        if max_bytes < size {
            return Err(host_session_error("transport observation buffer too small"));
        }
        let outcome = outcome.clone();
        drop(ledger);
        Ok(RetainedHostObservation::Terminal {
            receipt: operation,
            outcome,
        })
    }
    fn control(&self, operation: u64, control: u32) -> bmux_plugin_sdk::Result<bool> {
        if operation != 0 {
            return Err(host_session_error(
                "transport operation control unsupported",
            ));
        }
        match control {
            RETAINED_HOST_CONTROL_CLOSE => {
                self.close()?;
                Ok(true)
            }
            RETAINED_HOST_CONTROL_JOIN => {
                let mut ledger = self
                    .inner
                    .transport
                    .lock()
                    .map_err(|_| host_session_error("transport ledger poisoned"))?;
                ledger.joined = ledger
                    .records
                    .values()
                    .all(|record| record.outcome.is_some());
                let joined = ledger.joined;
                drop(ledger);
                Ok(joined)
            }
            _ => Err(host_session_error("transport cancellation not negotiated")),
        }
    }
    fn acknowledge(&self, operation: u64, receipt: u64) -> bmux_plugin_sdk::Result<()> {
        if receipt != operation {
            return Err(host_session_error("transport receipt mismatch"));
        }
        let mut ledger = self
            .inner
            .transport
            .lock()
            .map_err(|_| host_session_error("transport ledger poisoned"))?;
        let record = ledger
            .records
            .values_mut()
            .find(|record| record.operation == operation)
            .ok_or_else(|| host_session_error("unknown transport operation"))?;
        if record.outcome.is_none() {
            return Err(host_session_error("transport operation pending"));
        }
        record.acknowledged = true;
        // Keep bounded identity/facts tombstones; product outcome is untouched.
        record.outcome = Some(RetainedHostOutcome::Closed);
        drop(ledger);
        Ok(())
    }
    fn release(&self) -> bmux_plugin_sdk::Result<bool> {
        let ledger = self
            .inner
            .transport
            .lock()
            .map_err(|_| host_session_error("transport ledger poisoned"))?;
        let settled = ledger.joined && ledger.records.values().all(|record| record.acknowledged);
        drop(ledger);
        Ok(settled && self.inner.released.load(Ordering::Acquire))
    }
}

fn exact_transport_facts(
    record: &TransportRecord,
    request: &bmux_plugin_sdk::HostServiceRequest<'_>,
    reservation: NativeRetainedReservationV1,
) -> bool {
    record.facts.kind == request.kind
        && record.facts.capability == request.capability
        && record.facts.interface == request.interface
        && record.facts.operation == request.operation
        && record.facts.payload == request.payload
        && record.reservation.size == reservation.size
        && record.reservation.version == reservation.version
        && record.reservation.max_outcome_bytes == reservation.max_outcome_bytes
        && record.reservation.operation_slots == reservation.operation_slots
        && record.reservation.reserved == reservation.reserved
}
