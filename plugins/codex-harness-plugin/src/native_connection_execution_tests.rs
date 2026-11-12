//! Host lookup admission is exercised without native processes or declarative config.
use super::*;
use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HARNESS_SCHEMA_VERSION, HarnessAuthBinding, HarnessAuthCustody,
    HarnessFailure, HarnessRequest, HarnessRequestKind, HarnessServiceResponse,
    HarnessServiceResponseKind,
};
use bcode_plugin_sdk::{
    RustPlugin, SERVICE_BRIDGE_STATUS_OK, ServiceBridge, ServiceBridgeRequest,
    ServiceBridgeResponse,
};
use std::{ffi::c_void, sync::Mutex};

struct BridgeState {
    calls: Mutex<Vec<ServiceBridgeRequest>>,
    payload: serde_json::Value,
}
extern "C" fn lookup(
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    capacity: usize,
    output_len: *mut usize,
    data: *mut c_void,
) -> i32 {
    let request: ServiceBridgeRequest =
        serde_json::from_slice(unsafe { std::slice::from_raw_parts(input, input_len) }).unwrap();
    let state = unsafe { &*data.cast::<BridgeState>() };
    state.calls.lock().unwrap().push(request);
    let response =
        ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
            payload: state.payload.clone(),
        });
    let bytes = serde_json::to_vec(&response).unwrap();
    assert!(bytes.len() <= capacity);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
        *output_len = bytes.len();
    }
    SERVICE_BRIDGE_STATUS_OK
}
fn instance() -> HarnessInstance {
    HarnessInstance {
        instance_id: "native-instance".into(),
        plugin_id: crate::PLUGIN_ID.into(),
        executable: "/synthetic/codex".into(),
        config_namespace: "/synthetic/.codex".into(),
        auth: HarnessAuthBinding {
            provider_id: "bcode.codex".into(),
            profile: "native".into(),
            credential_namespace: "/synthetic/.codex".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    }
}
#[test]
fn runtime_without_config_requires_exact_host_resolve_before_process_access() {
    for payload in [
        serde_json::json!({"records":[]}),
        serde_json::json!({"unexpected":"future"}),
    ] {
        let request = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "inspect".into(),
            instance: instance(),
            kind: HarnessRequestKind::Inspect,
        };
        request.validate().unwrap();
        let mut context: NativeServiceContext = serde_json::from_value(serde_json::json!({
            "plugin_id":crate::PLUGIN_ID,
            "request":{"interface_id":HARNESS_INTERFACE_ID,"operation":"inspect","payload":serde_json::to_vec(&request).unwrap()}
        })).unwrap();
        let state = BridgeState {
            calls: Mutex::new(Vec::new()),
            payload,
        };
        context.bridge = ServiceBridge::new(
            Some(lookup),
            (&raw const state).cast_mut().cast(),
            context.cancellation.clone(),
        );
        let mut plugin = crate::CodexHarnessPlugin::default();
        let response = plugin.invoke_service(context);
        assert!(response.error.is_none());
        let response: HarnessServiceResponse = serde_json::from_slice(&response.payload).unwrap();
        assert!(matches!(
            response.kind,
            HarnessServiceResponseKind::Failure(
                HarnessFailure::Policy | HarnessFailure::InvalidRequest
            )
        ));
        assert!(plugin.owners.is_empty() && plugin.auth.is_empty());
        let calls = state.calls.into_inner().unwrap();
        let [ServiceBridgeRequest::InvokeService(call)] = calls.as_slice() else {
            panic!("one host lookup");
        };
        assert_eq!(call.interface_id, NATIVE_CONNECTION_LOOKUP_INTERFACE);
        assert_eq!(call.operation, "lookup");
        assert!(call.route_id.is_none());
        assert_eq!(
            serde_json::from_value::<NativeConnectionLookupRequest>(call.payload.clone()).unwrap(),
            NativeConnectionLookupRequest::Resolve {
                instance_id: request.instance.instance_id
            }
        );
    }
}

#[test]
fn status_policy_and_caller_evidence_cannot_become_execution_profiles() {
    let temp = tempfile::tempdir().unwrap();
    let namespace = temp.path().join(".codex");
    std::fs::create_dir(&namespace).unwrap();
    let candidate = candidate(Path::new("/usr/bin/true"), &namespace).unwrap();
    let mut instance = instance();
    instance.executable.clone_from(&candidate.executable);
    instance
        .config_namespace
        .clone_from(&candidate.config_namespace);
    instance
        .auth
        .credential_namespace
        .clone_from(&candidate.config_namespace);
    let mut connection = NativePreparedConnection {
        candidate,
        instance,
        protocol_profile: NATIVE_POLICY.into(),
        isolation_profile: NATIVE_POLICY.into(),
        compatibility_evidence: "caller-claims-ready".into(),
    };
    connection.validate().unwrap();
    assert!(matches!(
        prepared_launch(&connection, Path::new("/")),
        Err(CodexError::Unsupported)
    ));
    connection.protocol_profile = "unknown-future-profile".into();
    assert!(matches!(
        prepared_launch(&connection, Path::new("/")),
        Err(CodexError::Unsupported)
    ));
    connection.candidate.installation_id.push('0');
    assert!(matches!(
        prepared_launch(&connection, Path::new("/")),
        Err(CodexError::Stale)
    ));
}
