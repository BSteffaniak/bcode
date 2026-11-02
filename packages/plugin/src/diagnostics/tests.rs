//! Native callback integration coverage, authored but not executed during swarm convergence.

use super::*;
use crate::{LoadedPlugin, LoadedPluginBackend, ResolvedPluginConfig};
use bcode_plugin_sdk::diagnostics::payload::{PayloadControl, PayloadReply};
use bcode_plugin_sdk::{NativeServiceContext, ServiceResponse};
use bcode_xray::{XRAY_SCHEMA_VERSION, XrayCorrelation, XrayHub, XrayObserveRequest};
use sha2::{Digest, Sha256};
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct Probe;
static DEFERRED: Mutex<Option<CaptureContext>> = Mutex::new(None);
static PROBE_LOCK: Mutex<()> = Mutex::new(());
impl bcode_plugin_sdk::RustPlugin for Probe {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        let capture = CaptureContext::current().expect("SDK supplies explicit scope");
        capture.emit("fixture", "entered", serde_json::json!({}), false);
        let mut forged = capture.correlation().clone();
        forged.session_id = "foreign".into();
        forged.attempt_id = Some("forged".into());
        capture.with_correlation(forged).emit(
            "fixture",
            "body",
            serde_json::json!({"text":"protected"}),
            true,
        );
        *DEFERRED.lock().unwrap() = Some(capture);
        context.events.emit(b"ordinary-event");
        ServiceResponse::text("ordinary-response")
    }
}

#[allow(clippy::too_many_arguments)]
fn invoke(
    _: *const std::ffi::c_void,
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    capacity: usize,
    length: *mut usize,
    events: Option<bcode_plugin_sdk::ServiceEventCallback>,
    event_data: *mut std::ffi::c_void,
    bridge: Option<bcode_plugin_sdk::ServiceBridgeCallback>,
    bridge_data: *mut std::ffi::c_void,
    cancellation: Option<bcode_plugin_sdk::ServiceCancellationWaitCallback>,
    cancellation_data: *mut std::ffi::c_void,
) -> i32 {
    static INSTANCE: OnceLock<Mutex<Probe>> = OnceLock::new();
    bcode_plugin_sdk::invoke_service_with_emitter_export(
        INSTANCE.get_or_init(|| Mutex::new(Probe)),
        input,
        input_len,
        output,
        capacity,
        length,
        bcode_plugin_sdk::ServiceEventEmitter::new(events, event_data),
        bridge,
        bridge_data,
        cancellation,
        cancellation_data,
    )
}

fn observer(hub: &XrayHub, session: &str, deep_capture: bool) -> bcode_xray::XrayObservation {
    hub.observe(XrayObserveRequest {
        schema_version: XRAY_SCHEMA_VERSION,
        session_id: session.into(),
        deep_capture,
    })
    .unwrap()
}

#[test]
fn static_sdk_export_keeps_diagnostics_out_of_service_events_and_retains_async_origin() {
    let _serial = PROBE_LOCK.lock().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let hub = XrayHub::default();
            let mut deep = observer(&hub, "origin", true);
            let mut shallow = observer(&hub, "origin", false);
            let mut foreign = observer(&hub, "foreign", true);
            let capture = hub.context(XrayCorrelation {
                session_id: "origin".into(),
                attempt_id: Some("host-attempt".into()),
                ..Default::default()
            });
            let mut vtable = crate::tests::test_large_vtable();
            vtable.invoke_service_streaming = invoke;
            let plugin = LoadedPlugin {
                manifest: crate::tests::test_manifest("diagnostic-fixture"),
                backend: LoadedPluginBackend::Static { vtable },
                config: ResolvedPluginConfig::default(),
            };
            let mut ordinary = Vec::new();
            let response = capture
                .scope_sync(|| {
                    plugin.invoke_service_with_events("fixture/v1", "run", vec![], |event| {
                        ordinary.push(event);
                    })
                })
                .unwrap();
            assert_eq!(response.payload_text().unwrap(), "ordinary-response");
            assert_eq!(ordinary, vec![b"ordinary-event".to_vec()]);
            let deferred = DEFERRED.lock().unwrap().take().unwrap();
            assert!(deferred.capture_active());
            let reference = emit_deferred_payload(deferred);
            assert_eq!(reference.session_id, "origin");
            let range = hub
                .read_payload(&bcode_xray::XrayReadPayloadRequest {
                    schema_version: XRAY_SCHEMA_VERSION,
                    observation_id: deep.id().to_owned(),
                    session_id: "origin".into(),
                    payload_id: reference.payload_id.clone(),
                    offset: 0,
                    length: 4096,
                })
                .unwrap();
            assert_eq!(range.bytes, vec![47; 4096]);
            assert!(
                hub.read_payload(&bcode_xray::XrayReadPayloadRequest {
                    schema_version: XRAY_SCHEMA_VERSION,
                    observation_id: foreign.id().to_owned(),
                    session_id: "foreign".into(),
                    payload_id: reference.payload_id,
                    offset: 0,
                    length: 4096,
                })
                .is_err()
            );
            deep.finalize().unwrap();
            shallow.finalize().unwrap();
            foreign.finalize().unwrap();
            let mut names = Vec::new();
            while let Some(envelope) = deep.recv().await {
                if envelope.event.domain == "fixture" {
                    assert_eq!(envelope.event.correlation.session_id, "origin");
                    assert_eq!(
                        envelope.event.correlation.attempt_id.as_deref(),
                        Some("host-attempt")
                    );
                    if envelope.event.name == "payload" {
                        assert_eq!(envelope.payload_references().unwrap().len(), 1);
                    }
                    names.push(envelope.event.name);
                }
            }
            assert_eq!(names, ["entered", "body", "payload", "deferred"]);
            while let Some(envelope) = shallow.recv().await {
                assert_ne!(envelope.event.domain, "fixture");
            }
            while let Some(envelope) = foreign.recv().await {
                assert_ne!(envelope.event.domain, "fixture");
            }
        });
}

#[test]
fn executor_queue_and_blocking_worker_preserve_origin_for_both_concurrency_modes() {
    let _serial = PROBE_LOCK.lock().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for concurrency in [
                crate::PluginConcurrencyConfig::Exclusive,
                crate::PluginConcurrencyConfig::Concurrent,
            ] {
                let hub = XrayHub::default();
                let mut observation = observer(&hub, "executor-origin", true);
                let capture = hub.context(XrayCorrelation {
                    session_id: "executor-origin".into(),
                    request_id: Some("executor-request".into()),
                    ..Default::default()
                });
                let mut vtable = crate::tests::test_large_vtable();
                vtable.invoke_service_streaming = invoke;
                let mut manifest = crate::tests::test_manifest("diagnostic-executor");
                manifest.concurrency = concurrency;
                let host = crate::PluginHost {
                    configs: std::collections::BTreeMap::new(),
                    command_registry: bcode_command::CommandRegistry::new(),
                    auth_provider_registry: crate::AuthProviderRegistry::new(),
                    cleanup_pending: Vec::new(),
                    keymap_index: OnceLock::new(),
                    loaded: vec![LoadedPlugin {
                        manifest,
                        backend: LoadedPluginBackend::Static { vtable },
                        config: ResolvedPluginConfig::default(),
                    }],
                };
                let runtime = crate::PluginRuntimeHost::from(host);
                let response = capture
                    .scope(runtime.invoke_service(
                        "diagnostic-executor",
                        "diagnostic-executor",
                        "run",
                        vec![],
                    ))
                    .await
                    .unwrap();
                assert_eq!(response.payload_text().unwrap(), "ordinary-response");
                let deferred = DEFERRED.lock().unwrap().take().unwrap();
                deferred.emit(
                    "fixture",
                    "after_executor_response",
                    serde_json::json!({}),
                    false,
                );
                drop(deferred);
                runtime.deactivate_all().await.unwrap();
                drop(runtime);
                observation.finalize().unwrap();
                let mut names = Vec::new();
                while let Some(envelope) = observation.recv().await {
                    if envelope.event.domain == "fixture" {
                        assert_eq!(envelope.event.correlation.session_id, "executor-origin");
                        assert_eq!(
                            envelope.event.correlation.request_id.as_deref(),
                            Some("executor-request")
                        );
                        names.push(envelope.event.name);
                    }
                }
                assert_eq!(names, ["entered", "body", "after_executor_response"]);
            }
        });
}

// The test deliberately holds the registry lock across observation delivery to prove
// contention is reported before that lock is released; no provider execution waits on it.
#[allow(clippy::await_holding_lock)]
#[test]
fn contended_activity_and_event_callbacks_emit_gap_before_registry_unlock() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let hub = XrayHub::default();
            let mut observation = observer(&hub, "contention-origin", true);
            let capture = hub.context(XrayCorrelation {
                session_id: "contention-origin".into(),
                attempt_id: Some("host-attempt".into()),
                ..Default::default()
            });
            let mut stack = ();
            let ordinary = crate::invocation_callbacks::CallbackRegistration::new(&mut stack);
            let key = ordinary.user_data() as usize;
            let registration =
                DiagnosticRegistration::new(ordinary.user_data(), Some(capture)).unwrap();
            let request = |operation| {
                serde_json::to_vec(&DiagnosticControl {
                    version: 1,
                    operation,
                })
                .unwrap()
            };
            let held = slot(key).entry.lock().unwrap();
            assert!(control(key, &request(DiagnosticControlOperation::State)).is_none());
            assert!(control(key, &request(DiagnosticControlOperation::Acquire)).is_none());
            event(key, b"event skipped because registry is contended");
            // Finalize while the lock is STILL held: losses cannot depend on later callback,
            // successful lookup, or SDK drop flushing a deferred counter.
            observation.finalize().unwrap();
            assert_eq!(observation.recv().await.unwrap().event.name, "ready");
            for expected in [
                "callback_registry_contention",
                "lease_acquire_contention",
                "callback_registry_contention",
            ] {
                let gap = observation.recv().await.unwrap().event;
                assert_eq!(gap.name, "diagnostic_bridge_gap");
                assert_eq!(gap.data["reason"], expected);
                assert_eq!(gap.data["complete"], false);
                assert_eq!(gap.correlation.session_id, "contention-origin");
                assert_eq!(gap.correlation.attempt_id.as_deref(), Some("host-attempt"));
            }
            assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
            drop(held);
            drop(registration);
            drop(ordinary);
        });
}

#[test]
fn loss_control_is_independent_of_payload_negotiation_and_reaches_cutoff() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let hub = XrayHub::default();
            let mut observation = observer(&hub, "loss-origin", true);
            let capture = hub.context(XrayCorrelation {
                session_id: "loss-origin".into(),
                ..Default::default()
            });
            let mut stack = ();
            let ordinary = crate::invocation_callbacks::CallbackRegistration::new(&mut stack);
            let registration =
                DiagnosticRegistration::new(ordinary.user_data(), Some(capture)).unwrap();
            let request = serde_json::to_vec(&DiagnosticControl {
                version: NATIVE_DIAGNOSTICS_VERSION,
                operation: DiagnosticControlOperation::Loss { count: 7 },
            })
            .unwrap();
            let status = control(ordinary.user_data() as usize, &request).unwrap();
            assert!(matches!(
                status.payload,
                Some(bcode_plugin_sdk::diagnostics::payload::PayloadReply::Progress)
            ));
            observation.finalize().unwrap();
            let mut cutoff = None;
            while let Some(envelope) = observation.recv().await {
                if envelope.event.name == "finalized" {
                    cutoff = Some(envelope);
                }
            }
            assert_eq!(cutoff.unwrap().event.data["hub_contention_drops"], 7);
            drop(registration);
            drop(ordinary);
        });
}

#[test]
fn callback_capacity_preflight_preserves_lease_and_pending_payload_ownership() {
    let hub = XrayHub::default();
    let _observation = observer(&hub, "capacity-origin", true);
    let capture = hub.context(XrayCorrelation {
        session_id: "capacity-origin".into(),
        ..Default::default()
    });
    let mut stack = ();
    let ordinary = crate::invocation_callbacks::CallbackRegistration::new(&mut stack);
    let key = ordinary.user_data() as usize;
    let registration = DiagnosticRegistration::new(ordinary.user_data(), Some(capture)).unwrap();

    assert_eq!(
        call_control(key, DiagnosticControlOperation::Acquire, 255).0,
        bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_RESPONSE_TOO_LARGE
    );
    assert_eq!(lookup(key).unwrap().owners.load(Ordering::Acquire), 1);
    assert!(
        call_control(key, DiagnosticControlOperation::Acquire, 256)
            .1
            .is_some()
    );
    assert_eq!(lookup(key).unwrap().owners.load(Ordering::Acquire), 2);
    assert!(matches!(
        call_control(
            key,
            DiagnosticControlOperation::Payload(PayloadControl::Negotiate { version: 1 }),
            4096
        )
        .1
        .unwrap()
        .payload,
        Some(PayloadReply::Supported { .. })
    ));
    let begin = || {
        DiagnosticControlOperation::Payload(PayloadControl::Begin {
            byte_len: 4,
            sha256: hex::encode(sha2::Sha256::digest(b"abcd")),
            media_type: "text/plain".into(),
        })
    };
    assert_eq!(
        call_control(key, begin(), 256).0,
        bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_RESPONSE_TOO_LARGE
    );
    assert_eq!(pending_payloads(key), 0);
    let Some(PayloadReply::Accepted { transfer }) =
        call_control(key, begin(), 4096).1.unwrap().payload
    else {
        panic!("begin failed")
    };
    assert_eq!(pending_payloads(key), 1);
    assert_eq!(
        call_control(
            key,
            DiagnosticControlOperation::Payload(PayloadControl::Finish { transfer }),
            256
        )
        .0,
        bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_RESPONSE_TOO_LARGE
    );
    assert_eq!(pending_payloads(key), 1);
    assert_eq!(
        call_control(key, DiagnosticControlOperation::Release, 255).0,
        bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_RESPONSE_TOO_LARGE
    );
    assert_eq!(lookup(key).unwrap().owners.load(Ordering::Acquire), 2);
    drop(registration);
    assert_eq!(lookup(key).unwrap().owners.load(Ordering::Acquire), 1);
    assert!(
        call_control(key, DiagnosticControlOperation::Release, 256)
            .1
            .is_some()
    );
    assert!(lookup(key).is_none());
    let request = serde_json::to_vec(&DiagnosticControl {
        version: 1,
        operation: DiagnosticControlOperation::Payload(PayloadControl::Finish { transfer }),
    })
    .unwrap();
    assert!(control(key, &request).is_none());
    drop(ordinary);
}

#[test]
fn control_capacity_preflight_preserves_legacy_and_bounds_payload_side_effects() {
    let request = |operation| {
        serde_json::to_vec(&DiagnosticControl {
            version: 1,
            operation,
        })
        .unwrap()
    };
    assert_eq!(
        control_response_capacity(&request(DiagnosticControlOperation::Acquire)),
        256
    );
    assert_eq!(
        control_response_capacity(&request(DiagnosticControlOperation::Release)),
        256
    );
    assert_eq!(
        control_response_capacity(&request(DiagnosticControlOperation::Payload(
            bcode_plugin_sdk::diagnostics::payload::PayloadControl::Negotiate { version: 1 },
        ))),
        4096
    );
    assert_eq!(control_response_capacity(b"malformed"), 256);
}

#[test]
fn malformed_controls_and_events_are_explicit_origin_bound_uncertainty() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let hub = XrayHub::default();
            let mut observation = observer(&hub, "malformed-origin", true);
            let capture = hub.context(XrayCorrelation {
                session_id: "malformed-origin".into(),
                ..Default::default()
            });
            let mut stack = ();
            let ordinary = crate::invocation_callbacks::CallbackRegistration::new(&mut stack);
            let key = ordinary.user_data() as usize;
            let registration =
                DiagnosticRegistration::new(ordinary.user_data(), Some(capture)).unwrap();
            assert!(control(key, b"not json").is_none());
            assert!(control(key, &[b'x'; 257]).is_none());
            event(key, b"not json");
            observation.finalize().unwrap();
            assert_eq!(observation.recv().await.unwrap().event.name, "ready");
            for name in [
                "diagnostic_bridge_gap",
                "diagnostic_bridge_gap",
                "diagnostic_payload_omitted",
            ] {
                let event = observation.recv().await.unwrap().event;
                assert_eq!(event.name, name);
                assert_eq!(event.data["complete"], false);
                assert_eq!(event.correlation.session_id, "malformed-origin");
            }
            let cutoff = observation.recv().await.unwrap();
            assert_eq!(cutoff.event.name, "finalized");
            assert!(cutoff.event.data["hub_contention_drops"].as_u64().unwrap() >= 3);
            drop(registration);
            drop(ordinary);
        });
}

#[test]
fn callback_lease_closes_without_touching_ordinary_stack_state() {
    let hub = XrayHub::default();
    let observation = observer(&hub, "origin", true);
    let capture = hub.context(XrayCorrelation {
        session_id: "origin".into(),
        ..Default::default()
    });
    let mut stack = ();
    let ordinary = crate::invocation_callbacks::CallbackRegistration::new(&mut stack);
    let key = ordinary.user_data() as usize;
    let diagnostic = DiagnosticRegistration::new(ordinary.user_data(), Some(capture)).unwrap();
    let request = |operation| {
        serde_json::to_vec(&DiagnosticControl {
            version: 1,
            operation,
        })
        .unwrap()
    };
    assert!(control(key, &request(DiagnosticControlOperation::Acquire)).is_some());
    drop(ordinary);
    drop(diagnostic);
    assert!(
        control(key, &request(DiagnosticControlOperation::State))
            .unwrap()
            .active
    );
    drop(observation);
    assert!(
        !control(key, &request(DiagnosticControlOperation::State))
            .unwrap()
            .active
    );
    control(key, &request(DiagnosticControlOperation::Release)).unwrap();
    assert!(control(key, &request(DiagnosticControlOperation::State)).is_none());
}

/// Requires a freshly rebuilt real fake-provider dylib. No compilation occurs inside this test.
/// This intentionally exercises libloading + serialized context + SDK export inside another
/// image rather than replacing the dynamic call with an in-process function pointer.
#[test]
#[ignore = "set BCODE_XRAY_TEST_PROVIDER_LIBRARY to a rebuilt fake-provider dylib"]
fn real_dynamic_provider_sdk_boundary_captures_without_public_payload_leak() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let library = std::env::var_os("BCODE_XRAY_TEST_PROVIDER_LIBRARY")
                .expect("explicit fixture dylib");
            let path = std::path::PathBuf::from(library);
            assert!(
                path.is_absolute(),
                "fixture must use an explicit absolute artifact path"
            );
            let mut manifest: crate::PluginManifest = toml::from_str(include_str!(
                "../../../../plugins/fake-provider-plugin/bcode-plugin.toml"
            ))
            .unwrap();
            let crate::PluginRuntime::Native(runtime) = &mut manifest.runtime;
            runtime.library = path.clone();
            let registered = crate::RegisteredPlugin {
                manifest,
                manifest_path: path.with_file_name("fixture-manifest.toml"),
            };
            let plugin = crate::load_registered_plugin(&registered).unwrap();
            plugin.activate().unwrap();
            let hub = XrayHub::default();
            let mut observation = observer(&hub, "host-session", true);
            let capture = hub.context(XrayCorrelation {
                session_id: "host-session".into(),
                attempt_id: Some("dynamic-attempt".into()),
                ..Default::default()
            });
            // Exceed the native event bound and split multibyte scalars across chunks.
            let text = "界🙂e\u{301}".repeat(12 * 1024);
            let payload = serde_json::to_vec(&serde_json::json!({
                "session_id":"00000000-0000-0000-0000-000000000000", "turn_id":"dynamic-turn",
                "model_id":"fake-echo",
                "messages":[{"role":"user","content":[{"type":"text","text":text}]}],
                "provider_context":{"settings":{"api_key":"fixture-key-must-not-escape"}}
            }))
            .unwrap();
            let mut ordinary = Vec::new();
            let response = capture
                .scope_sync(|| {
                    plugin.invoke_service_with_events(
                        "bcode.model-provider/v3",
                        "run_turn",
                        payload,
                        |event| ordinary.push(event),
                    )
                })
                .unwrap();
            assert!(response.error.is_none());
            plugin.deactivate().unwrap();
            observation.finalize().unwrap();
            let mut provider_events = 0;
            let mut payload_references = Vec::new();
            while let Some(envelope) = observation.recv().await {
                let encoded = serde_json::to_string(&envelope).unwrap();
                assert!(!encoded.contains("fixture-key-must-not-escape"));
                if envelope.event.domain.starts_with("provider") {
                    provider_events += 1;
                    let references = envelope.payload_references().unwrap();
                    if envelope.event.name == "adapted_request" {
                        assert_eq!(references.len(), 1, "large native request must be retained");
                    }
                    for reference in references {
                        assert_eq!(reference.session_id, "host-session");
                        assert!(reference.byte_len > 32 * 1024);
                        payload_references.push(reference);
                    }
                    assert_eq!(envelope.event.correlation.session_id, "host-session");
                    assert_eq!(
                        envelope.event.correlation.attempt_id.as_deref(),
                        Some("dynamic-attempt")
                    );
                }
            }
            assert_dynamic_request_payloads(&hub, &observation, payload_references, &text);
            assert!(
                provider_events > 0,
                "real dylib provider must emit through negotiated callback"
            );
            for event in ordinary {
                assert!(!event.starts_with(bcode_plugin_sdk::diagnostics::DIAGNOSTIC_EVENT_PREFIX));
            }
        });
}

fn assert_dynamic_request_payloads(
    hub: &XrayHub,
    observation: &bcode_xray::XrayObservation,
    references: Vec<bcode_xray::XrayPayloadRef>,
    text: &str,
) {
    assert!(
        !references.is_empty(),
        "real dylib must transfer a large body into host retention"
    );
    let mut exact_request_seen = false;
    for reference in references {
        let retained = read_dynamic_provider_payload(hub, observation, &reference);
        if let Some(messages) = retained.get("messages") {
            assert_eq!(messages[0]["content"][0]["text"], text);
            assert!(reference.byte_len > 64 * 1024);
            exact_request_seen = true;
        }
    }
    assert!(
        exact_request_seen,
        "native retention must preserve the exact Unicode request"
    );
}

fn read_dynamic_provider_payload(
    hub: &XrayHub,
    observation: &bcode_xray::XrayObservation,
    reference: &bcode_xray::XrayPayloadRef,
) -> serde_json::Value {
    assert_eq!(reference.schema_version, XRAY_SCHEMA_VERSION);
    assert_eq!(reference.content_type, "application/json");
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < reference.byte_len {
        let range = hub
            .read_payload(&bcode_xray::XrayReadPayloadRequest {
                schema_version: XRAY_SCHEMA_VERSION,
                observation_id: observation.id().to_owned(),
                session_id: "host-session".into(),
                payload_id: reference.payload_id.clone(),
                offset: bytes.len() as u64,
                length: 4096,
            })
            .unwrap();
        assert_ne!(range.bytes, [] as [u8; 0]);
        bytes.extend_from_slice(&range.bytes);
    }
    assert_eq!(bytes.len() as u64, reference.byte_len);
    assert_eq!(hex::encode(Sha256::digest(&bytes)), reference.sha256);
    assert!(!String::from_utf8_lossy(&bytes).contains("fixture-key-must-not-escape"));
    let retained: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(serde_json::to_vec(&retained).unwrap(), bytes);
    retained
}

fn call_control(
    key: usize,
    operation: DiagnosticControlOperation,
    capacity: usize,
) -> (i32, Option<DiagnosticStatus>) {
    let mut request = bcode_plugin_sdk::diagnostics::DIAGNOSTIC_CONTROL_PREFIX.to_vec();
    request.extend(
        serde_json::to_vec(&DiagnosticControl {
            version: NATIVE_DIAGNOSTICS_VERSION,
            operation,
        })
        .unwrap(),
    );
    let mut output = vec![0; capacity];
    let mut length = 0;
    let status = crate::service_bridge_callback(
        request.as_ptr(),
        request.len(),
        output.as_mut_ptr(),
        output.len(),
        &raw mut length,
        key as *mut std::ffi::c_void,
    );
    let reply = (status == bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK)
        .then(|| serde_json::from_slice::<DiagnosticStatus>(&output[..length]).unwrap());
    (status, reply)
}

fn emit_deferred_payload(deferred: CaptureContext) -> bcode_xray::XrayPayloadRef {
    std::thread::spawn(move || {
        let reference = deferred
            .capture_payload("application/octet-stream", &vec![47; 96 * 1024])
            .unwrap()
            .unwrap();
        deferred.emit_with_capture(
            "fixture",
            "payload",
            serde_json::json!({"payload_ref": reference}),
            true,
            Some(bcode_xray::XrayCaptureAttachments::new(vec![
                bcode_xray::XrayCaptureAttachment::PayloadRef {
                    pointer: "/payload_ref".into(),
                },
            ])),
        );
        deferred.scope_sync(|| {
            CaptureContext::current().unwrap().emit(
                "fixture",
                "deferred",
                serde_json::json!({}),
                false,
            );
        });
        reference
    })
    .join()
    .unwrap()
}

fn pending_payloads(key: usize) -> usize {
    lookup(key)
        .unwrap()
        .payloads
        .lock()
        .unwrap()
        .pending_count()
}
