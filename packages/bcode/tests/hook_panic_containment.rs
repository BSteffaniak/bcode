#![cfg(feature = "testing")]

use bcode::{
    Agent, BcodeError, CancellationToken, ModelMiddleware, ProviderTurnEvent,
    ScopedAgentStreamItem, StopReason, TextStreamItem,
    testing::{ScriptedProvider, ScriptedProviderTurn},
};
use std::sync::{Arc, Barrier};

#[derive(Debug)]
struct PanicBeforeMiddleware;

impl ModelMiddleware for PanicBeforeMiddleware {
    fn before_request(
        &self,
        _request: bcode::AgentTurnRequest,
    ) -> bcode::Result<bcode::AgentTurnRequest> {
        panic!("before middleware panic")
    }
}

#[derive(Debug)]
struct PanicAfterMiddleware;

impl ModelMiddleware for PanicAfterMiddleware {
    fn after_response(
        &self,
        _request: &bcode::AgentTurnRequest,
        _response: bcode::GenerateTextResponse,
    ) -> bcode::Result<bcode::GenerateTextResponse> {
        panic!("after middleware panic")
    }
}

fn provider() -> ScriptedProvider {
    ScriptedProvider::new([ScriptedProviderTurn::complete_text("answer")])
}

#[tokio::test]
async fn middleware_panics_are_typed_for_buffered_and_streaming_calls() {
    let agent = Agent::builder()
        .model("model")
        .middleware_layer(PanicBeforeMiddleware)
        .build();
    let error = agent
        .run(&mut provider(), "hello")
        .await
        .expect_err("panic is contained");
    assert!(matches!(
        error,
        BcodeError::Hook(message) if message.contains("before_request panicked")
    ));

    let mut stream = agent.stream(provider(), "hello");
    let terminal = stream.next().await.expect("terminal item");
    assert!(matches!(
        terminal,
        ScopedAgentStreamItem::Error(BcodeError::Hook(message))
            if message.contains("before_request panicked")
    ));
    assert!(stream.next().await.is_none());

    let agent = Agent::builder()
        .model("model")
        .middleware_layer(PanicAfterMiddleware)
        .build();
    let error = agent
        .run(&mut provider(), "hello")
        .await
        .expect_err("response panic is contained");
    assert!(matches!(
        error,
        BcodeError::Hook(message) if message.contains("after_response panicked")
    ));

    let mut stream = agent.stream_text_with_provider(provider(), "hello");
    let mut terminal_errors = 0;
    while let Some(item) = stream.next().await {
        if let TextStreamItem::Error(BcodeError::Hook(message)) = item {
            assert!(message.contains("after_response panicked"));
            terminal_errors += 1;
        }
    }
    assert_eq!(terminal_errors, 1);
}

#[tokio::test]
async fn hook_panics_are_contained_without_corrupting_provider_cleanup() {
    let provider = provider();
    let probe = provider.probe();
    let agent = Agent::builder()
        .model("model")
        .on_after_model(|_, _| panic!("after hook panic"))
        .build();

    let error = agent
        .run(&mut provider.clone(), "hello")
        .await
        .expect_err("hook panic is typed");
    assert!(matches!(
        error,
        BcodeError::Hook(message) if message.contains("after-model hook panicked")
    ));
    probe
        .assert_finish_count(1)
        .expect("provider lifecycle completed before hook failure");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_remain_panic_contained_under_concurrent_execution_and_cancellation() {
    let barrier = Arc::new(Barrier::new(2));
    let agent = Arc::new(
        Agent::builder()
            .model("model")
            .on_before_model({
                let barrier = Arc::clone(&barrier);
                move |_| {
                    barrier.wait();
                    panic!("concurrent hook panic")
                }
            })
            .build(),
    );
    let cancellation = CancellationToken::new();
    let cancellation_for_task = cancellation.clone();
    let agent_for_task = Arc::clone(&agent);
    let task = tokio::spawn(async move {
        agent_for_task
            .stream_text_with_provider_and_cancellation(provider(), "hello", cancellation_for_task)
            .next()
            .await
    });
    barrier.wait();
    cancellation.cancel();
    let terminal = task.await.expect("task joins").expect("terminal item");
    assert!(matches!(
        terminal,
        TextStreamItem::Error(BcodeError::Hook(_))
    ));
}

#[tokio::test]
async fn before_tool_hook_panic_prevents_execution_and_stream_terminates_once() {
    let tool_provider = ScriptedProvider::new([ScriptedProviderTurn::new().events([
        ProviderTurnEvent::ToolCallFinished {
            call: bcode::ToolCall {
                id: "call-1".to_string(),
                name: "missing".to_string(),
                arguments: serde_json::json!({}),
            },
        },
        ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::ToolCall,
        },
    ])]);
    let agent = Agent::builder()
        .on_before_tool(|_| panic!("tool hook panic"))
        .build();
    let mut stream = agent.stream(tool_provider, "hello");
    let mut errors = 0;
    while let Some(item) = stream.next().await {
        if let ScopedAgentStreamItem::Error(BcodeError::Hook(message)) = item {
            assert!(message.contains("before-tool hook panicked"));
            errors += 1;
        }
    }
    assert_eq!(errors, 1);
}
