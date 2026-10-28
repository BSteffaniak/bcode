use crate::{
    CompactionCheckpointCommit, SessionEventKind, SessionManager, compaction_event_identity,
};
use bcode_session_models::{
    CompactionSynthesisCheckpoint, ProviderContextSnapshot, ProviderContextSnapshotOrigin,
};

#[tokio::test]
async fn memory_compaction_commit_validates_and_atomically_retires_native_and_local() {
    for native in [false, true] {
        let manager = SessionManager::default();
        let session = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let source_event = manager
            .append_event(
                session.id,
                SessionEventKind::SystemMessage {
                    text: "canonical source".into(),
                },
            )
            .await
            .unwrap();
        let checkpoint = manager
            .append_compaction_checkpoint(
                session.id,
                "operation",
                Some("accepted host synthesis".into()),
            )
            .await
            .unwrap();
        let marker = marker(native, source_event.sequence);
        let request = CompactionCheckpointCommit {
            operation_key: "operation".into(),
            expected_checkpoint_sequence: checkpoint.sequence,
            expected_prior_marker: None,
            source: vec![compaction_event_identity(&source_event).unwrap()],
            marker,
        };
        let before = manager.session_history(session.id).await.unwrap();
        let mut mismatched = request.clone();
        mismatched.source[0].sha256 = "changed".into();
        assert!(
            manager
                .commit_compaction_checkpoint(session.id, mismatched)
                .await
                .is_err()
        );
        assert_eq!(manager.session_history(session.id).await.unwrap(), before);
        let mut stale = request.clone();
        stale.expected_checkpoint_sequence += 1;
        assert!(
            manager
                .commit_compaction_checkpoint(session.id, stale)
                .await
                .is_err()
        );
        assert_eq!(manager.session_history(session.id).await.unwrap(), before);
        let committed = manager
            .commit_compaction_checkpoint(session.id, request.clone())
            .await
            .unwrap();
        assert_eq!(committed.sequence, checkpoint.sequence + 1);
        assert_eq!(committed.kind, request.marker);
        let history = manager.session_history(session.id).await.unwrap();
        assert_eq!(history.len(), before.len() + 2);
        assert_eq!(&history[..before.len()], &before);
        let retirement = history.last().unwrap();
        assert_eq!(retirement.sequence, committed.sequence + 1);
        let retired = CompactionSynthesisCheckpoint::from_event(retirement)
            .unwrap()
            .unwrap();
        assert!(retired.host_payload.is_none());
        assert_eq!(retired.operation_key, "operation");
        let context = manager.model_context_events(session.id).await.unwrap();
        assert!(context.iter().any(|event| event == &committed));
        assert!(
            !context
                .iter()
                .any(|event| event.sequence == source_event.sequence)
        );
        assert!(
            manager
                .commit_compaction_checkpoint(session.id, request)
                .await
                .is_err()
        );
        assert_eq!(manager.session_history(session.id).await.unwrap(), history);
    }
}

async fn memory_restart_fixture() -> (
    SessionManager,
    bcode_session_models::SessionId,
    bcode_session_models::SessionEvent,
    bcode_session_models::SessionEvent,
    crate::CompactionCheckpointRestart,
) {
    let manager = SessionManager::default();
    let session = manager
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let source = manager
        .append_event(
            session.id,
            SessionEventKind::SystemMessage {
                text: "source".into(),
            },
        )
        .await
        .unwrap();
    let checkpoint = manager
        .append_compaction_checkpoint(session.id, "old", Some("consumed=2".into()))
        .await
        .unwrap();
    // The host owns rejection semantics; the session primitive compares exact canonical bytes.
    let rejection = manager
        .append_event(
            session.id,
            SessionEventKind::SystemMessage {
                text: "complete rejection".into(),
            },
        )
        .await
        .unwrap();
    let request = crate::CompactionCheckpointRestart {
        operation_key: "old".into(),
        expected_checkpoint_sequence: checkpoint.sequence,
        expected_prior_marker: None,
        source: vec![compaction_event_identity(&source).unwrap()],
        rejection: compaction_event_identity(&rejection).unwrap(),
        expected_tail: None,
        replacement_key: "fresh".into(),
        replacement_payload: "fresh allowance".into(),
        pointer_key: "discovery".into(),
        pointer_payload: "fresh".into(),
    };
    (manager, session.id, source, rejection, request)
}

#[tokio::test]
async fn memory_restart_can_select_history_appended_after_old_checkpoint() {
    let (manager, id, _, old_rejection, mut request) = memory_restart_fixture().await;
    let added = manager
        .append_event(
            id,
            SessionEventKind::SystemMessage {
                text: "new history after the old checkpoint".into(),
            },
        )
        .await
        .unwrap();
    request.source.extend([
        compaction_event_identity(&old_rejection).unwrap(),
        compaction_event_identity(&added).unwrap(),
    ]);
    let rejection = manager
        .append_event(
            id,
            SessionEventKind::SystemMessage {
                text: "latest host-validated rejection".into(),
            },
        )
        .await
        .unwrap();
    request.rejection = compaction_event_identity(&rejection).unwrap();
    let before = manager.session_history(id).await.unwrap();
    for case in 0..5 {
        let mut invalid = request.clone();
        match case {
            0 => {
                invalid.source.remove(1);
            }
            1 => invalid.source[1].sha256 = "0".repeat(64),
            2 => invalid.source.push(invalid.rejection.clone()),
            3 => invalid.rejection.sequence = invalid.expected_checkpoint_sequence,
            _ => invalid.expected_checkpoint_sequence += 1,
        }
        assert!(
            manager
                .restart_compaction_checkpoint(id, invalid)
                .await
                .is_err()
        );
        assert_eq!(manager.session_history(id).await.unwrap(), before);
    }
    let replacement = manager
        .restart_compaction_checkpoint(id, request.clone())
        .await
        .unwrap();
    let after = manager.session_history(id).await.unwrap();
    assert_eq!(&after[..before.len()], &before);
    assert_eq!(after.len(), before.len() + 4);
    assert_eq!(
        manager
            .restart_compaction_checkpoint(id, request)
            .await
            .unwrap(),
        replacement
    );
    assert_eq!(manager.session_history(id).await.unwrap(), after);
}

#[tokio::test]
async fn memory_restart_is_atomic_fenced_and_idempotent() {
    let (manager, session_id, source, rejection, request) = memory_restart_fixture().await;
    let original = manager.session_history(session_id).await.unwrap();
    for field in 0..4 {
        let mut stale = request.clone();
        match field {
            0 => stale.rejection.sha256 = "0".repeat(64),
            1 => stale.source[0].sha256 = "0".repeat(64),
            2 => stale.expected_checkpoint_sequence = rejection.sequence,
            _ => {
                stale.expected_prior_marker = Some(crate::CompactionEventIdentity {
                    sequence: source.sequence,
                    sha256: "0".repeat(64),
                });
            }
        }
        assert!(
            manager
                .restart_compaction_checkpoint(session_id, stale)
                .await
                .is_err()
        );
        assert_eq!(manager.session_history(session_id).await.unwrap(), original);
    }
    let terminal = manager
        .append_event(
            session_id,
            SessionEventKind::ModelTurnFinished {
                turn_id: "owning-turn".into(),
                outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
                message: None,
            },
        )
        .await
        .unwrap();
    // Legacy requests remain exactly rejection-fenced; they cannot silently adopt the new tail.
    assert!(
        manager
            .restart_compaction_checkpoint(session_id, request.clone())
            .await
            .is_err()
    );
    let mut request = request;
    request.expected_tail = Some(compaction_event_identity(&terminal).unwrap());
    let mut wrong_digest = request.clone();
    wrong_digest.expected_tail.as_mut().unwrap().sha256 = "0".repeat(64);
    assert!(
        manager
            .restart_compaction_checkpoint(session_id, wrong_digest)
            .await
            .is_err()
    );
    let original = manager.session_history(session_id).await.unwrap();
    let replacement = manager
        .restart_compaction_checkpoint(session_id, request.clone())
        .await
        .unwrap();
    let history = manager.session_history(session_id).await.unwrap();
    assert_eq!(&history[..original.len()], &original);
    assert_eq!(history.len(), original.len() + 4);
    assert_eq!(history.last().unwrap().sequence, replacement.sequence + 1);
    assert!(
        CompactionSynthesisCheckpoint::from_event(&history[original.len() + 1])
            .unwrap()
            .unwrap()
            .host_payload
            .is_none()
    );
    assert_eq!(
        manager
            .restart_compaction_checkpoint(session_id, request.clone())
            .await
            .unwrap(),
        replacement
    );
    assert_eq!(manager.session_history(session_id).await.unwrap(), history);
    let mut conflict = request;
    conflict.replacement_payload = "more allowance".into();
    assert!(
        manager
            .restart_compaction_checkpoint(session_id, conflict)
            .await
            .is_err()
    );
    assert_eq!(manager.session_history(session_id).await.unwrap(), history);
}

fn marker(native: bool, boundary: u64) -> SessionEventKind {
    if native {
        SessionEventKind::ProviderContextCompacted {
            snapshot: ProviderContextSnapshot {
                format_version: 1,
                request_fingerprint: None,
                request_id: None,
                provider_plugin_id: "provider".into(),
                model_id: "model".into(),
                compatibility_key: "surface".into(),
                auth_profile: None,
                origin: ProviderContextSnapshotOrigin::Explicit,
                messages_json: r#"[{"opaque":"ciphertext"}]"#.into(),
                portable_summary: "portable fallback".into(),
            },
            compacted_through_sequence: boundary,
        }
    } else {
        SessionEventKind::ContextCompacted {
            summary: "portable fallback".into(),
            compacted_through_sequence: boundary,
        }
    }
}
