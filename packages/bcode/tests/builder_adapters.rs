use bcode::{
    Agent, InvocationScope, PreparationScope, PreparedToolInvocation, RegisteredTool,
    ToolAuthorizationCoordinator, ToolAuthorizationDecision, ToolAuthorizationRequest, ToolCall,
    ToolDefinition, ToolInvocationResponse, ToolInvoker, ToolPolicyIdentity, ToolPolicyOperation,
    ToolPolicyPreparation, ToolPreparationRequest, ToolPreparationResponse, TurnEventObservability,
    TurnEventPersistence,
};
use bcode_agent_runtime::{ModelProviderInvoker, RuntimeFuture, TurnScope};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use bcode_tool::{ToolContributionEvent, ToolContributionOperation, ToolContributionPersistence};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "adapter".to_string(),
        description: "adapter routing test".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
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

#[derive(Debug)]
struct WorkspaceContextInvoker(Arc<Mutex<Option<bcode_tool::ToolHostContextEntry>>>);

impl ToolInvoker for WorkspaceContextInvoker {
    fn prepare_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        request: &'a ToolPreparationRequest,
        scope: &'a PreparationScope,
    ) -> RuntimeFuture<'a, ToolPreparationResponse> {
        assert_eq!(scope.host_context(), request.host_context);
        let entry = request
            .host_context
            .iter()
            .find(|entry| entry.schema == bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA)
            .expect("direct SDK workspace context")
            .clone();
        *self.0.lock().expect("workspace observation") = Some(entry);
        let result = bcode_agent_profile::prepare_tool_policy(
            request,
            &tool.definition,
            bcode_agent_profile::ToolPolicyPreparation::read_only(),
        )
        .map_err(|message| bcode::RuntimeError::ToolPreparation {
            tool_name: request.invocation.tool_name.clone(),
            message,
        });
        Box::pin(async move { result })
    }

    fn invoke_tool<'a>(
        &'a self,
        _tool: &'a RegisteredTool,
        _invocation: &'a PreparedToolInvocation,
        _scope: &'a InvocationScope,
    ) -> RuntimeFuture<'a, ToolInvocationResponse> {
        Box::pin(async { Ok(response("workspace observed")) })
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
        let result = bcode_agent_profile::prepare_tool_policy(
            request,
            &tool.definition,
            bcode_agent_profile::ToolPolicyPreparation::read_only(),
        )
        .map_err(|message| bcode::RuntimeError::ToolPreparation {
            tool_name: request.invocation.tool_name.clone(),
            message,
        });
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
struct ContributionInvoker(ToolContributionPersistence);

impl ToolInvoker for ContributionInvoker {
    fn prepare_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        request: &'a ToolPreparationRequest,
        _scope: &'a PreparationScope,
    ) -> RuntimeFuture<'a, ToolPreparationResponse> {
        let result = bcode_agent_profile::prepare_tool_policy(
            request,
            &tool.definition,
            bcode_agent_profile::ToolPolicyPreparation::read_only(),
        )
        .map_err(|message| bcode::RuntimeError::ToolPreparation {
            tool_name: request.invocation.tool_name.clone(),
            message,
        });
        Box::pin(async move { result })
    }

    fn invoke_tool<'a>(
        &'a self,
        _tool: &'a RegisteredTool,
        _invocation: &'a PreparedToolInvocation,
        scope: &'a InvocationScope,
    ) -> RuntimeFuture<'a, ToolInvocationResponse> {
        let accepted = scope.emit_contribution(ToolContributionEvent {
            invocation_id: scope.invocation_id().to_string(),
            contribution_id: "opaque-surface".to_string(),
            sequence: 1,
            producer_id: "sdk-test".to_string(),
            schema: "example.unknown/v9".to_string(),
            schema_version: 9,
            operation: ToolContributionOperation::Upsert,
            persistence: self.0,
            artifact: None,
            payload: serde_json::json!({"opaque": [1, 2, 3]}),
        });
        Box::pin(async move {
            assert!(accepted, "contribution should reach SDK publication");
            Ok(response("contribution emitted"))
        })
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

struct ContributionCountingHostExtension(Arc<AtomicUsize>);

impl TurnEventPersistence for ContributionCountingHostExtension {
    fn persist(&self, event: &bcode::ScopedTurnEvent) -> bool {
        if matches!(event, bcode::ScopedTurnEvent::Contribution(_)) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        true
    }
}

impl TurnEventObservability for ContributionCountingHostExtension {
    fn observe(&self, event: &bcode::ScopedTurnEvent) {
        if matches!(event, bcode::ScopedTurnEvent::Contribution(_)) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[derive(Debug)]
struct AssertOwnerIdentityCoordinator;

impl ToolAuthorizationCoordinator for AssertOwnerIdentityCoordinator {
    fn authorize_batch<'a>(
        &'a self,
        requests: &'a [ToolAuthorizationRequest],
        _scope: &'a TurnScope,
    ) -> RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
        let metadata = requests
            .first()
            .and_then(|request| {
                bcode_agent_profile::tool_policy_authorization_metadata(
                    &request.facts,
                    &request.call.name,
                )
                .ok()
            })
            .expect("owner policy identity should be encoded");
        assert_eq!(metadata.aliases, vec!["adapter-alias"]);
        assert_eq!(metadata.capabilities, vec!["adapter.capability"]);
        assert_eq!(metadata.permission_category.as_deref(), Some("adapter"));
        Box::pin(async { Ok(vec![ToolAuthorizationDecision::Allow]) })
    }
}

#[tokio::test]
async fn direct_sdk_supplies_versioned_workspace_context_to_preparation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let observed = Arc::new(Mutex::new(None));
    let agent = Agent::builder()
        .cwd(workspace.path())
        .inline_tool(definition(), |_| Ok(response("inline")))
        .tool_invoker(Arc::new(WorkspaceContextInvoker(Arc::clone(&observed))))
        .build();

    agent
        .execute_tool_call(&call())
        .await
        .expect("workspace-aware invocation");

    let observed = observed.lock().expect("workspace observation");
    let entry = observed.as_ref().expect("workspace context");
    assert_eq!(entry.schema, bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA);
    assert_eq!(
        entry.schema_version,
        bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION
    );
    assert_eq!(
        entry.payload["working_directory"],
        workspace.path().display().to_string()
    );
}

#[tokio::test]
async fn inline_tool_policy_identity_is_owner_prepared() {
    let agent = Agent::builder()
        .inline_tool_with_policy(
            definition(),
            ToolPolicyPreparation::new(false, ToolPolicyOperation::ReadOnly).with_identity(
                ToolPolicyIdentity {
                    aliases: vec!["adapter-alias".to_owned()],
                    compatibility_aliases: Vec::new(),
                    capabilities: vec!["adapter.capability".to_owned()],
                    permission_category: Some("adapter".to_owned()),
                },
            ),
            |_| Ok(response("prepared")),
        )
        .authorization_coordinator(Arc::new(AssertOwnerIdentityCoordinator))
        .build();

    let output = agent
        .execute_tool_call(&call())
        .await
        .expect("owner-prepared inline tool should execute");
    assert_eq!(output.invocation.output, "prepared");
}

#[tokio::test]
async fn sdk_builder_persists_only_durable_contributions() {
    for (persistence, expected_persisted) in [
        (ToolContributionPersistence::Transient, 0),
        (ToolContributionPersistence::Durable, 1),
    ] {
        let persisted = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(AtomicUsize::new(0));
        let agent = Agent::builder()
            .inline_tool(definition(), |_| Ok(response("unused")))
            .tool_invoker(Arc::new(ContributionInvoker(persistence)))
            .event_persistence(Arc::new(ContributionCountingHostExtension(Arc::clone(
                &persisted,
            ))))
            .event_observability(Arc::new(ContributionCountingHostExtension(Arc::clone(
                &observed,
            ))))
            .build();

        agent
            .execute_tool_call(&call())
            .await
            .expect("contribution tool should execute");

        assert_eq!(persisted.load(Ordering::SeqCst), expected_persisted);
        assert_eq!(observed.load(Ordering::SeqCst), 1);
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
