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

#[tokio::test]
async fn memory_restart_is_atomic_fenced_and_idempotent() {
    use crate::CompactionCheckpointRestart;
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
    let request = CompactionCheckpointRestart {
        operation_key: "old".into(),
        expected_checkpoint_sequence: checkpoint.sequence,
        expected_prior_marker: None,
        source: vec![compaction_event_identity(&source).unwrap()],
        rejection: compaction_event_identity(&rejection).unwrap(),
        replacement_key: "fresh".into(),
        replacement_payload: "fresh allowance".into(),
        pointer_key: "discovery".into(),
        pointer_payload: "fresh".into(),
    };
    let original = manager.session_history(session.id).await.unwrap();
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
                .restart_compaction_checkpoint(session.id, stale)
                .await
                .is_err()
        );
        assert_eq!(manager.session_history(session.id).await.unwrap(), original);
    }
    let replacement = manager
        .restart_compaction_checkpoint(session.id, request.clone())
        .await
        .unwrap();
    let history = manager.session_history(session.id).await.unwrap();
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
            .restart_compaction_checkpoint(session.id, request.clone())
            .await
            .unwrap(),
        replacement
    );
    assert_eq!(manager.session_history(session.id).await.unwrap(), history);
    let mut conflict = request;
    conflict.replacement_payload = "more allowance".into();
    assert!(
        manager
            .restart_compaction_checkpoint(session.id, conflict)
            .await
            .is_err()
    );
    assert_eq!(manager.session_history(session.id).await.unwrap(), history);
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
