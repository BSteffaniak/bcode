//! Behavioral compaction privacy and native compatibility acceptance.
//!
//! Canonical fixtures are injected through the session owner; assertions use the actual
//! application boundary and ordinary turn-request assembler, not source inspection.

use super::*;
use crate::context_compaction::CompactionPlanningPolicy;
use bcode_session_models::{
    CompactionSynthesisCheckpoint, ProjectionWindowAnchor, ProjectionWindowDirection,
    ProjectionWindowLimits, ProjectionWindowRequest, ProjectionWindowTarget, SessionApplication,
    SessionHistoryDirection, SessionHistoryQuery, SessionObservationApplication,
    SessionObservationEvent, SessionObservationRequest, SessionProjectionKind,
};

const CHECKPOINT_CANARY: &str = "PRIVATE-COMPACTION-CARRY-7c5de018";
const OPERATION_CANARY: &str = "PRIVATE-OPERATION-KEY-a0d99126";
const NATIVE_CANARY: &str = "PRIVATE-NATIVE-STATE-3db479ea";
const PUBLIC_SUMMARY: &str = "Portable fact: preserve the read-only database requirement.";

fn assert_checkpoint_private(value: &impl serde::Serialize) {
    let json = serde_json::to_string(value).unwrap();
    assert!(
        !json.contains(CHECKPOINT_CANARY),
        "checkpoint carry crossed boundary"
    );
    assert!(
        !json.contains(OPERATION_CANARY),
        "operation key crossed boundary"
    );
}

fn window(projection: SessionProjectionKind) -> ProjectionWindowRequest {
    ProjectionWindowRequest {
        projection,
        anchor: ProjectionWindowAnchor::Latest,
        direction: ProjectionWindowDirection::Backward,
        target: ProjectionWindowTarget::default(),
        limits: ProjectionWindowLimits {
            max_items: 32,
            max_events_scanned: 64,
            max_bytes: 32 * 1024,
        },
    }
}

async fn checkpoint_fixture() -> (tempfile::TempDir, Arc<ServerState>, SessionId) {
    let directory = tempfile::tempdir().unwrap();
    let sessions = SessionManager::persistent(directory.path()).unwrap();
    let session = sessions
        .create_session(None, directory.path().to_path_buf())
        .await
        .unwrap();
    sessions
        .append_compaction_checkpoint(
            session.id,
            OPERATION_CANARY,
            Some(serde_json::json!({"private_carry": CHECKPOINT_CANARY}).to_string()),
        )
        .await
        .unwrap();
    let canonical = sessions
        .session_compaction_checkpoint(session.id, OPERATION_CANARY)
        .await
        .unwrap()
        .unwrap();
    let checkpoint = CompactionSynthesisCheckpoint::from_event(&canonical)
        .unwrap()
        .unwrap();
    assert!(checkpoint.host_payload.unwrap().contains(CHECKPOINT_CANARY));
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
    (directory, state, session.id)
}

#[tokio::test]
async fn semantic_observation_excludes_private_carry_while_source_pages_remain_canonical() {
    let (_directory, state, id) = checkpoint_fixture().await;
    let mut observation = {
        let application =
            session_application::SessionApplicationHandle::new(&state, ClientId::new());
        // The current projection-history application contract implements Transcript;
        // AuditLog is not supported by the owning session projection builder.
        let page = application
            .session_projection_history_page(id, window(SessionProjectionKind::Transcript))
            .await
            .unwrap();
        let canonical_page = state
            .sessions
            .session_projection_history_page(id, window(SessionProjectionKind::Transcript))
            .await
            .unwrap();
        assert_eq!(page, canonical_page);
        // ProjectionHistoryPage promises exact canonical source events, not a
        // frontend snapshot. Its checkpoint envelope must remain authentic.
        for event in &page.events {
            if let Some(checkpoint) = CompactionSynthesisCheckpoint::from_event(event).unwrap() {
                assert_eq!(checkpoint.operation_key, OPERATION_CANARY);
                assert!(checkpoint.host_payload.unwrap().contains(CHECKPOINT_CANARY));
            }
        }
        application
            .observe_session(SessionObservationRequest {
                schema_version: bcode_session_models::SESSION_OBSERVATION_SCHEMA_VERSION,
                session_id: id,
                window: window(SessionProjectionKind::Transcript),
                max_updates: 128,
                max_snapshot_bytes: 128 * 1024,
            })
            .await
            .unwrap()
    };
    let event = tokio::time::timeout(Duration::from_secs(5), observation.recv())
        .await
        .unwrap()
        .unwrap();
    match event.event {
        SessionObservationEvent::Snapshot { snapshot, initial } => {
            assert!(initial);
            assert_checkpoint_private(&snapshot);
        }
        SessionObservationEvent::Ended { .. } => panic!("expected initial semantic snapshot"),
    }
    let live = state
        .sessions
        .append_compaction_checkpoint(
            id,
            OPERATION_CANARY,
            Some(format!("{CHECKPOINT_CANARY}-live")),
        )
        .await
        .unwrap();
    let delivery = tokio::time::timeout(Duration::from_secs(5), observation.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.source_sequence, Some(live.sequence));
    match delivery.event {
        SessionObservationEvent::Snapshot { snapshot, initial } => {
            assert!(!initial);
            assert_checkpoint_private(&snapshot);
        }
        SessionObservationEvent::Ended { .. } => panic!("expected live semantic snapshot"),
    }
    let canonical = state
        .sessions
        .session_compaction_checkpoint(id, OPERATION_CANARY)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canonical, live);
    assert!(
        serde_json::to_string(&canonical)
            .unwrap()
            .contains(CHECKPOINT_CANARY)
    );
}

#[tokio::test]
async fn canonical_history_and_explicit_export_preserve_operational_evidence() {
    let (_directory, state, id) = checkpoint_fixture().await;
    let caller = ClientId::new();
    let page = session_application::SessionApplicationHandle::new(&state, caller)
        .session_history_page(
            id,
            SessionHistoryQuery {
                cursor: None,
                limit: 32,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
        .unwrap();
    // This application API explicitly promises canonical history, unlike portable
    // projection/observation. Do not silently turn exact evidence into a redacted log.
    let canonical = state
        .sessions
        .session_compaction_checkpoint(id, OPERATION_CANARY)
        .await
        .unwrap()
        .unwrap();
    assert!(page.events.contains(&canonical));
    let exported = session_application::SessionApplicationHandle::new(&state, caller)
        .export_session_history(id)
        .await
        .unwrap();
    assert!(exported.contains(&canonical));
    assert!(
        serde_json::to_string(&page)
            .unwrap()
            .contains(CHECKPOINT_CANARY)
    );
}

#[tokio::test]
async fn ordinary_assembled_request_excludes_canonical_compaction_checkpoint() {
    let (_directory, state, id) = checkpoint_fixture().await;
    let trigger = state
        .sessions
        .append_event(
            id,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "Continue with the public task".into(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        )
        .await
        .unwrap();
    let selection = SessionModelSelection {
        provider_plugin_id: Some("bcode.fake-provider".into()),
        requested_model_id: Some("fake-echo".into()),
        model_id: Some("fake-echo".into()),
        ..SessionModelSelection::default()
    };
    let policy =
        automatic_compaction_policy(&state, &selection, &state.startup_config.model.compaction)
            .await;
    let context = StaticModelTurnContext {
        optional_context_owner: None,
        tool_catalog: None,
        prefix_context: None,
        recovery_context: None,
        system_prompt: "Public instructions".into(),
        system_messages: Vec::new(),
        tools: Vec::new(),
        prompt_profile_layers: Vec::new(),
        prompt_profile_diagnostics: Vec::new(),
        tool_description_overrides: Vec::new(),
    };
    let prepared = build_model_turn_request(
        &state,
        id,
        &trigger,
        &bcode_session_models::TurnExecutionOptions::default(),
        0,
        selection.provider_plugin_id.as_deref(),
        selection.model_id.as_deref(),
        None,
        &selection,
        &policy,
        &context,
        &bcode_config::BcodeConfig::default(),
    )
    .await
    .unwrap();
    assert_checkpoint_private(&prepared.request);
    let json = serde_json::to_string(&prepared.request).unwrap();
    assert!(json.contains("Continue with the public task"));
    assert!(!json.contains(bcode_session_models::COMPACTION_SYNTHESIS_CHECKPOINT_EVENT));
    // Dispatch the exact assembled request through the real plugin service contract.
    // The bundled echo provider is offline and needs no credentials.
    let started: bcode_model::StartTurnResponse = state
        .plugins
        .invoke_service_json(
            "bcode.fake-provider",
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_START_TURN,
            &prepared.request,
        )
        .await
        .unwrap();
    let events: bcode_model::PollTurnEventsResponse = state
        .plugins
        .invoke_service_json(
            "bcode.fake-provider",
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_POLL_TURN_EVENTS,
            &bcode_model::PollTurnEventsRequest {
                provider_turn_id: started.provider_turn_id.clone(),
            },
        )
        .await
        .unwrap();
    let acknowledgement: bcode_model::AckResponse = state
        .plugins
        .invoke_service_json(
            "bcode.fake-provider",
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_FINISH_TURN,
            &bcode_model::FinishTurnRequest {
                provider_turn_id: started.provider_turn_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(acknowledgement, bcode_model::AckResponse::default());
    assert_checkpoint_private(&events);
    assert!(!events.events.is_empty());
}

#[tokio::test]
async fn assembled_synthesis_source_excludes_operational_checkpoint() {
    let (_directory, state, id) = checkpoint_fixture().await;
    for kind in [
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "Preserve the read-only requirement".into(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        },
        SessionEventKind::AssistantMessage {
            text: "I inspected the schema without changing it".into(),
        },
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "Now inspect the parser".into(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        },
    ] {
        state.sessions.append_event(id, kind).await.unwrap();
    }
    let history = state
        .sessions
        .activated_conversation_snapshot(id)
        .await
        .unwrap()
        .events;
    // Canonical evidence remains present even if the model-context projection
    // already filters operational records before the planner sees them.
    let canonical = state
        .sessions
        .session_compaction_checkpoint(id, OPERATION_CANARY)
        .await
        .unwrap()
        .unwrap();
    assert!(
        serde_json::to_string(&canonical)
            .unwrap()
            .contains(CHECKPOINT_CANARY)
    );
    let plan = context_compaction::structural_compaction_plan_with_policy(
        &history,
        2000,
        CompactionPlanningPolicy::OverflowRecovery,
    )
    .unwrap();
    let transcript = context_compaction::compaction_transcript_from_plan(&plan);
    let prompt = context_compaction::compaction_prompt_text(&transcript);
    let request = context_compaction::build_compaction_request(
        id,
        "fake-echo".into(),
        bcode_model::ProviderRequestContext::default(),
        &prompt,
        "privacy-synthesis".into(),
    );
    assert_checkpoint_private(&request);
    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains("Preserve the read-only requirement"));
    assert!(!json.contains(bcode_session_models::COMPACTION_SYNTHESIS_CHECKPOINT_EVENT));
}

#[test]
fn incompatible_native_snapshot_uses_only_explicit_portable_summary() {
    let snapshot = bcode_session_models::ProviderContextSnapshot {
        format_version: 1,
        request_fingerprint: None,
        request_id: None,
        provider_plugin_id: "provider".into(),
        model_id: "model".into(),
        compatibility_key: "prompt-and-packing-v1".into(),
        auth_profile: Some("profile".into()),
        origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
        messages_json: serde_json::to_string(&vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: NATIVE_CANARY.into(),
            }],
        }])
        .unwrap(),
        portable_summary: PUBLIC_SUMMARY.into(),
    };
    let history = vec![bcode_session_models::SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: SessionId::new(),
        timestamp_ms: 0,
        provenance: None,
        sequence: 2,
        kind: SessionEventKind::ProviderContextCompacted {
            snapshot,
            compacted_through_sequence: 1,
        },
    }];
    // The positive control proves that this fixture can actually reuse native state.
    let compatible = session_events_to_model_messages_for_target(
        &history,
        2000,
        Some("provider"),
        Some("model"),
        Some("profile"),
        Some(1),
        Some("prompt-and-packing-v1"),
    );
    assert!(
        serde_json::to_string(&compatible)
            .unwrap()
            .contains(NATIVE_CANARY)
    );
    for (provider, model, auth, version, policy) in [
        (
            "changed-provider",
            "model",
            Some("profile"),
            1,
            "prompt-and-packing-v1",
        ),
        (
            "provider",
            "changed-model",
            Some("profile"),
            1,
            "prompt-and-packing-v1",
        ),
        (
            "provider",
            "model",
            Some("changed-profile"),
            1,
            "prompt-and-packing-v1",
        ),
        ("provider", "model", None, 1, "prompt-and-packing-v1"),
        (
            "provider",
            "model",
            Some("profile"),
            2,
            "prompt-and-packing-v1",
        ),
        ("provider", "model", Some("profile"), 1, "changed-prompt"),
        ("provider", "model", Some("profile"), 1, "changed-packing"),
    ] {
        let messages = session_events_to_model_messages_for_target(
            &history,
            2000,
            Some(provider),
            Some(model),
            auth,
            Some(version),
            Some(policy),
        );
        let json = serde_json::to_string(&messages).unwrap();
        assert!(
            !json.contains(NATIVE_CANARY),
            "incompatible native state reused"
        );
        assert!(json.contains(PUBLIC_SUMMARY), "explicit fallback was lost");
    }
}
