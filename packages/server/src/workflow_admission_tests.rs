//! Admission handoff and live recovery regressions.
use super::*;

#[tokio::test]
async fn transient_storage_recovery_drives_canonical_run_once() {
    let (root, mut state, _session_id) = admission_fixture().await;
    let placeholder = tempfile::tempdir().unwrap();
    let canonical_path = state.workflow_store.lock().unwrap().path().to_path_buf();
    {
        let state = Arc::get_mut(&mut state).unwrap();
        state.state_root = root.path().to_path_buf();
        state.startup_config.workflows.admission_timeout_ms =
            std::num::NonZeroU64::new(5_000).unwrap();
        state.workflow_store = StdMutex::new(
            bcode_workflow_store::WorkflowStore::open_in_state_dir(placeholder.path()).unwrap(),
        )
        .into();
    }
    let ownership = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(canonical_path.parent().unwrap().join("workflow.lock"))
        .unwrap();
    ownership.try_lock().expect("exclusive maintenance owner");
    let error = bcode_workflow_store::WorkflowStore::initialize_in_state_dir(root.path(), 4)
        .expect_err("real initialization contention");
    assert!(error.is_transient_initialization_failure());
    Arc::get_mut(&mut state).unwrap().workflow_store_unavailable = StdMutex::new(Some(
        WorkflowInitializationFailure::from_store_error(&error),
    ));
    state.start_workflow_driver().await;
    // A request while maintenance still owns the store fails without admitting a run.
    assert!(state.require_workflow_store().is_err());
    assert!(!state.workflow_restore_pending.load(Ordering::SeqCst));
    drop(ownership);
    let mut retries = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let state = Arc::clone(&state);
        retries.spawn_blocking(move || state.require_workflow_store());
    }
    while let Some(result) = retries.join_next().await {
        let result = result.unwrap();
        assert!(
            result.is_ok() || matches!(result, Err(ServerError::WorkflowStorageUnavailable(_)))
        );
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.require_workflow_store().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background retry completes");
    // Starting again must reuse the original singleton, not replace it after recovery.
    state.start_workflow_driver().await;
    let completed = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = state
                .workflow_store
                .lock()
                .unwrap()
                .run_summary("admission-run")
                .unwrap()
                .unwrap()
                .status;
            if status == bcode_workflow_store::RunStatus::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let task = state.workflow_driver_task.lock().await.take().unwrap();
    task.abort();
    let _ = task.await;
    completed.expect("canonical run restored by existing driver");
    let attempts = state
        .workflow_store
        .lock()
        .unwrap()
        .attempt_history("admission-run", None, 10)
        .unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "succeeded");
    assert!(!state.workflow_restore_pending.load(Ordering::SeqCst));
    drop(state);
}

async fn admission_fixture() -> (tempfile::TempDir, Arc<ServerState>, SessionId) {
    admission_fixture_with_capability(false).await
}

async fn admission_fixture_with_capability(
    mutating: bool,
) -> (tempfile::TempDir, Arc<ServerState>, SessionId) {
    let root = tempfile::tempdir().unwrap();
    let sessions = SessionManager::default();
    let session = sessions
        .create_session(None, PathBuf::from("."))
        .await
        .unwrap();
    let mut store = bcode_workflow_store::WorkflowStore::open_in_state_dir(root.path()).unwrap();
    let schema = bcode_workflow::ValueSchema {
        type_name: "u32".into(),
        schema: serde_json::json!({"type":"integer","minimum":0}),
    };
    let definition = bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "admission".into(),
        input: schema.clone(),
        output: schema.clone(),
        nodes: BTreeMap::from([(
            "agent".into(),
            bcode_workflow::NodeDefinition {
                id: "agent".into(),
                name: "agent".into(),
                kind: bcode_workflow::NodeKind::Agent,
                dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                input: schema.clone(),
                output: schema.clone(),
                resources: Vec::new(),
                configuration: {
                    let mut config = tests::test_workflow_prompt_configuration(
                        schema,
                        bcode_workflow::PromptContextTarget::SharedParentSequential,
                    );
                    if mutating {
                        config["read_only"] = serde_json::json!(false);
                        config["tool_capability"] = serde_json::json!("mutating");
                    }
                    config
                },
            },
        )]),
        edges: Vec::new(),
        entries: vec!["agent".into()],
        exits: vec!["agent".into()],
    };
    store
        .persist_definition("admission", 1, &definition)
        .unwrap();
    create_admission_run(&mut store, "admission-run", session.id);
    let (mut state,) =
        (tests::test_server_state_with_fake_provider_and_workflow_store(sessions, store),);
    state.startup_config.workflows.admission_timeout_ms = std::num::NonZeroU64::new(50).unwrap();
    state
        .selected_provider_context
        .settings
        .insert("fake_structured_output_json".into(), "1".into());
    (root, Arc::new(state), session.id)
}

fn create_admission_run(
    store: &mut bcode_workflow_store::WorkflowStore,
    run_id: &str,
    session_id: SessionId,
) {
    store
        .create_run(&bcode_workflow_store::NewWorkflowRun {
            run_id: run_id.into(),
            definition_id: "admission".into(),
            definition_version: 1,
            workspace_snapshot: "snapshot".into(),
            parent_session_id: Some(session_id.to_string()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: Some(tests::test_workflow_execution_authority()),
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".into(),
                profile_id: "build".into(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: bcode_workflow_store::WorkflowRunLimits::default(),
        })
        .unwrap();
}

async fn hold_shared_session(
    state: &ServerState,
    session_id: SessionId,
) -> bcode_session::SharedExecutionSessionPermit {
    let provenance = ExecutionSessionProvenance {
        version: bcode_session_models::EXECUTION_SESSION_PROVENANCE_VERSION,
        owner: "bcode.workflow".into(),
        run_id: "admission-run".into(),
        node_id: "agent".into(),
        activation_id: Some("held-activation".into()),
        attempt: 1,
        parent_session_id: session_id,
        context_mode: bcode_session_models::ExecutionSessionContextMode::SharedSequential,
        parent_generation: None,
        workspace_snapshot: Some("snapshot".into()),
    };
    state
        .sessions
        .admit_shared_execution_session(session_id, &provenance)
        .await
        .unwrap()
}

#[tokio::test]
async fn workflow_admission_timeout_recovers_without_daemon_restart() {
    let (_root, state, session_id) = admission_fixture().await;
    // Hold the same shared-session permit that a previous execution can retain.
    let permit = hold_shared_session(&state, session_id).await;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        drive_workflow_run(&state, "admission-run"),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("admission timed out"));
    drop(permit);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            restore_workflow_runs(&state, vec!["admission-run".into()]).await;
            let status = state
                .workflow_store
                .lock()
                .unwrap()
                .run_summary("admission-run")
                .unwrap()
                .unwrap()
                .status;
            if status == bcode_workflow_store::RunStatus::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn workflow_admission_lost_receipt_is_not_replayed_when_mutating() {
    let (_root, state, session_id) = admission_fixture_with_capability(true).await;
    let path = state.workflow_store.lock().unwrap().path().to_path_buf();
    let mut store = bcode_workflow_store::WorkflowStore::open_at_path(&path).unwrap();
    let pending = store
        .pending_activations_for_run("admission-run", 1)
        .unwrap()
        .pop()
        .unwrap();
    let plan = WorkflowActivationOwner { state: &state }
        .plan(&pending)
        .await
        .unwrap()
        .unwrap();
    store
        .prepare_pending_activation(
            &pending.run_id,
            &pending.node_id,
            &pending.activation_id,
            plan.side_effect,
            plan.intent,
            2,
        )
        .unwrap()
        .unwrap();
    restore_workflow_runs(&state, vec!["admission-run".into()]).await;
    assert_eq!(
        store.run_summary("admission-run").unwrap().unwrap().status,
        bcode_workflow_store::RunStatus::RepairRequired
    );
    assert!(state.session_current_turn(session_id).await.is_none());
    assert_eq!(
        store
            .attempt_history("admission-run", None, 10)
            .unwrap()
            .len(),
        1
    );
}

struct LoseReceipt;

impl bcode_workflow_store::WorkflowDispatchFault for LoseReceipt {
    fn after_boundary(
        &self,
        boundary: bcode_workflow_store::WorkflowDispatchBoundary,
        _request: &bcode_workflow_store::PreparedActivationDispatch,
    ) -> Result<(), WorkflowStoreError> {
        if boundary == bcode_workflow_store::WorkflowDispatchBoundary::OwnerAccepted {
            Err(WorkflowStoreError::InvalidData(
                "injected receipt loss".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn workflow_admission_recovery_reuses_accepted_turn() {
    let (_root, state, session_id) = admission_fixture().await;
    let path = state.workflow_store.lock().unwrap().path().to_path_buf();
    let mut store = bcode_workflow_store::WorkflowStore::open_at_path(&path).unwrap();
    let gate = workflow_drive_gate(&state, "admission-run");
    let guard = gate.lock().await;
    assert!(
        store
            .dispatch_pending_activations_with_fault(
                &WorkflowActivationOwner { state: &state },
                &LoseReceipt,
                1,
                2,
            )
            .await
            .is_err()
    );
    drop(guard);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            restore_workflow_runs(&state, vec!["admission-run".into()]).await;
            if store.run_summary("admission-run").unwrap().unwrap().status
                == bcode_workflow_store::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let history = state.sessions.session_history(session_id).await.unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|e| matches!(e.kind, SessionEventKind::UserMessage { .. }))
            .count(),
        1
    );
    assert_eq!(
        store
            .attempt_history("admission-run", None, 10)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn workflow_admission_cancelled_preparation_is_not_dispatched() {
    let (_root, state, session_id) = admission_fixture().await;
    let path = state.workflow_store.lock().unwrap().path().to_path_buf();
    let mut store = bcode_workflow_store::WorkflowStore::open_at_path(&path).unwrap();
    let pending = store
        .pending_activations_for_run("admission-run", 1)
        .unwrap()
        .pop()
        .unwrap();
    let plan = WorkflowActivationOwner { state: &state }
        .plan(&pending)
        .await
        .unwrap()
        .unwrap();
    store
        .prepare_pending_activation(
            &pending.run_id,
            &pending.node_id,
            &pending.activation_id,
            plan.side_effect,
            plan.intent,
            2,
        )
        .unwrap()
        .unwrap();
    store.request_cancellation("admission-run", 3).unwrap();
    restore_workflow_runs(&state, vec!["admission-run".into()]).await;
    assert!(!store.attempt_history("admission-run", None, 1).unwrap()[0].has_receipt);
    assert!(
        state
            .sessions
            .session_history(session_id)
            .await
            .unwrap()
            .iter()
            .all(|e| !matches!(e.kind, SessionEventKind::UserMessage { .. }))
    );
}

#[tokio::test]
async fn workflow_admission_foreign_owner_is_not_recovered() {
    let (_root, state, _) = admission_fixture().await;
    let path = state.workflow_store.lock().unwrap().path().to_path_buf();
    let mut store = bcode_workflow_store::WorkflowStore::open_at_path(&path).unwrap();
    let pending = store
        .pending_activations_for_run("admission-run", 1)
        .unwrap()
        .pop()
        .unwrap();
    let plan = WorkflowActivationOwner { state: &state }
        .plan(&pending)
        .await
        .unwrap()
        .unwrap();
    store
        .prepare_pending_activation(
            &pending.run_id,
            &pending.node_id,
            &pending.activation_id,
            plan.side_effect,
            plan.intent,
            2,
        )
        .unwrap()
        .unwrap();
    let other_store = bcode_workflow_store::WorkflowStore::open_at_path(&path).unwrap();
    let (mut other,) = (
        tests::test_server_state_with_fake_provider_and_workflow_store(
            SessionManager::default(),
            other_store,
        ),
    );
    other.daemon_status.instance_id = "foreign-instance".into();
    restore_workflow_runs(&Arc::new(other), vec!["admission-run".into()]).await;
    let attempt = store
        .attempt_history("admission-run", None, 1)
        .unwrap()
        .pop()
        .unwrap();
    assert!(!attempt.has_receipt);
    assert_eq!(attempt.status, "prepared");
}

#[tokio::test]
async fn workflow_admission_blocked_run_does_not_block_other_runs() {
    let (_root, mut state, session_id) = admission_fixture().await;
    Arc::get_mut(&mut state)
        .unwrap()
        .startup_config
        .workflows
        .admission_timeout_ms = std::num::NonZeroU64::new(10_000).unwrap();
    let other = state
        .sessions
        .create_session(None, PathBuf::from("."))
        .await
        .unwrap();
    create_admission_run(
        &mut state.workflow_store.lock().unwrap(),
        "other-run",
        other.id,
    );
    let permit = hold_shared_session(&state, session_id).await;
    state.start_workflow_driver().await;
    let sender = state.workflow_driver_sender.get().unwrap();
    sender.send("admission-run".into()).await.unwrap();
    sender.send("other-run".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = state
                .workflow_store
                .lock()
                .unwrap()
                .run_summary("other-run")
                .unwrap()
                .unwrap()
                .status;
            if status == bcode_workflow_store::RunStatus::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let task = state.workflow_driver_task.lock().await.take().unwrap();
    task.abort();
    let _ = task.await;
    drop(permit);
    // Aborted supervised work releases the gate, allowing later qualified recovery.
    assert!(
        workflow_drive_gate(&state, "admission-run")
            .try_lock()
            .is_ok()
    );
}

#[tokio::test]
async fn workflow_admission_live_driver_excludes_recovery() {
    let (_root, state, _) = admission_fixture().await;
    let gate = workflow_drive_gate(&state, "admission-run");
    let guard = gate.lock().await;
    restore_workflow_runs(&state, vec!["admission-run".into()]).await;
    assert_eq!(
        state
            .workflow_store
            .lock()
            .unwrap()
            .pending_activations_for_run("admission-run", 10)
            .unwrap()
            .len(),
        1
    );
    drop(guard);
    assert!(
        workflow_drive_gate(&state, "admission-run")
            .try_lock()
            .is_ok()
    );
    assert!(workflow_drive_gate(&state, "other-run").try_lock().is_ok());
}
