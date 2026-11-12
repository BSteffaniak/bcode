//! Client-provided tools driven end to end: a real IPC provider connection, a fake-provider turn
//! that calls the tool, and the shared permission policy in between.

// Each test's fixture owns the daemon state and socket for the whole test on purpose.
#![allow(clippy::significant_drop_tightening)]

use std::sync::Arc;
use std::time::Duration;

use bcode_client::client_tools::{ClientToolProvider, ClientToolRequest};
use bcode_session::SessionManager;
use bcode_session_models::{
    SessionEventKind, SessionId, SessionOrigin, ToolInvocationResultRecord, TurnAdmissionMetadata,
    TurnExecutionOptions,
};
use bcode_tool::client_tool::{ClientToolDescriptor, ClientToolOutcome, ClientToolPolicy};

use crate::{ServerState, SubmittedModelTurn};

pub struct Fixture {
    pub state: Arc<ServerState>,
    pub client: bcode_client::BcodeClient,
    pub endpoint: bcode_ipc::IpcEndpoint,
    _server: tokio::task::JoinHandle<()>,
    pub workspace: tempfile::TempDir,
    _socket_dir: tempfile::TempDir,
}

/// Build the daemon state and serve it on a private socket. The accept loop ends with the
/// test's runtime.
pub fn fixture() -> Fixture {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = SessionManager::persistent(workspace.path().join("sessions")).expect("sessions");
    let provider = bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
        bcode_fake_provider_plugin::static_plugin(),
    );
    let filesystem = bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
        bcode_filesystem_plugin::static_plugin(),
    );
    let mut state = crate::tests::test_server_state(sessions);
    state.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: std::collections::BTreeSet::from([
                "bcode.fake-provider".to_owned(),
                "bcode.filesystem".to_owned(),
            ]),
            disabled: std::collections::BTreeSet::new(),
        },
        &[provider, filesystem],
    )
    .expect("load plugins");
    let state = Arc::new(state);
    let socket_dir = crate::tests::test_socket_dir().expect("socket dir");
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.path().join("server.sock"));
    let listener = crate::LocalIpcListener::bind(&endpoint).expect("listener");
    let server_state = Arc::clone(&state);
    let server = tokio::spawn(async move {
        while let Ok(stream) = listener.accept().await {
            let state = Arc::clone(&server_state);
            tokio::spawn(async move {
                let _ = crate::handle_client(stream, state).await;
            });
        }
    });
    Fixture {
        state,
        client: client(&endpoint),
        endpoint,
        _server: server,
        workspace,
        _socket_dir: socket_dir,
    }
}

/// A client that supplies a runtime context, as installed Bcode clients do.
pub fn client(endpoint: &bcode_ipc::IpcEndpoint) -> bcode_client::BcodeClient {
    bcode_client::BcodeClient::new(endpoint.clone())
        .with_runtime_context(Some(bcode_ipc::ClientRuntimeContext::default()))
}

fn tool(name: &str, policy: ClientToolPolicy) -> ClientToolDescriptor {
    ClientToolDescriptor {
        name: name.to_owned(),
        description: "Look a key up in the application's live state".to_owned(),
        input_schema: serde_json::json!({"type": "object", "properties": {"key": {"type": "string"}}}),
        policy,
        max_duration_ms: 5_000,
    }
}

async fn provider(fixture: &Fixture, policy: ClientToolPolicy) -> ClientToolProvider {
    fixture
        .client
        .client_tool_provider("app", vec![tool("app.lookup", policy)])
        .await
        .expect("register client tools")
}

async fn app_session(fixture: &Fixture, provider: &mut ClientToolProvider) -> SessionId {
    provider
        .create_session(
            Some("app session".to_owned()),
            fixture.workspace.path().to_path_buf(),
            Some(SessionOrigin::new("app")),
        )
        .await
        .expect("create app session")
        .id
}

/// Start a fake-provider turn that calls `app.lookup` once and then answers in text.
async fn start_lookup_turn(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> tokio::sync::oneshot::Receiver<crate::ModelTurnCompletion> {
    let submitted = crate::submit_session_model_turn_with_admission(
        state,
        session_id,
        r#"tool-call app.lookup {"key":"answer"}"#.to_owned(),
        None,
        TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                provider_plugin_id: Some("bcode.fake-provider".into()),
                model_id: Some("fake-echo".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    )
    .await
    .expect("admit turn");
    let SubmittedModelTurn::Started { completion, .. } = submitted else {
        panic!("expected a newly started turn");
    };
    completion
}

async fn finish(completion: tokio::sync::oneshot::Receiver<crate::ModelTurnCompletion>) {
    tokio::time::timeout(Duration::from_secs(20), completion)
        .await
        .expect("turn finishes")
        .expect("completion channel");
}

async fn tool_results(
    state: &ServerState,
    session_id: SessionId,
) -> Vec<ToolInvocationResultRecord> {
    state
        .sessions
        .session_events_range(session_id, 0, u64::MAX, 4096)
        .await
        .expect("history")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ToolInvocationResultRecorded { record } => Some(record),
            _ => None,
        })
        .collect()
}

async fn pending_permission_ids(state: &ServerState, expected: usize) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let pending = state.pending_permissions.lock().await;
            if pending.len() == expected {
                break pending.keys().cloned().collect();
            }
            drop(pending);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("permission prompt appears")
}

/// Answer the next invocation with the looked-up key and return the invocation.
async fn answer_next(
    provider: &mut ClientToolProvider,
) -> bcode_tool::client_tool::ClientToolInvocation {
    let request = tokio::time::timeout(Duration::from_secs(10), provider.next_request())
        .await
        .expect("invocation arrives")
        .expect("provider connection");
    let ClientToolRequest::Invoke(invocation) = request else {
        panic!("expected an invocation, got {request:?}");
    };
    provider
        .respond(
            invocation.invocation_id.clone(),
            ClientToolOutcome::Completed {
                content: "live value: 42".to_owned(),
            },
        )
        .await
        .expect("answer accepted");
    invocation
}

#[tokio::test]
async fn read_only_client_tool_runs_in_the_app_and_its_result_is_recorded() {
    let fixture = fixture();
    let mut provider = provider(&fixture, ClientToolPolicy::ReadOnly).await;
    let session_id = app_session(&fixture, &mut provider).await;
    let completion = start_lookup_turn(&fixture.state, session_id).await;

    let invocation = answer_next(&mut provider).await;
    assert_eq!(invocation.tool_name, "app.lookup");
    assert_eq!(invocation.session_id, session_id.to_string());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&invocation.arguments_json).unwrap(),
        serde_json::json!({"key": "answer"})
    );
    finish(completion).await;

    let results = tool_results(&fixture.state, session_id).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].is_error);
    assert_eq!(results[0].model_output, "live value: 42");
}

#[tokio::test]
async fn mutating_client_tool_asks_first_and_runs_only_when_approved() {
    for approved in [true, false] {
        let fixture = fixture();
        let mut provider = provider(&fixture, ClientToolPolicy::Mutating).await;
        let session_id = app_session(&fixture, &mut provider).await;
        let completion = start_lookup_turn(&fixture.state, session_id).await;

        let permissions = pending_permission_ids(&fixture.state, 1).await;
        let observer = client(&fixture.endpoint);
        let listed = observer
            .list_permissions()
            .await
            .expect("any client lists it");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].tool_name, "app.lookup");
        assert!(
            observer
                .resolve_permission(permissions[0].clone(), approved)
                .await
                .expect("any client answers it")
        );
        if approved {
            answer_next(&mut provider).await;
        }
        finish(completion).await;

        let results = tool_results(&fixture.state, session_id).await;
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].is_error, !approved, "{results:?}");
        if !approved {
            let nothing =
                tokio::time::timeout(Duration::from_millis(200), provider.next_request()).await;
            assert!(
                nothing.is_err(),
                "a denied call never reaches the app: {nothing:?}"
            );
        }
    }
}

#[tokio::test]
async fn unclaimed_sessions_never_offer_client_tools() {
    let fixture = fixture();
    let mut provider = provider(&fixture, ClientToolPolicy::ReadOnly).await;
    let session_id = fixture
        .client
        .create_session_in_working_directory(None, fixture.workspace.path().to_path_buf())
        .await
        .expect("ordinary session")
        .id;
    assert_eq!(fixture.state.client_tools.offered_tools(session_id), []);
    let completion = start_lookup_turn(&fixture.state, session_id).await;
    finish(completion).await;
    let nothing = tokio::time::timeout(Duration::from_millis(200), provider.next_request()).await;
    assert!(nothing.is_err(), "the app is never called: {nothing:?}");
    let results = tool_results(&fixture.state, session_id).await;
    assert!(results.iter().all(|result| result.is_error), "{results:?}");

    provider
        .claim_session(session_id)
        .await
        .expect("explicit claim");
    assert_eq!(
        fixture.state.client_tools.offered_tools(session_id).len(),
        1
    );
    provider.release_session(session_id).await.expect("release");
    assert_eq!(fixture.state.client_tools.offered_tools(session_id), []);
}

#[tokio::test]
async fn app_disconnect_during_a_call_fails_it_without_replay() {
    let fixture = fixture();
    let mut provider = provider(&fixture, ClientToolPolicy::ReadOnly).await;
    let session_id = app_session(&fixture, &mut provider).await;
    let completion = start_lookup_turn(&fixture.state, session_id).await;
    let request = tokio::time::timeout(Duration::from_secs(10), provider.next_request())
        .await
        .expect("invocation arrives")
        .expect("connection");
    assert!(matches!(request, ClientToolRequest::Invoke(_)));
    drop(provider);
    finish(completion).await;

    let results = tool_results(&fixture.state, session_id).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].is_error);
    assert!(
        results[0]
            .model_output
            .starts_with(bcode_tool::client_tool::CLIENT_TOOL_DISCONNECTED),
        "{results:?}"
    );
    assert_eq!(fixture.state.client_tools.offered_tools(session_id), []);
}

#[tokio::test]
async fn an_app_attaches_to_a_bcode_session_and_observes_its_own_tool_live() {
    let fixture = fixture();
    let session_id = fixture
        .client
        .create_session_in_working_directory(None, fixture.workspace.path().to_path_buf())
        .await
        .expect("session created by an ordinary client")
        .id;
    let mut provider = provider(&fixture, ClientToolPolicy::ReadOnly).await;
    provider.claim_session(session_id).await.expect("claim");
    let mut watcher = fixture
        .client
        .watch_session(session_id, 16)
        .await
        .expect("the app attaches to the session");
    let completion = start_lookup_turn(&fixture.state, session_id).await;
    answer_next(&mut provider).await;
    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let bcode_client::SessionWatchEvent::Durable(event) =
                watcher.next_event().await.expect("watch")
                && let SessionEventKind::ToolInvocationResultRecorded { record } = event.kind
            {
                break record;
            }
        }
    })
    .await
    .expect("the result is observed live");
    assert_eq!(observed.model_output, "live value: 42");
    finish(completion).await;
}

#[tokio::test]
async fn newest_claim_routes_and_the_previous_claimant_is_told() {
    let fixture = fixture();
    let mut first = provider(&fixture, ClientToolPolicy::ReadOnly).await;
    let mut second = provider(&fixture, ClientToolPolicy::ReadOnly).await;
    let session_id = app_session(&fixture, &mut first).await;
    second
        .claim_session(session_id)
        .await
        .expect("second claim");
    let lost = tokio::time::timeout(Duration::from_secs(5), first.next_request())
        .await
        .expect("claim loss notice")
        .expect("connection");
    assert_eq!(lost, ClientToolRequest::ClaimLost { session_id });

    let completion = start_lookup_turn(&fixture.state, session_id).await;
    answer_next(&mut second).await;
    finish(completion).await;
    let results = tool_results(&fixture.state, session_id).await;
    assert_eq!(results[0].model_output, "live value: 42");
}

#[tokio::test]
async fn registrations_outside_the_namespace_or_bounds_are_rejected_whole() {
    let fixture = fixture();
    let error = fixture
        .client
        .client_tool_provider(
            "app",
            vec![
                tool("app.lookup", ClientToolPolicy::ReadOnly),
                tool("other.lookup", ClientToolPolicy::ReadOnly),
            ],
        )
        .await
        .expect_err("namespace violation");
    assert!(
        matches!(&error, bcode_client::ClientError::Server { code, .. } if code == "client_tools_rejected"),
        "{error:?}"
    );
    let collision = fixture
        .client
        .client_tool_provider(
            "filesystem",
            vec![tool("filesystem.read", ClientToolPolicy::ReadOnly)],
        )
        .await
        .expect_err("plugin tool names are never shadowed");
    assert!(
        matches!(&collision, bcode_client::ClientError::Server { message, .. } if message.contains("filesystem.read")),
        "{collision:?}"
    );
}

#[tokio::test]
async fn a_split_provider_claims_and_answers_while_other_requests_are_in_flight() {
    let fixture = fixture();
    let provider = provider(&fixture, ClientToolPolicy::ReadOnly).await;
    let (handle, mut requests) = provider.into_concurrent();
    let session_id = handle
        .create_session(
            None,
            fixture.workspace.path().to_path_buf(),
            Some(SessionOrigin::new("app")),
        )
        .await
        .expect("create claimed session")
        .id;
    let other = fixture
        .client
        .create_session_in_working_directory(None, fixture.workspace.path().to_path_buf())
        .await
        .expect("ordinary session")
        .id;
    let completion = start_lookup_turn(&fixture.state, session_id).await;
    let request = tokio::time::timeout(Duration::from_secs(10), requests.recv())
        .await
        .expect("invocation arrives")
        .expect("connection open");
    let ClientToolRequest::Invoke(invocation) = request else {
        panic!("expected an invocation, got {request:?}");
    };
    // The invocation is still unanswered while the same connection claims another session.
    handle
        .claim_session(other)
        .await
        .expect("claim while a call runs");
    assert_eq!(fixture.state.client_tools.offered_tools(other).len(), 1);
    handle
        .respond(
            invocation.invocation_id,
            ClientToolOutcome::Completed {
                content: "live value: 42".to_owned(),
            },
        )
        .await
        .expect("answer");
    finish(completion).await;
    let results = tool_results(&fixture.state, session_id).await;
    assert_eq!(results[0].model_output, "live value: 42");

    drop(handle);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .expect("the stream ends")
            .is_none()
    );
}
