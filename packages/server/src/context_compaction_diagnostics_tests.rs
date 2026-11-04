use super::*;

async fn fixture() -> (
    tempfile::TempDir,
    ServerState,
    SessionId,
    SessionModelSelection,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("control")).unwrap();
    let sessions = bcode_session::SessionManager::default();
    let session = sessions
        .create_session(None, root.path().to_owned())
        .await
        .unwrap();
    for _ in 0..2 {
        sessions
            .append_event(
                session.id,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "BCODE_FIXTURE_SCOPE {\"key\":\"scope\",\"statement\":\"Do not delete\"}"
                        .into(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
            .await
            .unwrap();
        sessions
            .append_event(
                session.id,
                SessionEventKind::AssistantMessage {
                    text: "Historical investigation detail. ".repeat(24),
                },
            )
            .await
            .unwrap();
    }
    let mut state = crate::tests::test_server_state_with_fake_provider(sessions);
    state.startup_config.skills.enabled = false;
    state.startup_config.invariants.enabled = false;
    state.startup_config.system_prompt.mode = bcode_config::SystemPromptMode::Replace;
    state.startup_config.system_prompt.text = Some("Test compaction".into());
    state.startup_config.model.compaction.keep_recent_tokens = 1;
    state.startup_config.model.compaction.backend = bcode_config::CompactionBackend::Local;
    let mut selection = SessionModelSelection {
        provider_plugin_id: Some("bcode.fake-provider".into()),
        model_id: Some("fake-echo".into()),
        ..SessionModelSelection::default()
    };
    selection
        .provider_context
        .settings
        .insert("fake_compaction_fixture".into(), "scope-v1".into());
    selection.provider_context.settings.insert(
        "fake_compaction_control_dir".into(),
        root.path()
            .join("control")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .into(),
    );
    (root, state, session.id, selection)
}

async fn observations(state: &ServerState, id: SessionId) -> Vec<CompactionDiagnostic> {
    state
        .sessions
        .session_history(id)
        .await
        .unwrap()
        .iter()
        .filter_map(|event| {
            if let SessionEventKind::TraceEvent { trace } = &event.kind
                && let SessionTracePayload::ProviderEvent {
                    event_type,
                    detail: Some(detail),
                } = &trace.payload
                && event_type == "compaction_diagnostic_v1"
            {
                return Some(serde_json::from_str(detail).unwrap());
            }
            None
        })
        .collect()
}

#[tokio::test]
async fn compaction_diagnostics_correlate_malformed_repair_commit_and_resubmission() {
    let (_root, state, id, mut selection) = fixture().await;
    selection
        .provider_context
        .settings
        .insert("fake_compaction_outcome".into(), "malformed".into());
    let result = Box::pin(compact_session_context_with_limit(
        &state,
        id,
        &selection,
        None,
        None,
        &TurnCancelState::default(),
        None,
    ))
    .await;
    assert!(result.is_err());
    let first = observations(&state, id).await;
    let rejected = first
        .iter()
        .filter(|value| value.outcome == "rejected")
        .collect::<Vec<_>>();
    assert!(!rejected.is_empty(), "{first:?}");
    for attempt in 1..=2 {
        assert!(rejected.iter().any(|value| value.stage == "contract"
            && value.attempt == Some(attempt)
            && value.reason.as_deref() == Some("invalid_json")));
        assert!(
            rejected
                .iter()
                .any(|value| value.stage == "merge" && value.attempt == Some(attempt))
        );
    }
    assert!(
        first
            .iter()
            .all(|value| value.valid() && value.operation_id == first[0].operation_id)
    );
    let activations = first
        .iter()
        .filter(|value| value.outcome == "started")
        .map(|value| value.activation_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(activations.len(), 2); // One explicitly admitted correction, never a hidden reset.
    for activation in activations {
        let events = first
            .iter()
            .filter(|value| value.activation_id == activation)
            .collect::<Vec<_>>();
        assert_eq!(events.first().unwrap().outcome, "started");
        assert_eq!(events.last().unwrap().outcome, "failed");
    }
    for value in rejected {
        assert!(value.response.is_some());
        assert!(
            first
                .iter()
                .any(|dispatched| dispatched.outcome == "dispatch_requested"
                    && dispatched.request_id == value.request_id)
        );
    }
    assert_eq!(first.last().unwrap().outcome, "failed");
    assert!(!first.iter().any(|value| value.outcome == "committed"));
    assert!(
        Box::pin(compact_session_context_with_limit(
            &state,
            id,
            &selection,
            None,
            None,
            &TurnCancelState::default(),
            None
        ))
        .await
        .is_err()
    );
    let all = observations(&state, id).await;
    assert_eq!(all[first.len()].operation_id, first[0].operation_id);
    assert_ne!(
        all[first.len()].activation_id,
        first.last().unwrap().activation_id
    );
    assert_eq!(
        all.iter()
            .filter(|value| value.outcome == "dispatch_requested")
            .count(),
        first
            .iter()
            .filter(|value| value.outcome == "dispatch_requested")
            .count()
    );
}

#[tokio::test]
async fn compaction_diagnostics_correlate_successful_repair_and_atomic_commit() {
    let (_root, state, id, mut selection) = fixture().await;
    selection
        .provider_context
        .settings
        .insert("fake_compaction_outcome".into(), "repair".into());
    Box::pin(compact_session_context_with_limit(
        &state,
        id,
        &selection,
        None,
        None,
        &TurnCancelState::default(),
        None,
    ))
    .await
    .unwrap();
    let values = observations(&state, id).await;
    let committed = values
        .iter()
        .find(|value| value.outcome == "committed")
        .unwrap();
    assert_eq!(committed.operation_id, values[0].operation_id);
    assert_eq!(committed.activation_id, values[0].activation_id);
    assert!(committed.marker_sequence.is_some());
    assert!(values.iter().any(|value| value.outcome == "rejected"));
}

#[tokio::test]
async fn compaction_diagnostics_capture_wire_budget_and_consolidation_rejections() {
    let (_root, state, id, mut selection) = fixture().await;
    selection
        .provider_context
        .settings
        .insert("fake_compaction_outcome".into(), "oversize".into());
    assert!(
        Box::pin(compact_session_context_with_limit(
            &state,
            id,
            &selection,
            None,
            None,
            &TurnCancelState::default(),
            None
        ))
        .await
        .is_err()
    );
    let values = observations(&state, id).await;
    assert!(
        values
            .iter()
            .any(|value| value.outcome == "rejected"
                && value.reason.as_deref() == Some("wire_budget"))
    );
    assert!(!values.iter().any(|value| value.outcome == "committed"));

    let (_root, state, id, mut selection) = fixture().await;
    selection
        .provider_context
        .settings
        .insert("fake_compaction_outcome".into(), "malformed".into());
    let history = state.sessions.session_history(id).await.unwrap();
    let plan =
        checkpoint::frozen_plan(&history, 2000, history[history.len() - 2].sequence).unwrap();
    let transcript = compaction_transcript_from_plan(&plan);
    let policy = resolve_summary_budget(
        &state,
        id,
        &selection,
        &history,
        transcript.compacted_through_sequence,
        1,
        &TurnCancelState::default(),
    )
    .await
    .unwrap();
    let evidence = protected::initial(&transcript, 1_000_000).unwrap();
    run(async {
        start(&state, id, "consolidation-test").await.unwrap();
        assert!(
            Box::pin(consolidate_carry(
                &state,
                id,
                &selection,
                &transcript,
                &evidence,
                &policy,
                None,
                &TurnCancelState::default(),
                &mut None
            ))
            .await
            .is_err()
        );
    })
    .await;
    let values = observations(&state, id).await;
    assert!(values.iter().any(|value| value.stage == "consolidation"
        && value.outcome == "rejected"
        && value.contract.as_deref() == Some("consolidation")));
}

#[tokio::test]
async fn compaction_diagnostics_never_invent_completion_on_cancel() {
    let (_root, state, id, selection) = fixture().await;
    let cancel = TurnCancelState::default();
    cancel.cancel().await;
    let result = Box::pin(compact_session_context_with_policy(
        &state,
        id,
        &selection,
        None,
        None,
        &cancel,
        None,
        CompactionPlanningPolicy::Proactive {
            keep_recent_tokens: 1,
        },
    ))
    .await;
    assert!(
        matches!(result, Err(CompactionError::Cancelled)),
        "{:?}",
        result.as_ref().err()
    );
    let values = observations(&state, id).await;
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].outcome, "cancelled");
    assert!(values[0].operation_id.is_none());
}

#[tokio::test]
async fn compaction_diagnostics_preserve_failed_and_interrupted_outcomes() {
    let (_root, state, id, mut selection) = fixture().await;
    selection
        .provider_context
        .settings
        .insert("fake_compaction_outcome".into(), "fail".into());
    assert!(
        Box::pin(compact_session_context_with_limit(
            &state,
            id,
            &selection,
            None,
            None,
            &TurnCancelState::default(),
            None
        ))
        .await
        .is_err()
    );
    let values = observations(&state, id).await;
    assert!(
        values
            .iter()
            .any(|value| value.stage == "request" && value.outcome == "failed")
    );
    assert_eq!(values.last().unwrap().outcome, "failed");
    assert!(!values.iter().any(|value| value.outcome == "committed"));

    let (_root, state, id, selection) = fixture().await;
    let history = state.sessions.session_history(id).await.unwrap();
    let plan =
        checkpoint::frozen_plan(&history, 2000, history[history.len() - 2].sequence).unwrap();
    let transcript = compaction_transcript_from_plan(&plan);
    let policy = resolve_summary_budget(
        &state,
        id,
        &selection,
        &history,
        transcript.compacted_through_sequence,
        1,
        &TurnCancelState::default(),
    )
    .await
    .unwrap();
    run(async {
        start(&state, id, "failed-publication").await.unwrap();
        let result = Box::pin(publish_compaction_plan(
            &state,
            id,
            &selection,
            &TurnCancelState::default(),
            &history,
            &plan,
            &transcript,
            &policy,
            "failed-publication",
            None,
            None,
            String::new(),
        ))
        .await;
        assert!(result.is_err());
        finish(&state, id, &result).await.unwrap();
    })
    .await;
    let values = observations(&state, id).await;
    assert!(
        values
            .iter()
            .any(|value| value.stage == "publication" && value.outcome == "failed")
    );
    assert!(!values.iter().any(|value| value.outcome == "committed"));
    let count = values.len();
    run(async {
        start(&state, id, "interrupted-operation").await.unwrap();
        position(Some(1), 1);
        contract(response::ResponseContract::Recollection);
        request("synthetic-request");
        emit(&state, id, "request", "dispatch_requested", None, None)
            .await
            .unwrap();
        // Dropping an activation never fabricates settlement or terminal success.
    })
    .await;
    let values = observations(&state, id).await;
    assert_eq!(values.len(), count + 2);
    assert_eq!(values.last().unwrap().outcome, "dispatch_requested");
}
