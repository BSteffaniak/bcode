fn steering_test_turn(
    execution: TurnExecutionOptions,
    selection: SessionModelSelection,
) -> RuntimeCurrentTurn {
    RuntimeCurrentTurn {
        interaction_route: execution.interaction_route.clone(),
        kind: RuntimeOperationKind::ModelTurn,
        client_id: ClientId::new(),
        turn_id: "active".into(),
        cancel_state: Arc::new(TurnCancelState::default()),
        model: None,
        agent_profile: execution.agent_profile.clone(),
        execution: Some(execution),
        selection: Some(selection),
    }
}

#[test]
fn steering_accepts_equivalent_effective_selection_without_changing_policy() {
    let execution = TurnExecutionOptions::default();
    let selection = SessionModelSelection {
        provider_plugin_id: Some("provider".into()),
        requested_model_id: None,
        model_id: Some("model".into()),
        reasoning_effort: Some("high".into()),
        reasoning_summary: Some("auto".into()),
        ..Default::default()
    };
    let turn = steering_test_turn(execution.clone(), selection.clone());
    let explicit = TurnExecutionOptions {
        provider_plugin_id: selection.provider_plugin_id.clone(),
        model_id: selection.model_id.clone(),
        reasoning: Some(Box::new(bcode_session_models::TurnReasoningOptions {
            effort: selection.reasoning_effort.clone(),
            summary: selection.reasoning_summary.clone(),
        })),
        ..execution.clone()
    };
    let mut requested = selection.clone();
    apply_turn_model_selection(&mut requested, &explicit);
    assert!(turn.accepts_steering(&explicit, &requested));
    assert_eq!(turn.execution.as_ref(), Some(&execution));
    assert_eq!(turn.selection.as_ref(), Some(&selection));
    let explicit_turn = steering_test_turn(explicit.clone(), selection.clone());
    assert!(explicit_turn.accepts_steering(&execution, &selection));

    for changed in [
        TurnExecutionOptions {
            interaction_route: Some(bcode_session_models::ExecutionInteractionRoute {
                version: 1,
                destination_session_id: SessionId::new(),
            }),
            ..explicit.clone()
        },
        TurnExecutionOptions {
            correlation: Some(bcode_session_models::TurnExecutionCorrelation {
                execution_id: "execution".into(),
                unit_id: "unit".into(),
                attempt: 1,
            }),
            ..explicit.clone()
        },
        TurnExecutionOptions {
            request_context_id: Some("captured".into()),
            ..explicit.clone()
        },
        TurnExecutionOptions {
            historical_context: vec![],
            schema_version: execution.schema_version + 1,
            ..explicit.clone()
        },
    ] {
        assert!(!turn.accepts_steering(&changed, &selection));
    }
    let mut foreign_context = selection.clone();
    foreign_context
        .provider_context
        .settings
        .insert("different".into(), "context".into());
    assert!(!turn.accepts_steering(&explicit, &foreign_context));
    let missing_selection = RuntimeCurrentTurn {
        selection: None,
        ..turn
    };
    assert!(!missing_selection.accepts_steering(&explicit, &selection));
}

async fn submission_runtime_fixture(
    sessions: SessionManager,
) -> (
    Arc<ServerState>,
    SessionId,
    SessionRuntimeHandle,
    mpsc::Receiver<FollowupCommand>,
    mpsc::Receiver<SteeringCommand>,
    mpsc::Receiver<CancelCommand>,
) {
    let session = sessions
        .create_session(None, test_working_directory())
        .await
        .unwrap();
    let state = Arc::new(test_server_state(sessions));
    let (followup_commands, followups) = mpsc::channel(128);
    let (steering_commands, steering) = mpsc::channel(128);
    let (cancel_commands, cancels) = mpsc::channel(1);
    let handle = SessionRuntimeHandle {
        followup_commands,
        steering_commands,
        cancel_commands,
        queued_followups: Arc::new(AtomicUsize::new(0)),
        queued_steering: Arc::new(AtomicUsize::new(0)),
        phase: Arc::new(Mutex::new(SessionRuntimePhase::ProviderActive)),
        current_turn: Arc::new(Mutex::new(None)),
    };
    state
        .session_runtimes
        .lock()
        .await
        .insert(session.id, handle.clone());
    (state, session.id, handle, followups, steering, cancels)
}

async fn install_submission_test_turn(
    state: &ServerState,
    session_id: SessionId,
    handle: &SessionRuntimeHandle,
) {
    let execution = TurnExecutionOptions {
        agent_profile: Some("build".into()),
        ..Default::default()
    };
    let mut selection = session_model_selection_with_runtime_context(state, session_id, None).await;
    apply_turn_model_selection(&mut selection, &execution);
    *handle.current_turn.lock().await = Some(steering_test_turn(execution, selection));
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Exercises rejection and retry target preservation in one runtime.
async fn strict_steering_rejects_before_acceptance_and_preserves_retry_targets() {
    use bcode_session_models::{
        SubmissionId, SubmissionPlacement, SubmissionTarget, TurnAdmissionMetadata,
    };
    let workspace = tempfile::tempdir().unwrap();
    let sessions = SessionManager::persistent(workspace.path().join("sessions")).unwrap();
    let (state, session_id, handle, mut followups, _steering, _cancels) =
        submission_runtime_fixture(sessions).await;
    let client = ClientId::new();
    let initial_history = state.sessions.session_history(session_id).await.unwrap();
    let unavailable = enqueue_prompt_submission(
        &state,
        session_id,
        client,
        SubmissionId("absent".into()),
        "strict".into(),
        SubmissionPlacement::Steer,
        true,
        TurnAdmissionMetadata::default(),
    )
    .await;
    assert!(matches!(unavailable, Err(ServerError::SteeringUnavailable)));
    assert_eq!(
        state.sessions.session_history(session_id).await.unwrap(),
        initial_history
    );
    install_submission_test_turn(&state, session_id, &handle).await;
    for (id, placement, admission) in [
        (
            "followup",
            SubmissionPlacement::FollowUp,
            TurnAdmissionMetadata::default(),
        ),
        (
            "permission",
            SubmissionPlacement::Steer,
            TurnAdmissionMetadata {
                execution: TurnExecutionOptions {
                    permission_mode: bcode_session_models::TurnPermissionMode::Bypass,
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
        (
            "model",
            SubmissionPlacement::Steer,
            TurnAdmissionMetadata {
                execution: TurnExecutionOptions {
                    model_id: Some("different-model".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
    ] {
        let result = enqueue_prompt_submission(
            &state,
            session_id,
            client,
            SubmissionId(id.into()),
            "strict".into(),
            placement,
            true,
            admission,
        )
        .await;
        assert!(matches!(result, Err(ServerError::SteeringUnavailable)));
        assert!(
            state
                .sessions
                .submission(session_id, SubmissionId(id.into()))
                .await
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        state.sessions.session_history(session_id).await.unwrap(),
        initial_history
    );
    let id = SubmissionId("steering-retry".into());
    let receipt = enqueue_prompt_submission(
        &state,
        session_id,
        client,
        id.clone(),
        "strict".into(),
        SubmissionPlacement::Steer,
        true,
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    state
        .sessions
        .cancel_submission(session_id, id.clone())
        .await
        .unwrap();
    *handle.current_turn.lock().await = None;
    let conflicting = enqueue_prompt_submission(
        &state,
        session_id,
        client,
        id.clone(),
        "changed payload".into(),
        SubmissionPlacement::Steer,
        true,
        TurnAdmissionMetadata::default(),
    )
    .await;
    assert!(conflicting.is_err());
    assert_eq!(
        state
            .sessions
            .submission(session_id, id.clone())
            .await
            .unwrap()
            .unwrap()
            .receipt,
        receipt
    );
    let retry = enqueue_prompt_submission(
        &state,
        session_id,
        client,
        id.clone(),
        "strict".into(),
        SubmissionPlacement::Steer,
        true,
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    assert_eq!(receipt, retry);
    assert!(matches!(
        state
            .sessions
            .submission(session_id, id)
            .await
            .unwrap()
            .unwrap()
            .submission
            .target,
        SubmissionTarget::Steering { .. }
    ));
    let own_id = SubmissionId("own-retry".into());
    let own = enqueue_prompt_submission(
        &state,
        session_id,
        client,
        own_id.clone(),
        "legacy".into(),
        SubmissionPlacement::Steer,
        false,
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    install_submission_test_turn(&state, session_id, &handle).await;
    let strict = enqueue_prompt_submission(
        &state,
        session_id,
        client,
        own_id.clone(),
        "legacy".into(),
        SubmissionPlacement::Steer,
        true,
        TurnAdmissionMetadata::default(),
    )
    .await;
    assert!(matches!(strict, Err(ServerError::SteeringUnavailable)));
    let retry = enqueue_prompt_submission(
        &state,
        session_id,
        client,
        own_id.clone(),
        "legacy".into(),
        SubmissionPlacement::Steer,
        false,
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    assert_eq!(own, retry);
    assert!(matches!(
        state
            .sessions
            .submission(session_id, own_id)
            .await
            .unwrap()
            .unwrap()
            .submission
            .target,
        SubmissionTarget::OwnTurn { .. }
    ));
    while followups.try_recv().is_ok() {}
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Covers indexed pages and cancellation across the same burst.
async fn submission_burst_pages_past_steering_and_cancels_all_turn_pages() {
    use bcode_session_models::{
        SubmissionId, SubmissionPlacement, SubmissionStatus, TurnAdmissionMetadata,
    };
    let workspace = tempfile::tempdir().unwrap();
    let sessions = SessionManager::persistent(workspace.path().join("sessions")).unwrap();
    let (state, session_id, handle, mut followups, _steering, _cancels) =
        submission_runtime_fixture(sessions).await;
    install_submission_test_turn(&state, session_id, &handle).await;
    let client = ClientId::new();
    for index in 0..70 {
        enqueue_prompt_submission(
            &state,
            session_id,
            client,
            SubmissionId(format!("burst-{index}")),
            "steer".into(),
            SubmissionPlacement::Steer,
            true,
            TurnAdmissionMetadata::default(),
        )
        .await
        .unwrap();
    }
    let own_id = SubmissionId("after-burst".into());
    enqueue_prompt_submission(
        &state,
        session_id,
        client,
        own_id.clone(),
        "own".into(),
        SubmissionPlacement::FollowUp,
        false,
        TurnAdmissionMetadata::default(),
    )
    .await
    .unwrap();
    let first_page = pending_submissions_page(&state, session_id, None, 32)
        .await
        .unwrap();
    assert_eq!(first_page.records.len(), 32);
    assert!(first_page.has_more);
    let second_page = pending_submissions_page(&state, session_id, first_page.next_after, 32)
        .await
        .unwrap();
    assert_eq!(second_page.records.len(), 32);
    assert!(second_page.has_more);
    let last_page = pending_submissions_page(&state, session_id, second_page.next_after, 32)
        .await
        .unwrap();
    assert_eq!(last_page.records.len(), 7);
    assert!(!last_page.has_more);
    assert_eq!(last_page.next_after, None);
    assert_eq!(
        last_page.records.last().unwrap().receipt.submission_id,
        own_id
    );
    let mut hints = Vec::new();
    while let Ok(command) = followups.try_recv() {
        retain_dispatch_hint(&mut hints, command);
    }
    let mut cursor = None;
    assert!(
        next_pending_submission_command(&state, session_id, &mut cursor, &mut hints)
            .await
            .unwrap()
            .is_none()
    );
    assert!(cursor.is_some());
    assert!(
        next_pending_submission_command(&state, session_id, &mut cursor, &mut hints)
            .await
            .unwrap()
            .is_none()
    );
    let command = next_pending_submission_command(&state, session_id, &mut cursor, &mut hints)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(command_submission_id(&command), Some(&own_id));
    cancel_pending_turn_submissions(&state, session_id, "active")
        .await
        .unwrap();
    for index in 0..70 {
        let record = state
            .sessions
            .submission(session_id, SubmissionId(format!("burst-{index}")))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(record.status, SubmissionStatus::Cancelled { .. }));
    }
    assert!(matches!(
        state
            .sessions
            .submission(session_id, own_id.clone())
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Pending
    ));
    let remaining = pending_submissions_page(&state, session_id, None, 32)
        .await
        .unwrap();
    assert_eq!(remaining.records.len(), 1);
    assert!(!remaining.has_more);
    assert_eq!(remaining.records[0].receipt.submission_id, own_id);
    prune_settled_dispatch_hints(&state, session_id, &mut hints)
        .await
        .unwrap();
    assert!(hints.is_empty());
}

#[tokio::test]
async fn strict_steering_admits_explicit_equivalent_defaults() {
    use bcode_session_models::{
        SubmissionId, SubmissionPlacement, SubmissionTarget, TurnAdmissionMetadata,
    };
    let workspace = tempfile::tempdir().unwrap();
    let sessions = SessionManager::persistent(workspace.path().join("sessions")).unwrap();
    let (state, session_id, handle, _followups, _steering, _cancels) =
        submission_runtime_fixture(sessions).await;
    let selection = SessionModelSelection {
        provider_plugin_id: Some("provider".into()),
        model_id: Some("model".into()),
        reasoning_effort: Some("high".into()),
        ..Default::default()
    };
    state
        .session_model_selections
        .lock()
        .await
        .insert(session_id, selection.clone());
    let execution = TurnExecutionOptions {
        agent_profile: Some("build".into()),
        ..Default::default()
    };
    *handle.current_turn.lock().await =
        Some(steering_test_turn(execution.clone(), selection.clone()));
    let receipt = enqueue_prompt_submission(
        &state,
        session_id,
        ClientId::new(),
        SubmissionId("explicit-defaults".into()),
        "steer".into(),
        SubmissionPlacement::Steer,
        true,
        TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                provider_plugin_id: selection.provider_plugin_id.clone(),
                model_id: selection.model_id.clone(),
                reasoning: Some(Box::new(bcode_session_models::TurnReasoningOptions {
                    effort: selection.reasoning_effort.clone(),
                    summary: None,
                })),
                ..execution.clone()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let record = state
        .sessions
        .submission(session_id, receipt.submission_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        record.submission.target,
        SubmissionTarget::Steering { .. }
    ));
    assert_eq!(
        handle
            .current_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .execution
            .as_ref(),
        Some(&execution)
    );
}
