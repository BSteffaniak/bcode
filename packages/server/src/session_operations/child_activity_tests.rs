//! Behavioral tests for non-owning child activity observations.

use super::*;
use bcode_session_models::{
    SessionActivityCoverage, SessionActivityQuery, SessionActivityScope, SessionActivityState,
};

#[tokio::test]
async fn nonresident_session_is_unknown_without_installing_scheduler() {
    let sessions = bcode_session::SessionManager::default();
    let summary = sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let state = crate::tests::test_server_state(sessions);
    let page = activity(
        &state,
        summary.id,
        SessionActivityQuery {
            scope: SessionActivityScope::Own,
            after: None,
            limit: 1,
        },
    )
    .await
    .unwrap();
    let no_scheduler = state.session_runtimes.lock().await.is_empty();
    let no_work = state
        .runtime_work
        .activity_count_for_session(summary.id, 1)
        .await
        == (0, true);
    drop(state);
    assert_eq!(page.own.states, vec![SessionActivityState::Unknown]);
    assert_eq!(page.coverage, SessionActivityCoverage::Degraded);
    assert!(no_scheduler);
    assert!(no_work);
}

#[tokio::test]
async fn observed_failure_does_not_conceal_nonresident_unknown() {
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    let summary = state
        .sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    state
        .sessions
        .append_runtime_work_finished(
            summary.id,
            bcode_session_models::WorkId::new("work"),
            bcode_session_models::RuntimeWorkStatus::Failed,
            None,
            None,
        )
        .await
        .unwrap();
    let mut issues = Vec::new();
    let observed = observe_activity(&state, &summary, &mut issues).await;
    let no_scheduler = state.session_runtimes.lock().await.is_empty();
    drop(state);
    assert_eq!(
        observed.states,
        vec![SessionActivityState::Failed, SessionActivityState::Unknown]
    );
    assert!(!issues.is_empty());
    assert!(no_scheduler);
}

#[tokio::test]
async fn complete_empty_evidence_requires_resident_idle_scheduler() {
    use crate::{SessionRuntimeHandle, SessionRuntimePhase};
    use std::sync::{Arc, atomic::AtomicUsize};
    use tokio::sync::{Mutex, mpsc};
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    let summary = state
        .sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let followup_commands = mpsc::channel(1).0;
    let steering_commands = mpsc::channel(1).0;
    let cancel_commands = mpsc::channel(1).0;
    state.session_runtimes.lock().await.insert(
        summary.id,
        SessionRuntimeHandle {
            followup_commands,
            steering_commands,
            cancel_commands,
            queued_followups: Arc::new(AtomicUsize::new(0)),
            queued_steering: Arc::new(AtomicUsize::new(0)),
            phase: Arc::new(Mutex::new(SessionRuntimePhase::Idle)),
            current_turn: Arc::new(Mutex::new(None)),
        },
    );
    let mut issues = Vec::new();
    let observed = observe_activity(&state, &summary, &mut issues).await;
    drop(state);
    assert_eq!(observed.states, vec![SessionActivityState::Idle]);
    assert!(issues.is_empty());
}

async fn owned_terminal_fixture(
    root: &std::path::Path,
) -> (
    crate::ServerState,
    bcode_session_models::SessionId,
    bcode_session::SessionOwnershipGuard,
) {
    use crate::{SessionRuntimeHandle, SessionRuntimePhase};
    use std::sync::{Arc, atomic::AtomicUsize};
    use tokio::sync::{Mutex, mpsc};
    let sessions = bcode_session::SessionManager::persistent(root).unwrap();
    let state = crate::tests::test_server_state(sessions);
    let summary = state
        .sessions
        .create_session(None, root.to_path_buf())
        .await
        .unwrap();
    let id = summary.id;
    let ownership = state
        .sessions
        .acquire_session_ownership(id, bcode_session::SessionOwnershipKind::RuntimeWork)
        .await
        .unwrap();
    assert!(state.sessions.session_is_owned(id).await);
    let runtime = SessionRuntimeHandle {
        followup_commands: mpsc::channel(1).0,
        steering_commands: mpsc::channel(1).0,
        cancel_commands: mpsc::channel(1).0,
        queued_followups: Arc::new(AtomicUsize::new(0)),
        queued_steering: Arc::new(AtomicUsize::new(0)),
        phase: Arc::new(Mutex::new(SessionRuntimePhase::Idle)),
        current_turn: Arc::new(Mutex::new(None)),
    };
    state.session_runtimes.lock().await.insert(id, runtime);
    state
        .sessions
        .append_event(
            id,
            bcode_session_models::SessionEventKind::ModelTurnStarted {
                turn_id: "terminal-turn".into(),
            },
        )
        .await
        .unwrap();
    state
        .sessions
        .append_event(
            id,
            bcode_session_models::SessionEventKind::ModelTurnFinished {
                turn_id: "terminal-turn".into(),
                outcome: bcode_session_models::ModelTurnOutcome::Completed,
                message: None,
            },
        )
        .await
        .unwrap();
    state
        .sessions
        .append_runtime_work_finished(
            id,
            bcode_session_models::WorkId::new("terminal-work"),
            bcode_session_models::RuntimeWorkStatus::Completed,
            None,
            None,
        )
        .await
        .unwrap();
    state.session_runtimes.lock().await.remove(&id);
    (state, id, ownership)
}

#[tokio::test]
async fn terminal_scheduler_removal_preserves_evidence_until_explicit_ownership_release() {
    let root = tempfile::tempdir().unwrap();
    let (state, id, ownership) = owned_terminal_fixture(root.path()).await;
    let query = || SessionActivityQuery {
        scope: SessionActivityScope::Own,
        after: None,
        limit: 1,
    };
    let owned = state.sessions.session_is_owned(id).await;
    let work = state.runtime_work.activity_count_for_session(id, 128).await;
    let page = activity(&state, id, query()).await.unwrap();
    assert_eq!(
        page.coverage,
        SessionActivityCoverage::Complete,
        "owned={owned} work={work:?} states={:?} evidence={:?} issue_count={}",
        page.own.states,
        page.own.evidence,
        page.issues.len()
    );
    assert!(!page.own.states.contains(&SessionActivityState::Unknown));
    let evidence = page.own.evidence.unwrap();
    assert!(evidence.counts_complete);
    assert_eq!(evidence.observed_event_sequence, Some(3));
    assert_eq!(
        (
            evidence.running,
            evidence.queued,
            evidence.permission_waits,
            evidence.input_waits
        ),
        (0, 0, 0, 0)
    );
    drop(ownership);
    let outcome = crate::explicit_session_ownership_release_outcome(&state, id)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            bcode_ipc::SessionOwnershipReleaseOutcome::Released
                | bcode_ipc::SessionOwnershipReleaseOutcome::AlreadyUnowned
        ),
        "explicit ownership release remained blocked: {outcome:?}"
    );
    let observations = state.activity_observations.lock().await;
    assert!(!observations.evidence(id).counts_complete);
    assert_eq!(observations.evidence(id).observed_event_sequence, None);
    assert!(!observations.states(id).1);
    drop(observations);
    drop(state);
}

#[tokio::test]
async fn invalid_budget_does_not_install_runtime() {
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    for limit in [0, 129] {
        assert!(
            activity(
                &state,
                bcode_session_models::SessionId::new(),
                SessionActivityQuery {
                    scope: SessionActivityScope::DirectChildren,
                    after: None,
                    limit,
                }
            )
            .await
            .is_err()
        );
    }
    assert!(state.session_runtimes.lock().await.is_empty());
}
