use super::*;
use bmux_plugin_sdk::abi::retained::RetainedHostDispatch;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Wait(AtomicUsize);
impl RetainedSessionWait for Wait {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        assert!((1..=32).contains(&duration.as_millis()));
        self.0.fetch_add(1, Ordering::SeqCst);
        // This fake timer is only used with a finite fake host's Pending schedule.
        Box::pin(std::future::ready(()))
    }
}
type TransportRequestFacts = (String, Vec<u8>, u64);

struct Dispatch {
    requests: Mutex<BTreeMap<u64, TransportRequestFacts>>,
    pending: AtomicBool,
    failed_read: AtomicBool,
    acks: AtomicUsize,
    fail_begin: AtomicBool,
    fail_ack: AtomicBool,
    acknowledged: Mutex<std::collections::BTreeSet<u64>>,
    denied: AtomicBool,
}
impl Dispatch {
    fn new() -> Self {
        Self {
            requests: Mutex::new(BTreeMap::new()),
            pending: AtomicBool::new(false),
            failed_read: AtomicBool::new(false),
            fail_begin: AtomicBool::new(false),
            fail_ack: AtomicBool::new(false),
            acknowledged: Mutex::new(std::collections::BTreeSet::new()),
            denied: AtomicBool::new(false),
            acks: AtomicUsize::new(0),
        }
    }
}
fn failure() -> bmux_plugin_sdk::PluginError {
    bmux_plugin_sdk::PluginError::ServiceProtocol {
        details: "unavailable".into(),
    }
}
impl RetainedHostDispatch for Dispatch {
    fn begin(
        &self,
        id: u64,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> bmux_plugin_sdk::Result<u64> {
        assert_eq!(request.capability, PRODUCT_HOST_SESSION_CONTRACT);
        assert_eq!(request.interface, PRODUCT_HOST_SESSION_CONTRACT);
        assert_eq!(request.kind, bmux_plugin_sdk::ServiceKind::Query);
        assert_eq!(reservation.operation_slots, 1);
        assert_eq!(reservation.reserved, 0);
        let mut requests = self.requests.lock().unwrap();
        assert!(!requests.contains_key(&id));
        requests.insert(
            id,
            (
                request.operation.into(),
                request.payload.to_vec(),
                reservation.max_outcome_bytes,
            ),
        );
        drop(requests);
        if self.fail_begin.swap(false, Ordering::SeqCst) {
            return Err(failure());
        }
        Ok(id)
    }
    fn lookup(
        &self,
        id: u64,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> bmux_plugin_sdk::Result<Option<u64>> {
        let requests = self.requests.lock().unwrap();
        let result = match requests.get(&id) {
            Some((operation, payload, capacity))
                if operation == request.operation
                    && payload == request.payload
                    && *capacity == reservation.max_outcome_bytes =>
            {
                Some(id)
            }
            Some(_) => return Err(failure()),
            None => None,
        };
        drop(requests);
        Ok(result)
    }
    fn observe(
        &self,
        id: u64,
        _max_bytes: u64,
    ) -> bmux_plugin_sdk::Result<RetainedHostObservation> {
        if self.acknowledged.lock().unwrap().contains(&id) {
            return Err(failure());
        }
        if self.failed_read.swap(false, Ordering::SeqCst) {
            return Err(failure());
        }
        if self.pending.swap(false, Ordering::SeqCst) {
            return Ok(RetainedHostObservation::Pending);
        }
        if self.denied.swap(false, Ordering::SeqCst) {
            return Ok(RetainedHostObservation::Terminal {
                receipt: id + 100,
                outcome: RetainedHostOutcome::RejectedBeforeEffects,
            });
        }
        let requests = self.requests.lock().unwrap();
        let (operation, payload, reserved) = requests.get(&id).unwrap();
        let bytes = if operation == "negotiate" {
            assert_eq!(payload.as_slice(), &[] as &[u8]);
            serde_json::to_vec(&SessionNegotiation {
                contract: PRODUCT_HOST_SESSION_CONTRACT.into(),
                version: PRODUCT_HOST_SESSION_VERSION,
                budget: SessionBudget {
                    max_jobs: 2,
                    max_operations: 8,
                    max_request_bytes: 4096,
                    max_response_bytes: 4096,
                    retained_bytes: 65_536,
                },
            })
            .unwrap()
        } else {
            let request: HostSessionRequest = serde_json::from_slice(payload).unwrap();
            let reply = match request {
                HostSessionRequest::BeginJob { request_id, .. } => {
                    HostSessionResponse::JobBegun { job: request_id }
                }
                HostSessionRequest::Observe { .. } | HostSessionRequest::JoinJob { .. } => {
                    HostSessionResponse::Pending
                }
                HostSessionRequest::CloseSession => HostSessionResponse::AdmissionsClosed,
                _ => HostSessionResponse::Rejected {
                    code: SessionCode::InvalidRequest,
                },
            };
            serde_json::to_vec(&reply).unwrap()
        };
        assert!(bytes.len() as u64 + u64::from(RETAINED_HOST_RECORD_HEADER_BYTES) <= *reserved);
        drop(requests);
        Ok(RetainedHostObservation::Terminal {
            receipt: id + 100,
            outcome: RetainedHostOutcome::Completed(bytes),
        })
    }
    fn control(&self, _operation: u64, _control: u32) -> bmux_plugin_sdk::Result<bool> {
        Ok(false)
    }
    fn acknowledge(&self, operation: u64, receipt: u64) -> bmux_plugin_sdk::Result<()> {
        assert_eq!(receipt, operation + 100);
        if self.fail_ack.swap(false, Ordering::SeqCst) {
            return Err(failure());
        }
        if self.acknowledged.lock().unwrap().insert(operation) {
            self.acks.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn release(&self) -> bmux_plugin_sdk::Result<bool> {
        Ok(false)
    }
}
fn scope() -> PluginCreateContext {
    serde_json::from_value(serde_json::json!({
        "runtime_scope": "00000000-0000-0000-0000-000000000001",
        "instance": "00000000-0000-0000-0000-000000000002", "role": "frontend",
    }))
    .unwrap()
}
fn ready<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("finite fake host completes"),
    }
}
#[test]
fn hostile_session_negotiation_fields_are_not_authority() {
    let value = serde_json::json!({
        "contract": PRODUCT_HOST_SESSION_CONTRACT, "version": 1,
        "budget": { "max_jobs": 1, "max_operations": 2, "max_request_bytes": 64,
            "max_response_bytes": 64, "retained_bytes": 1024 },
        "activation": "fabricated",
    });
    assert!(serde_json::from_value::<SessionNegotiation>(value).is_err());
}

#[test]
fn uuid_payloads_and_transport_identity_survive_uncertain_observation_without_replay() {
    let dispatch = Arc::new(Dispatch::new());
    let wait = Arc::new(Wait(AtomicUsize::new(0)));
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        wait.clone(),
    ));
    dispatch.pending.store(true, Ordering::SeqCst);
    let client = ready(adapter.negotiate()).unwrap();
    assert_eq!(wait.0.load(Ordering::SeqCst), 1);
    assert_eq!(dispatch.acks.load(Ordering::SeqCst), 1);
    let request_id = uuid::Uuid::from_u128(u128::MAX - 1);
    let request = HostSessionRequest::BeginJob {
        request_id,
        reservation: super::super::super::host_session_models::JobReservation {
            operations: 1,
            request_bytes: 128,
            response_bytes: 128,
        },
    };
    dispatch.failed_read.store(true, Ordering::SeqCst);
    assert!(ready(client.request(request.clone())).is_err());
    assert_eq!(adapter.transport_ids().unwrap(), vec![1, 2]);
    assert_eq!(dispatch.requests.lock().unwrap().len(), 2);
    let recovered = ready(adapter.recover(2)).unwrap();
    assert_eq!(recovered, HostSessionResponse::JobBegun { job: request_id });
    assert_eq!(ready(client.request(request)).unwrap(), recovered);
    assert_eq!(dispatch.requests.lock().unwrap().len(), 2);
    adapter.acknowledge_transport(2).unwrap();
    adapter.acknowledge_transport(2).unwrap();
    assert_eq!(dispatch.acks.load(Ordering::SeqCst), 2);
    assert_eq!(ready(adapter.recover(2)).unwrap(), recovered);
    assert!(!adapter.release_transport().unwrap());
    assert_eq!(adapter.transport_ids().unwrap(), vec![1, 2]);
}
struct Park;
impl RetainedSessionWait for Park {
    fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}
#[test]
fn dropped_observer_keeps_operation_and_does_not_ack_or_restart() {
    let dispatch = Arc::new(Dispatch::new());
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        Arc::new(Park),
    ));
    ready(adapter.negotiate()).unwrap();
    let request = HostSessionRequest::JoinJob {
        job: uuid::Uuid::from_u128(1),
    };
    let id = adapter.begin_transport_request(request.clone()).unwrap();
    dispatch.pending.store(true, Ordering::SeqCst);
    let mut observer = Box::pin(adapter.recover(id));
    assert!(
        observer
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    drop(observer);
    assert_eq!(dispatch.acks.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.transport_request(id).unwrap(), Some(request));
    assert_eq!(
        ready(adapter.recover(id)).unwrap(),
        HostSessionResponse::Pending
    );
    assert_eq!(dispatch.requests.lock().unwrap().len(), 2);
}

#[test]
fn transport_denial_keeps_receipt_and_does_not_guess_application_success() {
    let dispatch = Arc::new(Dispatch::new());
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        Arc::new(Wait(AtomicUsize::new(0))),
    ));
    let client = ready(adapter.negotiate()).unwrap();
    dispatch.denied.store(true, Ordering::SeqCst);
    assert!(matches!(
        ready(client.request(HostSessionRequest::CloseSession)),
        Err(HostSessionClientError::Uncertain(_))
    ));
    assert_eq!(dispatch.requests.lock().unwrap().len(), 2);
    adapter.acknowledge_transport(2).unwrap();
    assert_eq!(dispatch.acks.load(Ordering::SeqCst), 2);
}

#[test]
fn author_context_retains_independent_port_after_original_call_drops() {
    let dispatch = Arc::new(Dispatch::new());
    let port = RetainedHostPort::new(dispatch);
    let services = bmux_plugin_sdk::definition::PluginHostServices::new(|_| Err(failure()))
        .with_retained_host(move || Ok(port.clone()));
    let call = PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: Some(services),
        callback_execution: None,
    };
    let context = ProductAuthorContext::capture(scope(), &call).unwrap();
    drop(call);
    let adapter = context
        .retained_session(Arc::new(Wait(AtomicUsize::new(0))))
        .unwrap();
    assert!(Arc::ptr_eq(
        &adapter,
        &context
            .retained_session(Arc::new(Wait(AtomicUsize::new(0))))
            .unwrap()
    ));
    let client = ready(adapter.negotiate()).unwrap();
    assert_eq!(
        ready(client.request(HostSessionRequest::CloseSession)).unwrap(),
        HostSessionResponse::AdmissionsClosed
    );
    let absent = PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: None,
        callback_execution: None,
    };
    assert!(matches!(
        ProductAuthorContext::capture(scope(), &absent),
        Err(HostSessionClientError::Unsupported)
    ));
}

#[test]
fn negotiation_ack_reclaims_bytes_but_retains_one_bounded_identity_slot() {
    let dispatch = Arc::new(Dispatch::new());
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        Arc::new(Wait(AtomicUsize::new(0))),
    ));
    dispatch.fail_ack.store(true, Ordering::SeqCst);
    assert!(ready(adapter.negotiate()).is_err());
    assert_eq!(adapter.transport_ids().unwrap(), vec![1]);
    assert_eq!(dispatch.acks.load(Ordering::SeqCst), 0);
    ready(adapter.negotiate()).unwrap();
    let state = adapter.state.lock().unwrap();
    assert_eq!(state.reserved, 512);
    assert!(state.records[&1].acknowledged);
    drop(state);
    assert_eq!(dispatch.requests.lock().unwrap().len(), 1);
    assert_eq!(dispatch.acks.load(Ordering::SeqCst), 1);
    ready(adapter.negotiate()).unwrap();
    assert_eq!(dispatch.requests.lock().unwrap().len(), 1);
}
#[test]
fn opener_distinguishes_absence_denial_and_corrupt_result() {
    assert_eq!(
        classify_open_error(&bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "open_retained_host"
        }),
        HostSessionClientError::Unsupported
    );
    assert_eq!(
        classify_open_error(&bmux_plugin_sdk::PluginError::CapabilityAccessDenied {
            plugin_id: "test".into(),
            capability: "retained".into(),
            operation: "open_retained_host",
            hint: None,
        }),
        HostSessionClientError::Uncertain(SessionCode::PermissionDenied)
    );
    assert_eq!(
        classify_open_error(&failure()),
        HostSessionClientError::Uncertain(SessionCode::Unavailable)
    );
}
#[test]
fn exact_escaped_envelope_and_exhausted_budget_reject_before_transport_begin() {
    let dispatch = Arc::new(Dispatch::new());
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        Arc::new(Wait(AtomicUsize::new(0))),
    ));
    ready(adapter.negotiate()).unwrap();
    let request = HostSessionRequest::Start {
        job: uuid::Uuid::from_u128(1),
        request_id: uuid::Uuid::from_u128(2),
        capability: "\"".repeat(64),
        kind: bmux_plugin_sdk::ServiceKind::Query,
        interface: "example/v1".into(),
        operation: "read".into(),
        payload: vec![255; 128],
    };
    let size = serde_json::to_vec(&request).unwrap().len();
    assert_eq!(encoded_size(&request, size).unwrap(), size);
    adapter
        .state
        .lock()
        .unwrap()
        .negotiation
        .as_mut()
        .unwrap()
        .budget
        .max_request_bytes = u32::try_from(size - 1).unwrap();
    assert!(matches!(
        adapter.begin_transport_request(request.clone()),
        Err(HostSessionClientError::Invalid(
            SessionCode::RequestLimitExceeded
        ))
    ));
    assert_eq!(adapter.transport_ids().unwrap(), vec![1]);
    let mut state = adapter.state.lock().unwrap();
    let budget = &mut state.negotiation.as_mut().unwrap().budget;
    budget.max_request_bytes = 4096;
    budget.retained_bytes = 512;
    drop(state);
    assert!(matches!(
        adapter.begin_transport_request(request),
        Err(HostSessionClientError::Invalid(
            SessionCode::CapacityExceeded
        ))
    ));
    assert_eq!(dispatch.requests.lock().unwrap().len(), 1);
}
#[test]
fn conflicting_request_identity_rejects_and_closed_session_keeps_join_observable() {
    let dispatch = Arc::new(Dispatch::new());
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        Arc::new(Wait(AtomicUsize::new(0))),
    ));
    let client = ready(adapter.negotiate()).unwrap();
    let id = uuid::Uuid::from_u128(1);
    let reservation = super::super::super::host_session_models::JobReservation {
        operations: 1,
        request_bytes: 64,
        response_bytes: 64,
    };
    ready(client.begin_job(id, reservation)).unwrap();
    let conflict = HostSessionRequest::BeginJob {
        request_id: id,
        reservation: super::super::super::host_session_models::JobReservation {
            operations: 2,
            ..reservation
        },
    };
    assert!(matches!(
        ready(client.request(conflict)),
        Err(HostSessionClientError::Invalid(
            SessionCode::ConflictingRequest
        ))
    ));
    let start = HostSessionRequest::Start {
        job: id,
        request_id: uuid::Uuid::from_u128(2),
        capability: "bcode.product".into(),
        kind: bmux_plugin_sdk::ServiceKind::Query,
        interface: "example/v1".into(),
        operation: "read".into(),
        payload: vec![],
    };
    let mut changed = start.clone();
    if let HostSessionRequest::Start { payload, .. } = &mut changed {
        payload.push(1);
    }
    assert!(conflicts(&start, &changed));
    assert!(!conflicts(&start, &start));
    assert_eq!(dispatch.requests.lock().unwrap().len(), 2);
    assert_eq!(
        ready(client.request(HostSessionRequest::CloseSession)).unwrap(),
        HostSessionResponse::AdmissionsClosed
    );
    assert!(matches!(
        ready(client.begin_job(uuid::Uuid::from_u128(9), reservation)),
        Err(HostSessionClientError::Invalid(
            SessionCode::AdmissionsClosed
        ))
    ));
    assert_eq!(
        ready(client.request(HostSessionRequest::JoinJob { job: id })).unwrap(),
        HostSessionResponse::Pending
    );
    assert_eq!(dispatch.requests.lock().unwrap().len(), 4);
}
#[test]
fn concurrent_negotiation_observer_waits_for_publication_without_second_begin() {
    let dispatch = Arc::new(Dispatch::new());
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        Arc::new(Park),
    ));
    let id = adapter.begin(None).unwrap();
    let operation = {
        let mut state = adapter.state.lock().unwrap();
        let record = state.records.get_mut(&id).unwrap();
        record.publishing = true;
        let operation = record.operation.take().unwrap();
        drop(state);
        operation
    };
    let mut observer = Box::pin(adapter.negotiate());
    assert!(
        observer
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    drop(observer);
    assert_eq!(dispatch.requests.lock().unwrap().len(), 1);
    let mut state = adapter.state.lock().unwrap();
    let record = state.records.get_mut(&id).unwrap();
    record.operation = Some(operation);
    record.publishing = false;
    drop(state);
    ready(adapter.negotiate()).unwrap();
    assert_eq!(dispatch.requests.lock().unwrap().len(), 1);
}

#[test]
fn ambiguous_begin_recovers_original_operation_without_replay() {
    let dispatch = Arc::new(Dispatch::new());
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        Arc::new(Wait(AtomicUsize::new(0))),
    ));
    let client = ready(adapter.negotiate()).unwrap();
    let request = HostSessionRequest::BeginJob {
        request_id: uuid::Uuid::from_u128(7),
        reservation: super::super::super::host_session_models::JobReservation {
            operations: 1,
            request_bytes: 64,
            response_bytes: 64,
        },
    };
    dispatch.fail_begin.store(true, Ordering::SeqCst);
    assert!(matches!(
        ready(client.request(request.clone())),
        Err(HostSessionClientError::Uncertain(_))
    ));
    assert_eq!(adapter.transport_ids().unwrap(), vec![1, 2]);
    assert_eq!(adapter.transport_request(2).unwrap(), Some(request.clone()));
    assert_eq!(
        ready(client.request(request)).unwrap(),
        HostSessionResponse::JobBegun {
            job: uuid::Uuid::from_u128(7)
        }
    );
    assert_eq!(
        ready(adapter.recover(2)).unwrap(),
        HostSessionResponse::JobBegun {
            job: uuid::Uuid::from_u128(7)
        }
    );
    assert_eq!(dispatch.requests.lock().unwrap().len(), 2);
    assert_eq!(dispatch.acks.load(Ordering::SeqCst), 1);
}
#[test]
fn one_slot_negotiated_budget_does_not_pretend_to_admit_product_requests() {
    let dispatch = Arc::new(Dispatch::new());
    let adapter = Arc::new(RetainedProductHostSession::new(
        RetainedHostPort::new(dispatch.clone()),
        scope(),
        Arc::new(Wait(AtomicUsize::new(0))),
    ));
    ready(adapter.negotiate()).unwrap();
    adapter
        .state
        .lock()
        .unwrap()
        .negotiation
        .as_mut()
        .unwrap()
        .budget
        .max_operations = 1;
    assert!(matches!(
        ready(adapter.negotiate()),
        Err(HostSessionClientError::Invalid(
            SessionCode::CapacityExceeded
        ))
    ));
    assert!(matches!(
        adapter.begin_transport_request(HostSessionRequest::CloseSession),
        Err(HostSessionClientError::Invalid(
            SessionCode::CapacityExceeded
        ))
    ));
    assert_eq!(dispatch.requests.lock().unwrap().len(), 1);
}

struct Legacy;
impl RetainedHostDispatch for Legacy {
    fn begin(
        &self,
        _id: u64,
        _request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        _reservation: NativeRetainedReservationV1,
    ) -> bmux_plugin_sdk::Result<u64> {
        panic!("no replay")
    }
    fn observe(&self, _id: u64, _bytes: u64) -> bmux_plugin_sdk::Result<RetainedHostObservation> {
        panic!("no invented operation")
    }
    fn control(&self, _id: u64, _control: u32) -> bmux_plugin_sdk::Result<bool> {
        Ok(false)
    }
    fn acknowledge(&self, _id: u64, _receipt: u64) -> bmux_plugin_sdk::Result<()> {
        panic!("no implicit ack")
    }
    fn release(&self) -> bmux_plugin_sdk::Result<bool> {
        Ok(false)
    }
}
#[test]
fn lookup_v1_unsupported_and_cross_owner_absence_do_not_replay() {
    let facts = bmux_plugin_sdk::HostServiceOwnedRequest {
        capability: PRODUCT_HOST_SESSION_CONTRACT.into(),
        interface: PRODUCT_HOST_SESSION_CONTRACT.into(),
        kind: bmux_plugin_sdk::ServiceKind::Query,
        operation: "negotiate".into(),
        payload: vec![],
    };
    let reservation = NativeRetainedReservationV1 {
        size: u32::try_from(std::mem::size_of::<NativeRetainedReservationV1>()).unwrap(),
        version: RETAINED_HOST_VERSION,
        max_outcome_bytes: 4112,
        operation_slots: 1,
        reserved: 0,
    };
    assert!(
        RetainedHostPort::new(Arc::new(Legacy))
            .lookup(1, &facts.as_request(), reservation)
            .is_err()
    );
    let foreign = Arc::new(Dispatch::new());
    assert!(
        RetainedHostPort::new(foreign.clone())
            .lookup(1, &facts.as_request(), reservation)
            .unwrap()
            .is_none()
    );
    assert!(foreign.requests.lock().unwrap().is_empty());
}

#[test]
fn recovery_v2_capture_selects_only_explicit_v2_opener_and_retains_owner() {
    let legacy_calls = Arc::new(AtomicUsize::new(0));
    let v2_calls = Arc::new(AtomicUsize::new(0));
    let dispatch = Arc::new(Dispatch::new());
    let port = RetainedHostPort::new(dispatch);
    let legacy = legacy_calls.clone();
    let selected = v2_calls.clone();
    let services = bmux_plugin_sdk::definition::PluginHostServices::new(|_| Err(failure()))
        .with_retained_host(move || {
            legacy.fetch_add(1, Ordering::SeqCst);
            Err(failure())
        })
        .with_retained_host_v2(move || {
            selected.fetch_add(1, Ordering::SeqCst);
            Ok(port.clone())
        });
    let call = PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: Some(services),
        callback_execution: None,
    };
    let adapter =
        super::super::open_retained_session_v2(scope(), &call, Arc::new(Wait(AtomicUsize::new(0))))
            .unwrap();
    drop(call);
    ready(adapter.negotiate()).unwrap();
    assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
    assert_eq!(v2_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn recovery_v2_missing_and_denied_never_fall_back_to_bound_v1() {
    let legacy_calls = Arc::new(AtomicUsize::new(0));
    let legacy = legacy_calls.clone();
    let services = bmux_plugin_sdk::definition::PluginHostServices::new(|_| Err(failure()))
        .with_retained_host(move || {
            legacy.fetch_add(1, Ordering::SeqCst);
            Ok(RetainedHostPort::new(Arc::new(Dispatch::new())))
        });
    let mut call = PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: Some(services.clone()),
        callback_execution: None,
    };
    assert!(matches!(
        ProductAuthorContext::capture_recovery_v2(scope(), &call),
        Err(HostSessionClientError::Unsupported)
    ));
    call.host_services = Some(services.with_retained_host_v2(|| {
        Err(bmux_plugin_sdk::PluginError::CapabilityAccessDenied {
            plugin_id: "test".into(),
            capability: "retained-v2".into(),
            operation: "open_retained_host_v2",
            hint: None,
        })
    }));
    assert!(matches!(
        ProductAuthorContext::capture_recovery_v2(scope(), &call),
        Err(HostSessionClientError::Uncertain(
            SessionCode::PermissionDenied
        ))
    ));
    assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
}
