//! A session's runtime permission mode driven end to end: a real IPC client steps the session
//! into and out of bypass while a fake-provider turn calls a mutating client tool.

// Each test's fixture owns the daemon state and socket for the whole test on purpose.
#![allow(clippy::significant_drop_tightening)]

use std::sync::Arc;
use std::time::Duration;

use bcode_session::SessionManager;
use bcode_session_models::{
    SessionId, TurnAdmissionMetadata, TurnExecutionOptions, TurnPermissionMode,
};
use bcode_tool::client_tool::ClientToolPolicy;

use crate::client_tools_tests::{
    answer_next, app_session, client, finish, fixture, fixture_with_options,
    pending_permission_ids, provider, tool_results,
};
use crate::{ServerState, SubmittedModelTurn, TurnCancelState};

/// Start a fake-provider turn with default execution options that calls `app.lookup` `calls`
/// times in a row and then answers in text.
async fn start_turn(
    state: &Arc<ServerState>,
    session_id: SessionId,
    calls: usize,
) -> tokio::sync::oneshot::Receiver<crate::ModelTurnCompletion> {
    let prompt = vec![r#"tool-call app.lookup {"key":"answer"}"#; calls].join("\n");
    let submitted = crate::submit_session_model_turn_with_admission(
        state,
        session_id,
        prompt,
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

async fn no_prompt_waits(state: &ServerState) -> bool {
    tokio::time::sleep(Duration::from_millis(200)).await;
    state.pending_permissions.lock().await.is_empty()
}

#[tokio::test]
async fn a_session_stepped_into_bypass_runs_asking_tools_without_a_prompt() {
    let fixture = fixture();
    let mut provider = provider(&fixture, ClientToolPolicy::Mutating).await;
    let session_id = app_session(&fixture, &mut provider).await;
    let observer = client(&fixture.endpoint);
    assert_eq!(
        observer.session_permission_mode(session_id).await.unwrap(),
        TurnPermissionMode::Enforce
    );
    assert_eq!(
        observer
            .set_session_permission_mode(session_id, TurnPermissionMode::Bypass)
            .await
            .unwrap(),
        TurnPermissionMode::Bypass
    );
    assert_eq!(
        fixture
            .client
            .session_permission_mode(session_id)
            .await
            .unwrap(),
        TurnPermissionMode::Bypass,
        "every client reads the session's mode"
    );

    let completion = start_turn(&fixture.state, session_id, 1).await;
    answer_next(&mut provider).await;
    finish(completion).await;
    let results = tool_results(&fixture.state, session_id).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].is_error, "{results:?}");

    // Stepped out, the next turn asks again.
    observer
        .set_session_permission_mode(session_id, TurnPermissionMode::Enforce)
        .await
        .unwrap();
    let completion = start_turn(&fixture.state, session_id, 1).await;
    let permissions = pending_permission_ids(&fixture.state, 1).await;
    assert!(
        observer
            .resolve_permission(permissions[0].clone(), true)
            .await
            .unwrap()
    );
    answer_next(&mut provider).await;
    finish(completion).await;
}

#[tokio::test]
async fn stepping_out_mid_turn_asks_at_the_running_turns_next_tool_call() {
    let fixture = fixture_with_options(false, "", true);
    let mut provider = provider(&fixture, ClientToolPolicy::Mutating).await;
    let session_id = app_session(&fixture, &mut provider).await;
    let observer = client(&fixture.endpoint);
    observer
        .set_session_permission_mode(session_id, TurnPermissionMode::Bypass)
        .await
        .unwrap();
    let completion = start_turn(&fixture.state, session_id, 2).await;

    // The first call ran without asking; step out before answering it.
    let first = tokio::time::timeout(Duration::from_secs(10), provider.next_request()).await;
    let Ok(Ok(bcode_client::client_tools::ClientToolRequest::Invoke(first))) = first else {
        panic!("the first call reaches the app: {first:?}");
    };
    observer
        .set_session_permission_mode(session_id, TurnPermissionMode::Enforce)
        .await
        .unwrap();
    provider
        .respond(
            first.invocation_id,
            bcode_tool::client_tool::ClientToolOutcome::Completed {
                content: "live value: 42".to_owned(),
            },
        )
        .await
        .unwrap();

    let permissions = pending_permission_ids(&fixture.state, 1).await;
    assert!(
        observer
            .resolve_permission(permissions[0].clone(), true)
            .await
            .unwrap()
    );
    answer_next(&mut provider).await;
    finish(completion).await;
    assert_eq!(tool_results(&fixture.state, session_id).await.len(), 2);
}

#[tokio::test]
async fn stepping_out_can_wait_for_the_next_turn() {
    let fixture = fixture_with_options(
        false,
        "[permissions]\nbypass_step_out = \"next_turn\"\n",
        true,
    );
    let mut provider = provider(&fixture, ClientToolPolicy::Mutating).await;
    let session_id = app_session(&fixture, &mut provider).await;
    let observer = client(&fixture.endpoint);
    observer
        .set_session_permission_mode(session_id, TurnPermissionMode::Bypass)
        .await
        .unwrap();
    let completion = start_turn(&fixture.state, session_id, 2).await;
    let first = tokio::time::timeout(Duration::from_secs(10), provider.next_request()).await;
    let Ok(Ok(bcode_client::client_tools::ClientToolRequest::Invoke(first))) = first else {
        panic!("the first call reaches the app: {first:?}");
    };
    observer
        .set_session_permission_mode(session_id, TurnPermissionMode::Enforce)
        .await
        .unwrap();
    provider
        .respond(
            first.invocation_id,
            bcode_tool::client_tool::ClientToolOutcome::Completed {
                content: "live value: 42".to_owned(),
            },
        )
        .await
        .unwrap();
    // The running turn keeps its bypass.
    answer_next(&mut provider).await;
    finish(completion).await;
    assert!(no_prompt_waits(&fixture.state).await);
    assert_eq!(tool_results(&fixture.state, session_id).await.len(), 2);
}

#[tokio::test]
async fn admission_follows_the_session_and_a_restart_revokes_bypass() {
    let state = crate::tests::test_server_state(SessionManager::default());
    let session = state
        .sessions
        .create_session(None, std::env::temp_dir())
        .await
        .expect("session");
    let id = session.id;

    // Nothing set: turns are admitted as they ask; a turn admitted in bypass keeps it, unless
    // it resumes after a daemon restart.
    let mut execution = TurnExecutionOptions::default();
    crate::permission_mode::stamp(&state, id, &mut execution);
    assert_eq!(execution.permission_mode, TurnPermissionMode::Enforce);
    assert!(!crate::permission_mode::starts_revoked(&state, id, false));
    assert!(crate::permission_mode::starts_revoked(&state, id, true));

    crate::permission_mode::set(&state, id, TurnPermissionMode::Bypass).await;
    crate::permission_mode::stamp(&state, id, &mut execution);
    assert_eq!(execution.permission_mode, TurnPermissionMode::Bypass);
    assert!(!crate::permission_mode::starts_revoked(&state, id, true));
    // Options too old to carry a permission mode are left alone.
    let mut old = TurnExecutionOptions {
        schema_version: 3,
        ..TurnExecutionOptions::default()
    };
    crate::permission_mode::stamp(&state, id, &mut old);
    assert_eq!(old.permission_mode, TurnPermissionMode::Enforce);

    // Stepped out: a queued bypass turn starts revoked and enforcing.
    crate::permission_mode::set(&state, id, TurnPermissionMode::Enforce).await;
    let cancel = TurnCancelState::default();
    let mut queued = TurnExecutionOptions {
        permission_mode: TurnPermissionMode::Bypass,
        ..TurnExecutionOptions::default()
    };
    crate::permission_mode::start_turn(&state, id, false, &cancel, &mut queued);
    assert!(cancel.bypass_revoked());
    assert_eq!(queued.permission_mode, TurnPermissionMode::Enforce);

    crate::permission_mode::forget(&state, id);
    assert_eq!(
        crate::permission_mode::current(&state, id),
        TurnPermissionMode::Enforce
    );
}

#[tokio::test]
async fn an_unknown_session_has_no_permission_mode() {
    let fixture = fixture();
    let unknown = SessionId::new();
    assert!(
        fixture
            .client
            .session_permission_mode(unknown)
            .await
            .is_err()
    );
    assert!(
        fixture
            .client
            .set_session_permission_mode(unknown, TurnPermissionMode::Bypass)
            .await
            .is_err()
    );
    assert_eq!(
        crate::permission_mode::current(&fixture.state, unknown),
        TurnPermissionMode::Enforce
    );
}
