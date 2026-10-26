//! Exercise real host trace persistence through the shared activity consumer.
use super::*;
use bcode_session_view::compaction_activity::{
    CompactionActivityUpdate, compaction_activity_update,
};
use bcode_session_view_models::SessionRuntimeViewState;

fn trace_test_state() -> Arc<ServerState> {
    let mut state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    state.observability = bcode_config::ObservabilityConfig {
        level: bcode_config::ObservabilityLevel::Debug,
        ..Default::default()
    };
    Arc::new(state)
}

#[tokio::test]
async fn producer_attributes_automatic_progress_and_never_rebinds_delayed_or_manual_events() {
    let state = trace_test_state();
    let session = state
        .sessions
        .create_session(None, std::path::PathBuf::from("/"))
        .await
        .unwrap();
    let handle = session_runtime_handle(&state, session.id).await;
    let active = RuntimeCurrentTurn {
        interaction_route: None,
        kind: RuntimeOperationKind::ModelTurn,
        client_id: ClientId::new(),
        turn_id: "actual-owner".into(),
        cancel_state: Arc::new(TurnCancelState::default()),
        model: None,
        agent_profile: None,
        execution: Some(TurnExecutionOptions::default()),
        selection: Some(SessionModelSelection::default()),
    };
    *handle.current_turn.lock().await = Some(active.clone());
    let owner = compaction_trace_owner(&state, session.id).await;
    COMPACTION_TRACE_OWNER
        .scope((session.id, owner), async {
            append_context_compaction_trace(
                &state,
                session.id,
                CompactionTraceKind::Started,
                "compaction_started",
                0,
                false,
                Some("older context".into()),
            )
            .await;
            let mut replacement = active.clone();
            replacement.turn_id = "replacement".into();
            *handle.current_turn.lock().await = Some(replacement);
            append_context_compaction_trace(
                &state,
                session.id,
                CompactionTraceKind::Diagnostic,
                "summary_chunk_accepted",
                0,
                false,
                Some("done=1 total=3 resume_offset=0".into()),
            )
            .await;
            append_context_compaction_trace(
                &state,
                session.id,
                CompactionTraceKind::Finished,
                "compaction_finished",
                0,
                true,
                Some("completed".into()),
            )
            .await;
        })
        .await;
    let mut manual = active;
    manual.kind = RuntimeOperationKind::ManualCompaction;
    manual.turn_id = "manual-operation".into();
    *handle.current_turn.lock().await = Some(manual);
    append_context_compaction_trace(
        &state,
        session.id,
        CompactionTraceKind::Started,
        "compaction_started",
        0,
        false,
        Some("manual older context".into()),
    )
    .await;
    let events = state.sessions.session_history(session.id).await.unwrap();
    drop(handle);
    drop(state);
    assert_progress_consumption(&events);
}

fn assert_progress_consumption(events: &[bcode_session_models::SessionEvent]) {
    let traces: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::TraceEvent { trace }
                if matches!(trace.payload, SessionTracePayload::ContextCompaction { .. }) =>
            {
                Some(trace.as_ref())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        traces.len(),
        4,
        "real producer must persist standard compaction traces"
    );
    let runtime = SessionRuntimeViewState {
        active_turn_id: Some("actual-owner".into()),
        ..Default::default()
    };
    assert!(matches!(
        compaction_activity_update(traces[0], &runtime, false),
        Some(CompactionActivityUpdate::Active { .. })
    ));
    assert!(
        matches!(compaction_activity_update(traces[1], &runtime, true), Some(CompactionActivityUpdate::Active { detail }) if detail.contains("done=1"))
    );
    assert_eq!(
        compaction_activity_update(traces[2], &runtime, true),
        Some(CompactionActivityUpdate::PreparingModelRequest)
    );
    let replacement = SessionRuntimeViewState {
        active_turn_id: Some("replacement".into()),
        ..Default::default()
    };
    for trace in &traces[..3] {
        assert_eq!(trace.turn_id.as_deref(), Some("actual-owner"));
        assert!(compaction_activity_update(trace, &replacement, true).is_none());
    }
    assert!(
        traces[3].turn_id.is_none(),
        "manual operation retains session-only diagnostics"
    );
    assert!(compaction_activity_update(traces[3], &replacement, false).is_none());
}
