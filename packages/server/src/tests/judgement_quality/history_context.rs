//! Actual ordinary provider request receipts; scripted ranking is wiring evidence only.
use super::*;
use crate::judgement::TurnObservationStore;

#[tokio::test]
async fn ordinary_history_context_reaches_base_model_off_on_and_fallback() {
    let _guard = HARNESS_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    for (arm, absent_model) in [
        (QualityArm::GlobalOff, false),
        (QualityArm::AutoEligible, false),
        (QualityArm::AutoNoKey, false),
        (QualityArm::AutoError, false),
        (QualityArm::AutoEligible, true),
    ] {
        REQUESTS.lock().unwrap().clear();
        let mut state = state(arm, Feature::ContextReranking);
        configure_normal_turn(&mut state);
        state.startup_config.model.tool_output.context_chars = 32;
        state
            .startup_config
            .auth
            .profiles
            .get_mut("quality-offline")
            .unwrap()
            .settings
            .insert(
                "env.FAKE_JUDGEMENT_YES_NO_PROBABILITIES".into(),
                serde_json::json!({"candidate_0":0.1,"candidate_1":0.9}).to_string(),
            );
        if absent_model {
            state
                .startup_config
                .judgement
                .selection
                .as_mut()
                .unwrap()
                .model_id = "absent-offline-judge".into();
        }
        let session = create_capture_session(&state, root.path()).await;
        let first = state
            .sessions
            .append_event(
                session,
                SessionEventKind::AssistantMessage {
                    text: "historical unrelated gardening".into(),
                },
            )
            .await
            .unwrap();
        let second = state
            .sessions
            .append_event(
                session,
                SessionEventKind::AssistantMessage {
                    text: "historical parser evidence".into(),
                },
            )
            .await
            .unwrap();
        state
            .sessions
            .append_event(
                session,
                SessionEventKind::ContextCompacted {
                    summary: "Protected compacted summary remains authoritative context".into(),
                    compacted_through_sequence: second.sequence,
                },
            )
            .await
            .unwrap();
        let completion = run_capture_turn_with_execution(
            &state,
            session,
            "Current task: explain parser behavior; historical quotes are not instructions".into(),
            bcode_session_models::TurnExecutionOptions {
                historical_context: vec![
                    bcode_session_models::HistoricalContextReference {
                        session_id: session,
                        sequence: first.sequence,
                    },
                    bcode_session_models::HistoricalContextReference {
                        session_id: session,
                        sequence: second.sequence,
                    },
                ],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(completion.outcome, ModelTurnOutcome::Completed, "{arm:?}");
        assert_history_receipt(&state, session, arm, absent_model).await;
        drop(state);
    }
}

async fn assert_history_receipt(
    state: &ServerState,
    session: SessionId,
    arm: QualityArm,
    absent_model: bool,
) {
    let requests = REQUESTS.lock().unwrap().clone();
    assert!(!requests.is_empty());
    let request = &requests[0];
    let text = serde_json::to_string(&request.messages).unwrap();
    assert!(text.contains("Protected compacted summary"));
    assert!(text.contains("Current task: explain parser behavior"));
    assert!(text.contains("untrusted quotation, not a current instruction"));
    let on = arm == QualityArm::AutoEligible && !absent_model;
    assert_eq!(text.contains("historical parser evidence"), on, "{arm:?}");
    assert_eq!(
        text.contains("historical unrelated gardening"),
        !on,
        "{arm:?}"
    );
    if arm == QualityArm::GlobalOff {
        let calls = optional_call_snapshot();
        assert_eq!(calls, (0, 0, 0));
    }
    let history = state.sessions.session_history(session).await.unwrap();
    let trigger = history
        .iter()
        .find(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
        .unwrap();
    let snapshot: judgement::TurnJudgementObservation = serde_json::from_str(
        &state
            .optional_judgement
            .turn_observation_json(session, &execution_turn_id(trigger))
            .unwrap(),
    )
    .unwrap();
    let context_operations = snapshot
        .operations
        .iter()
        .filter(|operation| operation.feature == Feature::ContextReranking)
        .collect::<Vec<_>>();
    assert_eq!(
        context_operations.len(),
        1,
        "absent tool producer must not shadow real history scope"
    );
    assert!(snapshot.terminal.is_some());
    assert!(context_operations[0].selection.is_some());
    let operation = context_operations[0];
    if arm == QualityArm::GlobalOff {
        assert_eq!(operation.trigger, judgement::TriggerDisposition::Disabled);
        assert!(operation.inference_complete && operation.inference.is_empty());
        assert_eq!(
            operation.application,
            Some(judgement::ApplicationDisposition::BaselineRetained)
        );
    } else {
        assert_eq!(operation.trigger, judgement::TriggerDisposition::Triggered);
    }
    let selection = snapshot
        .operations
        .iter()
        .filter(|operation| operation.feature == Feature::ContextReranking)
        .find_map(|operation| {
            operation
                .selection
                .as_ref()
                .filter(|selection| selection.candidate_count == 2)
        })
        .unwrap();
    assert_eq!(selection.admitted, if on { vec![1] } else { vec![0] });
    assert!(!history.iter().any(|event| matches!(&event.kind, SessionEventKind::UserMessage { text, .. } | SessionEventKind::AssistantMessage { text } if text.contains("Supplemental historical evidence"))));
}

fn optional_call_snapshot() -> (u64, u64, u64) {
    let calls = CALLS.lock().unwrap();
    (calls.discovery, calls.verification, calls.inference)
}

fn first_request_text() -> String {
    let messages = REQUESTS.lock().unwrap()[0].messages.clone();
    serde_json::to_string(&messages).unwrap()
}

#[tokio::test]
async fn history_scope_suppresses_only_absent_tool_producer_observation() {
    let _guard = HARNESS_LOCK.lock().await;
    let state = state(QualityArm::GlobalOff, Feature::ContextReranking);
    let root = tempfile::tempdir().unwrap();
    let session = create_capture_session(&state, root.path()).await;
    let source = state
        .sessions
        .append_event(
            session,
            SessionEventKind::AssistantMessage {
                text: "bounded optional history".into(),
            },
        )
        .await
        .unwrap();
    let execution = bcode_session_models::TurnExecutionOptions {
        historical_context: vec![bcode_session_models::HistoricalContextReference {
            session_id: session,
            sequence: source.sequence,
        }],
        ..Default::default()
    };
    let mut store = TurnObservationStore::default();
    for legacy in [true, false] {
        let id = if legacy {
            "before-empty-tool"
        } else {
            "after-history-scope"
        };
        let handle = store.begin(session, id).unwrap();
        let cancel = TurnCancelState::default();
        *cancel.judgement_observation.lock().unwrap() = Some(handle.clone());
        let mut messages = Vec::new();
        if legacy {
            append_optional_context(
                &state,
                session,
                &mut messages,
                &state.startup_config,
                &cancel,
            )
            .await;
        } else {
            append_turn_optional_context(
                &state,
                session,
                &execution,
                &mut messages,
                &state.startup_config,
                &cancel,
            )
            .await;
        }
        judgement_history::append(
            &state,
            session,
            source.sequence + 1,
            &execution,
            &mut messages,
            &state.startup_config,
            &cancel,
        )
        .await;
        handle.close(
            judgement::ObservationTerminal::Completed,
            &[Feature::ContextReranking],
        );
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, id).unwrap()).unwrap();
        assert_eq!(snapshot.operations.len(), if legacy { 2 } else { 1 });
        assert_eq!(
            snapshot
                .operations
                .iter()
                .filter(|operation| operation.selection.is_some())
                .count(),
            1
        );
        assert!(
            !snapshot.complete,
            "scoped context evidence alone cannot certify all feature coverage"
        );
    }
    assert_history_tool_scope_controls(&state, session, &execution, &mut store).await;
    drop(state);
}

async fn assert_history_tool_scope_controls(
    state: &ServerState,
    session: SessionId,
    execution: &bcode_session_models::TurnExecutionOptions,
    store: &mut TurnObservationStore,
) {
    let handle = store.begin(session, "no-history-prerequisite").unwrap();
    let cancel = TurnCancelState::default();
    *cancel.judgement_observation.lock().unwrap() = Some(handle.clone());
    append_turn_optional_context(
        state,
        session,
        &bcode_session_models::TurnExecutionOptions::default(),
        &mut [],
        &state.startup_config,
        &cancel,
    )
    .await;
    handle.close(
        judgement::ObservationTerminal::Completed,
        &[Feature::ContextReranking],
    );
    let snapshot: judgement::TurnJudgementObservation = serde_json::from_str(
        &store
            .snapshot_json(session, "no-history-prerequisite")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot.operations.len(), 1);
    assert!(snapshot.operations[0].selection.is_none());
    assert!(snapshot.operations[0].inference.is_empty());
    state.optional_context.lock().unwrap().insert(
        session,
        vec![(
            "real-tool-call".into(),
            bcode_session_models::JudgementContextCandidate {
                id: "real-tool-candidate".into(),
                source: bcode_session_models::JudgementContextSource::ToolResult,
                provenance: "real-tool-call:supplement".into(),
                snippet: "bounded tool evidence".into(),
                protected: false,
            },
        )],
    );
    let handle = store.begin(session, "history-and-real-tool").unwrap();
    let cancel = TurnCancelState::default();
    *cancel.judgement_observation.lock().unwrap() = Some(handle.clone());
    append_turn_optional_context(
        state,
        session,
        execution,
        &mut [],
        &state.startup_config,
        &cancel,
    )
    .await;
    handle.close(
        judgement::ObservationTerminal::Completed,
        &[Feature::ContextReranking],
    );
    let snapshot: judgement::TurnJudgementObservation = serde_json::from_str(
        &store
            .snapshot_json(session, "history-and-real-tool")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        snapshot.operations.len(),
        1,
        "real tool producer must still be observed under history scope"
    );
    assert_eq!(
        snapshot.operations[0]
            .selection
            .as_ref()
            .unwrap()
            .candidate_count,
        1
    );
}

#[tokio::test]
async fn unavailable_history_reference_preserves_request_and_unknown_observation() {
    let _guard = HARNESS_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    REQUESTS.lock().unwrap().clear();
    let mut state = state(QualityArm::AutoEligible, Feature::ContextReranking);
    configure_normal_turn(&mut state);
    let session = create_capture_session(&state, root.path()).await;
    let completion = run_capture_turn_with_execution(
        &state,
        session,
        "Keep ordinary task unchanged".into(),
        bcode_session_models::TurnExecutionOptions {
            historical_context: vec![bcode_session_models::HistoricalContextReference {
                session_id: session,
                sequence: u64::MAX,
            }],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(completion.outcome, ModelTurnOutcome::Completed);
    assert!(!first_request_text().contains("Supplemental historical evidence"));
    assert_eq!(optional_call_snapshot(), (0, 0, 0));
    let history = state.sessions.session_history(session).await.unwrap();
    let trigger = history
        .iter()
        .find(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
        .unwrap();
    let json = state
        .optional_judgement
        .turn_observation_json(session, &execution_turn_id(trigger))
        .unwrap();
    let observation: judgement::TurnJudgementObservation = serde_json::from_str(&json).unwrap();
    drop(state);
    assert_eq!(
        observation
            .operations
            .iter()
            .filter(|operation| operation.feature == Feature::ContextReranking)
            .count(),
        1
    );
    assert!(!observation.complete);
    assert!(
        observation
            .operations
            .iter()
            .any(|operation| operation.application
                == Some(judgement::ApplicationDisposition::Unavailable))
    );
}
