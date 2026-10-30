//! Transactional storage foundation tests. Publication remains disabled; fixture appends
//! exercise the same validators/projectors in explicit isolated storage transactions.
use super::*;
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, DURABLE_INTERACTION_VERSION,
    DurableInteractionKey, DurableInteractionPayload, ModelTurnOutcome, PromptSubmission,
    SubmissionConfigSource, SubmissionConfigSources, SubmissionPlacement, SubmissionRecoveryRecipe,
    SubmissionTarget, ToolExchangeRequest, ToolExchangeResolution, ToolExchangeResolutionEvent,
    ToolExchangeResponsePolicy, ToolInvocationResultRecord, TurnAdmissionMetadata,
    TurnContinuationHostState, TurnContinuationMember, TurnId, TurnOutputPosition,
};
use switchy::schema::runner::MigrationRunner;

fn source(sequence: u64, kind: &str) -> TurnContinuationSourceRef {
    TurnContinuationSourceRef {
        sequence,
        kind: kind.into(),
        digest: "a".repeat(64),
    }
}
fn prepare(session: SessionId, count: usize) -> TurnContinuationOperation {
    let key = TurnContinuationKey {
        session_id: session,
        turn_id: "turn".into(),
        continuation_id: "continuation".into(),
    };
    TurnContinuationOperation {
        version: TURN_CONTINUATION_VERSION,
        key: key.clone(),
        operation_id: "prepare".into(),
        request_digest: "b".repeat(64),
        expected_revision: None,
        transition: TurnContinuationTransition::Prepare {
            capture: Box::new(DurableTurnContinuationV1 {
                version: TURN_CONTINUATION_VERSION,
                key,
                submission_id: "submission".into(),
                model_work_id: "model_turn".into(),
                batch_id: "batch".into(),
                accepted: source(1, "submission_accepted"),
                activated: source(2, "submission_activated"),
                descriptor_source: source(1, "submission_accepted"),
                descriptor_digest: "c".repeat(64),
                selected_invocation_id: "call0".into(),
                selected_exchange_id: "exchange".into(),
                host_state: TurnContinuationHostState {
                    schema: "host.loop".into(),
                    version: 1,
                    json: "{}".into(),
                },
                members: (0..count)
                    .map(|index| TurnContinuationMember {
                        invocation_id: format!("call{index}"),
                        request: source(
                            3 + u64::try_from(index).unwrap(),
                            "positioned_tool_call_requested",
                        ),
                    })
                    .collect(),
            }),
        },
    }
}
fn event(session: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: session,
        sequence,
        timestamp_ms: 1,
        provenance: None,
        kind,
    }
}
fn operation_event(operation: &TurnContinuationOperation, sequence: u64) -> SessionEvent {
    event(
        operation.key.session_id,
        sequence,
        SessionEventKind::TurnContinuationCommitted {
            operation: Box::new(operation.clone()),
        },
    )
}
fn next(
    record: &TurnContinuationRecord,
    transition: TurnContinuationTransition,
) -> TurnContinuationOperation {
    TurnContinuationOperation {
        version: TURN_CONTINUATION_VERSION,
        key: record.capture.key.clone(),
        operation_id: format!("operation{}", record.receipt.revision + 1),
        request_digest: "d".repeat(64),
        expected_revision: Some(record.receipt.revision),
        transition,
    }
}
fn result(invocation: &str) -> ToolInvocationResultRecord {
    ToolInvocationResultRecord {
        invocation_id: invocation.into(),
        model_output: "answer".into(),
        is_error: false,
        content: vec![],
        presentation: None,
        result: None,
    }
}
fn descriptor() -> SubmissionExecutionDescriptor {
    SubmissionExecutionDescriptor {
        version: 2,
        provider: "provider".into(),
        model: "model".into(),
        reasoning: None,
        model_profile: None,
        auth_profile: None,
        config_identity: "config".into(),
        recovery_recipe: Some(Box::new(SubmissionRecoveryRecipe {
            version: 1,
            model_profile: None,
            auth_profile: None,
            auth_pool: None,
            config_source: SubmissionConfigSource {
                schema_version: 1,
                execution_digest: "e".repeat(64),
                source_presence: vec![true],
                sources: SubmissionConfigSources {
                    cwd: "/workspace".into(),
                    discovered_paths: vec![],
                    exclusive_config_path: Some("/config.toml".into()),
                    base_config_path: None,
                    env_config_path: None,
                    cli_config_path: None,
                    active_context: None,
                    model_profile: None,
                    inline_overrides: false,
                },
            },
        })),
    }
}
async fn checkpoint_at(db: &dyn Database, sequence: u64) {
    db.upsert("projection_checkpoints")
        .unique(&["projection_name"])
        .value("projection_name", "turn_continuations")
        .value("projection_version", 1_i64)
        .value(
            "last_event_seq",
            crate::db_event_store::seq_to_value(sequence),
        )
        .value("updated_at_ms", 1_i64)
        .execute(db)
        .await
        .unwrap();
}
async fn database(path: &std::path::Path) -> Box<dyn Database> {
    let db = crate::db_connection::init_turso_local_with_retry(path)
        .await
        .unwrap();
    MigrationRunner::new(Box::new(crate::current_schema::session_migrations()))
        .with_table_name("continuation_fixture_base")
        .run(&*db)
        .await
        .unwrap();
    let mut source = crate::current_schema::CurrentMigrationSource::default();
    crate::current_schema::add_durable_interaction_migrations(&mut source);
    MigrationRunner::new(Box::new(source))
        .with_table_name("continuation_fixture_current")
        .run(&*db)
        .await
        .unwrap();
    db
}

async fn interaction_ready(db: &dyn Database, record: &TurnContinuationRecord) {
    let session = record.capture.key.session_id;
    for projection in ["interaction_exchange_locators", "durable_interactions"] {
        db.update("projection_checkpoints")
            .value("last_event_seq", 6_i64)
            .where_eq("projection_name", projection)
            .execute(db)
            .await
            .unwrap();
    }
    let exchange = event(
        session,
        7,
        SessionEventKind::ToolExchangeRequested {
            request: ToolExchangeRequest {
                invocation_id: "call0".into(),
                exchange_id: "exchange".into(),
                producer_id: "producer".into(),
                schema: "question".into(),
                schema_version: 1,
                payload: serde_json::json!({}),
                response_policy: ToolExchangeResponsePolicy::Required,
            },
        },
    );
    let tx = db.begin_transaction().await.unwrap();
    crate::db_event_store::insert_event(&*tx, &exchange, None)
        .await
        .unwrap();
    crate::durable_interaction::project_exchange_locator(&*tx, &exchange)
        .await
        .unwrap();
    crate::durable_interaction::project(&*tx, &exchange)
        .await
        .unwrap();
    checkpoint_at(&*tx, 7).await;
    // Publication is deliberately disabled; this fixture explicitly advances the
    // interaction checkpoint that enabled canonical append would advance.
    tx.update("projection_checkpoints")
        .value("last_event_seq", 7_i64)
        .where_eq("projection_name", "durable_interactions")
        .execute(&*tx)
        .await
        .unwrap();
    let mut checkpoint = match &combined(record, 9)[0].0.kind {
        SessionEventKind::DurableInteractionCommitted { operation } => operation.clone(),
        _ => unreachable!(),
    };
    checkpoint.operation_id = "checkpoint".into();
    checkpoint.expected_revision = None;
    checkpoint.transition = DurableInteractionTransition::Checkpoint {
        payload: DurableInteractionPayload {
            contract_id: "question".into(),
            contract_version: 1,
            json: "{}".into(),
        },
    };
    crate::durable_interaction::validate_transition(&*tx, session, &checkpoint, 8)
        .await
        .unwrap();
    let checkpoint = event(
        session,
        8,
        SessionEventKind::DurableInteractionCommitted {
            operation: checkpoint,
        },
    );
    crate::db_event_store::insert_event(&*tx, &checkpoint, None)
        .await
        .unwrap();
    crate::durable_interaction::project_exchange_locator(&*tx, &checkpoint)
        .await
        .unwrap();
    crate::durable_interaction::project(&*tx, &checkpoint)
        .await
        .unwrap();
    checkpoint_at(&*tx, 8).await;
    tx.commit().await.unwrap();
}

fn combined(record: &TurnContinuationRecord, sequence: u64) -> Vec<(SessionEvent, Option<u64>)> {
    let invocation_id = record.capture.selected_invocation_id.clone();
    let resolution = ToolExchangeResolutionEvent {
        invocation_id: invocation_id.clone(),
        exchange_id: record.capture.selected_exchange_id.clone(),
        resolution: ToolExchangeResolution::Responded {
            payload: serde_json::json!({"answer": 1}),
        },
    };
    let interaction = DurableInteractionOperation {
        version: DURABLE_INTERACTION_VERSION,
        key: DurableInteractionKey {
            turn_id: record.capture.key.turn_id.clone(),
            invocation_id: invocation_id.clone(),
            interaction_id: "question".into(),
        },
        operation_id: "complete".into(),
        request_digest: "a".repeat(64),
        producer_plugin_id: "producer".into(),
        request_event_sequence: 3,
        exchange_request_event_sequence: 7,
        expected_revision: Some(1),
        transition: DurableInteractionTransition::Complete {
            outcome: DurableInteractionPayload {
                contract_id: "answer".into(),
                contract_version: 1,
                json: "{}".into(),
            },
            exchange_resolution: resolution.clone(),
            result: Box::new(result(&invocation_id)),
        },
    };
    let session = record.capture.key.session_id;
    let result = event(
        session,
        sequence + 2,
        SessionEventKind::ToolInvocationResultRecorded {
            record: result(&invocation_id),
        },
    );
    let attach = next(
        record,
        TurnContinuationTransition::AttachSelectedResult {
            invocation_id,
            result_ref: turn_continuation_source_ref(&result).unwrap(),
        },
    );
    vec![
        (
            event(
                session,
                sequence,
                SessionEventKind::DurableInteractionCommitted {
                    operation: interaction,
                },
            ),
            None,
        ),
        (
            event(
                session,
                sequence + 1,
                SessionEventKind::ToolExchangeResolved { event: resolution },
            ),
            None,
        ),
        (result, None),
        (operation_event(&attach, sequence + 3), None),
    ]
}

#[tokio::test]
// One sequential crash-window scenario retains and reopens the same canonical transaction facts.
#[allow(clippy::too_many_lines)]
async fn selected_complete_and_attachment_roll_back_together_and_reopen_original_receipt() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selected.db");
    let session = SessionId::new();
    let db = database(&path).await;
    let prepare = fixture(&*db, session).await;
    commit(&*db, &prepare, 5).await.unwrap();
    let pending = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    let start = next(
        &pending,
        TurnContinuationTransition::StartMember {
            invocation_id: "call0".into(),
            dispatch_id: "dispatch0".into(),
        },
    );
    commit(&*db, &start, 6).await.unwrap();
    let started = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    interaction_ready(&*db, &started).await;
    let batch = combined(&started, 9);
    assert!(
        validate_atomic_attachments(&*db, &batch[..3])
            .await
            .is_err()
    );
    assert!(
        validate_atomic_attachments(&*db, &batch[3..])
            .await
            .is_err()
    );
    validate_atomic_attachments(&*db, &batch).await.unwrap();
    let mut forged = batch.clone();
    if let SessionEventKind::TurnContinuationCommitted { operation } = &mut forged[3].0.kind
        && let TurnContinuationTransition::AttachSelectedResult { result_ref, .. } =
            &mut operation.transition
    {
        result_ref.digest = "f".repeat(64);
    }
    assert!(validate_atomic_attachments(&*db, &forged).await.is_err());
    for count in 1..=4 {
        let tx = db.begin_transaction().await.unwrap();
        validate_atomic_attachments(&*tx, &batch).await.unwrap();
        for (event, _) in batch.iter().take(count) {
            if let SessionEventKind::TurnContinuationCommitted { operation } = &event.kind {
                validate_operation(&*tx, session, operation, event.sequence)
                    .await
                    .unwrap();
            }
            crate::db_event_store::insert_event(&*tx, event, None)
                .await
                .unwrap();
            project(&*tx, event).await.unwrap();
            // Production advances every projection checkpoint in the same append. The
            // disabled foundation fixture advances only its own explicit checkpoint.
            checkpoint_at(&*tx, event.sequence).await;
        }
        tx.rollback().await.unwrap();
        assert_eq!(
            verified_lookup(&*db, &prepare.key).await.unwrap().unwrap(),
            started
        );
        assert!(
            operation(&*db, &prepare.key, "operation3")
                .await
                .unwrap()
                .is_none()
        );
    }
    let tx = db.begin_transaction().await.unwrap();
    validate_atomic_attachments(&*tx, &batch).await.unwrap();
    for (event, _) in &batch {
        if let SessionEventKind::TurnContinuationCommitted { operation } = &event.kind {
            validate_operation(&*tx, session, operation, event.sequence)
                .await
                .unwrap();
        }
        crate::db_event_store::insert_event(&*tx, event, None)
            .await
            .unwrap();
        project(&*tx, event).await.unwrap();
        checkpoint_at(&*tx, event.sequence).await;
    }
    tx.commit().await.unwrap();
    let settled = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    assert!(matches!(
        settled.members[0],
        TurnContinuationMemberState::Settled { .. }
    ));
    let original = operation(&*db, &prepare.key, "operation3")
        .await
        .unwrap()
        .unwrap();
    let mut denied = result("call1");
    denied.is_error = true;
    let deny = next(
        &settled,
        TurnContinuationTransition::SettleWithoutDispatch {
            invocation_id: "call1".into(),
            result: Box::new(denied),
        },
    );
    commit(&*db, &deny, 13).await.unwrap();
    let all = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    let ready = next(&all, TurnContinuationTransition::ReadyForModel);
    commit(&*db, &ready, 15).await.unwrap();
    let ready = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    let dispatch = next(
        &ready,
        TurnContinuationTransition::StartModel {
            dispatch_id: "provider-request".into(),
        },
    );
    commit(&*db, &dispatch, 16).await.unwrap();
    let dispatched = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    let repeated = next(
        &dispatched,
        TurnContinuationTransition::StartModel {
            dispatch_id: "unsafe-retry".into(),
        },
    );
    assert!(commit(&*db, &repeated, 17).await.is_err());
    let successor = event(
        session,
        17,
        SessionEventKind::ModelTurnFinished {
            turn_id: "turn".into(),
            outcome: ModelTurnOutcome::Completed,
            message: None,
        },
    );
    let tx = db.begin_transaction().await.unwrap();
    crate::db_event_store::insert_event(&*tx, &successor, None)
        .await
        .unwrap();
    checkpoint_at(&*tx, 17).await;
    tx.commit().await.unwrap();
    let retire = next(
        &dispatched,
        TurnContinuationTransition::Retire {
            successor_ref: turn_continuation_source_ref(&successor).unwrap(),
        },
    );
    commit(&*db, &retire, 18).await.unwrap();
    let retired = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    assert!(matches!(
        retired.phase,
        TurnContinuationPhase::Retired { .. }
    ));
    assert_eq!(retry(&*db, &prepare).await.unwrap().unwrap().revision, 1);
    let mut conflict = prepare.clone();
    conflict.request_digest = "f".repeat(64);
    assert!(retry(&*db, &conflict).await.is_err());
    db.close().await.unwrap();
    let db = database(&path).await;
    assert_eq!(
        operation(&*db, &prepare.key, "operation3")
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        verified_lookup(&*db, &prepare.key).await.unwrap().unwrap(),
        retired
    );
    db.close().await.unwrap();
}
async fn fixture(db: &dyn Database, session: SessionId) -> TurnContinuationOperation {
    let mut prepare = prepare(session, 2);
    let TurnContinuationTransition::Prepare { capture } = &mut prepare.transition else {
        unreachable!()
    };
    let submission = PromptSubmission {
        id: SubmissionId("submission".into()),
        client_id: ClientId::new(),
        text: "original".into(),
        admission: TurnAdmissionMetadata::default(),
        requested_placement: SubmissionPlacement::FollowUp,
        target: SubmissionTarget::OwnTurn {
            turn_id: TurnId("turn".into()),
        },
        execution_descriptor: Some(descriptor()),
    };
    let accepted = event(
        session,
        1,
        SessionEventKind::SubmissionAccepted {
            submission: submission.clone(),
        },
    );
    let activated = event(
        session,
        2,
        SessionEventKind::SubmissionActivated {
            submission_id: submission.id.clone(),
            turn_id: TurnId("turn".into()),
            client_id: submission.client_id,
            text: submission.text.clone(),
            admission: submission.admission.clone(),
        },
    );
    let tx = db.begin_transaction().await.unwrap();
    for event in [&accepted, &activated] {
        crate::db_event_store::insert_event(&*tx, event, None)
            .await
            .unwrap();
        crate::submission::project(&*tx, event).await.unwrap();
    }
    capture.accepted = turn_continuation_source_ref(&accepted).unwrap();
    capture.activated = turn_continuation_source_ref(&activated).unwrap();
    capture.descriptor_source = capture.accepted.clone();
    capture.descriptor_digest =
        turn_continuation_descriptor_digest(submission.execution_descriptor.as_ref().unwrap())
            .unwrap();
    for (index, member) in capture.members.iter_mut().enumerate() {
        let request = event(
            session,
            3 + u64::try_from(index).unwrap(),
            SessionEventKind::PositionedToolCallRequested {
                turn_id: "turn".into(),
                output_position: TurnOutputPosition::new(u64::try_from(index).unwrap()),
                tool_call_id: member.invocation_id.clone(),
                tool_name: "question".into(),
                arguments_json: "{}".into(),
                producer_plugin_id: Some("producer".into()),
                working_directory: None,
            },
        );
        crate::db_event_store::insert_event(&*tx, &request, None)
            .await
            .unwrap();
        tx.insert("tool_runs")
            .value("tool_call_id", member.invocation_id.clone())
            .value(
                "event_seq_start",
                crate::db_event_store::seq_to_value(request.sequence),
            )
            .value("status", "running")
            .execute(&*tx)
            .await
            .unwrap();
        member.request = turn_continuation_source_ref(&request).unwrap();
    }
    checkpoint_at(&*tx, 4).await;
    tx.commit().await.unwrap();
    prepare
}
async fn commit(
    db: &dyn Database,
    operation: &TurnContinuationOperation,
    sequence: u64,
) -> SessionDbResult<()> {
    let tx = db.begin_transaction().await?;
    validate_operation(&*tx, operation.key.session_id, operation, sequence).await?;
    let event = operation_event(operation, sequence);
    crate::db_event_store::insert_event(&*tx, &event, None).await?;
    project(&*tx, &event).await?;
    if let Some(result) = result_event(operation, &event)? {
        validate_member_result(&*tx, &result).await?;
        crate::db_event_store::insert_event(&*tx, &result, None).await?;
        project(&*tx, &result).await?;
        checkpoint_at(&*tx, result.sequence).await;
    }
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn canonical_cas_rollback_restart_and_receipts() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("continuation.db");
    let session = SessionId::new();
    let db = database(&path).await;
    let prepare = fixture(&*db, session).await;
    commit(&*db, &prepare, 5).await.unwrap();
    let initial = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    let start = next(
        &initial,
        TurnContinuationTransition::StartMember {
            invocation_id: "call1".into(),
            dispatch_id: "dispatch1".into(),
        },
    );
    let tx = db.begin_transaction().await.unwrap();
    validate_operation(&*tx, session, &start, 6).await.unwrap();
    let start_event = operation_event(&start, 6);
    crate::db_event_store::insert_event(&*tx, &start_event, None)
        .await
        .unwrap();
    project(&*tx, &start_event).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(
        operation(&*db, &prepare.key, &start.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        verified_lookup(&*db, &prepare.key).await.unwrap().unwrap(),
        initial
    );
    commit(&*db, &start, 6).await.unwrap();
    let mut stale = start.clone();
    stale.operation_id = "competing".into();
    assert!(commit(&*db, &stale, 7).await.is_err());
    db.close().await.unwrap();
    let db = database(&path).await;
    let started = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    assert_eq!(started.receipt.revision, 2);
    assert_eq!(
        operation(&*db, &prepare.key, "prepare")
            .await
            .unwrap()
            .unwrap()
            .receipt,
        initial.receipt
    );
    let settle = next(
        &started,
        TurnContinuationTransition::SettleMember {
            invocation_id: "call1".into(),
            dispatch_id: "dispatch1".into(),
            result: Box::new(result("call1")),
        },
    );
    commit(&*db, &settle, 7).await.unwrap();
    let settled = verified_lookup(&*db, &prepare.key).await.unwrap().unwrap();
    assert!(matches!(
        settled.members[1],
        TurnContinuationMemberState::Settled { .. }
    ));
    let ready = next(&settled, TurnContinuationTransition::ReadyForModel);
    assert!(commit(&*db, &ready, 9).await.is_err());
    let bypass = next(
        &settled,
        TurnContinuationTransition::SettleMember {
            invocation_id: "call0".into(),
            dispatch_id: "forged".into(),
            result: Box::new(result("call0")),
        },
    );
    assert!(commit(&*db, &bypass, 9).await.is_err());
    let receipt = operation(&*db, &prepare.key, &settle.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.receipt.result_ref.as_ref().unwrap().sequence, 8);
    // A plausible projection is not proof: exact canonical operation damage fails closed.
    db.update("events")
        .value("payload", "{}")
        .where_eq("event_seq", 7_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(
        operation(&*db, &prepare.key, &settle.operation_id)
            .await
            .is_err()
    );
    assert!(verified_lookup(&*db, &prepare.key).await.is_err());
    db.close().await.unwrap();
}

#[test]
fn full_256_member_lifecycle_exceeds_old_operation_cap_without_eviction() {
    let prepare = prepare(SessionId::new(), 256);
    let mut record = advance(None, &prepare, 1000, None).unwrap();
    let original = record.receipt.clone();
    let mut sequence = 1001;
    for index in 0..256 {
        let invocation_id = format!("call{index}");
        let start = next(
            &record,
            TurnContinuationTransition::StartMember {
                invocation_id: invocation_id.clone(),
                dispatch_id: format!("dispatch{index}"),
            },
        );
        record = advance(Some(record), &start, sequence, None).unwrap();
        sequence += 1;
        let result_ref = source(
            if index == 0 {
                sequence - 1
            } else {
                sequence + 1
            },
            "tool_invocation_result_recorded",
        );
        let transition = if index == 0 {
            TurnContinuationTransition::AttachSelectedResult {
                invocation_id,
                result_ref: result_ref.clone(),
            }
        } else {
            TurnContinuationTransition::SettleMember {
                invocation_id: invocation_id.clone(),
                dispatch_id: format!("dispatch{index}"),
                result: Box::new(result(&invocation_id)),
            }
        };
        let settle = next(&record, transition);
        record = advance(Some(record), &settle, sequence, Some(result_ref)).unwrap();
        sequence += 2;
    }
    assert_eq!(record.receipt.revision, 513);
    for transition in [
        TurnContinuationTransition::ReadyForModel,
        TurnContinuationTransition::StartModel {
            dispatch_id: "provider-dispatch".into(),
        },
        TurnContinuationTransition::Retire {
            successor_ref: source(sequence - 1, "model_turn_finished"),
        },
    ] {
        let operation = next(&record, transition);
        record = advance(Some(record), &operation, sequence, None).unwrap();
        sequence += 1;
    }
    assert_eq!(record.receipt.revision, 516);
    assert_eq!(original.revision, 1);
    assert!(
        advance(
            Some(record.clone()),
            &next(&record, TurnContinuationTransition::ReadyForModel),
            sequence,
            None
        )
        .is_err()
    );
    for phase in [
        TurnContinuationPhase::ReadyForModel,
        TurnContinuationPhase::ModelDispatchStarted {
            dispatch_id: "provider".into(),
        },
    ] {
        let mut invalid = record.clone();
        invalid.phase = phase;
        let mutation = next(
            &invalid,
            TurnContinuationTransition::StartMember {
                invocation_id: "call0".into(),
                dispatch_id: "again".into(),
            },
        );
        assert!(advance(Some(invalid), &mutation, sequence, None).is_err());
    }
    let mut exhausted = advance(None, &prepare, 1000, None).unwrap();
    exhausted.receipt.revision = MAX_TURN_CONTINUATION_OPERATIONS;
    let mutation = next(
        &exhausted,
        TurnContinuationTransition::StartMember {
            invocation_id: "call0".into(),
            dispatch_id: "start".into(),
        },
    );
    assert!(advance(Some(exhausted), &mutation, 1001, None).is_err());
    let mut exhausted = advance(None, &prepare, 1000, None).unwrap();
    exhausted.retained_operation_bytes = MAX_TURN_CONTINUATION_RETAINED_BYTES;
    let mutation = next(&exhausted, TurnContinuationTransition::ReadyForModel);
    assert!(advance(Some(exhausted), &mutation, 1001, None).is_err());
}
