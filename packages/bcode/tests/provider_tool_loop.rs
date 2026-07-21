use bcode::{
    Agent, AgentEvent, BcodeError, ModelContentBlock, ModelProviderInvoker, PreparationScope,
    PreparedToolInvocation, ProviderRoundPlan, ProviderRoundPlanContext, ProviderRoundPlanner,
    RegisteredTool, RuntimeFuture, ScopedAgentStreamItem, ScopedTurnEvent, ToolCall,
    ToolDefinition, ToolInvoker, ToolPreparationRequest, ToolPreparationResponse,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, MessageRole, ModelTurnRequest,
    PollTurnEventsRequest, PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
    StopReason,
};
use bcode_tool::{
    ToolContributionEvent, ToolContributionOperation, ToolContributionPersistence,
    ToolInvocationLifecycleEvent, ToolInvocationLifecycleStage, ToolInvocationResponse,
    ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
};
use std::collections::VecDeque;
use std::future::pending;
use std::num::NonZeroUsize;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Barrier;

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("provider batch tool {name}"),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: serde_json::Value::Null,
    }
}

#[derive(Debug)]
struct BatchProvider {
    requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    next_round: usize,
    events: VecDeque<ProviderTurnEvent>,
}

impl BatchProvider {
    fn new(requests: Arc<Mutex<Vec<ModelTurnRequest>>>) -> Self {
        Self {
            requests,
            next_round: 0,
            events: VecDeque::new(),
        }
    }
}

impl ModelProviderInvoker for BatchProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.requests
            .lock()
            .expect("provider requests lock")
            .push(request.clone());
        let round = self.next_round;
        self.next_round += 1;
        self.events = if round == 0 {
            [
                ProviderTurnEvent::ToolCallFinished {
                    call: call("call-first", "first"),
                },
                ProviderTurnEvent::ToolCallFinished {
                    call: call("call-second", "second"),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::ToolCall,
                },
            ]
            .into()
        } else {
            [
                ProviderTurnEvent::TextDelta {
                    text: "done".to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]
            .into()
        };
        Box::pin(async move {
            Ok(StartTurnResponse {
                provider_turn_id: format!("provider-round-{round}"),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        let events = self.events.drain(..).collect();
        Box::pin(async move { Ok(PollTurnEventsResponse { events }) })
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

#[derive(Debug)]
struct ParallelInvoker {
    barrier: Arc<Barrier>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

impl ToolInvoker for ParallelInvoker {
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
        scope: &'a bcode::InvocationScope,
    ) -> RuntimeFuture<'a, ToolInvocationResponse> {
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            assert!(scope.emit_lifecycle(ToolInvocationLifecycleEvent {
                invocation_id: scope.invocation_id().to_string(),
                sequence: 0,
                stage: ToolInvocationLifecycleStage::Progress,
                message: None,
                metadata: serde_json::Value::Null,
            }));
            assert!(scope.emit_contribution(ToolContributionEvent {
                invocation_id: scope.invocation_id().to_string(),
                contribution_id: "status".to_string(),
                sequence: 0,
                producer_id: "test".to_string(),
                schema: "test.status".to_string(),
                schema_version: 1,
                operation: ToolContributionOperation::Upsert,
                persistence: ToolContributionPersistence::Transient,
                payload: serde_json::Value::Null,
            }));
            self.barrier.wait().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolInvocationResponse {
                output: invocation.invocation.invocation_id.clone(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            })
        })
    }
}

fn agent_builder(invoker: Arc<ParallelInvoker>) -> bcode::AgentBuilder {
    Agent::builder()
        .max_tool_rounds(1)
        .execution_options(bcode::ToolExecutionOptions {
            max_concurrency: Some(NonZeroUsize::new(2).expect("two is non-zero")),
            ..bcode::ToolExecutionOptions::default()
        })
        .parallel_tool_capabilities(bcode_model::ParallelToolCallCapabilities {
            provider: true,
            model: true,
            canonical_runtime: true,
        })
        .inline_tool(definition("first"), |_| {
            unreachable!("custom invoker routes tools")
        })
        .inline_tool(definition("second"), |_| {
            unreachable!("custom invoker routes tools")
        })
        .tool_invoker(invoker)
}

fn agent(invoker: Arc<ParallelInvoker>) -> Agent {
    agent_builder(invoker).build()
}

fn invoker() -> (Arc<ParallelInvoker>, Arc<AtomicUsize>) {
    let maximum = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(ParallelInvoker {
            barrier: Arc::new(Barrier::new(2)),
            active: Arc::new(AtomicUsize::new(0)),
            maximum: Arc::clone(&maximum),
        }),
        maximum,
    )
}

#[derive(Debug, Default)]
struct SdkPlanner(AtomicUsize);

impl ProviderRoundPlanner for SdkPlanner {
    fn plan_round<'a>(
        &'a self,
        context: ProviderRoundPlanContext<'a>,
    ) -> RuntimeFuture<'a, ProviderRoundPlan> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let mut request = context.proposed_request.clone();
            request.tool_call_policy.parallel = false;
            request
                .metadata
                .insert("sdk_planner".to_string(), context.round.to_string());
            Ok(ProviderRoundPlan::Proceed { request })
        })
    }
}

#[tokio::test]
async fn sdk_builder_routes_provider_round_planner_through_canonical_loop() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = BatchProvider::new(Arc::clone(&requests));
    let (invoker, _) = invoker();
    let planner = Arc::new(SdkPlanner::default());
    let agent = agent_builder(invoker)
        .provider_round_planner(planner.clone())
        .build();

    let response = agent
        .run(&mut provider, "run tools")
        .await
        .expect("configured planner should run");

    assert_eq!(response.text, "done");
    assert_eq!(planner.0.load(Ordering::SeqCst), 2);
    let requests = requests.lock().expect("provider requests lock");
    assert!(
        requests
            .iter()
            .all(|request| request.tool_call_policy.parallel),
        "host planner must not override scheduler-owned provider parallel intent"
    );
    assert_eq!(
        requests[0].metadata.get("sdk_planner").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        requests[1].metadata.get("sdk_planner").map(String::as_str),
        Some("1")
    );
}

#[tokio::test]
async fn changing_model_after_capability_resolution_invalidates_parallel_signal() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = BatchProvider::new(Arc::clone(&requests));
    let (invoker, _) = invoker();
    let agent = agent_builder(invoker).model("different-model").build();

    agent
        .run(&mut provider, "run tools")
        .await
        .expect("selection change should safely fall back to sequential signaling");
    assert!(
        requests
            .lock()
            .expect("provider requests lock")
            .iter()
            .all(|request| !request.tool_call_policy.parallel)
    );
}

#[tokio::test]
async fn sdk_parallel_signal_falls_back_when_one_capability_is_missing() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = BatchProvider::new(Arc::clone(&requests));
    let (invoker, _) = invoker();
    let agent = agent_builder(invoker)
        .parallel_tool_capabilities(bcode_model::ParallelToolCallCapabilities {
            provider: true,
            model: false,
            canonical_runtime: true,
        })
        .build();

    agent
        .run(&mut provider, "run tools")
        .await
        .expect("sequential provider signal should not disable runtime execution");
    assert!(
        requests
            .lock()
            .expect("provider requests lock")
            .iter()
            .all(|request| !request.tool_call_policy.parallel)
    );
}

#[tokio::test]
async fn high_level_run_executes_provider_batch_once_and_returns_results_in_provider_order() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = BatchProvider::new(Arc::clone(&requests));
    let (invoker, maximum) = invoker();

    let response = agent(invoker)
        .run(&mut provider, "run tools")
        .await
        .expect("provider batch should complete automatically");

    assert_eq!(response.text, "done");
    assert_eq!(
        response
            .runtime
            .events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolResult(_)))
            .count(),
        2
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 2, "tools must overlap");
    let requests = requests.lock().expect("provider requests lock");
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.tool_call_policy.parallel),
        "canonical scheduler parallel intent must reach every provider continuation"
    );
    let feedback = &requests[1].messages;
    assert!(matches!(feedback[0].role, MessageRole::User));
    assert!(matches!(feedback[1].role, MessageRole::Assistant));
    let result_ids = feedback[2..]
        .iter()
        .map(|message| match &message.content[0] {
            ModelContentBlock::ToolResult { result } => result.call_id.as_str(),
            other => panic!("expected tool result, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(result_ids, ["call-first", "call-second"]);
}

#[tokio::test]
async fn high_level_run_preserves_tool_hooks_through_canonical_runtime_observer() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = BatchProvider::new(requests);
    let (invoker, _) = invoker();
    let before = Arc::new(AtomicUsize::new(0));
    let after = Arc::new(AtomicUsize::new(0));
    let before_count = Arc::clone(&before);
    let after_count = Arc::clone(&after);
    let agent = agent_builder(invoker)
        .on_before_tool(move |_| {
            before_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .on_after_tool(move |_, _| {
            after_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .build();

    let response = agent
        .run(&mut provider, "run tools")
        .await
        .expect("canonical observer should preserve hooks");

    assert_eq!(response.text, "done");
    assert_eq!(before.load(Ordering::SeqCst), 2);
    assert_eq!(after.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failing_before_tool_hook_prevents_canonical_batch_invocation() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = BatchProvider::new(requests);
    let (invoker, maximum) = invoker();
    let agent = agent_builder(invoker)
        .on_before_tool(|_| Err(BcodeError::Hook("blocked".to_string())))
        .build();

    let error = agent
        .run(&mut provider, "run tools")
        .await
        .expect_err("failing hook should reject the tool batch");

    assert!(matches!(error, BcodeError::Hook(message) if message == "blocked"));
    assert_eq!(maximum.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn generic_stream_exposes_runtime_lifecycle_and_contribution_events() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = BatchProvider::new(requests);
    let (invoker, maximum) = invoker();
    let mut stream = agent(invoker).stream(provider, "stream tools");
    let mut runtime = false;
    let mut lifecycle = false;
    let mut contribution = false;
    let mut finished = None;

    while let Some(item) = stream.next().await {
        match item {
            ScopedAgentStreamItem::Event(ScopedTurnEvent::Runtime(AgentEvent::ToolResult(_))) => {
                runtime = true
            }
            ScopedAgentStreamItem::Event(ScopedTurnEvent::InvocationLifecycle(_)) => {
                lifecycle = true;
            }
            ScopedAgentStreamItem::Event(ScopedTurnEvent::Contribution(_)) => contribution = true,
            ScopedAgentStreamItem::Event(_) => {}
            ScopedAgentStreamItem::Finished(response) => {
                finished = Some(response.text);
                break;
            }
            ScopedAgentStreamItem::Error(error) => panic!("stream failed: {error}"),
        }
    }

    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    assert!(runtime);
    assert!(lifecycle);
    assert!(contribution);
    assert_eq!(finished.as_deref(), Some("done"));
}

#[derive(Debug)]
struct BlockingInvoker {
    started: Arc<Barrier>,
}

impl ToolInvoker for BlockingInvoker {
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
        _invocation: &'a PreparedToolInvocation,
        _scope: &'a bcode::InvocationScope,
    ) -> RuntimeFuture<'a, ToolInvocationResponse> {
        Box::pin(async move {
            self.started.wait().await;
            pending().await
        })
    }
}

#[tokio::test]
async fn generic_stream_cancellation_terminates_blocked_provider_batch_immediately() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = BatchProvider::new(requests);
    let started = Arc::new(Barrier::new(3));
    let agent = Agent::builder()
        .inline_tool(definition("first"), |_| {
            unreachable!("custom invoker routes tools")
        })
        .inline_tool(definition("second"), |_| {
            unreachable!("custom invoker routes tools")
        })
        .tool_invoker(Arc::new(BlockingInvoker {
            started: Arc::clone(&started),
        }))
        .build();
    let cancellation = bcode::CancellationToken::new();
    let mut stream = agent.stream_with_cancellation(provider, "cancel tools", cancellation.clone());
    started.wait().await;

    cancellation.cancel();
    let terminal = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        loop {
            match stream.next().await {
                Some(ScopedAgentStreamItem::Error(error)) => break error,
                Some(ScopedAgentStreamItem::Finished(_)) => panic!("cancelled stream finished"),
                Some(ScopedAgentStreamItem::Event(_)) => {}
                None => panic!("cancelled stream closed without terminal item"),
            }
        }
    })
    .await
    .expect("cancellation must not wait for blocked invocations");

    assert!(matches!(
        terminal,
        bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled)
    ));
}

#[derive(Debug, Default)]
struct DependentRoundProvider {
    next_round: usize,
    events: VecDeque<ProviderTurnEvent>,
}

impl ModelProviderInvoker for DependentRoundProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        let round = self.next_round;
        self.next_round += 1;
        match round {
            0 => {
                assert!(!request.messages.iter().any(|message| {
                    message
                        .content
                        .iter()
                        .any(|content| matches!(content, ModelContentBlock::ToolResult { .. }))
                }));
                self.events = [
                    ProviderTurnEvent::ToolCallFinished {
                        call: call("call-first", "first"),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::ToolCall,
                    },
                ]
                .into();
            }
            1 => {
                assert!(request.messages.iter().any(|message| {
                    message.content.iter().any(|content| {
                        matches!(
                            content,
                            ModelContentBlock::ToolResult { result }
                                if result.call_id == "call-first"
                        )
                    })
                }));
                self.events = [
                    ProviderTurnEvent::ToolCallFinished {
                        call: call("call-second", "second"),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::ToolCall,
                    },
                ]
                .into();
            }
            2 => {
                assert!(request.messages.iter().any(|message| {
                    message.content.iter().any(|content| {
                        matches!(
                            content,
                            ModelContentBlock::ToolResult { result }
                                if result.call_id == "call-second"
                        )
                    })
                }));
                self.events = [
                    ProviderTurnEvent::TextDelta {
                        text: "dependent done".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ]
                .into();
            }
            _ => panic!("unexpected dependent provider round {round}"),
        }
        Box::pin(async move {
            Ok(StartTurnResponse {
                provider_turn_id: format!("dependent-round-{round}"),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        let events = self.events.drain(..).collect();
        Box::pin(async move { Ok(PollTurnEventsResponse { events }) })
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

#[tokio::test]
async fn dependent_calls_in_later_provider_rounds_wait_for_prior_results() {
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&invocations);
    let agent = Agent::builder()
        .max_tool_rounds(2)
        .scoped_inline_tool(definition("first"), move |invocation, _scope| {
            let observed = Arc::clone(&observed);
            async move {
                observed
                    .lock()
                    .expect("invocation order lock")
                    .push(invocation.invocation_id.clone());
                Ok(ToolInvocationResponse {
                    output: "first complete".to_string(),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                    host_action: None,
                    result: None,
                })
            }
        })
        .scoped_inline_tool(definition("second"), {
            let invocations = Arc::clone(&invocations);
            move |invocation, _scope| {
                let invocations = Arc::clone(&invocations);
                async move {
                    invocations
                        .lock()
                        .expect("invocation order lock")
                        .push(invocation.invocation_id.clone());
                    Ok(ToolInvocationResponse {
                        output: "second complete".to_string(),
                        is_error: false,
                        content: Vec::new(),
                        full_output: None,
                        host_action: None,
                        result: None,
                    })
                }
            }
        })
        .build();
    let mut provider = DependentRoundProvider::default();
    let response = agent
        .run(&mut provider, "run dependent tools")
        .await
        .expect("dependent provider rounds should complete");

    assert_eq!(response.text, "dependent done");
    assert_eq!(
        *invocations.lock().expect("invocation order lock"),
        ["call-first", "call-second"]
    );
}
