//! Offline behavioral coverage for durable managed external evidence.
//! These tests do not invoke plugins, native binaries, providers, or auth flows.

use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessAuthBinding, HarnessAuthCustody, HarnessCompleteness,
    HarnessDeliveryState, HarnessInstance, HarnessObservation, HarnessObservationKind,
    HarnessPlacement, HarnessReceipt, HarnessSessionBinding, HarnessSubmission,
};
use bcode_session::SessionManager;
use bcode_session_models::{
    SessionId,
    external_backend::{
        ExternalBackendEnvelope, ExternalBackendQuery, ExternalBackendRecord as Record,
        external_backend_key,
    },
};

fn binding(session: SessionId, generation: u64) -> Record {
    Record::Binding {
        instance: HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "offline-fixture".into(),
            executable: "/not/invoked".into(),
            config_namespace: "fixture".into(),
            auth: HarnessAuthBinding {
                provider_id: "fixture".into(),
                profile: "native".into(),
                credential_namespace: "fixture-account".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        binding: HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: session.to_string(),
            instance_id: "instance".into(),
            native_session_id: "native-session".into(),
            native_version: "fixture-v1".into(),
            generation,
        },
    }
}
fn observation(session: SessionId, id: &str, generation: u64) -> Record {
    Record::Observation {
        observation: HarnessObservation {
            observation_id: id.into(),
            session_id: session.to_string(),
            generation,
            native_correlations: vec!["native-turn".into()],
            completeness: HarnessCompleteness::Complete,
            kind: HarnessObservationKind::Text {
                segment_id: None,
                text: id.into(),
            },
        },
    }
}
fn query(limit: usize) -> ExternalBackendQuery {
    ExternalBackendQuery {
        after_sequence: None,
        limit,
        max_bytes: 1024 * 1024,
        correlation_key: None,
    }
}
fn submission(session: SessionId) -> Record {
    Record::Submission {
        submission: HarnessSubmission {
            submission_id: "input".into(),
            attempt_id: "attempt".into(),
            session_id: session.to_string(),
            generation: 1,
            text: "instruction".into(),
            placement: HarnessPlacement::NextTurn,
        },
    }
}
fn receipt(session: SessionId, generation: u64, state: HarnessDeliveryState) -> Record {
    Record::Receipt {
        receipt: HarnessReceipt {
            submission_id: "input".into(),
            attempt_id: "attempt".into(),
            session_id: session.to_string(),
            generation,
            state,
            native_request_id: None,
        },
    }
}

#[tokio::test]
async fn canonical_external_history_is_bounded_deduplicated_and_plugin_independent() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let session = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let first = manager
        .append_external_backend_record(session.id, binding(session.id, 1))
        .await
        .unwrap();
    assert_eq!(
        manager
            .append_external_backend_record(session.id, binding(session.id, 1))
            .await
            .unwrap(),
        first
    );
    for index in 0..5 {
        manager
            .append_external_backend_record(
                session.id,
                observation(session.id, &format!("observation-{index}"), 1),
            )
            .await
            .unwrap();
    }
    let mut cursor = query(2);
    let mut events = Vec::new();
    loop {
        let page = manager
            .session_external_backend_page(session.id, cursor.clone())
            .await
            .unwrap();
        assert!(page.events.len() <= 2);
        cursor.after_sequence = page.next_after_sequence;
        events.extend(page.events);
        if !page.has_more {
            break;
        }
    }
    assert_eq!(events.len(), 6);
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    let mut native = query(128);
    native.correlation_key = Some(external_backend_key("native", "native-turn"));
    assert_eq!(
        manager
            .session_external_backend_page(session.id, native)
            .await
            .unwrap()
            .events
            .len(),
        5
    );
    let mut tiny = query(1);
    tiny.max_bytes = 1;
    assert!(
        manager
            .session_external_backend_page(session.id, tiny)
            .await
            .is_err()
    );
    let mut conflicting = observation(session.id, "observation-0", 1);
    if let Record::Observation { observation } = &mut conflicting {
        observation.kind = HarnessObservationKind::Result {
            text: "different".into(),
        };
    }
    assert!(
        manager
            .append_external_backend_record(session.id, conflicting)
            .await
            .is_err()
    );
    manager
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    // A new manager has no plugin runtime and no retained native process.
    let reopened = SessionManager::persistent(root.path()).unwrap();
    assert_eq!(
        reopened
            .session_external_backend_binding(session.id)
            .await
            .unwrap(),
        Some(first)
    );
    assert_eq!(
        reopened
            .session_external_backend_page(session.id, query(128))
            .await
            .unwrap()
            .events
            .len(),
        6
    );
}

#[tokio::test]
async fn lost_ack_is_retained_without_replaying_delivery_and_generation_is_fenced() {
    use HarnessDeliveryState as D;
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let session = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    manager
        .append_external_backend_record(session.id, binding(session.id, 1))
        .await
        .unwrap();
    manager
        .append_external_backend_record(session.id, submission(session.id))
        .await
        .unwrap();
    manager
        .append_external_backend_record(session.id, receipt(session.id, 1, D::Prepared))
        .await
        .unwrap();
    let attempted = manager
        .append_external_backend_record(session.id, receipt(session.id, 1, D::DeliveryAttempted))
        .await
        .unwrap();
    assert_eq!(
        manager
            .append_external_backend_record(
                session.id,
                receipt(session.id, 1, D::DeliveryAttempted)
            )
            .await
            .unwrap(),
        attempted
    );
    manager
        .append_external_backend_record(session.id, receipt(session.id, 1, D::Uncertain))
        .await
        .unwrap();
    assert!(
        manager
            .append_external_backend_record(session.id, receipt(session.id, 1, D::Prepared))
            .await
            .is_ok()
    ); // exact old evidence only
    let mut altered = receipt(session.id, 1, D::DeliveryAttempted);
    if let Record::Receipt { receipt } = &mut altered {
        receipt.attempt_id = "second-attempt".into();
    }
    assert!(
        manager
            .append_external_backend_record(session.id, altered)
            .await
            .is_err()
    );
    manager
        .append_external_backend_record(session.id, binding(session.id, 2))
        .await
        .unwrap();
    assert!(
        manager
            .append_external_backend_record(session.id, observation(session.id, "stale", 1))
            .await
            .is_err()
    );
    assert!(
        manager
            .append_external_backend_record(session.id, binding(session.id, 4))
            .await
            .is_err()
    );
    manager
        .append_external_backend_record(session.id, receipt(session.id, 2, D::Settled))
        .await
        .unwrap();
    assert!(
        manager
            .append_external_backend_record(session.id, receipt(session.id, 2, D::Executing))
            .await
            .is_err()
    );
    let page = manager
        .session_external_backend_page(
            session.id,
            ExternalBackendQuery {
                correlation_key: Some(external_backend_key("submission_history", "input")),
                ..query(128)
            },
        )
        .await
        .unwrap();
    assert_eq!(page.events.len(), 5);
    let final_record = ExternalBackendEnvelope::from_event(page.events.last().unwrap())
        .unwrap()
        .unwrap();
    assert!(
        matches!(final_record.record, Record::Receipt { receipt } if receipt.state == D::Settled)
    );
}

#[tokio::test]
async fn observation_tools_never_become_canonical_tool_invocations() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let session = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    manager
        .append_external_backend_record(session.id, binding(session.id, 1))
        .await
        .unwrap();
    let mut record = observation(session.id, "native-tool", 1);
    if let Record::Observation { observation } = &mut record {
        observation.kind = HarnessObservationKind::Tool {
            input_json: None,
            native_tool_id: "tool".into(),
            name: "shell".into(),
            status: "observed".into(),
        };
    }
    let event = manager
        .append_external_backend_record(session.id, record.clone())
        .await
        .unwrap();
    assert!(matches!(
        event.kind,
        bcode_session_models::SessionEventKind::ExternalObservationRecorded { .. }
    ));
    assert_eq!(
        ExternalBackendEnvelope::from_event(&event)
            .unwrap()
            .unwrap()
            .record,
        record
    );
}

#[tokio::test]
async fn only_observed_outstanding_interactions_can_be_answered_once() {
    use bcode_harness_models::{
        HarnessInteraction, HarnessInteractionKind, HarnessResponse, HarnessResponseValue,
    };
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let session = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    manager
        .append_external_backend_record(session.id, binding(session.id, 1))
        .await
        .unwrap();
    for kind in [
        HarnessInteractionKind::Approval,
        HarnessInteractionKind::Question,
        HarnessInteractionKind::Assistance,
    ] {
        let id = format!("request-{kind:?}");
        let interaction = HarnessInteraction {
            input_preview: None,
            tool_name: None,
            request_id: id.clone(),
            session_id: session.id.to_string(),
            generation: 1,
            kind,
            prompt: "need input".into(),
            scope: "fixture".into(),
            choices: vec![],
        };
        let value = match kind {
            HarnessInteractionKind::Approval => HarnessResponseValue::Approval { approved: true },
            HarnessInteractionKind::Question => HarnessResponseValue::Answer {
                choice_id: None,
                text: "answer".into(),
            },
            HarnessInteractionKind::Assistance => HarnessResponseValue::Assistance {
                text: "help".into(),
            },
        };
        let response = HarnessResponse {
            response_id: id.clone(),
            request_id: id.clone(),
            session_id: session.id.to_string(),
            generation: 1,
            value,
        };
        let record = Record::Response {
            interaction: interaction.clone(),
            response: response.clone(),
        };
        assert!(
            manager
                .append_external_backend_record(session.id, record.clone())
                .await
                .is_err()
        );
        let mut observed = observation(session.id, &id, 1);
        if let Record::Observation { observation } = &mut observed {
            observation.kind = HarnessObservationKind::Interaction {
                interaction: interaction.clone(),
            };
        }
        manager
            .append_external_backend_record(session.id, observed)
            .await
            .unwrap();
        let accepted = manager
            .append_external_backend_record(session.id, record.clone())
            .await
            .unwrap();
        assert_eq!(
            manager
                .append_external_backend_record(session.id, record)
                .await
                .unwrap(),
            accepted
        );
        let mut different = response;
        different.response_id.push_str("-again");
        assert!(
            manager
                .append_external_backend_record(
                    session.id,
                    Record::Response {
                        interaction,
                        response: different
                    }
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn external_children_keep_ordinary_immutable_parentage() {
    use bcode_session_models::SessionChildCreateRequest;
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let parent = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let child = manager
        .create_child_session(SessionChildCreateRequest {
            child_id: SessionId::new(),
            parent_id: parent.id,
            name: Some("managed child".into()),
            working_directory: root.path().into(),
        })
        .await
        .unwrap();
    assert_eq!(child.parent_id, Some(parent.id));
    manager
        .append_external_backend_record(child.id, binding(child.id, 1))
        .await
        .unwrap();
    assert!(
        manager
            .session_external_backend_binding(parent.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        manager
            .append_external_backend_record(parent.id, observation(child.id, "wrong-session", 1))
            .await
            .is_err()
    );
    let history = manager.session_history(child.id).await.unwrap();
    assert!(history.iter().any(|event| matches!(&event.kind,
        bcode_session_models::SessionEventKind::ChildSessionCreated { request, .. } if request.parent_id == parent.id)));
}

#[tokio::test]
async fn result_and_idle_do_not_settle_but_explicit_settlement_is_terminal() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let session = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    manager
        .append_external_backend_record(session.id, binding(session.id, 1))
        .await
        .unwrap();
    let mut result = observation(session.id, "result", 1);
    if let Record::Observation { observation } = &mut result {
        observation.kind = HarnessObservationKind::Result {
            text: "result".into(),
        };
    }
    manager
        .append_external_backend_record(session.id, result)
        .await
        .unwrap();
    manager
        .append_external_backend_record(session.id, observation(session.id, "still-running", 1))
        .await
        .unwrap();
    let mut settlement = observation(session.id, "settled", 1);
    if let Record::Observation { observation } = &mut settlement {
        observation.kind = HarnessObservationKind::Settlement {
            native_turn_id: "native-turn".into(),
        };
        observation.completeness = HarnessCompleteness::Partial;
    }
    assert!(
        manager
            .append_external_backend_record(session.id, settlement.clone())
            .await
            .is_err()
    );
    if let Record::Observation { observation } = &mut settlement {
        observation.completeness = HarnessCompleteness::Complete;
    }
    manager
        .append_external_backend_record(session.id, settlement)
        .await
        .unwrap();
    assert!(
        manager
            .append_external_backend_record(session.id, observation(session.id, "late-running", 1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn missing_locator_index_fails_closed_without_repair() {
    use bcode_session::db::SessionDb;
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let session = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    manager
        .append_external_backend_record(session.id, binding(session.id, 1))
        .await
        .unwrap();
    manager
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    let db = SessionDb::open_existing_turso_in_root(session.id, root.path())
        .await
        .unwrap();
    let tail = db.last_event_sequence().await.unwrap();
    assert!(
        db.model_context_events()
            .await
            .unwrap()
            .iter()
            .all(|event| ExternalBackendEnvelope::from_event(event)
                .unwrap()
                .is_none())
    );
    switchy::database::schema::drop_index("idx_turn_evidence_sequence", "turn_evidence")
        .execute(db.database())
        .await
        .unwrap();
    assert!(db.external_backend_binding().await.is_err());
    assert!(db.external_backend_page(&query(1)).await.is_err());
    assert_eq!(db.last_event_sequence().await.unwrap(), tail);
    assert!(db.external_backend_binding().await.is_err());
}

#[tokio::test]
async fn managed_evidence_rejects_ephemeral_sessions() {
    let manager = SessionManager::default();
    let session = manager
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    assert!(
        manager
            .append_external_backend_record(session.id, binding(session.id, 1))
            .await
            .is_err()
    );
}

fn event(
    session_id: SessionId,
    kind: bcode_session_models::SessionEventKind,
) -> bcode_session_models::SessionEvent {
    bcode_session_models::SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 0,
        timestamp_ms: 0,
        session_id,
        provenance: None,
        kind,
    }
}

#[test]
fn typed_external_variants_round_trip_canonical_codec_without_trace() {
    use bcode_harness_models::{
        HarnessInteraction, HarnessInteractionKind, HarnessResponse, HarnessResponseValue,
    };
    let session = SessionId::new();
    let interaction = HarnessInteraction {
        input_preview: None,
        tool_name: None,
        request_id: "request".into(),
        session_id: session.to_string(),
        generation: 1,
        kind: HarnessInteractionKind::Approval,
        prompt: "approve".into(),
        scope: "fixture".into(),
        choices: vec![],
    };
    let response = HarnessResponse {
        response_id: "response".into(),
        request_id: "request".into(),
        session_id: session.to_string(),
        generation: 1,
        value: HarnessResponseValue::Approval { approved: true },
    };
    for record in [
        binding(session, 1),
        submission(session),
        receipt(session, 1, HarnessDeliveryState::Prepared),
        observation(session, "observed", 1),
        Record::Response {
            interaction,
            response,
        },
    ] {
        let kind = ExternalBackendEnvelope {
            version: 1,
            record: record.clone(),
        }
        .into_event_kind()
        .unwrap();
        assert!(!matches!(
            &kind,
            bcode_session_models::SessionEventKind::TraceEvent { .. }
        ));
        let event = event(session, kind);
        let encoded = bcode_session::persisted::encode_session_event(&event).unwrap();
        let decoded = bcode_session::persisted::decode_session_event(&encoded).unwrap();
        assert_eq!(decoded, event);
        assert_eq!(
            ExternalBackendEnvelope::from_event(&decoded)
                .unwrap()
                .unwrap()
                .record,
            record
        );
    }
}

#[test]
fn diagnostic_provider_event_cannot_supply_external_semantic_authority() {
    use bcode_session_models::{
        SessionEventKind, SessionTraceEvent, SessionTracePayload, SessionTracePhase,
    };
    let session = SessionId::new();
    let diagnostic = event(
        session,
        SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 0,
                turn_id: None,
                phase: SessionTracePhase::ModelProviderEvent,
                payload: SessionTracePayload::ProviderEvent {
                    event_type: "external_backend_v1".into(),
                    detail: Some(
                        serde_json::to_string(&ExternalBackendEnvelope {
                            version: 1,
                            record: binding(session, 1),
                        })
                        .unwrap(),
                    ),
                },
            }),
        },
    );
    assert!(
        ExternalBackendEnvelope::from_event(&diagnostic)
            .unwrap()
            .is_none()
    );
    let mut incompatible = event(
        session,
        ExternalBackendEnvelope {
            version: 1,
            record: binding(session, 1),
        }
        .into_event_kind()
        .unwrap(),
    );
    if let SessionEventKind::ExternalBackendBound { version, .. } = &mut incompatible.kind {
        *version = 2;
    }
    let encoded = bcode_session::persisted::encode_session_event(&incompatible).unwrap();
    assert!(bcode_session::persisted::decode_session_event(&encoded).is_err());
}

#[test]
fn external_observation_capacity_is_admitted_before_source_clone() {
    use bcode_session_models::retention::SessionObservationSourceLimits;
    let session = SessionId::new();
    let mut text = String::with_capacity(1024 * 1024);
    text.push('x');
    let mut record = observation(session, "capacity", 1);
    if let Record::Observation { observation } = &mut record {
        observation.kind = HarnessObservationKind::Text {
            segment_id: None,
            text,
        };
    }
    let kind = ExternalBackendEnvelope { version: 1, record }
        .into_event_kind()
        .unwrap();
    let event = event(session, kind);
    let limits = SessionObservationSourceLimits {
        max_scalar_capacity_bytes: 4096,
        max_collection_entries: 1024,
        max_value_nodes: 1024,
        max_depth: 32,
        max_queued_events: 1,
        max_in_flight_events: 1,
    };
    assert!(bcode_session_models::retention_events::inspect_durable(&event, &limits).is_err());
}

#[test]
fn envelope_compatibility_and_page_bounds_fail_closed() {
    let session = SessionId::new();
    assert!(
        ExternalBackendEnvelope {
            version: 2,
            record: binding(session, 1)
        }
        .into_event_kind()
        .is_err()
    );
    let mut page = query(0);
    assert!(page.validate().is_err());
    page.limit = 129;
    assert!(page.validate().is_err());
    page.limit = 1;
    page.after_sequence = Some(u64::MAX);
    assert!(page.validate().is_err());
    let kind = ExternalBackendEnvelope {
        version: 1,
        record: observation(session, "fixture", 1),
    }
    .into_event_kind()
    .unwrap();
    let mut event = bcode_session_models::SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 0,
        timestamp_ms: 0,
        session_id: SessionId::new(),
        provenance: None,
        kind,
    };
    assert!(ExternalBackendEnvelope::from_event(&event).is_err());
    event.session_id = session;
    assert!(
        ExternalBackendEnvelope::from_event(&event)
            .unwrap()
            .is_some()
    );
}
