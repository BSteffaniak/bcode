//! Native delegation behavior through the host operation, without an external daemon.

use super::*;
use bcode_session_models::delegation::{
    NATIVE_DELEGATION_CONTEXT_OPERATION, NATIVE_DELEGATION_DELEGATE_OPERATION,
    NATIVE_DELEGATION_INTERFACE_ID, NATIVE_DELEGATION_RESULT_OPERATION,
    NATIVE_DELEGATION_WAIT_OPERATION, NativeAdmissionResponse, NativeChildStatus,
    NativeContextResponse, NativeDelegateRequest, NativeDelegationLimits, NativeSummariesResponse,
    NativeWaitUntil, SessionDelegationPermissions,
};

use bcode_session_models::{SessionChildCreateRequest, SessionSpawnRequest, TurnPermissionMode};

const PLUGIN: &str = "bcode.harness";

struct Fixture {
    _root: tempfile::TempDir,
    state: Arc<ServerState>,
    parent: SessionSummary,
    cancel_state: TurnCancelState,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().expect("session root");
        let sessions = SessionManager::persistent(root.path()).expect("persistent sessions");
        let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
        let parent = state
            .sessions
            .create_session(Some("parent".into()), std::env::current_dir().unwrap())
            .await
            .unwrap();
        Self {
            _root: root,
            state,
            parent,
            cancel_state: TurnCancelState::default(),
        }
    }

    async fn call(
        &self,
        session_id: SessionId,
        operation: &str,
        payload: serde_json::Value,
    ) -> ToolInvocationServiceResolution {
        native_delegation::invocation(
            &self.state,
            native_delegation::Caller {
                session_id,
                plugin_id: PLUGIN,
                invocation_id: "call-1",
                cancel_state: &self.cancel_state,
            },
            bcode_tool::ToolInvocationServiceRequest {
                request_id: "call-1".into(),
                invocation_id: "call-1".into(),
                route_id: Some(NATIVE_DELEGATION_INTERFACE_ID.into()),
                interface_id: NATIVE_DELEGATION_INTERFACE_ID.into(),
                operation: operation.into(),
                payload,
            },
        )
        .await
    }

    async fn ok<T: serde::de::DeserializeOwned>(
        &self,
        session_id: SessionId,
        operation: &str,
        payload: serde_json::Value,
    ) -> T {
        match self.call(session_id, operation, payload).await {
            ToolInvocationServiceResolution::Responded { payload } => {
                serde_json::from_value(payload).unwrap()
            }
            other => panic!("{operation} failed: {other:?}"),
        }
    }

    async fn failure_code(
        &self,
        session_id: SessionId,
        operation: &str,
        payload: serde_json::Value,
    ) -> String {
        match self.call(session_id, operation, payload).await {
            ToolInvocationServiceResolution::Failed { code, .. } => code,
            other => panic!("{operation} unexpectedly succeeded: {other:?}"),
        }
    }
}

fn delegate_request(child_id: SessionId) -> NativeDelegateRequest {
    NativeDelegateRequest {
        child_id,
        title: Some("child task".into()),
        prompt: "echo-all delegated work".into(),
        agent: None,
        model: None,
        working_directory: None,
        worktree: false,
        permissions: SessionDelegationPermissions::AlwaysAsk,
        cancel_with_turn: true,
        limits: NativeDelegationLimits::default(),
    }
}

#[tokio::test]
async fn delegated_children_run_in_this_daemon_and_report_typed_results() {
    let fixture = Fixture::new().await;
    let child_id = SessionId::new();
    let request = serde_json::to_value(delegate_request(child_id)).unwrap();
    let admitted: NativeAdmissionResponse = fixture
        .ok(
            fixture.parent.id,
            NATIVE_DELEGATION_DELEGATE_OPERATION,
            request.clone(),
        )
        .await;
    assert_eq!(admitted.child.session_id, child_id);
    assert!(!admitted.reused);

    // The child is an ordinary child whose creation retains the delegation intent.
    let summary = fixture
        .state
        .sessions
        .session_summary_non_owning(child_id)
        .await
        .unwrap();
    assert_eq!(summary.parent_id, Some(fixture.parent.id));
    let (parent, intent) = native_delegation::delegation_intent(&fixture.state, child_id)
        .await
        .unwrap()
        .expect("retained intent");
    assert_eq!(parent, fixture.parent.id);
    assert_eq!(intent.producer_plugin_id, PLUGIN);
    assert_eq!(intent.invocation_id, "call-1");
    assert_eq!(intent.permissions, SessionDelegationPermissions::AlwaysAsk);
    assert!(intent.cancel_with_turn);

    let waited: NativeSummariesResponse = fixture
        .ok(
            fixture.parent.id,
            NATIVE_DELEGATION_WAIT_OPERATION,
            serde_json::json!({
                "session_ids": [child_id],
                "until": NativeWaitUntil::All,
                "timeout_ms": 30_000,
            }),
        )
        .await;
    assert!(!waited.timed_out, "{waited:?}");
    let child = &waited.children[0];
    assert_ne!(child.status, NativeChildStatus::Running);
    assert!(
        child
            .latest_result
            .as_deref()
            .is_some_and(|text| text.contains("delegated work")),
        "{child:?}"
    );

    // An identical retry reconciles with the existing child instead of creating another.
    let retried: NativeAdmissionResponse = fixture
        .ok(
            fixture.parent.id,
            NATIVE_DELEGATION_DELEGATE_OPERATION,
            request,
        )
        .await;
    assert!(retried.reused);
    assert_eq!(retried.child.session_id, child_id);

    // Reusing the identity with different options fails closed.
    let mut conflicting = delegate_request(child_id);
    conflicting.permissions = SessionDelegationPermissions::Inherit;
    assert_eq!(
        fixture
            .failure_code(
                fixture.parent.id,
                NATIVE_DELEGATION_DELEGATE_OPERATION,
                serde_json::to_value(conflicting).unwrap(),
            )
            .await,
        "native_delegation_conflict"
    );

    let context: NativeContextResponse = fixture
        .ok(
            fixture.parent.id,
            NATIVE_DELEGATION_CONTEXT_OPERATION,
            serde_json::json!({}),
        )
        .await;
    assert_eq!(context.depth, 0);
    assert!(
        context
            .children
            .iter()
            .any(|child| child.session_id == child_id)
    );
    assert!(context.agents.iter().any(|agent| agent.default));
    drop(fixture);
}

#[tokio::test]
async fn only_the_delegating_parent_controls_delegated_children() {
    let fixture = Fixture::new().await;
    // A plain child of the parent is not a delegated child.
    let plain = crate::session_operations::create_child(
        &fixture.state,
        SessionChildCreateRequest {
            child_id: SessionId::new(),
            parent_id: fixture.parent.id,
            name: None,
            working_directory: std::env::current_dir().unwrap(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        fixture
            .failure_code(
                fixture.parent.id,
                NATIVE_DELEGATION_RESULT_OPERATION,
                serde_json::json!({"session_ids": [plain.id]}),
            )
            .await,
        "native_delegation_not_delegated_child"
    );

    let child_id = SessionId::new();
    let _: NativeAdmissionResponse = fixture
        .ok(
            fixture.parent.id,
            NATIVE_DELEGATION_DELEGATE_OPERATION,
            serde_json::to_value(delegate_request(child_id)).unwrap(),
        )
        .await;
    // Another session cannot observe or control it.
    let stranger = fixture
        .state
        .sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .failure_code(
                stranger.id,
                NATIVE_DELEGATION_RESULT_OPERATION,
                serde_json::json!({"session_ids": [child_id]}),
            )
            .await,
        "native_delegation_not_delegated_child"
    );

    // Interactions route to the parent only for delegated children.
    let routed = TurnExecutionOptions {
        interaction_route: Some(bcode_session_models::ExecutionInteractionRoute {
            version: 1,
            destination_session_id: fixture.parent.id,
        }),
        ..TurnExecutionOptions::default()
    };
    assert!(
        validate_interaction_route(&fixture.state, child_id, &routed)
            .await
            .is_ok()
    );
    assert!(
        validate_interaction_route(&fixture.state, plain.id, &routed)
            .await
            .is_err()
    );
    drop(fixture);
}

#[tokio::test]
async fn invocations_are_bound_to_the_executing_tool_call() {
    let fixture = Fixture::new().await;
    let resolution = native_delegation::invocation(
        &fixture.state,
        native_delegation::Caller {
            session_id: fixture.parent.id,
            plugin_id: PLUGIN,
            invocation_id: "call-1",
            cancel_state: &fixture.cancel_state,
        },
        bcode_tool::ToolInvocationServiceRequest {
            request_id: "other".into(),
            invocation_id: "other-call".into(),
            route_id: Some(NATIVE_DELEGATION_INTERFACE_ID.into()),
            interface_id: NATIVE_DELEGATION_INTERFACE_ID.into(),
            operation: NATIVE_DELEGATION_CONTEXT_OPERATION.into(),
            payload: serde_json::json!({}),
        },
    )
    .await;
    assert!(matches!(
        resolution,
        ToolInvocationServiceResolution::Cancelled
    ));
    drop(fixture);
}

#[tokio::test]
async fn clients_cannot_assert_a_delegation_intent() {
    let fixture = Fixture::new().await;
    let child_id = SessionId::new();
    let result = crate::session_spawning::spawn(
        &fixture.state,
        ClientId::new(),
        SessionSpawnRequest {
            version: 1,
            creation: SessionChildCreateRequest {
                child_id,
                parent_id: fixture.parent.id,
                name: None,
                working_directory: std::env::current_dir().unwrap(),
            },
            worktree_directory: None,
            input: "forged".into(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
            auth_profile: None,
            managed: None,
            delegation: Some(Box::new(
                bcode_session_models::delegation::SessionDelegationIntent {
                    version: 1,
                    producer_plugin_id: PLUGIN.into(),
                    invocation_id: "forged".into(),
                    delegating_turn_id: None,
                    permissions: SessionDelegationPermissions::Inherit,
                    cancel_with_turn: false,
                },
            )),
        },
    )
    .await;
    assert_eq!(
        result.failure.unwrap().code,
        "session_spawn_delegation_unauthorized"
    );
    assert!(
        fixture
            .state
            .sessions
            .session_summary_non_owning(child_id)
            .await
            .is_err()
    );
    drop(fixture);
}

#[tokio::test]
async fn configured_depth_limits_nested_delegation() {
    let fixture = Fixture::new().await;
    let nested = crate::session_operations::create_child(
        &fixture.state,
        SessionChildCreateRequest {
            child_id: SessionId::new(),
            parent_id: fixture.parent.id,
            name: None,
            working_directory: std::env::current_dir().unwrap(),
        },
    )
    .await
    .unwrap();
    let mut request = delegate_request(SessionId::new());
    request.limits.max_depth = Some(1);
    // The nested session is at depth one, so its child would be at depth two.
    assert_eq!(
        fixture
            .failure_code(
                nested.id,
                NATIVE_DELEGATION_DELEGATE_OPERATION,
                serde_json::to_value(&request).unwrap(),
            )
            .await,
        "native_delegation_depth_exceeded"
    );
    // Without a configured depth, nesting is unlimited.
    request.limits.max_depth = None;
    let _: NativeAdmissionResponse = fixture
        .ok(
            nested.id,
            NATIVE_DELEGATION_DELEGATE_OPERATION,
            serde_json::to_value(&request).unwrap(),
        )
        .await;
    drop(fixture);
}

#[tokio::test]
async fn inherited_bypass_narrows_with_the_parent() {
    let fixture = Fixture::new().await;
    let state = &fixture.state;
    let parent = fixture.parent.id;
    let child = SessionId::new();
    permission_mode::set(state, parent, TurnPermissionMode::Bypass).await;
    state
        .delegated_inherit_children
        .lock()
        .unwrap()
        .entry(parent)
        .or_default()
        .insert(child);
    permission_mode::set(state, child, TurnPermissionMode::Bypass).await;
    assert_eq!(
        permission_mode::current(state, child),
        TurnPermissionMode::Bypass
    );
    permission_mode::set(state, parent, TurnPermissionMode::Enforce).await;
    assert_eq!(
        permission_mode::current(state, child),
        TurnPermissionMode::Enforce
    );
    drop(fixture);
}

#[tokio::test]
async fn interrupting_a_delegating_turn_interrupts_only_its_cancel_with_turn_children() {
    let fixture = Fixture::new().await;
    let state = Arc::clone(&fixture.state);
    let child = SessionId::new();
    let (followup_commands, _followups) = mpsc::channel(1);
    let (steering_commands, _steering) = mpsc::channel(1);
    let (cancel_commands, mut cancels) = mpsc::channel(1);
    state.session_runtimes.lock().await.insert(
        child,
        SessionRuntimeHandle {
            followup_commands,
            steering_commands,
            cancel_commands,
            queued_followups: Arc::new(ActivityCounter::new(0)),
            queued_steering: Arc::new(ActivityCounter::new(0)),
            phase: Arc::new(ActivityMutex::new(SessionRuntimePhase::default())),
            current_turn: Arc::new(Mutex::new(Some(RuntimeCurrentTurn {
                interaction_route: None,
                kind: RuntimeOperationKind::ModelTurn,
                client_id: ClientId::new(),
                turn_id: "child-turn".into(),
                cancel_state: Arc::new(TurnCancelState::default()),
                model: None,
                agent_profile: None,
                execution: None,
                selection: None,
            }))),
        },
    );
    state
        .delegated_turn_dependents
        .lock()
        .unwrap()
        .entry((fixture.parent.id, "parent-turn".into()))
        .or_default()
        .insert(child);
    let receiver = tokio::spawn(async move {
        let command = cancels.recv().await.expect("child cancellation");
        assert_eq!(command.expected_turn_id.as_deref(), Some("child-turn"));
        assert!(command.clear_queue);
        let _ = command.response.send(true);
    });
    // A different turn of the parent leaves the child alone.
    native_delegation::cancel_turn_dependents(&state, fixture.parent.id, "other-turn").await;
    native_delegation::cancel_turn_dependents(&state, fixture.parent.id, "parent-turn").await;
    tokio::time::timeout(Duration::from_secs(5), receiver)
        .await
        .expect("child interrupted")
        .unwrap();
    drop(state);
    drop(fixture);
}
