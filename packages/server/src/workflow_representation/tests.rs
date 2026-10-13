#[path = "tests/child.rs"]
mod child;
#[path = "tests/fanout.rs"]
mod fanout;
#[path = "tests/inspection.rs"]
mod inspection_tests;
#[path = "tests/old_artifact.rs"]
mod old_artifact;
#[path = "tests/parallel.rs"]
mod parallel;
#[path = "tests/publication.rs"]
mod publication;

use super::*;
use switchy_database::{Database, query::FilterableQuery};

fn request(id: &str) -> bcode_workflow_store::NewWorkflowRun {
    bcode_workflow_store::NewWorkflowRun {
        run_id: id.into(),
        definition_id: "fixture".into(),
        definition_version: 1,
        workspace_snapshot: "fixture".into(),
        parent_session_id: Some("parent".into()),
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
        limits: bcode_workflow::WorkflowRunLimits {
            concurrency_cap: Some(7),
            ..Default::default()
        },
    }
}

fn fixture_with_store(coexistence: bool) -> (tempfile::TempDir, ServerState) {
    let root = tempfile::tempdir().expect("root");
    let mut store = if coexistence {
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path())
    } else {
        bcode_workflow_store::WorkflowStore::open_in_state_dir(root.path())
    }
    .expect("store");
    let definition = bcode_workflow::WorkflowBuilder::new(
        "fixture",
        bcode_workflow::Step::task("worker", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .expect("definition")
    .definition()
    .clone();
    store
        .persist_definition("fixture", 1, &definition)
        .expect("legacy definition");
    store.create_run(&request("legacy")).expect("legacy run");
    let mut state = crate::tests::test_server_state(crate::SessionManager::default());
    state.workflow_store = std::sync::Arc::new(std::sync::Mutex::new(store));
    (root, state)
}

#[tokio::test]
async fn legacy_only_database_routes_without_installing_extension() {
    let (_root, state) = fixture_with_store(false);
    let path = crate::workflow_store_path(&state);
    assert_eq!(
        status(&state, "legacy")
            .await
            .expect("legacy status")
            .expect("run")
            .run_id,
        "legacy"
    );
    assert!(status(&state, "absent").await.expect("missing").is_none());
    let db = open(&path, false).expect("typed read");
    assert!(
        !db.table_exists("workflow_representation_contract")
            .await
            .expect("schema")
    );
}

async fn admit_extension(state: &ServerState) {
    admit_extension_with_observations(state, false).await;
}

async fn admit_extension_with_observations(state: &ServerState, observed: bool) {
    let db = open(&crate::workflow_store_path(state), true).expect("typed store");
    representation::install(&db)
        .await
        .expect("install fixture extension");
    let definition = bcode_workflow::WorkflowBuilder::new(
        "new",
        bcode_workflow::Step::task("worker", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .expect("definition")
    .definition()
    .clone();
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition("new", &definition)
        .expect("identity");
    state
        .workflow_store
        .lock()
        .unwrap()
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .unwrap();
    let mut run = request("new");
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    run.limits.concurrency_cap = None;
    if observed {
        representation::journal::install(&db).await.unwrap();
        representation::admit_registered_observed(&db, &run, &definition, true)
            .await
            .expect("observed registered admission");
    } else {
        representation::admit(&db, &run, &definition)
            .await
            .expect("extension admission");
    }
}

#[tokio::test]
async fn exact_new_status_and_detachment_do_not_acquire_execution_authority() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let run = status(&state, "new")
        .await
        .expect("status")
        .expect("new run");
    assert_eq!(run.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(run.status, bcode_workflow::RunStatus::Paused);
    assert!(require_legacy(&state, "new").await.is_err());
    assert_eq!(detach(&state, "new").await.expect("detach"), Some(true));
    assert_eq!(detach(&state, "new").await.expect("repeat"), Some(false));
    assert!(
        status(&state, "new")
            .await
            .expect("status")
            .expect("new")
            .parent_session_id
            .is_none()
    );
}

#[tokio::test]
async fn unknown_execution_is_not_legacy_but_known_association_can_detach() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let db = open(&crate::workflow_store_path(&state), true).expect("typed store");
    db.update("workflow_representation_contract")
        .value("version", 999_i64)
        .where_eq("representation", 2_i64)
        .execute(&db)
        .await
        .expect("future fixture");
    assert!(status(&state, "new").await.is_err());
    assert!(
        status(&state, "legacy")
            .await
            .expect("independent legacy")
            .is_some()
    );
    assert_eq!(
        detach(&state, "new").await.expect("association exempt"),
        Some(true)
    );
}

#[tokio::test]
async fn corrupt_extension_row_is_not_missing_or_legacy_and_unavailable_store_never_routes_placeholder()
 {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let db = open(&crate::workflow_store_path(&state), true).expect("typed store");
    db.update("workflow_runs_v2")
        .value("status", "future-state")
        .where_eq("run_id", "new")
        .execute(&db)
        .await
        .expect("corrupt fixture");
    assert!(status(&state, "new").await.is_err());
    assert!(require_legacy(&state, "new").await.is_err());
    assert!(
        status(&state, "legacy")
            .await
            .expect("legacy isolated")
            .is_some()
    );
    *state.workflow_store_unavailable.lock().expect("readiness") =
        Some("fixture unavailable".into());
    assert!(status(&state, "legacy").await.is_err());
    assert!(detach(&state, "new").await.is_err());
}

#[tokio::test]
async fn public_control_detaches_unknown_execution_without_fabricating_status() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let db = open(&crate::workflow_store_path(&state), true).expect("typed store");
    db.update("workflow_representation_contract")
        .value("version", 999_i64)
        .where_eq("representation", 2_i64)
        .execute(&db)
        .await
        .expect("future contract");
    let state = std::sync::Arc::new(state);
    let (summary, changed) = crate::workflow_operations::control_exact_run(
        &state,
        "new",
        bcode_workflow::WorkflowRunControlAction::Detach,
    )
    .await
    .expect("public detach");
    assert!(changed);
    assert!(
        summary.is_none(),
        "unsupported execution has no invented summary"
    );
    assert!(
        representation::runs::association(&db, "new")
            .await
            .expect("association")
            .is_none()
    );
    assert!(
        crate::workflow_operations::resume_run(&state, "new")
            .await
            .is_err()
    );
    assert!(
        crate::workflow_operations::cancel_run(&state, "new")
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn retained_handle_contention_does_not_block_runtime_progress() {
    let (_root, state) = fixture_with_store(true);
    state
        .poll_workflow_initialization(false)
        .expect("ready store");
    let state = std::sync::Arc::new(state);
    let (locked, acquired) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let owner = std::sync::Arc::clone(&state);
    let thread = std::thread::spawn(move || {
        let _guard = owner.workflow_store.lock().expect("store");
        locked.send(()).expect("notify lock");
        released.recv().expect("release lock");
    });
    acquired.await.expect("locked");
    let mut retain = Box::pin(retained_store(&state));
    tokio::select! {
        biased;
        result = &mut retain => panic!("locked store unexpectedly completed: {}", result.is_ok()),
        () = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
    }
    release.send(()).expect("release");
    thread.join().expect("lock owner");
    tokio::time::timeout(std::time::Duration::from_secs(1), retain)
        .await
        .expect("runtime remained responsive")
        .expect("retained handle");
    drop(state);
}

#[tokio::test]
async fn retained_adapter_handle_survives_server_handle_replacement() {
    let (_root, state) = fixture_with_store(true);
    let retained = retained_store(&state)
        .await
        .expect("retained maintenance fence");
    let path = retained.path().to_path_buf();
    let other = tempfile::tempdir().expect("replacement root");
    *state.workflow_store.lock().expect("store") =
        bcode_workflow_store::WorkflowStore::open_in_state_dir(other.path()).expect("replacement");
    assert_eq!(
        lookup_at(&path, "legacy").await.expect("original root"),
        RunLookup::Legacy
    );
    assert!(lookup_at(&path, "absent").await.is_ok());
}

async fn prepare_owner_fixture(
    db: &dyn Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> bcode_workflow_store::PreparedActivationDispatch {
    let (revision, definition) = representation::graph_revision::current(db, "new")
        .await
        .expect("graph");
    let activation = representation::graph::ready(db, "new", revision, None, 1)
        .await
        .expect("ready")
        .pop()
        .expect("activation");
    let prepared = bcode_workflow_store::PreparedAttempt {
        run_id: "new".into(),
        node_id: activation.node_id.clone(),
        activation_id: activation.activation_id.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"owner":"fixture"}),
        prepared_at_ms: 2,
    };
    let identity = representation::attempts::prepare(db, authority, &prepared)
        .await
        .expect("prepare");
    bcode_workflow_store::PreparedActivationDispatch {
        activation: bcode_workflow_store::PendingActivation {
            run_id: "new".into(),
            node_id: activation.node_id.clone(),
            activation_id: activation.activation_id.clone(),
            dependency_generation: activation.generation,
            input: Some(activation.input),
            node: definition.node(&activation.node_id).expect("node").clone(),
            created_at_ms: 1,
        },
        attempt: 1,
        dispatch_identity: identity,
        intent: prepared.intent,
    }
}

#[tokio::test]
async fn settlement_verification_never_claims_unowned_or_foreign_authority() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    assert!(
        owner::settlement_authority(&state, "new")
            .await
            .unwrap()
            .is_none()
    );
    let db = open(&crate::workflow_store_path(&state), true).unwrap();
    let mut foreign = crate::tests::test_workflow_execution_authority();
    foreign.daemon_instance_id = "foreign-ended-owner".into();
    representation::attempts::claim_authority(&db, "new", &foreign)
        .await
        .unwrap();
    assert!(
        owner::settlement_authority(&state, "new")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        representation::attempts::owner_boundary::execution_authority(&db, "new")
            .await
            .unwrap(),
        Some(foreign)
    );
}

#[tokio::test]
async fn common_owner_adapter_fences_receipts_and_atomically_rejects_invalid_output() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let db = open(&crate::workflow_store_path(&state), true).expect("typed store");
    let authority = crate::tests::test_workflow_execution_authority();
    representation::attempts::claim_authority(&db, "new", &authority)
        .await
        .expect("owner fixture");
    let request = prepare_owner_fixture(&db, &authority).await;
    assert!(
        owner::check_owner_admission(&state, &request, &authority)
            .await
            .expect("handoff")
            .is_none()
    );
    let mut foreign_owner = authority.clone();
    foreign_owner.generation += 1;
    assert!(
        owner::check_owner_admission(&state, &request, &foreign_owner)
            .await
            .is_err()
    );
    attach_owner_fixture_receipt(&state, &request, &authority).await;
    let mut output = bcode_workflow_store::ValidatedOutput {
        output_id: "canonical-output".into(),
        run_id: "new".into(),
        node_id: request.activation.node_id.clone(),
        activation_id: request.activation.activation_id.clone(),
        schema_id: request.activation.node.output.type_name.clone(),
        schema_version: 1,
        value: serde_json::json!("invalid integer"),
        artifact_reference: None,
        created_at_ms: 4,
    };
    assert!(
        owner::settle_observation(
            &state,
            &request,
            &authority,
            bcode_workflow_store::AttemptObservation::Succeeded {
                output: output.clone()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        representation::attempts::pending(&db, "new", None, 10)
            .await
            .expect("still pending")
            .len(),
        1
    );
    output.value = serde_json::json!(9);
    owner::settle_observation(
        &state,
        &request,
        &authority,
        bcode_workflow_store::AttemptObservation::Succeeded {
            output: output.clone(),
        },
    )
    .await
    .expect("atomic success");
    owner::settle_observation(
        &state,
        &request,
        &authority,
        bcode_workflow_store::AttemptObservation::Succeeded {
            output: output.clone(),
        },
    )
    .await
    .expect("identical terminal completion is idempotent");
    assert_eq!(
        representation::graph::completion::output(&db, "new", "canonical-output")
            .await
            .expect("output"),
        Some(output)
    );
    assert_eq!(
        status(&state, "new")
            .await
            .expect("summary")
            .expect("run")
            .status,
        bcode_workflow::RunStatus::Completed
    );
}

async fn attach_owner_fixture_receipt(
    state: &ServerState,
    request: &bcode_workflow_store::PreparedActivationDispatch,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) {
    let receipt = bcode_workflow_store::DispatchReceipt {
        run_id: "new".into(),
        node_id: request.activation.node_id.clone(),
        activation_id: request.activation.activation_id.clone(),
        attempt: 1,
        dispatch_identity: request.dispatch_identity.clone(),
        receipt: serde_json::json!({"accepted":true}),
        admitted_at_ms: 3,
    };
    owner::persist_receipt(state, &receipt, authority)
        .await
        .expect("receipt");
    owner::persist_receipt(state, &receipt, authority)
        .await
        .expect("duplicate receipt");
}

#[tokio::test]
async fn admission_family_selection_never_installs_or_dispatches_unsupported_lifecycle() {
    let (_root, state) = fixture_with_store(true);
    let mut run = request("uncapped");
    run.limits.concurrency_cap = None;
    assert!(admit_prepared(&state, &run).await.is_err());
    let db = open(&crate::workflow_store_path(&state), false).expect("store");
    assert!(
        !db.table_exists("workflow_representation_contract")
            .await
            .expect("no installation")
    );
    assert!(status(&state, "uncapped").await.expect("missing").is_none());
    run.limits.concurrency_cap = Some(7);
    assert!(
        admit_prepared(&state, &run).await.is_err(),
        "numeric caps do not bypass unsupported typed execution"
    );
    let (_historical_root, historical) = fixture_with_store(false);
    run.limits.concurrency_cap = None;
    assert!(
        admit_prepared(&historical, &run)
            .await
            .expect("historical route")
            .is_none()
    );
}

pub(super) async fn typed_provider_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    std::sync::Arc<ServerState>,
    bcode_workflow::WorkflowDefinition,
) {
    typed_provider_fixture_with_delegation(false).await
}

async fn typed_provider_fixture_with_delegation(
    delegation: bool,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    std::sync::Arc<ServerState>,
    bcode_workflow::WorkflowDefinition,
) {
    typed_provider_fixture_with_target(
        delegation,
        bcode_workflow::PromptContextTarget::FreshIsolated,
    )
    .await
}

async fn typed_provider_fixture_with_target(
    delegation: bool,
    target: bcode_workflow::PromptContextTarget,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    std::sync::Arc<ServerState>,
    bcode_workflow::WorkflowDefinition,
) {
    let root = tempfile::tempdir().expect("workflow root");
    let sessions_root = tempfile::tempdir().expect("sessions root");
    let sessions = crate::SessionManager::persistent_lazy(sessions_root.path());
    let parent = sessions
        .create_session(Some("typed-owner".into()), std::path::PathBuf::from("."))
        .await
        .expect("parent");
    let store = bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path())
        .expect("baseline");
    let state = std::sync::Arc::new({
        let mut configured =
            crate::tests::test_server_state_with_workflow_authorization(sessions, store);
        configured
            .selected_provider_context
            .settings
            .insert("fake_structured_output_json".into(), "9".into());
        if delegation {
            configure_delegation_fixture(&mut configured);
        }
        configured
    });
    let schema = bcode_workflow::ValueSchema::of::<u32>();
    let config: bcode_workflow::WorkflowPromptConfiguration = serde_json::from_value(
        crate::tests::test_workflow_prompt_configuration(schema, target),
    )
    .expect("configuration");
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "typed-agent",
        bcode_workflow::Step::<u32, u32>::agent("agent", &config).expect("agent"),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone();
    if delegation {
        let mut successor = definition.nodes["agent"].clone();
        successor.id = "final".into();
        successor.name = "final".into();
        definition.nodes.insert("final".into(), successor);
        definition.exits = vec!["final".into()];
        definition.edges.push(bcode_workflow::EdgeDefinition {
            from: "agent".into(),
            to: "final".into(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        });
    }
    let id = bcode_workflow::WorkflowDefinitionIdentity::for_definition("typed-agent", &definition)
        .expect("identity");
    let mut run = request("new");
    run.definition_id = id.definition_id;
    run.definition_version = id.definition_version;
    run.parent_session_id = Some(parent.id.to_string());
    run.limits.concurrency_cap = None;
    run.execution_authority = Some(bcode_workflow_store::WorkflowExecutionAuthority {
        target_artifact_id: crate::workflow_operations::current_artifact_id(&state),
        daemon_instance_id: state.daemon_status.instance_id.clone(),
        generation: 1,
        fencing_token: "fixture-fence".into(),
    });
    let db = open(&crate::workflow_store_path(&state), true).expect("typed store");
    representation::install(&db)
        .await
        .expect("explicit fixture installation");
    representation::admit(&db, &run, &definition)
        .await
        .expect("test-only admission");
    (root, sessions_root, state, definition)
}

#[tokio::test]
async fn typed_shared_parent_uses_real_prompt_owner_and_retains_canonical_link() {
    let (_root, _sessions_root, state, _) = typed_provider_fixture_with_target(
        false,
        bcode_workflow::PromptContextTarget::SharedParentSequential,
    )
    .await;
    let mut cleanup = FixtureCleanup::new(&state);
    let parent = status(&state, "new")
        .await
        .unwrap()
        .unwrap()
        .parent_session_id
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(6), async {
        loop {
            crate::drive_workflow_run(&state, "new").await.unwrap();
            if status(&state, "new").await.unwrap().unwrap().status
                == bcode_workflow::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shared parent completes within existing regression bound");
    let db = open(&crate::workflow_store_path(&state), false).unwrap();
    let links = representation::inspection_sessions::links(&db, "new", None, 10)
        .await
        .unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].session_id, parent);
    assert!(
        workflow_activity::verify_shared_parent_link(&db, "new", &links[0])
            .await
            .unwrap()
    );
    let id = parent.parse().unwrap();
    assert!(
        state
            .sessions
            .session_summary_non_owning(id)
            .await
            .unwrap()
            .execution
            .is_none()
    );
    let mut spoofed = links[0].clone();
    spoofed.session_id = bcode_session_models::SessionId::new().to_string();
    assert!(
        !workflow_activity::verify_shared_parent_link(&db, "new", &spoofed)
            .await
            .unwrap()
    );
    spoofed = links[0].clone();
    spoofed.workspace_snapshot = "wrong-workspace".into();
    assert!(
        !workflow_activity::verify_shared_parent_link(&db, "new", &spoofed)
            .await
            .unwrap()
    );
    cleanup.finish().await;
    drop(cleanup);
}

#[tokio::test]
async fn owner_admission_waits_for_store_writer_and_rechecks_authority() {
    use bcode_workflow_store::ActivationDispatchOwner;

    let (_root, _sessions, state, definition) = typed_provider_fixture().await;
    let path = crate::workflow_store_path(&state);
    let db = open(&path, true).expect("db");
    let guard = owner::local_authority(&state, "new")
        .await
        .expect("authority")
        .expect("owner");
    let activation = representation::graph::ready(&db, "new", 1, None, 1)
        .await
        .expect("ready")
        .pop()
        .expect("activation");
    let pending = bcode_workflow_store::PendingActivation {
        run_id: "new".into(),
        node_id: activation.node_id.clone(),
        activation_id: activation.activation_id,
        dependency_generation: activation.generation,
        input: Some(activation.input),
        node: definition.node(&activation.node_id).expect("node").clone(),
        created_at_ms: 1,
    };
    let plan = crate::WorkflowActivationOwner { state: &state }
        .plan(&pending)
        .await
        .expect("plan")
        .expect("supported");
    let prepared = bcode_workflow_store::PreparedAttempt {
        run_id: pending.run_id.clone(),
        node_id: pending.node_id.clone(),
        activation_id: pending.activation_id.clone(),
        attempt: 1,
        side_effect: plan.side_effect,
        intent: plan.intent,
        prepared_at_ms: crate::current_unix_millis(),
    };
    let identity = representation::attempts::prepare(&db, &guard.authority, &prepared)
        .await
        .expect("prepare");
    let dispatch = bcode_workflow_store::PreparedActivationDispatch {
        activation: pending,
        attempt: 1,
        dispatch_identity: identity,
        intent: prepared.intent,
    };
    let queue = backpressure::write_queue(&path).expect("queue");
    let writer = queue.lock().await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            owner::check_owner_admission(&state, &dispatch, &guard.authority),
        )
        .await
        .is_err(),
        "admission must not bypass an active persistence writer"
    );
    drop(writer);
    assert!(
        owner::check_owner_admission(&state, &dispatch, &guard.authority)
            .await
            .expect("released writer")
            .is_none()
    );
    let mut obsolete_authority = guard.authority.clone();
    obsolete_authority.fencing_token = "stale-owner".into();
    assert!(
        owner::check_owner_admission(&state, &dispatch, &obsolete_authority)
            .await
            .is_err(),
        "queue admission does not confer execution authority"
    );
    assert!(
        state
            .sessions
            .all_session_summaries()
            .await
            .iter()
            .all(|session| {
                session
                    .execution
                    .as_ref()
                    .is_none_or(|execution| execution.provenance.run_id != "new")
            })
    );
}

#[tokio::test]
async fn typed_agent_test_harness_uses_real_prompt_owner_and_fake_provider() {
    use bcode_workflow_store::ActivationDispatchOwner;
    let (_root, _sessions_root, state, definition) = typed_provider_fixture().await;
    let db = open(&crate::workflow_store_path(&state), true).expect("typed store");
    let guard = crate::workflow_operations::execution_authority(&state, "new")
        .await
        .expect("authority")
        .expect("owner");
    let activation = representation::graph::ready(&db, "new", 1, None, 1)
        .await
        .expect("ready")
        .pop()
        .expect("activation");
    let pending = bcode_workflow_store::PendingActivation {
        run_id: "new".into(),
        node_id: activation.node_id.clone(),
        activation_id: activation.activation_id,
        dependency_generation: activation.generation,
        input: Some(activation.input),
        node: definition.node(&activation.node_id).expect("node").clone(),
        created_at_ms: 1,
    };
    let plan = crate::WorkflowActivationOwner { state: &state }
        .plan(&pending)
        .await
        .expect("production plan")
        .expect("supported");
    let prepared = bcode_workflow_store::PreparedAttempt {
        run_id: pending.run_id.clone(),
        node_id: pending.node_id.clone(),
        activation_id: pending.activation_id.clone(),
        attempt: 1,
        side_effect: plan.side_effect,
        intent: plan.intent,
        prepared_at_ms: crate::current_unix_millis(),
    };
    let identity = representation::attempts::prepare(&db, &guard.authority, &prepared)
        .await
        .expect("prepare");
    let dispatch = bcode_workflow_store::PreparedActivationDispatch {
        activation: pending,
        attempt: 1,
        dispatch_identity: identity.clone(),
        intent: prepared.intent,
    };
    let receipt = crate::WorkflowActivationOwner { state: &state }
        .dispatch(&dispatch)
        .await
        .expect("actual provider admission");
    assert!(receipt.get("turn_id").is_some());
    if let Err(error) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if status(&state, "new")
                .await
                .expect("status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    {
        let session_id = receipt["session_id"]
            .as_str()
            .expect("session")
            .parse()
            .expect("id");
        let events = state
            .sessions
            .session_turn_evidence(session_id, receipt["turn_id"].as_str().expect("turn"))
            .await;
        panic!("production callback timeout: {error:?}, events: {events:?}");
    }
    assert!(
        representation::attempts::pending(&db, "new", None, 10)
            .await
            .expect("pending")
            .is_empty()
    );
    assert_eq!(
        crate::WorkflowActivationOwner { state: &state }
            .dispatch(&dispatch)
            .await
            .expect("idempotent receipt"),
        receipt
    );
}

#[tokio::test]
async fn normal_start_uses_typed_family_and_rescans_two_running_agents_without_duplicate_turns() {
    let (_root, _sessions_root, state, _) = typed_provider_fixture().await;
    let parent = state
        .sessions
        .create_session(
            Some("parallel-parent".into()),
            std::path::PathBuf::from("."),
        )
        .await
        .expect("parent");
    let definition = parallel_provider_definition();
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "parallel-provider",
        &definition,
    )
    .expect("identity");
    state
        .workflow_store
        .lock()
        .expect("store")
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .expect("registered");
    let result = crate::workflow_operations::start_run(
        &state,
        bcode_workflow::WorkflowRunStartRequest {
            definition_id: identity.definition_id,
            definition_version: identity.definition_version,
            run_id: Some("parallel-provider".into()),
            workspace_snapshot: "fixture".into(),
            parent_session_id: parent.id,
            parent_session_generation: None,
            binding: None,
            input: Some(serde_json::json!(1)),
            limits: bcode_workflow::WorkflowRunLimits::default(),
        },
        None,
    )
    .await
    .expect("normal authorized preparation and start");
    assert_eq!(result.run.run_id, "parallel-provider");
    crate::drive_workflow_run(&state, "parallel-provider")
        .await
        .expect("second scan");
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            crate::drive_workflow_run(&state, "parallel-provider")
                .await
                .expect("drive");
            if status(&state, "parallel-provider")
                .await
                .expect("status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("parallel completion");
    assert!(
        !state
            .runtime_work
            .active_all()
            .await
            .iter()
            .any(|(_, work)| work.work_id
                == bcode_session_models::WorkId::new("workflow:parallel-provider")),
        "terminal represented workflow must release its runtime registration"
    );
    assert!(
        state
            .workflow_store
            .lock()
            .expect("legacy")
            .run_summary("parallel-provider")
            .expect("legacy lookup")
            .is_none()
    );
    let sessions = state.sessions.all_session_summaries().await;
    assert_eq!(
        sessions
            .iter()
            .filter(|session| session
                .execution
                .as_ref()
                .is_some_and(|execution| execution.provenance.run_id == "parallel-provider"))
            .count(),
        2
    );
}

fn parallel_provider_definition() -> bcode_workflow::WorkflowDefinition {
    let config: bcode_workflow::WorkflowPromptConfiguration =
        serde_json::from_value(crate::tests::test_workflow_prompt_configuration(
            bcode_workflow::ValueSchema::of::<u32>(),
            bcode_workflow::PromptContextTarget::FreshIsolated,
        ))
        .expect("config");
    bcode_workflow::WorkflowBuilder::new(
        "parallel-provider",
        bcode_workflow::parallel_named(
            "join",
            bcode_workflow::Step::<u32, u32>::agent("left", &config).expect("left"),
            bcode_workflow::Step::<u32, u32>::agent("right", &config).expect("right"),
        ),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone()
}

#[tokio::test]
async fn authentic_loop_start_executes_plugin_judgement_and_two_provider_rounds() {
    let root = tempfile::tempdir().expect("root");
    let sessions = crate::SessionManager::persistent_lazy(root.path());
    let parent = sessions
        .create_session(None, root.path().into())
        .await
        .expect("parent");
    let store = bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path())
        .expect("baseline");
    let state = authentic_loop_server(sessions, store);
    let mut cleanup = FixtureCleanup::new(&state);
    let request = bcode_loop_plugin::test_loop_start_request(parent.id, 2).expect("authentic loop");
    let run_id = request.run_id.clone().expect("run identity");
    let response = crate::workflow_operations::start(
        &state,
        bcode_workflow::WorkflowStartRequest {
            identity: request.identity,
            definition: request.definition,
            run_id: request.run_id,
            parent_session_id: parent.id,
            workspace_snapshot: Some("fixture".into()),
            input: request.input,
            binding: bcode_workflow::WorkflowRunBinding {
                owner_plugin_id: request.binding.owner_plugin_id,
                workflow_kind: request.binding.workflow_kind,
                scope_key: request.binding.scope_key,
                display_label: request.binding.display_label,
                single_active: request.binding.single_active,
            },
            limits: bcode_workflow::WorkflowRunLimits {
                concurrency_cap: None,
                cycle_cap: 2,
                ..Default::default()
            },
        },
    )
    .await
    .expect("normal plugin start");
    assert_eq!(response.run.run_id, run_id);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            crate::drive_workflow_run(&state, &run_id)
                .await
                .expect("production drive");
            if status(&state, &run_id)
                .await
                .expect("status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("authentic two-round loop completes");
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    assert!(
        representation::attempts::pending(&db, &run_id, None, 100)
            .await
            .expect("settled")
            .is_empty()
    );
    assert!(
        state
            .workflow_store
            .lock()
            .expect("legacy")
            .run_summary(&run_id)
            .expect("legacy lookup")
            .is_none()
    );
    cleanup.finish().await;
    drop(cleanup);
    drop(state);
}

#[tokio::test]
async fn failed_receipt_is_terminal_not_repeated_scheduler_progress() {
    let (_root, _sessions_root, state, _) = typed_provider_fixture().await;
    let db = open(&crate::workflow_store_path(&state), true).expect("typed store");
    let guard = owner::local_authority(&state, "new")
        .await
        .expect("authority")
        .expect("local");
    let request = prepare_owner_fixture(&db, &guard.authority).await;
    attach_owner_fixture_receipt(&state, &request, &guard.authority).await;
    representation::attempts::settle(
        &db,
        &guard.authority,
        "new",
        &request.dispatch_identity,
        representation::attempts::TerminalOutcome::Failed,
        4,
    )
    .await
    .expect("confirmed failure");
    for _ in 0..3 {
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            crate::drive_workflow_run(&state, "new"),
        )
        .await
        .expect("terminal failure cannot hot-loop")
        .expect("settle failure");
    }
    assert_eq!(
        status(&state, "new")
            .await
            .expect("status")
            .expect("run")
            .status,
        bcode_workflow::RunStatus::Failed
    );
    assert!(
        representation::attempts::pending(&db, "new", None, 10)
            .await
            .expect("pending")
            .is_empty()
    );
    assert_eq!(
        representation::attempts::owner_boundary::terminal_outcome(
            &db,
            "new",
            &request.dispatch_identity
        )
        .await
        .expect("terminal ledger"),
        Some(representation::attempts::TerminalOutcome::Failed)
    );
}

struct ImmediateTerminalOwner;
impl bcode_workflow_store::ActivationDispatchOwner for ImmediateTerminalOwner {
    fn plan<'a>(
        &'a self,
        _: &'a bcode_workflow_store::PendingActivation,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Option<bcode_workflow_store::ActivationDispatchPlan>,
                        bcode_workflow_store::WorkflowStoreError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { panic!("fixture already prepared") })
    }
    fn dispatch<'a>(
        &'a self,
        _: &'a bcode_workflow_store::PreparedActivationDispatch,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<serde_json::Value, bcode_workflow_store::WorkflowStoreError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            let terminal_at_ms = crate::current_unix_millis();
            // Make completion strictly earlier than receipt persistence without changing clocks.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Ok(serde_json::json!({"terminal_at_ms": terminal_at_ms}))
        })
    }
}

#[tokio::test]
async fn synchronous_owner_completion_precedes_receipt_write_but_not_admission() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let authority = crate::tests::test_workflow_execution_authority();
    representation::attempts::claim_authority(&db, "new", &authority)
        .await
        .expect("authority");
    let request = prepare_owner_fixture(&db, &authority).await;
    driver::dispatch_and_record(&state, &authority, &request, &ImmediateTerminalOwner)
        .await
        .expect("handoff");
    let receipt =
        representation::attempts::owner_boundary::receipt(&db, &request.dispatch_identity)
            .await
            .expect("receipt")
            .expect("committed");
    let terminal_at_ms = receipt.receipt["terminal_at_ms"]
        .as_u64()
        .expect("terminal time");
    assert!(receipt.admitted_at_ms <= terminal_at_ms);
    assert!(terminal_at_ms < crate::current_unix_millis());
    let output = bcode_workflow_store::ValidatedOutput {
        output_id: "instant-output".into(),
        run_id: "new".into(),
        node_id: request.activation.node_id.clone(),
        activation_id: request.activation.activation_id.clone(),
        schema_id: request.activation.node.output.type_name.clone(),
        schema_version: 1,
        value: serde_json::json!(9),
        artifact_reference: None,
        created_at_ms: terminal_at_ms,
    };
    owner::settle_observation(
        &state,
        &request,
        &authority,
        bcode_workflow_store::AttemptObservation::Succeeded {
            output: output.clone(),
        },
    )
    .await
    .expect("truthful times settle");
    assert_eq!(
        representation::graph::completion::output(&db, "new", "instant-output")
            .await
            .expect("output"),
        Some(output)
    );
}

fn authentic_loop_server(
    sessions: crate::SessionManager,
    store: bcode_workflow_store::WorkflowStore,
) -> std::sync::Arc<ServerState> {
    let mut configured =
        crate::tests::test_server_state_with_workflow_authorization(sessions, store);
    configured
        .selected_provider_context
        .settings
        .insert("fake_structured_output_json".into(), "loop_until:1".into());
    configured.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: std::collections::BTreeSet::from([
                "bcode.loop".into(),
                "bcode.fake-provider".into(),
                "bcode.default-agents".into(),
            ]),
            disabled: std::collections::BTreeSet::new(),
        },
        &[
            bcode_bundled_plugins::static_loop_plugin(),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
                bcode_fake_provider_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::static_plugin(),
            ),
        ],
    )
    .expect("plugins");
    configured.selected_provider_plugin_id = Some("bcode.fake-provider".into());
    std::sync::Arc::new(configured)
}

fn configure_delegation_fixture(state: &mut ServerState) {
    state.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: std::collections::BTreeSet::from([
                "bcode.workflow".into(),
                "bcode.fake-provider".into(),
                "bcode.default-agents".into(),
            ]),
            disabled: std::collections::BTreeSet::new(),
        },
        &[
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/workflow-plugin/bcode-plugin.toml"),
                bcode_workflow_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
                bcode_fake_provider_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::static_plugin(),
            ),
        ],
    )
    .expect("plugins");
    state.selected_provider_plugin_id = Some("bcode.fake-provider".into());
    let gate = uuid::Uuid::new_v4().to_string();
    let script = bcode_fake_provider_plugin::FakeProviderEventScript {
        steps: vec![
            bcode_fake_provider_plugin::FakeProviderEventScriptStep {
                gate: Some(gate.clone()),
                delay_ms: None,
                event: bcode_model::ProviderTurnEvent::TextDelta { text: "9".into() },
            },
            bcode_fake_provider_plugin::FakeProviderEventScriptStep {
                gate: None,
                delay_ms: None,
                event: bcode_model::ProviderTurnEvent::TurnFinished {
                    stop_reason: bcode_model::StopReason::EndTurn,
                },
            },
        ],
    };
    state.selected_provider_context.settings.insert(
        "fake_event_script".into(),
        serde_json::to_string(&script).expect("script"),
    );
    state
        .selected_provider_context
        .settings
        .insert("delegation_fixture_gate".into(), gate);
    state.set_workflow_run_graph_edit_policy(crate::WorkflowRunGraphEditPolicy {
        evaluator: std::sync::Arc::new(|facts| {
            crate::workflow_operations::authorize_configured_run_graph_edit(
                facts,
                &std::collections::BTreeSet::from(["bcode.workflow".into()]),
            )
        }),
    });
    state.set_workflow_run_graph_publication_policy(crate::WorkflowRunGraphPublicationPolicy {
        evaluator: std::sync::Arc::new(|facts| {
            crate::workflow_operations::authorize_configured_run_graph_publication(
                facts,
                &std::collections::BTreeSet::from(["bcode.workflow".into()]),
                false,
            )
        }),
    });
}

#[tokio::test]
async fn typed_coordinator_stages_and_publishes_delegation_through_normal_tool() {
    tokio::time::timeout(std::time::Duration::from_secs(30), run_typed_delegation())
        .await
        .expect("whole delegation fixture deadline");
}

async fn run_typed_delegation() {
    let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
    let mut cleanup = FixtureCleanup::new(&state);
    let _gate = fanout::ReleaseGate(
        state.selected_provider_context.settings["delegation_fixture_gate"].clone(),
    );
    state.start_workflow_driver().await;
    crate::drive_workflow_run(&state, "new")
        .await
        .expect("coordinator dispatch");
    let child = state
        .sessions
        .all_session_summaries()
        .await
        .into_iter()
        .find(|session| {
            session
                .execution
                .as_ref()
                .is_some_and(|execution| execution.provenance.run_id == "new")
        })
        .expect("coordinator session");
    let activation = child
        .execution
        .as_ref()
        .expect("execution")
        .provenance
        .activation_id
        .clone()
        .expect("activation");
    let worker = |id: &str| {
        serde_json::json!({"task_id":id,"objective":"Return integer 9", "agent_profile":"build",
        "model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"},
        "output":bcode_workflow::ValueSchema::of::<u32>()})
    };
    let call = bcode_model::ToolCall {
        id: "typed-delegate".into(),
        name: "workflow.stage_delegation".into(),
        arguments: serde_json::json!({
            "run_id":"new", "expected_revision":1, "bind_source_activation":activation,
            "mutation_id":"two-workers", "tasks":[worker("left"),worker("right")],
            "continuation":{"objective":"Integrate worker results", "agent_profile":"build", "model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"}},
            "reconciliation":[]
        }),
    };
    let staged = invoke_delegation_tool(&state, child.id, &call).await;
    assert!(!staged.is_error, "{}", staged.output);
    let staged: serde_json::Value = serde_json::from_str(&staged.output).expect("stage response");
    let publish = staged
        .get("publication_arguments")
        .expect("canonical publication arguments")
        .clone();
    let publication = bcode_model::ToolCall {
        id: "typed-publish".into(),
        name: "workflow.publish_run_graph_edit".into(),
        arguments: publish,
    };
    let published = invoke_delegation_tool(&state, child.id, &publication).await;
    assert!(!published.is_error, "{}", published.output);
    bcode_fake_provider_plugin::release_fake_event_script_gate(
        &state.selected_provider_context.settings["delegation_fixture_gate"],
    );
    let finished = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            crate::drive_workflow_run(&state, "new")
                .await
                .expect("drive revised run");
            if status(&state, "new")
                .await
                .expect("status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    finished.expect("delegated completion");
    assert_delegation_sessions_are_unique(&state).await;
    cleanup.finish().await;
    drop(cleanup);
}

async fn assert_delegation_sessions_are_unique(state: &ServerState) {
    let children = state.sessions.all_session_summaries().await;
    for node in ["agent", "left", "right", "final"] {
        assert_eq!(
            children
                .iter()
                .filter(
                    |session| session.execution.as_ref().is_some_and(|execution| execution
                        .provenance
                        .run_id
                        == "new"
                        && execution.provenance.node_id == node)
                )
                .count(),
            1
        );
    }
}

async fn invoke_delegation_tool(
    state: &ServerState,
    session: crate::SessionId,
    call: &bcode_model::ToolCall,
) -> crate::ToolInvocationResponse {
    let (tool, preparation) = crate::prepare_server_tool(state, session, call)
        .await
        .expect("prepare");
    let metadata =
        crate::tool_policy_authorization_metadata(&preparation.authorization, &call.name)
            .expect("facts");
    let cancellation = crate::TurnCancelState::default();
    let invocation = crate::tests::invoke_prepared_tool_for_test(
        state,
        session,
        call,
        tool,
        preparation,
        &metadata,
        &cancellation,
    );
    tokio::pin!(invocation);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut invocation => return result.unwrap_or_else(|error| panic!("tool invocation {}: {error}", call.name)),
                () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                    let pending = state.pending_permissions.lock().await.values().next().cloned();
                    if let Some(pending) = pending {
                        state.pending_permissions.lock().await.remove(&pending.summary.permission_id);
                        *pending.decision.lock().await = Some(true);
                        pending.notify.notify_one();
                    }
                }
            }
        }
    }).await.expect("tool finishes through normal authorization")
}

#[tokio::test]
async fn public_cancellation_settles_real_gated_provider_without_late_reopen() {
    let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
    crate::drive_workflow_run(&state, "new")
        .await
        .expect("gated dispatch");
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    let attempt = representation::attempts::pending(&db, "new", None, 10)
        .await
        .expect("pending")
        .pop()
        .expect("active attempt");
    let receipt = attempt.receipt.expect("receipt-backed owner");
    assert!(
        crate::workflow_operations::cancel_run(&state, "new")
            .await
            .expect("normal cancel")
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if status(&state, "new")
                .await
                .expect("status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Cancelled
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("confirmed provider cancellation settles");
    bcode_fake_provider_plugin::release_fake_event_script_gate(
        &state.selected_provider_context.settings["delegation_fixture_gate"],
    );
    let session = receipt.receipt["session_id"]
        .as_str()
        .expect("session")
        .parse()
        .expect("id");
    let turn = receipt.receipt["turn_id"].as_str().expect("turn");
    let evidence = state
        .sessions
        .session_turn_evidence(session, turn)
        .await
        .expect("canonical evidence");
    assert!(evidence.iter().any(|event| matches!(
        event.kind,
        bcode_session_models::SessionEventKind::ModelTurnFinished {
            outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
            ..
        }
    )));
    for _ in 0..3 {
        crate::drive_workflow_run(&state, "new")
            .await
            .expect("late wake");
    }
    assert_eq!(
        status(&state, "new")
            .await
            .expect("status")
            .expect("run")
            .status,
        bcode_workflow::RunStatus::Cancelled
    );
    assert_eq!(
        representation::attempts::owner_boundary::terminal_outcome(
            &db,
            "new",
            &receipt.dispatch_identity
        )
        .await
        .expect("terminal"),
        Some(representation::attempts::TerminalOutcome::Cancelled)
    );
    assert!(
        representation::attempts::pending(&db, "new", None, 10)
            .await
            .expect("no live attempts")
            .is_empty()
    );
}

#[tokio::test]
async fn takeover_uses_real_lifetime_evidence_and_preserves_ambiguous_mutating_intent() {
    takeover_receipt_fixture(false).await;
    takeover_receipt_fixture(true).await;
}

async fn takeover_receipt_fixture(with_receipt: bool) {
    Box::pin(takeover_receipt_fixture_inner(with_receipt)).await;
}

async fn takeover_receipt_fixture_inner(with_receipt: bool) {
    let (_root, _sessions, original, _) = typed_provider_fixture().await;
    let mut state =
        std::sync::Arc::try_unwrap(original).unwrap_or_else(|_| panic!("exclusive fixture"));
    let evidence_root = tempfile::tempdir().expect("lifetime root");
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let old = representation::attempts::owner_boundary::execution_authority(&db, "new")
        .await
        .expect("authority")
        .expect("old");
    let record = replacement_lifetime_record(&mut state, evidence_root.path(), &old);
    assert!(
        recovery::takeover(&state, "new").await.is_err(),
        "missing evidence is not termination"
    );
    let lifetime = bcode_daemon_lifecycle::ExecutionLifetime::begin(evidence_root.path(), &record)
        .expect("live lifetime");
    assert!(
        recovery::takeover(&state, "new").await.is_err(),
        "live old owner cannot be stolen"
    );
    let activation = representation::graph::ready(&db, "new", 1, None, 1)
        .await
        .expect("ready")
        .pop()
        .expect("entry");
    let prepared = bcode_workflow_store::PreparedAttempt {
        run_id: "new".into(),
        node_id: activation.node_id,
        activation_id: activation.activation_id,
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::Mutating,
        intent: serde_json::json!({"effect":"ambiguous"}),
        prepared_at_ms: 2,
    };
    let dispatch = representation::attempts::prepare(&db, &old, &prepared)
        .await
        .expect("prepare before crash");
    attach_recovery_receipt(&db, &old, &prepared, &dispatch, with_receipt).await;
    drop(lifetime);
    let guard = recovery::takeover(&state, "new")
        .await
        .expect("verified ended owner");
    assert_eq!(guard.authority.generation, old.generation + 1);
    assert!(
        representation::safety::recovery_required(&db, "new")
            .await
            .expect("barrier")
    );
    recovery::reconcile(&state, "new", &guard.authority)
        .await
        .expect("bounded observe only");
    assert_eq!(
        representation::safety::recovery_required(&db, "new")
            .await
            .expect("barrier after observation"),
        !with_receipt
    );
    if with_receipt {
        assert_eq!(
            representation::graph::completion::output(&db, "new", &format!("{dispatch}:output"))
                .await
                .expect("output")
                .expect("retained success")
                .value,
            serde_json::json!(9)
        );
    }
    assert_eq!(
        representation::attempts::owner_boundary::prepared(&db, "new", &dispatch)
            .await
            .expect("intent"),
        Some(prepared)
    );
    assert_eq!(
        representation::attempts::owner_boundary::receipt(&db, &dispatch)
            .await
            .expect("receipt")
            .is_some(),
        with_receipt
    );
    assert!(
        state
            .sessions
            .all_session_summaries()
            .await
            .iter()
            .all(|session| session.execution.is_none()),
        "no duplicate external dispatch"
    );
    assert!(
        representation::attempts::owner_boundary::verify(&db, "new", &old)
            .await
            .is_err()
    );
    drop(guard);
    drop(state);
}

struct FixtureCleanup {
    state: std::sync::Arc<ServerState>,
    completed: bool,
}

impl FixtureCleanup {
    fn new(state: &std::sync::Arc<ServerState>) -> Self {
        Self {
            state: std::sync::Arc::clone(state),
            completed: false,
        }
    }

    async fn finish_run(&mut self, run_id: &str) {
        let run = status(&self.state, run_id)
            .await
            .expect("cleanup canonical status")
            .expect("cleanup run");
        assert!(
            matches!(
                run.status,
                bcode_workflow::RunStatus::Completed
                    | bcode_workflow::RunStatus::Failed
                    | bcode_workflow::RunStatus::Cancelled
            ),
            "whole canonical run must settle before successful cleanup"
        );
        self.finish().await;
    }

    async fn finish(&mut self) {
        drain_fixture_supervisor(&self.state)
            .await
            .expect("fixture supervisor settlement");
        stop_fixture_driver(&self.state).await;
        self.completed = true;
    }
}

impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        if let Some(gate) = self
            .state
            .selected_provider_context
            .settings
            .get("delegation_fixture_gate")
        {
            bcode_fake_provider_plugin::release_fake_event_script_gate(gate);
        }
        // Synchronous panic fallback requests normal shutdown, never retires live work.
        self.state
            .shutdown_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.state.shutdown.send(());
        let state = std::sync::Arc::clone(&self.state);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let cleanup_state = std::sync::Arc::clone(&state);
            let handle = runtime.spawn(async move {
                shutdown_fixture_driver(&cleanup_state, true).await;
            });
            // Keep the cleanup join handle even during panic unwinding. It has bounded waits.
            UNRESOLVED_FIXTURE_TASKS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(OwnedFixtureTask {
                    state,
                    handle: Some(handle),
                });
        } else {
            eprintln!("fixture cleanup unresolved: no runtime available; no drainage claimed");
        }
    }
}

// A task stays fixture-owned even when observation expires or its caller unwinds.
struct OwnedFixtureTask {
    state: std::sync::Arc<ServerState>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl OwnedFixtureTask {
    fn new(state: &std::sync::Arc<ServerState>, handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            state: std::sync::Arc::clone(state),
            handle: Some(handle),
        }
    }

    fn handle_mut(&mut self) -> &mut tokio::task::JoinHandle<()> {
        self.handle.as_mut().expect("owned fixture task")
    }

    // Call only after awaiting the join result; finished alone is not proof of a successful join.
    fn disarm_joined(&mut self) {
        let handle = self.handle.take().expect("joined fixture task");
        assert!(
            handle.is_finished(),
            "cannot disarm unresolved fixture work"
        );
        drop(handle);
    }
}

static UNRESOLVED_FIXTURE_TASKS: std::sync::Mutex<Vec<OwnedFixtureTask>> =
    std::sync::Mutex::new(Vec::new());

// Extract only this fixture's handles under the mutex, then join outside it.
async fn drain_fixture_supervisor(state: &std::sync::Arc<ServerState>) -> Result<(), &'static str> {
    let tasks = {
        let mut retained = UNRESOLVED_FIXTURE_TASKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut owned = Vec::new();
        let mut index = 0;
        while index < retained.len() {
            if std::sync::Arc::ptr_eq(&retained[index].state, state) {
                owned.push(retained.swap_remove(index));
            } else {
                index += 1;
            }
        }
        owned
    };
    let mut failed = false;
    for mut task in tasks {
        let joined =
            tokio::time::timeout(std::time::Duration::from_secs(2), task.handle_mut()).await;
        match joined {
            Ok(result) => {
                task.disarm_joined();
                failed |= result.is_err();
            }
            Err(_) => failed = true,
        }
        // A timed-out handle is re-retained by Drop; even a finished task must be joined.
        drop(task);
    }
    if failed {
        Err("fixture task join failed or remains unresolved")
    } else {
        Ok(())
    }
}

impl Drop for OwnedFixtureTask {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        if let Some(gate) = self
            .state
            .selected_provider_context
            .settings
            .get("delegation_fixture_gate")
        {
            bcode_fake_provider_plugin::release_fake_event_script_gate(gate);
        }
        eprintln!(
            "fixture task unresolved; retaining state and join handle (no settlement claimed)"
        );
        UNRESOLVED_FIXTURE_TASKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Self {
                state: std::sync::Arc::clone(&self.state),
                handle: Some(handle),
            });
    }
}

#[tokio::test]
async fn fixture_task_observation_timeout_does_not_drop_owned_work() {
    let state = std::sync::Arc::new(crate::tests::test_server_state(
        crate::SessionManager::default(),
    ));
    let (release, waiting) = tokio::sync::oneshot::channel::<()>();
    let mut task = OwnedFixtureTask::new(
        &state,
        tokio::spawn(async move {
            waiting.await.expect("owned fixture release");
        }),
    );
    assert!(
        tokio::time::timeout(std::time::Duration::ZERO, task.handle_mut())
            .await
            .is_err()
    );
    assert!(!task.handle_mut().is_finished());
    release.send(()).expect("fixture still owns waiting task");
    tokio::time::timeout(std::time::Duration::from_secs(2), task.handle_mut())
        .await
        .expect("fixture physical settlement")
        .expect("fixture joined");
    task.disarm_joined();
    drop(task);
}

#[tokio::test]
async fn fixture_supervisor_joins_and_reclaims_only_its_owned_tasks() {
    let state = std::sync::Arc::new(crate::tests::test_server_state(
        crate::SessionManager::default(),
    ));
    let weak = std::sync::Arc::downgrade(&state);
    let (release, waiting) = tokio::sync::oneshot::channel::<()>();
    let task = OwnedFixtureTask::new(
        &state,
        tokio::spawn(async move {
            waiting.await.expect("fixture release");
        }),
    );
    drop(task);
    release.send(()).expect("retained receiver");
    drain_fixture_supervisor(&state)
        .await
        .expect("join retained task");
    assert!(
        !UNRESOLVED_FIXTURE_TASKS
            .lock()
            .unwrap()
            .iter()
            .any(|task| std::sync::Arc::ptr_eq(&task.state, &state))
    );
    let mut cleanup = FixtureCleanup::new(&state);
    let mut shutdown = state.subscribe_shutdown();
    let (joined, driver_exited) = tokio::sync::oneshot::channel();
    *state.workflow_driver_task.lock().await = Some(tokio::spawn(async move {
        shutdown.recv().await.expect("successful cleanup shutdown");
        joined.send(()).expect("driver exit observed");
    }));
    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(!cleanup.completed, "shutdown request is not drainage");
    cleanup.finish().await;
    driver_exited
        .await
        .expect("real driver exited before guard disarm");
    assert!(state.workflow_driver_task.lock().await.is_none());
    assert!(state.runtime_work.active_all().await.is_empty());
    assert!(cleanup.completed);
    drop(cleanup);
    drop(state);
    assert!(weak.upgrade().is_none(), "joined fixture state reclaimed");
}

#[tokio::test]
async fn fixture_supervisor_reports_failed_join_without_claiming_cleanup() {
    let state = std::sync::Arc::new(crate::tests::test_server_state(
        crate::SessionManager::default(),
    ));
    let task = OwnedFixtureTask::new(
        &state,
        tokio::spawn(async { panic!("fixture task failure") }),
    );
    drop(task);
    assert!(drain_fixture_supervisor(&state).await.is_err());
    let mut cleanup = FixtureCleanup::new(&state);
    assert!(!cleanup.completed);
    cleanup.finish().await;
    drop(cleanup);
}

async fn fixture_stage_diagnostic(
    state: &ServerState,
    checkpoint: &str,
    started: std::time::Instant,
) {
    let observed = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        for (session, work) in state.runtime_work.active_all().await.into_iter().take(100) {
            eprintln!("fixture-stage={checkpoint} elapsed={:?} session={session} work={} kind={:?} status={:?} origin=runtime-registration-not-classified-synthetic", started.elapsed(), work.work_id, work.kind, work.status);
        }
        let Ok(db) = open(&crate::workflow_store_path(state), false) else { return; };
        let Ok(page) = representation::runs::catalog(&db, None, 8).await else { return; };
        for (run, _) in page.entries {
            if let Ok(pending) = representation::attempts::pending(&db, &run, None, 32).await {
                for attempt in pending {
                    eprintln!("fixture-stage={checkpoint} elapsed={:?} run={run} pending-dispatch={} activation={} attempt={} receipt-present={}", started.elapsed(), attempt.dispatch_identity, attempt.prepared.activation_id, attempt.prepared.attempt, attempt.receipt.is_some());
                }
            }
            if let Ok(snapshot) = representation::scheduling::snapshot(&db, &run, None, None, 32).await {
                eprintln!("fixture-stage={checkpoint} elapsed={:?} scheduling={snapshot:?}", started.elapsed());
                if let Ok(ready) = representation::graph::ready(&db, &run, snapshot.revision, None, 32).await {
                    for activation in ready {
                        eprintln!("fixture-stage={checkpoint} elapsed={:?} run={run} ready-activation={} node={} generation={}", started.elapsed(), activation.activation_id, activation.node_id, activation.generation);
                    }
                }
            }
        }
    }).await;
    if observed.is_err() {
        eprintln!(
            "fixture-stage={checkpoint} elapsed={:?} diagnostic=observation-timeout",
            started.elapsed()
        );
    }
}

async fn stop_fixture_driver(state: &std::sync::Arc<ServerState>) {
    // Successful callers must first establish whole-run canonical completion, not
    // an empty registry between predecessor retirement and successor admission.
    let started = std::time::Instant::now();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !state.runtime_work.active_all().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if drained.is_err() {
        fixture_stage_diagnostic(state, "registry-drain-before-shutdown-timeout", started).await;
        // Caller guard remains armed; its panic fallback retains cleanup ownership.
        panic!("fixture callback registrations unresolved before shutdown; no settlement claimed");
    }
    shutdown_fixture_driver(state, false).await;
}

async fn shutdown_fixture_driver(state: &std::sync::Arc<ServerState>, failed_cleanup: bool) {
    let started = std::time::Instant::now();
    // Only the uniquely configured gate belongs to this fixture; never reset global gates.
    if let Some(gate) = state
        .selected_provider_context
        .settings
        .get("delegation_fixture_gate")
    {
        bcode_fake_provider_plugin::release_fake_event_script_gate(gate);
    }
    fixture_stage_diagnostic(state, "gate-released-before-shutdown", started).await;
    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = state.shutdown.send(());
    let task = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.workflow_driver_task.lock(),
    )
    .await
    .expect("fixture driver handle lock unresolved")
    .take();
    if let Some(task) = task {
        let mut task = OwnedFixtureTask::new(state, task);
        let joined =
            tokio::time::timeout(std::time::Duration::from_secs(2), task.handle_mut()).await;
        if let Ok(result) = joined {
            result.expect("driver task");
            task.disarm_joined();
            drop(task);
        } else {
            // Drop retains this exact live task; neither abort nor event retirement proves settlement.
            fixture_stage_diagnostic(state, "driver-join-timeout", started).await;
            panic!("driver teardown unresolved; live join handle retained");
        }
    }
    fixture_stage_diagnostic(state, "after-driver-join", started).await;
    if !failed_cleanup {
        assert!(
            state.runtime_work.active_all().await.is_empty(),
            "work registered during successful driver join; no settlement claimed"
        );
        return;
    }
    let drained = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !state.runtime_work.active_all().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if drained.is_err() {
        fixture_stage_diagnostic(state, "registry-drain-timeout", started).await;
    }
    assert!(
        drained.is_ok(),
        "fixture runtime effects remain unresolved; bounded registry drain expired"
    );
}

fn replacement_lifetime_record(
    state: &mut ServerState,
    evidence_root: &std::path::Path,
    old: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> bcode_daemon_lifecycle::DaemonRecord {
    let mut record = bcode_daemon_lifecycle::DaemonRecord::current(
        &bcode_ipc::default_endpoint(),
        evidence_root.join("daemon.log"),
        None,
        old.daemon_instance_id.clone(),
    )
    .expect("record");
    record.artifact_id =
        Some(bcode_ipc::ArtifactId::parse(&old.target_artifact_id).expect("artifact"));
    state.state_root = evidence_root.into();
    state
        .daemon_status
        .state_location_id
        .clone_from(&record.state_location_id);
    state.daemon_status.instance_id = "replacement-daemon".into();
    record
}

async fn attach_recovery_receipt(
    db: &dyn Database,
    old: &bcode_workflow_store::WorkflowExecutionAuthority,
    prepared: &bcode_workflow_store::PreparedAttempt,
    dispatch: &str,
    with_receipt: bool,
) {
    if with_receipt {
        representation::attempts::attach_receipt(db, old, &bcode_workflow_store::DispatchReceipt {
            run_id: "new".into(), node_id: prepared.node_id.clone(), activation_id: prepared.activation_id.clone(),
            attempt: prepared.attempt, dispatch_identity: dispatch.to_owned(), admitted_at_ms: 3,
            receipt: serde_json::json!({"owner":bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID,"status":"succeeded","output":9,
                "output_schema_id":bcode_workflow::ValueSchema::of::<u32>().type_name,"terminal_at_ms":4}),
        }).await.expect("durable external effect receipt before owner exit");
    }
}

#[tokio::test]
async fn represented_acceptance_recovery_is_exact_idempotent_and_never_redispatches() {
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let authority = owner::local_authority(&state, "new")
        .await
        .expect("owner")
        .expect("local owner");
    let dispatch = prepare_acceptance_recovery_fixture(&state, &db, &authority.authority).await;
    let prepared =
        representation::attempts::owner_boundary::prepared(&db, "new", &dispatch.dispatch_identity)
            .await
            .expect("prepared lookup")
            .expect("prepared");
    assert!(
        recovery::recover_acceptance(&state, &prepared, &authority.authority)
            .await
            .expect("missing evidence")
            .is_none()
    );
    assert!(
        crate::drive_workflow_run(&state, "new").await.is_err(),
        "receipt-less preparation without acceptance must not be redispatched"
    );
    interrupt_after_canonical_acceptance(&state, &dispatch).await;
    assert!(
        representation::attempts::owner_boundary::receipt(&db, &dispatch.dispatch_identity)
            .await
            .expect("receipt lookup")
            .is_none()
    );
    let session = acceptance_recovery_session(&state, &prepared).await;
    let before = state
        .sessions
        .turn_receipt(session, "bcode.workflow", &dispatch.dispatch_identity)
        .await
        .expect("canonical acceptance")
        .expect("accepted");
    let mut invalid_authority = authority.authority.clone();
    invalid_authority.generation += 1;
    assert!(
        recovery::recover_acceptance(&state, &prepared, &invalid_authority)
            .await
            .is_err()
    );
    crate::drive_workflow_run(&state, "new")
        .await
        .expect("live same-owner driver reconciles acceptance without a recovery barrier");
    let recovered = recovery::recover_acceptance(&state, &prepared, &authority.authority)
        .await
        .expect("recover acceptance")
        .expect("recovered receipt");
    let accepted_event = state
        .sessions
        .session_history_around(
            session,
            bcode_session_models::SessionHistoryAroundQuery {
                sequence: before.accepted_event_sequence,
                before: 0,
                after: 0,
            },
        )
        .await
        .expect("acceptance event")
        .events
        .pop()
        .expect("event");
    assert_eq!(recovered.admitted_at_ms, accepted_event.timestamp_ms);
    assert_eq!(recovered.receipt["turn_id"], before.turn_id.to_string());
    assert_eq!(
        recovered.receipt["accepted_event_sequence"],
        before.accepted_event_sequence
    );
    assert_eq!(
        recovery::recover_acceptance(&state, &prepared, &authority.authority)
            .await
            .expect("duplicate recovery"),
        Some(recovered)
    );
    assert_eq!(
        state
            .sessions
            .turn_receipt(session, "bcode.workflow", &dispatch.dispatch_identity)
            .await
            .expect("same acceptance"),
        Some(before)
    );
    assert_eq!(
        state
            .sessions
            .all_session_summaries()
            .await
            .iter()
            .filter(|summary| summary.execution.is_some())
            .count(),
        1
    );
    stop_fixture_driver(&state).await;
}

#[tokio::test]
async fn pending_receipt_publication_cancellation_retains_accepted_turn_observer() {
    // Keep the existing agent leaf, without delegation's downstream final activation.
    let (_root, _sessions, mut state, definition) = typed_provider_fixture().await;
    configure_delegation_fixture(
        std::sync::Arc::get_mut(&mut state).expect("exclusive fixture configuration"),
    );
    assert_eq!(definition.nodes.len(), 1);
    let mut cleanup = FixtureCleanup::new(&state);
    let gate = state.selected_provider_context.settings["delegation_fixture_gate"].clone();
    let _provider_release = fanout::ReleaseGate(gate.clone());
    let path = crate::workflow_store_path(&state);
    let db = open(&path, true).expect("db");
    let authority = owner::local_authority(&state, "new")
        .await
        .expect("owner")
        .expect("local");
    let (unrelated, unchanged) =
        publication_unrelated_fixture(&db, &definition, &authority.authority).await;
    let dispatch = prepare_acceptance_recovery_fixture(&state, &db, &authority.authority).await;
    let (notification, pending) = tokio::sync::oneshot::channel();
    let _observation =
        owner::observe_receipt_publication_wait(&path, &dispatch.dispatch_identity, notification)
            .expect("observe exact receipt write");
    let (transfer, acquired) = tokio::sync::oneshot::channel();
    let owned_state = std::sync::Arc::clone(&state);
    let owned_dispatch = dispatch.clone();
    let mut outer = tokio::spawn(async move {
        crate::dispatch_workflow_prompt_turn_with_publication_hook(
            &owned_state,
            &owned_dispatch,
            || async move {
                let queue = backpressure::write_queue(&path).expect("canonical fixture queue");
                let held = queue.lock_owned().await;
                transfer
                    .send(held)
                    .expect("transfer fixture-owned write guard");
                Ok(())
            },
        )
        .await
    });
    let held = tokio::time::timeout(std::time::Duration::from_secs(10), acquired)
        .await
        .expect("write guard bound")
        .expect("write guard acquired");
    tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .expect("actual publication pending bound")
        .expect("actual lock future polled Pending");
    outer.abort();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut outer)
            .await
            .expect("outer joined")
            .expect_err("outer cancelled")
            .is_cancelled()
    );
    let (session, accepted) =
        publication_accepted_receipt(&state, &db, &dispatch.dispatch_identity).await;
    wait_publication_provider_gate(&gate).await;
    assert_single_publication_acceptance(
        &state,
        session,
        &dispatch.dispatch_identity,
        &accepted,
        false,
    )
    .await;
    assert!(
        representation::attempts::owner_boundary::receipt(&db, &dispatch.dispatch_identity)
            .await
            .expect("workflow receipt absent")
            .is_none()
    );
    drop(held);
    bcode_fake_provider_plugin::release_fake_event_script_gate(&gate);
    wait_publication_observer_settlement(&state, &db, &dispatch.dispatch_identity).await;
    assert_single_publication_acceptance(
        &state,
        session,
        &dispatch.dispatch_identity,
        &accepted,
        true,
    )
    .await;
    assert_eq!(
        representation::scheduling::snapshot(&db, &unrelated.run_id, None, None, 10)
            .await
            .expect("unrelated unchanged"),
        unchanged
    );
    cleanup.finish_run("new").await;
    drop(cleanup);
}

async fn publication_accepted_receipt(
    state: &ServerState,
    db: &dyn Database,
    dispatch_identity: &str,
) -> (crate::SessionId, bcode_session_models::TurnReceipt) {
    let prepared = representation::attempts::owner_boundary::prepared(db, "new", dispatch_identity)
        .await
        .expect("prepared")
        .expect("attempt");
    let session = acceptance_recovery_session(state, &prepared).await;
    let accepted = state
        .sessions
        .turn_receipt(session, "bcode.workflow", dispatch_identity)
        .await
        .expect("canonical receipt")
        .expect("accepted");
    (session, accepted)
}

async fn wait_publication_provider_gate(gate: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !bcode_fake_provider_plugin::fake_event_script_gate_ready(gate) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted provider remains gated");
}

#[tokio::test]
async fn publication_cancellation_retains_accepted_turn_observer() {
    let (_root, _sessions, state, definition) = typed_provider_fixture_with_delegation(true).await;
    let mut cleanup = FixtureCleanup::new(&state);
    let gate = state.selected_provider_context.settings["delegation_fixture_gate"].clone();
    let _provider_release = fanout::ReleaseGate(gate.clone());
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let authority = owner::local_authority(&state, "new")
        .await
        .expect("owner")
        .expect("local");
    let (unrelated, unchanged) =
        publication_unrelated_fixture(&db, &definition, &authority.authority).await;
    let dispatch = prepare_acceptance_recovery_fixture(&state, &db, &authority.authority).await;
    let (announced, reached) = tokio::sync::oneshot::channel();
    let (_publication_release, waiting) = tokio::sync::oneshot::channel::<()>();
    let owned_state = std::sync::Arc::clone(&state);
    let owned_dispatch = dispatch.clone();
    let mut outer = tokio::spawn(async move {
        crate::dispatch_workflow_prompt_turn_with_publication_hook(
            &owned_state,
            &owned_dispatch,
            || async move {
                announced.send(()).expect("hook announcement");
                waiting.await.expect("publication release");
                Ok(())
            },
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached)
        .await
        .expect("acceptance hook bound")
        .expect("hook reached");
    outer.abort();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut outer)
            .await
            .expect("outer joined")
            .expect_err("outer cancelled")
            .is_cancelled()
    );
    let prepared =
        representation::attempts::owner_boundary::prepared(&db, "new", &dispatch.dispatch_identity)
            .await
            .expect("prepared")
            .expect("attempt");
    let session = acceptance_recovery_session(&state, &prepared).await;
    let accepted = state
        .sessions
        .turn_receipt(session, "bcode.workflow", &dispatch.dispatch_identity)
        .await
        .expect("canonical receipt")
        .expect("accepted");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !bcode_fake_provider_plugin::fake_event_script_gate_ready(&gate) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted provider reaches owned gate");
    assert!(
        representation::attempts::owner_boundary::receipt(&db, &dispatch.dispatch_identity)
            .await
            .expect("workflow receipt")
            .is_none()
    );
    assert_single_publication_acceptance(
        &state,
        session,
        &dispatch.dispatch_identity,
        &accepted,
        false,
    )
    .await;
    bcode_fake_provider_plugin::release_fake_event_script_gate(&gate);
    wait_publication_observer_settlement(&state, &db, &dispatch.dispatch_identity).await;
    assert_single_publication_acceptance(
        &state,
        session,
        &dispatch.dispatch_identity,
        &accepted,
        true,
    )
    .await;
    assert_eq!(
        state
            .sessions
            .turn_receipt(session, "bcode.workflow", &dispatch.dispatch_identity)
            .await
            .expect("unchanged acceptance"),
        Some(accepted)
    );
    assert_eq!(
        representation::scheduling::snapshot(&db, &unrelated.run_id, None, None, 10)
            .await
            .expect("unrelated after"),
        unchanged
    );
    cleanup.finish_run("new").await;
    drop(cleanup);
}

async fn wait_publication_observer_settlement(
    state: &ServerState,
    db: &dyn Database,
    dispatch_identity: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let terminal = representation::attempts::owner_boundary::terminal_outcome(
                db,
                "new",
                dispatch_identity,
            )
            .await
            .expect("settlement")
            .is_some();
            let completed = status(state, "new")
                .await
                .expect("whole-run status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Completed;
            // Empty between agent retirement and successor admission is not settlement.
            if terminal && completed && state.runtime_work.active_all().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retained observer settles and drains accepted work");
}

async fn publication_unrelated_fixture(
    db: &dyn Database,
    definition: &bcode_workflow::WorkflowDefinition,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> (
    bcode_workflow_store::NewWorkflowRun,
    representation::scheduling::SchedulingSnapshot,
) {
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("typed-agent", definition)
            .expect("identity");
    let mut unrelated = request("unrelated-publication");
    unrelated.definition_id = identity.definition_id;
    unrelated.definition_version = identity.definition_version;
    unrelated.execution_authority = Some(authority.clone());
    representation::admit(db, &unrelated, definition)
        .await
        .expect("unrelated admission");
    let unchanged = representation::scheduling::snapshot(db, &unrelated.run_id, None, None, 10)
        .await
        .expect("unrelated before");
    (unrelated, unchanged)
}

async fn assert_single_publication_acceptance(
    state: &ServerState,
    session: crate::SessionId,
    dispatch_identity: &str,
    accepted: &bcode_session_models::TurnReceipt,
    terminal: bool,
) {
    assert_eq!(
        state
            .sessions
            .turn_receipt(session, "bcode.workflow", dispatch_identity)
            .await
            .expect("canonical idempotent acceptance"),
        Some(accepted.clone())
    );
    assert_eq!(
        *accepted,
        bcode_session_models::TurnReceipt::from_accepted_event(
            session,
            accepted.accepted_event_sequence
        )
    );
    let page = state
        .sessions
        .session_history_around(
            session,
            bcode_session_models::SessionHistoryAroundQuery {
                sequence: accepted.accepted_event_sequence,
                before: 0,
                after: 0,
            },
        )
        .await
        .expect("bounded canonical acceptance event");
    assert_eq!(page.events.len(), 1);
    let event = &page.events[0];
    assert_eq!(event.sequence, accepted.accepted_event_sequence);
    let admission = match &event.kind {
        bcode_session_models::SessionEventKind::UserMessage { admission, .. } => admission,
        bcode_session_models::SessionEventKind::SubmissionAccepted { submission } => {
            &submission.admission
        }
        _ => panic!("receipt sequence must identify canonical admission"),
    };
    assert_eq!(
        admission.idempotency_identity(),
        Some(("bcode.workflow", dispatch_identity))
    );
    let evidence = state
        .sessions
        .session_turn_evidence(session, &accepted.turn_id.to_string())
        .await
        .expect("canonical terminal evidence");
    assert_eq!(
        evidence.iter().any(|event| matches!(
            event.kind,
            bcode_session_models::SessionEventKind::ModelTurnFinished { .. }
        )),
        terminal
    );
}

async fn interrupt_after_canonical_acceptance(
    state: &std::sync::Arc<ServerState>,
    dispatch: &bcode_workflow_store::PreparedActivationDispatch,
) {
    let started = std::time::Instant::now();
    fixture_stage_diagnostic(state, "before-real-acceptance-interruption", started).await;
    assert!(
        crate::dispatch_workflow_prompt_turn_after_admission(state, dispatch, || {
            Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                "interrupted after canonical acceptance before workflow receipt".into(),
            ))
        })
        .await
        .is_err()
    );
    fixture_stage_diagnostic(state, "after-real-acceptance-interruption", started).await;
}

async fn acceptance_recovery_session(
    state: &ServerState,
    prepared: &bcode_workflow_store::PreparedAttempt,
) -> bcode_session_models::SessionId {
    owner::execution_session_link(
        state,
        &prepared.run_id,
        &prepared.node_id,
        &prepared.activation_id,
        prepared.attempt,
    )
    .await
    .expect("link lookup")
    .expect("exact link")
    .session_id
    .parse()
    .expect("session")
}

#[tokio::test]
async fn represented_acceptance_recovery_live_owner_cancel_settles_exact_turn() {
    let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let authority = owner::local_authority(&state, "new")
        .await
        .expect("owner")
        .expect("local");
    let dispatch = prepare_acceptance_recovery_fixture(&state, &db, &authority.authority).await;
    interrupt_after_canonical_acceptance(&state, &dispatch).await;
    let prepared =
        representation::attempts::owner_boundary::prepared(&db, "new", &dispatch.dispatch_identity)
            .await
            .expect("prepared lookup")
            .expect("prepared");
    let session = acceptance_recovery_session(&state, &prepared).await;
    let accepted = state
        .sessions
        .turn_receipt(session, "bcode.workflow", &dispatch.dispatch_identity)
        .await
        .expect("canonical acceptance")
        .expect("accepted");
    crate::drive_workflow_run(&state, "new")
        .await
        .expect("same owner recovers receipt");
    state.start_workflow_driver().await;
    assert!(
        crate::workflow_operations::cancel_run(&state, "new")
            .await
            .expect("cancel")
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if status(&state, "new")
                .await
                .expect("status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Cancelled
            {
                break;
            }
            crate::drive_workflow_run(&state, "new")
                .await
                .expect("observe cancellation");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("canonical cancellation settlement");
    let evidence = state
        .sessions
        .session_turn_evidence(session, &accepted.turn_id.to_string())
        .await
        .expect("turn evidence");
    assert!(evidence.iter().any(|event| matches!(
        event.kind,
        bcode_session_models::SessionEventKind::ModelTurnFinished {
            outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
            ..
        }
    )));
    assert_eq!(
        state
            .sessions
            .turn_receipt(session, "bcode.workflow", &dispatch.dispatch_identity)
            .await
            .expect("unchanged acceptance"),
        Some(accepted)
    );
    bcode_fake_provider_plugin::release_fake_event_script_gate(
        &state.selected_provider_context.settings["delegation_fixture_gate"],
    );
    stop_fixture_driver(&state).await;
}

async fn prepare_acceptance_recovery_fixture(
    state: &std::sync::Arc<ServerState>,
    db: &dyn Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> bcode_workflow_store::PreparedActivationDispatch {
    use bcode_workflow_store::ActivationDispatchOwner;
    let (revision, definition) = representation::graph_revision::current(db, "new")
        .await
        .expect("graph");
    let activation = representation::graph::ready(db, "new", revision, None, 1)
        .await
        .expect("ready")
        .pop()
        .expect("activation");
    let pending = bcode_workflow_store::PendingActivation {
        run_id: "new".into(),
        node_id: activation.node_id.clone(),
        activation_id: activation.activation_id.clone(),
        dependency_generation: activation.generation,
        input: Some(activation.input),
        node: definition.node(&activation.node_id).expect("node").clone(),
        created_at_ms: 1,
    };
    let plan = crate::WorkflowPromptTurnOwner { state }
        .plan(&pending)
        .await
        .expect("plan")
        .expect("agent plan");
    let prepared = bcode_workflow_store::PreparedAttempt {
        run_id: "new".into(),
        node_id: pending.node_id.clone(),
        activation_id: pending.activation_id.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::Mutating,
        intent: plan.intent,
        prepared_at_ms: 2,
    };
    let identity = representation::attempts::prepare(db, authority, &prepared)
        .await
        .expect("prepare");
    bcode_workflow_store::PreparedActivationDispatch {
        activation: pending,
        attempt: 1,
        dispatch_identity: identity,
        intent: prepared.intent,
    }
}

#[tokio::test]
async fn normal_pause_blocks_new_dispatch_and_resume_wakes_same_typed_run() {
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    state.start_workflow_driver().await;
    assert!(
        crate::workflow_operations::pause_run(&state, "new")
            .await
            .expect("pause")
    );
    assert!(
        !crate::workflow_operations::pause_run(&state, "new")
            .await
            .expect("duplicate pause")
    );
    crate::recover_workflow_continuation(&state, "new").await;
    crate::drive_workflow_run(&state, "new")
        .await
        .expect("paused drive");
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    assert!(
        representation::attempts::pending(&db, "new", None, 10)
            .await
            .expect("no attempts")
            .is_empty()
    );
    assert!(
        crate::workflow_operations::resume_run(&state, "new")
            .await
            .expect("resume")
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if status(&state, "new")
                .await
                .expect("status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("normal scheduled resume completes");
    cleanup.finish().await;
    drop(cleanup);
}

#[tokio::test]
async fn normal_input_resolution_preserves_gate_identity_and_resumes_provider_work() {
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let parent = state
        .sessions
        .create_session(None, std::path::PathBuf::from("."))
        .await
        .expect("parent");
    let definition = input_resume_definition();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("input-resume", &definition)
            .expect("identity");
    state
        .workflow_store
        .lock()
        .expect("store")
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .expect("definition");
    crate::workflow_operations::start_run(
        &state,
        bcode_workflow::WorkflowRunStartRequest {
            definition_id: identity.definition_id,
            definition_version: identity.definition_version,
            run_id: Some("input-resume".into()),
            workspace_snapshot: "fixture".into(),
            parent_session_id: parent.id,
            parent_session_generation: None,
            binding: None,
            input: Some(serde_json::json!(1)),
            limits: bcode_workflow::WorkflowRunLimits::default(),
        },
        None,
    )
    .await
    .expect("normal waiting run");
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    let activation = representation::graph::activation_id(&db, "input-resume", "question")
        .await
        .expect("id")
        .expect("gate");
    assert!(
        crate::workflow_operations::provide_input(
            &state,
            "input-resume",
            "question",
            "wrong-activation",
            serde_json::json!(9)
        )
        .await
        .is_err()
    );
    assert!(
        crate::workflow_operations::provide_input(
            &state,
            "input-resume",
            "question",
            &activation,
            serde_json::json!("bad schema")
        )
        .await
        .is_err()
    );
    let resolution = crate::workflow_operations::provide_input(
        &state,
        "input-resume",
        "question",
        &activation,
        serde_json::json!(9),
    )
    .await
    .expect("normal input resolution");
    assert_eq!(resolution.outcome, "accepted");
    assert!(
        resolution
            .activated
            .iter()
            .all(|activation| activation.created_at_ms > 0)
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            crate::drive_workflow_run(&state, "input-resume")
                .await
                .expect("drive");
            if status(&state, "input-resume")
                .await
                .expect("status")
                .expect("run")
                .status
                == bcode_workflow::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("resolved run completes");
}

#[tokio::test]
async fn normal_approval_denial_is_not_success_and_wrong_gate_kind_rejects() {
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let parent = state
        .sessions
        .create_session(None, std::path::PathBuf::from("."))
        .await
        .expect("parent");
    let definition = bcode_workflow::WorkflowBuilder::new(
        "approval",
        bcode_workflow::Step::<u32, u32>::approval("decision"),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("approval", &definition)
            .expect("identity");
    state
        .workflow_store
        .lock()
        .expect("store")
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .expect("definition");
    crate::workflow_operations::start_run(
        &state,
        bcode_workflow::WorkflowRunStartRequest {
            definition_id: identity.definition_id,
            definition_version: identity.definition_version,
            run_id: Some("approval".into()),
            workspace_snapshot: "fixture".into(),
            parent_session_id: parent.id,
            parent_session_generation: None,
            binding: None,
            input: Some(serde_json::json!(1)),
            limits: bcode_workflow::WorkflowRunLimits::default(),
        },
        None,
    )
    .await
    .expect("waiting approval");
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    let activation = representation::graph::activation_id(&db, "approval", "decision")
        .await
        .expect("id")
        .expect("activation");
    assert!(
        crate::workflow_operations::provide_input(
            &state,
            "approval",
            "decision",
            &activation,
            serde_json::json!(1)
        )
        .await
        .is_err()
    );
    let result = crate::workflow_operations::resolve_approval(
        &state,
        "approval",
        "decision",
        &activation,
        false,
    )
    .await
    .expect("deny");
    assert_eq!(result.outcome, "denied");
    assert_ne!(
        status(&state, "approval")
            .await
            .expect("status")
            .expect("run")
            .status,
        bcode_workflow::RunStatus::Completed
    );
}

fn input_resume_definition() -> bcode_workflow::WorkflowDefinition {
    let config: bcode_workflow::WorkflowPromptConfiguration =
        serde_json::from_value(crate::tests::test_workflow_prompt_configuration(
            bcode_workflow::ValueSchema::of::<u32>(),
            bcode_workflow::PromptContextTarget::FreshIsolated,
        ))
        .expect("config");
    bcode_workflow::WorkflowBuilder::new(
        "input-resume",
        bcode_workflow::Step::<u32, u32>::input("question")
            .then(bcode_workflow::Step::<u32, u32>::agent("answer", &config).expect("agent")),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone()
}
