use bcode::{
    Agent, HeadlessExchangePolicy, InvocationCapabilityFuture, InvocationExchangeBroker,
    InvocationScope, ToolCall, ToolDefinition, ToolExchangeRequest, ToolExchangeResolution,
    ToolExchangeResponsePolicy, ToolInvocationResponse,
};
use bcode_tool::{ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: "headless exchange test".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn response(output: &str) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: output.to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

fn exchange_with_policy(
    scope: &InvocationScope,
    response_policy: ToolExchangeResponsePolicy,
) -> ToolExchangeRequest {
    ToolExchangeRequest {
        invocation_id: scope.invocation_id().to_string(),
        exchange_id: "exchange".to_string(),
        producer_id: "test".to_string(),
        schema: "test.exchange".to_string(),
        schema_version: 1,
        payload: serde_json::Value::Null,
        response_policy,
    }
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.to_string(),
        arguments: serde_json::Value::Null,
    }
}

async fn run_with_policy(
    policy: Option<HeadlessExchangePolicy>,
    name: &str,
    response_policy: ToolExchangeResponsePolicy,
) -> ToolExchangeResolution {
    let agent = {
        let builder = Agent::builder().scoped_inline_tool(
            definition(name),
            move |_request, scope| async move {
                let resolution = scope
                    .request_exchange(exchange_with_policy(&scope, response_policy))
                    .await;
                let output =
                    serde_json::to_string(&resolution).map_err(|error| error.to_string())?;
                Ok(response(&output))
            },
        );
        policy
            .map_or(builder.clone(), |policy| {
                builder.headless_exchange_policy(policy)
            })
            .build()
    };
    let output = agent
        .execute_tool_call(&call(name))
        .await
        .expect("tool executes");
    serde_json::from_str(&output.invocation.output).expect("resolution decodes")
}

async fn run(policy: Option<HeadlessExchangePolicy>, name: &str) -> ToolExchangeResolution {
    run_with_policy(policy, name, ToolExchangeResponsePolicy::Required).await
}

#[derive(Debug)]
struct ForwardBroker(Arc<AtomicUsize>);

impl InvocationExchangeBroker for ForwardBroker {
    fn request(
        &self,
        _request: ToolExchangeRequest,
    ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolExchangeResolution::ConsumerDetached })
    }
}

#[tokio::test]
async fn headless_exchange_policies_are_explicit_and_default_required_is_unsupported() {
    assert_eq!(
        run(None, "default").await,
        ToolExchangeResolution::NoCompatibleConsumer
    );

    assert!(matches!(
        run(Some(HeadlessExchangePolicy::Reject), "reject").await,
        ToolExchangeResolution::Failed { code, .. } if code == "headless_exchange_rejected"
    ));

    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&callbacks);
    assert_eq!(
        run(
            Some(HeadlessExchangePolicy::Callback(Arc::new(move |_| {
                callback_count.fetch_add(1, Ordering::SeqCst);
                ToolExchangeResolution::TimedOut
            }))),
            "callback",
        )
        .await,
        ToolExchangeResolution::TimedOut
    );
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);

    let forwards = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        run(
            Some(HeadlessExchangePolicy::Forward(Arc::new(ForwardBroker(
                Arc::clone(&forwards),
            )))),
            "forward",
        )
        .await,
        ToolExchangeResolution::ConsumerDetached
    );
    assert_eq!(forwards.load(Ordering::SeqCst), 1);

    assert_eq!(
        run(
            Some(HeadlessExchangePolicy::AutoResponse(serde_json::json!({
                "answer": true
            }))),
            "auto",
        )
        .await,
        ToolExchangeResolution::Responded {
            payload: serde_json::json!({"answer": true}),
        }
    );
}

#[tokio::test]
async fn unsupported_headless_exchange_is_explicit_for_required_and_optional_policies() {
    assert_eq!(
        run_with_policy(
            None,
            "required-unsupported",
            ToolExchangeResponsePolicy::Required,
        )
        .await,
        ToolExchangeResolution::NoCompatibleConsumer
    );
    assert_eq!(
        run_with_policy(
            None,
            "optional-unsupported",
            ToolExchangeResponsePolicy::Optional,
        )
        .await,
        ToolExchangeResolution::NoCompatibleConsumer
    );
}
