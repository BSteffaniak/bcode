use crate::SessionManager;
use bcode_session_models::{
    ClientId, ModelTurnOutcome, SessionEventKind, SessionId, SessionMilestone,
    SessionMilestoneEvaluationRequest, SessionMilestoneProvenance,
    SessionMilestonePublicationRequest, SessionMilestoneQuery, TurnAdmissionMetadata, TurnId,
};

#[tokio::test]
async fn first_automatic_title_publication_and_identical_retry_succeed_without_explicit_name() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let id = manager
        .create_session(None, root.path().into())
        .await
        .unwrap()
        .id;
    let handle = manager.session_handle(id).await.unwrap();
    let ownership = handle
        .acquire_ownership(crate::actor::SessionOwnershipKind::RuntimeWork)
        .await
        .unwrap();
    let start = manager.current_session_generation(id).await.unwrap() + 1;
    let turn_id = TurnId::from_accepted_event(id, start).to_string();
    for kind in [
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "original fallback".into(),
            admission: TurnAdmissionMetadata::default(),
        },
        SessionEventKind::ModelTurnStarted {
            turn_id: turn_id.clone(),
        },
        SessionEventKind::AssistantResponseSegment {
            turn_id: turn_id.clone(),
            segment_id: "final".into(),
            segment_order: 0,
            text: "done".into(),
        },
        SessionEventKind::ModelTurnFinished {
            turn_id,
            outcome: ModelTurnOutcome::Completed,
            message: None,
        },
    ] {
        handle.append_event(kind, 100).await.unwrap();
    }
    let context = manager
        .session_milestone_context(id, 8192, 10)
        .await
        .unwrap();
    let source = context.source.unwrap();
    assert!(
        manager
            .request_session_milestone_evaluation(
                id,
                SessionMilestoneEvaluationRequest {
                    evaluation_id: "automatic".into(),
                    source: source.clone(),
                    expected_generation: context.generation,
                    expected_explicit_name: None,
                    max_requests_per_session: Some(10),
                }
            )
            .await
            .unwrap()
    );
    let request = SessionMilestonePublicationRequest {
        milestone: SessionMilestone {
            evaluation_id: "automatic".into(),
            source,
            header: "heading".into(),
            description: "description".into(),
            provenance: SessionMilestoneProvenance {
                provider_id: "offline".into(),
                model_id: "fixture".into(),
                input_truncated: false,
            },
        },
        automatic_title: Some("Generated title".into()),
    };
    manager
        .publish_session_milestone(id, request.clone())
        .await
        .unwrap();
    let generation = manager.current_session_generation(id).await.unwrap();
    manager
        .publish_session_milestone(id, request)
        .await
        .unwrap();
    assert_eq!(
        manager.current_session_generation(id).await.unwrap(),
        generation
    );
    let summary = handle.snapshot().summary;
    assert!(summary.explicit_name.is_none());
    assert_eq!(summary.automatic_title.as_deref(), Some("Generated title"));
    assert_eq!(summary.derived_title.as_deref(), Some("original fallback"));
    assert_eq!(summary.display_title(), "Generated title");
    drop(ownership);
    manager.release_persistent_idle_session_resources(id).await;
}

#[tokio::test]
async fn loaded_actor_reads_and_exact_publication_retry_preserve_explicit_name() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path().to_path_buf()).unwrap();
    let summary = manager
        .create_session(None, root.path().to_path_buf())
        .await
        .unwrap();
    let id = summary.id;
    let handle = manager.session_handle(id).await.unwrap();
    let ownership = handle
        .acquire_ownership(crate::actor::SessionOwnershipKind::RuntimeWork)
        .await
        .unwrap();
    append_settled_fixture_turn(&manager, id, &handle).await;
    let context = manager
        .session_milestone_context(id, 8192, 10)
        .await
        .unwrap();
    assert!(context.text.contains("cancelled"));
    let request = SessionMilestoneEvaluationRequest {
        evaluation_id: "actor".into(),
        source: context.source.unwrap(),
        expected_generation: context.generation,
        expected_explicit_name: None,
        max_requests_per_session: Some(10),
    };
    assert!(
        manager
            .request_session_milestone_evaluation(id, request.clone())
            .await
            .unwrap()
    );
    assert!(
        !manager
            .request_session_milestone_evaluation(id, request.clone())
            .await
            .unwrap()
    );
    let publication = SessionMilestonePublicationRequest {
        milestone: SessionMilestone {
            evaluation_id: "actor".into(),
            source: request.source,
            header: "heading".into(),
            description: "description".into(),
            provenance: SessionMilestoneProvenance {
                provider_id: "fixture".into(),
                model_id: "small".into(),
                input_truncated: false,
            },
        },
        automatic_title: Some("generated".into()),
    };
    manager
        .rename_session(id, Some("manual".into()))
        .await
        .unwrap();
    assert_publication_retry_and_conflict(&manager, id, publication).await;
    assert_completed_publication_queries(&manager, id).await;
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.summary.explicit_name.as_deref(), Some("manual"));
    assert_eq!(
        snapshot.summary.derived_title.as_deref(),
        Some("first prompt")
    );
    assert!(snapshot.summary.automatic_title.is_none());
    drop(ownership);
    manager.release_persistent_idle_session_resources(id).await;
}

async fn assert_publication_retry_and_conflict(
    manager: &SessionManager,
    id: SessionId,
    publication: SessionMilestonePublicationRequest,
) {
    manager
        .publish_session_milestone(id, publication.clone())
        .await
        .unwrap();
    manager
        .publish_session_milestone(id, publication.clone())
        .await
        .unwrap();
    let mut conflict = publication;
    conflict.automatic_title = Some("different".into());
    assert!(
        manager
            .publish_session_milestone(id, conflict)
            .await
            .is_err()
    );
}

async fn append_settled_fixture_turn(
    manager: &SessionManager,
    id: SessionId,
    handle: &crate::actor::SessionHandle,
) {
    let generation = manager.current_session_generation(id).await.unwrap();
    let start = generation + 1;
    let turn_id = TurnId::from_accepted_event(id, start).to_string();
    for kind in [
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "first prompt".into(),
            admission: TurnAdmissionMetadata::default(),
        },
        SessionEventKind::ModelTurnStarted {
            turn_id: turn_id.clone(),
        },
        SessionEventKind::AssistantResponseSegment {
            turn_id: turn_id.clone(),
            segment_id: "a".into(),
            segment_order: 0,
            text: "done".into(),
        },
        SessionEventKind::ModelTurnFinished {
            turn_id,
            outcome: ModelTurnOutcome::Cancelled,
            message: None,
        },
    ] {
        handle.append_event(kind, 100).await.unwrap();
    }
}

async fn assert_completed_publication_queries(manager: &SessionManager, id: SessionId) {
    assert_eq!(
        manager
            .session_milestones(
                id,
                SessionMilestoneQuery {
                    after_sequence: None,
                    limit: 1
                }
            )
            .await
            .unwrap()
            .milestones
            .len(),
        1
    );
    assert!(
        manager
            .session_milestone_evaluation(id, "actor")
            .await
            .unwrap()
            .unwrap()
            .completed
    );
    assert!(
        manager
            .session_milestone_evaluation_checkpoint(id)
            .await
            .unwrap()
            .unwrap()
            .completed
    );
}
