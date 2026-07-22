use bcode::{
    AgentEvent, FrontendContentBlock, FrontendEventCursor, FrontendSessionSnapshot,
    FrontendSnapshotApplyOutcome, FrontendSnapshotError, FrontendTurnStatus, MessageRole,
    ModelContentBlock, ModelMessage, ProviderRequestProjection, SessionId, StopReason, TokenUsage,
    ToolCall, ToolResult,
};

fn transcript() -> Vec<ModelMessage> {
    vec![ModelMessage {
        role: MessageRole::Assistant,
        content: vec![
            ModelContentBlock::Text {
                text: "visible".to_string(),
            },
            ModelContentBlock::CachePoint {
                hint: bcode_model::PromptCachePoint::default(),
            },
            ModelContentBlock::ProviderExtension {
                value: serde_json::json!({"provider_secret_state": true}),
            },
        ],
    }]
}

#[test]
fn transcript_projection_omits_provider_and_cache_internals() {
    let session_id = SessionId::new();
    let snapshot = FrontendSessionSnapshot::new(session_id, &transcript());

    assert_eq!(snapshot.transcript.len(), 1);
    assert_eq!(
        snapshot.transcript[0].content,
        [FrontendContentBlock::Text {
            text: "visible".to_string()
        }]
    );
    let encoded = serde_json::to_string(&snapshot).expect("snapshot JSON");
    assert!(!encoded.contains("provider_secret_state"));
    assert!(!encoded.contains("cache_point"));
    assert!(!encoded.contains("bcode_tui"));
}

#[test]
fn event_projection_omits_provider_metadata_without_consuming_sequence() {
    let session_id = SessionId::new();
    let mut cursor = FrontendEventCursor::new(session_id, "turn-1", 0);

    assert!(
        cursor
            .project(&AgentEvent::ProviderMetadata {
                key: "provider.response_id".to_string(),
                value: "secret-provider-id".to_string(),
            })
            .is_none()
    );
    assert!(
        cursor
            .project(&AgentEvent::RequestProjection(
                ProviderRequestProjection::default(),
            ))
            .is_none()
    );
    let started = cursor
        .project(&AgentEvent::TurnStarted)
        .expect("normal event projects");
    assert_eq!(started.sequence, 0);
    assert_eq!(cursor.next_sequence(), 1);
    let encoded = serde_json::to_string(&started).expect("event JSON");
    assert!(!encoded.contains("provider.response_id"));
    assert!(!encoded.contains("secret-provider-id"));
}

#[test]
fn snapshot_materializes_complete_neutral_turn_and_round_trips() {
    let session_id = SessionId::new();
    let mut cursor = FrontendEventCursor::new(session_id, "turn-1", 0);
    let mut snapshot = FrontendSessionSnapshot::new(session_id, &[]);
    let call = ToolCall {
        id: "call-1".to_string(),
        name: "lookup".to_string(),
        arguments: serde_json::json!({"query": "rust"}),
    };
    let result = ToolResult {
        call_id: "call-1".to_string(),
        output: "result".to_string(),
        is_error: false,
        content: Vec::new(),
    };
    let usage = TokenUsage {
        input_tokens: Some(10),
        output_tokens: Some(2),
        total_tokens: Some(12),
        ..TokenUsage::default()
    };
    let events = [
        AgentEvent::TurnStarted,
        AgentEvent::TextDelta("hello".to_string()),
        AgentEvent::ReasoningDelta("thinking".to_string()),
        AgentEvent::ToolCallFinished(call.clone()),
        AgentEvent::ToolResult(result.clone()),
        AgentEvent::Usage(usage.clone()),
        AgentEvent::ExactRequestInputTokens(bcode_model::ExactRequestInputTokens::new(11)),
        AgentEvent::Warning("warning".to_string()),
        AgentEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Some(usage.clone()),
            latency_ms: 42,
        },
    ];
    for event in &events {
        let envelope = cursor.project(event).expect("event projects");
        assert_eq!(
            snapshot.apply_event(&envelope).expect("event applies"),
            FrontendSnapshotApplyOutcome::Applied
        );
    }

    let turn = snapshot.active_turn.as_ref().expect("active turn snapshot");
    assert_eq!(turn.status, FrontendTurnStatus::Completed);
    assert_eq!(turn.text, "hello");
    assert_eq!(turn.reasoning, "thinking");
    assert_eq!(turn.tool_calls, [call]);
    assert_eq!(turn.tool_results, [result]);
    assert_eq!(turn.usage, Some(usage));
    assert_eq!(turn.exact_request_input_tokens, Some(11));
    assert_eq!(turn.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(turn.latency_ms, Some(42));
    assert_eq!(turn.warnings, ["warning"]);

    let encoded = serde_json::to_vec(&snapshot).expect("snapshot serializes");
    let decoded: FrontendSessionSnapshot =
        serde_json::from_slice(&encoded).expect("snapshot deserializes");
    assert_eq!(decoded, snapshot);
}

#[test]
fn snapshot_application_is_idempotent_and_rejects_gaps_conflicts_and_mixed_turns() {
    let session_id = SessionId::new();
    let mut cursor = FrontendEventCursor::new(session_id, "turn-1", 0);
    let first = cursor
        .project(&AgentEvent::TurnStarted)
        .expect("start event");
    let mut snapshot = FrontendSessionSnapshot::new(session_id, &[]);
    assert_eq!(
        snapshot.apply_event(&first).expect("first applies"),
        FrontendSnapshotApplyOutcome::Applied
    );
    assert_eq!(
        snapshot.apply_event(&first).expect("duplicate accepted"),
        FrontendSnapshotApplyOutcome::Duplicate
    );

    let mut conflict = first.clone();
    conflict.event = bcode::FrontendEvent::Warning("different".to_string());
    assert!(matches!(
        snapshot.apply_event(&conflict),
        Err(FrontendSnapshotError::ConflictingDuplicate { sequence: 0 })
    ));

    let mut gap = cursor
        .project(&AgentEvent::TextDelta("gap".to_string()))
        .expect("delta");
    gap.sequence = 2;
    assert!(matches!(
        snapshot.apply_event(&gap),
        Err(FrontendSnapshotError::SequenceGap {
            expected: 1,
            actual: 2
        })
    ));

    let mut mixed = cursor
        .project(&AgentEvent::TextDelta("mixed".to_string()))
        .expect("mixed delta");
    mixed.turn_id = "turn-2".to_string();
    mixed.sequence = 1;
    assert!(matches!(
        snapshot.apply_event(&mixed),
        Err(FrontendSnapshotError::TurnMismatch { .. })
    ));
}

#[test]
fn agent_session_snapshot_uses_visible_transcript_and_no_tui_types() {
    let session = bcode::Agent::builder()
        .build()
        .session_from_messages(transcript());
    let snapshot = session.frontend_snapshot();
    assert_eq!(
        snapshot.schema_version,
        bcode::FRONTEND_CONTRACT_SCHEMA_VERSION
    );
    assert_eq!(snapshot.transcript.len(), 1);
}
