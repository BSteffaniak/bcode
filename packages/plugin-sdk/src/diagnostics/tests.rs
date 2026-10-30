//! Optional callback protocol tests. Authored only during interface convergence.
use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Fixture {
    acquired: AtomicUsize,
    released: AtomicUsize,
    events: Mutex<Vec<Vec<u8>>>,
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
    match request.operation {
        DiagnosticControlOperation::Acquire => {
            fixture.acquired.fetch_add(1, Ordering::SeqCst);
        }
        DiagnosticControlOperation::Release => {
            fixture.released.fetch_add(1, Ordering::SeqCst);
        }
        DiagnosticControlOperation::State => {}
    }
    let bytes = serde_json::to_vec(&DiagnosticStatus {
        version: 1,
        active: true,
        deep_active: true,
    })
    .unwrap();
    assert!(bytes.len() <= capacity);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
        *output_length = bytes.len();
    }
    SERVICE_BRIDGE_STATUS_OK
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
