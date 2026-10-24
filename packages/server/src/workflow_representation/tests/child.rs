use super::*;
use bcode_workflow::{
    WorkflowCallTarget, WorkflowDefinitionIdentity, WorkflowTransformExpression as Expr,
};

#[tokio::test]
async fn package_root_and_finite_recursive_children_use_production_dispatch() {
    let _workflow_runtime_guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    if crate::tests::run_in_isolated_test_process(
        "workflow_representation::tests::child::package_root_and_finite_recursive_children_use_production_dispatch",
    )
    .await
    {
        return;
    }
    let (_root, _sessions, state, leaf, admission) = recursive_provider_fixture().await;
    let db = open(&crate::workflow_store_path(&state), true).unwrap();
    let definition = recursive_package_definition(&leaf);
    let identity = WorkflowDefinitionIdentity::for_definition("recursive", &definition).unwrap();
    let lock = recursive_package_lock(&identity);
    let digest = lock.digest_sha256().unwrap();
    publish_recursive_package(&state, &db, &definition, &identity, &lock, &digest).await;
    let root = start_recursive_package(&state, &db, admission, &identity, &lock, &digest).await;
    assert_package_resolution(&db, &definition, &lock, &digest).await;
    let expected_binding = bcode_workflow_store::WorkflowRunPackageBinding {
        version: 1,
        package_id: lock.package_id,
        lock_digest: digest,
    };
    assert_recursive_completion(&state, &db, &root, &expected_binding).await;
}

async fn recursive_provider_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    std::sync::Arc<ServerState>,
    bcode_workflow::WorkflowDefinition,
    bcode_workflow_store::NewWorkflowRun,
) {
    let root = tempfile::tempdir().expect("workflow root");
    let sessions_root = tempfile::tempdir().expect("sessions root");
    let sessions = crate::SessionManager::persistent_lazy(sessions_root.path());
    let parent = sessions
        .create_session(
            Some("recursive-owner".into()),
            std::path::PathBuf::from("."),
        )
        .await
        .expect("parent");
    let store = bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path())
        .expect("baseline");
    let mut state = std::sync::Arc::new(
        crate::tests::test_server_state_with_workflow_authorization(sessions, store),
    );
    configure_recursive_provider(
        std::sync::Arc::get_mut(&mut state).expect("unshared fixture state"),
    );
    let config: bcode_workflow::WorkflowPromptConfiguration =
        serde_json::from_value(crate::tests::test_workflow_prompt_configuration(
            bcode_workflow::ValueSchema::of::<u32>(),
            bcode_workflow::PromptContextTarget::FreshIsolated,
        ))
        .expect("configuration");
    let leaf = bcode_workflow::WorkflowBuilder::new(
        "typed-agent",
        bcode_workflow::Step::<u32, u32>::agent("agent", &config).expect("agent"),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone();
    let mut admission = super::request("package-recursive");
    admission.parent_session_id = Some(parent.id.to_string());
    admission.limits.concurrency_cap = None;
    // Only public root startup admits executable work. No incidental `new` run is seeded.
    (root, sessions_root, state, leaf, admission)
}

fn configure_recursive_provider(state: &mut ServerState) {
    state
        .selected_provider_context
        .settings
        .insert("fake_structured_output_json".into(), "9".into());
}

fn recursive_package_definition(
    leaf: &bcode_workflow::WorkflowDefinition,
) -> bcode_workflow::WorkflowDefinition {
    let mut definition = leaf.clone();
    let mut branch = leaf.nodes["agent"].clone();
    branch.id = "branch".into();
    branch.name = "branch".into();
    branch.kind = bcode_workflow::NodeKind::Branch;
    let predicate = bcode_workflow::field::<u32>("")
        .eq(0_u32)
        .expression()
        .clone();
    branch.configuration = serde_json::json!({
        "predicate_version": bcode_workflow::WORKFLOW_PREDICATE_VERSION,
        "predicate": predicate, "true_entries":["agent"], "false_entries":["recurse"],
        "true_nodes":["agent"], "false_nodes":["recurse"]
    });
    let mut call = leaf.nodes["agent"].clone();
    call.id = "recurse".into();
    call.name = "recurse".into();
    call.kind = bcode_workflow::NodeKind::WorkflowCall;
    call.configuration = serde_json::to_value(bcode_workflow::WorkflowCallConfiguration {
        version: bcode_workflow::WORKFLOW_CALL_VERSION,
        target: WorkflowCallTarget::PackageLocal {
            reference: bcode_workflow::WorkflowPackageLocalCall {
                version: 1,
                member_id: "recursive".into(),
                workflow_id: "recursive".into(),
                input: leaf.input.clone(),
                output: leaf.output.clone(),
            },
        },
        input: None,
        output: None,
    })
    .unwrap();
    let mut result = leaf.nodes["agent"].clone();
    result.id = "result".into();
    result.name = "result".into();
    definition.nodes.insert("result".into(), result);
    definition.nodes.insert("branch".into(), branch);
    definition.nodes.insert("recurse".into(), call);
    definition.entries = vec!["branch".into()];
    definition.exits = vec!["result".into()];
    definition.edges = vec![
        bcode_workflow::EdgeDefinition {
            from: "branch".into(),
            to: "agent".into(),
            kind: bcode_workflow::EdgeKind::Conditional {
                predicate: predicate.clone(),
                expected: true,
            },
            transform: None,
        },
        bcode_workflow::EdgeDefinition {
            from: "branch".into(),
            to: "recurse".into(),
            kind: bcode_workflow::EdgeKind::Conditional {
                predicate,
                expected: false,
            },
            transform: Some(bcode_workflow::WorkflowTransform {
                version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
                expression: Expr::Increment {
                    value: Box::new(Expr::Input {
                        source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.into(),
                        path: String::new(),
                    }),
                    by: -1,
                },
                output: leaf.input.clone(),
            }),
        },
    ];
    definition.edges.extend(
        ["agent", "recurse"].map(|from| bcode_workflow::EdgeDefinition {
            from: from.into(),
            to: "result".into(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        }),
    );
    assert_eq!(
        representation::definition_dispatch_availability(&definition).unwrap(),
        representation::DispatchAvailability::AgentControlLifecycle,
    );
    definition
}

fn recursive_package_lock(
    identity: &WorkflowDefinitionIdentity,
) -> bcode_workflow::WorkflowPackageLock {
    let executable_digest = identity
        .definition_id
        .rsplit_once('@')
        .unwrap()
        .1
        .to_owned();
    let lock = bcode_workflow::WorkflowPackageLock {
        version: bcode_workflow::WORKFLOW_PACKAGE_LOCK_VERSION,
        package_id: "test/recursive".into(),
        package_source_digest_sha256: "a".repeat(64),
        imports: vec![],
        exports: vec![bcode_workflow::WorkflowPackageLockedExport {
            export: "run".into(),
            member_id: "recursive".into(),
            definition_identity: identity.clone(),
            published_revision: None,
        }],
        members: vec![bcode_workflow::WorkflowPackageLockedMember {
            member_id: "recursive".into(),
            source_digest_sha256: "b".repeat(64),
            executable_digest_sha256: executable_digest,
            definition_identity: identity.clone(),
            published_revision: None,
            dependency_closure: vec![],
        }],
    };
    lock.validate().unwrap();
    lock
}

async fn publish_recursive_package(
    state: &ServerState,
    db: &dyn Database,
    definition: &bcode_workflow::WorkflowDefinition,
    identity: &WorkflowDefinitionIdentity,
    lock: &bcode_workflow::WorkflowPackageLock,
    digest: &str,
) {
    state
        .workflow_store
        .lock()
        .unwrap()
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            definition,
        )
        .unwrap();
    db.insert("workflow_package_publications")
        .value("package_id", lock.package_id.as_str())
        .value("package_lock_digest_sha256", digest)
        .value("lock_json", serde_json::to_string(lock).unwrap())
        .value("receipt_json", "{}")
        .value("published_at_ms", 1_i64)
        .execute(db)
        .await
        .unwrap();
}

async fn start_recursive_package(
    state: &std::sync::Arc<ServerState>,
    db: &dyn Database,
    admission: bcode_workflow_store::NewWorkflowRun,
    identity: &WorkflowDefinitionIdentity,
    lock: &bcode_workflow::WorkflowPackageLock,
    digest: &str,
) -> bcode_workflow_store::NewWorkflowRun {
    let mut root = admission;
    root.run_id = "package-recursive".into();
    root.definition_id.clone_from(&identity.definition_id);
    root.definition_version = identity.definition_version;
    root.input = Some(serde_json::json!(2));
    root.limits.node_execution_cap = 32;
    root.limits.recursion_depth_cap = 8;
    root.limits.descendant_cap = 8;
    assert!(
        admit_prepared_package(state, &root, Some((&lock.package_id, &"0".repeat(64))))
            .await
            .is_err()
    );
    let started = crate::workflow_operations::start_run_with_package(
        state,
        bcode_workflow::WorkflowRunStartRequest {
            definition_id: root.definition_id.clone(),
            definition_version: root.definition_version,
            run_id: Some(root.run_id.clone()),
            workspace_snapshot: root.workspace_snapshot.clone(),
            parent_session_id: root.parent_session_id.as_ref().unwrap().parse().unwrap(),
            parent_session_generation: None,
            binding: None,
            input: root.input.clone(),
            limits: root.limits.clone(),
        },
        None,
        Some((&lock.package_id, digest)),
    )
    .await
    .unwrap();
    assert_eq!(started.run.run_id, root.run_id);
    assert_eq!(started.run.definition_id, root.definition_id);
    assert_eq!(started.run.definition_version, root.definition_version);
    assert_eq!(started.run.parent_session_id, root.parent_session_id);
    assert_eq!(
        representation::identity(db, &root.run_id).await.unwrap(),
        Some(representation::RunRepresentation::Version2)
    );
    root
}

async fn assert_package_resolution(
    db: &dyn Database,
    definition: &bcode_workflow::WorkflowDefinition,
    lock: &bcode_workflow::WorkflowPackageLock,
    digest: &str,
) {
    // Package-local targets are not executable in an unbound admission or unknown binding.
    let target: bcode_workflow::WorkflowCallConfiguration =
        serde_json::from_value(definition.nodes["recurse"].configuration.clone()).unwrap();
    assert!(
        representation::hierarchy::resolve_call(db, None, &target.target)
            .await
            .is_err()
    );
    let future = bcode_workflow_store::WorkflowRunPackageBinding {
        version: 999,
        package_id: lock.package_id.clone(),
        lock_digest: digest.to_owned(),
    };
    assert!(
        representation::hierarchy::resolve_call(db, Some(&future), &target.target)
            .await
            .is_err()
    );
    let known = bcode_workflow_store::WorkflowRunPackageBinding {
        version: 1,
        package_id: lock.package_id.clone(),
        lock_digest: digest.to_owned(),
    };
    let member_call = WorkflowCallTarget::PackageMember {
        member_id: "recursive".into(),
        definition_identity: lock.members[0].definition_identity.clone(),
    };
    assert_eq!(
        representation::hierarchy::resolve_call(db, Some(&known), &member_call)
            .await
            .unwrap(),
        lock.members[0].definition_identity
    );
    let WorkflowCallTarget::PackageLocal {
        reference: mut wrong_interface,
    } = target.target.clone()
    else {
        panic!("local call");
    };
    wrong_interface.output = bcode_workflow::ValueSchema::of::<String>();
    assert!(wrong_interface.resolve(lock, definition).is_err());
    let mut wrong_identity = lock.members[0].definition_identity.clone();
    wrong_identity.definition_id.push_str("-drift");
    assert!(
        representation::hierarchy::resolve_call(
            db,
            Some(&known),
            &WorkflowCallTarget::PackageMember {
                member_id: "recursive".into(),
                definition_identity: wrong_identity,
            }
        )
        .await
        .is_err()
    );
}

async fn assert_recursive_completion(
    state: &std::sync::Arc<ServerState>,
    db: &dyn Database,
    root: &bcode_workflow_store::NewWorkflowRun,
    expected_binding: &bcode_workflow_store::WorkflowRunPackageBinding,
) {
    // Ordinary driver scheduling, not direct child construction or an in-process SDK runner.
    state.start_workflow_driver().await;
    drive_if_represented(state, &root.run_id).await.unwrap();
    let result = super::fanout::wait_public(state, &root.run_id).await;
    stop_fixture_driver(state).await;
    result.expect("finite recursive package reaches provider-backed base case");
    let first = representation::hierarchy::children(db, &root.run_id, None, 10)
        .await
        .unwrap();
    assert_eq!(first.children.len(), 1);
    let second = representation::hierarchy::children(db, &first.children[0].child_run_id, None, 10)
        .await
        .unwrap();
    assert_eq!(second.children.len(), 1);
    assert_eq!(second.children[0].depth, 3);
    assert!(
        representation::hierarchy::children(db, &second.children[0].child_run_id, None, 10)
            .await
            .unwrap()
            .children
            .is_empty()
    );
    for id in [
        &root.run_id,
        &first.children[0].child_run_id,
        &second.children[0].child_run_id,
    ] {
        assert_eq!(
            representation::admission_projection::package_binding(db, id)
                .await
                .unwrap(),
            Some(expected_binding.clone())
        );
        assert_eq!(
            status(state, id).await.unwrap().unwrap().status,
            bcode_workflow::RunStatus::Completed
        );
    }
}

#[tokio::test]
async fn definition_child_uses_common_owner_and_returns_provider_output() {
    let _workflow_runtime_guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    let (_root, _sessions, state, definition) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let guard = owner::local_authority(&state, "new")
        .await
        .expect("authority")
        .expect("owner");
    let context = representation::hierarchy::parent_context(&db, &guard.authority, "new")
        .await
        .expect("context");
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("child-agent", &definition)
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
        .expect("register child");
    let mut parent_definition = definition.clone();
    let node = parent_definition.nodes.get_mut("agent").expect("node");
    node.kind = bcode_workflow::NodeKind::WorkflowCall;
    node.configuration = serde_json::to_value(bcode_workflow::WorkflowCallConfiguration {
        version: bcode_workflow::WORKFLOW_CALL_VERSION,
        target: bcode_workflow::WorkflowCallTarget::Definition { identity },
        input: None,
        output: None,
    })
    .expect("config");
    let parent_identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "child-parent",
        &parent_definition,
    )
    .expect("parent identity");
    let mut parent = context.admission;
    parent.run_id = "child-parent".into();
    parent.definition_id = parent_identity.definition_id;
    parent.definition_version = parent_identity.definition_version;
    parent.limits.node_execution_cap = 2;
    representation::admit(&db, &parent, &parent_definition)
        .await
        .expect("fixture admission; capability remains gated");
    Box::pin(tokio::time::timeout(
        crate::workflow_test_support::WATCHDOG,
        async {
            let mut progress = crate::workflow_test_support::Progress::subscribe(&state);
            loop {
                driver::drive(&state, "child-parent")
                    .await
                    .expect("parent drive");
                let page = representation::hierarchy::children(&db, "child-parent", None, 32)
                    .await
                    .expect("children");
                for link in page.children {
                    driver::drive(&state, &link.child_run_id)
                        .await
                        .expect("child drive");
                }
                progress.drove();
                if status(&state, "child-parent")
                    .await
                    .expect("status")
                    .expect("parent")
                    .status
                    == bcode_workflow::RunStatus::Completed
                {
                    break;
                }
                progress.changed().await;
            }
        },
    ))
    .await
    .expect("child completes without recursive parent lock");
    let children = representation::hierarchy::children(&db, "child-parent", None, 32)
        .await
        .expect("children");
    assert_eq!(children.children.len(), 1);
    assert_eq!(children.children[0].depth, 2);
    cleanup.finish().await;
    drop(cleanup);
}

#[tokio::test]
async fn normal_definition_child_completion_wakes_parent() {
    let _workflow_runtime_guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    let (_root, _sessions, state, definition) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("normal-child", &definition)
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
        .expect("child definition");
    let mut parent = definition;
    let call = parent.nodes.get_mut("agent").expect("call");
    call.kind = bcode_workflow::NodeKind::WorkflowCall;
    call.configuration = serde_json::to_value(bcode_workflow::WorkflowCallConfiguration {
        version: bcode_workflow::WORKFLOW_CALL_VERSION,
        target: bcode_workflow::WorkflowCallTarget::Definition { identity },
        input: None,
        output: None,
    })
    .expect("call config");
    state.start_workflow_driver().await;
    super::fanout::start_public(&state, "normal-parent", parent, serde_json::json!(1), None).await;
    let result = super::fanout::wait_public(&state, "normal-parent").await;
    result.expect("child result reaches parent through ordinary scheduler");
    cleanup.finish().await;
    drop(cleanup);
}

#[tokio::test]
async fn public_allowance_grant_is_atomic_and_exact_retry_is_unchanged() {
    let _workflow_runtime_guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    let guard = owner::local_authority(&state, "new")
        .await
        .expect("authority")
        .expect("owner");
    let before = representation::hierarchy::parent_context(&db, &guard.authority, "new")
        .await
        .expect("context");
    let expected = before.effective_limits.node_execution_cap;
    let action = bcode_workflow::WorkflowRunControlAction::IncreaseExecutionAllowance {
        expected_cap: expected,
        target_cap: expected + 1,
    };
    let first = crate::workflow_operations::control_exact_run(&state, "new", action)
        .await
        .expect("grant");
    let duplicate = crate::workflow_operations::control_exact_run(&state, "new", action)
        .await
        .expect("duplicate");
    assert!(first.1);
    assert!(!duplicate.1);
    let after = representation::hierarchy::parent_context(&db, &guard.authority, "new")
        .await
        .expect("context");
    assert_eq!(after.effective_limits.node_execution_cap, expected + 1);
    assert_eq!(
        after.effective_limits.retry_cap,
        before.effective_limits.retry_cap
    );
    cleanup.finish().await;
    drop(cleanup);
}
