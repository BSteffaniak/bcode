//! Bound package-child admission acceptance under immutable locks and authority transfer.
use super::*;

fn bound_fixture() -> (
    tempfile::TempDir,
    WorkflowStore,
    NewChildWorkflowRun,
    WorkflowExecutionAuthority,
) {
    bound_fixture_with_local_call(false)
}

fn bound_fixture_with_local_call(
    local: bool,
) -> (
    tempfile::TempDir,
    WorkflowStore,
    NewChildWorkflowRun,
    WorkflowExecutionAuthority,
) {
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    let (lock, parent_identity, child_identity, target) = publish_bound_package(&mut store, local);
    let request = prepare_bound_child(&mut store, &lock, parent_identity, child_identity, target);
    let authority = request.run.execution_authority.clone().unwrap();
    (root, store, request, authority)
}

fn publish_bound_package(
    store: &mut WorkflowStore,
    local: bool,
) -> (
    WorkflowPackageLock,
    WorkflowDefinitionIdentity,
    WorkflowDefinitionIdentity,
    bcode_workflow::WorkflowCallTarget,
) {
    let child_definition = definition("package-child");
    let child_identity =
        WorkflowDefinitionIdentity::for_definition("test/package/child", &child_definition)
            .unwrap();
    let target = if local {
        bcode_workflow::WorkflowCallTarget::PackageLocal {
            reference: bcode_workflow::WorkflowPackageLocalCall {
                version: 1,
                member_id: "child".into(),
                workflow_id: child_identity.kind.clone(),
                input: child_definition.input.clone(),
                output: child_definition.output.clone(),
            },
        }
    } else {
        bcode_workflow::WorkflowCallTarget::PackageMember {
            member_id: "child".into(),
            definition_identity: child_identity.clone(),
        }
    };
    let mut parent_definition = workflow_call_definition(child_identity.clone());
    let mut call: bcode_workflow::WorkflowCallConfiguration =
        serde_json::from_value(parent_definition.nodes["call"].configuration.clone()).unwrap();
    call.target = target.clone();
    parent_definition
        .nodes
        .get_mut("call")
        .unwrap()
        .configuration = serde_json::to_value(call).unwrap();
    let parent_identity =
        WorkflowDefinitionIdentity::for_definition("test/package/parent", &parent_definition)
            .unwrap();
    let (workflow, draft, _) = authored_store_fixture();
    let mut locked = Vec::new();
    for (id, identity, definition) in [
        ("parent", &parent_identity, parent_definition),
        ("child", &child_identity, child_definition),
    ] {
        let mut workflow = workflow.clone();
        workflow.workflow_id = identity.kind.clone();
        workflow.active_revision = None;
        let mut draft = draft.clone();
        draft.workflow_id = identity.kind.clone();
        draft.draft_id = "package".into();
        draft.document.workflow_id = identity.kind.clone();
        draft.document.definition = definition;
        draft.document.bindings.clear();
        draft.document.requirements = bcode_workflow::WorkflowRequirementSummary::default();
        draft.checksum_sha256 = draft.document.source_digest_sha256().unwrap();
        store.create_authored_workflow(&workflow).unwrap();
        store.create_workflow_draft(&draft).unwrap();
        locked.push(bcode_workflow::WorkflowPackageLockedMember {
            member_id: id.into(),
            source_digest_sha256: draft.document.source_digest_sha256().unwrap(),
            executable_digest_sha256: draft.document.executable_source_digest_sha256().unwrap(),
            definition_identity: identity.clone(),
            published_revision: None,
            dependency_closure: if id == "parent" {
                vec!["child".into()]
            } else {
                Vec::new()
            },
        });
    }
    locked.sort_by(|a, b| a.member_id.cmp(&b.member_id));
    let lock = WorkflowPackageLock {
        version: bcode_workflow::WORKFLOW_PACKAGE_LOCK_VERSION,
        package_id: "test/package".into(),
        package_source_digest_sha256: "a".repeat(64),
        imports: Vec::new(),
        exports: vec![bcode_workflow::WorkflowPackageLockedExport {
            export: "main".into(),
            member_id: "parent".into(),
            definition_identity: parent_identity.clone(),
            published_revision: None,
        }],
        members: locked,
    };
    let published = store
        .publish_workflow_package(
            &WorkflowPackagePublishRequest {
                version: bcode_workflow::WORKFLOW_PACKAGE_MUTATION_VERSION,
                package_id: lock.package_id.clone(),
                expected_lock: lock,
                expected_generations: ["parent", "child"]
                    .into_iter()
                    .map(|id| bcode_workflow::WorkflowPackageExpectedGeneration {
                        member_id: id.into(),
                        expected_generation: 1,
                    })
                    .collect(),
            },
            10,
        )
        .unwrap();
    let lock = published.lock.unwrap();
    (lock, parent_identity, child_identity, target)
}

fn prepare_bound_child(
    store: &mut WorkflowStore,
    lock: &WorkflowPackageLock,
    parent_identity: WorkflowDefinitionIdentity,
    child_identity: WorkflowDefinitionIdentity,
    target: bcode_workflow::WorkflowCallTarget,
) -> NewChildWorkflowRun {
    prepare_bound_child_with_limits(
        store,
        lock,
        parent_identity,
        child_identity,
        target,
        WorkflowRunLimits::default(),
    )
}

fn prepare_bound_child_with_limits(
    store: &mut WorkflowStore,
    lock: &WorkflowPackageLock,
    parent_identity: WorkflowDefinitionIdentity,
    child_identity: WorkflowDefinitionIdentity,
    target: bcode_workflow::WorkflowCallTarget,
    limits: WorkflowRunLimits,
) -> NewChildWorkflowRun {
    let digest = lock.digest_sha256().unwrap();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact-a".into(),
        daemon_instance_id: "daemon-a".into(),
        generation: 1,
        fencing_token: "fence-a".into(),
    };
    let mut parent = new_run();
    parent.limits = limits;
    parent.run_id = "bound-parent".into();
    parent.definition_id = parent_identity.definition_id;
    parent.definition_version = parent_identity.definition_version;
    parent.authored_provenance = None;
    parent.execution_authority = Some(authority);
    parent.input = Some(serde_json::json!(1));
    store
        .create_run_with_package(&parent, Some((&lock.package_id, &digest)))
        .unwrap();
    let activation = store
        .pending_activations(10)
        .unwrap()
        .into_iter()
        .find(|activation| activation.run_id == parent.run_id)
        .unwrap();
    store
        .prepare_pending_activation(
            &parent.run_id,
            "call",
            &activation.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            11,
        )
        .unwrap();
    let child_run_id = workflow_child_run_id(
        &parent.run_id,
        &parent.run_id,
        &activation.activation_id,
        1,
        &target,
    );
    let mut child = parent.clone();
    child.run_id = child_run_id.clone();
    child.definition_id = child_identity.definition_id;
    child.definition_version = child_identity.definition_version;
    child.created_at_ms = 12;
    NewChildWorkflowRun {
        link: WorkflowRunLink {
            version: WORKFLOW_RUN_LINK_VERSION,
            root_run_id: parent.run_id.clone(),
            parent_run_id: parent.run_id,
            parent_node_id: "call".into(),
            parent_activation_id: activation.activation_id,
            parent_attempt: 1,
            child_run_id,
            target,
            depth: 2,
            created_at_ms: 12,
        },
        run: child,
    }
}

#[test]
fn bound_child_inherits_exact_lock_and_survives_reopen() {
    let (root, mut store, request, _) = bound_fixture();
    assert!(store.create_child_run_idempotent(&request).unwrap());
    assert!(!store.create_child_run_idempotent(&request).unwrap());
    let identity = store
        .resolve_run_package_member(&request.run.run_id, "child")
        .unwrap();
    assert_eq!(identity.definition_id, request.run.definition_id);
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    assert_eq!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .unwrap(),
        identity
    );
    assert!(!store.create_child_run_idempotent(&request).unwrap());
}

#[test]
fn stale_transfer_cannot_allocate_or_rebind_package_child() {
    let (_root, mut store, mut request, authority) = bound_fixture();
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "daemon-b".into(),
        generation: 2,
        fencing_token: "fence-b".into(),
        ..authority.clone()
    };
    store
        .transfer_execution_authority(&request.link.parent_run_id, &authority, &replacement, 12)
        .unwrap();
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert!(store.run_summary(&request.run.run_id).unwrap().is_none());
    assert!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .is_err()
    );
    request.run.execution_authority = Some(replacement);
    assert!(store.create_child_run_idempotent(&request).unwrap());
    assert_eq!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .unwrap()
            .definition_id,
        request.run.definition_id
    );
}

#[test]
fn package_local_child_inherits_exact_lock_and_survives_reopen() {
    let (root, mut store, request, _) = bound_fixture_with_local_call(true);
    assert!(store.create_child_run_idempotent(&request).unwrap());
    assert!(!store.create_child_run_idempotent(&request).unwrap());
    let identity = store
        .resolve_run_package_member(&request.run.run_id, "child")
        .unwrap();
    assert_eq!(identity.definition_id, request.run.definition_id);
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    assert_eq!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .unwrap(),
        identity
    );
    assert!(!store.create_child_run_idempotent(&request).unwrap());
}

#[test]
fn stale_transfer_cannot_allocate_package_local_child() {
    let (_root, mut store, mut request, authority) = bound_fixture_with_local_call(true);
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "daemon-b".into(),
        generation: 2,
        fencing_token: "fence-b".into(),
        ..authority.clone()
    };
    store
        .transfer_execution_authority(&request.link.parent_run_id, &authority, &replacement, 12)
        .unwrap();
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert!(store.run_summary(&request.run.run_id).unwrap().is_none());
    assert!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .is_err()
    );
    request.run.execution_authority = Some(replacement);
    assert!(store.create_child_run_idempotent(&request).unwrap());
    assert_eq!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .unwrap()
            .definition_id,
        request.run.definition_id
    );
}

#[test]
fn stale_transfer_cannot_rebind_existing_package_local_child() {
    let (_root, mut store, request, authority) = bound_fixture_with_local_call(true);
    assert!(store.create_child_run_idempotent(&request).unwrap());
    let identity = store
        .resolve_run_package_member(&request.run.run_id, "child")
        .unwrap();
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "daemon-b".into(),
        generation: 2,
        fencing_token: "fence-b".into(),
        ..authority.clone()
    };
    store
        .transfer_execution_authority(&request.link.parent_run_id, &authority, &replacement, 13)
        .unwrap();
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert_eq!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .unwrap(),
        identity
    );
    assert_eq!(
        execution_authority_in(&store.connection, &request.run.run_id).unwrap(),
        Some(authority)
    );
}

#[test]
fn package_local_child_rejects_wrong_executable_without_allocating() {
    let (_root, mut store, request, _) = bound_fixture_with_local_call(true);
    let parent = store
        .run_summary(&request.link.parent_run_id)
        .unwrap()
        .unwrap();
    let mut wrong = request.clone();
    wrong.run.definition_id = parent.definition_id;
    wrong.run.definition_version = parent.definition_version;
    assert!(store.create_child_run_idempotent(&wrong).is_err());
    assert!(store.run_summary(&request.run.run_id).unwrap().is_none());
    assert!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .is_err()
    );
    assert!(store.create_child_run_idempotent(&request).unwrap());
}

#[test]
fn known_schema_upgrade_preserves_package_local_binding_and_pending_child() {
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    let (lock, parent_identity, child_identity, target) = publish_bound_package(&mut store, true);
    let digest = lock.digest_sha256().unwrap();
    let request = prepare_bound_child_with_limits(
        &mut store,
        &lock,
        parent_identity.clone(),
        child_identity,
        target,
        WorkflowRunLimits {
            concurrency_cap: Some(7),
            ..WorkflowRunLimits::default()
        },
    );
    let limits = store
        .run_limits(&request.link.parent_run_id)
        .unwrap()
        .unwrap();
    let authority = request.run.execution_authority.clone().unwrap();
    assert_eq!(detected_store_schema(&store.connection), Some(42));
    let error = WorkflowStore::migrate_to_current_in_state_dir(root.path(), 20)
        .expect_err("retained owner must prevent migration");
    assert!(matches!(
        error,
        WorkflowStoreError::InvalidData(message)
            if message == "workflow store migration requires exclusive ownership; stop active workflow writers"
    ));
    assert_eq!(detected_store_schema(&store.connection), Some(42));
    assert_eq!(
        store
            .run_limits(&request.link.parent_run_id)
            .unwrap()
            .unwrap(),
        limits
    );
    assert!(store.run_summary(&request.run.run_id).unwrap().is_none());
    drop(store);

    let receipt = WorkflowStore::migrate_to_current_in_state_dir(root.path(), 20).unwrap();
    assert_eq!(receipt.previous_schema_version, 42);
    assert_eq!(receipt.new_schema_version, WORKFLOW_STORE_SCHEMA_VERSION);
    assert!(receipt.backup_path.is_file());
    let mut store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    assert_eq!(
        store
            .workflow_package_lock(&lock.package_id, &digest)
            .unwrap(),
        Some(lock)
    );
    assert_eq!(
        store
            .run_limits(&request.link.parent_run_id)
            .unwrap()
            .unwrap(),
        limits
    );
    assert_eq!(
        store
            .resolve_run_package_member(&request.link.parent_run_id, "parent")
            .unwrap(),
        parent_identity
    );
    assert_eq!(
        execution_authority_in(&store.connection, &request.link.parent_run_id).unwrap(),
        Some(authority)
    );
    assert!(store.create_child_run_idempotent(&request).unwrap());
    assert_eq!(
        store
            .resolve_run_package_member(&request.run.run_id, "child")
            .unwrap()
            .definition_id,
        request.run.definition_id
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    assert!(!store.create_child_run_idempotent(&request).unwrap());
}

#[test]
fn bound_child_rejects_historical_call_contract_without_allocating() {
    reject_historical_call_contract(false);
}

#[test]
fn package_local_child_rejects_historical_call_contract_without_allocating() {
    reject_historical_call_contract(true);
}

fn reject_historical_call_contract(local: bool) {
    let (_root, mut store, request, _) = bound_fixture_with_local_call(local);
    let node = store
        .run_graph_node(&request.link.parent_run_id, "call")
        .unwrap()
        .unwrap();
    let mut historical = node.clone();
    historical.configuration["version"] =
        serde_json::json!(bcode_workflow::WORKFLOW_CALL_VERSION - 1);
    crate::database::block_on(
        switchy_database::query::update("workflow_run_graph_nodes")
            .value("node_json", serde_json::to_string(&historical).unwrap())
            .where_eq("run_id", request.link.parent_run_id.as_str())
            .where_eq("node_id", "call")
            .execute(&store.connection),
    )
    .unwrap();
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert!(store.run_summary(&request.run.run_id).unwrap().is_none());
    crate::database::block_on(
        switchy_database::query::update("workflow_run_graph_nodes")
            .value("node_json", serde_json::to_string(&node).unwrap())
            .where_eq("run_id", request.link.parent_run_id.as_str())
            .where_eq("node_id", "call")
            .execute(&store.connection),
    )
    .unwrap();
    assert!(store.create_child_run_idempotent(&request).unwrap());
}

#[test]
fn bound_child_rejects_future_lineage_and_inherited_limit_widening() {
    reject_future_lineage_and_inherited_limit_widening(false);
}

#[test]
fn package_local_child_rejects_future_lineage_and_inherited_limit_widening() {
    reject_future_lineage_and_inherited_limit_widening(true);
}

fn reject_future_lineage_and_inherited_limit_widening(local: bool) {
    let (_root, mut store, request, _) = bound_fixture_with_local_call(local);
    let mut future = request.clone();
    future.link.version += 1;
    assert!(store.create_child_run_idempotent(&future).is_err());
    let mut widened = request.clone();
    widened.run.limits.recursion_depth_cap += 1;
    assert!(store.create_child_run_idempotent(&widened).is_err());
    assert!(store.run_summary(&request.run.run_id).unwrap().is_none());
    assert!(store.create_child_run_idempotent(&request).unwrap());
}
