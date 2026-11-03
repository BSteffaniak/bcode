//! Optional callback protocol tests. Authored only during interface convergence.
use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Fixture {
    acquired: AtomicUsize,
    released: AtomicUsize,
    events: Mutex<Vec<Vec<u8>>>,
    payload_mode: AtomicUsize,
    aborted: AtomicUsize,
    payload_bytes: Mutex<Vec<u8>>,
}
extern "C" fn control(
    request: *const u8,
    length: usize,
    output: *mut u8,
    capacity: usize,
    output_length: *mut usize,
    data: *mut c_void,
) -> i32 {
    let fixture = unsafe { &*data.cast::<Fixture>() };
    let bytes = unsafe { std::slice::from_raw_parts(request, length) };
    let request: DiagnosticControl =
        serde_json::from_slice(bytes.strip_prefix(DIAGNOSTIC_CONTROL_PREFIX).unwrap()).unwrap();
    if matches!(request.operation, DiagnosticControlOperation::Payload(_)) {
        match fixture.payload_mode.load(Ordering::SeqCst) {
            13 => return crate::SERVICE_BRIDGE_STATUS_FAILED,
            14 | 15 => {
                let future = b"{\"version\":99,\"active\":true,\"deep_active\":true}";
                let bytes: &[u8] = if fixture.payload_mode.load(Ordering::SeqCst) == 14 {
                    future
                } else {
                    b"{malformed"
                };
                assert!(bytes.len() <= capacity);
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
                    *output_length = bytes.len();
                }
                return SERVICE_BRIDGE_STATUS_OK;
            }
            _ => {}
        }
    }
    let payload = match &request.operation {
        DiagnosticControlOperation::Payload(operation) => fixture_payload(fixture, operation),
        _ => None,
    };
    match request.operation {
        DiagnosticControlOperation::Acquire => {
            fixture.acquired.fetch_add(1, Ordering::SeqCst);
        }
        DiagnosticControlOperation::Release => {
            fixture.released.fetch_add(1, Ordering::SeqCst);
        }
        DiagnosticControlOperation::State
        | DiagnosticControlOperation::Payload(_)
        | DiagnosticControlOperation::Loss { .. } => {}
    }
    let bytes = serde_json::to_vec(&DiagnosticStatus {
        version: 1,
        active: true,
        deep_active: true,
        payload,
    })
    .unwrap();
    assert!(bytes.len() <= capacity);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
        *output_length = bytes.len();
    }
    SERVICE_BRIDGE_STATUS_OK
}
fn fixture_payload(
    fixture: &Fixture,
    operation: &payload::PayloadControl,
) -> Option<payload::PayloadReply> {
    use payload::{PayloadControl, PayloadReply};
    use sha2::{Digest, Sha256};
    let mode = fixture.payload_mode.load(Ordering::SeqCst);
    if mode == 0 {
        return None; // Legacy event-only host.
    }
    if matches!(operation, PayloadControl::Negotiate { .. }) {
        match mode {
            9 => return Some(PayloadReply::Unavailable),
            10 => {
                return Some(PayloadReply::Supported {
                    version: 99,
                    max_bytes: 1024,
                    chunk_bytes: 1024,
                });
            }
            _ => {}
        }
    }
    if mode == 11 && matches!(operation, PayloadControl::Begin { .. }) {
        return Some(PayloadReply::Unavailable);
    }
    if mode == 12 && matches!(operation, PayloadControl::Finish { .. }) {
        return Some(PayloadReply::Unavailable);
    }
    Some(match operation {
        PayloadControl::Negotiate { .. } => PayloadReply::Supported {
            version: payload::PAYLOAD_VERSION,
            max_bytes: payload::MAX_PAYLOAD_BYTES,
            chunk_bytes: payload::MAX_CHUNK_BYTES,
        },
        PayloadControl::Begin { .. } => {
            fixture.payload_bytes.lock().unwrap().clear();
            PayloadReply::Accepted { transfer: 7 }
        }
        PayloadControl::Chunk { offset, bytes, .. } => {
            if mode == 2 {
                return None; // Lost/malformed acknowledgement after Begin.
            }
            let mut received = fixture.payload_bytes.lock().unwrap();
            assert_eq!(*offset, received.len());
            received.extend_from_slice(bytes);
            PayloadReply::Progress
        }
        PayloadControl::Finish { .. } => {
            let bytes = fixture.payload_bytes.lock().unwrap();
            PayloadReply::Retained {
                reference: bcode_xray::XrayPayloadRef {
                    schema_version: if mode == 3 {
                        99
                    } else {
                        bcode_xray::XRAY_SCHEMA_VERSION
                    },
                    payload_id: if mode == 8 {
                        String::new()
                    } else {
                        "opaque-reference".into()
                    },
                    session_id: if mode == 4 { "foreign" } else { "host" }.into(),
                    content_type: if mode == 5 {
                        "text/plain"
                    } else {
                        "application/json"
                    }
                    .into(),
                    byte_len: bytes.len() as u64 + u64::from(mode == 7),
                    sha256: if mode == 6 {
                        "0".repeat(64)
                    } else {
                        hex::encode(Sha256::digest(&*bytes))
                    },
                },
            }
        }
        PayloadControl::Abort { .. } => {
            fixture.aborted.fetch_add(1, Ordering::SeqCst);
            fixture.payload_bytes.lock().unwrap().clear();
            PayloadReply::Progress
        }
    })
}

#[test]
fn retained_payload_acknowledgements_validate_metadata_and_abort_every_failed_transfer() {
    let bytes =
        serde_json::to_vec(&serde_json::json!({"text": "界🙂e\u{301}".repeat(10_000)})).unwrap();
    for mode in 1..=15 {
        let mut fixture = Fixture::default();
        fixture.payload_mode.store(mode, Ordering::SeqCst);
        let data = std::ptr::from_mut(&mut fixture).cast();
        let capture = capture_context(
            Some(&negotiation()),
            ServiceEventEmitter::new(Some(event), data),
            Some(control),
            data,
        )
        .unwrap();
        let result = capture.capture_payload("application/json", &bytes);
        if mode == 1 {
            let reference = result.unwrap().expect("explicit retained acknowledgement");
            assert_eq!(reference.byte_len, bytes.len() as u64);
            assert_eq!(*fixture.payload_bytes.lock().unwrap(), bytes);
            assert_eq!(fixture.aborted.load(Ordering::SeqCst), 0);
        } else {
            assert!(
                matches!(result, Err(bcode_xray::XrayError::Unavailable)),
                "mode {mode} must fail unavailable, never unsupported or empty success"
            );
            assert_eq!(
                fixture.aborted.load(Ordering::SeqCst),
                usize::from(matches!(mode, 2..=8 | 12))
            );
            assert!(fixture.payload_bytes.lock().unwrap().is_empty());
            assert!(fixture.events.lock().unwrap().iter().all(|bytes| {
                let event: NativeDiagnosticEvent =
                    serde_json::from_slice(bytes.strip_prefix(DIAGNOSTIC_EVENT_PREFIX).unwrap())
                        .unwrap();
                event.event.data["reason"] != "external_sink_payload_unsupported"
            }));
        }
        drop(capture);
        assert_eq!(fixture.released.load(Ordering::SeqCst), 1);
    }
}

extern "C" fn event(payload: *const u8, length: usize, data: *mut c_void) {
    let fixture = unsafe { &*data.cast::<Fixture>() };
    let bytes = unsafe { std::slice::from_raw_parts(payload, length) };
    fixture.events.lock().unwrap().push(bytes.to_vec());
}
fn negotiation() -> NativeDiagnosticContext {
    NativeDiagnosticContext {
        version: 1,
        mode: NativeDiagnosticMode::CallbackV1,
        correlation: XrayCorrelation {
            session_id: "host".into(),
            ..Default::default()
        },
        max_event_bytes: MAX_NATIVE_DIAGNOSTIC_BYTES,
    }
}

#[test]
fn callback_sink_retains_once_across_context_clones_and_releases_last_owner() {
    let mut fixture = Fixture::default();
    let data = std::ptr::from_mut(&mut fixture).cast();
    let capture = capture_context(
        Some(&negotiation()),
        ServiceEventEmitter::new(Some(event), data),
        Some(control),
        data,
    )
    .unwrap();
    let retained = capture.clone();
    drop(capture);
    assert_eq!(fixture.acquired.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.released.load(Ordering::SeqCst), 0);
    retained.scope_sync(|| {
        CaptureContext::current().unwrap().emit(
            "fixture",
            "body",
            serde_json::json!({"body":"sensitive"}),
            true,
        );
    });
    let events = fixture.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let bytes = events[0].strip_prefix(DIAGNOSTIC_EVENT_PREFIX).unwrap();
    let envelope: NativeDiagnosticEvent = serde_json::from_slice(bytes).unwrap();
    assert!(envelope.event.sensitive);
    assert_eq!(envelope.event.data["body"], "sensitive");
    drop(events);
    drop(retained);
    assert_eq!(fixture.released.load(Ordering::SeqCst), 1);
}

#[test]
fn unsupported_or_absent_negotiation_never_acquires_callback() {
    let mut fixture = Fixture::default();
    let data = std::ptr::from_mut(&mut fixture).cast();
    let emitter = ServiceEventEmitter::new(Some(event), data);
    assert!(capture_context(None, emitter, Some(control), data).is_none());
    let mut context = negotiation();
    context.version = 99;
    assert!(capture_context(Some(&context), emitter, Some(control), data).is_none());
    context.version = 1;
    context.mode = NativeDiagnosticMode::Disabled;
    assert!(capture_context(Some(&context), emitter, Some(control), data).is_none());
    context.mode = NativeDiagnosticMode::CallbackV1;
    assert!(capture_context(Some(&context), emitter, None, data).is_none());
    let unknown: NativeDiagnosticContext = serde_json::from_value(serde_json::json!({
        "version":1, "mode":"future_transport", "correlation":{"session_id":"host"},
        "max_event_bytes":65536
    }))
    .unwrap();
    assert!(capture_context(Some(&unknown), emitter, Some(control), data).is_none());
    assert_eq!(fixture.acquired.load(Ordering::SeqCst), 0);
    assert!(fixture.events.lock().unwrap().is_empty());
}

#[test]
fn old_event_only_host_keeps_events_and_rejects_payload_extension_without_empty_success() {
    let mut fixture = Fixture::default();
    let data = std::ptr::from_mut(&mut fixture).cast();
    let capture = capture_context(
        Some(&negotiation()),
        ServiceEventEmitter::new(Some(event), data),
        Some(control),
        data,
    )
    .unwrap();
    assert!(matches!(
        capture.capture_payload("text/plain", b"payload"),
        Err(bcode_xray::XrayError::PayloadCaptureUnsupported)
    ));
    assert!(fixture.events.lock().unwrap().iter().any(|bytes| {
        let event: NativeDiagnosticEvent =
            serde_json::from_slice(bytes.strip_prefix(DIAGNOSTIC_EVENT_PREFIX).unwrap()).unwrap();
        event.event.domain == "capture"
            && event.event.name == "payload_omitted"
            && event.event.data["reason"] == "external_sink_payload_unsupported"
    }));
    assert!(!capture.report_external_loss(1));
    capture.emit("fixture", "still_available", serde_json::json!({}), true);
    assert!(fixture.events.lock().unwrap().iter().any(|bytes| {
        let event: NativeDiagnosticEvent =
            serde_json::from_slice(bytes.strip_prefix(DIAGNOSTIC_EVENT_PREFIX).unwrap()).unwrap();
        event.event.name == "still_available"
    }));
    drop(capture);
    assert_eq!(fixture.released.load(Ordering::SeqCst), 1);
}

#[test]
fn legacy_status_remains_decodable_without_payload_extension() {
    let status: DiagnosticStatus =
        serde_json::from_str(r#"{"version":1,"active":true,"deep_active":true}"#).unwrap();
    assert!(status.payload.is_none());
    let wire = serde_json::to_value(status).unwrap();
    assert!(wire.get("payload").is_none());
}

#[test]
fn malformed_optional_context_disables_only_diagnostics() {
    let context: crate::NativeServiceContext = serde_json::from_value(serde_json::json!({
        "plugin_id":"fixture", "request":{"interface_id":"fixture/v1","operation":"run","payload":[]},
        "diagnostics":{"version":99,"mode":"future_without_known_fields"}
    })).unwrap();
    assert!(context.diagnostics.is_none());
    assert_eq!(context.request.operation, "run");
}

#[test]
fn disabled_scope_masks_ambient_capture_and_restores_it_afterward() {
    let hub = bcode_xray::XrayHub::default();
    let observation = hub
        .observe(bcode_xray::XrayObserveRequest {
            schema_version: bcode_xray::XRAY_SCHEMA_VERSION,
            session_id: "host".into(),
            deep_capture: true,
        })
        .unwrap();
    let ambient = hub.context(XrayCorrelation {
        session_id: "host".into(),
        ..Default::default()
    });
    ambient.scope_sync(|| {
        assert!(CaptureContext::current().unwrap().deep_capture_active());
        scope(None, || {
            assert!(!CaptureContext::current().unwrap().deep_capture_active());
        });
        assert!(CaptureContext::current().unwrap().deep_capture_active());
    });
    assert!(CaptureContext::current().is_none());
    drop(observation);
}
