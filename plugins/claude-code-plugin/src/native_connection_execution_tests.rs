//! Exercise the public harness service path with no declarative instance or native process.
use super::*;
use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HARNESS_SCHEMA_VERSION, HarnessAuthBinding, HarnessAuthCustody,
    HarnessRequest, HarnessRequestKind, HarnessServiceResponse, HarnessServiceResponseKind,
};
use bcode_plugin_sdk::{
    RustPlugin, SERVICE_BRIDGE_STATUS_OK, ServiceBridge, ServiceBridgeRequest,
    ServiceBridgeResponse,
};
use std::{ffi::c_void, sync::Mutex};

extern "C" fn lookup(
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    capacity: usize,
    output_len: *mut usize,
    data: *mut c_void,
) -> i32 {
    let request: ServiceBridgeRequest =
        serde_json::from_slice(unsafe { std::slice::from_raw_parts(input, input_len) })
            .expect("typed bridge request");
    let calls = unsafe { &*data.cast::<Mutex<Vec<ServiceBridgeRequest>>>() };
    calls.lock().expect("calls").push(request);
    let response =
        ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
            payload: serde_json::to_value(NativeConnectionLookupResponse {
                records: Vec::new(),
            })
            .expect("lookup response"),
        });
    let bytes = serde_json::to_vec(&response).expect("bridge reply");
    assert!(bytes.len() <= capacity);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
        *output_len = bytes.len();
    }
    SERVICE_BRIDGE_STATUS_OK
}

#[test]
fn managed_execution_without_declarative_instance_resolves_host_consent() {
    let instance = HarnessInstance {
        instance_id: "native-instance".into(),
        plugin_id: crate::PLUGIN_ID.into(),
        executable: "/synthetic/claude".into(),
        config_namespace: "/synthetic/config".into(),
        auth: HarnessAuthBinding {
            provider_id: "claude-code".into(),
            profile: "native".into(),
            credential_namespace: "/synthetic/config".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    };
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "inspect".into(),
        instance,
        kind: HarnessRequestKind::Inspect,
    };
    request.validate().expect("valid harness request");
    let mut context: NativeServiceContext = serde_json::from_value(serde_json::json!({
        "plugin_id":crate::PLUGIN_ID,
        "request":{"interface_id":HARNESS_INTERFACE_ID,"operation":"inspect","payload":serde_json::to_vec(&request).expect("request")}
    })).expect("context without configuration");
    let calls = Mutex::new(Vec::new());
    context.bridge = ServiceBridge::new(
        Some(lookup),
        (&raw const calls).cast_mut().cast::<c_void>(),
        context.cancellation.clone(),
    );
    let mut plugin = crate::ClaudeCodePlugin::default();
    let response = plugin.invoke_service(context);
    assert!(response.error.is_none());
    let response: HarnessServiceResponse =
        serde_json::from_slice(&response.payload).expect("typed harness reply");
    assert!(matches!(
        response.kind,
        HarnessServiceResponseKind::Failure(HarnessFailure::Unavailable)
    ));
    assert!(
        plugin.runtime.is_none(),
        "no native runtime or process starts without publication"
    );
    let calls = calls.into_inner().expect("calls");
    let [ServiceBridgeRequest::InvokeService(call)] = calls.as_slice() else {
        panic!("one host lookup required");
    };
    assert_eq!(call.interface_id, NATIVE_CONNECTION_LOOKUP_INTERFACE);
    assert_eq!(call.operation, "lookup");
    assert!(call.route_id.is_none());
    assert_eq!(
        serde_json::from_value::<NativeConnectionLookupRequest>(call.payload.clone())
            .expect("own instance lookup"),
        NativeConnectionLookupRequest::Resolve {
            instance_id: request.instance.instance_id
        }
    );
}
