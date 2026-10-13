#[test]
fn shared_verification_failures_preserve_operation_and_safe_category() {
    use representation::RepresentationError as Error;
    for operation in [
        SharedVerificationOperation::BeginTransaction,
        SharedVerificationOperation::CommitTransaction,
        SharedVerificationOperation::RunSummary,
        SharedVerificationOperation::SessionLink,
        SharedVerificationOperation::ExecutionNode,
    ] {
        for (injected, category) in [
            (
                Error::Database(switchy_database::DatabaseError::Busy),
                SharedVerificationCategory::Busy,
            ),
            (
                Error::UnsupportedStore,
                SharedVerificationCategory::Unsupported,
            ),
            (
                Error::MaintenanceRequired,
                SharedVerificationCategory::MaintenanceRequired,
            ),
            (
                Error::InvalidAdmission,
                SharedVerificationCategory::InvalidState,
            ),
        ] {
            let failure = SharedVerificationFailure::from_storage(operation, &injected);
            assert_eq!(
                failure,
                SharedVerificationFailure {
                    operation,
                    category
                }
            );
        }
    }
}

#[test]
fn shared_failure_boundary_preserves_closed_codes_and_operations() {
    for (category, expected_code) in [
        (
            SharedVerificationCategory::Busy,
            "workflow_activity_shared_busy",
        ),
        (
            SharedVerificationCategory::Unsupported,
            "workflow_activity_shared_unsupported",
        ),
        (
            SharedVerificationCategory::MaintenanceRequired,
            "workflow_activity_shared_maintenance_required",
        ),
    ] {
        let error = preserve_live_failure(
            SharedVerificationFailure {
                operation: SharedVerificationOperation::SessionLink,
                category,
            }
            .into_server_error(),
        );
        let ServerError::WorkflowActivitySharedVerification {
            operation,
            code,
            message,
        } = error
        else {
            panic!("typed causal error was overwritten");
        };
        assert_eq!(operation, "session_link");
        assert_eq!(code, expected_code);
        assert!(!message.is_empty());
        let response =
            bcode_ipc::ErrorResponse::new(code, format!("{message}; operation={operation}"));
        let encoded = serde_json::to_value(response).expect("portable error response");
        assert_eq!(encoded["code"], expected_code);
    }
    for (operation, expected_operation) in [
        (
            SharedVerificationOperation::BeginTransaction,
            "begin_transaction",
        ),
        (
            SharedVerificationOperation::CommitTransaction,
            "commit_transaction",
        ),
    ] {
        let ServerError::WorkflowActivitySharedVerification {
            operation, code, ..
        } = preserve_live_failure(
            SharedVerificationFailure {
                operation,
                category: SharedVerificationCategory::Busy,
            }
            .into_server_error(),
        )
        else {
            panic!("typed transaction error was overwritten");
        };
        assert_eq!(operation, expected_operation);
        assert_eq!(code, "workflow_activity_shared_busy");
    }
    assert!(matches!(
        preserve_live_failure(super::super::unavailable()),
        ServerError::WorkflowActivityObservation {
            phase: "live_adaptation"
        }
    ));
}

// Behavioral contract/cursor tests; no source-text assertions.
use super::*;

async fn seeded_activity_state(
    intent: serde_json::Value,
) -> (tempfile::TempDir, ServerState, String, String) {
    let root = tempfile::tempdir().unwrap();
    let store =
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    let db = super::super::open(store.path(), true).unwrap();
    bcode_workflow_store::representation::install(&db)
        .await
        .unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "activity-endpoint",
        bcode_workflow::Step::task("worker", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "activity-endpoint",
        &definition,
    )
    .unwrap();
    let run = bcode_workflow_store::NewWorkflowRun {
        run_id: "endpoint-run".into(),
        definition_id: identity.definition_id,
        definition_version: identity.definition_version,
        workspace_snapshot: "fixture".into(),
        parent_session_id: None,
        parent_session_generation: None,
        binding: None,
        authored_provenance: None,
        input: Some(serde_json::json!(1)),
        execution_authority: None,
        created_at_ms: 1,
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            version: 1,
            provider_id: "fixture".into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::ReadOnly,
        limits: bcode_workflow::WorkflowRunLimits::default(),
    };
    bcode_workflow_store::representation::admit(&db, &run, &definition)
        .await
        .unwrap();
    let authority = bcode_workflow_store::WorkflowExecutionAuthority {
        target_artifact_id: "fixture-artifact".into(),
        daemon_instance_id: "fixture-daemon".into(),
        generation: 1,
        fencing_token: "fixture-fence".into(),
    };
    bcode_workflow_store::representation::attempts::claim_authority(&db, &run.run_id, &authority)
        .await
        .unwrap();
    let activation =
        bcode_workflow_store::representation::graph::activation_id(&db, &run.run_id, "worker")
            .await
            .unwrap()
            .unwrap();
    let dispatch = bcode_workflow_store::representation::attempts::prepare(
        &db,
        &authority,
        &bcode_workflow_store::PreparedAttempt {
            run_id: run.run_id.clone(),
            node_id: "worker".into(),
            activation_id: activation,
            attempt: 1,
            side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
            intent,
            prepared_at_ms: 10,
        },
    )
    .await
    .unwrap();
    let mut state = crate::tests::test_server_state(crate::SessionManager::default());
    state.workflow_store = std::sync::Arc::new(std::sync::Mutex::new(store));
    (root, state, run.run_id, dispatch)
}

async fn activity_dispatch_request(
    db: &dyn switchy_database::Database,
    run_id: &str,
    pending: &WorkflowActivityAttempt,
    dispatch: &str,
) -> bcode_workflow_store::PreparedActivationDispatch {
    let node =
        bcode_workflow_store::representation::graph::execution_node(db, run_id, &pending.node_id)
            .await
            .unwrap();
    bcode_workflow_store::PreparedActivationDispatch {
        activation: bcode_workflow_store::PendingActivation {
            run_id: run_id.into(),
            node_id: pending.node_id.clone(),
            activation_id: pending.activation_id.clone(),
            dependency_generation: 0,
            input: Some(serde_json::json!(1)),
            node,
            created_at_ms: 1,
        },
        attempt: 1,
        dispatch_identity: dispatch.into(),
        intent: serde_json::json!({}),
    }
}

fn shared_test_provenance(
    run_id: &str,
    pending: &WorkflowActivityAttempt,
    parent: bcode_session_models::SessionId,
) -> bcode_session_models::ExecutionSessionProvenance {
    bcode_session_models::ExecutionSessionProvenance {
        version: bcode_session_models::EXECUTION_SESSION_PROVENANCE_VERSION,
        owner: "bcode.workflow".into(),
        run_id: run_id.into(),
        node_id: pending.node_id.clone(),
        activation_id: Some(pending.activation_id.clone()),
        attempt: 1,
        parent_session_id: parent,
        context_mode: bcode_session_models::ExecutionSessionContextMode::SharedSequential,
        workspace_snapshot: Some("fixture".into()),
        parent_generation: None,
    }
}

async fn shared_completion_observation_link(
    state: &ServerState,
    session_id: crate::SessionId,
    run_id: &str,
    pending: &WorkflowActivityAttempt,
    dispatch: &str,
    permit: &bcode_session::SharedExecutionSessionPermit,
) -> bcode_workflow_store::WorkflowExecutionSessionLink {
    crate::register_workflow_node_runtime_work(
        state,
        session_id,
        &crate::WorkId::new(format!("workflow:{run_id}")),
        dispatch,
        "shared prompt".into(),
        crate::CancellationHandle::WorkflowNode(std::sync::Arc::new(
            crate::TurnCancelState::default(),
        )),
    )
    .await;
    assert!(
        state
            .runtime_work
            .bind_shared_execution(session_id, &crate::WorkId::new(dispatch.to_owned()), permit,)
            .await
    );
    bcode_workflow_store::WorkflowExecutionSessionLink::new(
        run_id.to_owned(),
        pending.node_id.clone(),
        pending.activation_id.clone(),
        1,
        session_id.to_string(),
        "fixture".into(),
        11,
    )
}

async fn completion_observation_token(
    state: &ServerState,
    session_id: crate::SessionId,
    dispatch: &str,
) -> Option<crate::runtime_work::WorkObservationToken> {
    state
        .runtime_work
        .observation_token(session_id, &crate::WorkId::new(dispatch.to_owned()))
        .await
}

#[tokio::test]
async fn shared_completion_waits_for_durable_settlement_before_releasing_guards() {
    let (_root, state, run_id, dispatch) =
        seeded_activity_state(serde_json::json!({"owner":"bcode.server.agent-turn/v1"})).await;
    let state = std::sync::Arc::new(state);
    let pending = fresh_attempt_page(&state, &run_id).await;
    let db = super::super::open(&crate::workflow_store_path(&state), true).unwrap();
    let authority =
        bcode_workflow_store::representation::attempts::owner_boundary::execution_authority(
            &db, &run_id,
        )
        .await
        .unwrap()
        .unwrap();
    let dispatch_request = activity_dispatch_request(&db, &run_id, &pending, &dispatch).await;
    let parent = state
        .sessions
        .create_session(None, std::env::temp_dir())
        .await
        .unwrap();
    let provenance = shared_test_provenance(&run_id, &pending, parent.id);
    let mut permit = Some(
        state
            .sessions
            .admit_shared_execution_session(parent.id, &provenance)
            .await
            .unwrap(),
    );
    let proof = permit.as_ref().unwrap().observation();
    let link = shared_completion_observation_link(
        &state,
        parent.id,
        &run_id,
        &pending,
        &dispatch,
        permit.as_ref().unwrap(),
    )
    .await;
    let finished = state
        .sessions
        .append_model_turn_finished(
            parent.id,
            "turn".into(),
            bcode_session_models::ModelTurnOutcome::Completed,
            None,
        )
        .await
        .unwrap();
    crate::publish_session_event(&state, &finished).await;
    let gate = crate::workflow_drive_gate(&state, &run_id);
    let guard = gate.lock().await;
    let receipt = serde_json::json!({"owner":"bcode.server.agent-turn/v1","session_id":parent.id,"turn_id":"turn"});
    let mut completion = Box::pin(crate::complete_workflow_prompt_dispatch(
        &state,
        &dispatch_request,
        &authority,
        &receipt,
        parent.id,
        (
            permit.take(),
            completion_observation_token(&state, parent.id, &dispatch_request.dispatch_identity)
                .await,
        ),
        Ok(bcode_workflow_store::AttemptObservation::Cancelled),
    ));
    assert!(futures::poll!(&mut completion).is_pending());
    assert!(proof.upgrade().unwrap().provenance().is_some());
    assert!(
        state
            .runtime_work
            .observes_shared_execution(parent.id, &link)
            .await
    );
    assert!(!fresh_attempt_page(&state, &run_id).await.terminal);
    drop(guard);
    completion.await;
    assert!(fresh_attempt_page(&state, &run_id).await.terminal);
    assert!(
        !state
            .runtime_work
            .observes_shared_execution(parent.id, &link)
            .await
    );
    assert!(proof.upgrade().is_none());
    drop(state);
}

#[tokio::test]
async fn failed_shared_settlement_releases_guards_without_terminalizing_attempt() {
    let (_root, state, run_id, dispatch) =
        seeded_activity_state(serde_json::json!({"owner":"bcode.server.agent-turn/v1"})).await;
    let state = std::sync::Arc::new(state);
    let pending = fresh_attempt_page(&state, &run_id).await;
    let db = super::super::open(&crate::workflow_store_path(&state), true).unwrap();
    let authority =
        bcode_workflow_store::representation::attempts::owner_boundary::execution_authority(
            &db, &run_id,
        )
        .await
        .unwrap()
        .unwrap();
    let dispatch_request = activity_dispatch_request(&db, &run_id, &pending, &dispatch).await;
    let parent = state
        .sessions
        .create_session(None, std::env::temp_dir())
        .await
        .unwrap();
    let provenance = shared_test_provenance(&run_id, &pending, parent.id);
    let mut permit = Some(
        state
            .sessions
            .admit_shared_execution_session(parent.id, &provenance)
            .await
            .unwrap(),
    );
    let proof = permit.as_ref().unwrap().observation();
    let receipt = serde_json::json!({"owner":"bcode.server.agent-turn/v1","session_id":parent.id,"turn_id":"turn"});
    bcode_workflow_store::representation::attempts::attach_receipt(
        &db,
        &authority,
        &bcode_workflow_store::DispatchReceipt {
            run_id: run_id.clone(),
            node_id: pending.node_id,
            activation_id: pending.activation_id,
            attempt: 1,
            dispatch_identity: dispatch.clone(),
            receipt: receipt.clone(),
            admitted_at_ms: 11,
        },
    )
    .await
    .unwrap();
    let mut stale_authority = authority;
    stale_authority.fencing_token = "stale".into();
    drop(db);
    crate::complete_workflow_prompt_dispatch(
        &state,
        &dispatch_request,
        &stale_authority,
        &receipt,
        parent.id,
        (
            permit.take(),
            completion_observation_token(&state, parent.id, &dispatch_request.dispatch_identity)
                .await,
        ),
        Ok(bcode_workflow_store::AttemptObservation::Cancelled),
    )
    .await;
    drop(permit);
    assert!(!fresh_attempt_page(&state, &run_id).await.terminal);
    assert!(proof.upgrade().is_none());
    // The sequential lock is reusable, rather than leaked by failed durable settlement.
    let replacement = tokio::time::timeout(
        Duration::from_secs(1),
        state
            .sessions
            .admit_shared_execution_session(parent.id, &provenance),
    )
    .await
    .unwrap()
    .unwrap();
    drop(replacement);
    drop(state);
}

#[tokio::test]
async fn retained_database_handle_keeps_snapshot_pages_readable_until_close() {
    let (_root, state, run_id, dispatch) = seeded_activity_state(serde_json::json!({})).await;
    let scans =
        WorkflowActivityScans::new(std::num::NonZeroUsize::MIN, Duration::from_secs(1)).unwrap();
    let mut query = request();
    query.root_run_id.clone_from(&run_id);
    query.run_id.clone_from(&run_id);
    query.expected_revision = None;
    let first = tokio::time::timeout(
        Duration::from_millis(250),
        page(&state, &scans, query.clone()),
    )
    .await
    .expect("start_scan and first page must not deadlock")
    .unwrap();
    assert_eq!(first.attempts.len(), 1);
    assert_eq!(first.attempts[0].dispatch_identity, dispatch);
    assert!(!first.attempts[0].terminal);
    assert!(matches!(
        first.attempts[0].observation,
        bcode_workflow_view_models::AttemptObservation::Unavailable { .. }
    ));
    assert!(first.inventory_exhausted);
    assert!(!first.subtree_coverage_complete);
    query.scan_id = Some(first.scan_id.clone());
    query.expected_revision = Some(first.revision);
    query.section = WorkflowActivitySection::Children;
    let second = tokio::time::timeout(Duration::from_millis(250), page(&state, &scans, query))
        .await
        .expect("retained snapshot page must not deadlock")
        .unwrap();
    drop(state);
    assert!(second.children.is_empty());
    assert!(second.inventory_exhausted);
    assert!(second.subtree_coverage_complete);
    tokio::time::timeout(Duration::from_millis(250), scans.close(&first.scan_id))
        .await
        .expect("snapshot release must not deadlock")
        .unwrap();
}

async fn fresh_attempt_page(state: &ServerState, run_id: &str) -> WorkflowActivityAttempt {
    let scans =
        WorkflowActivityScans::new(std::num::NonZeroUsize::MIN, Duration::from_secs(1)).unwrap();
    let mut query = request();
    query.root_run_id = run_id.into();
    query.run_id = run_id.into();
    query.expected_revision = None;
    let result = tokio::time::timeout(Duration::from_millis(250), page(state, &scans, query))
        .await
        .unwrap()
        .unwrap();
    scans.close(&result.scan_id).await.unwrap();
    result.attempts.into_iter().next().unwrap()
}

#[tokio::test]
async fn sessionless_admission_and_settlement_advance_only_canonical_stage_transitions() {
    use bcode_workflow_view_models::AttemptObservation;
    let (_root, state, run_id, dispatch) = seeded_activity_state(serde_json::json!({
        "owner": bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID,
    }))
    .await;
    let pending = fresh_attempt_page(&state, &run_id).await;
    assert!(
        matches!(pending.observation, AttemptObservation::PendingReadiness { ref stage, .. }
        if stage.sequence == 1 && stage.last_progress_at_ms == 10)
    );
    let db = super::super::open(&crate::workflow_store_path(&state), true).unwrap();
    let authority =
        bcode_workflow_store::representation::attempts::owner_boundary::execution_authority(
            &db, &run_id,
        )
        .await
        .unwrap()
        .unwrap();
    bcode_workflow_store::representation::attempts::attach_receipt(
        &db,
        &authority,
        &bcode_workflow_store::DispatchReceipt {
            run_id: run_id.clone(),
            node_id: pending.node_id.clone(),
            activation_id: pending.activation_id.clone(),
            attempt: 1,
            dispatch_identity: dispatch.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 11,
        },
    )
    .await
    .unwrap();
    let admitted = fresh_attempt_page(&state, &run_id).await;
    assert!(
        matches!(admitted.observation, AttemptObservation::RuntimeStage { ref progress }
        if progress.sequence == 2 && progress.last_progress_at_ms == 11)
    );
    assert_eq!(
        admitted.observation,
        fresh_attempt_page(&state, &run_id).await.observation
    );
    assert!(admitted.session_id.is_none());
    bcode_workflow_store::representation::attempts::settle(
        &db,
        &authority,
        &run_id,
        &dispatch,
        bcode_workflow_store::representation::attempts::TerminalOutcome::Failed,
        12,
    )
    .await
    .unwrap();
    let terminal = fresh_attempt_page(&state, &run_id).await;
    assert!(
        matches!(terminal.observation, AttemptObservation::Terminal { ref stage }
        if stage.sequence == 3 && stage.last_progress_at_ms == 12)
    );
    assert_eq!(
        terminal.observation,
        fresh_attempt_page(&state, &run_id).await.observation
    );
}

#[tokio::test]
async fn pending_agent_link_becomes_visible_only_in_a_fresh_scan() {
    use bcode_workflow_view_models::AttemptObservation;
    let (_root, state, run_id, _dispatch) = seeded_activity_state(serde_json::json!({
        "owner": "bcode.server.agent-turn/v1",
    }))
    .await;
    let pending = fresh_attempt_page(&state, &run_id).await;
    assert!(matches!(
        pending.observation,
        AttemptObservation::PendingReadiness { .. }
    ));
    let db = super::super::open(&crate::workflow_store_path(&state), true).unwrap();
    let authority =
        bcode_workflow_store::representation::attempts::owner_boundary::execution_authority(
            &db, &run_id,
        )
        .await
        .unwrap()
        .unwrap();
    let link = bcode_workflow_store::WorkflowExecutionSessionLink::new(
        run_id.clone(),
        pending.node_id,
        pending.activation_id,
        1,
        bcode_session_models::SessionId::new().to_string(),
        "fixture".into(),
        11,
    );
    bcode_workflow_store::representation::attempts::owner_boundary::persist_execution_session_link(
        &db, &authority, &link,
    )
    .await
    .unwrap();
    let linked = fresh_attempt_page(&state, &run_id).await;
    assert!(
        matches!(linked.observation, AttemptObservation::LinkedSession { ref progress, ref stage }
        if stage.sequence == 1 && matches!(progress.observation, SessionProgressObservation::Unavailable { .. }))
    );
    assert_eq!(linked.session_id.as_deref(), Some(link.session_id.as_str()));
}

fn request() -> WorkflowActivityPageRequest {
    WorkflowActivityPageRequest {
        version: WORKFLOW_ACTIVITY_VERSION,
        scan_id: None,
        root_run_id: "run".into(),
        run_id: "run".into(),
        expected_revision: Some(2),
        section: WorkflowActivitySection::Attempts,
        cursor: None,
        limit: 1,
    }
}

#[test]
fn cursor_is_bound_to_exact_run_revision_and_inventory_dimension() {
    let token = hex::encode(
        serde_json::to_vec(&Cursor {
            scan_id: "scan".into(),
            version: WORKFLOW_ACTIVITY_VERSION,
            run_id: "run".into(),
            revision: 2,
            section: WorkflowActivitySection::Attempts,
            after: "dispatch".into(),
        })
        .unwrap(),
    );
    let mut query = request();
    query.scan_id = Some("scan".into());
    query.cursor = Some(token);
    assert_eq!(decode(&query).unwrap().unwrap().after, "dispatch");
    query.expected_revision = Some(3);
    assert!(decode(&query).is_err());
    query.expected_revision = Some(2);
    query.section = WorkflowActivitySection::Children;
    assert!(decode(&query).is_err());
    query.section = WorkflowActivitySection::Attempts;
    query.run_id = "other".into();
    assert!(decode(&query).is_err());
}

#[test]
fn unknown_live_coverage_cannot_be_compared_as_progress_or_silence() {
    let snapshot = unknown("session".into());
    assert_eq!(snapshot.advanced_since(&snapshot), None);
    assert!(matches!(
        snapshot.observation,
        SessionProgressObservation::Unavailable {
            reason: SessionProgressUnavailableReason::CoverageUnavailable
        }
    ));
}

#[test]
fn recursive_snapshot_coverage_requires_both_sections_of_every_descendant() {
    let mut inventory = BTreeMap::new();
    assert!(!inventory_complete(&inventory));
    inventory.insert(
        "root".into(),
        RunCoverage {
            attempts_done: true,
            children_done: true,
            ..RunCoverage::default()
        },
    );
    assert!(inventory_complete(&inventory));
    inventory.insert("child".into(), RunCoverage::default());
    assert!(!inventory_complete(&inventory));
    inventory.get_mut("child").unwrap().attempts_done = true;
    assert!(!inventory_complete(&inventory));
    inventory.get_mut("child").unwrap().children_done = true;
    inventory.insert("grandchild".into(), RunCoverage::default());
    assert!(!inventory_complete(&inventory));
    let grandchild = inventory.get_mut("grandchild").unwrap();
    grandchild.attempts_done = true;
    grandchild.children_done = true;
    assert!(inventory_complete(&inventory));
}

#[tokio::test]
async fn scan_policy_rejects_zero_lifetime_and_close_is_idempotent() {
    assert!(
        WorkflowActivityScans::new(std::num::NonZeroUsize::MIN, std::time::Duration::ZERO,)
            .is_err()
    );
    let scans = WorkflowActivityScans::new(
        std::num::NonZeroUsize::MIN,
        std::time::Duration::from_secs(1),
    )
    .unwrap();
    scans.close("missing").await.unwrap();
    scans.close("missing").await.unwrap();
    scans.expire().await.unwrap();
    assert!(scans.scans.lock().await.is_empty());
}

#[test]
fn invalid_versions_limits_and_cursor_bytes_fail_closed() {
    let mut query = request();
    query.version += 1;
    assert!(decode(&query).is_err());
    query = request();
    query.limit = 0;
    assert!(decode(&query).is_err());
    query.limit = 101;
    assert!(decode(&query).is_err());
    query = request();
    query.cursor = Some("not-hex".into());
    assert!(decode(&query).is_err());
}
