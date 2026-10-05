#[path = "tests/child.rs"]
mod child;
#[path = "tests/fanout.rs"]
mod fanout;
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
    let mut run = request("new");
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    run.limits.concurrency_cap = None;
    representation::admit(&db, &run, &definition)
        .await
        .expect("extension admission");
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

#[tokio::test]
async fn retained_adapter_handle_survives_server_handle_replacement() {
    let (_root, state) = fixture_with_store(true);
    let retained = retained_store(&state).expect("retained maintenance fence");
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

async fn typed_provider_fixture() -> (
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
    let config: bcode_workflow::WorkflowPromptConfiguration =
        serde_json::from_value(crate::tests::test_workflow_prompt_configuration(
            schema,
            bcode_workflow::PromptContextTarget::FreshIsolated,
        ))
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
    stop_fixture_driver(&state).await;
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

async fn stop_fixture_driver(state: &ServerState) {
    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = state.shutdown.send(());
    let task = state.workflow_driver_task.lock().await.take();
    if let Some(task) = task {
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("driver teardown")
            .expect("driver task");
    }
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
async fn normal_pause_blocks_new_dispatch_and_resume_wakes_same_typed_run() {
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
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
    stop_fixture_driver(&state).await;
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
