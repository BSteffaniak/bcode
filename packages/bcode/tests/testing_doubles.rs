#![cfg(feature = "testing")]

use bcode::{
    Agent, AgentTurnRequest, BcodeError, GenerationStep, ModelResponseCache, PermissionDecision,
    PersistedSession, ProviderTurnEvent, RuntimeError, SessionId, SessionPersistenceAdapter,
    StopReason, ToolCall, ToolDefinition, ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
    generate_text_builder,
    testing::{
        ManualClock, ScriptedCacheOperation, ScriptedModelResponseCache, ScriptedPermissionPolicy,
        ScriptedProvider, ScriptedProviderTurn, ScriptedSessionStore, ScriptedTool,
        ScriptedToolOutcome,
    },
};
use std::sync::Arc;
use std::time::Duration;

fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "scripted".to_string(),
        description: "Deterministic scripted tool".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: true,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn tool_provider() -> ScriptedProvider {
    ScriptedProvider::new([
        ScriptedProviderTurn::new().events([
            ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id: "call-1".to_string(),
                    name: "scripted".to_string(),
                    arguments: serde_json::json!({"input": 1}),
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::ToolCall,
            },
        ]),
        ScriptedProviderTurn::complete_text("after tool"),
    ])
}

#[tokio::test]
async fn scripted_tools_and_permissions_capture_canonical_requests() {
    let tool = ScriptedTool::new([ScriptedToolOutcome::text("tool output")]);
    let tool_probe = tool.probe();
    let permissions = ScriptedPermissionPolicy::new([PermissionDecision::Allow]);
    let permission_probe = permissions.clone();
    let agent = tool
        .register(Agent::builder(), tool_definition())
        .custom_permission_policy(permissions)
        .build();

    let response = agent
        .run(&mut tool_provider(), "use the tool")
        .await
        .expect("scripted tool loop");
    assert_eq!(response.text, "after tool");
    assert_eq!(tool_probe.invocation_count(), 1);
    assert_eq!(tool_probe.invocations()[0].request.arguments["input"], 1);
    assert_eq!(permission_probe.requests().len(), 1);
    assert_eq!(permission_probe.requests()[0].call.name, "scripted");
    assert!(response.steps.iter().any(|step| matches!(
        step,
        GenerationStep::ToolResult { result, .. } if result.output == "tool output"
    )));
}

#[tokio::test]
async fn scripted_tool_delay_error_and_cancellation_are_network_free() {
    let delayed =
        ScriptedTool::new([ScriptedToolOutcome::text("delayed").after(Duration::from_millis(1))]);
    let delayed_agent = delayed
        .register(Agent::builder(), tool_definition())
        .custom_permission_policy(ScriptedPermissionPolicy::new([PermissionDecision::Allow]))
        .build();
    let delayed_response = delayed_agent
        .run(&mut tool_provider(), "delay")
        .await
        .expect("delayed tool");
    assert!(delayed_response.steps.iter().any(|step| matches!(
        step,
        GenerationStep::ToolResult { result, .. } if result.output == "delayed"
    )));

    let failing = ScriptedTool::new([ScriptedToolOutcome::Error("fixture failure".to_string())]);
    let failing_agent = failing
        .register(Agent::builder(), tool_definition())
        .custom_permission_policy(ScriptedPermissionPolicy::new([PermissionDecision::Allow]))
        .build();
    let failed_response = failing_agent
        .run(&mut tool_provider(), "fail")
        .await
        .expect("tool failure remains model-visible");
    assert!(failed_response.steps.iter().any(|step| matches!(
        step,
        GenerationStep::ToolResult { result, .. }
            if result.is_error && result.output.contains("fixture failure")
    )));

    let pending = ScriptedTool::new([ScriptedToolOutcome::PendingUntilCancelled]);
    let pending_probe = pending.probe();
    let cancellation = bcode::CancellationToken::new();
    let pending_agent = pending
        .register(Agent::builder(), tool_definition())
        .custom_permission_policy(ScriptedPermissionPolicy::new([PermissionDecision::Allow]))
        .build();
    let mut stream = pending_agent.stream_text_with_provider_and_cancellation(
        tool_provider(),
        "cancel tool",
        cancellation.clone(),
    );
    while pending_probe.invocation_count() == 0 {
        let item = stream.next().await.expect("stream remains active");
        assert!(!matches!(item, bcode::TextStreamItem::Error(_)));
    }
    cancellation.cancel();
    let mut terminal = None;
    while let Some(item) = stream.next().await {
        if let bcode::TextStreamItem::Error(error) = item {
            terminal = Some(error);
        }
    }
    assert!(matches!(
        terminal,
        Some(BcodeError::Runtime(RuntimeError::Cancelled))
    ));
}

#[tokio::test]
async fn scripted_cache_captures_hits_misses_and_failures() {
    let cache = Arc::new(ScriptedModelResponseCache::new());
    let mut provider = ScriptedProvider::new([ScriptedProviderTurn::complete_text("cached")]);
    let first = generate_text_builder()
        .prompt("cache me")
        .response_cache(cache.clone())
        .run(&mut provider)
        .await
        .expect("cache miss stores response");
    assert_eq!(first.text, "cached");

    let mut provider_without_script = ScriptedProvider::new([]);
    let second = generate_text_builder()
        .prompt("cache me")
        .response_cache(cache.clone())
        .run(&mut provider_without_script)
        .await
        .expect("cache hit bypasses provider");
    assert_eq!(second.text, "cached");
    assert_eq!(
        cache.operations(),
        vec![
            ScriptedCacheOperation::Get,
            ScriptedCacheOperation::Put,
            ScriptedCacheOperation::Get,
        ]
    );

    let failing_cache = Arc::new(ScriptedModelResponseCache::new());
    failing_cache.fail_next(ScriptedCacheOperation::Get, "fixture cache failure");
    let error = generate_text_builder()
        .prompt("fail cache")
        .response_cache(failing_cache)
        .run(&mut ScriptedProvider::new([]))
        .await
        .expect_err("cache failure is typed");
    assert!(matches!(error, BcodeError::Cache(message) if message == "fixture cache failure"));
}

#[test]
fn scripted_session_store_captures_payloads_and_failures() {
    let session_id = SessionId::new();
    let persisted = PersistedSession {
        schema_version: bcode::PERSISTED_SESSION_SCHEMA_VERSION,
        session_id,
        messages: Vec::new(),
        memories: Vec::new(),
    };
    let store = ScriptedSessionStore::new().with_session(persisted.clone());
    let loaded = store.load().expect("load").expect("stored session");
    assert_eq!(loaded, persisted);
    store.save(&persisted).expect("save");
    assert_eq!(store.load_count(), 1);
    assert_eq!(store.saves(), vec![persisted]);

    store.set_load_failure(Some("load failed".to_string()));
    assert!(matches!(
        store.load(),
        Err(BcodeError::SessionPersistence(message)) if message == "load failed"
    ));
    store.set_load_failure(None);
    store.set_save_failure(Some("save failed".to_string()));
    assert!(matches!(
        store.save(&loaded),
        Err(BcodeError::SessionPersistence(message)) if message == "save failed"
    ));
}

#[tokio::test]
async fn manual_clock_advances_without_wall_clock_sleep() {
    let clock = ManualClock::new();
    let sleeper = clock.clone();
    let task = tokio::spawn(async move {
        sleeper.sleep(Duration::from_secs(30)).await;
        sleeper.now()
    });
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    clock.advance(Duration::from_secs(29));
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    clock.advance(Duration::from_secs(1));
    assert_eq!(
        task.await.expect("manual sleep completes"),
        Duration::from_secs(30)
    );
}

#[test]
fn cache_fixture_implements_public_adapter_contract() {
    fn accepts_cache<T: ModelResponseCache>(_cache: &T) {}
    accepts_cache(&ScriptedModelResponseCache::new());
    let _request = AgentTurnRequest::new("model", "prompt");
}
