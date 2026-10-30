use super::{context, query};
use bcode_plugin_sdk::prelude::*;
use serde_json::{Value, json};

fn native_context(
    operation: &str,
    payload: Vec<u8>,
    bridge: ServiceBridge,
) -> NativeServiceContext {
    NativeServiceContext {
        diagnostics: None,
        plugin_id: "bcode.workflow".into(),
        request: ServiceRequest {
            interface_id: bcode_tool::TOOL_SERVICE_INTERFACE_ID.into(),
            operation: operation.into(),
            payload,
        },
        config: bcode_plugin_sdk::PluginConfigContext::default(),
        events: ServiceEventEmitter::default(),
        cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        bridge,
        transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
    }
}

fn prepare(arguments: Value) -> Value {
    let request = bcode_tool::ToolPreparationRequest {
        invocation: bcode_tool::ToolInvocationDescriptor {
            invocation_id: "projection-test".into(),
            tool_name: "workflow.execution_context".into(),
            arguments,
        },
        host_context: vec![bcode_tool::ToolHostContextEntry {
            schema: bcode_tool::TOOL_INVOCATION_SERVICE_ROUTES_SCHEMA.into(),
            schema_version: 1,
            payload: json!([{
                "route_id":"context-test", "interface_id":bcode_workflow::WORKFLOW_APPLICATION_INTERFACE_ID,
                "operations":["execution_context"]
            }]),
        }],
    };
    let response = crate::tools::invoke(&native_context(
        bcode_tool::OP_PREPARE_TOOL,
        serde_json::to_vec(&request).unwrap(),
        ServiceBridge::default(),
    ));
    assert!(response.error.is_none(), "{:?}", response.error);
    let prepared: bcode_tool::ToolPreparationResponse =
        serde_json::from_slice(&response.payload).unwrap();
    prepared.descriptor
}

extern "C" fn context_bridge(
    request_ptr: *const u8,
    request_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    // SAFETY: the SDK provides the valid request slice during this synchronous callback.
    let request = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
    let request: ServiceBridgeRequest = serde_json::from_slice(request).unwrap();
    let ServiceBridgeRequest::InvokeService(request) = request else {
        panic!("expected authenticated workflow service request");
    };
    // SAFETY: dispatch keeps this exclusive counter alive for the synchronous bridge call.
    unsafe {
        *user_data.cast::<usize>() += 1;
    }
    assert_eq!(request.operation, "execution_context");
    assert_eq!(request.route_id.as_deref(), Some("context-test"));
    assert_eq!(request.payload["output_id"], "worker-result");
    assert!(request.payload.get("output_pointer").is_none());
    let response =
        ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
            payload: serde_json::to_value(context(
                json!({"summary":"verified selection", "history":"x".repeat(20_000)}),
            ))
            .unwrap(),
        });
    let bytes = serde_json::to_vec(&response).unwrap();
    assert!(bytes.len() <= output_capacity);
    // SAFETY: the SDK provides writable output storage and length pointer of the stated capacity.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), output_ptr, bytes.len());
        *output_len = bytes.len();
    }
    bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
}

fn dispatch(arguments: Value, descriptor: Value, calls: &mut usize) -> ServiceResponse {
    let request = bcode_tool::ToolInvocationRequest {
        tool_call_id: "projection-test".into(),
        name: "workflow.execution_context".into(),
        arguments,
        preparation_descriptor: descriptor,
    };
    crate::tools::invoke(&native_context(
        bcode_tool::OP_INVOKE_TOOL,
        serde_json::to_vec(&request).unwrap(),
        ServiceBridge::new(
            Some(context_bridge),
            std::ptr::from_mut(calls).cast(),
            bcode_plugin_sdk::ServiceCancellation::default(),
        ),
    ))
}

#[test]
fn native_preparation_and_dispatch_preserve_projection_and_reject_swapping() {
    let arguments = query(json!("/summary"));
    let descriptor = prepare(arguments.clone());
    assert_eq!(descriptor["output_pointer"], "/summary");
    assert!(descriptor["edit"].get("output_pointer").is_none());
    let mut calls = 0;
    for altered in [query(json!("/history")), query(Value::Null)] {
        let response = dispatch(altered, descriptor.clone(), &mut calls);
        assert_eq!(response.error.unwrap().code, "invalid_request");
        assert_eq!(calls, 0);
    }
    let response = dispatch(arguments, descriptor, &mut calls);
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(calls, 1);
    let response: bcode_tool::ToolInvocationResponse =
        serde_json::from_slice(&response.payload).unwrap();
    let output: Value = serde_json::from_str(&response.output).unwrap();
    assert_eq!(output["projection"]["pointer"], "/summary");
    assert_eq!(output["projection"]["value"], "verified selection");
    assert_eq!(output["projection"]["complete"], true);
    assert_eq!(output["output"]["checksum_sha256"], "whole-value-checksum");
    assert!(output["output"].get("value").is_none());
}

#[test]
fn native_null_and_absent_selectors_have_equivalent_preparation_and_dispatch() {
    let absent = json!({"output_only":true,"output_id":"worker-result"});
    let nullable = query(Value::Null);
    assert_eq!(prepare(absent.clone()), prepare(nullable.clone()));
    let mut calls = 0;
    let response = dispatch(absent, prepare(nullable), &mut calls);
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(calls, 1);
    let response: bcode_tool::ToolInvocationResponse =
        serde_json::from_slice(&response.payload).unwrap();
    let output: Value = serde_json::from_str(&response.output).unwrap();
    assert!(output.get("projection").is_none());
    assert_eq!(output["output"]["value"]["summary"], "verified selection");
}
