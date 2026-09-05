#![cfg(feature = "testing")]

use bcode::{
    Agent, AgentTurnRequest, BcodeError, GenerationStep, ModelResponseCache, PermissionDecision,
    PersistedSession, ProviderTurnEvent, RuntimeError, SessionId, SessionPersistenceAdapter,
    StopReason, ToolCall, ToolDefinition, generate_text_builder,
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

#[test]
fn request_identity_scripts_reject_duplicates_and_empty_turn_ids() {
    let identity = bcode::ProviderRequestIdentity {
        session_id: "00000000-0000-4000-8000-000000000125"
            .parse()
            .expect("fixture ID"),
        turn_id: "same".to_string(),
    };
    assert!(
        bcode::testing::ScriptedRequestIdentities::new([identity.clone(), identity.clone()])
            .is_err()
    );
    assert!(
        bcode::testing::ScriptedRequestIdentities::new([bcode::ProviderRequestIdentity {
            turn_id: String::new(),
            ..identity
        }])
        .is_err()
    );
}

#[tokio::test]
async fn explicit_request_identity_reaches_provider_and_exhaustion_prevents_start() {
    let session_id: SessionId = "00000000-0000-4000-8000-000000000125"
        .parse()
        .expect("fixture ID");
    let identities = bcode::testing::ScriptedRequestIdentities::new((0..2).map(|index| {
        bcode::ProviderRequestIdentity {
            session_id,
            turn_id: format!("fixture-{index}"),
        }
    }))
    .expect("valid identities");
    let runtime =
        bcode::AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities));
    let mut provider = ScriptedProvider::new([
        ScriptedProviderTurn::complete_text("one"),
        ScriptedProviderTurn::complete_text("two"),
    ]);
    let probe = provider.probe();
    let agent = bcode::AgentBuilder::from_context(session_id, std::env::temp_dir())
        .runtime(runtime)
        .build();
    agent
        .run(&mut provider, "one")
        .await
        .expect("first request");
    agent
        .run(&mut provider, "two")
        .await
        .expect("second request");
    let error = agent
        .run(&mut provider, "three")
        .await
        .expect_err("exhaustion");
    assert!(
        matches!(error, BcodeError::Runtime(RuntimeError::ProviderInvocation(message)) if message == "request identity script exhausted")
    );
    let requests = probe.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].request.session_id, session_id);
    assert_eq!(requests[1].request.session_id, session_id);
    assert_eq!(requests[0].request.turn_id, "fixture-0");
    assert_eq!(requests[1].request.turn_id, "fixture-1");
}

#[tokio::test]
async fn explicit_builder_context_reaches_tool_authorization() {
    let session_id: SessionId = "00000000-0000-4000-8000-000000000123"
        .parse()
        .expect("fixture ID");
    let cwd = std::env::temp_dir().join("bcode-explicit-context-fixture");
    let permissions = ScriptedPermissionPolicy::new([PermissionDecision::Allow]);
    let probe = permissions.clone();
    let tool = ScriptedTool::new([ScriptedToolOutcome::text("explicit context")]);
    let agent = tool
        .register(
            bcode::AgentBuilder::from_context(session_id, cwd),
            tool_definition(),
        )
        .custom_permission_policy(permissions)
        .build();
    let response = agent
        .run(&mut tool_provider(), "use tool")
        .await
        .expect("tool loop");
    assert_eq!(response.text, "after tool");
    assert_eq!(probe.requests()[0].context.session_id, session_id);
}

#[tokio::test]
async fn explicit_builder_context_denial_prevents_tool_execution() {
    let session_id: SessionId = "00000000-0000-4000-8000-000000000124"
        .parse()
        .expect("fixture ID");
    let permissions =
        ScriptedPermissionPolicy::new([PermissionDecision::Deny("fixture denial".to_string())]);
    let permission_probe = permissions.clone();
    let tool = ScriptedTool::new([ScriptedToolOutcome::text("must not execute")]);
    let tool_probe = tool.probe();
    let agent = tool
        .register(
            bcode::AgentBuilder::from_context(session_id, std::env::temp_dir()),
            tool_definition(),
        )
        .custom_permission_policy(permissions)
        .build();
    let result = agent.run(&mut tool_provider(), "denied tool").await;
    assert_eq!(tool_probe.invocation_count(), 0);
    let requests = permission_probe.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].context.session_id, session_id);
    let response = result.expect("denial remains visible to the provider for continuation");
    assert_eq!(response.text, "after tool");
    assert!(response.steps.iter().any(|step| matches!(
        step,
        GenerationStep::ToolResult { result, .. }
            if result.is_error && result.output == "tool execution denied: fixture denial"
    )));
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
