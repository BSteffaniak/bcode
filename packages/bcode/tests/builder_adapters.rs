use bcode::{
    Agent, InvocationScope, PreparationScope, PreparedToolInvocation, RegisteredTool,
    ToolAuthorizationCoordinator, ToolAuthorizationDecision, ToolAuthorizationRequest, ToolCall,
    ToolDefinition, ToolInvocationResponse, ToolInvoker, ToolPreparationRequest,
    ToolPreparationResponse, TurnEventObservability, TurnEventPersistence,
};
use bcode_agent_runtime::{ModelProviderInvoker, RuntimeFuture, TurnScope};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use bcode_tool::{ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "adapter".to_string(),
        description: "adapter routing test".to_string(),
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

#[derive(Debug)]
struct CountingInvoker(Arc<AtomicUsize>);

impl ToolInvoker for CountingInvoker {
    fn prepare_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        request: &'a ToolPreparationRequest,
        _scope: &'a PreparationScope,
    ) -> RuntimeFuture<'a, ToolPreparationResponse> {
        let result = bcode_agent_profile::prepare_tool_policy(request, &tool.definition).map_err(
            |message| bcode::RuntimeError::ToolPreparation {
                tool_name: request.invocation.tool_name.clone(),
                message,
            },
        );
        Box::pin(async move { result })
    }

    fn invoke_tool<'a>(
        &'a self,
        _tool: &'a RegisteredTool,
        invocation: &'a PreparedToolInvocation,
        scope: &'a InvocationScope,
    ) -> RuntimeFuture<'a, ToolInvocationResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let output = format!(
            "{}:{}",
            invocation.invocation.invocation_id,
            scope.invocation_id()
        );
        Box::pin(async move { Ok(response(&output)) })
    }
}

#[derive(Debug)]
struct DenyCoordinator(Arc<AtomicUsize>);

impl ToolAuthorizationCoordinator for DenyCoordinator {
    fn authorize_batch<'a>(
        &'a self,
        requests: &'a [ToolAuthorizationRequest],
        _scope: &'a TurnScope,
    ) -> RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
        self.0.fetch_add(requests.len(), Ordering::SeqCst);
        Box::pin(async move {
            Ok(requests
                .iter()
                .map(|_| ToolAuthorizationDecision::Deny("custom coordinator".to_string()))
                .collect())
        })
    }
}

struct FakeProvider {
    events: VecDeque<ProviderTurnEvent>,
}

impl FakeProvider {
    fn new() -> Self {
        Self {
            events: [
                ProviderTurnEvent::TextDelta {
                    text: "factory".to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]
            .into(),
        }
    }
}

impl ModelProviderInvoker for FakeProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "factory-turn".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            Ok(PollTurnEventsResponse {
                events: self.events.pop_front().into_iter().collect(),
            })
        })
    }

    fn cancel_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async { Ok(AckResponse::default()) })
    }

    fn finish_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async { Ok(AckResponse::default()) })
    }
}

fn call() -> ToolCall {
    ToolCall {
        id: "adapter-call".to_string(),
        name: "adapter".to_string(),
        arguments: serde_json::Value::Null,
    }
}

struct CountingHostExtension(Arc<AtomicUsize>);

impl TurnEventPersistence for CountingHostExtension {
    fn persist(&self, _event: &bcode::ScopedTurnEvent) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        true
    }
}

impl TurnEventObservability for CountingHostExtension {
    fn observe(&self, _event: &bcode::ScopedTurnEvent) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn builder_routes_provider_tool_invoker_and_authorization_adapters() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let persisted = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let agent = Agent::builder()
        .provider_factory(|| Box::new(FakeProvider::new()))
        .inline_tool(definition(), |_| Ok(response("legacy")))
        .tool_invoker(Arc::new(CountingInvoker(Arc::clone(&invocations))))
        .event_persistence(Arc::new(CountingHostExtension(Arc::clone(&persisted))))
        .event_observability(Arc::new(CountingHostExtension(Arc::clone(&observed))))
        .build();

    assert_eq!(
        agent
            .generate_text("prompt")
            .await
            .expect("provider factory should be consumed")
            .text,
        "factory"
    );
    assert_eq!(
        agent
            .execute_tool_call(&call())
            .await
            .expect("custom invoker should execute")
            .invocation
            .output,
        "adapter-call:adapter-call"
    );
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert!(persisted.load(Ordering::SeqCst) > 0);
    assert_eq!(
        persisted.load(Ordering::SeqCst),
        observed.load(Ordering::SeqCst)
    );

    let authorizations = Arc::new(AtomicUsize::new(0));
    let denied = Agent::builder()
        .inline_tool(definition(), |_| Ok(response("must not run")))
        .tool_invoker(Arc::new(CountingInvoker(Arc::clone(&invocations))))
        .authorization_coordinator(Arc::new(DenyCoordinator(Arc::clone(&authorizations))))
        .build()
        .execute_tool_call(&call())
        .await;

    assert!(denied.is_err());
    assert_eq!(authorizations.load(Ordering::SeqCst), 1);
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
}
