//! Restored overflow diagnostics preserve only normalized bounded trigger fields.
use super::*;

#[tokio::test]
async fn resumed_synthesis_failure_retains_normalized_overflow_without_private_metadata() {
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    let session = state
        .sessions
        .create_session(None, std::env::temp_dir())
        .await
        .unwrap();
    let id = session.id;
    drop(session);
    let error: bcode_model::ProviderError = serde_json::from_value(serde_json::json!({
        "code": "context_length", "category": "context_length", "message": "normalized input overflow",
        "provider_message": "PRIVATE_PROVIDER_MESSAGE", "request_id": "PRIVATE_REQUEST_ID",
        "diagnostic_context": {"private": "PRIVATE_DIAGNOSTIC"}
    })).unwrap();
    let recovery = ModelTurnRecoveryState::default();
    let rounds = ModelTurnRoundState::new(None);
    let mut checkpoint =
        provider_retry_recovery::compaction_checkpoint(1, (&recovery, &rounds, 0, 0)).unwrap();
    checkpoint.overflow_trigger = Some(OverflowTrigger::capture(&error));
    checkpoint.compaction_plan = Some(Plan {
        key: "changed-source-policy".into(),
        boundary: 1,
        first_kept_sequence: None,
        planning_policy: context_compaction::CompactionPlanningPolicy::OverflowRecovery,
        progress_requirement: None,
    });
    provider_retry_recovery::persist(&state, id, "overflow", Some(&checkpoint))
        .await
        .unwrap();
    let restored = provider_retry_recovery::load(&state, id, "overflow")
        .await
        .unwrap()
        .unwrap();
    let encoded = serde_json::to_value(restored.overflow_trigger.unwrap()).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({"code":"context_length", "message":"normalized input overflow"})
    );
    let (_followup_tx, mut followup_rx) = mpsc::channel(1);
    let (_steering_tx, mut steering_rx) = mpsc::channel(1);
    let (_cancel_tx, mut cancel_rx) = mpsc::channel(1);
    let queued = AtomicUsize::new(0);
    let mut commands = RuntimeCommandContext::new(
        &mut followup_rx,
        &mut steering_rx,
        &mut cancel_rx,
        &queued,
        Arc::new(Mutex::new(None)),
    );
    let failure = Box::pin(resume(
        &state,
        id,
        "overflow",
        &SessionModelSelection::default(),
        &mut commands,
        &TurnCancelState::default(),
    ))
    .await
    .unwrap_err()
    .to_string();
    assert!(failure.contains("context_length"));
    assert!(failure.contains("normalized input overflow"));
    assert!(failure.contains("compaction recovery failed"));
    assert!(!failure.contains("PRIVATE_"));
    drop(state);
}

#[test]
fn overflow_trigger_limits_normalized_fields_without_splitting_unicode() {
    let error: bcode_model::ProviderError = serde_json::from_value(serde_json::json!({
        "code": "界".repeat(200), "category": "context_length", "message": "界".repeat(2000)
    }))
    .unwrap();
    let trigger = OverflowTrigger::capture(&error);
    assert!(trigger.valid());
    assert_eq!(trigger.code.chars().count(), 128);
    assert_eq!(trigger.message.chars().count(), 1024);
}
