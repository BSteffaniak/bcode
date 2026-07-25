#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_agent_profile::{ToolPolicyOperation, tool_policy_authorization_metadata};
use bcode_plugin_sdk::{
    ConcurrentRustPlugin, NativeServiceContext, PluginConfigContext, ServiceBridge,
    ServiceCancellation, ServiceEventEmitter, ServiceRequest,
};
use bcode_shell_plugin::ShellPlugin;
use bcode_tool::{
    OP_PREPARE_TOOL, TOOL_SERVICE_INTERFACE_ID, ToolInvocationDescriptor, ToolPreparationRequest,
    ToolPreparationResponse,
};

fn context(source: &str) -> NativeServiceContext {
    let request = ToolPreparationRequest {
        invocation: ToolInvocationDescriptor {
            invocation_id: "parity".to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments: serde_json::json!({"command": source}),
        },
        host_context: Vec::new(),
    };
    NativeServiceContext {
        plugin_id: "bcode.shell".to_owned(),
        request: ServiceRequest {
            interface_id: TOOL_SERVICE_INTERFACE_ID.to_owned(),
            operation: OP_PREPARE_TOOL.to_owned(),
            payload: serde_json::to_vec(&request).unwrap(),
        },
        config: PluginConfigContext::default(),
        events: ServiceEventEmitter::default(),
        cancellation: ServiceCancellation::default(),
        bridge: ServiceBridge::default(),
        transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
    }
}

fn preparation(source: &str) -> ToolPreparationResponse {
    ShellPlugin
        .invoke_service_concurrent(context(source))
        .payload_json()
        .expect("shell preparation should decode")
}

#[test]
fn concurrent_entrypoint_produces_stable_authorization_facts() {
    for source in [
        "printf hello",
        "printf ok; rm generated",
        "cmd=printf; \"$cmd\" ok",
        "if true; then",
    ] {
        let first = preparation(source);
        let second = preparation(source);
        assert_eq!(first.authorization, second.authorization, "{source}");
        let metadata = tool_policy_authorization_metadata(&first.authorization, "shell.run")
            .expect("authorization metadata should decode");
        let ToolPolicyOperation::Command {
            command,
            analysis,
            analysis_error,
        } = metadata.operation
        else {
            panic!("shell plugin must produce command policy");
        };
        assert_eq!(command.as_deref(), Some(source));
        assert_ne!(analysis.is_some(), analysis_error.is_some());
    }
}

#[cfg(feature = "static-bundled")]
#[test]
fn static_bundled_entrypoint_produces_same_fact_as_concurrent_plugin() {
    let source = "printf ok; rm generated";
    let expected = preparation(source);
    let context = context(source);
    let encoded = serde_json::to_vec(&context).unwrap();
    let vtable = bcode_shell_plugin::static_plugin();
    let mut output = vec![0_u8; 1024 * 1024];
    let mut output_len = 0_usize;
    let status = (vtable.invoke_service_streaming)(
        vtable.instance,
        encoded.as_ptr(),
        encoded.len(),
        output.as_mut_ptr(),
        output.len(),
        &raw mut output_len,
        None,
        std::ptr::null_mut(),
        None,
        std::ptr::null_mut(),
        None,
        std::ptr::null_mut(),
    );
    assert_eq!(status, 0);
    let response: bcode_plugin_sdk::ServiceResponse = serde_json::from_slice(&output[..output_len])
        .expect("static service response should decode");
    let actual = response
        .payload_json::<ToolPreparationResponse>()
        .expect("static preparation should decode");
    assert_eq!(actual.authorization, expected.authorization);
}
