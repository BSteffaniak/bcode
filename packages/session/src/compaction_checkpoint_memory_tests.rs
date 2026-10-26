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
