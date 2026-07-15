#![cfg(feature = "embedded-plugins")]

use bcode::{
    Agent, InvocationArtifactSink, InvocationCapabilityFuture, InvocationExchangeBroker,
    InvocationInputRouter, InvocationServiceRouter, ToolArtifactWriteRequest,
    ToolArtifactWriteResolution, ToolCall, ToolDefinition, ToolExchangeRequest,
    ToolExchangeResolution, ToolInvocationInput, ToolInvocationInputResolution,
    ToolInvocationServiceRequest, ToolInvocationServiceResolution,
};
use bcode_tool::{ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct Capabilities(Mutex<Vec<String>>);

impl Capabilities {
    fn record(&self, invocation_id: String) {
        self.0
            .lock()
            .expect("capability IDs lock")
            .push(invocation_id);
    }
}

impl InvocationExchangeBroker for Capabilities {
    fn request(
        &self,
        request: ToolExchangeRequest,
    ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
        self.record(request.invocation_id);
        Box::pin(async {
            ToolExchangeResolution::Responded {
                payload: serde_json::Value::Null,
            }
        })
    }
}

impl InvocationInputRouter for Capabilities {
    fn receive(
        &self,
        invocation_id: &str,
    ) -> InvocationCapabilityFuture<'_, ToolInvocationInputResolution> {
        let invocation_id = invocation_id.to_string();
        self.record(invocation_id.clone());
        Box::pin(async move {
            ToolInvocationInputResolution::Received {
                input: ToolInvocationInput {
                    invocation_id,
                    input_id: "input".to_string(),
                    producer_id: "test".to_string(),
                    schema: "test.input".to_string(),
                    schema_version: 1,
                    payload: serde_json::Value::Null,
                },
            }
        })
    }
}

impl InvocationServiceRouter for Capabilities {
    fn invoke(
        &self,
        request: ToolInvocationServiceRequest,
    ) -> InvocationCapabilityFuture<'_, ToolInvocationServiceResolution> {
        self.record(request.invocation_id);
        Box::pin(async {
            ToolInvocationServiceResolution::Responded {
                payload: serde_json::Value::Null,
            }
        })
    }
}

impl InvocationArtifactSink for Capabilities {
    fn write(
        &self,
        request: ToolArtifactWriteRequest,
    ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution> {
        self.record(request.invocation_id);
        Box::pin(async {
            ToolArtifactWriteResolution::Written {
                artifact_id: "hello-artifact".to_string(),
                byte_len: 5,
                reference: serde_json::Value::Null,
            }
        })
    }
}

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "hello_bridge".to_string(),
        description: "embedded bridge parity tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

#[tokio::test]
async fn embedded_plugin_uses_same_scope_and_capabilities_as_direct_tools() {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
        bcode_hello_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("hello plugin manifest should parse");
    let plugins = bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("hello plugin should load statically"),
    );
    let capabilities = Arc::new(Capabilities::default());
    let agent = Agent::builder()
        .plugin_runtime(plugins)
        .plugin_tool(definition(), "example.hello")
        .exchange_broker(capabilities.clone())
        .input_router(capabilities.clone())
        .service_router(capabilities.clone())
        .artifact_sink(capabilities.clone())
        .build();
    let call = ToolCall {
        id: "call-plugin".to_string(),
        name: "hello_bridge".to_string(),
        arguments: serde_json::Value::Null,
    };

    let output = agent
        .execute_tool_call(&call)
        .await
        .expect("embedded plugin tool should execute");

    assert_eq!(output.invocation.output, "call-plugin");
    assert_eq!(
        capabilities
            .0
            .lock()
            .expect("capability IDs lock")
            .as_slice(),
        ["call-plugin", "call-plugin", "call-plugin", "call-plugin"]
    );
}
