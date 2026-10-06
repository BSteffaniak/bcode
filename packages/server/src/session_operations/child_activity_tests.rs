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
    let sessions = bcode_session::SessionManager::default();
    let summary = sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let state = crate::tests::test_server_state(sessions);
    state
        .activity_observations
        .lock()
        .await
        .observe(&bcode_session_models::SessionEvent {
            session_id: summary.id,
            sequence: 1,
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            timestamp_ms: 0,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::RuntimeWorkFinished {
                work_id: bcode_session_models::WorkId::new("work"),
                status: bcode_session_models::RuntimeWorkStatus::Failed,
                finished_at_ms: None,
                message: None,
            },
        });
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
    let sessions = bcode_session::SessionManager::default();
    let summary = sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let state = crate::tests::test_server_state(sessions);
    state
        .activity_observations
        .lock()
        .await
        .observe(&bcode_session_models::SessionEvent {
            session_id: summary.id,
            sequence: 0,
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            timestamp_ms: 0,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::SessionCreated {
                name: None,
                working_directory: summary.working_directory.clone(),
            },
        });
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
