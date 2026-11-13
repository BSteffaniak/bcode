//! Agent profiles a connected application contributes: they narrow a base agent, never grant,
//! and fail closed once the application is gone.

// Each test's fixture owns the daemon state and socket for the whole test on purpose.
#![allow(clippy::significant_drop_tightening)]

use std::time::Duration;

use bcode_agent_profile::client_profile::ClientAgentProfile;
use bcode_agent_profile::{AgentDecision, ToolPolicyAuthorizationMetadata, ToolPolicyOperation};
use bcode_client::client_tools::{ClientToolProvider, ClientToolRequest};
use bcode_session_models::{
    SessionEventKind, SessionId, SessionOrigin, TurnAdmissionMetadata, TurnExecutionOptions,
};
use bcode_tool::client_tool::{ClientToolDescriptor, ClientToolOutcome, ClientToolPolicy};

use crate::client_tools_tests::{Fixture, fixture};

const PROFILE: &str = "app.reader";

fn profile() -> ClientAgentProfile {
    ClientAgentProfile {
        id: PROFILE.to_owned(),
        name: "Reader".to_owned(),
        description: "Looks things up in the app".to_owned(),
        base_agent: "build".to_owned(),
        instructions: "Answer from the app's live state.".to_owned(),
        tools: vec!["app.lookup".to_owned(), "app.change".to_owned()],
    }
}

fn tool(name: &str, policy: ClientToolPolicy) -> ClientToolDescriptor {
    ClientToolDescriptor {
        name: name.to_owned(),
        description: "Use the application's live state".to_owned(),
        input_schema: serde_json::json!({"type": "object"}),
        policy,
        max_duration_ms: 5_000,
    }
}

async fn provider_with_profile(fixture: &Fixture) -> (ClientToolProvider, SessionId) {
    let mut provider = fixture
        .client
        .client_tool_provider(
            "app",
            vec![
                tool("app.lookup", ClientToolPolicy::ReadOnly),
                tool("app.change", ClientToolPolicy::Mutating),
            ],
        )
        .await
        .expect("register tools");
    provider
        .register_agent_profiles(vec![profile()])
        .await
        .expect("register profile");
    let session_id = provider
        .create_session(
            None,
            fixture.workspace.path().to_path_buf(),
            Some(SessionOrigin::new("app")),
        )
        .await
        .expect("create session")
        .id;
    (provider, session_id)
}

fn call(name: &str) -> bcode_model::ToolCall {
    bcode_model::ToolCall {
        id: "call-1".to_owned(),
        name: name.to_owned(),
        arguments: serde_json::json!({}),
    }
}

const fn metadata(
    operation: ToolPolicyOperation,
    requires_permission: bool,
) -> ToolPolicyAuthorizationMetadata {
    ToolPolicyAuthorizationMetadata {
        requires_permission,
        aliases: Vec::new(),
        compatibility_aliases: Vec::new(),
        capabilities: Vec::new(),
        permission_category: None,
        operation,
    }
}

#[tokio::test]
async fn a_profile_is_listed_only_while_its_application_is_connected() {
    let fixture = fixture();
    let (provider, _session) = provider_with_profile(&fixture).await;
    let agents = fixture.client.list_agents().await.expect("agents");
    let listed = agents
        .iter()
        .find(|agent| agent.id == PROFILE)
        .expect("listed");
    assert_eq!(listed.name, "Reader");
    assert!(!listed.is_default);

    drop(provider);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.state.client_tools.agent_profile(PROFILE).is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the profile goes away with its connection");
    let agents = fixture.client.list_agents().await.expect("agents");
    assert!(agents.iter().all(|agent| agent.id != PROFILE));
}

#[tokio::test]
async fn a_profile_narrows_its_base_agent_and_adds_its_instructions() {
    let fixture = fixture();
    let (_provider, session_id) = provider_with_profile(&fixture).await;
    let available = crate::collect_tool_definitions(&fixture.state).await;
    assert_ne!(available.len(), 0);
    let context = crate::agent_context_with_tools(&fixture.state, session_id, PROFILE, available)
        .await
        .expect("context");
    let enabled = context.enabled_tools.expect("exact tool list");
    assert!(
        enabled.iter().all(|tool| profile().tools.contains(tool)),
        "{enabled:?}"
    );
    assert!(enabled.contains(&"app.lookup".to_owned()), "{enabled:?}");
    assert!(
        context
            .system_prompt_suffix
            .is_some_and(|suffix| suffix.ends_with("Answer from the app's live state.")),
    );
}

#[tokio::test]
async fn a_profile_never_grants_what_its_base_agent_would_ask_or_deny() {
    let fixture = fixture();
    let (_provider, session_id) = provider_with_profile(&fixture).await;
    let evaluate = |name: &'static str, operation, requires| {
        let state = std::sync::Arc::clone(&fixture.state);
        async move {
            crate::evaluate_agent_tool_policy_with_metadata(
                &state,
                session_id,
                PROFILE,
                &call(name),
                &metadata(operation, requires),
                None,
            )
            .await
            .decision
        }
    };
    // Outside the profile's selection: denied, even though the base agent would allow it.
    assert_eq!(
        evaluate(
            "read",
            ToolPolicyOperation::Read {
                paths: vec![".".into()]
            },
            false
        )
        .await,
        AgentDecision::Deny
    );
    // Inside the selection: the base agent decides; a mutating call still asks.
    assert_eq!(
        evaluate("app.lookup", ToolPolicyOperation::ReadOnly, false).await,
        AgentDecision::Allow
    );
    assert_eq!(
        evaluate("app.change", ToolPolicyOperation::Mutating, true).await,
        AgentDecision::Ask
    );
}

#[tokio::test]
async fn turns_run_under_a_connected_profile_and_fail_closed_without_it() {
    let fixture = fixture();
    let (mut provider, session_id) = provider_with_profile(&fixture).await;
    let admission = TurnAdmissionMetadata {
        execution: TurnExecutionOptions {
            provider_plugin_id: Some("bcode.fake-provider".into()),
            model_id: Some("fake-echo".into()),
            agent_profile: Some(PROFILE.into()),
            ..Default::default()
        },
        ..Default::default()
    };
    fixture
        .client
        .submit_turn(
            session_id,
            r#"tool-call app.lookup {"key":"answer"}"#.to_owned(),
            admission.clone(),
        )
        .await
        .expect("admitted while the app is connected");
    let request = tokio::time::timeout(Duration::from_secs(10), provider.next_request())
        .await
        .expect("invocation")
        .expect("connection");
    let ClientToolRequest::Invoke(invocation) = request else {
        panic!("expected an invocation, got {request:?}");
    };
    provider
        .respond(
            invocation.invocation_id,
            ClientToolOutcome::Completed {
                content: "42".into(),
            },
        )
        .await
        .expect("answer");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let finished = fixture
                .state
                .sessions
                .session_events_range(session_id, 0, u64::MAX, 4096)
                .await
                .expect("history")
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::ModelTurnFinished { .. }));
            if finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the turn finishes");

    drop(provider);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.state.client_tools.agent_profile(PROFILE).is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the profile goes away");
    let refused = fixture
        .client
        .submit_turn(session_id, "hello".to_owned(), admission)
        .await;
    assert!(
        matches!(
            &refused,
            Err(bcode_client::ClientError::Server { code, .. }) if code == "agent_profile_unavailable"
        ),
        "{refused:?}"
    );
}

#[tokio::test]
async fn profiles_outside_the_providers_namespace_are_rejected_whole() {
    let fixture = fixture();
    let mut provider = fixture
        .client
        .client_tool_provider("app", vec![tool("app.lookup", ClientToolPolicy::ReadOnly)])
        .await
        .expect("register tools");
    let mut foreign = profile();
    foreign.id = "other.reader".into();
    let rejected = provider
        .register_agent_profiles(vec![profile(), foreign])
        .await;
    assert!(
        matches!(
            &rejected,
            Err(bcode_client::ClientError::Server { code, .. })
                if code == "client_agent_profiles_rejected"
        ),
        "{rejected:?}"
    );
    assert!(fixture.state.client_tools.agent_profile(PROFILE).is_none());
    assert!(
        fixture
            .state
            .client_tools
            .agent_profile("other.reader")
            .is_none()
    );
}
