use super::*;
use bcode_session_models::{
    SessionEventKind, TurnExecutionOptions, TurnStructuredOutputRequest, TurnToolPolicy,
};

#[tokio::test]
async fn source_capture_is_pinned_private_and_fails_closed() {
    let sessions = bcode_session::SessionManager::default();
    let source = sessions
        .create_session(Some("source".into()), std::env::current_dir().unwrap())
        .await
        .unwrap();
    sessions
        .append_event(
            source.id,
            SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "Preserve streaming; fix reconnect".into(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        )
        .await
        .unwrap();
    let state = crate::tests::test_server_state_with_fake_provider(sessions);
    let captured = prepare(
        &state,
        bcode_session_models::PrepareContextGeneration {
            version: 1,
            source_session_id: source.id,
            name: "generation".into(),
        },
    )
    .await
    .unwrap();
    state
        .sessions
        .append_event(
            source.id,
            SessionEventKind::AssistantMessage {
                text: "later message outside capture".into(),
            },
        )
        .await
        .unwrap();
    let execution = TurnExecutionOptions {
        request_context_id: Some(captured.context_id.clone()),
        tools: TurnToolPolicy::Disabled,
        structured_output: Some(TurnStructuredOutputRequest {
            name: "goal".into(),
            schema: serde_json::json!({"type":"object"}),
            strict: true,
            max_corrections: 0,
        }),
        ..Default::default()
    };
    let history = events(&state, captured.session_id, &execution)
        .await
        .unwrap()
        .unwrap();
    let projected = crate::session_events_to_model_messages(&history);
    let text = serde_json::to_string(&projected).unwrap();
    assert!(text.contains("Preserve streaming"));
    assert!(!text.contains("later message"));
    Box::pin(assert_generation_request(
        &state,
        captured.session_id,
        &execution,
    ))
    .await;
    let destination = state
        .sessions
        .model_context_events(captured.session_id)
        .await
        .unwrap();
    assert!(
        !serde_json::to_string(&destination)
            .unwrap()
            .contains("Preserve streaming")
    );
    assert!(events(&state, source.id, &execution).await.is_err());
    let mut unsafe_execution = execution.clone();
    unsafe_execution.tools = TurnToolPolicy::Enabled;
    assert!(
        events(&state, captured.session_id, &unsafe_execution)
            .await
            .is_err()
    );
    state.generation_contexts.lock().await.clear();
    assert!(
        events(&state, captured.session_id, &execution)
            .await
            .is_err()
    );
    drop(state);
}

async fn assert_generation_request(
    state: &ServerState,
    session_id: SessionId,
    execution: &TurnExecutionOptions,
) {
    let trigger = state
        .sessions
        .append_event(
            session_id,
            SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "formulate the goal".into(),
                admission: bcode_session_models::TurnAdmissionMetadata {
                    execution: execution.clone(),
                    ..bcode_session_models::TurnAdmissionMetadata::default()
                },
            },
        )
        .await
        .unwrap();
    let selection = crate::SessionModelSelection {
        provider_plugin_id: Some("bcode.fake-provider".into()),
        model_id: Some("fake-echo".into()),
        ..crate::SessionModelSelection::default()
    };
    let policy = crate::automatic_compaction_policy(
        state,
        &selection,
        &state.startup_config.model.compaction,
    )
    .await;
    let static_context = crate::StaticModelTurnContext {
        system_prompt: String::new(),
        system_messages: vec![],
        tools: vec![],
        prompt_profile_layers: vec![],
        prompt_profile_diagnostics: vec![],
        tool_description_overrides: vec![],
    };
    let request = crate::build_model_turn_request(
        state,
        session_id,
        &trigger,
        execution,
        0,
        selection.provider_plugin_id.as_deref(),
        selection.model_id.as_deref(),
        None,
        &selection,
        &policy,
        &static_context,
        &bcode_config::BcodeConfig::default(),
    )
    .await
    .unwrap()
    .request;
    let actual = serde_json::to_string(&request.messages).unwrap();
    assert!(actual.contains("Preserve streaming"));
    assert!(actual.contains("formulate the goal"));
    assert!(!actual.contains("later message"));
    assert!(request.tools.is_empty());
}
