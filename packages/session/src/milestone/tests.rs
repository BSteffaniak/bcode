use super::{SessionDb, validate_source};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ModelTurnOutcome, SessionEvent,
    SessionEventKind, SessionId, SessionMilestone, SessionMilestoneEvaluationCompletion,
    SessionMilestoneEvaluationRequest, SessionMilestoneProvenance,
    SessionMilestonePublicationRequest, SessionMilestoneQuery, SessionMilestoneSource,
    TurnAdmissionMetadata, TurnId, TurnPriority, estimated_normalized_text_tokens,
};

#[tokio::test]
async fn producer_and_execution_correlated_turns_do_not_pollute_milestones() {
    use bcode_session_models::{TurnExecutionCorrelation, TurnOrigin};

    let (_root, db, mut sequence) = initialized_session().await;
    let admissions = [
        TurnAdmissionMetadata {
            priority: TurnPriority::Background,
            ..TurnAdmissionMetadata::default()
        },
        TurnAdmissionMetadata {
            execution: bcode_session_models::TurnExecutionOptions {
                correlation: Some(TurnExecutionCorrelation {
                    execution_id: "execution".into(),
                    unit_id: "unit".into(),
                    attempt: 1,
                }),
                ..bcode_session_models::TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        },
        TurnAdmissionMetadata {
            origin: Some(TurnOrigin {
                producer: "arbitrary-producer".into(),
                correlation_id: None,
                display_label: None,
            }),
            ..TurnAdmissionMetadata::default()
        },
    ];
    for activated in [false, true] {
        for admission in &admissions {
            append_excluded_turn(&db, &mut sequence, admission.clone(), activated).await;
        }
    }
    let empty = db.milestone_context(8192, 10).await.unwrap();
    assert!(empty.source.is_none());
    assert!(empty.text.is_empty());
    assert_eq!(empty.new_turns, 0);
    assert_eq!(empty.new_tokens, 0);

    let start = sequence;
    let foreground = TurnId::from_accepted_event(db.session_id(), start).to_string();
    append(
        &db,
        &mut sequence,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "foreground prompt".into(),
            admission: TurnAdmissionMetadata::default(),
        },
    )
    .await;
    for activated in [false, true] {
        for admission in &admissions {
            append_excluded_turn(&db, &mut sequence, admission.clone(), activated).await;
        }
    }
    let end = sequence;
    append(
        &db,
        &mut sequence,
        SessionEventKind::ModelTurnFinished {
            turn_id: foreground,
            outcome: ModelTurnOutcome::Completed,
            message: None,
        },
    )
    .await;
    for activated in [false, true] {
        for admission in &admissions {
            append_excluded_turn(&db, &mut sequence, admission.clone(), activated).await;
        }
    }
    let context = db.milestone_context(8192, 10).await.unwrap();
    assert_eq!(
        context.source,
        Some(SessionMilestoneSource {
            start_sequence: start,
            end_sequence: end,
            anchor_sequence: start,
        })
    );
    assert_eq!(context.new_turns, 1);
    assert_eq!(
        context.new_tokens,
        estimated_normalized_text_tokens("foreground prompt")
    );
    assert!(context.text.contains("foreground prompt"));
    assert!(!context.text.contains("excluded"));
    db.close().await.unwrap();
}

async fn append_excluded_turn(
    db: &SessionDb,
    sequence: &mut u64,
    admission: TurnAdmissionMetadata,
    activated: bool,
) {
    let turn_id = TurnId::from_accepted_event(db.session_id(), *sequence);
    let client_id = ClientId::new();
    let kind = if activated {
        let submission_id = bcode_session_models::SubmissionId::for_queued_turn(&turn_id);
        append(
            db,
            sequence,
            SessionEventKind::SubmissionAccepted {
                submission: bcode_session_models::PromptSubmission {
                    id: submission_id.clone(),
                    client_id,
                    text: "excluded prompt".into(),
                    admission: admission.clone(),
                    requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
                    target: bcode_session_models::SubmissionTarget::OwnTurn {
                        turn_id: turn_id.clone(),
                    },
                    execution_descriptor: None,
                },
            },
        )
        .await;
        SessionEventKind::SubmissionActivated {
            submission_id,
            turn_id: turn_id.clone(),
            client_id,
            text: "excluded prompt".into(),
            admission,
        }
    } else {
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "excluded prompt".into(),
            admission,
        }
    };
    let start = *sequence;
    append(db, sequence, kind).await;
    append(
        db,
        sequence,
        SessionEventKind::AssistantResponseSegment {
            turn_id: turn_id.to_string(),
            segment_id: "excluded".into(),
            segment_order: 0,
            text: "excluded output".into(),
        },
    )
    .await;
    let end = *sequence;
    append(
        db,
        sequence,
        SessionEventKind::ModelTurnFinished {
            turn_id: turn_id.to_string(),
            outcome: ModelTurnOutcome::Completed,
            message: None,
        },
    )
    .await;
    assert!(
        validate_source(
            db.database(),
            &SessionMilestoneSource {
                start_sequence: start,
                end_sequence: end,
                anchor_sequence: start,
            },
            db.session_id()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn canonical_execution_classification_excludes_interactive_uncorrelated_turns() {
    use bcode_session_models::{
        ExecutionSessionContextMode, ExecutionSessionProvenance, SessionVisibility,
    };
    for visibility in [SessionVisibility::Visible, SessionVisibility::Background] {
        let (_root, db, mut sequence) = initialized_session().await;
        let parent_id = SessionId::new();
        append(
            &db,
            &mut sequence,
            SessionEventKind::ChildSessionCreated {
                request: Box::new(bcode_session_models::SessionChildCreateRequest {
                    child_id: db.session_id(),
                    parent_id,
                    name: None,
                    working_directory: db.session_state().await.unwrap().unwrap().working_directory,
                }),
            },
        )
        .await;
        append(
            &db,
            &mut sequence,
            SessionEventKind::ExecutionSessionCreated {
                provenance: Box::new(ExecutionSessionProvenance {
                    version: bcode_session_models::EXECUTION_SESSION_PROVENANCE_VERSION,
                    owner: "generic-execution-owner".into(),
                    run_id: "run".into(),
                    node_id: "unit".into(),
                    activation_id: Some("activation".into()),
                    attempt: 1,
                    parent_session_id: parent_id,
                    context_mode: ExecutionSessionContextMode::FreshIsolated,
                    workspace_snapshot: None,
                    parent_generation: None,
                }),
                visibility,
            },
        )
        .await;
        let source = turn(&db, &mut sequence, false).await;
        assert!(db.milestone_context(8192, 10).await.is_err());
        assert!(
            validate_source(db.database(), &source, db.session_id())
                .await
                .is_err()
        );
        let scheduling = super::scheduling_state(db.database()).await.unwrap();
        assert_eq!(scheduling.turns, 0);
        assert_eq!(scheduling.text_tokens, 0);
        let request = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id: db.session_id(),
            sequence,
            timestamp_ms: 1000 + sequence,
            provenance: None,
            kind: SessionEventKind::MilestoneEvaluationRequested {
                request: SessionMilestoneEvaluationRequest {
                    evaluation_id: "excluded-execution".into(),
                    source,
                    expected_generation: sequence - 1,
                    expected_explicit_name: None,
                    max_requests_per_session: None,
                },
            },
        };
        assert!(db.append_event(&request).await.is_err());
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_top_level_and_user_child_conversations_remain_eligible() {
    for child in [false, true] {
        let (_root, db, mut sequence) = initialized_session().await;
        if child {
            append(
                &db,
                &mut sequence,
                SessionEventKind::ChildSessionCreated {
                    request: Box::new(bcode_session_models::SessionChildCreateRequest {
                        child_id: db.session_id(),
                        parent_id: SessionId::new(),
                        name: None,
                        working_directory: db
                            .session_state()
                            .await
                            .unwrap()
                            .unwrap()
                            .working_directory,
                    }),
                },
            )
            .await;
        }
        let source = turn(&db, &mut sequence, false).await;
        let context = db.milestone_context(8192, 10).await.unwrap();
        assert_eq!(context.source, Some(source.clone()));
        assert_eq!(context.new_turns, 1);
        assert!(context.new_tokens > 0);
        validate_source(db.database(), &source, db.session_id())
            .await
            .unwrap();
        db.close().await.unwrap();
    }
}

async fn initialized_session() -> (tempfile::TempDir, SessionDb, u64) {
    let root = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let db = SessionDb::initialize_turso_in_root(id, root.path())
        .await
        .unwrap();
    let mut sequence = 0;
    append(
        &db,
        &mut sequence,
        SessionEventKind::SessionCreated {
            name: None,
            working_directory: root.path().into(),
        },
    )
    .await;
    (root, db, sequence)
}

async fn append(db: &SessionDb, sequence: &mut u64, kind: SessionEventKind) {
    db.append_event(&SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: db.session_id(),
        sequence: *sequence,
        timestamp_ms: 1000 + *sequence,
        provenance: None,
        kind,
    })
    .await
    .unwrap();
    *sequence += 1;
}
async fn turn(db: &SessionDb, sequence: &mut u64, background: bool) -> SessionMilestoneSource {
    let start = *sequence;
    let id = format!("{}-{start}", db.session_id());
    append(
        db,
        sequence,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "hello 漢字".into(),
            admission: TurnAdmissionMetadata {
                priority: if background {
                    TurnPriority::Background
                } else {
                    TurnPriority::Interactive
                },
                ..TurnAdmissionMetadata::default()
            },
        },
    )
    .await;
    append(
        db,
        sequence,
        SessionEventKind::ModelTurnStarted {
            turn_id: id.clone(),
        },
    )
    .await;
    append(
        db,
        sequence,
        SessionEventKind::AssistantResponseSegment {
            turn_id: id.clone(),
            segment_id: "a".into(),
            segment_order: 0,
            text: "finished".into(),
        },
    )
    .await;
    let end = *sequence;
    append(
        db,
        sequence,
        SessionEventKind::ModelTurnFinished {
            turn_id: id,
            outcome: ModelTurnOutcome::Completed,
            message: None,
        },
    )
    .await;
    SessionMilestoneSource {
        start_sequence: start,
        end_sequence: end,
        anchor_sequence: start,
    }
}

#[tokio::test]
async fn bounded_context_excludes_background_preserves_no_result_source_and_counts_admissions() {
    let (_root, db, mut sequence) = initialized_session().await;
    let id = db.session_id();
    let source = turn(&db, &mut sequence, false).await;
    let background = turn(&db, &mut sequence, true).await;
    assert!(
        validate_source(db.database(), &background, id)
            .await
            .is_err()
    );
    let context = db.milestone_context(8192, 10).await.unwrap();
    assert_eq!(context.source, Some(source.clone()));
    assert_eq!(context.new_turns, 1);
    assert_eq!(
        context.new_tokens,
        estimated_normalized_text_tokens("hello 漢字")
            + estimated_normalized_text_tokens("finished")
    );
    assert_eq!(context.evaluation_count, 0);
    let request = SessionMilestoneEvaluationRequest {
        evaluation_id: "first".into(),
        source: source.clone(),
        expected_generation: sequence - 1,
        expected_explicit_name: None,
        max_requests_per_session: Some(1),
    };
    append(
        &db,
        &mut sequence,
        SessionEventKind::MilestoneEvaluationRequested { request },
    )
    .await;
    append(
        &db,
        &mut sequence,
        SessionEventKind::MilestoneEvaluationCompleted {
            completion: SessionMilestoneEvaluationCompletion {
                evaluation_id: "first".into(),
            },
        },
    )
    .await;
    let context = db.milestone_context(8192, 10).await.unwrap();
    assert_eq!(context.source, Some(source.clone()));
    assert_eq!(context.new_turns, 0);
    assert_eq!(context.new_tokens, 0);
    assert_eq!(context.evaluation_count, 1);
    assert!(context.last_evaluation_at_ms.is_some());
    let exhausted = SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: id,
        sequence,
        timestamp_ms: 1000 + sequence,
        provenance: None,
        kind: SessionEventKind::MilestoneEvaluationRequested {
            request: SessionMilestoneEvaluationRequest {
                evaluation_id: "second".into(),
                source: source.clone(),
                expected_generation: sequence - 1,
                expected_explicit_name: None,
                max_requests_per_session: Some(1),
            },
        },
    };
    assert!(db.append_event(&exhausted).await.is_err());
    let mut split = source;
    split.start_sequence += 1;
    assert!(validate_source(db.database(), &split, id).await.is_err());
    assert!(db.milestone_context(0, 1).await.is_err());
    assert!(db.milestone_context(16, 1).await.unwrap().truncated);
    db.close().await.unwrap();
}

#[tokio::test]
async fn interleaved_foreign_output_is_excluded_by_turn_identity() {
    let (_root, db, mut sequence) = initialized_session().await;
    let id = db.session_id();
    let start = sequence;
    let turn_id = TurnId::from_accepted_event(id, start).to_string();
    append(
        &db,
        &mut sequence,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "eligible prompt".into(),
            admission: TurnAdmissionMetadata::default(),
        },
    )
    .await;
    append(
        &db,
        &mut sequence,
        SessionEventKind::ModelTurnStarted {
            turn_id: turn_id.clone(),
        },
    )
    .await;
    let foreign_sequence = sequence;
    append(
        &db,
        &mut sequence,
        SessionEventKind::AssistantResponseSegment {
            turn_id: "foreign".into(),
            segment_id: "f".into(),
            segment_order: 0,
            text: "FOREIGN SECRET".into(),
        },
    )
    .await;
    append(
        &db,
        &mut sequence,
        SessionEventKind::AssistantResponseSegment {
            turn_id: turn_id.clone(),
            segment_id: "e".into(),
            segment_order: 0,
            text: "eligible result".into(),
        },
    )
    .await;
    let end = sequence;
    append(
        &db,
        &mut sequence,
        SessionEventKind::ModelTurnFinished {
            turn_id,
            outcome: ModelTurnOutcome::Error,
            message: Some("provider private diagnostic excluded".into()),
        },
    )
    .await;
    let context = db.milestone_context(8192, 10).await.unwrap();
    assert!(context.text.contains("eligible result"));
    assert!(context.text.contains("error"));
    assert!(!context.text.contains("FOREIGN SECRET"));
    assert!(!context.text.contains("private diagnostic"));
    assert_eq!(
        context.new_tokens,
        estimated_normalized_text_tokens("eligible prompt")
            + estimated_normalized_text_tokens("eligible result")
    );
    assert!(
        validate_source(
            db.database(),
            &SessionMilestoneSource {
                start_sequence: start,
                end_sequence: end,
                anchor_sequence: foreign_sequence
            },
            id
        )
        .await
        .is_err()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn published_timestamp_context_exclusion_and_rename_fence_are_durable() {
    let (root, db, mut sequence) = initialized_session().await;
    let id = db.session_id();
    let source = turn(&db, &mut sequence, false).await;
    let expected_generation = sequence - 1;
    append(
        &db,
        &mut sequence,
        SessionEventKind::MilestoneEvaluationRequested {
            request: SessionMilestoneEvaluationRequest {
                evaluation_id: "publish".into(),
                source: source.clone(),
                expected_generation,
                expected_explicit_name: None,
                max_requests_per_session: Some(10),
            },
        },
    )
    .await;
    let publication = SessionMilestonePublicationRequest {
        milestone: SessionMilestone {
            evaluation_id: "publish".into(),
            source,
            header: "Durable heading".into(),
            description: "Never inject this generated annotation".into(),
            provenance: SessionMilestoneProvenance {
                provider_id: "offline".into(),
                model_id: "fixture".into(),
                input_truncated: false,
            },
        },
        automatic_title: Some("Automatic".into()),
    };
    append(
        &db,
        &mut sequence,
        SessionEventKind::MilestonePublicationRequested {
            request: publication.clone(),
        },
    )
    .await;
    assert_eq!(
        db.milestone_publication("publish").await.unwrap(),
        Some(publication.clone())
    );
    let published_sequence = sequence;
    append(
        &db,
        &mut sequence,
        SessionEventKind::MilestonePublished {
            milestone: publication.milestone,
        },
    )
    .await;
    assert_initial_title_and_duplicate_rejection(&db, &mut sequence).await;
    append(
        &db,
        &mut sequence,
        SessionEventKind::SessionRenamed {
            name: Some("Explicit".into()),
        },
    )
    .await;
    let title = SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: id,
        sequence,
        timestamp_ms: 9999,
        provenance: None,
        kind: SessionEventKind::AutomaticTitleUpdated {
            evaluation_id: "publish".into(),
            title: "Overwrite".into(),
        },
    };
    assert!(db.append_event(&title).await.is_err());
    append(
        &db,
        &mut sequence,
        SessionEventKind::MilestoneEvaluationCompleted {
            completion: SessionMilestoneEvaluationCompletion {
                evaluation_id: "publish".into(),
            },
        },
    )
    .await;
    let page = assert_publication_projections(&db, published_sequence).await;
    db.close().await.unwrap();
    assert_reopened_publication(id, root.path(), &page).await;
}

async fn assert_publication_projections(
    db: &SessionDb,
    published_sequence: u64,
) -> bcode_session_models::SessionMilestonePage {
    let page = db
        .milestones(SessionMilestoneQuery {
            after_sequence: None,
            limit: 1,
        })
        .await
        .unwrap();
    assert_eq!(page.milestones.len(), 1);
    assert_eq!(page.milestones[0].timestamp_ms, 1000 + published_sequence);
    assert_eq!(page.milestones[0].event_sequence, published_sequence);
    assert!(
        db.milestones(SessionMilestoneQuery {
            after_sequence: Some(published_sequence),
            limit: 1
        })
        .await
        .unwrap()
        .milestones
        .is_empty()
    );
    assert!(
        db.model_context_events()
            .await
            .unwrap()
            .iter()
            .all(|event| !matches!(
                event.kind,
                SessionEventKind::MilestonePublished { .. }
                    | SessionEventKind::MilestonePublicationRequested { .. }
                    | SessionEventKind::MilestoneEvaluationRequested { .. }
                    | SessionEventKind::MilestoneEvaluationCompleted { .. }
                    | SessionEventKind::AutomaticTitleUpdated { .. }
            ))
    );
    assert_eq!(
        db.session_state()
            .await
            .unwrap()
            .unwrap()
            .explicit_name
            .as_deref(),
        Some("Explicit")
    );

    page
}

async fn assert_initial_title_and_duplicate_rejection(db: &SessionDb, sequence: &mut u64) {
    append(
        db,
        sequence,
        SessionEventKind::AutomaticTitleUpdated {
            evaluation_id: "publish".into(),
            title: "Automatic".into(),
        },
    )
    .await;
    let titled = db.session_state().await.unwrap().unwrap();
    assert_eq!(titled.automatic_title.as_deref(), Some("Automatic"));
    assert!(titled.explicit_name.is_none());
    assert!(titled.derived_title.is_some());
    let duplicate_title = SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: db.session_id(),
        sequence: *sequence,
        timestamp_ms: 9999,
        provenance: None,
        kind: SessionEventKind::AutomaticTitleUpdated {
            evaluation_id: "publish".into(),
            title: "Automatic".into(),
        },
    };
    assert!(db.append_event(&duplicate_title).await.is_err());
}

async fn assert_reopened_publication(
    id: SessionId,
    root: &std::path::Path,
    page: &bcode_session_models::SessionMilestonePage,
) {
    let reopened = SessionDb::open_existing_turso_in_root(id, root)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .milestones(SessionMilestoneQuery {
                after_sequence: None,
                limit: 10
            })
            .await
            .unwrap(),
        *page
    );
    reopened.close().await.unwrap();
}
