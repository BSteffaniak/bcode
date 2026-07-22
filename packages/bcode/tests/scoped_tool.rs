use bcode::{
    Agent, ArtifactCommitGuard, InvocationArtifactSink, InvocationCapabilityFuture,
    InvocationExchangeBroker, InvocationInputRouter, InvocationScope, InvocationServiceRouter,
    ToolArtifactWriteRequest, ToolArtifactWriteResolution, ToolCall, ToolDefinition,
    ToolExchangeRequest, ToolExchangeResolution, ToolExchangeResponsePolicy, ToolInvocationInput,
    ToolInvocationInputResolution, ToolInvocationResponse, ToolInvocationServiceRequest,
    ToolInvocationServiceResolution,
};
use bcode_tool::{ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata};
use std::sync::{Arc, Mutex};

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "scoped".to_string(),
        description: "scope-aware test tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

#[derive(Debug, Default)]
struct Capabilities {
    invocation_ids: Mutex<Vec<String>>,
}

impl InvocationExchangeBroker for Capabilities {
    fn request(
        &self,
        request: ToolExchangeRequest,
    ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
        self.invocation_ids
            .lock()
            .expect("capability IDs lock")
            .push(request.invocation_id);
        Box::pin(async {
            ToolExchangeResolution::Responded {
                payload: serde_json::json!({"exchange": true}),
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
        self.invocation_ids
            .lock()
            .expect("capability IDs lock")
            .push(invocation_id.clone());
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
        self.invocation_ids
            .lock()
            .expect("capability IDs lock")
            .push(request.invocation_id);
        Box::pin(async {
            ToolInvocationServiceResolution::Responded {
                payload: serde_json::json!({"service": true}),
            }
        })
    }
}

impl InvocationArtifactSink for Capabilities {
    fn write(
        &self,
        request: ToolArtifactWriteRequest,
        commit: ArtifactCommitGuard,
    ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution> {
        self.invocation_ids
            .lock()
            .expect("capability IDs lock")
            .push(request.invocation_id);
        Box::pin(async move {
            commit
                .commit(|| ToolArtifactWriteResolution::Written {
                    artifact_id: "artifact".to_string(),
                    byte_len: 5,
                    reference: serde_json::Value::Null,
                })
                .unwrap_or(ToolArtifactWriteResolution::Cancelled)
        })
    }
}

fn response(output: &str) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: output.to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

#[tokio::test]
async fn direct_tool_receives_canonical_scope_and_all_capabilities() {
    let capabilities = Arc::new(Capabilities::default());
    let agent = Agent::builder()
        .exchange_broker(capabilities.clone())
        .input_router(capabilities.clone())
        .service_router(capabilities.clone())
        .artifact_sink(capabilities.clone())
        .scoped_inline_tool(definition(), |request, scope: InvocationScope| async move {
            let invocation_id = scope.invocation_id().to_string();
            assert_eq!(request.invocation_id, invocation_id);
            assert_eq!(request.tool_name, "scoped");
            let exchange = scope
                .request_exchange(ToolExchangeRequest {
                    invocation_id: invocation_id.clone(),
                    exchange_id: "exchange".to_string(),
                    producer_id: "test".to_string(),
                    schema: "test.exchange".to_string(),
                    schema_version: 1,
                    payload: serde_json::Value::Null,
                    response_policy: ToolExchangeResponsePolicy::Required,
                })
                .await;
            let input = scope.receive_input().await;
            let service = scope
                .invoke_service(ToolInvocationServiceRequest {
                    invocation_id: invocation_id.clone(),
                    request_id: "service".to_string(),
                    route_id: None,
                    interface_id: "test.service/v1".to_string(),
                    operation: "run".to_string(),
                    payload: serde_json::Value::Null,
                })
                .await;
            let artifact = scope
                .write_artifact(ToolArtifactWriteRequest {
                    invocation_id: invocation_id.clone(),
                    artifact_id: "artifact".to_string(),
                    content_type: "text/plain".to_string(),
                    bytes: b"hello".to_vec(),
                    metadata: serde_json::Value::Null,
                })
                .await;
            assert!(matches!(exchange, ToolExchangeResolution::Responded { .. }));
            assert!(matches!(
                input,
                ToolInvocationInputResolution::Received { .. }
            ));
            assert!(matches!(
                service,
                ToolInvocationServiceResolution::Responded { .. }
            ));
            assert!(matches!(
                artifact,
                ToolArtifactWriteResolution::Written { .. }
            ));
            Ok(response(&invocation_id))
        })
        .build();
    let call = ToolCall {
        id: "call-scoped".to_string(),
        name: "scoped".to_string(),
        arguments: serde_json::Value::Null,
    };

    let output = agent
        .execute_tool_call(&call)
        .await
        .expect("scoped tool should execute");

    assert_eq!(output.invocation.output, "call-scoped");
    assert_eq!(
        capabilities
            .invocation_ids
            .lock()
            .expect("capability IDs lock")
            .as_slice(),
        ["call-scoped", "call-scoped", "call-scoped", "call-scoped"]
    );
}
