//! Owned guard fault injection; no production noop authority exists.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
pub(super) struct LimitedGuard {
    calls: AtomicUsize,
    successful_checks: usize,
}

impl crate::DurableInteractionCommitGuard for LimitedGuard {
    fn validate(&self) -> Result<(), SessionError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) < self.successful_checks {
            Ok(())
        } else {
            Err(SessionError::EventSerialization(
                "test execution authority revoked".into(),
            ))
        }
    }
}

pub(super) fn guard(successful_checks: usize) -> Arc<LimitedGuard> {
    Arc::new(LimitedGuard {
        calls: AtomicUsize::new(0),
        successful_checks,
    })
}

#[tokio::test]
async fn rejected_guard_never_acknowledges_or_changes_session() {
    let manager = SessionManager::default();
    let session = manager
        .create_session(None, std::env::temp_dir())
        .await
        .unwrap();
    let before = manager
        .current_session_generation(session.id)
        .await
        .unwrap();
    let authority = guard(0);
    assert!(
        manager
            .commit_durable_interaction(session.id, operation(), authority.clone())
            .await
            .is_err()
    );
    assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        manager
            .current_session_generation(session.id)
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn indexed_lifecycle_rejects_missing_invocation_wrong_request_and_finished_turn() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, operation) = prepare_terminal_fixture(root.path(), session).await;
    validate_current_lifecycle(db.database(), &operation)
        .await
        .unwrap();
    let mut mismatch = operation.clone();
    mismatch.request_event_sequence = 0;
    assert!(
        validate_current_lifecycle(db.database(), &mismatch)
            .await
            .is_err()
    );
    mismatch = operation.clone();
    mismatch.key.invocation_id = "missing".into();
    assert!(
        validate_current_lifecycle(db.database(), &mismatch)
            .await
            .is_err()
    );
    let mut finish = event(session, 4, operation.clone());
    finish.kind = SessionEventKind::ModelTurnFinished {
        turn_id: "turn".into(),
        outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
        message: None,
    };
    db.append_event(&finish).await.unwrap();
    assert!(
        validate_current_lifecycle(db.database(), &operation)
            .await
            .is_err()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn authority_loss_at_final_commit_check_rolls_back_events_and_projections() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let db = crate::db::SessionDb::open_turso_in_root(session, root.path())
        .await
        .unwrap();
    let mut created = event(session, 0, operation());
    created.kind = SessionEventKind::SessionCreated {
        name: None,
        working_directory: root.path().to_path_buf(),
    };
    db.append_event(&created).await.unwrap();
    let mut renamed = created.clone();
    renamed.sequence = 1;
    renamed.kind = SessionEventKind::SessionRenamed {
        name: Some("must roll back".into()),
    };
    // before begin, inside transaction, before mutation, after projection: all pass.
    // The fifth check immediately before COMMIT loses authority.
    let authority = guard(4);
    assert!(
        db.append_guarded_test_batch(&[(renamed, None)], authority.as_ref())
            .await
            .is_err()
    );
    assert_eq!(authority.calls.load(Ordering::SeqCst), 5);
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(0));
    assert_eq!(
        db.session_state().await.unwrap().unwrap().explicit_name,
        None
    );
    db.close().await.unwrap();
    let db = crate::db::SessionDb::open_turso_in_root(session, root.path())
        .await
        .unwrap();
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(0));
    assert_eq!(
        db.session_state().await.unwrap().unwrap().explicit_name,
        None
    );
    db.close().await.unwrap();
}
